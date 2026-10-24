//! Sending Claude Code through the proxy, and back.
//!
//! Routing has two halves that have to agree. `route` in config.json tells the
//! proxy where Claude's own model names go, and the proxy reads it on every
//! request, so a switch needs no restart. The `env` block in Claude Code's
//! settings (see `claude_settings`) is what makes a Claude Code window talk to
//! the proxy at all.
//!
//! Routing starts only when the user runs `cc-proxy start` or `cc-proxy use`.
//! It ends when they run `cc-proxy stop` or `cc-proxy use claude`, or when the
//! proxy turns out to be gone ([`heal`]). Nothing else turns it back on.

use std::time::Duration;

use anyhow::Result;

use crate::registry::Registry;
use crate::ui::{self, Mood};
use crate::{claude_settings, config, config_keys, daemon, tiers};

/// The built-in providers `start` and `use` can send Claude Code to.
pub const PROVIDERS: [&str; 11] = [
    "codex",
    "kimi",
    "cursor",
    "grok",
    "copilot",
    "glm",
    "anthropic",
    "gemini",
    "antigravity",
    "opencode",
    "opencode-zen",
];

/// Every provider `start` and `use` can send Claude Code to: the built-ins in
/// their canonical order, followed by any user-configured custom providers.
pub fn routable_providers(registry: &Registry) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = PROVIDERS
        .into_iter()
        .filter(|name| registry.provider(name).is_some())
        .collect();
    for name in registry.list_provider_names() {
        if !out.contains(&name.as_str()) {
            out.push(crate::provider::intern_provider_name(&name));
        }
    }
    out
}

/// What `cc-proxy use` takes to mean "no routing".
pub const CLAUDE: &str = "claude";

/// Fails before anything starts when Claude Code's settings can't be edited,
/// so a refused `start` doesn't leave a proxy running that nothing points at.
pub fn preflight() -> Result<()> {
    claude_settings::inspect().map(drop).map_err(|error| {
        anyhow::anyhow!("{error:#}. cc-proxy changed nothing. Fix the file, then run it again.")
    })
}

/// Send Claude Code through the proxy at `base_url`, to `provider`.
pub fn activate(provider: &str, base_url: &str) -> Result<()> {
    // The settings file is the step that can refuse (a file we can't read as
    // JSON), so it goes first and nothing else changes when it does.
    claude_settings::claim(base_url)?;
    config_keys::write_values([
        ("route.on", serde_json::Value::Bool(true)),
        ("route.provider", provider.into()),
    ])?;
    refresh_model_labels();
    Ok(())
}

/// Update names only while both the route and the settings ownership are active.
/// Observed IDs keep an existing Claude session's native model capabilities intact.
pub fn sync_model_labels(
    registry: &Registry,
    route: &config::Route,
    observed: Option<&str>,
) -> Result<bool> {
    let Some(target) = route.target() else {
        return Ok(false);
    };
    let base = picker_models(registry, target);
    let base_refs: Vec<&str> = base.iter().map(String::as_str).collect();
    claude_settings::sync_model_labels(&base_refs, observed, |model| {
        tiers::model_label(registry, route, model)
    })
}

/// The most catalog models Claude Code's `/model` picker gets. OpenCode's
/// catalogs fit. OpenRouter lists thousands, which would bury the picker and
/// cost seconds per request to label, so a catalog that big gets only the
/// four Claude names.
const PICKER_CATALOG_LIMIT: usize = 100;

/// What Claude Code's picker lists for `target`: the four Claude names, then
/// the whole catalog of a provider whose models those names don't cover,
/// when it is small enough to read.
fn picker_models(registry: &Registry, target: &str) -> Vec<String> {
    let mut models: Vec<String> = tiers::TIERS
        .iter()
        .map(|tier| (*tier).to_string())
        .collect();
    let whole_catalog = matches!(target, "antigravity" | "gemini")
        || registry
            .provider(target)
            .is_some_and(|handler| handler.aggregator());
    let catalog = registry.catalog_for(target);
    if whole_catalog && catalog.len() <= PICKER_CATALOG_LIMIT {
        for model in catalog {
            if !models.contains(&model) {
                models.push(model);
            }
        }
    }
    models
}

/// Commands refresh labels after saving their changes. A display failure must
/// not undo a valid routing or model change.
pub fn refresh_model_labels() -> bool {
    match sync_model_labels(&Registry::new(), &config::route(), None) {
        Ok(changed) => changed,
        Err(error) => {
            ui::eprint_note(
                Mood::Unsure,
                &[format!(
                    "Routing still works, but Claude Code's model labels couldn't be updated: {error:#}"
                )],
            );
            false
        }
    }
}

/// Send Claude Code back to Anthropic. Reports a settings file it couldn't
/// change, after the proxy has stopped serving Claude's names either way.
pub fn deactivate() -> Result<()> {
    let config = if config::route().on {
        config_keys::write_value("route.on", serde_json::Value::Bool(false)).map(drop)
    } else {
        Ok(())
    };
    let settings = claude_settings::release().map(drop);
    settings.and(config)
}

/// Point Claude Code at the proxy's new address after a restart moved it.
/// Does nothing while routing is off. Returns whether it wrote the settings.
pub fn refresh(base_url: &str) -> Result<bool> {
    if config::route().target().is_none() {
        return Ok(false);
    }
    let connection_changed = claude_settings::claim(base_url)?;
    Ok(refresh_model_labels() || connection_changed)
}

/// Routing that outlived its proxy (a crash, a reboot, a kill) leaves every
/// Claude Code window pointed at nothing. Put them back on Anthropic.
pub fn heal() {
    let Ok(found) = claude_settings::inspect() else {
        return;
    };
    let Some(url) = found.routed_to else {
        return;
    };
    if proxy_answers(&url) {
        return;
    }
    match deactivate() {
        Ok(()) => ui::eprint_note(
            Mood::Asleep,
            &[
                "cc-proxy wasn't running, so Claude Code goes back to your Claude subscription."
                    .into(),
                "Run `cc-proxy start` to route it again.".into(),
            ],
        ),
        Err(error) => ui::eprint_note(Mood::Unsure, &[error.to_string()]),
    }
}

/// Whether something serves `url`. Asks a few times: a `restart` in another
/// terminal is down for a moment, and that is not a reason to switch routing off.
fn proxy_answers(url: &str) -> bool {
    let claimed_port = url::Url::parse(url)
        .ok()
        .and_then(|url| url.port_or_known_default());
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(500));
        }
        let managed = !matches!(daemon::describe(), daemon::DaemonStatus::Stopped);
        if managed || claimed_port.is_some_and(daemon::probe_port_health) {
            return true;
        }
    }
    false
}

/// The lines `cc-proxy status` shows about routing.
pub fn status_lines() -> Vec<String> {
    let route = config::route();
    let found = match claude_settings::inspect() {
        Ok(found) => found,
        Err(error) => return vec![error.to_string()],
    };
    let mut lines = Vec::new();
    match (route.target(), found.routed_to) {
        (Some(provider), Some(_)) => lines.push(format!(
            "Claude Code goes to {}. `cc-proxy use claude` switches back to your subscription.",
            ui::provider_name(provider)
        )),
        (Some(provider), None) => lines.push(format!(
            "Routing is on for {}, but Claude Code's settings don't point at the proxy. \
             `cc-proxy use {provider}` fixes that.",
            ui::provider_name(provider)
        )),
        (None, Some(url)) => lines.push(format!(
            "Claude Code's settings point at {url}, but routing is off. `cc-proxy use claude` \
             points them back at Anthropic."
        )),
        (None, None) => lines.push(
            "Claude Code uses your Claude subscription. `cc-proxy start` routes it through a \
             provider."
                .into(),
        ),
    }
    lines.extend(found.warnings);
    if let Some(provider) = route.target()
        && !route.fallback.is_empty()
    {
        lines.push(format!(
            "When {} is spent, Claude Code moves to {}.",
            ui::provider_name(provider),
            route
                .fallback
                .iter()
                .map(|name| ui::provider_name(name))
                .collect::<Vec<_>>()
                .join(", then ")
        ));
    }
    lines
}

/// Save the providers to try, in order, when the routed provider is spent.
/// An empty list turns the chain off.
pub fn set_fallback(providers: &[String]) -> Result<()> {
    let routable = routable_providers(&Registry::new());
    let mut names: Vec<String> = Vec::new();
    for raw in providers {
        let name = raw.trim().to_ascii_lowercase();
        if name.is_empty() {
            continue;
        }
        if !routable.contains(&name.as_str()) {
            anyhow::bail!(
                "cc-proxy can't fall back to `{name}`. Use one of: {}.",
                routable.join(", ")
            );
        }
        if !names.contains(&name) {
            names.push(name);
        }
    }
    config_keys::write_values([("route.fallback", serde_json::json!(names))])?;
    Ok(())
}

/// A note for when the running proxy is not the version of this binary,
/// which happens after an upgrade until the proxy restarts.
pub fn version_note(port: u16) -> Option<String> {
    let ours = env!("CARGO_PKG_VERSION");
    match daemon::running_version(port) {
        Some(running) if running == ours => None,
        Some(running) => Some(format!(
            "The running proxy is {running}, and this is {ours}. `cc-proxy restart` updates it."
        )),
        // Answers, but doesn't say its version: it predates 0.2.0, so it also
        // ignores the provider picked here.
        None if daemon::probe_port_health(port) => Some(format!(
            "The running proxy is older than {ours} and ignores the provider you pick. \
             `cc-proxy restart` updates it."
        )),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::{CustomFormat, CustomProviderConfig};
    use crate::provider::Provider;
    use crate::providers::custom::CustomProvider;

    fn reseller(name: &str, models: usize) -> Arc<dyn Provider> {
        Arc::new(CustomProvider::from_config(
            name,
            CustomProviderConfig {
                base_url: "http://127.0.0.1:9/v1".into(),
                format: CustomFormat::OpenAi,
                api_key: None,
                api_key_env: None,
                models: (0..models).map(|i| format!("vendor/model-{i}")).collect(),
            },
        ))
    }

    #[test]
    fn gemini_picker_offers_the_preview_without_making_it_a_default() {
        let registry =
            Registry::from_providers([
                Arc::new(crate::providers::gemini::GeminiProvider::new()) as Arc<dyn Provider>
            ]);
        assert_eq!(
            picker_models(&registry, "gemini"),
            [
                "fable",
                "opus",
                "sonnet",
                "haiku",
                "gemini-3.1-pro-preview",
                "gemini-3.8-flash",
                "gemini-4-argon",
            ]
        );
    }

    #[test]
    fn a_catalog_too_big_to_read_leaves_the_picker_to_the_claude_names() {
        let registry = Registry::from_providers([reseller("thousands", 150), reseller("few", 3)]);
        assert_eq!(picker_models(&registry, "thousands"), tiers::TIERS);
        let few = picker_models(&registry, "few");
        assert_eq!(few.len(), tiers::TIERS.len() + 3);
        assert!(few.contains(&"few/vendor/model-2".to_string()), "{few:?}");
    }
}
