//! The models a Copilot account can run. Copilot lists every model it knows,
//! not only the ones the account may use, so the list is filtered. The result
//! is saved at sign-in and read back when cc-proxy starts, because the model
//! list is built without a network call.

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;

use super::{MODEL_PREFIX, auth};
use crate::{config, paths};

/// Used until a sign-in saves the account's own list. Each one answers on the
/// chat-completions wire; the GPT models that only speak Responses are left out.
const FALLBACK: &[&str] = &[
    "gpt-5.5",
    "gpt-5.4-mini",
    "gpt-5.3-codex",
    "claude-sonnet-5",
    "claude-opus-4.8",
];

fn cache_file() -> std::path::PathBuf {
    paths::provider_auth_file("copilot").with_file_name("models.json")
}

/// The `copilot/<id>` names cc-proxy advertises.
pub fn advertised() -> Vec<String> {
    let saved: Option<Vec<String>> = std::fs::read(cache_file())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    saved
        .filter(|ids| !ids.is_empty())
        .unwrap_or_else(|| FALLBACK.iter().map(|id| (*id).to_string()).collect())
        .into_iter()
        .map(|id| format!("{MODEL_PREFIX}{id}"))
        .collect()
}

/// Asks Copilot which models this account can run and saves the answer.
/// Blocking.
pub fn refresh(auth: &auth::StoredAuth) -> Result<Vec<String>> {
    let host = config::copilot_base_url().unwrap_or_else(|| auth.host.clone());
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()?;
    let mut request = client
        .get(format!("{}/models", host.trim_end_matches('/')))
        .header("Accept", "application/json")
        .bearer_auth(&auth.copilot_token);
    for (name, value) in auth::copilot_headers("user") {
        request = request.header(name, value);
    }
    let body: Value = request
        .send()
        .context("Couldn't reach Copilot for its model list")?
        .error_for_status()
        .context("Copilot refused the model list request")?
        .json()
        .context("Copilot's model list was not JSON")?;
    let ids = filter(&body);
    if ids.is_empty() {
        anyhow::bail!("Copilot's model list had no models this account can run");
    }
    let file = cache_file();
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&file, serde_json::to_vec(&ids)?)?;
    Ok(ids)
}

/// The ids of the `/models` entries the account can run: the ones its model
/// picker shows and policy doesn't disable, else the ones a policy enables
/// (some individual accounts report no picker flag at all), else every entry.
/// A model that can't call tools is dropped throughout, since Claude Code
/// can't work without them.
fn filter(body: &Value) -> Vec<String> {
    let usable: Vec<&Value> = body["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| entry["id"].as_str().is_some_and(|id| !id.is_empty()))
        .filter(|entry| entry["capabilities"]["supports"]["tool_calls"] != Value::Bool(false))
        .collect();
    let policy = |entry: &Value| entry["policy"]["state"].as_str().map(str::to_string);
    let picked: Vec<&Value> = usable
        .iter()
        .copied()
        .filter(|entry| {
            entry["model_picker_enabled"] == Value::Bool(true)
                && policy(entry).as_deref() != Some("disabled")
        })
        .collect();
    let chosen = if !picked.is_empty() {
        picked
    } else {
        let enabled: Vec<&Value> = usable
            .iter()
            .copied()
            .filter(|entry| policy(entry).as_deref() == Some("enabled"))
            .collect();
        if enabled.is_empty() { usable } else { enabled }
    };
    chosen
        .iter()
        .filter_map(|entry| entry["id"].as_str().map(str::to_string))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keeps_picker_models_that_can_call_tools() {
        let body = json!({"data": [
            {"id": "gpt-5.5", "model_picker_enabled": true},
            {"id": "off", "model_picker_enabled": true, "policy": {"state": "disabled"}},
            {"id": "no-tools", "model_picker_enabled": true,
             "capabilities": {"supports": {"tool_calls": false}}},
            {"id": "hidden", "model_picker_enabled": false},
        ]});
        assert_eq!(filter(&body), ["gpt-5.5"]);
    }

    #[test]
    fn falls_back_to_policy_enabled_then_everything() {
        let no_picker = json!({"data": [
            {"id": "a", "model_picker_enabled": false, "policy": {"state": "enabled"}},
            {"id": "b", "model_picker_enabled": false},
        ]});
        assert_eq!(filter(&no_picker), ["a"]);
        let nothing_flagged = json!({"data": [{"id": "a"}, {"id": "b"}]});
        assert_eq!(filter(&nothing_flagged), ["a", "b"]);
    }
}
