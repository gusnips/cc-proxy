//! Account actions reuse the same stores and sign-in flow as the CLI.

use anyhow::{Context, Result};

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
    DisableRouting,
    /// Sends Claude Code through the proxy again, with the pool as it is.
    EnableRouting,
    /// Holds new generations until resumed, or lets held ones go.
    SetPauseRequests(bool),
    /// Holds streamed Messages requests when every enabled account waits on a confirmed reset.
    SetWaitForReset(bool),
    /// Saves the pool in its new order after moving a row.
    SavePool {
        provider: String,
        account: Option<String>,
        at: usize,
        pool: Vec<config::PoolEntry>,
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
    /// A non-selectable section title in the action list.
    SectionHeader,
    /// Opens the in-panel model picker for `provider`.
    ChooseModels {
        provider: String,
    },
    /// Sets which model answers `tier` on `provider`.
    SetTierModel {
        provider: String,
        tier: &'static str,
        model: String,
    },
    /// Clears custom tier choices on `provider` so cc-proxy picks automatically.
    ResetTierModels {
        provider: String,
    },
    /// Fetches the live model catalog for `provider`.
    RefreshModels {
        provider: String,
    },
}

impl Action {
    /// Whether the action may ask a question or open a browser. Only these hand
    /// the screen over; the rest run behind the monitor and answer on its footer.
    pub fn needs_terminal(&self) -> bool {
        matches!(self, Self::StartProxy | Self::SignIn { .. })
    }

    pub fn run(&self) -> Result<String> {
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
            Self::EnableRouting => {
                route::preflight()?;
                route::resume(&daemon::client_url(&config::bind_address(), config::port()))?;
                Ok("Saved. Claude Code goes through the proxy again. Open windows follow within a few seconds.".into())
            }
            Self::DisableRouting => {
                route::preflight()?;
                route::deactivate()?;
                Ok("Saved. Claude Code uses its original settings and login, without the proxy. The proxy keeps running.".into())
            }
            Self::SetPauseRequests(paused) => {
                crate::config_keys::write_value(
                    "auth.pauseRequests",
                    serde_json::Value::Bool(*paused),
                )?;
                Ok(if *paused {
                    "Saved. New requests pause before they start. Running replies finish. Press p to resume. This stays on until you resume.".into()
                } else {
                    "Saved. Resumed. Held and new requests run now.".into()
                })
            }
            Self::SetWaitForReset(wait) => {
                crate::config_keys::write_value(
                    "auth.waitForReset",
                    serde_json::Value::Bool(*wait),
                )?;
                Ok(if *wait {
                    if config::auth_auto_rotate() {
                        "Saved. Auto-wait for reset is on. When every enabled account waits on a confirmed reset, streamed Messages requests hold until the earliest reset.".into()
                    } else {
                        "Saved. Auto-wait for reset is on. Turn on Auto-rotate accounts (3 Settings) too so streamed Messages requests wait when every enabled account hits a confirmed reset.".into()
                    }
                } else {
                    "Saved. Auto-wait for reset is off. When every enabled account is waiting, requests stop with 429 at once.".into()
                })
            }
            Self::SavePool {
                provider,
                account,
                at,
                pool,
            } => {
                pool::save(pool)?;
                route::refresh_model_labels();
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
                route::refresh_model_labels();
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
                // The monitor already asked: removing is a second, explicit choice.
                let accounts = open_accounts(provider)?;
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
                let accounts = open_accounts(provider)?;
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
                    "Saved the {} login. Enable its row to let new chats use it.",
                    ui::provider_name(provider)
                ))
            }
            Self::SectionHeader | Self::ChooseModels { .. } => Ok(String::new()),
            Self::SetTierModel {
                provider,
                tier,
                model,
            } => {
                let registry = Registry::new();
                crate::models_map::set(&registry, provider, tier, model)?;
                if config::route().target() == Some(provider.as_str()) {
                    route::refresh_model_labels();
                }
                Ok(format!(
                    "Saved. {} `{tier}` uses `{model}`.",
                    ui::provider_name(provider)
                ))
            }
            Self::ResetTierModels { provider } => {
                let path = format!("route.tiers.{provider}");
                crate::config_keys::remove_values([path.as_str()])?;
                if config::route().target() == Some(provider.as_str()) {
                    route::refresh_model_labels();
                }
                Ok(format!(
                    "Saved. {} uses automatic model picks again.",
                    ui::provider_name(provider)
                ))
            }
            Self::RefreshModels { provider } => {
                let name = ui::provider_name(provider);
                let count = crate::models_map::refresh_provider(provider)?
                    .map_or(0, |ids| ids.len());
                if config::route().target() == Some(provider.as_str()) {
                    route::refresh_model_labels();
                }
                Ok(format!("Saved. {name} lists {count} models now."))
            }
        }
    }
}

fn open_accounts(provider: &str) -> Result<Accounts> {
    let direct = Accounts::new(provider);
    if direct.has_index_file() {
        return Ok(direct);
    }
    let registry = Registry::new();
    let handler = registry
        .provider(provider)
        .context("This provider was removed. Press r to refresh.")?;
    let support = handler
        .cli()
        .accounts()
        .context("This provider does not support saved accounts.")?;
    Accounts::open(provider, support)
}

/// What a pool row is called in a message: `Codex / work`, or the provider
/// alone when it holds one credential.
fn row_label(provider: &str, account: Option<&str>) -> Result<String> {
    let provider_name = ui::provider_name(provider);
    let Some(id) = account else {
        return Ok(provider_name.to_string());
    };
    let name = saved_account(&open_accounts(provider)?, id)?.name;
    Ok(format!("{provider_name} / {name}"))
}

fn saved_account(accounts: &Accounts, id: &str) -> Result<Account> {
    accounts
        .list()?
        .into_iter()
        .find(|account| account.id == id)
        .context("This saved login was removed or replaced. Press Esc, then r to refresh.")
}
