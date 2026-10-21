//! `cc-proxy models`: see or change which model answers each of Claude Code's
//! names (`fable`, `opus`, `sonnet`, `haiku`) on a provider, and fetch a
//! provider's live model list.

use std::collections::HashSet;

use anyhow::{Result, bail};

use crate::auth::AuthStorage;
use crate::registry::Registry;
use crate::route::routable_providers;
use crate::tiers::{self, Basis, TIERS};
use crate::{config, config_keys, ui};

/// `cc-proxy models` and `cc-proxy models show`.
pub fn run_show(provider: Option<&str>) -> Result<()> {
    let registry = Registry::new();
    let provider = pick_provider(
        &registry,
        provider,
        "Which provider's models do you want to see?",
        true,
    )?;
    show(&registry, provider);
    Ok(())
}

pub fn run_choose(provider: Option<&str>) -> Result<()> {
    let registry = Registry::new();
    let provider = pick_provider(
        &registry,
        provider,
        "Which provider's models do you want to choose?",
        true,
    )?;
    choose(&registry, provider, &TIERS)?;
    changed(&registry, provider);
    Ok(())
}

pub fn run_set(tier: &str, model: &str, provider: Option<&str>) -> Result<()> {
    let registry = Registry::new();
    let provider = pick_provider(
        &registry,
        provider,
        "Which provider should use this model?",
        false,
    )?;
    set(&registry, provider, tier_arg(tier)?, model)?;
    changed(&registry, provider);
    Ok(())
}

pub fn run_reset(tier: Option<&str>, provider: Option<&str>) -> Result<()> {
    let registry = Registry::new();
    let provider = pick_provider(
        &registry,
        provider,
        "Which provider's model choices do you want to reset?",
        false,
    )?;
    let path = match tier {
        Some(tier) => tiers::choice_key(provider, tier_arg(tier)?),
        None => format!("route.tiers.{provider}"),
    };
    config_keys::remove_values([path.as_str()])?;
    changed(&registry, provider);
    Ok(())
}

/// After a change: Claude Code's labels follow it when Claude Code goes to
/// `provider`, and the names are shown as they stand.
fn changed(registry: &Registry, provider: &str) {
    if config::route().target() == Some(provider) {
        crate::route::refresh_model_labels();
    }
    show(registry, provider);
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

/// Fetches the live model list of GitHub Copilot or a provider you added. The
/// others list their models without asking the provider.
pub fn run_refresh(provider: Option<&str>) -> Result<()> {
    let registry = Registry::new();
    let provider = pick_provider(
        &registry,
        provider,
        "Which provider's model list do you want to refresh?",
        true,
    )?;
    let name = ui::provider_name(provider);
    if provider == "copilot" {
        let Some(auth) = crate::providers::copilot::auth::file_store().load()? else {
            bail!("GitHub Copilot isn't signed in. Run `cc-proxy auth login copilot` first.");
        };
        let ids = crate::providers::copilot::models::refresh(&auth)?;
        println!("{name} lists {} models now.", ids.len());
    } else if config::custom_providers().contains_key(provider) {
        let models = crate::providers::custom::refresh_models(provider)?;
        println!(
            "{name} lists {} models now. They're saved in config.json.",
            models.len()
        );
    } else {
        println!(
            "There's nothing to fetch for {name}. `cc-proxy models list {provider}` shows its models."
        );
        return Ok(());
    }
    if config::route().target() == Some(provider) {
        crate::route::refresh_model_labels();
    }
    Ok(())
}

fn pick_provider(
    registry: &Registry,
    asked: Option<&str>,
    question: &str,
    ask_when_omitted: bool,
) -> Result<&'static str> {
    let providers = routable_providers(registry);
    let remembered = config::route().provider;
    if let Some(asked) = asked {
        let asked = asked.trim().to_ascii_lowercase();
        return match providers.iter().find(|provider| **provider == asked) {
            Some(&provider) => Ok(provider),
            None => bail!(
                "`{asked}` isn't a provider cc-proxy can route to. Use one of: {}.",
                providers.join(", ")
            ),
        };
    }
    if crate::prompt::is_interactive() && (ask_when_omitted || remembered.is_none()) {
        let labels: Vec<String> = providers
            .iter()
            .map(|id| format!("{} ({id})", ui::provider_name(id)))
            .collect();
        let default = providers
            .iter()
            .position(|provider| Some(*provider) == remembered.as_deref())
            .unwrap_or(0);
        let idx = crate::prompt::choose(question, &labels, default)?;
        return Ok(providers[idx]);
    }
    let Some(asked) = remembered else {
        bail!(
            "No provider to use yet. Name one, or run `cc-proxy start` to choose where Claude Code goes."
        );
    };
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
            "cc-proxy doesn't serve a model called `{model}`. `cc-proxy models list {provider}` \
             shows the ones it does."
        );
    }
    config_keys::write_value(&tiers::choice_key(provider, tier), model.into())?;
    Ok(())
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
