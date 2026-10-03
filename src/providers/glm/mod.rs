pub mod auth;
pub mod client;

use async_trait::async_trait;
use axum::Json;
use axum::response::{IntoResponse, Response};
use http::StatusCode;

use crate::anthropic::error::json_error;
use crate::anthropic::schema::{CountTokensResponse, MessagesRequest};
use crate::auth::AuthStorage;
use crate::provider::{CliHandlers, Provider, RequestContext};
use crate::providers::opencode::messages::stream_body;
use crate::providers::upstream_error;
use crate::registry::{GLM_MODELS, normalize_incoming_model};

use self::auth::{
    auth_location, clear_glm_auth, env_glm_api_key, file_store, load_glm_api_key,
    missing_auth_message, save_glm_api_key,
};
use self::client::{GlmError, GlmHttpClient};

pub struct GlmProvider;

impl GlmProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for GlmProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for GlmProvider {
    fn name(&self) -> &'static str {
        "glm"
    }

    fn supported_models(&self) -> Vec<String> {
        GLM_MODELS.iter().map(|s| s.to_string()).collect()
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        &GLM_CLI
    }

    async fn handle_messages(&self, mut body: MessagesRequest, ctx: RequestContext) -> Response {
        let want_stream = body.stream;

        // z.ai is Anthropic-native: forward the request verbatim after stripping
        // the local [1m] compaction hint, then pipe the upstream Anthropic
        // SSE/JSON reply straight back to Claude Code (no format translation).
        body.model = body.model.map(|m| normalize_incoming_model(&m));

        let api_key = match load_glm_api_key() {
            Some(k) => k,
            None => {
                return json_error(
                    StatusCode::UNAUTHORIZED,
                    "authentication_error",
                    missing_auth_message(),
                );
            }
        };

        let body_bytes = match serde_json::to_vec(&body) {
            Ok(b) => b,
            Err(e) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    format!("Failed to serialize request: {e}"),
                );
            }
        };

        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.upstream_started(&ctx.req_id);
        }

        let client = match GlmHttpClient::new() {
            Ok(c) => c,
            Err(e) => {
                return json_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    format!("Failed to create HTTP client: {e}"),
                );
            }
        };

        let upstream = match client.post_messages(&api_key, &body_bytes).await {
            Ok(r) => r,
            Err(e) => return map_glm_error(&e),
        };

        if want_stream {
            stream_response(upstream, &ctx)
        } else {
            // A read that fails partway is a failed answer, not an empty one.
            let body = match upstream.bytes().await {
                Ok(body) => body,
                Err(e) => {
                    return map_glm_error(&GlmError {
                        status: 0,
                        detail: Some(format!(
                            "GLM stopped before the answer finished ({e}). Try again."
                        )),
                        retry_after: None,
                    });
                }
            };
            let json: serde_json::Value = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(e) => {
                    return json_error(
                        StatusCode::BAD_GATEWAY,
                        "api_error",
                        format!("Failed to parse upstream response: {e}"),
                    );
                }
            };
            if let Some(monitor) = ctx.monitor.as_ref() {
                monitor.usage_updated(
                    &ctx.req_id,
                    json.pointer("/usage/input_tokens").and_then(|v| v.as_u64()),
                    json.pointer("/usage/output_tokens")
                        .and_then(|v| v.as_u64()),
                );
            }
            (StatusCode::OK, Json(json)).into_response()
        }
    }

    async fn handle_count_tokens(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        // z.ai exposes the Anthropic count_tokens endpoint, but calling upstream
        // on every count adds latency. Use a rough local estimate (mirrors the
        // cursor provider), which is sufficient for Claude Code's compaction.
        let tokens = (serde_json::to_vec(&body).unwrap_or_default().len() / 4) as u64;
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.usage_updated(&ctx.req_id, Some(tokens), None);
        }
        (
            StatusCode::OK,
            Json(CountTokensResponse {
                input_tokens: tokens,
            }),
        )
            .into_response()
    }
}

/// Relays the SSE as it arrives. It used to be read whole first: Claude Code
/// saw nothing until the answer was done, and a read that failed partway
/// reached it as an empty 200.
fn stream_response(upstream: reqwest::Response, ctx: &RequestContext) -> Response {
    let body = stream_body(
        upstream.bytes_stream(),
        "GLM",
        ctx.monitor.clone(),
        ctx.req_id.clone(),
        ctx.traffic.clone(),
    );
    let headers = [
        (http::header::CONTENT_TYPE, "text/event-stream"),
        (http::header::CACHE_CONTROL, "no-cache"),
        (http::header::CONNECTION, "keep-alive"),
    ];
    (headers, body).into_response()
}

fn map_glm_error(err: &GlmError) -> Response {
    upstream_error::from_http(
        err.status,
        err.detail.as_deref().unwrap_or_default(),
        err.retry_after.as_deref(),
        "GLM",
    )
    .response()
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

pub(crate) struct GlmCli;

/// Sends a 1-token message on the cheapest model: the only call z.ai has
/// that needs a valid key.
pub async fn check_key(api_key: &str) -> crate::provider::KeyCheck {
    use crate::provider::KeyCheck;
    let body = serde_json::json!({
        "model": GLM_MODELS[1],
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "hi"}],
    });
    let client = match GlmHttpClient::new() {
        Ok(client) => client,
        Err(error) => return KeyCheck::Unverified(error.to_string()),
    };
    let body = body.to_string();
    let call = client.post_messages(api_key, body.as_bytes());
    match tokio::time::timeout(std::time::Duration::from_secs(20), call).await {
        Ok(Ok(_)) => KeyCheck::Accepted,
        Ok(Err(GlmError {
            status: status @ (401 | 403),
            ..
        })) => KeyCheck::Rejected(format!("HTTP {status}")),
        Ok(Err(GlmError {
            status: 0, detail, ..
        })) => KeyCheck::Unverified(detail.unwrap_or_else(|| "no answer".into())),
        Ok(Err(GlmError { status, .. })) => KeyCheck::Unverified(format!("HTTP {status}")),
        Err(_) => KeyCheck::Unverified("no answer within 20 seconds".into()),
    }
}

impl CliHandlers for GlmCli {
    fn auth_state(&self) -> crate::provider::AuthState {
        if load_glm_api_key().is_some() {
            crate::provider::AuthState::KeySaved
        } else {
            crate::provider::AuthState::Missing
        }
    }

    fn login(&self) -> Result<(), anyhow::Error> {
        println!("Paste your z.ai API key (https://z.ai; input is hidden) and press Enter:");
        let key = crate::prompt::read_hidden_line("API key: ")?;
        if key.is_empty() {
            anyhow::bail!("no API key provided");
        }
        save_glm_api_key(key)?;
        println!("GLM API key saved to {}", auth_location());
        println!("Tip: you can also export CCP_GLM_API_KEY (or GLM_API_KEY).");
        Ok(())
    }

    fn device(&self) -> Result<(), anyhow::Error> {
        anyhow::bail!(
            "glm: API-key provider: set CCP_GLM_API_KEY / GLM_API_KEY or run `cc-proxy glm auth login`"
        );
    }

    fn status(&self) -> Result<(), anyhow::Error> {
        let env_present = env_glm_api_key().is_some();
        let stored_present = file_store().load().ok().flatten().is_some();
        if !env_present && !stored_present {
            anyhow::bail!("Not authenticated");
        }
        println!("Authenticated: true");
        println!(
            "Env (CCP_GLM_API_KEY/GLM_API_KEY): {}",
            if env_present { "set" } else { "unset" }
        );
        if stored_present {
            println!("Stored: yes ({})", auth_location());
        } else {
            println!("Stored: no");
        }
        println!("API key has no expiry (static key).");
        Ok(())
    }

    fn logout(&self) -> Result<(), anyhow::Error> {
        clear_glm_auth()?;
        println!("GLM stored auth cleared. Unset CCP_GLM_API_KEY / GLM_API_KEY if using env auth.");
        Ok(())
    }
}

pub(crate) static GLM_CLI: GlmCli = GlmCli;

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures_util::StreamExt;

    use crate::monitor::{EndpointKind, MonitorHandle};

    // A reply in the Anthropic Messages shape z.ai streams.
    const REPLY: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"glm-5.3\",\"content\":[],\"usage\":{\"input_tokens\":12,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":48}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );

    /// A z.ai reply whose body arrives as these reads.
    fn upstream(
        reads: impl futures_util::Stream<Item = std::io::Result<&'static str>> + Send + 'static,
    ) -> reqwest::Response {
        let body = reqwest::Body::wrap_stream(reads.map(|read| read.map(Bytes::from)));
        reqwest::Response::from(http::Response::new(body))
    }

    fn ctx(monitor: Option<MonitorHandle>) -> RequestContext {
        RequestContext {
            req_id: "req".into(),
            session_id: None,
            session_seq: None,
            provider: "glm".into(),
            traffic: None,
            monitor,
        }
    }

    async fn relay(reads: Vec<std::io::Result<&'static str>>, ctx: &RequestContext) -> String {
        let response = stream_response(upstream(futures_util::stream::iter(reads)), ctx);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(body.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn the_first_read_reaches_claude_code_before_the_reply_is_done() {
        let first = &REPLY[..REPLY.find("event: content_block_stop").unwrap()];
        let reads = futures_util::stream::iter([Ok(first)]).chain(futures_util::stream::pending());
        // A buffered relay would wait here for a body that never ends.
        let response = stream_response(upstream(reads), &ctx(None));
        assert_eq!(
            response.headers()[http::header::CONTENT_TYPE],
            "text/event-stream"
        );
        let mut body = response.into_body().into_data_stream();
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), body.next())
            .await
            .expect("the first read was held back")
            .unwrap()
            .unwrap();
        assert_eq!(chunk, first.as_bytes());
    }

    #[tokio::test]
    async fn a_whole_reply_passes_through_unchanged_with_its_usage() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("req", None, None, EndpointKind::Messages);
        // Split mid-frame, the way a socket hands it over.
        let reads = vec![Ok(&REPLY[..150]), Ok(&REPLY[150..])];
        assert_eq!(relay(reads, &ctx(Some(monitor.clone()))).await, REPLY);
        let state = monitor.snapshot();
        assert_eq!(state.active[0].input_tokens, Some(12));
        assert_eq!(state.active[0].output_tokens, Some(48));
    }

    #[tokio::test]
    async fn a_done_terminator_after_message_stop_finishes_cleanly() {
        let reply = concat!(
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
            "data: [DONE]\n\n",
        );
        assert_eq!(relay(vec![Ok(reply)], &ctx(None)).await, reply);
    }

    #[tokio::test]
    async fn a_reply_cut_off_partway_ends_in_an_error_not_a_finished_answer() {
        let sent = &REPLY[..REPLY.find("event: message_delta").unwrap()];
        for reads in [
            // The read failed: the connection reset, or the read timeout fired.
            vec![Ok(sent), Err(std::io::Error::other("connection reset"))],
            // The body closed cleanly, but before message_stop.
            vec![Ok(sent)],
        ] {
            let body = relay(reads, &ctx(None)).await;
            let error = body.strip_prefix(sent).expect("what arrived is forwarded");
            assert!(error.starts_with("event: error\n"), "{error}");
            assert!(
                error.contains("GLM stopped before the answer finished"),
                "{error}"
            );
        }
    }

    #[test]
    fn supported_models_lists_known_glm_models() {
        let provider = GlmProvider::new();
        let models = provider.supported_models();
        assert!(models.contains(&"glm-5.3".to_string()));
        assert!(models.contains(&"glm-5.3-flash".to_string()));
        assert!(models.contains(&"glm-5.2".to_string()));
        assert!(!models.contains(&"glm-4.7".to_string()));
    }
}
