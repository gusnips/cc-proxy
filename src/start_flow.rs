//! The questions behind `cc-proxy start` and `cc-proxy use`: where should
//! Claude Code go, and is that provider signed in. A line-based flow: each
//! question is one prompt, and piped input answers it the same way a keyboard
//! does.

use anyhow::{Context, Result, bail};

use crate::provider::AuthState;
use crate::registry::Registry;
use crate::route::{CLAUDE, PROVIDERS};
use crate::ui::{self, Mood};
use crate::{config, prompt};

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
    match asked {
        Some(asked) => pick_named(&registry, asked),
        None => pick_for_start(&registry),
    }
}

fn pick_named(registry: &Registry, asked: &str) -> Result<Pick> {
    let asked = asked.trim().to_ascii_lowercase();
    if asked == CLAUDE {
        return Ok(Pick::Claude);
    }
    let Some(provider) = PROVIDERS.iter().find(|provider| **provider == asked) else {
        bail!(
            "cc-proxy can't send Claude Code to `{asked}`. Use one of: {}, or `{CLAUDE}` for your \
             Claude subscription.",
            PROVIDERS.join(", ")
        );
    };
    if is_signed_in(registry, provider) {
        return Ok(Pick::Provider(provider));
    }
    let name = ui::provider_name(provider);
    if !prompt::confirm(&format!("{name} isn't signed in. Sign in now?"), false)? {
        bail!("{name} isn't signed in. Run `cc-proxy {provider} auth login`, then try again.");
    }
    sign_in(registry, provider)?;
    Ok(Pick::Provider(provider))
}

fn pick_for_start(registry: &Registry) -> Result<Pick> {
    let signed_in: Vec<&'static str> = PROVIDERS
        .into_iter()
        .filter(|provider| is_signed_in(registry, provider))
        .collect();
    let remembered = config::route().provider;
    let claude_option = "Claude Code subscription, no routing".to_string();

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
            let mut options: Vec<String> = PROVIDERS
                .iter()
                .map(|provider| ui::provider_name(provider).to_string())
                .collect();
            options.push(claude_option);
            let answer = prompt::choose(
                "You aren't signed in to any provider yet. Which one do you want to sign in to?",
                &options,
                0,
            )?;
            let Some(provider) = PROVIDERS.get(answer) else {
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
    let cli = registry
        .provider(provider)
        .with_context(|| format!("{name} isn't a known provider"))?
        .cli();
    cli.login()?;
    if cli.auth_state() == AuthState::Missing {
        bail!(
            "{name} still isn't signed in. Run `cc-proxy {provider} auth login`, then try again."
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
        let error = pick_named(&registry, "grok").unwrap_err().to_string();
        assert!(error.contains("codex, kimi"), "{error}");
        assert!(error.contains("`claude`"), "{error}");
    }
}
