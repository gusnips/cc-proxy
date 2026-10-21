//! Google Gemini `v1beta` (`generateContent` / `streamGenerateContent?alt=sse`)
//! wire translator. Shared by OpenCode (`opencode`, `opencode-zen`), the
//! native `gemini` provider and `antigravity`.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;

use axum::body::Body;
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{Map, Value, json};

use crate::anthropic::{
    schema::MessagesRequest,
    sse::{encode_sse_event, parse_sse_events},
};
use crate::monitor::MonitorHandle;
use crate::providers::{
    grok::translate::stream::{SseDecoder, stream_error},
    translate_shared::{ContentBlock, flatten_system_text, normalize_content},
};
use crate::traffic::{StreamTrafficCapture, TrafficCapture};

const DEFAULT_MAX_TOKENS: u32 = 32_000;

fn emit_sse(out: &mut Vec<u8>, event: &str, payload: &Value) {
    let data = serde_json::to_string(payload).unwrap_or_else(|_| "{}".to_string());
    out.extend(encode_sse_event(Some(event), &data));
}

/// The field a Google backend reads tool schemas from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSchemas {
    /// `parametersJsonSchema`, standard JSON Schema: the Gemini API and the
    /// OpenCode gateways in front of it.
    JsonSchema,
    /// `parameters`, Google's OpenAPI subset: Cloud Code Assist (Antigravity),
    /// which turns it into Claude's `input_schema`.
    OpenApi,
}

pub fn prepare_request(body: &MessagesRequest, schemas: ToolSchemas) -> anyhow::Result<Value> {
    let mut tool_names_by_id: HashMap<String, String> = HashMap::new();
    for message in &body.messages {
        for block in normalize_content(&message.content, json!({})) {
            if let ContentBlock::ToolUse { id, name, .. } = block {
                tool_names_by_id.insert(id, name);
            }
        }
    }

    let mut contents = Vec::new();
    for message in &body.messages {
        let role = if message.role == "assistant" {
            "model"
        } else {
            "user"
        };
        let mut parts = Vec::new();
        let mut saw_call_in_turn = false;
        for block in normalize_content(&message.content, json!({})) {
            match block {
                ContentBlock::Text { text } => {
                    if !text.is_empty() {
                        parts.push(json!({ "text": text }));
                    }
                }
                ContentBlock::Thinking { .. } => {}
                ContentBlock::Image { source } => {
                    if source.source_type == "base64" && !source.data.is_empty() {
                        parts.push(json!({
                            "inlineData": {
                                "mimeType": source.media_type,
                                "data": source.data
                            }
                        }));
                    }
                }
                ContentBlock::ToolUse { id, name, input } => {
                    let args = if input.is_object() { input } else { json!({}) };
                    let mut part = json!({
                        "functionCall": {
                            "id": id,
                            "name": name,
                            "args": args
                        }
                    });
                    if !saw_call_in_turn {
                        saw_call_in_turn = true;
                        part["thoughtSignature"] = json!("skip_thought_signature_validator");
                    }
                    parts.push(part);
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    let name = tool_names_by_id
                        .get(&tool_use_id)
                        .cloned()
                        .unwrap_or_else(|| tool_use_id.clone());
                    parts.extend(tool_result_parts(
                        &tool_use_id,
                        &name,
                        &content,
                        is_error.unwrap_or(false),
                    ));
                }
            }
        }
        if !parts.is_empty() {
            contents.push(json!({
                "role": role,
                "parts": parts
            }));
        }
    }

    let mut payload = Map::new();
    payload.insert("contents".to_string(), Value::Array(contents));

    if let Some(system_text) = flatten_system_text(body.extra.get("system"))
        && !system_text.trim().is_empty()
    {
        payload.insert(
            "systemInstruction".to_string(),
            json!({
                "parts": [{ "text": system_text }]
            }),
        );
    }

    if let Some(tools) = body.extra.get("tools").and_then(Value::as_array) {
        let declarations: Vec<Value> = tools
            .iter()
            .filter_map(|tool| {
                let name = tool.get("name")?.as_str()?;
                if tool.get("input_schema").is_none()
                    && tool
                        .get("type")
                        .and_then(Value::as_str)
                        .is_some_and(|kind| kind != "custom" && kind != "function")
                {
                    return None;
                }
                let mut decl = Map::new();
                decl.insert("name".to_string(), Value::String(name.to_string()));
                if let Some(desc) = tool.get("description").and_then(Value::as_str)
                    && !desc.is_empty()
                {
                    decl.insert("description".to_string(), Value::String(desc.to_string()));
                }
                if let Some(schema) = tool.get("input_schema") {
                    let (field, schema) = match schemas {
                        ToolSchemas::JsonSchema => {
                            ("parametersJsonSchema", gemini_json_schema(schema))
                        }
                        ToolSchemas::OpenApi => ("parameters", clean_json_schema(schema)),
                    };
                    decl.insert(field.to_string(), schema);
                }
                Some(Value::Object(decl))
            })
            .collect();
        if !declarations.is_empty() {
            payload.insert(
                "tools".to_string(),
                json!([{ "functionDeclarations": declarations }]),
            );
        }
    }

    let mut gen_cfg = Map::new();
    let max_tokens = body
        .max_tokens
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_MAX_TOKENS);
    gen_cfg.insert("maxOutputTokens".to_string(), json!(max_tokens));
    if let Some(temp) = body.extra.get("temperature").and_then(Value::as_f64) {
        gen_cfg.insert("temperature".to_string(), json!(temp));
    }
    if let Some(top_p) = body.extra.get("top_p").and_then(Value::as_f64) {
        gen_cfg.insert("topP".to_string(), json!(top_p));
    }
    payload.insert("generationConfig".to_string(), Value::Object(gen_cfg));

    Ok(Value::Object(payload))
}

/// Builds the `functionResponse` part and any sibling `inlineData` image parts
/// for a tool result. Keeping base64 images out of `response.result` avoids
/// turning a screenshot into hundreds of thousands of text tokens.
fn tool_result_parts(tool_use_id: &str, name: &str, content: &Value, is_error: bool) -> Vec<Value> {
    let mut texts = Vec::new();
    let mut images = Vec::new();
    match content {
        Value::Null => {}
        Value::String(s) => texts.push(s.clone()),
        Value::Array(items) => {
            for item in items {
                if item.get("type").and_then(Value::as_str) == Some("image") {
                    if let Some(source) = item.get("source") {
                        let source_type = source
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("base64");
                        let media_type = source
                            .get("media_type")
                            .and_then(Value::as_str)
                            .unwrap_or("image/png");
                        let data = source
                            .get("data")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if source_type == "base64" && !data.is_empty() {
                            images.push(json!({
                                "inlineData": {
                                    "mimeType": media_type,
                                    "data": data
                                }
                            }));
                        }
                    }
                    texts.push("[image]".to_string());
                } else if let Some(text) = item.get("text").and_then(Value::as_str) {
                    texts.push(text.to_string());
                } else if let Ok(serialized) = serde_json::to_string(item) {
                    texts.push(serialized);
                }
            }
        }
        other => {
            if let Ok(serialized) = serde_json::to_string(other) {
                texts.push(serialized);
            }
        }
    }
    let mut out = Vec::with_capacity(1 + images.len());
    out.push(json!({
        "functionResponse": {
            "id": tool_use_id,
            "name": name,
            "response": {
                "result": texts.join("\n"),
                "is_error": is_error
            }
        }
    }));
    out.extend(images);
    out
}

/// Keys whose value maps names to schemas.
const SCHEMA_MAPS: [&str; 6] = [
    "properties",
    "patternProperties",
    "$defs",
    "definitions",
    "dependentSchemas",
    "dependencies",
];
/// Keys whose value is literal data, never a schema to rewrite.
const SCHEMA_VALUES: [&str; 5] = ["const", "default", "enum", "examples", "dependentRequired"];

/// A tool schema for `parametersJsonSchema`. Gemini takes standard JSON
/// Schema there, arrays without `items` included, and rejects only a few
/// shapes that published tool schemas often contain. Those are rewritten the
/// way opencode's `GeminiJsonSchema.normalize` does, and the rest goes as is.
fn gemini_json_schema(schema: &Value) -> Value {
    let Value::Object(mut root) = normalize_json_schema(schema) else {
        return json!({ "type": "object" });
    };
    // Function arguments are always an object.
    root.entry("type").or_insert_with(|| json!("object"));
    for defs in ["$defs", "definitions"] {
        if let Some(Value::Object(entries)) = root.get_mut(defs) {
            for (name, def) in entries.iter_mut() {
                *def = cut_ref_loops(def, &format!("#/{defs}/{name}"), false);
            }
        }
    }
    cut_ref_loops(&Value::Object(root), "#", false)
}

fn normalize_json_schema(schema: &Value) -> Value {
    let node = match schema {
        Value::Array(items) => {
            return Value::Array(items.iter().map(normalize_json_schema).collect());
        }
        Value::Object(node) => node,
        other => return other.clone(),
    };
    let properties = node.get("properties").and_then(Value::as_object);
    let tuple = node.get("items").is_some_and(Value::is_array);
    let mut out = Map::new();
    for (key, value) in node {
        match (key.as_str(), value) {
            (key, _) if SCHEMA_VALUES.contains(&key) => {
                out.insert(key.to_string(), value.clone());
            }
            (key, Value::Object(children)) if SCHEMA_MAPS.contains(&key) => {
                let children = children
                    .iter()
                    .map(|(name, child)| (name.clone(), normalize_json_schema(child)))
                    .collect();
                out.insert(key.to_string(), Value::Object(children));
            }
            // `required` may only name declared properties. Gemini refuses a
            // name it can't find, even when there is no `properties` at all.
            ("required", Value::Array(names)) => {
                let declared = names
                    .iter()
                    .filter(|name| {
                        name.as_str()
                            .is_some_and(|name| properties.is_some_and(|p| p.contains_key(name)))
                    })
                    .cloned()
                    .collect();
                out.insert(key.clone(), Value::Array(declared));
            }
            // Draft-04 boolean exclusive bounds become the numeric form.
            ("exclusiveMinimum" | "exclusiveMaximum", Value::Bool(exclusive)) => {
                let bound = if key == "exclusiveMinimum" {
                    "minimum"
                } else {
                    "maximum"
                };
                if let Some(limit) = node
                    .get(bound)
                    .filter(|limit| *exclusive && limit.is_number())
                {
                    out.insert(key.clone(), limit.clone());
                }
            }
            ("minimum", _) if node.get("exclusiveMinimum") == Some(&Value::Bool(true)) => {}
            ("maximum", _) if node.get("exclusiveMaximum") == Some(&Value::Bool(true)) => {}
            // Draft-07 tuples (`items` array plus `additionalItems`) are
            // `prefixItems` plus `items` in 2020-12.
            ("items", Value::Array(_)) => {
                out.insert("prefixItems".to_string(), normalize_json_schema(value));
            }
            ("additionalItems", _) if tuple => {
                out.insert("items".to_string(), normalize_json_schema(value));
            }
            _ => {
                out.insert(key.clone(), normalize_json_schema(value));
            }
        }
    }
    Value::Object(out)
}

/// Gemini takes a recursive `$ref` only when the loop passes through an
/// optional property or an `items` that may be empty. Any other
/// self-reference to `target` becomes an unconstrained schema.
fn cut_ref_loops(schema: &Value, target: &str, safe: bool) -> Value {
    let node = match schema {
        Value::Array(items) => {
            return Value::Array(
                items
                    .iter()
                    .map(|item| cut_ref_loops(item, target, safe))
                    .collect(),
            );
        }
        Value::Object(node) => node,
        other => return other.clone(),
    };
    if !safe && node.get("$ref").and_then(Value::as_str) == Some(target) {
        return json!({});
    }
    let required: Vec<&str> = node
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let may_be_empty = !node
        .get("minItems")
        .and_then(Value::as_f64)
        .is_some_and(|min| min > 0.0);
    let out = node
        .iter()
        .map(|(key, value)| {
            let value = match (key.as_str(), value) {
                (key, _) if SCHEMA_VALUES.contains(&key) => value.clone(),
                ("items", _) => cut_ref_loops(value, target, safe || may_be_empty),
                ("properties", Value::Object(children)) => Value::Object(
                    children
                        .iter()
                        .map(|(name, child)| {
                            let optional = !required.contains(&name.as_str());
                            (name.clone(), cut_ref_loops(child, target, safe || optional))
                        })
                        .collect(),
                ),
                (key, Value::Object(children)) if SCHEMA_MAPS.contains(&key) => Value::Object(
                    children
                        .iter()
                        .map(|(name, child)| (name.clone(), cut_ref_loops(child, target, safe)))
                        .collect(),
                ),
                _ => cut_ref_loops(value, target, safe),
            };
            (key.clone(), value)
        })
        .collect();
    Value::Object(out)
}

/// Sanitizes a JSON Schema to the subset Google's `parameters` field accepts
/// (`type`, `nullable`, `description`, `format`, `enum`, `properties`,
/// `items`, `required`, `anyOf`). Only Cloud Code Assist gets this dialect.
fn clean_json_schema(value: &Value) -> Value {
    let Some(map) = value.as_object() else {
        return json!({ "type": "object" });
    };
    let mut out = Map::new();
    let mut nullable = map
        .get("nullable")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if let Some(ty) = map.get("type") {
        match ty {
            Value::String(s) => {
                if s == "null" {
                    nullable = true;
                } else {
                    out.insert("type".to_string(), Value::String(s.clone()));
                }
            }
            Value::Array(arr) => {
                let mut picked = None;
                for item in arr {
                    if let Some(s) = item.as_str() {
                        if s == "null" {
                            nullable = true;
                        } else if picked.is_none() {
                            picked = Some(s.to_string());
                        }
                    }
                }
                if let Some(s) = picked {
                    out.insert("type".to_string(), Value::String(s));
                }
            }
            _ => {}
        }
    }
    if nullable {
        out.insert("nullable".to_string(), Value::Bool(true));
    }
    if let Some(desc) = map.get("description").and_then(Value::as_str)
        && !desc.is_empty()
    {
        out.insert("description".to_string(), Value::String(desc.to_string()));
    }
    if let Some(fmt) = map.get("format").and_then(Value::as_str)
        && matches!(fmt, "enum" | "date-time")
    {
        out.insert("format".to_string(), Value::String(fmt.to_string()));
    }
    if let Some(en) = map.get("enum").and_then(Value::as_array) {
        let strings: Vec<Value> = en
            .iter()
            .filter_map(|v| v.as_str().map(|s| Value::String(s.to_string())))
            .collect();
        if !strings.is_empty() {
            out.insert("enum".to_string(), Value::Array(strings));
        }
    } else if let Some(c) = map.get("const").and_then(Value::as_str) {
        out.insert("enum".to_string(), json!([c]));
    }
    if let Some(props) = map.get("properties").and_then(Value::as_object) {
        let mut clean_props = Map::new();
        for (prop_name, prop_schema) in props {
            clean_props.insert(prop_name.clone(), clean_json_schema(prop_schema));
        }
        if let Some(req) = map.get("required").and_then(Value::as_array) {
            let valid_req: Vec<Value> = req
                .iter()
                .filter_map(Value::as_str)
                .filter(|k| clean_props.contains_key(*k))
                .map(|k| Value::String(k.to_string()))
                .collect();
            if !valid_req.is_empty() {
                out.insert("required".to_string(), Value::Array(valid_req));
            }
        }
        out.insert("properties".to_string(), Value::Object(clean_props));
        if !out.contains_key("type") {
            out.insert("type".to_string(), Value::String("object".to_string()));
        }
    }
    if let Some(items) = map.get("items") {
        let clean_items = match items {
            Value::Array(arr) => arr
                .first()
                .map(clean_json_schema)
                .unwrap_or_else(|| json!({ "type": "string" })),
            other => clean_json_schema(other),
        };
        out.insert("items".to_string(), clean_items);
        if !out.contains_key("type") {
            out.insert("type".to_string(), Value::String("array".to_string()));
        }
    }
    if let Some(any_of) = map
        .get("anyOf")
        .or_else(|| map.get("oneOf"))
        .and_then(Value::as_array)
    {
        let mut branches = Vec::new();
        for branch in any_of {
            if branch.get("type").and_then(Value::as_str) == Some("null") {
                out.insert("nullable".to_string(), Value::Bool(true));
                continue;
            }
            branches.push(clean_json_schema(branch));
        }
        if branches.len() == 1 {
            if let Some(single) = branches.pop().and_then(|v| v.as_object().cloned()) {
                for (k, v) in single {
                    out.entry(k).or_insert(v);
                }
            }
        } else if !branches.is_empty() {
            out.insert("anyOf".to_string(), Value::Array(branches));
        }
    }
    if !out.contains_key("type") && !out.contains_key("anyOf") {
        out.insert("type".to_string(), Value::String("string".to_string()));
    }
    // `parameters` rejects an array without `items` for the whole request, so
    // a string item keeps it valid, as opencodex's `completeArrayItems` does.
    if out.get("type").and_then(Value::as_str) == Some("array") && !out.contains_key("items") {
        out.insert("items".to_string(), json!({ "type": "string" }));
    }
    Value::Object(out)
}

pub fn accumulate_response(
    bytes: &[u8],
    message_id: &str,
    requested_model: &str,
) -> anyhow::Result<Value> {
    let chunks: Vec<Value> = if let Ok(single) = serde_json::from_slice::<Value>(bytes) {
        if let Some(arr) = single.as_array() {
            arr.clone()
        } else {
            vec![single]
        }
    } else {
        let mut out = Vec::new();
        for event in parse_sse_events(bytes) {
            if event.data.trim().is_empty() || event.data.trim() == "[DONE]" {
                continue;
            }
            out.push(serde_json::from_str::<Value>(&event.data)?);
        }
        if out.is_empty() {
            anyhow::bail!("empty Google response payload");
        }
        out
    };

    let mut text_acc = String::new();
    let mut thinking_acc = String::new();
    let mut tool_uses = Vec::new();
    let mut finish_reason = "end_turn";
    let mut input_tokens = 0_u64;
    let mut output_tokens = 0_u64;

    for chunk in &chunks {
        let root = chunk.get("response").unwrap_or(chunk);
        if let Some(usage) = root.get("usageMetadata") {
            if let Some(inp) = usage.get("promptTokenCount").and_then(Value::as_u64) {
                input_tokens = inp;
            }
            if let Some(out) = usage.get("candidatesTokenCount").and_then(Value::as_u64) {
                output_tokens = out;
            }
        }
        let Some(candidate) = root
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        else {
            continue;
        };
        if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str)
            && reason == "MAX_TOKENS"
        {
            finish_reason = "max_tokens";
        }
        if let Some(parts) = candidate
            .pointer("/content/parts")
            .and_then(Value::as_array)
        {
            for part in parts {
                if part.get("thought").and_then(Value::as_bool) == Some(true) {
                    if let Some(t) = part.get("text").and_then(Value::as_str) {
                        thinking_acc.push_str(t);
                    }
                } else if let Some(t) = part.get("text").and_then(Value::as_str) {
                    text_acc.push_str(t);
                } else if let Some(call) = part.get("functionCall")
                    && let Some(name) = call.get("name").and_then(Value::as_str)
                {
                    let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
                    let id = format!("toolu_{}", uuid::Uuid::new_v4().simple());
                    tool_uses.push(json!({
                        "type": "tool_use",
                        "id": id,
                        "name": name,
                        "input": args
                    }));
                }
            }
        }
    }

    let mut content = Vec::new();
    if !thinking_acc.is_empty() {
        content.push(json!({
            "type": "thinking",
            "thinking": thinking_acc,
            "signature": "google-thought"
        }));
    }
    if !text_acc.is_empty() || (content.is_empty() && tool_uses.is_empty()) {
        content.push(json!({
            "type": "text",
            "text": text_acc
        }));
    }
    let has_tools = !tool_uses.is_empty();
    content.extend(tool_uses);
    if has_tools {
        finish_reason = "tool_use";
    }

    Ok(json!({
        "id": message_id,
        "type": "message",
        "role": "assistant",
        "model": requested_model,
        "content": content,
        "stop_reason": finish_reason,
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens
        }
    }))
}

pub fn stream_body<S, E>(
    upstream: S,
    provider_name: &'static str,
    message_id: String,
    requested_model: String,
    monitor: Option<MonitorHandle>,
    req_id: String,
    traffic: Option<Arc<TrafficCapture>>,
) -> Body
where
    S: futures_util::Stream<Item = Result<Bytes, E>> + Send + Unpin + 'static,
    E: Send + 'static,
{
    let state = GoogleStreamState {
        upstream,
        provider_name,
        decoder: SseDecoder::default(),
        message_id,
        requested_model,
        started: false,
        text_block_open: false,
        next_index: 0,
        saw_tool_use: false,
        finish_reason: "end_turn",
        input_tokens: 0,
        output_tokens: 0,
        terminal: false,
        monitor,
        req_id,
        bytes: 0,
        stream_capture: traffic.as_ref().map(|t| t.stream_capture()),
        traffic,
    };
    let stream = futures_util::stream::unfold(state, |mut state| async move {
        state
            .next_output()
            .await
            .map(|bytes| (Ok::<Bytes, Infallible>(bytes), state))
    });
    Body::from_stream(stream)
}

struct GoogleStreamState<S> {
    upstream: S,
    provider_name: &'static str,
    decoder: SseDecoder,
    message_id: String,
    requested_model: String,
    started: bool,
    text_block_open: bool,
    next_index: usize,
    saw_tool_use: bool,
    finish_reason: &'static str,
    input_tokens: u64,
    output_tokens: u64,
    terminal: bool,
    monitor: Option<MonitorHandle>,
    req_id: String,
    bytes: u64,
    stream_capture: Option<StreamTrafficCapture>,
    traffic: Option<Arc<TrafficCapture>>,
}

impl<S, E> GoogleStreamState<S>
where
    S: futures_util::Stream<Item = Result<Bytes, E>> + Unpin,
{
    async fn next_output(&mut self) -> Option<Bytes> {
        if self.terminal {
            return None;
        }
        loop {
            match self.upstream.next().await {
                Some(Ok(chunk)) => {
                    if self.bytes == 0
                        && let Some(monitor) = self.monitor.as_ref()
                    {
                        monitor.generation_started(&self.req_id);
                    }
                    self.bytes = self.bytes.saturating_add(chunk.len() as u64);
                    let events = match self.decoder.push(&chunk) {
                        Ok(events) => events,
                        Err(err) => {
                            self.terminal = true;
                            return Some(Bytes::from(stream_error(
                                "api_error",
                                &format!("{} stream decode failed: {err}", self.provider_name),
                            )));
                        }
                    };
                    let mut out = Vec::new();
                    for event in events {
                        if event.data.trim().is_empty() || event.data.trim() == "[DONE]" {
                            continue;
                        }
                        let Ok(frame) = serde_json::from_str::<Value>(&event.data) else {
                            self.terminal = true;
                            return Some(Bytes::from(stream_error(
                                "api_error",
                                &format!("{} returned malformed JSON in SSE", self.provider_name),
                            )));
                        };
                        if let Some(capture) = self.stream_capture.as_mut() {
                            capture.upstream_event(event.event.as_deref(), &frame);
                        }
                        self.translate_frame(&frame, &mut out);
                    }
                    if !out.is_empty() {
                        return Some(Bytes::from(out));
                    }
                }
                Some(Err(_)) => {
                    self.terminal = true;
                    return Some(Bytes::from(stream_error(
                        "api_error",
                        &format!("{} upstream stream failed", self.provider_name),
                    )));
                }
                None => {
                    self.terminal = true;
                    let _ = self.decoder.finish();
                    if !self.started {
                        return Some(Bytes::from(stream_error(
                            "api_error",
                            &format!(
                                "{} stream ended before sending any data",
                                self.provider_name
                            ),
                        )));
                    }
                    let mut out = Vec::new();
                    if self.text_block_open {
                        emit_sse(
                            &mut out,
                            "content_block_stop",
                            &json!({ "type": "content_block_stop", "index": self.next_index }),
                        );
                        self.text_block_open = false;
                        self.next_index += 1;
                    }
                    let stop_reason = if self.saw_tool_use {
                        "tool_use"
                    } else {
                        self.finish_reason
                    };
                    emit_sse(
                        &mut out,
                        "message_delta",
                        &json!({
                            "type": "message_delta",
                            "delta": { "stop_reason": stop_reason, "stop_sequence": Value::Null },
                            "usage": { "output_tokens": self.output_tokens }
                        }),
                    );
                    emit_sse(&mut out, "message_stop", &json!({ "type": "message_stop" }));
                    if let (Some(capture), Some(traffic)) =
                        (self.stream_capture.take(), self.traffic.as_ref())
                    {
                        capture.finish(
                            traffic,
                            json!({
                                "stop_reason": stop_reason,
                                "usage": {
                                    "input_tokens": self.input_tokens,
                                    "output_tokens": self.output_tokens
                                }
                            }),
                        );
                    }
                    if let Some(monitor) = self.monitor.as_ref() {
                        monitor.usage_updated(
                            &self.req_id,
                            Some(self.input_tokens),
                            Some(self.output_tokens),
                        );
                    }
                    return Some(Bytes::from(out));
                }
            }
        }
    }

    fn ensure_started(&mut self, out: &mut Vec<u8>) {
        if !self.started {
            self.started = true;
            emit_sse(
                out,
                "message_start",
                &json!({
                    "type": "message_start",
                    "message": {
                        "id": self.message_id,
                        "type": "message",
                        "role": "assistant",
                        "model": self.requested_model,
                        "content": [],
                        "stop_reason": Value::Null,
                        "stop_sequence": Value::Null,
                        "usage": {
                            "input_tokens": self.input_tokens,
                            "output_tokens": 0
                        }
                    }
                }),
            );
        }
    }

    fn translate_frame(&mut self, frame: &Value, out: &mut Vec<u8>) {
        let root = frame.get("response").unwrap_or(frame);
        if let Some(usage) = root.get("usageMetadata") {
            if let Some(inp) = usage.get("promptTokenCount").and_then(Value::as_u64) {
                self.input_tokens = inp;
            }
            if let Some(outp) = usage.get("candidatesTokenCount").and_then(Value::as_u64) {
                self.output_tokens = outp;
            }
        }
        self.ensure_started(out);
        let Some(candidate) = root
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        else {
            return;
        };
        if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str)
            && reason == "MAX_TOKENS"
        {
            self.finish_reason = "max_tokens";
        }
        let Some(parts) = candidate
            .pointer("/content/parts")
            .and_then(Value::as_array)
        else {
            return;
        };
        for part in parts {
            if part.get("thought").and_then(Value::as_bool) == Some(true) {
                if let Some(text) = part.get("text").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    if self.text_block_open {
                        emit_sse(
                            out,
                            "content_block_stop",
                            &json!({ "type": "content_block_stop", "index": self.next_index }),
                        );
                        self.text_block_open = false;
                        self.next_index += 1;
                    }
                    let idx = self.next_index;
                    self.next_index += 1;
                    emit_sse(
                        out,
                        "content_block_start",
                        &json!({
                            "type": "content_block_start",
                            "index": idx,
                            "content_block": { "type": "thinking", "thinking": "" }
                        }),
                    );
                    emit_sse(
                        out,
                        "content_block_delta",
                        &json!({
                            "type": "content_block_delta",
                            "index": idx,
                            "delta": { "type": "thinking_delta", "thinking": text }
                        }),
                    );
                    emit_sse(
                        out,
                        "content_block_delta",
                        &json!({
                            "type": "content_block_delta",
                            "index": idx,
                            "delta": { "type": "signature_delta", "signature": "google-thought" }
                        }),
                    );
                    emit_sse(
                        out,
                        "content_block_stop",
                        &json!({ "type": "content_block_stop", "index": idx }),
                    );
                }
            } else if let Some(text) = part.get("text").and_then(Value::as_str) {
                if !text.is_empty() {
                    if !self.text_block_open {
                        self.text_block_open = true;
                        emit_sse(
                            out,
                            "content_block_start",
                            &json!({
                                "type": "content_block_start",
                                "index": self.next_index,
                                "content_block": { "type": "text", "text": "" }
                            }),
                        );
                    }
                    emit_sse(
                        out,
                        "content_block_delta",
                        &json!({
                            "type": "content_block_delta",
                            "index": self.next_index,
                            "delta": { "type": "text_delta", "text": text }
                        }),
                    );
                }
            } else if let Some(call) = part.get("functionCall")
                && let Some(name) = call.get("name").and_then(Value::as_str)
            {
                if self.text_block_open {
                    emit_sse(
                        out,
                        "content_block_stop",
                        &json!({ "type": "content_block_stop", "index": self.next_index }),
                    );
                    self.text_block_open = false;
                    self.next_index += 1;
                }
                let idx = self.next_index;
                self.next_index += 1;
                self.saw_tool_use = true;
                let id = format!("toolu_{}", uuid::Uuid::new_v4().simple());
                let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
                let partial_json = serde_json::to_string(&args).unwrap_or_else(|_| "{}".into());
                emit_sse(
                    out,
                    "content_block_start",
                    &json!({
                        "type": "content_block_start",
                        "index": idx,
                        "content_block": {
                            "type": "tool_use",
                            "id": id,
                            "name": name,
                            "input": {}
                        }
                    }),
                );
                emit_sse(
                    out,
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": idx,
                        "delta": {
                            "type": "input_json_delta",
                            "partial_json": partial_json
                        }
                    }),
                );
                emit_sse(
                    out,
                    "content_block_stop",
                    &json!({ "type": "content_block_stop", "index": idx }),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepares_google_v1beta_request_and_accumulates_response() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gemini-4-argon",
            "max_tokens": 1024,
            "system": "You are helpful.",
            "messages": [
                {"role": "user", "content": "Hi"}
            ],
            "tools": [
                {
                    "name": "bash",
                    "description": "Run bash",
                    "input_schema": {
                        "$schema": "http://json-schema.org/draft-07/schema#",
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {"cmd": {"type": "string"}}
                    }
                }
            ]
        }))
        .unwrap();
        let prepared = prepare_request(&req, ToolSchemas::JsonSchema).unwrap();
        assert_eq!(
            prepared["systemInstruction"]["parts"][0]["text"],
            "You are helpful."
        );
        assert_eq!(prepared["contents"][0]["parts"][0]["text"], "Hi");
        // Gemini takes standard JSON Schema in `parametersJsonSchema`, so the
        // tool's schema goes as Claude Code wrote it.
        let declaration = &prepared["tools"][0]["functionDeclarations"][0];
        assert!(declaration.get("parameters").is_none());
        assert_eq!(
            declaration["parametersJsonSchema"],
            req.extra["tools"][0]["input_schema"]
        );

        let upstream = serde_json::to_vec(&json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {"text": "let me run bash"},
                        {"functionCall": {"name": "bash", "args": {"cmd": "ls"}}}
                    ]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 12, "candidatesTokenCount": 5}
        }))
        .unwrap();
        let acc = accumulate_response(&upstream, "msg_1", "gemini-4-argon").unwrap();
        assert_eq!(acc["stop_reason"], "tool_use");
        assert_eq!(acc["usage"]["input_tokens"], 12);
        assert_eq!(acc["usage"]["output_tokens"], 5);
        assert_eq!(acc["content"][0]["text"], "let me run bash");
        assert_eq!(acc["content"][1]["name"], "bash");
    }

    fn declared_schema(schema: Value, schemas: ToolSchemas) -> Value {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gemini-4-argon",
            "messages": [{"role": "user", "content": "Hi"}],
            "tools": [{"name": "probe", "input_schema": schema}]
        }))
        .unwrap();
        let prepared = prepare_request(&req, schemas).unwrap();
        let field = match schemas {
            ToolSchemas::JsonSchema => "parametersJsonSchema",
            ToolSchemas::OpenApi => "parameters",
        };
        prepared["tools"][0]["functionDeclarations"][0][field].clone()
    }

    /// Each rewrite matches a 400 the Gemini API returned for the shape as
    /// written. Everything it accepted goes through unchanged.
    #[test]
    fn json_schema_tools_rewrite_only_what_gemini_rejects() {
        let schema = declared_schema(
            json!({
                "properties": {
                    "batch": {"type": "array", "description": "Operations"},
                    "items": {"type": ["array", "null"]},
                    "pair": {
                        "type": "array",
                        "items": [{"type": "string"}, {"type": "number"}],
                        "additionalItems": false
                    },
                    "n": {"type": "number", "minimum": 0, "exclusiveMinimum": true},
                    "m": {"type": "number", "maximum": 9, "exclusiveMaximum": false},
                    "choice": {"anyOf": [{"type": "string"}, {"type": "null"}]},
                    "settings": {"type": "object", "required": ["ghost"]},
                    "child": {"$ref": "#"},
                    "children": {"type": "array", "items": {"$ref": "#"}},
                    "picked": {"default": {"items": [1, 2]}, "enum": [{"items": [3]}]}
                },
                "required": ["batch", "child", "ghost"],
                "$defs": {
                    "Node": {
                        "type": "object",
                        "properties": {
                            "next": {"$ref": "#/$defs/Node"},
                            "parent": {"$ref": "#/$defs/Node"}
                        },
                        "required": ["next"]
                    }
                }
            }),
            ToolSchemas::JsonSchema,
        );
        let properties = &schema["properties"];
        assert_eq!(schema["type"], "object");
        // Arrays without `items` are valid JSON Schema, and Gemini takes them.
        assert_eq!(
            properties["batch"],
            json!({"type": "array", "description": "Operations"})
        );
        assert_eq!(properties["items"], json!({"type": ["array", "null"]}));
        assert_eq!(
            properties["pair"],
            json!({"type": "array", "prefixItems": [{"type": "string"}, {"type": "number"}], "items": false})
        );
        assert_eq!(
            properties["n"],
            json!({"type": "number", "exclusiveMinimum": 0})
        );
        assert_eq!(properties["m"], json!({"type": "number", "maximum": 9}));
        assert_eq!(
            properties["choice"],
            json!({"anyOf": [{"type": "string"}, {"type": "null"}]})
        );
        assert_eq!(schema["required"], json!(["batch", "child"]));
        assert_eq!(
            properties["settings"],
            json!({"type": "object", "required": []})
        );
        // A required loop is cut, an optional one or one through `items` stays.
        assert_eq!(properties["child"], json!({}));
        assert_eq!(properties["children"]["items"], json!({"$ref": "#"}));
        let node = &schema["$defs"]["Node"]["properties"];
        assert_eq!(node["next"], json!({}));
        assert_eq!(node["parent"], json!({"$ref": "#/$defs/Node"}));
        // Literal values are data, not schemas.
        assert_eq!(
            properties["picked"],
            json!({"default": {"items": [1, 2]}, "enum": [{"items": [3]}]})
        );
    }

    #[test]
    fn parameters_give_every_array_items() {
        let schema = declared_schema(
            json!({
                "type": "object",
                "properties": {
                    "batch": {"type": "array"},
                    "items": {"type": ["array", "null"]},
                    "nested": {"type": "array", "items": {"type": "array"}},
                    "tags": {"type": "array", "items": {"type": "integer"}}
                }
            }),
            ToolSchemas::OpenApi,
        );
        let properties = &schema["properties"];
        assert_eq!(
            properties["batch"],
            json!({"type": "array", "items": {"type": "string"}})
        );
        assert_eq!(
            properties["items"],
            json!({"type": "array", "nullable": true, "items": {"type": "string"}})
        );
        assert_eq!(
            properties["nested"]["items"],
            json!({"type": "array", "items": {"type": "string"}})
        );
        assert_eq!(properties["tags"]["items"], json!({"type": "integer"}));
    }

    #[test]
    fn tool_result_images_become_inline_data_instead_of_base64_text() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gemini-4-argon",
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "id": "call_1", "name": "Read", "input": {"file_path": "shot.png"}}
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "call_1",
                            "content": [
                                {"type": "text", "text": "screenshot"},
                                {
                                    "type": "image",
                                    "source": {
                                        "type": "base64",
                                        "media_type": "image/png",
                                        "data": "iVBORw0KGgo="
                                    }
                                }
                            ]
                        }
                    ]
                }
            ]
        }))
        .unwrap();
        let prepared = prepare_request(&req, ToolSchemas::JsonSchema).unwrap();
        let parts = prepared["contents"][1]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(
            parts[0]["functionResponse"]["response"]["result"],
            "screenshot\n[image]"
        );
        assert_eq!(
            parts[1],
            json!({"inlineData": {"mimeType": "image/png", "data": "iVBORw0KGgo="}})
        );
    }
}
