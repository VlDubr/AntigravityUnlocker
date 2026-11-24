use crate::error::AppError;
use crate::AppState;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::collections::HashMap;

/// GET /v1/client-update — public update manifest.
/// Query: `platform` (macos normalizes to mac-arm).
pub async fn client_update(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Json<Value> {
    let updates = &state.config.updates;
    if updates.version.is_empty() {
        return Json(json!({ "version": "", "name": "", "body": "", "asset_id": "none" }));
    }
    let platform = match params.get("platform").map(String::as_str) {
        Some("macos") => "mac-arm",
        Some(p) => p,
        None => "",
    };
    let asset = updates.assets.get(platform);
    let asset_id = asset.map(|a| a.id.clone()).unwrap_or_else(|| "none".to_string());
    let size = asset
        .map(|a| std::fs::metadata(&a.path).map(|m| m.len()).unwrap_or(0))
        .unwrap_or(0);
    Json(json!({
        "version": updates.version,
        "name": updates.name,
        "body": updates.body,
        "asset_id": asset_id,
        "size": size,
    }))
}

/// GET /v1/client-update/download/{asset_id} — public streamed asset download.
pub async fn client_update_download(
    State(state): State<AppState>,
    Path(asset_id): Path<String>,
) -> Result<Response, AppError> {
    let asset = state
        .config
        .updates
        .assets
        .values()
        .find(|a| a.id == asset_id)
        .ok_or(AppError::NotFound)?;
    let meta = std::fs::metadata(&asset.path).map_err(|_| AppError::NotFound)?;
    let file = tokio::fs::File::open(&asset.path).await.map_err(|_| AppError::NotFound)?;
    let stream = tokio_util::io::ReaderStream::new(file);
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, header::HeaderValue::from_static("application/octet-stream"));
    headers.insert(header::CONTENT_LENGTH, header::HeaderValue::from(meta.len()));
    Ok((StatusCode::OK, headers, Body::from_stream(stream)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AssetConfig, Config};
    use axum::body::Body;
    use axum::http::Request;
    use std::collections::HashMap;
    use tower::ServiceExt;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push("target");
        std::fs::create_dir_all(&p).ok();
        p.push(format!("ag-update-{}-{}", name, std::process::id()));
        p
    }

    fn app_with(updates: crate::config::UpdatesConfig) -> axum::Router {
        let mut cfg = Config::default();
        cfg.updates = updates;
        crate::build_router(crate::AppState::new(cfg))
    }

    async fn get(app: axum::Router, uri: &str) -> axum::http::Response<Body> {
        app.oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn unconfigured_returns_empty_version() {
        let app = app_with(crate::config::UpdatesConfig::default());
        let resp = get(app, "/v1/client-update?platform=windows").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v: Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1024).await.unwrap())
                .unwrap();
        assert_eq!(v["version"], "");
        assert_eq!(v["name"], "");
        assert_eq!(v["body"], "");
        assert_eq!(v["asset_id"], "none");
    }

    #[tokio::test]
    async fn configured_returns_asset_id_and_size() {
        let file = temp_path("bin");
        std::fs::write(&file, b"12345").unwrap();
        let mut assets = HashMap::new();
        assets.insert(
            "mac-arm".to_string(),
            AssetConfig { id: "a-mac".to_string(), path: file.to_string_lossy().into_owned() },
        );
        assets.insert(
            "windows".to_string(),
            AssetConfig { id: "a-win".to_string(), path: file.to_string_lossy().into_owned() },
        );
        let updates = crate::config::UpdatesConfig {
            version: "1.2.3".to_string(),
            name: "rel".to_string(),
            body: "notes".to_string(),
            assets,
        };
        // windows platform
        let v: Value = serde_json::from_slice(
            &axum::body::to_bytes(get(app_with(updates.clone()), "/v1/client-update?platform=windows").await.into_body(), 1024).await.unwrap(),
        )
        .unwrap();
        assert_eq!(v["version"], "1.2.3");
        assert_eq!(v["name"], "rel");
        assert_eq!(v["body"], "notes");
        assert_eq!(v["asset_id"], "a-win");
        assert_eq!(v["size"], 5);
        // macos -> mac-arm normalization
        let v: Value = serde_json::from_slice(
            &axum::body::to_bytes(get(app_with(updates), "/v1/client-update?platform=macos").await.into_body(), 1024).await.unwrap(),
        )
        .unwrap();
        assert_eq!(v["asset_id"], "a-mac");
        std::fs::remove_file(&file).ok();
    }

    #[tokio::test]
    async fn download_unknown_asset_id_is_404() {
        let mut assets = HashMap::new();
        assets.insert("windows".to_string(), AssetConfig { id: "a-win".to_string(), path: "x".into() });
        let updates = crate::config::UpdatesConfig {
            version: "1.0.0".to_string(),
            name: String::new(),
            body: String::new(),
            assets,
        };
        let resp = get(app_with(updates), "/v1/client-update/download/nope").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn download_missing_file_is_404() {
        let mut assets = HashMap::new();
        assets.insert(
            "windows".to_string(),
            AssetConfig { id: "a-win".to_string(), path: "Z:\\definitely\\missing\\ag.bin".into() },
        );
        let updates = crate::config::UpdatesConfig {
            version: "1.0.0".to_string(),
            name: String::new(),
            body: String::new(),
            assets,
        };
        let resp = get(app_with(updates), "/v1/client-update/download/a-win").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn download_streams_bytes() {
        let file = temp_path("dl");
        std::fs::write(&file, b"hello-bytes").unwrap();
        let mut assets = HashMap::new();
        assets.insert(
            "windows".to_string(),
            AssetConfig { id: "a-win".to_string(), path: file.to_string_lossy().into_owned() },
        );
        let updates = crate::config::UpdatesConfig {
            version: "1.0.0".to_string(),
            name: String::new(),
            body: String::new(),
            assets,
        };
        let resp = get(app_with(updates), "/v1/client-update/download/a-win").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()["content-type"], "application/octet-stream");
        assert_eq!(resp.headers()["content-length"], "11");
        let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(&bytes[..], b"hello-bytes");
        std::fs::remove_file(&file).ok();
    }
}
