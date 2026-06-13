use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tempfile::TempDir;
use tokio::sync::RwLock;
use tower::util::ServiceExt;
use wakezilla::config::Config;
use wakezilla::forward::TurnOffLimiter;
use wakezilla::proxy_server::{api_routes, build_router};
use wakezilla::web::{AppState, Machine as InternalMachine};
use wakezilla::Machine;

struct EnvVarGuard {
    key: &'static str,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        std::env::set_var(key, value);
        Self { key }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        std::env::remove_var(self.key);
    }
}

fn setup_state(temp_dir: &TempDir) -> (AppState, EnvVarGuard) {
    let db_path = temp_dir.path().join("machines.json");
    let guard = EnvVarGuard::set(
        "WAKEZILLA__STORAGE__MACHINES_DB_PATH",
        db_path.to_str().expect("temp path should be valid utf-8"),
    );
    let config = Config::from_env().unwrap_or_default();

    let machines =
        Arc::new(RwLock::new(Vec::<InternalMachine>::new())) as Arc<RwLock<Vec<InternalMachine>>>;
    let proxies = Arc::new(RwLock::new(HashMap::new()));
    let state = AppState {
        machines,
        proxies,
        config: Arc::new(config),
        turn_off_limiter: Arc::new(TurnOffLimiter::new()),
        monitor_handle: Arc::new(std::sync::Mutex::new(None)),
        access_log: Arc::new(RwLock::new(wakezilla::access_log::AccessLog::new(2000))),
    };

    (state, guard)
}

fn sample_machine() -> InternalMachine {
    InternalMachine {
        mac: "AA:BB:CC:DD:EE:FF".to_string(),
        ip: "127.0.0.1".parse().expect("valid ip"),
        name: "Workstation".to_string(),
        description: Some("Primary workstation".to_string()),
        turn_off_port: None,
        can_be_turned_off: false,
        inactivity_period: 60,
        port_forwards: Vec::new(),
    }
}

#[tokio::test]
async fn access_history_returns_service_entries() {
    let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let (state, _guard) = setup_state(&temp_dir);

    let mut machine = sample_machine();
    machine.port_forwards = vec![wakezilla::web::PortForward {
        name: "konga".to_string(),
        local_port: 1234,
        target_port: 80,
    }];
    state.machines.write().await.push(machine.clone());

    {
        let key = wakezilla::access_log::service_key(&machine.mac, 1234);
        let mut log = state.access_log.write().await;
        log.record(&key, 1000);
        log.record(&key, 2000);
    }

    let app = build_router(state.clone()).merge(api_routes(state.clone()));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/machines/AA:BB:CC:DD:EE:FF/access-history")
                .method("GET")
                .body(Body::empty())
                .expect("failed to build request"),
        )
        .await
        .expect("handler failed");

    assert_eq!(response.status(), StatusCode::OK);
    let parsed: wakezilla::AccessHistory = serde_json::from_slice(
        &response
            .into_body()
            .collect()
            .await
            .expect("collect")
            .to_bytes(),
    )
    .expect("valid access history json");
    assert_eq!(parsed.services.len(), 1);
    assert_eq!(parsed.services[0].name.as_deref(), Some("konga"));
    assert_eq!(parsed.services[0].local_port, 1234);
    assert_eq!(parsed.services[0].timestamps, vec![1000, 2000]);
}

#[tokio::test]
async fn add_machine_accepts_null_port_forward_name() {
    let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let (state, _guard) = setup_state(&temp_dir);
    let app = build_router(state.clone()).merge(api_routes(state.clone()));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/machines")
                .method("POST")
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "mac": "22:33:44:55:66:77",
                        "ip": "192.168.1.20",
                        "name": "Host",
                        "description": null,
                        "turn_off_port": null,
                        "can_be_turned_off": false,
                        "inactivity_period": null,
                        "port_forwards": [{"name": null, "local_port": 8080, "target_port": 80}]
                    }))
                    .expect("serialize payload"),
                ))
                .expect("failed to build add-machine request"),
        )
        .await
        .expect("add-machine handler failed");

    assert_eq!(response.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn api_get_machine_details_returns_existing_machine() {
    let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let (state, _guard) = setup_state(&temp_dir);

    {
        let mut machines = state.machines.write().await;
        machines.push(sample_machine());
    }

    let app = build_router(state.clone()).merge(api_routes(state.clone()));
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/machines/AA:BB:CC:DD:EE:FF")
                .method("GET")
                .body(Body::empty())
                .expect("failed to build request"),
        )
        .await
        .expect("handler failed");

    assert_eq!(response.status(), StatusCode::OK);
    let machine: Machine = serde_json::from_slice(
        &response
            .into_body()
            .collect()
            .await
            .expect("failed to collect body")
            .to_bytes(),
    )
    .expect("expected valid machine json");
    assert_eq!(machine.name, "Workstation");

    let not_found = app
        .oneshot(
            Request::builder()
                .uri("/api/machines/11:22:33:44:55:66")
                .method("GET")
                .body(Body::empty())
                .expect("failed to build request"),
        )
        .await
        .expect("handler failed");

    assert_eq!(not_found.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn wake_endpoint_rejects_invalid_mac() {
    let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let (state, _guard) = setup_state(&temp_dir);
    let app = build_router(state.clone()).merge(api_routes(state.clone()));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/machines/invalid-mac/wake")
                .method("POST")
                .body(Body::empty())
                .expect("failed to build wake request"),
        )
        .await
        .expect("wake handler failed");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json: serde_json::Value = serde_json::from_slice(
        &response
            .into_body()
            .collect()
            .await
            .expect("failed to collect body")
            .to_bytes(),
    )
    .expect("valid json response");
    assert!(json["message"]
        .as_str()
        .unwrap_or_default()
        .contains("Invalid MAC"));
}

#[tokio::test]
async fn add_and_delete_machine_via_api_updates_state() {
    let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let (state, _guard) = setup_state(&temp_dir);
    let app = build_router(state.clone()).merge(api_routes(state.clone()));

    let add_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/machines")
                .method("POST")
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "mac": "11:22:33:44:55:66",
                        "ip": "192.168.1.10",
                        "name": "New Machine",
                        "description": null,
                        "turn_off_port": null,
                        "can_be_turned_off": false,
                        "inactivity_period": null,
                        "port_forwards": []
                    }))
                    .expect("serialize payload"),
                ))
                .expect("failed to build add-machine request"),
        )
        .await
        .expect("add-machine handler failed");

    assert_eq!(add_response.status(), StatusCode::CREATED);

    {
        let machines = state.machines.read().await;
        assert!(machines.iter().any(|m| m.mac == "11:22:33:44:55:66"));
    }

    let delete_response = app
        .oneshot(
            Request::builder()
                .uri("/api/machines/delete")
                .method("DELETE")
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "mac": "11:22:33:44:55:66"
                    }))
                    .expect("serialize delete payload"),
                ))
                .expect("failed to build delete-machine request"),
        )
        .await
        .expect("delete-machine handler failed");

    assert_eq!(delete_response.status(), StatusCode::OK);

    {
        let machines = state.machines.read().await;
        assert!(!machines.iter().any(|m| m.mac == "11:22:33:44:55:66"));
    }
}

#[tokio::test]
async fn add_machine_with_invalid_data_returns_errors() {
    let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let (state, _guard) = setup_state(&temp_dir);
    let app = build_router(state.clone()).merge(api_routes(state.clone()));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/machines")
                .method("POST")
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "mac": "bad",
                        "ip": "not-an-ip",
                        "name": "",
                        "description": null,
                        "turn_off_port": null,
                        "can_be_turned_off": false,
                        "inactivity_period": null,
                        "port_forwards": []
                    }))
                    .expect("serialize payload"),
                ))
                .expect("failed to build add-machine request"),
        )
        .await
        .expect("add-machine handler failed");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json: serde_json::Value = serde_json::from_slice(
        &response
            .into_body()
            .collect()
            .await
            .expect("failed to collect body")
            .to_bytes(),
    )
    .expect("valid error json");
    assert!(json["errors"]["mac"].is_array());
    assert!(json["errors"]["ip"].is_array());

    let machines = state.machines.read().await;
    assert!(machines.is_empty());
}
