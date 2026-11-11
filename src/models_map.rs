//! `cc-proxy models`: see or change which model answers each of Claude Code's
//! names (`fable`, `opus`, `sonnet`, `haiku`) on a provider, and fetch a
//! provider's live model list.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};

use crate::auth::AuthStorage;
use crate::provider::AuthState;
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
pub(crate) fn choices(registry: &Registry, provider: &str, tier: &str) -> Vec<String> {
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

pub(crate) fn basis_label(basis: Basis) -> &'static str {
    match basis {
        Basis::Chosen => "your choice",
        Basis::Sure => "picked by cc-proxy",
        Basis::Guess => "a guess, not confirmed",
    }
}

/// Providers that keep their saved list in `models.json`, which `refresh_provider`
/// replaces. A provider you added keeps its list in `config.json` instead.
const LIVE_LISTS: [&str; 7] = [
    "claude",
    "anthropic",
    "copilot",
    "antigravity",
    "gemini",
    "opencode",
    "opencode-zen",
];

pub(crate) fn supports_refresh(provider: &str) -> bool {
    LIVE_LISTS.contains(&provider) || config::custom_providers().contains_key(provider)
}

const LIST_UPKEEP_FIRST: Duration = Duration::from_secs(60);
const LIST_UPKEEP_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

/// Keeps the saved model lists current while the proxy runs, so a model a
/// provider launches today shows up without anyone running `models refresh`.
/// It asks each signed-in provider a minute after the proxy starts and then
/// every 6 hours. A plain thread, because some refreshes block on their own
/// runtime.
// ponytail: the 6 hours count awake time only (a sleeping laptop does not
// advance the clock), and a provider you added is not refreshed, because its
// list lives in config.json, which this job should not rewrite on its own.
pub(crate) fn spawn_list_upkeep(registry: Arc<Registry>) {
    let signed_in = move |provider: &str| {
        registry
            .provider(provider)
            .is_some_and(|handler| handler.cli().auth_state() != AuthState::Missing)
    };
    let job = move || {
        std::thread::sleep(LIST_UPKEEP_FIRST);
        loop {
            if refresh_lists(&signed_in, refresh_provider) {
                crate::route::refresh_model_labels();
            }
            std::thread::sleep(LIST_UPKEEP_EVERY);
        }
    };
    if let Err(error) = std::thread::Builder::new()
        .name("model-list-upkeep".into())
        .spawn(job)
    {
        tracing::warn!(%error, "Could not start the model list refresh");
    }
}

/// Refreshes the list of every signed-in provider. A failure keeps that list
/// as it was and does not stop the others. True when a list was replaced.
fn refresh_lists(
    signed_in: impl Fn(&str) -> bool,
    refresh: impl Fn(&str) -> Result<Option<Vec<String>>>,
) -> bool {
    let log = crate::logging::create_logger("models");
    let fields = |provider: &str, key: &str, value: serde_json::Value| {
        Some(serde_json::Map::from_iter([
            ("provider".to_string(), serde_json::json!(provider)),
            (key.to_string(), value),
        ]))
    };
    let mut replaced = false;
    for provider in LIVE_LISTS
        .into_iter()
        .filter(|provider| signed_in(provider))
    {
        match refresh(provider) {
            Ok(Some(ids)) => {
                replaced = true;
                log.info(
                    "models_refreshed",
                    fields(provider, "count", serde_json::json!(ids.len())),
                );
            }
            Ok(None) => {}
            Err(error) => log.warn(
                "models_refresh_failed",
                fields(provider, "error", serde_json::json!(format!("{error:#}"))),
            ),
        }
    }
    replaced
}

/// Fetches the live model list of a provider (or every provider that supports
/// live discovery when `provider` is `Some("all")`).
pub fn run_refresh(provider: Option<&str>) -> Result<()> {
    let registry = Registry::new();
    let asked = provider.map(|p| p.trim().to_ascii_lowercase());
    if asked.as_deref() == Some("all") {
        return refresh_all(&registry);
    }
    if asked.is_none() && crate::prompt::is_interactive() {
        let providers = routable_providers(&registry);
        let mut labels = vec!["All providers (all)".to_string()];
        labels.extend(
            providers
                .iter()
                .map(|id| format!("{} ({id})", ui::provider_name(id))),
        );
        let idx = crate::prompt::choose(
            "Which provider's model list do you want to refresh?",
            &labels,
            0,
        )?;
        if idx == 0 {
            return refresh_all(&registry);
        }
        return refresh_one(providers[idx - 1]);
    }
    let provider = pick_provider(
        &registry,
        asked.as_deref(),
        "Which provider's model list do you want to refresh?",
        true,
    )?;
    refresh_one(provider)
}

fn refresh_one(provider: &str) -> Result<()> {
    let name = ui::provider_name(provider);
    println!("Fetching {name}'s model list...");
    match refresh_provider(provider)? {
        Some(ids) => println!("{name} lists {} models now.", ids.len()),
        None => {
            print_discovery_unavailable(provider);
            return Ok(());
        }
    }
    if config::route().target() == Some(provider) {
        crate::route::refresh_model_labels();
    }
    show(&Registry::new(), provider);
    Ok(())
}

pub(crate) fn refresh_provider(provider: &str) -> Result<Option<Vec<String>>> {
    let ids = match provider {
        "copilot" => {
            let Some(auth) = crate::providers::copilot::auth::file_store().load()? else {
                bail!("GitHub Copilot isn't signed in. Run `cc-proxy auth login copilot` first.");
            };
            crate::providers::copilot::models::refresh(&auth)?
        }
        "claude" => crate::providers::claude::refresh_models()?,
        "anthropic" => crate::providers::custom::refresh_anthropic_models()?,
        "antigravity" => crate::providers::antigravity::refresh_models()?,
        "gemini" => crate::providers::gemini::refresh_models()?,
        "opencode" => crate::providers::opencode::model::refresh_models(false)?,
        "opencode-zen" => crate::providers::opencode::model::refresh_models(true)?,
        _ if config::custom_providers().contains_key(provider) => {
            crate::providers::custom::refresh_models(provider)?
        }
        _ => return Ok(None),
    };
    Ok(Some(ids))
}

fn print_discovery_unavailable(provider: &str) {
    if provider == "codex" {
        println!(
            "Codex: reads the Codex CLI's local model cache automatically. No live refresh was run."
        );
    } else {
        println!(
            "{}: live model discovery is not supported yet. Use `cc-proxy models list {provider}` to see the saved list.",
            ui::provider_name(provider)
        );
    }
}

fn refresh_all(registry: &Registry) -> Result<()> {
    let mut failures = 0;
    for provider in routable_providers(registry) {
        let name = ui::provider_name(provider);
        if matches!(
            provider,
            "claude" | "anthropic" | "antigravity" | "copilot" | "gemini"
        ) && registry.provider(provider).is_some_and(|handler| {
            matches!(
                handler.cli().auth_state(),
                crate::provider::AuthState::Missing
            )
        }) {
            println!(
                "{name}: skipped. Sign in with `cc-proxy auth login {provider}` to fetch its models."
            );
            continue;
        }
        println!("Fetching {name}'s model list...");
        match refresh_provider(provider) {
            Ok(Some(ids)) => println!("{name}: {} models.", ids.len()),
            Ok(None) => print_discovery_unavailable(provider),
            Err(error) => {
                failures += 1;
                eprintln!(
                    "{name}: {error:#}. The saved list has not changed. Retry with `cc-proxy models refresh {provider}`."
                );
            }
        }
    }
    crate::route::refresh_model_labels();
    if let Some(active) = config::route().target() {
        show(&Registry::new(), active);
    }
    if failures > 0 {
        bail!(
            "Could not refresh {failures} provider model lists. See the errors above. Other lists were updated."
        );
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

pub(crate) fn set(
    registry: &Registry,
    provider: &str,
    tier: &'static str,
    model: &str,
) -> Result<()> {
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
    fn the_claude_providers_have_live_model_lists() {
        // The monitor shows its refresh action for exactly these.
        assert!(supports_refresh("claude"));
        assert!(supports_refresh("anthropic"));
        assert!(!supports_refresh("kimi"));
    }

    #[test]
    fn the_refresh_job_skips_signed_out_providers_and_survives_a_failure() {
        use std::cell::RefCell;
        let asked = RefCell::new(Vec::new());
        let refresh = |provider: &str| {
            asked.borrow_mut().push(provider.to_string());
            match provider {
                "gemini" => bail!("Gemini refused the request"),
                _ => Ok(Some(vec!["a".to_string()])),
            }
        };

        assert!(refresh_lists(
            |provider| matches!(provider, "claude" | "gemini" | "copilot"),
            refresh
        ));
        // Listed order, no provider that is signed out, and Gemini's failure
        // did not stop Copilot.
        assert_eq!(*asked.borrow(), ["claude", "copilot", "gemini"]);

        asked.borrow_mut().clear();
        assert!(!refresh_lists(|_| false, refresh));
        assert!(asked.borrow().is_empty());
        assert!(!refresh_lists(|provider| provider == "gemini", refresh));
    }

    #[test]
    fn names_are_matched_in_any_case() {
        assert_eq!(tier_arg(" Opus ").unwrap(), "opus");
        let error = tier_arg("gemini").unwrap_err().to_string();
        assert!(error.contains("fable, opus, sonnet, haiku"), "{error}");
    }
}
