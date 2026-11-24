use anyhow::Result;
use axum::{
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json},
    routing::{get, post},
    Router,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::shutdown_auth::{
    unix_timestamp, ReplayGuard, SignedRequestHeaders, NONCE_HEADER, SIGNATURE_HEADER,
    TIMESTAMP_HEADER,
};
use crate::system;

#[derive(Clone)]
struct ClientState {
    shutdown_key: Option<String>,
    replay_guard: Arc<Mutex<ReplayGuard>>,
}

pub fn build_router(shutdown_key: Option<String>) -> Router {
    let state = ClientState {
        shutdown_key,
        replay_guard: Arc::new(Mutex::new(ReplayGuard::default())),
    };
    Router::new()
        .route("/health", get(health_check))
        .route("/health/secure", get(secure_health_check))
        .route("/machines/turn-off", post(turn_off_machine))
        .with_state(state)
}

pub async fn start(port: u16, shutdown_key: Option<String>) -> Result<()> {
    start_with_shutdown(port, shutdown_key, std::future::pending::<()>()).await
}

pub async fn start_with_shutdown(
    port: u16,
    shutdown_key: Option<String>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    if shutdown_key.is_none() {
        warn!(
            "client shutdown authentication is not configured; accepting legacy unsigned shutdown requests"
        );
    }
    let app = build_router(shutdown_key);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(addr).await?;
    info!("listening on http://{}", listener.local_addr()?);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await?;

    Ok(())
}

async fn health_check() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn secure_health_check(
    State(state): State<ClientState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    match authenticate(&state, "GET", "/health/secure", &headers).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "status": "ok" }))),
        Err(status) => (
            status,
            Json(serde_json::json!({ "status": "unauthorized" })),
        ),
    }
}

async fn turn_off_machine(
    State(state): State<ClientState>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let peer_addr = peer.map(|ConnectInfo(addr)| addr.to_string());
    let peer_addr = peer_addr.as_deref().unwrap_or("unknown");
    if state.shutdown_key.is_some() {
        if let Err(status) = authenticate(&state, "POST", "/machines/turn-off", &headers).await {
            warn!(
                peer_addr,
                method = "POST",
                path = "/machines/turn-off",
                status = status.as_u16(),
                "Rejected shutdown request"
            );
            return (status, "Unauthorized".to_string());
        }
    }

    warn!(
        peer_addr,
        method = "POST",
        path = "/machines/turn-off",
        authenticated = state.shutdown_key.is_some(),
        delay_secs = 5,
        "Accepted shutdown request"
    );
    system::shutdown_machine();
    (StatusCode::OK, "Shutting down this machine".to_string())
}

async fn authenticate(
    state: &ClientState,
    method: &str,
    path: &str,
    headers: &HeaderMap,
) -> std::result::Result<(), StatusCode> {
    let key = state
        .shutdown_key
        .as_deref()
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let signed = SignedRequestHeaders {
        timestamp: header_value(headers, TIMESTAMP_HEADER)?,
        nonce: header_value(headers, NONCE_HEADER)?,
        signature: header_value(headers, SIGNATURE_HEADER)?,
    };
    state
        .replay_guard
        .lock()
        .await
        .verify(key, method, path, &signed, unix_timestamp())
        .map_err(|error| {
            warn!("rejected authenticated client request: {error}");
            StatusCode::UNAUTHORIZED
        })
}

fn header_value(headers: &HeaderMap, name: &'static str) -> Result<String, StatusCode> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
        .ok_or(StatusCode::UNAUTHORIZED)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn health_check_returns_ok_json() {
        let response = health_check().await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
