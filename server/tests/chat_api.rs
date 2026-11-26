use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use antigravity_server::config::{AccountConfig, Config, KeyConfig};
use antigravity_server::{build_router, AppState};
use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use bytes::Bytes;
use futures_util::{stream, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;

#[derive(Default)]
struct Mock {
    projects: AtomicUsize,
    discoveries: Mutex<Vec<(String, Value)>>,
    requests: Mutex<Vec<(String, String, Value)>>,
    stream_release: tokio::sync::Notify,
}

async fn token(req: Request) -> Json<Value> {
    let body = to_bytes(req.into_body(), 4096).await.unwrap();
    let form = String::from_utf8_lossy(&body);
    let suffix = if form.contains("refresh_token=refresh-a") {
        "a"
    } else if form.contains("refresh_token=refresh-b") {
        "b"
    } else {
        "main"
    };
    Json(json!({ "access_token": format!("token-{suffix}"), "expires_in": 3600 }))
}

async fn upstream(State(mock): State<Arc<Mock>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let path = parts.uri.path().to_string();
    let authorization = parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = to_bytes(body, 1024 * 1024).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);

    if path == "/v1internal:loadCodeAssist" {
        mock.projects.fetch_add(1, Ordering::SeqCst);
        let user_agent = parts
            .headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        mock.discoveries
            .lock()
            .unwrap()
            .push((user_agent, body.clone()));
        let project = match authorization.as_str() {
            "Bearer token-a" => "project-a",
            "Bearer token-b" => "project-b",
            _ => "project-main",
        };
        if authorization == "Bearer token-main" {
            return Json(json!({ "cloudaicompanionProject": project })).into_response();
        }
        return Json(json!({ "cloudaicompanionProject": { "id": project } })).into_response();
    }

    if path == "/v1internal:fetchAvailableModels" {
        return Json(json!({
            "models": {
                "claude-opus-5-5-low": { "displayName": "Claude Opus 5.5 (Low)" },
                "claude-opus-5-5-medium": { "displayName": "Claude Opus 5.5 (Medium)" },
                "claude-opus-5-5-high": { "displayName": "Claude Opus 5.5 (High)" },
                "gemini-3-flash": { "displayName": "Gemini 3 Flash" }
            }
        }))
        .into_response();
    }

    mock.requests
        .lock()
        .unwrap()
        .push((path.clone(), authorization, body.clone()));
    if path == "/v1/chat/completions" {
        return Json(json!({ "passthrough": true })).into_response();
    }
    if body["model"] == "rate-limited" {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "error": { "message": "quota exceeded" } })),
        )
            .into_response();
    }
    if path == "/v1internal:streamGenerateContent" {
        let first = json!({
            "response": { "candidates": [{ "content": { "parts": [{ "text": "Reading " }] } }] }
        });
        let tool = json!({
            "response": { "candidates": [{ "content": { "parts": [{
                "functionCall": {
                    "id": "native-call",
                    "name": "read_file",
                    "args": { "path": "example.txt" }
                },
                "thoughtSignature": "opaque-signature"
            }] } }] }
        });
        let finish = json!({
            "response": {
                "candidates": [{ "finishReason": "STOP" }],
                "usageMetadata": {
                    "promptTokenCount": 12,
                    "cachedContentTokenCount": 9,
                    "candidatesTokenCount": 7,
                    "totalTokenCount": 19
                }
            }
        });
        let mut frames = vec![format!("data: {first}\r\n\r\n")];
        if body["model"] != "truncated" {
            frames.push(format!("data: {tool}\n\n"));
            frames.push(format!("data: {finish}\n\n"));
        }
        let frames = frames.into_iter();
        let body = stream::unfold((frames, 0, mock), |(mut frames, index, mock)| async move {
            let frame = frames.next()?;
            if index == 1 {
                mock.stream_release.notified().await;
            }
            Some((
                Ok::<_, std::io::Error>(Bytes::from(frame)),
                (frames, index + 1, mock),
            ))
        });
        return Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(body))
            .unwrap();
    }
    Json(json!({
        "response": {
            "candidates": [{
                "content": { "role": "model", "parts": [{ "text": "Hello" }] },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 5,
                "cachedContentTokenCount": 4,
                "candidatesTokenCount": 2,
                "totalTokenCount": 7
            }
        }
    }))
    .into_response()
}

async fn serve(app: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{address}")
}

async fn setup(enabled: bool, project: &str) -> (String, Arc<Mock>) {
    let mock = Arc::new(Mock::default());
    let upstream_url = serve(
        Router::new()
            .route("/token", post(token))
            .fallback(upstream)
            .with_state(mock.clone()),
    )
    .await;
    let mut config = Config::default();
    config.openai.enabled = enabled;
    config.auth.keys = vec![KeyConfig {
        key: "gateway-key".to_string(),
        ..KeyConfig::default()
    }];
    config.google.token_url = format!("{upstream_url}/token");
    config.google.client_id = "client".to_string();
    config.google.accounts = vec![AccountConfig {
        id: "main".to_string(),
        refresh_token: "refresh-main".to_string(),
        project_id: project.to_string(),
    }];
    config.upstream.base_url = upstream_url;
    let gateway = serve(build_router(AppState::new(config))).await;
    (gateway, mock)
}

fn request(client: &reqwest::Client, gateway: &str, body: Value) -> reqwest::RequestBuilder {
    client
        .post(format!("{gateway}/v1/chat/completions"))
        .bearer_auth("gateway-key")
        .json(&body)
}

#[tokio::test]
async fn adapter_is_opt_in_and_authentication_still_applies() {
    let (gateway, mock) = setup(false, "").await;
    let client = reqwest::Client::new();
    let body = json!({ "model": "example", "messages": [{ "role": "user", "content": "Hi" }] });
    let denied = client
        .post(format!("{gateway}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    let response: Value = request(&client, &gateway, body.clone())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(response["passthrough"], true);
    let captured = mock.requests.lock().unwrap();
    assert_eq!(captured[0].0, "/v1/chat/completions");
    assert_eq!(captured[0].1, "Bearer token-main");
    assert_eq!(captured[0].2, body);
}

#[tokio::test]
async fn nonstream_chat_discovers_and_caches_project() {
    let (gateway, mock) = setup(true, "").await;
    let client = reqwest::Client::new();
    let body = json!({
        "model": "gemini-3-flash",
        "messages": [
            { "role": "system", "content": "Be concise" },
            { "role": "user", "content": "Hello" }
        ]
    });
    for _ in 0..2 {
        let response = request(&client, &gateway, body.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response: Value = response.json().await.unwrap();
        assert_eq!(response["object"], "chat.completion");
        assert_eq!(response["choices"][0]["message"]["content"], "Hello");
        assert_eq!(response["choices"][0]["finish_reason"], "stop");
        assert_eq!(response["usage"]["total_tokens"], 7);
    }
    assert_eq!(mock.projects.load(Ordering::SeqCst), 1);
    let discoveries = mock.discoveries.lock().unwrap();
    assert_eq!(discoveries.len(), 1);
    assert!(
        discoveries[0]
            .0
            .starts_with("antigravity/hub/2.35.0 ")
    );
    assert_eq!(
        discoveries[0].1,
        json!({ "metadata": { "ideType": "ANTIGRAVITY" } })
    );
    drop(discoveries);
    let captured = mock.requests.lock().unwrap();
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[0].0, "/v1internal:generateContent");
    assert_eq!(captured[0].1, "Bearer token-main");
    assert_eq!(captured[0].2["project"], "project-main");
    assert_eq!(
        captured[0].2["request"]["contents"][0]["parts"][0]["text"],
        "Hello"
    );
    assert_eq!(
        captured[0].2["request"]["systemInstruction"]["parts"][0]["text"],
        "Be concise"
    );
}

#[tokio::test]
async fn wildcard_keys_keep_project_and_oauth_account_together() {
    let mock = Arc::new(Mock::default());
    let upstream_url = serve(
        Router::new()
            .route("/token", post(token))
            .fallback(upstream)
            .with_state(mock.clone()),
    )
    .await;
    let mut config = Config::default();
    config.openai.enabled = true;
    config.auth.keys = vec![KeyConfig {
        key: "gateway-key".to_string(),
        account_id: "*".to_string(),
        ..KeyConfig::default()
    }];
    config.google.token_url = format!("{upstream_url}/token");
    config.google.client_id = "client".to_string();
    config.google.accounts = ["a", "b"]
        .into_iter()
        .map(|id| AccountConfig {
            id: id.to_string(),
            refresh_token: format!("refresh-{id}"),
            project_id: String::new(),
        })
        .collect();
    config.upstream.base_url = upstream_url;
    let gateway = serve(build_router(AppState::new(config))).await;
    let client = reqwest::Client::new();
    for i in 0..4 {
        let body = json!({
            "model": "gemini-3-flash",
            "messages": [{ "role": "user", "content": format!("Hello {i}") }]
        });
        let response = request(&client, &gateway, body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    let captured = mock.requests.lock().unwrap();
    assert_eq!(captured.len(), 4);
    for (_, authorization, payload) in captured.iter() {
        let expected = match authorization.as_str() {
            "Bearer token-a" => "project-a",
            "Bearer token-b" => "project-b",
            other => panic!("unexpected account token: {other}"),
        };
        assert_eq!(payload["project"], expected);
    }
    assert_eq!(mock.projects.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn streams_tool_calls_and_preserves_signature_on_next_turn() {
    let (gateway, mock) = setup(true, "configured-project").await;
    let client = reqwest::Client::new();
    let tools = json!([{
        "type": "function",
        "function": {
            "name": "read_file",
            "description": "Read a file",
            "parameters": {
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }
        }
    }]);
    let response = request(
        &client,
        &gateway,
        json!({
            "model": "gemini-3-flash",
            "stream": true,
            "stream_options": { "include_usage": true },
            "messages": [{ "role": "user", "content": "Read the file" }],
            "tools": tools
        }),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = response.bytes_stream();
    let first = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("first event must arrive before the tool call is released")
        .unwrap()
        .unwrap();
    mock.stream_release.notify_one();
    let mut text = String::from_utf8(first.to_vec()).unwrap();
    while let Some(bytes) = stream.next().await {
        text.push_str(&String::from_utf8(bytes.unwrap().to_vec()).unwrap());
    }
    let chunks: Vec<Value> = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str(data).unwrap())
        .collect();
    assert!(text.contains("data: [DONE]"));
    let tool = chunks
        .iter()
        .find_map(|chunk| chunk["choices"][0]["delta"]["tool_calls"][0].as_object())
        .expect("tool call missing");
    let tool_id = tool["id"].as_str().unwrap().to_string();
    assert_eq!(tool["function"]["name"], "read_file");
    assert_eq!(
        chunks
            .iter()
            .find_map(|chunk| chunk["choices"][0]["finish_reason"].as_str()),
        Some("tool_calls")
    );
    assert!(chunks
        .iter()
        .any(|chunk| chunk["usage"]["total_tokens"] == 19));

    let follow_up = json!({
        "model": "gemini-3-flash",
        "messages": [
            { "role": "user", "content": "Read the file" },
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": tool_id,
                    "type": "function",
                    "function": { "name": "read_file", "arguments": "{\"path\":\"example.txt\"}" }
                }]
            },
            { "role": "tool", "tool_call_id": tool_id, "content": "file contents" }
        ],
        "tools": tools
    });
    let response = request(&client, &gateway, follow_up).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let captured = mock.requests.lock().unwrap();
    let contents = captured[1].2["request"]["contents"].as_array().unwrap();
    assert_eq!(
        contents[1]["parts"][0]["thoughtSignature"],
        "opaque-signature"
    );
    assert_eq!(contents[1]["parts"][0]["functionCall"]["id"], "native-call");
    assert_eq!(
        contents[2]["parts"][0]["functionResponse"]["name"],
        "read_file"
    );
    assert_eq!(
        contents[2]["parts"][0]["functionResponse"]["response"]["result"],
        "file contents"
    );
    assert_eq!(mock.projects.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn rejects_unsupported_input_and_never_completes_truncated_streams() {
    let (gateway, mock) = setup(true, "configured-project").await;
    let client = reqwest::Client::new();
    let bad = request(
        &client,
        &gateway,
        json!({
            "model": "example",
            "messages": [{ "role": "user", "content": [{ "type": "image_url", "image_url": { "url": "https://example.test/a.png" } }] }]
        }),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    assert!(mock.requests.lock().unwrap().is_empty());

    let limited = request(
        &client,
        &gateway,
        json!({ "model": "rate-limited", "messages": [{ "role": "user", "content": "Hi" }] }),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    let error: Value = limited.json().await.unwrap();
    assert_eq!(error["error"]["message"], "quota exceeded");

    let truncated = request(
        &client,
        &gateway,
        json!({
            "model": "truncated",
            "stream": true,
            "messages": [{ "role": "user", "content": "Hi" }]
        }),
    )
    .send()
    .await
    .unwrap()
    .text()
    .await
    .unwrap();
    assert!(truncated.contains("\"error\""));
    assert!(!truncated.contains("[DONE]"));
}

#[tokio::test]
async fn routes_reasoning_effort_to_matching_variant_model() {
    let (gateway, mock) = setup(true, "configured-project").await;
    let client = reqwest::Client::new();

    for (effort, expected_upstream) in [
        (Some("high"), "claude-opus-5-5-high"),
        (Some("low"), "claude-opus-5-5-low"),
        (None, "claude-opus-5-5-medium"),
    ] {
        let mut body = json!({
            "model": "claude-opus-5-5",
            "messages": [{ "role": "user", "content": "Hello" }]
        });
        if let Some(effort) = effort {
            body["reasoning_effort"] = json!(effort);
        }
        let response = request(&client, &gateway, body).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let parsed: Value = response.json().await.unwrap();
        assert_eq!(parsed["model"], "claude-opus-5-5");
        let captured = mock.requests.lock().unwrap();
        assert_eq!(captured.last().unwrap().2["model"], expected_upstream);
    }
}

#[tokio::test]
async fn prompt_caching_preserves_session_id_sticky_account_and_cached_tokens() {
    let mock = Arc::new(Mock::default());
    let upstream_url = serve(
        Router::new()
            .route("/token", post(token))
            .fallback(upstream)
            .with_state(mock.clone()),
    )
    .await;
    let mut config = Config::default();
    config.openai.enabled = true;
    config.auth.keys = vec![KeyConfig {
        key: "gateway-key".to_string(),
        account_id: "*".to_string(),
        ..KeyConfig::default()
    }];
    config.google.token_url = format!("{upstream_url}/token");
    config.google.client_id = "client".to_string();
    config.google.accounts = ["a", "b"]
        .into_iter()
        .map(|id| AccountConfig {
            id: id.to_string(),
            refresh_token: format!("refresh-{id}"),
            project_id: format!("project-{id}"),
        })
        .collect();
    config.upstream.base_url = upstream_url;
    let gateway = serve(build_router(AppState::new(config))).await;
    let client = reqwest::Client::new();

    // Multi-turn chat without prompt_cache_key must keep identical sessionId and sticky account.
    let turn1 = json!({
        "model": "gemini-3-flash",
        "messages": [
            { "role": "system", "content": "System rules" },
            { "role": "user", "content": "First prompt" }
        ]
    });
    let turn2 = json!({
        "model": "gemini-3-flash",
        "messages": [
            { "role": "system", "content": "System rules" },
            { "role": "user", "content": "First prompt" },
            { "role": "assistant", "content": "First answer" },
            { "role": "user", "content": "Follow-up question" }
        ]
    });
    let resp1: Value = request(&client, &gateway, turn1)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp1["usage"]["prompt_tokens_details"]["cached_tokens"], 4);

    let resp2 = request(&client, &gateway, turn2).send().await.unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);

    // Explicit prompt_cache_key + prompt_cache_retention overrides prefix and stays sticky.
    let keyed1 = json!({
        "model": "gemini-3-flash",
        "prompt_cache_key": "copilot-chat-99",
        "prompt_cache_retention": "24h",
        "messages": [{ "role": "user", "content": "Message A" }]
    });
    let keyed2 = json!({
        "model": "gemini-3-flash",
        "prompt_cache_key": "copilot-chat-99",
        "messages": [{ "role": "user", "content": "Message B" }]
    });
    assert_eq!(
        request(&client, &gateway, keyed1).send().await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(
        request(&client, &gateway, keyed2).send().await.unwrap().status(),
        StatusCode::OK
    );

    {
        let captured = mock.requests.lock().unwrap();
        assert_eq!(captured.len(), 4);
        assert_eq!(
            captured[0].2["request"]["sessionId"],
            captured[1].2["request"]["sessionId"],
            "multi-turn chat must reuse sessionId"
        );
        assert_eq!(
            captured[0].1, captured[1].1,
            "multi-turn chat must stay pinned to one wildcard account"
        );
        assert_eq!(
            captured[2].2["request"]["sessionId"],
            captured[3].2["request"]["sessionId"],
            "explicit prompt_cache_key must reuse sessionId"
        );
        assert_eq!(
            captured[2].1, captured[3].1,
            "explicit prompt_cache_key must stay pinned to one wildcard account"
        );
        assert_ne!(
            captured[0].2["request"]["sessionId"],
            captured[2].2["request"]["sessionId"]
        );
    }

    // Upstream generation 429 on a wildcard key must unpin the session so retries
    // rotate to another account instead of staying stranded.
    let rate_limited = json!({
        "model": "rate-limited",
        "prompt_cache_key": "quota-failover-chat",
        "messages": [{ "role": "user", "content": "Retry me" }]
    });
    assert_eq!(
        request(&client, &gateway, rate_limited.clone())
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        request(&client, &gateway, rate_limited)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    let captured = mock.requests.lock().unwrap();
    assert_eq!(captured.len(), 6);
    assert_ne!(
        captured[4].1, captured[5].1,
        "429 must unpin wildcard session so the next retry tries another account"
    );
}
