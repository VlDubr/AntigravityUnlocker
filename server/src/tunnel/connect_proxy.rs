//! Loopback HTTP CONNECT proxy bridging `reqwest` to tunned worker egress.
//!
//! A per-registry bridge listens on `127.0.0.1:0`. `reqwest` clients built
//! by [`ConnectProxy::reqwest_client`] send
//! `CONNECT host:port HTTP/1.1` with `Proxy-Authorization: Basic b64(x:secret)`.
//! The bridge validates the secret (constant-time), acquires a tunnel worker
//! from the registry, replies `200 Connection Established`, then pumps raw
//! bytes between the loopback TCP connection and the worker WebSocket. TLS
//! terminates end-to-end at the final target; the bridge is content-blind.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use rand::RngCore;
use subtle::ConstantTimeEq;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use super::registry::TunnelRegistry;

/// Reject request headers beyond this size.
const MAX_HEADERS: usize = 64 * 1024;

/// Handle to a running loopback CONNECT bridge.
pub struct ConnectProxy {
    local_addr: SocketAddr,
    secret: String,
}

impl ConnectProxy {
    /// Bind a loopback listener and spawn the accept loop.
    pub async fn spawn(
        registry: Arc<TunnelRegistry>,
        key: String,
        handshake_timeout: Duration,
        acquire_timeout: Duration,
    ) -> Result<Self, String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("tunnel bridge bind failed: {e}"))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| format!("tunnel bridge addr: {e}"))?;
        let mut secret_bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut secret_bytes);
        let secret: String = secret_bytes.iter().map(|b| format!("{b:02x}")).collect();

        let bridge_secret = secret.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _peer)) => {
                        let registry = Arc::clone(&registry);
                        let key = key.clone();
                        let secret = bridge_secret.clone();
                        tokio::spawn(handle_conn(
                            stream,
                            registry,
                            key,
                            secret,
                            handshake_timeout,
                            acquire_timeout,
                        ));
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "tunnel bridge accept error");
                        if e.raw_os_error() == Some(24) {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    }
                }
            }
        });

        tracing::info!(addr = %local_addr, "tunnel CONNECT bridge listening");
        Ok(Self { local_addr, secret })
    }

    /// Loopback address of the bridge.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Bridge shared secret (tests and diagnostics only).
    pub fn secret(&self) -> &str {
        &self.secret
    }

    /// `reqwest` client whose traffic egresses through the tunnel bridge.
    pub fn reqwest_client(&self) -> reqwest::Client {
        let proxy = reqwest::Proxy::all(format!("http://{}", self.local_addr))
            .expect("valid loopback proxy url")
            .basic_auth("x", &self.secret);
        reqwest::Client::builder()
            .proxy(proxy)
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .tcp_nodelay(true)
            .build()
            .expect("valid reqwest client")
    }
}

/// Parse a CONNECT request line + headers; returns (host, port, authed).
async fn read_connect(
    stream: &mut TcpStream,
    secret: &str,
) -> Result<(String, u16, bool), String> {
    let mut reader = BufReader::new(stream);
    let mut total = 0usize;
    let mut host_port = String::new();
    let mut authed = false;
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(|e| format!("read request: {e}"))?;
        if n == 0 {
            return Err("client closed before end of headers".into());
        }
        total += n;
        if total > MAX_HEADERS {
            return Err("request headers too large".into());
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if host_port.is_empty() {
            // Request line: CONNECT host:port HTTP/1.x
            let mut parts = trimmed.split_whitespace();
            let method = parts.next().unwrap_or_default();
            if method.ne("CONNECT") {
                return Err(format!("unsupported proxy method {method:?}"));
            }
            host_port = parts.next().unwrap_or_default().to_string();
        } else {
            let (name, value) = match trimmed.split_once(':') {
                Some(p) => p,
                None => continue,
            };
            if name.trim().eq_ignore_ascii_case("proxy-authorization") {
                authed = check_proxy_auth(value.trim(), secret);
            }
        }
    }
    if host_port.is_empty() {
        return Err("missing CONNECT target".into());
    }
    // IPv6 literals look like [::1]:443; rsplit_once keeps that correct.
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => (h.trim_matches(['[', ']']).to_string(), p.to_string()),
        None => (host_port.clone(), "443".to_string()),
    };
    let port: u16 = port.parse().map_err(|_| format!("bad port in {host_port:?}"))?;
    Ok((host, port, authed))
}

/// Validate `Proxy-Authorization: Basic b64(user:secret)` against `secret`.
fn check_proxy_auth(value: &str, secret: &str) -> bool {
    let b64 = match value.strip_prefix("Basic ") {
        Some(v) => v.trim(),
        None => return false,
    };
    let decoded = match base64::engine::general_purpose::STANDARD.decode(b64) {
        Ok(d) => d,
        Err(_) => return false,
    };
    let decoded = match String::from_utf8(decoded) {
        Ok(s) => s,
        Err(_) => return false,
    };
    // decoded is "user:password"; compare the password part constant-time.
    match decoded.split_once(':') {
        Some((_user, pass)) => pass.as_bytes().ct_eq(secret.as_bytes()).into(),
        None => false,
    }
}

async fn handle_conn(
    mut stream: TcpStream,
    registry: Arc<TunnelRegistry>,
    key: String,
    secret: String,
    handshake_timeout: Duration,
    acquire_timeout: Duration,
) {
    let (host, port, authed) = match read_connect(&mut stream, &secret).await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(error = %e, "tunnel bridge bad CONNECT request");
            return;
        }
    };
    if !authed {
        tracing::debug!("tunnel bridge rejected unauthenticated CONNECT");
        let _ = stream
            .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\n\
Proxy-Authenticate: Basic realm=\"tunnel\"\r\nContent-Length: 0\r\n\r\n")
            .await;
        return;
    }
    let ws = match registry
        .connect(&key, &host, port, handshake_timeout, acquire_timeout)
        .await
    {
        Ok(ws) => ws,
        Err(e) => {
            tracing::debug!(target = %format!("{host}:{port}"), error = %e, "tunnel bridge connect failed");
            let _ = stream
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                .await;
            return;
        }
    };
    if stream.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await.is_err() {
        return;
    }
    let _ = stream.set_nodelay(true);
    let mut ws = ws;
    let _ = tokio::io::copy_bidirectional(&mut stream, &mut ws).await;
    // Ensure the Close frame reaches the worker.
    let _ = tokio::io::AsyncWriteExt::shutdown(&mut ws).await;
    let _ = stream.shutdown().await;
}
