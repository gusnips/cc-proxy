//! `cc-proxy auth` for a provider that can keep more than one login: sign in,
//! add another account, switch between them, sign out.

use anyhow::{Result, bail};

use crate::accounts::{Account, AccountSupport, Accounts, clean_name};
use crate::{prompt, ui};

#[derive(Debug, Default, Clone)]
pub struct LoginOptions {
    pub device: bool,
    /// Sign in again to a saved account instead of asking what to do.
    pub replace: bool,
    /// With `replace`: the account to sign in to again. Default: the one in use.
    pub account: Option<String>,
}

/// Where a new sign-in is saved.
enum Slot {
    New(Option<String>),
    Replace(Account),
}

pub struct AccountCli<'a> {
    provider: &'a str,
    support: &'a dyn AccountSupport,
    accounts: Accounts,
}

impl<'a> AccountCli<'a> {
    pub fn open(provider: &'a str, support: &'a dyn AccountSupport) -> Result<Self> {
        Ok(Self::with(
            provider,
            support,
            Accounts::open(provider, support)?,
        ))
    }

    fn with(provider: &'a str, support: &'a dyn AccountSupport, accounts: Accounts) -> Self {
        Self {
            provider,
            support,
            accounts,
        }
    }

    fn label(&self) -> &str {
        ui::provider_name(self.provider)
    }

    /// `cc-proxy auth login`: the first sign-in, or a choice between adding an
    /// account and replacing the one in use.
    pub fn login(&self, options: &LoginOptions) -> Result<()> {
        if let Some(raw) = &options.account {
            return self.sign_in(options.device, Slot::Replace(self.accounts.require(raw)?));
        }
        let Some(current) = self.accounts.current()? else {
            return self.sign_in(options.device, Slot::New(None));
        };
        if options.replace {
            return self.sign_in(options.device, Slot::Replace(current));
        }
        let (label, provider) = (self.label(), self.provider);
        if !prompt::is_interactive() {
            bail!(
                "You're already signed in to {label} as `{}`. Run `cc-proxy auth add {provider}` to add another account, or `cc-proxy auth login {provider} --replace` to sign in to this one again.",
                current.name
            );
        }
        let question = format!(
            "You're signed in to {label} as `{}`. Add another account, or replace this one?",
            current.name
        );
        let choices = [
            "Add another account".to_string(),
            format!("Replace `{}`", current.name),
        ];
        match prompt::choose(&question, &choices, 0)? {
            0 => self.add(None, options.device),
            _ => self.sign_in(options.device, Slot::Replace(current)),
        }
    }

    /// `cc-proxy auth add`: one more account. The one in use stays in use.
    pub fn add(&self, name: Option<&str>, device: bool) -> Result<()> {
        let name = match name {
            Some(raw) => {
                let name = clean_name(raw)?;
                if self.accounts.find(&name)?.is_some() {
                    bail!(
                        "{} already has an account called `{name}`. Pick another name, or run `cc-proxy auth login {} --replace --account {name}` to sign in to it again.",
                        self.label(),
                        self.provider
                    );
                }
                Some(name)
            }
            None => None,
        };
        self.sign_in(device, Slot::New(name))
    }

    fn sign_in(&self, device: bool, slot: Slot) -> Result<()> {
        let (label, provider) = (self.label(), self.provider);
        let first = self.accounts.list()?.is_empty();
        let login = self.support.sign_in(device)?;
        let identity = self.support.identity(&login);
        let same = match &identity {
            Some(identity) => self
                .accounts
                .list()?
                .into_iter()
                .find(|account| account.identity.as_ref() == Some(identity)),
            None => None,
        };
        let (name, expected_id, renewed) = match (slot, same) {
            (Slot::New(name), None) => (
                match name {
                    Some(name) => name,
                    None => self
                        .accounts
                        .next_name(self.support.email(&login).as_deref())?,
                },
                None,
                false,
            ),
            (Slot::New(_), Some(same)) => {
                let question = format!(
                    "That {label} login is already saved as `{}`. Replace its saved sign-in?",
                    same.name
                );
                if !prompt::is_interactive() {
                    bail!(
                        "That {label} login is already saved as `{}`. Run `cc-proxy login {provider} --replace --account {}` to replace it. Nothing changed.",
                        same.name,
                        same.name
                    );
                }
                if !prompt::confirm(&question, false)? {
                    println!("Nothing changed.");
                    return Ok(());
                }
                (same.name, Some(same.id), true)
            }
            (Slot::Replace(old), Some(same)) if same.name != old.name => bail!(
                "You signed in as the account saved as `{}`, not `{}`. Nothing changed. Run the command again and sign in with the account you meant.",
                same.name,
                old.name
            ),
            (Slot::Replace(old), _) => (old.name, Some(old.id), true),
        };
        self.accounts
            .install(&name, expected_id.as_deref(), &login, identity)?;
        if renewed {
            println!("Signed in to {label} again as `{name}`.");
        } else if first {
            println!("Signed in to {label} as `{name}`.");
        } else {
            println!(
                "Added {label} account `{name}`. Switch to it with `cc-proxy auth use {provider} {name}`."
            );
        }
        Ok(())
    }

    /// `cc-proxy auth use`: the account requests use from now on.
    pub fn use_account(&self, raw: Option<&str>) -> Result<()> {
        let list = self.accounts.list()?;
        let (label, provider) = (self.label(), self.provider);
        if list.is_empty() {
            bail!("You're not signed in to {label}. Run `cc-proxy auth login {provider}` first.");
        }
        let name = match raw {
            Some(raw) => self.accounts.require(raw)?.name,
            None if prompt::is_interactive() => {
                let names: Vec<String> = list.iter().map(|a| a.name.clone()).collect();
                let current = self.accounts.current()?.map(|account| account.name);
                let default = names.iter().position(|n| Some(n) == current.as_ref());
                let question = format!("Which {label} account should requests use?");
                names[prompt::choose(&question, &names, default.unwrap_or(0))?].clone()
            }
            None => bail!(
                "Say which account: `cc-proxy auth use {provider} <account>`. You have: {}.",
                names_of(&list)
            ),
        };
        self.accounts.set_current(&name)?;
        println!("{label} now uses `{name}`. A running proxy switches on its next request.");
        Ok(())
    }

    /// `cc-proxy auth logout`: one account, or every account.
    pub fn logout(&self, raw: Option<&str>, all: bool) -> Result<()> {
        let list = self.accounts.list()?;
        let (label, provider) = (self.label(), self.provider);
        if list.is_empty() {
            println!("You're not signed in to {label}.");
            return Ok(());
        }
        if all {
            self.accounts.remove_all()?;
            println!("Signed out of every {label} account.");
            return Ok(());
        }
        let name = match raw {
            Some(raw) => self.accounts.require(raw)?.name,
            None if list.len() == 1 => list[0].name.clone(),
            None if prompt::is_interactive() => {
                let mut options: Vec<String> = list.iter().map(|a| a.name.clone()).collect();
                options.push("All accounts".to_string());
                let question = format!("Which {label} account do you want to sign out of?");
                let picked = prompt::choose(&question, &options, 0)?;
                if picked == list.len() {
                    self.accounts.remove_all()?;
                    println!("Signed out of every {label} account.");
                    return Ok(());
                }
                options.swap_remove(picked)
            }
            None => bail!(
                "{label} has {} accounts: {}. Say which one with `cc-proxy auth logout {provider} <account>`, or sign out of all of them with `--all`.",
                list.len(),
                names_of(&list)
            ),
        };
        let next = self.accounts.remove(&name)?;
        if list.len() == 1 {
            println!("Signed out of {label}.");
        } else {
            println!("Signed out of {label} account `{name}`.");
        }
        if let Some(next) = next {
            println!("{label} now uses `{}`.", next.name);
        }
        Ok(())
    }

    /// `cc-proxy auth status <provider>`: every account and what it holds.
    pub fn status(&self) -> Result<()> {
        let list = self.accounts.list()?;
        let Some(current) = self.accounts.current()? else {
            bail!("Not authenticated");
        };
        let label = self.label();
        if let Some(var) = self.support.pinned_credentials() {
            println!("${var} supplies the login. Saved accounts and auto-rotate are not used.");
        }
        if list.len() == 1 {
            println!(
                "{label} `{}` (current): {}",
                current.name,
                self.describe(&current)
            );
            return Ok(());
        }
        println!(
            "{label} has {} accounts. Requests use `{}`.",
            list.len(),
            current.name
        );
        let width = list.iter().map(|a| a.name.len()).max().unwrap_or(0);
        for account in &list {
            println!("  {:<width$}  {}", account.name, self.describe(account));
        }
        Ok(())
    }

    fn describe(&self, account: &Account) -> String {
        let state = match self.accounts.login(&account.name) {
            Ok(Some(login)) => self.support.state(&login).describe(),
            Err(error) => format!("can't read the sign-in: {error:#}"),
            Ok(None) => format!(
                "no sign-in saved. Run `cc-proxy login {} --replace --account {}`",
                self.provider, account.name
            ),
        };
        match crate::rotation::Cooldowns::new().waits(self.provider, &account.id) {
            Ok(waits) if waits.is_empty() => state,
            Ok(waits) => {
                let waits = waits
                    .into_iter()
                    .map(|(model, until)| {
                        let duration = ui::duration(std::time::Duration::from_millis(
                            until.saturating_sub(crate::rotation::now_ms()),
                        ));
                        let clock = i64::try_from(until)
                            .ok()
                            .and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok())
                            .map(|at| {
                                at.to_zoned(jiff::tz::TimeZone::system())
                                    .strftime("%H:%M")
                                    .to_string()
                            });
                        let scope = model.map_or(String::new(), |model| format!(" for {model}"));
                        format!(
                            "at its limit{scope}, back in {duration}{}",
                            clock.map_or(String::new(), |clock| format!(" ({clock})"))
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                format!("{waits}. {state}")
            }
            Err(error) => format!("{state}. Can't read the cooldown: {error:#}"),
        }
    }

    /// For the all-providers list: how many accounts, when there are several.
    pub fn summary(&self) -> Option<String> {
        let list = self.accounts.list().ok()?;
        let current = self.accounts.current().ok()??;
        (list.len() > 1).then(|| format!("{} accounts, using {}", list.len(), current.name))
    }
}

fn names_of(accounts: &[Account]) -> String {
    accounts
        .iter()
        .map(|account| account.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::{Value, json};

    use super::*;
    use crate::auth::{AuthStorage, InMemoryAuthStore};
    use crate::provider::AuthState;

    /// Signs in as whoever `next` names, with no browser.
    struct Fake {
        next: Mutex<Value>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                next: Mutex::new(json!({})),
            }
        }

        fn as_user(&self, who: &str) {
            *self.next.lock().unwrap() = json!({"user": who, "access": "token"});
        }

        fn as_user_with_email(&self, who: &str, email: &str) {
            self.as_user(who);
            self.next.lock().unwrap()["email"] = json!(email);
        }
    }

    impl AccountSupport for Fake {
        fn sign_in(&self, _device: bool) -> Result<Value> {
            Ok(self.next.lock().unwrap().clone())
        }

        fn identity(&self, login: &Value) -> Option<String> {
            login["user"].as_str().map(str::to_string)
        }

        fn email(&self, login: &Value) -> Option<String> {
            login["email"].as_str().map(str::to_string)
        }

        fn state(&self, login: &Value) -> AuthState {
            AuthState::SignedIn {
                account: login["user"].as_str().map(str::to_string),
                expires_ms: None,
            }
        }

        fn single_login(&self) -> Box<dyn AuthStorage<Value>> {
            Box::new(InMemoryAuthStore::new())
        }
    }

    fn cli<'a>(support: &'a Fake, dir: &tempfile::TempDir) -> AccountCli<'a> {
        AccountCli::with(
            "codex",
            support,
            Accounts::in_dir("codex", dir.path().join("codex")),
        )
    }

    fn names(cli: &AccountCli) -> Vec<String> {
        cli.accounts
            .list()
            .unwrap()
            .into_iter()
            .map(|a| a.name)
            .collect()
    }

    #[test]
    fn new_accounts_default_to_email_but_explicit_names_win() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new();
        let cli = cli(&fake, &dir);

        fake.as_user_with_email("ana", " Ana.Silva+code@Example.COM ");
        cli.login(&LoginOptions::default()).unwrap();
        let first = cli.accounts.current().unwrap().unwrap();
        fake.as_user_with_email("bo", "bo@example.com");
        cli.add(None, false).unwrap();
        fake.as_user_with_email("cy", "cy@example.com");
        cli.add(Some("Work"), false).unwrap();

        assert_eq!(
            names(&cli),
            ["ana.silva+code@example.com", "bo@example.com", "work"]
        );
        assert_eq!(cli.accounts.current().unwrap(), Some(first.clone()));
        assert_eq!(
            cli.accounts
                .require(" ANA.SILVA+CODE@EXAMPLE.COM ")
                .unwrap(),
            first
        );
        cli.use_account(Some("bo@example.com")).unwrap();
        assert_eq!(
            cli.accounts.current().unwrap().unwrap().name,
            "bo@example.com"
        );
        cli.logout(Some("bo@example.com"), false).unwrap();
        assert_eq!(cli.accounts.current().unwrap(), Some(first));
    }

    #[test]
    fn shared_emails_get_free_names_but_duplicate_identities_do_not() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new();
        let cli = cli(&fake, &dir);
        for user in ["personal", "team", "other-team"] {
            fake.as_user_with_email(user, "shared@example.com");
            cli.add(None, false).unwrap();
        }
        let first = cli.accounts.require("shared@example.com").unwrap();
        assert_eq!(
            names(&cli),
            [
                "shared@example.com",
                "shared@example.com-2",
                "shared@example.com-3"
            ]
        );
        assert_eq!(
            cli.accounts.login(&first.name).unwrap().unwrap()["user"],
            "personal"
        );

        fake.as_user_with_email("personal", "changed@example.com");
        let error = cli.add(None, false).unwrap_err().to_string();
        assert!(
            error.contains("already saved as `shared@example.com`"),
            "{error}"
        );
        assert_eq!(cli.accounts.require(&first.name).unwrap(), first);
        assert_eq!(names(&cli).len(), 3);
        let error = cli
            .add(Some("SHARED@example.com"), false)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("already has an account called `shared@example.com`"),
            "{error}"
        );
    }

    #[test]
    fn a_new_email_never_renames_a_saved_account() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new();
        let cli = cli(&fake, &dir);
        fake.as_user("ana");
        cli.login(&LoginOptions::default()).unwrap();
        fake.as_user_with_email("ana", "ana@example.com");
        cli.login(&LoginOptions {
            replace: true,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(names(&cli), ["main"]);
        assert_eq!(
            cli.accounts.login("main").unwrap().unwrap()["email"],
            "ana@example.com"
        );
    }

    #[test]
    fn an_identity_that_looks_like_email_is_not_a_default_name() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new();
        let cli = cli(&fake, &dir);
        fake.as_user("id@example.com");
        cli.add(None, false).unwrap();
        fake.as_user_with_email("other", "unsafe;name@example.com");
        cli.add(None, false).unwrap();
        assert_eq!(names(&cli), ["main", "account-2"]);
    }

    #[test]
    fn the_first_sign_in_is_main_and_add_keeps_it_in_use() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new();
        let cli = cli(&fake, &dir);

        fake.as_user("ana");
        cli.login(&LoginOptions::default()).unwrap();
        fake.as_user("bo");
        cli.add(Some("Work"), false).unwrap();

        assert_eq!(names(&cli), ["main", "work"]);
        assert_eq!(cli.accounts.current().unwrap().unwrap().name, "main");
        assert_eq!(cli.summary().as_deref(), Some("2 accounts, using main"));
    }

    #[test]
    fn a_duplicate_sign_in_requires_explicit_replacement_without_a_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new();
        let cli = cli(&fake, &dir);
        fake.as_user("ana");
        cli.add(None, false).unwrap();
        fake.as_user("bo");
        cli.add(Some("work"), false).unwrap();

        fake.as_user("bo");
        let error = cli.add(Some("another"), false).unwrap_err().to_string();
        assert!(
            error.contains("already saved as `work`") && error.contains("--replace"),
            "{error}"
        );
        assert_eq!(names(&cli), ["main", "work"]);
    }

    #[test]
    fn replacing_checks_it_is_the_account_you_meant() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new();
        let cli = cli(&fake, &dir);
        fake.as_user("ana");
        cli.add(None, false).unwrap();
        fake.as_user("bo");
        cli.add(Some("work"), false).unwrap();

        // Signing in as ana to replace `work`: refused, and `work` is unchanged.
        fake.as_user("ana");
        let options = LoginOptions {
            replace: true,
            account: Some("work".into()),
            ..Default::default()
        };
        let error = cli.login(&options).unwrap_err().to_string();
        assert!(error.contains("`main`, not `work`"), "{error}");
        assert_eq!(cli.accounts.login("work").unwrap().unwrap()["user"], "bo");

        // A new person may replace an account: that is what replace is for.
        fake.as_user("cy");
        cli.login(&options).unwrap();
        assert_eq!(cli.accounts.login("work").unwrap().unwrap()["user"], "cy");
    }

    #[test]
    fn without_a_terminal_login_never_guesses_between_add_and_replace() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new();
        let cli = cli(&fake, &dir);
        fake.as_user("ana");
        cli.login(&LoginOptions::default()).unwrap();

        let error = cli.login(&LoginOptions::default()).unwrap_err().to_string();
        assert!(
            error.contains("auth add codex") && error.contains("--replace"),
            "{error}"
        );
        assert_eq!(names(&cli), ["main"]);
    }

    #[test]
    fn a_taken_or_bad_name_is_refused_before_any_sign_in() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new();
        let cli = cli(&fake, &dir);
        fake.as_user("ana");
        cli.add(Some("work"), false).unwrap();

        let taken = cli.add(Some("WORK"), false).unwrap_err().to_string();
        assert!(
            taken.contains("already has an account called `work`"),
            "{taken}"
        );
        assert!(cli.add(Some("a b"), false).is_err());
    }

    #[test]
    fn use_and_logout_name_the_accounts_when_they_cannot_ask() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new();
        let cli = cli(&fake, &dir);
        assert!(
            cli.use_account(Some("x"))
                .unwrap_err()
                .to_string()
                .contains("not signed in")
        );
        for (user, name) in [("ana", "home"), ("bo", "work")] {
            fake.as_user(user);
            cli.add(Some(name), false).unwrap();
        }

        let error = cli.use_account(None).unwrap_err().to_string();
        assert!(error.contains("home, work"), "{error}");
        cli.use_account(Some("work")).unwrap();
        assert_eq!(cli.accounts.current().unwrap().unwrap().name, "work");
        let error = cli.use_account(Some("nope")).unwrap_err().to_string();
        assert!(
            error.contains("`nope`") && error.contains("home, work"),
            "{error}"
        );

        let error = cli.logout(None, false).unwrap_err().to_string();
        assert!(
            error.contains("--all") && error.contains("home, work"),
            "{error}"
        );
        cli.logout(Some("work"), false).unwrap();
        assert_eq!(names(&cli), ["home"]);
        assert_eq!(cli.accounts.current().unwrap().unwrap().name, "home");
        // One account left: no name needed.
        cli.logout(None, false).unwrap();
        assert!(names(&cli).is_empty());
        cli.logout(None, false).unwrap();
        assert_eq!(cli.status().unwrap_err().to_string(), "Not authenticated");
    }

    #[test]
    fn logout_all_signs_out_every_account() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new();
        let cli = cli(&fake, &dir);
        for (user, name) in [("ana", "home"), ("bo", "work")] {
            fake.as_user(user);
            cli.add(Some(name), false).unwrap();
        }
        cli.logout(None, true).unwrap();
        assert!(names(&cli).is_empty());
    }
}
