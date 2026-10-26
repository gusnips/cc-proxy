use serde_json::Value;

const CLIENT_IDENTITIES: &[&str] = &[
    "You are Claude Code, Anthropic's official CLI for Claude.",
    "You are Claude Code, Anthropic's official CLI for Claude, running within the Claude Agent SDK.",
    "You are a Claude agent, built on Anthropic's Claude Agent SDK.",
];

/// Server-side advisor tools require the native subscription endpoint.
pub(crate) fn prepare_for_provider(body: &mut super::MessagesRequest, provider: &str) {
    if provider != "claude"
        && let Some(Value::Array(tools)) = body.extra.get_mut("tools")
    {
        tools.retain(|tool| {
            !tool
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.starts_with("advisor"))
        });
    }
}

/// API keys must not carry the native client's OAuth account or billing identity.
pub fn prepare_api_key_request(body: &mut Value) {
    if let Some(body) = body.as_object_mut() {
        body.remove("metadata");
    }
    strip_client_system(body);
    strip_proxy_thinking(body);
}

/// Drops thinking blocks that this proxy signed (`ccp:<provider>:...`). They
/// come from a Codex, Kimi or OpenCode turn that ran before a fallback, and
/// Anthropic answers 400 "Invalid `signature` in `thinking` block" to them.
/// A message left with no blocks goes too. A message that was empty to begin
/// with stays, because Claude Code sends effort changes as empty system messages.
pub(crate) fn strip_proxy_thinking(body: &mut Value) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    messages.retain_mut(|message| {
        let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
            return true;
        };
        let before = blocks.len();
        blocks.retain(|block| {
            !(block.get("type").and_then(Value::as_str) == Some("thinking")
                && block
                    .get("signature")
                    .and_then(Value::as_str)
                    .is_some_and(|signature| signature.starts_with("ccp:")))
        });
        before == 0 || !blocks.is_empty()
    });
}

pub(crate) fn strip_client_system(body: &mut Value) {
    let Some(system) = body.get_mut("system") else {
        return;
    };
    match system {
        Value::String(text) => *text = without_client_lines(text),
        Value::Array(blocks) => {
            blocks.retain_mut(|block| {
                let Some(text) = block.get("text").and_then(Value::as_str) else {
                    return true;
                };
                let cleaned = without_client_lines(text);
                if cleaned.trim().is_empty() {
                    return false;
                }
                block["text"] = Value::String(cleaned);
                true
            });
        }
        _ => return,
    }
    if system.as_str().is_some_and(|text| text.trim().is_empty())
        || system.as_array().is_some_and(Vec::is_empty)
    {
        body.as_object_mut().unwrap().remove("system");
    }
}

fn without_client_lines(text: &str) -> String {
    text.split_inclusive('\n')
        .filter(|line| {
            let line = line.trim();
            !line.starts_with("x-anthropic-billing-header:") && !CLIENT_IDENTITIES.contains(&line)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn advisor_tools_follow_each_provider_attempt_not_the_original_route() {
        let body: super::super::MessagesRequest = serde_json::from_value(json!({
            "tools": [{"type":"advisor_20260301","name":"advisor"}, {"name":"Read","input_schema":{"type":"object"}}]
        })).unwrap();
        let mut native = body.clone();
        prepare_for_provider(&mut native, "claude");
        assert_eq!(native.extra["tools"].as_array().unwrap().len(), 2);
        for provider in ["anthropic", "codex", "antigravity"] {
            let mut attempt = body.clone();
            prepare_for_provider(&mut attempt, provider);
            assert_eq!(attempt.extra["tools"].as_array().unwrap().len(), 1);
            assert_eq!(attempt.extra["tools"][0]["name"], "Read");
        }
    }

    #[test]
    fn api_key_requests_keep_instructions_and_cache_markers_not_cli_identity() {
        let mut body = json!({
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1; cch=abc;"},
                {"type": "text", "text": CLIENT_IDENTITIES[0]},
                {"type": "text", "text": "Follow repository rules.", "cache_control": {"type": "ephemeral"}}
            ],
            "metadata": {"user_id": "native-account"},
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "Read"}]
        });
        prepare_api_key_request(&mut body);
        assert!(body.get("metadata").is_none());
        assert_eq!(
            body["system"],
            json!([
                {"type": "text", "text": "Follow repository rules.", "cache_control": {"type": "ephemeral"}}
            ])
        );
        assert_eq!(body["tools"][0]["name"], "Read");
        assert_eq!(body["messages"][0]["content"], "hi");
    }

    #[test]
    fn proxy_signed_thinking_is_dropped_and_anthropic_signed_thinking_stays() {
        let mut body = json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "", "signature": "ccp:codex:v2:abc"},
                {"type": "thinking", "thinking": "kept", "signature": "EqQBCkYI"},
                {"type": "text", "text": "Hello"}
            ]},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "", "signature": "ccp:kimi:v1:m:0"}
            ]},
            {"role": "system", "content": [], "output_config": {"effort": "xhigh"}},
            {"role": "user", "content": [{"type": "text", "text": "go"}]}
        ]});
        strip_proxy_thinking(&mut body);
        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "kept", "signature": "EqQBCkYI"},
                    {"type": "text", "text": "Hello"}
                ]},
                {"role": "system", "content": [], "output_config": {"effort": "xhigh"}},
                {"role": "user", "content": [{"type": "text", "text": "go"}]}
            ])
        );
    }

    #[test]
    fn combined_system_text_keeps_user_instructions_and_empty_system_is_removed() {
        let mut body = json!({"system": format!(
            "x-anthropic-billing-header: cch=abc;\n{}\nKeep this line.\n", CLIENT_IDENTITIES[0]
        )});
        prepare_api_key_request(&mut body);
        assert_eq!(body["system"], "Keep this line.\n");
        let mut body = json!({"system": CLIENT_IDENTITIES[0]});
        prepare_api_key_request(&mut body);
        assert!(body.get("system").is_none());
    }
}
