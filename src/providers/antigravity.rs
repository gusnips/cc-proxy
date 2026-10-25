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

use crate::accounts::{AccountAuthStore, Accounts};
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
use crate::providers::translate_shared::read_effort;
use crate::providers::upstream_error;
use crate::registry::normalize_incoming_model;
use crate::rotation::RotationHint;

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

pub(crate) const IDE_USER_AGENT: &str =
    "antigravity/ide/2.5.5 (os_type=windows; arch=amd64; aidev_client; auth_method=oauth)";
const CLI_USER_AGENT: &str =
    "antigravity/cli/1.3.4 (os_type=darwin; arch=arm64; aidev_client; auth_method=oauth)";

const REJECTED_CLAUDE_SDK_PARAGRAPH: &str =
    "You are a Claude agent, built on Anthropic's Claude Agent SDK.";

/// Fallback wire models used before `:fetchAvailableModels` has been saved.
pub const ANTIGRAVITY_FALLBACK_MODELS: &[&str] = &[
    "gemini-4-argon-medium",
    "gemini-4-argon-high",
    "gemini-4-argon-low",
    "gemini-3.8-flash-high",
    "gemini-3.8-flash-medium",
    "gemini-3.8-flash-low",
    "claude-opus-5-5-low",
    "claude-opus-5-5-medium",
    "claude-opus-5-5-high",
    "claude-sonnet-5-5-low",
    "claude-sonnet-5-5-medium",
    "claude-sonnet-5-5-high",
    "gpt-oss-120b-medium",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// The plan id Google gives an account that pays nothing.
const FREE_PLAN: &str = "free-tier";

/// A plan id in words. Only the free plan is named: Google's other ids are
/// shown as it spells them, because what each one includes keeps changing.
fn plan_label(plan: &str) -> String {
    if plan == FREE_PLAN {
        "free plan".to_string()
    } else {
        format!("{plan} plan")
    }
}

fn account_store(id: Option<&str>) -> Result<AccountAuthStore<StoredAntigravityAuth>> {
    Accounts::open("antigravity", &ANTIGRAVITY_CLI)?.for_request(id)
}

fn single_login_store() -> FileAuthStore<Value> {
    FileAuthStore::new(
        paths::provider_auth_file("antigravity")
            .to_string_lossy()
            .into_owned(),
        paths::provider_legacy_auth_file("antigravity")
            .to_string_lossy()
            .into_owned(),
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
    if wire_model.starts_with("gemini-") && wire_model.split('-').any(|word| word == "argon") {
        CLI_USER_AGENT
    } else {
        IDE_USER_AGENT
    }
}

fn wire_models() -> Vec<String> {
    super::model_catalog::load("antigravity").unwrap_or_else(|| {
        ANTIGRAVITY_FALLBACK_MODELS
            .iter()
            .map(|model| (*model).to_string())
            .collect()
    })
}

/// Collapses `-low` / `-medium` / `-high` wire families into their base model
/// id (`gemini-4-argon`, `claude-opus-5-5`) when all three tiers exist.
fn collapsed_models(wire_ids: &[String]) -> Vec<String> {
    let set: std::collections::HashSet<&str> = wire_ids.iter().map(String::as_str).collect();
    let mut out = Vec::new();
    for id in wire_ids {
        let base = ["low", "medium", "high"]
            .into_iter()
            .find_map(|tier| id.strip_suffix(&format!("-{tier}")));
        let candidate = match base {
            Some(stem)
                if ["low", "medium", "high"]
                    .into_iter()
                    .all(|tier| set.contains(format!("{stem}-{tier}").as_str())) =>
            {
                stem.to_string()
            }
            _ => id.clone(),
        };
        if !out.contains(&candidate) {
            out.push(candidate);
        }
    }
    out
}

pub fn advertised_models() -> Vec<String> {
    let wire = wire_models();
    let mut out: Vec<String> = collapsed_models(&wire)
        .into_iter()
        .chain(wire)
        .map(|m| format!("{MODEL_PREFIX}{m}"))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

pub fn catalog_models() -> Vec<String> {
    let mut out: Vec<String> = collapsed_models(&wire_models())
        .into_iter()
        .map(|m| format!("{MODEL_PREFIX}{m}"))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Parses `:fetchAvailableModels` JSON (`agentModelSorts[].groups[].modelIds`)
/// into the ordered list of agent wire model IDs.
fn parse_available_models(body: &Value) -> Vec<String> {
    let Some(models) = body.get("models").and_then(Value::as_object) else {
        return Vec::new();
    };
    let Some(sorts) = body.get("agentModelSorts").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for sort in sorts {
        let Some(groups) = sort.get("groups").and_then(Value::as_array) else {
            continue;
        };
        for group in groups {
            let Some(ids) = group.get("modelIds").and_then(Value::as_array) else {
                continue;
            };
            for id in ids.iter().filter_map(Value::as_str) {
                if !id.is_empty()
                    && models.contains_key(id)
                    && !out.iter().any(|existing| existing == id)
                {
                    out.push(id.to_string());
                }
            }
        }
    }
    out
}

fn save_refreshed_auth(
    store: &dyn AuthStorage<StoredAntigravityAuth>,
    stored: StoredAntigravityAuth,
    payload: &Value,
) -> Result<StoredAntigravityAuth, ProviderError> {
    let access = payload["access_token"]
        .as_str()
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| {
            ProviderError::new(
                StatusCode::BAD_GATEWAY,
                ProviderErrorKind::Api,
                "Antigravity token refresh response has no access token. Try the request again.",
            )
        })?
        .to_string();
    let refresh = payload["refresh_token"]
        .as_str()
        .filter(|token| !token.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| stored.refresh.clone());
    let expires_in = payload["expires_in"].as_u64().unwrap_or(3600);
    if expires_in == 0 {
        return Err(ProviderError::new(
            StatusCode::BAD_GATEWAY,
            ProviderErrorKind::Api,
            "Antigravity returned an expired access token. Try the request again.",
        ));
    }
    let updated = StoredAntigravityAuth {
        access,
        refresh,
        expires: now_ms()
            .saturating_add(expires_in.saturating_mul(1000))
            .saturating_sub(REFRESH_SKEW_MS),
        ..stored
    };
    store.save(updated.clone()).map_err(auth_storage_error)?;
    Ok(updated)
}

fn auth_storage_error(error: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        ProviderErrorKind::Api,
        format!("Couldn't read or save the Antigravity sign-in: {error}"),
    )
}

fn read_auth(
    store: &dyn AuthStorage<StoredAntigravityAuth>,
) -> Result<StoredAntigravityAuth, ProviderError> {
    store.load().map_err(auth_storage_error)?.ok_or_else(|| {
        ProviderError::new(
            StatusCode::UNAUTHORIZED,
            ProviderErrorKind::Authentication,
            "Antigravity is not signed in. Run `cc-proxy auth login antigravity`.",
        )
    })
}

fn ensure_valid_auth_blocking(
    store: &dyn AuthStorage<StoredAntigravityAuth>,
    token_endpoint: &str,
) -> Result<StoredAntigravityAuth, ProviderError> {
    let stored = read_auth(store)?;
    if stored.expires > now_ms() {
        return Ok(stored);
    }
    let lock = crate::auth::refresh_lock(&store.path());
    let _guard = lock.blocking_lock();
    let stored = read_auth(store)?;
    if stored.expires > now_ms() {
        return Ok(stored);
    }
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(auth_storage_error)?;
    let response = client
        .post(token_endpoint)
        .header("Accept", "application/json")
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", &client_id()),
            ("client_secret", &client_secret()),
            ("refresh_token", &stored.refresh),
        ])
        .send()
        .map_err(|error| ProviderError::new(
            StatusCode::BAD_GATEWAY,
            ProviderErrorKind::Api,
            format!("Antigravity token refresh failed: {error}. Check your connection and try again."),
        ))?;
    if !response.status().is_success() {
        // A peer process may have renewed this login during the HTTP request.
        let latest = read_auth(store)?;
        if latest != stored {
            return Ok(latest);
        }
        return Err(ProviderError::new(
            StatusCode::UNAUTHORIZED,
            ProviderErrorKind::Authentication,
            "Antigravity OAuth session expired or was revoked. Run `cc-proxy auth login antigravity --replace`.",
        ));
    }
    let payload: Value = response.json().map_err(|error| ProviderError::new(
        StatusCode::BAD_GATEWAY,
        ProviderErrorKind::Api,
        format!("Antigravity token refresh response was not valid JSON: {error}. Try the request again."),
    ))?;
    save_refreshed_auth(store, stored, &payload)
}

/// Queries Antigravity's `:fetchAvailableModels` endpoint and saves the live
/// agent wire model list to `antigravity/models.json`.
pub fn refresh_models() -> Result<Vec<String>> {
    let store = account_store(None)?;
    let auth = ensure_valid_auth_blocking(&store, TOKEN_ENDPOINT)?;
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()?;
    let mut last_err = None;
    for base in [DAILY_API, PROD_API] {
        let resp = client
            .post(format!("{base}/{API_VERSION}:fetchAvailableModels"))
            .header("Authorization", format!("Bearer {}", auth.access))
            .header("Content-Type", "application/json")
            .header("User-Agent", CLI_USER_AGENT)
            .json(&json!({ "project": auth.project_id }))
            .send();
        match resp {
            Ok(r) if r.status().is_success() => {
                let body: Value = r
                    .json()
                    .context("Antigravity model list response was not JSON")?;
                let wire_ids = parse_available_models(&body);
                if wire_ids.is_empty() {
                    bail!("Antigravity returned no agent models for this account");
                }
                super::model_catalog::save("antigravity", &wire_ids)?;
                return Ok(collapsed_models(&wire_ids));
            }
            Ok(r) => {
                last_err = Some(anyhow::anyhow!(
                    "Antigravity refused the model list request ({})",
                    r.status()
                ));
            }
            Err(e) => {
                last_err = Some(anyhow::Error::from(e));
            }
        }
    }
    Err(last_err
        .unwrap_or_else(|| anyhow::anyhow!("Couldn't reach Antigravity for its model list")))
}

fn resolve_wire_model(body: &MessagesRequest) -> Result<String, ProviderError> {
    let requested = body.model.as_deref().unwrap_or_default();
    let effort = read_effort(body).map_err(|error| {
        ProviderError::new(
            StatusCode::BAD_REQUEST,
            ProviderErrorKind::InvalidRequest,
            error.to_string(),
        )
    })?;
    wire_model(requested, effort, &wire_models()).ok_or_else(|| {
        ProviderError::new(
            StatusCode::BAD_REQUEST,
            ProviderErrorKind::InvalidRequest,
            format!("Invalid Antigravity model: {requested}. Use `antigravity/<model>` from `cc-proxy models list antigravity`."),
        )
    })
}

/// Effort suffixes come from discovery, never from guessing a model's name.
fn wire_model(raw: &str, effort: Option<&str>, known: &[String]) -> Option<String> {
    let normalized = normalize_incoming_model(raw);
    let id = normalized
        .strip_prefix(MODEL_PREFIX)
        .filter(|id| !id.is_empty())?;
    let default = if id.split('-').any(|word| word == "flash") {
        "medium"
    } else {
        "high"
    };
    let tier = match effort {
        Some("none" | "low") => "low",
        Some("medium") => "medium",
        Some("high" | "xhigh" | "max") => "high",
        _ => default,
    };
    let base = id
        .rsplit_once('-')
        .filter(|(_, suffix)| matches!(*suffix, "low" | "medium" | "high"))
        .map(|(base, _)| base);
    if let Some(base) = base {
        if effort.is_some() {
            let candidate = format!("{base}-{tier}");
            if known.contains(&candidate) {
                return Some(candidate);
            }
        }
        return Some(id.to_string());
    }
    if known.iter().any(|model| model == id) {
        return Some(id.to_string());
    }
    for suffix in [tier, default, "medium", "low", "high"] {
        let candidate = format!("{id}-{suffix}");
        if known.contains(&candidate) {
            return Some(candidate);
        }
    }
    Some(id.to_string())
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

pub(crate) async fn load_valid_auth(
    account: Option<&str>,
) -> Result<StoredAntigravityAuth, ProviderError> {
    let store = account_store(account).map_err(auth_storage_error)?;
    load_valid_auth_from_store(store, TOKEN_ENDPOINT.to_string()).await
}

async fn load_valid_auth_from_store(
    store: impl AuthStorage<StoredAntigravityAuth> + 'static,
    token_endpoint: String,
) -> Result<StoredAntigravityAuth, ProviderError> {
    let stored = read_auth(&store)?;
    if stored.expires > now_ms() {
        return Ok(stored);
    }
    tokio::task::spawn_blocking(move || ensure_valid_auth_blocking(&store, &token_endpoint))
        .await
        .map_err(auth_storage_error)?
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

/// Fill in the reset time Google's 429 left out. The quota endpoints know it,
/// which is also the only way to tell a spent window from a short throttle:
/// only a bucket at zero means the request waits.
async fn with_quota_reset(
    mut error: ProviderError,
    client: &reqwest::Client,
    auth: &StoredAntigravityAuth,
    wire_model: &str,
) -> ProviderError {
    if error.status != StatusCode::TOO_MANY_REQUESTS
        || matches!(error.rotation_hint, Some(RotationHint::RetryAt { .. }))
    {
        return error;
    }
    let until = match family_reset(client, auth, wire_model).await {
        Some(until) => Some(until),
        None => quota_reset(client, auth, wire_model).await,
    };
    let Some(until_ms) = until else {
        return error;
    };
    error.rotation_hint = Some(RotationHint::RetryAt {
        until_ms,
        per_model: true,
    });
    let when = jiff::Timestamp::from_millisecond(until_ms as i64)
        .map(|reset| {
            crate::usage::format_reset(reset, jiff::Timestamp::now(), &jiff::tz::TimeZone::system())
        })
        .unwrap_or_else(|_| "later".to_string());
    error.message = format!(
        "Antigravity is out of quota for {wire_model}, {when}. Pick another model or wait."
    );
    error
}

/// The models that share one allowance. Google keeps a 5-hour and a weekly
/// bucket for each family, not for each model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Family {
    Gemini,
    /// Claude and the GPT-OSS models share one group.
    Claude,
}

impl Family {
    pub(crate) fn of_model(wire_model: &str) -> Self {
        if wire_model.contains("gemini") {
            Self::Gemini
        } else {
            Self::Claude
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Gemini => "Gemini",
            Self::Claude => "Claude",
        }
    }

    /// Short labels for the monitor: the 5-hour and the weekly window.
    pub(crate) fn short_labels(self) -> [&'static str; 2] {
        match self {
            Self::Gemini => ["Gemini 5h", "Gemini week"],
            Self::Claude => ["Claude 5h", "Claude week"],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Bucket {
    pub family: Family,
    pub weekly: bool,
    pub remaining_fraction: f64,
    pub reset_ms: Option<i64>,
}

/// The request that asks how much of each allowance is left.
pub(crate) fn quota_request(
    client: &reqwest::Client,
    auth: &StoredAntigravityAuth,
    user_agent: &str,
) -> reqwest::RequestBuilder {
    client
        .post(format!(
            "{DAILY_API}/{API_VERSION}:retrieveUserQuotaSummary"
        ))
        .bearer_auth(&auth.access)
        .header("User-Agent", user_agent)
        .json(&json!({ "project": auth.project_id }))
}

/// Reads `:retrieveUserQuotaSummary`: `groups[]` of models, each with 5-hour
/// and weekly `buckets[]`. A group or bucket that can't be read is left out.
pub(crate) fn parse_quota_summary(body: &Value) -> Vec<Bucket> {
    let text = |value: &Value, keys: &[&str]| {
        keys.iter()
            .filter_map(|key| value.get(*key).and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    };
    let groups = body.get("groups").and_then(Value::as_array);
    let mut buckets = Vec::new();
    for group in groups.into_iter().flatten() {
        let name = text(group, &["displayName", "description"]);
        let family = if name.contains("gemini") {
            Family::Gemini
        } else if ["claude", "3p", "gpt"]
            .iter()
            .any(|word| name.contains(word))
        {
            Family::Claude
        } else {
            continue;
        };
        let group_buckets = group.get("buckets").and_then(Value::as_array);
        for bucket in group_buckets.into_iter().flatten() {
            let label = text(bucket, &["window", "bucketId", "displayName"]);
            let weekly = label.contains("week");
            if !weekly && !label.contains("5h") && !label.contains("five") {
                continue;
            }
            let Some(remaining) = bucket
                .get("remainingFraction")
                .or_else(|| bucket.pointer("/remaining/remainingFraction"))
                .and_then(Value::as_f64)
            else {
                continue;
            };
            buckets.push(Bucket {
                family,
                weekly,
                remaining_fraction: remaining.clamp(0.0, 1.0),
                reset_ms: bucket
                    .get("resetTime")
                    .and_then(Value::as_str)
                    .and_then(|time| time.parse::<jiff::Timestamp>().ok())
                    .map(|time| time.as_millisecond()),
            });
        }
    }
    buckets
}

/// When the family of `wire_model` comes back, if one of its windows is spent.
async fn family_reset(
    client: &reqwest::Client,
    auth: &StoredAntigravityAuth,
    wire_model: &str,
) -> Option<u64> {
    let response = quota_request(client, auth, user_agent_for_wire_model(wire_model))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: Value = response.json().await.ok()?;
    spent_until(
        &parse_quota_summary(&body),
        Family::of_model(wire_model),
        now_ms(),
    )
}

/// The latest reset among the family's spent windows: the request can only
/// go through once every spent window is back.
fn spent_until(buckets: &[Bucket], family: Family, now: u64) -> Option<u64> {
    buckets
        .iter()
        .filter(|bucket| bucket.family == family && bucket.remaining_fraction <= 0.0)
        .filter_map(|bucket| bucket.reset_ms.and_then(|ms| u64::try_from(ms).ok()))
        .filter(|until| *until > now)
        .max()
}

/// When `wire_model` comes back, from Google's per-model quota endpoint, for
/// accounts the summary doesn't cover. There is one
/// bucket per model; a bucket with quota left means the 429 was a short
/// throttle, not a spent window.
async fn quota_reset(
    client: &reqwest::Client,
    auth: &StoredAntigravityAuth,
    wire_model: &str,
) -> Option<u64> {
    let response = client
        .post(format!("{PROD_API}/{API_VERSION}:retrieveUserQuota"))
        .header("Authorization", format!("Bearer {}", auth.access))
        .header("Content-Type", "application/json")
        .header("User-Agent", user_agent_for_wire_model(wire_model))
        .timeout(Duration::from_secs(5))
        .json(&json!({ "project": auth.project_id }))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: Value = response.json().await.ok()?;
    quota_reset_from(&body, wire_model, now_ms())
}

/// The `resetTime` of the spent bucket for `wire_model`, in epoch ms. A
/// bucket with quota left means a short throttle, so it never counts.
fn quota_reset_from(body: &Value, wire_model: &str, now: u64) -> Option<u64> {
    let bucket = body.get("buckets")?.as_array()?.iter().find(|bucket| {
        bucket.get("modelId").and_then(Value::as_str) == Some(wire_model)
            && bucket
                .get("remainingFraction")
                .and_then(Value::as_f64)
                .is_some_and(|left| left <= 0.0)
    })?;
    bucket
        .get("resetTime")?
        .as_str()?
        .parse::<jiff::Timestamp>()
        .ok()
        .and_then(|at| u64::try_from(at.as_millisecond()).ok())
        .filter(|until| *until > now)
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
        auth: &StoredAntigravityAuth,
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
                .header("Authorization", format!("Bearer {}", auth.access))
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
                    let error = upstream_error::from_http(
                        status.as_u16(),
                        &text,
                        retry_after.as_deref(),
                        "Antigravity",
                    );
                    return Err(with_quota_reset(error, &client, auth, wire_model).await);
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
        mut ctx: RequestContext,
    ) -> Response {
        if let Err(error) = ctx.pin_account(&ANTIGRAVITY_CLI) {
            return error.response();
        }
        let requested = body.model.as_deref().unwrap_or_default();
        let wire_model = match resolve_wire_model(&body) {
            Ok(model) => model,
            Err(error) => return error.response(),
        };
        let auth = match load_valid_auth(ctx.account.as_deref()).await {
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
        let resp = match self.post_cca(&wire_model, &envelope, false, &auth).await {
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
        let value = match google::accumulate_response("Antigravity", &bytes, &message_id, requested)
        {
            Ok(v) => v,
            Err(err) => {
                return upstream_error::translation_error(
                    err,
                    "Antigravity response translation failed",
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
impl Provider for AntigravityProvider {
    fn name(&self) -> &'static str {
        "antigravity"
    }

    fn supported_models(&self) -> Vec<String> {
        advertised_models()
    }

    fn catalog(&self) -> Vec<String> {
        catalog_models()
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
        let wire_model = match resolve_wire_model(&body) {
            Ok(model) => model,
            Err(error) => return error.response(),
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
        mut ctx: RequestContext,
    ) -> Result<Generation, ProviderError> {
        ctx.pin_account(&ANTIGRAVITY_CLI)?;
        body.stream = true;
        let requested = body.model.as_deref().unwrap_or_default();
        let wire_model = resolve_wire_model(&body)?;
        let auth = load_valid_auth(ctx.account.as_deref()).await?;
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
        let resp = self.post_cca(&wire_model, &envelope, true, &auth).await?;
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

pub(crate) struct AntigravityCli;

fn generate_pkce() -> (String, String) {
    let mut bytes = [0u8; 48];
    rand::thread_rng().fill_bytes(&mut bytes);
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    (verifier, challenge)
}

fn sign_in() -> Result<StoredAntigravityAuth> {
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
    Ok(StoredAntigravityAuth {
        access,
        refresh,
        expires: now_ms()
            .saturating_add(expires_in.saturating_mul(1000))
            .saturating_sub(REFRESH_SKEW_MS),
        project_id,
        email,
        plan,
    })
}

impl crate::accounts::AccountSupport for AntigravityCli {
    fn sign_in(&self, _device: bool) -> Result<Value> {
        Ok(serde_json::to_value(sign_in()?)?)
    }

    fn identity(&self, login: &Value) -> Option<String> {
        let auth: StoredAntigravityAuth = serde_json::from_value(login.clone()).ok()?;
        auth.email
            .map(|email| email.trim().to_ascii_lowercase())
            .filter(|email| !email.is_empty())
            .or_else(|| (!auth.project_id.is_empty()).then_some(auth.project_id))
    }

    fn email(&self, login: &Value) -> Option<String> {
        let auth: StoredAntigravityAuth = serde_json::from_value(login.clone()).ok()?;
        auth.email
    }

    fn state(&self, login: &Value) -> AuthState {
        match serde_json::from_value::<StoredAntigravityAuth>(login.clone()) {
            Ok(auth) => {
                let who = auth.email.unwrap_or(auth.project_id);
                AuthState::SignedIn {
                    account: Some(match auth.plan.as_deref() {
                        Some(plan) => format!("{who} ({})", plan_label(plan)),
                        None => who,
                    }),
                    expires_ms: Some(auth.expires),
                }
            }
            Err(_) => AuthState::Missing,
        }
    }

    fn note(&self, login: &Value) -> Option<String> {
        let auth: StoredAntigravityAuth = serde_json::from_value(login.clone()).ok()?;
        (auth.plan.as_deref() == Some(FREE_PLAN)).then(|| {
            "Free plan. The numbers above are a weekly allowance, not a list of the models this plan can use. Google can refuse a model with \"Resource has been exhausted\" while this still shows 100%.".to_string()
        })
    }

    fn single_login(&self) -> Box<dyn AuthStorage<Value>> {
        Box::new(single_login_store())
    }
}

impl CliHandlers for AntigravityCli {
    fn auth_state(&self) -> AuthState {
        Accounts::current_state("antigravity", self)
    }

    fn login(&self) -> Result<()> {
        crate::auth_cli::AccountCli::open("antigravity", self)?.login(&Default::default())
    }

    fn device(&self) -> Result<()> {
        crate::auth_cli::AccountCli::open("antigravity", self)?.login(
            &crate::auth_cli::LoginOptions {
                device: true,
                ..Default::default()
            },
        )
    }

    fn status(&self) -> Result<()> {
        crate::auth_cli::AccountCli::open("antigravity", self)?.status()
    }

    fn logout(&self) -> Result<()> {
        crate::auth_cli::AccountCli::open("antigravity", self)?.logout(None, false)
    }

    fn accounts(&self) -> Option<&dyn crate::accounts::AccountSupport> {
        Some(self)
    }
}

pub(crate) static ANTIGRAVITY_CLI: AntigravityCli = AntigravityCli;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::AccountSupport;
    use crate::auth::InMemoryAuthStore;
    use crate::providers::codex::auth::test_http;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn stored_auth(name: &str) -> StoredAntigravityAuth {
        StoredAntigravityAuth {
            access: format!("access-{name}"),
            refresh: format!("refresh-{name}"),
            expires: 0,
            project_id: format!("project-{name}"),
            email: Some(format!("{name}@example.com")),
            plan: Some("test-plan".into()),
        }
    }

    #[test]
    fn the_plan_shows_beside_the_account_and_a_free_one_gets_a_note() {
        let mut login = serde_json::to_value(stored_auth("first")).unwrap();
        let state = |login: &Value| ANTIGRAVITY_CLI.state(login).describe();
        assert!(state(&login).starts_with("signed in as first@example.com (test-plan plan)"));
        assert_eq!(ANTIGRAVITY_CLI.note(&login), None);

        login["plan"] = json!("free-tier");
        assert!(state(&login).starts_with("signed in as first@example.com (free plan)"));
        let note = ANTIGRAVITY_CLI.note(&login).unwrap();
        for expected in [
            "Free plan",
            "weekly allowance",
            "Resource has been exhausted",
            "100%",
        ] {
            assert!(note.contains(expected), "{expected}: {note}");
        }

        // A login with no plan says nothing about one.
        login["plan"] = Value::Null;
        assert!(state(&login).starts_with("signed in as first@example.com,"));
        assert_eq!(ANTIGRAVITY_CLI.note(&login), None);
    }

    #[test]
    fn account_email_comes_from_metadata_not_the_project() {
        let mut login = serde_json::to_value(stored_auth("first")).unwrap();
        assert_eq!(
            ANTIGRAVITY_CLI.email(&login).as_deref(),
            Some("first@example.com")
        );
        login["email"] = Value::Null;
        login["projectId"] = json!("project@example.com");
        assert_eq!(ANTIGRAVITY_CLI.email(&login), None);
        assert_eq!(
            ANTIGRAVITY_CLI.identity(&login).as_deref(),
            Some("project@example.com")
        );
    }

    #[test]
    fn account_migration_keeps_the_identity_and_project() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = Accounts::in_dir("antigravity", dir.path().join("antigravity"));
        let legacy = InMemoryAuthStore::new();
        let auth = stored_auth("first");
        legacy.save(serde_json::to_value(&auth).unwrap()).unwrap();
        accounts
            .migrate(&legacy, |login| ANTIGRAVITY_CLI.identity(login))
            .unwrap();
        let current = accounts.current().unwrap().unwrap();
        assert_eq!(current.identity.as_deref(), Some("first@example.com"));
        assert_eq!(
            accounts
                .store::<StoredAntigravityAuth>(&current.id)
                .load()
                .unwrap()
                .unwrap(),
            auth
        );
        assert!(legacy.load().unwrap().is_none());
        assert!(ANTIGRAVITY_CLI.accounts().is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn runtime_and_model_refresh_share_only_the_selected_accounts_lock() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = Accounts::in_dir("antigravity", dir.path().join("antigravity"));
        for name in ["first", "second"] {
            accounts
                .install(
                    name,
                    None,
                    &serde_json::to_value(stored_auth(name)).unwrap(),
                    None,
                )
                .unwrap();
        }
        let first_id = accounts.require("first").unwrap().id;
        let second_id = accounts.require("second").unwrap().id;
        let refreshes = Arc::new(AtomicUsize::new(0));
        let calls = refreshes.clone();
        let server = test_http::spawn_mock_server("antigravity oauth mock ready", move |request| {
            calls.fetch_add(1, Ordering::SeqCst);
            let name = if request.contains("refresh_token=refresh-first") {
                "first"
            } else {
                "second"
            };
            test_http::json_response(
                200,
                &format!(
                    r#"{{"access_token":"rotated-{name}","refresh_token":"new-refresh-{name}","expires_in":3600}}"#
                ),
            )
        });
        let endpoint = server.url.clone();
        let blocking_store = accounts.store(&first_id);
        let blocking = tokio::task::spawn_blocking(move || {
            ensure_valid_auth_blocking(&blocking_store, &endpoint)
        });
        let (runtime_auth, blocking_auth) = tokio::join!(
            load_valid_auth_from_store(accounts.store(&first_id), server.url.clone()),
            blocking,
        );
        let runtime_auth = runtime_auth.unwrap();
        assert_eq!(runtime_auth, blocking_auth.unwrap().unwrap());
        assert_eq!(runtime_auth.access, "rotated-first");
        assert_eq!(runtime_auth.project_id, "project-first");
        assert_eq!(runtime_auth.email.as_deref(), Some("first@example.com"));
        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        assert_eq!(
            accounts
                .store::<StoredAntigravityAuth>(&second_id)
                .load()
                .unwrap()
                .unwrap()
                .refresh,
            "refresh-second"
        );

        let other = load_valid_auth_from_store(accounts.store(&second_id), server.url.clone())
            .await
            .unwrap();
        assert_eq!(other.access, "rotated-second");
        assert_eq!(other.project_id, "project-second");
        assert_eq!(
            accounts
                .store::<StoredAntigravityAuth>(&first_id)
                .load()
                .unwrap()
                .unwrap(),
            runtime_auth
        );
        assert_eq!(refreshes.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_late_refresh_cannot_overwrite_a_new_sign_in() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = Accounts::in_dir("antigravity", dir.path().join("antigravity"));
        accounts
            .install(
                "main",
                None,
                &serde_json::to_value(stored_auth("first")).unwrap(),
                None,
            )
            .unwrap();
        let id = accounts.require("main").unwrap().id;
        let store = accounts.store(&id);
        let server_accounts = accounts.clone();
        let server = test_http::spawn_mock_server("antigravity oauth mock ready", move |_| {
            let replacement = StoredAntigravityAuth {
                expires: u64::MAX,
                ..stored_auth("replacement")
            };
            server_accounts
                .install(
                    "main",
                    Some(&id),
                    &serde_json::to_value(replacement).unwrap(),
                    None,
                )
                .unwrap();
            test_http::json_response(
                200,
                r#"{"access_token":"too-late","refresh_token":"too-late-refresh","expires_in":3600}"#,
            )
        });
        let error = load_valid_auth_from_store(store, server.url.clone())
            .await
            .unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::Api);
        assert!(error.message.contains("signed out or replaced"));
        let current = accounts.require("main").unwrap();
        assert_eq!(
            accounts
                .store::<StoredAntigravityAuth>(&current.id)
                .load()
                .unwrap()
                .unwrap()
                .access,
            "access-replacement"
        );
    }

    #[test]
    fn invalid_refresh_responses_leave_the_saved_login_unchanged() {
        let store = InMemoryAuthStore::new();
        let auth = stored_auth("first");
        store.save(auth.clone()).unwrap();
        for payload in [json!({}), json!({"access_token": "new", "expires_in": 0})] {
            assert!(save_refreshed_auth(&store, auth.clone(), &payload).is_err());
            assert_eq!(store.load().unwrap(), Some(auth.clone()));
        }
    }

    #[test]
    fn parse_and_collapse_available_models_matches_cca_response() {
        let body = json!({
            "models": {
                "gemini-4-argon-low": {},
                "gemini-4-argon-medium": {},
                "gemini-4-argon-high": {},
                "gemini-3.8-flash-low": {},
                "gemini-3.8-flash-medium": {},
                "gemini-3.8-flash-high": {},
                "claude-opus-5-5-low": {},
                "claude-opus-5-5-medium": {},
                "claude-opus-5-5-high": {},
                "claude-sonnet-5-5-low": {},
                "claude-sonnet-5-5-medium": {},
                "claude-sonnet-5-5-high": {},
                "gpt-oss-120b-medium": {},
                "tab_flash_lite_preview": {}
            },
            "agentModelSorts": [{
                "displayName": "Recommended",
                "groups": [{
                    "modelIds": [
                        "gemini-4-argon-medium",
                        "gemini-4-argon-high",
                        "gemini-4-argon-low",
                        "gemini-3.8-flash-high",
                        "gemini-3.8-flash-medium",
                        "gemini-3.8-flash-low",
                        "claude-opus-5-5-low",
                        "claude-opus-5-5-medium",
                        "claude-opus-5-5-high",
                        "claude-sonnet-5-5-low",
                        "claude-sonnet-5-5-medium",
                        "claude-sonnet-5-5-high",
                        "gpt-oss-120b-medium"
                    ]
                }]
            }]
        });
        let wire = parse_available_models(&body);
        assert_eq!(
            collapsed_models(&wire),
            [
                "gemini-4-argon",
                "gemini-3.8-flash",
                "claude-opus-5-5",
                "claude-sonnet-5-5",
                "gpt-oss-120b-medium"
            ]
        );
    }

    #[test]
    fn discovered_families_resolve_without_inventing_effort_variants() {
        let known = [
            "future-9-low",
            "future-9-medium",
            "future-9-high",
            "only-medium",
            "plain",
        ]
        .map(str::to_string);
        assert_eq!(
            wire_model("antigravity/future-9[1m]", Some("max"), &known).as_deref(),
            Some("future-9-high")
        );
        assert_eq!(
            wire_model("antigravity/only-medium", Some("max"), &known).as_deref(),
            Some("only-medium")
        );
        assert_eq!(
            wire_model("antigravity/plain", Some("high"), &known).as_deref(),
            Some("plain")
        );
        assert_eq!(
            wire_model("antigravity/claude-future", Some("high"), &known).as_deref(),
            Some("claude-future")
        );
        assert_eq!(wire_model("antigravity/", None, &known), None);
        assert_eq!(wire_model("gemini-9", None, &known), None);
        assert_eq!(
            collapsed_models(&["partial-low".into(), "partial-medium".into()]),
            ["partial-low", "partial-medium"]
        );
        assert_eq!(
            user_agent_for_wire_model("gemini-9-argon-high"),
            CLI_USER_AGENT
        );
        assert_eq!(
            user_agent_for_wire_model("gemini-9-flash-medium"),
            IDE_USER_AGENT
        );
        assert!(parse_available_models(&json!({"models": {"tab": {}}})).is_empty());
    }

    #[test]
    fn resolve_wire_model_maps_effort_including_xhigh_and_max_to_high() {
        for (model, effort, expected) in [
            ("antigravity/claude-opus-5-5", None, "claude-opus-5-5-high"),
            (
                "antigravity/claude-opus-5-5",
                Some("none"),
                "claude-opus-5-5-low",
            ),
            (
                "antigravity/claude-opus-5-5",
                Some("low"),
                "claude-opus-5-5-low",
            ),
            (
                "antigravity/claude-opus-5-5",
                Some("medium"),
                "claude-opus-5-5-medium",
            ),
            (
                "antigravity/claude-opus-5-5",
                Some("high"),
                "claude-opus-5-5-high",
            ),
            (
                "antigravity/claude-opus-5-5",
                Some("xhigh"),
                "claude-opus-5-5-high",
            ),
            (
                "antigravity/claude-opus-5-5",
                Some("max"),
                "claude-opus-5-5-high",
            ),
            ("antigravity/gemini-4-argon", None, "gemini-4-argon-high"),
            (
                "antigravity/gemini-4-argon",
                Some("low"),
                "gemini-4-argon-low",
            ),
            (
                "antigravity/gemini-4-argon",
                Some("medium"),
                "gemini-4-argon-medium",
            ),
            (
                "antigravity/gemini-4-argon",
                Some("xhigh"),
                "gemini-4-argon-high",
            ),
            (
                "antigravity/gemini-3.8-flash",
                None,
                "gemini-3.8-flash-medium",
            ),
            (
                "antigravity/gemini-3.8-flash",
                Some("low"),
                "gemini-3.8-flash-low",
            ),
            (
                "antigravity/gemini-3.8-flash",
                Some("max"),
                "gemini-3.8-flash-high",
            ),
            (
                "antigravity/gpt-oss-120b-medium",
                None,
                "gpt-oss-120b-medium",
            ),
            (
                "antigravity/claude-sonnet-5-5-low",
                None,
                "claude-sonnet-5-5-low",
            ),
            (
                "antigravity/claude-sonnet-5-5-low",
                Some("max"),
                "claude-sonnet-5-5-high",
            ),
        ] {
            assert_eq!(
                wire_model(
                    model,
                    effort,
                    &ANTIGRAVITY_FALLBACK_MODELS
                        .iter()
                        .map(|model| (*model).to_string())
                        .collect::<Vec<_>>()
                )
                .as_deref(),
                Some(expected),
                "{model} ({effort:?})"
            );
        }
    }

    #[test]
    fn a_spent_bucket_names_the_reset_and_a_full_one_does_not() {
        let now = now_ms();
        let later = now + 4 * 60 * 60 * 1_000;
        let reset = jiff::Timestamp::from_millisecond(later as i64).unwrap();
        let body = json!({"buckets": [
            {"modelId": "gemini-4-argon-high", "remainingFraction": 1, "resetTime": reset.to_string()},
            {"modelId": "claude-opus-5-5-high", "remainingFraction": 0, "resetTime": reset.to_string()},
            {"modelId": "chat_20706", "remainingFraction": 1}
        ]});
        assert_eq!(
            quota_reset_from(&body, "claude-opus-5-5-high", now),
            Some(later)
        );
        // Quota left: a short throttle, not a spent window.
        assert_eq!(quota_reset_from(&body, "gemini-4-argon-high", now), None);
        assert_eq!(quota_reset_from(&body, "missing-model", now), None);
        assert_eq!(quota_reset_from(&body, "claude-opus-5-5-high", later), None);
    }

    #[test]
    fn a_spent_family_waits_for_its_latest_spent_window_and_others_keep_going() {
        let body = json!({"groups": [
            {"displayName": "Gemini Models", "buckets": [
                {"bucketId": "gemini-weekly", "window": "weekly", "resetTime": "2026-09-11T02:25:41Z", "remainingFraction": 0},
                {"bucketId": "gemini-5h", "window": "5h", "resetTime": "2026-09-04T12:25:41Z", "remainingFraction": 0}
            ]},
            {"displayName": "Claude and GPT models", "description": "Claude Opus, GPT-OSS", "buckets": [
                {"bucketId": "3p-weekly", "displayName": "Weekly Limit Remaining", "resetTime": "2026-09-11T10:57:22Z", "remaining": {"remainingFraction": 0.95}},
                {"bucketId": "3p-5h", "displayName": "Five Hour Limit Remaining", "resetTime": "2026-09-04T12:54:45Z", "remainingFraction": 1.0}
            ]},
            {"displayName": "Image models", "buckets": [
                {"window": "5h", "remainingFraction": 0}
            ]}
        ]});
        let buckets = parse_quota_summary(&body);
        assert_eq!(buckets.len(), 4);
        let now = "2026-09-04T08:00:00Z"
            .parse::<jiff::Timestamp>()
            .unwrap()
            .as_millisecond() as u64;
        let reset =
            |time: &str| Some(time.parse::<jiff::Timestamp>().unwrap().as_millisecond() as u64);
        // Both Gemini windows are spent: the model waits for the later one.
        assert_eq!(
            spent_until(&buckets, Family::of_model("gemini-4-argon-high"), now),
            reset("2026-09-11T02:25:41Z")
        );
        // Claude still has quota in both windows, so a 429 is a short throttle.
        assert_eq!(
            spent_until(&buckets, Family::of_model("claude-opus-5-5-high"), now),
            None
        );
        assert_eq!(Family::of_model("gpt-oss-120b-medium"), Family::Claude);
        // A reset that already passed proves nothing.
        let after = reset("2026-09-12T00:00:00Z").unwrap();
        assert_eq!(spent_until(&buckets, Family::Gemini, after), None);
    }
}
