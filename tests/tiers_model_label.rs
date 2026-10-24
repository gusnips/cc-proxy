mod common;

use cc_proxy::{Registry, config::Route, providers, tiers};
use common::{EnvGuard, env_lock};
use serde_json::json;

fn isolated_models(config: &tempfile::TempDir) -> Vec<EnvGuard> {
    std::fs::write(config.path().join("models_cache.json"), r#"{"models":[]}"#).unwrap();
    vec![
        EnvGuard::set("CCP_CONFIG_DIR", config.path()),
        EnvGuard::set("CODEX_HOME", config.path()),
        EnvGuard::set("HOME", config.path()),
        EnvGuard::unset("CCP_CODEX_MODEL"),
    ]
}

fn routed(provider: &str, model: &str) -> Route {
    Route {
        on: true,
        provider: Some(provider.into()),
        fallback: Vec::new(),
        tiers: [(
            provider.into(),
            tiers::TIERS.map(|tier| (tier.into(), model.into())).into(),
        )]
        .into(),
    }
}

#[test]
fn labels_show_the_final_models_for_all_seven_providers_without_changing_requests() {
    let _lock = env_lock();
    let config = tempfile::tempdir().unwrap();
    let _env = isolated_models(&config);
    let registry = Registry::new();
    for (provider, mapped, expected) in [
        ("codex", "gpt-6-astra-ultrafast", "codex/gpt-6-astra"),
        ("kimi", "kimi-k3", "kimi/k3"),
        ("cursor", "cursor-plan:gpt-6-sol-fast", "cursor/gpt-6-sol"),
        ("grok", "grok-4.7", "grok/grok-4.7"),
        (
            "copilot",
            "copilot/claude-opus-4.8",
            "copilot/claude-opus-4.8",
        ),
        ("glm", "glm-5.3", "glm/glm-5.3"),
        ("opencode", "opencode-go/grok-4.7", "opencode/grok-4.7"),
    ] {
        let route = routed(provider, mapped);
        let original_route = route.clone();
        for (requested, name) in [
            ("opus", "Opus"),
            ("claude-opus-5-5", "Opus"),
            ("claude-opus-99-7[1m]", "Opus"),
            ("claude-fable-5-1", "Fable"),
            ("claude-sonnet-5-5", "Sonnet"),
            ("haiku[1m]", "Haiku"),
        ] {
            let label = tiers::model_label(&registry, &route, requested).unwrap();
            assert_eq!(label.model, requested);
            assert_eq!(label.label, expected, "{provider}");
            assert!(
                label
                    .description
                    .contains(&format!("Answers {name} requests."))
            );
            assert!(label.description.contains(requested));
            assert_eq!(
                tiers::resolve(&registry, &route, "opus").unwrap(),
                mapped,
                "a display label must not replace the routing ID"
            );
            assert_eq!(route, original_route);
        }
        let requested = format!("{mapped}[1M]");
        let label = tiers::model_label(&registry, &route, &requested).unwrap();
        assert_eq!(label.model, requested);
        assert_eq!(label.label, expected);
        assert!(
            label
                .description
                .starts_with(&format!("Claude Code sends {requested}."))
        );
    }
}

#[test]
fn labels_follow_the_actual_handler_when_a_chosen_model_belongs_to_another_provider() {
    let _lock = env_lock();
    let config = tempfile::tempdir().unwrap();
    let _env = isolated_models(&config);
    let registry = Registry::new();
    for (mapped, expected) in [
        ("gpt-6-sol-fast", "codex/gpt-6-sol"),
        ("kimi-k2.6", "kimi/kimi-for-coding"),
        ("opencode-go/kimi-k2.6", "opencode/kimi-k2.6"),
        ("copilot/gpt-6-sol-fast", "copilot/gpt-6-sol-fast"),
    ] {
        let route = routed("opencode", mapped);
        let label = tiers::model_label(&registry, &route, "claude-opus").unwrap();
        assert_eq!(label.label, expected);
        assert_eq!(
            tiers::resolve(&registry, &route, "claude-opus").unwrap(),
            mapped
        );
    }
}

#[test]
fn cursor_labels_keep_defaults_truthful_and_distinguish_modes() {
    let _lock = env_lock();
    let config = tempfile::tempdir().unwrap();
    let _env = isolated_models(&config);
    let registry = Registry::new();
    for (mapped, upstream, mode) in [
        ("cursor-agent", "default", "agent"),
        ("cursor-plan", "default", "plan"),
        ("cursor-ask", "default", "ask"),
        ("cursor-composer-fast", "composer-2.5", "agent"),
    ] {
        let route = routed("cursor", mapped);
        let label = tiers::model_label(&registry, &route, "opus").unwrap();
        assert_eq!(label.label, format!("cursor/{upstream}"));
        assert!(label.description.contains(&format!("Cursor {mode} mode")));
        let wire = providers::cursor::model::resolve_cursor_model(mapped).unwrap();
        assert_eq!(wire.model_id, upstream);
        assert_eq!(tiers::resolve(&registry, &route, "opus").unwrap(), mapped);
    }
}

#[test]
fn codex_labels_follow_environment_and_file_overrides_and_reject_invalid_ones() {
    let _lock = env_lock();
    let config = tempfile::tempdir().unwrap();
    let _env = isolated_models(&config);
    let registry = Registry::new();
    let route = routed("codex", "gpt-6-sol-fast");
    let config_path = config.path().join("config.json");
    std::fs::write(
        &config_path,
        json!({"codex": {"model": "gpt-6-luna-fast"}}).to_string(),
    )
    .unwrap();
    assert_eq!(
        tiers::model_label(&registry, &route, "opus").unwrap().label,
        "codex/gpt-6-luna"
    );
    {
        let _override = EnvGuard::set("CCP_CODEX_MODEL", "gpt-6-astra-ultrafast");
        let label = tiers::model_label(&registry, &route, "claude-opus-5-5[1m]").unwrap();
        assert_eq!(label.model, "claude-opus-5-5[1m]");
        assert_eq!(label.label, "codex/gpt-6-astra");
        let wire =
            providers::codex::translate::model_allowlist::resolve_model_request("gpt-6-sol-fast");
        assert_eq!(wire.model, "gpt-6-astra");
        assert_eq!(
            wire.service_tier,
            Some(providers::codex::translate::request::ServiceTier::Ultrafast)
        );
        assert_eq!(
            tiers::resolve(&registry, &route, "opus").unwrap(),
            "gpt-6-sol-fast"
        );
        assert!(tiers::model_label(&registry, &route, "gpt-6-sol-fast-fast").is_none());
    }
    for invalid in ["not-a-model", "grok-4.7", "gpt-6-sol-fast-fast"] {
        let _override = EnvGuard::set("CCP_CODEX_MODEL", invalid);
        assert!(
            tiers::model_label(&registry, &route, "opus").is_none(),
            "{invalid}"
        );
    }
    std::fs::write(
        &config_path,
        json!({"codex": {"model": "not-a-model"}}).to_string(),
    )
    .unwrap();
    assert!(tiers::model_label(&registry, &route, "opus").is_none());
}

#[test]
fn labels_skip_unknown_unresolvable_and_disabled_routes() {
    let _lock = env_lock();
    let config = tempfile::tempdir().unwrap();
    let _env = isolated_models(&config);
    let registry = Registry::new();
    let route = routed("grok", "grok-4.7");
    for requested in [
        "unknown-model",
        "claude-sonnetwork-5-5",
        "copilot/",
        "opencode-go/",
        "cursor:",
        "gpt-6-sol-fast-fast",
    ] {
        assert!(
            tiers::model_label(&registry, &route, requested).is_none(),
            "{requested}"
        );
    }
    let no_models = Registry::from_providers([]);
    assert!(tiers::model_label(&no_models, &route, "sonnet").is_none());
    let unknown = routed("grok", "unknown-model");
    assert!(tiers::model_label(&registry, &unknown, "opus").is_none());
    let mut off = route.clone();
    off.on = false;
    for requested in ["opus", "grok-4.7", "copilot/gpt-5.5"] {
        assert!(tiers::model_label(&registry, &off, requested).is_none());
    }
}
