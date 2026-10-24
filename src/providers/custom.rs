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

pub const ANTHROPIC_MODELS: &[&str] = &[
    "claude-fable-5-1",
    "claude-opus-5-5",
    "claude-opus-4-8",
    "claude-sonnet-5-5",
    "claude-sonnet-4-6",
    "claude-haiku-5-5",
    "claude-haiku-4-5",
];

pub const BUILTIN_PROVIDER_NAMES: &[&str] = &[
    "codex",
    "kimi",
    "cursor",
    "grok",
    "copilot",
    "glm",
    "anthropic",
    "opencode",
    "opencode-go",
    "opencode-zen",
    "gemini",
    "antigravity",
];

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
        let client = OpenCodeClient::with_key_source(
            config::anthropic_base_url(),
            config::anthropic_api_key,
        )
        .map(Arc::new)
        .map(ClientState::Ready)
        .unwrap_or_else(|e| ClientState::Invalid(e.to_string()));
        let models = ANTHROPIC_MODELS.iter().map(|m| (*m).to_string()).collect();
        Self {
            name: "anthropic",
            label: "Anthropic",
            format: CustomFormat::Anthropic,
            models,
            builtin_anthropic: true,
            client,
            cli: CustomCli {
                name: "anthropic",
                label: "Anthropic",
                builtin_anthropic: true,
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
            config::custom_provider_api_key(interned, &current).0
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
                builtin_anthropic: false,
            },
        }
    }

    fn endpoint(&self) -> EndpointKind {
        match self.format {
            CustomFormat::OpenAi => EndpointKind::ChatCompletions,
            CustomFormat::Responses => EndpointKind::Responses,
            CustomFormat::Anthropic => EndpointKind::Messages,
        }
    }

    fn client(&self) -> Result<Arc<OpenCodeClient>, String> {
        match &self.client {
            ClientState::Ready(client) => Ok(client.clone()),
            ClientState::Invalid(error) => Err(error.clone()),
        }
    }

    fn model_ids(&self) -> Vec<String> {
        if !self.builtin_anthropic
            && let Some(cfg) = config::custom_providers().remove(self.name)
        {
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
        if !self.builtin_anthropic && self.model_ids().iter().any(|m| m == &normalized) {
            return Some(normalized);
        }
        None
    }

    fn check_anthropic_auth(&self) -> Result<(), ProviderError> {
        if self.builtin_anthropic && config::anthropic_api_key().is_none() {
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
        ctx: RequestContext,
    ) -> Response {
        if let Err(err) = self.check_anthropic_auth() {
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
        let client = match self.client() {
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
                        self.builtin_anthropic,
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
                let translated = match messages::prepare_request(&body, &resolved) {
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
                        self.builtin_anthropic,
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
                        self.builtin_anthropic,
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
        if models.is_empty() {
            return vec![format!("{prefix}default")];
        }
        let mut out = Vec::new();
        for model in &models {
            if model.starts_with(&prefix) {
                out.push(model.clone());
            } else {
                out.push(format!("{prefix}{model}"));
                if !self.builtin_anthropic {
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
        if models.is_empty() {
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
        !self.builtin_anthropic
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
        ctx: RequestContext,
    ) -> Result<Generation, ProviderError> {
        self.check_anthropic_auth()?;
        body.stream = true;
        let requested = body.model.as_deref().unwrap_or_default();
        let resolved = self.resolve_model(requested).ok_or_else(|| {
            ProviderError::new(
                StatusCode::BAD_REQUEST,
                ProviderErrorKind::InvalidRequest,
                format!("Unsupported {} model: {requested}", self.label),
            )
        })?;
        let client = self.client().map_err(|error| {
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
                        self.builtin_anthropic,
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
                let translated = messages::prepare_request(&body, &resolved).map_err(|e| {
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
                        self.builtin_anthropic,
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
                        self.builtin_anthropic,
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

struct CustomCli {
    name: &'static str,
    label: &'static str,
    builtin_anthropic: bool,
}

fn is_loopback_url(raw: &str) -> bool {
    reqwest::Url::parse(raw)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|h| matches!(h.as_str(), "localhost" | "127.0.0.1" | "::1" | "[::1]"))
}

impl CliHandlers for CustomCli {
    fn auth_state(&self) -> AuthState {
        if self.builtin_anthropic {
            return if config::anthropic_api_key().is_some() {
                AuthState::KeySaved
            } else {
                AuthState::Missing
            };
        }
        let Some(cfg) = config::custom_providers().remove(self.name) else {
            return AuthState::Missing;
        };
        let (key, _) = config::custom_provider_api_key(self.name, &cfg);
        if key.is_some() || is_loopback_url(&cfg.base_url) {
            AuthState::KeySaved
        } else {
            AuthState::Missing
        }
    }

    fn login(&self) -> Result<()> {
        if self.builtin_anthropic {
            let url = "https://console.anthropic.com/settings/keys";
            println!("Open {url} to get your Anthropic API key.");
            let _ = crate::browser::open(url);
        }
        println!(
            "Paste your {} API key (input is hidden) and press Enter:",
            self.label
        );
        let key = crate::prompt::read_hidden_line("API key: ")?;
        if key.is_empty() {
            bail!("no API key provided");
        }
        let path = if self.builtin_anthropic {
            config::save_anthropic_api_key(&key)?
        } else {
            config::save_custom_provider_api_key(self.name, &key)?
        };
        println!("{} API key saved to {}", self.label, path.display());
        Ok(())
    }

    fn device(&self) -> Result<()> {
        bail!(
            "{}: API-key provider: run `cc-proxy auth login {}`",
            self.name,
            self.name
        )
    }

    fn status(&self) -> Result<()> {
        if self.builtin_anthropic {
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
        let cleared = if self.builtin_anthropic {
            config::clear_anthropic_api_key()?
        } else {
            config::clear_custom_provider_api_key(self.name)?
        };
        if cleared {
            println!("{} stored key cleared.", self.label);
        } else {
            println!("No stored {} key in config.json.", self.label);
        }
        Ok(())
    }
}

pub fn validate_custom_provider_name(name: &str) -> Result<String> {
    let trimmed = name.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        bail!("provider name must not be empty");
    }
    if trimmed == crate::route::CLAUDE || BUILTIN_PROVIDER_NAMES.contains(&trimmed.as_str()) {
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
    let (key, _) = config::custom_provider_api_key(name, &cfg);
    let models = super::model_catalog::fetch_openai(&cfg.base_url, key.as_deref(), Duration::from_secs(30))
        .with_context(|| format!("Could not refresh {name}. Check `cc-proxy auth status {name}`, then retry `cc-proxy models refresh {name}`."))?;
    cfg.models = models.clone();
    config::save_custom_provider(name, &cfg)?;
    Ok(models)
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
    if BUILTIN_PROVIDER_NAMES.contains(&chosen.as_str()) {
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
            "Switch with `cc-proxy use <provider>`, or go back to your Claude subscription with \
             `cc-proxy use claude`."
        );
    } else {
        println!(
            "Claude Code uses your Claude subscription. `cc-proxy use <provider>` sends it through \
             one."
        );
    }
    println!(
        "Sign in with `cc-proxy auth login <provider>`, and pick models with \
         `cc-proxy models choose <provider>`."
    );
    Ok(())
}
