//! `cc-proxy models choose` and `cc-proxy models map`: see or change which
//! model answers each of Claude Code's names (`fable`, `opus`, `sonnet`,
//! `haiku`) on a provider.

use std::collections::HashSet;

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
        (Some(tier), None, false) => choose(&registry, provider, &[tier])?,
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

pub fn run_choose(provider: Option<&str>) -> Result<()> {
    let registry = Registry::new();
    let provider = provider_arg(&registry, provider)?;
    choose(&registry, provider, &TIERS)?;
    if config::route().target() == Some(provider) {
        crate::route::refresh_model_labels();
    }
    show(&registry, provider);
    Ok(())
}

/// Asks which `provider` model each of `tiers` uses, with search, because a
/// catalog can hold thousands. Enter keeps what a name has now, or leaves it
/// without a model. Each answer is saved as it comes.
pub fn choose(registry: &Registry, provider: &str, tiers: &[&'static str]) -> Result<()> {
    let name = ui::provider_name(provider);
    let now = tiers::mapping(registry, &config::route(), provider);
    for &tier in tiers {
        let models = choices(registry, provider, tier);
        if models.is_empty() {
            bail!(
                "{name} has no models listed. Run `cc-proxy models refresh {provider}`, then try again."
            );
        }
        let current = match now.iter().find(|mapping| mapping.tier == tier) {
            Some(tiers::Mapping {
                model: Some(model),
                basis,
                ..
            }) => format!("{model} ({})", basis_label(*basis)),
            _ => "no model yet".to_string(),
        };
        let question = format!("Which {name} model should `{tier}` use? Now: {current}.");
        if let Some(index) = crate::prompt::search(&question, &models)? {
            set(registry, provider, tier, &models[index])?;
        }
    }
    Ok(())
}

/// Every model `provider` offers, the best fits for `tier` first.
fn choices(registry: &Registry, provider: &str, tier: &str) -> Vec<String> {
    let mut models = tiers::ranked(registry, provider, tier);
    let ranked: HashSet<String> = models.iter().cloned().collect();
    models.extend(
        registry
            .catalog_for(provider)
            .into_iter()
            .filter(|model| !ranked.contains(model)),
    );
    once_each(models)
}

/// `models` with a model listed both bare and as `prefix/model` kept once, in
/// the prefixed form: it reaches this provider even if another one starts
/// serving the bare name.
fn once_each(models: Vec<String>) -> Vec<String> {
    let listed: HashSet<&str> = models.iter().map(String::as_str).collect();
    let prefixes: HashSet<&str> = models
        .iter()
        .filter_map(|model| model.split_once('/').map(|(prefix, _)| prefix))
        .collect();
    let prefixed_too = |model: &str| {
        !model.contains('/')
            && prefixes
                .iter()
                .any(|prefix| listed.contains(format!("{prefix}/{model}").as_str()))
    };
    let keep: Vec<bool> = models.iter().map(|model| !prefixed_too(model)).collect();
    models
        .into_iter()
        .zip(keep)
        .filter_map(|(model, keep)| keep.then_some(model))
        .collect()
}

fn basis_label(basis: Basis) -> &'static str {
    match basis {
        Basis::Chosen => "your choice",
        Basis::Sure => "picked by cc-proxy",
        Basis::Guess => "a guess, not confirmed",
    }
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
        match &mapping.model {
            Some(model) => println!(
                "  {:<7} {model} ({})",
                mapping.tier,
                basis_label(mapping.basis)
            ),
            None => println!("  {:<7} no model yet", mapping.tier),
        }
    }
    println!("Change them with `cc-proxy models choose {provider}`.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_model_listed_bare_and_prefixed_is_offered_once_prefixed() {
        let models = [
            "kimi-k2.5",
            "opencode-go/kimi-k2.5",
            "glm-5",
            "opencode-go/kimi-k3",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(
            once_each(models),
            ["opencode-go/kimi-k2.5", "glm-5", "opencode-go/kimi-k3"]
        );
    }

    #[test]
    fn names_are_matched_in_any_case() {
        assert_eq!(tier_arg(" Opus ").unwrap(), "opus");
        let error = tier_arg("gemini").unwrap_err().to_string();
        assert!(error.contains("fable, opus, sonnet, haiku"), "{error}");
    }
}
