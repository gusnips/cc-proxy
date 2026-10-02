use base64::Engine;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use prost::Message;
use tokio::sync::mpsc;

use crate::config;
use crate::providers::cursor::connect::{
    ConnectFrame, ConnectFrameDecoder, FLAG_END, FLAG_GZIP, encode_connect_frame,
    parse_connect_error,
};
use crate::providers::cursor::model::CursorModelResolution;
use crate::providers::cursor::proto;
use crate::providers::cursor::request::CursorSelectedImage;
use crate::providers::cursor::response::frame_ends_turn;

/// How long Cursor may send nothing at all before the request fails.
///
/// A gap is not an answer. Only Cursor's `turn_ended` update or the Connect
/// end frame says the turn is over. Failing a stalled stream lets Claude Code
/// retry, where reading the gap as the end handed it a cut-off answer that
/// claimed to be finished.
const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Upstream response from the Cursor API.
///
/// Contains the raw response bytes (or body bytes for streaming) and the
/// HTTP status.
pub struct CursorUpstreamResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub error_detail: Option<String>,
}

impl CursorUpstreamResponse {
    pub fn is_success(&self) -> bool {
        self.status >= 200 && self.status < 300
    }
}

/// HTTP/2 client for the Cursor AgentService/Run endpoint.
pub struct CursorHttpClient {
    client: reqwest::Client,
    base_url: String,
}

impl Default for CursorHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorHttpClient {
    pub fn new() -> Self {
        // Use HTTP/2 prior knowledge for cleartext URLs (mock testing) and
        // standard TLS for https URLs.
        let base_url = config::cursor_base_url();
        let is_cleartext = base_url.starts_with("http://");

        let mut builder = reqwest::Client::builder()
            .http2_keep_alive_timeout(std::time::Duration::from_secs(30))
            .http2_keep_alive_while_idle(true);

        if is_cleartext {
            builder = builder.http2_prior_knowledge();
        }

        let client = builder.build().expect("CursorHttpClient: reqwest client");

        Self { client, base_url }
    }

    /// Run the Cursor agent with the given prompt and token.
    ///
    /// Opens a bidirectional Connect stream, sends the agent request frames,
    /// and keeps the request body alive while collecting the response frames.
    pub async fn run_agent(
        &self,
        token: &str,
        prompt: &str,
        model: &str,
        images: &[CursorSelectedImage],
    ) -> Result<CursorUpstreamResponse, CursorError> {
        let resolved = super::model::resolve_cursor_model(model)
            .map_err(|e| CursorError::internal(format!("model resolution: {e}")))?;

        let request_id = uuid::Uuid::new_v4().to_string();
        let frames = build_run_frames(prompt, &resolved, images, &request_id);
        let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(8);
        let sender = tokio::spawn(async move {
            for (index, frame) in frames.into_iter().enumerate() {
                if tx.send(Ok(frame)).await.is_err() {
                    return;
                }
                let delay = match index {
                    0 => std::time::Duration::from_millis(1500),
                    1 => std::time::Duration::from_millis(800),
                    _ => std::time::Duration::from_millis(400),
                };
                tokio::time::sleep(delay).await;
            }

            let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(5));
            heartbeat.tick().await;
            loop {
                heartbeat.tick().await;
                if tx.send(Ok(heartbeat_frame())).await.is_err() {
                    return;
                }
            }
        });
        let body =
            reqwest::Body::wrap_stream(futures_util::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|item| (item, rx))
            }));

        let url = format!(
            "{}/agent.v1.AgentService/Run",
            self.base_url.trim_end_matches('/')
        );
        let client_version = config::cursor_client_version();

        let response = self
            .client
            .post(&url)
            .bearer_auth(token)
            .header("content-type", "application/connect+proto")
            .header("connect-protocol-version", "1")
            .header("connect-accept-encoding", "gzip,br")
            .header("user-agent", "connect-es/1.6.1")
            .header("x-cursor-client-type", "cli")
            .header("x-cursor-client-version", &client_version)
            .header("x-ghost-mode", "true")
            .header("x-request-id", &request_id)
            .header("x-original-request-id", &request_id)
            .header("x-cursor-streaming", "true")
            .header("te", "trailers")
            .body(body)
            .send()
            .await
            .map_err(CursorError::from_reqwest)?;

        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let error_detail = response
            .headers()
            .get("grpc-message")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let collected = collect_turn(response.bytes_stream(), IDLE_TIMEOUT).await;
        sender.abort();
        let (body_bytes, finished) = collected?;

        if status >= 400 {
            let detail = parse_error_body(&body_bytes, &headers);
            let mut error = CursorError::new(
                status,
                format!("Cursor upstream returned HTTP {status}"),
                detail,
            );
            error.retry_after = headers
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            return Err(error);
        }
        if !finished {
            return Err(CursorError::internal(
                "Cursor closed the connection before the answer was finished. Retry this turn.",
            ));
        }

        Ok(CursorUpstreamResponse {
            status,
            body: body_bytes,
            error_detail,
        })
    }
}

fn encode_varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push(((value as u8) & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn field_bytes(field: u64, value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len() + 4);
    encode_varint((field << 3) | 2, &mut out);
    encode_varint(value.len() as u64, &mut out);
    out.extend_from_slice(value);
    out
}

fn field_string(field: u64, value: &str) -> Vec<u8> {
    field_bytes(field, value.as_bytes())
}

fn field_varint(field: u64, value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    encode_varint(field << 3, &mut out);
    encode_varint(value, &mut out);
    out
}

fn model_message(model: &str, fast: bool) -> Vec<u8> {
    let mut out = field_string(1, model);
    let mut parameter = field_string(1, "fast");
    parameter.extend(field_string(2, if fast { "true" } else { "false" }));
    out.extend(field_bytes(3, &parameter));
    out
}

fn mode_value(resolved: &CursorModelResolution) -> u64 {
    match resolved.mode {
        super::model::CursorAgentMode::Agent => 1,
        super::model::CursorAgentMode::Ask => 2,
        super::model::CursorAgentMode::Plan => 3,
    }
}

fn selected_context(images: &[CursorSelectedImage]) -> Option<Vec<u8>> {
    if images.is_empty() {
        return None;
    }

    let mut context = Vec::new();
    for image in images {
        let data = base64::engine::general_purpose::STANDARD
            .decode(&image.data)
            .unwrap_or_default();
        let mut selected = field_string(2, &image.uuid);
        selected.extend(field_string(3, &image.path));
        selected.extend(field_string(7, &image.mime_type));
        selected.extend(field_bytes(8, &data));
        context.extend(field_bytes(1, &selected));
    }
    Some(context)
}

fn build_run_frames(
    prompt: &str,
    resolved: &CursorModelResolution,
    images: &[CursorSelectedImage],
    request_id: &str,
) -> Vec<Bytes> {
    let conversation_id = uuid::Uuid::new_v4().to_string();
    let mut user_message = field_string(1, prompt);
    user_message.extend(field_string(2, request_id));
    if let Some(context) = selected_context(images) {
        user_message.extend(field_bytes(3, &context));
    } else {
        user_message.extend(field_bytes(3, &[]));
    }
    user_message.extend(field_varint(4, mode_value(resolved)));

    let action = field_bytes(1, &field_bytes(1, &user_message));
    let mut request = field_bytes(1, &[]);
    request.extend(field_bytes(2, &action));
    request.extend(field_bytes(4, &[]));
    request.extend(field_string(5, &conversation_id));
    request.extend(field_bytes(
        9,
        &model_message(&resolved.model_id, resolved.fast),
    ));
    request.extend(field_varint(12, 0));
    request.extend(field_bytes(14, &field_string(1, "default")));
    request.extend(field_bytes(
        14,
        &model_message(&resolved.model_id, resolved.fast),
    ));
    request.extend(field_string(16, &conversation_id));
    let first = encode_connect_frame(field_bytes(1, &request), 0);

    let cwd = std::env::current_dir()
        .ok()
        .and_then(|path| path.to_str().map(str::to_string))
        .unwrap_or_default();
    let mut environment = field_string(1, std::env::consts::OS);
    environment.extend(field_string(2, &cwd));
    environment.extend(field_string(
        3,
        if cfg!(windows) { "powershell" } else { "bash" },
    ));
    environment.extend(field_string(10, "UTC"));
    environment.extend(field_string(11, &cwd));
    environment.extend(field_varint(14, 1));
    environment.extend(field_varint(16, 1));
    environment.extend(field_varint(19, 0));
    environment.extend(field_varint(20, 0));
    environment.extend(field_string(21, &cwd));
    environment.extend(field_varint(22, 0));
    let context = field_bytes(
        2,
        &field_bytes(
            10,
            &field_bytes(1, &field_bytes(1, &field_bytes(4, &environment))),
        ),
    );

    let mut frames = vec![first, encode_connect_frame(context, 0)];
    frames.push(encode_connect_frame(
        field_bytes(5, &field_string(1, "")),
        0,
    ));
    frames.push(encode_connect_frame(
        field_bytes(3, &field_string(3, "")),
        0,
    ));
    for sequence in 1..=8 {
        let mut marker = field_varint(1, sequence);
        marker.extend(field_string(3, ""));
        frames.push(encode_connect_frame(field_bytes(3, &marker), 0));
    }
    frames
}

fn heartbeat_frame() -> Bytes {
    encode_connect_frame(field_bytes(7, &[]), 0)
}

/// Read Cursor's response until its turn ends.
///
/// Returns the bytes and whether the turn finished. The stream closing first
/// is not a finish. Neither is a pause: Cursor can go quiet mid-answer for a
/// long think, so only `idle_timeout` with no bytes at all fails the read.
async fn collect_turn<E: std::fmt::Display>(
    stream: impl Stream<Item = Result<Bytes, E>>,
    idle_timeout: std::time::Duration,
) -> Result<(Vec<u8>, bool), CursorError> {
    let mut stream = std::pin::pin!(stream);
    let mut body = Vec::new();
    let mut decoder = ConnectFrameDecoder::new();
    loop {
        match tokio::time::timeout(idle_timeout, stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                body.extend_from_slice(&chunk);
                // `turn_ended` ends the read as well as the end frame: our
                // side of the stream stays open with heartbeats, so the end
                // frame may not come until it closes. A body that is not
                // Connect frames (an HTTP error page) never matches and is
                // read to the end.
                if decoder
                    .push(&chunk)
                    .is_ok_and(|frames| frames.iter().any(frame_ends_turn))
                {
                    return Ok((body, true));
                }
            }
            Ok(Some(Err(error))) => {
                return Err(CursorError::internal(format!("read body: {error}")));
            }
            Ok(None) => return Ok((body, false)),
            Err(_) => {
                return Err(CursorError::internal(format!(
                    "Cursor sent nothing for {} seconds, so the request was stopped. Retry this turn.",
                    idle_timeout.as_secs()
                )));
            }
        }
    }
}

fn parse_error_body(body_bytes: &[u8], _headers: &reqwest::header::HeaderMap) -> Option<String> {
    if body_bytes.len() < 5 {
        return None;
    }
    // Try to parse as Connect end frame with JSON error
    if body_bytes.len() >= 5 {
        let flags = body_bytes[0];
        let len = u32::from_be_bytes([body_bytes[1], body_bytes[2], body_bytes[3], body_bytes[4]])
            as usize;
        if flags & FLAG_END != 0 && body_bytes.len() >= 5 + len {
            let payload = &body_bytes[5..5 + len];
            let err = parse_connect_error(payload);
            if err.is_some() {
                return err.map(|e| e.detail);
            }
        }
    }

    // Try plain text error
    if let Ok(text) = String::from_utf8(body_bytes.to_vec())
        && !text.is_empty()
    {
        return Some(text);
    }
    None
}

/// Decode upstream response bytes into Connect frames containing
/// AgentServerMessage values.
pub fn decode_upstream_frames(body: &[u8]) -> Result<Vec<ConnectFrame>, CursorError> {
    let mut decoder = ConnectFrameDecoder::new();
    let frames = decoder
        .push(body)
        .map_err(|e| CursorError::internal(format!("frame decode: {e}")))?;
    Ok(frames)
}

/// Decode a single Connect frame payload into an AgentServerMessage.
/// Handles gzip decompression if the FLAG_GZIP bit is set.
pub fn decode_frame_payload(
    frame: &ConnectFrame,
) -> Result<proto::AgentServerMessage, CursorError> {
    let payload = if frame.flags & FLAG_GZIP != 0 {
        super::connect::decode_gzip_frame(&frame.payload)
            .map_err(|e| CursorError::internal(format!("gzip decompress: {e}")))?
    } else {
        frame.payload.to_vec()
    };

    proto::AgentServerMessage::decode(&payload[..])
        .map_err(|e| CursorError::internal(format!("prost decode: {e}")))
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CursorError {
    pub status: u16,
    pub message: String,
    pub detail: Option<String>,
    pub retry_after: Option<String>,
}

impl CursorError {
    pub fn new(status: u16, message: impl Into<String>, detail: Option<String>) -> Self {
        Self {
            status,
            message: message.into(),
            detail,
            retry_after: None,
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            status: 502,
            message: message.into(),
            detail: None,
            retry_after: None,
        }
    }

    pub fn from_reqwest(e: reqwest::Error) -> Self {
        let status = e.status().map(|s| s.as_u16()).unwrap_or(502);
        Self {
            status,
            message: e.to_string(),
            detail: None,
            retry_after: None,
        }
    }
}

impl std::fmt::Display for CursorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cursor error {}: {}", self.status, self.message)
    }
}

impl std::error::Error for CursorError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::cursor::response::{CursorStreamEvent, decode_upstream_response};
    use crate::providers::cursor::test_frames;
    use futures_util::stream;

    fn chunks(frames: Vec<Vec<u8>>) -> impl Stream<Item = Result<Bytes, std::io::Error>> {
        stream::iter(frames.into_iter().map(|frame| Ok(Bytes::from(frame))))
    }

    #[tokio::test(start_paused = true)]
    async fn turn_ended_finishes_the_read_while_the_stream_stays_open() {
        let stream = chunks(vec![
            test_frames::text_frame("hi"),
            test_frames::usage_frame(1, 1),
        ])
        .chain(stream::pending());
        let started = tokio::time::Instant::now();

        let (_, finished) = collect_turn(stream, IDLE_TIMEOUT).await.unwrap();

        assert!(finished);
        assert_eq!(started.elapsed(), std::time::Duration::ZERO);
    }

    #[tokio::test]
    async fn a_stream_that_closes_mid_answer_is_not_finished() {
        let stream = chunks(vec![test_frames::text_frame("half an ans")]);

        let (_, finished) = collect_turn(stream, IDLE_TIMEOUT).await.unwrap();

        assert!(!finished);
    }

    #[tokio::test(start_paused = true)]
    async fn long_pauses_between_chunks_are_not_the_end() {
        // A 45-second think between deltas used to read as a finished answer
        // after 5 seconds.
        let stream = chunks(vec![
            test_frames::text_frame("first"),
            test_frames::text_frame(" second"),
            test_frames::usage_frame(1, 1),
        ])
        .then(|chunk| async {
            tokio::time::sleep(std::time::Duration::from_secs(45)).await;
            chunk
        });

        let (body, finished) = collect_turn(stream, IDLE_TIMEOUT).await.unwrap();

        assert!(finished);
        let text: String = decode_upstream_response(&body)
            .unwrap()
            .into_iter()
            .filter_map(|event| match event {
                CursorStreamEvent::TextDelta { text } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(text, "first second");
    }

    #[tokio::test(start_paused = true)]
    async fn silence_for_the_idle_timeout_fails_the_read() {
        let stream = chunks(vec![test_frames::text_frame("thinking")]).chain(stream::pending());
        let started = tokio::time::Instant::now();

        let error = collect_turn(stream, IDLE_TIMEOUT).await.unwrap_err();

        assert_eq!(started.elapsed(), IDLE_TIMEOUT);
        assert_eq!(error.status, 502);
        assert!(error.message.contains("60 seconds"), "{}", error.message);
    }
}
