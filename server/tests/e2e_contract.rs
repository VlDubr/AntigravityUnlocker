//! End-to-end acceptance-contract suite (T6).
//!
//! Serves the real `build_router` on ephemeral loopback ports against a mock
//! upstream + mock OAuth `/token` endpoint and verifies all 8 acceptance
//! criteria (AC1..AC8). Mock helpers are replicated from `tests/forward.rs`
//! and `tests/tunnel.rs` (they are private to their test binaries). Timing
//! bounds are deliberately generous to avoid flakiness.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use antigravity_server::config::{AccountConfig, AssetConfig, Config, KeyConfig, UpdatesConfig};
use antigravity_server::tunnel::spawn_connect_proxy;
use antigravity_server::{build_router, AppState};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

const GATEWAY_KEY: &str = "e2e-contract-key";

/// What the mock upstream does with forwarded requests.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Echo a small JSON body.
    Normal,
    /// Emit 3 SSE chunks ~100ms apart, to prove incremental streaming.
    Sse100,
    /// First forwarded request answers 401, second answers 200.
    DenyOnce,
}

struct Mock {
    mode: Mode,
    token_hits: AtomicUsize,
    upstream_hits: AtomicUsize,
    /// Captured (path, query, headers) per forwarded request.
    captured: Mutex<Vec<(String, Option<String>, Vec<(String, String)>)>>,
}

async fn token_handler(State(mock): State<Arc<Mock>>) -> Json<serde_json::Value> {
    let n = mock.token_hits.fetch_add(1, Ordering::SeqCst) + 1;
    Json(json!({
        "access_token": format!("ya29.t{n}"),
        "expires_in": 3600u64,
    }))
}

fn sse_body() -> Body {
    let stream = futures_util::stream::unfold(0u8, |i| async move {
        let (text, next) = match i {
            0 => ("data: c1\n\n", 1u8),
            1 => ("data: c2\n\n", 2u8),
            2 => ("data: c3\n\n", 3u8),
            _ => return None,
        };
        if next > 1 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Some((Ok::<_, std::io::Error>(Bytes::from_static(text.as_bytes())), next))
    });
    Body::from_stream(stream)
}

async fn upstream_handler(
    State(mock): State<Arc<Mock>>,
    req: Request,
) -> axum::response::Response {
    let n = mock.upstream_hits.fetch_add(1, Ordering::SeqCst);
    let (parts, _body) = req.into_parts();
    let headers: Vec<(String, String)> = parts
        .headers
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_lowercase(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    mock.captured.lock().unwrap().push((
        parts.uri.path().to_string(),
        parts.uri.query().map(str::to_string),
        headers,
    ));
    match mock.mode {
        Mode::Normal => axum::response::Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(Body::from(json!({"ok": true}).to_string()))
            .unwrap(),
        Mode::Sse100 => axum::response::Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(sse_body())
            .unwrap(),
        Mode::DenyOnce => {
            if n == 0 {
                axum::response::Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .body(Body::from("{\"error\":\"token expired\"}"))
                    .unwrap()
            } else {
                axum::response::Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"ok": true, "retry": true}).to_string()))
                    .unwrap()
            }
        }
    }
}

/// Spawn mock upstream + mock OAuth token endpoint on one ephemeral port.
async fn spawn_mock(mode: Mode) -> (String, Arc<Mock>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mock = Arc::new(Mock {
        mode,
        token_hits: AtomicUsize::new(0),
        upstream_hits: AtomicUsize::new(0),
        captured: Mutex::new(Vec::new()),
    });
    let app = Router::new()
        .route("/token", post(token_handler))
        .fallback(upstream_handler)
        .with_state(mock.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), mock)
}

fn temp_state_path(name: &str) -> String {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("target");
    std::fs::create_dir_all(&p).ok();
    p.push(format!("ag-e2e-{name}-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p.to_string_lossy().into_owned()
}

/// Baseline gateway config: one auth key, mock oauth/upstream, isolated state file.
fn base_config(name: &str, mock_base: &str) -> Config {
    let mut cfg = Config::default();
    cfg.auth.keys = vec![KeyConfig {
        key: GATEWAY_KEY.to_string(),
        ..KeyConfig::default()
    }];
    cfg.google.token_url = format!("{mock_base}/token");
    cfg.google.client_id = "cid".to_string();
    cfg.google.accounts = vec![AccountConfig {
        id: "main".to_string(),
        refresh_token: "rt".to_string(),
        project_id: String::new(),
    }];
    cfg.upstream.base_url = mock_base.to_string();
    cfg.server.state_file = temp_state_path(name);
    cfg
}

/// Serve the real router on an ephemeral port. Returns (http base URL, addr, state).
async fn spawn_gateway(cfg: Config) -> (String, SocketAddr, AppState) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = AppState::new(cfg);
    let app = build_router(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), addr, state)
}

fn captured_header(mock: &Arc<Mock>, idx: usize, name: &str) -> Option<String> {
    let cap = mock.captured.lock().unwrap();
    cap[idx]
        .2
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone())
}

async fn wait_until<F: Fn() -> bool>(what: &str, secs: u64, f: F) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out after {secs}s waiting for: {what}");
}

// ---------------------------------------------------------------------------
// AC1: GET /health, no auth, 200 {"status":"ok"}, fast.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread")]
async fn ac1_health_no_auth_ok_fast() {
    let (mock_base, _mock) = spawn_mock(Mode::Normal).await;
    let (gw, _addr, _state) = spawn_gateway(base_config("ac1", &mock_base)).await;
    let start = Instant::now();
    let resp = reqwest::Client::new().get(format!("{gw}/health")).send().await.unwrap();
    let elapsed = start.elapsed();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["status"], "ok");
    assert!(elapsed < Duration::from_millis(2000), "health took {elapsed:?}");
}

// ---------------------------------------------------------------------------
// AC2: GET /v1/client-update unconfigured + configured; download streams
// exact bytes; unknown asset id 404. All public (no auth).
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread")]
async fn ac2_client_update_unconfigured() {
    let (mock_base, _mock) = spawn_mock(Mode::Normal).await;
    let (gw, _addr, _state) = spawn_gateway(base_config("ac2u", &mock_base)).await;
    let resp = reqwest::Client::new()
        .get(format!("{gw}/v1/client-update?platform=windows"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["version"], "");
    assert_eq!(v["name"], "");
    assert_eq!(v["body"], "");
    assert_eq!(v["asset_id"], "none");
}

#[tokio::test(flavor = "multi_thread")]
async fn ac2_client_update_configured_and_download() {
    let mut file = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    file.push("target");
    file.push(format!("ag-e2e-asset-{}.bin", std::process::id()));
    let payload: Vec<u8> = (0u32..4096).map(|i| (i % 251) as u8).collect();
    std::fs::write(&file, &payload).unwrap();

    let (mock_base, _mock) = spawn_mock(Mode::Normal).await;
    let mut cfg = base_config("ac2c", &mock_base);
    let mut assets = HashMap::new();
    assets.insert(
        "windows".to_string(),
        AssetConfig {
            id: "e2e-win".to_string(),
            path: file.to_string_lossy().into_owned(),
        },
    );
    cfg.updates = UpdatesConfig {
        version: "9.9.9".to_string(),
        name: "e2e".to_string(),
        body: "notes".to_string(),
        assets,
    };
    let (gw, _addr, _state) = spawn_gateway(cfg).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{gw}/v1/client-update?platform=windows"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["version"], "9.9.9");
    assert_eq!(v["name"], "e2e");
    assert_eq!(v["body"], "notes");
    assert_eq!(v["asset_id"], "e2e-win");
    assert_eq!(v["size"], payload.len() as u64);

    let resp = client
        .get(format!("{gw}/v1/client-update/download/e2e-win"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let got = resp.bytes().await.unwrap();
    assert_eq!(&got[..], &payload[..], "downloaded bytes must match exactly");

    let resp = client
        .get(format!("{gw}/v1/client-update/download/nope"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    std::fs::remove_file(&file).ok();
}

// ---------------------------------------------------------------------------
// AC3: GET /v1/models - 401 JSON on bad/missing Bearer; >=29 models on valid.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread")]
async fn ac3_models_auth_and_list() {
    let (mock_base, _mock) = spawn_mock(Mode::Normal).await;
    let (gw, _addr, _state) = spawn_gateway(base_config("ac3", &mock_base)).await;
    let client = reqwest::Client::new();

    for (label, builder) in [
        ("missing", client.get(format!("{gw}/v1/models"))),
        (
            "bad",
            client
                .get(format!("{gw}/v1/models"))
                .header("Authorization", "Bearer wrong-key"),
        ),
    ] {
        let resp = builder.send().await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{label} bearer");
        let v: Value = resp.json().await.unwrap();
        assert!(
            v["error"]["message"].is_string(),
            "{label}: expected error.message JSON, got {v}"
        );
    }

    let resp = client
        .get(format!("{gw}/v1/models"))
        .header("Authorization", format!("Bearer {GATEWAY_KEY}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["object"], "list");
    let data = v["data"].as_array().unwrap();
    assert!(data.len() >= 29, "expected >=29 models, got {}", data.len());
    assert!(data[0]["id"].is_string());
    assert_eq!(data[0]["object"], "model");
    assert_eq!(data[0]["owned_by"], "antigravity");
}

// ---------------------------------------------------------------------------
// AC4: GET /v1/quota shape; POST /v1/quota/config flips, GET reflects.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread")]
async fn ac4_quota_shape_and_config_flip() {
    let (mock_base, _mock) = spawn_mock(Mode::Normal).await;
    let (gw, _addr, _state) = spawn_gateway(base_config("ac4", &mock_base)).await;
    let client = reqwest::Client::new();
    let auth = format!("Bearer {GATEWAY_KEY}");

    let resp = client.get(format!("{gw}/v1/quota")).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "quota requires auth");

    let resp = client
        .get(format!("{gw}/v1/quota"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["tier"], 3);
    assert_eq!(v["total_credits"], 9999);
    assert!(v["expires_at"].is_null(), "expires_at default is null");
    assert_eq!(v["enable_credit_overages"], false);
    assert!(v["models"].is_object());
    assert!(v["resets"].is_object());

    // Flip ON, then verify GET reflects it.
    let resp = client
        .post(format!("{gw}/v1/quota/config"))
        .header("Authorization", &auth)
        .json(&json!({"enable_credit_overages": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["enable_credit_overages"], true);
    let v: Value = client
        .get(format!("{gw}/v1/quota"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["enable_credit_overages"], true);

    // Flip OFF again.
    let resp = client
        .post(format!("{gw}/v1/quota/config"))
        .header("Authorization", &auth)
        .json(&json!({"enable_credit_overages": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: Value = client
        .get(format!("{gw}/v1/quota"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["enable_credit_overages"], false);

    // Invalid body -> 400; no auth -> 401.
    let resp = client
        .post(format!("{gw}/v1/quota/config"))
        .header("Authorization", &auth)
        .json(&json!({"other": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let resp = client
        .post(format!("{gw}/v1/quota/config"))
        .json(&json!({"enable_credit_overages": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// AC5: path normalization - all three client path shapes reach the upstream
// as /v1internal:streamGenerateContent?alt=sse, with the Google bearer and
// no provenance headers.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread")]
async fn ac5_path_normalization_token_and_no_xff() {
    let (mock_base, mock) = spawn_mock(Mode::Normal).await;
    let (gw, _addr, _state) = spawn_gateway(base_config("ac5", &mock_base)).await;
    let client = reqwest::Client::new();
    for input in [
        "/v1/v1internal:streamGenerateContent?alt=sse",
        "/v1internal/streamGenerateContent?alt=sse",
        "/v1internal:streamGenerateContent?alt=sse",
    ] {
        let resp = client
            .post(format!("{gw}{input}"))
            .header("Authorization", format!("Bearer {GATEWAY_KEY}"))
            .header("X-Forwarded-For", "203.0.113.9")
            .header("X-Real-IP", "203.0.113.9")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "input {input}");
    }
    {
        let cap = mock.captured.lock().unwrap();
        assert_eq!(cap.len(), 3);
        for (path, query, _) in cap.iter() {
            assert_eq!(path, "/v1internal:streamGenerateContent");
            assert_eq!(query.as_deref(), Some("alt=sse"));
        }
    }
    for i in 0..3 {
        let auth = captured_header(&mock, i, "authorization").unwrap_or_default();
        assert!(
            auth.starts_with("Bearer ya29.t"),
            "upstream must see Google token, got {auth}"
        );
        assert_ne!(auth, format!("Bearer {GATEWAY_KEY}"), "client key leaked upstream");
        assert!(captured_header(&mock, i, "x-forwarded-for").is_none(), "xff leaked");
        assert!(captured_header(&mock, i, "x-real-ip").is_none(), "x-real-ip leaked");
    }
}

// ---------------------------------------------------------------------------
// AC6: SSE pass-through streams incrementally (mock chunks ~100ms apart;
// first client arrival must be early AND clearly before stream end, proving
// no buffering to EOF).
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread")]
async fn ac6_sse_streams_incrementally() {
    let (mock_base, _mock) = spawn_mock(Mode::Sse100).await;
    let (gw, _addr, _state) = spawn_gateway(base_config("ac6", &mock_base)).await;
    let resp = reqwest::Client::new()
        .post(format!("{gw}/v1internal:streamGenerateContent?alt=sse"))
        .header("Authorization", format!("Bearer {GATEWAY_KEY}"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    assert_eq!(resp.headers()["x-accel-buffering"], "no");

    let start = Instant::now();
    let mut arrivals = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        arrivals.push((start.elapsed(), chunk.unwrap()));
    }
    let all: Vec<u8> = arrivals.iter().flat_map(|(_, b)| b.iter().copied()).collect();
    assert_eq!(
        String::from_utf8(all).unwrap(),
        "data: c1\n\ndata: c2\n\ndata: c3\n\n"
    );
    assert!(!arrivals.is_empty());
    let first = arrivals.first().unwrap().0;
    let last = arrivals.last().unwrap().0;
    // Mock emits at ~0/100/200ms. A buffering proxy would deliver everything
    // at once near ~200ms: first==last. Incremental streaming requires the
    // first bytes early AND a clear gap between first and last arrival.
    assert!(first < Duration::from_millis(400), "first chunk late ({first:?}) => buffered");
    assert!(
        last.saturating_sub(first) >= Duration::from_millis(100),
        "first ({first:?}) and last ({last:?}) arrived together => buffered to EOF"
    );
}

// ---------------------------------------------------------------------------
// AC7: upstream 401 -> oauth /token refresh + transparent replay. Token
// endpoint hit count must be exactly 2 (initial + one refresh after 401).
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread")]
async fn ac7_upstream_401_refresh_and_replay() {
    let (mock_base, mock) = spawn_mock(Mode::DenyOnce).await;
    let (gw, _addr, _state) = spawn_gateway(base_config("ac7", &mock_base)).await;
    let resp = reqwest::Client::new()
        .post(format!("{gw}/v1internal:generateContent"))
        .header("Authorization", format!("Bearer {GATEWAY_KEY}"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "401 must be retried transparently");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["retry"], true);
    assert_eq!(mock.upstream_hits.load(Ordering::SeqCst), 2);
    assert_eq!(
        mock.token_hits.load(Ordering::SeqCst),
        2,
        "token endpoint must be hit exactly twice (initial + post-401 refresh)"
    );
    assert_eq!(
        captured_header(&mock, 0, "authorization").as_deref(),
        Some("Bearer ya29.t1")
    );
    assert_eq!(
        captured_header(&mock, 1, "authorization").as_deref(),
        Some("Bearer ya29.t2")
    );
}

// ---------------------------------------------------------------------------
// AC8: 12 simulated tunnel workers (src-tauri/src/tunnel.rs protocol) stay
// alive across ping cycles; routing_mode="always" routes a forward request
// through a worker; only Binary frames after CONNECTED; pool replenishes
// after the session EOF.
// ---------------------------------------------------------------------------

/// Local echo TCP server; returns its address.
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

/// One worker session replicating `src-tauri/src/tunnel.rs`: connect, wait
/// for `CONNECT {host} {port}`, TCP to the requested target, answer
/// CONNECTED, bridge Binary<->TCP. Logs protocol events.
async fn worker_session(url: &str, log: Arc<Mutex<Vec<String>>>) {
    let (ws, _resp) = connect_async(url).await.expect("worker ws connect");
    let (mut write, mut read) = ws.split();

    // Idle phase: wait for the CONNECT command (tungstenite answers server
    // Pings with Pongs automatically, like the real client).
    let cmd = loop {
        match read.next().await {
            Some(Ok(Message::Text(t))) => {
                if t.starts_with("CONNECT ") {
                    break t.as_str().to_string();
                }
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => panic!("worker idle read error: {e}"),
            None => panic!("server closed before CONNECT"),
        }
    };
    log.lock().unwrap().push(format!("connect {cmd}"));
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    assert_eq!(parts.len(), 3, "CONNECT command must have 3 tokens: {cmd}");
    let host = parts[1].to_string();
    let port: u16 = parts[2].parse().expect("CONNECT port");

    let tcp = TcpStream::connect((host.as_str(), port)).await.expect("worker target connect");
    write
        .send(Message::Text("CONNECTED".to_string().into()))
        .await
        .unwrap();
    log.lock().unwrap().push("connected".to_string());

    let (mut tr, mut tw) = tcp.into_split();
    let log_ws = Arc::clone(&log);
    let ws_to_tcp = tokio::spawn(async move {
        // Exact client shape: any non-Binary frame ends the bridge.
        let mut read = read;
        loop {
            match read.next().await {
                Some(Ok(Message::Binary(data))) => {
                    log_ws.lock().unwrap().push("binary".to_string());
                    if tw.write_all(&data).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) => {
                    log_ws.lock().unwrap().push("close".to_string());
                    break;
                }
                Some(Ok(other)) => {
                    let _ = other;
                    log_ws.lock().unwrap().push("VIOLATION".to_string());
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
    // One-shot lifecycle: client drops the socket after a session.
    let _ = write.close().await;
}

/// Real client reconnect loop (5s backoff in production; 50ms here).
async fn worker_loop(url: String, log: Arc<Mutex<Vec<String>>>) {
    loop {
        worker_session(&url, Arc::clone(&log)).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ac8_twelve_workers_survive_pings_and_route_through_tunnel() {
    let (mock_base, mock) = spawn_mock(Mode::Normal).await;
    let echo = spawn_echo().await;

    let mut cfg = base_config("ac8", &mock_base);
    cfg.tunnel.enabled = true;
    cfg.tunnel.routing_mode = "always".to_string();
    cfg.tunnel.idle_ping_secs = 1;
    cfg.tunnel.handshake_timeout_ms = 5_000;
    cfg.tunnel.acquire_timeout_ms = 4_000;
    // The worker TCP-connects to this local echo target when the bridge
    // issues `CONNECT 127.0.0.1 <echo_port>`.
    cfg.upstream.base_url = format!("https://127.0.0.1:{}", echo.port());

    let (gw, addr, state) = spawn_gateway(cfg).await;
    let url = format!("ws://{addr}/v1/tunnel?token={GATEWAY_KEY}");

    let mut logs = Vec::new();
    for _ in 0..12 {
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let u = url.clone();
        tokio::spawn(worker_loop(u, Arc::clone(&log)));
        logs.push(log);
    }
    wait_until("12 workers parked", 15, || {
        state.tunnel.idle_count(GATEWAY_KEY) == 12
    })
    .await;

    // Survive several idle-ping cycles (ping interval is 1s).
    tokio::time::sleep(Duration::from_millis(2600)).await;
    assert_eq!(
        state.tunnel.idle_count(GATEWAY_KEY),
        12,
        "workers must stay alive across ping cycles"
    );
    for log in &logs {
        let l = log.lock().unwrap();
        assert!(l.iter().all(|e| e != "binary"), "no Binary during idle: {l:?}");
        assert!(l.iter().all(|e| e != "VIOLATION"), "no protocol violation: {l:?}");
    }

    // Route a forward request through the tunnel. The egress channel is the
    // worker; TLS to the plain echo target fails on the client side, so the
    // gateway answers 502 - but the request MUST have been routed through a
    // worker, and MUST NOT have egressed directly.
    let resp = tokio::time::timeout(
        Duration::from_secs(20),
        reqwest::Client::new()
            .post(format!("{gw}/v1internal:generateContent"))
            .header("Authorization", format!("Bearer {GATEWAY_KEY}"))
            .body("{}")
            .send(),
    )
    .await
    .expect("forward request hung")
    .expect("forward request transport error");
    assert_eq!(
        resp.status(),
        StatusCode::BAD_GATEWAY,
        "tunneled TLS to a non-TLS echo target must fail end-to-end (proof of tunnel egress)"
    );
    assert_eq!(mock.upstream_hits.load(Ordering::SeqCst), 0, "no direct egress allowed");

    // At least one worker received the CONNECT command for the echo target
    // and bridged Binary frames afterwards.
    let want = format!("connect CONNECT 127.0.0.1 {}", echo.port());
    wait_until("a worker got the CONNECT command for the echo target", 10, || {
        logs.iter()
            .any(|l| l.lock().unwrap().iter().any(|e| *e == want.as_str()))
    })
    .await;
    wait_until("Binary frames flowed through a worker", 10, || {
        logs.iter()
            .any(|l| l.lock().unwrap().iter().any(|e| *e == "binary"))
    })
    .await;
    for log in &logs {
        let l = log.lock().unwrap();
        assert!(
            !l.iter().any(|e| e == "VIOLATION"),
            "non-Binary frame after CONNECTED: {l:?}"
        );
    }

    // Pool replenishes: consumed workers drop and reconnect (client loop).
    wait_until("pool replenished to 12 workers", 20, || {
        state.tunnel.idle_count(GATEWAY_KEY) == 12
    })
    .await;
}

/// The tunnel data plane itself: a manual CONNECT through the loopback
/// bridge round-trips arbitrary bytes via a worker (success path), binary
/// only after CONNECTED, and the pool refills after EOF.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ac8_bridge_roundtrip_binary_only_and_pool_replenish() {
    let (mock_base, _mock) = spawn_mock(Mode::Normal).await;
    let echo = spawn_echo().await;
    let mut cfg = base_config("ac8b", &mock_base);
    cfg.tunnel.enabled = true;
    cfg.tunnel.idle_ping_secs = 1;
    let (_gw, addr, state) = spawn_gateway(cfg).await;

    let proxy = spawn_connect_proxy(&state).await.expect("bridge spawn");
    let url = format!("ws://{addr}/v1/tunnel?token={GATEWAY_KEY}");
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let log2 = Arc::clone(&log);
    let u = url.clone();
    tokio::spawn(async move { worker_loop(u, log2).await });
    wait_until("worker parked", 15, || state.tunnel.idle_count(GATEWAY_KEY) == 1).await;

    let mut sock = TcpStream::connect(proxy.local_addr()).await.unwrap();
    let creds =
        base64::engine::general_purpose::STANDARD.encode(format!("x:{}", proxy.secret()));
    sock.write_all(
        format!(
            "CONNECT 127.0.0.1:{} HTTP/1.1\r\nProxy-Authorization: Basic {creds}\r\n\r\n",
            echo.port()
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut head = [0u8; 39];
    sock.read_exact(&mut head).await.unwrap();
    assert!(
        std::str::from_utf8(&head).unwrap().starts_with("HTTP/1.1 200"),
        "bridge must accept authenticated CONNECT: {:?}",
        std::str::from_utf8(&head)
    );
    for payload in [b"e2e-via-tunnel".to_vec(), vec![0x5Au8; 30_000]] {
        sock.write_all(&payload).await.unwrap();
        let mut got = vec![0u8; payload.len()];
        sock.read_exact(&mut got).await.unwrap();
        assert_eq!(got, payload);
    }
    // EOF: end the bridge session.
    drop(sock);

    // Session must have bridged Binary frames with no protocol violation.
    wait_until("session bridged Binary frames", 10, || {
        log.lock().unwrap().iter().any(|e| *e == "binary")
    })
    .await;
    let l = log.lock().unwrap();
    assert!(!l.iter().any(|e| e == "VIOLATION"), "non-Binary after CONNECTED: {l:?}");
    drop(l);

    // Pool refills after the worker reconnects.
    wait_until("pool replenished after EOF", 20, || {
        state.tunnel.idle_count(GATEWAY_KEY) == 1
    })
    .await;
}
