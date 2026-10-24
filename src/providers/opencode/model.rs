pub const MODEL_PREFIX: &str = "opencode-go/";
pub const ZEN_MODEL_PREFIX: &str = "opencode-zen/";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointKind {
    ChatCompletions,
    Messages,
    Responses,
    Google,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelSpec {
    pub id: &'static str,
    pub endpoint: EndpointKind,
}

pub const MODELS: &[ModelSpec] = &[
    ModelSpec {
        id: "grok-4.7",
        endpoint: EndpointKind::Responses,
    },
    ModelSpec {
        id: "grok-4.6",
        endpoint: EndpointKind::Responses,
    },
    ModelSpec {
        id: "grok-4.5",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "gpt-6-luna",
        endpoint: EndpointKind::Responses,
    },
    ModelSpec {
        id: "gpt-5.6-luna",
        endpoint: EndpointKind::Responses,
    },
    ModelSpec {
        id: "glm-5.2",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "glm-5.3-flash",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "glm-5.3",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "glm-5.1",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "glm-5",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "kimi-k3",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "kimi-k2.7-code",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "kimi-k2.6",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "kimi-k2.5",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "longcat-2.0",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "longcat-2.5-preview-free",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "deepseek-v4-pro",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "deepseek-v4-flash",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "deepseek-flash",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "deepseek-v4.1-flash",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "deepseek-v4-flash-vision-exp",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "mimo-v2-pro",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "mimo-v2-omni",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "mimo-v2.5",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "mimo-v2.5-pro",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "mimo-v2.6-flash",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "mimo-v2.6-pro",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "minimax-m3",
        endpoint: EndpointKind::Messages,
    },
    ModelSpec {
        id: "minimax-m2.7",
        endpoint: EndpointKind::Messages,
    },
    ModelSpec {
        id: "minimax-m2.5",
        endpoint: EndpointKind::Messages,
    },
    ModelSpec {
        id: "qwen3.8-max",
        endpoint: EndpointKind::Messages,
    },
    ModelSpec {
        id: "qwen3.8-flash",
        endpoint: EndpointKind::Messages,
    },
    ModelSpec {
        id: "qwen3.7-max",
        endpoint: EndpointKind::Messages,
    },
    ModelSpec {
        id: "qwen3.7-plus",
        endpoint: EndpointKind::Messages,
    },
    ModelSpec {
        id: "qwen3.6-plus",
        endpoint: EndpointKind::Messages,
    },
    ModelSpec {
        id: "qwen3.5-plus",
        endpoint: EndpointKind::Messages,
    },
    ModelSpec {
        id: "hy3",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "hy4-preview",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "hy3-preview",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "muse-spark-1.3-contributor",
        endpoint: EndpointKind::Responses,
    },
    ModelSpec {
        id: "muse-spark-1.2-contributor",
        endpoint: EndpointKind::Responses,
    },
    ModelSpec {
        id: "omen-alpha",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "space-bunny-free",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "gemini-4-argon-contributor",
        endpoint: EndpointKind::Google,
    },
];

pub const ZEN_MODELS: &[ModelSpec] = &[
    ModelSpec {
        id: "gpt-6-astra",
        endpoint: EndpointKind::Responses,
    },
    ModelSpec {
        id: "gpt-6-sol",
        endpoint: EndpointKind::Responses,
    },
    ModelSpec {
        id: "gpt-6.1-sol",
        endpoint: EndpointKind::Responses,
    },
    ModelSpec {
        id: "gpt-6-luna",
        endpoint: EndpointKind::Responses,
    },
    ModelSpec {
        id: "gpt-5.6-sol",
        endpoint: EndpointKind::Responses,
    },
    ModelSpec {
        id: "gpt-5.6-terra",
        endpoint: EndpointKind::Responses,
    },
    ModelSpec {
        id: "gpt-5.6-luna",
        endpoint: EndpointKind::Responses,
    },
    ModelSpec {
        id: "gpt-5.5",
        endpoint: EndpointKind::Responses,
    },
    ModelSpec {
        id: "claude-fable-5-1",
        endpoint: EndpointKind::Messages,
    },
    ModelSpec {
        id: "claude-opus-5-5",
        endpoint: EndpointKind::Messages,
    },
    ModelSpec {
        id: "claude-sonnet-5-5",
        endpoint: EndpointKind::Messages,
    },
    ModelSpec {
        id: "claude-haiku-5-5",
        endpoint: EndpointKind::Messages,
    },
    ModelSpec {
        id: "gemini-4-argon",
        endpoint: EndpointKind::Google,
    },
    ModelSpec {
        id: "gemini-3.8-flash",
        endpoint: EndpointKind::Google,
    },
    ModelSpec {
        id: "gemini-3.7-flash",
        endpoint: EndpointKind::Google,
    },
    ModelSpec {
        id: "gemini-3.6-flash",
        endpoint: EndpointKind::Google,
    },
    ModelSpec {
        id: "gemini-3.5-flash",
        endpoint: EndpointKind::Google,
    },
    ModelSpec {
        id: "gemini-3.5-flash-lite",
        endpoint: EndpointKind::Google,
    },
    ModelSpec {
        id: "gemini-3.1-pro",
        endpoint: EndpointKind::Google,
    },
    ModelSpec {
        id: "gemini-3-flash",
        endpoint: EndpointKind::Google,
    },
    ModelSpec {
        id: "grok-4.7",
        endpoint: EndpointKind::Responses,
    },
    ModelSpec {
        id: "grok-4.6",
        endpoint: EndpointKind::Responses,
    },
    ModelSpec {
        id: "grok-4.5",
        endpoint: EndpointKind::Responses,
    },
    ModelSpec {
        id: "qwen3.8-max",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "qwen3.8-flash",
        endpoint: EndpointKind::Messages,
    },
    ModelSpec {
        id: "qwen3.7-max",
        endpoint: EndpointKind::Messages,
    },
    ModelSpec {
        id: "qwen3.7-plus",
        endpoint: EndpointKind::Messages,
    },
    ModelSpec {
        id: "deepseek-v4.1-flash",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "deepseek-v4-pro",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "minimax-m3",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "glm-5.3-flash",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "glm-5.3",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "kimi-k3",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "kimi-k2.7-code",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "mistral-large-4",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "big-pickle",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "glm-5.3-flash-free",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "deepseek-v4.1-flash-free",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "space-bunny-free",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "beam-free",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "ll-alpha-free",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "longcat-2.5-preview-free",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "exo-free",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "mimo-v2.6-flash-free",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "mimo-v2.5-free",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "nemotron-3-ultra-free",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "nemotron-3.5-lightning-free",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "nemotron-3.5-super-vl-free",
        endpoint: EndpointKind::ChatCompletions,
    },
];

/// A model ID resolved to the wire protocol used for the upstream request.
/// Known catalog entries resolve to their registered endpoint; unknown
/// `opencode-go/` IDs resolve with an inferred endpoint so the request still
/// reaches OpenCode Go, which reports genuinely unknown IDs itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModel {
    pub id: String,
    pub endpoint: EndpointKind,
}

pub fn resolve(raw: &str) -> Option<ResolvedModel> {
    let prefixed = raw.strip_prefix(MODEL_PREFIX);
    let id = prefixed.unwrap_or(raw);
    if id.is_empty() {
        return None;
    }
    if let Some(known) = MODELS.iter().find(|model| model.id == id) {
        return Some(ResolvedModel {
            id: id.to_string(),
            endpoint: known.endpoint,
        });
    }
    if prefixed.is_some()
        || super::super::model_catalog::load("opencode")
            .is_some_and(|saved| saved.iter().any(|m| m == id))
    {
        return Some(ResolvedModel {
            id: id.to_string(),
            endpoint: infer_endpoint(id),
        });
    }
    None
}

/// Wire-protocol fallback for model IDs outside the registered catalog.
/// Mirrors the official endpoint table: minimax and qwen models speak the
/// Anthropic messages protocol, grok/gpt/muse-spark models speak Responses,
/// and everything else speaks OpenAI chat completions.
/// `scripts/refresh-opencode-models.py` applies the same rules when it
/// registers new catalog entries, so keep the two in sync.
pub fn infer_endpoint(id: &str) -> EndpointKind {
    if id.starts_with("gemini-") {
        EndpointKind::Google
    } else if id.starts_with("minimax-") || id.starts_with("qwen") || id.starts_with("claude-") {
        EndpointKind::Messages
    } else if id.starts_with("grok-") || id.starts_with("gpt-") || id.starts_with("muse-spark-") {
        EndpointKind::Responses
    } else {
        EndpointKind::ChatCompletions
    }
}

pub fn is_zen_free_model(id: &str) -> bool {
    id == "big-pickle" || id.ends_with("-free")
}

pub fn resolve_zen(raw: &str) -> Option<ResolvedModel> {
    let prefixed = raw.strip_prefix(ZEN_MODEL_PREFIX);
    let id = prefixed?;
    if id.is_empty() {
        return None;
    }
    if let Some(known) = ZEN_MODELS.iter().find(|model| model.id == id) {
        return Some(ResolvedModel {
            id: id.to_string(),
            endpoint: known.endpoint,
        });
    }
    Some(ResolvedModel {
        id: id.to_string(),
        endpoint: infer_endpoint(id),
    })
}

pub fn advertised_zen_models() -> Vec<String> {
    let ids = super::super::model_catalog::load("opencode-zen").unwrap_or_else(|| {
        ZEN_MODELS
            .iter()
            .map(|model| model.id.to_string())
            .collect()
    });
    ids.into_iter()
        .map(|id| format!("{ZEN_MODEL_PREFIX}{id}"))
        .collect()
}

pub fn advertised_models() -> Vec<String> {
    // The registry keeps conflicting bare IDs with their native provider.
    let ids = super::super::model_catalog::load("opencode")
        .unwrap_or_else(|| MODELS.iter().map(|model| model.id.to_string()).collect());
    ids.into_iter()
        .flat_map(|id| [format!("{MODEL_PREFIX}{id}"), id])
        .collect()
}

pub fn refresh_models(zen: bool) -> anyhow::Result<Vec<String>> {
    let (provider, base_url, api_key) = if zen {
        (
            "opencode-zen",
            crate::config::opencode_zen_base_url(),
            crate::config::opencode_zen_api_key(),
        )
    } else {
        (
            "opencode",
            crate::config::opencode_base_url(),
            crate::accounts::saved_api_key("opencode", None)
                .or_else(crate::config::opencode_api_key),
        )
    };
    let ids = super::super::model_catalog::fetch_openai(
        &base_url,
        api_key.as_deref(),
        std::time::Duration::from_secs(30),
    )?;
    super::super::model_catalog::save(provider, &ids)?;
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_catalog_is_partitioned_by_wire_protocol() {
        let chat = MODELS
            .iter()
            .filter(|model| model.endpoint == EndpointKind::ChatCompletions)
            .count();
        let messages = MODELS
            .iter()
            .filter(|model| model.endpoint == EndpointKind::Messages)
            .count();
        let responses = MODELS
            .iter()
            .filter(|model| model.endpoint == EndpointKind::Responses)
            .count();
        let google = MODELS
            .iter()
            .filter(|model| model.endpoint == EndpointKind::Google)
            .count();
        // Partition counts are derived, not hardcoded, so the refresh script
        // can add catalog entries without touching this test.
        assert_eq!(chat + messages + responses + google, MODELS.len());
        assert!(chat > messages);
        assert!(messages > 0);
        assert!(responses > 0);
        // Every registered entry resolves to its own endpoint, including the
        // grok-4.5 override that differs from the family fallback.
        for model in MODELS {
            let resolved = resolve(model.id).expect("registered model");
            assert_eq!(resolved.id, model.id);
            assert_eq!(resolved.endpoint, model.endpoint);
        }
        assert_eq!(
            resolve("grok-4.5").expect("override").endpoint,
            EndpointKind::ChatCompletions
        );
    }

    #[test]
    fn refreshed_models_resolve_and_are_advertised() {
        let advertised = advertised_models();
        for (id, endpoint) in [
            ("grok-4.7", EndpointKind::Responses),
            ("gpt-6-luna", EndpointKind::Responses),
            ("glm-5.3-flash", EndpointKind::ChatCompletions),
            ("glm-5.3", EndpointKind::ChatCompletions),
            ("glm-5", EndpointKind::ChatCompletions),
            ("longcat-2.0", EndpointKind::ChatCompletions),
            ("longcat-2.5-preview-free", EndpointKind::ChatCompletions),
            ("kimi-k2.5", EndpointKind::ChatCompletions),
            ("deepseek-flash", EndpointKind::ChatCompletions),
            ("deepseek-v4.1-flash", EndpointKind::ChatCompletions),
            (
                "deepseek-v4-flash-vision-exp",
                EndpointKind::ChatCompletions,
            ),
            ("mimo-v2-pro", EndpointKind::ChatCompletions),
            ("mimo-v2-omni", EndpointKind::ChatCompletions),
            ("mimo-v2.6-flash", EndpointKind::ChatCompletions),
            ("mimo-v2.6-pro", EndpointKind::ChatCompletions),
            ("qwen3.8-max", EndpointKind::Messages),
            ("qwen3.8-flash", EndpointKind::Messages),
            ("qwen3.5-plus", EndpointKind::Messages),
            ("hy4-preview", EndpointKind::ChatCompletions),
            ("hy3-preview", EndpointKind::ChatCompletions),
            ("muse-spark-1.3-contributor", EndpointKind::Responses),
            ("muse-spark-1.2-contributor", EndpointKind::Responses),
            ("omen-alpha", EndpointKind::ChatCompletions),
            ("space-bunny-free", EndpointKind::ChatCompletions),
        ] {
            let qualified = format!("{MODEL_PREFIX}{id}");
            assert_eq!(
                resolve(id).expect("registered bare model").endpoint,
                endpoint
            );
            assert_eq!(
                resolve(&qualified)
                    .expect("registered provider-qualified model")
                    .endpoint,
                endpoint
            );
            assert!(advertised.iter().any(|model| model == id));
            assert!(advertised.contains(&qualified));
        }
    }

    #[test]
    fn canonical_prefix_resolves_and_unknown_models_do_not() {
        let spec = resolve("opencode-go/minimax-m3").expect("known model");
        assert_eq!(spec.id, "minimax-m3");
        assert_eq!(spec.endpoint, EndpointKind::Messages);
        assert_eq!(
            resolve("qwen3.7-plus").unwrap().endpoint,
            EndpointKind::Messages
        );
        // Bare unknown IDs stay unknown: without the provider prefix the
        // proxy cannot know they belong to OpenCode Go.
        assert!(resolve("not-a-model").is_none());
        assert!(resolve("").is_none());
        assert!(resolve("opencode-go/").is_none());
    }

    #[test]
    fn unknown_prefixed_models_resolve_with_family_fallback() {
        // New catalog additions keep working before the refresh script
        // registers them; OpenCode Go itself reports IDs it never heard of.
        for (id, endpoint) in [
            ("some-new-chat-model", EndpointKind::ChatCompletions),
            ("minimax-next", EndpointKind::Messages),
            ("qwen-next-plus", EndpointKind::Messages),
            ("grok-next", EndpointKind::Responses),
            ("gpt-next-luna", EndpointKind::Responses),
            ("muse-spark-next", EndpointKind::Responses),
            ("mimo-next-pro", EndpointKind::ChatCompletions),
            ("deepseek-next-flash", EndpointKind::ChatCompletions),
        ] {
            let qualified = format!("{MODEL_PREFIX}{id}");
            let resolved = resolve(&qualified).expect("prefixed model");
            assert_eq!(resolved.id, id);
            assert_eq!(resolved.endpoint, endpoint, "{id}");
            // The same inference backs genuinely new families.
            assert_eq!(infer_endpoint(id), endpoint, "{id}");
        }
    }

    #[test]
    fn conflicting_provider_ids_are_advertised_both_ways() {
        // The registry filters bare IDs claimed by other providers, so the
        // advertisement itself carries both forms for every catalog entry and
        // needs no exclusion table. See `Registry::supported_models_for`.
        let models = advertised_models();
        for id in [
            "gpt-6-luna",
            "gpt-5.6-luna",
            "grok-4.7",
            "grok-4.6",
            "grok-4.5",
            "kimi-k3",
            "kimi-k2.6",
        ] {
            assert!(resolve(id).is_some());
            assert!(models.iter().any(|model| model == id));
            assert!(
                models
                    .iter()
                    .any(|model| model == &format!("opencode-go/{id}"))
            );
        }
    }
}
