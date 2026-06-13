use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use std::sync::Arc;
use tokio::sync::RwLock;
use wakezilla::access_log::AccessLog;
use wakezilla::config::Config;
use wakezilla::forward::{self, TurnOffLimiter};
use wakezilla::web::Machine;

fn find_free_port() -> std::io::Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_forwards_tcp_traffic_and_can_shutdown() {
    let remote_listener = match TcpListener::bind("127.0.0.1:0").await {
        Ok(listener) => listener,
        Err(err) if err.kind() == ErrorKind::PermissionDenied => {
            eprintln!(
                "skipping proxy integration test because binding TCP sockets is not permitted: {}",
                err
            );
            return;
        }
        Err(err) => panic!("failed to bind remote listener: {err}"),
    };
    let remote_addr = remote_listener
        .local_addr()
        .expect("failed to read remote listener addr");

    let remote_task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = match remote_listener.accept().await {
                Ok(pair) => pair,
                Err(_) => break,
            };

            tokio::spawn(async move {
                let mut buf = vec![0u8; 1024];
                match socket.read(&mut buf).await {
                    Ok(0) | Err(_) => {}
                    Ok(n) => {
                        let _ = socket.write_all(&buf[..n]).await;
                    }
                }
            });
        }
    });

    let machine = Machine {
        mac: "AA:BB:CC:DD:EE:FF".to_string(),
        ip: match remote_addr.ip() {
            IpAddr::V4(ip) => ip,
            IpAddr::V6(_) => panic!("expected IPv4 address"),
        },
        name: "proxy-integration".to_string(),
        description: None,
        turn_off_port: None,
        can_be_turned_off: false,
        inactivity_period: 60,
        port_forwards: Vec::new(),
    };

    let (tx, rx) = watch::channel(true);
    let local_port = match find_free_port() {
        Ok(port) => port,
        Err(err) if err.kind() == ErrorKind::PermissionDenied => {
            eprintln!(
                "skipping proxy integration test because discovering free ports is not permitted: {}",
                err
            );
            return;
        }
        Err(err) => panic!("failed to discover free port: {err}"),
    };

    let limiter = Arc::new(TurnOffLimiter::new());
    let config = Arc::new(Config::default());
    let access_log = Arc::new(RwLock::new(AccessLog::new(2000)));
    let proxy_task = tokio::spawn(forward::TurnOffLimiter::proxy(
        local_port,
        remote_addr,
        machine,
        rx,
        limiter,
        config,
        access_log,
    ));

    // Give the proxy a moment to bind its listener
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut client =
        TcpStream::connect(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), local_port))
            .await
            .expect("client failed to connect to proxy");

    client
        .write_all(b"ping")
        .await
        .expect("failed to write to proxy");

    let mut buf = [0u8; 4];
    client
        .read_exact(&mut buf)
        .await
        .expect("failed to read echoed bytes");
    assert_eq!(&buf, b"ping");

    drop(client);

    // Shut down the proxy via watch channel
    tx.send(false).expect("failed to send shutdown signal");

    proxy_task
        .await
        .expect("proxy task panicked")
        .expect("proxy task returned error");

    remote_task.abort();
}
