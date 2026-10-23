use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::accounts::{AccountAuthStore, Accounts};
use crate::auth::{AuthStorage, KeychainFileAuthStore, SystemKeychain};
use crate::paths;

pub const KEYCHAIN_SERVICE: &str = "claude-code-proxy.codex";
pub const KEYCHAIN_ACCOUNT: &str = "auth";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredAuth {
    pub access: String,
    pub refresh: String,
    pub expires: u64,
    #[serde(
        default,
        rename = "accountId",
        alias = "account_id",
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
}

pub struct CodexTokenStore<S: AuthStorage<StoredAuth>> {
    store: S,
}

impl<S: AuthStorage<StoredAuth>> CodexTokenStore<S> {
    pub fn new(store: S) -> Self {
        Self { store }
    }

    pub fn load_auth(&self) -> Result<Option<StoredAuth>, anyhow::Error> {
        self.store.load()
    }

    pub fn save_auth(&self, value: StoredAuth) -> Result<(), anyhow::Error> {
        self.store.save(value)
    }

    pub fn clear_auth(&self) -> Result<(), anyhow::Error> {
        self.store.clear()
    }

    pub fn auth_path(&self) -> String {
        self.store.path()
    }
}

pub type DefaultCodexAuthStore = AccountAuthStore<StoredAuth>;

pub fn account_store(id: &str) -> CodexTokenStore<DefaultCodexAuthStore> {
    CodexTokenStore::new(Accounts::new("codex").store(id))
}

pub fn current_account() -> Result<Option<crate::accounts::Account>> {
    Accounts::open("codex", &super::super::CODEX_CLI)?.current()
}

/// Reads the current account once. Its token manager stays pinned to this ID.
pub fn file_store() -> Result<CodexTokenStore<DefaultCodexAuthStore>> {
    let account = current_account()?;
    Ok(account_store(
        account.as_ref().map_or("missing", |a| a.id.as_str()),
    ))
}

/// Where the one login was kept before accounts. It moves into the first
/// account the first time accounts are read.
pub fn single_login_store() -> KeychainFileAuthStore<serde_json::Value, SystemKeychain> {
    let primary = paths::provider_auth_file("codex");
    let legacy = paths::provider_legacy_auth_file("codex");
    KeychainFileAuthStore::new(
        primary.to_string_lossy().to_string(),
        legacy.to_string_lossy().to_string(),
        KEYCHAIN_SERVICE,
        KEYCHAIN_ACCOUNT,
        use_macos_keychain(),
        SystemKeychain,
    )
}

fn use_macos_keychain() -> bool {
    cfg!(target_os = "macos") && std::env::var_os("CCP_CONFIG_DIR").is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::InMemoryAuthStore;
    use serde_json::json;

    #[test]
    fn stored_auth_reads_account_id_alias() {
        let auth: StoredAuth = serde_json::from_value(json!({
            "access": "a",
            "refresh": "r",
            "expires": 123,
            "accountId": "acct"
        }))
        .unwrap();
        assert_eq!(auth.account_id.as_deref(), Some("acct"));
    }

    #[test]
    fn stored_auth_writes_account_id_key() {
        let auth = StoredAuth {
            access: "a".into(),
            refresh: "r".into(),
            expires: 4102444800000,
            account_id: Some("acct_1".into()),
        };
        let value = serde_json::to_value(auth).unwrap();
        assert_eq!(value["accountId"], "acct_1");
        assert!(value.get("account_id").is_none());
    }

    #[test]
    fn stored_auth_roundtrip() {
        let store = CodexTokenStore::new(InMemoryAuthStore::new());
        let auth = StoredAuth {
            access: "token".into(),
            refresh: "refresh".into(),
            expires: 9999999999999,
            account_id: Some("acct_1".into()),
        };
        store.save_auth(auth.clone()).unwrap();
        let loaded = store.load_auth().unwrap().unwrap();
        assert_eq!(loaded.access, "token");
        assert_eq!(loaded.account_id.as_deref(), Some("acct_1"));
    }
}
