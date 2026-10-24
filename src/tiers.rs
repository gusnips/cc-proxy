//! Which model answers each of Claude Code's four model names.
//!
//! Claude Code asks for `fable`, `opus`, `sonnet` or `haiku`, or a longer
//! name such as `claude-opus-4-8`. While routing is on, each of them means one
//! model of the provider Claude Code is sent to. The proxy works that model
//! out from what the provider offers, so a new release is picked up the day
//! it ships, with no list of versions here. A choice the user made always
//! wins over what the proxy works out.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::Route;
use crate::registry::{Registry, anthropic_model_family};
use crate::ui;

/// Biggest first. This is also the order `cc-proxy` prints them in.
pub const TIERS: [&str; 4] = ["fable", "opus", "sonnet", "haiku"];

/// Words in a model id that say which tier it plays, from the words that
/// mean the biggest model to the smallest. A smaller word only counts when
/// no bigger one is there: `gpt-6-sol-fast` is an opus, not a haiku.
const WORDS: [(&str, &[&str]); 4] = [
    ("fable", &["astra", "fable"]),
    ("opus", &["opus", "sol", "max", "ultra", "argon"]),
    ("sonnet", &["sonnet", "terra", "pro", "plus"]),
    (
        "haiku",
        &[
            "haiku", "luna", "flash", "mini", "lite", "fast", "air", "nano",
        ],
    ),
];

/// Words in a model id for something that is not a chat model, or not ready
/// to be the model that answers a name.
const NOT_FOR_CHAT: [&str; 11] = [
    "embed",
    "embedding",
    "tts",
    "audio",
    "image",
    "rerank",
    "moderation",
    "realtime",
    "vision",
    "preview",
    "exp",
];

/// The model the proxy would pick for a tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Guess {
    pub model: String,
    /// False when another model had an equal claim. A guess that isn't
    /// confident is shown to the user to confirm, once.
    pub confident: bool,
}

struct Parsed<'a> {
    id: &'a str,
    /// The id without its version or tier words: models of one line share it.
    stem: String,
    /// `6.10` is newer than `6.9`, so the parts are numbers.
    version: Vec<u32>,
    tiers: BTreeSet<&'static str>,
}

/// A version token is a number, a dotted number, or either with one letter in
/// front (`5.3`, `v4`, `k2.6`).
fn version_part(token: &str) -> Option<Vec<u32>> {
    let digits = match token.strip_prefix(|c: char| c.is_ascii_alphabetic()) {
        Some(rest) if rest.starts_with(|c: char| c.is_ascii_digit()) => rest,
        _ => token,
    };
    digits.split('.').map(|part| part.parse().ok()).collect()
}

fn parse(id: &str) -> Option<Parsed<'_>> {
    let lower = id.to_ascii_lowercase();
    let tokens: Vec<&str> = lower
        .split(['-', '/', ':', '_'])
        .filter(|token| !token.is_empty())
        .collect();
    if tokens.iter().any(|token| NOT_FOR_CHAT.contains(token)) {
        return None;
    }

    let mut tiers: BTreeSet<&'static str> = BTreeSet::new();
    let mut version = Vec::new();
    let mut stem = Vec::new();
    for token in tokens {
        if let Some(part) = version_part(token) {
            version.extend(part);
        } else if let Some((tier, _)) = WORDS.iter().find(|(_, words)| words.contains(&token)) {
            tiers.insert(tier);
        } else if version.is_empty() {
            // Words after the version (`-highspeed`, `-codex`) are not the line.
            stem.push(token);
        }
    }
    if tiers.len() > 1 {
        tiers.remove("haiku");
    }
    Some(Parsed {
        id,
        stem: stem.join(" "),
        version,
        tiers,
    })
}

/// The models to choose from for `tier`: the ones that play it, else the ones
/// that play a nearby tier, else the models that play none (a provider with
/// one line of models, like GLM or Grok), else whatever the provider has.
fn pool<'a, 'b>(parsed: &'b [Parsed<'a>], tier: &str) -> Vec<&'b Parsed<'a>> {
    let nearby: &[&str] = match tier {
        "fable" => &["opus", "sonnet"],
        "opus" => &["sonnet"],
        "sonnet" => &["opus"],
        _ => &["sonnet", "opus"],
    };
    for candidate in std::iter::once(tier).chain(nearby.iter().copied()) {
        let plays_it: Vec<_> = parsed
            .iter()
            .filter(|model| model.tiers.contains(candidate))
            .collect();
        if !plays_it.is_empty() {
            let claude_only: Vec<_> = plays_it
                .iter()
                .copied()
                .filter(|model| model.stem.split_whitespace().any(|w| w == "claude"))
                .collect();
            if !claude_only.is_empty() {
                return claude_only;
            }
            return plays_it;
        }
    }
    let untagged: Vec<_> = parsed
        .iter()
        .filter(|model| model.tiers.is_empty())
        .collect();
    if untagged.is_empty() {
        parsed.iter().collect()
    } else {
        untagged
    }
}

fn best(pool: &[&Parsed<'_>]) -> Option<Guess> {
    // Newest version first. Of equal versions the shorter id wins
    // (`glm-5.3` over `glm-5.3-highspeed`).
    let top = pool.iter().max_by(|a, b| {
        a.version
            .cmp(&b.version)
            .then_with(|| b.id.len().cmp(&a.id.len()))
            .then_with(|| b.id.cmp(a.id))
    })?;
    let one_line = pool.iter().all(|model| model.stem == top.stem);
    let no_tie = pool
        .iter()
        .filter(|model| model.version == top.version)
        .count()
        == 1;
    Some(Guess {
        model: top.id.to_string(),
        confident: one_line && no_tie,
    })
}

fn antigravity_pool<'a, 'b>(parsed: &'b [Parsed<'a>], tier: &str) -> Vec<&'b Parsed<'a>> {
    let is_claude = |model: &Parsed<'_>| model.stem.split_whitespace().any(|w| w == "claude");
    let is_gemini = |model: &Parsed<'_>| model.stem.split_whitespace().any(|w| w == "gemini");
    let primary: Vec<_> = match tier {
        "fable" => parsed
            .iter()
            .filter(|m| is_claude(m) && m.tiers.contains("opus"))
            .collect(),
        "opus" => parsed
            .iter()
            .filter(|m| is_claude(m) && m.tiers.contains("sonnet"))
            .collect(),
        "sonnet" => parsed
            .iter()
            .filter(|m| is_gemini(m) && (m.tiers.contains("opus") || m.tiers.contains("sonnet")))
            .collect(),
        "haiku" => parsed
            .iter()
            .filter(|m| is_gemini(m) && m.tiers.contains("haiku"))
            .collect(),
        _ => Vec::new(),
    };
    if !primary.is_empty() {
        primary
    } else {
        pool(parsed, tier)
    }
}

/// The model for each tier, worked out from `models`. A tier is missing when
/// there is nothing to pick from.
pub fn derive(models: &[String]) -> BTreeMap<&'static str, Guess> {
    let parsed: Vec<Parsed> = models.iter().filter_map(|id| parse(id)).collect();
    TIERS
        .into_iter()
        .filter_map(|tier| Some((tier, best(&pool(&parsed, tier))?)))
        .collect()
}

/// `derive` for a provider the registry knows. A provider that resells other
/// vendors' models gets no guesses: nothing in its catalog is the obvious
/// match, so the user chooses with `cc-proxy models choose`.
pub fn for_provider(registry: &Registry, provider: &str) -> BTreeMap<&'static str, Guess> {
    if registry
        .provider(provider)
        .is_some_and(|handler| handler.aggregator())
    {
        return BTreeMap::new();
    }
    let models = registry.catalog_for(provider);
    if provider == "antigravity" {
        let parsed: Vec<Parsed> = models.iter().filter_map(|id| parse(id)).collect();
        return TIERS
            .into_iter()
            .filter_map(|tier| Some((tier, best(&antigravity_pool(&parsed, tier))?)))
            .collect();
    }
    derive(&models)
}

/// The config.json key that holds the user's choice for `tier` on `provider`.
pub fn choice_key(provider: &str, tier: &str) -> String {
    format!("route.tiers.{provider}.{tier}")
}

/// Why a tier maps to its model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// The user chose it.
    Chosen,
    /// The proxy worked it out and nothing else came close.
    Sure,
    /// The proxy's best guess, or no model at all. The user should confirm it.
    Guess,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapping {
    pub tier: &'static str,
    pub model: Option<String>,
    pub basis: Basis,
}

/// The four tiers on `provider` as they stand now, biggest first.
pub fn mapping(registry: &Registry, route: &Route, provider: &str) -> Vec<Mapping> {
    let mut guesses = for_provider(registry, provider);
    TIERS
        .into_iter()
        .map(|tier| {
            let guess = guesses.remove(tier);
            if let Some(chosen) = route.tier(provider, tier) {
                return Mapping {
                    tier,
                    model: Some(chosen.to_string()),
                    basis: Basis::Chosen,
                };
            }
            let basis = match &guess {
                Some(guess) if guess.confident => Basis::Sure,
                _ => Basis::Guess,
            };
            Mapping {
                tier,
                model: guess.map(|guess| guess.model),
                basis,
            }
        })
        .collect()
}

/// What each Claude name means on `provider`, one line each, or one line when
/// they all mean the same model.
pub fn describe(provider: &str, mappings: &[Mapping]) -> Vec<String> {
    let first = mappings.first().and_then(|first| first.model.as_deref());
    if let Some(model) = first
        && mappings.iter().all(|m| m.model.as_deref() == Some(model))
    {
        return vec![format!("Every Claude name uses {model}.")];
    }
    mappings
        .iter()
        .map(|m| match &m.model {
            Some(model) => format!("{} uses {model}.", m.tier),
            None => format!(
                "{} has no model yet. Pick one with `cc-proxy models choose {provider}`.",
                m.tier
            ),
        })
        .collect()
}

/// Models that rank for a tier, best first, for the user to choose from.
pub fn ranked(registry: &Registry, provider: &str, tier: &str) -> Vec<String> {
    let models = registry.catalog_for(provider);
    let parsed: Vec<Parsed> = models.iter().filter_map(|id| parse(id)).collect();
    let mut candidates = if provider == "antigravity" {
        antigravity_pool(&parsed, tier)
    } else {
        pool(&parsed, tier)
    };
    candidates.sort_by(|a, b| {
        b.version
            .cmp(&a.version)
            .then_with(|| a.id.len().cmp(&b.id.len()))
            .then_with(|| a.id.cmp(b.id))
    });
    candidates
        .into_iter()
        .map(|model| model.id.to_string())
        .collect()
}

/// What a model name from Claude Code means now.
///
/// A Claude name becomes the model chosen for its tier on the provider routing
/// points at: the user's choice if there is one, else the best guess. Any
/// other name passes through unchanged. The error is the message for a 400:
/// routing is off, or the provider has no model for the tier.
pub fn resolve(registry: &Registry, route: &Route, model: &str) -> Result<String, String> {
    if anthropic_model_family(model).is_none() {
        return Ok(model.to_string());
    }
    let Some(provider) = route.target() else {
        return Err(format!(
            "cc-proxy routing is off, so it can't serve \"{model}\". This Claude Code window \
             still points at the proxy. Quit it and start it again to use your Claude \
             subscription, or run `cc-proxy start` to route it through a provider."
        ));
    };
    resolve_for(registry, route, provider, model)
}

/// `resolve` as if Claude Code pointed at `provider`: what a routed name
/// means on another provider, for the fallback chain.
pub fn resolve_for(
    registry: &Registry,
    route: &Route,
    provider: &str,
    model: &str,
) -> Result<String, String> {
    let Some(tier) = anthropic_model_family(model) else {
        return Ok(model.to_string());
    };
    if let Some(chosen) = route.tier(provider, tier) {
        return Ok(chosen.to_string());
    }
    if provider == "anthropic" && model.starts_with("claude-") && model != format!("claude-{tier}")
    {
        return Ok(format!("anthropic/{model}"));
    }
    match for_provider(registry, provider).remove(tier) {
        Some(guess) => Ok(guess.model),
        None => Err(format!(
            "cc-proxy has no {tier} model for {}. Run `cc-proxy models choose {provider}` to pick \
             one.",
            ui::provider_name(provider)
        )),
    }
}

/// A display-only name for the provider and model that answer `requested`.
/// The original request ID stays intact so Claude Code keeps its capabilities.
pub fn model_label(
    registry: &Registry,
    route: &Route,
    requested: &str,
) -> Option<crate::claude_settings::ModelLabel> {
    use crate::providers::{codex, copilot, cursor, grok, kimi, opencode};
    use crate::registry::normalize_incoming_model;

    route.target()?;
    let normalized = normalize_incoming_model(requested);
    let resolved = resolve(registry, route, &normalized).ok()?;
    let handler = registry.provider_for_model(&resolved)?;
    let provider = handler.name();
    let mut description = match anthropic_model_family(&normalized) {
        Some(tier) => {
            let mut name = tier.to_string();
            name[..1].make_ascii_uppercase();
            format!("Answers {name} requests. Claude Code sends {requested}.")
        }
        None => format!("Claude Code sends {requested}."),
    };
    let upstream = match provider {
        "codex" => {
            let model = codex::translate::model_allowlist::resolve_model(&resolved);
            codex::translate::model_allowlist::assert_routable_model(&model).ok()?;
            model
        }
        "kimi" => {
            let model = kimi::translate::model_allowlist::resolve_model(&resolved);
            kimi::translate::model_allowlist::assert_routable_model(&model).ok()?;
            model
        }
        "grok" => {
            let model = grok::translate::model_allowlist::resolve_model(&resolved);
            grok::translate::model_allowlist::assert_routable_model(&model).ok()?;
            model
        }
        "opencode" => opencode::model::resolve(&resolved)?.id,
        "opencode-zen" => opencode::model::resolve_zen(&resolved)?.id,
        "cursor" => {
            let model = cursor::model::resolve_cursor_model(&resolved).ok()?;
            let mode = match model.mode {
                cursor::model::CursorAgentMode::Agent => "agent",
                cursor::model::CursorAgentMode::Plan => "plan",
                cursor::model::CursorAgentMode::Ask => "ask",
            };
            description.push_str(&format!(" Cursor {mode} mode."));
            model.model_id
        }
        "copilot" => resolved.strip_prefix(copilot::MODEL_PREFIX)?.to_string(),
        "glm" => normalize_incoming_model(&resolved),
        other => {
            let prefix = format!("{other}/");
            let normalized = normalize_incoming_model(&resolved);
            normalized
                .strip_prefix(&prefix)
                .unwrap_or(&normalized)
                .to_string()
        }
    };
    if upstream.is_empty() {
        return None;
    }
    Some(crate::claude_settings::ModelLabel {
        model: requested.to_string(),
        label: format!("{provider}/{upstream}"),
        description,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(models: &[&str]) -> Vec<String> {
        models.iter().map(|model| (*model).to_string()).collect()
    }

    fn picked(models: &[&str]) -> Vec<(&'static str, String, bool)> {
        let mut found = derive(&ids(models));
        TIERS
            .into_iter()
            .filter_map(|tier| Some((tier, found.remove(tier)?)))
            .map(|(tier, guess)| (tier, guess.model, guess.confident))
            .collect()
    }

    #[test]
    fn codex_families_map_by_their_words_and_the_newest_version_wins() {
        let found = derive(&ids(&[
            "gpt-5.5",
            "gpt-5.6-luna",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-6-astra",
            "gpt-6-luna",
            "gpt-6-sol",
            "gpt-6.1-sol",
        ]));
        let model = |tier: &str| found[tier].model.as_str();
        assert_eq!(model("fable"), "gpt-6-astra");
        // 6.1 is newer than 6, and the plain `gpt-5.5` plays no tier.
        assert_eq!(model("opus"), "gpt-6.1-sol");
        assert_eq!(model("sonnet"), "gpt-5.6-terra");
        assert_eq!(model("haiku"), "gpt-6-luna");
        assert!(found.values().all(|guess| guess.confident));
    }

    #[test]
    fn versions_compare_as_numbers() {
        let found = derive(&ids(&["gpt-6.9-sol", "gpt-6.10-sol"]));
        assert_eq!(found["opus"].model, "gpt-6.10-sol");
    }

    #[test]
    fn a_service_variant_does_not_become_the_small_model() {
        // `-fast` is a haiku word, but `sol` is a bigger one.
        let found = derive(&ids(&["gpt-6-sol", "gpt-6-sol-fast"]));
        assert_eq!(found["opus"].model, "gpt-6-sol");
        assert_eq!(found["haiku"].model, "gpt-6-sol");
    }

    #[test]
    fn one_line_of_models_fills_every_tier_and_small_ones_take_haiku() {
        let found = picked(&["grok-4.5", "grok-4.6", "grok-4.7", "grok-composer-2.5-fast"]);
        assert_eq!(
            found,
            [
                ("fable", "grok-4.7".to_string(), true),
                ("opus", "grok-4.7".to_string(), true),
                ("sonnet", "grok-4.7".to_string(), true),
                ("haiku", "grok-composer-2.5-fast".to_string(), true),
            ]
        );
    }

    #[test]
    fn a_tie_is_a_guess_not_a_certainty() {
        let found = derive(&ids(&["glm-5.2", "glm-5.3", "glm-5.3-highspeed"]));
        assert_eq!(found["opus"].model, "glm-5.3");
        assert!(!found["opus"].confident);
    }

    #[test]
    fn models_from_different_lines_are_a_guess() {
        let found = derive(&ids(&["deepseek-v4-pro", "mimo-v2.6-pro"]));
        assert_eq!(found["sonnet"].model, "deepseek-v4-pro");
        assert!(!found["sonnet"].confident);
    }

    #[test]
    fn a_reseller_gets_no_guesses() {
        let registry = Registry::new();
        for provider in ["opencode", "opencode-zen"] {
            assert!(for_provider(&registry, provider).is_empty(), "{provider}");
        }
    }

    #[test]
    fn copilot_keeps_claude_models_for_claude_names() {
        let found = picked(&[
            "copilot/claude-opus-4.8",
            "copilot/claude-sonnet-5",
            "copilot/gpt-5.3-codex",
            "copilot/gpt-5.4-mini",
            "copilot/gpt-5.5",
        ]);
        assert_eq!(
            found,
            [
                ("fable", "copilot/claude-opus-4.8".to_string(), true),
                ("opus", "copilot/claude-opus-4.8".to_string(), true),
                ("sonnet", "copilot/claude-sonnet-5".to_string(), true),
                ("haiku", "copilot/gpt-5.4-mini".to_string(), true),
            ]
        );
    }

    #[test]
    fn models_that_are_not_for_chat_or_not_ready_are_left_out() {
        let found = derive(&ids(&[
            "text-embedding-3",
            "deepseek-v4-flash-vision-exp",
            "hy4-preview",
            "deepseek-v4-flash",
        ]));
        assert_eq!(found["haiku"].model, "deepseek-v4-flash");
        assert_eq!(found["opus"].model, "deepseek-v4-flash");
    }

    #[test]
    fn a_provider_with_no_models_has_no_tiers() {
        assert!(derive(&[]).is_empty());
    }

    #[test]
    fn mapping_says_who_decided_each_tier() {
        let registry = Registry::new();
        let mut route = Route::default();
        route
            .tiers
            .entry("grok".into())
            .or_default()
            .insert("opus".into(), "grok-4.5".into());
        let found = mapping(&registry, &route, "grok");
        let tiers: Vec<_> = found.iter().map(|m| m.tier).collect();
        assert_eq!(tiers, TIERS);
        assert_eq!(found[1].model.as_deref(), Some("grok-4.5"));
        assert_eq!(found[1].basis, Basis::Chosen);
        assert_eq!(found[2].basis, Basis::Sure);

        // OpenCode resells other vendors' models, so it has none until you choose.
        let found = mapping(&registry, &route, "opencode");
        assert!(
            found
                .iter()
                .all(|m| m.basis == Basis::Guess && m.model.is_none())
        );
    }

    #[test]
    fn describe_folds_equal_names_into_one_line() {
        let same = |tier| Mapping {
            tier,
            model: Some("k3".into()),
            basis: Basis::Sure,
        };
        assert_eq!(
            describe("kimi", &TIERS.map(same)),
            ["Every Claude name uses k3."]
        );

        let mut mixed = TIERS.map(same);
        mixed[3].model = None;
        let lines = describe("kimi", &mixed);
        assert_eq!(lines[0], "fable uses k3.");
        assert_eq!(
            lines[3],
            "haiku has no model yet. Pick one with `cc-proxy models choose kimi`."
        );
    }

    #[test]
    fn resolve_follows_the_route_and_the_users_choice() {
        let registry = Registry::new();
        let mut route = Route {
            on: true,
            provider: Some("grok".into()),
            ..Route::default()
        };
        assert_eq!(
            resolve(&registry, &route, "claude-opus-4-8"),
            Ok("grok-4.7".into())
        );
        assert_eq!(
            resolve(&registry, &route, "gpt-6-sol"),
            Ok("gpt-6-sol".into())
        );

        route
            .tiers
            .entry("grok".into())
            .or_default()
            .insert("opus".into(), "grok-4.5".into());
        assert_eq!(resolve(&registry, &route, "opus"), Ok("grok-4.5".into()));

        route.on = false;
        let off = resolve(&registry, &route, "sonnet").unwrap_err();
        assert!(off.contains("routing is off"), "{off}");
        // A model named in full never needed the route.
        assert_eq!(
            resolve(&registry, &route, "grok-4.7"),
            Ok("grok-4.7".into())
        );
    }
}
