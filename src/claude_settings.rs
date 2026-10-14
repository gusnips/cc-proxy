//! Routing and model labels cc-proxy keeps in Claude Code's user settings.
//!
//! Claude Code reads `~/.claude/settings.json` (or `$CLAUDE_CONFIG_DIR/settings.json`)
//! when it starts and again whenever the file changes, in every open window.
//! So one edit here routes terminals, editors and windows that are already
//! open, with no shell hook and no new launch.
//!
//! Claude Code adds `env` values but never removes one, so turning routing off
//! writes values that point back at Anthropic instead of deleting the keys.
//!
//! The file is the user's own and often lives in a dotfiles repo. We keep its
//! key order and the text of every value we don't change, follow a symlink and
//! write in one step. What the user had in a key we take over is saved under `cc-proxy` in
//! the same file, in the same write, and put back on release.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};
use serde::ser::{Serialize, SerializeMap, Serializer};
use serde_json::{Map, Value, value::RawValue};

use crate::{fsutil, paths, registry};

/// Top-level key that holds the backup.
const STATE_KEY: &str = "cc-proxy";
const MODEL_PICKER: &str = "modelPicker";

const BASE_URL: &str = "ANTHROPIC_BASE_URL";
const OAUTH_TOKEN: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// Where Claude Code goes when routing is off.
const NEUTRAL_BASE_URL: &str = "https://api.anthropic.com";

/// Claude Code asks for a credential and the proxy never forwards it. An OAuth
/// token, not ANTHROPIC_AUTH_TOKEN: a session that sets that one is not a
/// claude.ai subscriber, so Claude Code ignores the usage-limit headers. With
/// this one it reads them, shows when the limit resets and can continue the
/// task by itself once it has. It also keeps the user's real login token from
/// being sent to the local proxy.
const PLACEHOLDER_TOKEN: &str = "unused";

/// Taken over while routing is on. The user's own value is saved and comes back.
const ROUTING: [&str; 2] = [BASE_URL, OAUTH_TOKEN];

/// Switches a routed session wants. Set only when the user hasn't set them, and
/// a value the user wrote is never touched.
const TUNING: [(&str, &str); 3] = [
    ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
    // Without it, a stream that fails part-way is sent again without
    // streaming, which can run its tool calls twice.
    ("CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK", "1"),
    ("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY", "1"),
];

/// Settings that pin a model by name, which the proxy then sees as given.
const PINNED_MODEL_KEYS: [&str; 5] = [
    "ANTHROPIC_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
];

/// Where Claude Code reads the user's settings.
pub fn path() -> PathBuf {
    let env = paths::DirResolverEnv::default();
    let dir = match env
        .env
        .get("CLAUDE_CONFIG_DIR")
        .filter(|dir| !dir.is_empty())
    {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(&env.home).join(".claude"),
    };
    dir.join("settings.json")
}

/// A display name for a model. The model ID still selects Claude Code's capabilities.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelLabel {
    pub model: String,
    pub label: String,
    pub description: String,
}

/// What cc-proxy wrote, and what it replaced.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct State {
    /// The user's value before we took the key over. `null` when there was none.
    #[serde(default)]
    claimed: Map<String, Value>,
    /// The value we put there. A key that no longer holds it was edited since.
    #[serde(default)]
    written: Map<String, Value>,
    #[serde(
        default,
        deserialize_with = "model_picker_state",
        skip_serializing_if = "Option::is_none"
    )]
    model_picker: Option<ModelPickerState>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ModelPickerState {
    /// Raw JSON keeps an absent key distinct from a key whose value was `null`.
    #[serde(deserialize_with = "Deserialize::deserialize")]
    original: Option<String>,
    written: Value,
    #[serde(default)]
    observed: Vec<String>,
    #[serde(default)]
    paused: bool,
}

/// Damaged picker metadata must not discard the separate routing backup.
fn model_picker_state<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<ModelPickerState>, D::Error> {
    let value = Value::deserialize(deserializer)?;
    let state: Option<ModelPickerState> = serde_json::from_value(value).ok();
    Ok(state.filter(|picker| {
        picker.written.is_object() && model_picker(picker.original.as_deref()).is_ok()
    }))
}

impl State {
    fn written_text(&self, key: &str) -> Option<&str> {
        self.written.get(key).and_then(Value::as_str)
    }
}

/// A JSON object that keeps its keys in file order and its values byte for byte.
struct Ordered(Vec<(String, Box<RawValue>)>);

impl<'de> Deserialize<'de> for Ordered {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Pairs;
        impl<'de> Visitor<'de> for Pairs {
            type Value = Ordered;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Ordered, A::Error> {
                let mut pairs = Vec::new();
                while let Some(pair) = map.next_entry::<String, Box<RawValue>>()? {
                    pairs.push(pair);
                }
                Ok(Ordered(pairs))
            }
        }
        deserializer.deserialize_map(Pairs)
    }
}

impl Serialize for Ordered {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (key, value) in &self.0 {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

impl Ordered {
    fn parse(text: &str) -> Result<Self, serde_json::Error> {
        if text.trim().is_empty() {
            return Ok(Self(Vec::new()));
        }
        serde_json::from_str(text)
    }

    /// The last entry wins, as in Claude Code's own parser.
    fn raw(&self, key: &str) -> Option<&RawValue> {
        self.0
            .iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_ref())
    }

    fn value(&self, key: &str) -> Option<Value> {
        let value: Value = serde_json::from_str(self.raw(key)?.get()).ok()?;
        (!value.is_null()).then_some(value)
    }

    /// A string as itself, any other value as its JSON text.
    fn text(&self, key: &str) -> Option<String> {
        Some(match self.value(key)? {
            Value::String(text) => text,
            other => other.to_string(),
        })
    }

    fn set_raw(&mut self, key: &str, json: String) -> Result<()> {
        let raw = RawValue::from_string(json)?;
        match self.0.iter().rposition(|(name, _)| name == key) {
            Some(index) => {
                self.0[index].1 = raw;
                // The last entry wins, so earlier copies of the key can only mislead.
                let mut position = 0;
                self.0.retain(|(name, _)| {
                    let keep = name != key || position == index;
                    position += 1;
                    keep
                });
            }
            None => self.0.push((key.to_string(), raw)),
        }
        Ok(())
    }

    fn set(&mut self, key: &str, value: &Value) -> Result<()> {
        self.set_raw(key, value.to_string())
    }

    fn remove(&mut self, key: &str) {
        self.0.retain(|(name, _)| name != key);
    }
}

/// The settings file as text: the top level and its `env` block, plus the
/// look of the file so a rewrite doesn't reformat it.
struct Document {
    root: Ordered,
    indent: String,
    final_newline: bool,
}

impl Document {
    fn parse(text: &str) -> Result<Self, serde_json::Error> {
        Ok(Self {
            root: Ordered::parse(text)?,
            indent: indent_of(text),
            final_newline: text.is_empty() || text.ends_with('\n'),
        })
    }

    fn env(&self) -> Result<Ordered> {
        match self.root.raw("env").map(RawValue::get) {
            None | Some("null") => Ok(Ordered(Vec::new())),
            Some(raw) => serde_json::from_str(raw)
                .map_err(|_| anyhow!("`env` in the settings file must be an object")),
        }
    }

    fn set_env(&mut self, env: &Ordered) -> Result<()> {
        let json = self.nested(env)?;
        self.root.set_raw("env", json)
    }

    fn state(&self) -> Option<State> {
        serde_json::from_str(self.root.raw(STATE_KEY)?.get()).ok()
    }

    fn set_state(&mut self, state: Option<&State>) -> Result<()> {
        match state {
            Some(state) => {
                let json = self.nested(state)?;
                self.root.set_raw(STATE_KEY, json)
            }
            None => {
                self.root.remove(STATE_KEY);
                Ok(())
            }
        }
    }

    /// `value` as pretty JSON, indented to sit one level down in the file.
    fn nested(&self, value: &impl Serialize) -> Result<String> {
        Ok(pretty(value, &self.indent)?.replace('\n', &format!("\n{}", self.indent)))
    }

    fn render(&self) -> Result<String> {
        let mut text = pretty(&self.root, &self.indent)?;
        if self.final_newline {
            text.push('\n');
        }
        Ok(text)
    }
}

fn pretty(value: &impl Serialize, indent: &str) -> Result<String> {
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes());
    value.serialize(&mut serde_json::Serializer::with_formatter(
        &mut out, formatter,
    ))?;
    Ok(String::from_utf8(out)?)
}

/// The indent of the file's first indented line: two spaces, four, a tab.
fn indent_of(text: &str) -> String {
    text.lines()
        .find_map(|line| {
            let indent = &line[..line.len() - line.trim_start().len()];
            (!indent.is_empty() && !line.trim().is_empty()).then(|| indent.to_string())
        })
        .unwrap_or_else(|| "  ".to_string())
}

/// What a key holds when routing is off and the user had nothing there.
fn neutral(key: &str) -> Value {
    Value::String(if key == BASE_URL {
        NEUTRAL_BASE_URL.to_string()
    } else {
        String::new()
    })
}

/// A file that points at a proxy of ours but has lost its backup, for example
/// when a dotfiles sync wrote an older copy over it.
fn looks_routed(env: &Ordered) -> bool {
    env.text(OAUTH_TOKEN).as_deref() == Some(PLACEHOLDER_TOKEN)
        && env
            .text(BASE_URL)
            .is_some_and(|url| url.starts_with("http://"))
}

fn claim_in(doc: &mut Document, base_url: &str) -> Result<()> {
    let mut env = doc.env()?;
    let prior = doc.state();
    let backup_lost = prior.is_none() && looks_routed(&env);
    let mut state = State::default();

    let routing = [(BASE_URL, base_url), (OAUTH_TOKEN, PLACEHOLDER_TOKEN)];
    let wanted = routing
        .into_iter()
        .map(|(key, value)| (key, value, false))
        .chain(TUNING.into_iter().map(|(key, value)| (key, value, true)));
    for (key, value, tuning) in wanted {
        let current = env.text(key);
        let ours = prior
            .as_ref()
            .and_then(|prior| prior.written_text(key))
            .is_some_and(|written| Some(written) == current.as_deref());
        if tuning && !ours && current.as_deref().is_some_and(|text| !text.is_empty()) {
            continue;
        }
        let claimed = if ours {
            prior
                .as_ref()
                .and_then(|prior| prior.claimed.get(key))
                .cloned()
                .unwrap_or(Value::Null)
        } else if backup_lost && !tuning {
            Value::Null
        } else {
            env.value(key).unwrap_or(Value::Null)
        };
        let value = Value::String(value.to_string());
        env.set(key, &value)?;
        state.claimed.insert(key.to_string(), claimed);
        state.written.insert(key.to_string(), value);
    }
    state.model_picker = prior.and_then(|prior| prior.model_picker);
    doc.set_env(&env)?;
    doc.set_state(Some(&state))
}

/// Whether there was anything of ours to hand back.
fn release_in(doc: &mut Document) -> Result<bool> {
    let mut env = doc.env()?;
    match doc.state() {
        Some(state) => {
            for key in state.written.keys() {
                if env.text(key).as_deref() != state.written_text(key) {
                    // Edited since we wrote it: now the user's.
                    continue;
                }
                let restored = match state.claimed.get(key) {
                    Some(previous) if !previous.is_null() => previous.clone(),
                    _ => neutral(key),
                };
                env.set(key, &restored)?;
            }
            if let Some(picker) = state.model_picker
                && !picker.paused
                && doc.root.raw(MODEL_PICKER).is_some_and(|raw| {
                    serde_json::from_str::<Value>(raw.get()).ok().as_ref() == Some(&picker.written)
                })
            {
                match picker.original {
                    Some(original) => doc.root.set_raw(MODEL_PICKER, original)?,
                    None => doc.root.remove(MODEL_PICKER),
                }
            }
        }
        None if looks_routed(&env) => {
            for key in ROUTING {
                env.set(key, &neutral(key))?;
            }
        }
        None => return Ok(false),
    }
    doc.set_env(&env)?;
    doc.set_state(None)?;
    Ok(true)
}

/// `text` with the routing values written and the previous ones saved.
pub(crate) fn claim_text(text: &str, base_url: &str) -> Result<String> {
    let mut doc = Document::parse(text)?;
    claim_in(&mut doc, base_url)?;
    doc.render()
}

/// `text` with the routing values handed back.
pub(crate) fn release_text(text: &str) -> Result<String> {
    let mut doc = Document::parse(text)?;
    if release_in(&mut doc)? {
        doc.render()
    } else {
        // Nothing of ours in it: not a byte of the file moves.
        Ok(text.to_string())
    }
}

/// The file's text, or nothing when there is no file yet.
fn read(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error).with_context(|| format!("couldn't read {}", path.display())),
    }
}

/// Read, change and write the settings file. Gives up on a file we can't read
/// as JSON without touching it, and re-reads once if another program wrote the
/// file while we worked.
fn edit(path: &Path, change: impl Fn(&str) -> Result<String>) -> Result<bool> {
    let stamp = || {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok()
    };
    for _ in 0..2 {
        let before = stamp();
        let text = read(path)?;
        let updated = change(&text).map_err(|error| {
            anyhow!(
                "cc-proxy couldn't use {}: {error}. It left the file as it was. Fix it, then \
                 run the command again.",
                path.display()
            )
        })?;
        if updated == text {
            return Ok(false);
        }
        if stamp() != before {
            continue;
        }
        fsutil::write_atomic(path, updated.as_bytes())
            .with_context(|| format!("couldn't write {}", path.display()))?;
        return Ok(true);
    }
    bail!(
        "{} kept changing while cc-proxy edited it. Run the command again.",
        path.display()
    )
}

/// Point Claude Code at the proxy at `base_url`. Returns whether the file changed.
pub fn claim(base_url: &str) -> Result<bool> {
    edit(&path(), |text| claim_text(text, base_url))
}

/// Point Claude Code back at Anthropic. Returns whether the file changed.
pub fn release() -> Result<bool> {
    edit(&path(), release_text)
}

fn model_picker(raw: Option<&str>) -> Result<Map<String, Value>> {
    let value: Value = serde_json::from_str(raw.unwrap_or("{}"))?;
    let picker = match value {
        Value::Null => Map::new(),
        Value::Object(picker) => picker,
        _ => bail!("`modelPicker` must be an object or null. Fix it in Claude Code's settings"),
    };
    if picker
        .get("replaceBuiltInOptions")
        .is_some_and(|value| !value.is_boolean())
    {
        bail!(
            "`modelPicker.replaceBuiltInOptions` must be true or false. Fix it in Claude Code's settings"
        );
    }
    if let Some(options) = picker.get("options") {
        let Some(options) = options.as_array() else {
            bail!("`modelPicker.options` must be an array. Fix it in Claude Code's settings");
        };
        for option in options {
            if !option.get("model").is_some_and(Value::is_string)
                || !option.get("label").is_some_and(Value::is_string)
                || option
                    .get("description")
                    .is_some_and(|value| !value.is_string())
            {
                bail!(
                    "Each `modelPicker.options` row needs a model and label as text, and an optional description as text. Fix it in Claude Code's settings"
                );
            }
        }
    }
    Ok(picker)
}

fn sync_model_labels_text(
    text: &str,
    base_models: &[&str],
    observed: Option<&str>,
    resolve: &impl Fn(&str) -> Option<ModelLabel>,
) -> Result<String> {
    let mut doc = Document::parse(text)?;
    let Some(mut state) = doc.state() else {
        return Ok(text.to_string());
    };
    let env = doc.env()?;
    if state.written_text(BASE_URL).is_none()
        || env.text(BASE_URL).as_deref() != state.written_text(BASE_URL)
    {
        return Ok(text.to_string());
    }

    let current = doc
        .root
        .raw(MODEL_PICKER)
        .and_then(|raw| serde_json::from_str::<Value>(raw.get()).ok());
    if let Some(picker) = state.model_picker.as_mut() {
        if picker.paused {
            return Ok(text.to_string());
        }
        if current.as_ref() != Some(&picker.written) {
            picker.paused = true;
            doc.set_state(Some(&state))?;
            return doc.render();
        }
    }

    let original = match state.model_picker.as_ref() {
        Some(picker) => picker.original.clone(),
        None => doc.root.raw(MODEL_PICKER).map(|raw| raw.get().to_string()),
    };
    let mut picker = model_picker(original.as_deref())?;
    let mut options = picker
        .get("options")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut remembered = state
        .model_picker
        .as_ref()
        .map(|picker| picker.observed.clone())
        .unwrap_or_default();
    let mut models = Vec::new();
    for model in base_models
        .iter()
        .copied()
        .chain(remembered.iter().map(String::as_str))
        .chain(observed)
    {
        if !models.contains(&model) {
            models.push(model);
        }
    }
    let mut labels = Vec::new();
    let mut remember_observed = false;
    for model in models {
        let Some(label) = resolve(model) else {
            continue;
        };
        if label.model != model {
            bail!("The label for `{model}` must keep that model ID. cc-proxy changed no settings");
        }
        remember_observed |= observed == Some(model);
        labels.push(label);
    }
    if labels.is_empty() && state.model_picker.is_none() {
        return Ok(text.to_string());
    }
    if let Some(observed) = observed
        && remember_observed
        && !base_models.contains(&observed)
        && !remembered.iter().any(|model| model == observed)
    {
        remembered.push(observed.to_string());
    }
    let concrete_families: Vec<String> = labels
        .iter()
        .filter(|label| label.model.starts_with("claude-"))
        .filter_map(|label| {
            registry::anthropic_model_family(&registry::normalize_incoming_model(&label.model))
                .map(str::to_string)
        })
        .collect();
    for label in &labels {
        // A concrete native ID replaces only our fallback alias, never a user's row.
        if !concrete_families.contains(&label.model)
            && !options.iter().any(|option| {
                option.get("model").and_then(Value::as_str).map(str::trim)
                    == Some(label.model.as_str())
            })
        {
            options.push(serde_json::to_value(label)?);
        }
    }
    // Appended rows lose to Claude Code's built-in labels for the same model ID.
    // With no resolved labels, keep the built-ins available alongside the user's rows.
    picker.insert(
        "replaceBuiltInOptions".into(),
        Value::Bool(!labels.is_empty()),
    );
    picker.insert("options".into(), Value::Array(options));
    let written = Value::Object(picker);
    let previous_observed = state
        .model_picker
        .as_ref()
        .map(|picker| picker.observed.as_slice())
        .unwrap_or_default();
    if current.as_ref() == Some(&written) && previous_observed == remembered {
        return Ok(text.to_string());
    }
    state.model_picker = Some(ModelPickerState {
        original,
        written: written.clone(),
        observed: remembered,
        paused: false,
    });
    if current.as_ref() != Some(&written) {
        doc.root.set_raw(MODEL_PICKER, doc.nested(&written)?)?;
    }
    doc.set_state(Some(&state))?;
    doc.render()
}

/// Update picker labels only while the routing settings still belong to cc-proxy.
/// User-written options and labels take precedence, and manual edits pause updates.
pub fn sync_model_labels(
    base_models: &[&str],
    observed: Option<&str>,
    resolve: impl Fn(&str) -> Option<ModelLabel>,
) -> Result<bool> {
    edit(&path(), |text| {
        sync_model_labels_text(text, base_models, observed, &resolve)
    })
}

/// What the settings file says about routing.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Inspection {
    /// The proxy URL the file sends Claude Code to, when it is ours.
    pub routed_to: Option<String>,
    /// Things in the file that change how a routed session behaves.
    pub warnings: Vec<String>,
}

pub fn inspect() -> Result<Inspection> {
    let path = path();
    inspect_text(&read(&path)?, &path)
}

fn inspect_text(text: &str, path: &Path) -> Result<Inspection> {
    let doc =
        Document::parse(text).with_context(|| format!("{} isn't valid JSON", path.display()))?;
    let env = doc.env()?;
    let state = doc.state();
    let mut out = Inspection::default();

    let written_url = state
        .as_ref()
        .and_then(|state| state.written_text(BASE_URL));
    let current_url = env.text(BASE_URL);
    match (written_url, current_url) {
        (Some(written), Some(current)) if written == current => {
            out.routed_to = Some(current);
        }
        (Some(written), _) => out.warnings.push(format!(
            "{BASE_URL} in {} no longer matches what cc-proxy wrote ({written}), so Claude Code \
             isn't going through it. `cc-proxy start` writes it again.",
            path.display()
        )),
        (None, Some(current)) if looks_routed(&env) => out.routed_to = Some(current),
        (None, _) => {}
    }

    let has_env = |key| env.text(key).is_some_and(|value| !value.is_empty());
    let has_top = |key| doc.root.value(key).is_some();
    if has_env("ANTHROPIC_AUTH_TOKEN") || has_env("ANTHROPIC_API_KEY") || has_top("apiKeyHelper") {
        out.warnings.push(
            "Your settings carry an API key, an auth token or an apiKeyHelper. Claude Code then \
             ignores the usage-limit headers, so it can't show when a limit resets."
                .to_string(),
        );
    }
    let pinned: Vec<&str> = PINNED_MODEL_KEYS
        .into_iter()
        .filter(|key| has_env(key))
        .chain(has_top("model").then_some("model"))
        .collect();
    if !pinned.is_empty() {
        out.warnings.push(format!(
            "Your settings pin a model ({}). The proxy gets that name as it is.",
            pinned.join(", ")
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const URL: &str = "http://127.0.0.1:18765";
    const BASE_MODELS: [&str; 2] = ["opus", "sonnet"];

    fn sync_labels(text: &str, observed: Option<&str>, provider: &str) -> String {
        sync_model_labels_text(text, &BASE_MODELS, observed, &|model| {
            Some(ModelLabel {
                model: model.to_string(),
                label: format!("{provider}/actual-model"),
                description: format!("{model} routes to {provider}/actual-model"),
            })
        })
        .unwrap()
    }

    fn picker_of(text: &str) -> Value {
        serde_json::from_str::<Value>(text).unwrap()[MODEL_PICKER].clone()
    }

    fn env_of(text: &str) -> Value {
        serde_json::from_str::<Value>(text).unwrap()["env"].clone()
    }

    /// `text` as a person editing `env` by hand would leave it: the backup stays as it was.
    fn hand_edit(text: &str, key: &str, value: &str) -> String {
        let mut root: Value = serde_json::from_str(text).unwrap();
        root["env"][key] = json!(value);
        serde_json::to_string_pretty(&root).unwrap()
    }

    /// A settings file the way a person keeps it: their own key order, extra
    /// top-level blocks, a tuning key of their own.
    const HAND_EDITED: &str = r#"{
  "model": "opus",
  "env": {
    "ZED": "last-key-first",
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW": "350000",
    "ANTHROPIC_BASE_URL": "https://gateway.example.com",
    "ALPHA": "1"
  },
  "env-chat": {
    "ANTHROPIC_MODEL": "gpt-6.1-sol[1m]"
  },
  "permissions": { "allow": ["Bash(ls:*)"] }
}
"#;

    #[test]
    fn claim_then_release_gives_back_the_users_values() {
        let claimed = claim_text(HAND_EDITED, URL).unwrap();
        let env = env_of(&claimed);
        assert_eq!(env["ANTHROPIC_BASE_URL"], URL);
        assert_eq!(env["CLAUDE_CODE_OAUTH_TOKEN"], "unused");
        assert_eq!(env["CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK"], "1");
        assert_eq!(env["CLAUDE_CODE_AUTO_COMPACT_WINDOW"], "350000");

        let released = release_text(&claimed).unwrap();
        let env = env_of(&released);
        // The user's own gateway is back; the token they never had is neutral.
        assert_eq!(env["ANTHROPIC_BASE_URL"], "https://gateway.example.com");
        assert_eq!(env["CLAUDE_CODE_OAUTH_TOKEN"], "");
        assert_eq!(env["CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"], "");
        assert_eq!(env["CLAUDE_CODE_AUTO_COMPACT_WINDOW"], "350000");
        assert_eq!(env["ZED"], "last-key-first");
        assert!(
            serde_json::from_str::<Value>(&released)
                .unwrap()
                .get(STATE_KEY)
                .is_none(),
            "the backup goes with the release"
        );
    }

    #[test]
    fn release_without_a_previous_value_points_back_at_anthropic() {
        let released = release_text(&claim_text("{}", URL).unwrap()).unwrap();
        let env = env_of(&released);
        assert_eq!(env["ANTHROPIC_BASE_URL"], "https://api.anthropic.com");
        assert_eq!(env["CLAUDE_CODE_OAUTH_TOKEN"], "");
    }

    #[test]
    fn a_tuning_key_the_user_set_is_never_touched() {
        let text = r#"{"env":{"CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK":"0"}}"#;
        let claimed = claim_text(text, URL).unwrap();
        assert_eq!(
            env_of(&claimed)["CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK"],
            "0"
        );
        let released = release_text(&claimed).unwrap();
        assert_eq!(
            env_of(&released)["CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK"],
            "0"
        );
    }

    #[test]
    fn claiming_again_keeps_the_first_backup() {
        let once = claim_text(HAND_EDITED, URL).unwrap();
        let twice = claim_text(&once, "http://127.0.0.1:19000").unwrap();
        assert_eq!(
            env_of(&twice)["ANTHROPIC_BASE_URL"],
            "http://127.0.0.1:19000"
        );
        let released = release_text(&twice).unwrap();
        assert_eq!(
            env_of(&released)["ANTHROPIC_BASE_URL"],
            "https://gateway.example.com"
        );
    }

    #[test]
    fn claiming_the_same_thing_twice_changes_nothing() {
        let once = claim_text(HAND_EDITED, URL).unwrap();
        assert_eq!(claim_text(&once, URL).unwrap(), once);
    }

    #[test]
    fn a_hand_edit_made_while_routed_survives_the_release() {
        let claimed = claim_text(HAND_EDITED, URL).unwrap();
        let edited = hand_edit(&claimed, BASE_URL, "http://127.0.0.1:9999");
        let released = release_text(&edited).unwrap();
        assert_eq!(
            env_of(&released)["ANTHROPIC_BASE_URL"],
            "http://127.0.0.1:9999"
        );
        assert_eq!(env_of(&released)["CLAUDE_CODE_OAUTH_TOKEN"], "");
    }

    #[test]
    fn key_order_and_other_blocks_stay_where_they_were() {
        let claimed = claim_text(HAND_EDITED, URL).unwrap();
        let order = |text: &str| -> Vec<String> {
            Ordered::parse(text)
                .unwrap()
                .0
                .into_iter()
                .map(|(key, _)| key)
                .collect()
        };
        assert_eq!(
            order(&claimed),
            ["model", "env", "env-chat", "permissions", STATE_KEY]
        );
        let env_text = Ordered::parse(&claimed).unwrap();
        let env = Ordered::parse(env_text.raw("env").unwrap().get()).unwrap();
        assert_eq!(
            env.0
                .iter()
                .map(|(key, _)| key.as_str())
                .take(4)
                .collect::<Vec<_>>(),
            [
                "ZED",
                "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
                "ANTHROPIC_BASE_URL",
                "ALPHA"
            ]
        );
        // Untouched blocks keep their exact text.
        assert!(claimed.contains("\"permissions\": { \"allow\": [\"Bash(ls:*)\"] }"));
        assert!(
            claimed.contains("\"env-chat\": {\n    \"ANTHROPIC_MODEL\": \"gpt-6.1-sol[1m]\"\n  }")
        );
        assert!(claimed.ends_with("}\n"));
    }

    #[test]
    fn a_four_space_file_stays_four_spaces() {
        let text = "{\n    \"a\": 1\n}\n";
        let claimed = claim_text(text, URL).unwrap();
        assert!(
            claimed.contains("\n    \"env\": {\n        \"ANTHROPIC_BASE_URL\""),
            "{claimed}"
        );
    }

    #[test]
    fn a_file_with_no_final_newline_gets_none() {
        let claimed = claim_text(r#"{"a":1}"#, URL).unwrap();
        assert!(!claimed.ends_with('\n'));
    }

    #[test]
    fn an_empty_or_missing_file_becomes_a_settings_file() {
        let claimed = claim_text("", URL).unwrap();
        assert_eq!(env_of(&claimed)["ANTHROPIC_BASE_URL"], URL);
        assert!(claimed.ends_with("}\n"));
    }

    #[test]
    fn a_file_that_is_not_json_is_an_error_and_the_text_is_not_ours_to_change() {
        for text in ["{ not json", "[1,2]", r#"{"env": []}"#, "// comment\n{}"] {
            assert!(claim_text(text, URL).is_err(), "{text}");
            assert!(release_text(text).is_err(), "{text}");
        }
    }

    #[test]
    fn release_with_nothing_of_ours_changes_nothing() {
        assert_eq!(release_text(HAND_EDITED).unwrap(), HAND_EDITED);
    }

    #[test]
    fn a_lost_backup_is_adopted_not_restored_as_the_users_value() {
        let claimed = claim_text(HAND_EDITED, URL).unwrap();
        let mut root: Map<String, Value> = serde_json::from_str(&claimed).unwrap();
        root.remove(STATE_KEY);
        let without_backup = serde_json::to_string_pretty(&root).unwrap();

        // Claiming again must not save the proxy URL as the user's own value.
        let again = claim_text(&without_backup, "http://127.0.0.1:19000").unwrap();
        let released = release_text(&again).unwrap();
        assert_eq!(
            env_of(&released)["ANTHROPIC_BASE_URL"],
            "https://api.anthropic.com"
        );

        // Releasing points the routing keys back at Anthropic.
        let released = release_text(&without_backup).unwrap();
        assert_eq!(
            env_of(&released)["ANTHROPIC_BASE_URL"],
            "https://api.anthropic.com"
        );
        assert_eq!(env_of(&released)["CLAUDE_CODE_OAUTH_TOKEN"], "");
    }

    #[test]
    fn a_repeated_key_is_replaced_not_shadowed() {
        let text = r#"{"env":{"ANTHROPIC_BASE_URL":"a","ANTHROPIC_BASE_URL":"b"}}"#;
        let claimed = claim_text(text, URL).unwrap();
        assert_eq!(env_of(&claimed)["ANTHROPIC_BASE_URL"], URL);
        assert_eq!(
            Ordered::parse(&claimed).unwrap().0.len(),
            2,
            "env and the backup"
        );
    }

    #[test]
    fn inspect_reports_the_route_and_what_gets_in_its_way() {
        let path = Path::new("/home/ada/.claude/settings.json");
        let routed = claim_text(
            r#"{"model":"opus","apiKeyHelper":"/bin/key","env":{"ANTHROPIC_DEFAULT_HAIKU_MODEL":"x"}}"#,
            URL,
        )
        .unwrap();
        let found = inspect_text(&routed, path).unwrap();
        assert_eq!(found.routed_to.as_deref(), Some(URL));
        assert_eq!(found.warnings.len(), 2, "{:?}", found.warnings);
        assert!(found.warnings[0].contains("usage-limit"));
        assert!(found.warnings[1].contains("ANTHROPIC_DEFAULT_HAIKU_MODEL, model"));

        assert_eq!(inspect_text("{}", path).unwrap(), Inspection::default());

        let drifted = hand_edit(&routed, BASE_URL, "http://127.0.0.1:1");
        let found = inspect_text(&drifted, path).unwrap();
        assert_eq!(found.routed_to, None);
        assert!(found.warnings[0].contains("no longer matches"));
    }

    #[test]
    fn the_file_on_disk_is_replaced_in_one_piece_and_a_symlink_survives() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles/settings.json");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, HAND_EDITED).unwrap();
        let link = dir.path().join("settings.json");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        #[cfg(not(unix))]
        let link = real.clone();

        assert!(edit(&link, |text| claim_text(text, URL)).unwrap());
        assert!(
            !edit(&link, |text| claim_text(text, URL)).unwrap(),
            "already claimed"
        );
        assert_eq!(
            env_of(&std::fs::read_to_string(&real).unwrap())["ANTHROPIC_BASE_URL"],
            URL
        );
        #[cfg(unix)]
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());

        assert!(edit(&link, |text| Ok(sync_labels(text, None, "first"))).unwrap());
        assert!(!edit(&link, |text| Ok(sync_labels(text, None, "first"))).unwrap());
        #[cfg(unix)]
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());

        assert!(edit(&link, release_text).unwrap());
        assert_eq!(
            env_of(&std::fs::read_to_string(&real).unwrap())["ANTHROPIC_BASE_URL"],
            "https://gateway.example.com"
        );
        let leftovers: Vec<_> = std::fs::read_dir(real.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(leftovers.len(), 1, "{leftovers:?}");
    }

    #[test]
    fn invalid_json_on_disk_is_left_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{ // comments\n}").unwrap();
        let error = edit(&path, |text| claim_text(text, URL))
            .unwrap_err()
            .to_string();
        assert!(error.contains("left the file as it was"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ // comments\n}");
        assert!(
            edit(&path, |text| sync_model_labels_text(
                text,
                &BASE_MODELS,
                None,
                &|_| None
            ))
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ // comments\n}");
    }

    #[test]
    fn labels_refresh_remembered_model_ids_after_a_provider_switch() {
        let claimed = claim_text(HAND_EDITED, URL).unwrap();
        let first = sync_labels(&claimed, Some("claude-sonnet-native"), "first");
        assert_eq!(
            sync_labels(&first, Some("claude-sonnet-native"), "first"),
            first
        );
        let switched = claim_text(&first, "http://127.0.0.1:19000").unwrap();
        let calls = std::cell::RefCell::new(Vec::new());
        let refreshed = sync_model_labels_text(
            &switched,
            &BASE_MODELS,
            Some("claude-opus-native"),
            &|model| {
                calls.borrow_mut().push(model.to_string());
                Some(ModelLabel {
                    model: model.to_string(),
                    label: "second/new-model".into(),
                    description: "Uses second/new-model".into(),
                })
            },
        )
        .unwrap();
        assert_eq!(
            *calls.borrow(),
            [
                "opus",
                "sonnet",
                "claude-sonnet-native",
                "claude-opus-native"
            ]
        );
        let options = picker_of(&refreshed)["options"].as_array().unwrap().clone();
        assert_eq!(options.len(), 2);
        assert_eq!(options[0]["model"], "claude-sonnet-native");
        assert_eq!(options[1]["model"], "claude-opus-native");
        assert!(
            options
                .iter()
                .all(|option| option["label"] == "second/new-model")
        );
        let state = Document::parse(&refreshed).unwrap().state().unwrap();
        assert_eq!(
            state.model_picker.unwrap().observed,
            ["claude-sonnet-native", "claude-opus-native"]
        );
        let stable = sync_labels(&refreshed, None, "second");
        assert_eq!(sync_labels(&stable, None, "second"), stable);
        let released = release_text(&refreshed).unwrap();
        assert!(
            Document::parse(&released)
                .unwrap()
                .root
                .raw(MODEL_PICKER)
                .is_none()
        );
        assert_eq!(env_of(&released)[BASE_URL], "https://gateway.example.com");
    }

    #[test]
    fn concrete_ids_replace_fallback_aliases_but_keep_context_suffixes_and_versions() {
        let base = ["fable", "opus", "sonnet", "haiku"];
        let claimed = claim_text("{}", URL).unwrap();
        let sync = |text: &str, observed: Option<&str>| {
            sync_model_labels_text(text, &base, observed, &|model| {
                Some(ModelLabel {
                    model: model.into(),
                    label: "provider/actual-model".into(),
                    description: format!("Requested model: {model}."),
                })
            })
            .unwrap()
        };
        let seeded = sync(&claimed, None);
        assert_eq!(
            picker_of(&seeded)["options"]
                .as_array()
                .unwrap()
                .iter()
                .map(|option| option["model"].as_str().unwrap())
                .collect::<Vec<_>>(),
            base
        );
        let first = sync(&seeded, Some("claude-opus-future"));
        let second = sync(&first, Some("claude-opus-newer[1m]"));
        let picker = picker_of(&second);
        let options = picker["options"].as_array().unwrap();
        assert_eq!(
            options
                .iter()
                .map(|option| option["model"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "fable",
                "sonnet",
                "haiku",
                "claude-opus-future",
                "claude-opus-newer[1m]"
            ]
        );
        assert_eq!(
            options[3]["description"],
            "Requested model: claude-opus-future."
        );
        assert_eq!(
            options[4]["description"],
            "Requested model: claude-opus-newer[1m]."
        );
        assert_eq!(picker["replaceBuiltInOptions"], true);
        assert_eq!(sync(&second, None), second);
        let switched = claim_text(&second, "http://127.0.0.1:19000").unwrap();
        assert_eq!(picker_of(&sync(&switched, None)), picker);
        assert!(
            Document::parse(&release_text(&switched).unwrap())
                .unwrap()
                .root
                .raw(MODEL_PICKER)
                .is_none()
        );
    }

    #[test]
    fn an_unresolved_native_id_keeps_its_fallback_until_it_can_be_labeled_again() {
        let claimed = claim_text("{}", URL).unwrap();
        let first = sync_labels(&claimed, Some("claude-opus-native"), "first");
        let fallback = sync_model_labels_text(&first, &BASE_MODELS, None, &|model| {
            (!model.starts_with("claude-")).then(|| ModelLabel {
                model: model.into(),
                label: "second/actual-model".into(),
                description: format!("Requested model: {model}."),
            })
        })
        .unwrap();
        assert_eq!(
            picker_of(&fallback)["options"]
                .as_array()
                .unwrap()
                .iter()
                .map(|option| option["model"].as_str().unwrap())
                .collect::<Vec<_>>(),
            BASE_MODELS
        );
        assert_eq!(
            Document::parse(&fallback)
                .unwrap()
                .state()
                .unwrap()
                .model_picker
                .unwrap()
                .observed,
            ["claude-opus-native"]
        );
        let available = sync_labels(&fallback, None, "third");
        assert_eq!(
            picker_of(&available)["options"]
                .as_array()
                .unwrap()
                .iter()
                .map(|option| option["model"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["sonnet", "claude-opus-native"]
        );
    }

    #[test]
    fn unavailable_labels_keep_built_in_choices_and_can_recover_remembered_ids() {
        let claimed = claim_text(HAND_EDITED, URL).unwrap();
        let first = sync_labels(&claimed, Some("claude-opus-native"), "first");
        let fallback = sync_model_labels_text(&first, &BASE_MODELS, None, &|_| None).unwrap();
        assert_eq!(picker_of(&fallback)["options"], json!([]));
        assert_eq!(picker_of(&fallback)["replaceBuiltInOptions"], false);
        assert_eq!(env_of(&fallback), env_of(&first));
        assert_eq!(
            serde_json::from_str::<Value>(&fallback).unwrap()["model"],
            "opus"
        );
        assert_eq!(
            Document::parse(&fallback)
                .unwrap()
                .state()
                .unwrap()
                .model_picker
                .unwrap()
                .observed,
            ["claude-opus-native"]
        );
        assert_eq!(
            sync_model_labels_text(&fallback, &BASE_MODELS, None, &|_| None).unwrap(),
            fallback
        );
        let recovered = sync_labels(&fallback, None, "second");
        let picker = picker_of(&recovered);
        assert_eq!(picker["replaceBuiltInOptions"], true);
        assert_eq!(picker["options"][0]["model"], "sonnet");
        assert_eq!(picker["options"][1]["model"], "claude-opus-native");
        assert_eq!(picker["options"][1]["label"], "second/actual-model");
        let released = release_text(&fallback).unwrap();
        assert!(
            Document::parse(&released)
                .unwrap()
                .root
                .raw(MODEL_PICKER)
                .is_none()
        );
        assert_eq!(env_of(&released)[BASE_URL], "https://gateway.example.com");
    }

    #[test]
    fn unavailable_labels_restore_built_ins_alongside_original_custom_rows() {
        let original = r#"{"modelPicker":{"replaceBuiltInOptions":true,"options":[
            {"model":"custom","label":"My model","extra":7}
        ]}}"#;
        let claimed = claim_text(original, URL).unwrap();
        let first = sync_labels(&claimed, None, "first");
        let fallback = sync_model_labels_text(&first, &BASE_MODELS, None, &|_| None).unwrap();
        assert_eq!(picker_of(&fallback)["replaceBuiltInOptions"], false);
        assert_eq!(
            picker_of(&fallback)["options"],
            picker_of(original)["options"]
        );
        assert_eq!(env_of(&fallback), env_of(&first));
        let recovered = sync_labels(&fallback, None, "second");
        let picker = picker_of(&recovered);
        assert_eq!(picker["replaceBuiltInOptions"], true);
        assert_eq!(picker["options"][0], picker_of(original)["options"][0]);
        assert_eq!(picker["options"].as_array().unwrap().len(), 3);
        assert_eq!(picker["options"][1]["label"], "second/actual-model");
        assert_eq!(
            picker_of(&release_text(&fallback).unwrap()),
            picker_of(original)
        );
    }

    #[test]
    fn native_observations_never_replace_original_custom_labels_or_alias_rows() {
        let original = r#"{"modelPicker":{"replaceBuiltInOptions":false,"options":[
            {"model":"  opus  ","label":"My alias"},
            {"model":"  claude-opus-native  ","label":"My native model","extra":7}
        ]}}"#;
        let claimed = claim_text(original, URL).unwrap();
        let seeded = sync_labels(&claimed, None, "first");
        assert_eq!(picker_of(&seeded)["options"].as_array().unwrap().len(), 3);
        assert_eq!(
            picker_of(&seeded)["options"][0],
            picker_of(original)["options"][0]
        );
        let updated = sync_labels(&seeded, Some("claude-opus-native"), "first");
        let picker = picker_of(&updated);
        assert_eq!(picker["replaceBuiltInOptions"], true);
        assert_eq!(picker["options"][0], picker_of(original)["options"][0]);
        assert_eq!(picker["options"][1], picker_of(original)["options"][1]);
        assert_eq!(picker["options"][2]["model"], "sonnet");
        assert_eq!(picker["options"].as_array().unwrap().len(), 3);
        let switched = claim_text(&updated, "http://127.0.0.1:19000").unwrap();
        let refreshed = sync_labels(&switched, None, "second");
        assert_eq!(picker_of(&refreshed)["options"][0]["label"], "My alias");
        assert_eq!(
            picker_of(&refreshed)["options"][1]["label"],
            "My native model"
        );
        assert_eq!(
            picker_of(&release_text(&refreshed).unwrap()),
            picker_of(original)
        );
    }

    #[test]
    fn custom_picker_rows_and_labels_win_and_the_original_text_comes_back() {
        let original = r#"{
  "modelPicker": { "replaceBuiltInOptions": true, "other": 7, "options": [
    { "model": "opus", "label": "My Opus", "extra": "kept" },
    { "model": "my-model", "label": "Mine", "description": "My own model" }
  ] },
  "statusLine": { "type": "command", "command": "my-status" },
  "model": "sonnet",
  "env": { "ANTHROPIC_MODEL": "mine", "ANTHROPIC_DEFAULT_OPUS_MODEL": "my-opus" }
}
"#;
        let claimed = claim_text(original, URL).unwrap();
        let updated = sync_labels(&claimed, None, "first");
        let picker = picker_of(&updated);
        let original_picker = picker_of(original);
        assert_eq!(picker["replaceBuiltInOptions"], true);
        assert_eq!(picker["other"], 7);
        assert_eq!(picker["options"][0], original_picker["options"][0]);
        assert_eq!(picker["options"][1], original_picker["options"][1]);
        assert_eq!(picker["options"][2]["model"], "sonnet");
        assert_eq!(picker["options"].as_array().unwrap().len(), 3);
        let refreshed = sync_labels(&updated, Some("opus"), "second");
        assert_eq!(picker_of(&refreshed)["options"][0]["label"], "My Opus");
        let root: Value = serde_json::from_str(&refreshed).unwrap();
        assert_eq!(
            root["statusLine"],
            json!({"type":"command","command":"my-status"})
        );
        assert_eq!(root["model"], "sonnet");
        assert_eq!(root["env"]["ANTHROPIC_MODEL"], "mine");
        assert_eq!(root["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL"], "my-opus");
        assert_eq!(env_of(&claimed), env_of(&refreshed));
        let released = release_text(&refreshed).unwrap();
        assert_eq!(
            Document::parse(&released)
                .unwrap()
                .root
                .raw(MODEL_PICKER)
                .unwrap()
                .get(),
            Document::parse(original)
                .unwrap()
                .root
                .raw(MODEL_PICKER)
                .unwrap()
                .get()
        );
    }

    #[test]
    fn absent_null_and_existing_picker_modes_are_restored() {
        for original in [
            "{}",
            r#"{"modelPicker":null}"#,
            r#"{"modelPicker":{"replaceBuiltInOptions":false,"options":[]}}"#,
            r#"{"modelPicker":{"options":[]}}"#,
        ] {
            let claimed = claim_text(original, URL).unwrap();
            let updated = sync_labels(&claimed, None, "first");
            let before = Document::parse(original).unwrap();
            let owned = Document::parse(&updated)
                .unwrap()
                .state()
                .unwrap()
                .model_picker
                .unwrap();
            assert_eq!(
                owned.original.as_deref(),
                before.root.raw(MODEL_PICKER).map(RawValue::get)
            );
            assert_eq!(picker_of(&updated)["replaceBuiltInOptions"], true);
            let released = release_text(&updated).unwrap();
            assert_eq!(
                Document::parse(&released)
                    .unwrap()
                    .root
                    .raw(MODEL_PICKER)
                    .map(RawValue::get),
                before.root.raw(MODEL_PICKER).map(RawValue::get)
            );
        }
    }

    #[test]
    fn manual_picker_changes_pause_sync_and_survive_release() {
        let claimed = claim_text(r#"{"modelPicker":{"options":[]}}"#, URL).unwrap();
        let updated = sync_labels(&claimed, None, "first");
        for change in ["add", "edit", "remove", "delete", "null"] {
            let mut root: Value = serde_json::from_str(&updated).unwrap();
            match change {
                "add" => root[MODEL_PICKER]["options"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({
                        "model": "custom", "label": "My model"
                    })),
                "edit" => root[MODEL_PICKER]["options"][0]["label"] = json!("My label"),
                "remove" => {
                    root[MODEL_PICKER]["options"]
                        .as_array_mut()
                        .unwrap()
                        .remove(0);
                }
                "delete" => {
                    root.as_object_mut().unwrap().remove(MODEL_PICKER);
                }
                "null" => root[MODEL_PICKER] = Value::Null,
                _ => unreachable!(),
            }
            let edited = serde_json::to_string_pretty(&root).unwrap();
            let paused = sync_labels(&edited, Some("claude-opus-native"), "second");
            let state = Document::parse(&paused).unwrap().state().unwrap();
            assert!(state.model_picker.unwrap().paused, "{change}");
            assert_eq!(picker_of(&paused), picker_of(&edited));
            assert_eq!(sync_labels(&paused, None, "third"), paused);
            let reclaimed = claim_text(&paused, "http://127.0.0.1:19000").unwrap();
            assert_eq!(sync_labels(&reclaimed, None, "third"), reclaimed);
            let released = release_text(&reclaimed).unwrap();
            assert_eq!(picker_of(&released), picker_of(&edited));
            assert_eq!(
                Document::parse(&released)
                    .unwrap()
                    .root
                    .raw(MODEL_PICKER)
                    .is_some(),
                Document::parse(&edited)
                    .unwrap()
                    .root
                    .raw(MODEL_PICKER)
                    .is_some()
            );
            let reactivated = claim_text(&released, URL).unwrap();
            let new_labels = sync_labels(&reactivated, None, "fourth");
            if change == "delete" || change == "null" {
                assert_eq!(
                    picker_of(&new_labels)["options"][0]["label"],
                    "fourth/actual-model"
                );
            } else if change == "remove" {
                assert_eq!(
                    picker_of(&new_labels)["options"][0],
                    picker_of(&edited)["options"][0]
                );
                assert_eq!(picker_of(&new_labels)["options"][1]["model"], "opus");
            } else {
                assert_eq!(picker_of(&new_labels), picker_of(&edited));
            }
        }
    }

    #[test]
    fn a_paused_picker_stays_paused_if_the_user_puts_our_old_value_back() {
        let claimed = claim_text("{}", URL).unwrap();
        let updated = sync_labels(&claimed, None, "first");
        let mut root: Value = serde_json::from_str(&updated).unwrap();
        root[MODEL_PICKER]["options"][0]["label"] = json!("My label");
        let paused = sync_labels(&root.to_string(), None, "second");
        let mut root: Value = serde_json::from_str(&paused).unwrap();
        root[MODEL_PICKER] = picker_of(&updated);
        let reverted = root.to_string();
        assert_eq!(sync_labels(&reverted, None, "second"), reverted);
        let released = release_text(&reverted).unwrap();
        assert_eq!(picker_of(&released), picker_of(&updated));
        assert_eq!(env_of(&released)[BASE_URL], NEUTRAL_BASE_URL);
        assert!(Document::parse(&released).unwrap().state().is_none());
    }

    #[test]
    fn labels_do_not_activate_routing_or_adopt_an_unowned_url() {
        let claimed = claim_text("{}", URL).unwrap();
        let drifted = hand_edit(&claimed, BASE_URL, "http://127.0.0.1:9999");
        let released = release_text(&claimed).unwrap();
        for text in [
            "",
            "{}",
            HAND_EDITED,
            &drifted,
            &released,
            r#"{"cc-proxy":{"written":{}}}"#,
        ] {
            assert_eq!(
                sync_model_labels_text(text, &BASE_MODELS, None, &|_| panic!("not routed"))
                    .unwrap(),
                text
            );
        }
    }

    #[test]
    fn labels_accept_a_legacy_env_only_backup() {
        let legacy = json!({
            "env": { BASE_URL: URL, OAUTH_TOKEN: PLACEHOLDER_TOKEN },
            STATE_KEY: {
                "claimed": { BASE_URL: null, OAUTH_TOKEN: null },
                "written": { BASE_URL: URL, OAUTH_TOKEN: PLACEHOLDER_TOKEN }
            }
        })
        .to_string();
        let updated = sync_labels(&legacy, None, "first");
        assert_eq!(picker_of(&updated)["options"].as_array().unwrap().len(), 2);
        let released = release_text(&updated).unwrap();
        assert_eq!(env_of(&released)[BASE_URL], NEUTRAL_BASE_URL);
        assert!(
            Document::parse(&released)
                .unwrap()
                .root
                .raw(MODEL_PICKER)
                .is_none()
        );
    }

    #[test]
    fn a_lost_backup_leaves_picker_labels_alone() {
        let claimed = claim_text("{}", URL).unwrap();
        let updated = sync_labels(&claimed, None, "first");
        let mut root: Value = serde_json::from_str(&updated).unwrap();
        root.as_object_mut().unwrap().remove(STATE_KEY);
        let lost = root.to_string();
        assert_eq!(sync_labels(&lost, None, "second"), lost);
        let released = release_text(&lost).unwrap();
        assert_eq!(picker_of(&released), picker_of(&lost));
        let reclaimed = claim_text(&lost, URL).unwrap();
        let refreshed = sync_labels(&reclaimed, None, "second");
        assert_eq!(
            refreshed, reclaimed,
            "existing labels now belong to the user"
        );
        assert_eq!(
            picker_of(&release_text(&refreshed).unwrap()),
            picker_of(&lost)
        );
    }

    #[test]
    fn damaged_picker_metadata_keeps_the_routing_backup_and_picker_data() {
        let claimed = claim_text(HAND_EDITED, URL).unwrap();
        let updated = sync_labels(&claimed, None, "first");
        for missing in ["original", "written"] {
            let mut root: Value = serde_json::from_str(&updated).unwrap();
            root[STATE_KEY]["model_picker"]
                .as_object_mut()
                .unwrap()
                .remove(missing);
            let damaged = root.to_string();
            let state = Document::parse(&damaged).unwrap().state().unwrap();
            assert!(state.model_picker.is_none(), "{missing}");
            assert_eq!(sync_labels(&damaged, None, "second"), damaged);
            let released = release_text(&damaged).unwrap();
            assert_eq!(picker_of(&released), picker_of(&damaged));
            assert_eq!(env_of(&released)[BASE_URL], "https://gateway.example.com");
        }
        for metadata in [
            json!("broken"),
            json!([]),
            json!({"original": false, "written": {}}),
            json!({"original": "not json", "written": picker_of(&updated)}),
            json!({"original": "[]", "written": picker_of(&updated)}),
            json!({"original": null, "written": null}),
        ] {
            let mut root: Value = serde_json::from_str(&updated).unwrap();
            root[STATE_KEY]["model_picker"] = metadata;
            let damaged = root.to_string();
            let released = release_text(&damaged).unwrap();
            assert_eq!(picker_of(&released), picker_of(&damaged));
            assert_eq!(env_of(&released)[BASE_URL], "https://gateway.example.com");
        }
    }

    #[test]
    fn invalid_picker_shapes_refuse_only_the_label_update() {
        for picker in [
            json!([]),
            json!(false),
            json!({"options": null}),
            json!({"options": {}}),
            json!({"replaceBuiltInOptions":"yes"}),
            json!({"options":["opus"]}),
            json!({"options":[{"model":"opus"}]}),
            json!({"options":[{"model":1,"label":"Opus"}]}),
            json!({"options":[{"model":"opus","label":"Opus","description":null}]}),
        ] {
            let claimed = claim_text(&json!({MODEL_PICKER:picker}).to_string(), URL).unwrap();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("settings.json");
            std::fs::write(&path, &claimed).unwrap();
            let error = edit(&path, |text| {
                sync_model_labels_text(text, &BASE_MODELS, None, &|model| {
                    Some(ModelLabel {
                        model: model.into(),
                        label: "provider/model".into(),
                        description: "Uses provider/model".into(),
                    })
                })
            })
            .unwrap_err()
            .to_string();
            assert!(error.contains("modelPicker"), "{error}");
            assert!(error.contains("Fix it"), "{error}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), claimed);
            assert_eq!(env_of(&claimed)[BASE_URL], URL);
            assert!(release_text(&claimed).is_ok());
        }
    }

    #[test]
    fn unchanged_labels_leave_the_files_text_and_timestamp_alone() {
        let claimed = claim_text("{}", URL).unwrap();
        let updated = sync_labels(&claimed, None, "first");
        let compact = serde_json::from_str::<Value>(&updated).unwrap().to_string();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, &compact).unwrap();
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert!(!edit(&path, |text| Ok(sync_labels(text, None, "first"))).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), compact);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before
        );
    }

    #[test]
    fn duplicate_ids_resolve_once_and_unresolved_observations_are_not_saved() {
        let claimed = claim_text("{}", URL).unwrap();
        let calls = std::cell::RefCell::new(Vec::new());
        let updated = sync_model_labels_text(
            &claimed,
            &["opus", "opus", "sonnet"],
            Some("custom"),
            &|model| {
                calls.borrow_mut().push(model.to_string());
                (model != "custom").then(|| ModelLabel {
                    model: model.into(),
                    label: "provider/model".into(),
                    description: "Uses provider/model".into(),
                })
            },
        )
        .unwrap();
        assert_eq!(*calls.borrow(), ["opus", "sonnet", "custom"]);
        assert_eq!(picker_of(&updated)["options"].as_array().unwrap().len(), 2);
        assert!(
            Document::parse(&updated)
                .unwrap()
                .state()
                .unwrap()
                .model_picker
                .unwrap()
                .observed
                .is_empty()
        );
        assert_eq!(
            sync_model_labels_text(&claimed, &BASE_MODELS, None, &|_| None).unwrap(),
            claimed
        );
        assert!(
            sync_model_labels_text(&claimed, &BASE_MODELS, None, &|_| Some(ModelLabel {
                model: "different-id".into(),
                label: "provider/model".into(),
                description: "Uses provider/model".into()
            }))
            .is_err()
        );
    }

    #[test]
    fn serializing_the_backup_is_stable() {
        let state = State {
            claimed: json!({"A": null}).as_object().unwrap().clone(),
            written: json!({"A": "x"}).as_object().unwrap().clone(),
            ..State::default()
        };
        assert_eq!(
            pretty(&state, "  ").unwrap(),
            "{\n  \"claimed\": {\n    \"A\": null\n  },\n  \"written\": {\n    \"A\": \"x\"\n  }\n}"
        );
    }
}
