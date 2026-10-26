use std::{collections::HashMap, sync::Arc};

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::RwLock;
use tower::ServiceExt;
use wakezilla::{config::Config, forward::TurnOffLimiter, proxy_server::api_routes, web::AppState};

const MAC_A: &str = "02:00:00:00:00:01";
const MAC_B: &str = "02:00:00:00:00:02";
const MAC_C: &str = "02:00:00:00:00:03";

fn state(dir: &TempDir) -> AppState {
    let mut config = Config::default();
    config.storage.machines_db_path = dir
        .path()
        .join("machines.json")
        .to_string_lossy()
        .into_owned();
    AppState {
        machines: Arc::new(RwLock::new(vec![])),
        proxies: Arc::new(RwLock::new(HashMap::new())),
        config: Arc::new(config),
        turn_off_limiter: Arc::new(TurnOffLimiter::new()),
        monitor_handle: Arc::new(std::sync::Mutex::new(None)),
        access_log: Arc::new(RwLock::new(wakezilla::access_log::AccessLog::new(10))),
    }
}

async fn request(state: AppState, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let response = api_routes(state)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn find_free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("reserve a loopback port")
        .local_addr()
        .expect("read reserved port")
        .port()
}

fn service(local_port: u16, target_port: u16) -> Value {
    json!({ "name": "svc", "local_port": local_port, "target_port": target_port })
}

fn machine_payload(name: &str, mac: &str, port_forwards: Vec<Value>) -> Value {
    json!({
        "name": name,
        "mac": mac,
        "ip": "127.0.0.1",
        "description": null,
        "turn_off_port": null,
        "can_be_turned_off": false,
        "port_forwards": port_forwards,
    })
}

fn assert_port_forwards_error(body: &Value, expected: &str) {
    let errors = body["errors"].as_object().expect("errors object");
    assert_eq!(errors.len(), 1, "unexpected error fields: {body}");
    let messages = errors
        .get("port_forwards")
        .and_then(Value::as_array)
        .expect("port_forwards messages");
    assert!(
        messages
            .iter()
            .filter_map(Value::as_str)
            .any(|message| message.contains(expected)),
        "expected port_forwards error containing {expected:?}, got {body}"
    );
}

async fn add_machine(state: &AppState, name: &str, mac: &str, port_forwards: Vec<Value>) {
    let (status, body) = request(
        state.clone(),
        "POST",
        "/api/machines",
        machine_payload(name, mac, port_forwards),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "seeding {name} failed: {body}");
}

#[tokio::test]
async fn add_machine_with_local_port_owned_by_another_machine_returns_400() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(&dir);
    let owned_port = find_free_port();

    add_machine(&state, "A", MAC_A, vec![service(owned_port, 80)]).await;

    let (status, body) = request(
        state.clone(),
        "POST",
        "/api/machines",
        machine_payload("B", MAC_B, vec![service(owned_port, 8080)]),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_port_forwards_error(&body, "already assigned to another machine");
    assert_eq!(
        state.machines.read().await.len(),
        1,
        "rejected machine must not be saved"
    );
}

#[tokio::test]
async fn update_machine_to_local_port_owned_by_another_machine_returns_400() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(&dir);
    let owned_port = find_free_port();

    add_machine(&state, "A", MAC_A, vec![service(owned_port, 80)]).await;
    add_machine(&state, "B", MAC_B, vec![]).await;

    let (status, body) = request(
        state.clone(),
        "PUT",
        &format!("/api/machines/{MAC_B}"),
        machine_payload("B", MAC_B, vec![service(owned_port, 8080)]),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_port_forwards_error(&body, "already assigned to another machine");
    let machines = state.machines.read().await;
    assert_eq!(machines.len(), 2);
    let machine_b = machines
        .iter()
        .find(|machine| machine.mac == MAC_B)
        .expect("machine B to exist");
    assert!(
        machine_b.port_forwards.is_empty(),
        "rejected update must not change machine B"
    );
}

#[tokio::test]
async fn update_machine_keeping_its_own_local_ports_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(&dir);
    let owned_port = find_free_port();

    add_machine(&state, "B", MAC_B, vec![service(owned_port, 80)]).await;

    let (status, body) = request(
        state.clone(),
        "PUT",
        &format!("/api/machines/{MAC_B}"),
        machine_payload("B renamed", MAC_B, vec![service(owned_port, 8080)]),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "unexpected response body: {body}");
    let machines = state.machines.read().await;
    assert_eq!(machines.len(), 1);
    assert_eq!(machines[0].name, "B renamed");
    assert_eq!(machines[0].port_forwards[0].local_port, owned_port);
    assert_eq!(machines[0].port_forwards[0].target_port, 8080);
    drop(machines);

    if let Some(handle) = state.monitor_handle.lock().unwrap().take() {
        handle.abort();
    };
}

#[tokio::test]
async fn add_machine_with_two_services_on_the_same_local_port_returns_400() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(&dir);

    let (status, body) = request(
        state.clone(),
        "POST",
        "/api/machines",
        machine_payload("C", MAC_C, vec![service(2020, 80), service(2020, 443)]),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_port_forwards_error(&body, "unique local ports");
    assert!(
        state.machines.read().await.is_empty(),
        "rejected machine must not be saved"
    );
}

#[tokio::test]
async fn update_machine_with_two_services_on_the_same_local_port_returns_400() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(&dir);

    add_machine(&state, "B", MAC_B, vec![]).await;

    let (status, body) = request(
        state.clone(),
        "PUT",
        &format!("/api/machines/{MAC_B}"),
        machine_payload("B", MAC_B, vec![service(2020, 80), service(2020, 443)]),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_port_forwards_error(&body, "unique local ports");
    let machines = state.machines.read().await;
    assert_eq!(machines.len(), 1);
    assert!(
        machines[0].port_forwards.is_empty(),
        "rejected update must not change machine B"
    );
}
