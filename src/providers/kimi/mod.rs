pub mod auth;
pub mod client;
pub mod count_tokens;
pub mod translate;

use async_trait::async_trait;
use axum::Json;
use axum::response::{IntoResponse, Response};
use http::StatusCode;

use crate::anthropic::schema::{CountTokensResponse, MessagesRequest};
use crate::provider::{
    CliHandlers, Generation, GenerationBody, Provider, ProviderError, ProviderErrorKind,
    RequestContext,
};
use crate::providers::kimi::translate::model_allowlist::{
    KIMI_DEFAULT_MODEL, assert_routable_model, resolve_model,
};
use crate::providers::kimi::translate::request::{TranslateOptions, translate_request};
use crate::providers::opencode::{
    capture_buffered_upstream,
    chat::{self, ChatUpstream},
    sse_response, update_buffered_usage,
};
use crate::providers::upstream_error;
use crate::registry::KIMI_MODELS;

/// Kimi answers on the OpenAI chat-completions wire, the same one OpenCode Go
/// serves its chat models on, so both share one stream translator.
const KIMI: ChatUpstream = ChatUpstream {
    name: "Kimi",
    slug: "kimi",
    tool_names: &[],
};

pub struct KimiProvider;

impl Default for KimiProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl KimiProvider {
    pub fn new() -> Self {
        Self
    }

    /// Translates the request and sends it, returning the accepted response
    /// unread together with the model it resolved to.
    async fn send(
        &self,
        body: &MessagesRequest,
        ctx: &mut RequestContext,
    ) -> Result<(reqwest::Response, String), ProviderError> {
        ctx.pin_account(&KIMI_CLI)?;
        let requested = body.model.as_deref().unwrap_or(KIMI_DEFAULT_MODEL);
        let resolved = resolve_model(requested);
        assert_routable_model(&resolved).map_err(|error| {
            invalid_request(format!(
                "Model \"{requested}\" resolves to unsupported model \"{}\"",
                error.model
            ))
        })?;
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, &resolved);
        }
        let translated = translate_request(
            body,
            TranslateOptions {
                session_id: ctx.session_id.clone(),
            },
        )
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
        let response = client::KimiHttpClient::new()
            .post_kimi(&translated, ctx.account.as_deref())
            .await
            .map_err(|error| kimi_error(&error))?;
        Ok((response, resolved))
    }

    async fn buffered_response(
        &self,
        body: MessagesRequest,
        mut ctx: RequestContext,
    ) -> Result<serde_json::Value, ProviderError> {
        let (response, _) = self.send(&body, &mut ctx).await?;
        let bytes = response
            .bytes()
            .await
            .map_err(|error| kimi_error(&client::KimiError::new(0, error)))?;
        capture_buffered_upstream(&ctx, &bytes, "sse");
        let message_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        let model = body.model.as_deref().unwrap_or(KIMI_DEFAULT_MODEL);
        let value =
            chat::accumulate_response(KIMI, &bytes, &message_id, model).map_err(|error| {
                upstream_error::translation_error(error, "Kimi response translation failed")
            })?;
        update_buffered_usage(&ctx, &value);
        Ok(value)
    }
}

#[async_trait]
impl Provider for KimiProvider {
    fn name(&self) -> &'static str {
        "kimi"
    }

    fn supported_models(&self) -> Vec<String> {
        KIMI_MODELS.iter().map(|s| s.to_string()).collect()
    }

    /// `kimi-for-coding` is the older line; every Claude name means the newest.
    fn catalog(&self) -> Vec<String> {
        vec![KIMI_DEFAULT_MODEL.to_string()]
    }

    fn cli(&self) -> &dyn CliHandlers {
        &KIMI_CLI
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
        let model = body.model.as_deref().unwrap_or(KIMI_DEFAULT_MODEL);
        let resolved = resolve_model(model);
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, &resolved);
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
        mut ctx: RequestContext,
    ) -> Result<Generation, ProviderError> {
        body.stream = true;
        let (response, resolved_model) = self.send(&body, &mut ctx).await?;
        let model = body.model.unwrap_or_else(|| KIMI_DEFAULT_MODEL.to_string());
        let message_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        Ok(Generation {
            body: GenerationBody::LiveSse(chat::stream_body(
                KIMI,
                response.bytes_stream(),
                message_id,
                model,
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

fn kimi_error(err: &client::KimiError) -> ProviderError {
    upstream_error::from_http(
        err.status,
        err.detail.as_deref().unwrap_or_default(),
        err.retry_after.as_deref(),
        "Kimi",
    )
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

pub(crate) struct KimiCli;

impl crate::accounts::AccountSupport for KimiCli {
    fn sign_in(&self, _device: bool) -> anyhow::Result<serde_json::Value> {
        let device_id = uuid::Uuid::new_v4().simple().to_string();
        let tokens = auth::login::run_device_login(&device_id)?;
        Ok(serde_json::to_value(auth::manager::initial_auth(
            &tokens, device_id,
        )?)?)
    }

    fn identity(&self, login: &serde_json::Value) -> Option<String> {
        let auth: auth::token_store::StoredAuth = serde_json::from_value(login.clone()).ok()?;
        auth.user_id
            .or_else(|| auth::jwt::extract_user_id(&auth.access))
    }

    fn state(&self, login: &serde_json::Value) -> crate::provider::AuthState {
        match serde_json::from_value::<auth::token_store::StoredAuth>(login.clone()) {
            Ok(auth) => crate::provider::AuthState::SignedIn {
                account: auth.user_id,
                expires_ms: Some(auth.expires),
            },
            Err(_) => crate::provider::AuthState::Missing,
        }
    }

    fn single_login(&self) -> Box<dyn crate::auth::AuthStorage<serde_json::Value>> {
        Box::new(auth::token_store::single_login_store())
    }
}

impl CliHandlers for KimiCli {
    fn auth_state(&self) -> crate::provider::AuthState {
        crate::accounts::Accounts::current_state("kimi", self)
    }

    fn login(&self) -> anyhow::Result<()> {
        crate::auth_cli::AccountCli::open("kimi", self)?.login(&Default::default())
    }

    fn device(&self) -> anyhow::Result<()> {
        crate::auth_cli::AccountCli::open("kimi", self)?.login(&crate::auth_cli::LoginOptions {
            device: true,
            ..Default::default()
        })
    }

    fn status(&self) -> anyhow::Result<()> {
        crate::auth_cli::AccountCli::open("kimi", self)?.status()
    }

    fn logout(&self) -> anyhow::Result<()> {
        crate::auth_cli::AccountCli::open("kimi", self)?.logout(None, false)
    }

    fn accounts(&self) -> Option<&dyn crate::accounts::AccountSupport> {
        Some(self)
    }
}

pub(crate) static KIMI_CLI: KimiCli = KimiCli;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn kimi_wire_assembles_through_the_shared_translator() {
        let upstream = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"functions.search:0\",\"function\":{\"name\":\"search\",\"arguments\":\"{\\\"q\\\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\":\\\"rust\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3,\"prompt_tokens_details\":{\"cached_tokens\":4},\"completion_tokens_details\":{\"reasoning_tokens\":2}}}\n\n",
            "data: [DONE]\n\n"
        );
        let response = chat::accumulate_response(KIMI, upstream.as_bytes(), "msg_1", "k3").unwrap();
        // The signature keeps the bytes the Kimi translator wrote before it
        // moved to the shared one: `ccp:kimi:v1:msg_1:0`.
        assert_eq!(
            response["content"][0]["signature"],
            "Y2NwOmtpbWk6djE6bXNnXzE6MA"
        );
        assert_eq!(response["content"][1]["text"], "hi");
        assert_eq!(response["content"][2]["id"], "toolu_msg_1_0");
        assert_eq!(response["content"][2]["input"], json!({"q":"rust"}));
        assert_eq!(response["stop_reason"], "tool_use");
        // Kimi counts cached tokens inside prompt_tokens; Anthropic does not.
        assert_eq!(
            response["usage"],
            json!({
                "input_tokens":6,
                "output_tokens":3,
                "cache_creation_input_tokens":0,
                "cache_read_input_tokens":4,
                "output_tokens_details":{"reasoning_tokens":2,"thinking_tokens":2},
            })
        );
    }

    #[test]
    fn kimi_in_band_errors_keep_their_kind() {
        for (error, status, should_retry) in [
            (
                r#"{"message":"rate limit exceeded"}"#,
                StatusCode::TOO_MANY_REQUESTS,
                None,
            ),
            (
                r#"{"type":"exceeded_current_quota_error","message":"Your account balance is insufficient"}"#,
                StatusCode::TOO_MANY_REQUESTS,
                Some(false),
            ),
        ] {
            let upstream = format!("data: {{\"error\":{error}}}\n\n");
            let error =
                chat::accumulate_response(KIMI, upstream.as_bytes(), "m", "k3").unwrap_err();
            let failure = upstream_error::carried(&error).expect("classified upstream error");
            assert_eq!(failure.status, status);
            assert_eq!(failure.should_retry, should_retry);
        }
    }
}
