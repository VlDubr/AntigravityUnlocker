//! End-to-end tests for the streaming forward handler: path normalization,
//! header hygiene, token injection/refresh-on-401, and zero-buffer SSE.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use antigravity_server::build_router;
use antigravity_server::config::{AccountConfig, Config, KeyConfig};
use antigravity_server::AppState;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::json;
use tokio::net::TcpListener;

/// What the mock upstream does with forwarded requests.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Echo a small JSON body.
    Normal,
    /// Emit 3 SSE chunks, ~300ms apart, to prove incremental streaming.
    Sse,
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
            0 => ("data: chunk-1\n\n", 1u8),
            1 => ("data: chunk-2\n\n", 2u8),
            2 => ("data: chunk-3\n\n", 3u8),
            _ => return None,
        };
        if next > 1 {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        Some((Ok::<_, std::io::Error>(Bytes::from_static(text.as_bytes())), next))
    });
    Body::from_stream(stream)
}

async fn upstream_handler(State(mock): State<Arc<Mock>>, req: Request) -> axum::response::Response {
    let n = mock.upstream_hits.fetch_add(1, Ordering::SeqCst);
    let (parts, _body) = req.into_parts();
    let headers: Vec<(String, String)> = parts
        .headers
        .iter()
        .map(|(k, v)| (k.as_str().to_lowercase(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
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
            .header("set-cookie", "upstream=1")
            .body(Body::from(json!({"ok": true}).to_string()))
            .unwrap(),
        Mode::Sse => axum::response::Response::builder()
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

fn gateway_config(mock_base: &str) -> Config {
    let mut cfg = Config::default();
    cfg.auth.keys = vec![KeyConfig {
        key: "k1".to_string(),
        account_id: "main".to_string(),
        ..KeyConfig::default()
    }];
    cfg.google.token_url = format!("{mock_base}/token");
    cfg.google.client_id = "cid".to_string();
    cfg.google.accounts = vec![AccountConfig {
        id: "main".to_string(),
        refresh_token: "rt".to_string(),
        project_id: "proj".to_string(),
    }];
    cfg.upstream.base_url = mock_base.to_string();
    cfg.server.state_file = format!(
        "{}/target/ag-fwd-state-{}.json",
        env!("CARGO_MANIFEST_DIR"),
        std::process::id()
    );
    cfg
}

/// Spawn the gateway (build_router) on an ephemeral port; return base URL.
async fn spawn_gateway(cfg: Config) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = build_router(AppState::new(cfg));
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn captured_auth(mock: &Arc<Mock>, idx: usize) -> String {
    let cap = mock.captured.lock().unwrap();
    let (_, _, headers) = &cap[idx];
    headers
        .iter()
        .find(|(k, _)| k == "authorization")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

fn captured_header(mock: &Arc<Mock>, idx: usize, name: &str) -> Option<String> {
    let cap = mock.captured.lock().unwrap();
    cap[idx]
        .2
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone())
}

// (a) All three client path shapes arrive at upstream as the colon form,
//     with query preserved verbatim.
#[tokio::test(flavor = "multi_thread")]
async fn path_shapes_normalized_to_colon_form() {
    let (mock_base, mock) = spawn_mock(Mode::Normal).await;
    let gw = spawn_gateway(gateway_config(&mock_base)).await;
    let client = reqwest::Client::new();
    for input in [
        "/v1/v1internal:streamGenerateContent",
        "/v1internal/streamGenerateContent",
        "/v1internal:streamGenerateContent",
    ] {
        let resp = client
            .post(format!("{gw}{input}?alt=sse"))
            .header("Authorization", "Bearer k1")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "input {input}");
    }
    let cap = mock.captured.lock().unwrap();
    assert_eq!(cap.len(), 3);
    for (path, query, _) in cap.iter() {
        assert_eq!(path, "/v1internal:streamGenerateContent");
        assert_eq!(query.as_deref(), Some("alt=sse"));
    }
}

// (b) Upstream sees the Google OAuth bearer, not the client gateway key.
// (c) Upstream sees no x-forwarded-for / provenance headers.
#[tokio::test(flavor = "multi_thread")]
async fn token_injected_and_provenance_stripped() {
    let (mock_base, mock) = spawn_mock(Mode::Normal).await;
    let gw = spawn_gateway(gateway_config(&mock_base)).await;
    let resp = reqwest::Client::new()
        .post(format!("{gw}/v1internal:generateContent"))
        .header("Authorization", "Bearer k1")
        .header("X-Forwarded-For", "203.0.113.9")
        .header("X-Real-IP", "203.0.113.9")
        .header("True-Client-IP", "203.0.113.9")
        .header("Forwarded", "for=203.0.113.9")
        .header("Cf-Connecting-Ip", "203.0.113.9")
        .header("X-Api-Version", "7")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // (b) exactly one token refresh; bearer is the Google token.
    assert_eq!(captured_auth(&mock, 0), "Bearer ya29.t1");
    // (c) no provenance headers leaked, unrelated headers passed through.
    for h in ["x-forwarded-for", "x-real-ip", "true-client-ip", "forwarded", "cf-connecting-ip"] {
        assert!(captured_header(&mock, 0, h).is_none(), "{h} leaked upstream");
    }
    assert_eq!(captured_header(&mock, 0, "x-api-version").as_deref(), Some("7"));
    assert_ne!(
        captured_header(&mock, 0, "accept").as_deref(),
        Some("text/event-stream")
    );
}

// (d) SSE chunks reach the client incrementally (zero buffering).
#[tokio::test(flavor = "multi_thread")]
async fn sse_streams_incrementally() {
    let (mock_base, _mock) = spawn_mock(Mode::Sse).await;
    let gw = spawn_gateway(gateway_config(&mock_base)).await;
    let resp = reqwest::Client::new()
        .post(format!("{gw}/v1internal:streamGenerateContent?alt=sse"))
        .header("Authorization", "Bearer k1")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()["content-type"],
        "text/event-stream",
        "SSE content-type preserved"
    );
    assert_eq!(resp.headers()["cache-control"], "no-cache");
    assert_eq!(resp.headers()["x-accel-buffering"], "no");
    assert_eq!(resp.headers()["access-control-allow-origin"], "*");

    let start = Instant::now();
    let mut arrivals = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        arrivals.push((start.elapsed(), chunk.unwrap()));
    }
    let all: Vec<u8> = arrivals.iter().flat_map(|(_, b)| b.iter().copied()).collect();
    let text = String::from_utf8(all).unwrap();
    assert_eq!(text, "data: chunk-1\n\ndata: chunk-2\n\ndata: chunk-3\n\n");
    assert!(!arrivals.is_empty());
    // Mock emits at ~0ms/300ms/600ms: a buffering gateway would deliver the
    // first bytes only after ~600ms. Incremental => first arrival much
    // earlier, last arrival near the end.
    let first = arrivals.first().unwrap().0;
    let last = arrivals.last().unwrap().0;
    assert!(
        first < Duration::from_millis(400),
        "first chunk arrived too late ({first:?}) — response is being buffered"
    );
    assert!(last >= Duration::from_millis(500), "last chunk arrived too early ({last:?})");
}

// (e) Upstream 401 -> token invalidated + refreshed, retried once -> 200.
#[tokio::test(flavor = "multi_thread")]
async fn upstream_401_triggers_single_retry_with_fresh_token() {
    let (mock_base, mock) = spawn_mock(Mode::DenyOnce).await;
    let gw = spawn_gateway(gateway_config(&mock_base)).await;
    let resp = reqwest::Client::new()
        .post(format!("{gw}/v1internal:generateContent"))
        .header("Authorization", "Bearer k1")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "401 must be retried transparently");
    assert_eq!(mock.upstream_hits.load(Ordering::SeqCst), 2);
    assert_eq!(mock.token_hits.load(Ordering::SeqCst), 2, "exactly one refresh after 401");
    assert_eq!(captured_auth(&mock, 0), "Bearer ya29.t1");
    assert_eq!(captured_auth(&mock, 1), "Bearer ya29.t2");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["retry"], true);
}

// Fallback gating: unknown non-v1 paths 404; /v1/* honors
// passthrough_unknown_v1; missing auth is 401 before any forwarding.
#[tokio::test(flavor = "multi_thread")]
async fn fallback_gating_and_auth() {
    let (mock_base, mock) = spawn_mock(Mode::Normal).await;
    let gw = spawn_gateway(gateway_config(&mock_base)).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{gw}/nope"))
        .header("Authorization", "Bearer k1")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let resp = client
        .get(format!("{gw}/v1/whatever"))
        .header("Authorization", "Bearer k1")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "/v1/* passthrough is on by default");

    let resp = client.get(format!("{gw}/v1internal:generateContent")).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "auth runs before forwarding");
    assert_eq!(mock.upstream_hits.load(Ordering::SeqCst), 1, "only the /v1/whatever call");
}
