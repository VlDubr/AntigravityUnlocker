//! Upstream path normalization, egress transport selection, and target URL
//! building for the streaming forward handler (`handlers::forward`).

use crate::config::{Config, KeyConfig};
use crate::tunnel::TunnelRegistry;

/// Egress path for a forwarded request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Connect to the upstream directly from the server.
    Direct,
    /// Egress through a reverse-tunnel worker via the CONNECT bridge.
    Tunnel,
}

/// Duplicate gateway prefixes produced by clients that set
/// `proxyBaseUrl = {server}/v1` (the IDE then calls, e.g.,
/// `{server}/v1/v1internal:streamGenerateContent`). Applied repeatedly as
/// long as they keep matching.
const GATEWAY_PREFIXES: &[(&str, &str)] = &[
    ("/v1/v1internal:", "/v1internal:"),
    ("/v1/v1internal/", "/v1internal/"),
    ("/v1internal/v1internal:", "/v1internal:"),
    ("/v1internal/v1internal/", "/v1internal/"),
];

/// Normalize the upstream request path (path only — the caller appends the
/// query verbatim).
///
/// 1. Collapse repeated `//` into single `/`.
/// 2. Strip duplicate gateway prefixes (`/v1/v1internal...`,
///    `/v1internal/v1internal...`), e.g. `/v1/v1internal:streamGenerateContent`
///    becomes `/v1internal:streamGenerateContent`.
/// 3. When `normalize_colon` is set, rewrite single-segment
///    `/v1internal/{method}` into Google's colon form `/v1internal:{method}`.
///    Multi-segment paths such as `/v1internal/operations/{id}` keep the
///    slash, as does the bare `/v1internal/operations` collection.
pub fn normalize_path(raw: &str, normalize_colon: bool) -> String {
    // 1. Collapse "//" runs.
    let mut path = raw.to_string();
    while path.contains("//") {
        path = path.replace("//", "/");
    }
    // 2. Strip duplicate gateway prefixes until stable.
    loop {
        let before = path.clone();
        for (from, to) in GATEWAY_PREFIXES {
            if let Some(rest) = path.strip_prefix(from) {
                path = format!("{to}{rest}");
                break;
            }
        }
        if path == before {
            break;
        }
    }
    // 3. Slash -> colon for single-segment v1internal method calls.
    if normalize_colon {
        if let Some(method) = path.strip_prefix("/v1internal/") {
            if !method.is_empty() && !method.contains('/') && method != "operations" {
                path = format!("/v1internal:{method}");
            }
        }
    }
    path
}

/// Extract the bare host from a base URL without a URL-parsing crate:
/// strip the scheme, cut the authority at `/?#`, drop any port, and trim
/// IPv6 brackets.
pub fn host_of(base_url: &str) -> String {
    let rest = match base_url.find("://") {
        Some(i) => &base_url[i + 3..],
        None => base_url,
    };
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    // rsplit_once keeps IPv6 literals like "[::1]:443" intact.
    let host = match authority.rsplit_once(':') {
        Some((h, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => authority,
    };
    host.trim_matches(['[', ']']).to_ascii_lowercase()
}

/// Match a host against a route pattern: exact, or `*.suffix` wildcard
/// (matches `suffix` itself and anything ending in `.suffix`).
fn host_matches(host: &str, pattern: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(suffix) => {
            host == suffix || host.strip_suffix(suffix).is_some_and(|prefix| prefix.ends_with('.'))
        }
        None => host == pattern,
    }
}

/// Choose the egress transport for `host`, per `tunnel.routing_mode`:
/// `direct` (default), `allowlist` (tunnel only for `route_hosts` matches),
/// `prefer_tunnel` (tunnel when this key has idle workers), `always`.
///
/// Any tunnel-requiring mode degrades to [`Transport::Direct`] when the
/// tunnel is globally disabled or the key lacks `allow_tunnel_egress` —
/// including `always`, since routing such a key through the bridge can only
/// fail (407/502); direct egress is the safer fallback.
pub fn select_transport(
    config: &Config,
    tunnel: &TunnelRegistry,
    key: &KeyConfig,
    host: &str,
) -> Transport {
    if !config.tunnel.enabled || !key.allow_tunnel_egress {
        return Transport::Direct;
    }
    let host = host.to_ascii_lowercase();
    match config.tunnel.routing_mode.as_str() {
        "always" => Transport::Tunnel,
        "allowlist" => {
            let hit = config
                .tunnel
                .route_hosts
                .iter()
                .any(|p| host_matches(&host, &p.to_ascii_lowercase()));
            if hit {
                Transport::Tunnel
            } else {
                Transport::Direct
            }
        }
        "prefer_tunnel" => {
            if tunnel.idle_count(&key.key) > 0 {
                Transport::Tunnel
            } else {
                Transport::Direct
            }
        }
        // "direct" and unknown modes.
        _ => Transport::Direct,
    }
}

/// Build the full upstream URL: base (minus trailing `/`) + normalized path
/// (guaranteed leading `/`) + verbatim query, when non-empty.
pub fn build_target_url(base_url: &str, normalized_path: &str, query: Option<&str>) -> String {
    let base = base_url.trim_end_matches('/');
    let path = if normalized_path.starts_with('/') {
        normalized_path.to_string()
    } else {
        format!("/{normalized_path}")
    };
    match query.filter(|q| !q.is_empty()) {
        Some(q) => format!("{base}{path}?{q}"),
        None => format!("{base}{path}"),
    }
}

/// Drop the gateway `token` query parameter before forwarding upstream.
///
/// The client may authenticate via `?token=<key>` (WebSocket upgrades and
/// fallback). That key is a *gateway* secret and must never leak to Google or
/// any upstream. Other query parameters (e.g. `alt=sse`) are preserved verbatim
/// and in order.
pub fn strip_token_query(query: Option<&str>) -> Option<String> {
    let q = query?;
    let kept: Vec<&str> = q
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter(|pair| {
            let key = pair.split('=').next().unwrap_or("");
            key != "token"
        })
        .collect();
    if kept.is_empty() {
        None
    } else {
        Some(kept.join("&"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{TunnelConfig, UpstreamConfig};

    fn norm(raw: &str) -> String {
        normalize_path(raw, true)
    }

    #[test]
    fn strip_token_query_removes_gateway_key_keeps_rest() {
        assert_eq!(strip_token_query(None), None);
        assert_eq!(strip_token_query(Some("")), None);
        assert_eq!(strip_token_query(Some("token=SECRET")), None);
        assert_eq!(
            strip_token_query(Some("alt=sse&token=SECRET")),
            Some("alt=sse".to_string())
        );
        assert_eq!(
            strip_token_query(Some("token=SECRET&alt=sse")),
            Some("alt=sse".to_string())
        );
        assert_eq!(
            strip_token_query(Some("a=1&token=SECRET&b=2")),
            Some("a=1&b=2".to_string())
        );
        assert_eq!(
            strip_token_query(Some("alt=sse")),
            Some("alt=sse".to_string())
        );
        // A parameter whose name merely contains "token" is preserved.
        assert_eq!(
            strip_token_query(Some("tokeninfo=1")),
            Some("tokeninfo=1".to_string())
        );
    }

    #[test]
    fn strips_duplicate_gateway_prefixes() {
        assert_eq!(
            norm("/v1/v1internal:streamGenerateContent"),
            "/v1internal:streamGenerateContent"
        );
        assert_eq!(
            norm("/v1/v1internal/streamGenerateContent"),
            "/v1internal:streamGenerateContent"
        );
        assert_eq!(
            norm("/v1internal/v1internal:streamGenerateContent"),
            "/v1internal:streamGenerateContent"
        );
        assert_eq!(
            norm("/v1internal/v1internal/streamGenerateContent"),
            "/v1internal:streamGenerateContent"
        );
        // Only the listed duplicate prefixes are stripped; a plain extra /v1
        // namespace segment is left alone.
        assert_eq!(
            norm("/v1/v1/v1internal:streamGenerateContent"),
            "/v1/v1/v1internal:streamGenerateContent"
        );
    }

    #[test]
    fn rewrites_single_segment_to_colon_form() {
        assert_eq!(
            norm("/v1internal/streamGenerateContent"),
            "/v1internal:streamGenerateContent"
        );
        assert_eq!(norm("/v1internal/loadCodeAssist"), "/v1internal:loadCodeAssist");
        // Colon form passes through unchanged.
        assert_eq!(
            norm("/v1internal:streamGenerateContent"),
            "/v1internal:streamGenerateContent"
        );
    }

    #[test]
    fn keeps_multi_segment_and_operations_paths() {
        assert_eq!(norm("/v1internal/operations/123"), "/v1internal/operations/123");
        assert_eq!(norm("/v1internal/operations"), "/v1internal/operations");
        assert_eq!(
            norm("/v1internal/operations/123.fetch"),
            "/v1internal/operations/123.fetch"
        );
        // Non-v1internal paths untouched.
        assert_eq!(norm("/v1/models"), "/v1/models");
        assert_eq!(norm("/health"), "/health");
        assert_eq!(norm("/v1/chat/sessions"), "/v1/chat/sessions");
    }

    #[test]
    fn collapses_double_slashes() {
        assert_eq!(
            norm("//v1internal//streamGenerateContent"),
            "/v1internal:streamGenerateContent"
        );
        assert_eq!(
            norm("/v1///v1internal:streamGenerateContent"),
            "/v1internal:streamGenerateContent"
        );
        assert_eq!(norm("///"), "/");
    }

    #[test]
    fn normalize_colon_false_keeps_slash_form() {
        assert_eq!(
            normalize_path("/v1internal/streamGenerateContent", false),
            "/v1internal/streamGenerateContent"
        );
        // Prefix stripping still happens without colon rewriting.
        assert_eq!(
            normalize_path("/v1/v1internal/streamGenerateContent", false),
            "/v1internal/streamGenerateContent"
        );
        assert_eq!(
            normalize_path("/v1/v1internal:streamGenerateContent", false),
            "/v1internal:streamGenerateContent"
        );
    }

    #[test]
    fn build_target_url_joins_base_path_query() {
        assert_eq!(
            build_target_url("https://upstream.example", "/v1internal:x", None),
            "https://upstream.example/v1internal:x"
        );
        assert_eq!(
            build_target_url("https://upstream.example/", "/v1internal:x", Some("alt=sse")),
            "https://upstream.example/v1internal:x?alt=sse"
        );
        assert_eq!(
            build_target_url("http://127.0.0.1:8080", "v1internal:x", Some("a=1&b=2")),
            "http://127.0.0.1:8080/v1internal:x?a=1&b=2"
        );
        // Empty query is not appended.
        assert_eq!(
            build_target_url("http://h:1", "/p", Some("")),
            "http://h:1/p"
        );
    }

    #[test]
    fn host_of_extracts_bare_host() {
        assert_eq!(
            host_of("https://daily-cloudcode-pa.googleapis.com"),
            "daily-cloudcode-pa.googleapis.com"
        );
        assert_eq!(host_of("http://127.0.0.1:8080"), "127.0.0.1");
        assert_eq!(host_of("http://127.0.0.1:8080/"), "127.0.0.1");
        assert_eq!(host_of("https://API.Example.COM/path"), "api.example.com");
        assert_eq!(host_of("https://example.com:443/x?q=1"), "example.com");
        assert_eq!(host_of("[::1]:9000"), "::1");
    }

    #[test]
    fn host_matches_exact_and_wildcard() {
        assert!(host_matches("api.example.com", "api.example.com"));
        assert!(!host_matches("xapi.example.com", "api.example.com"));
        assert!(host_matches("a.example.com", "*.example.com"));
        assert!(host_matches("deep.a.example.com", "*.example.com"));
        assert!(host_matches("example.com", "*.example.com"));
        assert!(!host_matches("notexample.com", "*.example.com"));
    }

    fn cfg_with(mode: &str, hosts: &[&str], enabled: bool) -> Config {
        let mut config = Config::default();
        config.tunnel = TunnelConfig {
            enabled,
            routing_mode: mode.to_string(),
            route_hosts: hosts.iter().map(|s| s.to_string()).collect(),
            ..TunnelConfig::default()
        };
        config
    }

    fn key(egress: bool) -> KeyConfig {
        KeyConfig {
            key: "k1".to_string(),
            allow_tunnel_egress: egress,
            ..KeyConfig::default()
        }
    }

    #[test]
    fn select_transport_direct_mode() {
        let tunnel = TunnelRegistry::new(4);
        let config = cfg_with("direct", &[], true);
        assert_eq!(
            select_transport(&config, &tunnel, &key(true), "any.host"),
            Transport::Direct
        );
    }

    #[test]
    fn select_transport_always_mode() {
        let tunnel = TunnelRegistry::new(4);
        let config = cfg_with("always", &[], true);
        assert_eq!(
            select_transport(&config, &tunnel, &key(true), "any.host"),
            Transport::Tunnel
        );
        // No egress permission -> direct, to avoid an unavoidable bridge 502.
        assert_eq!(
            select_transport(&config, &tunnel, &key(false), "any.host"),
            Transport::Direct
        );
    }

    #[test]
    fn select_transport_allowlist_mode() {
        let tunnel = TunnelRegistry::new(4);
        let config = cfg_with("allowlist", &["*.tunnel.dev", "exact.host"], true);
        assert_eq!(
            select_transport(&config, &tunnel, &key(true), "a.tunnel.dev"),
            Transport::Tunnel
        );
        assert_eq!(
            select_transport(&config, &tunnel, &key(true), "exact.host"),
            Transport::Tunnel
        );
        assert_eq!(
            select_transport(&config, &tunnel, &key(true), "other.host"),
            Transport::Direct
        );
        assert_eq!(
            select_transport(&config, &tunnel, &key(false), "exact.host"),
            Transport::Direct
        );
    }

    #[test]
    fn select_transport_prefer_tunnel_without_workers_is_direct() {
        let tunnel = TunnelRegistry::new(4);
        let config = cfg_with("prefer_tunnel", &[], true);
        // No idle workers parked for this key.
        assert_eq!(
            select_transport(&config, &tunnel, &key(true), "any.host"),
            Transport::Direct
        );
        assert_eq!(
            select_transport(&config, &tunnel, &key(false), "any.host"),
            Transport::Direct
        );
    }

    #[test]
    fn select_transport_tunnel_disabled_is_direct() {
        let tunnel = TunnelRegistry::new(4);
        let config = cfg_with("always", &[], false);
        assert_eq!(
            select_transport(&config, &tunnel, &key(true), "any.host"),
            Transport::Direct
        );
    }

    #[test]
    fn unknown_mode_defaults_to_direct() {
        let tunnel = TunnelRegistry::new(4);
        let config = cfg_with("nonsense", &[], true);
        assert_eq!(
            select_transport(&config, &tunnel, &key(true), "any.host"),
            Transport::Direct
        );
    }

    #[test]
    fn default_upstream_base_url_is_cloudcode_pa() {
        let config = Config::default();
        assert_eq!(
            config.upstream.base_url,
            "https://daily-cloudcode-pa.googleapis.com"
        );
        let placeholder = UpstreamConfig::default();
        assert!(placeholder.normalize_v1internal_colon);
        assert!(placeholder.passthrough_unknown_v1);
    }
}
