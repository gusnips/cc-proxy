//! Google Antigravity (Cloud Code Assist) OAuth provider, ported from
//! `../tabrunner/refs/opencodex/src/oauth/google-antigravity.ts` and
//! `src/adapters/google-antigravity-wire.ts`.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use base64::Engine;
use futures_util::StreamExt;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::anthropic::{
    error::json_error,
    schema::{CountTokensResponse, MessagesRequest},
};
use crate::auth::{AuthStorage, FileAuthStore};
use crate::paths;
use crate::provider::{
    AuthState, CliHandlers, Generation, GenerationBody, Provider, ProviderError, ProviderErrorKind,
    RequestContext,
};
use crate::providers::kimi::count_tokens;
use crate::providers::opencode::{
    capture_buffered_upstream, google, sse_response, update_buffered_usage,
};
use crate::providers::upstream_error;
use crate::registry::normalize_incoming_model;

pub const MODEL_PREFIX: &str = "antigravity/";
// Public OAuth client identifiers embedded in the Antigravity desktop client.
const DEFAULT_CLIENT_ID_PARTS: [&str; 4] = [
    "1071006060591-",
    "tmhssin2h21lcre235vtolojh4g403ep",
    ".apps.google",
    "usercontent.com",
];
const DEFAULT_CLIENT_SECRET_PARTS: [&str; 4] = ["GOCS", "PX-", "K58FWR486LdL", "J1mLB8sXC4z6qDAf"];
const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
const PROD_API: &str = "https://cloudcode-pa.googleapis.com";
const DAILY_API: &str = "https://daily-cloudcode-pa.googleapis.com";
const API_VERSION: &str = "v1internal";
const CALLBACK_PORT: u16 = 51121;
const CALLBACK_PATH: &str = "/callback";
const REFRESH_SKEW_MS: u64 = 5 * 60 * 1000;

const SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
];

const IDE_USER_AGENT: &str =
    "antigravity/ide/2.5.5 (os_type=windows; arch=amd64; aidev_client; auth_method=oauth)";
const CLI_USER_AGENT: &str =
    "antigravity/cli/1.3.4 (os_type=darwin; arch=arm64; aidev_client; auth_method=oauth)";

const REJECTED_CLAUDE_SDK_PARAGRAPH: &str =
    "You are a Claude agent, built on Anthropic's Claude Agent SDK.";

pub const ANTIGRAVITY_MODELS: &[&str] = &[
    "gemini-4-argon",
    "gemini-4-argon-high",
    "gemini-4-argon-medium",
    "gemini-4-argon-low",
    "gemini-3.8-flash",
    "gemini-3.8-flash-high",
    "gemini-3.8-flash-medium",
    "gemini-3.8-flash-low",
    "gemini-3.7-flash",
    "gemini-3.1-pro",
    "gemini-pro-agent",
    "gemini-3.1-pro-low",
    "claude-opus-5-5",
    "claude-sonnet-5-5",
    "claude-sonnet-4-6",
    "claude-opus-4-6-thinking",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredAntigravityAuth {
    pub access: String,
    pub refresh: String,
    pub expires: u64,
    pub project_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
}

pub fn file_store() -> FileAuthStore<StoredAntigravityAuth> {
    FileAuthStore::new(
        paths::provider_auth_file("antigravity")
            .to_string_lossy()
            .to_string(),
        paths::provider_legacy_auth_file("antigravity")
            .to_string_lossy()
            .to_string(),
    )
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn client_id() -> String {
    std::env::var("GOOGLE_ANTIGRAVITY_CLIENT_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_CLIENT_ID_PARTS.concat())
}

fn client_secret() -> String {
    std::env::var("GOOGLE_ANTIGRAVITY_CLIENT_SECRET")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_CLIENT_SECRET_PARTS.concat())
}

fn user_agent_for_wire_model(wire_model: &str) -> &'static str {
    if wire_model.starts_with("gemini-4-argon") {
        CLI_USER_AGENT
    } else {
        IDE_USER_AGENT
    }
}

pub fn advertised_models() -> Vec<String> {
    let mut out: Vec<String> = ANTIGRAVITY_MODELS
        .iter()
        .map(|m| format!("{MODEL_PREFIX}{m}"))
        .collect();
    out.sort_unstable();
    out
}

pub fn resolve_wire_model(raw: &str) -> Option<String> {
    let normalized = normalize_incoming_model(raw);
    let stripped = normalized.strip_prefix(MODEL_PREFIX)?;
    if stripped.is_empty() {
        return None;
    }
    let wire = match stripped {
        "gemini-4-argon" => "gemini-4-argon-high",
        "gemini-3.8-flash" => "gemini-3.8-flash-medium",
        "gemini-3.7-flash" => "gemini-3.7-flash-tiered",
        "gemini-3.1-pro" => "gemini-pro-agent",
        "claude-opus-5-5" => "claude-opus-5-5-high",
        "claude-sonnet-5-5" => "claude-sonnet-5-5-high",
        other => other,
    };
    Some(wire.to_string())
}

fn antigravity_session_id(session_id: Option<&str>) -> String {
    let seed = session_id
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let digest = Sha256::digest(seed.as_bytes());
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    let masked = u64::from_be_bytes(bytes) & 0x7fff_ffff_ffff_ffff;
    format!("-{masked}")
}

fn extract_project_id(data: &Value) -> Option<String> {
    for key in ["cloudaicompanionProject", "projectId", "project"] {
        if let Some(val) = data.get(key) {
            if let Some(s) = val.as_str().filter(|s| !s.is_empty()) {
                return Some(s.to_string());
            }
            if let Some(id) = val
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                return Some(id.to_string());
            }
        }
    }
    None
}

fn decode_jwt_email(token: &str) -> Option<String> {
    let part = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(part)
        .ok()?;
    let json: Value = serde_json::from_slice(&bytes).ok()?;
    json.get("email")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase)
}

fn discover_project_blocking(
    client: &reqwest::blocking::Client,
    access_token: &str,
) -> Result<(String, Option<String>)> {
    if let Ok(resp) = client
        .post(format!("{PROD_API}/{API_VERSION}:loadCodeAssist"))
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Content-Type", "application/json")
        .header("User-Agent", IDE_USER_AGENT)
        .json(&json!({ "metadata": { "ideType": "ANTIGRAVITY" } }))
        .send()
        && resp.status().is_success()
        && let Ok(body) = resp.json::<Value>()
    {
        let plan = body
            .pointer("/paidTier/id")
            .or_else(|| body.pointer("/currentTier/id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(project_id) = extract_project_id(&body) {
            return Ok((project_id, plan));
        }
    }

    for _ in 0..5 {
        if let Ok(resp) = client
            .post(format!("{DAILY_API}/{API_VERSION}:onboardUser"))
            .header("Authorization", format!("Bearer {access_token}"))
            .header("Content-Type", "application/json")
            .header("User-Agent", IDE_USER_AGENT)
            .json(&json!({
                "tier_id": "free-tier",
                "metadata": {
                    "ide_type": "ANTIGRAVITY",
                    "ide_name": "antigravity",
                    "ide_version": "2.5.5"
                }
            }))
            .send()
            && resp.status().is_success()
            && let Ok(body) = resp.json::<Value>()
            && body.get("done").and_then(Value::as_bool) == Some(true)
            && let Some(resp_obj) = body.get("response")
            && let Some(project_id) = extract_project_id(resp_obj)
        {
            return Ok((project_id, Some("free-tier".to_string())));
        }
        std::thread::sleep(Duration::from_secs(2));
    }

    bail!(
        "Antigravity login could not discover a Cloud Code Assist project for this Google account."
    )
}

async fn load_valid_auth() -> Result<StoredAntigravityAuth, ProviderError> {
    let stored = file_store()
        .load()
        .map_err(|e| {
            ProviderError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                ProviderErrorKind::Api,
                format!("Failed to read Antigravity credentials: {e}"),
            )
        })?
        .ok_or_else(|| {
            ProviderError::new(
                StatusCode::UNAUTHORIZED,
                ProviderErrorKind::Authentication,
                "Antigravity is not signed in. Run `cc-proxy auth login antigravity`.",
            )
        })?;

    if stored.expires > now_ms() {
        return Ok(stored);
    }

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| {
            ProviderError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                ProviderErrorKind::Api,
                e.to_string(),
            )
        })?;

    let resp = client
        .post(TOKEN_ENDPOINT)
        .header("Accept", "application/json")
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", &client_id()),
            ("client_secret", &client_secret()),
            ("refresh_token", &stored.refresh),
        ])
        .send()
        .await
        .map_err(|e| {
            ProviderError::new(
                StatusCode::BAD_GATEWAY,
                ProviderErrorKind::Api,
                format!("Antigravity token refresh failed: {e}"),
            )
        })?;

    if !resp.status().is_success() {
        return Err(ProviderError::new(
            StatusCode::UNAUTHORIZED,
            ProviderErrorKind::Authentication,
            "Antigravity OAuth session expired or was revoked. Run `cc-proxy auth login antigravity`.",
        ));
    }

    let payload: Value = resp.json().await.map_err(|e| {
        ProviderError::new(
            StatusCode::BAD_GATEWAY,
            ProviderErrorKind::Api,
            format!("Invalid Antigravity token refresh response: {e}"),
        )
    })?;

    let access = payload["access_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ProviderError::new(
                StatusCode::BAD_GATEWAY,
                ProviderErrorKind::Api,
                "Antigravity token refresh response missing access_token",
            )
        })?
        .to_string();
    let refresh = payload["refresh_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| stored.refresh.clone());
    let expires_in = payload["expires_in"].as_u64().unwrap_or(3600);
    let updated = StoredAntigravityAuth {
        access,
        refresh,
        expires: (now_ms() + expires_in * 1000).saturating_sub(REFRESH_SKEW_MS),
        project_id: stored.project_id,
        email: stored.email,
        plan: stored.plan,
    };
    let _ = file_store().save(updated.clone());
    Ok(updated)
}

fn build_antigravity_envelope(
    body: &MessagesRequest,
    wire_model: &str,
    project_id: &str,
    session_id: Option<&str>,
) -> Result<Value> {
    let mut inner = google::prepare_request(body, google::ToolSchemas::OpenApi)?;
    if let Some(parts) = inner
        .pointer_mut("/systemInstruction/parts")
        .and_then(Value::as_array_mut)
    {
        for part in parts {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                let cleaned = text
                    .split("\n\n")
                    .filter(|p| *p != REJECTED_CLAUDE_SDK_PARAGRAPH)
                    .collect::<Vec<_>>()
                    .join("\n\n");
                part["text"] = Value::String(cleaned);
            }
        }
    }
    inner["sessionId"] = Value::String(antigravity_session_id(session_id));
    if wire_model.contains("claude") {
        inner["toolConfig"] = json!({
            "functionCallingConfig": {
                "mode": "VALIDATED"
            }
        });
    }
    Ok(json!({
        "model": wire_model,
        "userAgent": "antigravity",
        "requestType": "agent",
        "project": project_id,
        "requestId": format!("agent-{}", uuid::Uuid::new_v4()),
        "request": inner
    }))
}

pub struct AntigravityProvider;

impl Default for AntigravityProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl AntigravityProvider {
    pub fn new() -> Self {
        Self
    }

    async fn post_cca(
        &self,
        wire_model: &str,
        envelope: &Value,
        stream: bool,
        access_token: &str,
    ) -> Result<reqwest::Response, ProviderError> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| {
                ProviderError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ProviderErrorKind::Api,
                    e.to_string(),
                )
            })?;
        let action = if stream {
            "streamGenerateContent?alt=sse"
        } else {
            "generateContent"
        };
        let accept = if stream {
            "text/event-stream"
        } else {
            "application/json"
        };
        let ua = user_agent_for_wire_model(wire_model);
        let endpoints = [DAILY_API, PROD_API];
        let mut last_err = None;

        for (idx, base) in endpoints.iter().enumerate() {
            let url = format!("{base}/{API_VERSION}:{action}");
            let resp = client
                .post(&url)
                .header("Authorization", format!("Bearer {access_token}"))
                .header("Accept", accept)
                .header("Content-Type", "application/json")
                .header("User-Agent", ua)
                .json(envelope)
                .send()
                .await;
            match resp {
                Ok(r) if r.status().is_success() => return Ok(r),
                Ok(r) => {
                    let status = r.status();
                    let retry_after = r
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    let text = r.text().await.unwrap_or_default();
                    if idx == 0 && matches!(status.as_u16(), 403 | 404 | 500 | 502 | 503 | 504) {
                        last_err = Some(upstream_error::from_http(
                            status.as_u16(),
                            &text,
                            retry_after.as_deref(),
                            "Antigravity",
                        ));
                        continue;
                    }
                    return Err(upstream_error::from_http(
                        status.as_u16(),
                        &text,
                        retry_after.as_deref(),
                        "Antigravity",
                    ));
                }
                Err(err) => {
                    last_err = Some(ProviderError::new(
                        StatusCode::BAD_GATEWAY,
                        ProviderErrorKind::Api,
                        format!("Antigravity upstream request failed: {err}"),
                    ));
                }
            }
        }
        Err(last_err.unwrap_or_else(|| {
            ProviderError::new(
                StatusCode::BAD_GATEWAY,
                ProviderErrorKind::Api,
                "Antigravity upstream request failed",
            )
        }))
    }

    async fn buffered_messages_response(
        &self,
        body: MessagesRequest,
        ctx: RequestContext,
    ) -> Response {
        let requested = body.model.as_deref().unwrap_or_default();
        let Some(wire_model) = resolve_wire_model(requested) else {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("Unsupported Antigravity model: {requested}"),
            );
        };
        let auth = match load_valid_auth().await {
            Ok(auth) => auth,
            Err(err) => return err.response(),
        };
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, &wire_model);
            monitor.upstream_started(&ctx.req_id);
        }
        let envelope = match build_antigravity_envelope(
            &body,
            &wire_model,
            &auth.project_id,
            ctx.session_id.as_deref(),
        ) {
            Ok(env) => env,
            Err(err) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    err.to_string(),
                );
            }
        };
        let resp = match self
            .post_cca(&wire_model, &envelope, false, &auth.access)
            .await
        {
            Ok(r) => r,
            Err(err) => return err.response(),
        };
        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(err) => {
                return json_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    format!("Antigravity response read failed: {err}"),
                );
            }
        };
        capture_buffered_upstream(&ctx, &bytes, "json");
        let message_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        let value = match google::accumulate_response(&bytes, &message_id, requested) {
            Ok(v) => v,
            Err(err) => {
                return json_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    format!("Antigravity response translation failed: {err}"),
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
impl Provider for AntigravityProvider {
    fn name(&self) -> &'static str {
        "antigravity"
    }

    fn supported_models(&self) -> Vec<String> {
        advertised_models()
    }

    fn catalog(&self) -> Vec<String> {
        vec![
            "antigravity/gemini-4-argon".to_string(),
            "antigravity/gemini-3.1-pro".to_string(),
            "antigravity/gemini-3.8-flash".to_string(),
        ]
    }

    fn cli(&self) -> &dyn CliHandlers {
        &ANTIGRAVITY_CLI
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
        let Some(wire_model) = resolve_wire_model(requested) else {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("Unsupported Antigravity model: {requested}"),
            );
        };
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, &wire_model);
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
        let wire_model = resolve_wire_model(requested).ok_or_else(|| {
            ProviderError::new(
                StatusCode::BAD_REQUEST,
                ProviderErrorKind::InvalidRequest,
                format!("Unsupported Antigravity model: {requested}"),
            )
        })?;
        let auth = load_valid_auth().await?;
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, &wire_model);
            monitor.upstream_started(&ctx.req_id);
        }
        let envelope = build_antigravity_envelope(
            &body,
            &wire_model,
            &auth.project_id,
            ctx.session_id.as_deref(),
        )
        .map_err(|e| {
            ProviderError::new(
                StatusCode::BAD_REQUEST,
                ProviderErrorKind::InvalidRequest,
                e.to_string(),
            )
        })?;
        let resp = self
            .post_cca(&wire_model, &envelope, true, &auth.access)
            .await?;
        let message_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        let stream = google::stream_body(
            resp.bytes_stream().map(|c| c.map_err(|_| ())),
            "Antigravity",
            message_id,
            requested.to_string(),
            ctx.monitor.clone(),
            ctx.req_id.clone(),
            ctx.traffic.clone(),
        );
        Ok(Generation {
            body: GenerationBody::LiveSse(stream),
            resolved_model: wire_model,
        })
    }
}

struct AntigravityCli;

fn generate_pkce() -> (String, String) {
    let mut bytes = [0u8; 48];
    rand::thread_rng().fill_bytes(&mut bytes);
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    (verifier, challenge)
}

impl CliHandlers for AntigravityCli {
    fn auth_state(&self) -> AuthState {
        match file_store().load().ok().flatten() {
            Some(auth) => AuthState::SignedIn {
                account: auth.email.or(Some(auth.project_id)),
                expires_ms: Some(auth.expires),
            },
            None => AuthState::Missing,
        }
    }

    fn login(&self) -> Result<()> {
        let listener = TcpListener::bind(("127.0.0.1", CALLBACK_PORT))
            .or_else(|_| TcpListener::bind(("127.0.0.1", 0)))
            .context("Failed to bind local OAuth callback port for Antigravity")?;
        let port = listener.local_addr()?.port();
        let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
        let (verifier, challenge) = generate_pkce();
        let state = uuid::Uuid::new_v4().simple().to_string();

        let mut auth_url = reqwest::Url::parse(AUTH_ENDPOINT)?;
        auth_url
            .query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &client_id())
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("scope", &SCOPES.join(" "))
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("access_type", "offline")
            .append_pair("prompt", "consent")
            .append_pair("state", &state);

        println!("Open this URL in your browser to sign in to Google Antigravity:");
        println!("  {}", auth_url.as_str());
        let _ = crate::browser::open(auth_url.as_str());

        let (mut stream, _) = listener
            .accept()
            .context("Failed waiting for OAuth callback")?;
        let mut reader = BufReader::new(&stream);
        let mut request_line = String::new();
        reader.read_line(&mut request_line)?;
        let path = request_line
            .split_whitespace()
            .nth(1)
            .ok_or_else(|| anyhow::anyhow!("Malformed HTTP callback request"))?;
        let callback_url = reqwest::Url::parse(&format!("http://127.0.0.1{path}"))?;
        let mut code = None;
        let mut returned_state = None;
        for (k, v) in callback_url.query_pairs() {
            if k == "code" {
                code = Some(v.into_owned());
            } else if k == "state" {
                returned_state = Some(v.into_owned());
            }
        }
        let html = "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n<html><body><h2>Antigravity sign-in complete.</h2><p>You can close this tab and return to the terminal.</p></body></html>";
        let _ = stream.write_all(html.as_bytes());
        let _ = stream.flush();

        if returned_state.as_deref() != Some(state.as_str()) {
            bail!("Antigravity OAuth state mismatch");
        }
        let code = code.ok_or_else(|| anyhow::anyhow!("Missing authorization code in callback"))?;

        let http = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()?;
        let token_resp: Value = http
            .post(TOKEN_ENDPOINT)
            .header("Accept", "application/json")
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", &client_id()),
                ("client_secret", &client_secret()),
                ("code", &code),
                ("redirect_uri", &redirect_uri),
                ("code_verifier", &verifier),
            ])
            .send()?
            .error_for_status()
            .context("Antigravity token exchange failed")?
            .json()?;

        let access = token_resp["access_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing access_token from Google"))?
            .to_string();
        let refresh = token_resp["refresh_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing refresh_token from Google"))?
            .to_string();
        let expires_in = token_resp["expires_in"].as_u64().unwrap_or(3600);
        let email = token_resp["id_token"]
            .as_str()
            .and_then(decode_jwt_email)
            .or_else(|| decode_jwt_email(&access));

        println!("Discovering Cloud Code Assist project...");
        let (project_id, plan) = discover_project_blocking(&http, &access)?;
        let stored = StoredAntigravityAuth {
            access,
            refresh,
            expires: (now_ms() + expires_in * 1000).saturating_sub(REFRESH_SKEW_MS),
            project_id: project_id.clone(),
            email: email.clone(),
            plan,
        };
        file_store().save(stored)?;
        if let Some(email) = email {
            println!("Signed in to Antigravity as {email} (project: {project_id}).");
        } else {
            println!("Signed in to Antigravity (project: {project_id}).");
        }
        Ok(())
    }

    fn device(&self) -> Result<()> {
        self.login()
    }

    fn status(&self) -> Result<()> {
        let Some(stored) = file_store().load()? else {
            bail!("Not authenticated");
        };
        if let Some(email) = &stored.email {
            println!("Account: {email}");
        }
        println!("Project: {}", stored.project_id);
        if let Some(plan) = &stored.plan {
            println!("Plan: {plan}");
        }
        println!("Storage: {}", file_store().path());
        Ok(())
    }

    fn logout(&self) -> Result<()> {
        file_store().clear()?;
        println!("Antigravity stored auth cleared.");
        Ok(())
    }
}

static ANTIGRAVITY_CLI: AntigravityCli = AntigravityCli;
