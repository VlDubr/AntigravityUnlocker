use crate::auth::AuthenticatedKey;
use crate::error::AppError;
use crate::handlers::forward;
use crate::handlers::models::RefreshQuery;
use crate::AppState;
use axum::body::{to_bytes, Body};
use axum::extract::{Query, State};
use axum::http::{header, Method, Request, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const PLAN_CACHE_TTL: Duration = Duration::from_secs(300);
const SUMMARY_CACHE_TTL: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
struct GroupBucket {
    bucket_id: String,
    window: String,
    percent: u64,
    reset_time: Option<String>,
}

#[derive(Debug, Clone)]
struct ParsedQuotaGroup {
    display_name: String,
    is_third_party: bool,
    is_gemini: bool,
    buckets: Vec<GroupBucket>,
}

impl ParsedQuotaGroup {
    /// Effective remaining percent and reset time across the group's windows
    /// (5h and weekly): the tighter (lower percent) window governs usability.
    fn effective_limit(&self) -> Option<(u64, Option<String>)> {
        let mut best: Option<(u64, bool, Option<String>)> = None;
        for b in &self.buckets {
            let is_5h = b.window.eq_ignore_ascii_case("5h") || b.bucket_id.ends_with("-5h");
            let replace = match &best {
                None => true,
                Some((cur_pct, cur_is_5h, _)) => {
                    b.percent < *cur_pct || (b.percent == *cur_pct && is_5h && !*cur_is_5h)
                }
            };
            if replace {
                best = Some((b.percent, is_5h, b.reset_time.clone()));
            }
        }
        best.map(|(pct, _, reset)| (pct, reset))
    }

    fn to_json(&self) -> Value {
        let buckets: Vec<Value> = self
            .buckets
            .iter()
            .map(|b| {
                json!({
                    "bucket_id": b.bucket_id,
                    "window": b.window,
                    "percent": b.percent,
                    "reset_time": b.reset_time,
                })
            })
            .collect();
        json!({
            "display_name": self.display_name,
            "buckets": buckets,
        })
    }
}

fn fraction_to_percent(fraction: f64) -> u64 {
    if fraction <= 0.0 {
        0
    } else if fraction >= 1.0 {
        100
    } else {
        (fraction * 100.0).round().clamp(1.0, 99.0) as u64
    }
}

fn parse_quota_summary(v: &Value) -> Vec<ParsedQuotaGroup> {
    let Some(groups) = v.get("groups").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for g in groups {
        let display_name = g
            .get("displayName")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        let Some(raw_buckets) = g.get("buckets").and_then(Value::as_array) else {
            continue;
        };
        let mut buckets = Vec::new();
        for b in raw_buckets {
            let bucket_id = b
                .get("bucketId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let window = b
                .get("window")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let reset_time = b
                .get("resetTime")
                .and_then(Value::as_str)
                .map(str::to_string);
            let fraction = b
                .get("remainingFraction")
                .and_then(Value::as_f64)
                .or_else(|| reset_time.is_some().then_some(0.0));
            let Some(fraction) = fraction else {
                continue;
            };
            buckets.push(GroupBucket {
                bucket_id,
                window,
                percent: fraction_to_percent(fraction),
                reset_time,
            });
        }
        if buckets.is_empty() {
            continue;
        }
        // Order 5h window first, then weekly, then anything else.
        buckets.sort_by_key(|b| {
            if b.window.eq_ignore_ascii_case("5h") || b.bucket_id.ends_with("-5h") {
                0
            } else if b.window.eq_ignore_ascii_case("weekly") || b.bucket_id.ends_with("-weekly") {
                1
            } else {
                2
            }
        });
        let lower_name = display_name.to_ascii_lowercase();
        let is_third_party = buckets
            .iter()
            .any(|b| b.bucket_id.to_ascii_lowercase().starts_with("3p-"))
            || lower_name.contains("claude")
            || lower_name.contains("gpt")
            || lower_name.contains("third");
        let is_gemini = buckets
            .iter()
            .any(|b| b.bucket_id.to_ascii_lowercase().starts_with("gemini-"))
            || lower_name.contains("gemini");
        out.push(ParsedQuotaGroup {
            display_name,
            is_third_party,
            is_gemini,
            buckets,
        });
    }
    out
}

/// GET /v1/quota — quota snapshot for the authenticated key.
/// `expires_at` is null or Unix SECONDS (the client multiplies by 1000).
pub async fn quota(
    State(state): State<AppState>,
    auth: AuthenticatedKey,
    Query(query): Query<RefreshQuery>,
) -> impl IntoResponse {
    let key = auth.0.clone();
    let force_refresh = query.force_refresh();
    let expires_at = if key.expires_at > 0 {
        Value::Number(key.expires_at.into())
    } else {
        Value::Null
    };

    let account_plan = fetch_account_plan(&state, &auth, force_refresh).await;
    let discovered_project = account_plan
        .as_ref()
        .and_then(|(_, _, proj)| proj.clone());
    let (tier, tier_id, plan_name) = match account_plan {
        Some((tid, pname, _)) => {
            let numeric = tier_id_to_numeric(&tid);
            (numeric, Value::String(tid), Value::String(pname))
        }
        None => (key.tier, Value::Null, Value::Null),
    };

    let summary_groups = fetch_quota_summary(
        &state,
        &auth,
        discovered_project.as_deref(),
        force_refresh,
    )
    .await
    .map(|v| parse_quota_summary(&v))
    .unwrap_or_default();

    let gemini_limit = summary_groups
        .iter()
        .find(|g| g.is_gemini)
        .and_then(ParsedQuotaGroup::effective_limit);
    let third_party_limit = summary_groups
        .iter()
        .find(|g| g.is_third_party)
        .and_then(ParsedQuotaGroup::effective_limit);

    // Real per-model remaining quota from the upstream account, when available.
    // Populate both raw variant IDs and consolidated base IDs.
    let mut models = serde_json::Map::new();
    let mut resets = serde_json::Map::new();
    if state.config.models.advertise.is_empty() {
        let raw = crate::handlers::models::fetch_raw_upstream_models(&state, &auth, force_refresh)
            .await
            .unwrap_or_default();
        let consolidated = crate::handlers::models::consolidate_models(&raw);
        for entry in raw.iter().chain(consolidated.iter()) {
            let Some(id) = entry.get("id").and_then(Value::as_str) else {
                continue;
            };
            let lower_id = id.to_ascii_lowercase();
            // Prefer authoritative 5h/weekly group summary from retrieveUserQuotaSummary
            // (fetchAvailableModels reports stale 100% for 3p Claude/GPT models).
            let group_override = if lower_id.starts_with("claude-") || lower_id.starts_with("gpt-")
            {
                third_party_limit.clone()
            } else if lower_id.starts_with("gemini-") {
                gemini_limit.clone()
            } else {
                None
            };
            if let Some((pct, reset)) = group_override {
                models.insert(id.to_string(), json!(pct));
                if let Some(reset) = reset {
                    resets.insert(id.to_string(), json!(reset));
                }
                continue;
            }
            let Some(fraction) = entry.get("quota_remaining_fraction").and_then(Value::as_f64)
            else {
                continue;
            };
            models.insert(id.to_string(), json!(fraction_to_percent(fraction)));
            if let Some(reset) = entry.get("quota_reset_time").and_then(Value::as_str) {
                resets.insert(id.to_string(), json!(reset));
            }
        }
    }

    let quota_groups_json: Vec<Value> = summary_groups
        .iter()
        .map(ParsedQuotaGroup::to_json)
        .collect();

    Json(json!({
        "total_credits": key.total_credits,
        "expires_at": expires_at,
        "enable_credit_overages": state.credit_overages.load(Ordering::Relaxed),
        "tier": tier,
        "tier_id": tier_id,
        "plan_name": plan_name,
        "models": models,
        "resets": resets,
        "quota_groups": quota_groups_json,
    }))
}

fn tier_id_to_numeric(tier_id: &str) -> i64 {
    let lower = tier_id.to_ascii_lowercase();
    if lower.contains("free") {
        0
    } else if lower.contains("ultra") || lower.contains("enterprise") {
        2
    } else {
        1
    }
}

fn extract_account_plan(v: &Value) -> Option<(String, String, Option<String>)> {
    let project = v
        .get("cloudaicompanionProject")
        .and_then(|p| p.as_str().or_else(|| p.get("id").and_then(Value::as_str)))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    if let Some(paid) = v.get("paidTier").and_then(Value::as_object) {
        let id = paid
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        let name = paid
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if !id.is_empty() || name.is_some() {
            let tier_id = if id.is_empty() {
                "paid-tier".to_string()
            } else {
                id.to_string()
            };
            let plan_name = name.unwrap_or(&tier_id).to_string();
            return Some((tier_id, plan_name, project));
        }
    }
    if let Some(cur) = v.get("currentTier").and_then(Value::as_object) {
        let id = cur
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        let name = cur
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if !id.is_empty() || name.is_some() {
            let tier_id = if id.is_empty() {
                "unknown".to_string()
            } else {
                id.to_string()
            };
            let plan_name = if tier_id.eq_ignore_ascii_case("free-tier") {
                "Free".to_string()
            } else {
                name.unwrap_or(&tier_id).to_string()
            };
            return Some((tier_id, plan_name, project));
        }
    }
    None
}

async fn fetch_account_plan(
    state: &AppState,
    auth: &AuthenticatedKey,
    force_refresh: bool,
) -> Option<(String, String, Option<String>)> {
    if state.config.google.accounts.is_empty() {
        return None;
    }

    let account_id = state
        .oauth
        .get_token_with_generation(&auth.0.account_id)
        .await
        .ok()?
        .2;

    let stale = {
        let cache = state.account_plans.lock().await;
        if let Some((fetched_at, cached)) = cache.get(&account_id) {
            if !force_refresh && fetched_at.elapsed() < PLAN_CACHE_TTL {
                return Some(cached.clone());
            }
            Some(cached.clone())
        } else {
            None
        }
    };

    let mut resolved_key = auth.0.clone();
    resolved_key.account_id = account_id.clone();

    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1internal:loadCodeAssist")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({ "metadata": { "ideType": "ANTIGRAVITY" } }).to_string(),
        ))
        .ok()?;

    let timeout = Duration::from_secs(state.config.upstream.connect_timeout_secs.clamp(1, 30));
    let resp = tokio::time::timeout(
        timeout,
        forward::forward(State(state.clone()), AuthenticatedKey(resolved_key), req),
    )
    .await
    .ok()?
    .ok()?;

    if !resp.status().is_success() {
        tracing::warn!(status = %resp.status(), "loadCodeAssist returned non-2xx for /v1/quota");
        return stale;
    }

    let bytes = tokio::time::timeout(timeout, to_bytes(resp.into_body(), 1024 * 1024))
        .await
        .ok()?
        .ok()?;
    let json_val: Value = serde_json::from_slice(&bytes).ok()?;
    let plan = extract_account_plan(&json_val)?;

    let mut cache = state.account_plans.lock().await;
    cache.insert(account_id, (Instant::now(), plan.clone()));
    Some(plan)
}

async fn fetch_quota_summary(
    state: &AppState,
    auth: &AuthenticatedKey,
    discovered_project: Option<&str>,
    force_refresh: bool,
) -> Option<Value> {
    if state.config.google.accounts.is_empty() {
        return None;
    }

    let account_id = state
        .oauth
        .get_token_with_generation(&auth.0.account_id)
        .await
        .ok()?
        .2;

    let stale = {
        let cache = state.quota_summary_cache.lock().await;
        if let Some((fetched_at, cached)) = cache.get(&account_id) {
            if !force_refresh && fetched_at.elapsed() < SUMMARY_CACHE_TTL {
                return Some(cached.clone());
            }
            Some(cached.clone())
        } else {
            None
        }
    };

    let configured_project = state
        .config
        .google
        .accounts
        .iter()
        .find(|a| a.id == account_id)
        .map(|a| a.project_id.trim())
        .filter(|s| !s.is_empty());

    let project = configured_project.or(discovered_project);
    let body = match project {
        Some(p) => json!({ "project": p }).to_string(),
        None => "{}".to_string(),
    };

    let mut resolved_key = auth.0.clone();
    resolved_key.account_id = account_id.clone();

    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1internal:retrieveUserQuotaSummary")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .ok()?;

    let timeout = Duration::from_secs(state.config.upstream.connect_timeout_secs.clamp(1, 30));
    let resp = tokio::time::timeout(
        timeout,
        forward::forward(State(state.clone()), AuthenticatedKey(resolved_key), req),
    )
    .await
    .ok()?
    .ok()?;

    if !resp.status().is_success() {
        return stale;
    }

    let bytes = tokio::time::timeout(timeout, to_bytes(resp.into_body(), 1024 * 1024))
        .await
        .ok()?
        .ok()?;
    let json_val: Value = serde_json::from_slice(&bytes).ok()?;
    if json_val.get("groups").and_then(Value::as_array).is_none() {
        return stale;
    }

    let mut cache = state.quota_summary_cache.lock().await;
    cache.insert(account_id, (Instant::now(), json_val.clone()));
    Some(json_val)
}

/// POST /v1/quota/config — flip the credit-overages flag. Auth required.
pub async fn quota_config(
    State(state): State<AppState>,
    _auth: AuthenticatedKey,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, AppError> {
    let enabled = body
        .get("enable_credit_overages")
        .and_then(Value::as_bool)
        .ok_or_else(|| AppError::BadRequest("enable_credit_overages (bool) required".to_string()))?;
    state.credit_overages.store(enabled, Ordering::Relaxed);
    persist_overages(&state, enabled);
    Ok((
        StatusCode::OK,
        Json(json!({ "enable_credit_overages": enabled })),
    ))
}

/// Best-effort merge of `enable_credit_overages` into the state file.
fn persist_overages(state: &AppState, enabled: bool) {
    let path = &state.config.server.state_file;
    let mut obj: serde_json::Map<String, Value> = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    obj.insert("enable_credit_overages".to_string(), json!(enabled));
    if let Ok(text) = serde_json::to_string_pretty(&Value::Object(obj)) {
        let _ = std::fs::write(path, text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, KeyConfig};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn temp_state_path(name: &str) -> String {
        let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push("target");
        std::fs::create_dir_all(&p).ok();
        p.push(format!("ag-quota-{}-{}.json", name, std::process::id()));
        let _ = std::fs::remove_file(&p);
        p.to_string_lossy().into_owned()
    }

    fn app_with_key(name: &str, expires_at: i64, total_credits: i64) -> (axum::Router, AppState) {
        let mut cfg = Config::default();
        cfg.server.state_file = temp_state_path(name);
        cfg.auth.keys.push(KeyConfig {
            key: "k1".to_string(),
            expires_at,
            total_credits,
            ..KeyConfig::default()
        });
        let state = crate::AppState::new(cfg);
        let app = crate::build_router(state.clone());
        (app, state)
    }

    async fn get_json(app: axum::Router) -> (StatusCode, Value) {
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/v1/quota")
                    .header("Authorization", "Bearer k1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn quota_requires_auth() {
        let (app, _) = app_with_key("auth", 0, 100);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/v1/quota")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn quota_expires_at_null_when_zero() {
        let (app, _state) = app_with_key("null", 0, 9999);
        let (status, v) = get_json(app).await;
        assert_eq!(status, StatusCode::OK);
        assert!(v["expires_at"].is_null());
        assert_eq!(v["total_credits"], 9999);
        assert_eq!(v["tier"], 3);
        assert_eq!(v["enable_credit_overages"], false);
        assert!(v["models"].is_object());
        assert!(v["resets"].is_object());
    }

    #[tokio::test]
    async fn quota_expires_at_seconds_when_positive() {
        let future = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            + 7200;
        let (app, _state) = app_with_key("secs", future, 4242);
        let (status, v) = get_json(app).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["expires_at"].as_i64(), Some(future));
        assert_eq!(v["total_credits"], 4242);
    }

    async fn post_config(app: axum::Router, body: Value) -> (StatusCode, Value) {
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/quota/config")
                    .header("Authorization", "Bearer k1")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
            .await
            .unwrap();
        (
            status,
            if bytes.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&bytes).unwrap()
            },
        )
    }

    #[tokio::test]
    async fn config_flips_flag_and_get_reflects() {
        let (app, state) = app_with_key("flip", 0, 100);
        let (status, v) =
            post_config(app.clone(), json!({ "enable_credit_overages": true })).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["enable_credit_overages"], true);
        assert!(state.credit_overages.load(Ordering::Relaxed));
        let (_, v) = get_json(app.clone()).await;
        assert_eq!(v["enable_credit_overages"], true);

        let (status, v) =
            post_config(app.clone(), json!({ "enable_credit_overages": false })).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["enable_credit_overages"], false);
        let (_, v) = get_json(app).await;
        assert_eq!(v["enable_credit_overages"], false);
    }

    #[tokio::test]
    async fn config_missing_field_is_400() {
        let (app, _state) = app_with_key("missing", 0, 100);
        let (status, v) = post_config(app, json!({ "other": 1 })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(v["error"]["message"].is_string());
    }

    #[tokio::test]
    async fn config_requires_auth() {
        let (app, _state) = app_with_key("cfgauth", 0, 100);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/quota/config")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"enable_credit_overages":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn extract_account_plan_prefers_paid_tier_and_formats_free_tier() {
        let with_pro = json!({
            "cloudaicompanionProject": "aicode-consumers",
            "currentTier": { "id": "free-tier", "name": "Antigravity" },
            "paidTier": { "id": "g1-pro-tier", "name": "Google AI Pro" }
        });
        assert_eq!(
            extract_account_plan(&with_pro),
            Some((
                "g1-pro-tier".to_string(),
                "Google AI Pro".to_string(),
                Some("aicode-consumers".to_string()),
            ))
        );
        assert_eq!(tier_id_to_numeric("g1-pro-tier"), 1);

        let free_only = json!({
            "currentTier": { "id": "free-tier", "name": "Antigravity" }
        });
        assert_eq!(
            extract_account_plan(&free_only),
            Some(("free-tier".to_string(), "Free".to_string(), None))
        );
        assert_eq!(tier_id_to_numeric("free-tier"), 0);

        let ultra = json!({
            "paidTier": { "id": "g1-ultra-tier", "name": "Google AI Ultra" }
        });
        assert_eq!(
            extract_account_plan(&ultra),
            Some((
                "g1-ultra-tier".to_string(),
                "Google AI Ultra".to_string(),
                None
            ))
        );
        assert_eq!(tier_id_to_numeric("g1-ultra-tier"), 2);
    }

    #[test]
    fn parse_quota_summary_picks_tighter_window_for_3p_and_gemini() {
        let summary = json!({
            "groups": [
                {
                    "displayName": "Gemini Models",
                    "buckets": [
                        {
                            "bucketId": "gemini-weekly",
                            "window": "weekly",
                            "resetTime": "2026-12-02T16:06:53Z",
                            "remainingFraction": 0.9488881
                        },
                        {
                            "bucketId": "gemini-5h",
                            "window": "5h",
                            "resetTime": "2026-11-25T21:06:53Z",
                            "remainingFraction": 0.79555255
                        }
                    ]
                },
                {
                    "displayName": "Claude and GPT models",
                    "buckets": [
                        {
                            "bucketId": "3p-weekly",
                            "window": "weekly",
                            "resetTime": "2026-12-02T11:36:48Z",
                            "remainingFraction": 0.49997362
                        },
                        {
                            "bucketId": "3p-5h",
                            "window": "5h",
                            "resetTime": "2026-11-25T21:39:50Z",
                            "remainingFraction": 0
                        }
                    ]
                }
            ]
        });
        let groups = parse_quota_summary(&summary);
        assert_eq!(groups.len(), 2);
        assert!(groups[0].is_gemini);
        assert_eq!(
            groups[0].effective_limit(),
            Some((80, Some("2026-11-25T21:06:53Z".to_string())))
        );
        assert!(groups[1].is_third_party);
        assert_eq!(
            groups[1].effective_limit(),
            Some((0, Some("2026-11-25T21:39:50Z".to_string())))
        );
    }
}
