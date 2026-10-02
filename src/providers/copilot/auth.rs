//! GitHub Copilot sign-in: a GitHub device login, then a second step that
//! trades the long-lived GitHub token for a Copilot token that lasts about 25
//! minutes. Trading again is the refresh, so no OAuth refresh token exists.

use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::{AuthStorage, FileAuthStore};
use crate::{config, paths};

/// VS Code's own Copilot Chat client id. GitHub issues no self-service Copilot
/// client id, so every third-party client of this API borrows it, which is why
/// the consent page says "GitHub Copilot Chat".
const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
/// Enough to read the account name. The account's plan grants Copilot itself.
const SCOPE: &str = "read:user";
const GRANT_DEVICE_CODE: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// Where a seat without a plan-specific host is served from.
const DEFAULT_BASE_URL: &str = "https://api.individual.githubcopilot.com";
/// A token this close to its end is minted again before use.
const REFRESH_MARGIN_MS: u64 = 5 * 60 * 1000;
const VERSION: &str = env!("CARGO_PKG_VERSION");

pub const LOGIN_HINT: &str = "Run `cc-proxy copilot auth login`.";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredAuth {
    /// The long-lived GitHub token that mints Copilot tokens.
    pub github_token: String,
    /// The short-lived token chat calls carry.
    pub copilot_token: String,
    /// When `copilot_token` ends, in epoch milliseconds.
    pub expires: u64,
    /// The chat host the Copilot token names.
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

pub fn file_store() -> FileAuthStore<StoredAuth> {
    FileAuthStore::new(
        paths::provider_auth_file("copilot")
            .to_string_lossy()
            .to_string(),
        paths::provider_legacy_auth_file("copilot")
            .to_string_lossy()
            .to_string(),
    )
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// The editor fingerprint Copilot's gate wants on every call. `initiator`
/// declares who started the turn: Copilot bills one premium request per `user`
/// turn and lets the agent's own follow-ups (`agent`) ride free.
pub fn copilot_headers(initiator: &str) -> [(&'static str, String); 7] {
    let editor = format!("cc-proxy/{VERSION}");
    [
        ("Copilot-Integration-Id", "vscode-chat".to_string()),
        ("Editor-Version", editor.clone()),
        ("Editor-Plugin-Version", editor),
        ("X-GitHub-Api-Version", "2026-06-01".to_string()),
        ("Openai-Intent", "conversation-edits".to_string()),
        // A capability declaration. Without it Copilot drops images.
        ("Copilot-Vision-Request", "true".to_string()),
        ("X-Initiator", initiator.to_string()),
    ]
}

fn http() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .user_agent(format!("cc-proxy/{VERSION}"))
        .build()?)
}

// ---------------------------------------------------------------------------
// Sign in
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    interval: Option<u64>,
}

/// What `login` shows the person before it waits for them.
pub struct DevicePrompt<'a> {
    pub url: &'a str,
    pub code: &'a str,
}

/// Signs in with GitHub's device flow and saves the result. `show` runs once
/// the code is known, so the caller decides how to print it.
pub fn login(show: impl FnOnce(DevicePrompt)) -> Result<StoredAuth> {
    let client = http()?;
    let github = config::copilot_github_url();
    let response = client
        .post(format!("{github}/login/device/code"))
        .header("Accept", "application/json")
        .form(&[("client_id", CLIENT_ID), ("scope", SCOPE)])
        .send()
        .context("Couldn't reach GitHub to start the sign-in")?;
    if !response.status().is_success() {
        bail!(
            "GitHub refused to start the sign-in (HTTP {}). Try again in a minute.",
            response.status().as_u16()
        );
    }
    let device: DeviceCode = response
        .json()
        .context("GitHub's sign-in reply was not what cc-proxy expected")?;
    if !device.verification_uri.starts_with("http") {
        bail!("GitHub's sign-in reply had no web address to open");
    }
    show(DevicePrompt {
        url: &device.verification_uri,
        code: &device.user_code,
    });

    let github_token = poll_for_token(&client, &github, &device)?;
    let auth = mint(&client, &github_token, None)?;
    file_store().save(auth.clone())?;
    Ok(auth)
}

fn poll_for_token(
    client: &reqwest::blocking::Client,
    github: &str,
    device: &DeviceCode,
) -> Result<String> {
    let mut interval = device.interval.unwrap_or(5).max(1);
    let deadline =
        std::time::Instant::now() + Duration::from_secs(device.expires_in.unwrap_or(900));
    loop {
        std::thread::sleep(Duration::from_secs(interval));
        let body: Value = client
            .post(format!("{github}/login/oauth/access_token"))
            .header("Accept", "application/json")
            .form(&[
                ("client_id", CLIENT_ID),
                ("device_code", device.device_code.as_str()),
                ("grant_type", GRANT_DEVICE_CODE),
            ])
            .send()
            .context("Couldn't reach GitHub while waiting for you to approve")?
            .json()
            .unwrap_or(Value::Null);
        // GitHub answers 200 with an `error` while the person hasn't approved.
        if let Some(token) = body["access_token"].as_str().filter(|t| !t.is_empty()) {
            return Ok(token.to_string());
        }
        match body["error"].as_str() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += 5,
            Some("expired_token") => bail!("The code expired. {LOGIN_HINT}"),
            Some("access_denied") => bail!("You denied the sign-in on GitHub. {LOGIN_HINT}"),
            other => bail!(
                "GitHub's sign-in failed ({}). {LOGIN_HINT}",
                other.unwrap_or("no token in the reply")
            ),
        }
        if std::time::Instant::now() >= deadline {
            bail!("The code expired. {LOGIN_HINT}");
        }
    }
}

// ---------------------------------------------------------------------------
// Mint
// ---------------------------------------------------------------------------

/// Trades a GitHub token for a Copilot token. `account` carries the name a
/// earlier sign-in knows; without it the name is looked up, best effort.
fn mint(
    client: &reqwest::blocking::Client,
    github_token: &str,
    account: Option<String>,
) -> Result<StoredAuth> {
    let api = config::copilot_github_api_url();
    let mut request = client
        .get(format!("{api}/copilot_internal/v2/token"))
        .header("Accept", "application/json")
        .bearer_auth(github_token);
    for (name, value) in copilot_headers("user") {
        request = request.header(name, value);
    }
    let response = request
        .send()
        .context("Couldn't reach GitHub to get a Copilot token")?;
    let status = response.status();
    let body: Value = response.json().unwrap_or(Value::Null);
    if !status.is_success() {
        let detail = body["message"].as_str().unwrap_or("no details");
        bail!(match status.as_u16() {
            401 => format!("GitHub rejected the saved sign-in. {LOGIN_HINT}"),
            403 | 404 => format!(
                "This GitHub account can't use Copilot ({detail}). Check that it has a Copilot plan."
            ),
            code => format!("GitHub refused the Copilot token (HTTP {code}: {detail})"),
        });
    }
    // `expires_at` is epoch seconds, unlike the `expires_in` other vendors send.
    let (Some(token), Some(expires_at)) = (
        body["token"].as_str().filter(|t| !t.is_empty()),
        body["expires_at"].as_u64(),
    ) else {
        bail!("GitHub's Copilot token reply was missing the token or its expiry");
    };
    Ok(StoredAuth {
        github_token: github_token.to_string(),
        copilot_token: token.to_string(),
        expires: expires_at * 1000,
        host: host_for(&body["endpoints"], token),
        account: account.or_else(|| github_login(client, &api, github_token)),
    })
}

/// The chat host: the one the reply names, else the one inside the token, else
/// the individual-plan host. Only https hosts count, because the token is sent
/// there.
fn host_for(endpoints: &Value, token: &str) -> String {
    let named = endpoints["api"].as_str().map(str::to_string);
    let from_token = token
        .split(';')
        .find_map(|field| field.strip_prefix("proxy-ep="))
        .map(|proxy| format!("https://{}", proxy.replacen("proxy.", "api.", 1)));
    [named, from_token]
        .into_iter()
        .flatten()
        .find(|host| host.starts_with("https://"))
        .map(|host| host.trim_end_matches('/').to_string())
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
}

/// The GitHub login, for `status`. A sign-in that worked must not fail because
/// this lookup did.
fn github_login(
    client: &reqwest::blocking::Client,
    api: &str,
    github_token: &str,
) -> Option<String> {
    let body: Value = client
        .get(format!("{api}/user"))
        .header("Accept", "application/json")
        .bearer_auth(github_token)
        .send()
        .ok()?
        .json()
        .ok()?;
    body["login"].as_str().map(str::to_string)
}

// ---------------------------------------------------------------------------
// Token for a request
// ---------------------------------------------------------------------------

/// One mint at a time: two requests that find the token expired together
/// would otherwise both trade, and the second would overwrite the first.
static MINT_LOCK: Mutex<()> = Mutex::new(());

fn load() -> Result<StoredAuth> {
    file_store()
        .load()?
        .ok_or_else(|| anyhow::anyhow!("GitHub Copilot isn't signed in. {LOGIN_HINT}"))
}

/// A Copilot token good for at least a few more minutes, minted when the saved
/// one is near its end. Blocking: call it from a blocking thread.
pub fn current() -> Result<StoredAuth> {
    let stored = load()?;
    if stored.expires > now_ms() + REFRESH_MARGIN_MS {
        return Ok(stored);
    }
    renew(&stored.copilot_token)
}

/// Mints again after Copilot rejected `rejected`, unless another request
/// already did.
pub fn renew(rejected: &str) -> Result<StoredAuth> {
    let _guard = MINT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let stored = load()?;
    if stored.copilot_token != rejected && stored.expires > now_ms() + REFRESH_MARGIN_MS {
        return Ok(stored);
    }
    let fresh = mint(&http()?, &stored.github_token, stored.account.clone())?;
    file_store().save(fresh.clone())?;
    Ok(fresh)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn host_prefers_the_named_endpoint_then_the_token_then_the_default() {
        let named = json!({"api": "https://api.business.githubcopilot.com/"});
        assert_eq!(
            host_for(&named, "tid=1"),
            "https://api.business.githubcopilot.com"
        );
        assert_eq!(
            host_for(
                &Value::Null,
                "tid=1;proxy-ep=proxy.enterprise.githubcopilot.com;exp=2"
            ),
            "https://api.enterprise.githubcopilot.com"
        );
        // A plain-http host would carry the token in the clear.
        assert_eq!(
            host_for(&json!({"api": "http://evil.example"}), "tid=1"),
            DEFAULT_BASE_URL
        );
    }

    /// Points the config dir and both GitHub hosts at a mock. Env is
    /// process-wide, so the guard also holds a lock.
    struct Mocked {
        _lock: std::sync::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
        _server: crate::providers::codex::auth::test_http::MockServer,
        mints: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    impl Mocked {
        fn start() -> Self {
            use crate::providers::codex::auth::test_http::{json_response, spawn_mock_server};
            use std::sync::atomic::{AtomicUsize, Ordering};
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let polls = AtomicUsize::new(0);
            let mints = std::sync::Arc::new(AtomicUsize::new(0));
            let counted = mints.clone();
            let server = spawn_mock_server("copilot github mock ready", move |request| {
                let line = request.lines().next().unwrap_or_default();
                if line.starts_with("POST /login/device/code") {
                    json_response(
                        200,
                        r#"{"device_code":"dc","user_code":"ABCD-1234","verification_uri":"https://github.com/login/device","interval":1,"expires_in":900}"#,
                    )
                } else if line.starts_with("POST /login/oauth/access_token") {
                    // GitHub answers 200 while the person hasn't approved yet.
                    if polls.fetch_add(1, Ordering::SeqCst) == 0 {
                        json_response(200, r#"{"error":"authorization_pending"}"#)
                    } else {
                        json_response(200, r#"{"access_token":"gho_test"}"#)
                    }
                } else if line.starts_with("GET /copilot_internal/v2/token") {
                    assert!(request.contains("authorization: Bearer gho_test"));
                    assert!(request.contains("x-initiator: user"));
                    let n = counted.fetch_add(1, Ordering::SeqCst);
                    json_response(
                        200,
                        &format!(
                            r#"{{"token":"cop-{n}","expires_at":4102444800,"endpoints":{{"api":"https://api.business.githubcopilot.com"}}}}"#
                        ),
                    )
                } else if line.starts_with("GET /user") {
                    json_response(200, r#"{"login":"octocat"}"#)
                } else {
                    json_response(404, "{}")
                }
            });
            let dir = tempfile::tempdir().unwrap();
            // SAFETY: the lock above keeps every other test off the environment.
            unsafe {
                std::env::set_var("CCP_CONFIG_DIR", dir.path());
                std::env::set_var("CCP_COPILOT_GITHUB_URL", &server.url);
                std::env::set_var("CCP_COPILOT_GITHUB_API_URL", &server.url);
            }
            Self {
                _lock: lock,
                _dir: dir,
                _server: server,
                mints,
            }
        }
    }

    impl Drop for Mocked {
        fn drop(&mut self) {
            // SAFETY: still under the lock.
            unsafe {
                std::env::remove_var("CCP_CONFIG_DIR");
                std::env::remove_var("CCP_COPILOT_GITHUB_URL");
                std::env::remove_var("CCP_COPILOT_GITHUB_API_URL");
            }
        }
    }

    #[test]
    fn device_login_waits_for_approval_then_saves_the_minted_token() {
        let mocked = Mocked::start();
        let mut shown = String::new();
        let signed_in = login(|prompt| shown = format!("{} {}", prompt.url, prompt.code)).unwrap();
        assert_eq!(shown, "https://github.com/login/device ABCD-1234");
        assert_eq!(signed_in.copilot_token, "cop-0");
        assert_eq!(signed_in.github_token, "gho_test");
        assert_eq!(signed_in.host, "https://api.business.githubcopilot.com");
        assert_eq!(signed_in.account.as_deref(), Some("octocat"));
        let saved = file_store().load().unwrap().unwrap();
        assert_eq!(saved.copilot_token, "cop-0");
        assert_eq!(mocked.mints.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn an_expired_token_is_minted_again_and_a_fresh_one_is_reused() {
        let mocked = Mocked::start();
        let expired = StoredAuth {
            github_token: "gho_test".into(),
            copilot_token: "old".into(),
            expires: 1,
            host: DEFAULT_BASE_URL.into(),
            account: Some("octocat".into()),
        };
        file_store().save(expired).unwrap();
        assert_eq!(current().unwrap().copilot_token, "cop-0");
        // Fresh now: no second mint.
        assert_eq!(current().unwrap().copilot_token, "cop-0");
        // Copilot rejected a token another request already replaced: adopt it.
        assert_eq!(renew("old").unwrap().copilot_token, "cop-0");
        assert_eq!(mocked.mints.load(std::sync::atomic::Ordering::SeqCst), 1);
        // Rejected while still the saved one: mint again.
        assert_eq!(renew("cop-0").unwrap().copilot_token, "cop-1");
    }

    #[test]
    fn headers_declare_who_started_the_turn() {
        let headers = copilot_headers("agent");
        let value = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(value("X-Initiator"), Some("agent"));
        assert_eq!(
            value("Editor-Version"),
            Some(format!("cc-proxy/{VERSION}").as_str())
        );
    }
}
