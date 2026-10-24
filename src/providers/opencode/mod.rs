pub mod chat;
pub mod client;
pub mod google;
pub mod messages;
pub mod model;
pub mod responses;
pub mod usage;

use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    Json,
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Response},
};

use crate::anthropic::{
    error::json_error,
    schema::{CountTokensResponse, MessagesRequest},
};
use crate::provider::{
    CliHandlers, Generation, GenerationBody, Provider, ProviderError, ProviderErrorKind,
    RequestContext,
};
use crate::providers::kimi::count_tokens;
use crate::providers::upstream_error;

use self::client::{OpenCodeClient, OpenCodeError};
use self::model::EndpointKind;

enum ClientState {
    Ready(Arc<OpenCodeClient>),
    Invalid(String),
}

pub struct OpenCodeProvider {
    client: ClientState,
    zen: bool,
}

impl OpenCodeProvider {
    pub fn new() -> Self {
        let client = OpenCodeClient::with_key_source(
            crate::config::opencode_base_url(),
            crate::config::opencode_api_key,
        )
        .map(Arc::new)
        .map(ClientState::Ready)
        .unwrap_or_else(|error| ClientState::Invalid(error.to_string()));
        Self { client, zen: false }
    }

    pub fn new_zen() -> Self {
        let client = OpenCodeClient::with_key_source(
            crate::config::opencode_zen_base_url(),
            crate::config::opencode_zen_api_key,
        )
        .map(Arc::new)
        .map(ClientState::Ready)
        .unwrap_or_else(|error| ClientState::Invalid(error.to_string()));
        Self { client, zen: true }
    }

    #[cfg(test)]
    fn with_client(client: OpenCodeClient) -> Self {
        Self {
            client: ClientState::Ready(Arc::new(client)),
            zen: false,
        }
    }

    fn label(&self) -> &'static str {
        if self.zen {
            "OpenCode Zen"
        } else {
            "OpenCode Go"
        }
    }

    fn slug(&self) -> &'static str {
        if self.zen { "opencode-zen" } else { "opencode" }
    }

    fn resolve_spec(&self, requested: &str) -> Option<model::ResolvedModel> {
        if self.zen {
            model::resolve_zen(requested)
        } else {
            model::resolve(requested)
        }
    }

    fn require_key(&self, model_id: &str) -> bool {
        if self.zen {
            !model::is_zen_free_model(model_id)
        } else {
            true
        }
    }

    fn client(&self) -> Result<Arc<OpenCodeClient>, String> {
        match &self.client {
            ClientState::Ready(client) => Ok(client.clone()),
            ClientState::Invalid(error) => Err(error.clone()),
        }
    }

    /// Claude Code's betas, for Zen's Claude models: their bodies carry the
    /// fields those betas switch on, and the OpenCode CLI sends Zen's Messages
    /// endpoint an `anthropic-beta` header too.
    fn anthropic_beta<'a>(&self, body: &'a MessagesRequest) -> Option<&'a str> {
        self.zen.then_some(body.anthropic_beta.as_deref()).flatten()
    }

    /// OpenCode Zen's free models take no key, and refuse a stale one.
    fn client_for(&self, model_id: &str) -> Result<Arc<OpenCodeClient>, String> {
        let client = self.client()?;
        Ok(if self.zen && model::is_zen_free_model(model_id) {
            Arc::new(client.anonymous())
        } else {
            client
        })
    }

    fn prepare_chat(
        &self,
        body: &mut MessagesRequest,
        model_id: &str,
    ) -> anyhow::Result<(chat::ChatRequest, chat::ChatUpstream)> {
        let upstream = if !self.zen {
            chat::OPENCODE_GO
        } else if model::is_zen_free_model(model_id) && offer_opencode_read(body) {
            chat::OPENCODE_ZEN_READ
        } else {
            chat::OPENCODE_ZEN
        };
        Ok((chat::prepare_request(body, model_id)?, upstream))
    }

    async fn buffered_messages_response(
        &self,
        mut body: MessagesRequest,
        ctx: RequestContext,
    ) -> Response {
        let requested_model = body.model.clone().unwrap_or_default();
        let requested = requested_model.as_str();
        let Some(spec) = self.resolve_spec(requested) else {
            return unsupported_model(requested);
        };
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, spec.id.as_str());
        }
        let client = match self.client_for(&spec.id) {
            Ok(client) => client,
            Err(error) => return invalid_configuration_response(error),
        };
        let message_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        let require_key = self.require_key(&spec.id);

        let value = match spec.endpoint {
            EndpointKind::ChatCompletions => {
                let (translated, chat_upstream) = match self.prepare_chat(&mut body, &spec.id) {
                    Ok(prepared) => prepared,
                    Err(error) => return invalid_request_response(error),
                };
                mark_upstream_started(&ctx);
                let upstream = match client
                    .post_for_provider(
                        spec.endpoint,
                        &translated,
                        true,
                        ctx.traffic.clone(),
                        ctx.session_id.as_deref(),
                        self.slug(),
                        self.label(),
                        require_key,
                        None,
                    )
                    .await
                {
                    Ok(upstream) => upstream,
                    Err(error) => return map_error(error),
                };
                let bytes = match upstream.into_bytes().await {
                    Ok(bytes) => bytes,
                    Err(error) => return map_error(error),
                };
                capture_buffered_upstream(&ctx, &bytes, "sse");
                match chat::accumulate_response(chat_upstream, &bytes, &message_id, requested) {
                    Ok(value) => value,
                    Err(error) => {
                        return upstream_error::translation_error(
                            error,
                            &format!("{} response translation failed", self.label()),
                        )
                        .response();
                    }
                }
            }
            EndpointKind::Messages => {
                let translated = match messages::prepare_request(&body, &spec.id) {
                    Ok(translated) => translated,
                    Err(error) => return invalid_request_response(error),
                };
                mark_upstream_started(&ctx);
                let upstream = match client
                    .post_for_provider(
                        spec.endpoint,
                        &translated,
                        false,
                        ctx.traffic.clone(),
                        ctx.session_id.as_deref(),
                        self.slug(),
                        self.label(),
                        require_key,
                        self.anthropic_beta(&body),
                    )
                    .await
                {
                    Ok(upstream) => upstream,
                    Err(error) => return map_error(error),
                };
                let bytes = match upstream.into_bytes().await {
                    Ok(bytes) => bytes,
                    Err(error) => return map_error(error),
                };
                capture_buffered_upstream(&ctx, &bytes, "json");
                match serde_json::from_slice::<serde_json::Value>(&bytes) {
                    Ok(value) => value,
                    Err(error) => return invalid_upstream_response(error),
                }
            }
            EndpointKind::Responses => {
                let translated =
                    match responses::prepare_request(&body, &spec.id, ctx.session_id.clone()) {
                        Ok(translated) => translated,
                        Err(error) => return invalid_request_response(error),
                    };
                mark_upstream_started(&ctx);
                let upstream = match client
                    .post_for_provider(
                        spec.endpoint,
                        &translated,
                        true,
                        ctx.traffic.clone(),
                        ctx.session_id.as_deref(),
                        self.slug(),
                        self.label(),
                        require_key,
                        None,
                    )
                    .await
                {
                    Ok(upstream) => upstream,
                    Err(error) => return map_error(error),
                };
                let bytes = match upstream.into_bytes().await {
                    Ok(bytes) => bytes,
                    Err(error) => return map_error(error),
                };
                capture_buffered_upstream(&ctx, &bytes, "sse");
                match responses::accumulate_response(&bytes, &message_id, requested) {
                    Ok(value) => value,
                    Err(error) => return invalid_upstream_response(error),
                }
            }
            EndpointKind::Google => {
                let translated =
                    match google::prepare_request(&body, google::ToolSchemas::JsonSchema) {
                        Ok(translated) => translated,
                        Err(error) => return invalid_request_response(error),
                    };
                mark_upstream_started(&ctx);
                let upstream = match client
                    .post_google(
                        &spec.id,
                        &translated,
                        false,
                        ctx.traffic.clone(),
                        ctx.session_id.as_deref(),
                        self.slug(),
                        self.label(),
                        require_key,
                        false,
                    )
                    .await
                {
                    Ok(upstream) => upstream,
                    Err(error) => return map_error(error),
                };
                let bytes = match upstream.into_bytes().await {
                    Ok(bytes) => bytes,
                    Err(error) => return map_error(error),
                };
                capture_buffered_upstream(&ctx, &bytes, "json");
                match google::accumulate_response(self.label(), &bytes, &message_id, requested) {
                    Ok(value) => value,
                    Err(error) => {
                        return upstream_error::translation_error(
                            error,
                            &format!("{} response translation failed", self.label()),
                        )
                        .response();
                    }
                }
            }
        };

        if let Some(traffic) = ctx.traffic.as_ref() {
            traffic.write_json("051-downstream-response", &value);
        }
        update_buffered_usage(&ctx, &value);
        (StatusCode::OK, Json(value)).into_response()
    }
}

impl Default for OpenCodeProvider {
    fn default() -> Self {
        Self::new()
    }
}

pub fn advertised_models() -> Vec<String> {
    model::advertised_models()
}

pub fn advertised_zen_models() -> Vec<String> {
    model::advertised_zen_models()
}

#[async_trait]
impl Provider for OpenCodeProvider {
    fn name(&self) -> &'static str {
        self.slug()
    }

    fn supported_models(&self) -> Vec<String> {
        if self.zen {
            advertised_zen_models()
        } else {
            advertised_models()
        }
    }

    fn aggregator(&self) -> bool {
        true
    }

    fn cli(&self) -> &dyn CliHandlers {
        if self.zen {
            &OPENCODE_ZEN_CLI
        } else {
            &OPENCODE_CLI
        }
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
        let Some(spec) = self.resolve_spec(requested) else {
            return unsupported_model(requested);
        };
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, spec.id.as_str());
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
        let requested_model = body.model.clone().unwrap_or_default();
        let requested = requested_model.as_str();
        let spec = self.resolve_spec(requested).ok_or_else(|| {
            ProviderError::new(
                StatusCode::BAD_REQUEST,
                ProviderErrorKind::InvalidRequest,
                format!("Unsupported {} model: {requested}", self.label()),
            )
        })?;
        let client = self.client_for(&spec.id).map_err(|error| {
            ProviderError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                ProviderErrorKind::Api,
                format!("Invalid {} configuration: {error}", self.label()),
            )
        })?;

        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, spec.id.as_str());
            monitor.upstream_started(&ctx.req_id);
        }
        let message_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        let require_key = self.require_key(&spec.id);
        let body = match spec.endpoint {
            EndpointKind::ChatCompletions => {
                let (translated, chat_upstream) = self
                    .prepare_chat(&mut body, &spec.id)
                    .map_err(invalid_request_provider_error)?;
                let upstream = client
                    .post_for_provider(
                        spec.endpoint,
                        &translated,
                        true,
                        ctx.traffic.clone(),
                        ctx.session_id.as_deref(),
                        self.slug(),
                        self.label(),
                        require_key,
                        None,
                    )
                    .await
                    .map_err(opencode_provider_error)?;
                chat::stream_body(
                    chat_upstream,
                    upstream.into_stream(),
                    message_id,
                    requested.to_string(),
                    ctx.monitor.clone(),
                    ctx.req_id.clone(),
                    ctx.traffic.clone(),
                )
            }
            EndpointKind::Messages => {
                let translated = messages::prepare_request(&body, &spec.id)
                    .map_err(invalid_request_provider_error)?;
                let upstream = client
                    .post_for_provider(
                        spec.endpoint,
                        &translated,
                        true,
                        ctx.traffic.clone(),
                        ctx.session_id.as_deref(),
                        self.slug(),
                        self.label(),
                        require_key,
                        self.anthropic_beta(&body),
                    )
                    .await
                    .map_err(opencode_provider_error)?;
                messages::stream_body(
                    upstream.into_stream(),
                    self.label(),
                    ctx.monitor.clone(),
                    ctx.req_id.clone(),
                    ctx.traffic.clone(),
                )
            }
            EndpointKind::Responses => {
                let translated =
                    responses::prepare_request(&body, &spec.id, ctx.session_id.clone())
                        .map_err(invalid_request_provider_error)?;
                let upstream = client
                    .post_for_provider(
                        spec.endpoint,
                        &translated,
                        true,
                        ctx.traffic.clone(),
                        ctx.session_id.as_deref(),
                        self.slug(),
                        self.label(),
                        require_key,
                        None,
                    )
                    .await
                    .map_err(opencode_provider_error)?;
                responses::stream_body(
                    upstream,
                    message_id,
                    requested.to_string(),
                    ctx.monitor.clone(),
                    ctx.req_id.clone(),
                    ctx.traffic.clone(),
                )
            }
            EndpointKind::Google => {
                let translated = google::prepare_request(&body, google::ToolSchemas::JsonSchema)
                    .map_err(invalid_request_provider_error)?;
                let upstream = client
                    .post_google(
                        &spec.id,
                        &translated,
                        true,
                        ctx.traffic.clone(),
                        ctx.session_id.as_deref(),
                        self.slug(),
                        self.label(),
                        require_key,
                        false,
                    )
                    .await
                    .map_err(opencode_provider_error)?;
                google::stream_body(
                    upstream.into_stream(),
                    self.label(),
                    message_id,
                    requested.to_string(),
                    ctx.monitor.clone(),
                    ctx.req_id.clone(),
                    ctx.traffic.clone(),
                )
            }
        };

        Ok(Generation {
            body: GenerationBody::LiveSse(body),
            resolved_model: spec.id,
        })
    }
}

fn invalid_request_provider_error(error: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(
        StatusCode::BAD_REQUEST,
        ProviderErrorKind::InvalidRequest,
        error.to_string(),
    )
}

fn opencode_provider_error(error: OpenCodeError) -> ProviderError {
    upstream_error::from_http(
        error.status.as_u16(),
        error.body.as_deref().unwrap_or(&error.message),
        error.retry_after.as_deref(),
        "OpenCode Go",
    )
}

fn map_error(error: OpenCodeError) -> Response {
    opencode_provider_error(error).response()
}

/// OpenCode Zen's free tier answers only a streamed call whose tools include
/// OpenCode's own `read`, and every chat call here streams. Claude Code names
/// that tool `Read`, so it goes out as `read`. A call without it gets a `read`
/// the model is told not to use, or can't use when the call had no tools.
/// Returns whether `Read` was renamed, so the model's calls get its name back.
fn offer_opencode_read(body: &mut MessagesRequest) -> bool {
    const SENT: &str = "read";
    const CLAUDE: &str = "Read";
    let named = |value: &serde_json::Value, name: &str| {
        value.get("name").and_then(serde_json::Value::as_str) == Some(name)
    };
    let tools = body
        .extra
        .entry("tools")
        .or_insert_with(|| serde_json::json!([]));
    let Some(tools) = tools.as_array_mut() else {
        return false;
    };
    if tools.iter().any(|tool| named(tool, SENT)) {
        return false;
    }
    let Some(read) = tools.iter_mut().find(|tool| named(tool, CLAUDE)) else {
        let had_tools = !tools.is_empty();
        tools.push(serde_json::json!({
            "name": SENT,
            "description": "Not available in this conversation. Never call it.",
            "input_schema": {"type": "object", "properties": {}}
        }));
        if !had_tools {
            body.extra
                .insert("tool_choice".into(), serde_json::json!({"type": "none"}));
        }
        return false;
    };
    read["name"] = serde_json::json!(SENT);
    for message in &mut body.messages {
        let Some(blocks) = message.content.as_array_mut() else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(serde_json::Value::as_str) == Some("tool_use")
                && named(block, CLAUDE)
            {
                block["name"] = serde_json::json!(SENT);
            }
        }
    }
    if let Some(choice) = body.extra.get_mut("tool_choice")
        && named(choice, CLAUDE)
    {
        choice["name"] = serde_json::json!(SENT);
    }
    true
}

fn unsupported_model(requested: &str) -> Response {
    json_error(
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        format!("Unsupported OpenCode Go model: {requested}"),
    )
}

fn invalid_configuration_response(error: impl std::fmt::Display) -> Response {
    json_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "api_error",
        format!("Invalid OpenCode Go configuration: {error}"),
    )
}

fn invalid_request_response(error: impl std::fmt::Display) -> Response {
    json_error(
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        error.to_string(),
    )
}

fn invalid_upstream_response(error: impl std::fmt::Display) -> Response {
    json_error(
        StatusCode::BAD_GATEWAY,
        "api_error",
        format!("OpenCode Go response translation failed: {error}"),
    )
}

fn mark_upstream_started(ctx: &RequestContext) {
    if let Some(monitor) = ctx.monitor.as_ref() {
        monitor.upstream_started(&ctx.req_id);
    }
}

pub(crate) fn capture_buffered_upstream(ctx: &RequestContext, bytes: &[u8], extension: &str) {
    if let Some(traffic) = ctx.traffic.as_ref() {
        traffic.write_bytes(&format!("032-upstream-response-body.{extension}"), bytes);
    }
}

pub(crate) fn update_buffered_usage(ctx: &RequestContext, value: &serde_json::Value) {
    if let Some(monitor) = ctx.monitor.as_ref() {
        monitor.usage_updated(
            &ctx.req_id,
            value
                .pointer("/usage/input_tokens")
                .and_then(serde_json::Value::as_u64),
            value
                .pointer("/usage/output_tokens")
                .and_then(serde_json::Value::as_u64),
        );
    }
}

pub(crate) fn sse_response(body: GenerationBody) -> Response {
    let body = match body {
        GenerationBody::BufferedSse(bytes) => Body::from(bytes),
        GenerationBody::LiveSse(body) => body,
    };
    (
        [
            (http::header::CONTENT_TYPE, "text/event-stream"),
            (http::header::CACHE_CONTROL, "no-cache"),
            (http::header::CONNECTION, "keep-alive"),
        ],
        body,
    )
        .into_response()
}

struct OpenCodeCli;

/// Asks OpenCode Go for usage, the cheapest call that needs a valid key.
pub async fn check_key(api_key: &str) -> crate::provider::KeyCheck {
    use crate::provider::KeyCheck;
    let client = match OpenCodeClient::new(
        crate::config::opencode_base_url(),
        Some(api_key.to_string()),
    ) {
        Ok(client) => client,
        Err(error) => return KeyCheck::Unverified(error.to_string()),
    };
    match client.get_usage().await {
        Ok(_) => KeyCheck::Accepted,
        Err(error) if matches!(error.status.as_u16(), 401 | 403) => {
            KeyCheck::Rejected(format!("HTTP {}", error.status.as_u16()))
        }
        Err(error) => KeyCheck::Unverified(error.message),
    }
}

impl CliHandlers for OpenCodeCli {
    fn auth_state(&self) -> crate::provider::AuthState {
        if crate::config::opencode_api_key().is_some() {
            crate::provider::AuthState::KeySaved
        } else {
            crate::provider::AuthState::Missing
        }
    }

    fn login(&self) -> anyhow::Result<()> {
        let url = "https://opencode.ai/zen";
        println!("Open {url} to get your OpenCode Go API key.");
        let _ = crate::browser::open(url);
        println!("Paste your OpenCode Go API key (input is hidden) and press Enter:");
        let key = crate::prompt::read_hidden_line("API key: ")?;
        if key.is_empty() {
            anyhow::bail!("no API key provided");
        }
        let path = crate::config::save_opencode_api_key(&key)?;
        println!("OpenCode API key saved to {}", path.display());
        if crate::config::opencode_api_key_source().is_some_and(|source| source != "config.json") {
            println!(
                "Note: an environment key ({}) takes precedence; unset it to use the stored key.",
                crate::config::opencode_api_key_source().unwrap_or("env")
            );
        }
        println!("Check it with `cc-proxy usage opencode`.");
        Ok(())
    }

    fn device(&self) -> anyhow::Result<()> {
        anyhow::bail!(
            "opencode: API-key provider: set CCP_OPENCODE_API_KEY / OPENCODE_API_KEY or run `cc-proxy auth login opencode`"
        )
    }

    fn status(&self) -> anyhow::Result<()> {
        let Some(source) = crate::config::opencode_api_key_source() else {
            anyhow::bail!("Not authenticated");
        };
        println!("API key configured: true");
        println!("Source: {source}");
        println!("Base URL: {}", crate::config::opencode_base_url());
        Ok(())
    }

    fn logout(&self) -> anyhow::Result<()> {
        if crate::config::clear_opencode_api_key()? {
            println!("OpenCode stored key cleared.");
        } else {
            println!("No stored OpenCode key in config.json.");
        }
        println!(
            "Unset CCP_OPENCODE_API_KEY / OPENCODE_API_KEY if using env auth; a key in Claude Code settings.json is used only as a last resort."
        );
        Ok(())
    }
}

static OPENCODE_CLI: OpenCodeCli = OpenCodeCli;

struct OpenCodeZenCli;

impl CliHandlers for OpenCodeZenCli {
    fn auth_state(&self) -> crate::provider::AuthState {
        if crate::config::opencode_zen_api_key().is_some() {
            crate::provider::AuthState::KeySaved
        } else {
            crate::provider::AuthState::Missing
        }
    }

    fn login(&self) -> anyhow::Result<()> {
        let url = "https://opencode.ai/zen";
        println!("Open {url} to get your OpenCode Zen API key.");
        let _ = crate::browser::open(url);
        println!("Paste your OpenCode Zen API key (input is hidden) and press Enter:");
        let key = crate::prompt::read_hidden_line("API key: ")?;
        if key.is_empty() {
            anyhow::bail!("no API key provided");
        }
        let path = crate::config::save_opencode_zen_api_key(&key)?;
        println!("OpenCode Zen API key saved to {}", path.display());
        Ok(())
    }

    fn device(&self) -> anyhow::Result<()> {
        use std::time::{Duration, Instant};
        let console = std::env::var("CCP_OPENCODE_CONSOLE_URL")
            .unwrap_or_else(|_| "https://opencode.ai/console".to_string());
        let console = console.trim_end_matches('/');
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .user_agent("opencode/latest/2.0.32/cli")
            .build()?;
        let device: serde_json::Value = client
            .post(format!("{console}/auth/device/code"))
            .header("Accept", "application/json")
            .json(&serde_json::json!({
                "client_id": "opencode-cli",
                "supports_org_scope": true
            }))
            .send()?
            .error_for_status()?
            .json()?;
        let device_code = device["device_code"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing device_code from OpenCode Console"))?;
        let user_code = device["user_code"].as_str().unwrap_or_default();
        let uri = device["verification_uri_complete"]
            .as_str()
            .unwrap_or("https://opencode.ai/console");
        let interval = device["interval"].as_u64().unwrap_or(5).max(1);
        let expires_in = device["expires_in"].as_u64().unwrap_or(600);

        println!("Open this URL to sign in to OpenCode Console:");
        println!("  {uri}");
        if !user_code.is_empty() {
            println!("Enter code: {user_code}");
        }
        let _ = crate::browser::open(uri);

        let deadline = Instant::now() + Duration::from_secs(expires_in);
        let mut wait = Duration::from_secs(interval);
        while Instant::now() < deadline {
            std::thread::sleep(wait);
            let resp = client
                .post(format!("{console}/auth/device/token"))
                .header("Accept", "application/json")
                .json(&serde_json::json!({
                    "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
                    "device_code": device_code,
                    "client_id": "opencode-cli"
                }))
                .send()?;
            let body: serde_json::Value = resp.json().unwrap_or_default();
            if let Some(token) = body["access_token"].as_str().filter(|s| !s.is_empty()) {
                let path = crate::config::save_opencode_zen_api_key(token)?;
                println!(
                    "Signed in to OpenCode Zen. Token saved to {}",
                    path.display()
                );
                return Ok(());
            }
            match body["error"].as_str() {
                Some("authorization_pending") => continue,
                Some("slow_down") => {
                    wait += Duration::from_secs(5);
                }
                Some(err) => anyhow::bail!("OpenCode device sign-in failed: {err}"),
                None => anyhow::bail!("Unexpected response from OpenCode Console"),
            }
        }
        anyhow::bail!("OpenCode device sign-in timed out")
    }

    fn status(&self) -> anyhow::Result<()> {
        let Some(source) = crate::config::opencode_zen_api_key_source() else {
            anyhow::bail!("Not authenticated");
        };
        println!("API key configured: true");
        println!("Source: {source}");
        println!("Base URL: {}", crate::config::opencode_zen_base_url());
        Ok(())
    }

    fn logout(&self) -> anyhow::Result<()> {
        if crate::config::clear_opencode_zen_api_key()? {
            println!("OpenCode Zen stored key cleared.");
        } else {
            println!("No stored OpenCode Zen key in config.json.");
        }
        Ok(())
    }
}

static OPENCODE_ZEN_CLI: OpenCodeZenCli = OpenCodeZenCli;

#[cfg(test)]
mod tests {
    use axum::{
        Json, Router,
        body::Body,
        extract::{OriginalUri, State},
        http::HeaderMap,
        response::{IntoResponse, Response},
        routing::post,
    };
    use bytes::Bytes;
    use futures_util::StreamExt;
    use serde_json::json;
    use std::convert::Infallible;
    use std::sync::Mutex;

    use super::*;

    fn context() -> RequestContext {
        RequestContext {
            account: None,
            req_id: "req_test".to_string(),
            provider: "opencode".to_string(),
            session_id: None,
            session_seq: None,
            monitor: None,
            traffic: None,
        }
    }

    fn context_with_session(session_id: &str) -> RequestContext {
        RequestContext {
            session_id: Some(session_id.to_string()),
            ..context()
        }
    }

    #[tokio::test]
    async fn claude_code_session_id_is_forwarded_as_opencode_session_header() {
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        async fn capture_session_header(
            State(seen): State<Arc<Mutex<Vec<String>>>>,
            headers: HeaderMap,
            Json(_body): Json<serde_json::Value>,
        ) -> Response {
            seen.lock().unwrap().push(
                headers
                    .get("x-opencode-session")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_string(),
            );
            Json(json!({
                "id": "msg_native",
                "type": "message",
                "role": "assistant",
                "model": "minimax-m3",
                "content": [{"type":"text","text":"hello"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens":1,"output_tokens":1}
            }))
            .into_response()
        }

        let app = Router::new()
            .route("/v1/messages", post(capture_session_header))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let provider = OpenCodeProvider::with_client(
            OpenCodeClient::new(format!("http://{address}/v1"), Some("test-key".to_string()))
                .unwrap(),
        );

        let body: MessagesRequest = serde_json::from_value(json!({
            "model": "opencode-go/minimax-m3",
            "stream": false,
            "messages": [{"role":"user","content":"hello"}]
        }))
        .unwrap();
        let response = provider
            .handle_messages(body, context_with_session("claude-session-42"))
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        server.abort();

        let seen = seen.lock().unwrap();
        assert_eq!(seen.as_slice(), ["claude-session-42"]);
    }

    #[tokio::test]
    async fn missing_key_is_actionable() {
        let client = OpenCodeClient::new("https://example.com/v1".into(), None).unwrap();
        let provider = OpenCodeProvider::with_client(client);
        let body: MessagesRequest = serde_json::from_value(json!({
            "model": "glm-5.2",
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap();
        let response = provider.handle_messages(body, context()).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            value["error"]["message"],
            "OpenCode Go has no API key. Run `cc-proxy auth login opencode` to add one."
        );
    }

    #[tokio::test]
    async fn count_tokens_is_local_for_all_protocol_families() {
        let provider = OpenCodeProvider::with_client(
            OpenCodeClient::new("https://example.com/v1".into(), None).unwrap(),
        );
        for model in ["glm-5.2", "minimax-m3", "opencode-go/gpt-5.6-luna"] {
            let body: MessagesRequest = serde_json::from_value(json!({
                "model": model,
                "messages": [{"role": "user", "content": "hello world"}]
            }))
            .unwrap();
            let response = provider.handle_count_tokens(body, context()).await;
            assert_eq!(response.status(), StatusCode::OK);
        }
    }

    async fn mock_go_upstream(
        OriginalUri(uri): OriginalUri,
        headers: HeaderMap,
        Json(body): Json<serde_json::Value>,
    ) -> Response {
        if uri.path().ends_with("/messages") {
            assert_eq!(
                headers
                    .get("x-api-key")
                    .and_then(|value| value.to_str().ok()),
                Some("test-key")
            );
        } else {
            assert_eq!(
                headers
                    .get(http::header::AUTHORIZATION)
                    .and_then(|value| value.to_str().ok()),
                Some("Bearer test-key")
            );
        }
        if body["model"] == "qwen3.7-max" {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(http::header::RETRY_AFTER, "17")],
                Json(json!({"error":{"message":"Go limit reached"}})),
            )
                .into_response();
        }
        if uri.path().ends_with("/chat/completions") {
            return (
                [(http::header::CONTENT_TYPE, "text/event-stream")],
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"hello from chat\"}}]}\n\n",
                    "data: {\"choices\":[{\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3}}\n\n",
                    "data: [DONE]\n\n"
                ),
            )
                .into_response();
        }
        if uri.path().ends_with("/responses") {
            if body["max_output_tokens"] == 8 {
                return (
                    [(http::header::CONTENT_TYPE, "text/event-stream")],
                    concat!(
                        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_limited\"}}\n\n",
                        "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"truncated response\"}\n\n",
                        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
                        "data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp_limited\",\"status\":\"incomplete\",\"error\":null,\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"usage\":{\"input_tokens\":6,\"output_tokens\":8}}}\n\n",
                        "data: [DONE]\n\n"
                    ),
                )
                    .into_response();
            }
            if body["max_output_tokens"] == 9 {
                return (
                    [(http::header::CONTENT_TYPE, "text/event-stream")],
                    concat!(
                        "data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp_filtered\",\"status\":\"incomplete\",\"error\":null,\"incomplete_details\":{\"reason\":\"content_filter\"},\"usage\":{\"input_tokens\":6,\"output_tokens\":0}}}\n\n",
                        "data: [DONE]\n\n"
                    ),
                )
                    .into_response();
            }
            return (
                [(http::header::CONTENT_TYPE, "text/event-stream")],
                concat!(
                    "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
                    "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"hello from responses\"}\n\n",
                    "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":6,\"output_tokens\":3}}}\n\n"
                ),
            )
                .into_response();
        }
        if body["stream"] == true {
            return (
                [(http::header::CONTENT_TYPE, "text/event-stream")],
                concat!(
                    "event: message_start\n",
                    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_native\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"minimax-m3\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":4,\"output_tokens\":0}}}\n\n",
                    "event: message_stop\n",
                    "data: {\"type\":\"message_stop\"}\n\n"
                ),
            )
                .into_response();
        }
        Json(json!({
            "id": "msg_native",
            "type": "message",
            "role": "assistant",
            "model": body["model"],
            "content": [{"type":"text","text":"hello from messages"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens":4,"output_tokens":3}
        }))
        .into_response()
    }

    async fn mock_provider() -> (OpenCodeProvider, tokio::task::JoinHandle<()>) {
        let app = Router::new()
            .route("/v1/chat/completions", post(mock_go_upstream))
            .route("/v1/messages", post(mock_go_upstream))
            .route("/v1/responses", post(mock_go_upstream));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client =
            OpenCodeClient::new(format!("http://{address}/v1"), Some("test-key".to_string()))
                .unwrap();
        (OpenCodeProvider::with_client(client), server)
    }

    async fn delayed_chat_upstream() -> Response {
        let stream = futures_util::stream::unfold(0u8, |step| async move {
            match step {
                0 => Some((
                    Ok::<Bytes, Infallible>(Bytes::from_static(
                        b"data: {\"choices\":[{\"delta\":{\"content\":\"early\"}}]}\n\n",
                    )),
                    1,
                )),
                1 => {
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    Some((
                        Ok(Bytes::from_static(
                            b"data: {\"choices\":[{\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1}}\n\ndata: [DONE]\n\n",
                        )),
                        2,
                    ))
                }
                _ => None,
            }
        });
        (
            [(http::header::CONTENT_TYPE, "text/event-stream")],
            Body::from_stream(stream),
        )
            .into_response()
    }

    #[tokio::test]
    async fn generate_contract_streams_before_upstream_completion() {
        let app = Router::new().route("/v1/chat/completions", post(delayed_chat_upstream));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let provider = OpenCodeProvider::with_client(
            OpenCodeClient::new(format!("http://{address}/v1"), Some("test-key".to_string()))
                .unwrap(),
        );
        let body: MessagesRequest = serde_json::from_value(json!({
            "model": "glm-5.2",
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap();
        let generation = provider
            .generate_anthropic_stream(body, context())
            .await
            .unwrap();
        let GenerationBody::LiveSse(body) = generation.body else {
            panic!("OpenCode Go generation must remain live");
        };
        let mut stream = body.into_data_stream();
        let first = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
            .await
            .expect("the proxy buffered the stream until upstream completion")
            .expect("stream ended before the first translated event")
            .unwrap();
        assert!(String::from_utf8_lossy(&first).contains("early"));
        server.abort();
    }

    #[tokio::test]
    async fn buffered_messages_cover_all_three_upstream_protocols() {
        let (provider, server) = mock_provider().await;
        for (model, expected) in [
            ("glm-5.2", "hello from chat"),
            ("opencode-go/minimax-m3", "hello from messages"),
            ("opencode-go/gpt-5.6-luna", "hello from responses"),
        ] {
            let body: MessagesRequest = serde_json::from_value(json!({
                "model": model,
                "stream": false,
                "messages": [{"role":"user","content":"hello"}]
            }))
            .unwrap();
            let response = provider.handle_messages(body, context()).await;
            assert_eq!(response.status(), StatusCode::OK, "model {model}");
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["content"][0]["text"], expected, "model {model}");
        }
        server.abort();
    }

    #[tokio::test]
    async fn responses_max_output_tokens_is_a_normal_buffered_completion() {
        let (provider, server) = mock_provider().await;
        let body: MessagesRequest = serde_json::from_value(json!({
            "model": "opencode-go/gpt-5.6-luna",
            "stream": false,
            "max_tokens": 8,
            "messages": [{"role":"user","content":"write a long response"}]
        }))
        .unwrap();
        let response = provider.handle_messages(body, context()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["content"][0]["text"], "truncated response");
        assert_eq!(value["stop_reason"], "max_tokens");
        assert_eq!(value["usage"]["output_tokens"], 8);
        server.abort();
    }

    #[tokio::test]
    async fn responses_max_output_tokens_is_a_normal_streaming_completion() {
        let (provider, server) = mock_provider().await;
        let body: MessagesRequest = serde_json::from_value(json!({
            "model": "opencode-go/gpt-5.6-luna",
            "max_tokens": 8,
            "messages": [{"role":"user","content":"write a long response"}]
        }))
        .unwrap();
        let generation = provider
            .generate_anthropic_stream(body, context())
            .await
            .unwrap();
        let GenerationBody::LiveSse(body) = generation.body else {
            panic!("OpenCode Go generation must remain live");
        };
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let output = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(output.contains("truncated response"));
        assert!(output.contains(r#""stop_reason":"max_tokens""#));
        assert!(output.contains("event: message_stop"));
        assert!(!output.contains("event: error"));
        server.abort();
    }

    #[tokio::test]
    async fn responses_content_filter_is_a_refusal_not_max_tokens() {
        let (provider, server) = mock_provider().await;
        let body: MessagesRequest = serde_json::from_value(json!({
            "model": "opencode-go/gpt-5.6-luna",
            "stream": false,
            "max_tokens": 9,
            "messages": [{"role":"user","content":"hello"}]
        }))
        .unwrap();
        let response = provider.handle_messages(body, context()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["stop_reason"], "refusal");
        assert_eq!(value["usage"]["input_tokens"], 6);
        server.abort();
    }

    #[tokio::test]
    async fn streaming_contract_covers_all_three_upstream_protocols() {
        let (provider, server) = mock_provider().await;
        for model in [
            "glm-5.2",
            "opencode-go/minimax-m3",
            "opencode-go/gpt-5.6-luna",
        ] {
            let body: MessagesRequest = serde_json::from_value(json!({
                "model": model,
                "messages": [{"role":"user","content":"hello"}]
            }))
            .unwrap();
            let generation = provider
                .generate_anthropic_stream(body, context())
                .await
                .unwrap();
            assert_eq!(generation.resolved_model, model::resolve(model).unwrap().id);
            let GenerationBody::LiveSse(body) = generation.body else {
                panic!("OpenCode Go generation must remain live");
            };
            let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
            assert!(
                String::from_utf8_lossy(&bytes).contains("message_stop"),
                "model {model}"
            );
        }
        server.abort();
    }

    #[tokio::test]
    async fn rate_limit_and_retry_after_are_preserved() {
        let (provider, server) = mock_provider().await;
        let body: MessagesRequest = serde_json::from_value(json!({
            "model": "qwen3.7-max",
            "messages": [{"role":"user","content":"hello"}]
        }))
        .unwrap();
        let response = provider.handle_messages(body, context()).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response
                .headers()
                .get(http::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok()),
            Some("17")
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["type"], "rate_limit_error");
        assert_eq!(value["error"]["message"], "Go limit reached");
        server.abort();
    }

    #[test]
    fn permission_errors_are_not_reported_as_authentication_failures() {
        for status in [StatusCode::PAYMENT_REQUIRED, StatusCode::FORBIDDEN] {
            let error = opencode_provider_error(OpenCodeError {
                status,
                retry_after: None,
                body: None,
                message: "denied".into(),
            });
            assert_eq!(error.status, status);
            assert_eq!(error.kind, ProviderErrorKind::Permission);
        }
    }

    type SeenChat = Arc<Mutex<Vec<(String, serde_json::Value)>>>;

    /// What the live free tier checks, measured against OpenCode Zen: a
    /// streamed call with an `opencode/` user agent, `x-opencode-session`, and
    /// a tool named `read`. A stale key turns it into "Invalid credential".
    #[tokio::test]
    async fn zen_free_models_get_opencodes_read_and_no_key() {
        async fn capture_chat(
            State(seen): State<SeenChat>,
            headers: HeaderMap,
            Json(body): Json<serde_json::Value>,
        ) -> Response {
            let auth = headers
                .get(http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string();
            seen.lock().unwrap().push((auth, body));
            let sse = concat!(
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"file_path\\\":\\\"a.txt\\\"}\"}}]}}]}\n\n",
                "data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n\n",
                "data: [DONE]\n\n"
            );
            ([(http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }

        let seen: SeenChat = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/v1/chat/completions", post(capture_chat))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client =
            OpenCodeClient::new(format!("http://{address}/v1"), Some("stale-key".into())).unwrap();
        let provider = OpenCodeProvider {
            client: ClientState::Ready(Arc::new(client)),
            zen: true,
        };
        let request = |model: &str| -> MessagesRequest {
            serde_json::from_value(json!({
                "model": model,
                "stream": false,
                "messages": [
                    {"role": "user", "content": "Open a.txt"},
                    {"role": "assistant", "content": [
                        {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {"file_path": "b.txt"}}
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_1", "content": "b"}
                    ]}
                ],
                "tools": [
                    {"name": "Bash", "input_schema": {"type": "object"}},
                    {"name": "Read", "input_schema": {"type": "object"}}
                ],
                "tool_choice": {"type": "tool", "name": "Read"}
            }))
            .unwrap()
        };

        let free = provider
            .handle_messages(request("opencode-zen/glm-5.3-flash-free"), context())
            .await;
        assert_eq!(free.status(), StatusCode::OK);
        let free = axum::body::to_bytes(free.into_body(), usize::MAX)
            .await
            .unwrap();
        let free: serde_json::Value = serde_json::from_slice(&free).unwrap();
        // The model called OpenCode's `read`; Claude Code gets its own `Read`.
        assert_eq!(free["content"][0]["name"], "Read");
        let paid = provider
            .handle_messages(request("opencode-zen/glm-5.3"), context())
            .await;
        assert_eq!(paid.status(), StatusCode::OK);
        server.abort();

        let seen = seen.lock().unwrap();
        let tool_names = |body: &serde_json::Value| {
            body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["function"]["name"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        let (auth, sent) = &seen[0];
        assert!(auth.is_empty(), "the free tier got a key: {auth}");
        assert_eq!(tool_names(sent), ["Bash", "read"]);
        assert_eq!(
            sent["tool_choice"],
            json!({"type": "function", "function": {"name": "read"}})
        );
        let earlier_call = sent["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|message| message.get("tool_calls"))
            .unwrap();
        assert_eq!(earlier_call[0]["function"]["name"], "read");
        // Paid models keep the key and Claude Code's names.
        let (auth, sent) = &seen[1];
        assert_eq!(auth, "Bearer stale-key");
        assert_eq!(tool_names(sent), ["Bash", "Read"]);
    }

    #[tokio::test]
    async fn zen_claude_models_get_claude_codes_betas() {
        async fn capture_beta(
            State(seen): State<Arc<Mutex<Vec<String>>>>,
            headers: HeaderMap,
            Json(_body): Json<serde_json::Value>,
        ) -> Response {
            seen.lock().unwrap().push(
                headers
                    .get("anthropic-beta")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_string(),
            );
            Json(json!({
                "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
                "content": [{"type": "text", "text": "ok"}],
                "stop_reason": "end_turn", "usage": {"input_tokens": 1, "output_tokens": 1}
            }))
            .into_response()
        }

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/v1/messages", post(capture_beta))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client =
            || OpenCodeClient::new(format!("http://{address}/v1"), Some("key".into())).unwrap();
        let zen = OpenCodeProvider {
            client: ClientState::Ready(Arc::new(client())),
            zen: true,
        };
        let go = OpenCodeProvider::with_client(client());
        let request = |model: &str| {
            let mut body: MessagesRequest = serde_json::from_value(json!({
                "model": model,
                "stream": false,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap();
            body.anthropic_beta = Some("oauth-2025-04-20,context-management-2025-06-27".into());
            body
        };

        let zen_reply = zen
            .handle_messages(request("opencode-zen/claude-haiku-5-5"), context())
            .await;
        assert_eq!(zen_reply.status(), StatusCode::OK);
        let go_reply = go
            .handle_messages(request("opencode-go/minimax-m3"), context())
            .await;
        assert_eq!(go_reply.status(), StatusCode::OK);
        server.abort();

        assert_eq!(
            seen.lock().unwrap().as_slice(),
            ["context-management-2025-06-27", ""]
        );
    }

    #[test]
    fn calls_without_claude_codes_read_still_offer_opencodes() {
        let request = |extra: serde_json::Value| -> MessagesRequest {
            let mut body = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});
            body.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            serde_json::from_value(body).unwrap()
        };

        // No tools at all: a `read` the model can't call.
        let mut bare = request(json!({}));
        assert!(!offer_opencode_read(&mut bare));
        assert_eq!(bare.extra["tools"][0]["name"], "read");
        assert_eq!(bare.extra["tool_choice"], json!({"type": "none"}));

        // Tools without `Read`: a `read` it is told not to use.
        let mut other = request(json!({"tools": [{"name": "Bash", "input_schema": {}}]}));
        assert!(!offer_opencode_read(&mut other));
        assert_eq!(other.extra["tools"][1]["name"], "read");
        assert!(other.extra.get("tool_choice").is_none());

        // Already OpenCode's own `read`: left as it is.
        let mut native = request(json!({"tools": [{"name": "read", "input_schema": {}}]}));
        assert!(!offer_opencode_read(&mut native));
        assert_eq!(native.extra["tools"].as_array().unwrap().len(), 1);
    }
}
