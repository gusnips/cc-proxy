use std::convert::Infallible;
use std::sync::Arc;

use axum::body::Body;
use bytes::Bytes;
use futures_util::StreamExt;

use crate::anthropic::schema::MessagesRequest;
use crate::anthropic::sse::encode_sse_event;
use crate::monitor::MonitorHandle;
use crate::providers::grok::translate::stream::SseDecoder;
use crate::providers::upstream_error;
use crate::traffic::{StreamTrafficCapture, TrafficCapture};

const DEFAULT_MAX_TOKENS: u32 = 32_000;

pub fn prepare_request(
    body: &MessagesRequest,
    model: &str,
) -> Result<serde_json::Value, serde_json::Error> {
    let mut translated = serde_json::to_value(body)?;
    translated["model"] = serde_json::Value::String(model.to_string());
    translated["max_tokens"] = serde_json::json!(
        body.max_tokens
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_MAX_TOKENS)
    );
    if model == "minimax-m3" && translated.get("thinking").is_none() {
        translated["thinking"] = serde_json::json!({"type": "adaptive"});
    }
    Ok(translated)
}

/// Passes an upstream that already speaks Anthropic Messages SSE through to
/// Claude Code as it arrives, and turns any stream that is not whole (a read
/// that fails, a frame that is not JSON, an end without `message_stop`) into
/// an `error` event. GLM shares it: the checks are about the wire shape, not
/// about who sent it.
///
/// Only whole frames go out, each written again from its decoded event. A
/// read can end halfway through a frame; forwarding it as it came glued the
/// next `error` event onto that half, and Claude Code could read neither.
pub fn stream_body<S, E>(
    upstream: S,
    provider: &'static str,
    monitor: Option<MonitorHandle>,
    req_id: String,
    traffic: Option<Arc<TrafficCapture>>,
) -> Body
where
    S: futures_util::Stream<Item = Result<Bytes, E>> + Send + Unpin + 'static,
    E: Send + 'static,
{
    let state = MessagesStreamState {
        upstream,
        provider,
        decoder: SseDecoder::default(),
        terminal: false,
        error_sent: false,
        monitor,
        req_id,
        bytes: 0,
        chunks: 0,
        stream_capture: traffic.as_ref().map(|traffic| traffic.stream_capture()),
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

struct MessagesStreamState<S> {
    upstream: S,
    provider: &'static str,
    decoder: SseDecoder,
    terminal: bool,
    error_sent: bool,
    monitor: Option<MonitorHandle>,
    req_id: String,
    bytes: u64,
    chunks: u64,
    stream_capture: Option<StreamTrafficCapture>,
    traffic: Option<Arc<TrafficCapture>>,
}

impl<S, E> MessagesStreamState<S>
where
    S: futures_util::Stream<Item = Result<Bytes, E>> + Unpin,
{
    async fn next_output(&mut self) -> Option<Bytes> {
        if self.terminal {
            return None;
        }
        if self.error_sent {
            self.terminal = true;
            return None;
        }

        loop {
            let chunk = match self.upstream.next().await {
                Some(Ok(chunk)) => chunk,
                Some(Err(_)) => return Some(self.cut_off("transport", "upstream_stream")),
                None => {
                    if self.decoder.finish().is_err() {
                        return Some(self.cut_off("decoder", "incomplete_stream"));
                    }
                    return Some(self.cut_off("protocol", "missing_message_stop"));
                }
            };
            if self.bytes == 0
                && let Some(monitor) = self.monitor.as_ref()
            {
                monitor.generation_started(&self.req_id);
            }
            self.bytes = self.bytes.saturating_add(chunk.len() as u64);
            self.chunks = self.chunks.saturating_add(1);
            if let Some(output) = self.relay(&chunk) {
                return Some(Bytes::from(output));
            }
        }
    }

    /// The whole frames `chunk` completes, or None when it completes none. A
    /// failure comes after the frames before it, which reached cc-proxy whole
    /// and are still worth sending.
    fn relay(&mut self, chunk: &[u8]) -> Option<Vec<u8>> {
        let events = match self.decoder.push(chunk) {
            Ok(events) => events,
            Err(_) => return Some(self.fail_at("decoder", "malformed_sse")),
        };
        let mut output = Vec::new();
        let mut input_tokens = None;
        let mut output_tokens = None;
        let mut terminal = false;
        for event in events {
            // Some Anthropic-compatible gateways end with OpenAI's `data:
            // [DONE]`, even after message_stop. GLM forwarded z.ai's bytes
            // raw before it came through here, so nothing shows z.ai never
            // sends one, and failing a finished answer over a terminator
            // costs the whole turn. It is skipped, not parsed, and still
            // forwarded, as GLM always did.
            if event.data.trim() == "[DONE]" {
                output.extend(encode_sse_event(event.event.as_deref(), &event.data));
                continue;
            }
            if terminal {
                output.extend(self.fail_at("protocol", "event_after_message_stop"));
                return Some(output);
            }
            let value: serde_json::Value = match serde_json::from_str(&event.data) {
                Ok(value) => value,
                Err(_) => {
                    output.extend(self.fail_at("json", "malformed_event"));
                    return Some(output);
                }
            };
            if let Some(capture) = self.stream_capture.as_mut() {
                capture.upstream_event(event.event.as_deref(), &value);
                capture
                    .downstream_event(event.event.as_deref().unwrap_or("message"), value.clone());
            }
            input_tokens = value
                .pointer("/message/usage/input_tokens")
                .or_else(|| value.pointer("/usage/input_tokens"))
                .and_then(serde_json::Value::as_u64)
                .or(input_tokens);
            output_tokens = value
                .pointer("/message/usage/output_tokens")
                .or_else(|| value.pointer("/usage/output_tokens"))
                .and_then(serde_json::Value::as_u64)
                .or(output_tokens);
            let kind = value.get("type").and_then(serde_json::Value::as_str);
            if event.event.as_deref() == Some("error") || kind == Some("error") {
                // Forward what failed, not that something did: Claude Code
                // retries an overload, waits out a throttle, and compacts a
                // prompt that is too long, but only when the type says so.
                let failure = upstream_error::from_stream(
                    value.get("error").unwrap_or(&value),
                    self.provider,
                );
                output.extend(self.fail(
                    "upstream",
                    "error_event",
                    failure.error_type(),
                    &failure.message,
                ));
                return Some(output);
            }
            output.extend(encode_sse_event(event.event.as_deref(), &event.data));
            if event.event.as_deref() == Some("message_stop") || kind == Some("message_stop") {
                terminal = true;
            }
        }
        if let Some(monitor) = self.monitor.as_ref() {
            monitor.stream_progress(
                &self.req_id,
                chunk.len() as u64,
                1,
                input_tokens,
                output_tokens,
            );
        }
        if terminal {
            if self.decoder.finish().is_err() {
                // Nothing may follow message_stop, so the error goes alone.
                return Some(self.fail_at("decoder", "trailing_incomplete_frame"));
            }
            self.terminal = true;
            self.finish_capture(true);
        }
        (!output.is_empty()).then_some(output)
    }

    /// The answer stopped partway: the read failed or timed out, or the
    /// stream closed before `message_stop`. Saying so, rather than "invalid",
    /// tells the reader that sending it again is the fix.
    fn cut_off(&mut self, stage: &str, kind: &str) -> Bytes {
        let message = format!(
            "{} stopped before the answer finished. Try again.",
            self.provider
        );
        Bytes::from(self.fail(stage, kind, "api_error", &message))
    }

    fn fail_at(&mut self, stage: &str, kind: &str) -> Vec<u8> {
        let message = format!("{} sent a stream cc-proxy could not read.", self.provider);
        self.fail(stage, kind, "api_error", &message)
    }

    fn fail(&mut self, stage: &str, kind: &str, error_type: &str, message: &str) -> Vec<u8> {
        self.error_sent = true;
        if let Some(capture) = self.stream_capture.as_mut() {
            capture.malformed(stage, kind);
        }
        if let Some(traffic) = self.traffic.as_ref() {
            traffic.write_json(
                "060-messages-stream-error",
                &serde_json::json!({
                    "stage": stage,
                    "kind": kind,
                    "bytes": self.bytes,
                    "chunks": self.chunks,
                }),
            );
        }
        let value = serde_json::json!({
            "type": "error",
            "error": {
                "type": error_type,
                "message": message
            }
        });
        if let Some(capture) = self.stream_capture.as_mut() {
            capture.downstream_event("error", value.clone());
        }
        self.finish_capture(false);
        encode_sse_event(Some("error"), &value.to_string())
    }

    fn finish_capture(&mut self, completed: bool) {
        if let (Some(capture), Some(traffic)) = (self.stream_capture.take(), self.traffic.as_ref())
        {
            capture.finish_named(
                traffic,
                serde_json::json!({
                    "kind": if completed { "stream_completion" } else { "stream_error" },
                    "bytes": self.bytes,
                    "chunks": self.chunks,
                }),
                "061-messages-stream-summary",
            );
        }
    }
}

impl<S> Drop for MessagesStreamState<S> {
    fn drop(&mut self) {
        if self.terminal || self.stream_capture.is_none() {
            return;
        }
        if let (Some(capture), Some(traffic)) = (self.stream_capture.take(), self.traffic.as_ref())
        {
            capture.finish_named(
                traffic,
                serde_json::json!({
                    "kind": "stream_abandoned",
                    "reason": "downstream_body_dropped",
                    "bytes": self.bytes,
                    "chunks": self.chunks,
                }),
                "061-messages-stream-summary",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn minimax_m3_defaults_to_adaptive_thinking_without_overriding_the_caller() {
        let body: MessagesRequest = serde_json::from_value(json!({
            "model": "minimax-m3",
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap();
        let translated = prepare_request(&body, "minimax-m3").unwrap();
        assert_eq!(translated["thinking"], json!({"type": "adaptive"}));
        assert_eq!(translated["max_tokens"], DEFAULT_MAX_TOKENS);

        let body: MessagesRequest = serde_json::from_value(json!({
            "model": "minimax-m3",
            "max_tokens": 2048,
            "messages": [{"role": "user", "content": "hello"}],
            "thinking": {"type": "enabled", "budget_tokens": 2048}
        }))
        .unwrap();
        let translated = prepare_request(&body, "minimax-m3").unwrap();
        assert_eq!(
            translated["thinking"],
            json!({"type": "enabled", "budget_tokens": 2048})
        );
        assert_eq!(translated["max_tokens"], 2048);
    }

    fn relay<const N: usize>(
        reads: [&'static [u8]; N],
    ) -> MessagesStreamState<impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Unpin>
    {
        MessagesStreamState {
            upstream: futures_util::stream::iter(reads.map(|read| Ok(Bytes::from_static(read)))),
            provider: "OpenCode Go",
            decoder: SseDecoder::default(),
            terminal: false,
            error_sent: false,
            monitor: None,
            req_id: "req".into(),
            bytes: 0,
            chunks: 0,
            stream_capture: None,
            traffic: None,
        }
    }

    #[tokio::test]
    async fn live_stream_rejects_an_incomplete_frame_after_message_stop() {
        let mut state =
            relay([b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\ndata: {"]);
        let output = state.next_output().await.expect("error event");
        assert!(
            String::from_utf8_lossy(&output)
                .contains("OpenCode Go sent a stream cc-proxy could not read.")
        );
    }

    #[tokio::test]
    async fn live_stream_forwards_the_upstream_error_type() {
        let mut state = relay([
            b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
        ]);
        let output = state.next_output().await.expect("error event");
        let output = String::from_utf8_lossy(&output);
        assert!(output.contains("overloaded_error"), "{output}");
        assert!(output.contains("Overloaded"), "{output}");
    }

    #[tokio::test]
    async fn live_stream_rejects_a_complete_event_after_message_stop() {
        let mut state = relay([concat!(
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"late\"}}\n\n"
        )
        .as_bytes()]);
        let output = state.next_output().await.expect("error event");
        assert!(
            String::from_utf8_lossy(&output)
                .contains("OpenCode Go sent a stream cc-proxy could not read.")
        );
    }

    #[tokio::test]
    async fn live_stream_sends_a_split_frame_whole_before_the_error_after_it() {
        // The first read ends halfway through the delta. Sent as it came,
        // the error event in the next read was glued onto that half.
        let mut state = relay([
            b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,",
            b"\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\r\n\r\nevent: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"Slow down\"}}\n\n",
        ]);
        let output = state.next_output().await.expect("frames");
        let output = String::from_utf8_lossy(&output);
        let delta = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n";
        assert!(output.starts_with(delta), "{output}");
        let error = &output[delta.len()..];
        assert!(error.starts_with("event: error\ndata: "), "{output}");
        assert!(error.contains("rate_limit_error") && error.contains("Slow down"));
        assert!(state.next_output().await.is_none());
    }
}
