use std::collections::HashSet;

use serde_json::{Value, json};

use crate::anthropic::schema::MessagesRequest;
use crate::providers::opencode::messages;

const IDENTITY: &str = "You are a Claude agent, built on Anthropic's Claude Agent SDK.";
const PREFIX: &str = "custom_";

pub fn prepare(
    body: &MessagesRequest,
    model: &str,
    account_id: Option<&str>,
) -> Result<Value, serde_json::Error> {
    let mut wire = messages::prepare_request(body, model)?;
    crate::anthropic::request::strip_client_system(&mut wire);
    let system = match wire.get("system") {
        Some(Value::Array(blocks)) => blocks.clone(),
        Some(Value::String(text)) => vec![json!({"type": "text", "text": text})],
        _ => vec![],
    };
    wire["system"] = Value::Array(
        std::iter::once(json!({"type": "text", "text": IDENTITY}))
            .chain(system)
            .collect(),
    );
    let mut client_names = HashSet::new();
    if let Some(tools) = wire.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            if tool
                .get("type")
                .and_then(Value::as_str)
                .is_none_or(|kind| kind == "custom")
            {
                if let Some(name) = tool.get("name").and_then(Value::as_str) {
                    client_names.insert(name.to_string());
                }
                prefix_field(tool, "name");
            }
        }
    }
    if let Some(choice) = wire.get_mut("tool_choice")
        && choice.get("type").and_then(Value::as_str) == Some("tool")
        && choice
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| client_names.contains(name))
    {
        prefix_field(choice, "name");
    }
    if let Some(messages) = wire.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            if let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) {
                prefix_blocks(content);
            }
        }
    }
    if let Some(edits) = wire
        .pointer_mut("/context_management/edits")
        .and_then(Value::as_array_mut)
    {
        for edit in edits {
            if let Some(names) = edit.get_mut("exclude_tools").and_then(Value::as_array_mut) {
                for name in names {
                    if let Some(text) = name.as_str().filter(|name| client_names.contains(*name)) {
                        *name = Value::String(format!("{PREFIX}{text}"));
                    }
                }
            }
        }
    }
    if let Some(user_id) = wire.pointer_mut("/metadata/user_id") {
        if let Some(mut parsed) = user_id
            .as_str()
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
            .filter(Value::is_object)
        {
            if let Some(id) = account_id {
                parsed["account_uuid"] = id.into();
            } else {
                parsed.as_object_mut().unwrap().remove("account_uuid");
            }
            *user_id = Value::String(parsed.to_string());
        } else {
            wire.as_object_mut().unwrap().remove("metadata");
        }
    }
    Ok(wire)
}

fn prefix_field(value: &mut Value, field: &str) {
    if let Some(name) = value.get(field).and_then(Value::as_str) {
        value[field] = Value::String(format!("{PREFIX}{name}"));
    }
}

fn prefix_blocks(blocks: &mut [Value]) {
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("tool_use") => prefix_field(block, "name"),
            Some("tool_reference") => prefix_field(block, "tool_name"),
            Some("tool_addition" | "tool_removal") => {
                if let Some(tool) = block.get_mut("tool") {
                    if let Some(definition) = tool.get_mut("definition") {
                        prefix_field(definition, "name");
                    } else {
                        prefix_field(tool, "name");
                    }
                }
            }
            _ => {}
        }
        if let Some(nested) = block.get_mut("content").and_then(Value::as_array_mut) {
            prefix_blocks(nested);
        }
    }
}

pub fn unprefix(value: &mut Value) {
    if let Some(content) = value.get_mut("content").and_then(Value::as_array_mut) {
        for block in content {
            unprefix_tool(block);
        }
    }
    if let Some(block) = value.get_mut("content_block") {
        unprefix_tool(block);
    }
}

fn unprefix_tool(block: &mut Value) {
    if block.get("type").and_then(Value::as_str) == Some("tool_use")
        && let Some(name) = block
            .get("name")
            .and_then(Value::as_str)
            .and_then(|name| name.strip_prefix(PREFIX))
    {
        block["name"] = name.into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_tools_round_trip_without_touching_server_tools_or_user_text() {
        let request: MessagesRequest = serde_json::from_value(json!({
            "model": "claude/claude-sonnet-5-5",
            "system": "You are Claude Code, Anthropic's official CLI for Claude.",
            "metadata": {"user_id": "{\"account_uuid\":\"old\",\"session_id\":\"session\"}"},
            "tools": [{"name":"custom_Read","input_schema":{"type":"object"}}, {"type":"web_search_20260209","name":"web_search"}],
            "tool_choice": {"type":"tool","name":"custom_Read"},
            "messages": [{"role":"assistant","content":[{"type":"tool_use","id":"t","name":"custom_Read","input":{"name":"not a tool name"}}]}]
        })).unwrap();
        let wire = prepare(&request, "claude-sonnet-5-5", Some("new")).unwrap();
        assert_eq!(wire["tools"][0]["name"], "custom_custom_Read");
        assert_eq!(wire["tools"][1]["name"], "web_search");
        assert_eq!(wire["tool_choice"]["name"], "custom_custom_Read");
        assert_eq!(
            wire["messages"][0]["content"][0]["name"],
            "custom_custom_Read"
        );
        assert_eq!(
            wire["messages"][0]["content"][0]["input"]["name"],
            "not a tool name"
        );
        let metadata: Value =
            serde_json::from_str(wire["metadata"]["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(
            metadata,
            json!({"account_uuid":"new","session_id":"session"})
        );
        assert_eq!(wire["system"][0]["text"], IDENTITY);
        let mut event = json!({"type":"content_block_start","content_block":{"type":"tool_use","name":"custom_custom_Read"}});
        unprefix(&mut event);
        assert_eq!(event["content_block"]["name"], "custom_Read");
        let mut server_choice = request;
        server_choice.extra.insert(
            "tool_choice".into(),
            json!({"type":"tool","name":"web_search"}),
        );
        let wire = prepare(&server_choice, "claude-sonnet-5-5", None).unwrap();
        assert_eq!(wire["tool_choice"]["name"], "web_search");
    }

    #[test]
    fn system_messages_keep_effort_and_lifetime_controls() {
        let messages = json!([
            {"role":"user","content":"summary"},
            {"role":"system","content":[],"output_config":{"effort":"xhigh"}},
            {"role":"assistant","content":"Ready."},
            {"role":"user","content":"Continue."},
            {"role":"system","content":[{"type":"text","text":"Be brief."}],"clear_at":"next_user_message"}
        ]);
        let request: MessagesRequest = serde_json::from_value(json!({
            "model": "claude/claude-sonnet-5-5",
            "messages": messages
        }))
        .unwrap();
        let wire = prepare(&request, "claude-sonnet-5-5", None).unwrap();
        assert_eq!(wire["messages"], messages);
    }
}
