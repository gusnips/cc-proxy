use crate::{
    anthropic::{json_error, schema::MessagesRequest},
    config::AliasProvider,
    provider::{CliHandlers, Provider, RequestContext},
};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use axum::{http::StatusCode, response::Response};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

pub const ANTHROPIC_STYLE_ALIASES: &[&str] = &[
    "haiku",
    "claude-haiku-4-5",
    "claude-haiku-4-5-20251001",
    "sonnet",
    "claude-sonnet-4-6",
    "claude-sonnet-5",
    "opus",
    "claude-opus-4-7",
    "claude-opus-4-8",
    "claude-opus-5",
    "claude-opus-5-5",
    "fable",
    "claude-fable-5",
];

pub const CURSOR_PREFIXES: &[&str] = &["cursor:", "cursor-plan:", "cursor-ask:"];

const CURSOR_LEGACY_MODELS: &[&str] = &[
    "cursor",
    "cursor-agent",
    "cursor-composer",
    "cursor-composer-fast",
    "cursor-plan",
    "cursor-ask",
    "composer-2.5",
    "composer-2.5-fast",
];

pub(crate) const CODEX_MODELS: &[&str] = &[
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
];

pub(crate) const KIMI_MODELS: &[&str] = &["kimi-for-coding", "kimi-k2.6", "kimi-k3", "k2.6", "k3"];
pub(crate) const GROK_MODELS: &[&str] =
    &["grok-composer-2.5-fast", "grok-4.5", "grok-4.6", "grok-4.7"];

pub struct Registry {
    alias_provider: AliasProvider,
    models: BTreeMap<String, Vec<String>>,
    handlers: BTreeMap<String, Arc<dyn Provider>>,
}

impl Registry {
    pub fn new(alias_provider: AliasProvider) -> Self {
        let mut models: BTreeMap<String, Vec<String>> = BTreeMap::new();
        models.insert("codex".into(), expand_codex_models());
        models.insert(
            "kimi".into(),
            KIMI_MODELS.iter().map(|m| (*m).to_string()).collect(),
        );
        models.insert("cursor".into(), build_cursor_models());
        models.insert(
            "grok".into(),
            GROK_MODELS
                .iter()
                .map(|model| (*model).to_string())
                .collect(),
        );
        models.insert(
            "opencode".into(),
            crate::providers::opencode::advertised_models(),
        );

        let mut handlers = BTreeMap::new();
        for (name, entries) in &models {
            let handler: Arc<dyn Provider> = match name.as_str() {
                "codex" => Arc::new(crate::providers::codex::CodexProvider::new()),
                "kimi" => Arc::new(crate::providers::kimi::KimiProvider::new()),
                "cursor" => Arc::new(crate::providers::cursor::CursorProvider::new()),
                "grok" => Arc::new(crate::providers::grok::GrokProvider::new()),
                "opencode" => Arc::new(crate::providers::opencode::OpenCodeProvider::new()),
                _ => Arc::new(PlaceholderProvider::new(name, entries.clone())),
            };
            handlers.insert(name.clone(), handler);
        }

        Self {
            alias_provider,
            models,
            handlers,
        }
    }

    pub fn with_default_alias() -> Self {
        Self::new(crate::config::alias_provider())
    }

    pub fn from_providers(
        alias_provider: AliasProvider,
        providers: impl IntoIterator<Item = Arc<dyn Provider>>,
    ) -> Self {
        let mut models = BTreeMap::new();
        let mut handlers = BTreeMap::new();
        for provider in providers {
            let name = provider.name().to_string();
            models.insert(name.clone(), provider.supported_models());
            handlers.insert(name, provider);
        }
        Self {
            alias_provider,
            models,
            handlers,
        }
    }

    pub fn list_provider_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.handlers.keys().cloned().collect();
        names.sort_unstable();
        names
    }

    pub fn provider(&self, name: &str) -> Option<Arc<dyn Provider>> {
        self.handlers.get(name).cloned()
    }

    pub fn supported_models_for(&self, provider: &str) -> Vec<String> {
        let mut models = self.models.get(provider).cloned().unwrap_or_default();
        if provider == "opencode" {
            // Bare IDs owned by another provider's catalog stay with that
            // provider; only the `opencode-go/` qualified form selects the
            // OpenCode Go version. Filtering here instead of in the
            // advertisement keeps the conflict policy automatic across
            // catalog refreshes.
            let claimed: HashSet<&str> = self
                .models
                .iter()
                .filter(|(name, _)| name.as_str() != "opencode")
                .flat_map(|(_, entries)| entries.iter().map(String::as_str))
                .collect();
            models.retain(|model| {
                model.starts_with(crate::providers::opencode::model::MODEL_PREFIX)
                    || !claimed.contains(model.as_str())
            });
        }
        if provider == self.alias_provider.as_str() {
            for alias in ANTHROPIC_STYLE_ALIASES {
                if !models.iter().any(|value| value == alias) {
                    models.push((*alias).to_string());
                }
            }
        }
        models.sort_unstable();
        models
    }

    pub fn all_supported_models(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for provider in self.handlers.keys() {
            for model in self.supported_models_for(provider) {
                out.push((model, provider.clone()));
            }
        }
        out
    }

    pub fn grouped_models(&self) -> BTreeMap<String, Vec<String>> {
        let mut out = BTreeMap::new();
        for provider in self.handlers.keys() {
            out.insert(provider.clone(), self.supported_models_for(provider));
        }
        out
    }

    pub fn provider_for_model(
        &self,
        raw_model: &str,
        session_affinity: Option<&AliasProvider>,
    ) -> Option<Arc<dyn Provider>> {
        let normalized = normalize_incoming_model(raw_model);
        // Any `opencode-go/` ID routes to OpenCode Go, registered or not.
        // Unknown IDs are forwarded with an inferred wire protocol and
        // OpenCode Go reports the ones it never heard of, so a catalog
        // refresh on their side never breaks routing on ours.
        if normalized.starts_with(crate::providers::opencode::model::MODEL_PREFIX) {
            return self.handlers.get("opencode").cloned();
        }
        if is_anthropic_alias(&normalized) {
            let target = session_affinity.unwrap_or(&self.alias_provider);
            return self.handlers.get(target.as_str()).cloned();
        }
        if is_cursor_model(&normalized) {
            return self.handlers.get("cursor").cloned();
        }

        // Explicit priority, not map order: a bare ID owned by several
        // catalogs stays with its native provider. The `opencode-go/`
        // qualified form above is the only way to select the OpenCode Go
        // version of a conflicting ID.
        for name in ["codex", "kimi", "cursor", "grok", "opencode"] {
            if self
                .models
                .get(name)
                .is_some_and(|entries| entries.iter().any(|candidate| candidate == &normalized))
            {
                return self.handlers.get(name).cloned();
            }
        }

        None
    }

    pub fn unknown_model_message(&self) -> String {
        let mut parts = Vec::new();
        for (provider, models) in self.grouped_models() {
            let mut models = models;
            models.sort_unstable();
            parts.push(format!("{}: {}", provider, models.join(", ")));
        }
        format!("Supported: {}.", parts.join("; "))
    }
}

pub fn normalize_incoming_model(model: &str) -> String {
    let suffix = "[1m]";
    if model.len() >= suffix.len() && model.to_ascii_lowercase().ends_with(suffix) {
        return model[..model.len() - suffix.len()].to_string();
    }
    model.to_string()
}

pub fn is_anthropic_alias(model: &str) -> bool {
    ANTHROPIC_STYLE_ALIASES.contains(&model)
}

pub fn is_cursor_model(model: &str) -> bool {
    if CURSOR_LEGACY_MODELS.contains(&model) {
        return true;
    }

    CURSOR_PREFIXES
        .iter()
        .any(|prefix| model.starts_with(prefix))
}

struct PlaceholderProvider {
    name: &'static str,
    models: Vec<String>,
}

impl PlaceholderProvider {
    fn new(name: &str, models: Vec<String>) -> Self {
        let name = match name {
            "codex" => "codex",
            "kimi" => "kimi",
            "cursor" => "cursor",
            "grok" => "grok",
            _ => "codex",
        };
        Self { name, models }
    }
}

#[async_trait]
impl Provider for PlaceholderProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn supported_models(&self) -> Vec<String> {
        self.models.clone()
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        match self.name {
            "codex" => &CODEX_CLI,
            "kimi" => &KIMI_CLI,
            "cursor" => &CURSOR_CLI,
            "grok" => &GROK_CLI,
            _ => &CODEX_CLI,
        }
    }

    async fn handle_messages(&self, _body: MessagesRequest, ctx: RequestContext) -> Response {
        placeholder_provider_response("messages", &ctx.provider)
    }

    async fn handle_count_tokens(&self, _body: MessagesRequest, ctx: RequestContext) -> Response {
        placeholder_provider_response("count_tokens", &ctx.provider)
    }
}

fn placeholder_provider_response(route: &str, provider: &str) -> Response {
    let _ = route;
    json_error(
        StatusCode::NOT_IMPLEMENTED,
        "unsupported_provider_error",
        format!("provider '{}' is not yet implemented", provider),
    )
}

#[derive(Clone, Copy)]
struct PlaceholderCli {
    provider: &'static str,
}

impl CliHandlers for PlaceholderCli {
    fn login(&self) -> Result<()> {
        Err(anyhow!("{}: browser login not supported", self.provider))
    }

    fn device(&self) -> Result<()> {
        Err(anyhow!("{}: device login not supported", self.provider))
    }

    fn status(&self) -> Result<()> {
        use serde_json::Value;
        let path = crate::paths::provider_auth_file(self.provider);
        let legacy = crate::paths::provider_legacy_auth_file(self.provider);
        if crate::auth::load_auth_file_with_legacy::<Value>(&path, &legacy).is_some() {
            Ok(())
        } else {
            Err(anyhow!("Not authenticated"))
        }
    }

    fn logout(&self) -> Result<()> {
        let path = crate::paths::provider_auth_file(self.provider);
        let legacy = crate::paths::provider_legacy_auth_file(self.provider);
        let _ = crate::auth::delete_auth_file(&path, &legacy);
        Ok(())
    }
}

const CODEX_CLI: PlaceholderCli = PlaceholderCli { provider: "codex" };
const KIMI_CLI: PlaceholderCli = PlaceholderCli { provider: "kimi" };
const CURSOR_CLI: PlaceholderCli = PlaceholderCli { provider: "cursor" };
const GROK_CLI: PlaceholderCli = PlaceholderCli { provider: "grok" };
fn expand_codex_models() -> Vec<String> {
    let mut set = HashSet::new();
    let mut out = Vec::new();
    for model in CODEX_MODELS {
        if set.insert((*model).to_string()) {
            out.push((*model).to_string());
        }
        let fast = format!("{model}-fast");
        if set.insert(fast.clone()) {
            out.push(fast);
        }
    }
    out.sort_unstable();
    out
}

fn build_cursor_models() -> Vec<String> {
    let mut out: Vec<String> = CURSOR_LEGACY_MODELS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    out.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_model_trims_hint() {
        assert_eq!(normalize_incoming_model("gpt-5.4-fast[1m]"), "gpt-5.4-fast");
        assert_eq!(normalize_incoming_model("gpt-5.4-fast"), "gpt-5.4-fast");
    }

    #[test]
    fn alias_routes_to_configured_provider() {
        let registry = Registry::new(AliasProvider::Kimi);
        let p = registry.provider_for_model("haiku", None);
        assert!(p.is_some());
        assert_eq!(p.expect("provider").name(), "kimi");
    }

    #[test]
    fn opus_4_8_routes_to_configured_provider() {
        let registry = Registry::new(AliasProvider::Codex);
        let p = registry.provider_for_model("claude-opus-4-8", None);
        assert!(p.is_some());
        assert_eq!(p.expect("provider").name(), "codex");
    }

    #[test]
    fn claude_5_aliases_route_to_configured_provider() {
        let registry = Registry::new(AliasProvider::Codex);
        for model in [
            "claude-sonnet-5",
            "claude-opus-5",
            "claude-opus-5-5",
            "fable",
            "claude-fable-5",
        ] {
            let p = registry.provider_for_model(model, None);
            assert!(p.is_some(), "{model} should route to a provider");
            assert_eq!(p.expect("provider").name(), "codex");
        }
    }

    #[test]
    fn cursor_prefix_routes() {
        let registry = Registry::new(AliasProvider::Codex);
        assert_eq!(
            registry
                .provider_for_model("cursor:gpt-5.5", None)
                .unwrap()
                .name(),
            "cursor"
        );
        assert_eq!(
            registry
                .provider_for_model("cursor-plan:gpt-5.5", None)
                .unwrap()
                .name(),
            "cursor"
        );
        assert_eq!(
            registry
                .provider_for_model("cursor-ask:gpt-5.5", None)
                .unwrap()
                .name(),
            "cursor"
        );
    }

    #[test]
    fn grok_4_7_routes_to_grok() {
        let registry = Registry::new(AliasProvider::Codex);
        assert_eq!(
            registry
                .provider_for_model("grok-4.7", None)
                .unwrap()
                .name(),
            "grok"
        );
    }

    #[test]
    fn gpt_6_sol_and_luna_route_to_codex() {
        let registry = Registry::new(AliasProvider::Codex);
        for model in ["gpt-6-sol", "gpt-6-sol-fast", "gpt-6-luna"] {
            assert_eq!(
                registry.provider_for_model(model, None).unwrap().name(),
                "codex"
            );
        }
    }

    #[test]
    fn opencode_models_route_without_stealing_existing_provider_ids() {
        let registry = Registry::new(AliasProvider::Codex);
        assert_eq!(
            registry
                .provider_for_model("kimi-k2.7-code", None)
                .unwrap()
                .name(),
            "opencode"
        );
        assert_eq!(
            registry
                .provider_for_model("opencode-go/kimi-k2.6", None)
                .unwrap()
                .name(),
            "opencode"
        );
        // Refreshed catalog entries behave the same way.
        assert_eq!(
            registry
                .provider_for_model("deepseek-v4.1-flash", None)
                .unwrap()
                .name(),
            "opencode"
        );
        assert_eq!(
            registry
                .provider_for_model("space-bunny-free", None)
                .unwrap()
                .name(),
            "opencode"
        );
        assert_eq!(
            registry
                .provider_for_model("kimi-k2.6", None)
                .unwrap()
                .name(),
            "kimi"
        );
        for (model, owner) in [
            ("gpt-5.6-luna", "codex"),
            ("gpt-6-luna", "codex"),
            ("grok-4.5", "grok"),
            ("grok-4.6", "grok"),
            ("grok-4.7", "grok"),
            ("kimi-k3", "kimi"),
        ] {
            assert_eq!(
                registry.provider_for_model(model, None).unwrap().name(),
                owner
            );
            assert_eq!(
                registry
                    .provider_for_model(&format!("opencode-go/{model}"), None)
                    .unwrap()
                    .name(),
                "opencode"
            );
        }
    }

    #[test]
    fn opencode_prefix_routes_unknown_models_upstream() {
        // IDs OpenCode Go adds on their side keep routing here even before
        // the local catalog learns them; their API reports unknown IDs.
        let registry = Registry::new(AliasProvider::Codex);
        for model in [
            "opencode-go/some-future-model",
            "opencode-go/grok-5",
            "opencode-go/qwen-next-max",
            "opencode-go/minimax-next",
            "opencode-go/gpt-6-sol[1m]",
        ] {
            assert_eq!(
                registry.provider_for_model(model, None).unwrap().name(),
                "opencode",
                "{model}"
            );
        }
        // Bare unknown IDs still have no provider.
        assert!(registry.provider_for_model("some-future-model", None).is_none());
    }

    #[test]
    fn opencode_lists_only_unconflicted_bare_ids() {
        let registry = Registry::new(AliasProvider::Codex);
        let models = registry.supported_models_for("opencode");
        for id in [
            "gpt-5.6-luna",
            "gpt-6-luna",
            "grok-4.5",
            "grok-4.6",
            "grok-4.7",
            "kimi-k3",
            "kimi-k2.6",
        ] {
            assert!(
                !models.iter().any(|model| model == id),
                "bare {id} must stay with its native provider"
            );
            assert!(
                models
                    .iter()
                    .any(|model| model == &format!("opencode-go/{id}")),
                "qualified opencode-go/{id} must stay listed"
            );
        }
        for id in ["glm-5.2", "deepseek-v4.1-flash", "space-bunny-free"] {
            assert!(models.iter().any(|model| model == id));
            assert!(
                models
                    .iter()
                    .any(|model| model == &format!("opencode-go/{id}"))
            );
        }
    }
}
