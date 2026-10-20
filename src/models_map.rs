//! `cc-proxy models map`: see or change which model answers each of Claude
//! Code's names (`fable`, `opus`, `sonnet`, `haiku`) on a provider.

use anyhow::{Result, bail};

use crate::auth::AuthStorage;
use crate::registry::Registry;
use crate::route::routable_providers;
use crate::tiers::{self, Basis, TIERS};
use crate::{config, config_keys, ui};

pub fn run(
    tier: Option<&str>,
    model: Option<&str>,
    reset: bool,
    provider: Option<&str>,
) -> Result<()> {
    let registry = Registry::new();
    let provider = provider_arg(&registry, provider)?;
    let tier = tier.map(tier_arg).transpose()?;
    match (tier, model, reset) {
        (Some(tier), Some(model), false) => set(&registry, provider, tier, model)?,
        (Some(tier), None, false) => {
            let chosen = pick_model_interactively(&registry, provider, tier)?;
            set(&registry, provider, tier, &chosen)?;
        }
        (tier, None, true) => reset_choice(provider, tier)?,
        (None, None, false) => {}
        _ => bail!(
            "Use `cc-proxy models map <name> [model]` to choose, or `cc-proxy models map [name] \
             --reset` to undo a choice."
        ),
    }
    if (tier.is_some() || model.is_some() || reset) && config::route().target() == Some(provider) {
        crate::route::refresh_model_labels();
    }
    show(&registry, provider);
    Ok(())
}

pub fn run_interactive_set(provider: Option<&str>) -> Result<()> {
    let registry = Registry::new();
    let provider = provider_arg(&registry, provider)?;
    for tier in TIERS {
        let chosen = pick_model_interactively(&registry, provider, tier)?;
        set(&registry, provider, tier, &chosen)?;
    }
    if config::route().target() == Some(provider) {
        crate::route::refresh_model_labels();
    }
    show(&registry, provider);
    Ok(())
}

pub fn run_refresh(provider: Option<&str>) -> Result<()> {
    let registry = Registry::new();
    let provider = provider_arg(&registry, provider)?;
    match provider {
        "copilot" => {
            let Some(auth) = crate::providers::copilot::auth::file_store().load()? else {
                bail!("Copilot is not signed in. Run `cc-proxy auth login copilot` first.");
            };
            let ids = crate::providers::copilot::models::refresh(&auth)?;
            println!("Refreshed {} Copilot models.", ids.len());
        }
        other => {
            let models = registry.supported_models_for(other);
            println!(
                "{} has {} models available: {}",
                ui::provider_name(other),
                models.len(),
                models.join(", ")
            );
        }
    }
    if config::route().target() == Some(provider) {
        crate::route::refresh_model_labels();
    }
    Ok(())
}

fn pick_model_interactively(registry: &Registry, provider: &str, tier: &str) -> Result<String> {
    let mut models = tiers::ranked(registry, provider, tier);
    for m in registry.catalog_for(provider) {
        if !models.contains(&m) {
            models.push(m);
        }
    }
    if models.is_empty() {
        bail!(
            "{} has no models listed. Run `cc-proxy models` to see what cc-proxy knows.",
            ui::provider_name(provider)
        );
    }
    let question = format!(
        "Which {} model should `{tier}` use?",
        ui::provider_name(provider)
    );
    let idx = crate::prompt::choose(&question, &models, 0)?;
    Ok(models[idx].clone())
}

fn provider_arg(registry: &Registry, asked: Option<&str>) -> Result<&'static str> {
    let remembered = config::route().provider;
    let asked = asked.map(str::to_ascii_lowercase).or(remembered);
    let Some(asked) = asked else {
        bail!("Name a provider with `--provider`, or run `cc-proxy start` to choose one first.");
    };
    let providers = routable_providers(registry);
    match providers.iter().find(|provider| **provider == asked) {
        Some(&provider) => Ok(provider),
        None => bail!(
            "`{asked}` isn't a provider cc-proxy can route to. Use one of: {}.",
            providers.join(", ")
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
