use crate::auth::AuthenticatedKey;
use crate::AppState;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;

/// GET /v1/models — OpenAI-compatible model list. Auth required.
pub async fn models(State(state): State<AppState>, _auth: AuthenticatedKey) -> impl IntoResponse {
    let data: Vec<_> = state
        .config
        .effective_advertise()
        .into_iter()
        .map(|id| json!({ "id": id, "object": "model", "owned_by": "antigravity" }))
        .collect();
    Json(json!({ "object": "list", "data": data }))
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
}
