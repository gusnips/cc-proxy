//! Account actions reuse the same stores and sign-in flow as the CLI.

use anyhow::{Context, Result, bail};

use crate::{
    accounts::Accounts, auth_cli, config, config_keys, registry::Registry, route, start_flow, ui,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Action {
    UseProvider(String),
    UseAccount {
        provider: String,
        id: String,
    },
    Rotate(bool),
    Fallback(Vec<String>),
    SignIn {
        provider: String,
        account: Option<String>,
        add: bool,
    },
}

impl Action {
    pub fn needs_terminal(&self) -> bool {
        matches!(self, Self::UseProvider(_) | Self::SignIn { .. })
    }

    pub fn run(&self, base_url: &str) -> Result<String> {
        match self {
            Self::UseProvider(provider) => {
                route::preflight()?;
                match start_flow::pick(Some(provider))? {
                    start_flow::Pick::Provider(provider) => route::activate(provider, base_url)?,
                    start_flow::Pick::Claude => route::deactivate()?,
                }
                Ok(format!(
                    "Saved. Claude Code uses {}. Open windows follow within a few seconds.",
                    ui::provider_name(provider)
                ))
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
                let account = accounts
                    .list()?
                    .into_iter()
                    .find(|account| &account.id == id)
                    .context(
                        "This saved login was removed or replaced. Press Esc, then r to refresh.",
                    )?;
                accounts.set_current(&account.name)?;
                Ok(format!(
                    "Saved. New {} requests use {}. Requests already running keep their account.",
                    ui::provider_name(provider),
                    account.name
                ))
            }
            Self::Rotate(on) => {
                config_keys::write_value("auth.autoRotate", (*on).into())?;
                Ok(format!(
                    "Saved. Auto-rotate is {} for new requests.",
                    if *on { "on" } else { "off" }
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
                        Some(id) => Some(Accounts::open(provider, support)?.list()?.into_iter()
                            .find(|account| &account.id == id)
                            .context("This saved login was removed or replaced. Press Esc, then r to refresh.")?.name),
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
