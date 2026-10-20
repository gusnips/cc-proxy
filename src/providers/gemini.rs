use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};

use crate::anthropic::{
    error::json_error,
    schema::{CountTokensResponse, MessagesRequest},
};
use crate::config;
use crate::provider::{
    AuthState, CliHandlers, Generation, GenerationBody, Provider, ProviderError, ProviderErrorKind,
    RequestContext,
};
use crate::providers::kimi::count_tokens;
use crate::providers::opencode::{
    capture_buffered_upstream,
    client::{OpenCodeClient, OpenCodeError},
    google, sse_response, update_buffered_usage,
};
use crate::providers::upstream_error;
use crate::registry::normalize_incoming_model;

pub const MODEL_PREFIX: &str = "gemini/";

pub const GEMINI_MODELS: &[&str] = &[
    "gemini-4-argon",
    "gemini-3.8-flash",
    "gemini-3.7-flash",
    "gemini-3.6-flash",
    "gemini-3.5-flash",
    "gemini-3.5-flash-lite",
    "gemini-3.1-pro",
    "gemini-3-flash",
    "gemini-2.5-pro",
    "gemini-2.5-flash",
];

pub fn advertised_models() -> Vec<String> {
    let mut out = Vec::with_capacity(GEMINI_MODELS.len() * 2);
    for model in GEMINI_MODELS {
        out.push((*model).to_string());
        out.push(format!("{MODEL_PREFIX}{model}"));
    }
    out.sort_unstable();
    out
}

pub fn resolve_model(raw: &str) -> Option<String> {
    let normalized = normalize_incoming_model(raw);
    let bare = normalized
        .strip_prefix(MODEL_PREFIX)
        .unwrap_or(normalized.as_str());
    if bare.is_empty() || !bare.starts_with("gemini-") {
        return None;
    }
    Some(bare.to_string())
}

pub struct GeminiProvider;

impl Default for GeminiProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl GeminiProvider {
    pub fn new() -> Self {
        Self
    }

    fn client(&self) -> Result<Arc<OpenCodeClient>, ProviderError> {
        let Some(key) = config::gemini_api_key() else {
            return Err(ProviderError::new(
                StatusCode::UNAUTHORIZED,
                ProviderErrorKind::Authentication,
                "Gemini API key is not configured. Run `cc-proxy auth login gemini` or set CCP_GEMINI_API_KEY / GEMINI_API_KEY.",
            ));
        };
        let client = OpenCodeClient::new(config::gemini_base_url(), Some(key)).map_err(|err| {
            ProviderError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                ProviderErrorKind::Api,
                format!("Invalid Gemini configuration: {err}"),
            )
        })?;
        Ok(Arc::new(client))
    }

    fn map_error(error: OpenCodeError) -> ProviderError {
        upstream_error::from_http(
            error.status.as_u16(),
            error.body.as_deref().unwrap_or(&error.message),
            error.retry_after.as_deref(),
            "Gemini",
        )
    }

    async fn buffered_messages_response(
        &self,
        body: MessagesRequest,
        ctx: RequestContext,
    ) -> Response {
        let requested = body.model.as_deref().unwrap_or_default();
        let Some(resolved) = resolve_model(requested) else {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("Unsupported Gemini model: {requested}"),
            );
        };
        let client = match self.client() {
            Ok(client) => client,
            Err(err) => return err.response(),
        };
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, &resolved);
            monitor.upstream_started(&ctx.req_id);
        }
        let translated = match google::prepare_request(&body) {
            Ok(translated) => translated,
            Err(err) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    err.to_string(),
                );
            }
        };
        let upstream = match client
            .post_google(
                &resolved,
                &translated,
                false,
                ctx.traffic.clone(),
                ctx.session_id.as_deref(),
                "gemini",
                "Gemini",
                true,
                false,
            )
            .await
        {
            Ok(upstream) => upstream,
            Err(err) => return Self::map_error(err).response(),
        };
        let bytes = match upstream.into_bytes().await {
            Ok(bytes) => bytes,
            Err(err) => return Self::map_error(err).response(),
        };
        capture_buffered_upstream(&ctx, &bytes, "json");
        let message_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        let value = match google::accumulate_response(&bytes, &message_id, requested) {
            Ok(value) => value,
            Err(err) => {
                return json_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    format!("Gemini response translation failed: {err}"),
                );
            }
        };
        if let Some(traffic) = ctx.traffic.as_ref() {
            traffic.write_json("051-downstream-response", &value);
        }
        update_buffered_usage(&ctx, &value);
        (StatusCode::OK, Json(value)).into_response()
    }
}

#[async_trait]
impl Provider for GeminiProvider {
    fn name(&self) -> &'static str {
        "gemini"
    }

    fn supported_models(&self) -> Vec<String> {
        advertised_models()
    }

    fn catalog(&self) -> Vec<String> {
        vec![
            "gemini-4-argon".to_string(),
            "gemini-3.1-pro".to_string(),
            "gemini-3.8-flash".to_string(),
        ]
    }

    fn cli(&self) -> &dyn CliHandlers {
        &GEMINI_CLI
    }

    async fn handle_messages(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        if !body.stream {
            return self.buffered_messages_response(body, ctx).await;
        }
        match self.generate_anthropic_stream(body, ctx).await {
            Ok(generation) => sse_response(generation.body),
            Err(error) => error.response(),
        }
    }

    async fn handle_count_tokens(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let requested = body.model.as_deref().unwrap_or_default();
        let Some(resolved) = resolve_model(requested) else {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("Unsupported Gemini model: {requested}"),
            );
        };
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
        ctx: RequestContext,
    ) -> Result<Generation, ProviderError> {
        body.stream = true;
        let requested = body.model.as_deref().unwrap_or_default();
        let resolved = resolve_model(requested).ok_or_else(|| {
            ProviderError::new(
                StatusCode::BAD_REQUEST,
                ProviderErrorKind::InvalidRequest,
                format!("Unsupported Gemini model: {requested}"),
            )
        })?;
        let client = self.client()?;
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, &resolved);
            monitor.upstream_started(&ctx.req_id);
        }
        let translated = google::prepare_request(&body).map_err(|err| {
            ProviderError::new(
                StatusCode::BAD_REQUEST,
                ProviderErrorKind::InvalidRequest,
                err.to_string(),
            )
        })?;
        let upstream = client
            .post_google(
                &resolved,
                &translated,
                true,
                ctx.traffic.clone(),
                ctx.session_id.as_deref(),
                "gemini",
                "Gemini",
                true,
                false,
            )
            .await
            .map_err(Self::map_error)?;
        let message_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        let stream = google::stream_body(
            upstream.into_stream(),
            "Gemini",
            message_id,
            requested.to_string(),
            ctx.monitor.clone(),
            ctx.req_id.clone(),
            ctx.traffic.clone(),
        );
        Ok(Generation {
            body: GenerationBody::LiveSse(stream),
            resolved_model: resolved,
        })
    }
}

struct GeminiCli;

impl CliHandlers for GeminiCli {
    fn auth_state(&self) -> AuthState {
        if config::gemini_api_key().is_some() {
            AuthState::KeySaved
        } else {
            AuthState::Missing
        }
    }

    fn login(&self) -> Result<()> {
        let url = "https://aistudio.google.com/apikey";
        println!("Open {url} to get your Google AI Studio API key.");
        let _ = crate::browser::open(url);
        println!("Paste your Google AI Studio Gemini API key (input is hidden) and press Enter:");
        let key = crate::prompt::read_hidden_line("API key: ")?;
        if key.is_empty() {
            bail!("no API key provided");
        }
        let path = config::save_gemini_api_key(&key)?;
        println!("Gemini API key saved to {}", path.display());
        Ok(())
    }

    fn device(&self) -> Result<()> {
        bail!(
            "gemini: API-key provider: set CCP_GEMINI_API_KEY / GEMINI_API_KEY or run `cc-proxy auth login gemini` (for Google OAuth, use `cc-proxy auth login antigravity`)"
        )
    }

    fn status(&self) -> Result<()> {
        let Some(source) = config::gemini_api_key_source() else {
            bail!("Not authenticated");
        };
        println!("API key configured: true");
        println!("Source: {source}");
        println!("Base URL: {}", config::gemini_base_url());
        Ok(())
    }

    fn logout(&self) -> Result<()> {
        if config::clear_gemini_api_key()? {
            println!("Gemini stored key cleared.");
        } else {
            println!("No stored Gemini key in config.json.");
        }
        Ok(())
    }
}

static GEMINI_CLI: GeminiCli = GeminiCli;
