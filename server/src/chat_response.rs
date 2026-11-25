//! Google Cloud Code (Gemini) `generateContent` response -> OpenAI
//! `chat/completions` response converter. Pure JSON in, JSON out: no network
//! access and no claims about upstream verification.
//!
//! Accepted input is either a Cloud Code envelope `{"response": {...}}` or a
//! direct `GenerateContentResponse` object. Only `candidates[0]` is used.
//!
//! # Parts policy
//!
//! - `{"text": ..., "thought"?: bool}`: text is emitted only for non-thought
//!   parts; thought text is never exposed.
//! - `{"functionCall": {"id"?, "name", "args"?}}`: translated to an OpenAI
//!   `function` tool call. `args` must be a JSON object and becomes the
//!   `arguments` string (`"{}"` when absent).
//! - any other part shape (`inlineData`, `functionResponse`, unknown keys,
//!   wrong types) is rejected as unsupported - never silently dropped.
//!
//! # Signatures
//!
//! A `thoughtSignature` on a functionCall part is used for that call. A
//! signature riding a standalone/preceding thought part attaches to the *next*
//! functionCall exactly once and is then cleared; one signature is never reused
//! across calls. The signature reaches the client inside the opaque tool-call id
//! built by `crate::chat_request::encode_tool_call_id`.
//!
//! Complete function calls without a native `id` get a fresh random id.
//! Unidentified partial streaming calls are rejected.
//!
//! # Finish reasons and usage
//!
//! `STOP` -> `stop`, `MAX_TOKENS` -> `length`, safety-style refusals ->
//! `content_filter`; anything else is rejected. Tool calls without an explicit
//! reason report `tool_calls`. Usage is forwarded only when upstream sent real
//! numeric token counts - numbers are never invented.

use std::collections::{HashMap, HashSet};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngCore;
use serde_json::{json, Map, Value};

use crate::chat_request::encode_tool_call_id;
use crate::error::AppError;

/// Stable identity for every response object this converter produces.
#[derive(Debug, Clone)]
pub(crate) struct ChatMeta {
    pub id: String,
    pub model: String,
    pub created: u64,
}

/// Google finish reasons that map onto the OpenAI `content_filter` value.
const CONTENT_FILTER_REASONS: &[&str] = &[
    "SAFETY",
    "RECITATION",
    "BLOCKLIST",
    "PROHIBITED_CONTENT",
    "SPII",
    "JAILBREAK",
];

fn bad(message: impl Into<String>) -> AppError {
    AppError::BadGateway(message.into())
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Unwraps the Cloud Code envelope or accepts a direct response object.
fn resolve_payload(value: &Value) -> Result<&Map<String, Value>, AppError> {
    let inner = match value.get("response") {
        Some(inner) => inner,
        None => value,
    };
    inner
        .as_object()
        .ok_or_else(|| bad("upstream response is not a JSON object"))
}

/// Maps a Google `finishReason` onto the OpenAI `finish_reason` vocabulary.
fn map_finish_reason(reason: &str) -> Result<&'static str, AppError> {
    if reason.eq_ignore_ascii_case("STOP") {
        return Ok("stop");
    }
    if reason.eq_ignore_ascii_case("MAX_TOKENS") {
        return Ok("length");
    }
    if CONTENT_FILTER_REASONS
        .iter()
        .any(|name| name.eq_ignore_ascii_case(reason))
    {
        tracing::warn!(%reason, "upstream finishReason mapped to content_filter");
        return Ok("content_filter");
    }
    Err(bad(format!(
        "upstream reported an unknown finishReason `{reason}`"
    )))
}

/// Reads a required non-empty string field.
fn require_string<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    at: &str,
) -> Result<&'a str, AppError> {
    let value = object
        .get(key)
        .filter(|value| !value.is_null())
        .ok_or_else(|| bad(format!("`{at}` has no string `{key}`")))?;
    value
        .as_str()
        .filter(|text| !text.is_empty())
        .ok_or_else(|| bad(format!("`{at}` field `{key}` is not a non-empty string")))
}

/// Reads an optional string field.
fn optional_string<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    at: &str,
) -> Result<Option<&'a str>, AppError> {
    match object.get(key).filter(|value| !value.is_null()) {
        Some(Value::String(text)) if !text.is_empty() => Ok(Some(text)),
        Some(Value::String(_)) => Err(bad(format!("`{at}` field `{key}` is empty"))),
        Some(other) => Err(bad(format!(
            "`{at}` field `{key}` is {}, expected a string",
            type_name(other)
        ))),
        None => Ok(None),
    }
}

/// Optional `thoughtSignature` on a part.
fn part_signature<'a>(
    object: &'a Map<String, Value>,
    at: &str,
) -> Result<Option<&'a str>, AppError> {
    optional_string(object, "thoughtSignature", at)
}

/// Extracts real numeric token counts; never fabricates numbers.
fn extract_usage(payload: &Map<String, Value>) -> Result<Option<Value>, AppError> {
    let Some(meta) = payload.get("usageMetadata").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let meta = meta.as_object().ok_or_else(|| {
        bad(format!(
            "upstream `usageMetadata` is {}, expected an object",
            type_name(meta)
        ))
    })?;
    let count = |key: &str| -> Result<Option<u64>, AppError> {
        match meta.get(key).filter(|value| !value.is_null()) {
            None => Ok(None),
            Some(value) => value.as_u64().map(Some).ok_or_else(|| {
                bad(format!(
                    "upstream `usageMetadata.{key}` is {}, expected a non-negative integer",
                    type_name(value)
                ))
            }),
        }
    };
    let prompt = count("promptTokenCount")?;
    let completion = count("candidatesTokenCount")?;
    let total = match count("totalTokenCount")? {
        Some(total) => Some(total),
        None => prompt.zip(completion).map(|(p, c)| p + c),
    };
    if prompt.is_none() && completion.is_none() && total.is_none() {
        return Ok(None);
    }
    let mut usage = Map::new();
    if let Some(prompt) = prompt {
        usage.insert("prompt_tokens".to_string(), json!(prompt));
    }
    if let Some(completion) = completion {
        usage.insert("completion_tokens".to_string(), json!(completion));
    }
    if let Some(total) = total {
        usage.insert("total_tokens".to_string(), json!(total));
    }
    Ok(Some(Value::Object(usage)))
}

/// Fresh random native tool-call id for calls upstream left unlabelled.
fn random_native_id() -> String {
    let mut bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut bytes);
    format!("native_{}", URL_SAFE_NO_PAD.encode(bytes))
}

/// One `functionCall` found in an upstream event.
struct CallPart {
    native_id: Option<String>,
    name: Option<String>,
    arguments: String,
    signature: Option<String>,
}

/// Everything one upstream event contributes.
struct Event {
    text: String,
    calls: Vec<CallPart>,
    finish: Option<&'static str>,
    usage: Option<Value>,
}

/// Renders `functionCall.args` as an `arguments` fragment. Streaming also
/// accepts a raw string fragment, because streamed arguments may be split.
fn call_arguments(args: Option<&Value>, at: &str, streaming: bool) -> Result<String, AppError> {
    match args.filter(|value| !value.is_null()) {
        None => Ok(if streaming {
            String::new()
        } else {
            "{}".to_string()
        }),
        Some(Value::Object(object)) => Ok(Value::Object(object.clone()).to_string()),
        Some(Value::String(fragment)) if streaming => Ok(fragment.clone()),
        Some(other) => Err(bad(format!(
            "`{at}` has `functionCall.args` that is {}, expected a JSON object",
            type_name(other)
        ))),
    }
}

/// Scans `candidates[0].content.parts`, the finish reason and usage metadata of
/// one event. `pending_signature` carries a signature from an earlier event so
/// it can still attach to the next function call.
fn read_event(
    payload: &Map<String, Value>,
    streaming: bool,
    pending_signature: &mut Option<String>,
) -> Result<Event, AppError> {
    let mut event = Event {
        text: String::new(),
        calls: Vec::new(),
        finish: None,
        usage: extract_usage(payload)?,
    };

    if let Some(reason) = payload
        .get("promptFeedback")
        .and_then(|feedback| feedback.get("blockReason"))
        .and_then(Value::as_str)
        .filter(|reason| !reason.is_empty() && *reason != "BLOCK_REASON_UNSPECIFIED")
    {
        tracing::warn!(%reason, "upstream blocked the prompt");
        event.finish = Some("content_filter");
    }

    if let Some(reason) = payload.get("finishReason").filter(|value| !value.is_null()) {
        let reason = reason.as_str().ok_or_else(|| {
            bad(format!(
                "upstream `finishReason` is {}, expected a string",
                type_name(reason)
            ))
        })?;
        event.finish = Some(map_finish_reason(reason)?);
    }

    let candidates = match payload.get("candidates").filter(|value| !value.is_null()) {
        None => return Ok(event),
        Some(Value::Array(list)) => list,
        Some(other) => {
            return Err(bad(format!(
                "upstream `candidates` is {}, expected an array",
                type_name(other)
            )))
        }
    };
    let Some(first) = candidates.first().filter(|value| !value.is_null()) else {
        return Ok(event);
    };
    let candidate = first
        .as_object()
        .ok_or_else(|| bad("upstream `candidates[0]` is not a JSON object"))?;

    if event.finish.is_none() {
        if let Some(reason) = candidate
            .get("finishReason")
            .filter(|value| !value.is_null())
        {
            let reason = reason.as_str().ok_or_else(|| {
                bad(format!(
                    "upstream `candidates[0].finishReason` is {}, expected a string",
                    type_name(reason)
                ))
            })?;
            event.finish = Some(map_finish_reason(reason)?);
        }
    }

    let Some(content) = candidate.get("content").filter(|value| !value.is_null()) else {
        return Ok(event);
    };
    let content = content
        .as_object()
        .ok_or_else(|| bad("upstream candidate `content` is not a JSON object"))?;
    let parts = match content.get("parts").filter(|value| !value.is_null()) {
        None => return Ok(event),
        Some(Value::Array(list)) => list,
        Some(other) => {
            return Err(bad(format!(
                "upstream candidate `parts` is {}, expected an array",
                type_name(other)
            )))
        }
    };

    for (index, part) in parts.iter().enumerate() {
        let at = format!("upstream `candidates[0].content.parts[{index}]`");
        let object = part
            .as_object()
            .ok_or_else(|| bad(format!("`{at}` is {}, expected an object", type_name(part))))?;
        let signature = part_signature(object, &at)?;

        if let Some(call) = object.get("functionCall").filter(|value| !value.is_null()) {
            let call_object = call.as_object().ok_or_else(|| {
                bad(format!(
                    "`{at}` has a `functionCall` that is {}, expected an object",
                    type_name(call)
                ))
            })?;
            let name = if streaming {
                optional_string(call_object, "name", &at)?.map(str::to_string)
            } else {
                Some(require_string(call_object, "name", &at)?.to_string())
            };
            let native_id = optional_string(call_object, "id", &at)?.map(str::to_string);
            let arguments = call_arguments(call_object.get("args"), &at, streaming)?;
            // A signature on the call itself wins; otherwise consume the
            // signature parked on a preceding thought part, exactly once.
            let previous_signature = pending_signature.take();
            let signature = signature.map(str::to_string).or(previous_signature);
            event.calls.push(CallPart {
                native_id,
                name,
                arguments,
                signature,
            });
            continue;
        }

        let text = match object.get("text").filter(|value| !value.is_null()) {
            Some(Value::String(text)) => text.as_str(),
            Some(other) => {
                return Err(bad(format!(
                    "`{at}` has a `text` field that is {}, expected a string",
                    type_name(other)
                )))
            }
            None => {
                return Err(bad(format!(
                    "`{at}` is not a supported part (expected `text` or `functionCall`)"
                )))
            }
        };
        match object.get("thought").filter(|value| !value.is_null()) {
            Some(Value::Bool(_)) | None => {}
            Some(other) => {
                return Err(bad(format!(
                    "`{at}` has a `thought` field that is {}, expected a boolean",
                    type_name(other)
                )))
            }
        }
        if object
            .get("thought")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            // Thought text is private reasoning: never exposed. A signature
            // riding it must still survive to the next tool call.
            if let Some(signature) = signature {
                *pending_signature = Some(signature.to_string());
            }
            continue;
        }
        if let Some(signature) = signature {
            *pending_signature = Some(signature.to_string());
        }
        event.text.push_str(text);
    }

    Ok(event)
}

/// Builds the OpenAI tool-call object for one native call.
fn build_tool_call(native_id: &str, name: &str, arguments: &str, signature: Option<&str>) -> Value {
    json!({
        "id": encode_tool_call_id(native_id, signature),
        "type": "function",
        "function": { "name": name, "arguments": arguments },
    })
}

/// Converts one native non-streaming response into a `chat.completion` object.
pub(crate) fn convert_response(value: &Value, meta: &ChatMeta) -> Result<Value, AppError> {
    let payload = resolve_payload(value)?;
    let mut pending_signature = None;
    let event = read_event(payload, false, &mut pending_signature)?;
    if event.finish.is_none() && event.text.is_empty() && event.calls.is_empty() {
        return Err(bad("upstream returned an empty completion"));
    }

    let mut seen: HashSet<String> = HashSet::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    for call in &event.calls {
        let native_id = match &call.native_id {
            Some(id) => {
                if !seen.insert(id.clone()) {
                    return Err(bad(format!(
                        "upstream reused tool-call id `{id}`; ids must be unique"
                    )));
                }
                id.clone()
            }
            None => loop {
                let id = random_native_id();
                if seen.insert(id.clone()) {
                    break id;
                }
            },
        };
        let name = call
            .name
            .as_deref()
            .ok_or_else(|| bad("upstream function call has no name"))?;
        tool_calls.push(build_tool_call(
            &native_id,
            name,
            &call.arguments,
            call.signature.as_deref(),
        ));
    }

    let finish_reason = match (event.finish, tool_calls.is_empty()) {
        (Some("stop") | None, false) => "tool_calls",
        (Some(reason), _) => reason,
        (None, true) => "stop",
    };

    let mut message = Map::new();
    message.insert("role".to_string(), json!("assistant"));
    message.insert(
        "content".to_string(),
        if event.text.is_empty() {
            Value::Null
        } else {
            json!(event.text)
        },
    );
    if !tool_calls.is_empty() {
        message.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }

    let mut choice = Map::new();
    choice.insert("index".to_string(), json!(0));
    choice.insert("message".to_string(), Value::Object(message));
    choice.insert("logprobs".to_string(), Value::Null);
    choice.insert("finish_reason".to_string(), json!(finish_reason));

    let mut response = Map::new();
    response.insert("id".to_string(), json!(meta.id));
    response.insert("object".to_string(), json!("chat.completion"));
    response.insert("created".to_string(), json!(meta.created));
    response.insert("model".to_string(), json!(meta.model));
    response.insert("choices".to_string(), json!([choice]));
    if let Some(usage) = event.usage {
        response.insert("usage".to_string(), usage);
    }
    Ok(Value::Object(response))
}

/// Incremental converter for one native SSE stream.
pub(crate) struct StreamState {
    meta: ChatMeta,
    include_usage: bool,
    role_emitted: bool,
    next_index: u64,
    open: HashMap<String, usize>,
    names: Vec<String>,
    had_tool_calls: bool,
    finish: Option<&'static str>,
    usage: Option<Value>,
    pending_signature: Option<String>,
    closed: bool,
    failed: bool,
}

impl StreamState {
    pub(crate) fn new(meta: ChatMeta, include_usage: bool) -> Self {
        StreamState {
            meta,
            include_usage,
            role_emitted: false,
            next_index: 0,
            open: HashMap::new(),
            names: Vec::new(),
            had_tool_calls: false,
            finish: None,
            usage: None,
            pending_signature: None,
            closed: false,
            failed: false,
        }
    }

    fn chunk(&self, delta: Value, finish_reason: Option<&str>) -> Value {
        let mut choice = Map::new();
        choice.insert("index".to_string(), json!(0));
        choice.insert("delta".to_string(), delta);
        choice.insert(
            "finish_reason".to_string(),
            match finish_reason {
                Some(reason) => json!(reason),
                None => Value::Null,
            },
        );
        self.base(json!([choice]))
    }

    fn base(&self, choices: Value) -> Value {
        let mut object = Map::new();
        object.insert("id".to_string(), json!(self.meta.id));
        object.insert("object".to_string(), json!("chat.completion.chunk"));
        object.insert("created".to_string(), json!(self.meta.created));
        object.insert("model".to_string(), json!(self.meta.model));
        object.insert("choices".to_string(), choices);
        Value::Object(object)
    }

    /// Starts a delta object, emitting the assistant role exactly once.
    fn delta(&mut self) -> Map<String, Value> {
        let mut delta = Map::new();
        if !self.role_emitted {
            self.role_emitted = true;
            delta.insert("role".to_string(), json!("assistant"));
        }
        delta
    }

    pub(crate) fn push(&mut self, value: &Value) -> Result<Vec<Value>, AppError> {
        let result = self.push_inner(value);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn push_inner(&mut self, value: &Value) -> Result<Vec<Value>, AppError> {
        if self.failed {
            return Err(bad("upstream stream already failed"));
        }
        if self.closed {
            return Ok(Vec::new());
        }
        let payload = resolve_payload(value)?;
        let event = read_event(payload, true, &mut self.pending_signature)?;
        if event.finish.is_some() {
            self.finish = event.finish;
        }
        if event.usage.is_some() {
            self.usage = event.usage;
        }

        let mut out = Vec::new();
        if !event.text.is_empty() {
            let mut delta = self.delta();
            delta.insert("content".to_string(), json!(event.text));
            out.push(self.chunk(Value::Object(delta), None));
        }

        for call in &event.calls {
            let generated;
            let native_id = match call.native_id.as_deref() {
                Some(id) => id,
                None if call.name.is_some() => {
                    generated = random_native_id();
                    &generated
                }
                None => return Err(bad("upstream streamed an unidentified partial function call; partial function calls are not supported")),
            };
            if let Some(&index) = self.open.get(native_id) {
                if let Some(name) = &call.name {
                    if self.names[index] != *name {
                        return Err(bad(format!(
                            "upstream reused tool-call id `{native_id}` for a different function name"
                        )));
                    }
                }
                if !call.arguments.is_empty() {
                    let mut delta = Map::new();
                    delta.insert("index".to_string(), json!(index));
                    delta.insert(
                        "function".to_string(),
                        json!({ "arguments": call.arguments }),
                    );
                    out.push(self.chunk(json!({ "tool_calls": [delta] }), None));
                }
                continue;
            }
            let name = call.name.as_deref().ok_or_else(|| {
                bad(format!(
                    "upstream streamed a new function call `{native_id}` without a name"
                ))
            })?;
            let index = self.next_index;
            self.next_index += 1;
            self.open.insert(native_id.to_string(), index as usize);
            self.names.push(name.to_string());
            self.had_tool_calls = true;
            let mut call_delta = Map::new();
            call_delta.insert("index".to_string(), json!(index));
            call_delta.insert(
                "id".to_string(),
                json!(encode_tool_call_id(native_id, call.signature.as_deref())),
            );
            call_delta.insert("type".to_string(), json!("function"));
            call_delta.insert(
                "function".to_string(),
                json!({ "name": name, "arguments": call.arguments }),
            );
            let mut delta = self.delta();
            delta.insert("tool_calls".to_string(), json!([call_delta]));
            out.push(self.chunk(Value::Object(delta), None));
        }
        Ok(out)
    }

    /// Produces the terminal chunk(s). `saw_done` reports whether upstream sent
    /// an explicit `[DONE]`. Never emits `[DONE]` itself - the caller owns
    /// SSE framing.
    pub(crate) fn finish(&mut self, saw_done: bool) -> Result<Vec<Value>, AppError> {
        if self.failed {
            return Err(bad("upstream stream already failed"));
        }
        if self.closed {
            return Ok(Vec::new());
        }
        let finish_reason = match (self.finish, saw_done) {
            (Some("stop"), _) if self.had_tool_calls => "tool_calls",
            (Some(reason), _) => reason,
            (None, true) => {
                if self.had_tool_calls {
                    "tool_calls"
                } else {
                    "stop"
                }
            }
            (None, false) => {
                return Err(bad(
                    "upstream stream ended before reporting a finish reason",
                ))
            }
        };

        if finish_reason == "content_filter" || (!self.role_emitted && !self.had_tool_calls) {
            tracing::warn!(
                %finish_reason,
                role_emitted = self.role_emitted,
                had_tool_calls = self.had_tool_calls,
                "OpenAI stream finished with content_filter or empty output"
            );
        }
        let mut out = Vec::new();
        if !self.role_emitted {
            self.role_emitted = true;
            let mut delta = Map::new();
            delta.insert("role".to_string(), json!("assistant"));
            out.push(self.chunk(Value::Object(delta), None));
        }
        out.push(self.chunk(Value::Object(Map::new()), Some(finish_reason)));
        if self.include_usage {
            if let Some(usage) = self.usage.take() {
                let mut chunk = Map::new();
                chunk.insert("id".to_string(), json!(self.meta.id));
                chunk.insert("object".to_string(), json!("chat.completion.chunk"));
                chunk.insert("created".to_string(), json!(self.meta.created));
                chunk.insert("model".to_string(), json!(self.meta.model));
                chunk.insert("choices".to_string(), json!([]));
                chunk.insert("usage".to_string(), usage);
                out.push(Value::Object(chunk));
            }
        }
        self.closed = true;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_request::decode_tool_call_id;

    fn meta() -> ChatMeta {
        ChatMeta {
            id: "chatcmpl-test".to_string(),
            model: "gemini-pro".to_string(),
            created: 1_700_000_000,
        }
    }

    fn native(parts: Vec<Value>, finish: Option<&str>) -> Value {
        let mut candidate = json!({ "content": { "role": "model", "parts": parts } });
        if let Some(finish) = finish {
            candidate
                .as_object_mut()
                .unwrap()
                .insert("finishReason".to_string(), json!(finish));
        }
        json!({ "candidates": [candidate] })
    }

    fn error_contains(result: Result<impl Sized, AppError>, needle: &str) {
        let message = result
            .err()
            .unwrap_or_else(|| panic!("expected an error"))
            .message();
        assert!(
            message.contains(needle),
            "error `{message}` does not contain `{needle}`"
        );
    }

    fn ok<T>(result: Result<T, AppError>) -> T {
        result.unwrap_or_else(|err| panic!("unexpected error: {}", err.message()))
    }

    #[test]
    fn converts_plain_text_with_envelope() {
        let response = ok(convert_response(
            &json!({ "response": native(vec![json!({ "text": "Hi" })], Some("STOP")) }),
            &meta(),
        ));
        assert_eq!(response["id"], "chatcmpl-test");
        assert_eq!(response["object"], "chat.completion");
        assert_eq!(response["created"], 1_700_000_000);
        assert_eq!(response["model"], "gemini-pro");
        assert_eq!(response["choices"][0]["message"]["role"], "assistant");
        assert_eq!(response["choices"][0]["message"]["content"], "Hi");
        assert_eq!(response["choices"][0]["finish_reason"], "stop");
        assert!(response.get("usage").is_none(), "no invented usage");
    }

    #[test]
    fn direct_shape_null_content_and_usage_passthrough() {
        let mut value = native(vec![json!({ "text": "" })], Some("MAX_TOKENS"));
        value["usageMetadata"] =
            json!({ "promptTokenCount": 7, "candidatesTokenCount": 3, "totalTokenCount": 10 });
        let response = ok(convert_response(&value, &meta()));
        assert_eq!(response["choices"][0]["message"]["content"], Value::Null);
        assert_eq!(response["choices"][0]["finish_reason"], "length");
        assert_eq!(response["usage"]["prompt_tokens"], 7);
        assert_eq!(response["usage"]["completion_tokens"], 3);
        assert_eq!(response["usage"]["total_tokens"], 10);
    }

    #[test]
    fn thought_text_is_never_exposed() {
        let value = native(
            vec![
                json!({ "text": "private", "thought": true }),
                json!({ "text": "public" }),
            ],
            Some("STOP"),
        );
        let response = ok(convert_response(&value, &meta()));
        assert_eq!(response["choices"][0]["message"]["content"], "public");
        assert!(!response.to_string().contains("private"));
    }

    #[test]
    fn signed_text_is_returned_in_both_response_modes() {
        let value = native(
            vec![json!({ "text": "answer", "thoughtSignature": "signature" })],
            Some("STOP"),
        );
        let response = ok(convert_response(&value, &meta()));
        assert_eq!(response["choices"][0]["message"]["content"], "answer");
        let mut state = StreamState::new(meta(), false);
        let chunks = ok(state.push(&value));
        assert_eq!(chunks[0]["choices"][0]["delta"]["content"], "answer");
    }

    #[test]
    fn blocked_and_empty_completions_are_not_successful_stops() {
        let response = ok(convert_response(
            &json!({ "promptFeedback": { "blockReason": "SAFETY" } }),
            &meta(),
        ));
        assert_eq!(response["choices"][0]["finish_reason"], "content_filter");
        error_contains(convert_response(&json!({}), &meta()), "empty completion");
    }

    #[test]
    fn tool_call_signature_round_trip() {
        let value = native(
            vec![json!({
                "functionCall": { "id": "call-1", "name": "get_weather", "args": { "city": "R" } },
                "thoughtSignature": "sig-1",
            })],
            Some("STOP"),
        );
        let response = ok(convert_response(&value, &meta()));
        let call = &response["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["name"], "get_weather");
        assert_eq!(call["function"]["arguments"], r#"{"city":"R"}"#);
        assert_eq!(response["choices"][0]["finish_reason"], "tool_calls");
        let (id, signature) = decode_tool_call_id(call["id"].as_str().unwrap()).expect("decodes");
        assert_eq!(id, "call-1");
        assert_eq!(signature.as_deref(), Some("sig-1"));
    }

    #[test]
    fn pending_signature_attaches_once() {
        let value = native(
            vec![
                json!({ "text": "think", "thought": true, "thoughtSignature": "sig-A" }),
                json!({ "functionCall": { "id": "a", "name": "one" } }),
                json!({ "functionCall": { "id": "b", "name": "two" } }),
            ],
            None,
        );
        let response = ok(convert_response(&value, &meta()));
        let calls = response["choices"][0]["message"]["tool_calls"]
            .as_array()
            .unwrap()
            .clone();
        let (_, first) = decode_tool_call_id(calls[0]["id"].as_str().unwrap()).unwrap();
        let (_, second) = decode_tool_call_id(calls[1]["id"].as_str().unwrap()).unwrap();
        assert_eq!(first.as_deref(), Some("sig-A"));
        assert_eq!(second, None, "signature must not be reused");
        assert_eq!(calls[0]["function"]["arguments"], "{}");
    }

    #[test]
    fn multi_tool_gets_unique_generated_ids() {
        let value = native(
            vec![
                json!({ "functionCall": { "name": "one", "args": {} } }),
                json!({ "functionCall": { "name": "two", "args": {} } }),
            ],
            None,
        );
        let response = ok(convert_response(&value, &meta()));
        let calls = response["choices"][0]["message"]["tool_calls"]
            .as_array()
            .unwrap()
            .clone();
        let first = calls[0]["id"].as_str().unwrap();
        let second = calls[1]["id"].as_str().unwrap();
        assert_ne!(first, second);
        for call in &calls {
            let (id, _) = decode_tool_call_id(call["id"].as_str().unwrap()).unwrap();
            assert!(id.starts_with("native_"), "generated id: {id}");
        }
    }

    #[test]
    fn duplicate_native_ids_rejected() {
        let value = native(
            vec![
                json!({ "functionCall": { "id": "dup", "name": "one" } }),
                json!({ "functionCall": { "id": "dup", "name": "two" } }),
            ],
            None,
        );
        error_contains(convert_response(&value, &meta()), "ids must be unique");
    }

    #[test]
    fn unknown_finish_reason_rejected() {
        error_contains(
            convert_response(&native(vec![json!({ "text": "x" })], Some("NOPE")), &meta()),
            "unknown finishReason",
        );
    }

    #[test]
    fn invalid_function_args_rejected() {
        let value = native(
            vec![json!({ "functionCall": { "id": "c", "name": "f", "args": [1, 2] } })],
            None,
        );
        error_contains(convert_response(&value, &meta()), "expected a JSON object");
    }

    #[test]
    fn unsupported_part_rejected() {
        let value = native(
            vec![json!({ "inlineData": { "mimeType": "image/png" } })],
            None,
        );
        error_contains(convert_response(&value, &meta()), "not a supported part");
    }

    #[test]
    fn truncated_maps_to_length_and_safety_to_content_filter() {
        let response = ok(convert_response(
            &native(vec![json!({ "text": "cut" })], Some("MAX_TOKENS")),
            &meta(),
        ));
        assert_eq!(response["choices"][0]["finish_reason"], "length");
        let response = ok(convert_response(&native(vec![], Some("SAFETY")), &meta()));
        assert_eq!(response["choices"][0]["finish_reason"], "content_filter");
    }

    fn stream_part(value: Value) -> Value {
        json!({ "response": value })
    }

    #[test]
    fn stream_happy_path() {
        let mut state = StreamState::new(meta(), true);
        let chunks = ok(state.push(&stream_part(native(vec![json!({ "text": "Hel" })], None))));
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(chunks[0]["choices"][0]["delta"]["content"], "Hel");
        assert_eq!(chunks[0]["object"], "chat.completion.chunk");

        let chunks = ok(state.push(&stream_part(native(
            vec![
                json!({ "text": "lo" }),
                json!({ "functionCall": { "id": "s1", "name": "f", "args": { "a": 1 } } }),
            ],
            None,
        ))));
        assert_eq!(chunks[0]["choices"][0]["delta"]["content"], "lo");
        assert!(
            chunks[0]["choices"][0]["delta"].get("role").is_none(),
            "role emitted once"
        );
        let delta = &chunks[1]["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(delta["index"], 0);
        assert_eq!(delta["function"]["name"], "f");

        let chunks = ok(state.push(&stream_part(native(
            vec![
                json!({ "functionCall": { "id": "s1", "args": "{\"b\":2}" } }),
                json!({ "functionCall": { "id": "s2", "name": "g" } }),
            ],
            None,
        ))));
        assert_eq!(
            chunks[0]["choices"][0]["delta"]["tool_calls"][0]["index"],
            0
        );
        assert_eq!(
            chunks[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "{\"b\":2}"
        );
        assert_eq!(
            chunks[1]["choices"][0]["delta"]["tool_calls"][0]["index"],
            1
        );
        assert_eq!(
            chunks[1]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
            "g"
        );

        let mut value = native(vec![], Some("STOP"));
        value["usageMetadata"] = json!({ "promptTokenCount": 4, "candidatesTokenCount": 6 });
        let chunks = ok(state.push(&stream_part(value)));
        assert!(chunks.is_empty(), "finish reason recorded silently");

        let chunks = ok(state.finish(true));
        assert_eq!(chunks[0]["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(chunks[0]["choices"][0]["delta"], json!({}));
        assert_eq!(chunks[1]["choices"], json!([]));
        assert_eq!(chunks[1]["usage"]["prompt_tokens"], 4);
        assert_eq!(chunks[1]["usage"]["total_tokens"], 10);
        let text = chunks
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("");
        assert!(!text.contains("[DONE]"), "never emits [DONE]");
        assert!(ok(state.finish(true)).is_empty(), "finish is idempotent");
    }

    #[test]
    fn stream_tool_call_finish_default_and_done() {
        let mut state = StreamState::new(meta(), false);
        ok(state.push(&stream_part(native(
            vec![json!({ "functionCall": { "id": "x", "name": "f" } })],
            None,
        ))));
        let chunks = ok(state.finish(true));
        assert_eq!(chunks[0]["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(
            chunks.len(),
            1,
            "usage chunk only when requested and present"
        );
    }

    #[test]
    fn stream_eof_without_finish_reason_errors() {
        let mut state = StreamState::new(meta(), false);
        ok(state.push(&stream_part(native(
            vec![json!({ "text": "partial" })],
            None,
        ))));
        error_contains(
            state.finish(false),
            "ended before reporting a finish reason",
        );
    }

    #[test]
    fn stream_partial_call_rejected() {
        let mut state = StreamState::new(meta(), false);
        error_contains(
            state.push(&stream_part(native(
                vec![json!({ "functionCall": { "args": { "a": 1 } } })],
                None,
            ))),
            "partial function calls are not supported",
        );
        error_contains(state.finish(true), "already failed");
    }

    #[test]
    fn call_signature_does_not_reuse_pending_signature() {
        let value = native(
            vec![
                json!({ "text": "thought", "thought": true, "thoughtSignature": "old" }),
                json!({ "functionCall": { "name": "f", "args": {} }, "thoughtSignature": "new" }),
                json!({ "functionCall": { "name": "g", "args": {} } }),
            ],
            Some("STOP"),
        );
        let response = ok(convert_response(&value, &meta()));
        let calls = response["choices"][0]["message"]["tool_calls"]
            .as_array()
            .unwrap();
        let first = decode_tool_call_id(calls[0]["id"].as_str().unwrap()).unwrap();
        let second = decode_tool_call_id(calls[1]["id"].as_str().unwrap()).unwrap();
        assert_eq!(first.1.as_deref(), Some("new"));
        assert_eq!(second.1, None);
    }

    #[test]
    fn stream_complete_call_without_native_id() {
        let mut state = StreamState::new(meta(), false);
        let chunks = ok(state.push(&stream_part(native(
            vec![json!({ "functionCall": { "name": "f", "args": {} } })],
            None,
        ))));
        let id = chunks[0]["choices"][0]["delta"]["tool_calls"][0]["id"]
            .as_str()
            .unwrap();
        assert!(!decode_tool_call_id(id).unwrap().0.is_empty());
    }

    #[test]
    fn stream_signature_survives_events() {
        let mut state = StreamState::new(meta(), false);
        ok(state.push(&stream_part(native(
            vec![json!({ "text": "why", "thought": true, "thoughtSignature": "sg" })],
            None,
        ))));
        let chunks = ok(state.push(&stream_part(native(
            vec![json!({ "functionCall": { "id": "z", "name": "f" } })],
            None,
        ))));
        let id = chunks[0]["choices"][0]["delta"]["tool_calls"][0]["id"]
            .as_str()
            .unwrap();
        let (native_id, signature) = decode_tool_call_id(id).unwrap();
        assert_eq!(native_id, "z");
        assert_eq!(signature.as_deref(), Some("sg"));
        assert!(!chunks
            .iter()
            .map(Value::to_string)
            .collect::<String>()
            .contains("why"));
    }

    #[test]
    fn stream_reuses_stable_meta_identity() {
        let mut state = StreamState::new(meta(), false);
        let chunks = ok(state.push(&stream_part(native(vec![json!({ "text": "a" })], None))));
        assert_eq!(chunks[0]["id"], "chatcmpl-test");
        assert_eq!(chunks[0]["created"], 1_700_000_000);
        assert_eq!(chunks[0]["model"], "gemini-pro");
    }
}
