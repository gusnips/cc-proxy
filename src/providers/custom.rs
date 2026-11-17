use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::Value;

use crate::accounts::{
    AccountSupport, Accounts, ApiKeySupport, NoLogin, api_key_state, saved_api_key,
};
use crate::anthropic::{
    error::json_error,
    schema::{CountTokensResponse, MessagesRequest},
};
use crate::config::{self, CustomFormat, CustomProviderConfig};
use crate::provider::{
    AuthState, CliHandlers, Generation, GenerationBody, Provider, ProviderError, ProviderErrorKind,
    RequestContext, intern_provider_name,
};
use crate::providers::kimi::count_tokens;
use crate::providers::opencode::{
    capture_buffered_upstream,
    chat::{self, ChatUpstream},
    client::{OpenCodeClient, OpenCodeError},
    messages,
    model::EndpointKind,
    responses, sse_response, update_buffered_usage,
};
use crate::providers::upstream_error;
use crate::registry::normalize_incoming_model;
use crate::ui;

const ANTHROPIC_MODELS: &[&str] = &[
    "claude-fable-5-1",
    "claude-opus-5-5",
    "claude-opus-4-8",
    "claude-sonnet-5-5",
    "claude-sonnet-4-6",
    "claude-haiku-5-5",
    "claude-haiku-4-5",
];

/// Claude models `provider` (`claude` or `anthropic`) offers: the list its last
/// `cc-proxy models refresh` saved, else the built-in one.
pub fn anthropic_models(provider: &str) -> Vec<String> {
    super::model_catalog::load(provider).unwrap_or_else(|| {
        ANTHROPIC_MODELS
            .iter()
            .map(|model| (*model).to_string())
            .collect()
    })
}

pub(crate) const OPENAI_API: &str = "openai-api";

enum ClientState {
    Ready(Arc<OpenCodeClient>),
    Invalid(String),
}

pub struct CustomProvider {
    name: &'static str,
    label: &'static str,
    format: CustomFormat,
    models: Vec<String>,
    builtin_anthropic: bool,
    client: ClientState,
    cli: CustomCli,
}

impl CustomProvider {
    pub fn new_anthropic() -> Self {
        Self::new_builtin(
            "anthropic",
            CustomFormat::Anthropic,
            config::anthropic_base_url(),
            || anthropic_key_for(None),
            &ANTHROPIC_ACCOUNTS,
        )
    }

    pub fn new_openai_api() -> Self {
        Self::new_builtin(
            OPENAI_API,
            CustomFormat::Responses,
            openai_api_base_url(),
            openai_api_key,
            &OPENAI_ACCOUNTS,
        )
    }

    fn new_builtin(
        name: &'static str,
        format: CustomFormat,
        base_url: String,
        key: impl Fn() -> Option<String> + Send + Sync + 'static,
        accounts: &'static ApiKeySupport,
    ) -> Self {
        let client = OpenCodeClient::with_key_source(base_url, key)
            .map(Arc::new)
            .map(ClientState::Ready)
            .unwrap_or_else(|e| ClientState::Invalid(e.to_string()));
        let label = ui::provider_name(name);
        Self {
            name,
            label,
            format,
            models: Vec::new(),
            builtin_anthropic: name == "anthropic",
            client,
            cli: CustomCli {
                name,
                label,
                builtin_accounts: Some(accounts),
            },
        }
    }

    pub fn from_config(name: &str, cfg: CustomProviderConfig) -> Self {
        let interned = intern_provider_name(name);
        // The entry is read again on each request, so a key from
        // `cc-proxy auth login <name>` works on a proxy that is running.
        let started_with = cfg.clone();
        let client = OpenCodeClient::with_key_source(cfg.base_url.clone(), move || {
            let current = config::custom_providers()
                .remove(interned)
                .unwrap_or_else(|| started_with.clone());
            custom_key_for(interned, &current, None)
        })
        .map(Arc::new)
        .map(ClientState::Ready)
        .unwrap_or_else(|e| ClientState::Invalid(e.to_string()));
        Self {
            name: interned,
            label: interned,
            format: cfg.format,
            models: cfg.models,
            builtin_anthropic: false,
            client,
            cli: CustomCli {
                name: interned,
                label: interned,
                builtin_accounts: None,
            },
        }
    }

    fn requires_key(&self) -> bool {
        self.cli.builtin_accounts.is_some()
    }

    fn endpoint(&self) -> EndpointKind {
        match self.format {
            CustomFormat::OpenAi => EndpointKind::ChatCompletions,
            CustomFormat::Responses => EndpointKind::Responses,
            CustomFormat::Anthropic => EndpointKind::Messages,
        }
    }

    fn client(&self, account: Option<&str>) -> Result<Arc<OpenCodeClient>, String> {
        let client = match &self.client {
            ClientState::Ready(client) => client.clone(),
            ClientState::Invalid(error) => return Err(error.clone()),
        };
        if self.cli.pinned_credentials().is_some() {
            return Ok(client);
        }
        let Some(id) = account else {
            return Ok(client);
        };
        let Some(key) = saved_api_key(self.name, Some(id)) else {
            return Ok(client);
        };
        Ok(Arc::new(client.with_key(Some(key))))
    }

    fn model_ids(&self) -> Vec<String> {
        if self.requires_key() {
            return if self.builtin_anthropic {
                anthropic_models(self.name)
            } else {
                super::model_catalog::load(self.name).unwrap_or_default()
            };
        }
        if let Some(cfg) = config::custom_providers().remove(self.name) {
            return cfg.models;
        }
        self.models.clone()
    }

    pub fn resolve_model(&self, raw: &str) -> Option<String> {
        let normalized = normalize_incoming_model(raw);
        let prefix = format!("{}/", self.name);
        if let Some(stripped) = normalized.strip_prefix(&prefix) {
            if !stripped.is_empty() {
                return Some(stripped.to_string());
            }
            return None;
        }
        if !self.requires_key() && self.model_ids().iter().any(|m| m == &normalized) {
            return Some(normalized);
        }
        None
    }

    fn prepare_messages(
        &self,
        body: &MessagesRequest,
        model: &str,
    ) -> Result<Value, serde_json::Error> {
        let mut translated = messages::prepare_request(body, model)?;
        if self.builtin_anthropic {
            crate::anthropic::request::prepare_api_key_request(&mut translated);
        }
        Ok(translated)
    }

    fn check_anthropic_auth(&self, account: Option<&str>) -> Result<(), ProviderError> {
        if self.builtin_anthropic && anthropic_key_for(account).is_none() {
            return Err(ProviderError::new(
                StatusCode::UNAUTHORIZED,
                ProviderErrorKind::Authentication,
                "Anthropic API key is not configured. Run `cc-proxy auth login anthropic` or set CCP_ANTHROPIC_API_KEY / ANTHROPIC_API_KEY.",
            ));
        }
        Ok(())
    }

    fn map_error(&self, error: OpenCodeError) -> ProviderError {
        upstream_error::from_http(
            error.status.as_u16(),
            error.body.as_deref().unwrap_or(&error.message),
            error.retry_after.as_deref(),
            self.label,
        )
    }

    async fn buffered_messages_response(
        &self,
        body: MessagesRequest,
        mut ctx: RequestContext,
    ) -> Response {
        if let Err(err) = ctx.pin_account(&self.cli) {
            return err.response();
        }
        if let Err(err) = self.check_anthropic_auth(ctx.account.as_deref()) {
            return err.response();
        }
        let requested = body.model.as_deref().unwrap_or_default();
        let Some(resolved) = self.resolve_model(requested) else {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("Unsupported {} model: {requested}", self.label),
            );
        };
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, &resolved);
        }
        let client = match self.client(ctx.account.as_deref()) {
            Ok(client) => client,
            Err(error) => {
                return json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "api_error",
                    format!("Invalid {} configuration: {error}", self.label),
                );
            }
        };
        let message_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        let endpoint = self.endpoint();
        let chat_upstream = ChatUpstream {
            name: self.label,
            slug: self.name,
            tool_names: &[],
        };

        let value = match endpoint {
            EndpointKind::ChatCompletions => {
                let translated = match chat::prepare_request(&body, &resolved) {
                    Ok(translated) => translated,
                    Err(error) => {
                        return json_error(
                            StatusCode::BAD_REQUEST,
                            "invalid_request_error",
                            error.to_string(),
                        );
                    }
                };
                if let Some(monitor) = ctx.monitor.as_ref() {
                    monitor.upstream_started(&ctx.req_id);
                }
                let upstream = match client
                    .post_for_provider(
                        endpoint,
                        &translated,
                        true,
                        ctx.traffic.clone(),
                        ctx.session_id.as_deref(),
                        self.name,
                        self.label,
                        self.requires_key(),
                        None,
                    )
                    .await
                {
                    Ok(upstream) => upstream,
                    Err(error) => return self.map_error(error).response(),
                };
                let bytes = match upstream.into_bytes().await {
                    Ok(bytes) => bytes,
                    Err(error) => return self.map_error(error).response(),
                };
                capture_buffered_upstream(&ctx, &bytes, "sse");
                match chat::accumulate_response(chat_upstream, &bytes, &message_id, requested) {
                    Ok(value) => value,
                    Err(error) => {
                        return upstream_error::translation_error(
                            error,
                            &format!("{} response translation failed", self.label),
                        )
                        .response();
                    }
                }
            }
            EndpointKind::Messages => {
                let translated = match self.prepare_messages(&body, &resolved) {
                    Ok(translated) => translated,
                    Err(error) => {
                        return json_error(
                            StatusCode::BAD_REQUEST,
                            "invalid_request_error",
                            error.to_string(),
                        );
                    }
                };
                if let Some(monitor) = ctx.monitor.as_ref() {
                    monitor.upstream_started(&ctx.req_id);
                }
                let upstream = match client
                    .post_for_provider(
                        endpoint,
                        &translated,
                        false,
                        ctx.traffic.clone(),
                        ctx.session_id.as_deref(),
                        self.name,
                        self.label,
                        self.requires_key(),
                        body.anthropic_beta.as_deref(),
                    )
                    .await
                {
                    Ok(upstream) => upstream,
                    Err(error) => return self.map_error(error).response(),
                };
                let bytes = match upstream.into_bytes().await {
                    Ok(bytes) => bytes,
                    Err(error) => return self.map_error(error).response(),
                };
                capture_buffered_upstream(&ctx, &bytes, "json");
                match serde_json::from_slice::<Value>(&bytes) {
                    Ok(value) => value,
                    Err(error) => {
                        return json_error(
                            StatusCode::BAD_GATEWAY,
                            "api_error",
                            format!("{} response translation failed: {error}", self.label),
                        );
                    }
                }
            }
            EndpointKind::Responses => {
                let translated =
                    match responses::prepare_request(&body, &resolved, ctx.session_id.clone()) {
                        Ok(translated) => translated,
                        Err(error) => {
                            return json_error(
                                StatusCode::BAD_REQUEST,
                                "invalid_request_error",
                                error.to_string(),
                            );
                        }
                    };
                if let Some(monitor) = ctx.monitor.as_ref() {
                    monitor.upstream_started(&ctx.req_id);
                }
                let upstream = match client
                    .post_for_provider(
                        endpoint,
                        &translated,
                        true,
                        ctx.traffic.clone(),
                        ctx.session_id.as_deref(),
                        self.name,
                        self.label,
                        self.requires_key(),
                        None,
                    )
                    .await
                {
                    Ok(upstream) => upstream,
                    Err(error) => return self.map_error(error).response(),
                };
                let bytes = match upstream.into_bytes().await {
                    Ok(bytes) => bytes,
                    Err(error) => return self.map_error(error).response(),
                };
                capture_buffered_upstream(&ctx, &bytes, "sse");
                match responses::accumulate_response(&bytes, &message_id, requested) {
                    Ok(value) => value,
                    Err(error) => {
                        return json_error(
                            StatusCode::BAD_GATEWAY,
                            "api_error",
                            format!("{} response translation failed: {error}", self.label),
                        );
                    }
                }
            }
            EndpointKind::Google => unreachable!("custom providers use openai/responses/anthropic"),
        };

        if let Some(traffic) = ctx.traffic.as_ref() {
            traffic.write_json("051-downstream-response", &value);
        }
        update_buffered_usage(&ctx, &value);
        (StatusCode::OK, Json(value)).into_response()
    }
}

#[async_trait]
impl Provider for CustomProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn supported_models(&self) -> Vec<String> {
        let prefix = format!("{}/", self.name);
        let models = self.model_ids();
        if models.is_empty() && !self.requires_key() {
            return vec![format!("{prefix}default")];
        }
        let mut out = Vec::new();
        for model in &models {
            if model.starts_with(&prefix) {
                out.push(model.clone());
            } else {
                out.push(format!("{prefix}{model}"));
                if !self.requires_key() {
                    out.push(model.clone());
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    fn catalog(&self) -> Vec<String> {
        let prefix = format!("{}/", self.name);
        let models = self.model_ids();
        if models.is_empty() && !self.requires_key() {
            return vec![format!("{prefix}default")];
        }
        models
            .iter()
            .map(|m| {
                if m.starts_with(&prefix) {
                    m.clone()
                } else {
                    format!("{prefix}{m}")
                }
            })
            .collect()
    }

    fn aggregator(&self) -> bool {
        !self.requires_key()
    }

    fn cli(&self) -> &dyn CliHandlers {
        &self.cli
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
        let Some(resolved) = self.resolve_model(requested) else {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("Unsupported {} model: {requested}", self.label),
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
        mut ctx: RequestContext,
    ) -> Result<Generation, ProviderError> {
        ctx.pin_account(&self.cli)?;
        self.check_anthropic_auth(ctx.account.as_deref())?;
        body.stream = true;
        let requested = body.model.as_deref().unwrap_or_default();
        let resolved = self.resolve_model(requested).ok_or_else(|| {
            ProviderError::new(
                StatusCode::BAD_REQUEST,
                ProviderErrorKind::InvalidRequest,
                format!("Unsupported {} model: {requested}", self.label),
            )
        })?;
        let client = self.client(ctx.account.as_deref()).map_err(|error| {
            ProviderError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                ProviderErrorKind::Api,
                format!("Invalid {} configuration: {error}", self.label),
            )
        })?;

        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, &resolved);
            monitor.upstream_started(&ctx.req_id);
        }
        let message_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        let endpoint = self.endpoint();
        let chat_upstream = ChatUpstream {
            name: self.label,
            slug: self.name,
            tool_names: &[],
        };

        let stream_body = match endpoint {
            EndpointKind::ChatCompletions => {
                let translated = chat::prepare_request(&body, &resolved).map_err(|e| {
                    ProviderError::new(
                        StatusCode::BAD_REQUEST,
                        ProviderErrorKind::InvalidRequest,
                        e.to_string(),
                    )
                })?;
                let upstream = client
                    .post_for_provider(
                        endpoint,
                        &translated,
                        true,
                        ctx.traffic.clone(),
                        ctx.session_id.as_deref(),
                        self.name,
                        self.label,
                        self.requires_key(),
                        None,
                    )
                    .await
                    .map_err(|e| self.map_error(e))?;
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
                let translated = self.prepare_messages(&body, &resolved).map_err(|e| {
                    ProviderError::new(
                        StatusCode::BAD_REQUEST,
                        ProviderErrorKind::InvalidRequest,
                        e.to_string(),
                    )
                })?;
                let upstream = client
                    .post_for_provider(
                        endpoint,
                        &translated,
                        true,
                        ctx.traffic.clone(),
                        ctx.session_id.as_deref(),
                        self.name,
                        self.label,
                        self.requires_key(),
                        body.anthropic_beta.as_deref(),
                    )
                    .await
                    .map_err(|e| self.map_error(e))?;
                messages::stream_body(
                    upstream.into_stream(),
                    self.label,
                    ctx.monitor.clone(),
                    ctx.req_id.clone(),
                    ctx.traffic.clone(),
                )
            }
            EndpointKind::Responses => {
                let translated =
                    responses::prepare_request(&body, &resolved, ctx.session_id.clone()).map_err(
                        |e| {
                            ProviderError::new(
                                StatusCode::BAD_REQUEST,
                                ProviderErrorKind::InvalidRequest,
                                e.to_string(),
                            )
                        },
                    )?;
                let upstream = client
                    .post_for_provider(
                        endpoint,
                        &translated,
                        true,
                        ctx.traffic.clone(),
                        ctx.session_id.as_deref(),
                        self.name,
                        self.label,
                        self.requires_key(),
                        None,
                    )
                    .await
                    .map_err(|e| self.map_error(e))?;
                responses::stream_body(
                    upstream,
                    message_id,
                    requested.to_string(),
                    ctx.monitor.clone(),
                    ctx.req_id.clone(),
                    ctx.traffic.clone(),
                )
            }
            EndpointKind::Google => unreachable!("custom providers use openai/responses/anthropic"),
        };

        Ok(Generation {
            body: GenerationBody::LiveSse(stream_body),
            resolved_model: resolved,
        })
    }
}

static ANTHROPIC_ACCOUNTS: ApiKeySupport = ApiKeySupport::new(
    "anthropic",
    "Open https://console.anthropic.com/settings/keys to get your Anthropic API key.",
    &["CCP_ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"],
);

fn anthropic_key_for(account: Option<&str>) -> Option<String> {
    ANTHROPIC_ACCOUNTS.key_for(account, config::anthropic_api_key)
}

static OPENAI_ACCOUNTS: ApiKeySupport = ApiKeySupport::new(
    OPENAI_API,
    "Get a key at https://platform.openai.com/api-keys. API usage is billed separately from ChatGPT.",
    &["CCP_OPENAI_API_KEY", "OPENAI_API_KEY"],
);

fn openai_api_key() -> Option<String> {
    OPENAI_ACCOUNTS.key(|| {
        OPENAI_ACCOUNTS
            .pinned_credentials()
            .and_then(|var| std::env::var(var).ok())
            .map(|key| key.trim().to_string())
    })
}

fn openai_api_base_url() -> String {
    std::env::var("CCP_OPENAI_API_BASE_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string())
}

fn custom_key_for(name: &str, cfg: &CustomProviderConfig, account: Option<&str>) -> Option<String> {
    let (configured, source) = config::custom_provider_api_key(name, cfg);
    if source.as_deref().is_some_and(|src| src != "config.json") {
        return configured;
    }
    saved_api_key(name, account).or(configured)
}

struct CustomCli {
    name: &'static str,
    label: &'static str,
    builtin_accounts: Option<&'static ApiKeySupport>,
}

fn is_loopback_url(raw: &str) -> bool {
    reqwest::Url::parse(raw)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|h| matches!(h.as_str(), "localhost" | "127.0.0.1" | "::1" | "[::1]"))
}

impl AccountSupport for CustomCli {
    fn sign_in(&self, device: bool) -> Result<Value> {
        if let Some(accounts) = self.builtin_accounts {
            return accounts.sign_in(device);
        }
        println!(
            "Paste your {} API key (input is hidden) and press Enter:",
            self.label
        );
        let key = crate::prompt::read_hidden_line("API key: ")?;
        let key = key.trim();
        if key.is_empty() {
            bail!("no API key provided");
        }
        Ok(serde_json::json!({ "apiKey": key }))
    }

    fn identity(&self, _login: &Value) -> Option<String> {
        None
    }

    fn state(&self, login: &Value) -> AuthState {
        api_key_state(login)
    }

    fn single_login(&self) -> Box<dyn crate::auth::AuthStorage<Value>> {
        Box::new(NoLogin)
    }

    fn pinned_credentials(&self) -> Option<&'static str> {
        if let Some(accounts) = self.builtin_accounts {
            return accounts.pinned_credentials();
        }
        let cfg = config::custom_providers().remove(self.name)?;
        let (_, source) = config::custom_provider_api_key(self.name, &cfg);
        source
            .filter(|src| src != "config.json")
            .map(|src| intern_provider_name(&src))
    }
}

impl CliHandlers for CustomCli {
    fn auth_state(&self) -> AuthState {
        if self.builtin_accounts.is_some() {
            let key = if self.name == "anthropic" {
                anthropic_key_for(None)
            } else {
                openai_api_key()
            };
            return if key.is_some() {
                AuthState::KeySaved
            } else {
                AuthState::Missing
            };
        }
        let Some(cfg) = config::custom_providers().remove(self.name) else {
            return AuthState::Missing;
        };
        if custom_key_for(self.name, &cfg, None).is_some() || is_loopback_url(&cfg.base_url) {
            AuthState::KeySaved
        } else {
            AuthState::Missing
        }
    }

    fn login(&self) -> Result<()> {
        crate::auth_cli::AccountCli::open(self.name, self)?.login(&Default::default())
    }

    fn device(&self) -> Result<()> {
        bail!(
            "{}: API-key provider: run `cc-proxy auth login {}`",
            self.name,
            self.name
        )
    }

    fn status(&self) -> Result<()> {
        let accounts = Accounts::open(self.name, self)?;
        if !accounts.list()?.is_empty() {
            return crate::auth_cli::AccountCli::open(self.name, self)?.status();
        }
        if self.name == OPENAI_API {
            let Some(source) = self.pinned_credentials() else {
                bail!("OpenAI API has no key. Run `cc-proxy auth login openai-api`.");
            };
            println!("API key configured: true");
            println!("Source: {source}");
            return Ok(());
        }
        if self.name == "anthropic" {
            let Some(source) = config::anthropic_api_key_source() else {
                bail!("Not authenticated");
            };
            println!("API key configured: true");
            println!("Source: {source}");
            println!("Base URL: {}", config::anthropic_base_url());
            return Ok(());
        }
        let Some(cfg) = config::custom_providers().remove(self.name) else {
            bail!("Not authenticated");
        };
        let (_, source) = config::custom_provider_api_key(self.name, &cfg);
        if let Some(source) = source {
            println!("API key configured: true");
            println!("Source: {source}");
        } else if is_loopback_url(&cfg.base_url) {
            println!("API key configured: false (local endpoint)");
        } else {
            bail!("Not authenticated");
        }
        println!("Format: {}", cfg.format.as_str());
        println!("Base URL: {}", cfg.base_url);
        Ok(())
    }

    fn logout(&self) -> Result<()> {
        crate::auth_cli::AccountCli::open(self.name, self)?.logout(None, false)
    }

    fn accounts(&self) -> Option<&dyn AccountSupport> {
        Some(self)
    }
}

fn builtin_provider_name(name: &str) -> bool {
    name == "opencode-go" || crate::route::PROVIDERS.contains(&name)
}

pub fn validate_custom_provider_name(name: &str) -> Result<String> {
    let trimmed = name.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        bail!("provider name must not be empty");
    }
    if trimmed == crate::route::DIRECT {
        bail!("`{trimmed}` bypasses the proxy. Pick a different provider name.");
    }
    if builtin_provider_name(&trimmed) {
        bail!("`{trimmed}` is a built-in provider name. Pick a different name.");
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'))
    {
        bail!("provider name `{trimmed}` may only contain lowercase letters, digits, `-`, and `_`");
    }
    Ok(trimmed)
}

pub fn validate_base_url(raw: &str) -> Result<String> {
    let url =
        reqwest::Url::parse(raw.trim()).with_context(|| format!("invalid base URL `{raw}`"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("base URL `{raw}` must start with http:// or https://");
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

pub fn run_provider_add(
    name: &str,
    base_url: &str,
    format: CustomFormat,
    mut models: Vec<String>,
    api_key_env: Option<String>,
) -> Result<()> {
    let name = validate_custom_provider_name(name)?;
    let base_url = validate_base_url(base_url)?;
    let temp_cfg = CustomProviderConfig {
        base_url: base_url.clone(),
        format,
        api_key: None,
        api_key_env: api_key_env.clone(),
        models: Vec::new(),
    };
    let (existing_key, _) = config::custom_provider_api_key(&name, &temp_cfg);
    if models.is_empty() {
        models = super::model_catalog::fetch_openai(
            &base_url,
            existing_key.as_deref(),
            Duration::from_secs(3),
        )
        .unwrap_or_default();
    }
    let existing = config::custom_providers().remove(&name);
    let cfg = CustomProviderConfig {
        base_url: base_url.clone(),
        format,
        api_key: existing.and_then(|c| c.api_key),
        api_key_env,
        models: models.clone(),
    };
    let path = config::save_custom_provider(&name, &cfg)?;
    println!(
        "Added provider `{name}` ({}, {}) to {}",
        format.as_str(),
        base_url,
        path.display()
    );
    if models.is_empty() {
        println!("Route any model with `{name}/<model-id>`.");
    } else {
        println!(
            "It lists {} models. `cc-proxy models choose {name}` picks one for each Claude name.",
            models.len()
        );
    }
    Ok(())
}

/// Fetches the live model list of a provider you added and saves it, for
/// catalogs such as OpenRouter's that change every week.
pub fn refresh_models(name: &str) -> Result<Vec<String>> {
    let Some(mut cfg) = config::custom_providers().remove(name) else {
        bail!("`{name}` isn't a provider you added. `cc-proxy provider list` shows them.");
    };
    let key = custom_key_for(name, &cfg, None);
    let models = super::model_catalog::fetch_openai(&cfg.base_url, key.as_deref(), Duration::from_secs(30))
        .with_context(|| format!("Could not refresh {name}. Check `cc-proxy auth status {name}`, then retry `cc-proxy models refresh {name}`."))?;
    cfg.models = models.clone();
    config::save_custom_provider(name, &cfg)?;
    Ok(models)
}

pub(crate) fn refresh_openai_api_models() -> Result<Vec<String>> {
    let Some(key) = openai_api_key() else {
        bail!("OpenAI API has no key. Run `cc-proxy auth login openai-api` first.");
    };
    let mut ids = super::model_catalog::fetch_ids(
        &openai_api_base_url(),
        &[("Authorization", format!("Bearer {key}"))],
        Duration::from_secs(10),
    )
    .context("Could not refresh OpenAI API models. Check `cc-proxy auth status openai-api`, then retry `cc-proxy models refresh openai-api`.")?;
    // ponytail: /models lists IDs, not their endpoints. List the GPT and o-series
    // text families. Other IDs can still be requested with the provider prefix.
    ids.retain(|id| {
        (id.starts_with("gpt-")
            || id
                .strip_prefix('o')
                .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit())))
            && crate::tiers::is_chat_model(id)
            && !id.ends_with("-instruct")
    });
    if ids.is_empty() {
        bail!(
            "OpenAI API returned no chat models. Check model access in your OpenAI project, then retry `cc-proxy models refresh openai-api`. The saved list has not changed."
        );
    }
    super::model_catalog::save(OPENAI_API, &ids)?;
    Ok(ids)
}

/// Asks Anthropic which models the API key can use and saves the list.
pub fn refresh_anthropic_models() -> Result<Vec<String>> {
    let Some(key) = anthropic_key_for(None) else {
        bail!("Anthropic API key isn't configured. Run `cc-proxy auth login anthropic` first.");
    };
    let ids = super::model_catalog::fetch_anthropic(
        &config::anthropic_base_url(),
        super::model_catalog::AnthropicLogin::ApiKey(&key),
        Duration::from_secs(30),
    )
    .context("Could not refresh Anthropic. Check `cc-proxy auth status anthropic`, then retry `cc-proxy models refresh anthropic`.")?;
    super::model_catalog::save("anthropic", &ids)?;
    Ok(ids)
}

pub fn run_provider_remove(name: Option<&str>) -> Result<()> {
    let chosen = match name {
        Some(name) => name.trim().to_ascii_lowercase(),
        None => {
            let names: Vec<String> = config::custom_providers().into_keys().collect();
            if names.is_empty() {
                bail!("no custom providers are configured in config.json");
            }
            let idx =
                crate::prompt::choose("Which custom provider do you want to remove?", &names, 0)?;
            names[idx].clone()
        }
    };
    if builtin_provider_name(&chosen) {
        bail!("`{chosen}` is a built-in provider and cannot be removed.");
    }
    if !config::remove_custom_provider(&chosen)? {
        bail!("provider `{chosen}` is not configured in config.json");
    }
    println!("Removed provider `{chosen}`.");
    Ok(())
}

pub fn run_provider_list(registry: &crate::registry::Registry) -> Result<()> {
    let styled = ui::styled(&std::io::stdout());
    let custom = config::custom_providers();
    let route = config::route();
    let active = route.target();
    for id in registry.list_provider_names() {
        let Some(provider) = registry.provider(&id) else {
            continue;
        };
        let label = ui::strong(&id, ui::provider_color(&id), styled);
        let state = provider.cli().auth_state().describe();
        let here = if active == Some(id.as_str()) {
            " · Claude Code goes here"
        } else {
            ""
        };
        if let Some(cfg) = custom.get(&id) {
            println!(
                "{label}: custom ({}, {}) · {state}{here}",
                cfg.format.as_str(),
                cfg.base_url
            );
        } else {
            println!("{label}: {} · {state}{here}", ui::provider_name(&id));
        }
    }
    println!();
    if active.is_some() {
        println!(
            "Switch with `cc-proxy use <provider>`, or bypass the proxy with \
             `cc-proxy use direct`."
        );
    } else {
        println!(
            "Claude Code uses its original settings and login, without the proxy. \
             `cc-proxy use <provider>` sends it through one."
        );
    }
    println!(
        "Sign in with `cc-proxy auth login <provider>`, and pick models with \
         `cc-proxy models choose <provider>`."
    );
    Ok(())
}

#[cfg(test)]
mod request_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_direct_anthropic_api_calls_drop_native_client_identity() {
        let body: MessagesRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-5-5",
            "system": "You are Claude Code, Anthropic's official CLI for Claude.",
            "metadata": {"user_id": "native-account"},
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .unwrap();
        let direct = CustomProvider::new_anthropic()
            .prepare_messages(&body, "claude-sonnet-5-5")
            .unwrap();
        assert!(direct.get("system").is_none());
        assert!(direct.get("metadata").is_none());
        let custom = CustomProvider::from_config(
            "local-test",
            CustomProviderConfig {
                base_url: "http://127.0.0.1:1".into(),
                format: CustomFormat::Anthropic,
                api_key: None,
                api_key_env: None,
                models: vec![],
            },
        )
        .prepare_messages(&body, "claude-sonnet-5-5")
        .unwrap();
        assert_eq!(custom["metadata"]["user_id"], "native-account");
        assert_eq!(custom["system"], body.extra["system"]);
    }
}
