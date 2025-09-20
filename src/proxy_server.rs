use crate::connection_pool::ConnectionPool;
use anyhow::Result;
use askama_axum::Template;
use axum::{
    extract::{Form, Query, State},
    response::{IntoResponse, Json, Redirect},
    routing::{get, post},
    Router,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::{net::TcpListener, sync::RwLock};
use tracing::{debug, error, info};
use validator::Validate;

use crate::forward;
use crate::scanner;
use crate::web::{self, AppState, DeleteForm, Machine, RemoteTurnOffForm, WakeForm};
use crate::wol;

pub async fn start(port: u16) -> Result<()> {
    let initial_machines = web::load_machines().unwrap_or_default();

    // Create connection pool and start cleanup task
    let connection_pool = ConnectionPool::new();
    let cleanup_handle = connection_pool.start_cleanup_task();

    // Spawn cleanup task
    tokio::spawn(async move {
        cleanup_handle.await.ok();
    });

    let state = AppState {
        machines: Arc::new(RwLock::new(initial_machines.clone())),
        proxies: Arc::new(RwLock::new(HashMap::new())),
        connection_pool,
    };

    for machine in &initial_machines {
        web::start_proxy_if_configured(machine, &state);
    }

    let app = build_router(state.clone());

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(addr).await?;
    info!("listening on http://{}", listener.local_addr()?);
    axum::serve(listener, app).await?;

    Ok(())
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(show_machines))
        .route("/scan", get(scan_network_handler))
        .route("/interfaces", get(list_interfaces_handler))
        .route("/machines", post(add_machine))
        .route("/machines/delete", post(delete_machine))
        .route("/machines/health", post(machine_health))
        .route("/machines/add-port-forward", post(add_port_forward))
        .route("/machines/:mac", get(machine_detail))
        .route("/machines/update-ports", post(update_ports))
        .route("/machines/update-config", post(update_machine_config))
        .route("/machines/remote-turn-off", post(turn_off_remote_machine))
        .route("/wol", post(wake_machine))
        .with_state(state)
}

type FormErrors = HashMap<String, Vec<String>>;

#[derive(Template)]
#[template(path = "machines.html")]
struct MachinesTemplate {
    machines: Vec<Machine>,
    errors: FormErrors,
    form: Option<web::AddMachineForm>,
}

#[derive(Template)]
#[template(path = "machine_detail.html")]
struct MachineDetailTemplate {
    machine: Machine,
}

async fn scan_network_handler(Query(params): Query<HashMap<String, String>>) -> impl IntoResponse {
    let interface = params.get("interface").map(|s| s.as_str());
    match scanner::scan_network_with_interface(interface).await {
        Ok(devices) => Ok(Json(devices)),
        Err(e) => {
            error!("Network scan failed: {}", e);
            Err(axum::http::StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn list_interfaces_handler() -> impl IntoResponse {
    match scanner::list_interfaces().await {
        Ok(interfaces) => Ok(Json(interfaces)),
        Err(e) => {
            error!("Failed to list interfaces: {}", e);
            Err(axum::http::StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

use axum::extract::Path;

async fn machine_detail(
    State(state): State<AppState>,
    Path(mac): Path<String>,
) -> impl IntoResponse {
    let machines = state.machines.read().await;
    if let Some(machine) = machines.iter().find(|m| m.mac == mac).cloned() {
        MachineDetailTemplate { machine }.into_response()
    } else {
        axum::http::StatusCode::NOT_FOUND.into_response()
    }
}

async fn show_machines(State(state): State<AppState>) -> impl IntoResponse {
    let machines = state.machines.read().await.clone();
    MachinesTemplate {
        machines,
        errors: HashMap::new(),
        form: None,
    }
}

async fn add_machine(
    State(state): State<AppState>,
    Form(form): Form<web::AddMachineForm>,
) -> impl IntoResponse {
    if let Err(errors) = form.validate() {
        let errors_map = errors
            .field_errors()
            .iter()
            .map(|(key, value)| {
                let error_messages: Vec<String> =
                    value.iter().map(|error| error.code.to_string()).collect();
                (key.to_string(), error_messages)
            })
            .collect();

        let machines = state.machines.read().await.clone();
        return MachinesTemplate {
            machines,
            form: Some(form),
            errors: errors_map,
        }
        .into_response();
    }

    let new_machine = Machine {
        mac: form.mac,
        ip: form.ip.parse().expect("Invalid IP address"),
        name: form.name,
        description: form.description,
        turn_off_port: form.turn_off_port,
        can_be_turned_off: form.can_be_turned_off,
        request_rate: web::RequestRateConfig {
            max_requests: form.requests_per_hour.unwrap_or(1000),
            period_minutes: form.period_minutes.unwrap_or(60),
        },
        port_forwards: Vec::new(),
    };

    let mut machines = state.machines.write().await;
    machines.push(new_machine);

    if let Err(e) = web::save_machines(&machines) {
        error!("Error saving machines: {}", e);
    }

    Redirect::to("/").into_response()
}

async fn delete_machine(
    State(state): State<AppState>,
    Form(payload): Form<DeleteForm>,
) -> Redirect {
    // Stop all proxies associated with this machine
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

    // Remove connections from pool for this machine's IP
    if let Some(machine) = machines.iter().find(|m| m.mac == payload.mac) {
        let target_addr = SocketAddr::from((machine.ip, 0));
        state.connection_pool.remove_target(target_addr).await;
        debug!("Removed connections from pool for machine {}", machine.ip);
    }

    machines.retain(|m| m.mac != payload.mac);
    if let Err(e) = web::save_machines(&machines) {
        error!("Error saving machines: {}", e);
    }
    Redirect::to("/")
}

async fn turn_off_remote_machine(
    State(state): State<AppState>,
    Form(payload): Form<RemoteTurnOffForm>,
) -> (axum::http::StatusCode, String) {
    let machine = {
        let machines = state.machines.read().await;
        machines.iter().find(|m| m.mac == payload.mac).cloned()
    };

    if let Some(machine) = machine {
        if let Some(port) = machine.turn_off_port {
            info!("Sending turn-off request to {}:{}", machine.ip, port);
            match forward::turn_off_remote_machine(&machine.ip.to_string(), port).await {
                Ok(_) => {
                    return (
                        axum::http::StatusCode::OK,
                        format!("Sent turn-off request to {}", payload.mac),
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
                format!("No turn-off port configured for {}", payload.mac),
            );
        }
    }

    (
        axum::http::StatusCode::NOT_FOUND,
        format!("Machine {} not found", payload.mac),
    )
}

async fn update_machine_config(
    State(state): State<AppState>,
    Form(payload): Form<std::collections::HashMap<String, String>>,
) -> Redirect {
    let mac = payload.get("mac").cloned().unwrap_or_default();
    let mut machines = state.machines.write().await;
    if let Some(machine) = machines.iter_mut().find(|m| m.mac == mac) {
        if let Some(new_name) = payload.get("name") {
            machine.name = new_name.clone();
        }
        machine.description = payload.get("description").and_then(|v| {
            if v.trim().is_empty() {
                None
            } else {
                Some(v.clone())
            }
        });
        // If the box is present in the form (checked), the value is "true".
        machine.can_be_turned_off = payload.contains_key("can_be_turned_off");
        if let Some(port_str) = payload.get("turn_off_port") {
            if let Ok(port) = port_str.parse::<u16>() {
                machine.turn_off_port = Some(port);
            } else {
                machine.turn_off_port = None;
            }
        } else {
            machine.turn_off_port = None;
        }
        if let Some(rph) = payload.get("requests_per_hour") {
            if let Ok(num) = rph.parse() {
                machine.request_rate.max_requests = num;
            }
        }
        if let Some(pm) = payload.get("period_minutes") {
            if let Ok(num) = pm.parse() {
                if num > 0 {
                    machine.request_rate.period_minutes = num;
                }
            }
        }
        let _ = web::save_machines(&machines);
        return Redirect::to(&format!("/machines/{}", mac));
    }
    Redirect::to("/")
}

async fn update_ports(
    State(state): State<AppState>,
    Form(payload): Form<std::collections::HashMap<String, String>>,
) -> Redirect {
    let mac = payload.get("mac").cloned().unwrap_or_default();
    let mut machines = state.machines.write().await;
    if let Some(machine) = machines.iter_mut().find(|m| m.mac == mac) {
        let mut ports = Vec::new();
        let mut idx = 0;
        loop {
            let name_key = format!("pf_name_{}", idx);
            let local_key = format!("pf_local_{}", idx);
            let target_key = format!("pf_target_{}", idx);
            match (
                payload.get(&name_key),
                payload.get(&local_key),
                payload.get(&target_key),
            ) {
                (Some(name), Some(local), Some(target)) => {
                    let remove_key = format!("pf_remove_{}", idx);
                    let remove_checked = payload.contains_key(&remove_key);
                    if !remove_checked
                        && !name.trim().is_empty()
                        && !local.trim().is_empty()
                        && !target.trim().is_empty()
                    {
                        if let (Ok(local), Ok(target)) = (local.parse(), target.parse()) {
                            ports.push(web::PortForward {
                                name: name.clone(),
                                local_port: local,
                                target_port: target,
                            });
                        }
                    }
                }
                _ => break,
            }
            idx += 1;
        }
        // Add new port if submitted
        if let (Some(name), Some(local), Some(target)) = (
            payload.get("pf_name_new"),
            payload.get("pf_local_new"),
            payload.get("pf_target_new"),
        ) {
            if !name.trim().is_empty() && !local.trim().is_empty() && !target.trim().is_empty() {
                if let (Ok(local), Ok(target)) = (local.parse(), target.parse()) {
                    ports.push(web::PortForward {
                        name: name.clone(),
                        local_port: local,
                        target_port: target,
                    });
                }
            }
        }
        machine.port_forwards = ports;
        let machine_clone = machine.clone();
        let _ = web::save_machines(&machines);
        // Remove proxies for this mac
        let mut proxies = state.proxies.write().await;
        proxies.retain(|key, tx| {
            let should_remove = key.starts_with(&mac);
            if should_remove {
                let _ = tx.send(false);
            }
            !should_remove
        });
        drop(proxies); // Release the write lock
                       // Restart with new ports
        web::start_proxy_if_configured(&machine_clone, &state);
        return Redirect::to(&format!("/machines/{}", mac));
    }
    Redirect::to("/")
}

async fn add_port_forward(
    State(state): State<AppState>,
    Form(payload): Form<web::AddPortForwardForm>,
) -> Redirect {
    let mut machines = state.machines.write().await;
    if let Some(machine) = machines.iter_mut().find(|m| m.mac == payload.mac) {
        let new_port_forward = web::PortForward {
            name: payload.name,
            local_port: payload.local_port,
            target_port: payload.target_port,
        };
        machine.port_forwards.push(new_port_forward.clone());

        let machine_clone_for_proxy = machine.clone();

        if let Err(e) = web::save_machines(&machines) {
            error!("Error saving machines: {}", e);
        }

        drop(machines); // Release the write lock

        // Start the new proxy
        web::start_proxy_if_configured(&machine_clone_for_proxy, &state);
    }
    Redirect::to("/")
}

async fn wake_machine(Form(payload): Form<WakeForm>) -> (axum::http::StatusCode, String) {
    let mac = match wol::parse_mac(&payload.mac) {
        Ok(mac) => mac,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("Invalid MAC address '{}': {}", payload.mac, e),
            );
        }
    };

    let bcast = std::net::Ipv4Addr::new(255, 255, 255, 255);
    let port = 9; // Default WOL port
    let count = 3;

    match crate::wol::send_packets(&mac, bcast, port, count, &Default::default()).await {
        Ok(_) => (
            axum::http::StatusCode::OK,
            format!("Sent WOL packet to {}", payload.mac),
        ),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to send WOL packet: {}", e),
        ),
    }
}

#[derive(serde::Deserialize)]
struct MachineHealthRequest {
    ip: String,
    turn_off_port: Option<u16>,
}

async fn machine_health(Json(payload): Json<MachineHealthRequest>) -> Json<serde_json::Value> {
    if let Some(turn_off_port) = payload.turn_off_port {
        // Make a request to the machine's health endpoint
        match reqwest::Client::new()
            .get(format!("http://{}:{}/health", payload.ip, turn_off_port))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
        {
            Ok(response) => {
                if response.status().is_success() {
                    Json(serde_json::json!({
                        "status": "online",
                        "http_status": response.status().as_u16()
                    }))
                } else {
                    Json(serde_json::json!({
                        "status": "offline",
                        "http_status": response.status().as_u16()
                    }))
                }
            }
            Err(_) => Json(serde_json::json!({
                "status": "offline",
                "http_status": 0
            })),
        }
    } else {
        Json(serde_json::json!({
            "status": "unknown",
            "message": "No turn-off port configured"
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ENV_LOCK;
    use axum::extract::Form;
    use axum::extract::Path;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::Json;
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
        AppState {
            machines: Arc::new(RwLock::new(machines)),
            proxies: Arc::new(RwLock::new(HashMap::new())),
            connection_pool: ConnectionPool::new(),
        }
    }

    fn sample_machine() -> Machine {
        Machine {
            mac: "AA:BB:CC:DD:EE:FF".to_string(),
            ip: Ipv4Addr::new(10, 0, 0, 1),
            name: "Sample".to_string(),
            description: Some("Desc".to_string()),
            turn_off_port: Some(8080),
            can_be_turned_off: false,
            request_rate: web::RequestRateConfig {
                max_requests: 5,
                period_minutes: 30,
            },
            port_forwards: vec![],
        }
    }

    #[tokio::test]
    async fn machine_detail_returns_not_found_when_missing() {
        let state = state_with_machines(vec![]);
        let response = machine_detail(State(state), Path("AA:BB:CC:DD:EE:FF".to_string()))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn turn_off_remote_machine_handles_missing_machine() {
        let state = state_with_machines(vec![]);
        let (status, message) = turn_off_remote_machine(
            State(state),
            Form(RemoteTurnOffForm {
                mac: "AA:BB:CC:DD:EE:FF".to_string(),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(message.contains("not found"));
    }

    #[tokio::test]
    async fn turn_off_remote_machine_requires_port() {
        let mut machine = sample_machine();
        machine.turn_off_port = None;
        let state = state_with_machines(vec![machine]);
        let (status, message) = turn_off_remote_machine(
            State(state),
            Form(RemoteTurnOffForm {
                mac: "AA:BB:CC:DD:EE:FF".to_string(),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(message.contains("No turn-off port"));
    }

    #[tokio::test]
    async fn add_machine_persists_new_entry() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp_dir = tempdir().expect("failed to create temp dir");
        let file_path = tmp_dir.path().join("machines.json");
        let _guard = EnvGuard::set_path("WAKEZILLA_MACHINES_DB_PATH", &file_path);

        let state = state_with_machines(vec![]);
        let form = web::AddMachineForm {
            mac: "AA:BB:CC:DD:EE:FF".to_string(),
            ip: "192.168.1.10".to_string(),
            name: "New machine".to_string(),
            description: Some("Test machine".to_string()),
            turn_off_port: Some(8080),
            can_be_turned_off: true,
            requests_per_hour: Some(12),
            period_minutes: Some(6),
        };

        let response = add_machine(State(state.clone()), Form(form)).await;
        assert_eq!(response.into_response().status(), StatusCode::SEE_OTHER);

        let machines = state.machines.read().await;
        assert_eq!(machines.len(), 1);
        assert_eq!(machines[0].mac, "AA:BB:CC:DD:EE:FF");
        assert_eq!(machines[0].name, "New machine");
        assert_eq!(machines[0].request_rate.max_requests, 12);
        assert_eq!(machines[0].request_rate.period_minutes, 6);
    }

    #[tokio::test]
    async fn add_machine_returns_errors_for_invalid_payload() {
        let state = state_with_machines(vec![]);
        let form = web::AddMachineForm {
            mac: "invalid".to_string(),
            ip: "not-an-ip".to_string(),
            name: "Bad".to_string(),
            description: None,
            turn_off_port: None,
            can_be_turned_off: false,
            requests_per_hour: None,
            period_minutes: None,
        };

        let response = add_machine(State(state.clone()), Form(form)).await;
        assert_eq!(response.into_response().status(), StatusCode::OK);
        assert!(state.machines.read().await.is_empty());
    }

    #[tokio::test]
    async fn turn_off_remote_machine_sends_request_when_configured() {
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

        let (status, message) = turn_off_remote_machine(
            State(state),
            Form(RemoteTurnOffForm {
                mac: "AA:BB:CC:DD:EE:FF".to_string(),
            }),
        )
        .await;

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
    async fn wake_machine_rejects_invalid_mac() {
        let (status, message) = wake_machine(Form(WakeForm {
            mac: "invalid".to_string(),
        }))
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(message.contains("Invalid MAC address"));
    }

    #[tokio::test]
    async fn machine_health_reports_unknown_without_port() {
        let Json(value) = machine_health(Json(MachineHealthRequest {
            ip: "127.0.0.1".to_string(),
            turn_off_port: None,
        }))
        .await;
        assert_eq!(value["status"], "unknown");
    }

    #[tokio::test]
    async fn machine_health_reports_offline_with_error_status() {
        let listener = match TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                eprintln!(
                    "skipping machine_health_reports_offline_with_error_status: {}",
                    err
                );
                return;
            }
            Err(err) => panic!("failed to bind listener: {err}"),
        };
        let addr = listener.local_addr().expect("failed to read listener addr");

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = vec![0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let _ = socket
                    .write_all(b"HTTP/1.1 500 INTERNAL SERVER ERROR\r\nContent-Length: 0\r\n\r\n")
                    .await;
            }
        });

        let Json(value) = machine_health(Json(MachineHealthRequest {
            ip: addr.ip().to_string(),
            turn_off_port: Some(addr.port()),
        }))
        .await;

        assert_eq!(value["status"], "offline");
        assert_eq!(value["http_status"], 500);
    }

    #[tokio::test]
    async fn machine_health_reports_online_with_successful_response() {
        let listener = match TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                eprintln!(
                    "skipping machine_health_reports_online_with_successful_response: {}",
                    err
                );
                return;
            }
            Err(err) => panic!("failed to bind listener: {err}"),
        };
        let addr = listener.local_addr().expect("failed to read listener addr");

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = vec![0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                    .await;
            }
        });

        let Json(value) = machine_health(Json(MachineHealthRequest {
            ip: addr.ip().to_string(),
            turn_off_port: Some(addr.port()),
        }))
        .await;

        assert_eq!(value["status"], "online");
        assert_eq!(value["http_status"], 200);
    }

    #[tokio::test]
    async fn update_machine_config_applies_changes() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp_dir = tempdir().expect("failed to create temp dir");
        let file_path = tmp_dir.path().join("machines.json");
        let _guard = EnvGuard::set_path("WAKEZILLA_MACHINES_DB_PATH", &file_path);

        let state = state_with_machines(vec![sample_machine()]);
        let mut payload = HashMap::new();
        payload.insert("mac".to_string(), "AA:BB:CC:DD:EE:FF".to_string());
        payload.insert("name".to_string(), "Updated".to_string());
        payload.insert("description".to_string(), "New description".to_string());
        payload.insert("can_be_turned_off".to_string(), "true".to_string());
        payload.insert("requests_per_hour".to_string(), "15".to_string());
        payload.insert("period_minutes".to_string(), "5".to_string());
        payload.insert("turn_off_port".to_string(), "8080".to_string());

        let response = update_machine_config(State(state.clone()), Form(payload)).await;
        assert_eq!(response.into_response().status(), StatusCode::SEE_OTHER);

        let machines = state.machines.read().await;
        let updated = machines.first().expect("machine should exist");
        assert_eq!(updated.name, "Updated");
        assert_eq!(updated.description.as_deref(), Some("New description"));
        assert!(updated.can_be_turned_off);
        assert_eq!(updated.request_rate.max_requests, 15);
        assert_eq!(updated.request_rate.period_minutes, 5);
        assert_eq!(updated.turn_off_port, Some(8080));
    }

    #[tokio::test]
    async fn delete_machine_stops_proxy_and_removes_machine() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp_dir = tempdir().expect("failed to create temp dir");
        let file_path = tmp_dir.path().join("machines.json");
        let _guard = EnvGuard::set_path("WAKEZILLA_MACHINES_DB_PATH", &file_path);

        let machine = sample_machine();
        let state = state_with_machines(vec![machine.clone()]);

        {
            let mut proxies = state.proxies.write().await;
            let (tx, _rx) = watch::channel(true);
            proxies.insert(format!("{}-proxy", machine.mac), tx);
        }

        let redirect = delete_machine(
            State(state.clone()),
            Form(DeleteForm {
                mac: machine.mac.clone(),
            }),
        )
        .await;
        let response = redirect.into_response();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::LOCATION)
                .and_then(|value| value.to_str().ok()),
            Some("/")
        );

        assert!(state.machines.read().await.is_empty());
        assert!(state.proxies.read().await.is_empty());
    }

    #[tokio::test]
    async fn update_ports_removes_marked_entries() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp_dir = tempdir().expect("failed to create temp dir");
        let file_path = tmp_dir.path().join("machines.json");
        let _guard = EnvGuard::set_path("WAKEZILLA_MACHINES_DB_PATH", &file_path);

        let mut machine = sample_machine();
        machine.port_forwards = vec![web::PortForward {
            name: "SSH".to_string(),
            local_port: 22,
            target_port: 22,
        }];
        let state = state_with_machines(vec![machine.clone()]);

        {
            let mut proxies = state.proxies.write().await;
            let (tx, _rx) = watch::channel(true);
            proxies.insert(format!("{}-ssh", machine.mac), tx);
        }

        let mut payload = HashMap::new();
        payload.insert("mac".to_string(), machine.mac.clone());
        payload.insert("pf_name_0".to_string(), "SSH".to_string());
        payload.insert("pf_local_0".to_string(), "22".to_string());
        payload.insert("pf_target_0".to_string(), "22".to_string());
        payload.insert("pf_remove_0".to_string(), "on".to_string());

        let response = update_ports(State(state.clone()), Form(payload)).await;
        assert_eq!(response.into_response().status(), StatusCode::SEE_OTHER);

        let machines = state.machines.read().await;
        assert!(machines[0].port_forwards.is_empty());
        assert!(state.proxies.read().await.is_empty());
    }
}
