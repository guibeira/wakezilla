use anyhow::Result;
use axum::{
    body::Body,
    extract::{Json as JsonExtract, Path, Query, State},
    http::{header, Method, Request, Response, StatusCode},
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

async fn serve_index() -> Response<Body> {
    // if debug build, redirect to vite dev server (localhost:3000)
    if cfg!(debug_assertions) {
        return Redirect::to("http://localhost:8080").into_response();
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
    let port = config.server.proxy_port;
    let initial_machines = match web::load_machines() {
        Ok(machines) => machines,
        Err(err) => {
            error!("Failed to load machines from storage: {err}");
            Vec::new()
        }
    };

    let max_access_records = config.storage.max_access_records;
    let state = AppState {
        machines: Arc::new(RwLock::new(initial_machines.clone())),
        proxies: Arc::new(RwLock::new(HashMap::new())),
        config: Arc::new(config),
        turn_off_limiter: Arc::new(forward::TurnOffLimiter::new()),
        monitor_handle: Arc::new(std::sync::Mutex::new(None)),
        access_log: Arc::new(RwLock::new(crate::access_log::AccessLog::load(
            max_access_records,
        ))),
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
        .with_graceful_shutdown(shutdown_signal())
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
        .route("/api/machines/delete", delete(delete_machine_api))
        .with_state(state)
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

    match reqwest::get(&url).await {
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
            Err(axum::http::StatusCode::NOT_FOUND)
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
    let new_machine = match web::api_machine_to_internal(&api_machine) {
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

    let mut machines = state.machines.write().await;
    web::start_proxy_if_configured(&new_machine, &state);
    machines.push(new_machine);

    if let Err(e) = web::save_machines(&machines) {
        error!("Error saving machines: {}", e);
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "Failed to save machines" })),
        );
    }
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

    // check if the machine exists
    let exists = machines.iter().any(|m| m.mac == mac);
    if !exists {
        return Err((
            axum::http::StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Machine not found" })),
        ));
    }
    // Find the old machine to get its IP for stopping proxies
    let old_machine = machines.iter().find(|m| m.mac == mac).cloned();

    // remove the machine to update
    machines.retain(|m| m.mac != mac);

    let api_machine = wakezilla_common::Machine {
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
    let new_machine = match web::api_machine_to_internal(&api_machine) {
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

    machines.push(new_machine.clone());
    if let Err(e) = web::save_machines(&machines) {
        error!("Error saving machines: {}", e);
        return Err((
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "Failed to save machines" })),
        ));
    }

    // Stop old proxies if machine existed
    if old_machine.is_some() {
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
    // Stop all proxies associated with this machine
    info!("Deleting machine with MAC: {}", payload.mac);
    let mut proxies = state.proxies.write().await;
    proxies.retain(|key, tx| {
        if key.starts_with(&payload.mac) {
            if tx.send(false).is_ok() {
                info!("Sent stop signal to proxy for MAC/key: {}", key);
            }
            false // Remove the entry
        } else {
            true // Keep the entry
        }
    });
    drop(proxies); // Release the write lock

    let mut machines = state.machines.write().await;

    machines.retain(|m| m.mac != payload.mac);

    if let Err(e) = web::save_machines(&machines) {
        error!("Error saving machines: {}", e);
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "Failed to save machines" })),
        );
    }
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
            match forward::turn_off_remote_machine(&machine.ip.to_string(), port).await {
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
            turn_off_port: Some(8080),
            can_be_turned_off: false,
            inactivity_period: 30,
            port_forwards: vec![],
        }
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
    }

    #[tokio::test]
    async fn add_machine_api_returns_errors_for_invalid_payload() {
        let state = state_with_machines(vec![]);
        let form = wakezilla_common::AddMachinePayload {
            mac: "invalid".to_string(),
            ip: "not-an-ip".to_string(),
            name: "Bad".to_string(),
            description: None,
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
