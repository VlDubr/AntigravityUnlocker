use crate::config::{Config, KeyConfig};
use crate::error::AppError;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, Uri};
use subtle::ConstantTimeEq;

/// Extract the raw key from an `Authorization: Bearer <key>` header.
pub fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let rest = value.strip_prefix("Bearer ")?;
    let rest = rest.trim();
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_string())
    }
}

/// Extract `?token=<key>` from a URI query (used for websocket upgrades).
pub fn extract_token_query(uri: &Uri) -> Option<String> {
    let query = uri.query()?;
    for pair in query.split('&') {
        let (k, v) = match pair.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        if k == "token" && !v.is_empty() {
            return Some(percent_decode(v));
        }
    }
    None
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Constant-time comparison of two byte slices; no early exit on mismatch.
/// Lengths are compared as full `usize` (never truncated) so a key padded to a
/// multiple of 256 bytes cannot collide with a shorter key's length.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let len_eq = a.len().ct_eq(&b.len());
    let n = a.len().max(b.len());
    let mut acc: u8 = 0;
    for i in 0..n {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        acc |= x ^ y;
    }
    bool::from(acc.ct_eq(&0)) & bool::from(len_eq)
}

fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Authenticate a request: Bearer header first, `?token=` query as fallback.
pub fn authenticate(config: &Config, headers: &HeaderMap, uri: &Uri) -> Result<KeyConfig, AppError> {
    let token = match extract_bearer(headers).or_else(|| extract_token_query(uri)) {
        Some(t) => t,
        None => return Err(AppError::Unauthorized("Invalid token".to_string())),
    };
    let token_bytes = token.as_bytes();
    let mut matched: Option<&KeyConfig> = None;
    // Compare against every configured key; never short-circuit (anti-timing).
    for entry in &config.auth.keys {
        let eq = constant_time_eq(token_bytes, entry.key.as_bytes());
        if eq && matched.is_none() {
            matched = Some(entry);
        }
    }
    match matched {
        None => Err(AppError::Unauthorized("Invalid token".to_string())),
        Some(entry) => {
            if entry.expires_at > 0 && now_unix_secs() >= entry.expires_at {
                return Err(AppError::Forbidden("Token expired".to_string()));
            }
            Ok(entry.clone())
        }
    }
}

/// Axum extractor injecting the authenticated key into protected handlers.
pub struct AuthenticatedKey(pub KeyConfig);

impl FromRequestParts<crate::AppState> for AuthenticatedKey {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &crate::AppState,
    ) -> Result<Self, Self::Rejection> {
        let key = authenticate(&state.config, &parts.headers, &parts.uri)?;
        Ok(AuthenticatedKey(key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::KeyConfig;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    fn cfg_with_keys(keys: Vec<KeyConfig>) -> Config {
        let mut cfg = Config::default();
        cfg.auth.keys = keys;
        cfg
    }

    fn key(k: &str) -> KeyConfig {
        KeyConfig {
            key: k.to_string(),
            ..KeyConfig::default()
        }
    }

    fn headers_with_bearer(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, v.parse().unwrap());
        h
    }

    #[test]
    fn bearer_extraction_valid_and_invalid() {
        let h = headers_with_bearer("Bearer abc123");
        assert_eq!(extract_bearer(&h).as_deref(), Some("abc123"));
        let h = headers_with_bearer("Basic abc123");
        assert_eq!(extract_bearer(&h), None);
        let h = headers_with_bearer("Bearer ");
        assert_eq!(extract_bearer(&h), None);
        assert_eq!(extract_bearer(&HeaderMap::new()), None);
    }

    #[test]
    fn query_token_extraction_with_percent_decoding() {
        let uri: Uri = "/ws?token=abc123".parse().unwrap();
        assert_eq!(extract_token_query(&uri).as_deref(), Some("abc123"));
        let uri: Uri = "/ws?a=1&token=x%2Fy&b=2".parse().unwrap();
        assert_eq!(extract_token_query(&uri).as_deref(), Some("x/y"));
        let uri: Uri = "/ws?a=1".parse().unwrap();
        assert_eq!(extract_token_query(&uri), None);
        let uri: Uri = "/ws?token=".parse().unwrap();
        assert_eq!(extract_token_query(&uri), None);
    }

    #[test]
    fn constant_time_eq_correctness() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcx"));
        assert!(!constant_time_eq(b"abcx", b"abc"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(!constant_time_eq(b"a", b""));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"k1", b"k1"));
        assert!(!constant_time_eq(b"k1", b"k2"));
    }

    #[test]
    fn constant_time_eq_no_length_truncation_bypass() {
        // Regression: lengths must compare as full usize, not truncated to u8.
        // A 257-byte input (len as u8 == 1) must NOT match a 1-byte key even
        // when the extra 256 bytes are zero.
        let short = b"k1";
        let mut padded = vec![0u8; 257];
        padded[0] = b'k';
        padded[1] = b'1';
        assert!(!constant_time_eq(short, &padded));
        assert!(!constant_time_eq(&padded, short));
        // Exact 256-byte vs 0-byte: both truncate to 0 as u8, must differ.
        let zeros = vec![0u8; 256];
        assert!(!constant_time_eq(b"", &zeros));
        assert!(!constant_time_eq(&zeros, b""));
        // Same-length 257-byte inputs still compare correctly.
        let mut other = padded.clone();
        other[256] = 1;
        assert!(!constant_time_eq(&padded, &other));
        assert!(constant_time_eq(&padded, &padded));
    }

    #[test]
    fn valid_bearer_returns_key_clone() {
        let cfg = cfg_with_keys(vec![key("k1"), key("k2")]);
        let h = headers_with_bearer("Bearer k2");
        let got = authenticate(&cfg, &h, &Uri::default()).unwrap();
        assert_eq!(got.key, "k2");
        assert_eq!(got.account_id, "main");
    }

    #[test]
    fn bad_key_is_unauthorized() {
        let cfg = cfg_with_keys(vec![key("k1")]);
        let h = headers_with_bearer("Bearer nope");
        let err = authenticate(&cfg, &h, &Uri::default()).unwrap_err();
        assert_eq!(err.status_code(), StatusCode::UNAUTHORIZED);
        assert_eq!(err.message(), "Invalid token");
        // prefix of a real key must not match
        let h = headers_with_bearer("Bearer k");
        assert!(authenticate(&cfg, &h, &Uri::default()).is_err());
    }

    #[test]
    fn missing_auth_is_unauthorized() {
        let cfg = cfg_with_keys(vec![key("k1")]);
        let err = authenticate(&cfg, &HeaderMap::new(), &Uri::default()).unwrap_err();
        assert_eq!(err.status_code(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn expired_key_is_forbidden() {
        let now = now_unix_secs();
        let cfg = cfg_with_keys(vec![KeyConfig {
            key: "k1".to_string(),
            expires_at: now - 60,
            ..KeyConfig::default()
        }]);
        let h = headers_with_bearer("Bearer k1");
        let err = authenticate(&cfg, &h, &Uri::default()).unwrap_err();
        assert_eq!(err.status_code(), StatusCode::FORBIDDEN);
        assert_eq!(err.message(), "Token expired");
    }

    #[test]
    fn future_expiry_still_valid() {
        let cfg = cfg_with_keys(vec![KeyConfig {
            key: "k1".to_string(),
            expires_at: now_unix_secs() + 3600,
            ..KeyConfig::default()
        }]);
        let h = headers_with_bearer("Bearer k1");
        assert!(authenticate(&cfg, &h, &Uri::default()).is_ok());
    }

    #[test]
    fn query_token_fallback_when_no_bearer() {
        let cfg = cfg_with_keys(vec![key("k1")]);
        let uri: Uri = "/v1/models?token=k1".parse().unwrap();
        let got = authenticate(&cfg, &HeaderMap::new(), &uri).unwrap();
        assert_eq!(got.key, "k1");
        // bearer takes precedence over query
        let h = headers_with_bearer("Bearer k1");
        let uri: Uri = "/v1/models?token=wrong".parse().unwrap();
        assert_eq!(authenticate(&cfg, &h, &uri).unwrap().key, "k1");
    }

    fn test_app(cfg: Config) -> Router {
        let state = crate::AppState::new(cfg);
        Router::new()
            .route("/v1/models", get(|_a: crate::auth::AuthenticatedKey| async { "ok" }))
            .with_state(state)
    }

    #[tokio::test]
    async fn extractor_valid_bearer_gives_200() {
        let app = test_app(cfg_with_keys(vec![key("k1")]));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .header("Authorization", "Bearer k1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn extractor_bad_key_gives_401_json() {
        let app = test_app(cfg_with_keys(vec![key("k1")]));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .header("Authorization", "Bearer bad")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["message"], "Invalid token");
    }

    #[tokio::test]
    async fn extractor_expired_key_gives_403() {
        let now = now_unix_secs();
        let app = test_app(cfg_with_keys(vec![KeyConfig {
            key: "k1".to_string(),
            expires_at: now - 1,
            ..KeyConfig::default()
        }]));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .header("Authorization", "Bearer k1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["message"], "Token expired");
    }
}
