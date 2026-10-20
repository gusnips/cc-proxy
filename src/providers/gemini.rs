use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

use crate::anthropic::{
    error::json_error,
    schema::{CountTokensResponse, MessagesRequest},
};
use crate::config;
use crate::paths;
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredGeminiOAuth {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiry_date: Option<u64>,
}

enum GeminiCredential {
    ApiKey(String),
    OAuth(StoredGeminiOAuth),
}

fn oauth_auth_path() -> std::path::PathBuf {
    paths::provider_auth_file("gemini")
}

fn gemini_cli_oauth_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(
        std::path::PathBuf::from(home)
            .join(".gemini")
            .join("oauth_creds.json"),
    )
}

fn load_oauth() -> Option<StoredGeminiOAuth> {
    let path = oauth_auth_path();
    if let Ok(raw) = std::fs::read_to_string(&path)
        && let Ok(parsed) = serde_json::from_str::<StoredGeminiOAuth>(&raw)
        && !parsed.access_token.trim().is_empty()
    {
        return Some(parsed);
    }
    if std::env::var_os("CCP_CONFIG_DIR").is_none()
        && let Some(cli_path) = gemini_cli_oauth_path()
        && let Ok(raw) = std::fs::read_to_string(cli_path)
        && let Ok(parsed) = serde_json::from_str::<StoredGeminiOAuth>(&raw)
        && !parsed.access_token.trim().is_empty()
    {
        return Some(parsed);
    }
    None
}

fn resolve_credential() -> Option<GeminiCredential> {
    if let Some(key) = config::gemini_api_key() {
        return Some(GeminiCredential::ApiKey(key));
    }
    load_oauth().map(GeminiCredential::OAuth)
}

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
    if let Some(stripped) = normalized.strip_prefix(MODEL_PREFIX) {
        if !stripped.is_empty() {
            return Some(stripped.to_string());
        }
        return None;
    }
    if normalized.starts_with("gemini-") {
        return Some(normalized);
    }
    None
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

    fn client(&self) -> Result<(Arc<OpenCodeClient>, bool), ProviderError> {
        let Some(cred) = resolve_credential() else {
            return Err(ProviderError::new(
                StatusCode::UNAUTHORIZED,
                ProviderErrorKind::Authentication,
                "Gemini is not configured. Run `cc-proxy auth login gemini` or set CCP_GEMINI_API_KEY / GEMINI_API_KEY.",
            ));
        };
        let (secret, bearer_only) = match cred {
            GeminiCredential::ApiKey(key) => (key, false),
            GeminiCredential::OAuth(oauth) => (oauth.access_token, true),
        };
        let client =
            OpenCodeClient::new(config::gemini_base_url(), Some(secret)).map_err(|err| {
                ProviderError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ProviderErrorKind::Api,
                    format!("Invalid Gemini configuration: {err}"),
                )
            })?;
        Ok((Arc::new(client), bearer_only))
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
        let (client, bearer_only) = match self.client() {
            Ok(pair) => pair,
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
                bearer_only,
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
        GEMINI_MODELS.iter().map(|m| (*m).to_string()).collect()
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
        let (client, bearer_only) = self.client()?;
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
                bearer_only,
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
            return AuthState::KeySaved;
        }
        if let Some(oauth) = load_oauth() {
            return AuthState::SignedIn {
                account: Some("oauth".to_string()),
                expires_ms: oauth.expiry_date,
            };
        }
        AuthState::Missing
    }

    fn login(&self) -> Result<()> {
        println!(
            "Paste your Gemini API key (or press Enter to import ~/.gemini/oauth_creds.json):"
        );
        let key = crate::prompt::read_hidden_line("API key: ")?;
        if !key.trim().is_empty() {
            let path = config::save_gemini_api_key(key.trim())?;
            println!("Gemini API key saved to {}", path.display());
            return Ok(());
        }
        if let Some(cli_path) = gemini_cli_oauth_path().filter(|p| p.exists()) {
            let raw = std::fs::read_to_string(&cli_path)?;
            let parsed: StoredGeminiOAuth = serde_json::from_str(&raw)
                .map_err(|e| anyhow::anyhow!("invalid {}: {e}", cli_path.display()))?;
            let dest = oauth_auth_path();
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dest, serde_json::to_vec_pretty(&parsed)?)?;
            println!(
                "Imported Gemini CLI OAuth credentials to {}",
                dest.display()
            );
            return Ok(());
        }
        bail!("no Gemini API key provided and ~/.gemini/oauth_creds.json was not found")
    }

    fn device(&self) -> Result<()> {
        bail!(
            "gemini: run `cc-proxy auth login gemini` to paste an API key or import ~/.gemini/oauth_creds.json"
        )
    }

    fn status(&self) -> Result<()> {
        if let Some(source) = config::gemini_api_key_source() {
            println!("API key configured: true");
            println!("Source: {source}");
            println!("Base URL: {}", config::gemini_base_url());
            return Ok(());
        }
        if let Some(oauth) = load_oauth() {
            println!("Authenticated: true (OAuth)");
            if let Some(exp) = oauth.expiry_date {
                println!("Expires: {exp}");
            }
            println!("Base URL: {}", config::gemini_base_url());
            return Ok(());
        }
        bail!("Not authenticated")
    }

    fn logout(&self) -> Result<()> {
        let key_cleared = config::clear_gemini_api_key()?;
        let oauth_path = oauth_auth_path();
        let oauth_cleared = if oauth_path.exists() {
            let _ = std::fs::remove_file(&oauth_path);
            true
        } else {
            false
        };
        if key_cleared || oauth_cleared {
            println!("Gemini stored credentials cleared.");
        } else {
            println!("No stored Gemini credentials found.");
        }
        Ok(())
    }
}

static GEMINI_CLI: GeminiCli = GeminiCli;
