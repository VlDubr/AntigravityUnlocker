use crate::auth::AuthenticatedKey;
use crate::handlers::forward;
use crate::AppState;
use axum::body::{to_bytes, Body};
use axum::extract::{Query, State};
use axum::http::{header, Method, Request};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

const MODELS_CACHE_TTL: Duration = Duration::from_secs(15);

/// Canonical ordering of reasoning effort tiers from lowest to highest.
const EFFORT_ORDER: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "max"];

/// Suffix check order: longer suffixes (`-xhigh`) must be checked before shorter
/// suffixes (`-high`) to avoid mis-splitting.
const EFFORT_SUFFIXES: &[(&str, &str)] = &[
    ("-minimal", "minimal"),
    ("-xhigh", "xhigh"),
    ("-medium", "medium"),
    ("-high", "high"),
    ("-low", "low"),
    ("-max", "max"),
];

#[derive(Debug, Default, Deserialize)]
pub struct RefreshQuery {
    #[serde(default)]
    pub refresh: Option<String>,
}

impl RefreshQuery {
    pub fn force_refresh(&self) -> bool {
        self.refresh
            .as_deref()
            .map(str::trim)
            .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
    }
}

/// Split a trailing reasoning-effort suffix (`-low`, `-medium`, `-high`,
/// `-xhigh`, `-minimal`, `-max`) from a model id.
pub(crate) fn split_effort_suffix(id: &str) -> Option<(&str, &'static str)> {
    for &(suffix, effort) in EFFORT_SUFFIXES {
        if let Some(base) = id.strip_suffix(suffix) {
            if !base.is_empty() {
                return Some((base, effort));
            }
        }
    }
    None
}

fn strip_effort_from_display_name(name: &str) -> String {
    let trimmed = name.trim();
    for tag in [
        "(XHigh)",
        "(Minimal)",
        "(Medium)",
        "(High)",
        "(Low)",
        "(Max)",
        "XHigh",
        "Minimal",
        "Medium",
        "High",
        "Low",
        "Max",
    ] {
        if trimmed.len() > tag.len() {
            let tail = &trimmed[trimmed.len() - tag.len()..];
            if tail.eq_ignore_ascii_case(tag) {
                let prefix = trimmed[..trimmed.len() - tag.len()].trim_end();
                if !prefix.is_empty() {
                    return prefix.to_string();
                }
            }
        }
    }
    trimmed.to_string()
}

/// GET /v1/models — OpenAI-compatible model list. Auth required.
pub async fn models(
    State(state): State<AppState>,
    auth: AuthenticatedKey,
    Query(query): Query<RefreshQuery>,
) -> impl IntoResponse {
    if !state.config.models.advertise.is_empty() {
        let data: Vec<_> = state
            .config
            .models
            .advertise
            .iter()
            .map(|id| json!({ "id": id, "object": "model", "owned_by": "antigravity" }))
            .collect();
        return Json(json!({ "object": "list", "data": data }));
    }

    if let Some(raw) = fetch_raw_upstream_models(&state, &auth, query.force_refresh()).await {
        let data = consolidate_models(&raw);
        return Json(json!({ "object": "list", "data": data }));
    }

    let data: Vec<_> = state
        .config
        .effective_advertise()
        .into_iter()
        .map(|id| json!({ "id": id, "object": "model", "owned_by": "antigravity" }))
        .collect();
    Json(json!({ "object": "list", "data": data }))
}

/// Fetch raw upstream models from `/v1internal:fetchAvailableModels` with a
/// short per-account cache. Pass `force_refresh = true` to bypass a fresh cache.
pub(crate) async fn fetch_raw_upstream_models(
    state: &AppState,
    auth: &AuthenticatedKey,
    force_refresh: bool,
) -> Option<Vec<Value>> {
    if state.config.google.accounts.is_empty() {
        return None;
    }

    let account_id = match state
        .oauth
        .get_token_with_generation(&auth.0.account_id)
        .await
    {
        Ok((_, _, id)) => id,
        Err(e) => {
            tracing::warn!(error = %e, "failed to resolve OAuth account for /v1/models");
            return None;
        }
    };

    let stale = {
        let cache = state.models_cache.lock().await;
        if let Some((fetched_at, cached)) = cache.get(&account_id) {
            if !force_refresh && fetched_at.elapsed() < MODELS_CACHE_TTL {
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
        .uri("/v1internal:fetchAvailableModels")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
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
        tracing::warn!(status = %resp.status(), "fetchAvailableModels returned non-2xx");
        return stale;
    }

    let bytes = tokio::time::timeout(timeout, to_bytes(resp.into_body(), 2 * 1024 * 1024))
        .await
        .ok()?
        .ok()?;
    let json_val: Value = serde_json::from_slice(&bytes).ok()?;
    let models_map = json_val.get("models")?.as_object()?;

    let mut entries: Vec<(String, Value)> = models_map
        .iter()
        .filter(|(_, info)| !info.get("disabled").and_then(Value::as_bool).unwrap_or(false))
        .map(|(id, info)| (id.clone(), build_model_entry(id, info)))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let list: Vec<Value> = entries.into_iter().map(|(_, v)| v).collect();
    if list.is_empty() {
        return stale;
    }

    let mut cache = state.models_cache.lock().await;
    cache.insert(account_id, (Instant::now(), list.clone()));
    Some(list)
}

fn build_model_entry(id: &str, info: &Value) -> Value {
    let mut obj = Map::new();
    obj.insert("id".to_string(), json!(id));
    obj.insert("object".to_string(), json!("model"));
    obj.insert("owned_by".to_string(), json!("antigravity"));

    if let Some(name) = info
        .get("displayName")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        obj.insert("display_name".to_string(), json!(name));
    }
    if let Some(ctx) = info
        .get("maxTokens")
        .or_else(|| info.get("inputTokenLimit"))
        .and_then(Value::as_u64)
        .filter(|&n| n > 0)
    {
        obj.insert("context_window".to_string(), json!(ctx));
    }
    if let Some(out) = info
        .get("maxOutputTokens")
        .or_else(|| info.get("outputTokenLimit"))
        .and_then(Value::as_u64)
        .filter(|&n| n > 0)
    {
        obj.insert("max_output_tokens".to_string(), json!(out));
    }
    if let Some(img) = info.get("supportsImages").and_then(Value::as_bool) {
        obj.insert("supports_images".to_string(), json!(img));
    }
    if let Some(tools) = info
        .get("supportsTools")
        .or_else(|| info.get("supportsFunctionCalling"))
        .and_then(Value::as_bool)
    {
        obj.insert("supports_tools".to_string(), json!(tools));
    }
    if let Some(quota) = info.get("quotaInfo").and_then(Value::as_object) {
        // Proto3 omits default zero-valued float fields (`remainingFraction = 0.0`).
        // When quotaInfo contains `resetTime` without `remainingFraction`, the quota
        // for this model is exhausted (0%).
        let fraction = quota
            .get("remainingFraction")
            .and_then(Value::as_f64)
            .or_else(|| quota.contains_key("resetTime").then_some(0.0));
        if let Some(fraction) = fraction {
            obj.insert("quota_remaining_fraction".to_string(), json!(fraction));
        }
        if let Some(reset) = quota.get("resetTime").and_then(Value::as_str) {
            obj.insert("quota_reset_time".to_string(), json!(reset));
        }
    }

    Value::Object(obj)
}

/// Consolidate reasoning-effort variants (e.g. `claude-opus-5-5-low`,
/// `claude-opus-5-5-medium`, `claude-opus-5-5-high`) into a single model entry
/// (`claude-opus-5-5`) with `supports_reasoning_effort` and `reasoning_variants`.
/// Single-variant models (such as `gpt-oss-120b-medium` or `gemini-3-flash`)
/// keep their original ID unchanged.
pub(crate) fn consolidate_models(raw: &[Value]) -> Vec<Value> {
    let mut groups: BTreeMap<String, Vec<(Option<&'static str>, &Value)>> = BTreeMap::new();
    for entry in raw {
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        if let Some((base, effort)) = split_effort_suffix(id) {
            groups
                .entry(base.to_string())
                .or_default()
                .push((Some(effort), entry));
        } else {
            groups
                .entry(id.to_string())
                .or_default()
                .push((None, entry));
        }
    }

    let mut result = Vec::with_capacity(groups.len());
    for (base_id, items) in groups {
        let has_effort_variants = items.iter().any(|(effort, _)| effort.is_some());
        if items.len() < 2 || !has_effort_variants {
            for (_, entry) in items {
                result.push(entry.clone());
            }
            continue;
        }

        let mut effort_map: HashMap<&'static str, String> = HashMap::new();
        for (effort, entry) in &items {
            if let (Some(effort), Some(id)) = (effort, entry.get("id").and_then(Value::as_str)) {
                effort_map.insert(*effort, id.to_string());
            }
        }
        let ordered_efforts: Vec<&str> = EFFORT_ORDER
            .iter()
            .copied()
            .filter(|e| effort_map.contains_key(*e))
            .collect();

        let mut obj = Map::new();
        obj.insert("id".to_string(), json!(base_id));
        obj.insert("object".to_string(), json!("model"));
        obj.insert("owned_by".to_string(), json!("antigravity"));

        if let Some(raw_name) = items
            .iter()
            .find_map(|(_, e)| e.get("display_name").and_then(Value::as_str))
        {
            let mut clean = strip_effort_from_display_name(raw_name);
            if base_id.ends_with("-tiered") && !clean.to_ascii_lowercase().contains("tiered") {
                clean.push_str(" (Tiered)");
            }
            obj.insert("display_name".to_string(), json!(clean));
        }
        if let Some(ctx) = items
            .iter()
            .filter_map(|(_, e)| e.get("context_window").and_then(Value::as_u64))
            .max()
        {
            obj.insert("context_window".to_string(), json!(ctx));
        }
        if let Some(out) = items
            .iter()
            .filter_map(|(_, e)| e.get("max_output_tokens").and_then(Value::as_u64))
            .max()
        {
            obj.insert("max_output_tokens".to_string(), json!(out));
        }
        let img_flags: Vec<bool> = items
            .iter()
            .filter_map(|(_, e)| e.get("supports_images").and_then(Value::as_bool))
            .collect();
        if !img_flags.is_empty() {
            obj.insert(
                "supports_images".to_string(),
                json!(img_flags.into_iter().any(|b| b)),
            );
        }
        let tool_flags: Vec<bool> = items
            .iter()
            .filter_map(|(_, e)| e.get("supports_tools").and_then(Value::as_bool))
            .collect();
        if !tool_flags.is_empty() {
            obj.insert(
                "supports_tools".to_string(),
                json!(tool_flags.into_iter().any(|b| b)),
            );
        }

        obj.insert("thinking".to_string(), json!(true));
        obj.insert("supports_reasoning_effort".to_string(), json!(ordered_efforts));
        let mut variants_obj = Map::new();
        for effort in EFFORT_ORDER {
            if let Some(vid) = effort_map.get(effort) {
                variants_obj.insert((*effort).to_string(), json!(vid));
            }
        }
        obj.insert("reasoning_variants".to_string(), Value::Object(variants_obj));

        // Aggregate quota conservatively: take the minimum remaining fraction
        // across all variants in the group so exhausted limits are never hidden.
        let mut min_fraction: Option<f64> = None;
        let mut min_reset: Option<String> = None;
        for (_, entry) in &items {
            let frac = entry
                .get("quota_remaining_fraction")
                .and_then(Value::as_f64);
            let reset = entry
                .get("quota_reset_time")
                .and_then(Value::as_str)
                .map(str::to_string);
            if let Some(f) = frac {
                if min_fraction.is_none_or(|cur| f < cur) {
                    min_fraction = Some(f);
                    min_reset = reset.or(min_reset);
                }
            } else if min_reset.is_none() {
                min_reset = reset;
            }
        }
        if let Some(f) = min_fraction {
            obj.insert("quota_remaining_fraction".to_string(), json!(f));
        }
        if let Some(r) = min_reset {
            obj.insert("quota_reset_time".to_string(), json!(r));
        }

        result.push(Value::Object(obj));
    }

    result.sort_by(|a, b| {
        let ka = a.get("id").and_then(Value::as_str).unwrap_or_default();
        let kb = b.get("id").and_then(Value::as_str).unwrap_or_default();
        ka.cmp(kb)
    });
    result
}

fn effort_fallback_order(effort: &str) -> &'static [&'static str] {
    match effort {
        "none" => &["minimal", "low", "medium", "high", "xhigh", "max"],
        "minimal" => &["minimal", "low", "medium", "high", "xhigh", "max"],
        "low" => &["low", "minimal", "medium", "high", "xhigh", "max"],
        "medium" => &["medium", "high", "low", "xhigh", "minimal", "max"],
        "high" => &["high", "xhigh", "medium", "max", "low", "minimal"],
        "xhigh" => &["xhigh", "high", "max", "medium", "low", "minimal"],
        "max" => &["max", "xhigh", "high", "medium", "low", "minimal"],
        _ => &["medium", "high", "low", "xhigh", "minimal", "max"],
    }
}

/// Resolve an OpenAI `(model, reasoning_effort)` pair to a concrete upstream
/// model ID using the raw upstream model catalog.
pub(crate) fn resolve_upstream_model(
    requested_model: &str,
    reasoning_effort: Option<&str>,
    raw_models: &[Value],
) -> String {
    let available: HashSet<&str> = raw_models
        .iter()
        .filter_map(|m| m.get("id").and_then(Value::as_str))
        .collect();

    if available.is_empty() {
        return requested_model.to_string();
    }

    let base_id = split_effort_suffix(requested_model)
        .map(|(base, _)| base)
        .unwrap_or(requested_model);

    if let Some(effort) = reasoning_effort {
        if effort == "none" && available.contains(base_id) {
            return base_id.to_string();
        }
        for candidate_effort in effort_fallback_order(effort) {
            let candidate = format!("{base_id}-{candidate_effort}");
            if available.contains(candidate.as_str()) {
                return candidate;
            }
        }
        if effort != "none" {
            let thinking = format!("{base_id}-thinking");
            if available.contains(thinking.as_str()) {
                return thinking;
            }
        }
        if available.contains(requested_model) {
            return requested_model.to_string();
        }
        if available.contains(base_id) {
            return base_id.to_string();
        }
        return requested_model.to_string();
    }

    // No explicit reasoning_effort: preserve exact upstream IDs when valid.
    if available.contains(requested_model) {
        return requested_model.to_string();
    }

    // Consolidated base ID without explicit effort -> pick sensible default tier.
    for candidate_effort in ["medium", "high", "low", "minimal", "xhigh", "max"] {
        let candidate = format!("{base_id}-{candidate_effort}");
        if available.contains(candidate.as_str()) {
            return candidate;
        }
    }

    requested_model.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn models_requires_auth() {
        let app = crate::build_router(crate::AppState::new(Config::default()));
        let resp = app
            .oneshot(Request::builder().uri("/v1/models").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn models_returns_advertised_list() {
        let mut cfg = Config::default();
        cfg.auth.keys.push(crate::config::KeyConfig {
            key: "k1".to_string(),
            ..Default::default()
        });
        let expected = cfg.effective_advertise();
        let app = crate::build_router(crate::AppState::new(cfg));
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
        let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["object"], "list");
        let data = v["data"].as_array().unwrap();
        assert_eq!(data.len(), expected.len());
        assert_eq!(data[0]["id"], expected[0]);
        assert_eq!(data[0]["object"], "model");
        assert_eq!(data[0]["owned_by"], "antigravity");
    }

    #[tokio::test]
    async fn models_consolidates_reasoning_variants_and_handles_zero_quota() {
        use axum::routing::post;
        use axum::{Json, Router};

        let mock = Router::new()
            .route(
                "/token",
                post(|| async {
                    Json(serde_json::json!({
                        "access_token": "tok",
                        "expires_in": 3600
                    }))
                }),
            )
            .route(
                "/v1internal:fetchAvailableModels",
                post(|| async {
                    Json(serde_json::json!({
                        "models": {
                            "gemini-4-neon-disabled": {
                                "disabled": true,
                                "displayName": "Disabled"
                            },
                            "claude-opus-5-5-high": {
                                "displayName": "Claude Opus 5.5 (High)",
                                "maxTokens": 1000000,
                                "maxOutputTokens": 128000,
                                "supportsImages": true,
                                "supportsTools": true,
                                "quotaInfo": {
                                    "resetTime": "2026-11-25T21:08:50Z"
                                }
                            },
                            "claude-opus-5-5-low": {
                                "displayName": "Claude Opus 5.5 (Low)",
                                "maxTokens": 1000000,
                                "maxOutputTokens": 128000,
                                "supportsImages": true,
                                "supportsTools": true,
                                "quotaInfo": {
                                    "remainingFraction": 0.5,
                                    "resetTime": "2026-11-25T21:08:50Z"
                                }
                            },
                            "claude-opus-5-5-medium": {
                                "displayName": "Claude Opus 5.5 (Medium)",
                                "maxTokens": 1000000,
                                "maxOutputTokens": 128000,
                                "supportsImages": true,
                                "supportsTools": true,
                                "quotaInfo": {
                                    "remainingFraction": 0.5,
                                    "resetTime": "2026-11-25T21:08:50Z"
                                }
                            },
                            "gemini-3-flash": {
                                "displayName": "Gemini 3 Flash",
                                "maxTokens": 1048576,
                                "maxOutputTokens": 65536,
                                "supportsImages": true,
                                "supportsFunctionCalling": true,
                                "quotaInfo": {
                                    "remainingFraction": 0.42,
                                    "resetTime": "2026-11-25T14:51:51Z"
                                }
                            },
                            "gpt-oss-120b-medium": {
                                "recommended": true
                            }
                        }
                    }))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, mock).await.unwrap();
        });

        let mut cfg = Config::default();
        cfg.upstream.base_url = format!("http://{addr}");
        cfg.google.token_url = format!("http://{addr}/token");
        cfg.google.client_id = "client".to_string();
        cfg.auth.keys.push(crate::config::KeyConfig {
            key: "k1".to_string(),
            account_id: "*".to_string(),
            ..Default::default()
        });
        cfg.google.accounts.push(crate::config::AccountConfig {
            id: "a1".to_string(),
            refresh_token: "refresh-a1".to_string(),
            project_id: String::new(),
        });

        let app = crate::build_router(crate::AppState::new(cfg));
        let resp = app
            .clone()
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
        let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let data = v["data"].as_array().unwrap();
        assert_eq!(data.len(), 3);
        assert_eq!(data[0]["id"], "claude-opus-5-5");
        assert_eq!(data[0]["display_name"], "Claude Opus 5.5");
        assert_eq!(data[0]["thinking"], true);
        assert_eq!(
            data[0]["supports_reasoning_effort"],
            json!(["low", "medium", "high"])
        );
        assert_eq!(data[0]["reasoning_variants"]["high"], "claude-opus-5-5-high");
        assert_eq!(data[0]["quota_remaining_fraction"], 0.0);

        assert_eq!(data[1]["id"], "gemini-3-flash");
        assert_eq!(data[1]["display_name"], "Gemini 3 Flash");
        assert_eq!(data[1]["context_window"], 1048576);
        assert_eq!(data[1]["max_output_tokens"], 65536);
        assert_eq!(data[1]["supports_images"], true);
        assert_eq!(data[1]["supports_tools"], true);
        assert_eq!(data[2]["id"], "gpt-oss-120b-medium");

        let quota = app
            .oneshot(
                Request::builder()
                    .uri("/v1/quota")
                    .header("Authorization", "Bearer k1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(quota.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(quota.into_body(), 1_000_000).await.unwrap();
        let q: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        // Exhausted variant (`remainingFraction` omitted by proto3) -> 0%, propagated to consolidated base
        assert_eq!(q["models"]["claude-opus-5-5-high"], 0);
        assert_eq!(q["models"]["claude-opus-5-5"], 0);
        assert_eq!(q["models"]["claude-opus-5-5-low"], 50);
        assert_eq!(q["resets"]["claude-opus-5-5"], "2026-11-25T21:08:50Z");
        assert_eq!(q["models"]["gemini-3-flash"], 42);
        assert_eq!(q["resets"]["gemini-3-flash"], "2026-11-25T14:51:51Z");
        assert!(q["models"].get("gpt-oss-120b-medium").is_none());
    }

    #[test]
    fn resolve_upstream_model_routes_by_effort_and_preserves_direct_ids() {
        let raw = vec![
            json!({ "id": "claude-opus-5-5-low" }),
            json!({ "id": "claude-opus-5-5-medium" }),
            json!({ "id": "claude-opus-5-5-high" }),
            json!({ "id": "gemini-4-hydrogen-low" }),
            json!({ "id": "gemini-4-hydrogen-medium" }),
            json!({ "id": "gemini-4-hydrogen-high" }),
            json!({ "id": "gemini-4-hydrogen-xhigh" }),
            json!({ "id": "gemini-3-flash" }),
        ];

        assert_eq!(
            resolve_upstream_model("claude-opus-5-5", Some("high"), &raw),
            "claude-opus-5-5-high"
        );
        assert_eq!(
            resolve_upstream_model("claude-opus-5-5", Some("low"), &raw),
            "claude-opus-5-5-low"
        );
        assert_eq!(
            resolve_upstream_model("claude-opus-5-5", None, &raw),
            "claude-opus-5-5-medium"
        );
        assert_eq!(
            resolve_upstream_model("claude-opus-5-5", Some("xhigh"), &raw),
            "claude-opus-5-5-high"
        );
        assert_eq!(
            resolve_upstream_model("claude-opus-5-5-low", None, &raw),
            "claude-opus-5-5-low"
        );
        assert_eq!(
            resolve_upstream_model("claude-opus-5-5-low", Some("high"), &raw),
            "claude-opus-5-5-high"
        );
        assert_eq!(
            resolve_upstream_model("gemini-4-hydrogen", Some("xhigh"), &raw),
            "gemini-4-hydrogen-xhigh"
        );
        assert_eq!(
            resolve_upstream_model("gemini-3-flash", Some("high"), &raw),
            "gemini-3-flash"
        );
    }
}
