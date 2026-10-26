//! More than one saved login per provider.
//!
//! `<config>/<provider>/accounts.json` lists the accounts and says which one is
//! current. Each account's login sits in `<config>/<provider>/accounts/<id>.json`,
//! the same JSON the one login used to keep in `auth.json`. A token refresh
//! rewrites only its own account's file, so two accounts never write the same
//! file. A short file lock protects list changes and rejects late refresh writes.

use std::fs::{self, File};
use std::io;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::{AuthStorage, FileAuthStore, write_atomically};
use crate::provider::AuthState;

/// The fallback for the first account, and the name an old single login keeps.
const FIRST_NAME: &str = "main";
const MAX_NAME_LEN: usize = 32;
const MAX_EMAIL_NAME_LEN: usize = 254;

/// What a provider gives the account commands to work with. A provider that
/// returns this from `CliHandlers::accounts` can keep more than one login.
pub trait AccountSupport: Send + Sync {
    /// Signs in (in the browser, or with a device code) and returns the login to keep.
    fn sign_in(&self, device: bool) -> Result<Value>;
    /// Who a saved login belongs to, when it says.
    fn identity(&self, login: &Value) -> Option<String>;
    /// An explicit email from the saved sign-in, never an account ID or a key.
    fn email(&self, _login: &Value) -> Option<String> {
        None
    }
    /// What a saved login holds.
    fn state(&self, login: &Value) -> AuthState;
    /// Something the monitor should say about this account that its quota
    /// numbers do not: what its plan can't do.
    fn note(&self, _login: &Value) -> Option<String> {
        None
    }
    /// Why this login can't serve requests at all, when its plan says so (a
    /// free Claude plan). Rotation skips it and choosing it is refused. Written
    /// as a clause: "cc-proxy won't use `work`: {reason}."
    fn unusable(&self, _login: &Value) -> Option<&'static str> {
        None
    }
    /// Where the provider kept its one login before accounts existed.
    fn single_login(&self) -> Box<dyn AuthStorage<Value>>;
    /// Copy an existing local login without taking ownership of its source.
    fn prepare(&self, _accounts: &Accounts) -> Result<()> {
        Ok(())
    }
    /// A credential override pins all requests to one login and disables rotation.
    fn pinned_credentials(&self) -> Option<&'static str> {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// Changes on a new sign-in, so old requests cannot write to its replacement.
    pub id: String,
    pub name: String,
    /// Who the sign-in belongs to, when the provider says. Signing in as the same
    /// identity again refreshes the account instead of saving a second copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Index {
    current: Option<String>,
    accounts: Vec<Account>,
}

impl Index {
    fn position(&self, name: &str) -> Option<usize> {
        self.accounts
            .iter()
            .position(|account| account.name == name)
    }
}

#[derive(Clone)]
pub struct Accounts {
    provider: String,
    dir: PathBuf,
}

/// Normalizes a short label or a shell-safe email used as an account name.
pub fn clean_name(raw: &str) -> Result<String> {
    let name = raw.trim().to_ascii_lowercase();
    let starts_well = name
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_alphanumeric());
    let fits = if let Some((local, domain)) = name.split_once('@') {
        name.len() <= MAX_EMAIL_NAME_LEN
            && !domain.is_empty()
            && local
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || b"._+-".contains(&ch))
            && domain
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || b".-".contains(&ch))
    } else {
        name.len() <= MAX_NAME_LEN
            && name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    };
    if !starts_well || !fits {
        bail!(
            "`{}` can't be an account name. Use a short name such as `work`: up to {MAX_NAME_LEN} letters, numbers, `-` or `_`. An email can use letters, numbers, `.`, `_`, `+` and `-`, up to {MAX_EMAIL_NAME_LEN} characters. Start with a letter or number.",
            raw.trim().escape_debug()
        );
    }
    Ok(name)
}

/// A better name for an account, when its name is only a number in the list
/// (`main`, `account-2`) and the sign-in says an email. A name the person chose
/// is never second-guessed.
pub fn suggested_name(name: &str, email: Option<&str>) -> Option<String> {
    let numbered = name == FIRST_NAME
        || name
            .strip_prefix("account-")
            .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()));
    if !numbered {
        return None;
    }
    clean_name(email?).ok().filter(|email| email.contains('@'))
}

impl Accounts {
    pub fn new(provider: &str) -> Self {
        Self::in_dir(provider, crate::paths::config_dir().join(provider))
    }

    /// The accounts of a provider, with a login from before accounts existed
    /// moved in first.
    pub fn open(provider: &str, support: &dyn AccountSupport) -> Result<Self> {
        let accounts = Self::new(provider);
        accounts.migrate(support.single_login().as_ref(), |login| {
            support.identity(login)
        })?;
        support.prepare(&accounts)?;
        Ok(accounts)
    }

    /// What the current account holds, for a provider's `auth_state`.
    pub fn current_state(provider: &str, support: &dyn AccountSupport) -> AuthState {
        let state = || -> Result<Option<AuthState>> {
            let accounts = Self::open(provider, support)?;
            let Some(account) = accounts.current()? else {
                return Ok(None);
            };
            Ok(accounts
                .login(&account.name)?
                .map(|login| support.state(&login)))
        };
        state().ok().flatten().unwrap_or(AuthState::Missing)
    }

    pub(crate) fn in_dir(provider: &str, dir: PathBuf) -> Self {
        Self {
            provider: provider.to_string(),
            dir,
        }
    }

    fn index_path(&self) -> PathBuf {
        self.dir.join("accounts.json")
    }

    pub(crate) fn has_index_file(&self) -> bool {
        self.index_path().exists()
    }

    /// Copy a native login only before the first account list is saved.
    pub(crate) fn import_once(
        &self,
        login: &Value,
        identity: Option<String>,
        email: Option<&str>,
    ) -> Result<()> {
        self.locked(|| {
            if self.has_index_file() {
                return Ok(());
            }
            self.save_unlocked(&self.next_name(email)?, login, identity)
        })
    }

    /// Where an account's login is kept.
    pub fn file(&self, id: &str) -> PathBuf {
        self.dir.join("accounts").join(format!("{id}.json"))
    }

    /// The store of one account's login, for a provider's token manager.
    pub fn store<T>(&self, id: &str) -> AccountAuthStore<T>
    where
        T: Serialize + serde::de::DeserializeOwned + Send + Sync + Clone,
    {
        AccountAuthStore {
            accounts: self.clone(),
            id: id.to_string(),
            file: self.file_store(id),
        }
    }

    /// Pins the selected login, rejecting IDs retired by sign-out or replacement.
    pub fn for_request<T>(&self, id: Option<&str>) -> Result<AccountAuthStore<T>>
    where
        T: Serialize + serde::de::DeserializeOwned + Send + Sync + Clone,
    {
        let account = match id {
            Some(id) => Some(self.list()?.into_iter().find(|account| account.id == id)
                .context("This account was signed out or replaced. Retry the request to use the current account.")?),
            None => self.current()?,
        };
        Ok(self.store(
            account
                .as_ref()
                .map_or("missing", |account| account.id.as_str()),
        ))
    }

    fn file_store<T>(&self, id: &str) -> FileAuthStore<T>
    where
        T: Serialize + serde::de::DeserializeOwned + Send + Sync + Clone,
    {
        let file = self.file(id).to_string_lossy().into_owned();
        FileAuthStore::new(file.clone(), file)
    }

    fn read_index(&self) -> Result<Index> {
        let path = self.index_path();
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Index::default()),
            Err(error) => {
                return Err(error).with_context(|| format!("can't read {}", path.display()));
            }
        };
        let index: Index = serde_json::from_str(&raw).with_context(|| {
            format!(
                "can't read the {} account list at {}. Your logins are still in {}. Fix the account list, then try again.",
                self.provider,
                path.display(),
                self.dir.join("accounts").display()
            )
        })?;
        let mut names = std::collections::HashSet::new();
        let mut ids = std::collections::HashSet::new();
        for account in &index.accounts {
            if clean_name(&account.name)? != account.name
                || uuid::Uuid::parse_str(&account.id).is_err()
                || !names.insert(&account.name)
                || !ids.insert(&account.id)
            {
                bail!(
                    "Invalid or repeated account in {}. Fix the account list, then try again.",
                    path.display()
                );
            }
        }
        if index
            .current
            .as_ref()
            .is_some_and(|name| !names.contains(name))
        {
            bail!(
                "The current account in {} is not listed. Fix the account list, then try again.",
                path.display()
            );
        }
        Ok(index)
    }

    fn write_index(&self, index: &Index) -> Result<()> {
        write_atomically(&self.index_path().to_string_lossy(), index)
    }

    /// Runs `change` while no other cc-proxy process changes the list.
    fn locked<R>(&self, change: impl FnOnce() -> Result<R>) -> Result<R> {
        fs::create_dir_all(&self.dir)?;
        let lock = File::create(self.dir.join("accounts.lock"))?;
        lock.lock()
            .with_context(|| format!("can't lock the {} account list", self.provider))?;
        change()
    }

    pub fn list(&self) -> Result<Vec<Account>> {
        Ok(self.read_index()?.accounts)
    }

    /// The account called `raw`, or an error that lists the accounts there are.
    pub fn require(&self, raw: &str) -> Result<Account> {
        let index = self.read_index()?;
        let name = raw.trim().to_ascii_lowercase();
        match index.position(&name) {
            Some(at) => Ok(index.accounts[at].clone()),
            None => bail!(self.missing(&name, &index)),
        }
    }

    pub fn find(&self, name: &str) -> Result<Option<Account>> {
        Ok(self
            .list()?
            .into_iter()
            .find(|account| account.name == name))
    }

    /// The account requests use now. When the list names none, the first one.
    pub fn current(&self) -> Result<Option<Account>> {
        let index = self.read_index()?;
        let at = index
            .current
            .as_deref()
            .and_then(|name| index.position(name))
            .unwrap_or(0);
        Ok(index.accounts.into_iter().nth(at))
    }

    /// A free email name when available, else `main`, `account-2`, `account-3`.
    pub fn next_name(&self, email: Option<&str>) -> Result<String> {
        let taken = self.list()?;
        let free = |name: &str| taken.iter().all(|account| account.name != name);
        if let Some(email) = email
            .and_then(|email| clean_name(email).ok())
            .filter(|email| email.contains('@'))
        {
            if free(&email) {
                return Ok(email);
            }
            if let Some(name) = (2..)
                .map(|number| format!("{email}-{number}"))
                .take_while(|name| name.len() <= MAX_EMAIL_NAME_LEN)
                .find(|name| free(name))
            {
                return Ok(name);
            }
        }
        if free(FIRST_NAME) {
            return Ok(FIRST_NAME.to_string());
        }
        Ok((2..)
            .map(|number| format!("account-{number}"))
            .find(|name| free(name))
            .expect("an unbounded range has a free name"))
    }

    /// The saved login, as the provider wrote it.
    pub fn login(&self, name: &str) -> Result<Option<Value>> {
        self.file_store(&self.require(name)?.id).load()
    }

    /// A name worth offering for `account`, when it has only a number for a
    /// name and its saved sign-in says an email that no other account uses.
    pub fn suggest_name(&self, support: &dyn AccountSupport, account: &Account) -> Option<String> {
        let login: Value = self.file_store(&account.id).load().ok()??;
        let email = support.email(&login);
        let suggestion = suggested_name(&account.name, email.as_deref())?;
        let taken = self.list().ok()?;
        taken
            .iter()
            .all(|other| other.name != suggestion)
            .then_some(suggestion)
    }

    /// Why `account` can't serve requests, when its saved plan says so.
    pub fn unusable(
        &self,
        support: &dyn AccountSupport,
        account: &Account,
    ) -> Option<&'static str> {
        let login: Value = self.file_store(&account.id).load().ok()??;
        support.unusable(&login)
    }

    /// Saves only if the slot still matches the command's starting state.
    /// A browser sign-in can take minutes; another process may change it meanwhile.
    pub fn install(
        &self,
        name: &str,
        expected_id: Option<&str>,
        login: &Value,
        identity: Option<String>,
    ) -> Result<()> {
        let name = clean_name(name)?;
        let name = name.as_str();
        self.locked(|| {
            let index = self.read_index()?;
            let current_id = index.position(name).map(|at| index.accounts[at].id.as_str());
            if current_id != expected_id {
                bail!("The `{name}` account changed while you were signing in. Nothing was replaced. Run the command again.");
            }
            if let Some(identity) = &identity
                && let Some(same) = index.accounts.iter().find(|a| a.identity.as_ref() == Some(identity) && a.name != name)
            {
                bail!("That sign-in is already saved as `{}`. Nothing changed. Sign in again to that account instead.", same.name);
            }
            self.save_unlocked(name, login, identity)
        })
    }

    #[cfg(test)]
    fn save(&self, name: &str, login: &Value, identity: Option<String>) -> Result<()> {
        self.locked(|| self.save_unlocked(name, login, identity))
    }

    fn save_unlocked(&self, name: &str, login: &Value, identity: Option<String>) -> Result<()> {
        let name = clean_name(name)?;
        let mut index = self.read_index()?;
        let old_id = index
            .position(&name)
            .map(|at| index.accounts[at].id.clone());
        let account = Account {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.clone(),
            identity,
        };
        // The login first: a crash leaves an unlisted file, not a missing login.
        write_atomically(&self.file(&account.id).to_string_lossy(), login)?;
        match index.position(&name) {
            Some(at) => index.accounts[at] = account,
            None => index.accounts.push(account),
        }
        if index.current.is_none() {
            index.current = Some(name);
        }
        self.write_index(&index)?;
        if let Some(id) = old_id {
            remove_file_if_present(&self.file(&id))?;
        }
        Ok(())
    }

    /// A late request must not overwrite a switch made by a newer request or
    /// by `auth use` while it was waiting for the provider. Returns whether it
    /// switched.
    pub(crate) fn switch_if_current(&self, expected_id: &str, next_id: &str) -> Result<bool> {
        self.locked(|| {
            let mut index = self.read_index()?;
            let current = index
                .current
                .as_deref()
                .and_then(|name| index.position(name));
            if current.is_some_and(|at| index.accounts[at].id == expected_id)
                && let Some(next) = index.accounts.iter().find(|a| a.id == next_id)
            {
                index.current = Some(next.name.clone());
                self.write_index(&index)?;
                return Ok(true);
            }
            Ok(false)
        })
    }

    /// Gives an account a new name. Its id, login and place stay as they are,
    /// so waits, usage and request history stay with it. Returns the account
    /// under its new name.
    pub fn rename(&self, old: &str, new: &str) -> Result<Account> {
        let new = clean_name(new)?;
        self.locked(|| {
            let mut index = self.read_index()?;
            let Some(at) = index.position(old) else {
                bail!(self.missing(old, &index));
            };
            if new != old && index.position(&new).is_some() {
                bail!(
                    "{} already has an account called `{new}`. Pick another name.",
                    crate::ui::provider_name(&self.provider)
                );
            }
            index.accounts[at].name = new.clone();
            // The index keeps the current account by name.
            if index.current.as_deref() == Some(old) {
                index.current = Some(new);
            }
            let account = index.accounts[at].clone();
            self.write_index(&index)?;
            Ok(account)
        })
    }

    pub fn set_current(&self, name: &str) -> Result<()> {
        self.locked(|| {
            let mut index = self.read_index()?;
            if index.position(name).is_none() {
                bail!(self.missing(name, &index));
            }
            index.current = Some(name.to_string());
            self.write_index(&index)
        })
    }

    /// Removes one account and its login. When it was the current one, the
    /// first account left takes over; returns that account.
    pub fn remove(&self, account: &Account) -> Result<Option<Account>> {
        self.locked(|| {
            let mut index = self.read_index()?;
            let name = account.name.as_str();
            let Some(at) = index.position(name) else {
                bail!(self.missing(name, &index));
            };
            if index.accounts[at].id != account.id {
                bail!("The `{name}` login was replaced. Nothing was removed. Refresh the account list, then try again.");
            }
            let removed = index.accounts.remove(at);
            let was_current = index.current.as_deref() == Some(name);
            if was_current {
                index.current = index.accounts.first().map(|account| account.name.clone());
            }
            // The list first, so a crash leaves a file nobody lists.
            self.write_index(&index)?;
            remove_file_if_present(&self.file(&removed.id))?;
            Ok(was_current
                .then(|| index.accounts.first().cloned())
                .flatten())
        })
    }

    pub fn remove_all(&self) -> Result<()> {
        self.locked(|| {
            let index = self.read_index()?;
            self.write_index(&Index::default())?;
            for account in &index.accounts {
                remove_file_if_present(&self.file(&account.id))?;
            }
            Ok(())
        })
    }

    fn missing(&self, name: &str, index: &Index) -> String {
        let names: Vec<&str> = index.accounts.iter().map(|a| a.name.as_str()).collect();
        format!(
            "No {} account is called `{name}`. You have: {}.",
            crate::ui::provider_name(&self.provider),
            names.join(", ")
        )
    }

    /// Moves a login saved before accounts existed into the first account.
    /// Does nothing once the list exists. The old copy is removed only after the
    /// new one is saved, so a failure on the way loses nothing.
    pub fn migrate(
        &self,
        old: &dyn AuthStorage<Value>,
        identity_of: impl Fn(&Value) -> Option<String>,
    ) -> Result<()> {
        // Most calls end here, so a provider nobody signed in to is read without
        // taking the lock or creating a file.
        if self.index_path().exists() || old.load()?.is_none() {
            return Ok(());
        }
        self.locked(|| {
            if self.index_path().exists() {
                return Ok(());
            }
            let Some(login) = old.load()? else {
                return Ok(());
            };
            self.save_unlocked(FIRST_NAME, &login, identity_of(&login))?;
            // A copy left behind is harmless: nothing reads it any more.
            let _ = old.clear();
            Ok(())
        })
    }
}

/// A refresh writes only to a still-listed account. A sign-out or a new sign-in
/// retires its ID, so a late refresh cannot restore an old login.
pub struct AccountAuthStore<T>
where
    T: Serialize + serde::de::DeserializeOwned + Send + Sync + Clone,
{
    accounts: Accounts,
    id: String,
    file: FileAuthStore<T>,
}

impl<T> AuthStorage<T> for AccountAuthStore<T>
where
    T: Serialize + serde::de::DeserializeOwned + Send + Sync + Clone,
{
    fn load(&self) -> Result<Option<T>> {
        if !self
            .accounts
            .list()?
            .iter()
            .any(|account| account.id == self.id)
        {
            return Ok(None);
        }
        self.file.load()
    }

    fn save(&self, value: T) -> Result<()> {
        self.accounts.locked(|| {
            if !self.accounts.list()?.iter().any(|a| a.id == self.id) {
                bail!("This account was signed out or replaced. Try the request again to use the current account.");
            }
            self.file.save(value)
        })
    }

    fn clear(&self) -> Result<()> {
        self.file.clear()
    }

    fn path(&self) -> String {
        self.file.path()
    }
}

fn remove_file_if_present(path: &std::path::Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

/// A provider whose login is an API key: each account holds one, so a provider
/// can keep several and the same rotation picks between them.
pub struct ApiKeySupport {
    provider: &'static str,
    /// Where to get a key, printed before `auth add` asks for one.
    hint: &'static str,
    /// Environment variables that pin every request to one key.
    env: &'static [&'static str],
}

impl ApiKeySupport {
    pub const fn new(
        provider: &'static str,
        hint: &'static str,
        env: &'static [&'static str],
    ) -> Self {
        Self {
            provider,
            hint,
            env,
        }
    }

    /// The key a request uses: the environment's when it is set, else the
    /// selected account's, else the provider's one key from config.
    pub fn key(&self, configured: impl FnOnce() -> Option<String>) -> Option<String> {
        self.key_for(None, configured)
    }

    /// Like `key`, for the account `account` when rotation pinned one.
    pub fn key_for(
        &self,
        account: Option<&str>,
        configured: impl FnOnce() -> Option<String>,
    ) -> Option<String> {
        if self.pinned_credentials().is_some() {
            return configured();
        }
        saved_api_key(self.provider, account).or_else(configured)
    }
}

impl AccountSupport for ApiKeySupport {
    fn sign_in(&self, _device: bool) -> Result<Value> {
        println!("{}", self.hint);
        let key = crate::prompt::read_hidden_line("API key: ")?;
        let key = key.trim();
        if key.is_empty() {
            bail!("no API key provided");
        }
        Ok(serde_json::json!({ "apiKey": key }))
    }

    /// Keys are saved and switched by name; they carry no identity to show.
    fn identity(&self, _login: &Value) -> Option<String> {
        None
    }

    fn state(&self, login: &Value) -> AuthState {
        api_key_state(login)
    }

    /// A key provider keeps its first key in config or the environment, so
    /// there is no single login file to move into accounts.
    fn single_login(&self) -> Box<dyn AuthStorage<Value>> {
        Box::new(NoLogin)
    }

    fn pinned_credentials(&self) -> Option<&'static str> {
        self.env
            .iter()
            .copied()
            .find(|name| std::env::var(name).is_ok_and(|value| !value.trim().is_empty()))
    }
}

/// What a login that holds an API key looks like.
pub fn api_key_state(login: &Value) -> AuthState {
    match api_key_in(login) {
        Some(_) => AuthState::KeySaved,
        None => AuthState::Missing,
    }
}

/// A provider with no single login to migrate.
pub struct NoLogin;

impl AuthStorage<Value> for NoLogin {
    fn load(&self) -> Result<Option<Value>> {
        Ok(None)
    }
    fn save(&self, _value: Value) -> Result<()> {
        Ok(())
    }
    fn clear(&self) -> Result<()> {
        Ok(())
    }
    fn path(&self) -> String {
        String::new()
    }
}

/// The API key saved for one account, or for the current one when `account`
/// is not pinned.
pub fn saved_api_key(provider: &str, account: Option<&str>) -> Option<String> {
    saved_api_key_in(&Accounts::new(provider), account)
}

fn saved_api_key_in(accounts: &Accounts, account: Option<&str>) -> Option<String> {
    let login = match account {
        Some(id) => accounts
            .for_request::<Value>(Some(id))
            .ok()?
            .load()
            .ok()??,
        None => accounts.login(&accounts.current().ok()??.name).ok()??,
    };
    api_key_in(&login)
}

fn api_key_in(login: &Value) -> Option<String> {
    login
        .get("apiKey")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::auth::InMemoryAuthStore;

    fn accounts(dir: &tempfile::TempDir) -> Accounts {
        Accounts::in_dir("codex", dir.path().join("codex"))
    }

    fn names(accounts: &Accounts) -> Vec<String> {
        accounts
            .list()
            .unwrap()
            .into_iter()
            .map(|account| account.name)
            .collect()
    }

    #[test]
    fn saved_api_keys_follow_the_current_account() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(&dir);
        accounts
            .save("main", &json!({"apiKey": "sk-a"}), None)
            .unwrap();
        accounts
            .save("work", &json!({"apiKey": "sk-b"}), None)
            .unwrap();
        assert_eq!(saved_api_key_in(&accounts, None).as_deref(), Some("sk-a"));
        let work = accounts
            .list()
            .unwrap()
            .into_iter()
            .find(|account| account.name == "work")
            .unwrap();
        assert_eq!(
            saved_api_key_in(&accounts, Some(&work.id)).as_deref(),
            Some("sk-b")
        );
        accounts.set_current("work").unwrap();
        assert_eq!(saved_api_key_in(&accounts, None).as_deref(), Some("sk-b"));
        assert_eq!(saved_api_key_in(&accounts, Some("retired")), None);
    }

    #[test]
    fn a_key_account_reports_a_saved_key() {
        let support = ApiKeySupport::new("glm", "hint", &[]);
        assert_eq!(
            support.state(&json!({"apiKey": "sk-test"})),
            AuthState::KeySaved
        );
        assert_eq!(support.state(&json!({})), AuthState::Missing);
        assert_eq!(support.state(&json!({"apiKey": "   "})), AuthState::Missing);
        assert_eq!(support.email(&json!({"apiKey": "user@example.com"})), None);
    }

    #[test]
    fn the_first_account_is_current_and_the_next_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(&dir);
        assert!(accounts.current().unwrap().is_none());
        assert_eq!(accounts.next_name(None).unwrap(), "main");

        accounts
            .save("main", &json!({"access": "a"}), Some("id-a".into()))
            .unwrap();
        accounts
            .save("work", &json!({"access": "w"}), None)
            .unwrap();
        assert_eq!(names(&accounts), ["main", "work"]);
        assert_eq!(accounts.current().unwrap().unwrap().name, "main");
        assert_eq!(accounts.next_name(None).unwrap(), "account-2");
        assert_eq!(
            accounts.login("work").unwrap().unwrap(),
            json!({"access": "w"})
        );

        accounts.set_current("work").unwrap();
        assert_eq!(accounts.current().unwrap().unwrap().name, "work");
        let error = accounts.set_current("nope").unwrap_err().to_string();
        assert!(
            error.contains("`nope`") && error.contains("main, work"),
            "{error}"
        );
    }

    #[test]
    fn request_stores_pin_current_and_reject_retired_or_unlisted_files() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(&dir);
        accounts
            .install("main", None, &json!({"access":"first"}), None)
            .unwrap();
        let first = accounts.require("main").unwrap();
        let pinned = accounts.for_request::<Value>(None).unwrap();
        accounts
            .install("work", None, &json!({"access":"second"}), None)
            .unwrap();
        accounts.set_current("work").unwrap();
        assert_eq!(pinned.load().unwrap().unwrap()["access"], "first");
        assert_eq!(
            accounts
                .for_request::<Value>(None)
                .unwrap()
                .load()
                .unwrap()
                .unwrap()["access"],
            "second"
        );
        assert!(accounts.install(" MAIN ", None, &json!({}), None).is_err());
        accounts.remove(&accounts.require("main").unwrap()).unwrap();
        write_atomically(
            &accounts.file(&first.id).to_string_lossy(),
            &json!({"access":"leftover"}),
        )
        .unwrap();
        assert!(pinned.load().unwrap().is_none());
        assert!(accounts.for_request::<Value>(Some(&first.id)).is_err());
        assert!(
            accounts
                .for_request::<Value>(Some("../../outside"))
                .is_err()
        );
    }

    #[test]
    fn saving_again_replaces_the_login_and_keeps_the_place_in_the_list() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(&dir);
        accounts
            .save("main", &json!({"access": "old"}), Some("id-a".into()))
            .unwrap();
        accounts
            .save("work", &json!({"access": "w"}), None)
            .unwrap();
        accounts.set_current("work").unwrap();

        accounts
            .save("main", &json!({"access": "new"}), Some("id-b".into()))
            .unwrap();
        assert_eq!(names(&accounts), ["main", "work"]);
        assert_eq!(accounts.current().unwrap().unwrap().name, "work");
        assert_eq!(accounts.login("main").unwrap().unwrap()["access"], "new");
        assert_eq!(
            accounts.find("main").unwrap().unwrap().identity.as_deref(),
            Some("id-b")
        );
    }

    #[test]
    fn removing_the_current_account_hands_over_to_the_first_left() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(&dir);
        for name in ["main", "work", "home"] {
            accounts.save(name, &json!({"access": name}), None).unwrap();
        }
        accounts.set_current("work").unwrap();
        let work_file = accounts.file(&accounts.require("work").unwrap().id);
        let main_file = accounts.file(&accounts.require("main").unwrap().id);

        // Another account going away changes nothing.
        assert!(
            accounts
                .remove(&accounts.require("home").unwrap())
                .unwrap()
                .is_none()
        );
        assert_eq!(accounts.current().unwrap().unwrap().name, "work");

        let next = accounts
            .remove(&accounts.require("work").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(next.name, "main");
        assert_eq!(accounts.current().unwrap().unwrap().name, "main");
        assert!(!work_file.exists());
        assert!(main_file.exists());

        accounts.remove_all().unwrap();
        assert!(accounts.list().unwrap().is_empty());
        assert!(!main_file.exists());
        assert!(accounts.index_path().exists());
    }

    #[test]
    fn removing_an_old_selection_never_deletes_its_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(&dir);
        accounts
            .install("work", None, &json!({"access": "old"}), None)
            .unwrap();
        let old = accounts.require("work").unwrap();
        accounts
            .install("work", Some(&old.id), &json!({"access": "new"}), None)
            .unwrap();
        let replacement = accounts.require("work").unwrap();
        let error = accounts.remove(&old).unwrap_err().to_string();
        assert!(
            error.contains("replaced") && error.contains("Nothing was removed"),
            "{error}"
        );
        assert_eq!(accounts.require("work").unwrap(), replacement);
        assert_eq!(accounts.login("work").unwrap().unwrap()["access"], "new");
    }

    #[test]
    fn a_list_that_cannot_be_read_is_never_treated_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(&dir);
        accounts
            .save("main", &json!({"access": "a"}), None)
            .unwrap();
        let main_file = accounts.file(&accounts.require("main").unwrap().id);
        fs::write(accounts.index_path(), "{ not json").unwrap();

        let error = format!("{:#}", accounts.list().unwrap_err());
        assert!(
            error.contains("account list") && error.contains("accounts.json"),
            "{error}"
        );
        // Writing is refused too, so the logins are not orphaned by a fresh list.
        assert!(accounts.save("work", &json!({}), None).is_err());
        assert_eq!(
            fs::read_to_string(accounts.index_path()).unwrap(),
            "{ not json"
        );
        assert!(main_file.exists());
    }

    #[test]
    fn names_are_cleaned_or_refused() {
        assert_eq!(clean_name("  Work ").unwrap(), "work");
        assert_eq!(clean_name("a_b-2").unwrap(), "a_b-2");
        assert_eq!(
            clean_name(" Ana.Silva+code@Example.COM ").unwrap(),
            "ana.silva+code@example.com"
        );
        for bad in [
            "",
            "-x",
            "a b",
            "../x",
            "x/y",
            "é",
            &"a".repeat(33),
            "a@",
            "a@@example.com",
            "a/b@example.com",
            "a\\b@example.com",
            "a;echo@example.com",
            "a$(id)@example.com",
            "a`id`@example.com",
            "a\"b@example.com",
            "a'b@example.com",
            "a\nb@example.com",
            "a\u{1b}[31m@example.com",
            "-a@example.com",
        ] {
            assert!(clean_name(bad).is_err(), "{bad:?}");
        }
        let unsafe_error = clean_name("a\u{1b}[31m@example.com")
            .unwrap_err()
            .to_string();
        assert!(!unsafe_error.contains('\u{1b}'));
        let error = clean_name("a b").unwrap_err().to_string();
        assert!(error.contains("`a b`") && error.contains("work"), "{error}");
    }

    #[test]
    fn email_defaults_skip_taken_names_and_still_use_uuid_files() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(&dir);
        let email = "long.account+tag@subdomain.example.com";
        assert_eq!(accounts.next_name(Some(email)).unwrap(), email);
        accounts
            .install(email, None, &json!({"user": "one"}), Some("one".into()))
            .unwrap();
        let first = accounts.require(email).unwrap();
        assert_eq!(
            accounts.file(&first.id).file_name().unwrap(),
            format!("{}.json", first.id).as_str()
        );
        assert_eq!(
            accounts.next_name(Some(email)).unwrap(),
            format!("{email}-2")
        );
        accounts
            .install(
                &format!("{email}-2"),
                None,
                &json!({"user": "two"}),
                Some("two".into()),
            )
            .unwrap();
        assert_eq!(
            accounts.next_name(Some(email)).unwrap(),
            format!("{email}-3")
        );
        // A name claimed after it was chosen must never overwrite that login.
        assert!(
            accounts
                .install(email, None, &json!({"user": "late"}), Some("late".into()))
                .is_err()
        );
        assert_eq!(accounts.require(email).unwrap(), first);
        assert_eq!(accounts.login(email).unwrap().unwrap()["user"], "one");
    }

    #[test]
    fn unusable_email_defaults_fall_back_to_a_free_short_name() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(&dir);
        for email in [
            "opaque-id",
            "",
            "a/b@example.com",
            "a;echo@example.com",
            "a\u{1b}@example.com",
        ] {
            assert_eq!(accounts.next_name(Some(email)).unwrap(), "main");
        }
        let longest = format!(
            "{}@{}.{}.{}.com",
            "a".repeat(64),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(57)
        );
        assert_eq!(longest.len(), MAX_EMAIL_NAME_LEN);
        assert_eq!(accounts.next_name(Some(&longest)).unwrap(), longest);
        accounts.install(&longest, None, &json!({}), None).unwrap();
        assert_eq!(accounts.next_name(Some(&longest)).unwrap(), "main");
        accounts.install("main", None, &json!({}), None).unwrap();
        assert_eq!(accounts.next_name(Some(&longest)).unwrap(), "account-2");
        assert_eq!(
            accounts.next_name(Some(&format!("a{longest}"))).unwrap(),
            "account-2"
        );
        let room_for_suffix = &longest[2..];
        accounts
            .install(room_for_suffix, None, &json!({}), None)
            .unwrap();
        let suffixed = accounts.next_name(Some(room_for_suffix)).unwrap();
        assert_eq!(suffixed, format!("{room_for_suffix}-2"));
        assert_eq!(suffixed.len(), MAX_EMAIL_NAME_LEN);
        assert_eq!(clean_name(&suffixed).unwrap(), suffixed);
    }

    #[test]
    fn renaming_keeps_the_id_the_login_and_the_saved_choice() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(&dir);
        accounts
            .save("main", &json!({"access": "m"}), Some("id-m".into()))
            .unwrap();
        accounts
            .save("work", &json!({"access": "w"}), None)
            .unwrap();
        let before = accounts.require("main").unwrap();

        let renamed = accounts.rename("main", " Ana@Example.com ").unwrap();
        assert_eq!(renamed.name, "ana@example.com");
        assert_eq!(renamed.id, before.id);
        assert_eq!(renamed.identity, before.identity);
        assert_eq!(names(&accounts), ["ana@example.com", "work"]);
        // `main` was the saved choice: the index follows it to its new name.
        assert_eq!(accounts.current().unwrap().unwrap().id, before.id);
        assert_eq!(
            accounts.login("ana@example.com").unwrap().unwrap()["access"],
            "m"
        );
        assert!(accounts.require("main").is_err());

        // Another account being renamed leaves the choice alone.
        accounts.rename("work", "office").unwrap();
        assert_eq!(accounts.current().unwrap().unwrap().id, before.id);
    }

    #[test]
    fn a_rename_that_cannot_work_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(&dir);
        accounts.save("main", &json!({}), None).unwrap();
        accounts.save("work", &json!({}), None).unwrap();

        let taken = accounts.rename("work", "MAIN").unwrap_err().to_string();
        assert!(
            taken.contains("already has an account called `main`"),
            "{taken}"
        );
        for bad in ["", "   ", "a b", "-x"] {
            assert!(accounts.rename("work", bad).is_err(), "{bad:?}");
        }
        assert!(accounts.rename("nope", "fine").is_err());
        assert_eq!(names(&accounts), ["main", "work"]);
        // The same name again is not an error.
        assert_eq!(accounts.rename("work", "Work").unwrap().name, "work");
    }

    #[test]
    fn only_a_number_for_a_name_gets_an_email_suggestion() {
        let email = Some("Ana@Example.com");
        assert_eq!(
            suggested_name("main", email).as_deref(),
            Some("ana@example.com")
        );
        assert_eq!(
            suggested_name("account-2", email).as_deref(),
            Some("ana@example.com")
        );
        assert_eq!(
            suggested_name("account-15", email).as_deref(),
            Some("ana@example.com")
        );
        for name in [
            "work",
            "account-",
            "account-x",
            "account-2b",
            "mainframe",
            "ana@example.com",
        ] {
            assert_eq!(suggested_name(name, email), None, "{name}");
        }
        assert_eq!(suggested_name("main", None), None);
        assert_eq!(suggested_name("main", Some("not an email")), None);
        assert_eq!(suggested_name("main", Some("opaque-id")), None);
    }

    #[test]
    fn a_suggestion_reads_the_saved_email_and_skips_a_name_already_taken() {
        struct WithEmail;
        impl AccountSupport for WithEmail {
            fn sign_in(&self, _device: bool) -> Result<Value> {
                unreachable!()
            }
            fn identity(&self, _login: &Value) -> Option<String> {
                None
            }
            fn email(&self, login: &Value) -> Option<String> {
                login["email"].as_str().map(str::to_string)
            }
            fn state(&self, _login: &Value) -> AuthState {
                AuthState::Missing
            }
            fn single_login(&self) -> Box<dyn AuthStorage<Value>> {
                Box::new(NoLogin)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(&dir);
        accounts
            .save("main", &json!({"email": "ana@example.com"}), None)
            .unwrap();
        accounts.save("account-2", &json!({}), None).unwrap();
        let main = accounts.require("main").unwrap();
        let no_email = accounts.require("account-2").unwrap();

        assert_eq!(
            accounts.suggest_name(&WithEmail, &main).as_deref(),
            Some("ana@example.com")
        );
        assert_eq!(accounts.suggest_name(&WithEmail, &no_email), None);
        accounts.rename("account-2", "ana@example.com").unwrap();
        assert_eq!(accounts.suggest_name(&WithEmail, &main), None);
    }

    #[test]
    fn an_old_single_login_moves_over_once() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(&dir);
        let old = InMemoryAuthStore::<Value>::new();
        old.save(json!({"access": "a", "accountId": "acct-1", "email": "ana@example.com"}))
            .unwrap();
        let identity = |login: &Value| login["accountId"].as_str().map(str::to_string);

        accounts.migrate(&old, identity).unwrap();
        assert_eq!(names(&accounts), ["main"]);
        assert_eq!(accounts.login("main").unwrap().unwrap()["access"], "a");
        assert_eq!(
            accounts.current().unwrap().unwrap().identity.as_deref(),
            Some("acct-1")
        );
        assert!(old.load().unwrap().is_none());
        let original = accounts.current().unwrap().unwrap();

        // A login saved later is not "migrated" over the new accounts.
        old.save(json!({"access": "later", "email": "other@example.com"}))
            .unwrap();
        accounts.migrate(&old, identity).unwrap();
        assert_eq!(accounts.current().unwrap(), Some(original));
        assert_eq!(accounts.login("main").unwrap().unwrap()["access"], "a");
        assert!(old.load().unwrap().is_some());
    }

    #[test]
    fn nothing_to_move_leaves_no_list_behind() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(&dir);
        accounts
            .migrate(&InMemoryAuthStore::<Value>::new(), |_| None)
            .unwrap();
        assert!(!accounts.index_path().exists());
    }

    #[test]
    fn two_processes_adding_at_once_both_end_up_in_the_list() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("codex");
        let handles: Vec<_> = (0..8)
            .map(|number| {
                let root = root.clone();
                std::thread::spawn(move || {
                    // Each thread opens its own handle, as a separate process would.
                    Accounts::in_dir("codex", root)
                        .save(&format!("acct-{number}"), &json!({"n": number}), None)
                        .unwrap();
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let accounts = Accounts::in_dir("codex", root);
        assert_eq!(accounts.list().unwrap().len(), 8);
    }

    #[test]
    fn late_refresh_cannot_restore_a_removed_or_replaced_login() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(&dir);
        accounts
            .install("work", None, &json!({"access": "old"}), None)
            .unwrap();
        let old = accounts.require("work").unwrap();
        let store = accounts.store::<Value>(&old.id);
        accounts
            .install("work", Some(&old.id), &json!({"access": "new"}), None)
            .unwrap();
        assert!(store.save(json!({"access": "late"})).is_err());
        assert!(!accounts.file(&old.id).exists());
        assert_eq!(accounts.login("work").unwrap().unwrap()["access"], "new");
        // A stale sign-in command cannot overwrite the replacement either.
        assert!(
            accounts
                .install("work", Some(&old.id), &json!({}), None)
                .is_err()
        );
        let current = accounts.require("work").unwrap();
        let current_store = accounts.store::<Value>(&current.id);
        accounts.remove_all().unwrap();
        assert!(current_store.save(json!({})).is_err());
        assert!(!accounts.file(&current.id).exists());
    }

    #[test]
    fn empty_index_prevents_a_leftover_legacy_login_from_returning() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(&dir);
        accounts.save("main", &json!({}), None).unwrap();
        accounts.remove_all().unwrap();
        let old = InMemoryAuthStore::<Value>::new();
        old.save(json!({"access": "leftover"})).unwrap();
        accounts.migrate(&old, |_| None).unwrap();
        assert!(accounts.list().unwrap().is_empty());
    }

    #[test]
    fn index_paths_are_validated_before_opening_a_login() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(&dir);
        accounts.save("main", &json!({}), None).unwrap();
        let mut index: Value =
            serde_json::from_str(&fs::read_to_string(accounts.index_path()).unwrap()).unwrap();
        index["accounts"][0]["id"] = json!("../../outside");
        fs::write(accounts.index_path(), index.to_string()).unwrap();
        assert!(accounts.list().is_err());
    }

    #[test]
    #[cfg(unix)]
    fn logins_are_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(&dir);
        accounts
            .save("main", &json!({"access": "a"}), None)
            .unwrap();
        let mode = |path: PathBuf| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode(accounts.file(&accounts.require("main").unwrap().id)),
            0o600
        );
        assert_eq!(mode(accounts.index_path()), 0o600);
    }
}
