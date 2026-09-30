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

/// Baseline and listed API-supported CLI cache models, with local tier
/// variants. Kept here so Registry and the provider advertise the same IDs.
pub fn advertised_models() -> Vec<String> {
    advertised_models_with_catalog(listed_catalog_models())
}

fn advertised_models_with_catalog(catalog: Vec<String>) -> Vec<String> {
    let mut models = HashSet::new();
    for model in ALLOWED_MODELS
        .iter()
        .map(|model| (*model).to_string())
        .chain(catalog)
    {
        models.insert(model.clone());
        models.extend(tier_model_variants(&model));
    }
    let mut models: Vec<String> = models.into_iter().collect();
    models.sort_unstable();
    models
}

pub fn allowed_models_display() -> String {
    allowed_models().join(", ")
}

/// Models with known ultrafast support. The current CLI cache has no tier
/// metadata, so catalog discovery alone does not imply ultrafast support.
pub const ULTRAFAST_MODELS: &[&str] = &["gpt-6-astra"];

/// Local tier suffixes also apply to catalog models and unlisted `gpt-*`
/// IDs. Strip once; the routing gate rejects any remaining local suffix.
pub fn split_tier_suffix(model: &str) -> Option<(&str, ServiceTier)> {
    let (base, tier) = if let Some(base) = model.strip_suffix("-ultrafast") {
        (base, ServiceTier::Ultrafast)
    } else {
        (model.strip_suffix("-fast")?, ServiceTier::Priority)
    };
    (base.starts_with("gpt-") || is_allowed_model(base)).then_some((base, tier))
}

pub fn tier_model_variants(model: &str) -> Vec<String> {
    let mut variants = vec![format!("{model}-fast")];
    if ULTRAFAST_MODELS.contains(&model) {
        variants.push(format!("{model}-ultrafast"));
    }
    variants
}

/// Unknown tier support uses the existing priority tier, not a claim that
/// every discovered or unlisted model supports ultrafast.
pub fn service_tier_for_model(model: &str, tier: ServiceTier) -> ServiceTier {
    match tier {
        ServiceTier::Ultrafast if !ULTRAFAST_MODELS.contains(&model) => ServiceTier::Priority,
        tier => tier,
    }
}

fn resolve_tier_model_alias(model: &str) -> ResolvedModel {
    match split_tier_suffix(model) {
        Some((base, tier)) => ResolvedModel {
            model: base.to_string(),
            service_tier: Some(tier),
        },
        None => ResolvedModel {
            model: model.to_string(),
            service_tier: None,
        },
    }
}

pub fn resolve_model_request(model: &str) -> ResolvedModel {
    resolve_model_request_with_config_override(model, true)
}

pub fn resolve_model_request_with_config_override(
    model: &str,
    apply_config_override: bool,
) -> ResolvedModel {
    let override_model = apply_config_override.then(config::codex_model).flatten();
    resolve_with_model_override(model, override_model.as_deref())
}

fn resolve_with_model_override(model: &str, override_model: Option<&str>) -> ResolvedModel {
    let alias = MODEL_ALIASES
        .iter()
        .find(|(alias, _)| *alias == model)
        .map(|(_, target)| *target)
        .unwrap_or(model);

    let requested = resolve_tier_model_alias(alias);
    // A model override must not hide a repeated or mixed local suffix.
    // Keep the unresolved suffix so the routing gate rejects the request.
    if requested.model.ends_with("-fast") || requested.model.ends_with("-ultrafast") {
        return requested;
    }
    let resolved = match override_model {
        Some(val) if !val.is_empty() => resolve_tier_model_alias(val),
        _ => requested.clone(),
    };
    // An override's own suffix wins. Otherwise keep the requested tier,
    // narrowed against the final model rather than the original request.
    let service_tier = resolved
        .service_tier
        .or(requested.service_tier)
        .map(|tier| service_tier_for_model(&resolved.model, tier));

    ResolvedModel {
        model: resolved.model,
        service_tier,
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
    // Called after model resolution: a remaining suffix means the caller
    // stacked local tier suffixes. Never forward those synthetic names.
    if !model.ends_with("-fast")
        && !model.ends_with("-ultrafast")
        && (is_allowed_model(model) || model.starts_with("gpt-"))
    {
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
    if split_tier_suffix(model).is_some_and(|(base, _)| is_allowed_model(base)) {
        return true;
    }
    MODEL_ALIASES.iter().any(|(alias, _)| *alias == model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_listing_preserves_catalog_and_only_advertises_astra_ultrafast() {
        let models = advertised_models_with_catalog(vec![
            "gpt-9-catalog".to_string(),
            "codex-cache-only".to_string(),
            "gpt-6-astra".to_string(),
        ]);
        for model in ALLOWED_MODELS
            .iter()
            .copied()
            .chain(["gpt-9-catalog", "codex-cache-only"])
        {
            assert!(models.contains(&model.to_string()), "{model}");
            assert!(models.contains(&format!("{model}-fast")), "{model}");
        }
        assert!(models.contains(&"gpt-6-astra-ultrafast".to_string()));
        assert!(!models.contains(&"gpt-6-sol-ultrafast".to_string()));
        assert!(!models.contains(&"gpt-9-catalog-ultrafast".to_string()));
        assert!(models.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn tier_suffixes_preserve_baseline_catalog_and_unlisted_fast_models() {
        let catalog = super::super::model_catalog::parse_catalog(
            br#"{"models":[{"slug":"gpt-9-catalog","use_responses_lite":true}]}"#,
        )
        .unwrap();
        for model in ALLOWED_MODELS
            .iter()
            .copied()
            .chain(catalog.iter().map(|entry| entry.slug.as_str()))
            .chain(["gpt-9-unlisted"])
        {
            let base = resolve_with_model_override(model, None);
            assert_eq!(base.model, model);
            assert_eq!(base.service_tier, None);
            let fast = resolve_with_model_override(&format!("{model}-fast"), None);
            assert_eq!(fast.model, model);
            assert_eq!(fast.service_tier, Some(ServiceTier::Priority));
            assert!(assert_routable_model(&fast.model).is_ok());
        }
    }

    #[test]
    fn ultrafast_suffix_uses_astra_or_priority_fallback() {
        for (model, tier) in [
            ("gpt-6-astra", ServiceTier::Ultrafast),
            ("gpt-6-sol", ServiceTier::Priority),
            ("gpt-9-unlisted", ServiceTier::Priority),
        ] {
            let resolved = resolve_with_model_override(&format!("{model}-ultrafast"), None);
            assert_eq!(resolved.model, model);
            assert_eq!(resolved.service_tier, Some(tier));
            assert!(assert_routable_model(&resolved.model).is_ok());
        }
    }

    #[test]
    fn model_override_tier_precedence_uses_final_model() {
        for (requested, override_model, model, tier) in [
            (
                "gpt-6-sol-fast",
                "gpt-6-astra-ultrafast",
                "gpt-6-astra",
                ServiceTier::Ultrafast,
            ),
            (
                "gpt-6-astra-ultrafast",
                "gpt-6-astra-fast",
                "gpt-6-astra",
                ServiceTier::Priority,
            ),
            (
                "gpt-6-astra-ultrafast",
                "gpt-6-sol",
                "gpt-6-sol",
                ServiceTier::Priority,
            ),
            (
                "gpt-6-astra-ultrafast",
                "gpt-6-astra",
                "gpt-6-astra",
                ServiceTier::Ultrafast,
            ),
            (
                "gpt-9-unlisted-fast",
                "gpt-6-astra",
                "gpt-6-astra",
                ServiceTier::Priority,
            ),
        ] {
            let resolved = resolve_with_model_override(requested, Some(override_model));
            assert_eq!(resolved.model, model);
            assert_eq!(resolved.service_tier, Some(tier));
        }
        for (alias, model) in [
            ("haiku", "gpt-6-luna"),
            ("sonnet", "gpt-5.6-terra"),
            ("fable", "gpt-6-sol"),
        ] {
            let resolved = resolve_with_model_override(alias, None);
            assert_eq!(resolved.model, model);
            assert_eq!(resolved.service_tier, None);
        }
    }

    #[test]
    fn tier_suffixes_strip_once_and_reject_stacked_names() {
        for model in [
            "gpt-6-astra-fast-fast",
            "gpt-6-astra-ultrafast-fast",
            "gpt-6-astra-fast-ultrafast",
            "gpt-9-unlisted-ultrafast-ultrafast",
        ] {
            for override_model in [None, Some("gpt-6-astra")] {
                let resolved = resolve_with_model_override(model, override_model);
                assert!(assert_routable_model(&resolved.model).is_err(), "{model}");
            }
        }
        for model in ["-fast", "grok-9-fast", "kimi-k9-ultrafast"] {
            assert_eq!(split_tier_suffix(model), None);
            assert_eq!(resolve_with_model_override(model, None).model, model);
        }
    }

    #[test]
    fn tier_model_variants_advertise_ultrafast_only_for_astra() {
        assert_eq!(
            tier_model_variants("gpt-6-astra"),
            ["gpt-6-astra-fast", "gpt-6-astra-ultrafast"]
        );
        for model in ["gpt-6-sol", "gpt-9-catalog"] {
            assert_eq!(tier_model_variants(model), [format!("{model}-fast")]);
        }
    }

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
