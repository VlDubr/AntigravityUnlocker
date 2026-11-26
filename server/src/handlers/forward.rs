//! Streaming pass-through forward handler (router fallback).
//!
//! Forwards any unmatched `/v1/*`, `/v1internal/*` or `/v1internal:*`
//! request to the configured upstream: normalizes the gateway path, selects
//! direct or tunneled egress, injects the Google OAuth token, strips
//! hop-by-hop and privacy-sensitive headers, and streams the upstream body
//! back with zero buffering.

use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::auth::AuthenticatedKey;
use crate::config::KeyConfig;
use crate::error::AppError;
use crate::tunnel::ConnectProxy;
use crate::upstream::{
    build_target_url, host_of, normalize_path, select_transport, strip_token_query, Transport,
};
use crate::AppState;

/// Client headers never forwarded upstream: credentials, connection-control,
/// proxy/provenance, and headers reqwest must own (content-length,
/// transfer-encoding, accept-encoding).
const STRIP_REQUEST_HEADERS: &[&str] = &[
    "authorization",
    "host",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
    "forwarded",
    "x-forwarded-for",
    "x-real-ip",
    "cf-connecting-ip",
    "true-client-ip",
    "accept-encoding",
];

/// Response headers never forwarded downstream: hop-by-hop, framing
/// (content-length is invalid once the body may be re-chunked), and alt-svc.
const STRIP_RESPONSE_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
    "alt-svc",
];

/// Fallback handler: matches any path not covered by explicit routes.
pub async fn forward(
    State(state): State<AppState>,
    auth: AuthenticatedKey,
    req: axum::extract::Request,
) -> Result<Response, AppError> {
    let (parts, body) = req.into_parts();
    let uri = parts.uri;
    let path = uri.path().to_string();
    // Never forward the gateway `?token=` key upstream (credential leak).
    let query = strip_token_query(uri.query());

    if !path_is_forwardable(&path, state.config.upstream.passthrough_unknown_v1) {
        return Err(AppError::NotFound);
    }

    let normalized = normalize_path(&path, state.config.upstream.normalize_v1internal_colon);
    let host = host_of(&state.config.upstream.base_url);
    let transport = select_transport(&state.config, &state.tunnel, &auth.0, &host);
    let client = match transport {
        Transport::Direct => state.http.clone(),
        Transport::Tunnel => tunnel_client(&state, &auth.0).await?,
    };
    let target = build_target_url(&state.config.upstream.base_url, &normalized, query.as_deref());

    let body_bytes = to_bytes(body, state.config.server.max_body_bytes)
        .await
        .map_err(|e| AppError::BadRequest(format!("failed to read request body: {e}")))?;

    let base_headers = build_forward_headers(&state, &parts.headers, &normalized, query.as_deref());

    let method =
        reqwest::Method::from_bytes(parts.method.as_str().as_bytes()).map_err(|_| {
            AppError::BadRequest(format!("unsupported method: {}", parts.method))
        })?;

    let account_id = auth.0.account_id.clone();
    let session_key = if account_id == "*" && !body_bytes.is_empty() {
        extract_native_session_id(&body_bytes)
            .map(|sid| format!("{}:{}:{sid}", auth.0.key.len(), auth.0.key))
    } else {
        None
    };
    let (token, generation, concrete_account) = state
        .oauth
        .get_token_for_session(&account_id, session_key.as_deref())
        .await?;
    let mut resp = send_upstream(&client, method.clone(), &target, &base_headers, &token, &body_bytes)
        .await?;

    // Single-retry on upstream 401: drop the token we just used and refresh.
    // Invalidate by the CONCRETE account id (resolved from "*" round-robin),
    // not the pseudo-id, or invalidation no-ops and the retry reuses the
    // rejected token.
    if resp.status() == StatusCode::UNAUTHORIZED {
        state
            .oauth
            .invalidate_if_generation(&concrete_account, generation)
            .await;
        let (fresh, _, _) = state
            .oauth
            .get_token_for_session(&account_id, session_key.as_deref())
            .await?;
        resp = send_upstream(&client, method, &target, &base_headers, &fresh, &body_bytes).await?;
    }
    if resp.status() == StatusCode::TOO_MANY_REQUESTS {
        if let Some(sk) = session_key.as_deref() {
            state.oauth.unpin_session(sk, &concrete_account).await;
        }
    }

    Ok(build_client_response(resp, &normalized))
}

fn extract_native_session_id(body: &[u8]) -> Option<String> {
    let val: serde_json::Value = serde_json::from_slice(body).ok()?;
    val.get("request")
        .and_then(|r| r.get("sessionId"))
        .or_else(|| val.get("sessionId"))
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Whether a raw request path may be forwarded upstream.
fn path_is_forwardable(path: &str, passthrough_unknown_v1: bool) -> bool {
    if path.starts_with("/v1internal:") || path.starts_with("/v1internal/") {
        return true;
    }
    passthrough_unknown_v1 && path.starts_with("/v1/")
}

/// Copy client headers, dropping hop-by-hop/privacy headers, then apply
/// upstream overrides (user agent, configured extras, SSE accept).
fn build_forward_headers(
    state: &AppState,
    incoming: &HeaderMap,
    normalized_path: &str,
    query: Option<&str>,
) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in incoming.iter() {
        if !STRIP_REQUEST_HEADERS.contains(&name.as_str()) {
            out.append(name.clone(), value.clone());
        }
    }
    if !state.config.upstream.user_agent.is_empty() {
        out.insert(
            header::USER_AGENT,
            HeaderValue::from_str(&state.config.upstream.user_agent)
                .unwrap_or_else(|_| HeaderValue::from_static("antigravity-gateway")),
        );
    }
    for (name, value) in &state.config.upstream.extra_headers {
        match (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) {
            (Ok(n), Ok(v)) => {
                out.insert(n, v);
            }
            _ => {
                tracing::warn!(name = %name, "ignoring invalid upstream extra_header");
            }
        }
    }
    let wants_sse = query.is_some_and(|q| q.split('&').any(|p| p == "alt=sse"))
        || normalized_path.contains("streamGenerateContent");
    if wants_sse && !out.contains_key(header::ACCEPT) {
        out.insert(header::ACCEPT, HeaderValue::from_static("text/event-stream"));
    }
    out
}

/// One upstream attempt with the given bearer token.
async fn send_upstream(
    client: &reqwest::Client,
    method: reqwest::Method,
    target: &str,
    base_headers: &HeaderMap,
    token: &str,
    body: &[u8],
) -> Result<reqwest::Response, AppError> {
    let mut headers = base_headers.clone();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| AppError::Internal("invalid token characters".to_string()))?,
    );
    client
        .request(method, target)
        .headers(headers)
        .body(body.to_vec())
        .send()
        .await
        .map_err(|e| AppError::BadGateway(format!("upstream request failed: {e}")))
}

/// Translate the upstream response for the client: strip hop-by-hop
/// headers, add SSE anti-buffering hints, and stream the body through
/// untouched (zero buffering, chunked transfer).
fn build_client_response(resp: reqwest::Response, normalized_path: &str) -> Response {
    let status = resp.status();
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let mut out = HeaderMap::new();
    for (name, value) in resp.headers().iter() {
        if !STRIP_RESPONSE_HEADERS.contains(&name.as_str()) {
            out.append(name.clone(), value.clone());
        }
    }
    let is_stream = content_type.starts_with("text/event-stream")
        || normalized_path.contains("streamGenerateContent")
        || normalized_path.contains("loadCodeAssist")
        || normalized_path.contains("chat");
    if is_stream {
        out.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        out.insert(
            HeaderName::from_static("x-accel-buffering"),
            HeaderValue::from_static("no"),
        );
    }
    out.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );

    let mut builder = Response::builder().status(status);
    if let Some(headers) = builder.headers_mut() {
        *headers = out;
    }
    let body = Body::from_stream(resp.bytes_stream());
    builder
        .body(body)
        .map(IntoResponse::into_response)
        .unwrap_or_else(|e| {
            AppError::Internal(format!("failed to build upstream response: {e}")).into_response()
        })
}

/// Get (lazily spawning and caching per API key) the loopback CONNECT proxy
/// reqwest client that egresses through this key's tunnel workers.
async fn tunnel_client(state: &AppState, key: &KeyConfig) -> Result<reqwest::Client, AppError> {
    let mut proxies = state.tunnel_proxies.lock().await;
    if let Some(proxy) = proxies.get(&key.key) {
        return Ok(proxy.reqwest_client());
    }
    let proxy = ConnectProxy::spawn(
        Arc::clone(&state.tunnel),
        key.key.clone(),
        Duration::from_millis(state.config.tunnel.handshake_timeout_ms),
        Duration::from_millis(state.config.tunnel.acquire_timeout_ms),
    )
    .await
    .map_err(AppError::BadGateway)?;
    let proxy = Arc::new(proxy);
    let client = proxy.reqwest_client();
    proxies.insert(key.key.clone(), proxy);
    Ok(client)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(ua: &str, extras: &[(&str, &str)]) -> AppState {
        let mut config = crate::config::Config::default();
        config.upstream.user_agent = ua.to_string();
        for (k, v) in extras {
            config
                .upstream
                .extra_headers
                .insert(k.to_string(), v.to_string());
        }
        AppState::new(config)
    }

    #[test]
    fn forwardable_paths_gate() {
        assert!(path_is_forwardable("/v1internal:streamGenerateContent", true));
        assert!(path_is_forwardable("/v1internal/streamGenerateContent", true));
        assert!(path_is_forwardable("/v1/anything", true));
        assert!(!path_is_forwardable("/v1/anything", false));
        assert!(path_is_forwardable("/v1internal:foo", false));
        assert!(!path_is_forwardable("/health", true));
        assert!(!path_is_forwardable("/v1x/foo", true));
    }

    #[tokio::test]
    async fn strips_privacy_and_hop_headers() {
        let state = cfg("", &[]);
        let mut incoming = HeaderMap::new();
        incoming.insert("x-forwarded-for", "1.2.3.4".parse().unwrap());
        incoming.insert("x-real-ip", "5.6.7.8".parse().unwrap());
        incoming.insert("forwarded", "for=x".parse().unwrap());
        incoming.insert("authorization", "Bearer client-key".parse().unwrap());
        incoming.insert("connection", "close".parse().unwrap());
        incoming.insert("content-length", "12".parse().unwrap());
        incoming.insert("accept-encoding", "gzip".parse().unwrap());
        incoming.insert("x-api-version", "7".parse().unwrap());
        let out = build_forward_headers(&state, &incoming, "/v1internal:x", None);
        assert!(out.get("x-forwarded-for").is_none());
        assert!(out.get("x-real-ip").is_none());
        assert!(out.get("forwarded").is_none());
        assert!(out.get("authorization").is_none());
        assert!(out.get("connection").is_none());
        assert!(out.get("content-length").is_none());
        assert!(out.get("accept-encoding").is_none());
        assert_eq!(out.get("x-api-version").unwrap(), "7");
    }

    #[tokio::test]
    async fn user_agent_override_and_passthrough() {
        let state = cfg("antigravity/2.0", &[]);
        let mut incoming = HeaderMap::new();
        incoming.insert("user-agent", "IDE/1.0".parse().unwrap());
        let out = build_forward_headers(&state, &incoming, "/v1internal:x", None);
        assert_eq!(out.get("user-agent").unwrap(), "antigravity/2.0");

        let state = cfg("", &[]);
        let out = build_forward_headers(&state, &incoming, "/v1internal:x", None);
        assert_eq!(out.get("user-agent").unwrap(), "IDE/1.0");
    }

    #[tokio::test]
    async fn extra_headers_injected_and_sse_accept() {
        let state = cfg("", &[("x-goog-user-project", "proj-1")]);
        let out = build_forward_headers(&state, &HeaderMap::new(), "/v1internal:streamGenerateContent", None);
        assert_eq!(out.get("x-goog-user-project").unwrap(), "proj-1");
        assert_eq!(out.get("accept").unwrap(), "text/event-stream");

        let out = build_forward_headers(&state, &HeaderMap::new(), "/v1internal:x", Some("alt=sse"));
        assert_eq!(out.get("accept").unwrap(), "text/event-stream");

        // Explicit client accept wins.
        let mut incoming = HeaderMap::new();
        incoming.insert("accept", "application/json".parse().unwrap());
        let out = build_forward_headers(&state, &incoming, "/v1internal:streamGenerateContent", None);
        assert_eq!(out.get("accept").unwrap(), "application/json");

        // Non-stream path gets no accept injection.
        let out = build_forward_headers(&state, &HeaderMap::new(), "/v1internal/loadCodeAssist", None);
        assert!(out.get("accept").is_none());
    }
}
