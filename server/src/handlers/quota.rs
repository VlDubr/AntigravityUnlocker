use crate::auth::AuthenticatedKey;
use crate::error::AppError;
use crate::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::{json, Value};
use std::sync::atomic::Ordering;

/// GET /v1/quota — quota snapshot for the authenticated key.
/// `expires_at` is null or Unix SECONDS (the client multiplies by 1000).
pub async fn quota(
    State(state): State<AppState>,
    AuthenticatedKey(key): AuthenticatedKey,
) -> impl IntoResponse {
    let expires_at = if key.expires_at > 0 {
        Value::Number(key.expires_at.into())
    } else {
        Value::Null
    };
    Json(json!({
        "total_credits": key.total_credits,
        "expires_at": expires_at,
        "enable_credit_overages": state.credit_overages.load(Ordering::Relaxed),
        "tier": key.tier,
        "models": {},
        "resets": {},
    }))
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
        let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn quota_requires_auth() {
        let (app, _) = app_with_key("auth", 0, 100);
        let resp = app
            .oneshot(Request::builder().uri("/v1/quota").body(Body::empty()).unwrap())
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
        let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000).await.unwrap();
        (
            status,
            if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() },
        )
    }

    #[tokio::test]
    async fn config_flips_flag_and_get_reflects() {
        let (app, state) = app_with_key("flip", 0, 100);
        let (status, v) = post_config(app.clone(), json!({ "enable_credit_overages": true })).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["enable_credit_overages"], true);
        assert!(state.credit_overages.load(Ordering::Relaxed));
        let (_, v) = get_json(app.clone()).await;
        assert_eq!(v["enable_credit_overages"], true);

        let (status, v) = post_config(app.clone(), json!({ "enable_credit_overages": false })).await;
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
}
