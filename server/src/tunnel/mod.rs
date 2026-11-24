//! Reverse WebSocket tunnel: desktop workers connect to `/v1/tunnel` and
//! wait for `CONNECT {host} {port}` commands, giving the server egress
//! through the worker's network. Matches the client in
//! `src-tauri/src/tunnel.rs` exactly.
//!
//! - `registry`: pool of idle worker sockets per API key.
//! - `stream`: `AsyncRead`/`AsyncWrite` bridge over a handed-over socket.
//! - `connect_proxy`: loopback HTTP CONNECT proxy so `reqwest` can use
//!   tunneled egress transparently.

pub mod connect_proxy;
pub mod registry;
pub mod stream;

pub use connect_proxy::ConnectProxy;
pub use registry::{TunnelError, TunnelRegistry};
pub use stream::WsByteStream;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

use crate::auth;
use crate::AppState;

/// HTTP upgrade handler for `GET /v1/tunnel?token=KEY` (or Bearer header).
///
/// Auth failures, disabled tunnel, and keys without egress permission are
/// rejected *before* the WebSocket upgrade, as plain HTTP errors — matching
/// what the Tauri client expects (it just retries on failure).
pub async fn tunnel_ws(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if !state.config.tunnel.enabled {
        return (StatusCode::NOT_FOUND, "tunnel disabled").into_response();
    }
    let key = match auth::authenticate(&state.config, &headers, &uri) {
        Ok(k) => k,
        Err(e) => return e.into_response(),
    };
    if !key.allow_tunnel_egress {
        return (StatusCode::FORBIDDEN, "tunnel egress not allowed for this key")
            .into_response();
    }
    let idle_ping = Duration::from_secs(state.config.tunnel.idle_ping_secs.max(1));
    ws.on_upgrade(move |socket| {
        let registry = Arc::clone(&state.tunnel);
        async move {
            let handle = registry.register(&key.key, socket, idle_ping);
            tracing::debug!(account = %key.account_id, id = handle.id, "tunnel worker parked");
        }
    })
}

/// Spawn a loopback CONNECT bridge for the tunnel pool, for use as a
/// `reqwest` proxy (`routing_mode != "direct"`). Returns the bridge handle.
pub async fn spawn_connect_proxy(
    state: &AppState,
) -> Result<ConnectProxy, String> {
    let key = state
        .config
        .auth
        .keys
        .iter()
        .find(|k| k.allow_tunnel_egress)
        .map(|k| k.key.clone())
        .ok_or_else(|| "no key with allow_tunnel_egress".to_string())?;
    ConnectProxy::spawn(
        Arc::clone(&state.tunnel),
        key,
        Duration::from_millis(state.config.tunnel.handshake_timeout_ms),
        Duration::from_millis(state.config.tunnel.acquire_timeout_ms),
    )
    .await
}
