pub fn resolve_model(model: &str) -> String {
    model.to_string()
}

pub fn assert_allowed_model(model: &str) -> anyhow::Result<()> {
    if matches!(
        model,
        "grok-composer-2.5-fast" | "grok-4.5" | "grok-4.6" | "grok-4.7"
    ) {
        Ok(())
    } else {
        anyhow::bail!("unsupported Grok model")
    }
}

/// Routing gate: everything `assert_allowed_model` accepts, plus unlisted
/// `grok-*` IDs, which forward verbatim — the Grok translation is
/// model-agnostic, and Grok reports the IDs it never heard of. Same pattern
/// as `opencode-go/` IDs for OpenCode Go.
pub fn assert_routable_model(model: &str) -> anyhow::Result<()> {
    if model.starts_with("grok-") {
        Ok(())
    } else {
        assert_allowed_model(model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grok_4_7_is_allowed() {
        assert!(assert_allowed_model("grok-4.7").is_ok());
    }

    #[test]
    fn routable_accepts_unlisted_grok_ids() {
        assert!(assert_routable_model("grok-9").is_ok());
        assert!(assert_routable_model("something-else").is_err());
    }
}
