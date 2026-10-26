//! Account actions reuse the same stores and sign-in flow as the CLI.

use anyhow::{Context, Result, bail};

use crate::{
    accounts::{Account, Accounts},
    auth_cli, config, daemon,
    registry::Registry,
    route, start_flow, ui,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Action {
    /// Starts the proxy from a monitor opened while it was off.
    StartProxy,
    UseProvider(String),
    UseAccount {
        provider: String,
        id: String,
    },
    RemoveAccount {
        provider: String,
        id: String,
    },
    Fallback(Vec<String>),
    SignIn {
        provider: String,
        account: Option<String>,
        add: bool,
    },
}

impl Action {
    /// Whether the action may ask a question or open a browser. Only these hand
    /// the screen over; the rest run behind the monitor and answer on its footer.
    pub fn needs_terminal(&self) -> bool {
        match self {
            Self::StartProxy | Self::SignIn { .. } => true,
            Self::UseProvider(provider) => start_flow::will_ask(provider),
            Self::UseAccount { .. } | Self::RemoveAccount { .. } | Self::Fallback(_) => false,
        }
    }

    pub fn run(&self, base_url: &str) -> Result<String> {
        match self {
            Self::StartProxy => {
                route::preflight()?;
                // The saved provider needs no question. Without one, `start` asks.
                let pick = start_flow::pick(config::route().provider.as_deref())?;
                let label = format!("starting cc-proxy on port {}", config::port());
                let info = match ui::waiting(&label, || daemon::serve_background(None))? {
                    daemon::ServeOutcome::AlreadyRunning(info)
                    | daemon::ServeOutcome::Started(info) => info,
                };
                match pick {
                    start_flow::Pick::Provider(provider) => {
                        route::activate(provider, &info.listen_url(), route::Chain::Keep)?;
                        Ok(format!(
                            "The proxy is on. Claude Code uses {} through it. Open windows follow within a few seconds.",
                            ui::provider_name(provider)
                        ))
                    }
                    start_flow::Pick::Direct => Ok(
                        "The proxy is on. Claude Code still uses its original settings and login."
                            .into(),
                    ),
                }
            }
            Self::UseProvider(provider) => {
                route::preflight()?;
                match start_flow::pick(Some(provider))? {
                    start_flow::Pick::Provider(provider) => {
                        route::activate(provider, base_url, route::Chain::Keep)?;
                        Ok(format!(
                            "Saved. Claude Code uses {} through the proxy. Open windows follow within a few seconds.",
                            ui::provider_name(provider)
                        ))
                    }
                    start_flow::Pick::Direct => {
                        route::deactivate()?;
                        Ok("Saved. Claude Code uses its original settings and login, without the proxy. The proxy keeps running.".into())
                    }
                }
            }
            Self::UseAccount { provider, id } => {
                let registry = Registry::new();
                let handler = registry
                    .provider(provider)
                    .context("This provider was removed. Press r to refresh.")?;
                let support = handler
                    .cli()
                    .accounts()
                    .context("This provider does not support saved accounts.")?;
                if let Some(var) = support.pinned_credentials() {
                    bail!(
                        "${var} supplies the login. Unset it in the proxy service before choosing a saved account."
                    );
                }
                let accounts = Accounts::open(provider, support)?;
                let account = saved_account(&accounts, id)?;
                crate::rotation::switch_account(
                    provider,
                    &accounts,
                    &crate::rotation::Cooldowns::new(),
                    &account,
                    crate::rotation::Switch::Chosen(support),
                )?;
                Ok(format!(
                    "Saved. New {} requests use {}. Requests already running keep their account.",
                    ui::provider_name(provider),
                    account.name
                ))
            }
            Self::RemoveAccount { provider, id } => {
                let registry = Registry::new();
                let handler = registry
                    .provider(provider)
                    .context("This provider was removed. Press r to refresh.")?;
                let support = handler
                    .cli()
                    .accounts()
                    .context("This provider does not support saved accounts.")?;
                // The monitor already asked: removing is a second, explicit choice.
                let accounts = Accounts::open(provider, support)?;
                let account = saved_account(&accounts, id)?;
                let next = accounts.remove(&account)?;
                let _ = crate::rotation::Cooldowns::new().clear_account(provider, id);
                let next = next.map_or_else(String::new, |next| {
                    format!(" New requests use {}.", next.name)
                });
                Ok(format!(
                    "Removed {} account {}.{next} Sign in to add it back.",
                    ui::provider_name(provider),
                    account.name
                ))
            }
            Self::Fallback(providers) => {
                route::set_fallback(providers)?;
                Ok(if config::auth_auto_rotate() {
                    "Saved the fallback order. New requests use it when a provider is spent.".into()
                } else {
                    "Saved the fallback order. Turn on auto-rotate to try other saved accounts too."
                        .into()
                })
            }
            Self::SignIn {
                provider,
                account,
                add,
            } => {
                let registry = Registry::new();
                let handler = registry
                    .provider(provider)
                    .context("This provider was removed. Press r to refresh.")?;
                let cli = handler.cli();
                if let Some(support) = cli.accounts() {
                    let account = match account {
                        Some(id) => {
                            Some(saved_account(&Accounts::open(provider, support)?, id)?.name)
                        }
                        None => None,
                    };
                    let accounts = auth_cli::AccountCli::open(provider, support)?;
                    if *add {
                        accounts.add(None, false)?;
                    } else {
                        accounts.login(&auth_cli::LoginOptions {
                            device: false,
                            replace: account.is_some(),
                            account: account.clone(),
                        })?;
                    }
                } else {
                    cli.login()?;
                }
                Ok(format!(
                    "Saved the {} login. Choose Use provider to send Claude Code there.",
                    ui::provider_name(provider)
                ))
            }
        }
    }
}

fn saved_account(accounts: &Accounts, id: &str) -> Result<Account> {
    accounts
        .list()?
        .into_iter()
        .find(|account| account.id == id)
        .context("This saved login was removed or replaced. Press Esc, then r to refresh.")
}
