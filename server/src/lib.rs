pub mod auth;
pub mod config;
pub mod error;
pub mod handlers;
pub mod oauth;
pub mod tunnel;
pub mod upstream;

mod chat_request;
mod chat_response;

use crate::config::Config;
use crate::tunnel::{ConnectProxy, TunnelRegistry};
use axum::extract::DefaultBodyLimit;
use axum::routing::{any, get, post};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

pub use error::AppError;

/// Shared application state handed to all handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub http: reqwest::Client,
    pub credit_overages: Arc<AtomicBool>,
    pub oauth: Arc<oauth::OAuthManager>,
    pub tunnel: Arc<TunnelRegistry>,
    /// Lazily spawned loopback CONNECT bridges, one per API key, used for
    /// tunneled upstream egress (`routing_mode != "direct"`).
    pub tunnel_proxies: Arc<tokio::sync::Mutex<HashMap<String, Arc<ConnectProxy>>>>,
    pub(crate) openai_projects: Arc<tokio::sync::Mutex<HashMap<String, String>>>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        let mut builder = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(config.upstream.connect_timeout_secs))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .tcp_nodelay(true);
        if !config.upstream.user_agent.is_empty() {
            builder = builder.user_agent(config.upstream.user_agent.clone());
        }
        let http = builder
            .build()
            .expect("failed to build reqwest client");
        let credit_overages = load_persisted_overages(config.server.state_file.as_str());
        let max_workers_per_key = config.tunnel.max_workers_per_key;
        let config = Arc::new(config);
        let oauth = Arc::new(oauth::OAuthManager::new(&config, http.clone()));
        // Proactive token refresh: tokens within this window of expiry are renewed.
        let skew = std::time::Duration::from_secs(config.google.refresh_skew_secs);
        oauth.clone().spawn_background_refresh(skew);
        Self {
            config,
            http,
            credit_overages: Arc::new(AtomicBool::new(credit_overages)),
            oauth,
            tunnel: Arc::new(TunnelRegistry::new(max_workers_per_key)),
            tunnel_proxies: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            openai_projects: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }
}

/// Best-effort load of the persisted `enable_credit_overages` flag from the
/// state file. Missing file or malformed JSON yields `false`.
pub fn load_persisted_overages(state_file: &str) -> bool {
    std::fs::read_to_string(state_file)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("enable_credit_overages")?.as_bool())
        .unwrap_or(false)
}

/// Build the application router. Later tasks add routes here.
pub fn build_router(state: AppState) -> axum::Router {
    let max_body_bytes = state.config.server.max_body_bytes;
    axum::Router::new()
        .route("/health", get(handlers::health::health))
        .route("/v1/models", get(handlers::models::models))
        .route("/v1/chat/completions", any(handlers::chat::completions))
        .route("/v1/quota", get(handlers::quota::quota))
        .route("/v1/quota/config", post(handlers::quota::quota_config))
        .route("/v1/client-update", get(handlers::update::client_update))
        .route(
            "/v1/client-update/download/{asset_id}",
            get(handlers::update::client_update_download),
        )
        .route("/v1/tunnel", get(tunnel::tunnel_ws))
        .fallback(handlers::forward::forward)
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .with_state(state)
}
