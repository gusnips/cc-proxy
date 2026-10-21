//! The questions behind `cc-proxy start` and `cc-proxy use`: where should
//! Claude Code go, and is that provider signed in. A line-based flow: each
//! question is one prompt, and piped input answers it the same way a keyboard
//! does.

use anyhow::{Context, Result, bail};

use crate::provider::AuthState;
use crate::registry::Registry;
use crate::route::{CLAUDE, routable_providers};
use crate::tiers::{self, Basis};
use crate::ui::{self, Mood};
use crate::{config, config_keys, models_map, prompt};

/// Where Claude Code goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    /// Through the proxy, to this provider.
    Provider(&'static str),
    /// Straight to Anthropic, on the user's Claude subscription.
    Claude,
}

/// `asked` is what the user typed after `cc-proxy use`: a provider or `claude`.
/// Without it, `start` works the answer out and asks only when it has to.
pub fn pick(asked: Option<&str>) -> Result<Pick> {
    let registry = Registry::new();
    let pick = match asked {
        Some(asked) => pick_named(&registry, asked)?,
        None => pick_for_start(&registry)?,
    };
    if let Pick::Provider(provider) = pick {
        settle_tiers(&registry, provider)?;
    }
    Ok(pick)
}

/// Asks which model a Claude name should use, but only for the names with no
/// model or only a guess, and only once: the answers are kept. A name with no
/// model (a provider that resells other vendors' models has none until you
/// choose) is asked with search, and Enter skips it. With no answer to read,
/// guesses stay in use this time and nothing is kept.
fn settle_tiers(registry: &Registry, provider: &str) -> Result<()> {
    let open: Vec<_> = tiers::mapping(registry, &config::route(), provider)
        .into_iter()
        .filter(|mapping| mapping.basis == Basis::Guess)
        .collect();
    if open.is_empty() {
        return Ok(());
    }
    let open_tiers: Vec<&'static str> = open.iter().map(|mapping| mapping.tier).collect();
    let name = ui::provider_name(provider);
    if open.iter().any(|mapping| mapping.model.is_none()) {
        ui::print_note(
            Mood::Awake,
            &[format!(
                "Choose the {name} model each Claude name uses. Press Enter to skip one."
            )],
        );
        return models_map::choose(registry, provider, &open_tiers);
    }

    let mut lines = vec![format!(
        "cc-proxy can't tell which {name} model each Claude name should use."
    )];
    lines.extend(open.iter().filter_map(|mapping| {
        Some(format!(
            "{} would use {}.",
            mapping.tier,
            mapping.model.as_ref()?
        ))
    }));
    ui::print_note(Mood::Unsure, &lines);
    match prompt::ask_yes_no("Use these?", true)? {
        None => {
            ui::print_note(
                Mood::Unsure,
                &[format!(
                    "No answer, so cc-proxy uses these guesses for now. \
                     `cc-proxy models choose {provider}` picks them for good."
                )],
            );
            Ok(())
        }
        Some(false) => models_map::choose(registry, provider, &open_tiers),
        Some(true) => {
            let paths: Vec<String> = open
                .iter()
                .map(|mapping| tiers::choice_key(provider, mapping.tier))
                .collect();
            let models = open.iter().filter_map(|mapping| mapping.model.clone());
            config_keys::write_values(
                paths.iter().map(String::as_str).zip(models.map(Into::into)),
            )?;
            Ok(())
        }
    }
}

fn pick_named(registry: &Registry, asked: &str) -> Result<Pick> {
    let asked = asked.trim().to_ascii_lowercase();
    if asked == CLAUDE {
        return Ok(Pick::Claude);
    }
    let providers = routable_providers(registry);
    let Some(&provider) = providers.iter().find(|provider| **provider == asked) else {
        bail!(
            "cc-proxy can't send Claude Code to `{asked}`. Use one of: {}, or `{CLAUDE}` for your \
             Claude subscription.",
            providers.join(", ")
        );
    };
    if is_signed_in(registry, provider) {
        return Ok(Pick::Provider(provider));
    }
    let name = ui::provider_name(provider);
    if !prompt::confirm(&format!("{name} isn't signed in. Sign in now?"), false)? {
        bail!("{name} isn't signed in. Run `cc-proxy auth login {provider}`, then try again.");
    }
    sign_in(registry, provider)?;
    Ok(Pick::Provider(provider))
}

fn pick_for_start(registry: &Registry) -> Result<Pick> {
    let providers = routable_providers(registry);
    let signed_in: Vec<&'static str> = providers
        .iter()
        .copied()
        .filter(|provider| is_signed_in(registry, provider))
        .collect();
    let remembered = config::route().provider;
    let claude_option = "Claude Code subscription, no routing".to_string();

    if prompt::is_interactive() {
        let mut options: Vec<String> = providers
            .iter()
            .map(|provider| {
                let status = if is_signed_in(registry, provider) {
                    "signed in"
                } else {
                    "not set up"
                };
                format!("{} ({provider}) · {status}", ui::provider_name(provider))
            })
            .collect();
        options.push(claude_option);
        let default = providers
            .iter()
            .position(|provider| Some(*provider) == remembered.as_deref())
            .or_else(|| {
                signed_in
                    .first()
                    .and_then(|first| providers.iter().position(|p| p == first))
            })
            .unwrap_or(0);
        let answer = prompt::choose("Where should Claude Code go?", &options, default)?;
        let Some(&provider) = providers.get(answer) else {
            return Ok(Pick::Claude);
        };
        if !is_signed_in(registry, provider) {
            sign_in(registry, provider)?;
        }
        return Ok(Pick::Provider(provider));
    }

    match signed_in.as_slice() {
        [only] => {
            ui::print_note(
                Mood::Awake,
                &[format!(
                    "{} is the only provider signed in, so Claude Code goes there. \
                     `cc-proxy use <provider>` picks another.",
                    ui::provider_name(only)
                )],
            );
            Ok(Pick::Provider(only))
        }
        [] => {
            let mut options: Vec<String> = providers
                .iter()
                .map(|provider| ui::provider_name(provider).to_string())
                .collect();
            options.push(claude_option);
            let answer = prompt::choose(
                "You aren't signed in to any provider yet. Which one do you want to sign in to?",
                &options,
                0,
            )?;
            let Some(&provider) = providers.get(answer) else {
                return Ok(Pick::Claude);
            };
            sign_in(registry, provider)?;
            Ok(Pick::Provider(provider))
        }
        several => {
            let mut options: Vec<String> = several
                .iter()
                .map(|provider| ui::provider_name(provider).to_string())
                .collect();
            options.push(claude_option);
            let default = several
                .iter()
                .position(|provider| Some(*provider) == remembered.as_deref())
                .unwrap_or(0);
            let answer = prompt::choose("Where should Claude Code go?", &options, default)?;
            Ok(match several.get(answer) {
                Some(provider) => Pick::Provider(provider),
                None => Pick::Claude,
            })
        }
    }
}

fn is_signed_in(registry: &Registry, provider: &str) -> bool {
    registry
        .provider(provider)
        .is_some_and(|handler| handler.cli().auth_state() != AuthState::Missing)
}

fn sign_in(registry: &Registry, provider: &str) -> Result<()> {
    let name = ui::provider_name(provider);
    let handler = registry
        .provider(provider)
        .with_context(|| format!("{name} isn't a known provider"))?;
    let cli = handler.cli();
    cli.login()?;
    if cli.auth_state() == AuthState::Missing {
        bail!(
            "{name} still isn't signed in. Run `cc-proxy auth login {provider}`, then try again."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn use_takes_a_provider_or_claude_in_any_case() {
        let registry = Registry::new();
        assert_eq!(pick_named(&registry, " Claude ").unwrap(), Pick::Claude);
        let error = pick_named(&registry, "unknown").unwrap_err().to_string();
        assert!(error.contains("codex, kimi, cursor, grok"), "{error}");
        assert!(error.contains("`claude`"), "{error}");
    }
}
