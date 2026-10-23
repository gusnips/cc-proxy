use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::accounts::{AccountAuthStore, Accounts};
use crate::auth::{AuthStorage, FileAuthStore};
use crate::paths;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredAuth {
    pub access: String,
    pub refresh: String,
    pub expires: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(rename = "userId", default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(rename = "deviceId")]
    pub device_id: String,
}

pub struct KimiTokenStore<S: AuthStorage<StoredAuth>> {
    store: S,
}

impl<S: AuthStorage<StoredAuth>> KimiTokenStore<S> {
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

pub type DefaultKimiAuthStore = AccountAuthStore<StoredAuth>;

/// Chooses the current login once, or pins the account this request selected.
pub fn for_request(id: Option<&str>) -> anyhow::Result<KimiTokenStore<DefaultKimiAuthStore>> {
    let accounts = Accounts::open("kimi", &super::super::KIMI_CLI)?;
    Ok(KimiTokenStore::new(accounts.for_request(id)?))
}

pub fn file_store() -> anyhow::Result<KimiTokenStore<DefaultKimiAuthStore>> {
    for_request(None)
}

/// Adds the old shared device ID to the login before the account migration
/// copies it. New sign-ins generate their own device ID instead.
pub struct SingleLoginStore {
    store: FileAuthStore<Value>,
}

impl AuthStorage<Value> for SingleLoginStore {
    fn load(&self) -> anyhow::Result<Option<Value>> {
        self.store
            .load()?
            .map(|mut login| {
                attach_device_id(&mut login, super::device_id::get_device_id)?;
                Ok(login)
            })
            .transpose()
    }

    fn save(&self, value: Value) -> anyhow::Result<()> {
        self.store.save(value)
    }

    fn clear(&self) -> anyhow::Result<()> {
        self.store.clear()
    }

    fn path(&self) -> String {
        self.store.path()
    }
}

fn attach_device_id(
    login: &mut Value,
    legacy_device_id: impl FnOnce() -> anyhow::Result<String>,
) -> anyhow::Result<()> {
    let object = login.as_object_mut().ok_or_else(|| {
        anyhow::anyhow!("The saved Kimi sign-in is not a JSON object. Sign in again to replace it.")
    })?;
    if !object
        .get("deviceId")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty())
    {
        object.insert("deviceId".to_string(), Value::String(legacy_device_id()?));
    }
    Ok(())
}

pub fn single_login_store() -> SingleLoginStore {
    SingleLoginStore {
        store: FileAuthStore::new(
            paths::provider_auth_file("kimi")
                .to_string_lossy()
                .into_owned(),
            paths::provider_legacy_auth_file("kimi")
                .to_string_lossy()
                .into_owned(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn migration_keeps_the_device_id_used_by_the_old_login() {
        let mut login = json!({"access": "a", "refresh": "r", "expires": 1});
        attach_device_id(&mut login, || Ok("original-device".into())).unwrap();
        let auth: StoredAuth = serde_json::from_value(login.clone()).unwrap();
        assert_eq!(auth.device_id, "original-device");
        attach_device_id(&mut login, || panic!("must not replace a saved device ID")).unwrap();
        assert_eq!(login["deviceId"], "original-device");
    }
}
