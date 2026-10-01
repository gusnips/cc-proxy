//! Golden stream cases for every provider.
//!
//! Each test points one provider at an in-process mock upstream through its
//! env override, serves a hand-written reply in that provider's own wire
//! format, and reads back the Anthropic stream Claude Code would see. The mock
//! writes the body in slices that cut through frames, and through a CRLF
//! separator where the wire has one: the recent stream fixes (Cursor 8c7429f,
//! GLM 53253ff, the shared chat translator, OpenCode 95bb137) were all about a
//! stream that splits or ends badly. Ported from providerkit's golden test.
//!
//! The transcripts pin current behavior. A case whose current behavior looks
//! like a bug is `#[ignore]`d with the reason, and asserts what it should be.
//!
//! Run with `cargo test --test golden`; add `-- --ignored` for the suspected
//! bugs. No case reaches a real provider or reads real credentials: config and
//! HOME point at a temp dir, so the macOS Keychain is never consulted.

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use cc_proxy::providers::cursor::connect::{FLAG_END, encode_connect_frame};
use common::{EnvGuard, call_messages_body_with_headers, env_lock, write_auth};
use prost::Message;
use serde_json::{Value, json};
use tempfile::TempDir;

use Provider::*;

// ---------------------------------------------------------------------------
// Providers
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Provider {
    Codex,
    Kimi,
    Grok,
    Glm,
    OpenCodeChat,
    OpenCodeMessages,
    OpenCodeResponses,
    Cursor,
}

impl Provider {
    fn model(self) -> &'static str {
        match self {
            Codex => "gpt-5.5",
            Kimi => "kimi-for-coding",
            Grok => "grok-4.5",
            Glm => "glm-5.3",
            OpenCodeChat => "opencode-go/deepseek-v4-pro",
            OpenCodeMessages => "opencode-go/minimax-m3",
            OpenCodeResponses => "opencode-go/gpt-6-luna",
            Cursor => "cursor:gpt-5.5",
        }
    }

    fn fixtures(self) -> &'static str {
        match self {
            Codex => "codex",
            Kimi => "kimi",
            Grok => "grok",
            Glm => "glm",
            OpenCodeChat => "opencode-chat",
            OpenCodeMessages => "opencode-messages",
            OpenCodeResponses => "opencode-responses",
            Cursor => unreachable!("Cursor's frames are built in code"),
        }
    }

    /// The base URL each provider is given, shaped like its real one, and the
    /// path it must then call: one request, to the right route.
    fn route(self) -> (&'static str, &'static str) {
        match self {
            Codex => (
                "/backend-api/codex/responses",
                "/backend-api/codex/responses",
            ),
            Kimi => ("/coding/v1", "/coding/v1/chat/completions"),
            Grok => ("/v1", "/v1/responses"),
            Glm => ("/api/anthropic", "/api/anthropic/v1/messages"),
            OpenCodeChat => ("/zen/go/v1", "/zen/go/v1/chat/completions"),
            OpenCodeMessages => ("/zen/go/v1", "/zen/go/v1/messages"),
            OpenCodeResponses => ("/zen/go/v1", "/zen/go/v1/responses"),
            Cursor => ("", "/agent.v1.AgentService/Run"),
        }
    }

    /// Point the provider at `base` with test credentials under `config`.
    fn configure(self, config: &Path, base: &str) -> Vec<EnvGuard> {
        match self {
            Codex => {
                write_auth(config, "codex");
                vec![
                    EnvGuard::set("CCP_CODEX_BASE_URL", base),
                    EnvGuard::set("CCP_CODEX_TRANSPORT", "http"),
                ]
            }
            Kimi => {
                write_auth(config, "kimi");
                vec![EnvGuard::set("CCP_KIMI_BASE_URL", base)]
            }
            Grok => {
                write_auth(config, "grok");
                vec![EnvGuard::set("CCP_GROK_BASE_URL", base)]
            }
            Glm => vec![
                EnvGuard::set("CCP_GLM_BASE_URL", base),
                EnvGuard::set("CCP_GLM_API_KEY", "test-glm-key"),
            ],
            OpenCodeChat | OpenCodeMessages | OpenCodeResponses => vec![
                EnvGuard::set("CCP_OPENCODE_BASE_URL", base),
                EnvGuard::set("CCP_OPENCODE_API_KEY", "test-opencode-key"),
            ],
            Cursor => vec![
                EnvGuard::set("CCP_CURSOR_BASE_URL", base),
                EnvGuard::set("CCP_CURSOR_AUTH_TOKEN", "test-cursor-token"),
                // Otherwise the version is read by running `cursor-agent`.
                EnvGuard::set("CCP_CURSOR_CLIENT_VERSION", "0.0.0-golden"),
            ],
        }
    }
}

/// Settings from the developer's shell that would change what a provider
/// sends or how it answers.
const CLEARED_ENV: &[&str] = &[
    "CCP_ALIAS_PROVIDER",
    "CCP_TRAFFIC_LOG",
    "CCP_USER_AGENT",
    "CCP_CODEX_MODEL",
    "CCP_CODEX_EFFORT",
    "CCP_CODEX_PREVIOUS_RESPONSE_ID",
    "CCP_CODEX_QUOTA_STATE_FILE",
    "CCP_CODEX_REASONING_SUMMARY",
    "CCP_CODEX_SERVER_COMPACTION",
    "CCP_CODEX_RESPONSES_API",
    "CCP_GROK_HOSTED_SEARCH",
    "CCP_GROK_TOOL_IMAGE",
    "CCP_KIMI_USER_AGENT",
];

// ---------------------------------------------------------------------------
// Mock upstream
// ---------------------------------------------------------------------------

/// What the mock upstream answers.
#[derive(Clone)]
struct Reply {
    status: u16,
    content_type: &'static str,
    headers: Vec<(&'static str, &'static str)>,
    body: Vec<u8>,
    /// The socket dies after the body instead of the response ending.
    dies: bool,
}

impl Reply {
    fn new(status: u16, content_type: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type,
            headers: Vec::new(),
            body: body.into(),
            dies: false,
        }
    }

    fn error(status: u16, body: &str) -> Self {
        Self::new(status, "application/json", body)
    }

    fn header(mut self, name: &'static str, value: &'static str) -> Self {
        self.headers.push((name, value));
        self
    }

    fn dying(mut self) -> Self {
        self.dies = true;
        self
    }

    /// The same reply cut off inside its last frame.
    fn mid_frame(mut self) -> Self {
        self.body.truncate(self.body.len() - 10);
        self.dying()
    }
}

/// A 200 that streams the provider's fixture for `case`.
fn fixture(provider: Provider, case: &str) -> Reply {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/golden")
        .join(provider.fixtures())
        .join(format!("{case}.sse"));
    let body = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    // SSE allows CRLF line ends and gateways send them. One wire is served
    // that way so a separator split between its CR and LF is covered.
    let body = match provider {
        OpenCodeChat => body.replace('\n', "\r\n"),
        _ => body,
    };
    Reply::new(200, "text/event-stream", body)
}

/// Where a socket may split the body: in thirds, and inside the first CRLF
/// frame separator, between its second CR and its LF. Ported from
/// providerkit's `socketBody`.
fn socket_reads(body: &[u8]) -> Vec<Bytes> {
    let mut cuts = vec![0, body.len() / 3, body.len() * 2 / 3, body.len()];
    if let Some(separator) = body.windows(4).position(|w| w == b"\r\n\r\n") {
        cuts.push(separator + 3);
    }
    cuts.sort_unstable();
    cuts.dedup();
    cuts.windows(2)
        .map(|cut| Bytes::copy_from_slice(&body[cut[0]..cut[1]]))
        .collect()
}

struct Upstream {
    url: String,
    paths: Arc<Mutex<Vec<String>>>,
}

/// Serve `reply` to every request on a fresh local port, over HTTP/1.1 or
/// h2c (Cursor), recording each request's path.
async fn serve(reply: Reply) -> Upstream {
    let paths = Arc::new(Mutex::new(Vec::new()));
    let recorded = paths.clone();
    let app = Router::new().fallback(move |request: axum::extract::Request| {
        recorded
            .lock()
            .unwrap()
            .push(request.uri().path().to_string());
        let reply = reply.clone();
        async move { respond(reply) }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    Upstream { url, paths }
}

fn respond(reply: Reply) -> Response {
    let reads = socket_reads(&reply.body).into_iter();
    let body = futures_util::stream::unfold((reads, reply.dies), |(mut reads, dies)| async move {
        if let Some(read) = reads.next() {
            // Pending between slices, so each leaves as its own write.
            tokio::task::yield_now().await;
            return Some((Ok(read), (reads, dies)));
        }
        if !dies {
            return None;
        }
        // Let the slices already written leave before the socket dies.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let died = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "socket died");
        Some((Err(died), (reads, false)))
    });
    let mut response = Response::builder()
        .status(reply.status)
        .header(header::CONTENT_TYPE, reply.content_type);
    for (name, value) in reply.headers {
        response = response.header(name, value);
    }
    response.body(Body::from_stream(body)).unwrap()
}

// ---------------------------------------------------------------------------
// Running one case
// ---------------------------------------------------------------------------

/// One Claude Code turn: a tool, thinking on, and a session id, which is what
/// sends Cursor through its tool bridge.
fn request(model: &str) -> Value {
    json!({
        "model": model,
        "max_tokens": 4096,
        "stream": true,
        "thinking": {"type": "enabled", "budget_tokens": 1024},
        "tools": [{
            "name": "Read",
            "description": "Read a file from the local filesystem.",
            "input_schema": {
                "type": "object",
                "properties": {"file_path": {"type": "string"}},
                "required": ["file_path"]
            }
        }],
        "messages": [{"role": "user", "content": "What does notes.txt say?"}]
    })
}

/// Send the turn to `provider` against a mock serving `reply`, and return
/// what came back as a transcript.
#[allow(clippy::await_holding_lock)]
async fn run(provider: Provider, reply: Reply) -> Vec<String> {
    let _env = env_lock();
    let config = TempDir::new().unwrap();
    let mut guards: Vec<EnvGuard> = CLEARED_ENV.iter().map(|key| EnvGuard::unset(key)).collect();
    guards.push(EnvGuard::set("HOME", config.path()));
    guards.push(EnvGuard::set("CCP_CONFIG_DIR", config.path()));

    let upstream = serve(reply).await;
    let (base_path, path) = provider.route();
    guards.extend(provider.configure(config.path(), &format!("{}{base_path}", upstream.url)));

    let response = call_messages_body_with_headers(
        request(provider.model()),
        &[("x-claude-code-session-id", "golden-session")],
    )
    .await;
    let transcript = transcript(response, &upstream.url).await;
    assert_eq!(*upstream.paths.lock().unwrap(), [path], "upstream requests");
    transcript
}

// ---------------------------------------------------------------------------
// Transcript
// ---------------------------------------------------------------------------

/// The response as one line per Anthropic event, ids left out. An error
/// status is one line. The headers Claude Code acts on follow either.
async fn transcript(response: Response, upstream: &str) -> Vec<String> {
    let status = response.status();
    let headers = response.headers().clone();
    let body = tokio::time::timeout(
        Duration::from_secs(10),
        axum::body::to_bytes(response.into_body(), usize::MAX),
    )
    .await
    .expect("the response must end");
    let mut lines = match body {
        Ok(body) => {
            // A port number is not part of the answer.
            let text = String::from_utf8_lossy(&body).replace(upstream, "<upstream>");
            if status == StatusCode::OK {
                sse_lines(&text)
            } else {
                vec![error_line(status, &text)]
            }
        }
        Err(error) => vec![format!("body error: {error}")],
    };
    let mut named: Vec<_> = headers
        .iter()
        .filter(|(name, _)| {
            let name = name.as_str();
            name == "retry-after"
                || name == "x-should-retry"
                || name.starts_with("anthropic-ratelimit-")
        })
        .map(|(name, value)| format!("{name}: {}", value.to_str().unwrap_or("?")))
        .collect();
    named.sort();
    lines.extend(named);
    lines
}

fn error_line(status: StatusCode, body: &str) -> String {
    let value: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    match (
        value.pointer("/error/type").and_then(Value::as_str),
        value.pointer("/error/message").and_then(Value::as_str),
    ) {
        (Some(kind), Some(message)) => format!("HTTP {} {kind}: {message}", status.as_u16()),
        _ => format!("HTTP {} {body:?}", status.as_u16()),
    }
}

/// Parse the stream strictly: every frame ends in a blank line, its `event:`
/// names the `type` in its JSON, and nothing trails the last frame. Pings
/// carry nothing and are dropped.
fn sse_lines(text: &str) -> Vec<String> {
    let text = text.replace("\r\n", "\n");
    let mut frames: Vec<&str> = text.split("\n\n").collect();
    let trailing = frames.pop().unwrap_or_default();
    let mut lines: Vec<String> = frames
        .into_iter()
        .filter(|frame| !frame.is_empty())
        .filter_map(frame_line)
        .collect();
    if !trailing.is_empty() {
        lines.push(format!("unterminated frame: {trailing:?}"));
    }
    lines
}

fn frame_line(frame: &str) -> Option<String> {
    let malformed = || Some(format!("malformed frame: {frame:?}"));
    let mut event = None;
    let mut data = Vec::new();
    for line in frame.lines() {
        if let Some(value) = line.strip_prefix("event:") {
            event = Some(value.trim());
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.trim_start());
        } else if !line.starts_with(':') {
            return malformed();
        }
    }
    let data = data.join("\n");
    if data == "[DONE]" && event.is_none() {
        return Some("[DONE]".into());
    }
    let Ok(value) = serde_json::from_str::<Value>(&data) else {
        return malformed();
    };
    let kind = value["type"].as_str().unwrap_or_default();
    if event.is_some_and(|event| event != kind) {
        return malformed();
    }
    let index = &value["index"];
    Some(match kind {
        "ping" => return None,
        "content_block_start" => {
            let block = &value["content_block"];
            let block_type = block["type"].as_str().unwrap_or_default();
            match block["name"].as_str() {
                Some(name) => format!("content_block_start {index} {block_type} {name}"),
                None => format!("content_block_start {index} {block_type}"),
            }
        }
        "content_block_delta" => {
            let delta = &value["delta"];
            let text = |field: &str| format!("{:?}", delta[field].as_str().unwrap_or_default());
            match delta["type"].as_str().unwrap_or_default() {
                "text_delta" => format!("content_block_delta {index} text {}", text("text")),
                "thinking_delta" => {
                    format!("content_block_delta {index} thinking {}", text("thinking"))
                }
                "input_json_delta" => {
                    format!("content_block_delta {index} json {}", text("partial_json"))
                }
                // Opaque and provider-made: that it arrives is what matters.
                "signature_delta" => format!("content_block_delta {index} signature"),
                other => format!("content_block_delta {index} {other}"),
            }
        }
        "content_block_stop" => format!("content_block_stop {index}"),
        "message_delta" => format!(
            "message_delta {}",
            value["delta"]["stop_reason"].as_str().unwrap_or("null")
        ),
        "error" => format!(
            "error {}: {}",
            value["error"]["type"].as_str().unwrap_or_default(),
            value["error"]["message"].as_str().unwrap_or_default()
        ),
        other => other.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Cursor frames
// ---------------------------------------------------------------------------

// Cursor's current server message layout, the one the decoder tries first.
// The crate's `proto` module keeps an older layout that numbers these fields
// differently, so the frames are built from these.

#[derive(Clone, PartialEq, Message)]
struct ServerMessage {
    #[prost(message, optional, tag = "1")]
    interaction: Option<Interaction>,
}

#[derive(Clone, PartialEq, Message)]
struct Interaction {
    #[prost(message, optional, tag = "1")]
    text: Option<Delta>,
    #[prost(message, optional, tag = "4")]
    thinking: Option<Delta>,
    #[prost(message, optional, tag = "14")]
    turn_ended: Option<TurnEnded>,
}

#[derive(Clone, PartialEq, Message)]
struct Delta {
    #[prost(string, tag = "1")]
    text: String,
}

#[derive(Clone, PartialEq, Message)]
struct TurnEnded {
    #[prost(uint64, tag = "1")]
    input_tokens: u64,
    #[prost(uint64, tag = "2")]
    output_tokens: u64,
}

fn cursor_frame(interaction: Interaction) -> Vec<u8> {
    let message = ServerMessage {
        interaction: Some(interaction),
    };
    encode_connect_frame(message.encode_to_vec(), 0).to_vec()
}

fn text_frame(text: &str) -> Vec<u8> {
    cursor_frame(Interaction {
        text: Some(Delta { text: text.into() }),
        ..Default::default()
    })
}

fn thinking_frame(text: &str) -> Vec<u8> {
    cursor_frame(Interaction {
        thinking: Some(Delta { text: text.into() }),
        ..Default::default()
    })
}

fn turn_ended_frame(output_tokens: u64) -> Vec<u8> {
    cursor_frame(Interaction {
        turn_ended: Some(TurnEnded {
            input_tokens: 1200,
            output_tokens,
        }),
        ..Default::default()
    })
}

/// The Connect end-of-stream frame, carrying an error when there is one.
fn end_frame(error: Option<(&str, &str)>) -> Vec<u8> {
    let payload = error
        .map(|(code, message)| json!({"error": {"code": code, "message": message}}).to_string())
        .unwrap_or_default();
    encode_connect_frame(payload, FLAG_END).to_vec()
}

fn cursor_reply(frames: &[Vec<u8>]) -> Reply {
    Reply::new(200, "application/connect+proto", frames.concat())
}

// ---------------------------------------------------------------------------
// Error replies
// ---------------------------------------------------------------------------

// The vendor wordings below are plausible, not captured from the wire: each
// is one the shared classifier recognizes, so the cases pin the plumbing
// (status, type, headers) rather than a guess at the prose.

/// A spent balance or usage window, as an HTTP 429.
fn quota(provider: Provider) -> Reply {
    match provider {
        Codex => Reply::error(
            429,
            r#"{"error":{"type":"usage_limit_reached","message":"The usage limit has been reached","plan_type":"plus","resets_at":1790518773,"resets_in_seconds":518773}}"#,
        )
        .header("x-codex-primary-used-percent", "34")
        .header("x-codex-primary-window-minutes", "300")
        .header("x-codex-primary-reset-after-seconds", "9000")
        .header("x-codex-primary-reset-at", "1790009000")
        .header("x-codex-secondary-used-percent", "100")
        .header("x-codex-secondary-window-minutes", "10080")
        .header("x-codex-secondary-reset-after-seconds", "518773")
        .header("x-codex-secondary-reset-at", "1790518773"),
        Kimi => Reply::error(
            429,
            r#"{"error":{"message":"You've reached your weekly usage limit. It resets on Monday.","type":"rate_limit_reached_error"}}"#,
        )
        .header("retry-after", "3600"),
        Grok => Reply::error(
            429,
            r#"{"code":"Some resource has been exhausted","error":"You've reached your weekly usage limit for Grok. It resets in 3 days."}"#,
        )
        .header("retry-after", "3600"),
        Glm => Reply::error(
            429,
            r#"{"error":{"code":"1113","message":"Insufficient balance or no resource package. Please recharge."}}"#,
        )
        .header("retry-after", "3600"),
        OpenCodeChat | OpenCodeMessages | OpenCodeResponses => Reply::error(
            429,
            r#"{"type":"error","error":{"type":"insufficient_quota","message":"Monthly usage limit reached for OpenCode Go."}}"#,
        )
        .header("retry-after", "3600"),
        Cursor => unreachable!("Cursor's quota cases build their own reply"),
    }
}

/// A prompt that outgrew the context window.
fn context(provider: Provider) -> Reply {
    match provider {
        // Codex reports it in-band, inside a 200.
        Codex => Reply::new(
            200,
            "text/event-stream",
            concat!(
                "event: response.created\n",
                r#"data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_68dd1c7a","object":"response","created_at":1790000000,"status":"in_progress","model":"gpt-5.5","output":[]}}"#,
                "\n\n",
                "event: response.failed\n",
                r#"data: {"type":"response.failed","sequence_number":1,"response":{"id":"resp_68dd1c7a","object":"response","created_at":1790000000,"status":"failed","model":"gpt-5.5","output":[],"error":{"code":"context_length_exceeded","message":"Your input exceeds the context window of this model. Please adjust your input and try again."}}}"#,
                "\n\n",
            ),
        ),
        Kimi => Reply::error(
            400,
            r#"{"error":{"message":"Invalid request: This model's maximum context length is 262144 tokens. However, you requested 270000 tokens.","type":"invalid_request_error"}}"#,
        ),
        Grok => Reply::error(
            400,
            r#"{"code":"Client specified an invalid argument","error":"This model's maximum context length is 256000 tokens. However, your request has 270000 tokens."}"#,
        ),
        Glm => Reply::error(
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 210000 tokens > 202752 maximum"}}"#,
        ),
        OpenCodeChat | OpenCodeMessages | OpenCodeResponses => Reply::error(
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"Input is too long for requested model."}}"#,
        ),
        Cursor => unreachable!("Cursor has no context case"),
    }
}

// ---------------------------------------------------------------------------
// Transcripts several providers share
// ---------------------------------------------------------------------------

const TEXT: &[&str] = &[
    "message_start",
    "content_block_start 0 text",
    r#"content_block_delta 0 text "It says""#,
    r#"content_block_delta 0 text " hello.""#,
    "content_block_stop 0",
    "message_delta end_turn",
    "message_stop",
];

/// The arguments arrive in the two pieces the upstream sent.
const TOOL_USE: &[&str] = &[
    "message_start",
    "content_block_start 0 tool_use Read",
    r#"content_block_delta 0 json "{\"file_path\":""#,
    r#"content_block_delta 0 json "\"notes.txt\"}""#,
    "content_block_stop 0",
    "message_delta tool_use",
    "message_stop",
];

/// The arguments arrive whole, in one delta.
const TOOL_USE_WHOLE: &[&str] = &[
    "message_start",
    "content_block_start 0 tool_use Read",
    r#"content_block_delta 0 json "{\"file_path\":\"notes.txt\"}""#,
    "content_block_stop 0",
    "message_delta tool_use",
    "message_stop",
];

const THINKING: &[&str] = &[
    "message_start",
    "content_block_start 0 thinking",
    r#"content_block_delta 0 thinking "The user wants""#,
    r#"content_block_delta 0 thinking " notes.txt.""#,
    "content_block_delta 0 signature",
    "content_block_stop 0",
    "content_block_start 1 text",
    r#"content_block_delta 1 text "It says hello.""#,
    "content_block_stop 1",
    "message_delta end_turn",
    "message_stop",
];

/// A Responses reasoning summary: one delta, signed with the encrypted
/// reasoning.
const THINKING_SUMMARY: &[&str] = &[
    "message_start",
    "content_block_start 0 thinking",
    r#"content_block_delta 0 thinking "The user wants notes.txt.""#,
    "content_block_delta 0 signature",
    "content_block_stop 0",
    "content_block_start 1 text",
    r#"content_block_delta 1 text "It says hello.""#,
    "content_block_stop 1",
    "message_delta end_turn",
    "message_stop",
];

/// The shared chat translator and Grok's reducer drop what they had
/// translated from a read once an error turns up in it. "It says" shared its
/// read with the error, so only the error reaches Claude Code.
const THROTTLED: &[&str] = &["error rate_limit_error: Rate limit reached. Try again in 20s."];

/// What a relay that forwards whole frames sends when the error event comes.
const THROTTLED_AFTER_TEXT: &[&str] = &[
    "message_start",
    "content_block_start 0 text",
    r#"content_block_delta 0 text "It says""#,
    "error rate_limit_error: Rate limit reached. Try again in 20s.",
];

// ---------------------------------------------------------------------------
// Codex
// ---------------------------------------------------------------------------

#[tokio::test]
async fn codex_text() {
    assert_eq!(run(Codex, fixture(Codex, "text")).await, TEXT);
}

#[tokio::test]
async fn codex_tool_use() {
    assert_eq!(run(Codex, fixture(Codex, "tool_use")).await, TOOL_USE_WHOLE);
}

#[tokio::test]
async fn codex_thinking() {
    assert_eq!(
        run(Codex, fixture(Codex, "thinking")).await,
        THINKING_SUMMARY
    );
}

#[tokio::test]
async fn codex_error_event() {
    assert_eq!(
        run(Codex, fixture(Codex, "error_event")).await,
        [
            "message_start",
            "content_block_start 0 text",
            r#"content_block_delta 0 text "It says""#,
            "content_block_stop 0",
            "error rate_limit_error: Rate limit reached. Try again in 20s.",
        ]
    );
}

#[tokio::test]
async fn codex_cut_off() {
    assert_eq!(
        run(Codex, fixture(Codex, "cut_off").dying()).await,
        [
            "message_start",
            "content_block_start 0 text",
            r#"content_block_delta 0 text "It says""#,
            r#"content_block_delta 0 text " hello.""#,
            "content_block_stop 0",
            "error api_error: Transport error reading Codex response body: error decoding response body Codex disconnected after output started. Automatic replay stopped to avoid duplicate text or tool actions. Retry this turn.",
        ]
    );
}

#[tokio::test]
async fn codex_cut_off_mid_frame() {
    assert_eq!(
        run(Codex, fixture(Codex, "cut_off").mid_frame()).await,
        [
            "message_start",
            "content_block_start 0 text",
            r#"content_block_delta 0 text "It says""#,
            "content_block_stop 0",
            "error api_error: Transport error reading Codex response body: error decoding response body Codex disconnected after output started. Automatic replay stopped to avoid duplicate text or tool actions. Retry this turn.",
        ]
    );
}

#[tokio::test]
async fn codex_quota() {
    // No Retry-After: the window reopens in days, and clients sleep for it.
    assert_eq!(
        run(Codex, quota(Codex)).await,
        [
            "HTTP 429 rate_limit_error: The usage limit has been reached",
            "anthropic-ratelimit-unified-representative-claim: seven_day",
            "anthropic-ratelimit-unified-reset: 1790518773",
            "anthropic-ratelimit-unified-status: rejected",
            "x-should-retry: false",
        ]
    );
}

#[tokio::test]
async fn codex_context() {
    // 413, not 400: see smoke_codex_http_context_window_error_requests_compaction.
    assert_eq!(
        run(Codex, context(Codex)).await,
        [
            "HTTP 413 request_too_large: Your input exceeds the context window of this model. Please adjust your input and try again.",
            "x-should-retry: false",
        ]
    );
}

// ---------------------------------------------------------------------------
// Kimi
// ---------------------------------------------------------------------------

#[tokio::test]
async fn kimi_text() {
    assert_eq!(run(Kimi, fixture(Kimi, "text")).await, TEXT);
}

#[tokio::test]
async fn kimi_tool_use() {
    assert_eq!(run(Kimi, fixture(Kimi, "tool_use")).await, TOOL_USE);
}

#[tokio::test]
async fn kimi_thinking() {
    assert_eq!(run(Kimi, fixture(Kimi, "thinking")).await, THINKING);
}

#[tokio::test]
async fn kimi_error_event() {
    assert_eq!(run(Kimi, fixture(Kimi, "error_event")).await, THROTTLED);
}

#[tokio::test]
async fn kimi_cut_off() {
    assert_eq!(
        run(Kimi, fixture(Kimi, "cut_off").dying()).await,
        [
            "message_start",
            "content_block_start 0 text",
            r#"content_block_delta 0 text "It says""#,
            r#"content_block_delta 0 text " hello.""#,
            "error api_error: Kimi stream is invalid",
        ]
    );
}

#[tokio::test]
async fn kimi_cut_off_mid_frame() {
    assert_eq!(
        run(Kimi, fixture(Kimi, "cut_off").mid_frame()).await,
        [
            "message_start",
            "content_block_start 0 text",
            r#"content_block_delta 0 text "It says""#,
            "error api_error: Kimi stream is invalid",
        ]
    );
}

#[tokio::test]
async fn kimi_quota() {
    assert_eq!(
        run(Kimi, quota(Kimi)).await,
        [
            "HTTP 429 rate_limit_error: You've reached your weekly usage limit. It resets on Monday.",
            "retry-after: 3600",
            "x-should-retry: false",
        ]
    );
}

#[tokio::test]
async fn kimi_context() {
    assert_eq!(
        run(Kimi, context(Kimi)).await,
        [
            "HTTP 400 invalid_request_error: prompt is too long: Invalid request: This model's maximum context length is 262144 tokens. However, you requested 270000 tokens.",
        ]
    );
}

// ---------------------------------------------------------------------------
// Grok
// ---------------------------------------------------------------------------

#[tokio::test]
async fn grok_text() {
    assert_eq!(run(Grok, fixture(Grok, "text")).await, TEXT);
}

#[tokio::test]
async fn grok_tool_use() {
    assert_eq!(run(Grok, fixture(Grok, "tool_use")).await, TOOL_USE);
}

#[tokio::test]
async fn grok_thinking() {
    // Grok's reasoning comes unsigned: no encrypted reasoning to carry.
    assert_eq!(
        run(Grok, fixture(Grok, "thinking")).await,
        [
            "message_start",
            "content_block_start 0 thinking",
            r#"content_block_delta 0 thinking "The user wants notes.txt.""#,
            "content_block_stop 0",
            "content_block_start 1 text",
            r#"content_block_delta 1 text "It says hello.""#,
            "content_block_stop 1",
            "message_delta end_turn",
            "message_stop",
        ]
    );
}

#[tokio::test]
async fn grok_error_event() {
    assert_eq!(run(Grok, fixture(Grok, "error_event")).await, THROTTLED);
}

#[tokio::test]
async fn grok_cut_off() {
    assert_eq!(
        run(Grok, fixture(Grok, "cut_off").dying()).await,
        [
            "message_start",
            "content_block_start 0 text",
            r#"content_block_delta 0 text "It says""#,
            r#"content_block_delta 0 text " hello.""#,
            "error api_error: Grok stream is invalid",
        ]
    );
}

#[tokio::test]
async fn grok_cut_off_mid_frame() {
    assert_eq!(
        run(Grok, fixture(Grok, "cut_off").mid_frame()).await,
        [
            "message_start",
            "content_block_start 0 text",
            r#"content_block_delta 0 text "It says""#,
            "error api_error: Grok stream is invalid",
        ]
    );
}

#[tokio::test]
async fn grok_quota() {
    assert_eq!(
        run(Grok, quota(Grok)).await,
        [
            "HTTP 429 rate_limit_error: You've reached your weekly usage limit for Grok. It resets in 3 days.",
            "retry-after: 3600",
            "x-should-retry: false",
        ]
    );
}

#[tokio::test]
async fn grok_context() {
    assert_eq!(
        run(Grok, context(Grok)).await,
        [
            "HTTP 400 invalid_request_error: prompt is too long: This model's maximum context length is 256000 tokens. However, your request has 270000 tokens.",
        ]
    );
}

// ---------------------------------------------------------------------------
// GLM
// ---------------------------------------------------------------------------

#[tokio::test]
async fn glm_text() {
    assert_eq!(run(Glm, fixture(Glm, "text")).await, TEXT);
}

#[tokio::test]
async fn glm_tool_use() {
    assert_eq!(run(Glm, fixture(Glm, "tool_use")).await, TOOL_USE);
}

#[tokio::test]
async fn glm_thinking() {
    assert_eq!(run(Glm, fixture(Glm, "thinking")).await, THINKING);
}

#[tokio::test]
#[ignore = "suspected bug: the relay sends half of a split frame, then glues the error event onto it"]
async fn glm_error_event() {
    assert_eq!(
        run(Glm, fixture(Glm, "error_event")).await,
        THROTTLED_AFTER_TEXT
    );
}

#[tokio::test]
async fn glm_cut_off() {
    // Whole frames, then the socket dies: before 53253ff this was an empty 200.
    assert_eq!(
        run(Glm, fixture(Glm, "cut_off").dying()).await,
        [
            "message_start",
            "content_block_start 0 text",
            r#"content_block_delta 0 text "It says""#,
            r#"content_block_delta 0 text " hello.""#,
            "error api_error: GLM stopped before the answer finished. Try again.",
        ]
    );
}

#[tokio::test]
#[ignore = "suspected bug: the relay sends half of the cut frame, then glues the error event onto it"]
async fn glm_cut_off_mid_frame() {
    assert_eq!(
        run(Glm, fixture(Glm, "cut_off").mid_frame()).await,
        [
            "message_start",
            "content_block_start 0 text",
            r#"content_block_delta 0 text "It says""#,
            "error api_error: GLM stopped before the answer finished. Try again.",
        ]
    );
}

#[tokio::test]
async fn glm_quota() {
    assert_eq!(
        run(Glm, quota(Glm)).await,
        [
            "HTTP 429 rate_limit_error: Insufficient balance or no resource package. Please recharge.",
            "retry-after: 3600",
            "x-should-retry: false",
        ]
    );
}

#[tokio::test]
async fn glm_context() {
    assert_eq!(
        run(Glm, context(Glm)).await,
        ["HTTP 400 invalid_request_error: prompt is too long: 210000 tokens > 202752 maximum"]
    );
}

// ---------------------------------------------------------------------------
// OpenCode Go, chat route
// ---------------------------------------------------------------------------

#[tokio::test]
async fn opencode_chat_text() {
    assert_eq!(run(OpenCodeChat, fixture(OpenCodeChat, "text")).await, TEXT);
}

#[tokio::test]
async fn opencode_chat_tool_use() {
    assert_eq!(
        run(OpenCodeChat, fixture(OpenCodeChat, "tool_use")).await,
        TOOL_USE
    );
}

#[tokio::test]
async fn opencode_chat_thinking() {
    assert_eq!(
        run(OpenCodeChat, fixture(OpenCodeChat, "thinking")).await,
        THINKING
    );
}

#[tokio::test]
async fn opencode_chat_error_event() {
    assert_eq!(
        run(OpenCodeChat, fixture(OpenCodeChat, "error_event")).await,
        THROTTLED
    );
}

#[tokio::test]
async fn opencode_chat_cut_off() {
    assert_eq!(
        run(OpenCodeChat, fixture(OpenCodeChat, "cut_off").dying()).await,
        [
            "message_start",
            "content_block_start 0 text",
            r#"content_block_delta 0 text "It says""#,
            r#"content_block_delta 0 text " hello.""#,
            "error api_error: OpenCode Go stream is invalid",
        ]
    );
}

#[tokio::test]
async fn opencode_chat_cut_off_mid_frame() {
    assert_eq!(
        run(OpenCodeChat, fixture(OpenCodeChat, "cut_off").mid_frame()).await,
        [
            "message_start",
            "content_block_start 0 text",
            r#"content_block_delta 0 text "It says""#,
            "error api_error: OpenCode Go stream is invalid",
        ]
    );
}

#[tokio::test]
async fn opencode_chat_quota() {
    assert_eq!(run(OpenCodeChat, quota(OpenCodeChat)).await, OPENCODE_QUOTA);
}

#[tokio::test]
async fn opencode_chat_context() {
    assert_eq!(
        run(OpenCodeChat, context(OpenCodeChat)).await,
        OPENCODE_CONTEXT
    );
}

// The three OpenCode Go routes share one client, so they reject the same way.

const OPENCODE_QUOTA: &[&str] = &[
    "HTTP 429 rate_limit_error: Monthly usage limit reached for OpenCode Go.",
    "retry-after: 3600",
    "x-should-retry: false",
];

const OPENCODE_CONTEXT: &[&str] =
    &["HTTP 400 invalid_request_error: prompt is too long: Input is too long for requested model."];

// ---------------------------------------------------------------------------
// OpenCode Go, messages route
// ---------------------------------------------------------------------------

// The fixtures end with OpenAI's `data: [DONE]` after `message_stop`, as some
// gateways do. The relay skips it and forwards it as it came (95bb137).

#[tokio::test]
async fn opencode_messages_text() {
    assert_eq!(
        run(OpenCodeMessages, fixture(OpenCodeMessages, "text")).await,
        [TEXT, &["[DONE]"]].concat()
    );
}

#[tokio::test]
async fn opencode_messages_tool_use() {
    assert_eq!(
        run(OpenCodeMessages, fixture(OpenCodeMessages, "tool_use")).await,
        [TOOL_USE, &["[DONE]"]].concat()
    );
}

#[tokio::test]
async fn opencode_messages_thinking() {
    assert_eq!(
        run(OpenCodeMessages, fixture(OpenCodeMessages, "thinking")).await,
        [THINKING, &["[DONE]"]].concat()
    );
}

#[tokio::test]
#[ignore = "suspected bug: the relay sends half of a split frame, then glues the error event onto it"]
async fn opencode_messages_error_event() {
    assert_eq!(
        run(OpenCodeMessages, fixture(OpenCodeMessages, "error_event")).await,
        THROTTLED_AFTER_TEXT
    );
}

#[tokio::test]
async fn opencode_messages_cut_off() {
    assert_eq!(
        run(
            OpenCodeMessages,
            fixture(OpenCodeMessages, "cut_off").dying()
        )
        .await,
        [
            "message_start",
            "content_block_start 0 text",
            r#"content_block_delta 0 text "It says""#,
            r#"content_block_delta 0 text " hello.""#,
            "error api_error: OpenCode Go stopped before the answer finished. Try again.",
        ]
    );
}

#[tokio::test]
#[ignore = "suspected bug: the relay sends half of the cut frame, then glues the error event onto it"]
async fn opencode_messages_cut_off_mid_frame() {
    assert_eq!(
        run(
            OpenCodeMessages,
            fixture(OpenCodeMessages, "cut_off").mid_frame()
        )
        .await,
        [
            "message_start",
            "content_block_start 0 text",
            r#"content_block_delta 0 text "It says""#,
            "error api_error: OpenCode Go stopped before the answer finished. Try again.",
        ]
    );
}

#[tokio::test]
async fn opencode_messages_quota() {
    assert_eq!(
        run(OpenCodeMessages, quota(OpenCodeMessages)).await,
        OPENCODE_QUOTA
    );
}

#[tokio::test]
async fn opencode_messages_context() {
    assert_eq!(
        run(OpenCodeMessages, context(OpenCodeMessages)).await,
        OPENCODE_CONTEXT
    );
}

// ---------------------------------------------------------------------------
// OpenCode Go, responses route
// ---------------------------------------------------------------------------

#[tokio::test]
async fn opencode_responses_text() {
    assert_eq!(
        run(OpenCodeResponses, fixture(OpenCodeResponses, "text")).await,
        TEXT
    );
}

#[tokio::test]
async fn opencode_responses_tool_use() {
    assert_eq!(
        run(OpenCodeResponses, fixture(OpenCodeResponses, "tool_use")).await,
        TOOL_USE_WHOLE
    );
}

#[tokio::test]
async fn opencode_responses_thinking() {
    assert_eq!(
        run(OpenCodeResponses, fixture(OpenCodeResponses, "thinking")).await,
        THINKING_SUMMARY
    );
}

#[tokio::test]
#[ignore = "suspected bug: an in-band response.failed becomes a generic api_error, so a throttle loses its kind"]
async fn opencode_responses_error_event() {
    assert_eq!(
        run(OpenCodeResponses, fixture(OpenCodeResponses, "error_event")).await,
        [
            "message_start",
            "content_block_start 0 text",
            "content_block_stop 0",
            "error rate_limit_error: Rate limit reached. Try again in 20s.",
        ]
    );
}

#[tokio::test]
async fn opencode_responses_cut_off() {
    assert_eq!(
        run(
            OpenCodeResponses,
            fixture(OpenCodeResponses, "cut_off").dying()
        )
        .await,
        [
            "message_start",
            "content_block_start 0 text",
            r#"content_block_delta 0 text "It says""#,
            r#"content_block_delta 0 text " hello.""#,
            "content_block_stop 0",
            "error api_error: OpenCode Go Responses stream is invalid",
        ]
    );
}

#[tokio::test]
async fn opencode_responses_cut_off_mid_frame() {
    assert_eq!(
        run(
            OpenCodeResponses,
            fixture(OpenCodeResponses, "cut_off").mid_frame()
        )
        .await,
        [
            "message_start",
            "content_block_start 0 text",
            r#"content_block_delta 0 text "It says""#,
            "content_block_stop 0",
            "error api_error: OpenCode Go Responses stream is invalid",
        ]
    );
}

#[tokio::test]
async fn opencode_responses_quota() {
    assert_eq!(
        run(OpenCodeResponses, quota(OpenCodeResponses)).await,
        OPENCODE_QUOTA
    );
}

#[tokio::test]
async fn opencode_responses_context() {
    assert_eq!(
        run(OpenCodeResponses, context(OpenCodeResponses)).await,
        OPENCODE_CONTEXT
    );
}

// ---------------------------------------------------------------------------
// Cursor
// ---------------------------------------------------------------------------

// Cursor reads the whole turn before answering, so every failure is an HTTP
// status, never an error event. Context overflow is skipped: Cursor's mapping
// has no context branch, and there is no known Cursor wording to pin.

#[tokio::test]
async fn cursor_text() {
    let reply = cursor_reply(&[
        text_frame("It says"),
        text_frame(" hello."),
        turn_ended_frame(6),
        end_frame(None),
    ]);
    assert_eq!(run(Cursor, reply).await, TEXT);
}

#[tokio::test]
async fn cursor_tool_use() {
    // Cursor's model writes the call as text, and the bridge lifts it out.
    let reply = cursor_reply(&[
        text_frame(r#"<tool_use id="toolu_c1" name="Read">{"file_path":"#),
        text_frame(r#""notes.txt"}</tool_use>"#),
        turn_ended_frame(18),
        end_frame(None),
    ]);
    assert_eq!(run(Cursor, reply).await, TOOL_USE_WHOLE);
}

#[tokio::test]
async fn cursor_thinking() {
    let reply = cursor_reply(&[
        thinking_frame("The user wants"),
        thinking_frame(" notes.txt."),
        text_frame("It says hello."),
        turn_ended_frame(30),
        end_frame(None),
    ]);
    assert_eq!(
        run(Cursor, reply).await,
        [
            "message_start",
            "content_block_start 0 thinking",
            r#"content_block_delta 0 thinking "The user wants""#,
            r#"content_block_delta 0 thinking " notes.txt.""#,
            "content_block_stop 0",
            "content_block_start 1 text",
            r#"content_block_delta 1 text "It says hello.""#,
            "content_block_stop 1",
            "message_delta end_turn",
            "message_stop",
        ]
    );
}

#[tokio::test]
async fn cursor_error_event() {
    let reply = cursor_reply(&[
        text_frame("It says"),
        end_frame(Some((
            "resource_exhausted",
            "Rate limit reached. Try again in 20s.",
        ))),
    ]);
    assert_eq!(
        run(Cursor, reply).await,
        [
            "HTTP 429 rate_limit_error: Connect error 429: Rate limit reached. Try again in 20s. (resource_exhausted)"
        ]
    );
}

#[tokio::test]
async fn cursor_cut_off() {
    let reply = cursor_reply(&[text_frame("It says"), text_frame(" hello.")]).dying();
    assert_eq!(
        run(Cursor, reply).await,
        ["HTTP 502 api_error: read body: error decoding response body"]
    );
}

#[tokio::test]
async fn cursor_cut_off_mid_frame() {
    let reply = cursor_reply(&[text_frame("It says"), text_frame(" hello.")]).mid_frame();
    assert_eq!(
        run(Cursor, reply).await,
        ["HTTP 502 api_error: read body: error decoding response body"]
    );
}

/// What a spent Cursor quota should come back as, however it arrives.
const CURSOR_QUOTA: &[&str] = &[
    "HTTP 429 rate_limit_error: You've reached your monthly usage limit.",
    "x-should-retry: false",
];

#[tokio::test]
#[ignore = "suspected bug: a spent Cursor quota comes back as a throttle, so Claude Code keeps retrying"]
async fn cursor_quota() {
    let reply = cursor_reply(&[end_frame(Some((
        "resource_exhausted",
        "You've reached your monthly usage limit.",
    )))]);
    assert_eq!(run(Cursor, reply).await, CURSOR_QUOTA);
}

#[tokio::test]
#[ignore = "suspected bug: a Cursor HTTP 429 gets an invented retry-after: 5 and loses the upstream's reason"]
async fn cursor_quota_http() {
    let reply = Reply::error(
        429,
        r#"{"code":"resource_exhausted","message":"You've reached your monthly usage limit."}"#,
    );
    assert_eq!(run(Cursor, reply).await, CURSOR_QUOTA);
}
