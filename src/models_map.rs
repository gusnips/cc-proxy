//! `cc-proxy models map`: see or change which model answers each of Claude
//! Code's names (`fable`, `opus`, `sonnet`, `haiku`) on a provider.

use anyhow::{Result, bail};

use crate::registry::Registry;
use crate::route::PROVIDERS;
use crate::tiers::{self, Basis, TIERS};
use crate::{config, config_keys, ui};

pub fn run(
    tier: Option<&str>,
    model: Option<&str>,
    reset: bool,
    provider: Option<&str>,
) -> Result<()> {
    let provider = provider_arg(provider)?;
    let tier = tier.map(tier_arg).transpose()?;
    let registry = Registry::new();
    match (tier, model, reset) {
        (Some(tier), Some(model), false) => set(&registry, provider, tier, model)?,
        (tier, None, true) => reset_choice(provider, tier)?,
        (None, None, false) => {}
        _ => bail!(
            "Use `cc-proxy models map <name> <model>` to choose, or `cc-proxy models map [name] \
             --reset` to undo a choice."
        ),
    }
    if (model.is_some() || reset) && config::route().target() == Some(provider) {
        crate::route::refresh_model_labels();
    }
    show(&registry, provider);
    Ok(())
}

fn provider_arg(asked: Option<&str>) -> Result<&'static str> {
    let remembered = config::route().provider;
    let asked = asked.map(str::to_ascii_lowercase).or(remembered);
    let Some(asked) = asked else {
        bail!("Name a provider with `--provider`, or run `cc-proxy start` to choose one first.");
    };
    match PROVIDERS.iter().find(|provider| **provider == asked) {
        Some(provider) => Ok(provider),
        None => bail!(
            "`{asked}` isn't a provider cc-proxy can route to. Use one of: {}.",
            PROVIDERS.join(", ")
        ),
    }
}

fn tier_arg(asked: &str) -> Result<&'static str> {
    let asked = asked.trim().to_ascii_lowercase();
    match TIERS.iter().find(|tier| **tier == asked) {
        Some(tier) => Ok(tier),
        None => bail!(
            "`{asked}` isn't one of Claude Code's names. Use one of: {}.",
            TIERS.join(", ")
        ),
    }
}

fn set(registry: &Registry, provider: &str, tier: &'static str, model: &str) -> Result<()> {
    if registry.provider_for_model(model).is_none() {
        bail!(
            "cc-proxy doesn't serve a model called `{model}`. Run `cc-proxy models` to see the \
             ones it does."
        );
    }
    config_keys::write_value(&tiers::choice_key(provider, tier), model.into())?;
    Ok(())
}

fn reset_choice(provider: &str, tier: Option<&str>) -> Result<()> {
    let path = match tier {
        Some(tier) => tiers::choice_key(provider, tier),
        None => format!("route.tiers.{provider}"),
    };
    config_keys::remove_values([path.as_str()])
}

fn show(registry: &Registry, provider: &str) {
    let mappings = tiers::mapping(registry, &config::route(), provider);
    println!("Claude names on {}:", ui::provider_name(provider));
    for mapping in &mappings {
        let model = mapping.model.as_deref().unwrap_or("no model");
        let basis = match mapping.basis {
            Basis::Chosen => "your choice",
            Basis::Sure => "picked by cc-proxy",
            Basis::Guess => "a guess, not confirmed",
        };
        println!("  {:<7} {model} ({basis})", mapping.tier);
    }
    println!("Change one with `cc-proxy models map <name> <model>`.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_matched_in_any_case() {
        assert_eq!(tier_arg(" Opus ").unwrap(), "opus");
        let error = tier_arg("gemini").unwrap_err().to_string();
        assert!(error.contains("fable, opus, sonnet, haiku"), "{error}");
    }
}
