use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::Query;
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::auth::AuthenticatedKey;
use crate::error::AppError;

#[derive(Debug, Deserialize)]
pub struct ProxyConnectQuery {
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
}

fn default_port() -> u16 {
    443
}

/// GET /v1/proxy-connect?host=...&port=443 — authenticated forward TCP tunnel
/// over WebSocket for HTTPS CONNECT clients (e.g. Antigravity 2.0).
pub async fn proxy_connect(
    _auth: AuthenticatedKey,
    Query(query): Query<ProxyConnectQuery>,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, AppError> {
    let (host, port) = validate_connect_params(&query.host, query.port)?;
    let addr = resolve_public_addr(&host, port).await?;
    Ok(ws.on_upgrade(move |socket| handle_ws_tunnel(socket, addr)))
}

fn validate_connect_params(raw_host: &str, port: u16) -> Result<(String, u16), AppError> {
    let host = raw_host.trim().to_string();
    if host.is_empty() || host.len() > 253 {
        return Err(AppError::BadRequest("invalid host".to_string()));
    }
    if port != 443 && port != 80 {
        return Err(AppError::Forbidden(
            "only ports 80 and 443 are allowed".to_string(),
        ));
    }
    Ok((host, port))
}

async fn resolve_public_addr(host: &str, port: u16) -> Result<SocketAddr, AppError> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !is_public_ip(ip) {
            return Err(AppError::Forbidden(
                "private or loopback IP is not allowed".to_string(),
            ));
        }
        return Ok(SocketAddr::new(ip, port));
    }

    let addrs = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::net::lookup_host((host, port)),
    )
    .await
    .map_err(|_| AppError::BadGateway("DNS lookup timed out".to_string()))?
    .map_err(|e| AppError::BadGateway(format!("DNS lookup failed: {e}")))?;

    for addr in addrs {
        if is_public_ip(addr.ip()) {
            return Ok(addr);
        }
    }

    Err(AppError::Forbidden(
        "target resolved only to non-public IP addresses".to_string(),
    ))
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_ipv4(v4),
        IpAddr::V6(v6) => is_public_ipv6(v6),
    }
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    let is_cgnat = octets[0] == 100 && (octets[1] & 0b1100_0000) == 64;
    let is_benchmark = octets[0] == 198 && (octets[1] == 18 || octets[1] == 19);
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        || is_cgnat
        || is_benchmark)
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_ipv4(v4);
    }
    let segs = ip.segments();
    let is_ula = (segs[0] & 0xfe00) == 0xfc00;
    let is_link_local = (segs[0] & 0xffc0) == 0xfe80;
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || is_ula
        || is_link_local)
}

async fn handle_ws_tunnel(ws: WebSocket, addr: SocketAddr) {
    let stream = match tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(addr)).await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            tracing::warn!(%addr, error = %e, "proxy-connect TCP connect failed");
            return;
        }
        Err(_) => {
            tracing::warn!(%addr, "proxy-connect TCP connect timed out");
            return;
        }
    };
    let _ = stream.set_nodelay(true);

    let (mut tcp_read, mut tcp_write) = stream.into_split();
    let (mut ws_write, mut ws_read) = ws.split();

    let to_ws = async move {
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            match tcp_read.read(&mut buf).await {
                Ok(0) => {
                    let _ = ws_write.send(Message::Close(None)).await;
                    break;
                }
                Ok(n) => {
                    if ws_write
                        .send(Message::Binary(buf[..n].to_vec().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    };

    let to_tcp = async move {
        while let Some(Ok(msg)) = ws_read.next().await {
            match msg {
                Message::Binary(data) => {
                    if tcp_write.write_all(&data).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
        let _ = tcp_write.shutdown().await;
    };

    tokio::select! {
        _ = to_ws => {}
        _ = to_tcp => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, KeyConfig};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[test]
    fn public_ip_rejects_private_loopback_cgnat() {
        assert!(!is_public_ip("127.0.0.1".parse().unwrap()));
        assert!(!is_public_ip("10.0.0.1".parse().unwrap()));
        assert!(!is_public_ip("192.168.1.1".parse().unwrap()));
        assert!(!is_public_ip("172.16.0.1".parse().unwrap()));
        assert!(!is_public_ip("100.64.0.1".parse().unwrap()));
        assert!(!is_public_ip("198.18.0.1".parse().unwrap()));
        assert!(!is_public_ip("::1".parse().unwrap()));
        assert!(!is_public_ip("fe80::1".parse().unwrap()));
        assert!(is_public_ip("142.250.74.14".parse().unwrap()));
    }

    #[tokio::test]
    async fn proxy_connect_requires_auth_and_blocks_bad_ports_and_private_ips() {
        let mut cfg = Config::default();
        cfg.auth.keys.push(KeyConfig {
            key: "k1".to_string(),
            ..Default::default()
        });
        let app = crate::build_router(crate::AppState::new(cfg));

        let unauth = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/proxy-connect?host=142.250.74.14&port=443")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

        let host_err = resolve_public_addr("127.0.0.1", 443).await.unwrap_err();
        assert!(matches!(host_err, AppError::Forbidden(_)));

        let port_err = validate_connect_params("142.250.74.14", 22).unwrap_err();
        assert!(matches!(port_err, AppError::Forbidden(_)));

        let ok_params = validate_connect_params("142.250.74.14", 443).unwrap();
        assert_eq!(ok_params, ("142.250.74.14".to_string(), 443));
    }
}
