use std::collections::VecDeque;
use std::pin::Pin;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::{header, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use bytes::Bytes;
use futures_util::{stream, Stream, StreamExt};
use rand::RngCore;
use serde_json::{json, Value};

use crate::auth::AuthenticatedKey;
use crate::chat_request::convert_request;
use crate::chat_response::{convert_response, ChatMeta, StreamState};
use crate::error::AppError;
use crate::handlers::forward;
use crate::AppState;

pub async fn completions(
    State(state): State<AppState>,
    auth: AuthenticatedKey,
    req: Request,
) -> Result<Response, AppError> {
    if !state.config.openai.enabled || req.method() != Method::POST {
        return forward::forward(State(state), auth, req).await;
    }

    let body = to_bytes(req.into_body(), state.config.server.max_body_bytes)
        .await
        .map_err(|e| AppError::BadRequest(format!("failed to read request body: {e}")))?;
    let input: Value = serde_json::from_slice(&body)
        .map_err(|e| AppError::BadRequest(format!("invalid JSON request: {e}")))?;
    let converted = convert_request(&input)?;

    // Resolve a wildcard key once so project discovery and generation use the same account.
    let (_, _, account_id) = state
        .oauth
        .get_token_with_generation(&auth.0.account_id)
        .await?;
    let mut resolved_key = auth.0;
    resolved_key.account_id = account_id;
    let auth = AuthenticatedKey(resolved_key);
    let project = project_id(&state, &auth).await?;

    let id = random_id();
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| AppError::Internal(format!("system clock is before Unix epoch: {e}")))?
        .as_secs();
    let meta = ChatMeta {
        id: format!("chatcmpl-{id}"),
        model: converted.model.clone(),
        created,
    };
    let mut native_request = converted.request;
    let request_object = native_request
        .as_object_mut()
        .ok_or_else(|| AppError::Internal("invalid converted request".to_string()))?;
    request_object.insert("sessionId".to_string(), json!(format!("session-{id}")));
    let payload = json!({
        "project": project,
        "model": converted.model,
        "userAgent": "antigravity",
        "requestId": format!("agent-{id}"),
        "requestType": "agent",
        "request": native_request,
    });
    let path = if converted.stream {
        "/v1internal:streamGenerateContent?alt=sse"
    } else {
        "/v1internal:generateContent"
    };
    let call = call_native(&state, auth, path, &payload);
    let timeout_secs = state.config.upstream.stream_idle_timeout_secs;
    let upstream = if timeout_secs == 0 {
        call.await?
    } else {
        tokio::time::timeout(Duration::from_secs(timeout_secs), call)
            .await
            .map_err(|_| AppError::BadGateway("upstream request timed out".to_string()))??
    };
    if !upstream.status().is_success() {
        return Ok(upstream_error(
            upstream,
            converted.model.as_str(),
            payload.to_string().len(),
            converted.stream,
            &payload["request"],
        )
        .await);
    }

    if converted.stream {
        let content_type = upstream
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if !content_type
            .split(';')
            .next()
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("text/event-stream"))
        {
            return Err(AppError::BadGateway(
                "upstream did not return an SSE stream".to_string(),
            ));
        }
        stream_response(
            upstream,
            meta,
            converted.include_usage,
            state.config.server.max_body_bytes,
            state.config.upstream.stream_idle_timeout_secs,
        )
    } else {
        let bytes = read_body(
            upstream.into_body(),
            state.config.server.max_body_bytes,
            state.config.upstream.stream_idle_timeout_secs,
        )
        .await?;
        let native: Value = serde_json::from_slice(&bytes)
            .map_err(|e| AppError::BadGateway(format!("invalid upstream JSON: {e}")))?;
        Ok(Json(convert_response(&native, &meta)?).into_response())
    }
}

fn random_id() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

async fn call_native(
    state: &AppState,
    auth: AuthenticatedKey,
    path: &str,
    payload: &Value,
) -> Result<Response, AppError> {
    let req = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(payload.to_string()))
        .map_err(|e| AppError::Internal(format!("failed to build upstream request: {e}")))?;
    forward::forward(State(state.clone()), auth, req).await
}

async fn project_id(state: &AppState, auth: &AuthenticatedKey) -> Result<String, AppError> {
    let configured = state
        .config
        .google
        .accounts
        .iter()
        .find(|account| account.id == auth.0.account_id)
        .map(|account| account.project_id.trim())
        .filter(|id| !id.is_empty());
    if let Some(id) = configured {
        return Ok(id.to_string());
    }

    let mut cache = state.openai_projects.lock().await;
    if let Some(id) = cache.get(&auth.0.account_id) {
        return Ok(id.clone());
    }

    let payload = json!({
        "metadata": {
            "ideType": "ANTIGRAVITY"
        }
    });
    let timeout = Duration::from_secs(state.config.upstream.connect_timeout_secs.max(1).min(30));
    let response = tokio::time::timeout(
        timeout,
        call_native(
            state,
            AuthenticatedKey(auth.0.clone()),
            "/v1internal:loadCodeAssist",
            &payload,
        ),
    )
    .await
    .map_err(|_| AppError::BadGateway("Cloud Code project discovery timed out".to_string()))??;
    if !response.status().is_success() {
        return Err(AppError::BadGateway(format!(
            "Cloud Code project discovery failed with HTTP {}",
            response.status()
        )));
    }
    let bytes = read_body(response.into_body(), 1024 * 1024, timeout.as_secs()).await?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|e| AppError::BadGateway(format!("invalid project discovery response: {e}")))?;
    let project = value
        .get("cloudaicompanionProject")
        .and_then(|v| v.as_str().or_else(|| v.get("id").and_then(Value::as_str)))
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            AppError::BadGateway(
                "Cloud Code project ID unavailable; configure AG_GOOGLE_PROJECT_ID".to_string(),
            )
        })?;
    let project = project.to_string();
    cache.insert(auth.0.account_id.clone(), project.clone());
    Ok(project)
}

async fn read_body(body: Body, limit: usize, timeout_secs: u64) -> Result<Bytes, AppError> {
    let read = to_bytes(body, limit);
    let result = if timeout_secs == 0 {
        read.await
    } else {
        tokio::time::timeout(Duration::from_secs(timeout_secs), read)
            .await
            .map_err(|_| AppError::BadGateway("upstream response timed out".to_string()))?
    };
    result.map_err(|e| AppError::BadGateway(format!("failed to read upstream response: {e}")))
}

async fn upstream_error(
    response: Response,
    model: &str,
    request_bytes: usize,
    stream: bool,
    native_request: &Value,
) -> Response {
    let status = response.status();
    let bytes = read_body(response.into_body(), 64 * 1024, 15).await;
    if let Err(err) = &bytes {
        tracing::warn!(error = %err, "failed to read upstream error response");
    }
    let raw_body = bytes
        .as_ref()
        .ok()
        .map(|b| {
            let end = b.len().min(2048);
            String::from_utf8_lossy(&b[..end]).replace(['\r', '\n'], " ")
        })
        .unwrap_or_default();
    let message = bytes
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| format!("upstream returned HTTP {status}"));
    let contents_len = native_request
        .get("contents")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let has_system = native_request.get("systemInstruction").is_some();
    let has_tools = native_request.get("tools").is_some();
    let gen_cfg = native_request
        .get("generationConfig")
        .map(Value::to_string)
        .unwrap_or_else(|| "none".to_string());
    tracing::warn!(
        %status,
        %model,
        stream,
        request_bytes,
        contents_len,
        has_system,
        has_tools,
        %gen_cfg,
        %message,
        %raw_body,
        "generateContent upstream returned a non-2xx response"
    );
    let client_status = if status == StatusCode::UNAUTHORIZED {
        StatusCode::BAD_GATEWAY
    } else {
        status
    };
    (
        client_status,
        Json(json!({ "error": { "message": message, "type": "upstream_error" } })),
    )
        .into_response()
}

type UpstreamStream = Pin<Box<dyn Stream<Item = Result<Bytes, axum::Error>> + Send>>;

struct EventReader {
    input: UpstreamStream,
    buffer: Vec<u8>,
    data: Vec<u8>,
    has_data: bool,
    max_bytes: usize,
    idle_timeout_secs: u64,
    eof: bool,
}

impl EventReader {
    fn new(body: Body, max_bytes: usize, idle_timeout_secs: u64) -> Self {
        Self {
            input: Box::pin(body.into_data_stream()),
            buffer: Vec::new(),
            data: Vec::new(),
            has_data: false,
            max_bytes,
            idle_timeout_secs,
            eof: false,
        }
    }

    async fn next_event(&mut self) -> Result<Option<Vec<u8>>, String> {
        loop {
            if let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
                let mut line: Vec<u8> = self.buffer.drain(..=end).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                if line.is_empty() {
                    if self.has_data {
                        self.has_data = false;
                        return Ok(Some(std::mem::take(&mut self.data)));
                    }
                } else if let Some(value) = line.strip_prefix(b"data:") {
                    let value = value.strip_prefix(b" ").unwrap_or(value);
                    let separator_len = if self.has_data { 1 } else { 0 };
                    if self
                        .data
                        .len()
                        .saturating_add(separator_len)
                        .saturating_add(value.len())
                        > self.max_bytes
                    {
                        return Err("upstream SSE event is too large".to_string());
                    }
                    if self.has_data {
                        self.data.push(b'\n');
                    }
                    self.has_data = true;
                    self.data.extend_from_slice(value);
                }
                continue;
            }

            if self.buffer.len() > self.max_bytes.saturating_add(b"data: \r".len()) {
                return Err("upstream SSE line is too large".to_string());
            }
            if self.eof {
                if !self.buffer.is_empty() {
                    self.buffer.push(b'\n');
                    continue;
                }
                return if !self.has_data {
                    Ok(None)
                } else {
                    self.has_data = false;
                    Ok(Some(std::mem::take(&mut self.data)))
                };
            }

            let next = if self.idle_timeout_secs == 0 {
                self.input.next().await
            } else {
                tokio::time::timeout(
                    Duration::from_secs(self.idle_timeout_secs),
                    self.input.next(),
                )
                .await
                .map_err(|_| "upstream SSE stream timed out".to_string())?
            };
            match next {
                Some(Ok(bytes)) => self.buffer.extend_from_slice(&bytes),
                Some(Err(e)) => return Err(format!("upstream SSE stream failed: {e}")),
                None => self.eof = true,
            }
        }
    }
}

struct StreamAdapter {
    reader: EventReader,
    converter: StreamState,
    pending: VecDeque<Bytes>,
    finished: bool,
}

impl StreamAdapter {
    fn queue_events(&mut self, events: Vec<Value>) {
        self.pending.extend(
            events
                .into_iter()
                .map(|value| Bytes::from(format!("data: {value}\n\n"))),
        );
    }

    fn fail(&mut self, message: String) {
        tracing::warn!(%message, "OpenAI adapter stream failed");
        let error = json!({ "error": { "message": message, "type": "upstream_error" } });
        self.pending
            .push_back(Bytes::from(format!("data: {error}\n\n")));
        self.finished = true;
    }

    fn finish(&mut self, saw_done: bool) {
        match self.converter.finish(saw_done) {
            Ok(events) => {
                self.queue_events(events);
                self.pending
                    .push_back(Bytes::from_static(b"data: [DONE]\n\n"));
                self.finished = true;
            }
            Err(err) => self.fail(err.message()),
        }
    }
}

fn stream_response(
    response: Response,
    meta: ChatMeta,
    include_usage: bool,
    max_bytes: usize,
    idle_timeout_secs: u64,
) -> Result<Response, AppError> {
    let adapter = StreamAdapter {
        reader: EventReader::new(response.into_body(), max_bytes, idle_timeout_secs),
        converter: StreamState::new(meta, include_usage),
        pending: VecDeque::new(),
        finished: false,
    };
    let output = stream::unfold(adapter, |mut adapter| async move {
        loop {
            if let Some(bytes) = adapter.pending.pop_front() {
                return Some((Ok::<_, std::io::Error>(bytes), adapter));
            }
            if adapter.finished {
                return None;
            }
            match adapter.reader.next_event().await {
                Ok(Some(data)) if data == b"[DONE]" => adapter.finish(true),
                Ok(Some(data)) => match serde_json::from_slice::<Value>(&data) {
                    Ok(value) if value.get("error").is_some() => {
                        adapter.fail("upstream returned a stream error".to_string());
                    }
                    Ok(value) => match adapter.converter.push(&value) {
                        Ok(events) => adapter.queue_events(events),
                        Err(err) => adapter.fail(err.message()),
                    },
                    Err(err) => adapter.fail(format!("invalid upstream SSE data: {err}")),
                },
                Ok(None) => adapter.finish(false),
                Err(err) => adapter.fail(err),
            }
        }
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(output))
        .map_err(|e| AppError::Internal(format!("failed to build streaming response: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn event_reader_handles_split_crlf_and_multiple_data_lines() {
        let chunks = stream::iter([
            Ok::<_, std::io::Error>(Bytes::from_static(b": keepalive\r\ndata: {\"te")),
            Ok(Bytes::from_static(b"xt\":\r\ndata: \"ok\"}\r\n\r")),
            Ok(Bytes::from_static(b"\ndata: [DONE]\n\n")),
        ]);
        let mut reader = EventReader::new(Body::from_stream(chunks), 1024, 1);
        assert_eq!(
            reader.next_event().await.unwrap(),
            Some(b"{\"text\":\n\"ok\"}".to_vec())
        );
        assert_eq!(reader.next_event().await.unwrap(), Some(b"[DONE]".to_vec()));
        assert_eq!(reader.next_event().await.unwrap(), None);
    }

    #[tokio::test]
    async fn event_reader_enforces_data_size_without_counting_prefix() {
        let chunks = stream::iter([
            Ok::<_, std::io::Error>(Bytes::from_static(b"data: 12345")),
            Ok(Bytes::from_static(b"\n\ndata: 123456\n\n")),
        ]);
        let mut reader = EventReader::new(Body::from_stream(chunks), 5, 1);
        assert_eq!(reader.next_event().await.unwrap(), Some(b"12345".to_vec()));
        assert_eq!(
            reader.next_event().await.unwrap_err(),
            "upstream SSE event is too large"
        );
    }
}
