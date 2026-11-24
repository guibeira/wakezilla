use crate::access_log::{now_millis, AccessLog};
use crate::{config::Config, web::Machine, wol};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::copy_bidirectional;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::sync::RwLock;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

fn turn_off_url(remote_ip: &str, turn_off_port: u16) -> String {
    format!("http://{}:{}/machines/turn-off", remote_ip, turn_off_port)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientVerification {
    Verified,
    KeyMismatch,
    Unreachable,
}

pub async fn verify_remote_client(
    remote_ip: &str,
    client_port: u16,
    shutdown_auth_key: &str,
) -> ClientVerification {
    let client = match reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(client) => client,
        Err(_) => return ClientVerification::Unreachable,
    };
    let signed =
        match crate::shutdown_auth::sign_request(shutdown_auth_key, "GET", "/health/secure") {
            Ok(signed) => signed,
            Err(_) => return ClientVerification::KeyMismatch,
        };
    let url = format!("http://{remote_ip}:{client_port}/health/secure");
    match client
        .get(url)
        .header(crate::shutdown_auth::TIMESTAMP_HEADER, signed.timestamp)
        .header(crate::shutdown_auth::NONCE_HEADER, signed.nonce)
        .header(crate::shutdown_auth::SIGNATURE_HEADER, signed.signature)
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => ClientVerification::Verified,
        Ok(response)
            if matches!(
                response.status(),
                reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
            ) =>
        {
            ClientVerification::KeyMismatch
        }
        Ok(_) | Err(_) => ClientVerification::Unreachable,
    }
}

struct MachineConfig {
    ip: Ipv4Addr,
    window: Duration,
    turn_off_port: u16,
    shutdown_auth_key: Option<String>,
    triggered: AtomicBool,
    last_request: Instant,
}

#[derive(Clone)]
pub struct TurnOffLimiter {
    machines: Arc<Mutex<HashMap<String, MachineConfig>>>,
}

impl Default for TurnOffLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl TurnOffLimiter {
    pub fn new() -> Self {
        Self {
            machines: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn initialize_machine(&self, machine: &Machine, turn_off_port: u16) {
        if !machine.can_be_turned_off || machine.inactivity_period == 0 {
            self.remove_machine(&machine.mac);
            return;
        }
        let window_minutes = machine.inactivity_period;
        let window_secs = window_minutes.saturating_mul(60);
        let config = MachineConfig {
            ip: machine.ip,
            window: Duration::from_secs(window_secs as u64),
            turn_off_port,
            shutdown_auth_key: machine.shutdown_auth_key.clone(),
            triggered: AtomicBool::new(false),
            last_request: Instant::now(),
        };
        let mut machines = self.machines.lock().unwrap();
        machines.insert(machine.mac.clone(), config);
    }

    #[allow(dead_code)]
    pub fn update_machine(&self, machine: &Machine, turn_off_port: u16) {
        if !machine.can_be_turned_off || machine.inactivity_period == 0 {
            self.remove_machine(&machine.mac);
            return;
        }
        let window_minutes = machine.inactivity_period;
        let window_secs = window_minutes.saturating_mul(60);
        let mut machines = self.machines.lock().unwrap();
        if let Some(config) = machines.get_mut(&machine.mac) {
            // Update existing configuration
            config.ip = machine.ip;
            config.window = Duration::from_secs(window_secs as u64);
            config.turn_off_port = turn_off_port;
            config.shutdown_auth_key = machine.shutdown_auth_key.clone();
            // Reset triggered flag so it can trigger again if needed
            config.triggered.store(false, Ordering::SeqCst);
            debug!(
                "Updated inactivity monitoring configuration for machine {} (IP: {}): {}min",
                machine.mac, machine.ip, machine.inactivity_period
            );
        } else {
            // Machine not found, initialize it
            drop(machines);
            self.initialize_machine(machine, turn_off_port);
        }
    }

    pub fn remove_machine(&self, mac: &str) {
        if let Ok(mut machines) = self.machines.lock() {
            machines.remove(mac);
        }
    }

    pub fn update_last_request(&self, mac: &str) {
        let mut machines = self.machines.lock().unwrap();
        if let Some(config) = machines.get_mut(mac) {
            config.last_request = Instant::now();
            config.triggered.store(false, Ordering::SeqCst);
            debug!(
                "Updated last_request for machine {} (IP: {})",
                mac, config.ip
            );
        }
    }

    pub fn start_inactivity_monitor(&self) -> tokio::task::AbortHandle {
        let limiter = self.clone();
        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                let now = Instant::now();
                let machines_to_check: Vec<(Ipv4Addr, u16, String, Option<String>)> = {
                    let machines = limiter.machines.lock().unwrap();
                    machines
                        .iter()
                        .filter_map(|(mac, config)| {
                            let time_since_last_request = now.duration_since(config.last_request);
                            debug!(
                                "Checking inactivity for machine {} (IP: {}): last request was {:?} ago, window is {:?}",
                                mac, config.ip, time_since_last_request, config.window
                            );
                            if time_since_last_request > config.window {
                                // Use swap to atomically check and set triggered flag
                                if !config.triggered.swap(true, Ordering::SeqCst) {
                                    info!(
                                        trigger = "inactivity_timeout",
                                        machine_mac = %mac,
                                        target_ip = %config.ip,
                                        target_port = config.turn_off_port,
                                        inactivity_limit_secs = config.window.as_secs(),
                                        inactive_secs = time_since_last_request.as_secs(),
                                        "Scheduling shutdown after inactivity limit"
                                    );
                                    Some((
                                        config.ip,
                                        config.turn_off_port,
                                        mac.clone(),
                                        config.shutdown_auth_key.clone(),
                                    ))
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        })
                        .collect()
                };

                for (ip, turn_off_port, mac, shutdown_auth_key) in machines_to_check {
                    let remote_ip = ip.to_string();
                    debug!(
                        "Sending turn-off signal for inactive machine {} (IP: {})",
                        mac, remote_ip
                    );
                    tokio::spawn(async move {
                        if let Err(e) = turn_off_remote_machine(
                            &remote_ip,
                            turn_off_port,
                            shutdown_auth_key.as_deref(),
                        )
                        .await
                        {
                            error!(
                                "Failed to send turn-off signal for inactive machine {} on {}:{}: {}",
                                mac, remote_ip, turn_off_port, e
                            );
                        }
                    });
                }
            }
        }).abort_handle();
        handle
    }

    pub async fn proxy_internal(
        &self,
        local_port: u16,
        remote_addr: SocketAddr,
        machine: Machine,
        mut rx: watch::Receiver<bool>,
        config: Arc<Config>,
        access_log: Arc<RwLock<AccessLog>>,
    ) -> Result<()> {
        let service_key = crate::access_log::service_key(&machine.mac, local_port);
        let listen_addr = format!("0.0.0.0:{}", local_port);
        let listener = TcpListener::bind(&listen_addr)
            .await
            .with_context(|| format!("Failed to bind TCP listener on {}", listen_addr))?;
        info!(
            "TCP Forwarder listening on {}, proxying to {}, inactivity period: {}min",
            listen_addr, remote_addr, machine.inactivity_period
        );

        // Note: Monitor is started globally, not per proxy

        loop {
            tokio::select! {
                result = rx.changed() => {
                    if result.is_err() || !*rx.borrow() {
                        info!("Proxy for {} on port {} cancelled.", remote_addr, local_port);
                        return Ok(());
                    }
                }
                result = listener.accept() => {
                    let (mut inbound, client_addr) = result
                        .context("Failed to accept incoming connection")?;
                    info!(
                        peer_addr = %client_addr,
                        local_port,
                        target_addr = %remote_addr,
                        machine_mac = %machine.mac,
                        "Accepted forwarded connection"
                    );

                    access_log
                        .write()
                        .await
                        .record(&service_key, now_millis());

                    let remote_addr_clone = remote_addr;
                    let mac_str_clone = machine.mac.clone();
                    let rate_limiter = self.clone();
                    let machine_mac_clone = machine.mac.clone();
                    let config_clone = Arc::clone(&config);

                    tokio::spawn(async move {
                        // Update last_request whenever we receive a connection
                        rate_limiter.update_last_request(&machine_mac_clone);

                        let connect_timeout = Duration::from_millis(1000);
                        if !wol::tcp_check(remote_addr_clone, connect_timeout).await {
                            info!(
                                "Host {} seems to be down. Sending WOL packet to MAC {}.",
                                remote_addr_clone, mac_str_clone
                            );

                            let mac = match wol::parse_mac(&mac_str_clone) {
                                Ok(m) => m,
                                Err(e) => {
                                    error!("Invalid MAC for WOL on proxy: {}: {}", mac_str_clone, e);
                                    return;
                                }
                            };

                            let wol_port = config_clone.wol.default_port;
                            let wol_count = config_clone.wol.default_packet_count;
                            let wol_broadcast = config_clone.get_default_broadcast_addr();
                            if let Err(e) = crate::wol::send_packets(
                                &mac,
                                wol_broadcast,
                                wol_port,
                                wol_count,
                                &config_clone,
                            )
                            .await
                            {
                                error!("Failed to send WOL packet for {}: {}", mac_str_clone, e);
                                return;
                            }

                            info!(
                                "WOL packet sent. Waiting up to 60s for {} to become reachable...",
                                remote_addr_clone
                            );

                            let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
                            let mut host_up = false;
                            while tokio::time::Instant::now() < deadline {
                                if wol::tcp_check(remote_addr_clone, connect_timeout).await {
                                    info!("Host {} is now up.", remote_addr_clone);
                                    host_up = true;
                                    break;
                                }
                                tokio::time::sleep(Duration::from_secs(2)).await;
                            }

                            if !host_up {
                                warn!(
                                    "Timeout waiting for host {} to come up. Dropping connection from {}.",
                                    remote_addr_clone, client_addr
                                );
                                return;
                            }
                        }

                        let mut outbound = match tokio::time::timeout(
                            Duration::from_secs(30),
                            tokio::net::TcpStream::connect(remote_addr_clone),
                        )
                        .await
                        {
                            Ok(Ok(stream)) => {
                                debug!("Successfully connected to {}", remote_addr_clone);
                                stream
                            }
                            Ok(Err(e)) => {
                                error!(
                                    "Failed to connect to remote {}: {}",
                                    remote_addr_clone, e
                                );
                                return;
                            }
                            Err(_) => {
                                error!("Timeout connecting to remote {}", remote_addr_clone);
                                return;
                            }
                        };

                        match copy_bidirectional(&mut inbound, &mut outbound).await {
                            Ok(_) => {
                                drop(outbound);
                                debug!(
                                    "Completed data transfer for {} (connection closed)",
                                    remote_addr_clone
                                );
                            }
                            Err(e) => {
                                drop(outbound);
                                warn!(
                                    "Error forwarding data between {} and {}: {}",
                                    client_addr, remote_addr_clone, e
                                );
                            }
                        }

                    });
                }
            }
        }
    }

    pub async fn proxy(
        local_port: u16,
        remote_addr: SocketAddr,
        machine: Machine,
        rx: watch::Receiver<bool>,
        limiter: Arc<TurnOffLimiter>,
        config: Arc<Config>,
        access_log: Arc<RwLock<AccessLog>>,
    ) -> Result<()> {
        limiter
            .proxy_internal(local_port, remote_addr, machine, rx, config, access_log)
            .await
    }
}

pub async fn turn_off_remote_machine(
    remote_ip: &str,
    turn_off_port: u16,
    shutdown_auth_key: Option<&str>,
) -> Result<()> {
    let url = turn_off_url(remote_ip, turn_off_port);
    info!("Sending turn-off signal to {}", url);
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()?;

    let mut request = client.post(&url);
    if let Some(key) = shutdown_auth_key {
        let signed = crate::shutdown_auth::sign_request(key, "POST", "/machines/turn-off")
            .context("failed to sign turn-off request")?;
        request = request
            .header(crate::shutdown_auth::TIMESTAMP_HEADER, signed.timestamp)
            .header(crate::shutdown_auth::NONCE_HEADER, signed.nonce)
            .header(crate::shutdown_auth::SIGNATURE_HEADER, signed.signature);
    }
    request.send().await?.error_for_status()?;
    info!(
        "Successfully sent turn-off signal to {}:{}",
        remote_ip, turn_off_port
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

    fn monitored_machine(minutes: u32) -> Machine {
        serde_json::from_value(serde_json::json!({
            "mac": "02:00:00:00:00:01", "ip": "127.0.0.1", "name": "test",
            "description": null, "turn_off_port": 3001, "can_be_turned_off": true,
            "inactivity_period": minutes, "port_forwards": []
        }))
        .expect("valid test machine")
    }

    #[test]
    fn zero_inactivity_does_not_schedule_shutdown() {
        let limiter = TurnOffLimiter::new();
        limiter.initialize_machine(&monitored_machine(0), 3001);
        assert!(limiter.machines.lock().unwrap().is_empty());
    }

    #[test]
    fn setting_zero_cancels_an_existing_shutdown_timer() {
        let limiter = TurnOffLimiter::new();
        limiter.initialize_machine(&monitored_machine(30), 3001);
        assert_eq!(limiter.machines.lock().unwrap().len(), 1);
        limiter.update_machine(&monitored_machine(0), 3001);
        assert!(limiter.machines.lock().unwrap().is_empty());
    }

    #[test]
    fn disabled_machine_with_shared_ip_does_not_cancel_another_timer() {
        let limiter = TurnOffLimiter::new();
        let monitored = monitored_machine(30);
        let mut disabled = monitored_machine(0);
        disabled.mac = "02:00:00:00:00:02".to_string();

        limiter.initialize_machine(&monitored, 3001);
        limiter.initialize_machine(&disabled, 3001);

        let machines = limiter.machines.lock().unwrap();
        assert_eq!(machines.len(), 1);
        assert!(machines.contains_key(&monitored.mac));
    }

    #[test]
    fn turn_off_url_formats_expected_path() {
        let url = super::turn_off_url("192.168.1.10", 8080);
        assert_eq!(url, "http://192.168.1.10:8080/machines/turn-off");
    }

    #[tokio::test]
    async fn turn_off_remote_machine_sends_expected_request() {
        let listener = match TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(err)
                if matches!(
                    err.kind(),
                    ErrorKind::PermissionDenied | ErrorKind::AddrNotAvailable
                ) =>
            {
                eprintln!(
                    "skipping test because binding TCP sockets is not permitted: {}",
                    err
                );
                return;
            }
            Err(err) => panic!("failed to bind http test listener: {err}"),
        };
        let addr = listener.local_addr().expect("failed to read listener addr");

        let received = Arc::new(Mutex::new(None));
        let received_clone = received.clone();

        let server_task = tokio::spawn(async move {
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

        turn_off_remote_machine(&addr.ip().to_string(), addr.port(), None)
            .await
            .expect("turn_off_remote_machine should succeed");

        server_task.await.expect("server task panicked");

        let request = received.lock().await.clone().expect("no request captured");
        assert!(request.starts_with("POST /machines/turn-off"));

        let host_line = request
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("host:"))
            .unwrap_or_else(|| panic!("Host header missing in request: {request}"));

        let host_value = host_line.split_once(':').map(|(_, value)| value.trim());
        let expected_ip = addr.ip().to_string();
        let expected_with_port = format!("{}:{}", expected_ip, addr.port());
        assert!(
            matches!(host_value, Some(value) if value.eq_ignore_ascii_case(&expected_ip) || value.eq_ignore_ascii_case(&expected_with_port)),
            "unexpected host header: {host_line}"
        );
    }
}
