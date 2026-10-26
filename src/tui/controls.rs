//! Account actions reuse the same stores and sign-in flow as the CLI.

use anyhow::{Context, Result, bail};

use crate::{
    accounts::{Account, Accounts},
    auth_cli, config, daemon, pool,
    registry::Registry,
    route, start_flow, ui,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Action {
    /// Starts the proxy from a monitor opened while it was off.
    StartProxy,
    UseProvider(String),
    /// Puts a saved account first in the pool and turns it on, so new chats use it.
    UseAccount {
        provider: String,
        id: String,
    },
    /// Moves a row up (negative) or down (positive) in the pool.
    MoveRow {
        provider: String,
        account: Option<String>,
        offset: isize,
    },
    /// Turns a row on or off. It keeps its place.
    SetEnabled {
        provider: String,
        account: Option<String>,
        enabled: bool,
    },
    RemoveAccount {
        provider: String,
        id: String,
    },
    /// Gives a saved account a new name. Its sign-in and id stay as they are.
    RenameAccount {
        provider: String,
        id: String,
        name: String,
    },
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
            Self::UseAccount { .. }
            | Self::MoveRow { .. }
            | Self::SetEnabled { .. }
            | Self::RemoveAccount { .. }
            | Self::RenameAccount { .. } => false,
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
                pool::use_now(provider, id)?;
                Ok(format!(
                    "Saved. {} / {} is first in the list now. New chats use it. Chats already running keep their account.",
                    ui::provider_name(provider),
                    account.name
                ))
            }
            Self::MoveRow {
                provider,
                account,
                offset,
            } => {
                let at = pool::move_by(provider, account.as_deref(), *offset)?;
                Ok(format!(
                    "Saved. {} is number {} in the list. New chats try the rows from the top.",
                    row_label(provider, account.as_deref())?,
                    at + 1
                ))
            }
            Self::SetEnabled {
                provider,
                account,
                enabled,
            } => {
                pool::set_enabled(provider, account.as_deref(), *enabled)?;
                let label = row_label(provider, account.as_deref())?;
                Ok(if *enabled {
                    format!("Saved. {label} is on. New chats can use it.")
                } else {
                    format!(
                        "Saved. {label} is off. New chats skip it. Chats already on it move to the next row when they need to."
                    )
                })
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
                accounts.remove(&account)?;
                let _ = crate::rotation::Cooldowns::new().clear_account(provider, id);
                Ok(format!(
                    "Removed {} account {}. Sign in to add it back.",
                    ui::provider_name(provider),
                    account.name
                ))
            }
            Self::RenameAccount { provider, id, name } => {
                let registry = Registry::new();
                let handler = registry
                    .provider(provider)
                    .context("This provider was removed. Press r to refresh.")?;
                let support = handler
                    .cli()
                    .accounts()
                    .context("This provider does not support saved accounts.")?;
                let accounts = Accounts::open(provider, support)?;
                let account = saved_account(&accounts, id)?;
                let renamed = accounts.rename(&account.name, name)?;
                Ok(format!(
                    "Renamed {} account {} to {}. Its sign-in stays the same.",
                    ui::provider_name(provider),
                    account.name,
                    renamed.name
                ))
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

/// What a pool row is called in a message: `Codex / work`, or the provider
/// alone when it holds one credential.
fn row_label(provider: &str, account: Option<&str>) -> Result<String> {
    let provider_name = ui::provider_name(provider);
    let Some(id) = account else {
        return Ok(provider_name.to_string());
    };
    let registry = Registry::new();
    let handler = registry
        .provider(provider)
        .context("This provider was removed. Press r to refresh.")?;
    let support = handler
        .cli()
        .accounts()
        .context("This provider does not support saved accounts.")?;
    let name = saved_account(&Accounts::open(provider, support)?, id)?.name;
    Ok(format!("{provider_name} / {name}"))
}

fn saved_account(accounts: &Accounts, id: &str) -> Result<Account> {
    accounts
        .list()?
        .into_iter()
        .find(|account| account.id == id)
        .context("This saved login was removed or replaced. Press Esc, then r to refresh.")
}
