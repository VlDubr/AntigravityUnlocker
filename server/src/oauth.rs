//! Google OAuth2 access-token manager.
//!
//! Per-account cached access tokens with singleflight refresh, round-robin
//! account selection for the `"*"` pseudo-account, generation counters for
//! lock-free single-retry invalidation, 429 cooldowns, and an optional
//! background proactive-refresh task.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, RwLock};
use tracing::warn;

use crate::config::Config;
use crate::error::AppError;

/// How long an account is skipped after a 429 from the token endpoint.
const COOLDOWN: Duration = Duration::from_secs(60);
/// How long a session stays pinned to the same Google account under `"*"`
/// wildcard routing so upstream KV/prompt caches stay warm.
const SESSION_AFFINITY_TTL: Duration = Duration::from_secs(15 * 60);
const SESSION_AFFINITY_PRUNE_THRESHOLD: usize = 1024;
const SESSION_AFFINITY_MAX_ENTRIES: usize = 4096;
/// Background refresh loop period.
const BACKGROUND_TICK: Duration = Duration::from_secs(30);
/// Hard ceiling on a single token-endpoint refresh (send + body read). Without
/// it a stalled response would hold the per-account singleflight lock forever
/// and hang every subsequent refresh for that account.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(15);

/// A cached access token for one account.
#[derive(Debug, Clone)]
pub struct CachedToken {
    pub access_token: String,
    pub expires_at: Instant,
    pub generation: u64,
}

struct AccountState {
    id: String,
    refresh_token: String,
    token: RwLock<Option<CachedToken>>,
    /// Singleflight: only one task refreshes this account at a time.
    refresh_lock: Mutex<()>,
    /// Incremented on every successful refresh.
    generation: AtomicU64,
}

impl AccountState {
    /// Clone of the cached token if present and not within `skew` of expiry.
    async fn cached_fresh(&self, skew: Duration) -> Option<CachedToken> {
        let guard = self.token.read().await;
        guard
            .as_ref()
            .filter(|t| t.expires_at > Instant::now() + skew)
            .cloned()
    }
}

#[derive(serde::Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
}

/// Manages Google OAuth2 access tokens for all configured accounts.
pub struct OAuthManager {
    http: reqwest::Client,
    token_url: String,
    client_id: String,
    client_secret: String,
    refresh_skew: Duration,
    accounts: Vec<AccountState>,
    /// Round-robin cursor for the `"*"` pseudo-account.
    rr: AtomicUsize,
    /// account id -> cooldown-until instant (set on upstream 429).
    cooldowns: Mutex<HashMap<String, Instant>>,
    /// Fixed-size hash of session affinity key -> (concrete account id, expires_at).
    /// Lock order: always acquire `session_accounts` before `cooldowns`.
    session_accounts: Mutex<HashMap<[u8; 16], (String, Instant)>>,
}

impl OAuthManager {
    pub fn new(config: &Config, http: reqwest::Client) -> Self {
        let accounts = config
            .google
            .accounts
            .iter()
            .map(|a| AccountState {
                id: a.id.clone(),
                refresh_token: a.refresh_token.clone(),
                token: RwLock::new(None),
                refresh_lock: Mutex::new(()),
                generation: AtomicU64::new(0),
            })
            .collect();
        Self {
            http,
            token_url: config.google.token_url.clone(),
            client_id: config.google.client_id.clone(),
            client_secret: config.google.client_secret.clone(),
            refresh_skew: Duration::from_secs(config.google.refresh_skew_secs),
            accounts,
            rr: AtomicUsize::new(0),
            cooldowns: Mutex::new(HashMap::new()),
            session_accounts: Mutex::new(HashMap::new()),
        }
    }

    fn find(&self, id: &str) -> Option<&AccountState> {
        self.accounts.iter().find(|a| a.id == id)
    }

    async fn on_cooldown(&self, id: &str) -> bool {
        let map = self.cooldowns.lock().await;
        map.get(id).is_some_and(|until| *until > Instant::now())
    }

    async fn set_cooldown(&self, id: &str) {
        self.cooldowns
            .lock()
            .await
            .insert(id.to_string(), Instant::now() + COOLDOWN);
    }

    /// Resolve the account for `account_id`. `"*"` means round-robin across
    /// accounts, skipping any account currently in 429 cooldown.
    async fn resolve(&self, account_id: &str) -> Result<&AccountState, AppError> {
        if account_id != "*" {
            return self
                .find(account_id)
                .ok_or_else(|| AppError::Internal(format!("unknown account: {account_id}")));
        }
        let n = self.accounts.len();
        if n == 0 {
            return Err(AppError::Internal(
                "unknown account: no accounts configured".to_string(),
            ));
        }
        let start = self.rr.fetch_add(1, Ordering::Relaxed);
        for i in 0..n {
            let acc = &self.accounts[(start + i) % n];
            if !self.on_cooldown(&acc.id).await {
                return Ok(acc);
            }
        }
        Err(AppError::BadGateway(
            "all accounts in cooldown".to_string(),
        ))
    }

    /// Get a valid access token for `account_id` (`"*"` = round-robin).
    pub async fn get_token(&self, account_id: &str) -> Result<String, AppError> {
        self.get_token_with_generation(account_id)
            .await
            .map(|(token, _, _)| token)
    }

    /// Like [`get_token`](Self::get_token) but also returns the token
    /// generation and the **concrete** account id that served it (resolved
    /// from `"*"` round-robin). Callers must pass that concrete id — not the
    /// `"*"` pseudo-id — to
    /// [`invalidate_if_generation`](Self::invalidate_if_generation), otherwise
    /// invalidation silently no-ops and a 401 retry reuses the rejected token.
    pub async fn get_token_with_generation(
        &self,
        account_id: &str,
    ) -> Result<(String, u64, String), AppError> {
        let acc = self.resolve(account_id).await?;
        self.get_token_for_account(acc).await
    }

    fn session_key_hash(session_key: &str) -> [u8; 16] {
        let digest = crate::handlers::chat::sha256(session_key.as_bytes());
        let mut out = [0u8; 16];
        out.copy_from_slice(&digest[..16]);
        out
    }

    /// Remove a session's sticky account pin if it still points to `account_id`.
    /// Called on account-scoped generation quota (429) errors so retries can
    /// rotate to another healthy account in a wildcard pool.
    pub async fn unpin_session(&self, session_key: &str, account_id: &str) {
        if session_key.is_empty() {
            return;
        }
        let key_hash = Self::session_key_hash(session_key);
        let mut map = self.session_accounts.lock().await;
        if map.get(&key_hash).is_some_and(|(id, _)| id == account_id) {
            map.remove(&key_hash);
        }
    }

    /// Like [`get_token_with_generation`](Self::get_token_with_generation),
    /// but when `account_id == "*"` and a non-empty `session_key` is provided,
    /// atomically pins that session to one healthy Google account for
    /// [`SESSION_AFFINITY_TTL`] so multi-turn prompt caching hits the same
    /// account. Falls back to round-robin if the pinned account is missing or
    /// on 429 cooldown.
    pub async fn get_token_for_session(
        &self,
        account_id: &str,
        session_key: Option<&str>,
    ) -> Result<(String, u64, String), AppError> {
        let Some(sk) = session_key.filter(|s| !s.is_empty() && account_id == "*") else {
            return self.get_token_with_generation(account_id).await;
        };

        let key_hash = Self::session_key_hash(sk);
        let now = Instant::now();
        let acc = {
            let mut map = self.session_accounts.lock().await;
            let cooldowns = self.cooldowns.lock().await;
            let is_cooled = |id: &str| cooldowns.get(id).is_some_and(|until| *until > now);

            if let Some((mapped_id, expires_at)) = map.get(&key_hash) {
                if *expires_at > now && !is_cooled(mapped_id) {
                    if let Some(acc) = self.find(mapped_id) {
                        let id = acc.id.clone();
                        map.insert(key_hash, (id, now + SESSION_AFFINITY_TTL));
                        acc
                    } else {
                        self.pick_and_pin_locked(&mut map, &is_cooled, key_hash, now)?
                    }
                } else {
                    self.pick_and_pin_locked(&mut map, &is_cooled, key_hash, now)?
                }
            } else {
                self.pick_and_pin_locked(&mut map, &is_cooled, key_hash, now)?
            }
        };

        self.get_token_for_account(acc).await
    }

    fn pick_and_pin_locked<'a>(
        &'a self,
        map: &mut HashMap<[u8; 16], (String, Instant)>,
        is_cooled: &impl Fn(&str) -> bool,
        key_hash: [u8; 16],
        now: Instant,
    ) -> Result<&'a AccountState, AppError> {
        let n = self.accounts.len();
        if n == 0 {
            return Err(AppError::Internal(
                "unknown account: no accounts configured".to_string(),
            ));
        }
        let start = self.rr.fetch_add(1, Ordering::Relaxed);
        let mut chosen = None;
        for i in 0..n {
            let acc = &self.accounts[(start + i) % n];
            if !is_cooled(&acc.id) {
                chosen = Some(acc);
                break;
            }
        }
        let acc = chosen.ok_or_else(|| AppError::BadGateway("all accounts in cooldown".to_string()))?;
        if map.len() >= SESSION_AFFINITY_PRUNE_THRESHOLD {
            map.retain(|_, (_, expires_at)| *expires_at > now);
        }
        if map.len() >= SESSION_AFFINITY_MAX_ENTRIES {
            if let Some(oldest_key) = map
                .iter()
                .min_by_key(|(_, (_, exp))| *exp)
                .map(|(k, _)| *k)
            {
                map.remove(&oldest_key);
            }
        }
        map.insert(key_hash, (acc.id.clone(), now + SESSION_AFFINITY_TTL));
        Ok(acc)
    }

    async fn get_token_for_account(
        &self,
        acc: &AccountState,
    ) -> Result<(String, u64, String), AppError> {
        // Fast path: valid cached token.
        if let Some(t) = acc.cached_fresh(self.refresh_skew).await {
            return Ok((t.access_token, t.generation, acc.id.clone()));
        }
        // Singleflight: one refresher at a time...
        let _guard = acc.refresh_lock.lock().await;
        // ...re-check; another task may have refreshed while we waited.
        if let Some(t) = acc.cached_fresh(self.refresh_skew).await {
            return Ok((t.access_token, t.generation, acc.id.clone()));
        }
        if acc.refresh_token.is_empty() {
            return Err(AppError::Internal(format!(
                "no refresh_token configured for account {}",
                acc.id
            )));
        }
        let token = self.refresh(acc).await?;
        Ok((token.access_token, token.generation, acc.id.clone()))
    }

    /// POST a refresh_token grant to the Google token endpoint and cache the
    /// result. Caller must hold `acc.refresh_lock`.
    async fn refresh(&self, acc: &AccountState) -> Result<CachedToken, AppError> {
        let mut form: Vec<(&str, &str)> = vec![
            ("grant_type", "refresh_token"),
            ("client_id", self.client_id.as_str()),
            ("refresh_token", acc.refresh_token.as_str()),
        ];
        if !self.client_secret.is_empty() {
            form.push(("client_secret", self.client_secret.as_str()));
        }
        // Bound the whole refresh (send + body read) so a stalled token
        // endpoint cannot hold the singleflight lock indefinitely.
        let resp = tokio::time::timeout(
            REFRESH_TIMEOUT,
            self.http.post(&self.token_url).form(&form).send(),
        )
        .await
        .map_err(|_| AppError::BadGateway("oauth refresh timed out".to_string()))?
        .map_err(|e| AppError::BadGateway(format!("oauth refresh failed: {e}")))?;
        let status = resp.status();
        let body = tokio::time::timeout(REFRESH_TIMEOUT, resp.text())
            .await
            .map_err(|_| AppError::BadGateway("oauth refresh body read timed out".to_string()))?
            .unwrap_or_default();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            self.set_cooldown(&acc.id).await;
            warn!(account = %acc.id, "oauth refresh got 429, cooling account down");
        }
        if !status.is_success() {
            return Err(AppError::BadGateway(format!(
                "oauth refresh failed: {status} {body}"
            )));
        }
        let parsed: TokenResponse = serde_json::from_str(&body).map_err(|e| {
            AppError::BadGateway(format!("oauth refresh failed: bad token response: {e}"))
        })?;
        let generation = acc.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let token = CachedToken {
            access_token: parsed.access_token,
            expires_at: Instant::now() + Duration::from_secs(parsed.expires_in),
            generation,
        };
        *acc.token.write().await = Some(token.clone());
        Ok(token)
    }

    /// Clear the cached token for `account_id` if it still carries generation
    /// `seen_gen`, so the next [`get_token`](Self::get_token) refreshes.
    /// Supports lock-free single-retry on upstream 401.
    pub async fn invalidate_if_generation(&self, account_id: &str, seen_gen: u64) {
        let Some(acc) = self.find(account_id) else {
            return;
        };
        if acc.generation.load(Ordering::SeqCst) != seen_gen {
            return;
        }
        let mut guard = acc.token.write().await;
        if guard.as_ref().map(|t| t.generation) == Some(seen_gen) {
            *guard = None;
        }
    }

    /// Spawn a best-effort background loop that refreshes, every 30s, tokens
    /// that are missing or within `skew` of expiry. Failures are logged; any
    /// still-valid cached token is kept.
    pub fn spawn_background_refresh(self: Arc<Self>, skew: Duration) {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(BACKGROUND_TICK);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                for acc in &self.accounts {
                    let needs_refresh = match acc.token.read().await.as_ref() {
                        Some(t) => t.expires_at <= Instant::now() + skew,
                        None => true,
                    };
                    if needs_refresh {
                        if let Err(e) = self.get_token(&acc.id).await {
                            warn!(
                                account = %acc.id,
                                error = %e,
                                "background oauth refresh failed, keeping existing token"
                            );
                        }
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AccountConfig;
    use axum::extract::State;
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::json;
    use tokio::net::TcpListener;

    #[derive(Clone)]
    struct MockState {
        hits: Arc<AtomicUsize>,
        expires_in: u64,
        last_form: Arc<std::sync::Mutex<HashMap<String, String>>>,
    }

    async fn token_handler(
        State(st): State<MockState>,
        axum::Form(form): axum::Form<HashMap<String, String>>,
    ) -> Json<serde_json::Value> {
        st.hits.fetch_add(1, Ordering::SeqCst);
        *st.last_form.lock().unwrap() = form;
        Json(json!({ "access_token": "ya29.test", "expires_in": st.expires_in }))
    }

    async fn spawn_mock(expires_in: u64) -> (String, MockState) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let st = MockState {
            hits: Arc::new(AtomicUsize::new(0)),
            expires_in,
            last_form: Arc::new(std::sync::Mutex::new(HashMap::new())),
        };
        let app = Router::new()
            .route("/token", post(token_handler))
            .with_state(st.clone());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}/token"), st)
    }

    fn manager(token_url: String, client_secret: &str, accounts: Vec<(&str, &str)>) -> OAuthManager {
        let mut config = Config::default();
        config.google.token_url = token_url;
        config.google.client_id = "cid".to_string();
        config.google.client_secret = client_secret.to_string();
        config.google.accounts = accounts
            .into_iter()
            .map(|(id, rt)| AccountConfig {
                id: id.to_string(),
                refresh_token: rt.to_string(),
                project_id: "proj".to_string(),
            })
            .collect();
        OAuthManager::new(&config, reqwest::Client::new())
    }

    // (a) 10 concurrent get_token calls trigger exactly one HTTP refresh.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_get_token_performs_single_refresh() {
        let (url, mock) = spawn_mock(3599).await;
        let mgr = Arc::new(manager(url, "secret", vec![("main", "rt")]));
        let mut handles = Vec::new();
        for _ in 0..10 {
            let m = mgr.clone();
            handles.push(tokio::spawn(async move { m.get_token("main").await.unwrap() }));
        }
        for h in handles {
            assert_eq!(h.await.unwrap(), "ya29.test");
        }
        assert_eq!(mock.hits.load(Ordering::SeqCst), 1);
    }

    // (b) invalidate_if_generation -> exactly one new refresh on next get_token.
    #[tokio::test]
    async fn invalidate_if_generation_forces_one_refresh() {
        let (url, mock) = spawn_mock(3599).await;
        let mgr = manager(url, "secret", vec![("main", "rt")]);
        let (token, gen1, _concrete) = mgr.get_token_with_generation("main").await.unwrap();
        assert_eq!(token, "ya29.test");
        // Served from cache: no extra refresh.
        mgr.get_token("main").await.unwrap();
        assert_eq!(mock.hits.load(Ordering::SeqCst), 1);

        mgr.invalidate_if_generation("main", gen1).await;
        mgr.get_token("main").await.unwrap();
        assert_eq!(mock.hits.load(Ordering::SeqCst), 2);

        // Stale generation must not clear the fresh token.
        mgr.invalidate_if_generation("main", gen1).await;
        mgr.get_token("main").await.unwrap();
        assert_eq!(mock.hits.load(Ordering::SeqCst), 2);
    }

    // (c) empty client_secret omits the form key; non-empty includes it.
    #[tokio::test]
    async fn empty_client_secret_omits_form_key() {
        let (url, mock) = spawn_mock(3599).await;
        let mgr = manager(url, "", vec![("main", "rt")]);
        mgr.get_token("main").await.unwrap();
        let form = mock.last_form.lock().unwrap().clone();
        assert!(
            !form.contains_key("client_secret"),
            "client_secret must be omitted when empty, form: {form:?}"
        );
        assert_eq!(form.get("grant_type").map(String::as_str), Some("refresh_token"));
        assert_eq!(form.get("client_id").map(String::as_str), Some("cid"));
        assert_eq!(form.get("refresh_token").map(String::as_str), Some("rt"));

        let (url2, mock2) = spawn_mock(3599).await;
        let mgr2 = manager(url2, "s3cr3t", vec![("main", "rt")]);
        mgr2.get_token("main").await.unwrap();
        assert_eq!(
            mock2.last_form.lock().unwrap().get("client_secret").map(String::as_str),
            Some("s3cr3t")
        );
    }

    // (d) token expiring within the skew is refreshed.
    #[tokio::test]
    async fn expired_token_is_refreshed() {
        let (url, mock) = spawn_mock(1).await; // expires_in=1s << refresh skew
        let mgr = manager(url, "secret", vec![("main", "rt")]);
        mgr.get_token("main").await.unwrap();
        assert_eq!(mock.hits.load(Ordering::SeqCst), 1);
        // Already inside the skew window -> refreshed again.
        mgr.get_token("main").await.unwrap();
        assert_eq!(mock.hits.load(Ordering::SeqCst), 2);
        // Truly expired.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        mgr.get_token("main").await.unwrap();
        assert_eq!(mock.hits.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn wildcard_round_robins_accounts() {
        let (url, mock) = spawn_mock(3599).await;
        let mgr = manager(url, "secret", vec![("a", "rt1"), ("b", "rt2")]);
        for _ in 0..4 {
            mgr.get_token("*").await.unwrap();
        }
        // One refresh per distinct account, not per call.
        assert_eq!(mock.hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn unknown_account_and_missing_refresh_token() {
        let (url, _mock) = spawn_mock(3599).await;
        let mgr = manager(url, "secret", vec![("main", ""), ("other", "rt")]);
        let err = mgr.get_token("nope").await.unwrap_err();
        assert!(err.message().contains("unknown account"), "got: {}", err.message());
        let err = mgr.get_token("main").await.unwrap_err();
        assert!(
            err.message().contains("no refresh_token configured for account"),
            "got: {}",
            err.message()
        );
    }

    // Regression: get_token_with_generation("*") must return the CONCRETE
    // account id so invalidate_if_generation can target it. Invalidating the
    // "*" pseudo-id no-ops and a 401 retry would reuse the rejected token.
    #[tokio::test]
    async fn wildcard_returns_concrete_id_and_invalidation_works() {
        let (url, mock) = spawn_mock(3599).await;
        let mgr = manager(url, "secret", vec![("a", "rt1"), ("b", "rt2")]);
        let (_token, gen, concrete) = mgr.get_token_with_generation("*").await.unwrap();
        assert!(concrete == "a" || concrete == "b", "concrete id: {concrete}");
        assert_ne!(concrete, "*");
        let hits_before = mock.hits.load(Ordering::SeqCst);
        // Invalidating the pseudo-id must NOT clear the cache (no-op).
        mgr.invalidate_if_generation("*", gen).await;
        mgr.get_token(&concrete).await.unwrap();
        assert_eq!(mock.hits.load(Ordering::SeqCst), hits_before);
        // Invalidating the concrete id forces exactly one refresh.
        mgr.invalidate_if_generation(&concrete, gen).await;
        mgr.get_token(&concrete).await.unwrap();
        assert_eq!(mock.hits.load(Ordering::SeqCst), hits_before + 1);
    }

    #[tokio::test]
    async fn sticky_session_pins_wildcard_account_and_fails_over_on_cooldown() {
        let (url, _mock) = spawn_mock(3599).await;
        let mgr = manager(url, "secret", vec![("a", "rt1"), ("b", "rt2")]);

        let (_, _, first) = mgr.get_token_for_session("*", Some("sess-1")).await.unwrap();
        for _ in 0..4 {
            let (_, _, next) = mgr.get_token_for_session("*", Some("sess-1")).await.unwrap();
            assert_eq!(next, first, "same session must stay pinned to one account");
        }

        // Distinct session gets the next round-robin account.
        let (_, _, second) = mgr.get_token_for_session("*", Some("sess-2")).await.unwrap();
        assert_ne!(second, first, "distinct sessions should distribute across pool");

        // Cooldown on the pinned account forces failover and re-pins to the healthy one.
        mgr.set_cooldown(&first).await;
        let (_, _, failover) = mgr.get_token_for_session("*", Some("sess-1")).await.unwrap();
        assert_ne!(failover, first, "cooled-down pinned account must fail over");

        // Unpinning a matching account rotates on the next call.
        let (url2, _mock2) = spawn_mock(3599).await;
        let mgr2 = Arc::new(manager(url2, "secret", vec![("a", "rt1"), ("b", "rt2")]));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let m = mgr2.clone();
            handles.push(tokio::spawn(async move {
                m.get_token_for_session("*", Some("concurrent-sess"))
                    .await
                    .unwrap()
                    .2
            }));
        }
        let mut picked = std::collections::HashSet::new();
        for h in handles {
            picked.insert(h.await.unwrap());
        }
        assert_eq!(picked.len(), 1, "concurrent calls for one session must pick one account");
        let pinned = picked.into_iter().next().unwrap();
        mgr2.unpin_session("concurrent-sess", &pinned).await;
        let (_, _, rotated) = mgr2
            .get_token_for_session("*", Some("concurrent-sess"))
            .await
            .unwrap();
        assert_ne!(rotated, pinned, "unpinned session should rotate to next account");
    }
}
