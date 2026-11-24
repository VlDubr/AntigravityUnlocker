use serde::Deserialize;
use std::collections::HashMap;

/// Default model ids advertised when `[models] advertise` is empty.
pub fn default_model_ids() -> Vec<String> {
    [
        "claude-sonnet-4-6",
        "claude-sonnet-4-6-thinking",
        "claude-opus-4-6",
        "claude-opus-4-6-thinking",
        "gemini-3.8-flash-high",
        "gemini-3.8-flash-medium",
        "gemini-3.8-flash-low",
        "gemini-3.8-flash-tiered",
        "gemini-3.8-flash-thinking",
        "gemini-3.8-flash",
        "gemini-3.7-flash-high",
        "gemini-3.7-flash-medium",
        "gemini-3.7-flash-low",
        "gemini-3.7-flash-tiered",
        "gemini-3.7-flash-thinking",
        "gemini-3.7-flash",
        "gemini-3.6-flash-high",
        "gemini-3.6-flash-medium",
        "gemini-3.6-flash-low",
        "gemini-3.6-flash-tiered",
        "gemini-3.5-flash-high",
        "gemini-3.1-pro-high",
        "gemini-3.1-pro-low",
        "gemini-3.1-flash-lite",
        "gemini-3.1-flash-image",
        "gemini-3-flash",
        "gemini-3-flash-agent",
        "gemini-pro-agent",
        "gpt-oss-120b-medium",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub google: GoogleConfig,
    #[serde(default)]
    pub upstream: UpstreamConfig,
    #[serde(default)]
    pub tunnel: TunnelConfig,
    #[serde(default)]
    pub models: ModelsConfig,
    #[serde(default)]
    pub updates: UpdatesConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub listen_addr: String,
    pub public_url: String,
    pub max_body_bytes: usize,
    pub state_file: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:8045".to_string(),
            public_url: String::new(),
            max_body_bytes: 67_108_864,
            state_file: "./state.json".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    pub keys: Vec<KeyConfig>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self { keys: Vec::new() }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyConfig {
    pub key: String,
    #[serde(default = "default_account_id")]
    pub account_id: String,
    #[serde(default = "default_tier")]
    pub tier: i64,
    #[serde(default)]
    pub expires_at: i64,
    #[serde(default = "default_total_credits")]
    pub total_credits: i64,
    #[serde(default = "default_true")]
    pub allow_tunnel_egress: bool,
}

impl Default for KeyConfig {
    fn default() -> Self {
        Self {
            key: String::new(),
            account_id: default_account_id(),
            tier: default_tier(),
            expires_at: 0,
            total_credits: default_total_credits(),
            allow_tunnel_egress: true,
        }
    }
}

fn default_account_id() -> String {
    "main".to_string()
}
fn default_tier() -> i64 {
    3
}
fn default_total_credits() -> i64 {
    9999
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GoogleConfig {
    pub token_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub refresh_skew_secs: u64,
    pub accounts: Vec<AccountConfig>,
}

impl Default for GoogleConfig {
    fn default() -> Self {
        Self {
            token_url: "https://oauth2.googleapis.com/token".to_string(),
            client_id: String::new(),
            client_secret: String::new(),
            refresh_skew_secs: 120,
            accounts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccountConfig {
    pub id: String,
    pub refresh_token: String,
    pub project_id: String,
}

impl Default for AccountConfig {
    fn default() -> Self {
        Self {
            id: default_account_id(),
            refresh_token: String::new(),
            project_id: String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpstreamConfig {
    pub base_url: String,
    pub normalize_v1internal_colon: bool,
    pub passthrough_unknown_v1: bool,
    pub connect_timeout_secs: u64,
    pub stream_idle_timeout_secs: u64,
    pub user_agent: String,
    pub extra_headers: HashMap<String, String>,
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        Self {
            base_url: "https://daily-cloudcode-pa.googleapis.com".to_string(),
            normalize_v1internal_colon: true,
            passthrough_unknown_v1: true,
            connect_timeout_secs: 15,
            stream_idle_timeout_secs: 300,
            user_agent: String::new(),
            extra_headers: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TunnelConfig {
    pub enabled: bool,
    /// direct | allowlist | prefer_tunnel | always
    pub routing_mode: String,
    pub route_hosts: Vec<String>,
    pub handshake_timeout_ms: u64,
    pub acquire_timeout_ms: u64,
    pub idle_ping_secs: u64,
    pub max_workers_per_key: usize,
}

impl Default for TunnelConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            routing_mode: "direct".to_string(),
            route_hosts: Vec::new(),
            handshake_timeout_ms: 10_000,
            acquire_timeout_ms: 3_000,
            idle_ping_secs: 25,
            max_workers_per_key: 32,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelsConfig {
    pub advertise: Vec<String>,
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            advertise: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpdatesConfig {
    pub version: String,
    pub name: String,
    pub body: String,
    pub assets: HashMap<String, AssetConfig>,
}

impl Default for UpdatesConfig {
    fn default() -> Self {
        Self {
            version: String::new(),
            name: String::new(),
            body: String::new(),
            assets: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetConfig {
    pub id: String,
    pub path: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            auth: AuthConfig::default(),
            google: GoogleConfig::default(),
            upstream: UpstreamConfig::default(),
            tunnel: TunnelConfig::default(),
            models: ModelsConfig::default(),
            updates: UpdatesConfig::default(),
        }
    }
}

impl Config {
    /// Load config from TOML file (env `AG_CONFIG`, default `./server.toml`)
    /// and apply environment overrides. A missing file yields pure defaults.
    pub fn load() -> Result<Config, String> {
        let path = std::env::var("AG_CONFIG").unwrap_or_else(|_| "./server.toml".to_string());
        let mut config = match std::fs::read_to_string(&path) {
            Ok(text) => Self::from_toml_str(&text)
                .map_err(|e| format!("failed to parse config {path}: {e}"))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(path, "config file not found, using defaults");
                Config::default()
            }
            Err(err) => return Err(format!("failed to read config {path}: {err}")),
        };
        config.apply_env(|name| std::env::var(name).ok());
        Ok(config)
    }

    pub fn from_toml_str(text: &str) -> Result<Config, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }

    /// Apply environment overrides. `var` abstracts the environment for tests.
    pub fn apply_env(&mut self, var: impl Fn(&str) -> Option<String>) {
        if let Some(v) = var("AG_LISTEN_ADDR") {
            self.server.listen_addr = v;
        }
        if let Some(v) = var("AG_PUBLIC_URL") {
            self.server.public_url = v;
        }
        if let Some(v) = var("AG_UPSTREAM_BASE_URL") {
            self.upstream.base_url = v;
        }
        if let Some(v) = var("AG_TUNNEL_ENABLED") {
            match v.parse::<bool>() {
                Ok(b) => self.tunnel.enabled = b,
                Err(_) => tracing::warn!(value = %v, "ignoring invalid AG_TUNNEL_ENABLED"),
            }
        }
        if let Some(v) = var("AG_GOOGLE_CLIENT_ID") {
            self.google.client_id = v;
        }
        if let Some(v) = var("AG_GOOGLE_CLIENT_SECRET") {
            self.google.client_secret = v;
        }
        if let Some(v) = var("AG_CLIENT_KEY") {
            if let Some(entry) = self.auth.keys.iter_mut().find(|k| k.account_id == "main") {
                entry.key = v;
            } else {
                self.auth.keys.push(KeyConfig {
                    key: v,
                    account_id: "main".to_string(),
                    ..KeyConfig::default()
                });
            }
        }
        if let Some(v) = var("AG_GOOGLE_REFRESH_TOKEN") {
            if let Some(acc) = self.google.accounts.iter_mut().find(|a| a.id == "main") {
                acc.refresh_token = v;
            } else {
                self.google.accounts.push(AccountConfig {
                    id: "main".to_string(),
                    refresh_token: v,
                    project_id: String::new(),
                });
            }
        }
    }

    /// Effective advertised model list (configured list, or the default constant).
    pub fn effective_advertise(&self) -> Vec<String> {
        if self.models.advertise.is_empty() {
            default_model_ids()
        } else {
            self.models.advertise.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that mutate the real process environment.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn temp_config_path(name: &str) -> std::path::PathBuf {
        let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push("target");
        std::fs::create_dir_all(&p).ok();
        p.push(format!("ag-test-{}-{}.toml", name, std::process::id()));
        p
    }

    #[test]
    fn default_model_ids_has_at_least_29() {
        let ids = default_model_ids();
        assert!(ids.len() >= 29, "expected >=29 ids, got {}", ids.len());
        assert_eq!(ids[0], "claude-sonnet-4-6");
        assert_eq!(ids[3], "claude-opus-4-6-thinking");
        assert!(ids.iter().any(|i| i == "gpt-oss-120b-medium"));
        let mut dedup: Vec<&String> = ids.iter().collect();
        dedup.sort();
        dedup.dedup();
        assert_eq!(dedup.len(), ids.len(), "duplicate ids present");
    }

    #[test]
    fn parse_minimal_toml_applies_defaults() {
        let cfg = Config::from_toml_str(
            r#"
[auth]
[[auth.keys]]
key = "k1"

[google]
client_id = "cid"
"#,
        )
        .expect("minimal toml parses");
        assert_eq!(cfg.server.listen_addr, "0.0.0.0:8045");
        assert_eq!(cfg.server.max_body_bytes, 67_108_864);
        assert_eq!(cfg.server.state_file, "./state.json");
        assert_eq!(cfg.upstream.base_url, "https://daily-cloudcode-pa.googleapis.com");
        assert!(cfg.upstream.normalize_v1internal_colon);
        assert!(cfg.upstream.passthrough_unknown_v1);
        assert_eq!(cfg.upstream.connect_timeout_secs, 15);
        assert_eq!(cfg.upstream.stream_idle_timeout_secs, 300);
        assert!(cfg.tunnel.enabled);
        assert_eq!(cfg.tunnel.routing_mode, "direct");
        assert_eq!(cfg.tunnel.max_workers_per_key, 32);
        assert_eq!(cfg.google.token_url, "https://oauth2.googleapis.com/token");
        assert_eq!(cfg.google.refresh_skew_secs, 120);
        assert_eq!(cfg.auth.keys.len(), 1);
        assert_eq!(cfg.auth.keys[0].account_id, "main");
        assert_eq!(cfg.auth.keys[0].tier, 3);
        assert_eq!(cfg.auth.keys[0].expires_at, 0);
        assert_eq!(cfg.auth.keys[0].total_credits, 9999);
        assert!(cfg.auth.keys[0].allow_tunnel_egress);
        assert_eq!(cfg.effective_advertise(), default_model_ids());
    }

    #[test]
    fn apply_env_overrides_via_injected_env() {
        let mut cfg = Config::default();
        let env = |name: &str| -> Option<String> {
            match name {
                "AG_LISTEN_ADDR" => Some("127.0.0.1:9999".to_string()),
                "AG_UPSTREAM_BASE_URL" => Some("https://upstream.example".to_string()),
                "AG_TUNNEL_ENABLED" => Some("false".to_string()),
                "AG_CLIENT_KEY" => Some("env-key".to_string()),
                _ => None,
            }
        };
        cfg.apply_env(|n| env(n));
        assert_eq!(cfg.server.listen_addr, "127.0.0.1:9999");
        assert_eq!(cfg.upstream.base_url, "https://upstream.example");
        assert!(!cfg.tunnel.enabled);
        assert_eq!(cfg.auth.keys.len(), 1);
        assert_eq!(cfg.auth.keys[0].key, "env-key");
        assert_eq!(cfg.auth.keys[0].account_id, "main");
    }

    #[test]
    fn apply_env_client_key_replaces_existing_main_key() {
        let mut cfg = Config::default();
        cfg.auth.keys.push(KeyConfig {
            key: "old".to_string(),
            account_id: "main".to_string(),
            ..KeyConfig::default()
        });
        cfg.apply_env(|n| {
            if n == "AG_CLIENT_KEY" {
                Some("new".to_string())
            } else {
                None
            }
        });
        assert_eq!(cfg.auth.keys.len(), 1);
        assert_eq!(cfg.auth.keys[0].key, "new");
    }

    #[test]
    fn env_overrides_via_real_process_env() {
        let _guard = env_lock();
        for name in [
            "AG_LISTEN_ADDR",
            "AG_UPSTREAM_BASE_URL",
            "AG_TUNNEL_ENABLED",
            "AG_CLIENT_KEY",
            "AG_CONFIG",
        ] {
            std::env::remove_var(name);
        }
        std::env::set_var("AG_LISTEN_ADDR", "127.0.0.1:1234");
        std::env::set_var("AG_UPSTREAM_BASE_URL", "https://env-upstream.example");
        std::env::set_var("AG_TUNNEL_ENABLED", "false");
        std::env::set_var("AG_CLIENT_KEY", "real-env-key");
        // Point AG_CONFIG at a nonexistent file so defaults are used.
        std::env::set_var("AG_CONFIG", "./__definitely_missing_ag_config__.toml");

        let cfg = Config::load().expect("load with env overrides");
        assert_eq!(cfg.server.listen_addr, "127.0.0.1:1234");
        assert_eq!(cfg.upstream.base_url, "https://env-upstream.example");
        assert!(!cfg.tunnel.enabled);
        assert_eq!(cfg.auth.keys.len(), 1);
        assert_eq!(cfg.auth.keys[0].key, "real-env-key");

        for name in [
            "AG_LISTEN_ADDR",
            "AG_UPSTREAM_BASE_URL",
            "AG_TUNNEL_ENABLED",
            "AG_CLIENT_KEY",
            "AG_CONFIG",
        ] {
            std::env::remove_var(name);
        }
    }

    #[test]
    fn load_reads_toml_file_from_ag_config() {
        let _guard = env_lock();
        std::env::remove_var("AG_LISTEN_ADDR");
        std::env::remove_var("AG_UPSTREAM_BASE_URL");
        std::env::remove_var("AG_TUNNEL_ENABLED");
        std::env::remove_var("AG_CLIENT_KEY");
        let path = temp_config_path("load_file");
        std::fs::write(
            &path,
            "[server]\nlisten_addr = \"127.0.0.1:7777\"\nmax_body_bytes = 1024\n",
        )
        .unwrap();
        std::env::set_var("AG_CONFIG", &path);
        let cfg = Config::load().expect("load from file");
        std::env::remove_var("AG_CONFIG");
        std::fs::remove_file(&path).ok();
        assert_eq!(cfg.server.listen_addr, "127.0.0.1:7777");
        assert_eq!(cfg.server.max_body_bytes, 1024);
        assert_eq!(cfg.server.public_url, "");
    }

    #[test]
    fn config_example_toml_parses() {
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/config.example.toml"
        ))
        .expect("config.example.toml readable");
        let cfg = Config::from_toml_str(&text).expect("config.example.toml parses");
        assert_eq!(cfg.auth.keys.len(), 1);
        assert_eq!(cfg.auth.keys[0].key, "IMPULS-change-me");
        assert_eq!(cfg.upstream.base_url, "https://daily-cloudcode-pa.googleapis.com");
        assert!(cfg.google.client_id.is_empty());
    }

    #[test]
    fn defaults_applied_with_no_file_and_no_env() {
        let _guard = env_lock();
        for name in [
            "AG_LISTEN_ADDR",
            "AG_PUBLIC_URL",
            "AG_UPSTREAM_BASE_URL",
            "AG_TUNNEL_ENABLED",
            "AG_CLIENT_KEY",
            "AG_GOOGLE_REFRESH_TOKEN",
            "AG_GOOGLE_CLIENT_ID",
            "AG_GOOGLE_CLIENT_SECRET",
        ] {
            std::env::remove_var(name);
        }
        let cfg = Config::default();
        assert_eq!(cfg.server.listen_addr, "0.0.0.0:8045");
        assert_eq!(cfg.server.public_url, "");
        assert_eq!(cfg.upstream.base_url, "https://daily-cloudcode-pa.googleapis.com");
        assert!(cfg.tunnel.enabled);
        assert!(cfg.auth.keys.is_empty());
        assert!(cfg.google.accounts.is_empty());
        assert!(cfg.google.client_id.is_empty());
        assert!(cfg.upstream.extra_headers.is_empty());
        assert_eq!(cfg.effective_advertise().len(), default_model_ids().len());
    }
}
