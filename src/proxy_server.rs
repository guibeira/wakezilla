use anyhow::Result;
use axum::{
    body::Body,
    extract::{Host, Json as JsonExtract, Path, Query, State},
    http::{header, uri::Authority, Method, Request, Response, StatusCode},
    response::{IntoResponse, Json, Redirect},
    routing::{delete, get, post, put},
    Router,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::{net::TcpListener, sync::RwLock};
use tower::ServiceBuilder;
use tower_http::cors::CorsLayer;
use tracing::{error, info};

use crate::forward;
use crate::scanner;
#[cfg(test)]
use crate::web::Machine;
use crate::web::{self, AppState};
use crate::wol;
use include_dir::{include_dir, Dir};
use mime_guess::from_path;
use std::path::{Component, Path as StdPath};

static FRONTEND_DIST: Dir<'_> = include_dir!("$WAKEZILLA_FRONTEND_DIST");

fn shutdown_setup_for_machine(machine: &web::Machine) -> wakezilla_common::ShutdownSetup {
    use wakezilla_common::{ShutdownSetup, ShutdownSetupStatus};

    if !machine.can_be_turned_off || machine.turn_off_port.is_none() {
        return ShutdownSetup {
            status: ShutdownSetupStatus::Disabled,
            unix_command: None,
            windows_command: None,
        };
    }
    let Some(key) = machine.shutdown_auth_key.as_deref() else {
        return ShutdownSetup {
            status: ShutdownSetupStatus::Legacy,
            unix_command: None,
            windows_command: None,
        };
    };
    if machine.shutdown_auth_verified {
        return ShutdownSetup {
            status: ShutdownSetupStatus::Verified,
            unix_command: None,
            windows_command: None,
        };
    }

    let port = machine.turn_off_port.expect("checked above");
    let args = format!("wakezilla setup --mode client --port {port} --key {key} --yes");
    ShutdownSetup {
        status: ShutdownSetupStatus::Pending,
        unix_command: Some(format!("sudo {args}")),
        windows_command: Some(args),
    }
}

fn apply_shutdown_security(old: Option<&web::Machine>, new: &mut web::Machine) {
    if !new.can_be_turned_off || new.turn_off_port.is_none() {
        new.shutdown_auth_key = None;
        new.shutdown_auth_verified = false;
        return;
    }

    let Some(old) = old.filter(|old| old.can_be_turned_off && old.turn_off_port.is_some()) else {
        new.shutdown_auth_key = Some(crate::shutdown_auth::generate_key());
        new.shutdown_auth_verified = false;
        return;
    };
    let Some(key) = old.shutdown_auth_key.clone() else {
        // Preserve intentionally compatible legacy installations until the user
        // explicitly chooses to secure them.
        new.shutdown_auth_key = None;
        new.shutdown_auth_verified = false;
        return;
    };

    let connection_unchanged =
        old.mac == new.mac && old.ip == new.ip && old.turn_off_port == new.turn_off_port;
    new.shutdown_auth_key = Some(key);
    new.shutdown_auth_verified = old.shutdown_auth_verified && connection_unchanged;
}

fn respond_with_file(file: &include_dir::File<'_>) -> Response<Body> {
    let mime = from_path(file.path()).first_or_octet_stream();
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime.as_ref())
        .body(Body::from(file.contents().to_vec()))
        .unwrap()
}

fn not_found() -> Response<Body> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::empty())
        .unwrap()
}

fn asset_response(path: &str) -> Response<Body> {
    let trimmed = path.trim_start_matches('/');
    let target = if trimmed.is_empty() {
        "index.html"
    } else {
        trimmed
    };

    if StdPath::new(target)
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return not_found();
    }

    if let Some(file) = FRONTEND_DIST.get_file(target) {
        return respond_with_file(file);
    }

    if !target.contains('.') {
        if let Some(index) = FRONTEND_DIST.get_file("index.html") {
            return respond_with_file(index);
        }
    }

    not_found()
}

fn development_server_url(request_host: &str) -> String {
    let hostname = request_host
        .parse::<Authority>()
        .map(|authority| authority.host().to_owned())
        .unwrap_or_else(|_| "localhost".to_owned());
    let hostname = if hostname.contains(':') && !hostname.starts_with('[') {
        format!("[{hostname}]")
    } else {
        hostname
    };
    format!("http://{hostname}:8080")
}

async fn serve_index(Host(host): Host) -> Response<Body> {
    if cfg!(debug_assertions) {
        return Redirect::to(&development_server_url(&host)).into_response();
    }

    asset_response("")
}

async fn spa_fallback(req: Request<Body>) -> Response<Body> {
    match req.method() {
        &Method::GET | &Method::HEAD => {
            let mut response = asset_response(req.uri().path());
            if req.method() == Method::HEAD {
                *response.body_mut() = Body::empty();
            }
            response
        }
        _ => not_found(),
    }
}

pub async fn start(config: crate::config::Config) -> Result<()> {
    start_with_shutdown(config, shutdown_signal()).await
}

pub async fn start_with_shutdown(
    config: crate::config::Config,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let port = config.server.proxy_port;
    let initial_machines = match web::load_machines_with_config(&config) {
        Ok(machines) => machines,
        Err(err) => {
            error!("Failed to load machines from storage: {err}");
            Vec::new()
        }
    };

    let max_access_records = config.storage.max_access_records;
    let access_log = crate::access_log::AccessLog::load_with_config(max_access_records, &config);
    let state = AppState {
        machines: Arc::new(RwLock::new(initial_machines.clone())),
        proxies: Arc::new(RwLock::new(HashMap::new())),
        config: Arc::new(config),
        turn_off_limiter: Arc::new(forward::TurnOffLimiter::new()),
        monitor_handle: Arc::new(std::sync::Mutex::new(None)),
        access_log: Arc::new(RwLock::new(access_log)),
    };

    // Start global monitor
    web::start_global_monitor(&state);

    {
        let flush_state = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            interval.tick().await; // consume the immediate first tick
            loop {
                interval.tick().await;
                let snapshot = flush_state.access_log.read().await.clone();
                match tokio::task::spawn_blocking(move || snapshot.save()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => error!("Failed to flush access history: {e}"),
                    Err(e) => error!("Access history flush task panicked: {e}"),
                }
            }
        });
    }

    for machine in &initial_machines {
        web::start_proxy_if_configured(machine, &state);
    }

    let app = build_router(state.clone());
    let endpoints = api_routes(state.clone());
    let app = app.merge(endpoints);

    let cors_layer = CorsLayer::permissive();

    let app = app.layer(ServiceBuilder::new().layer(cors_layer).into_inner());
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(addr).await?;
    info!("listening on http://{}", listener.local_addr()?);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;

    // Persist access history on shutdown so the last interval isn't lost.
    let snapshot = state.access_log.read().await.clone();
    if let Err(e) = snapshot.save() {
        error!("Failed to flush access history on shutdown: {e}");
    }

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    info!("Shutdown signal received");
}

pub fn api_routes(state: AppState) -> Router {
    Router::new()
        .route("/api/interfaces", get(list_interfaces_handler))
        .route("/api/scan", get(scan_network_handler))
        .route(
            "/api/machines",
            get(show_machines_api).post(add_machine_api),
        )
        .route("/api/machines/:mac", get(get_machine_details_api))
        .route(
            "/api/machines/:mac/access-history",
            get(get_access_history_api),
        )
        .route("/api/machines/:mac", put(update_machine_api))
        .route(
            "/api/machines/:mac/remote-turn-off",
            post(api_turn_off_remote_machine),
        )
        .route("/api/machines/:mac/wake", post(api_wake_machine))
        .route("/api/machines/:mac/is-on", get(is_machine_on_api))
        .route("/api/machines/:mac/shutdown-setup", get(get_shutdown_setup))
        .route(
            "/api/machines/:mac/shutdown-setup/verify",
            post(verify_shutdown_setup),
        )
        .route(
            "/api/machines/:mac/shutdown-setup/rotate",
            post(rotate_shutdown_key),
        )
        .route("/api/machines/delete", delete(delete_machine_api))
        .with_state(state)
}

type ShutdownSetupError = (axum::http::StatusCode, Json<serde_json::Value>);

fn shutdown_setup_error(status: StatusCode, message: &str) -> ShutdownSetupError {
    (status, Json(serde_json::json!({ "error": message })))
}

async fn get_shutdown_setup(
    State(state): State<AppState>,
    Path(mac): Path<String>,
) -> Result<Json<wakezilla_common::ShutdownSetup>, ShutdownSetupError> {
    let machines = state.machines.read().await;
    let machine = machines
        .iter()
        .find(|machine| machine.mac == mac)
        .ok_or_else(|| shutdown_setup_error(StatusCode::NOT_FOUND, "Machine not found"))?;
    Ok(Json(shutdown_setup_for_machine(machine)))
}

async fn verify_shutdown_setup(
    State(state): State<AppState>,
    Path(mac): Path<String>,
) -> Result<Json<wakezilla_common::ShutdownSetup>, ShutdownSetupError> {
    use wakezilla_common::ShutdownSetupStatus;

    let (machine, key) = {
        let machines = state.machines.read().await;
        let machine = machines
            .iter()
            .find(|machine| machine.mac == mac)
            .cloned()
            .ok_or_else(|| shutdown_setup_error(StatusCode::NOT_FOUND, "Machine not found"))?;
        let setup = shutdown_setup_for_machine(&machine);
        if setup.status != ShutdownSetupStatus::Pending {
            return Ok(Json(setup));
        }
        let key = machine
            .shutdown_auth_key
            .clone()
            .expect("pending setup always has a key");
        (machine, key)
    };

    let verification = forward::verify_remote_client(
        &machine.ip.to_string(),
        machine
            .turn_off_port
            .expect("pending setup always has a port"),
        &key,
    )
    .await;

    match verification {
        forward::ClientVerification::Verified => {
            let mut machines = state.machines.write().await;
            let index = machines
                .iter_mut()
                .position(|candidate| candidate.mac == mac)
                .ok_or_else(|| shutdown_setup_error(StatusCode::NOT_FOUND, "Machine not found"))?;
            if machines[index].shutdown_auth_key.as_deref() != Some(key.as_str()) {
                return Ok(Json(shutdown_setup_for_machine(&machines[index])));
            }
            let previous_verified = machines[index].shutdown_auth_verified;
            machines[index].shutdown_auth_verified = true;
            let setup = shutdown_setup_for_machine(&machines[index]);
            if let Err(error) = web::save_machines_with_config(&machines, &state.config) {
                machines[index].shutdown_auth_verified = previous_verified;
                error!("failed to persist verified shutdown setup: {error}");
                return Err(shutdown_setup_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to persist shutdown setup",
                ));
            }
            Ok(Json(setup))
        }
        forward::ClientVerification::KeyMismatch => {
            let mut setup = shutdown_setup_for_machine(&machine);
            setup.status = ShutdownSetupStatus::KeyMismatch;
            Ok(Json(setup))
        }
        forward::ClientVerification::Unreachable => {
            let mut setup = shutdown_setup_for_machine(&machine);
            setup.status = ShutdownSetupStatus::Unreachable;
            Ok(Json(setup))
        }
    }
}

async fn rotate_shutdown_key(
    State(state): State<AppState>,
    Path(mac): Path<String>,
) -> Result<Json<wakezilla_common::ShutdownSetup>, ShutdownSetupError> {
    let mut machines = state.machines.write().await;
    let index = machines
        .iter()
        .position(|machine| machine.mac == mac)
        .ok_or_else(|| shutdown_setup_error(StatusCode::NOT_FOUND, "Machine not found"))?;
    let machine = &mut machines[index];
    if !machine.can_be_turned_off || machine.turn_off_port.is_none() {
        return Err(shutdown_setup_error(
            StatusCode::BAD_REQUEST,
            "Remote shutdown is disabled",
        ));
    }
    let previous_key = machine.shutdown_auth_key.clone();
    let previous_verified = machine.shutdown_auth_verified;
    machine.shutdown_auth_key = Some(crate::shutdown_auth::generate_key());
    machine.shutdown_auth_verified = false;
    let setup = shutdown_setup_for_machine(machine);
    if let Err(error) = web::save_machines_with_config(&machines, &state.config) {
        machines[index].shutdown_auth_key = previous_key;
        machines[index].shutdown_auth_verified = previous_verified;
        error!("failed to persist rotated shutdown key: {error}");
        return Err(shutdown_setup_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to persist shutdown setup",
        ));
    }
    Ok(Json(setup))
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(serve_index))
        .fallback(spa_fallback)
        .with_state(state)
}

async fn scan_network_handler(Query(params): Query<HashMap<String, String>>) -> impl IntoResponse {
    let interface = params.get("interface").map(|s| s.as_str());
    match scanner::NetworkInterface::scan_network_with_interface(interface).await {
        Ok(devices) => Ok(Json(
            devices
                .into_iter()
                .map(|d| wakezilla_common::DiscoveredDevice {
                    ip: d.ip,
                    mac: d.mac,
                    hostname: d.hostname,
                })
                .collect::<Vec<_>>(),
        )),
        Err(e) => {
            error!("Network scan failed: {}", e);
            Err(axum::http::StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn is_machine_on_api(
    State(state): State<AppState>,
    Path(mac): Path<String>,
) -> impl IntoResponse {
    let Some((url, machine_name)) = ({
        let machines = state.machines.read().await;

        machines.iter().find(|m| m.mac == mac).map(|machine| {
            let url = format!(
                "http://{}:{}/health",
                machine.ip,
                machine.turn_off_port.unwrap_or(3001)
            );
            (url, machine.name.clone())
        })
    }) else {
        return Err(axum::http::StatusCode::NOT_FOUND);
    };

    let client = match reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(
            state.config.server.health_timeout_secs.max(1),
        ))
        .build()
    {
        Ok(client) => client,
        Err(_) => return Err(axum::http::StatusCode::INTERNAL_SERVER_ERROR),
    };
    match client.get(&url).send().await {
        Ok(res) => {
            if res.status() == 200 {
                Ok((
                    axum::http::StatusCode::OK,
                    Json(serde_json::json!({ "is_on": true })),
                ))
            } else {
                Ok((
                    axum::http::StatusCode::OK,
                    Json(serde_json::json!({ "is_on": false })),
                ))
            }
        }
        Err(e) => {
            info!("Network error for machine {}: {}", machine_name, e);
            Ok((
                axum::http::StatusCode::OK,
                Json(serde_json::json!({ "is_on": false })),
            ))
        }
    }
}

async fn list_interfaces_handler() -> impl IntoResponse {
    match scanner::NetworkInterface::list_interfaces().await {
        Ok(interfaces) => Ok(Json(
            interfaces
                .into_iter()
                .map(|iface| wakezilla_common::NetworkInterface {
                    name: iface.name,
                    ip: iface.ip,
                    mac: iface.mac,
                    is_up: iface.is_up,
                })
                .collect::<Vec<_>>(),
        )),
        Err(e) => {
            error!("Failed to list interfaces: {}", e);
            Err(axum::http::StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

fn validate_machine_config(
    machine: &wakezilla_common::Machine,
    machines: &[web::Machine],
    replacing: Option<&str>,
) -> Result<(), HashMap<String, Vec<String>>> {
    let mut errors = HashMap::new();
    if machine.name.trim().is_empty() {
        errors.insert("name".into(), vec!["Name is required".into()]);
    }
    if machine.ip.parse::<std::net::Ipv4Addr>().is_err() {
        errors.insert("ip".into(), vec!["Invalid IPv4 address".into()]);
    }
    if web::validate_mac(&machine.mac).is_err() {
        errors.insert("mac".into(), vec!["Invalid MAC address".into()]);
    }
    if machine.turn_off_port == Some(0)
        || (machine.can_be_turned_off && machine.turn_off_port.is_none())
    {
        errors.insert(
            "turn_off_port".into(),
            vec!["A client port between 1 and 65535 is required".into()],
        );
    }
    let mut ports = std::collections::HashSet::new();
    for service in &machine.port_forwards {
        if service.local_port == 0 || service.target_port == 0 || !ports.insert(service.local_port)
        {
            errors.insert(
                "port_forwards".into(),
                vec!["Use valid, unique local ports and valid target ports".into()],
            );
        }
    }
    for existing in machines
        .iter()
        .filter(|existing| Some(existing.mac.as_str()) != replacing)
    {
        if existing
            .mac
            .replace('-', ":")
            .eq_ignore_ascii_case(&machine.mac.replace('-', ":"))
        {
            errors.insert(
                "mac".into(),
                vec!["This MAC address is already registered".into()],
            );
        }
        if existing
            .port_forwards
            .iter()
            .any(|service| ports.contains(&service.local_port))
        {
            errors.insert(
                "port_forwards".into(),
                vec!["A local port is already assigned to another machine".into()],
            );
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

async fn add_machine_api(
    State(state): State<AppState>,
    JsonExtract(payload): JsonExtract<wakezilla_common::AddMachinePayload>,
) -> impl IntoResponse {
    let mut errors_map = HashMap::new();
    if payload.name.trim().is_empty() {
        errors_map.insert("name".to_string(), vec!["Name is required".to_string()]);
    }
    if web::validate_ip(&payload.ip).is_err() {
        errors_map.insert("ip".to_string(), vec!["Invalid IP address".to_string()]);
    }
    if web::validate_mac(&payload.mac).is_err() {
        errors_map.insert("mac".to_string(), vec!["Invalid MAC address".to_string()]);
    }
    if !errors_map.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "errors": errors_map })),
        );
    }

    let api_machine = wakezilla_common::Machine {
        machine_type: payload.machine_type.unwrap_or_default(),
        mac: payload.mac,
        ip: payload.ip,
        name: payload.name,
        description: payload.description,
        turn_off_port: payload.turn_off_port,
        can_be_turned_off: payload.can_be_turned_off,
        inactivity_period: payload
            .inactivity_period
            .unwrap_or(web::get_default_inactivity_period()),
        port_forwards: payload.port_forwards.unwrap_or_default(),
    };
    let mut new_machine = match web::api_machine_to_internal(&api_machine) {
        Ok(machine) => machine,
        Err(err) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "errors": { "ip": [err.to_string()] }
                })),
            );
        }
    };
    if new_machine.can_be_turned_off && new_machine.turn_off_port.is_some() {
        new_machine.shutdown_auth_key = Some(crate::shutdown_auth::generate_key());
    }

    let mut machines = state.machines.write().await;
    if let Err(errors) = validate_machine_config(&api_machine, &machines, None) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "errors": errors })),
        );
    }
    let mut updated = machines.clone();
    updated.push(new_machine.clone());
    if let Err(e) = web::save_machines_with_config(&updated, &state.config) {
        error!("Error saving machines: {}", e);
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "Failed to save machines" })),
        );
    }
    *machines = updated;
    web::start_proxy_if_configured(&new_machine, &state);
    (
        axum::http::StatusCode::CREATED,
        Json(serde_json::json!({ "status": "Machine added" })),
    )
}

async fn show_machines_api(State(state): State<AppState>) -> impl IntoResponse {
    let mut machines = state.machines.read().await.clone();
    machines.reverse();
    Json(
        machines
            .iter()
            .map(web::machine_to_api_machine)
            .collect::<Vec<_>>(),
    )
}

async fn get_machine_details_api(
    State(state): State<AppState>,
    Path(mac): Path<String>,
) -> Result<Json<wakezilla_common::Machine>, (axum::http::StatusCode, Json<serde_json::Value>)> {
    let machines = state.machines.read().await;
    if let Some(machine) = machines.iter().find(|m| m.mac == mac).cloned() {
        Ok(Json(web::machine_to_api_machine(&machine)))
    } else {
        Err((
            axum::http::StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Machine not found" })),
        ))
    }
}

async fn get_access_history_api(
    State(state): State<AppState>,
    Path(mac): Path<String>,
) -> Result<Json<wakezilla_common::AccessHistory>, (axum::http::StatusCode, Json<serde_json::Value>)>
{
    let machines = state.machines.read().await;
    let machine = machines.iter().find(|m| m.mac == mac).cloned();
    drop(machines);

    let Some(machine) = machine else {
        return Err((
            axum::http::StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Machine not found" })),
        ));
    };

    let log = state.access_log.read().await;
    let services = machine
        .port_forwards
        .iter()
        .map(|pf| {
            let key = crate::access_log::service_key(&machine.mac, pf.local_port);
            wakezilla_common::ServiceAccessHistory {
                name: if pf.name.trim().is_empty() {
                    None
                } else {
                    Some(pf.name.clone())
                },
                local_port: pf.local_port,
                target_port: pf.target_port,
                timestamps: log.get(&key),
            }
        })
        .collect();

    Ok(Json(wakezilla_common::AccessHistory { services }))
}

async fn update_machine_api(
    State(state): State<AppState>,
    Path(mac): Path<String>,
    JsonExtract(payload): JsonExtract<wakezilla_common::UpdateMachinePayload>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, Json<serde_json::Value>)> {
    let mut machines = state.machines.write().await;

    let index = machines.iter().position(|m| m.mac == mac).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Machine not found" })),
        )
    })?;
    let old_machine = machines[index].clone();

    let api_machine = wakezilla_common::Machine {
        machine_type: payload.machine_type.unwrap_or(old_machine.machine_type),
        mac: payload.mac.clone(),
        ip: payload.ip.clone(),
        name: payload.name.clone(),
        description: payload.description.clone(),
        turn_off_port: payload.turn_off_port,
        can_be_turned_off: payload.can_be_turned_off,
        inactivity_period: payload
            .inactivity_period
            .unwrap_or(web::get_default_inactivity_period()),
        port_forwards: payload.port_forwards.clone().unwrap_or_default(),
    };
    validate_machine_config(&api_machine, &machines, Some(&mac)).map_err(|errors| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "errors": errors })),
        )
    })?;
    let mut new_machine = match web::api_machine_to_internal(&api_machine) {
        Ok(machine) => machine,
        Err(err) => {
            return Err((
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "errors": { "ip": [err.to_string()] }
                })),
            ));
        }
    };
    apply_shutdown_security(Some(&old_machine), &mut new_machine);

    let mut updated = machines.clone();
    updated[index] = new_machine.clone();
    if let Err(e) = web::save_machines_with_config(&updated, &state.config) {
        error!("Error saving machines: {}", e);
        return Err((
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "Failed to save machines" })),
        ));
    }
    *machines = updated;
    state.turn_off_limiter.remove_machine(&old_machine.mac);

    // Stop old proxies if machine existed
    {
        let mut proxies = state.proxies.write().await;
        let keys_to_stop: Vec<String> = proxies
            .keys()
            .filter(|key| key.starts_with(&mac))
            .cloned()
            .collect();

        for key in keys_to_stop {
            if let Some(tx) = proxies.get(&key) {
                if tx.send(false).is_ok() {
                    info!("Stopped old proxy for key: {}", key);
                }
            }
            proxies.remove(&key);
        }
        drop(proxies);
    }

    // Restart proxy with updated configuration
    web::start_proxy_if_configured(&new_machine, &state);

    // Restart global monitor to pick up configuration changes
    web::restart_global_monitor(&state);

    Ok(Json(serde_json::json!({ "status": "Machine updated" })))
}

async fn delete_machine_api(
    State(state): State<AppState>,
    JsonExtract(payload): JsonExtract<wakezilla_common::DeleteMachinePayload>,
) -> impl IntoResponse {
    let mut machines = state.machines.write().await;
    let removed = machines
        .iter()
        .find(|machine| machine.mac == payload.mac)
        .cloned();
    let updated: Vec<_> = machines
        .iter()
        .filter(|machine| machine.mac != payload.mac)
        .cloned()
        .collect();
    if let Err(e) = web::save_machines_with_config(&updated, &state.config) {
        error!("Error saving machines: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "Failed to save machines" })),
        );
    }
    *machines = updated;
    if let Some(machine) = removed {
        state.turn_off_limiter.remove_machine(&machine.mac);
    }
    let mut proxies = state.proxies.write().await;
    let prefix = format!("{}-", payload.mac);
    proxies.retain(|key, tx| {
        if key.starts_with(&prefix) {
            let _ = tx.send(false);
            false
        } else {
            true
        }
    });

    (
        axum::http::StatusCode::OK,
        Json(serde_json::json!({ "status": "Machine deleted" })),
    )
}

async fn execute_remote_turn_off(state: &AppState, mac: &str) -> (axum::http::StatusCode, String) {
    let machine = {
        let machines = state.machines.read().await;
        machines.iter().find(|m| m.mac == mac).cloned()
    };

    if let Some(machine) = machine {
        if let Some(port) = machine.turn_off_port {
            info!("Sending turn-off request to {}:{}", machine.ip, port);
            match forward::turn_off_remote_machine(
                &machine.ip.to_string(),
                port,
                machine.shutdown_auth_key.as_deref(),
            )
            .await
            {
                Ok(_) => {
                    return (
                        axum::http::StatusCode::OK,
                        format!("Sent turn-off request to {}", mac),
                    );
                }
                Err(e) => {
                    return (
                        axum::http::StatusCode::BAD_GATEWAY,
                        format!("Failed to send turn-off request: {}", e),
                    );
                }
            }
        } else {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("No turn-off port configured for {}", mac),
            );
        }
    }

    (
        axum::http::StatusCode::NOT_FOUND,
        format!("Machine {} not found", mac),
    )
}

async fn api_turn_off_remote_machine(
    State(state): State<AppState>,
    Path(mac): Path<String>,
) -> impl IntoResponse {
    let (status, message) = execute_remote_turn_off(&state, &mac).await;
    (
        status,
        Json(serde_json::json!({
            "message": message,
        })),
    )
}

async fn execute_wake(state: &AppState, mac_input: &str) -> (axum::http::StatusCode, String) {
    let parsed_mac = match wol::parse_mac(mac_input) {
        Ok(mac) => mac,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("Invalid MAC address '{}': {}", mac_input, e),
            );
        }
    };

    let port = state.config.wol.default_port;
    let count = state.config.wol.default_packet_count;
    let broadcast = state.config.get_default_broadcast_addr();

    match crate::wol::send_packets(&parsed_mac, broadcast, port, count, &state.config).await {
        Ok(_) => (
            axum::http::StatusCode::OK,
            format!("Sent WOL packet to {}", mac_input),
        ),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to send WOL packet to {}: {}", mac_input, e),
        ),
    }
}

async fn api_wake_machine(
    State(state): State<AppState>,
    Path(mac): Path<String>,
) -> impl IntoResponse {
    let (status, message) = execute_wake(&state, &mac).await;
    (
        status,
        Json(serde_json::json!({
            "message": message,
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        extract::{Path, State},
        http::{Method, Request, StatusCode},
        response::IntoResponse,
        Json,
    };
    use std::collections::HashMap;
    use std::io::ErrorKind;
    use std::net::Ipv4Addr;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::{watch, Mutex as AsyncMutex, RwLock};

    struct EnvGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvGuard {
        fn set_path(key: &'static str, value: &std::path::Path) -> Self {
            let original = std::env::var(key).ok();
            std::env::set_var(key, value.as_os_str());
            Self { key, original }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(ref original) = self.original {
                std::env::set_var(self.key, original);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    fn state_with_machines(machines: Vec<Machine>) -> AppState {
        let config = crate::config::Config::default();
        let max_access_records = config.storage.max_access_records;
        let state = AppState {
            machines: Arc::new(RwLock::new(machines)),
            proxies: Arc::new(RwLock::new(HashMap::new())),
            config: Arc::new(config),
            turn_off_limiter: Arc::new(forward::TurnOffLimiter::new()),
            monitor_handle: Arc::new(std::sync::Mutex::new(None)),
            access_log: Arc::new(RwLock::new(crate::access_log::AccessLog::new(
                max_access_records,
            ))),
        };
        web::start_global_monitor(&state);
        state
    }

    fn sample_machine() -> Machine {
        Machine {
            mac: "AA:BB:CC:DD:EE:FF".to_string(),
            ip: Ipv4Addr::new(10, 0, 0, 1),
            name: "Sample".to_string(),
            description: Some("Desc".to_string()),
            machine_type: Default::default(),
            turn_off_port: Some(8080),
            can_be_turned_off: false,
            shutdown_auth_key: None,
            shutdown_auth_verified: false,
            inactivity_period: 30,
            port_forwards: vec![],
        }
    }

    fn fwd(local_port: u16, target_port: u16) -> wakezilla_common::PortForward {
        wakezilla_common::PortForward {
            name: None,
            local_port,
            target_port,
        }
    }

    fn candidate_machine(
        mac: &str,
        forwards: Vec<wakezilla_common::PortForward>,
    ) -> wakezilla_common::Machine {
        wakezilla_common::Machine {
            name: "Candidate".to_string(),
            mac: mac.to_string(),
            ip: "10.0.0.2".to_string(),
            port_forwards: forwards,
            ..Default::default()
        }
    }

    fn registered_machine(mac: &str, local_ports: &[u16]) -> Machine {
        let mut machine = sample_machine();
        machine.mac = mac.to_string();
        machine.port_forwards = local_ports
            .iter()
            .map(|port| web::PortForward {
                name: "svc".to_string(),
                local_port: *port,
                target_port: 80,
            })
            .collect();
        machine
    }

    #[test]
    fn validate_machine_config_rejects_duplicate_local_ports_within_one_machine() {
        let machine = candidate_machine("AA:BB:CC:DD:EE:FF", vec![fwd(2020, 80), fwd(2020, 443)]);

        let errors = validate_machine_config(&machine, &[], None).unwrap_err();

        assert_eq!(errors.len(), 1);
        let messages = errors.get("port_forwards").expect("port_forwards error");
        assert!(messages[0].contains("unique local ports"));
    }

    #[test]
    fn validate_machine_config_rejects_local_port_used_by_another_machine() {
        let existing = registered_machine("11:22:33:44:55:66", &[2020]);
        let machine = candidate_machine("AA:BB:CC:DD:EE:FF", vec![fwd(2020, 80)]);

        let errors = validate_machine_config(&machine, &[existing], None).unwrap_err();

        assert_eq!(errors.len(), 1);
        let messages = errors.get("port_forwards").expect("port_forwards error");
        assert!(messages[0].contains("already assigned to another machine"));
    }

    #[test]
    fn validate_machine_config_rejects_new_service_on_port_owned_by_another_machine_when_updating()
    {
        let existing_a = registered_machine("11:22:33:44:55:66", &[2020]);
        let existing_b = registered_machine("AA:BB:CC:DD:EE:FF", &[3000]);
        let machine = candidate_machine("AA:BB:CC:DD:EE:FF", vec![fwd(3000, 80), fwd(2020, 80)]);

        let errors = validate_machine_config(
            &machine,
            &[existing_a, existing_b],
            Some("AA:BB:CC:DD:EE:FF"),
        )
        .unwrap_err();

        assert_eq!(errors.len(), 1);
        let messages = errors.get("port_forwards").expect("port_forwards error");
        assert!(messages[0].contains("already assigned to another machine"));
    }

    #[test]
    fn validate_machine_config_accepts_unique_ports_across_machines() {
        let existing = registered_machine("11:22:33:44:55:66", &[2020]);
        let machine = candidate_machine("AA:BB:CC:DD:EE:FF", vec![fwd(3000, 80)]);

        assert!(validate_machine_config(&machine, &[existing], None).is_ok());
    }

    #[test]
    fn validate_machine_config_ignores_ports_of_machine_being_replaced() {
        let existing_a = registered_machine("11:22:33:44:55:66", &[2020]);
        let existing_b = registered_machine("AA:BB:CC:DD:EE:FF", &[3000]);
        let machine = candidate_machine("AA:BB:CC:DD:EE:FF", vec![fwd(3000, 80)]);

        assert!(validate_machine_config(
            &machine,
            &[existing_a, existing_b],
            Some("AA:BB:CC:DD:EE:FF"),
        )
        .is_ok());
    }

    #[tokio::test]
    async fn get_machine_details_api_returns_not_found() {
        let state = state_with_machines(vec![]);
        let result =
            get_machine_details_api(State(state), Path("AA:BB:CC:DD:EE:FF".to_string())).await;
        let (status, body) = result.expect_err("expected missing machine");
        assert_eq!(status, StatusCode::NOT_FOUND);
        let json = body.0;
        assert_eq!(json["error"], "Machine not found");
    }

    #[tokio::test]
    async fn execute_remote_turn_off_handles_missing_machine() {
        let state = state_with_machines(vec![]);
        let (status, message) = execute_remote_turn_off(&state, "AA:BB:CC:DD:EE:FF").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(message.contains("not found"));
    }

    #[tokio::test]
    async fn execute_remote_turn_off_requires_port() {
        let mut machine = sample_machine();
        machine.turn_off_port = None;
        let state = state_with_machines(vec![machine]);
        let (status, message) = execute_remote_turn_off(&state, "AA:BB:CC:DD:EE:FF").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(message.contains("No turn-off port"));
    }

    #[tokio::test]
    async fn api_turn_off_remote_machine_returns_json_message() {
        let state = state_with_machines(vec![]);
        let response =
            api_turn_off_remote_machine(State(state), Path("AA:BB:CC:DD:EE:FF".to_string()))
                .await
                .into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body_bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body to be readable");
        let json: serde_json::Value =
            serde_json::from_slice(&body_bytes).expect("response to be valid json");
        assert!(
            json["message"]
                .as_str()
                .unwrap_or_default()
                .contains("not found"),
            "expected message to mention missing machine"
        );
    }

    #[tokio::test]
    // ENV_LOCK serializes mutation of process env vars across these tests; it must be held
    // across the awaited handler call, so the sync guard intentionally spans the await point.
    #[allow(clippy::await_holding_lock)]
    async fn add_machine_api_persists_new_entry() {
        let _lock = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp_dir = tempdir().expect("failed to create temp dir");
        let file_path = tmp_dir.path().join("machines.json");
        let _guard = EnvGuard::set_path("WAKEZILLA__STORAGE__MACHINES_DB_PATH", &file_path);

        let state = state_with_machines(vec![]);
        let form = wakezilla_common::AddMachinePayload {
            mac: "AA:BB:CC:DD:EE:FF".to_string(),
            ip: "192.168.1.10".to_string(),
            name: "New machine".to_string(),
            description: Some("Test machine".to_string()),
            machine_type: Default::default(),
            turn_off_port: Some(8080),
            can_be_turned_off: true,
            inactivity_period: Some(6),
            port_forwards: None,
        };

        let response = add_machine_api(State(state.clone()), Json(form))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::CREATED);

        let machines = state.machines.read().await;
        assert_eq!(machines.len(), 1);
        assert_eq!(machines[0].mac, "AA:BB:CC:DD:EE:FF");
        assert_eq!(machines[0].name, "New machine");
        assert_eq!(machines[0].inactivity_period, 6);
        assert!(machines[0].shutdown_auth_key.is_some());
        assert!(!machines[0].shutdown_auth_verified);
    }

    #[test]
    fn pending_shutdown_setup_contains_platform_commands() {
        let mut machine = sample_machine();
        machine.can_be_turned_off = true;
        machine.shutdown_auth_key = Some(crate::shutdown_auth::generate_key());

        let setup = shutdown_setup_for_machine(&machine);

        assert_eq!(setup.status, wakezilla_common::ShutdownSetupStatus::Pending);
        assert!(setup
            .unix_command
            .unwrap()
            .starts_with("sudo wakezilla setup --mode client --port 8080 --key "));
        assert!(setup
            .windows_command
            .unwrap()
            .starts_with("wakezilla setup --mode client --port 8080 --key "));
    }

    #[test]
    fn verified_shutdown_setup_does_not_expose_commands() {
        let mut machine = sample_machine();
        machine.can_be_turned_off = true;
        machine.shutdown_auth_key = Some(crate::shutdown_auth::generate_key());
        machine.shutdown_auth_verified = true;

        let setup = shutdown_setup_for_machine(&machine);

        assert_eq!(
            setup.status,
            wakezilla_common::ShutdownSetupStatus::Verified
        );
        assert_eq!(setup.unix_command, None);
        assert_eq!(setup.windows_command, None);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn verify_shutdown_setup_persists_first_success() {
        let _lock = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp_dir = tempdir().unwrap();
        let file_path = tmp_dir.path().join("machines.json");
        let _guard = EnvGuard::set_path("WAKEZILLA__STORAGE__MACHINES_DB_PATH", &file_path);

        let key = crate::shutdown_auth::generate_key();
        let key_for_server = key.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                crate::client_server::build_router(Some(key_for_server)),
            )
            .await
            .unwrap();
        });

        let mut machine = sample_machine();
        machine.ip = addr.ip().to_string().parse().unwrap();
        machine.turn_off_port = Some(addr.port());
        machine.can_be_turned_off = true;
        machine.shutdown_auth_key = Some(key);
        let state = state_with_machines(vec![machine]);

        let response =
            verify_shutdown_setup(State(state.clone()), Path("AA:BB:CC:DD:EE:FF".to_string()))
                .await
                .expect("verification should succeed");

        assert_eq!(
            response.0.status,
            wakezilla_common::ShutdownSetupStatus::Verified
        );
        assert!(state.machines.read().await[0].shutdown_auth_verified);
        server.abort();
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn rotate_shutdown_key_replaces_key_and_marks_pending() {
        let _lock = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp_dir = tempdir().unwrap();
        let file_path = tmp_dir.path().join("machines.json");
        let _guard = EnvGuard::set_path("WAKEZILLA__STORAGE__MACHINES_DB_PATH", &file_path);

        let mut machine = sample_machine();
        machine.can_be_turned_off = true;
        machine.shutdown_auth_key = Some(crate::shutdown_auth::generate_key());
        machine.shutdown_auth_verified = true;
        let original_key = machine.shutdown_auth_key.clone();
        let state = state_with_machines(vec![machine]);

        let response =
            rotate_shutdown_key(State(state.clone()), Path("AA:BB:CC:DD:EE:FF".to_string()))
                .await
                .expect("rotation should succeed");

        assert_eq!(
            response.0.status,
            wakezilla_common::ShutdownSetupStatus::Pending
        );
        let machines = state.machines.read().await;
        assert_ne!(machines[0].shutdown_auth_key, original_key);
        assert!(!machines[0].shutdown_auth_verified);
    }

    #[tokio::test]
    async fn rotate_shutdown_key_rolls_back_when_persistence_fails() {
        let mut machine = sample_machine();
        machine.can_be_turned_off = true;
        machine.shutdown_auth_key = Some(crate::shutdown_auth::generate_key());
        machine.shutdown_auth_verified = true;
        let original_key = machine.shutdown_auth_key.clone();
        let mut state = state_with_machines(vec![machine]);
        let directory = tempdir().unwrap();
        Arc::make_mut(&mut state.config).storage.machines_db_path =
            directory.path().to_string_lossy().into_owned();

        let result =
            rotate_shutdown_key(State(state.clone()), Path("AA:BB:CC:DD:EE:FF".to_string())).await;

        assert!(result.is_err());
        let machines = state.machines.read().await;
        assert_eq!(machines[0].shutdown_auth_key, original_key);
        assert!(machines[0].shutdown_auth_verified);
    }

    #[tokio::test]
    async fn verify_shutdown_setup_rolls_back_when_persistence_fails() {
        let key = crate::shutdown_auth::generate_key();
        let key_for_server = key.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                crate::client_server::build_router(Some(key_for_server)),
            )
            .await
            .unwrap();
        });

        let mut machine = sample_machine();
        machine.ip = addr.ip().to_string().parse().unwrap();
        machine.turn_off_port = Some(addr.port());
        machine.can_be_turned_off = true;
        machine.shutdown_auth_key = Some(key);
        let mut state = state_with_machines(vec![machine]);
        let directory = tempdir().unwrap();
        Arc::make_mut(&mut state.config).storage.machines_db_path =
            directory.path().to_string_lossy().into_owned();

        let result =
            verify_shutdown_setup(State(state.clone()), Path("AA:BB:CC:DD:EE:FF".to_string()))
                .await;

        assert!(result.is_err());
        assert!(!state.machines.read().await[0].shutdown_auth_verified);
        server.abort();
    }

    #[tokio::test]
    async fn add_machine_api_returns_errors_for_invalid_payload() {
        let state = state_with_machines(vec![]);
        let form = wakezilla_common::AddMachinePayload {
            mac: "invalid".to_string(),
            ip: "not-an-ip".to_string(),
            name: "Bad".to_string(),
            description: None,
            machine_type: Default::default(),
            turn_off_port: None,
            can_be_turned_off: false,
            inactivity_period: None,
            port_forwards: None,
        };

        let response = add_machine_api(State(state.clone()), Json(form))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(state.machines.read().await.is_empty());
    }

    #[tokio::test]
    async fn execute_remote_turn_off_sends_request_when_configured() {
        let listener = match TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                eprintln!(
                    "skipping turn_off_remote_machine_sends_request_when_configured: {}",
                    err
                );
                return;
            }
            Err(err) => panic!("failed to bind listener: {err}"),
        };
        let addr = listener.local_addr().expect("failed to get addr");
        let received = Arc::new(AsyncMutex::new(None));
        let received_clone = received.clone();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = vec![0u8; 1024];
                if let Ok(n) = socket.read(&mut buf).await {
                    if n > 0 {
                        let request = String::from_utf8_lossy(&buf[..n]).to_string();
                        *received_clone.lock().await = Some(request);
                    }
                }
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                    .await;
            }
        });

        let mut machine = sample_machine();
        machine.turn_off_port = Some(addr.port());
        machine.ip = addr.ip().to_string().parse().unwrap();
        let state = state_with_machines(vec![machine]);

        let (status, message) = execute_remote_turn_off(&state, "AA:BB:CC:DD:EE:FF").await;

        assert_eq!(status, StatusCode::OK);
        assert!(message.contains("Sent turn-off request"));
        let request = received
            .lock()
            .await
            .clone()
            .expect("expected request to be captured");
        assert!(request.starts_with("POST /machines/turn-off"));
    }

    #[tokio::test]
    async fn execute_wake_rejects_invalid_mac() {
        let state = state_with_machines(vec![]);
        let (status, message) = execute_wake(&state, "invalid").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(message.contains("Invalid MAC address"));
    }

    #[tokio::test]
    async fn api_wake_machine_returns_json_for_invalid_mac() {
        let state = state_with_machines(vec![]);
        let response = api_wake_machine(State(state), Path("invalid".to_string()))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body_bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body to be readable");
        let json: serde_json::Value =
            serde_json::from_slice(&body_bytes).expect("response to be valid json");
        assert!(json["message"]
            .as_str()
            .unwrap_or_default()
            .contains("Invalid MAC"));
    }

    #[tokio::test]
    // ENV_LOCK serializes mutation of process env vars across these tests; it must be held
    // across the awaited handler call, so the sync guard intentionally spans the await point.
    #[allow(clippy::await_holding_lock)]
    async fn update_machine_api_applies_changes() {
        let _lock = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp_dir = tempdir().expect("failed to create temp dir");
        let file_path = tmp_dir.path().join("machines.json");
        let _guard = EnvGuard::set_path("WAKEZILLA__STORAGE__MACHINES_DB_PATH", &file_path);

        let state = state_with_machines(vec![sample_machine()]);
        let payload = wakezilla_common::UpdateMachinePayload {
            mac: "AA:BB:CC:DD:EE:FF".to_string(),
            ip: "10.0.0.2".to_string(),
            name: "Updated".to_string(),
            description: Some("New description".to_string()),
            machine_type: Default::default(),
            turn_off_port: Some(9090),
            can_be_turned_off: true,
            inactivity_period: Some(12),
            port_forwards: Some(vec![]),
        };

        let response = update_machine_api(
            State(state.clone()),
            Path("AA:BB:CC:DD:EE:FF".to_string()),
            Json(payload),
        )
        .await
        .expect("update should succeed")
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);

        let machines = state.machines.read().await;
        let updated = machines.first().expect("machine should exist");
        assert_eq!(updated.name, "Updated");
        assert_eq!(updated.description.as_deref(), Some("New description"));
        assert!(updated.can_be_turned_off);
        assert_eq!(updated.inactivity_period, 12);
        assert_eq!(updated.turn_off_port, Some(9090));
        assert_eq!(updated.ip, Ipv4Addr::new(10, 0, 0, 2));
        assert!(updated.shutdown_auth_key.is_some());
        assert!(!updated.shutdown_auth_verified);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn update_machine_preserves_verified_key_when_connection_details_do_not_change() {
        let _lock = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp_dir = tempdir().unwrap();
        let file_path = tmp_dir.path().join("machines.json");
        let _guard = EnvGuard::set_path("WAKEZILLA__STORAGE__MACHINES_DB_PATH", &file_path);

        let mut machine = sample_machine();
        machine.can_be_turned_off = true;
        machine.shutdown_auth_key = Some(crate::shutdown_auth::generate_key());
        machine.shutdown_auth_verified = true;
        let original_key = machine.shutdown_auth_key.clone();
        let state = state_with_machines(vec![machine]);
        let payload = wakezilla_common::UpdateMachinePayload {
            mac: "AA:BB:CC:DD:EE:FF".to_string(),
            ip: "10.0.0.1".to_string(),
            name: "Renamed".to_string(),
            description: None,
            machine_type: Default::default(),
            turn_off_port: Some(8080),
            can_be_turned_off: true,
            inactivity_period: Some(30),
            port_forwards: Some(vec![]),
        };

        let _ = update_machine_api(
            State(state.clone()),
            Path("AA:BB:CC:DD:EE:FF".to_string()),
            Json(payload),
        )
        .await
        .unwrap();

        let machines = state.machines.read().await;
        assert_eq!(machines[0].shutdown_auth_key, original_key);
        assert!(machines[0].shutdown_auth_verified);
    }

    #[tokio::test]
    // ENV_LOCK serializes mutation of process env vars across these tests; it must be held
    // across the awaited handler call, so the sync guard intentionally spans the await point.
    #[allow(clippy::await_holding_lock)]
    async fn delete_machine_api_stops_proxy_and_removes_machine() {
        let _lock = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp_dir = tempdir().expect("failed to create temp dir");
        let file_path = tmp_dir.path().join("machines.json");
        let _guard = EnvGuard::set_path("WAKEZILLA__STORAGE__MACHINES_DB_PATH", &file_path);

        let machine = sample_machine();
        let state = state_with_machines(vec![machine.clone()]);

        {
            let mut proxies = state.proxies.write().await;
            let (tx, _rx) = watch::channel(true);
            proxies.insert(format!("{}-proxy", machine.mac), tx);
        }

        let response = delete_machine_api(
            State(state.clone()),
            Json(wakezilla_common::DeleteMachinePayload {
                mac: machine.mac.clone(),
            }),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);

        assert!(state.machines.read().await.is_empty());
        assert!(state.proxies.read().await.is_empty());
    }

    #[tokio::test]
    async fn spa_fallback_serves_index_for_client_routes() {
        let request = Request::builder()
            .method(Method::GET)
            .uri("/dashboard/settings")
            .body(Body::empty())
            .unwrap();

        let response = spa_fallback(request).await;
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers().clone();
        assert_eq!(
            headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/html")
        );
    }

    #[tokio::test]
    async fn spa_fallback_returns_not_found_for_post_requests() {
        let request = Request::builder()
            .method(Method::POST)
            .uri("/dashboard/settings")
            .body(Body::empty())
            .unwrap();

        let response = spa_fallback(request).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
