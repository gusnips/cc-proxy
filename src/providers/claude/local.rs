//! Copy-only import. Never clear or rewrite Claude Code's own credentials.

use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::Value;

use super::auth::StoredAuth;
use crate::accounts::Accounts;
use crate::auth::{AuthStorage, Keychain, SystemKeychain};
use crate::paths::DirResolverEnv;

struct CachedKeychain {
    home: String,
    checked: Instant,
    raw: Option<String>,
}

static KEYCHAIN: LazyLock<Mutex<Option<CachedKeychain>>> = LazyLock::new(|| Mutex::new(None));

fn claude_dir(env: &DirResolverEnv) -> PathBuf {
    env.env
        .get("CLAUDE_CONFIG_DIR")
        .filter(|dir| !dir.trim().is_empty())
        .map_or_else(|| PathBuf::from(&env.home).join(".claude"), PathBuf::from)
}

fn keychain(env: &DirResolverEnv) -> Option<String> {
    if env.platform != "macos"
        || env.env.contains_key("CLAUDE_CONFIG_DIR")
        || env.env.contains_key("CCP_CONFIG_DIR")
    {
        return None;
    }
    let user = env.env.get("USER").or_else(|| env.env.get("LOGNAME"))?;
    let mut cache = KEYCHAIN.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(cached) = cache.as_ref()
        && cached.home == env.home
        && cached.checked.elapsed() < Duration::from_secs(30)
    {
        return cached.raw.clone();
    }
    let raw = SystemKeychain
        .read("Claude Code-credentials", user)
        .ok()
        .flatten();
    *cache = Some(CachedKeychain {
        home: env.home.clone(),
        checked: Instant::now(),
        raw: raw.clone(),
    });
    raw
}

pub fn detect(env: &DirResolverEnv) -> Option<StoredAuth> {
    // An isolated proxy must not silently adopt the operator's real login.
    if env.env.contains_key("CCP_CONFIG_DIR") && !env.env.contains_key("CLAUDE_CONFIG_DIR") {
        return None;
    }
    let dir = claude_dir(env);
    let raw =
        keychain(env).or_else(|| std::fs::read_to_string(dir.join(".credentials.json")).ok())?;
    let payload: Value = serde_json::from_str(&raw).ok()?;
    let oauth = payload.get("claudeAiOauth")?;
    let access = oauth.get("accessToken")?.as_str()?.trim();
    let refresh = oauth.get("refreshToken")?.as_str()?.trim();
    if access.is_empty() || access == "unused" || refresh.is_empty() {
        return None;
    }
    let profile_files = if env.env.contains_key("CLAUDE_CONFIG_DIR") {
        vec![dir.join(".claude.json"), dir.join(".config.json")]
    } else {
        vec![
            PathBuf::from(&env.home).join(".claude.json"),
            dir.join(".claude.json"),
        ]
    };
    let profile = profile_files
        .into_iter()
        .find_map(|path| {
            let raw = std::fs::read_to_string(path).ok()?;
            serde_json::from_str::<Value>(&raw)
                .ok()?
                .get("oauthAccount")
                .cloned()
        })
        .unwrap_or(Value::Null);
    Some(StoredAuth {
        access: access.to_string(),
        refresh: refresh.to_string(),
        expires: oauth.get("expiresAt")?.as_u64()?,
        email: profile
            .get("emailAddress")
            .and_then(Value::as_str)
            .map(|email| email.trim().to_ascii_lowercase())
            .filter(|email| email.contains('@')),
        account_id: profile
            .get("accountUuid")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string),
        plan: oauth
            .get("subscriptionType")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

pub fn import_login(accounts: &Accounts, env: &DirResolverEnv) -> Result<()> {
    let existing = accounts.list()?;
    // An empty index means the user signed out. Do not resurrect that login.
    if accounts.has_index_file() && existing.is_empty() {
        return Ok(());
    }
    let Some(local) = detect(env) else {
        return Ok(());
    };
    for account in &existing {
        let store = accounts.store::<StoredAuth>(&account.id);
        let Ok(Some(saved)) = store.load() else {
            continue;
        };
        let same_account = local.account_id.is_some() && local.account_id == saved.account_id;
        let same_token = local.access == saved.access || local.refresh == saved.refresh;
        if same_account || same_token {
            let mut next = saved.clone();
            if local.expires > saved.expires {
                next.access = local.access;
                next.refresh = local.refresh;
                next.expires = local.expires;
            }
            next.email = next.email.or(local.email);
            next.account_id = next.account_id.or(local.account_id);
            next.plan = next.plan.or(local.plan);
            if next != saved {
                store.save(next)?;
            }
            return Ok(());
        }
    }
    accounts.import_once(
        &serde_json::to_value(&local)?,
        local.identity(),
        local.email.as_deref(),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn import_copies_native_login_names_it_by_email_and_respects_sign_out() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("native");
        std::fs::create_dir_all(&dir).unwrap();
        let native = json!({"claudeAiOauth": {
            "accessToken": "local-access", "refreshToken": "local-refresh", "expiresAt": 4_102_444_800_000u64, "subscriptionType": "max"
        }}).to_string();
        std::fs::write(dir.join(".credentials.json"), &native).unwrap();
        std::fs::write(
            dir.join(".claude.json"),
            json!({"oauthAccount": {
                "emailAddress": "Founder@Example.com", "accountUuid": "local-account"
            }})
            .to_string(),
        )
        .unwrap();
        let env = DirResolverEnv {
            platform: "macos".into(),
            home: temp.path().to_string_lossy().into_owned(),
            env: HashMap::from([
                (
                    "CCP_CONFIG_DIR".into(),
                    temp.path().join("proxy").to_string_lossy().into_owned(),
                ),
                (
                    "CLAUDE_CONFIG_DIR".into(),
                    dir.to_string_lossy().into_owned(),
                ),
            ]),
        };
        let accounts = Accounts::in_dir("claude", temp.path().join("proxy/claude"));
        import_login(&accounts, &env).unwrap();
        let first = accounts.current().unwrap().unwrap();
        assert_eq!(first.name, "founder@example.com");
        assert_eq!(first.identity.as_deref(), Some("local-account"));
        import_login(&accounts, &env).unwrap();
        assert_eq!(accounts.list().unwrap().len(), 1);
        accounts.remove_all().unwrap();
        import_login(&accounts, &env).unwrap();
        assert!(accounts.list().unwrap().is_empty());
        assert_eq!(
            std::fs::read_to_string(dir.join(".credentials.json")).unwrap(),
            native
        );
    }

    #[test]
    fn isolated_config_never_reads_the_real_native_login() {
        let env = DirResolverEnv {
            platform: "macos".into(),
            home: "/unused".into(),
            env: HashMap::from([("CCP_CONFIG_DIR".into(), "/isolated".into())]),
        };
        assert!(detect(&env).is_none());
    }
}
