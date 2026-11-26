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
use crate::chat_request::{convert_request, ensure_gemini_thought_signatures};
use crate::chat_response::{convert_response, ChatMeta, StreamState};
use crate::error::AppError;
use crate::handlers::forward;
use crate::AppState;

const SSE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

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
    let converted = convert_request(&input).map_err(|err| {
        tracing::warn!(error = %err, "OpenAI /v1/chat/completions request rejected");
        err
    })?;

    let session_id = derive_session_id(
        &auth.0.key,
        converted.prompt_cache_key.as_deref(),
        converted.first_user_message.as_deref(),
        &converted.request,
    );

    // Resolve a wildcard key once with session affinity so multi-turn prompt
    // caching, project discovery and generation all stay on the same account.
    let was_wildcard = auth.0.account_id == "*";
    let (_, _, account_id) = state
        .oauth
        .get_token_for_session(&auth.0.account_id, Some(&session_id))
        .await?;
    let mut resolved_key = auth.0;
    resolved_key.account_id = account_id;
    let auth = AuthenticatedKey(resolved_key);
    let project = project_id(&state, &auth).await?;
    let raw_models = crate::handlers::models::fetch_raw_upstream_models(&state, &auth, false)
        .await
        .unwrap_or_default();
    let upstream_model = crate::handlers::models::resolve_upstream_model(
        &converted.model,
        converted.reasoning_effort.as_deref(),
        &raw_models,
    );
    tracing::info!(
        requested_model = %converted.model,
        %upstream_model,
        %session_id,
        reasoning_effort = ?converted.reasoning_effort,
        stream = converted.stream,
        "routing /v1/chat/completions"
    );

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
    if upstream_model.to_ascii_lowercase().starts_with("gemini") {
        ensure_gemini_thought_signatures(&mut native_request);
    }
    let request_object = native_request
        .as_object_mut()
        .ok_or_else(|| AppError::Internal("invalid converted request".to_string()))?;
    request_object.insert("sessionId".to_string(), json!(session_id));
    let payload = json!({
        "project": project,
        "model": upstream_model,
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
    let resolved_account_id = auth.0.account_id.clone();
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
        if was_wildcard && upstream.status() == StatusCode::TOO_MANY_REQUESTS {
            state
                .oauth
                .unpin_session(&session_id, &resolved_account_id)
                .await;
        }
        return Ok(upstream_error(
            upstream,
            upstream_model.as_str(),
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
            upstream_model,
            session_id,
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
        let converted_resp = convert_response(&native, &meta)?;
        log_chat_usage(
            &upstream_model,
            &session_id,
            false,
            converted_resp.get("usage"),
        );
        Ok(Json(converted_resp).into_response())
    }
}

fn derive_session_id(
    client_key: &str,
    prompt_cache_key: Option<&str>,
    first_user_message: Option<&str>,
    native_request: &Value,
) -> String {
    let mut input = Vec::new();
    let mut push_part = |bytes: &[u8]| {
        input.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        input.extend_from_slice(bytes);
    };
    push_part(b"ag-session-v1");
    push_part(client_key.as_bytes());
    if let Some(cache_key) = prompt_cache_key {
        push_part(b"key");
        push_part(cache_key.as_bytes());
    } else {
        push_part(b"prefix");
        let system = native_request
            .get("systemInstruction")
            .map(Value::to_string)
            .unwrap_or_default();
        let fallback_turn;
        let first_turn = match first_user_message {
            Some(msg) => msg,
            None => {
                fallback_turn = native_request
                    .get("contents")
                    .and_then(Value::as_array)
                    .and_then(|contents| contents.first())
                    .map(Value::to_string)
                    .unwrap_or_default();
                &fallback_turn
            }
        };
        push_part(system.as_bytes());
        push_part(first_turn.as_bytes());
    }
    let digest = sha256(&input);
    let mut hex = String::with_capacity(32);
    for byte in &digest[..16] {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    format!("session-{hex}")
}

fn log_chat_usage(model: &str, session_id: &str, stream: bool, usage: Option<&Value>) {
    let Some(usage) = usage else {
        return;
    };
    let prompt_tokens = usage
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached_tokens = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let completion_tokens = usage
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_hit_pct = cached_tokens
        .saturating_mul(100)
        .checked_div(prompt_tokens)
        .unwrap_or(0);
    tracing::info!(
        %model,
        %session_id,
        stream,
        prompt_tokens,
        cached_tokens,
        completion_tokens,
        cache_hit_pct,
        "chat completion usage"
    );
}

pub(crate) fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity((data.len() + 9 + 63) & !63);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (i, word) in chunk.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
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

#[derive(Debug, PartialEq, Eq)]
enum ReadStep {
    Event(Vec<u8>),
    KeepAlive,
    Eof,
}

struct EventReader {
    input: UpstreamStream,
    buffer: Vec<u8>,
    data: Vec<u8>,
    has_data: bool,
    max_bytes: usize,
    idle_timeout_secs: u64,
    upstream_wait_elapsed: Duration,
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
            upstream_wait_elapsed: Duration::ZERO,
            eof: false,
        }
    }

    #[cfg(test)]
    async fn next_event(&mut self) -> Result<Option<Vec<u8>>, String> {
        match self.next_step(None).await? {
            ReadStep::Event(data) => Ok(Some(data)),
            ReadStep::Eof => Ok(None),
            ReadStep::KeepAlive => unreachable!("keepalive disabled"),
        }
    }

    async fn next_step(
        &mut self,
        keepalive_deadline: Option<tokio::time::Instant>,
    ) -> Result<ReadStep, String> {
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
                        return Ok(ReadStep::Event(std::mem::take(&mut self.data)));
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
                    Ok(ReadStep::Eof)
                } else {
                    self.has_data = false;
                    Ok(ReadStep::Event(std::mem::take(&mut self.data)))
                };
            }

            let now = tokio::time::Instant::now();
            if keepalive_deadline.is_some_and(|deadline| now >= deadline) {
                return Ok(ReadStep::KeepAlive);
            }
            let idle_remaining = if self.idle_timeout_secs > 0 {
                let limit = Duration::from_secs(self.idle_timeout_secs);
                if self.upstream_wait_elapsed >= limit {
                    return Err("upstream SSE stream timed out".to_string());
                }
                Some(limit - self.upstream_wait_elapsed)
            } else {
                None
            };
            let keepalive_remaining =
                keepalive_deadline.map(|deadline| deadline.saturating_duration_since(now));
            let wait = match (idle_remaining, keepalive_remaining) {
                (Some(idle), Some(ka)) => Some(idle.min(ka)),
                (Some(idle), None) => Some(idle),
                (None, Some(ka)) => Some(ka),
                (None, None) => None,
            };

            let wait_started = tokio::time::Instant::now();
            let next = match wait {
                None => self.input.next().await,
                Some(duration) => match tokio::time::timeout(duration, self.input.next()).await {
                    Ok(item) => item,
                    Err(_) => {
                        self.upstream_wait_elapsed = self
                            .upstream_wait_elapsed
                            .saturating_add(wait_started.elapsed());
                        if self.idle_timeout_secs > 0
                            && self.upstream_wait_elapsed >= Duration::from_secs(self.idle_timeout_secs)
                        {
                            return Err("upstream SSE stream timed out".to_string());
                        }
                        return Ok(ReadStep::KeepAlive);
                    }
                },
            };
            match next {
                Some(Ok(bytes)) => {
                    self.upstream_wait_elapsed = Duration::ZERO;
                    self.buffer.extend_from_slice(&bytes);
                }
                Some(Err(e)) => return Err(format!("upstream SSE stream failed: {e}")),
                None => self.eof = true,
            }
        }
    }
}

struct StreamAdapter {
    reader: EventReader,
    converter: StreamState,
    upstream_model: String,
    session_id: String,
    keepalive_interval: Duration,
    last_client_emit: tokio::time::Instant,
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
                log_chat_usage(
                    &self.upstream_model,
                    &self.session_id,
                    true,
                    self.converter.usage(),
                );
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
    upstream_model: String,
    session_id: String,
    include_usage: bool,
    max_bytes: usize,
    idle_timeout_secs: u64,
) -> Result<Response, AppError> {
    stream_response_with_keepalive(
        response,
        meta,
        upstream_model,
        session_id,
        include_usage,
        max_bytes,
        idle_timeout_secs,
        SSE_KEEPALIVE_INTERVAL,
    )
}

#[allow(clippy::too_many_arguments)]
fn stream_response_with_keepalive(
    response: Response,
    meta: ChatMeta,
    upstream_model: String,
    session_id: String,
    include_usage: bool,
    max_bytes: usize,
    idle_timeout_secs: u64,
    keepalive_interval: Duration,
) -> Result<Response, AppError> {
    let adapter = StreamAdapter {
        reader: EventReader::new(response.into_body(), max_bytes, idle_timeout_secs),
        converter: StreamState::new(meta, include_usage),
        upstream_model,
        session_id,
        keepalive_interval,
        last_client_emit: tokio::time::Instant::now(),
        pending: VecDeque::new(),
        finished: false,
    };
    let output = stream::unfold(adapter, |mut adapter| async move {
        loop {
            if let Some(bytes) = adapter.pending.pop_front() {
                adapter.last_client_emit = tokio::time::Instant::now();
                return Some((Ok::<_, std::io::Error>(bytes), adapter));
            }
            if adapter.finished {
                return None;
            }
            let keepalive_deadline = (!adapter.keepalive_interval.is_zero())
                .then(|| adapter.last_client_emit + adapter.keepalive_interval);
            match adapter.reader.next_step(keepalive_deadline).await {
                Ok(ReadStep::KeepAlive) => {
                    adapter
                        .pending
                        .push_back(Bytes::from_static(b": keepalive\n\n"));
                }
                Ok(ReadStep::Event(data)) if data == b"[DONE]" => adapter.finish(true),
                Ok(ReadStep::Event(data)) => match serde_json::from_slice::<Value>(&data) {
                    Ok(value) if value.get("error").is_some() => {
                        adapter.fail("upstream returned a stream error".to_string());
                    }
                    Ok(value) => match adapter.converter.push(&value) {
                        Ok(events) => adapter.queue_events(events),
                        Err(err) => adapter.fail(err.message()),
                    },
                    Err(err) => adapter.fail(format!("invalid upstream SSE data: {err}")),
                },
                Ok(ReadStep::Eof) => adapter.finish(false),
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

    #[test]
    fn sha256_matches_standard_vectors() {
        let to_hex = |bytes: [u8; 32]| -> String {
            bytes.iter().map(|b| format!("{b:02x}")).collect()
        };
        assert_eq!(
            to_hex(sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            to_hex(sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn derive_session_id_is_stable_across_turns_and_isolated_by_key() {
        let turn1 = json!({
            "systemInstruction": { "role": "user", "parts": [{ "text": "You are helpful." }] },
            "contents": [
                { "role": "user", "parts": [{ "text": "Start task" }] }
            ]
        });
        let turn2 = json!({
            "systemInstruction": { "role": "user", "parts": [{ "text": "You are helpful." }] },
            "contents": [
                { "role": "user", "parts": [{ "text": "Start task" }] },
                { "role": "model", "parts": [{ "text": "Working on it." }] },
                { "role": "user", "parts": [{ "text": "Now step 2" }] }
            ]
        });
        let other_chat = json!({
            "systemInstruction": { "role": "user", "parts": [{ "text": "You are helpful." }] },
            "contents": [
                { "role": "user", "parts": [{ "text": "Different task" }] }
            ]
        });

        let id1 = derive_session_id("key-a", None, Some("[\"Start task\"]"), &turn1);
        let id2 = derive_session_id("key-a", None, Some("[\"Start task\"]"), &turn2);
        let id_other_chat = derive_session_id("key-a", None, Some("[\"Different task\"]"), &other_chat);
        let id_other_key = derive_session_id("key-b", None, Some("[\"Start task\"]"), &turn1);

        assert!(id1.starts_with("session-"));
        assert_eq!(id1.len(), "session-".len() + 32);
        assert_eq!(id1, id2, "multi-turn conversation must keep same sessionId");
        assert_ne!(id1, id_other_chat);
        assert_ne!(id1, id_other_key);

        // Appending a second adjacent user message still preserves sessionId
        // because conversion captures the first user message before turn merging.
        let single_user = convert_request(&json!({
            "model": "m",
            "messages": [{ "role": "user", "content": "First" }]
        }))
        .unwrap();
        let two_users = convert_request(&json!({
            "model": "m",
            "messages": [
                { "role": "user", "content": "First" },
                { "role": "user", "content": "Second" }
            ]
        }))
        .unwrap();
        assert_eq!(
            derive_session_id(
                "key-a",
                None,
                single_user.first_user_message.as_deref(),
                &single_user.request
            ),
            derive_session_id(
                "key-a",
                None,
                two_users.first_user_message.as_deref(),
                &two_users.request
            )
        );

        // Explicit prompt_cache_key overrides message prefix.
        let explicit1 = derive_session_id("key-a", Some("conv-42"), Some("[\"Start task\"]"), &turn1);
        let explicit2 = derive_session_id("key-a", Some("conv-42"), Some("[\"Different task\"]"), &other_chat);
        let explicit_other_key = derive_session_id("key-b", Some("conv-42"), Some("[\"Start task\"]"), &turn1);
        assert_eq!(explicit1, explicit2);
        assert_ne!(explicit1, id1);
        assert_ne!(explicit1, explicit_other_key);
    }

    #[tokio::test]
    async fn sse_stream_emits_keepalive_during_silence_and_hidden_thoughts() {
        let chunks = stream::unfold(0u8, |step| async move {
            match step {
                0 => {
                    // Stay silent past keepalive interval, then send a hidden thought chunk.
                    tokio::time::sleep(Duration::from_millis(60)).await;
                    let thought = json!({
                        "response": {
                            "candidates": [{ "content": { "parts": [{ "text": "thinking...", "thought": true }] } }]
                        }
                    });
                    Some((
                        Ok::<_, std::io::Error>(Bytes::from(format!("data: {thought}\n\n"))),
                        1,
                    ))
                }
                1 => {
                    // Another hidden thought chunk after another interval.
                    tokio::time::sleep(Duration::from_millis(60)).await;
                    let thought = json!({
                        "response": {
                            "candidates": [{ "content": { "parts": [{ "text": "still thinking...", "thought": true }] } }]
                        }
                    });
                    Some((
                        Ok::<_, std::io::Error>(Bytes::from(format!("data: {thought}\n\n"))),
                        2,
                    ))
                }
                2 => {
                    let answer = json!({
                        "response": {
                            "candidates": [{
                                "content": { "parts": [{ "text": "Done" }] },
                                "finishReason": "STOP"
                            }]
                        }
                    });
                    Some((
                        Ok::<_, std::io::Error>(Bytes::from(format!("data: {answer}\n\n"))),
                        3,
                    ))
                }
                _ => None,
            }
        });
        let upstream = Response::builder()
            .status(StatusCode::OK)
            .body(Body::from_stream(chunks))
            .unwrap();
        let meta = ChatMeta {
            id: "chatcmpl-test".to_string(),
            model: "gemini-3-flash".to_string(),
            created: 1_700_000_000,
        };
        let resp = stream_response_with_keepalive(
            upstream,
            meta,
            "gemini-3-flash".to_string(),
            "session-test".to_string(),
            false,
            1024 * 64,
            5,
            Duration::from_millis(35),
        )
        .unwrap();

        let bytes = to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        let keepalives = text.matches(": keepalive\n\n").count();
        assert!(
            keepalives >= 2,
            "expected at least 2 keepalive frames during silent/thought intervals, got {keepalives}: {text}"
        );
        assert!(text.contains("\"Done\""));
        assert!(text.contains("data: [DONE]\n\n"));
    }

    #[tokio::test]
    async fn sse_keepalive_does_not_mask_upstream_idle_timeout() {
        let never = stream::pending::<Result<Bytes, std::io::Error>>();
        let mut reader = EventReader::new(Body::from_stream(never), 1024, 1);
        let mut keepalives = 0usize;
        let err = loop {
            let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
            match reader.next_step(Some(deadline)).await {
                Ok(ReadStep::KeepAlive) => keepalives += 1,
                Err(e) => break e,
                other => panic!("unexpected step: {other:?}"),
            }
        };
        assert!(keepalives >= 2, "expected keepalives before timeout, got {keepalives}");
        assert_eq!(err, "upstream SSE stream timed out");
    }

    #[tokio::test]
    async fn sse_upstream_comments_do_not_suppress_downstream_keepalive() {
        // Upstream emits frequent SSE comment lines faster than keepalive_interval.
        let chunks = stream::unfold(0u8, |step| async move {
            if step < 6 {
                tokio::time::sleep(Duration::from_millis(15)).await;
                Some((
                    Ok::<_, std::io::Error>(Bytes::from_static(b": upstream-ping\n\n")),
                    step + 1,
                ))
            } else if step == 6 {
                let answer = json!({
                    "response": {
                        "candidates": [{
                            "content": { "parts": [{ "text": "Ok" }] },
                            "finishReason": "STOP"
                        }]
                    }
                });
                Some((
                    Ok::<_, std::io::Error>(Bytes::from(format!("data: {answer}\n\n"))),
                    7,
                ))
            } else {
                None
            }
        });
        let upstream = Response::builder()
            .status(StatusCode::OK)
            .body(Body::from_stream(chunks))
            .unwrap();
        let meta = ChatMeta {
            id: "chatcmpl-comment".to_string(),
            model: "gemini-3-flash".to_string(),
            created: 1_700_000_000,
        };
        let resp = stream_response_with_keepalive(
            upstream,
            meta,
            "gemini-3-flash".to_string(),
            "session-comment".to_string(),
            false,
            1024 * 64,
            5,
            Duration::from_millis(35),
        )
        .unwrap();
        let bytes = to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            text.contains(": keepalive\n\n"),
            "upstream comments must not suppress downstream keepalive: {text}"
        );
        assert!(text.contains("\"Ok\""));
    }

    #[tokio::test]
    async fn sse_downstream_pause_does_not_trigger_false_upstream_timeout() {
        let chunks = stream::iter([Ok::<_, std::io::Error>(Bytes::from_static(
            b"data: {\"ok\":true}\n\n",
        ))]);
        let mut reader = EventReader::new(Body::from_stream(chunks), 1024, 1);
        // Simulate downstream pausing longer than idle_timeout_secs before reading.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_eq!(
            reader.next_event().await.unwrap(),
            Some(b"{\"ok\":true}".to_vec())
        );
    }
}
