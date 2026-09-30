use std::collections::HashMap;

pub const KIMI_DEFAULT_MODEL: &str = "k3";

/// Wire model kept for the legacy `kimi-for-coding` ID and its K2.6 aliases.
pub const KIMI_LEGACY_MODEL: &str = "kimi-for-coding";

static ALIAS_TARGETS: once_cell::sync::Lazy<HashMap<&'static str, &'static str>> =
    once_cell::sync::Lazy::new(|| {
        let mut m = HashMap::new();
        m.insert("haiku", KIMI_DEFAULT_MODEL);
        m.insert("claude-haiku-4-5", KIMI_DEFAULT_MODEL);
        m.insert("claude-haiku-4-5-20251001", KIMI_DEFAULT_MODEL);
        m.insert("sonnet", KIMI_DEFAULT_MODEL);
        m.insert("claude-sonnet-4-6", KIMI_DEFAULT_MODEL);
        m.insert("claude-sonnet-5", KIMI_DEFAULT_MODEL);
        m.insert("opus", KIMI_DEFAULT_MODEL);
        m.insert("claude-opus-4-7", KIMI_DEFAULT_MODEL);
        m.insert("claude-opus-4-8", KIMI_DEFAULT_MODEL);
        m.insert("claude-opus-5", KIMI_DEFAULT_MODEL);
        m.insert("fable", KIMI_DEFAULT_MODEL);
        m.insert("claude-fable-5", KIMI_DEFAULT_MODEL);
        m.insert("kimi-for-coding", KIMI_LEGACY_MODEL);
        m.insert("kimi-k2.6", KIMI_LEGACY_MODEL);
        m.insert("k2.6", KIMI_LEGACY_MODEL);
        m.insert("kimi-k3", "k3");
        m.insert("k3", "k3");
        m
    });

const ALLOWED_MODELS: &[&str] = &[KIMI_LEGACY_MODEL, KIMI_DEFAULT_MODEL];

pub fn resolve_model(model: &str) -> String {
    if let Some(target) = ALIAS_TARGETS.get(model).copied() {
        return target.to_string();
    }
    // Unlisted `kimi-*`/`k2-*` IDs forward verbatim instead of collapsing
    // onto the default: silently serving k3 when the caller asked for a
    // newer model would bill and behave as the wrong model. Kimi reports
    // the IDs it never heard of.
    if model.starts_with("kimi-") || model.starts_with("k2-") {
        return model.to_string();
    }
    KIMI_DEFAULT_MODEL.to_string()
}

pub fn is_k3(model: &str) -> bool {
    model == KIMI_DEFAULT_MODEL
}

pub fn assert_allowed_model(model: &str) -> Result<(), ModelNotAllowedError> {
    if ALLOWED_MODELS.contains(&model) {
        Ok(())
    } else {
        Err(ModelNotAllowedError {
            model: model.to_string(),
        })
    }
}

/// Routing gate: everything `assert_allowed_model` accepts, plus unlisted
/// `kimi-*`/`k2-*` IDs, which forward verbatim on the legacy (non-k3)
/// translation path. Same pattern as `opencode-go/` IDs for OpenCode Go.
pub fn assert_routable_model(model: &str) -> Result<(), ModelNotAllowedError> {
    if ALLOWED_MODELS.contains(&model) || model.starts_with("kimi-") || model.starts_with("k2-") {
        Ok(())
    } else {
        Err(ModelNotAllowedError {
            model: model.to_string(),
        })
    }
}

#[derive(Debug)]
pub struct ModelNotAllowedError {
    pub model: String,
}

impl std::fmt::Display for ModelNotAllowedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Model not allowed: {}", self.model)
    }
}

impl std::error::Error for ModelNotAllowedError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_haiku_to_default() {
        assert_eq!(resolve_model("haiku"), KIMI_DEFAULT_MODEL);
    }

    #[test]
    fn resolve_opus_4_8_to_default() {
        assert_eq!(resolve_model("claude-opus-4-8"), KIMI_DEFAULT_MODEL);
    }

    #[test]
    fn resolve_claude_5_aliases_to_default() {
        for model in [
            "claude-sonnet-5",
            "claude-opus-5",
            "fable",
            "claude-fable-5",
        ] {
            assert_eq!(resolve_model(model), KIMI_DEFAULT_MODEL);
        }
    }

    #[test]
    fn resolve_unknown_to_default() {
        assert_eq!(resolve_model("unknown-model"), KIMI_DEFAULT_MODEL);
    }

    #[test]
    fn resolve_kimi_for_coding() {
        assert_eq!(resolve_model("kimi-for-coding"), KIMI_LEGACY_MODEL);
    }

    #[test]
    fn resolve_kimi_k3_to_k3() {
        assert_eq!(resolve_model("kimi-k3"), "k3");
    }

    #[test]
    fn resolve_k3_to_k3() {
        assert_eq!(resolve_model("k3"), "k3");
    }

    #[test]
    fn k3_detected() {
        assert!(is_k3("k3"));
        assert!(!is_k3("kimi-for-coding"));
    }

    #[test]
    fn assert_allowed_accepts_default() {
        assert!(assert_allowed_model(KIMI_DEFAULT_MODEL).is_ok());
    }

    #[test]
    fn assert_allowed_accepts_k3() {
        assert!(assert_allowed_model("k3").is_ok());
    }

    #[test]
    fn assert_allowed_rejects_other() {
        assert!(assert_allowed_model("kimi-k2.6").is_err());
    }

    #[test]
    fn unlisted_kimi_ids_forward_verbatim_instead_of_collapsing_to_default() {
        assert_eq!(resolve_model("kimi-k9"), "kimi-k9");
        assert_eq!(resolve_model("k2-9"), "k2-9");
        assert_eq!(resolve_model("something-else"), KIMI_DEFAULT_MODEL);
    }

    #[test]
    fn routable_accepts_unlisted_kimi_ids() {
        assert!(assert_routable_model("kimi-k9").is_ok());
        assert!(assert_routable_model("something-else").is_err());
    }
}
