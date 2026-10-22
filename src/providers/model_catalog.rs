//! Shared storage and OpenAI-style discovery for provider model lists.

use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use serde_json::Value;

pub(crate) fn cache_file(provider: &str) -> PathBuf {
    crate::paths::provider_auth_file(provider).with_file_name("models.json")
}

pub(crate) fn load(provider: &str) -> Option<Vec<String>> {
    let bytes = std::fs::read(cache_file(provider)).ok()?;
    let ids: Vec<String> = serde_json::from_slice(&bytes).ok()?;
    (!ids.is_empty() && ids.iter().all(|id| !id.trim().is_empty())).then_some(ids)
}

pub(crate) fn save(provider: &str, ids: &[String]) -> Result<()> {
    if ids.is_empty() {
        bail!("{provider} returned no models. The saved list has not changed.");
    }
    crate::fsutil::write_atomic(&cache_file(provider), &serde_json::to_vec(ids)?)?;
    Ok(())
}

pub(crate) fn fetch_openai(
    base_url: &str,
    api_key: Option<&str>,
    timeout: Duration,
) -> Result<Vec<String>> {
    let mut url = reqwest::Url::parse(base_url)?;
    let path = url.path().trim_end_matches('/');
    let path = if path.is_empty() {
        "/v1/models".to_string()
    } else {
        format!("{path}/models")
    };
    url.set_path(&path);
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(timeout.min(Duration::from_secs(10)))
        .timeout(timeout)
        .build()?;
    let mut request = client.get(url).header("Accept", "application/json");
    if let Some(key) = api_key.filter(|key| !key.is_empty()) {
        request = request.bearer_auth(key).header("x-api-key", key);
    }
    let body: Value = request
        .send()
        .context("Could not fetch the model list. Check the provider address and your connection.")?
        .error_for_status()
        .context("The provider refused the model list request. Check its address and API key.")?
        .json()
        .context("The provider's model list was not JSON.")?;
    openai_ids(&body)
}

fn openai_ids(body: &Value) -> Result<Vec<String>> {
    let mut ids: Vec<String> = body["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry["id"].as_str())
        .filter(|id| !id.trim().is_empty())
        .map(str::to_string)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        bail!("The provider returned no model IDs in `data`. The saved list has not changed.");
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_unique_ids_and_rejects_empty_or_invalid_lists() {
        assert_eq!(
            openai_ids(&json!({"data": [
                {"id": "z"}, {"id": "a"}, {"id": "z"}, {"id": " "}, {"id": 3}
            ]}))
            .unwrap(),
            ["a", "z"]
        );
        for invalid in [json!({"error": "denied"}), json!({"data": []}), json!(null)] {
            assert!(openai_ids(&invalid).is_err());
        }
    }
}
