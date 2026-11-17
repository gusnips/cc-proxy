//! Codex model metadata fetched by the proxy, with the Codex CLI's local cache
//! as a fallback until the first successful cloud refresh.
//!
//! Both sources use the same model schema. Keep lane and visibility metadata,
//! not just IDs, so new models use the right Responses API. Malformed entries
//! are skipped individually. A failed refresh leaves the saved catalog alone.

use std::{
    fs,
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{config, providers::model_catalog};

use super::super::auth::{
    constants::{CODEX_API_ENDPOINT, ORIGINATOR},
    manager::CodexAuthManager,
    token_store,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogModel {
    pub slug: String,
    /// `use_responses_lite` — the model exists behind the Responses Lite lane.
    pub use_responses_lite: bool,
    /// `supported_in_api` — false for TUI-only models the backend rejects.
    pub supported_in_api: bool,
    /// `visibility == "list"` — shown in the Codex picker (hidden models are
    /// still accepted when requested explicitly, just not advertised).
    pub listed: bool,
}

#[derive(Deserialize)]
struct CacheFile {
    models: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct CacheModel {
    slug: String,
    #[serde(default)]
    use_responses_lite: bool,
    #[serde(default = "default_true")]
    supported_in_api: bool,
    #[serde(default = "default_visibility")]
    visibility: String,
}

fn default_true() -> bool {
    true
}

fn default_visibility() -> String {
    "list".to_string()
}

struct CacheState {
    key: Option<(PathBuf, SystemTime, u64)>,
    models: Vec<CatalogModel>,
}

static CACHE: OnceLock<Mutex<CacheState>> = OnceLock::new();

pub fn catalog_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CODEX_HOME") {
        return Some(PathBuf::from(dir).join("models_cache.json"));
    }
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".codex").join("models_cache.json"))
}

/// Parses the cache file bytes. Returns `None` only if the top level is not
/// the expected object; individual malformed models are skipped.
pub fn parse_catalog(bytes: &[u8]) -> Option<Vec<CatalogModel>> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    if !value.is_object() {
        return None;
    }
    let file: CacheFile = serde_json::from_value(value).ok()?;
    let models = file
        .models
        .into_iter()
        .filter_map(|value| serde_json::from_value::<CacheModel>(value).ok())
        .filter(|model| !model.slug.is_empty())
        .map(|model| CatalogModel {
            listed: model.visibility == "list",
            slug: model.slug,
            use_responses_lite: model.use_responses_lite,
            supported_in_api: model.supported_in_api,
        })
        .collect();
    Some(models)
}

/// Refreshes the list using cc-proxy's saved Codex sign-in, not the Codex CLI.
pub(crate) fn refresh_models() -> Result<Vec<String>> {
    refresh_and_save().context(
        "Could not refresh Codex models. The saved list has not changed. Retry with `cc-proxy models refresh codex`.",
    )
}

fn refresh_and_save() -> Result<Vec<String>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let bytes = runtime.block_on(async {
        let auth = CodexAuthManager::new(token_store::file_store()?)
            .get_auth()
            .await
            .context("Check your Codex sign-in with `cc-proxy auth status codex`.")?;
        let base_url = config::codex_base_url(CODEX_API_ENDPOINT);
        let mut url = reqwest::Url::parse(&base_url).context("Invalid Codex address.")?;
        let path = url.path().trim_end_matches('/');
        let root = path.strip_suffix("/responses").unwrap_or(path);
        url.set_path(&format!("{root}/models"));
        // Codex expects a Codex protocol version, not the cc-proxy version.
        // This is the released Codex version used to verify the wire format.
        const CLIENT_VERSION: &str = "0.169.0";
        let headers = super::super::client::build_codex_read_headers(&auth)
            .map_err(|error| anyhow::anyhow!(error.message))?;
        let response = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()?
            .get(url)
            .query(&[("client_version", CLIENT_VERSION)])
            .headers(headers)
            .header("version", CLIENT_VERSION)
            .header("originator", config::codex_originator(ORIGINATOR))
            .send()
            .await
            .context("Could not reach Codex. Check your connection.")?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            bail!("Codex rejected the saved sign-in. Run `cc-proxy auth login codex`.");
        }
        if !status.is_success() {
            bail!("Codex returned HTTP {status} for the model list.");
        }
        response
            .bytes()
            .await
            .context("Could not read the Codex model list.")
    })?;
    let mut ids: Vec<_> = parse_catalog(&bytes)
        .context("Codex did not return a valid model list.")?
        .into_iter()
        .filter(|model| model.listed && model.supported_in_api)
        .map(|model| model.slug)
        .collect();
    if ids.is_empty() {
        bail!("Codex returned no models cc-proxy can list.");
    }
    ids.sort_unstable();
    ids.dedup();
    crate::fsutil::write_atomic(&model_catalog::cache_file("codex"), &bytes)
        .context("Could not save the Codex model list.")?;
    Ok(ids)
}

/// Current catalog snapshot (cached per path, mtime and length).
pub fn catalog_models() -> Vec<CatalogModel> {
    let mutex = CACHE.get_or_init(|| {
        Mutex::new(CacheState {
            key: None,
            models: Vec::new(),
        })
    });
    let mut state = mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    for path in std::iter::once(model_catalog::cache_file("codex")).chain(catalog_path()) {
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let key = (path, modified, meta.len());
        if state.key.as_ref() == Some(&key) {
            break;
        }
        if let Some(models) = fs::read(&key.0)
            .ok()
            .and_then(|bytes| parse_catalog(&bytes))
        {
            state.key = Some(key);
            state.models = models;
            break;
        }
    }
    state.models.clone()
}

pub fn catalog_model(slug: &str) -> Option<CatalogModel> {
    catalog_models()
        .into_iter()
        .find(|model| model.slug == slug)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
        "fetched_at": "2026-09-04T20:07:48Z",
        "models": [
            {"slug": "gpt-6-astra", "use_responses_lite": true, "supported_in_api": true, "visibility": "list"},
            {"slug": "gpt-reserve", "use_responses_lite": true, "supported_in_api": true, "visibility": "hide"},
            {"slug": "gpt-5.3-codex-spark", "use_responses_lite": false, "supported_in_api": false, "visibility": "list"},
            {"slug": "gpt-7-minimal"},
            {"nonsense": true},
            {"slug": ""}
        ]
    }"#;

    #[test]
    fn parses_only_needed_fields_and_skips_broken_entries() {
        let models = parse_catalog(FIXTURE.as_bytes()).expect("fixture parses");
        assert_eq!(models.len(), 4);
        assert_eq!(
            models[0],
            CatalogModel {
                slug: "gpt-6-astra".into(),
                use_responses_lite: true,
                supported_in_api: true,
                listed: true,
            }
        );
        assert!(!models[1].listed, "hidden models are kept but not listed");
        assert!(!models[2].supported_in_api);
        // Missing fields fall back to: full lane, API-supported, listed.
        assert_eq!(
            models[3],
            CatalogModel {
                slug: "gpt-7-minimal".into(),
                use_responses_lite: false,
                supported_in_api: true,
                listed: true,
            }
        );
    }

    #[test]
    fn garbage_yields_none_and_empty_models_yields_empty() {
        assert!(parse_catalog(b"not json").is_none());
        assert!(parse_catalog(b"[]").is_none());
        assert!(parse_catalog(b"{}").is_none());
        assert!(parse_catalog(br#"{"models":[]}"#).unwrap().is_empty());
    }
}
