use serde_json::Value;

use super::translate::request::{
    ResponsesContentPart, ResponsesFunctionCallOutput, ResponsesFunctionCallOutputContentPart,
    ResponsesInputItem, ResponsesRequest, ResponsesTool,
};

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CodexRequestSizeSummary {
    pub body_json_bytes: u64,
    pub instructions_bytes: u64,
    pub input_json_bytes: u64,
    pub tools_json_bytes: u64,
    pub text_json_bytes: u64,
    pub reasoning_json_bytes: u64,
    pub include_json_bytes: u64,
    pub client_metadata_json_bytes: u64,
    pub input_item_count: usize,
    pub tool_count: usize,
    pub input_image_part_count: usize,
    pub input_image_data_url_bytes: u64,
    pub input_type_counts: std::collections::BTreeMap<String, usize>,
    pub role_counts: std::collections::BTreeMap<String, usize>,
    pub largest_input_items: Vec<InputItemSummary>,
    pub largest_input_images: Vec<InputImageSummary>,
    pub largest_tools: Vec<ToolSummary>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct InputItemSummary {
    pub index: usize,
    pub r#type: String,
    pub role: Option<String>,
    pub json_bytes: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct InputImageSummary {
    pub item_index: usize,
    pub part_index: usize,
    pub json_bytes: u64,
    pub image_url_bytes: u64,
    pub data_url: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolSummary {
    pub index: usize,
    pub name: String,
    pub json_bytes: u64,
}

fn byte_length(s: &str) -> u64 {
    s.len() as u64
}

/// The Responses `type` of an input item.
fn item_kind(item: &ResponsesInputItem) -> &'static str {
    match item {
        ResponsesInputItem::AdditionalTools { .. } => "additional_tools",
        ResponsesInputItem::Message { .. } => "message",
        ResponsesInputItem::FunctionCall { .. } => "function_call",
        ResponsesInputItem::FunctionCallOutput { .. } => "function_call_output",
        ResponsesInputItem::Reasoning { .. } => "reasoning",
        ResponsesInputItem::Compaction { .. } => "compaction",
        ResponsesInputItem::CompactionTrigger => "compaction_trigger",
    }
}

/// Where one tool call and its output sit in a request's input: counts,
/// positions and item types only, so it is safe to log.
#[derive(Debug, PartialEq, Eq)]
pub struct ToolCallPairing {
    pub input_items: usize,
    pub function_calls: usize,
    pub function_call_outputs: usize,
    pub call_index: Option<usize>,
    pub output_index: Option<usize>,
    /// The types of the last items, oldest first.
    pub tail: Vec<&'static str>,
}

const PAIRING_TAIL_ITEMS: usize = 6;

pub fn tool_call_pairing(input: &[ResponsesInputItem], call_id: &str) -> ToolCallPairing {
    let mut pairing = ToolCallPairing {
        input_items: input.len(),
        function_calls: 0,
        function_call_outputs: 0,
        call_index: None,
        output_index: None,
        tail: input[input.len().saturating_sub(PAIRING_TAIL_ITEMS)..]
            .iter()
            .map(item_kind)
            .collect(),
    };
    for (index, item) in input.iter().enumerate() {
        match item {
            ResponsesInputItem::FunctionCall { call_id: id, .. } => {
                pairing.function_calls += 1;
                if id == call_id {
                    pairing.call_index.get_or_insert(index);
                }
            }
            ResponsesInputItem::FunctionCallOutput { call_id: id, .. } => {
                pairing.function_call_outputs += 1;
                if id == call_id {
                    pairing.output_index.get_or_insert(index);
                }
            }
            _ => {}
        }
    }
    pairing
}

fn json_bytes(value: Option<&Value>) -> u64 {
    match value {
        Some(v) => byte_length(&serde_json::to_string(v).unwrap_or_default()),
        None => 0,
    }
}

fn input_image_parts(input: &[ResponsesInputItem]) -> Vec<(usize, usize, &str)> {
    let mut parts = Vec::new();
    for (item_idx, item) in input.iter().enumerate() {
        match item {
            ResponsesInputItem::Message { content, .. } => {
                for (part_idx, part) in content.iter().enumerate() {
                    if let ResponsesContentPart::InputImage { image_url, .. } = part {
                        parts.push((item_idx, part_idx, image_url.as_str()));
                    }
                }
            }
            ResponsesInputItem::FunctionCallOutput {
                output: ResponsesFunctionCallOutput::ContentItems(content),
                ..
            } => {
                for (part_idx, part) in content.iter().enumerate() {
                    if let ResponsesFunctionCallOutputContentPart::InputImage {
                        image_url, ..
                    } = part
                    {
                        parts.push((item_idx, part_idx, image_url.as_str()));
                    }
                }
            }
            _ => {}
        }
    }
    parts
}

pub fn summarize_codex_request_size(body: &ResponsesRequest) -> CodexRequestSizeSummary {
    let body_json = serde_json::to_string(body).unwrap_or_default();
    let image_parts = input_image_parts(&body.input);

    let input_type_counts = count_items_by(&body.input, |item| Some(item_kind(item).to_string()));

    let role_counts = count_items_by(&body.input, |item| match item {
        ResponsesInputItem::AdditionalTools { role, .. } => Some(role.clone()),
        ResponsesInputItem::Message { role, .. } => Some(role.clone()),
        _ => None,
    });

    let largest_input_items = {
        let mut items: Vec<InputItemSummary> = body
            .input
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let role = match item {
                    ResponsesInputItem::AdditionalTools { role, .. }
                    | ResponsesInputItem::Message { role, .. } => Some(role.clone()),
                    _ => None,
                };
                let json_bytes_val =
                    json_bytes(Some(&serde_json::to_value(item).unwrap_or_default()));
                InputItemSummary {
                    index: i,
                    r#type: item_kind(item).to_string(),
                    role,
                    json_bytes: json_bytes_val,
                }
            })
            .collect();
        items.sort_by_key(|item| std::cmp::Reverse(item.json_bytes));
        items.truncate(5);
        items
    };

    let largest_input_images = {
        let mut items: Vec<InputImageSummary> = image_parts
            .iter()
            .map(|&(item_idx, part_idx, url)| {
                let json_bytes_val = json_bytes(Some(&serde_json::json!({
                    "type": "input_image",
                    "image_url": url,
                })));
                InputImageSummary {
                    item_index: item_idx,
                    part_index: part_idx,
                    json_bytes: json_bytes_val,
                    image_url_bytes: byte_length(url),
                    data_url: url.starts_with("data:"),
                }
            })
            .collect();
        items.sort_by_key(|item| std::cmp::Reverse(item.image_url_bytes));
        items.truncate(5);
        items
    };

    let largest_tools = {
        let mut items: Vec<ToolSummary> = Vec::new();
        if let Some(ref tools) = body.tools {
            for (i, tool) in tools.iter().enumerate() {
                let name = match tool {
                    ResponsesTool::Function(f) => f.name.clone(),
                    ResponsesTool::WebSearch(_) => "web_search".to_string(),
                };
                let json_bytes_val =
                    json_bytes(Some(&serde_json::to_value(tool).unwrap_or_default()));
                items.push(ToolSummary {
                    index: i,
                    name,
                    json_bytes: json_bytes_val,
                });
            }
        }
        items.sort_by_key(|item| std::cmp::Reverse(item.json_bytes));
        items.truncate(5);
        items
    };

    CodexRequestSizeSummary {
        body_json_bytes: byte_length(&body_json),
        instructions_bytes: body.instructions.as_ref().map_or(0, |s| byte_length(s)),
        input_json_bytes: json_bytes(Some(&serde_json::to_value(&body.input).unwrap_or_default())),
        tools_json_bytes: match &body.tools {
            Some(tools) => json_bytes(Some(&serde_json::to_value(tools).unwrap_or_default())),
            None => 0,
        },
        text_json_bytes: json_bytes(Some(&serde_json::to_value(&body.text).unwrap_or_default())),
        reasoning_json_bytes: json_bytes(
            body.reasoning
                .as_ref()
                .map(|r| serde_json::to_value(r).unwrap_or_default())
                .as_ref(),
        ),
        include_json_bytes: json_bytes(
            body.include
                .as_ref()
                .map(|i| serde_json::to_value(i).unwrap_or_default())
                .as_ref(),
        ),
        client_metadata_json_bytes: json_bytes(
            body.client_metadata
                .as_ref()
                .map(|m| serde_json::to_value(m).unwrap_or_default())
                .as_ref(),
        ),
        input_item_count: body.input.len(),
        tool_count: body.tools.as_ref().map_or(0, |t| t.len()),
        input_image_part_count: image_parts.len(),
        input_image_data_url_bytes: image_parts
            .iter()
            .filter(|(_, _, url)| url.starts_with("data:"))
            .map(|(_, _, url)| byte_length(url))
            .sum(),
        input_type_counts,
        role_counts,
        largest_input_items,
        largest_input_images,
        largest_tools,
    }
}

fn count_items_by<T, F>(items: &[T], f: F) -> std::collections::BTreeMap<String, usize>
where
    F: Fn(&T) -> Option<String>,
{
    let mut counts = std::collections::BTreeMap::new();
    for item in items {
        if let Some(key) = f(item) {
            *counts.entry(key).or_insert(0) += 1;
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(id: &str) -> ResponsesInputItem {
        ResponsesInputItem::FunctionCall {
            call_id: id.to_string(),
            name: "Bash".to_string(),
            arguments: "{}".to_string(),
        }
    }

    fn output(id: &str) -> ResponsesInputItem {
        ResponsesInputItem::FunctionCallOutput {
            call_id: id.to_string(),
            output: ResponsesFunctionCallOutput::Text("ok".to_string()),
        }
    }

    #[test]
    fn pairing_reports_positions_counts_and_tail_without_text() {
        let input = vec![call("a"), output("a"), call("b"), call("c"), output("b")];
        let pairing = tool_call_pairing(&input, "b");
        assert_eq!(
            pairing,
            ToolCallPairing {
                input_items: 5,
                function_calls: 3,
                function_call_outputs: 2,
                call_index: Some(2),
                output_index: Some(4),
                tail: vec![
                    "function_call",
                    "function_call_output",
                    "function_call",
                    "function_call",
                    "function_call_output",
                ],
            }
        );
    }

    #[test]
    fn pairing_shows_an_orphan_and_keeps_only_the_last_items() {
        let mut input: Vec<_> = (0..10).map(|i| call(&format!("old{i}"))).collect();
        input.push(output("lost"));
        let pairing = tool_call_pairing(&input, "lost");
        assert_eq!(pairing.call_index, None);
        assert_eq!(pairing.output_index, Some(10));
        assert_eq!(pairing.tail.len(), PAIRING_TAIL_ITEMS);
        assert_eq!(pairing.tail.last(), Some(&"function_call_output"));
    }

    #[test]
    fn summarize_simple_request() {
        let input = vec![ResponsesInputItem::Message {
            role: "user".to_string(),
            content: vec![ResponsesContentPart::InputText {
                text: "hello".to_string(),
            }],
        }];
        let req = ResponsesRequest {
            model: "gpt-5.5".to_string(),
            instructions: None,
            input,
            tools: None,
            tool_choice: None,
            store: false,
            stream: true,
            parallel_tool_calls: true,
            include: None,
            client_metadata: None,
            service_tier: None,
            prompt_cache_key: None,
            text: super::super::translate::request::ResponsesText {
                verbosity: Some("low".to_string()),
                format: None,
            },
            reasoning: None,
        };
        let summary = summarize_codex_request_size(&req);
        assert_eq!(summary.input_item_count, 1);
        assert_eq!(summary.tool_count, 0);
        assert!(summary.body_json_bytes > 0);
    }

    #[test]
    fn summarize_with_tools_and_images() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "describe"},
                        {"type": "input_image", "image_url": "data:image/png;base64,abc"}
                    ]
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": [
                        {"type": "input_text", "text": "tool image"},
                        {"type": "input_image", "image_url": "data:image/jpeg;base64,def"}
                    ]
                }
            ],
            "store": false,
            "stream": true,
            "parallel_tool_calls": true,
            "text": {"verbosity": "low"}
        }))
        .unwrap();
        let summary = summarize_codex_request_size(&req);
        assert_eq!(summary.input_image_part_count, 2);
        assert!(summary.input_image_data_url_bytes > 0);
    }
}
