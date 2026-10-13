//! The `env` block cc-proxy keeps in Claude Code's user settings.
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

use crate::{fsutil, paths};

/// Top-level key that holds the backup.
const STATE_KEY: &str = "cc-proxy";

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

/// What cc-proxy wrote, and what it replaced.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct State {
    /// The user's value before we took the key over. `null` when there was none.
    #[serde(default)]
    claimed: Map<String, Value>,
    /// The value we put there. A key that no longer holds it was edited since.
    #[serde(default)]
    written: Map<String, Value>,
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
    }

    #[test]
    fn serializing_the_backup_is_stable() {
        let state = State {
            claimed: json!({"A": null}).as_object().unwrap().clone(),
            written: json!({"A": "x"}).as_object().unwrap().clone(),
        };
        assert_eq!(
            pretty(&state, "  ").unwrap(),
            "{\n  \"claimed\": {\n    \"A\": null\n  },\n  \"written\": {\n    \"A\": \"x\"\n  }\n}"
        );
    }
}
