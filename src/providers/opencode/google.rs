//! Google Gemini `v1beta` (`generateContent` / `streamGenerateContent?alt=sse`)
//! wire translator. Shared by OpenCode (`opencode`, `opencode-zen`) and the
//! native `gemini` provider.

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

pub fn prepare_request(body: &MessagesRequest) -> anyhow::Result<Value> {
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
                    let text = flatten_tool_result(&content);
                    parts.push(json!({
                        "functionResponse": {
                            "id": tool_use_id,
                            "name": name,
                            "response": {
                                "result": text,
                                "is_error": is_error.unwrap_or(false)
                            }
                        }
                    }));
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
                let mut decl = Map::new();
                decl.insert("name".to_string(), Value::String(name.to_string()));
                if let Some(desc) = tool.get("description").and_then(Value::as_str)
                    && !desc.is_empty()
                {
                    decl.insert("description".to_string(), Value::String(desc.to_string()));
                }
                if let Some(schema) = tool.get("input_schema") {
                    decl.insert("parameters".to_string(), clean_json_schema(schema));
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

fn flatten_tool_result(content: &Value) -> String {
    match content {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| {
                item.get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| serde_json::to_string(item).ok())
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Sanitizes a JSON Schema to the exact subset Google's `v1beta` and
/// Antigravity `functionDeclarations` accept (`type`, `nullable`,
/// `description`, `format`, `enum`, `properties`, `items`, `required`, `anyOf`).
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
        let prepared = prepare_request(&req).unwrap();
        assert_eq!(
            prepared["systemInstruction"]["parts"][0]["text"],
            "You are helpful."
        );
        assert_eq!(prepared["contents"][0]["parts"][0]["text"], "Hi");
        let params = &prepared["tools"][0]["functionDeclarations"][0]["parameters"];
        assert!(params.get("$schema").is_none());
        assert!(params.get("additionalProperties").is_none());

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
}
