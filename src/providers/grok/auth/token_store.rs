use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::accounts::{AccountAuthStore, Accounts};
use crate::auth::{AuthStorage, FileAuthStore};
use crate::paths;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredAuth {
    pub access: String,
    pub refresh: String,
    pub expires_at_ms: u64,
    pub issuer: String,
    pub client_id: String,
}

pub struct GrokTokenStore<S: AuthStorage<StoredAuth>> {
    store: S,
}

impl<S: AuthStorage<StoredAuth>> GrokTokenStore<S> {
    pub fn new(store: S) -> Self {
        Self { store }
    }
    pub fn load_auth(&self) -> anyhow::Result<Option<StoredAuth>> {
        self.store.load()
    }
    pub fn save_auth(&self, auth: StoredAuth) -> anyhow::Result<()> {
        self.store.save(auth)
    }
    pub fn clear_auth(&self) -> anyhow::Result<()> {
        self.store.clear()
    }
    pub fn auth_path(&self) -> String {
        self.store.path()
    }
}

pub type DefaultGrokAuthStore = AccountAuthStore<StoredAuth>;

/// Chooses the current login once, or pins the account this request selected.
pub fn for_request(id: Option<&str>) -> anyhow::Result<GrokTokenStore<DefaultGrokAuthStore>> {
    let accounts = Accounts::open("grok", &super::super::GROK_CLI)?;
    Ok(GrokTokenStore::new(accounts.for_request(id)?))
}

pub fn single_login_store() -> FileAuthStore<Value> {
    FileAuthStore::new(
        paths::provider_auth_file("grok")
            .to_string_lossy()
            .into_owned(),
        paths::provider_legacy_auth_file("grok")
            .to_string_lossy()
            .into_owned(),
    )
}

/// The subject is used only to recognize a login already saved, not to grant
/// access. Grok validates the access token when a request reaches it.
pub fn identity(auth: &StoredAuth) -> Option<String> {
    let payload = auth.access.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    claims
        .get("sub")?
        .as_str()
        .filter(|subject| !subject.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_identity_uses_the_subject_not_the_rotating_token() {
        let auth = StoredAuth {
            access: "header.eyJzdWIiOiJhY2NvdW50LTEifQ.signature".into(),
            refresh: "refresh".into(),
            expires_at_ms: u64::MAX,
            issuer: super::super::login::CANONICAL_ISSUER.into(),
            client_id: super::super::login::CLIENT_ID.into(),
        };
        assert_eq!(identity(&auth).as_deref(), Some("account-1"));
        assert!(
            identity(&StoredAuth {
                access: "opaque-token".into(),
                ..auth
            })
            .is_none()
        );
    }
}
