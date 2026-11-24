//! End-to-end tests for the reverse WebSocket tunnel.
//!
//! The mock worker replicates `src-tauri/src/tunnel.rs` client logic:
//! connect, wait for `CONNECT {host} {port}` Text, open TCP to the target,
//! answer `CONNECTED`, then bridge Binary<->TCP, terminating the bridge on
//! any non-Binary frame.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use antigravity_server::config::{Config, KeyConfig};
use antigravity_server::tunnel::{spawn_connect_proxy, TunnelError, TunnelRegistry};
use antigravity_server::{build_router, AppState};
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

const KEY: &str = "test-tunnel-key-123";

/// Spawn the server (tunnel enabled, one key) on an ephemeral loopback port.
/// Returns the HTTP address and the app state.
async fn spawn_server() -> (SocketAddr, AppState) {
    let mut config = Config::default();
    config.tunnel.enabled = true;
    config.tunnel.idle_ping_secs = 1;
    config.tunnel.handshake_timeout_ms = 4_000;
    config.tunnel.acquire_timeout_ms = 2_000;
    config.auth.keys = vec![KeyConfig {
        key: KEY.to_string(),
        ..Default::default()
    }];
    let state = AppState::new(config);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serve_state = state.clone();
    tokio::spawn(async move {
        axum::serve(listener, build_router(serve_state)).await.unwrap();
    });
    // Wait for the server to accept connections.
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    (addr, state)
}

/// Local echo TCP server; returns its address. Echoes every byte back.
async fn spawn_echo() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                loop {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if sock.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });
    addr
}

/// One worker session replicating the Tauri client. Records `"binary"` for
/// each Binary frame seen after `CONNECTED` and `"NONBINARY"` if any other
/// frame (or stream end) terminates the bridge.
async fn worker_session(url: &str, target: SocketAddr, log: Arc<Mutex<Vec<&'static str>>>) {
    let (ws, _resp) = connect_async(url).await.expect("ws connect");
    let (mut write, mut read) = ws.split();

    // Idle phase: wait for the CONNECT command (Pong replies to server Ping
    // are automatic in tungstenite, like the client).
    let cmd = loop {
        match read.next().await {
            Some(Ok(Message::Text(t))) => {
                if t.starts_with("CONNECT ") {
                    break t;
                }
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => panic!("worker idle read error: {e}"),
            None => panic!("server closed before CONNECT"),
        }
    };
    let parts: Vec<&str> = cmd.as_str().split_whitespace().collect();
    assert_eq!(parts.len(), 3, "CONNECT command must have 3 tokens");

    let tcp = TcpStream::connect(target).await.expect("target connect");
    write
        .send(Message::Text("CONNECTED".to_string().into()))
        .await
        .unwrap();

    let (mut tr, mut tw) = tcp.into_split();
    let log_ws = Arc::clone(&log);
    let ws_to_tcp = tokio::spawn(async move {
        // Exact client shape: any non-Binary frame ends the bridge.
        let mut read = read;
        loop {
            match read.next().await {
                Some(Ok(Message::Binary(data))) => {
                    log_ws.lock().unwrap().push("binary");
                    if tw.write_all(&data).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) => {
                    // Allowed by spec, but only at session end.
                    log_ws.lock().unwrap().push("close");
                    break;
                }
                Some(Ok(other)) => {
                    // Ping/Text/Pong after CONNECTED: protocol violation.
                    let _ = other;
                    log_ws.lock().unwrap().push("VIOLATION");
                    break;
                }
                Some(Err(_)) | None => break,
            }
        }
    });
    let tcp_to_ws = async {
        let mut buf = vec![0u8; 8192];
        loop {
            match tr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if write
                        .send(Message::Binary(buf[..n].to_vec().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    };
    tokio::select! {
        _ = ws_to_tcp => {}
        _ = tcp_to_ws => {}
    }
    // Client drops the socket after one session (one-shot lifecycle).
    let _ = write.close().await;
}

/// Worker that answers CONNECT with `ERROR No IP found` (failure path).
async fn worker_session_error(url: &str) {
    let (ws, _resp) = connect_async(url).await.expect("ws connect");
    let (mut write, mut read) = ws.split();
    loop {
        match read.next().await {
            Some(Ok(Message::Text(t))) => {
                if t.starts_with("CONNECT ") {
                    break;
                }
            }
            Some(Ok(_)) => {}
            _ => panic!("server closed before CONNECT"),
        }
    }
    let _ = write
        .send(Message::Text("ERROR No IP found".to_string().into()))
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
}

async fn wait_idle(registry: &TunnelRegistry, key: &str, want: usize) {
    for _ in 0..100 {
        if registry.idle_count(key) == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "timed out waiting for idle_count == {want} (got {})",
        registry.idle_count(key)
    );
}

async fn roundtrip<S: AsyncRead + AsyncWrite + Unpin>(mut s: S, payload: &[u8]) {
    s.write_all(payload).await.unwrap();
    s.flush().await.unwrap();
    let mut got = vec![0u8; payload.len()];
    s.read_exact(&mut got).await.unwrap();
    assert_eq!(got, payload);
}

use tokio::net::TcpStream;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tunnel_roundtrip_and_binary_only_after_connected() {
    let (addr, state) = spawn_server().await;
    let echo = spawn_echo().await;
    let url = format!("ws://{addr}/v1/tunnel?token={KEY}");
    let log = Arc::new(Mutex::new(Vec::new()));
    let log2 = Arc::clone(&log);
    tokio::spawn(async move { worker_session(&url, echo, log2).await });
    wait_idle(&state.tunnel, KEY, 1).await;

    // Give the 1s idle-ping interval time to fire at least once while idle.
    tokio::time::sleep(Duration::from_millis(1300)).await;

    let big = vec![0xA5u8; 40_000]; // forces multi-frame writes (16 KiB cap)
    let mut stream = tokio::time::timeout(
        Duration::from_secs(8),
        state.tunnel.connect(KEY, "127.0.0.1", echo.port(), Duration::from_secs(4), Duration::from_secs(2)),
    )
    .await
    .expect("connect timed out")
    .expect("registry connect failed");
    roundtrip(&mut stream, b"hello tunnel").await;
    roundtrip(&mut stream, &big).await;

    // End the session; the stream drop closes the socket.
    drop(stream);

    // Protocol rule: after CONNECTED the server sent ONLY Binary frames;
    // a Close frame is acceptable, but only as the very last frame.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let entries = log.lock().unwrap();
    assert!(
        entries.iter().any(|e| *e == "binary"),
        "expected Binary frames, log: {entries:?}"
    );
    assert!(
        !entries.iter().any(|e| *e == "VIOLATION"),
        "server sent Ping/Text/Pong after CONNECTED: {entries:?}"
    );
    assert!(
        !entries[..entries.len().saturating_sub(1)].iter().any(|e| *e == "close"),
        "Close frame must be the last frame: {entries:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_park_and_take_leaves_pool_empty_then_replenishes_on_reconnect() {
    let (addr, state) = spawn_server().await;
    let echo = spawn_echo().await;
    let url = format!("ws://{addr}/v1/tunnel?token={KEY}");
    let log = Arc::new(Mutex::new(Vec::new()));

    // First worker session.
    let log2 = Arc::clone(&log);
    let u1 = url.clone();
    let h1 = tokio::spawn(async move { worker_session(&u1, echo, log2).await });
    wait_idle(&state.tunnel, KEY, 1).await;

    let mut s = state
        .tunnel
        .connect(KEY, "127.0.0.1", echo.port(), Duration::from_secs(4), Duration::from_secs(2))
        .await
        .expect("first connect");
    roundtrip(&mut s, b"one").await;
    drop(s);
    h1.await.unwrap();
    assert_eq!(state.tunnel.idle_count(KEY), 0, "taken worker must leave the pool");

    // Reconnect (client 5s backoff in prod; immediate here).
    let log3 = Arc::clone(&log);
    let h2 = tokio::spawn(async move { worker_session(&url, echo, log3).await });
    wait_idle(&state.tunnel, KEY, 1).await;
    let mut s2 = state
        .tunnel
        .connect(KEY, "127.0.0.1", echo.port(), Duration::from_secs(4), Duration::from_secs(2))
        .await
        .expect("second connect after reconnect");
    roundtrip(&mut s2, b"two").await;
    drop(s2);
    let _ = h2.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_worker_yields_no_worker_error() {
    let (_addr, state) = spawn_server().await;
    let err = match state
        .tunnel
        .connect(KEY, "example.com", 443, Duration::from_secs(1), Duration::from_millis(300))
        .await
    {
        Err(e) => e,
        Ok(_) => panic!("expected NoWorker error"),
    };
    assert_eq!(err, TunnelError::NoWorker);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_error_reports_remote_connect_failed() {
    let (addr, state) = spawn_server().await;
    let url = format!("ws://{addr}/v1/tunnel?token={KEY}");
    tokio::spawn(async move { worker_session_error(&url).await });
    wait_idle(&state.tunnel, KEY, 1).await;
    let err = match state
        .tunnel
        .connect(KEY, "127.0.0.1", 9, Duration::from_secs(4), Duration::from_secs(2))
        .await
    {
        Err(e) => e,
        Ok(_) => panic!("expected RemoteConnectFailed error"),
    };
    match err {
        TunnelError::RemoteConnectFailed(d) => assert!(d.contains("No IP found"), "got {d}"),
        other => panic!("expected RemoteConnectFailed, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handshake_timeout_when_worker_never_answers() {
    let (addr, state) = spawn_server().await;
    let url = format!("ws://{addr}/v1/tunnel?token={KEY}");
    // Worker connects but stays silent after CONNECT command.
    tokio::spawn(async move {
        let (ws, _r) = connect_async(url).await.unwrap();
        let (_w, mut read) = ws.split();
        while let Some(_) = read.next().await {}
    });
    wait_idle(&state.tunnel, KEY, 1).await;
    let err = match state
        .tunnel
        .connect(KEY, "127.0.0.1", 9, Duration::from_millis(300), Duration::from_secs(2))
        .await
    {
        Err(e) => e,
        Ok(_) => panic!("expected HandshakeTimeout error"),
    };
    assert_eq!(err, TunnelError::HandshakeTimeout);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connect_proxy_bridge_end_to_end() {
    let (addr, state) = spawn_server().await;
    let echo = spawn_echo().await;
    let proxy = spawn_connect_proxy(&state).await.expect("bridge spawn");
    assert!(proxy.local_addr().ip().is_loopback());

    let url = format!("ws://{addr}/v1/tunnel?token={KEY}");
    let log = Arc::new(Mutex::new(Vec::new()));
    let log2 = Arc::clone(&log);
    tokio::spawn(async move { worker_session(&url, echo, log2).await });
    wait_idle(&state.tunnel, KEY, 1).await;

    // Manual CONNECT request through the bridge (what reqwest emits for TLS).
    let mut sock = TcpStream::connect(proxy.local_addr()).await.unwrap();
    let creds =
        base64::engine::general_purpose::STANDARD.encode(format!("x:{}", proxy.secret()));
    sock.write_all(
        format!("CONNECT 127.0.0.1:{} HTTP/1.1\r\nProxy-Authorization: Basic {creds}\r\n\r\n", echo.port())
            .as_bytes(),
    )
    .await
    .unwrap();
    let mut head = [0u8; 39];
    sock.read_exact(&mut head).await.unwrap();
    assert!(
        std::str::from_utf8(&head).unwrap().starts_with("HTTP/1.1 200"),
        "head: {:?}",
        std::str::from_utf8(&head)
    );
    roundtrip(&mut sock, b"through-the-bridge").await;
    roundtrip(&mut sock, b"x".repeat(30_000).as_slice()).await;
    drop(sock);
    // reqwest_client must build without panicking.
    let _ = proxy.reqwest_client();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let entries = log.lock().unwrap();
    assert!(!entries.iter().any(|e| *e == "VIOLATION"), "{entries:?}");
    assert!(
        !entries
            .iter()
            .take(entries.len().saturating_sub(1))
            .any(|e| *e == "close"),
        "Close must be last: {entries:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_proxy_rejects_bad_secret() {
    let (_addr, state) = spawn_server().await;
    let proxy = spawn_connect_proxy(&state).await.expect("bridge spawn");
    let mut sock = TcpStream::connect(proxy.local_addr()).await.unwrap();
    let creds = base64::engine::general_purpose::STANDARD.encode("x:wrong-secret");
    sock.write_all(
        format!("CONNECT 127.0.0.1:9 HTTP/1.1\r\nProxy-Authorization: Basic {creds}\r\n\r\n")
            .as_bytes(),
    )
    .await
    .unwrap();
    let mut resp = vec![0u8; 64];
    let n = sock.read(&mut resp).await.unwrap();
    assert!(
        std::str::from_utf8(&resp[..n]).unwrap().starts_with("HTTP/1.1 407"),
        "resp: {:?}",
        std::str::from_utf8(&resp[..n])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tunnel_endpoint_auth_and_permission_gates() {
    let (addr, _state) = spawn_server().await;
    let client = reqwest::Client::new();
    let mk = |token: &str| {
        client
            .get(format!("http://{addr}/v1/tunnel?token={token}"))
            .header("connection", "upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-key", "dGVzdC1rZXktdGVzdC1rZXk=")
            .header("sec-websocket-version", "13")
    };
    // Bad token: 401 before upgrade.
    let resp = mk("bogus").send().await.unwrap();
    assert_eq!(resp.status(), 401);
    // Bearer header instead of query param: accepted (101).
    let resp = client
        .get(format!("http://{addr}/v1/tunnel"))
        .header("authorization", format!("Bearer {KEY}"))
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-key", "dGVzdC1rZXktdGVzdC1rZXk=")
        .header("sec-websocket-version", "13")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 101, "Bearer auth upgrade must succeed");

    // allow_tunnel_egress = false: 403.
    let mut config = Config::default();
    config.tunnel.enabled = true;
    config.auth.keys = vec![KeyConfig {
        key: KEY.to_string(),
        allow_tunnel_egress: false,
        ..Default::default()
    }];
    let state = AppState::new(config);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr2 = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, build_router(state)).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let resp = client
        .get(format!("http://{addr2}/v1/tunnel"))
        .header("authorization", format!("Bearer {KEY}"))
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-key", "dGVzdC1rZXktdGVzdC1rZXk=")
        .header("sec-websocket-version", "13")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);

    // Tunnel disabled: 404.
    let mut config = Config::default();
    config.tunnel.enabled = false;
    config.auth.keys = vec![KeyConfig { key: KEY.to_string(), ..Default::default() }];
    let state = AppState::new(config);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr3 = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, build_router(state)).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let resp = client
        .get(format!("http://{addr3}/v1/tunnel"))
        .header("authorization", format!("Bearer {KEY}"))
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-key", "dGVzdC1rZXktdGVzdC1rZXk=")
        .header("sec-websocket-version", "13")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}
