//! OpenAI `chat/completions` request -> Gemini `generateContent` request
//! converter. Pure JSON in, JSON out: no network access, no upstream calls and
//! no claims about upstream verification.
//!
//! # Tool-call id codec
//!
//! A Google `thoughtSignature` has to survive a full tool round-trip, but the
//! OpenAI wire format only offers the tool-call `id`. Both are therefore packed
//! into the id handed to the client:
//!
//! ```text
//! call_ag1_ + URL_SAFE_NO_PAD(base64( {"i": "<native id>", "s": "<signature>"} ))
//! ```
//!
//! `"s"` is omitted entirely when there is no signature. The codec is stateless
//! and deterministic (no randomness, no clock): identical input always produces
//! an identical id, and encode -> decode preserves both fields. Ids without the
//! `call_ag1_` prefix pass through unchanged; prefixed ids that fail to decode
//! are rejected, never silently treated as plain ids.
//!
//! # Top-level option policy
//!
//! Every top-level key must be known: unknown keys are rejected by name and
//! never silently dropped. An explicit `null` counts as "absent" for every
//! optional key.
//!
//! Converted and used:
//! - `model` (also echoed in [`ConvertedChat::model`]), `messages`
//! - `n` (only `1`), `stream`, `stream_options.include_usage`
//! - `temperature`, `top_p`, `max_tokens`/`max_completion_tokens`, `stop`
//! - `tools`, `tool_choice`
//!
//! Accepted and deliberately ignored:
//! - `user` (string) - logging-only on our side
//! - `parallel_tool_calls` - `true` only (no native equivalent switch)
//! - `service_tier` - `null`, `"auto"` or `"default"`
//! - `logprobs` (`null`/`false`), `top_logprobs` (`null`), `store`/`background`
//!   (`null`/`false`) - the values that mean "off"
//! - `modalities` - `null` or exactly `["text"]`
//! - `web_search_options` - `null` or `{}`
//!
//! Rejected with a specific error, because silently dropping them would change
//! the meaning of the caller's request:
//! - legacy `functions` / `function_call` (use `tools`)
//! - `response_format`, `audio`, `prediction`, `prompt_cache_key`,
//!   `prompt_cache_retention`
//!
//! Unmapped keys *inside* a message (e.g. `name`) are ignored; message content
//! and tool calls are always validated.
//!
//! # Request key order
//!
//! Keys are inserted as `contents`, `systemInstruction`, `tools`,
//! `generationConfig`. This crate uses `serde_json` without `preserve_order`,
//! so object keys serialize in map order; the native API does not depend on
//! object key order.

use std::collections::{HashMap, HashSet};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::{json, Map, Value};

use crate::error::AppError;

/// Wire prefix marking a tool-call id that carries an embedded native id and,
/// optionally, a Google `thoughtSignature`.
const TOOL_ID_PREFIX: &str = "call_ag1_";

/// Joiner used when several `system`/`developer` messages collapse into the
/// single Gemini `systemInstruction`.
const SYSTEM_JOINER: &str = "\n\n";

/// Every top-level key this converter knows about: converted, ignored on
/// purpose, or rejected with its own dedicated message. Anything else is
/// reported as an unsupported parameter.
const KNOWN_KEYS: &[&str] = &[
    "audio",
    "background",
    "function_call",
    "functions",
    "logprobs",
    "max_completion_tokens",
    "max_tokens",
    "messages",
    "modalities",
    "model",
    "n",
    "parallel_tool_calls",
    "prediction",
    "prompt_cache_key",
    "prompt_cache_retention",
    "reasoning",
    "reasoning_effort",
    "response_format",
    "service_tier",
    "stop",
    "store",
    "stream",
    "stream_options",
    "temperature",
    "tool_choice",
    "tools",
    "top_logprobs",
    "top_p",
    "user",
    "web_search_options",
];

/// One converted OpenAI chat-completions request, ready for the native
/// `generateContent`/`streamGenerateContent` call.
#[derive(Debug, Clone)]
pub(crate) struct ConvertedChat {
    /// Model name as requested by the client (passed through unchanged).
    pub model: String,
    /// Optional reasoning effort requested by the client (`minimal`, `low`,
    /// `medium`, `high`, `xhigh`, `max`, `none`) used by the gateway to route
    /// consolidated models to the matching upstream variant.
    pub reasoning_effort: Option<String>,
    /// Whether the client asked for a streamed response.
    pub stream: bool,
    /// `stream_options.include_usage`: emit a trailing usage-only chunk.
    pub include_usage: bool,
    /// Native request body: `contents`, optional `systemInstruction`, `tools`
    /// and `generationConfig`. `model`, `stream` and `include_usage` are
    /// transport-level and deliberately absent from this body.
    pub request: Value,
}

/// Convert an OpenAI chat-completions request body into a Gemini
/// `generateContent` request body.
///
/// Every rejected or unsupported input yields [`AppError::BadRequest`] naming
/// the offending field (and index where applicable). Never panics on malformed
/// input.
pub(crate) fn convert_request(input: &Value) -> Result<ConvertedChat, AppError> {
    let body = require_object(input, "request body")?;

    for key in body.keys() {
        if !KNOWN_KEYS.contains(&key.as_str()) {
            return bad(format!(
                "unsupported top-level request parameter `{key}`; it cannot be forwarded to the native API"
            ));
        }
    }

    let model = body
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| {
            AppError::BadRequest("`model` is required and must be a non-empty string".to_string())
        })?
        .to_string();

    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .filter(|messages| !messages.is_empty())
        .ok_or_else(|| {
            AppError::BadRequest("`messages` is required and must be a non-empty array".to_string())
        })?;

    if let Some(n) = significant(body.get("n")) {
        if n.as_u64() != Some(1) {
            return bad(format!("`n` must be 1: only n=1 supported (got {n})"));
        }
    }

    let stream = match significant(body.get("stream")) {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(other) => {
            return bad(format!(
                "`stream` must be a boolean, got {}",
                type_name(other)
            ))
        }
    };

    let mut include_usage = false;
    if let Some(options) = significant(body.get("stream_options")) {
        let options = require_object(options, "stream_options")?;
        for (key, value) in options {
            if key != "include_usage" {
                return bad(format!(
                    "`stream_options.{key}` is not supported (only `include_usage` is accepted)"
                ));
            }
            include_usage = value.as_bool().ok_or_else(|| {
                AppError::BadRequest("`stream_options.include_usage` must be a boolean".to_string())
            })?;
        }
    }

    for key in ["functions", "function_call"] {
        if significant(body.get(key)).is_some() {
            return bad(format!("`{key}` is not supported, use `tools` instead"));
        }
    }
    for key in [
        "response_format",
        "audio",
        "prediction",
        "prompt_cache_key",
        "prompt_cache_retention",
    ] {
        reject_if_present(body, key)?;
    }
    if let Some(modalities) = significant(body.get("modalities")) {
        let text_only = modalities
            .as_array()
            .is_some_and(|items| items.len() == 1 && items[0] == Value::String("text".to_string()));
        if !text_only {
            return bad("`modalities` is not supported (only null, absent or exactly [\"text\"] are accepted)");
        }
    }
    if matches!(significant(body.get("logprobs")), Some(value) if value.as_bool() != Some(false)) {
        return bad("`logprobs` is not supported (only null or false are accepted)");
    }
    if significant(body.get("top_logprobs")).is_some() {
        return bad("`top_logprobs` is not supported (only null is accepted)");
    }
    if let Some(value) = significant(body.get("web_search_options")) {
        if !value.as_object().is_some_and(Map::is_empty) {
            return bad(
                "`web_search_options` is not supported (only null or an empty object are accepted)",
            );
        }
    }
    for key in ["store", "background"] {
        if matches!(significant(body.get(key)), Some(value) if value.as_bool() != Some(false)) {
            return bad(format!(
                "`{key}` is not supported (only null or false are accepted)"
            ));
        }
    }
    if let Some(value) = significant(body.get("service_tier")) {
        if !matches!(value.as_str(), Some("auto") | Some("default")) {
            return bad(
                "`service_tier` is not supported (only null, \"auto\" or \"default\" are accepted)",
            );
        }
    }
    if matches!(significant(body.get("user")), Some(value) if !value.is_string()) {
        return bad("`user` must be a string when present");
    }
    if matches!(significant(body.get("parallel_tool_calls")), Some(value) if value.as_bool() != Some(true))
    {
        return bad(
            "`parallel_tool_calls` is not supported (only null, absent or true are accepted)",
        );
    }

    let reasoning_effort = parse_reasoning_effort(body)?;

    let generation_config = generation_config(body)?;
    let tools = tool_declarations(significant(body.get("tools")))?;
    let tool_config = match significant(body.get("tool_choice")) {
        Some(choice) if tools.is_some() => Some(tool_choice_config(choice)?),
        Some(_) => return bad("`tool_choice` requires a non-empty `tools` array"),
        None => None,
    };

    let mut system_texts: Vec<String> = Vec::new();
    let mut builder = TurnBuilder::new();
    for (index, message) in messages.iter().enumerate() {
        let message = require_object(message, &format!("messages[{index}]"))?;
        let role = message.get("role").and_then(Value::as_str).ok_or_else(|| {
            AppError::BadRequest(format!(
                "`messages[{index}].role` is required and must be a string"
            ))
        })?;
        let at = format!("messages[{index}].content");
        match role {
            "system" | "developer" => {
                let texts = text_parts(message.get("content"), &at)?;
                if texts.is_empty() {
                    return bad(format!(
                        "`{at}` must contain at least one text part for role `{role}`"
                    ));
                }
                system_texts.extend(texts);
            }
            "user" => {
                let texts = text_parts(message.get("content"), &at)?;
                if texts.is_empty() {
                    return bad(format!(
                        "`{at}` must be a non-empty string or a text-part array for role `user`"
                    ));
                }
                builder.push_turn("user", texts.iter().map(|text| text_part(text)).collect());
            }
            "assistant" => {
                let mut parts: Vec<Value> = text_parts(message.get("content"), &at)?
                    .iter()
                    .map(|text| text_part(text))
                    .collect();
                match significant(message.get("tool_calls")) {
                    None => {}
                    Some(Value::Array(calls)) => {
                        for (call_index, call) in calls.iter().enumerate() {
                            let call_at = format!("messages[{index}].tool_calls[{call_index}]");
                            parts.push(tool_call_part(call, &call_at, &mut builder)?);
                        }
                    }
                    Some(other) => {
                        return bad(format!(
                            "`messages[{index}].tool_calls` must be an array, got {}",
                            type_name(other)
                        ))
                    }
                }
                if parts.is_empty() {
                    return bad(format!(
                        "`messages[{index}]` (assistant) needs non-empty `content` or non-empty `tool_calls`"
                    ));
                }
                builder.push_turn("model", parts);
            }
            "tool" => {
                let wire_id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| {
                        AppError::BadRequest(format!(
                            "`messages[{index}].tool_call_id` is required and must be a non-empty string"
                        ))
                    })?;
                let content = significant(message.get("content")).ok_or_else(|| {
                    AppError::BadRequest(format!(
                        "`{at}` is required for role `tool` and must not be null"
                    ))
                })?;
                let (native_id, _) = decode_tool_call_id(wire_id)
                    .map_err(|err| contextual(err, &format!("messages[{index}].tool_call_id")))?;
                let name = builder.take_call(&native_id, &format!("messages[{index}]"))?;
                let mut call = Map::new();
                call.insert("id".to_string(), json!(native_id));
                call.insert("name".to_string(), json!(name));
                call.insert(
                    "response".to_string(),
                    json!({ "result": content.clone() }),
                );
                builder.push_turn("user", vec![json!({ "functionResponse": call })]);
            }
            other => {
                return bad(format!(
                    "`messages[{index}].role` value `{other}` is not supported (expected system, developer, user, assistant or tool)"
                ))
            }
        }
    }

    let contents = builder.finish();
    if contents.is_empty() {
        return bad("`messages` must contain at least one user, assistant or tool message");
    }

    let mut request = Map::new();
    request.insert("contents".to_string(), Value::Array(contents));
    if !system_texts.is_empty() {
        // The native contract carries the system text on a literal "user"
        // role inside `systemInstruction`.
        // Upstream returns a synthetic 429 RESOURCE_EXHAUSTED on Copilot's
        // "Follow Microsoft content policies" boilerplate; normalize it.
        let system_text = system_texts
            .join(SYSTEM_JOINER)
            .replace("Follow Microsoft content policies", "Follow standard content policies");
        request.insert(
            "systemInstruction".to_string(),
            json!({ "role": "user", "parts": [text_part(&system_text)] }),
        );
    }
    if let Some(tools) = tools {
        request.insert(
            "tools".to_string(),
            json!([{ "functionDeclarations": tools }]),
        );
    }
    if let Some(config) = tool_config {
        request.insert(
            "toolConfig".to_string(),
            json!({ "functionCallingConfig": config }),
        );
    }
    if !generation_config.is_empty() {
        request.insert(
            "generationConfig".to_string(),
            Value::Object(generation_config),
        );
    }

    Ok(ConvertedChat {
        model,
        reasoning_effort,
        stream,
        include_usage,
        request: Value::Object(request),
    })
}

fn normalize_effort_value(raw: &str, field: &str) -> Result<Option<String>, AppError> {
    let lower = raw.trim().to_ascii_lowercase();
    match lower.as_str() {
        "default" | "" => Ok(None),
        "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" => Ok(Some(lower)),
        _ => bad(format!(
            "`{field}` value `{raw}` is not supported (expected none, minimal, low, medium, high, xhigh or max)"
        )),
    }
}

fn parse_reasoning_effort(body: &Map<String, Value>) -> Result<Option<String>, AppError> {
    if let Some(value) = significant(body.get("reasoning_effort")) {
        let s = value.as_str().ok_or_else(|| {
            AppError::BadRequest(format!(
                "`reasoning_effort` must be a string, got {}",
                type_name(value)
            ))
        })?;
        return normalize_effort_value(s, "reasoning_effort");
    }
    if let Some(value) = significant(body.get("reasoning")) {
        let obj = require_object(value, "reasoning")?;
        if let Some(effort) = significant(obj.get("effort")) {
            let s = effort.as_str().ok_or_else(|| {
                AppError::BadRequest(format!(
                    "`reasoning.effort` must be a string, got {}",
                    type_name(effort)
                ))
            })?;
            return normalize_effort_value(s, "reasoning.effort");
        }
    }
    Ok(None)
}
/// Pack a native tool-call id and an optional Google `thoughtSignature` into
/// the id exposed to OpenAI clients. Deterministic: identical inputs produce
/// identical output.
pub(crate) fn encode_tool_call_id(id: &str, signature: Option<&str>) -> String {
    let mut object = Map::new();
    object.insert("i".to_string(), json!(id));
    if let Some(signature) = signature {
        object.insert("s".to_string(), json!(signature));
    }
    format!(
        "{TOOL_ID_PREFIX}{}",
        URL_SAFE_NO_PAD.encode(Value::Object(object).to_string())
    )
}

/// Inverse of [`encode_tool_call_id`].
///
/// Ids without the `call_ag1_` prefix are returned unchanged with no signature,
/// so a plain native id keeps working. A prefixed id that is not valid
/// base64url-encoded JSON `{"i": <non-empty string>, "s"?: <string>}` is
/// rejected - never handed back as a plain id.
pub(crate) fn decode_tool_call_id(id: &str) -> Result<(String, Option<String>), AppError> {
    let Some(payload) = id.strip_prefix(TOOL_ID_PREFIX) else {
        return Ok((id.to_string(), None));
    };
    let malformed = |reason: String| {
        AppError::BadRequest(format!(
            "tool-call id `{id}` carries the `{TOOL_ID_PREFIX}` prefix but {reason}"
        ))
    };
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|err| malformed(format!("is not valid base64url: {err}")))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|err| malformed(format!("does not decode to JSON: {err}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| malformed("does not decode to a JSON object".to_string()))?;
    let native = object
        .get("i")
        .and_then(Value::as_str)
        .filter(|native| !native.is_empty())
        .ok_or_else(|| malformed("has no non-empty string `i` field".to_string()))?
        .to_string();
    let signature = match object.get("s") {
        None => None,
        Some(Value::String(signature)) => Some(signature.clone()),
        Some(_) => return Err(malformed("`s` field is not a string".to_string())),
    };
    Ok((native, signature))
}

/// Accumulates Gemini contents, merging consecutive turns that share a Gemini
/// role, and tracks which `functionCall` ids still await a `functionResponse`.
struct TurnBuilder {
    contents: Vec<Value>,
    /// Native id -> tool name, for calls not yet answered.
    open_calls: HashMap<String, String>,
    /// Native ids that already produced a `functionResponse`.
    answered: HashSet<String>,
}

impl TurnBuilder {
    fn new() -> Self {
        Self {
            contents: Vec::new(),
            open_calls: HashMap::new(),
            answered: HashSet::new(),
        }
    }

    /// Appends a turn, concatenating its parts into the previous content when
    /// that content has the same Gemini role.
    fn push_turn(&mut self, role: &str, parts: Vec<Value>) {
        if let Some(last) = self.contents.last_mut() {
            let same_role = last
                .get("role")
                .and_then(Value::as_str)
                .is_some_and(|existing| existing == role);
            if same_role {
                if let Some(existing) = last.get_mut("parts").and_then(Value::as_array_mut) {
                    existing.extend(parts);
                    return;
                }
            }
        }
        let mut turn = Map::new();
        turn.insert("role".to_string(), json!(role));
        turn.insert("parts".to_string(), Value::Array(parts));
        self.contents.push(Value::Object(turn));
    }

    /// Registers a call produced by an assistant message; rejects an id that
    /// was already used (in the same message or an earlier one).
    fn register_call(&mut self, native_id: &str, name: &str, at: &str) -> Result<(), AppError> {
        if self.open_calls.contains_key(native_id) || self.answered.contains(native_id) {
            return bad(format!(
                "`{at}` reuses tool-call id `{native_id}`; tool-call ids must be unique"
            ));
        }
        self.open_calls
            .insert(native_id.to_string(), name.to_string());
        Ok(())
    }

    /// Consumes an outstanding call for a `tool` message, returning the tool
    /// name recorded when the call was made.
    fn take_call(&mut self, native_id: &str, at: &str) -> Result<String, AppError> {
        if self.answered.contains(native_id) {
            return bad(format!(
                "`{at}` responds to tool-call id `{native_id}` twice; duplicate tool responses are not allowed"
            ));
        }
        let name = self.open_calls.remove(native_id).ok_or_else(|| {
            AppError::BadRequest(format!(
                "`{at}` is a tool message without matching prior tool_call for id `{native_id}`"
            ))
        })?;
        self.answered.insert(native_id.to_string());
        Ok(name)
    }

    fn finish(self) -> Vec<Value> {
        self.contents
    }
}

/// Build `generationConfig` from the recognised sampling parameters. An empty
/// map means the caller omits the whole object.
fn generation_config(body: &Map<String, Value>) -> Result<Map<String, Value>, AppError> {
    let mut config = Map::new();
    for (key, field) in [("temperature", "temperature"), ("top_p", "topP")] {
        if let Some(value) = significant(body.get(key)) {
            let number = value.as_f64().ok_or_else(|| {
                AppError::BadRequest(format!(
                    "`{key}` must be a number, got {}",
                    type_name(value)
                ))
            })?;
            config.insert(field.to_string(), json!(number));
        }
    }

    let mut max_tokens: Option<u64> = None;
    for key in ["max_tokens", "max_completion_tokens"] {
        let Some(value) = significant(body.get(key)) else {
            continue;
        };
        let value = value.as_u64().filter(|value| *value > 0).ok_or_else(|| {
            AppError::BadRequest(format!(
                "`{key}` must be a positive integer, got {}",
                type_name(value)
            ))
        })?;
        match max_tokens {
            Some(other) if other != value => {
                return bad(format!(
                    "`max_tokens` ({other}) and `{key}` ({value}) disagree; send one value"
                ));
            }
            Some(_) => {}
            None => max_tokens = Some(value),
        }
    }
    if let Some(value) = max_tokens {
        config.insert("maxOutputTokens".to_string(), json!(value));
    }

    if let Some(stop) = significant(body.get("stop")) {
        let sequences = match stop {
            Value::String(stop) => vec![json!(stop)],
            Value::Array(items) => items
                .iter()
                .map(|item| {
                    item.as_str().map(|item| json!(item)).ok_or_else(|| {
                        AppError::BadRequest(format!(
                            "`stop` array entries must be strings, got {}",
                            type_name(item)
                        ))
                    })
                })
                .collect::<Result<Vec<Value>, AppError>>()?,
            other => {
                return bad(format!(
                    "`stop` must be a string or an array of strings, got {}",
                    type_name(other)
                ))
            }
        };
        config.insert("stopSequences".to_string(), Value::Array(sequences));
    }
    Ok(config)
}

/// Validate the OpenAI `tools` array and produce the Gemini
/// `[{ "functionDeclarations": [...] }]` value. `Ok(None)` means "omit tools".
fn tool_declarations(value: Option<&Value>) -> Result<Option<Vec<Value>>, AppError> {
    let Some(value) = value else { return Ok(None) };
    let items = value.as_array().ok_or_else(|| {
        AppError::BadRequest(format!(
            "`tools` must be an array, got {}",
            type_name(value)
        ))
    })?;
    let mut declarations: Vec<Value> = Vec::new();
    let mut names: HashSet<&str> = HashSet::new();
    for (index, tool) in items.iter().enumerate() {
        let at = format!("tools[{index}]");
        let tool = require_object(tool, &at)?;
        match significant(tool.get("type")) {
            None => {}
            Some(Value::String(kind)) if kind == "function" => {}
            Some(other) => {
                return bad(format!(
                    "`{at}.type` must be \"function\", got {}",
                    type_name(other)
                ))
            }
        }
        let absent: &Value = &Value::Null;
        let function = require_object(
            tool.get("function").unwrap_or(absent),
            &format!("{at}.function"),
        )?;
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                AppError::BadRequest(format!(
                    "`{at}.function.name` is required and must be a non-empty string"
                ))
            })?;
        if !names.insert(name) {
            return bad(format!(
                "`{at}.function.name` duplicates tool name `{name}`"
            ));
        }
        let mut declaration = Map::new();
        declaration.insert("name".to_string(), json!(name));
        if let Some(description) = significant(function.get("description")) {
            let description = description.as_str().ok_or_else(|| {
                AppError::BadRequest(format!(
                    "`{at}.function.description` must be a string, got {}",
                    type_name(description)
                ))
            })?;
            declaration.insert("description".to_string(), json!(description));
        }
        if let Some(schema) = merge_schema(
            significant(function.get("parameters")),
            significant(function.get("parametersJsonSchema")),
            &format!("{at}.function"),
        )? {
            declaration.insert("parametersJsonSchema".to_string(), schema.clone());
        }
        declarations.push(Value::Object(declaration));
    }
    if declarations.is_empty() {
        return Ok(None);
    }
    Ok(Some(declarations))
}

/// Pick the schema to embed: `parameters` and `parametersJsonSchema` must agree
/// and are forwarded unmodified as `parametersJsonSchema`.
fn merge_schema<'a>(
    parameters: Option<&'a Value>,
    parameters_json_schema: Option<&'a Value>,
    at: &str,
) -> Result<Option<&'a Value>, AppError> {
    let schema = match (parameters, parameters_json_schema) {
        (None, None) => return Ok(None),
        (Some(schema), None) | (None, Some(schema)) => schema,
        (Some(parameters), Some(json_schema)) => {
            if parameters != json_schema {
                return bad(format!(
                    "`{at}.parameters` and `{at}.parametersJsonSchema` conflict; send one value"
                ));
            }
            parameters
        }
    };
    if schema.as_object().is_none() {
        return bad(format!(
            "`{at}` schema must be a JSON object, got {}",
            type_name(schema)
        ));
    }
    Ok(Some(schema))
}
/// Translate OpenAI `tool_choice` into a Gemini `functionCallingConfig`.
fn tool_choice_config(value: &Value) -> Result<Value, AppError> {
    if let Some(kind) = value.as_str() {
        let mode = match kind {
            "auto" => "AUTO",
            "none" => "NONE",
            "required" => "ANY",
            other => {
                return bad(format!(
                    "`tool_choice` value `{other}` is not supported (expected auto, none, required or a function object)"
                ))
            }
        };
        return Ok(json!({ "mode": mode }));
    }
    let object = require_object(value, "tool_choice")?;
    let kind = object.get("type").and_then(Value::as_str).ok_or_else(|| {
        AppError::BadRequest("`tool_choice.type` is required and must be a string".to_string())
    })?;
    if kind == "allowed_tools" {
        return bad("`tool_choice` type `allowed_tools` is not supported");
    }
    if kind != "function" {
        return bad(format!(
            "`tool_choice.type` value `{kind}` is not supported (expected \"function\")"
        ));
    }
    let null: &Value = &Value::Null;
    let function = require_object(
        object.get("function").unwrap_or(null),
        "tool_choice.function",
    )?;
    let name = function
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            AppError::BadRequest(
                "`tool_choice.function.name` is required and must be a non-empty string"
                    .to_string(),
            )
        })?;
    Ok(json!({ "mode": "ANY", "allowedFunctionNames": [name] }))
}

/// Build one `functionCall` part from an OpenAI tool-call entry. The wire id is
/// decoded, so an embedded `thoughtSignature` is restored into the part and the
/// native id is what the upstream sees.
fn tool_call_part(call: &Value, at: &str, builder: &mut TurnBuilder) -> Result<Value, AppError> {
    let call = require_object(call, at)?;
    match significant(call.get("type")) {
        None => {}
        Some(Value::String(kind)) if kind == "function" => {}
        Some(other) => {
            return bad(format!(
                "`{at}.type` must be \"function\", got {}",
                type_name(other)
            ))
        }
    }
    let wire_id = call
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            AppError::BadRequest(format!(
                "`{at}.id` is required and must be a non-empty string"
            ))
        })?;
    let null: &Value = &Value::Null;
    let function = require_object(
        call.get("function").unwrap_or(null),
        &format!("{at}.function"),
    )?;
    let name = function
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            AppError::BadRequest(format!(
                "`{at}.function.name` is required and must be a non-empty string"
            ))
        })?;
    let args = match significant(function.get("arguments")) {
        None => Value::Object(Map::new()),
        Some(Value::String(raw)) if raw.trim().is_empty() => Value::Object(Map::new()),
        Some(Value::String(raw)) => {
            let parsed: Value = serde_json::from_str(raw).map_err(|err| {
                AppError::BadRequest(format!(
                    "`{at}.function.arguments` is not valid JSON: {err}"
                ))
            })?;
            Value::Object(require_object(&parsed, &format!("{at}.function.arguments"))?.clone())
        }
        Some(object @ Value::Object(_)) => object.clone(),
        Some(other) => {
            return bad(format!(
            "`{at}.function.arguments` must be a JSON object or a string containing one, got {}",
            type_name(other)
        ))
        }
    };

    let (native_id, signature) =
        decode_tool_call_id(wire_id).map_err(|err| contextual(err, &format!("{at}.id")))?;
    builder.register_call(&native_id, name, at)?;

    let mut call_object = Map::new();
    call_object.insert("id".to_string(), json!(native_id));
    call_object.insert("name".to_string(), json!(name));
    call_object.insert("args".to_string(), args);
    let mut part = Map::new();
    part.insert("functionCall".to_string(), Value::Object(call_object));
    if let Some(signature) = signature {
        part.insert("thoughtSignature".to_string(), json!(signature));
    }
    Ok(Value::Object(part))
}

/// Extract plain text from OpenAI message `content`: a string, or an array of
/// `{type:"text",text:string}` parts. Any other part type is rejected.
fn text_parts(value: Option<&Value>, at: &str) -> Result<Vec<String>, AppError> {
    let Some(value) = significant(value) else {
        return Ok(Vec::new());
    };
    match value {
        Value::String(text) if text.is_empty() => Ok(Vec::new()),
        Value::String(text) => Ok(vec![text.clone()]),
        Value::Array(items) => {
            let mut texts = Vec::new();
            for (index, item) in items.iter().enumerate() {
                let item_at = format!("{at}[{index}]");
                let object = require_object(item, &item_at)?;
                if let Some(kind) = significant(object.get("type")) {
                    let kind = kind.as_str().unwrap_or_else(|| type_name(kind));
                    if kind != "text" {
                        return bad(format!(
                            "unsupported content part type `{kind}` in `{item_at}` (image, audio and file inputs are not converted)"
                        ));
                    }
                }
                let text = object
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .ok_or_else(|| {
                        AppError::BadRequest(format!(
                            "`{item_at}.text` is required and must be a non-empty string"
                        ))
                    })?;
                texts.push(text.to_string());
            }
            Ok(texts)
        }
        other => bad(format!(
            "`{at}` must be a string or an array of content parts, got {}",
            type_name(other)
        )),
    }
}

/// Wrap text in a Gemini text part.
fn text_part(text: &str) -> Value {
    let mut part = Map::new();
    part.insert("text".to_string(), json!(text));
    Value::Object(part)
}

/// The value unless it is absent or explicitly `null`.
fn significant(value: Option<&Value>) -> Option<&Value> {
    value.filter(|value| !value.is_null())
}

/// Reject an option this converter cannot honour.
fn reject_if_present(body: &Map<String, Value>, key: &str) -> Result<(), AppError> {
    if significant(body.get(key)).is_some() {
        return bad(format!(
            "`{key}` is not supported by this proxy and cannot be forwarded to the native API"
        ));
    }
    Ok(())
}

/// Require a JSON object, naming the field in the error.
fn require_object<'a>(value: &'a Value, at: &str) -> Result<&'a Map<String, Value>, AppError> {
    value.as_object().ok_or_else(|| {
        AppError::BadRequest(format!(
            "`{at}` must be a JSON object, got {}",
            type_name(value)
        ))
    })
}

/// Short human-readable JSON type name for error messages.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Build a `BadRequest` error.
fn bad<T>(message: impl Into<String>) -> Result<T, AppError> {
    Err(AppError::BadRequest(message.into()))
}

/// Prefix an existing `BadRequest` message with the field it came from.
fn contextual(error: AppError, at: &str) -> AppError {
    match error {
        AppError::BadRequest(message) => AppError::BadRequest(format!("`{at}`: {message}")),
        other => other,
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// Unwrap a successful conversion, or panic with the error message.
    fn ok(input: Value) -> ConvertedChat {
        match convert_request(&input) {
            Ok(converted) => converted,
            Err(err) => panic!("expected success, got: {}", err.message()),
        }
    }

    /// Assert a conversion fails and that the message mentions `needle`.
    fn error_contains(input: Value, needle: &str) {
        match convert_request(&input) {
            Err(AppError::BadRequest(message)) => assert!(
                message.contains(needle),
                "expected message to contain {needle:?}, got: {message}"
            ),
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(converted) => panic!("expected error mentioning {needle:?}, got: {converted:?}"),
        }
    }

    fn contents(value: &Value) -> &Vec<Value> {
        value["contents"].as_array().expect("contents array")
    }

    fn part(content: &Value, index: usize) -> &Value {
        &content["parts"][index]
    }

    fn user_msg() -> Value {
        json!({"role": "user", "content": "hello"})
    }

    #[test]
    fn codec_round_trips_id_and_signature() {
        let encoded = encode_tool_call_id("native-1", Some("sig+ABC/="));
        let (id, signature) = decode_tool_call_id(&encoded).expect("decodes");
        assert_eq!(id, "native-1");
        assert_eq!(signature.as_deref(), Some("sig+ABC/="));

        let without = encode_tool_call_id("native-2", None);
        let (id, signature) = decode_tool_call_id(&without).expect("decodes");
        assert_eq!(id, "native-2");
        assert_eq!(signature, None);
        let raw = String::from_utf8(
            URL_SAFE_NO_PAD
                .decode(without.strip_prefix(TOOL_ID_PREFIX).expect("prefix"))
                .expect("base64"),
        )
        .expect("utf8");
        assert!(!raw.contains("\"s\""), "`s` key must be omitted: {raw}");
    }

    #[test]
    fn codec_escapes_special_characters() {
        let tricky = "id\"with\\slash и юникод";
        let encoded = encode_tool_call_id(tricky, Some("sig \"quoted\""));
        let (id, signature) = decode_tool_call_id(&encoded).expect("decodes");
        assert_eq!(id, tricky);
        assert_eq!(signature.as_deref(), Some("sig \"quoted\""));
    }

    #[test]
    fn codec_is_deterministic() {
        assert_eq!(
            encode_tool_call_id("abc", Some("sig")),
            encode_tool_call_id("abc", Some("sig"))
        );
        assert_eq!(
            encode_tool_call_id("abc", None),
            encode_tool_call_id("abc", None)
        );
        assert_ne!(
            encode_tool_call_id("abc", None),
            encode_tool_call_id("abc", Some("sig"))
        );
    }

    #[test]
    fn codec_passes_plain_ids_through() {
        assert_eq!(
            decode_tool_call_id("call_abc123").expect("plain id"),
            ("call_abc123".to_string(), None)
        );
        assert_eq!(
            decode_tool_call_id("").expect("empty id"),
            (String::new(), None)
        );
    }

    #[test]
    fn codec_rejects_malformed_prefixed_ids() {
        let cases = [
            "call_ag1_!!!".to_string(),
            "call_ag1_".to_string(),
            format!("call_ag1_{}", URL_SAFE_NO_PAD.encode(br#"{"i":""}"#)),
            format!("call_ag1_{}", URL_SAFE_NO_PAD.encode(b"not json at all")),
            format!("call_ag1_{}", URL_SAFE_NO_PAD.encode(b"[1,2]")),
            format!("call_ag1_{}", URL_SAFE_NO_PAD.encode(br#"{"i":"x","s":5}"#)),
        ];
        for id in cases {
            match decode_tool_call_id(&id) {
                Err(AppError::BadRequest(message)) => assert!(
                    message.contains("call_ag1_"),
                    "message should name the id: {message}"
                ),
                other => panic!("expected BadRequest for {id:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn converts_text_turns_with_system_instruction() {
        let converted = ok(json!({
            "model": "gemini-pro",
            "messages": [
                {"role": "system", "content": "be terse"},
                {"role": "developer", "content": [{"type": "text", "text": "be safe"}]},
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "hello"}
            ],
            "temperature": 0.5,
            "top_p": 0.9,
            "max_tokens": 128,
            "stop": ["\n", "END"],
            "user": "tenant-1"
        }));
        assert_eq!(converted.model, "gemini-pro");
        assert!(!converted.stream);
        assert!(!converted.include_usage);

        let request = &converted.request;
        assert_eq!(request["systemInstruction"]["role"], "user");
        assert_eq!(
            request["systemInstruction"]["parts"][0]["text"],
            "be terse\n\nbe safe"
        );
        assert!(request.get("model").is_none());
        assert!(request.get("stream").is_none());
        assert!(request.get("include_usage").is_none());

        let contents = contents(request);
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(part(&contents[0], 0)["text"], "hi");
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(part(&contents[1], 0)["text"], "hello");

        assert_eq!(request["generationConfig"]["temperature"], 0.5);
        assert_eq!(request["generationConfig"]["topP"], 0.9);
        assert_eq!(request["generationConfig"]["maxOutputTokens"], 128);
        assert_eq!(
            request["generationConfig"]["stopSequences"],
            json!(["\n", "END"])
        );
    }

    #[test]
    fn stop_string_and_optional_sections_omission() {
        let converted = ok(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "x"}],
            "stop": "###"
        }));
        assert_eq!(
            converted.request["generationConfig"]["stopSequences"],
            json!(["###"])
        );
        let bare = ok(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "x"}]
        }));
        assert!(bare.request.get("generationConfig").is_none());
        assert!(bare.request.get("systemInstruction").is_none());
        assert!(bare.request.get("tools").is_none());
    }

    #[test]
    fn tool_round_trip_restores_signature() {
        let first_call = encode_tool_call_id("native-call-1", Some("thought-sig"));
        let second_call = encode_tool_call_id("native-call-2", None);
        let second_response = encode_tool_call_id("native-call-2", None);

        let converted = ok(json!({
            "model": "gemini-pro",
            "stream": true,
            "stream_options": {"include_usage": true},
            "messages": [
                {"role": "system", "content": "tools on"},
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": first_call.clone(), "type": "function",
                     "function": {"name": "get_weather", "arguments": "{\"city\":\"Rome\"}"}}
                ]},
                {"role": "tool", "tool_call_id": first_call.clone(), "content": "{\"temp\":30}"},
                {"role": "assistant", "content": "one more", "tool_calls": [
                    {"id": second_call.clone(), "function": {"name": "get_time", "arguments": ""}}
                ]},
                {"role": "tool", "tool_call_id": second_response, "content": 42}
            ],
            "tools": [
                {"type": "function", "function": {
                    "name": "get_weather",
                    "description": "lookup",
                    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
                }},
                {"type": "function", "function": {
                    "name": "get_time",
                    "parametersJsonSchema": {"type": "object"}
                }}
            ],
            "tool_choice": "auto",
            "parallel_tool_calls": true
        }));

        assert!(converted.stream);
        assert!(converted.include_usage);

        let request = &converted.request;
        let tools = request["tools"].as_array().expect("tools");
        assert_eq!(tools.len(), 1);
        let declarations = tools[0]["functionDeclarations"]
            .as_array()
            .expect("declarations");
        assert_eq!(declarations.len(), 2);
        assert_eq!(declarations[0]["name"], "get_weather");
        assert_eq!(declarations[0]["description"], "lookup");
        assert_eq!(
            declarations[0]["parametersJsonSchema"]["properties"]["city"]["type"],
            "string"
        );
        assert_eq!(
            declarations[1]["parametersJsonSchema"],
            json!({"type": "object"})
        );
        assert_eq!(
            request["toolConfig"]["functionCallingConfig"]["mode"],
            "AUTO"
        );

        let contents = contents(request);
        // user | model(call) | user(response) | model(text+call) | user(response)
        assert_eq!(contents.len(), 5);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[2]["role"], "user");
        assert_eq!(contents[3]["role"], "model");
        assert_eq!(contents[4]["role"], "user");

        let call = &part(&contents[1], 0)["functionCall"];
        assert_eq!(call["id"], "native-call-1");
        assert_eq!(call["name"], "get_weather");
        assert_eq!(call["args"], json!({"city": "Rome"}));
        assert_eq!(part(&contents[1], 0)["thoughtSignature"], "thought-sig");

        let responses = contents[2]["parts"].as_array().expect("parts");
        assert_eq!(responses.len(), 1);
        let response = &responses[0]["functionResponse"];
        assert_eq!(response["id"], "native-call-1");
        assert_eq!(response["name"], "get_weather");
        assert_eq!(response["response"]["result"], "{\"temp\":30}");

        // Text part precedes the functionCall part in a model turn.
        assert_eq!(part(&contents[3], 0)["text"], "one more");
        let second = &part(&contents[3], 1)["functionCall"];
        assert_eq!(second["id"], "native-call-2");
        assert_eq!(second["name"], "get_time");
        assert_eq!(second["args"], json!({}));
        assert!(second.get("thoughtSignature").is_none());
        assert_eq!(
            part(&contents[4], 0)["functionResponse"]["response"]["result"],
            json!(42)
        );
    }

    #[test]
    fn consecutive_same_role_messages_merge_into_one_content() {
        let converted = ok(json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "hi", "tool_calls": null},
                {"role": "user", "content": "and again"},
                {"role": "assistant", "tool_calls": [
                    {"id": "c1", "function": {"name": "f", "arguments": "{}"}},
                    {"id": "c2", "function": {"name": "g"}}
                ]},
                {"role": "tool", "tool_call_id": "c1", "content": "one"},
                {"role": "tool", "tool_call_id": "c2", "content": "two"}
            ]
        }));
        let contents = contents(&converted.request);
        assert_eq!(contents.len(), 3);
        assert_eq!(contents[0]["parts"].as_array().expect("parts").len(), 2);
        assert_eq!(contents[1]["parts"].as_array().expect("parts").len(), 2);
        let responses = contents[2]["parts"].as_array().expect("parts");
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["functionResponse"]["name"], "f");
        assert_eq!(responses[1]["functionResponse"]["name"], "g");
        assert_eq!(responses[1]["functionResponse"]["id"], "c2");
    }

    #[test]
    fn tool_reply_then_user_text_share_one_user_content() {
        let converted = ok(json!({
            "model": "m",
            "messages": [
                {"role": "assistant", "tool_calls": [
                    {"id": "c1", "function": {"name": "f", "arguments": {"a": 1}}}
                ]},
                {"role": "tool", "tool_call_id": "c1", "content": "ok"},
                {"role": "user", "content": "thanks"}
            ]
        }));
        let contents = contents(&converted.request);
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[1]["role"], "user");
        let parts = contents[1]["parts"].as_array().expect("parts");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["functionResponse"]["id"], "c1");
        assert_eq!(parts[1]["text"], "thanks");
    }

    #[test]
    fn tool_choice_shapes_map_to_function_calling_config() {
        let tools = || json!([{"type": "function", "function": {"name": "f"}}]);
        let cases = [
            (json!("auto"), "AUTO", None),
            (json!("none"), "NONE", None),
            (json!("required"), "ANY", None),
            (
                json!({"type": "function", "function": {"name": "f"}}),
                "ANY",
                Some(json!(["f"])),
            ),
        ];
        for (tool_choice, mode, names) in cases {
            let converted = ok(json!({
                "model": "m", "messages": [user_msg()],
                "tools": tools(), "tool_choice": tool_choice
            }));
            let config = &converted.request["toolConfig"]["functionCallingConfig"];
            assert_eq!(config["mode"], mode, "for {tool_choice}");
            match names {
                Some(names) => assert_eq!(config["allowedFunctionNames"], names),
                None => assert!(config.get("allowedFunctionNames").is_none()),
            }
        }
        // tool_choice absent -> no functionCallingConfig at all.
        let converted = ok(json!({
            "model": "m", "messages": [user_msg()], "tools": tools()
        }));
        assert!(converted.request.get("toolConfig").is_none());
        // Empty tools array omits tools entirely.
        let converted = ok(json!({
            "model": "m", "messages": [user_msg()], "tools": []
        }));
        assert!(converted.request.get("tools").is_none());
    }

    #[test]
    fn tool_ids_match_across_encoding_forms() {
        // A plain native call is answerable by an encoded tool_call_id.
        let encoded_reply = encode_tool_call_id("c1", None);
        let converted = ok(json!({
            "model": "m",
            "messages": [
                {"role": "assistant", "tool_calls": [
                    {"id": "c1", "function": {"name": "f", "arguments": null}}
                ]},
                {"role": "tool", "tool_call_id": encoded_reply, "content": "ok"}
            ]
        }));
        let response = &part(&contents(&converted.request)[1], 0)["functionResponse"];
        assert_eq!(response["id"], "c1");
        assert_eq!(response["name"], "f");
        assert_eq!(response["response"]["result"], "ok");

        // A malformed prefixed tool_call_id is rejected, not passed through.
        error_contains(
            json!({
                "model": "m",
                "messages": [
                    {"role": "assistant", "tool_calls": [
                        {"id": "c1", "function": {"name": "f"}}
                    ]},
                    {"role": "tool", "tool_call_id": "call_ag1_!!!", "content": "ok"}
                ]
            }),
            "is not valid base64url",
        );
    }

    #[test]
    fn rejected_inputs() {
        let tool = || json!([{"type": "function", "function": {"name": "f"}}]);
        let cases: Vec<(Value, &str)> = vec![
            (json!({"messages": [user_msg()]}), "`model` is required"),
            (
                json!({"model": "", "messages": [user_msg()]}),
                "non-empty string",
            ),
            (json!({"model": "m"}), "`messages` is required"),
            (
                json!({"model": "m", "messages": []}),
                "`messages` is required",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "n": 2}),
                "only n=1 supported",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "n": 0}),
                "only n=1 supported",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "n": -1}),
                "only n=1 supported",
            ),
            (
                json!({"model": "m", "messages": [{"role": "user"}]}),
                "non-empty string or a text-part array",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "stop": 5}),
                "`stop` must be a string or an array of strings",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "stop": ["a", 3]}),
                "`stop` array entries must be strings",
            ),
            (
                json!({"model": "m", "messages": [{"role": "assistant"}]}),
                "needs non-empty `content` or non-empty `tool_calls`",
            ),
            (
                json!({"model": "m", "messages": [{"role": "assistant", "content": []}]}),
                "needs non-empty `content` or non-empty `tool_calls`",
            ),
            (
                json!({"model": "m", "messages": [{"role": "tool", "tool_call_id": "c1", "content": "x"}]}),
                "without matching prior tool_call",
            ),
            (
                json!({"model": "m", "messages": [
                    {"role": "assistant", "tool_calls": [{"id": "c1", "function": {"name": "f"}}]},
                    {"role": "tool", "tool_call_id": "c1", "content": "a"},
                    {"role": "tool", "tool_call_id": "c1", "content": "b"}
                ]}),
                "twice",
            ),
            (
                json!({"model": "m", "messages": [
                    {"role": "assistant", "tool_calls": [
                        {"id": "c1", "function": {"name": "f"}},
                        {"id": "c1", "function": {"name": "g"}}
                    ]}
                ]}),
                "reuses tool-call id",
            ),
            (
                json!({"model": "m", "messages": [
                    {"role": "assistant", "tool_calls": [
                        {"id": "c1", "function": {"name": "f", "arguments": "{oops"}}
                    ]}
                ]}),
                "is not valid JSON",
            ),
            (
                json!({"model": "m", "messages": [
                    {"role": "assistant", "tool_calls": [
                        {"id": "c1", "function": {"name": "f", "arguments": "[1]"}}
                    ]}
                ]}),
                "must be a JSON object",
            ),
            (
                json!({"model": "m", "messages": [{"role": "user", "content": [
                    {"type": "text", "text": "a"},
                    {"type": "image_url", "image_url": {"url": "http://x"}}
                ]}]}),
                "unsupported content part type `image_url`",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "venue": "x"}),
                "unsupported top-level request parameter `venue`",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "functions": []}),
                "use `tools` instead",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "function_call": "auto"}),
                "use `tools` instead",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "tool_choice": "auto"}),
                "requires a non-empty `tools` array",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "tools": [], "tool_choice": "auto"}),
                "requires a non-empty `tools` array",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "tools": [
                    {"type": "function", "function": {"name": "f"}},
                    {"type": "function", "function": {"name": "f"}}
                ]}),
                "duplicates tool name",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "tools": [
                    {"type": "mcp", "function": {"name": "f"}}
                ]}),
                "`tools[0].type` must be \"function\"",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "tools": [
                    {"type": "function", "function": {"name": "f",
                     "parameters": {"type": "object"},
                     "parametersJsonSchema": {"type": "array"}}}
                ]}),
                "conflict",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "tools": [
                    {"type": "function", "function": {"name": "f", "parameters": [1]}}
                ]}),
                "schema must be a JSON object",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "tools": tool(), "tool_choice": {"type": "allowed_tools"}}),
                "`allowed_tools` is not supported",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "tools": tool(), "tool_choice": "forced"}),
                "is not supported (expected auto, none, required",
            ),
            (
                json!({"model": "m", "messages": [{"role": "system", "content": "only system"}]}),
                "at least one user, assistant or tool message",
            ),
            (
                json!({"model": "m", "messages": [{"role": "guest", "content": "x"}]}),
                "is not supported (expected system, developer, user, assistant or tool)",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "max_tokens": 0}),
                "positive integer",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "max_tokens": -2}),
                "positive integer",
            ),
            (
                json!({"model": "m", "messages": [user_msg()],
                       "max_tokens": 10, "max_completion_tokens": 20}),
                "disagree",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "temperature": "hot"}),
                "`temperature` must be a number",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "stream": "yes"}),
                "`stream` must be a boolean",
            ),
            (
                json!({"model": "m", "messages": [user_msg()],
                       "stream_options": {"include_usage": true, "other": 1}}),
                "`stream_options.other` is not supported",
            ),
            (
                json!({"model": "m", "messages": [user_msg()],
                       "stream_options": {"include_usage": "true"}}),
                "must be a boolean",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "response_format": {"type": "json_object"}}),
                "`response_format` is not supported",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "audio": {"voice": "alloy"}}),
                "`audio` is not supported",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "prediction": {"id": "p"}}),
                "`prediction` is not supported",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "logprobs": true}),
                "`logprobs` is not supported",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "top_logprobs": 2}),
                "`top_logprobs` is not supported",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "modalities": ["text", "audio"]}),
                "`modalities` is not supported",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "web_search_options": {"search_context_size": "low"}}),
                "`web_search_options` is not supported",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "store": true}),
                "`store` is not supported",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "background": true}),
                "`background` is not supported",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "service_tier": "pro"}),
                "`service_tier` is not supported",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "prompt_cache_key": "k"}),
                "`prompt_cache_key` is not supported",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "prompt_cache_retention": "24h"}),
                "`prompt_cache_retention` is not supported",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "parallel_tool_calls": false}),
                "`parallel_tool_calls` is not supported",
            ),
            (
                json!({"model": "m", "messages": [user_msg()], "user": 5}),
                "`user` must be a string",
            ),
            (
                json!({"model": "m", "messages": ["nope"]}),
                "`messages[0]` must be a JSON object",
            ),
            (
                json!({"model": "m", "messages": [{"content": "x"}]}),
                "`messages[0].role` is required",
            ),
            (json!("nope"), "`request body` must be a JSON object"),
            (
                json!({"model": "m", "messages": [{"role": "tool", "content": "x"}]}),
                "tool_call_id` is required",
            ),
            (
                json!({"model": "m", "messages": [{"role": "tool", "tool_call_id": "c1"}]}),
                "must not be null",
            ),
            (
                json!({"model": "m", "messages": [
                    {"role": "assistant", "tool_calls": [{"function": {"name": "f"}}]}
                ]}),
                ".id` is required",
            ),
        ];
        for (input, needle) in cases {
            error_contains(input, needle);
        }
    }

    #[test]
    fn ignored_options_are_accepted() {
        let converted = ok(json!({
            "model": "m",
            "messages": [user_msg()],
            "user": "u1",
            "logprobs": false,
            "top_logprobs": null,
            "store": false,
            "background": null,
            "service_tier": "auto",
            "modalities": ["text"],
            "web_search_options": {},
            "stream_options": {},
            "n": 1,
            "max_completion_tokens": 64
        }));
        assert_eq!(converted.request["generationConfig"]["maxOutputTokens"], 64);
        assert!(!converted.stream);
        assert!(!converted.include_usage);

        let streamed = ok(json!({
            "model": "m", "messages": [user_msg()], "stream": true
        }));
        assert!(streamed.stream);
        assert!(!streamed.include_usage);
    }

    #[test]
    fn max_tokens_aliases_agree() {
        let converted = ok(json!({
            "model": "m", "messages": [user_msg()],
            "max_tokens": 32, "max_completion_tokens": 32
        }));
        assert_eq!(converted.request["generationConfig"]["maxOutputTokens"], 32);
    }
}
