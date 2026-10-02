pub mod auth;
pub mod client;
pub mod models;

use async_trait::async_trait;
use axum::Json;
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use serde_json::Value;

use crate::anthropic::schema::{CountTokensResponse, MessagesRequest};
use crate::provider::{
    CliHandlers, Generation, GenerationBody, Provider, ProviderError, ProviderErrorKind,
    RequestContext,
};
use crate::providers::kimi::count_tokens;
use crate::providers::opencode::{
    capture_buffered_upstream,
    chat::{self, ChatUpstream},
    sse_response, update_buffered_usage,
};
use crate::providers::upstream_error;
use crate::ui::{self, Mood};

/// The namespace that routes a model here, so Copilot's `gpt-*` and `claude-*`
/// ids never collide with the native providers'.
pub const MODEL_PREFIX: &str = "copilot/";

/// Copilot answers on the OpenAI chat-completions wire, so it shares the
/// translator Kimi and OpenCode Go use.
const COPILOT: ChatUpstream = ChatUpstream {
    name: "GitHub Copilot",
    slug: "copilot",
};

pub fn advertised_models() -> Vec<String> {
    models::advertised()
}

pub struct CopilotProvider;

impl Default for CopilotProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl CopilotProvider {
    pub fn new() -> Self {
        Self
    }

    /// Translates the request and sends it, returning the accepted response
    /// unread together with the model it resolved to.
    async fn send(
        &self,
        body: &MessagesRequest,
        ctx: &RequestContext,
    ) -> Result<(reqwest::Response, String), ProviderError> {
        let resolved = upstream_model(body)?;
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, &resolved);
        }
        let translated = chat::prepare_request(body, &resolved)
            .map_err(|error| invalid_request(error.to_string()))?;
        if let Some(traffic) = ctx.traffic.as_ref() {
            traffic.write_json(
                "020-upstream-request",
                &serde_json::to_value(&translated).unwrap_or_default(),
            );
        }
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.upstream_started(&ctx.req_id);
        }
        let response = client::CopilotClient::new()
            .post_chat(&translated, initiator(body))
            .await?;
        Ok((response, resolved))
    }

    async fn buffered_response(
        &self,
        body: MessagesRequest,
        ctx: RequestContext,
    ) -> Result<Value, ProviderError> {
        let (response, resolved) = self.send(&body, &ctx).await?;
        let bytes = response.bytes().await.map_err(|error| {
            upstream_error::from_http(0, &error.to_string(), None, COPILOT.name)
        })?;
        capture_buffered_upstream(&ctx, &bytes, "sse");
        let message_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        let value = chat::accumulate_response(COPILOT, &bytes, &message_id, &resolved).map_err(
            |error| {
                upstream_error::translation_error(
                    error,
                    "GitHub Copilot response translation failed",
                )
            },
        )?;
        update_buffered_usage(&ctx, &value);
        Ok(value)
    }
}

/// The model Copilot is asked for: the request's id without the `copilot/`
/// namespace.
fn upstream_model(body: &MessagesRequest) -> Result<String, ProviderError> {
    let requested = body.model.as_deref().unwrap_or_default();
    match requested.strip_prefix(MODEL_PREFIX) {
        Some(id) if !id.is_empty() => Ok(id.to_string()),
        _ => Err(invalid_request(format!(
            "Model \"{requested}\" is not a Copilot model. Name it `{MODEL_PREFIX}<id>`, for example `{MODEL_PREFIX}gpt-5.5`."
        ))),
    }
}

/// Who started this turn. Copilot bills one premium request for each `user`
/// turn, and the agent's own follow-ups after tool results (`agent`) ride free.
/// A turn is the agent's when the last message is the user's and holds only
/// tool results.
fn initiator(body: &MessagesRequest) -> &'static str {
    let only_tool_results = body.messages.last().is_some_and(|message| {
        message.role == "user"
            && message.content.as_array().is_some_and(|blocks| {
                !blocks.is_empty()
                    && blocks
                        .iter()
                        .all(|block| block["type"].as_str() == Some("tool_result"))
            })
    });
    if only_tool_results { "agent" } else { "user" }
}

#[async_trait]
impl Provider for CopilotProvider {
    fn name(&self) -> &'static str {
        "copilot"
    }

    fn supported_models(&self) -> Vec<String> {
        advertised_models()
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        &COPILOT_CLI
    }

    async fn handle_messages(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let result = if body.stream {
            self.generate_anthropic_stream(body, ctx)
                .await
                .map(|generation| sse_response(generation.body))
        } else {
            self.buffered_response(body, ctx)
                .await
                .map(|value| (StatusCode::OK, Json(value)).into_response())
        };
        result.unwrap_or_else(ProviderError::response)
    }

    async fn handle_count_tokens(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, body.model.as_deref().unwrap_or_default());
        }
        let tokens = count_tokens::count_tokens(&body);
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

    async fn generate_anthropic_stream(
        &self,
        mut body: MessagesRequest,
        ctx: RequestContext,
    ) -> Result<Generation, ProviderError> {
        body.stream = true;
        let (response, resolved_model) = self.send(&body, &ctx).await?;
        let message_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        Ok(Generation {
            body: GenerationBody::LiveSse(chat::stream_body(
                COPILOT,
                response.bytes_stream(),
                message_id,
                resolved_model.clone(),
                ctx.monitor,
                ctx.req_id,
                ctx.traffic,
            )),
            resolved_model,
        })
    }
}

fn invalid_request(message: String) -> ProviderError {
    ProviderError::new(
        StatusCode::BAD_REQUEST,
        ProviderErrorKind::InvalidRequest,
        message,
    )
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

pub(crate) struct CopilotCli;

impl CliHandlers for CopilotCli {
    fn auth_state(&self) -> crate::provider::AuthState {
        use crate::auth::AuthStorage;
        match auth::file_store().load().ok().flatten() {
            Some(stored) => crate::provider::AuthState::SignedIn {
                account: stored.account,
                expires_ms: Some(stored.expires),
            },
            None => crate::provider::AuthState::Missing,
        }
    }

    fn login(&self) -> Result<(), anyhow::Error> {
        let signed_in = auth::login(|prompt| {
            ui::eprint_note(
                Mood::Awake,
                &[
                    format!("Open {} and enter the code {}", prompt.url, prompt.code),
                    "Waiting for you to approve it on GitHub.".to_string(),
                ],
            );
        })?;
        // The list is a convenience: a failure here must not fail the sign-in.
        let listed = models::refresh(&signed_in);
        let mut lines = vec![match signed_in.account.as_deref() {
            Some(account) => format!("Signed in to GitHub Copilot as {account}"),
            None => "Signed in to GitHub Copilot".to_string(),
        }];
        lines.push(format!(
            "Auth saved in {}",
            crate::paths::provider_auth_file("copilot").display()
        ));
        match listed {
            Ok(ids) => lines.push(format!(
                "{} models. Use one as {MODEL_PREFIX}<id>, for example {MODEL_PREFIX}{}",
                ids.len(),
                ids[0]
            )),
            Err(error) => lines.push(format!(
                "Couldn't read your model list ({error}); the built-in list is used instead."
            )),
        }
        ui::print_note(Mood::Glad, &lines);
        Ok(())
    }

    fn device(&self) -> Result<(), anyhow::Error> {
        self.login()
    }

    fn status(&self) -> Result<(), anyhow::Error> {
        use crate::auth::AuthStorage;
        let store = auth::file_store();
        let Some(stored) = store.load()? else {
            anyhow::bail!("Not authenticated");
        };
        println!("Auth path: {}", store.path());
        println!("Authenticated: true");
        if let Some(account) = stored.account.as_deref() {
            println!("Account: {account}");
        }
        println!("Host: {}", stored.host);
        // The Copilot token lasts minutes and is minted again on use, so only
        // the GitHub sign-in itself decides whether this account still works.
        let remaining = stored.expires.saturating_sub(auth::now_ms()) / 1000;
        println!("Copilot token expires in {remaining}s (renewed on use)");
        Ok(())
    }

    fn logout(&self) -> Result<(), anyhow::Error> {
        use crate::auth::AuthStorage;
        auth::file_store().clear()?;
        let _ = std::fs::remove_file(
            crate::paths::provider_auth_file("copilot").with_file_name("models.json"),
        );
        println!("Logged out");
        Ok(())
    }
}

pub(crate) static COPILOT_CLI: CopilotCli = CopilotCli;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(messages: Value) -> MessagesRequest {
        serde_json::from_value(json!({"model": "copilot/gpt-5.5", "messages": messages})).unwrap()
    }

    #[test]
    fn a_turn_of_only_tool_results_is_the_agents() {
        let results = json!([
            {"role": "user", "content": "fix it"},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "Read", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]},
        ]);
        assert_eq!(initiator(&request(results)), "agent");
    }

    #[test]
    fn a_person_typing_is_a_user_turn() {
        assert_eq!(
            initiator(&request(json!([{"role": "user", "content": "hi"}]))),
            "user"
        );
        // Results with the person's own words alongside count as the person's.
        let mixed = json!([{"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "ok"},
            {"type": "text", "text": "now do this"},
        ]}]);
        assert_eq!(initiator(&request(mixed)), "user");
    }

    #[test]
    fn only_copilot_namespaced_models_are_sent() {
        let body = request(json!([]));
        assert_eq!(upstream_model(&body).unwrap(), "gpt-5.5");
        let mut bare = body;
        bare.model = Some("gpt-5.5".into());
        assert!(upstream_model(&bare).is_err());
    }
}
