//! Claude subscription sign-in and refresh. API keys belong to `anthropic`.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::accounts::{AccountSupport, Accounts, NoLogin};
use crate::auth::AuthStorage;
use crate::provider::{AuthState, CliHandlers};

pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const TOKEN_URL: &str = "https://api.anthropic.com/v1/oauth/token";
pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
pub const OAUTH_BETA: &str = "claude-code-20250219,oauth-2025-04-20";
pub const USAGE_BETA: &str = "oauth-2025-04-20";

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredAuth {
    pub access: String,
    pub refresh: String,
    pub expires: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
}

impl StoredAuth {
    pub fn identity(&self) -> Option<String> {
        self.account_id.clone().or_else(|| self.email.clone())
    }
}

#[derive(Deserialize)]
pub(super) struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: u64,
    #[serde(default)]
    account: Option<TokenAccount>,
}

#[derive(Deserialize)]
struct TokenAccount {
    uuid: Option<String>,
    email_address: Option<String>,
}

impl TokenResponse {
    pub(super) fn into_auth(self, previous: Option<&StoredAuth>) -> Result<StoredAuth> {
        if self.access_token.trim().is_empty() || self.expires_in == 0 {
            bail!(
                "Claude returned an empty or expired sign-in. Run `cc-proxy auth login claude` again."
            );
        }
        let refresh = self
            .refresh_token
            .filter(|token| !token.trim().is_empty())
            .or_else(|| previous.map(|auth| auth.refresh.clone()))
            .context("Claude returned no refresh token. Run `cc-proxy auth login claude` again.")?;
        Ok(StoredAuth {
            access: self.access_token,
            refresh,
            expires: now_ms().saturating_add(self.expires_in.saturating_mul(1000)),
            email: self
                .account
                .as_ref()
                .and_then(|account| account.email_address.clone())
                .map(|email| email.to_ascii_lowercase())
                .or_else(|| previous.and_then(|auth| auth.email.clone())),
            account_id: self
                .account
                .and_then(|account| account.uuid)
                .or_else(|| previous.and_then(|auth| auth.account_id.clone())),
            plan: previous.and_then(|auth| auth.plan.clone()),
        })
    }
}

pub struct ClaudeCli;
pub static CLI: ClaudeCli = ClaudeCli;

impl AccountSupport for ClaudeCli {
    fn sign_in(&self, device: bool) -> Result<Value> {
        serde_json::to_value(super::login::sign_in(device)?).map_err(Into::into)
    }

    fn identity(&self, login: &Value) -> Option<String> {
        serde_json::from_value::<StoredAuth>(login.clone())
            .ok()?
            .identity()
    }

    fn email(&self, login: &Value) -> Option<String> {
        serde_json::from_value::<StoredAuth>(login.clone())
            .ok()?
            .email
    }

    fn state(&self, login: &Value) -> AuthState {
        match serde_json::from_value::<StoredAuth>(login.clone()) {
            Ok(auth) if !auth.access.is_empty() => AuthState::SignedIn {
                account: auth.email.or(auth.account_id),
                expires_ms: Some(auth.expires),
            },
            _ => AuthState::Missing,
        }
    }

    fn single_login(&self) -> Box<dyn AuthStorage<Value>> {
        Box::new(NoLogin)
    }

    fn prepare(&self, accounts: &Accounts) -> Result<()> {
        super::local::import_login(accounts, &crate::paths::DirResolverEnv::default())
    }
}

impl CliHandlers for ClaudeCli {
    fn auth_state(&self) -> AuthState {
        Accounts::current_state("claude", self)
    }

    fn login(&self) -> Result<()> {
        crate::auth_cli::AccountCli::open("claude", self)?.login(&Default::default())
    }

    fn device(&self) -> Result<()> {
        crate::auth_cli::AccountCli::open("claude", self)?.login(&crate::auth_cli::LoginOptions {
            device: true,
            ..Default::default()
        })
    }

    fn status(&self) -> Result<()> {
        crate::auth_cli::AccountCli::open("claude", self)?.status()
    }

    fn logout(&self) -> Result<()> {
        crate::auth_cli::AccountCli::open("claude", self)?.logout(None, false)
    }

    fn accounts(&self) -> Option<&dyn AccountSupport> {
        Some(self)
    }
}

pub struct AuthManager<S: AuthStorage<StoredAuth>> {
    store: S,
    lock: Arc<tokio::sync::Mutex<()>>,
    client: reqwest::Client,
    token_url: String,
}

impl<S: AuthStorage<StoredAuth>> AuthManager<S> {
    pub fn new(store: S) -> Self {
        Self::with_endpoint(store, TOKEN_URL.into())
    }

    fn with_endpoint(store: S, token_url: String) -> Self {
        Self {
            lock: crate::auth::refresh_lock(&store.path()),
            store,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(20))
                .build()
                .expect("failed to build Claude refresh client"),
            token_url,
        }
    }

    pub async fn get_auth(&self) -> Result<StoredAuth> {
        self.refresh(None).await
    }

    pub async fn force_refresh(&self, rejected: &str) -> Result<StoredAuth> {
        self.refresh(Some(rejected)).await
    }

    async fn refresh(&self, rejected: Option<&str>) -> Result<StoredAuth> {
        let _guard = self.lock.lock().await;
        let current = self
            .store
            .load()?
            .context("No Claude subscription is saved. Run `cc-proxy auth login claude`.")?;
        if rejected.map_or(
            current.expires > now_ms().saturating_add(60_000),
            |access| current.access != access,
        ) {
            return Ok(current);
        }
        let response = self
            .client
            .post(&self.token_url)
            .json(&json!({
                "grant_type": "refresh_token",
                "client_id": CLIENT_ID,
                "refresh_token": current.refresh,
            }))
            .send()
            .await
            .context("Claude sign-in could not be renewed. Check your connection and retry.")?;
        if !response.status().is_success() {
            // Another process may have renewed this login during the request.
            if let Some(latest) = self.store.load()?
                && latest != current
            {
                return Ok(latest);
            }
            bail!(
                "Claude sign-in renewal failed (HTTP {}). Run `cc-proxy auth login claude --replace`.",
                response.status()
            );
        }
        let tokens: TokenResponse = response
            .json()
            .await
            .context("Claude returned an unreadable sign-in. Retry the request.")?;
        let next = tokens.into_auth(Some(&current))?;
        self.store.save(next.clone())?;
        Ok(next)
    }
}

pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::InMemoryAuthStore;
    use axum::{Json, Router, routing::post};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn expired() -> StoredAuth {
        StoredAuth {
            access: "expired".into(),
            refresh: "refresh".into(),
            expires: 0,
            email: Some("person@example.com".into()),
            account_id: Some("account".into()),
            plan: Some("max".into()),
        }
    }

    #[tokio::test]
    async fn concurrent_refreshes_persist_once_keep_metadata_and_reuse_rotated_access() {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let app = Router::new().route("/token", post(move |Json(body): Json<Value>| {
            let count = count.clone();
            async move {
                assert_eq!(body["refresh_token"], "refresh");
                count.fetch_add(1, Ordering::SeqCst);
                Json(json!({"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let store = InMemoryAuthStore::new();
        store.save(expired()).unwrap();
        let manager = AuthManager::with_endpoint(store.clone(), format!("http://{address}/token"));
        let (a, b) = tokio::join!(manager.get_auth(), manager.get_auth());
        assert_eq!(a.unwrap().access, "new-access");
        assert_eq!(b.unwrap().access, "new-access");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let next = store.load().unwrap().unwrap();
        assert_eq!(next.email.as_deref(), Some("person@example.com"));
        assert_eq!(next.account_id.as_deref(), Some("account"));
        assert_eq!(next.plan.as_deref(), Some("max"));
        assert_eq!(
            manager.force_refresh("expired").await.unwrap().access,
            "new-access"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        store.clear().unwrap();
        assert!(manager.get_auth().await.is_err());
        server.abort();
    }
}
