//! Cursor tool bridge: turns a `<tool_use>` the model writes as text into a
//! Claude Code tool call.
//!
//! The bridge keeps no state between requests. It stops the answer at the
//! first tool call with `stop_reason: "tool_use"`. Claude Code runs the tool
//! and sends the result in its next request, and `render_cursor_prompt`
//! replays the call and its result to Cursor in that request's prompt.

use std::collections::BTreeSet;

use crate::anthropic::schema::MessagesRequest;
use crate::providers::cursor::response::CursorStreamEvent;
use crate::providers::cursor::sse::CursorSseFramer;
use crate::providers::cursor::tool_use_xml::{CursorToolUseXmlParser, RecoveredCursorEvent};

// ---------------------------------------------------------------------------
// Tool detection helpers
// ---------------------------------------------------------------------------

/// Extract advertised tool names from a MessagesRequest.
pub fn advertised_tool_names(body: &MessagesRequest) -> Option<BTreeSet<String>> {
    let tools = body.extra.get("tools")?.as_array()?;
    if tools.is_empty() {
        return None;
    }
    let names: BTreeSet<String> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .map(|n| n.to_string())
        .collect();
    if names.is_empty() { None } else { Some(names) }
}

/// Whether the request can use the Cursor native tool bridge.
///
/// Returns `true` when the request is streaming, has a session id, and
/// advertises at least one of Read, Write, or Bash.
pub fn can_bridge_cursor_native_tools(body: &MessagesRequest, session_id: Option<&str>) -> bool {
    let _sid = match session_id {
        Some(id) if !id.is_empty() => id,
        _ => return false,
    };
    if !body.stream {
        return false;
    }
    let names = match advertised_tool_names(body) {
        Some(n) => n,
        None => return false,
    };
    names.contains("Read") || names.contains("Write") || names.contains("Bash")
}

// ---------------------------------------------------------------------------
// Bridge
// ---------------------------------------------------------------------------

/// Frame one Cursor response for Claude Code, stopping at the first
/// `<tool_use>` the model writes.
///
/// Cursor does not wait for a tool result. It keeps writing after the call,
/// so everything after it was written without the result and is dropped.
/// The next request gives Cursor the real result through the prompt.
pub fn start_cursor_tool_bridge(
    message_id: &str,
    model: &str,
    events: &[CursorStreamEvent],
    allowed_tool_names: Option<BTreeSet<String>>,
    id_factory: Box<dyn FnMut() -> String + Send>,
) -> Vec<u8> {
    let mut sse = Vec::new();
    let mut framer = CursorSseFramer::new(&mut sse, message_id, model);
    let mut parser = CursorToolUseXmlParser::new_with_id_factory(allowed_tool_names, id_factory);

    // Cursor reports usage once, at the end of the whole response, which the
    // loop below never reaches when it stops at a tool call. Cursor still
    // billed those tokens, and Claude Code sizes its context from them.
    if let Some(CursorStreamEvent::Usage {
        input_tokens,
        output_tokens,
        ..
    }) = events
        .iter()
        .rev()
        .find(|event| matches!(event, CursorStreamEvent::Usage { .. }))
    {
        framer.record_usage(*input_tokens, *output_tokens, 0, 0);
    }

    'events: for event in events {
        let recovered = match event {
            CursorStreamEvent::ThinkingDelta { text } => {
                framer.emit_thinking_delta(text);
                continue;
            }
            CursorStreamEvent::TextDelta { text } => parser.push(text),
            CursorStreamEvent::End => parser.flush(),
            CursorStreamEvent::Usage { .. } | CursorStreamEvent::Session { .. } => continue,
        };
        for recovered_event in recovered {
            match recovered_event {
                RecoveredCursorEvent::Text(text) => framer.emit_text_delta(&text),
                RecoveredCursorEvent::ToolUse(tool_use) => {
                    let input_json =
                        serde_json::to_string(&tool_use.input).unwrap_or_else(|_| "{}".to_string());
                    framer.emit_tool_pause(&tool_use.id, &tool_use.name, &input_json);
                    break 'events;
                }
            }
        }
        if matches!(event, CursorStreamEvent::End) {
            framer.emit_final_message("end_turn");
            break;
        }
    }

    framer.finalize();
    sse
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::schema::MessagesRequest;

    // -----------------------------------------------------------------------
    // advertised_tool_names tests
    // -----------------------------------------------------------------------

    #[test]
    fn advertised_tool_names_extracts_read_write_bash() {
        let body: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [
                {"name": "Read", "description": "read", "input_schema": {}},
                {"name": "Write", "description": "write", "input_schema": {}},
                {"name": "Bash", "description": "bash", "input_schema": {}}
            ]
        }))
        .unwrap();
        let names = advertised_tool_names(&body).unwrap();
        assert!(names.contains("Read"));
        assert!(names.contains("Write"));
        assert!(names.contains("Bash"));
    }

    #[test]
    fn advertised_tool_names_no_tools_returns_none() {
        let body: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .unwrap();
        assert!(advertised_tool_names(&body).is_none());
    }

    #[test]
    fn can_bridge_returns_true_for_stream_with_read_tool() {
        let body: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "cursor:gpt-5.5",
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "Read", "description": "read", "input_schema": {}}]
        }))
        .unwrap();
        assert!(can_bridge_cursor_native_tools(&body, Some("session-1")));
    }

    #[test]
    fn can_bridge_returns_false_for_non_streaming() {
        let body: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "cursor:gpt-5.5",
            "stream": false,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "Read", "description": "read", "input_schema": {}}]
        }))
        .unwrap();
        assert!(!can_bridge_cursor_native_tools(&body, Some("session-1")));
    }

    #[test]
    fn can_bridge_returns_false_without_session_id() {
        let body: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "cursor:gpt-5.5",
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "Read", "description": "read", "input_schema": {}}]
        }))
        .unwrap();
        assert!(!can_bridge_cursor_native_tools(&body, None));
        assert!(!can_bridge_cursor_native_tools(&body, Some("")));
    }
}
