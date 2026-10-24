use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};

use crate::accounts::ApiKeySupport;
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

/// Saved API keys for Gemini, so one account can keep several and rotate.
static GEMINI_ACCOUNTS: ApiKeySupport = ApiKeySupport::new(
    "gemini",
    "Open https://aistudio.google.com/apikey to get an API key.",
    &["CCP_GEMINI_API_KEY", "GEMINI_API_KEY", "GOOGLE_API_KEY"],
);

/// The key a request uses: the environment's when set, else the selected
/// saved account's, else the one in config.json.
fn api_key() -> Option<String> {
    api_key_for(None)
}

fn api_key_for(account: Option<&str>) -> Option<String> {
    GEMINI_ACCOUNTS.key_for(account, config::gemini_api_key)
}

pub const GEMINI_MODELS: &[&str] = &[
    "gemini-4-argon",
    "gemini-3.8-flash",
    "gemini-3.7-flash",
    "gemini-3.6-flash",
    "gemini-3.5-flash",
    "gemini-3.5-flash-lite",
    "gemini-3.1-pro-preview",
    "gemini-3-flash-preview",
    "gemini-2.5-pro",
    "gemini-2.5-flash",
];

fn model_ids() -> Vec<String> {
    super::model_catalog::load("gemini").unwrap_or_else(|| {
        GEMINI_MODELS
            .iter()
            .map(|model| (*model).to_string())
            .collect()
    })
}

pub fn advertised_models() -> Vec<String> {
    model_ids()
        .into_iter()
        .flat_map(|model| [format!("{MODEL_PREFIX}{model}"), model])
        .collect()
}

pub fn catalog_models() -> Vec<String> {
    super::model_catalog::load("gemini").unwrap_or_else(|| {
        [
            "gemini-4-argon",
            "gemini-3.1-pro-preview",
            "gemini-3.8-flash",
        ]
        .map(str::to_string)
        .to_vec()
    })
}

fn parse_models(body: &serde_json::Value) -> Vec<String> {
    body["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| {
            entry["supportedGenerationMethods"]
                .as_array()
                .is_some_and(|methods| {
                    methods
                        .iter()
                        .any(|method| method.as_str() == Some("generateContent"))
                })
        })
        .filter_map(|entry| entry["name"].as_str())
        .map(|name| name.strip_prefix("models/").unwrap_or(name))
        .filter(|id| {
            id.starts_with("gemini-")
                && ![
                    "tts",
                    "image",
                    "embedding",
                    "robotics",
                    "computer-use",
                    "customtools",
                    "audio",
                    "live",
                ]
                .iter()
                .any(|word| id.contains(word))
        })
        .map(str::to_string)
        .collect()
}

fn fetch_models(base_url: &str, key: &str) -> Result<Vec<String>> {
    use anyhow::Context;
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut ids = Vec::new();
    let mut page = String::new();
    let mut seen_pages = std::collections::HashSet::new();
    loop {
        let body: serde_json::Value = client
            .get(&url)
            .query(&[("pageSize", "1000"), ("pageToken", page.as_str())])
            .header("x-goog-api-key", key)
            .send()
            .context("Could not reach Gemini for its model list. Check your connection and try again.")?
            .error_for_status()
            .context("Gemini refused the model list request. Check your API key with `cc-proxy auth status gemini`.")?
            .json()
            .context("Gemini's model list was not JSON.")?;
        ids.extend(parse_models(&body));
        let Some(next) = body["nextPageToken"]
            .as_str()
            .filter(|token| !token.is_empty())
        else {
            break;
        };
        if !seen_pages.insert(next.to_string()) {
            bail!(
                "Gemini repeated a model-list page. The saved list has not changed. Try again later."
            );
        }
        page = next.to_string();
    }
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        bail!("Gemini returned no chat models. The saved list has not changed.");
    }
    Ok(ids)
}

pub fn refresh_models() -> Result<Vec<String>> {
    let Some(key) = api_key() else {
        bail!("Gemini API key isn't configured. Run `cc-proxy auth login gemini` first.");
    };
    let ids = fetch_models(&config::gemini_base_url(), &key)?;
    super::model_catalog::save("gemini", &ids)?;
    Ok(ids)
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

    fn client(&self, account: Option<&str>) -> Result<Arc<OpenCodeClient>, ProviderError> {
        let Some(key) = api_key_for(account) else {
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
        mut ctx: RequestContext,
    ) -> Response {
        if let Err(error) = ctx.pin_account(&GEMINI_ACCOUNTS) {
            return error.response();
        }
        let requested = body.model.as_deref().unwrap_or_default();
        let Some(resolved) = resolve_model(requested) else {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("Unsupported Gemini model: {requested}"),
            );
        };
        let client = match self.client(ctx.account.as_deref()) {
            Ok(client) => client,
            Err(err) => return err.response(),
        };
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, &resolved);
            monitor.upstream_started(&ctx.req_id);
        }
        let translated = match google::prepare_request(&body, google::ToolSchemas::JsonSchema) {
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
        let value = match google::accumulate_response("Gemini", &bytes, &message_id, requested) {
            Ok(value) => value,
            Err(err) => {
                return upstream_error::translation_error(
                    err,
                    "Gemini response translation failed",
                )
                .response();
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
        catalog_models()
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
        mut ctx: RequestContext,
    ) -> Result<Generation, ProviderError> {
        ctx.pin_account(&GEMINI_ACCOUNTS)?;
        body.stream = true;
        let requested = body.model.as_deref().unwrap_or_default();
        let resolved = resolve_model(requested).ok_or_else(|| {
            ProviderError::new(
                StatusCode::BAD_REQUEST,
                ProviderErrorKind::InvalidRequest,
                format!("Unsupported Gemini model: {requested}"),
            )
        })?;
        let client = self.client(ctx.account.as_deref())?;
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, &resolved);
            monitor.upstream_started(&ctx.req_id);
        }
        let translated =
            google::prepare_request(&body, google::ToolSchemas::JsonSchema).map_err(|err| {
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
        if api_key().is_some() {
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
        println!("Fetching Gemini's model list...");
        if let Err(error) = refresh_models() {
            eprintln!(
                "The key is saved, but the model list could not be refreshed: {error:#}. Retry with `cc-proxy models refresh gemini`."
            );
        }
        Ok(())
    }

    fn device(&self) -> Result<()> {
        bail!(
            "gemini: API-key provider: set CCP_GEMINI_API_KEY / GEMINI_API_KEY or run `cc-proxy auth login gemini` (for Google OAuth, use `cc-proxy auth login antigravity`)"
        )
    }

    fn status(&self) -> Result<()> {
        let accounts = crate::accounts::Accounts::open("gemini", &GEMINI_ACCOUNTS)?;
        if !accounts.list()?.is_empty() {
            return crate::auth_cli::AccountCli::open("gemini", &GEMINI_ACCOUNTS)?.status();
        }
        let Some(source) = config::gemini_api_key_source() else {
            bail!("Not authenticated");
        };
        println!("API key configured: true");
        println!("Source: {source}");
        println!("Base URL: {}", config::gemini_base_url());
        Ok(())
    }

    fn logout(&self) -> Result<()> {
        let _ = std::fs::remove_file(super::model_catalog::cache_file("gemini"));
        if config::clear_gemini_api_key()? {
            println!("Gemini stored key cleared.");
        } else {
            println!("No stored Gemini key in config.json.");
        }
        Ok(())
    }

    fn accounts(&self) -> Option<&dyn crate::accounts::AccountSupport> {
        Some(&GEMINI_ACCOUNTS)
    }
}

static GEMINI_CLI: GeminiCli = GeminiCli;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{registry::Registry, tiers};

    #[test]
    fn gemini_lists_and_forwards_the_real_preview_ids() {
        let models = advertised_models();
        for model in ["gemini-3.1-pro-preview", "gemini-3-flash-preview"] {
            let qualified = format!("{MODEL_PREFIX}{model}");
            assert!(models.contains(&model.to_string()));
            assert!(models.contains(&qualified));
            assert_eq!(resolve_model(model).as_deref(), Some(model));
            assert_eq!(
                resolve_model(&format!("{qualified}[1m]")).as_deref(),
                Some(model)
            );
        }
        for invalid in ["gemini-3.1-pro", "gemini-3-flash"] {
            assert!(!models.contains(&invalid.to_string()));
            assert!(!models.contains(&format!("{MODEL_PREFIX}{invalid}")));
        }
    }

    #[test]
    fn gemini_defaults_skip_previews_but_explicit_choices_keep_them() {
        let registry = Registry::new();
        let mut route = config::Route {
            on: true,
            provider: Some("gemini".into()),
            ..config::Route::default()
        };
        for (tier, expected) in [
            ("fable", "gemini-4-argon"),
            ("opus", "gemini-4-argon"),
            ("sonnet", "gemini-4-argon"),
            ("haiku", "gemini-3.8-flash"),
        ] {
            assert_eq!(
                tiers::resolve(&registry, &route, &format!("claude-{tier}-5-5")),
                Ok(expected.to_string())
            );
        }
        let preview = "gemini/gemini-3.1-pro-preview";
        route
            .tiers
            .entry("gemini".into())
            .or_default()
            .insert("sonnet".into(), preview.into());
        assert_eq!(
            tiers::resolve(&registry, &route, "sonnet"),
            Ok(preview.into())
        );
    }
}
