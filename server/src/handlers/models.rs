use crate::auth::AuthenticatedKey;
use crate::handlers::forward;
use crate::AppState;
use axum::body::{to_bytes, Body};
use axum::extract::State;
use axum::http::{header, Method, Request};
use axum::response::IntoResponse;
use axum::Json;
use serde_json::{json, Map, Value};
use std::time::{Duration, Instant};

const MODELS_CACHE_TTL: Duration = Duration::from_secs(60);

/// GET /v1/models — OpenAI-compatible model list. Auth required.
pub async fn models(State(state): State<AppState>, auth: AuthenticatedKey) -> impl IntoResponse {
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

    if let Some(data) = fetch_upstream_models(&state, &auth).await {
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

pub(crate) async fn fetch_upstream_models(
    state: &AppState,
    auth: &AuthenticatedKey,
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
            if fetched_at.elapsed() < MODELS_CACHE_TTL {
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
    if let Some(quota) = info.get("quotaInfo") {
        if let Some(fraction) = quota.get("remainingFraction").and_then(Value::as_f64) {
            obj.insert("quota_remaining_fraction".to_string(), json!(fraction));
        }
        if let Some(reset) = quota.get("resetTime").and_then(Value::as_str) {
            obj.insert("quota_reset_time".to_string(), json!(reset));
        }
    }

    Value::Object(obj)
}

#[cfg(test)]
mod tests {
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
    async fn models_fetches_available_models_from_upstream_and_filters_disabled() {
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
        assert_eq!(data.len(), 2);
        assert_eq!(data[0]["id"], "gemini-3-flash");
        assert_eq!(data[0]["display_name"], "Gemini 3 Flash");
        assert_eq!(data[0]["context_window"], 1048576);
        assert_eq!(data[0]["max_output_tokens"], 65536);
        assert_eq!(data[0]["supports_images"], true);
        assert_eq!(data[0]["supports_tools"], true);
        assert_eq!(data[1]["id"], "gpt-oss-120b-medium");

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
        assert_eq!(q["models"]["gemini-3-flash"], 42);
        assert_eq!(q["resets"]["gemini-3-flash"], "2026-11-25T14:51:51Z");
        assert!(q["models"].get("gpt-oss-120b-medium").is_none());
    }
}
