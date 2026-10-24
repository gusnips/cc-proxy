//! OpenCode Zen accounts: the Console device sign-in, the token each account
//! keeps, and its renewal. Mirrors the `opencode-zen` flow in
//! `@providerkit/core` (the one tabrunner uses), so both get the same sign-in.

use std::time::Duration;

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::OPENCODE_ZEN_CLI;
use crate::accounts::{AccountAuthStore, Accounts};
use crate::auth::AuthStorage;
use crate::paths;

/// The Console's public client, the same one the opencode CLI signs in with.
const CLIENT_ID: &str = "opencode-cli";
/// Renew this long before the token actually expires.
const REFRESH_SKEW_MS: u64 = 60_000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// One Zen account's credential, as the Console issued it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredAuth {
    /// The bearer token requests carry.
    pub access: String,
    /// Trades for a new access token. Empty when the Console sent none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub refresh: String,
    /// 0 means the Console gave no expiry, so the token is used as it is.
    #[serde(default)]
    pub expires_at_ms: u64,
    /// The organization the sign-in belongs to, when the Console says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_id: Option<String>,
}

impl StoredAuth {
    fn from_token(body: &Value, fallback_refresh: Option<&str>) -> anyhow::Result<Self> {
        let access = body["access_token"]
            .as_str()
            .filter(|token| !token.is_empty())
            .ok_or_else(|| anyhow::anyhow!("OpenCode Console's reply has no access token"))?;
        let refresh = body["refresh_token"]
            .as_str()
            .filter(|token| !token.is_empty())
            .or(fallback_refresh)
            .unwrap_or_default();
        let expires_at_ms = body["expires_in"]
            .as_u64()
            .map(|seconds| {
                now_ms()
                    .saturating_add(seconds.saturating_mul(1000).saturating_sub(REFRESH_SKEW_MS))
            })
            .unwrap_or(0);
        Ok(Self {
            access: access.to_string(),
            refresh: refresh.to_string(),
            expires_at_ms,
            org_id: body["org_id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .map(str::to_string),
        })
    }
}

/// Who a saved sign-in belongs to: the organization the Console named, or the
/// subject inside the token.
pub fn identity(auth: &StoredAuth) -> Option<String> {
    auth.org_id
        .clone()
        .or_else(|| jwt_claim(&auth.access, &["email", "sub"]))
}

/// What the Console base URL is: the override when set, `opencode.ai` otherwise.
pub fn console_url() -> String {
    std::env::var("CCP_OPENCODE_CONSOLE_URL")
        .unwrap_or_else(|_| "https://opencode.ai/console".to_string())
        .trim_end_matches('/')
        .to_string()
}

/// The page that approves a device code. The Console answers with a path
/// relative to its origin (`/console/device?user_code=…`), so it is resolved
/// the way a browser would resolve it, never glued onto the base path (which
/// produced `/console/console/device` and landed on the wrong page).
pub fn verification_url(console: &str, raw: &str) -> String {
    if let Ok(url) = reqwest::Url::parse(raw)
        && matches!(url.scheme(), "http" | "https")
    {
        return url.to_string();
    }
    reqwest::Url::parse(console)
        .and_then(|base| base.join(raw))
        .map_or_else(|_| raw.to_string(), |url| url.to_string())
}

/// The store of the account this request selected.
pub fn for_request(id: Option<&str>) -> anyhow::Result<AccountAuthStore<StoredAuth>> {
    let accounts = Accounts::open("opencode-zen", &OPENCODE_ZEN_CLI)?;
    accounts.for_request(id)
}

/// The token a request sends. An environment key wins and pins the credential;
/// otherwise the selected account's token is renewed when it is about to lapse.
pub async fn access_token(account: Option<&str>) -> anyhow::Result<Option<String>> {
    if let Some(key) = non_empty(crate::config::opencode_zen_env_key()) {
        return Ok(Some(key));
    }
    let store = for_request(account)?;
    let Some(auth) = store.load()? else {
        return Ok(non_empty(crate::config::opencode_zen_saved_key()));
    };
    Ok(Some(fresh_token(&store, auth).await?))
}

fn non_empty(key: Option<String>) -> Option<String> {
    key.filter(|key| !key.trim().is_empty())
}

async fn fresh_token(
    store: &AccountAuthStore<StoredAuth>,
    auth: StoredAuth,
) -> anyhow::Result<String> {
    if !needs_refresh(&auth, now_ms()) || auth.refresh.is_empty() {
        return Ok(auth.access);
    }
    match renew(&auth.refresh).await {
        Ok(mut fresh) => {
            if fresh.org_id.is_none() {
                fresh.org_id = auth.org_id.clone();
            }
            store.save(fresh.clone())?;
            Ok(fresh.access)
        }
        // A failed renewal leaves a token that may still work.
        Err(_) => Ok(auth.access),
    }
}

fn needs_refresh(auth: &StoredAuth, now: u64) -> bool {
    auth.expires_at_ms != 0 && now >= auth.expires_at_ms
}

async fn renew(refresh_token: &str) -> anyhow::Result<StoredAuth> {
    let body: Value = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(REQUEST_TIMEOUT)
        .user_agent("opencode/latest/2.0.32/cli")
        .build()?
        .post(format!("{}/auth/device/token", console_url()))
        .header("Accept", "application/json")
        .json(&serde_json::json!({
            "client_id": CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    StoredAuth::from_token(&body, Some(refresh_token))
}

/// Signs in with the Console's device code and returns the new account's
/// credential. The verification page carries the code, so there is nothing to
/// type beyond approving it.
pub fn login() -> anyhow::Result<StoredAuth> {
    use std::time::Instant;

    let console = console_url();
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(REQUEST_TIMEOUT)
        .user_agent("opencode/latest/2.0.32/cli")
        .build()?;
    let device: Value = client
        .post(format!("{console}/auth/device/code"))
        .header("Accept", "application/json")
        .json(&serde_json::json!({
            "client_id": CLIENT_ID,
            "supports_org_scope": true
        }))
        .send()?
        .error_for_status()?
        .json()?;
    let device_code = device["device_code"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing device_code from OpenCode Console"))?;
    let user_code = device["user_code"].as_str().unwrap_or_default();
    let raw_uri = device["verification_uri_complete"]
        .as_str()
        .or_else(|| device["verification_uri"].as_str())
        .unwrap_or("/console/device");
    let uri = verification_url(&console, raw_uri);
    let interval = device["interval"].as_u64().unwrap_or(5).max(1);
    let expires_in = device["expires_in"].as_u64().unwrap_or(600);

    println!("Open this URL to sign in to OpenCode Console:");
    println!("  {uri}");
    if !user_code.is_empty() {
        println!("Enter code: {user_code}");
    }
    let _ = crate::browser::open(&uri);

    let deadline = Instant::now() + Duration::from_secs(expires_in);
    let mut wait = Duration::from_secs(interval);
    while Instant::now() < deadline {
        std::thread::sleep(wait);
        let body: Value = client
            .post(format!("{console}/auth/device/token"))
            .header("Accept", "application/json")
            .json(&serde_json::json!({
                "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
                "device_code": device_code,
                "client_id": CLIENT_ID
            }))
            .send()?
            .json()
            .unwrap_or_default();
        if body["access_token"]
            .as_str()
            .is_some_and(|token| !token.is_empty())
        {
            return StoredAuth::from_token(&body, None);
        }
        match body["error"].as_str() {
            Some("authorization_pending") => continue,
            Some("slow_down") => wait += Duration::from_secs(5),
            Some(error) => anyhow::bail!("OpenCode device sign-in failed: {error}"),
            None => anyhow::bail!("Unexpected response from OpenCode Console"),
        }
    }
    anyhow::bail!("OpenCode device sign-in timed out")
}

/// The one login Zen kept before accounts existed: an API key in `config.json`.
pub struct LegacyKeyStore {
    path: String,
}

impl LegacyKeyStore {
    pub fn new() -> Self {
        Self {
            path: paths::config_dir()
                .join("config.json")
                .to_string_lossy()
                .into_owned(),
        }
    }
}

impl Default for LegacyKeyStore {
    fn default() -> Self {
        Self::new()
    }
}

impl AuthStorage<Value> for LegacyKeyStore {
    fn load(&self) -> anyhow::Result<Option<Value>> {
        Ok(crate::config::opencode_zen_saved_key().map(|key| serde_json::json!({ "access": key })))
    }

    fn save(&self, value: Value) -> anyhow::Result<()> {
        let key = value["access"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("an OpenCode Zen login needs an access token"))?;
        crate::config::save_opencode_zen_api_key(key)?;
        Ok(())
    }

    fn clear(&self) -> anyhow::Result<()> {
        crate::config::clear_opencode_zen_api_key()?;
        Ok(())
    }

    fn path(&self) -> String {
        self.path.clone()
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn jwt_claim(token: &str, names: &[&str]) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    names
        .iter()
        .find_map(|name| claims.get(name).and_then(Value::as_str))
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verification_url_resolves_against_the_console_origin() {
        let console = "https://opencode.ai/console";
        // What the Console actually returns, and the page it means.
        assert_eq!(
            verification_url(
                console,
                "/console/device?user_code=KTWD-VBNZ&client_id=opencode-cli"
            ),
            "https://opencode.ai/console/device?user_code=KTWD-VBNZ&client_id=opencode-cli"
        );
        assert_eq!(
            verification_url(console, "https://opencode.ai/console/device?user_code=X"),
            "https://opencode.ai/console/device?user_code=X"
        );
    }

    #[test]
    fn a_token_is_renewed_only_when_it_is_about_to_lapse() {
        let auth = StoredAuth {
            access: "token".into(),
            refresh: "refresh".into(),
            expires_at_ms: 10_000,
            org_id: None,
        };
        assert!(!needs_refresh(&auth, 9_999));
        assert!(needs_refresh(&auth, 10_000));
        // No expiry from the Console: used as it is.
        assert!(!needs_refresh(
            &StoredAuth {
                expires_at_ms: 0,
                ..auth
            },
            1_000_000
        ));
    }

    #[test]
    fn the_organization_names_the_account() {
        let auth = StoredAuth {
            access: "opaque".into(),
            refresh: String::new(),
            expires_at_ms: 0,
            org_id: Some("org_123".into()),
        };
        assert_eq!(identity(&auth).as_deref(), Some("org_123"));
    }
}
