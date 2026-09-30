use std::collections::HashSet;

use crate::config;

use super::model_catalog::{catalog_model, catalog_models};
use super::request::ServiceTier;

/// Baseline allowlist compiled into the binary. At runtime the Codex CLI's
/// model cache (`model_catalog`) extends it, so models OpenAI ships to Codex
/// work before a proxy release lists them.
pub const ALLOWED_MODELS: &[&str] = &[
    "gpt-5.2",
    "gpt-5.3-codex",
    "gpt-5.3-codex-spark",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.5",
    "gpt-5.6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-6-astra",
    "gpt-6-luna",
    "gpt-6-sol",
    "gpt-6.1-sol",
];

pub const MODEL_ALIASES: &[(&str, &str)] = &[
    ("haiku", "gpt-6-luna"),
    ("claude-haiku-4-5", "gpt-6-luna"),
    ("claude-haiku-4-5-20251001", "gpt-6-luna"),
    ("sonnet", "gpt-5.6-terra"),
    ("claude-sonnet-4-6", "gpt-5.6-terra"),
    ("claude-sonnet-5", "gpt-5.6-terra"),
    ("opus", "gpt-6-sol"),
    ("claude-opus-4-7", "gpt-6-sol"),
    ("claude-opus-4-8", "gpt-6-sol"),
    ("claude-opus-5", "gpt-6-sol"),
    ("claude-opus-5-5", "gpt-6-sol"),
    ("fable", "gpt-6-sol"),
    ("claude-fable-5", "gpt-6-sol"),
];

#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub model: String,
    pub service_tier: Option<ServiceTier>,
}

/// True for baseline models and for API-supported models from the Codex
/// model cache.
pub fn is_allowed_model(model: &str) -> bool {
    ALLOWED_MODELS.contains(&model)
        || catalog_model(model).is_some_and(|entry| entry.supported_in_api)
}

/// Baseline ∪ API-supported catalog models, sorted and de-duplicated —
/// for listings and error messages.
pub fn allowed_models() -> Vec<String> {
    let mut set: HashSet<String> = ALLOWED_MODELS.iter().map(|m| (*m).to_string()).collect();
    for entry in catalog_models() {
        if entry.supported_in_api {
            set.insert(entry.slug);
        }
    }
    let mut out: Vec<String> = set.into_iter().collect();
    out.sort_unstable();
    out
}

/// Catalog models advertised by the Codex picker (`visibility == "list"`)
/// and usable through the API — what `models` and the unknown-model hint
/// should show in addition to the baseline.
pub fn listed_catalog_models() -> Vec<String> {
    catalog_models()
        .into_iter()
        .filter(|entry| entry.listed && entry.supported_in_api)
        .map(|entry| entry.slug)
        .collect()
}

pub fn allowed_models_display() -> String {
    allowed_models().join(", ")
}

fn fast_model_aliases() -> HashSet<String> {
    allowed_models()
        .iter()
        .map(|m| format!("{m}-fast"))
        .collect()
}

fn resolve_fast_model_alias(model: &str) -> ResolvedModel {
    let fast_set = fast_model_aliases();
    if fast_set.contains(model) {
        let base = model.trim_end_matches("-fast");
        ResolvedModel {
            model: base.to_string(),
            service_tier: Some(ServiceTier::Priority),
        }
    } else if let Some(base) = model
        .strip_suffix("-fast")
        .filter(|base| base.starts_with("gpt-"))
    {
        // The `-fast` suffix is ours: an unlisted `gpt-9-x-fast` still means
        // `gpt-9-x` at priority tier, and Codex reports the base ID itself
        // when it never heard of it.
        ResolvedModel {
            model: base.to_string(),
            service_tier: Some(ServiceTier::Priority),
        }
    } else {
        ResolvedModel {
            model: model.to_string(),
            service_tier: None,
        }
    }
}

pub fn resolve_model_request(model: &str) -> ResolvedModel {
    resolve_model_request_with_config_override(model, true)
}

pub fn resolve_model_request_with_config_override(
    model: &str,
    apply_config_override: bool,
) -> ResolvedModel {
    let alias = MODEL_ALIASES
        .iter()
        .find(|(alias, _)| *alias == model)
        .map(|(_, target)| *target)
        .unwrap_or(model);

    let requested = resolve_fast_model_alias(alias);

    let override_model = apply_config_override.then(config::codex_model).flatten();
    let resolved = match override_model {
        Some(ref val) if !val.is_empty() => resolve_fast_model_alias(val),
        _ => requested.clone(),
    };

    ResolvedModel {
        model: resolved.model,
        service_tier: if requested.service_tier == Some(ServiceTier::Priority)
            || resolved.service_tier == Some(ServiceTier::Priority)
        {
            Some(ServiceTier::Priority)
        } else {
            resolved.service_tier
        },
    }
}

pub fn resolve_model(model: &str) -> String {
    resolve_model_request(model).model
}

#[derive(Debug, Clone)]
pub struct ModelNotAllowedError {
    pub model: String,
}

impl std::fmt::Display for ModelNotAllowedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Model not allowed: {}", self.model)
    }
}

pub fn assert_allowed_model(model: &str) -> Result<(), ModelNotAllowedError> {
    if is_allowed_model(model) {
        Ok(())
    } else {
        Err(ModelNotAllowedError {
            model: model.to_string(),
        })
    }
}

/// Routing gate: everything `assert_allowed_model` accepts, plus any
/// `gpt-*` ID the catalog never listed. Unlisted IDs forward to Codex
/// verbatim — Codex reports the ones it never heard of, so a launch-day
/// model works before any proxy release lists it. Same pattern as
/// `opencode-go/` IDs for OpenCode Go.
pub fn assert_routable_model(model: &str) -> Result<(), ModelNotAllowedError> {
    if is_allowed_model(model) || model.starts_with("gpt-") {
        Ok(())
    } else {
        Err(ModelNotAllowedError {
            model: model.to_string(),
        })
    }
}

/// The gpt-5.6 family defaults to the Responses Lite lane. The lite lane
/// requires `parallel_tool_calls: false` (the backend rejects the request
/// with 400 `unsupported_value` otherwise), so every tool call is serialized
/// into its own assistant turn there. `gpt-5.6-sol` and `gpt-5.6-terra` also
/// exist on the full Responses lane, where parallel tool calls work;
/// `codex.fullLane` / `CCP_CODEX_FULL_LANE` opts them into it. `gpt-5.6-luna`
/// stays on the lite lane unconditionally — see [`full_lane_web_search_model`].
pub fn uses_responses_lite(model: &str) -> bool {
    uses_responses_lite_with_full_lane(model, config::codex_full_lane())
}

fn uses_responses_lite_with_full_lane(model: &str, full_lane: bool) -> bool {
    if model == "gpt-5.6-luna" {
        return true;
    }
    if full_lane && matches!(model, "gpt-5.6-sol" | "gpt-5.6-terra") {
        return false;
    }
    matches!(
        model,
        "gpt-5.6-luna"
            | "gpt-5.6-sol"
            | "gpt-5.6-terra"
            | "gpt-6-astra"
            | "gpt-6-luna"
            | "gpt-6-sol"
    ) || catalog_model(model).is_some_and(|entry| entry.use_responses_lite)
}

/// Luna models exist only behind the Responses Lite lane; the full
/// Responses API resolves them to a `-free` variant and returns 404 (Model not
/// found gpt-5.6-luna-free-...). Hosted web_search requests must run on the
/// full lane, so luna is upgraded to its nearest full-lane sibling.
pub fn full_lane_web_search_model(model: &str) -> &str {
    match model {
        "gpt-5.6-luna" => "gpt-5.6-sol",
        "gpt-6-luna" => "gpt-6-sol",
        _ => model,
    }
}

pub fn is_valid_model_for_codex(model: &str) -> bool {
    if is_allowed_model(model) {
        return true;
    }
    let fast_set = fast_model_aliases();
    if fast_set.contains(model) {
        return true;
    }
    MODEL_ALIASES.iter().any(|(alias, _)| *alias == model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn haiku_resolves_to_luna() {
        let r = resolve_model_request("haiku");
        assert_eq!(r.model, "gpt-6-luna");
    }

    #[test]
    fn responses_lite_defaults_for_56_family_only() {
        for model in ["gpt-5.6-luna", "gpt-5.6-sol", "gpt-5.6-terra"] {
            assert!(uses_responses_lite_with_full_lane(model, false));
        }
        for model in ["gpt-5.3-codex", "gpt-5.4", "gpt-5.5"] {
            assert!(!uses_responses_lite_with_full_lane(model, false));
        }
    }

    #[test]
    fn full_lane_opts_sol_and_terra_out_of_lite_but_never_luna() {
        assert!(!uses_responses_lite_with_full_lane("gpt-5.6-sol", true));
        assert!(!uses_responses_lite_with_full_lane("gpt-5.6-terra", true));
        assert!(uses_responses_lite_with_full_lane("gpt-5.6-luna", true));
        assert!(!uses_responses_lite_with_full_lane("gpt-5.4", true));
    }

    #[test]
    fn web_search_upgrades_luna_to_full_lane_sibling() {
        assert_eq!(full_lane_web_search_model("gpt-5.6-luna"), "gpt-5.6-sol");
        assert_eq!(full_lane_web_search_model("gpt-5.6-sol"), "gpt-5.6-sol");
        assert_eq!(full_lane_web_search_model("gpt-5.6-terra"), "gpt-5.6-terra");
        assert_eq!(full_lane_web_search_model("gpt-5.4"), "gpt-5.4");
        assert_eq!(full_lane_web_search_model("gpt-6-luna"), "gpt-6-sol");
        assert_eq!(full_lane_web_search_model("gpt-6-sol"), "gpt-6-sol");
    }

    #[test]
    fn sonnet_resolves_to_terra() {
        let r = resolve_model_request("sonnet");
        assert_eq!(r.model, "gpt-5.6-terra");
    }

    #[test]
    fn sonnet_5_resolves_to_terra() {
        let r = resolve_model_request("claude-sonnet-5");
        assert_eq!(r.model, "gpt-5.6-terra");
    }

    #[test]
    fn opus_resolves_to_sol() {
        let r = resolve_model_request("opus");
        assert_eq!(r.model, "gpt-6-sol");
    }

    #[test]
    fn opus_aliases_resolve_to_sol() {
        for model in ["claude-opus-4-8", "claude-opus-5", "claude-opus-5-5"] {
            let r = resolve_model_request(model);
            assert_eq!(r.model, "gpt-6-sol");
        }
    }

    #[test]
    fn fable_5_resolves_to_sol() {
        for model in ["fable", "claude-fable-5"] {
            let r = resolve_model_request(model);
            assert_eq!(r.model, "gpt-6-sol");
        }
    }

    #[test]
    fn gpt_6_sol_fast_adds_priority() {
        let r = resolve_model_request("gpt-6-sol-fast");
        assert_eq!(r.model, "gpt-6-sol");
        assert_eq!(r.service_tier, Some(ServiceTier::Priority));
    }

    #[test]
    fn gpt_6_models_use_responses_lite() {
        assert!(uses_responses_lite("gpt-6-sol"));
        assert!(uses_responses_lite("gpt-6-luna"));
    }

    #[test]
    fn fast_suffix_adds_priority() {
        let r = resolve_model_request("gpt-5.6-sol-fast");
        assert_eq!(r.model, "gpt-5.6-sol");
        assert_eq!(r.service_tier, Some(ServiceTier::Priority));
    }

    #[test]
    fn allowed_models_accept_base() {
        assert!(assert_allowed_model("gpt-5.4").is_ok());
        assert!(assert_allowed_model("gpt-5.6-sol").is_ok());
        assert!(assert_allowed_model("gpt-5.6-terra").is_ok());
        assert!(assert_allowed_model("gpt-6-astra").is_ok());
        assert!(assert_allowed_model("gpt-5.6-luna").is_ok());
    }

    #[test]
    fn not_allowed_rejected() {
        assert!(assert_allowed_model("gpt-7").is_err());
        assert!(assert_allowed_model("gpt-7-fast").is_err());
    }

    #[test]
    fn routable_accepts_unlisted_gpt_but_rejects_foreign_ids() {
        assert!(assert_routable_model("gpt-6.1-sol").is_ok());
        assert!(assert_routable_model("gpt-9-future").is_ok());
        assert!(assert_routable_model("not-a-model").is_err());
        assert!(assert_routable_model("claude-sonnet-9").is_err());
    }

    #[test]
    fn unlisted_gpt_fast_strips_suffix_with_priority() {
        let r = resolve_model_request("gpt-9-future-fast");
        assert_eq!(r.model, "gpt-9-future");
        assert_eq!(r.service_tier, Some(ServiceTier::Priority));
    }

    #[test]
    fn allowed_models_listing_contains_baseline() {
        let listing = allowed_models();
        for baseline in ALLOWED_MODELS {
            assert!(listing.iter().any(|m| m == baseline), "{baseline} missing");
        }
        assert!(allowed_models_display().contains("gpt-6-astra"));
    }
}
