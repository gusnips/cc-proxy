//! `cc-proxy config get/set/list/edit`: inspect and change config.json
//! through dotted keys (`port`, `opencode.apiKey`, `codex.fullLane`). With no
//! subcommand, `cc-proxy config` opens the same keys as a screen (`interactive`).
//!
//! Reads show the FILE value — the thing `set` manages. When an environment
//! variable overrides a key at runtime, get/list say so instead of
//! pretending the file wins. Secret keys never print their value.

mod editor;
mod interactive;

pub use interactive::{SettingsOutcome, SettingsPanel};

use anyhow::{Context, Result};

use crate::ui::{self, Mood};
use crate::{config, paths};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigKind {
    /// Non-empty string.
    Str,
    /// TCP port 1-65535.
    Port,
    /// true/false (also 1/0, yes/no, on/off).
    Bool,
    /// Codex auto-review effort, or off to inherit ordinary effort.
    AutoReviewEffort,
    /// Codex tier, validated by the request translator.
    ServiceTier,
    /// Like Str, but get/list only ever print `set`/`unset`.
    Secret,
}

#[derive(Debug, Clone, Copy)]
struct ConfigKey {
    path: &'static str,
    kind: ConfigKind,
    /// Env vars that override the file at runtime, in precedence order.
    env: &'static [&'static str],
    default: Option<&'static str>,
    blurb: &'static str,
}

impl ConfigKey {
    /// The fixed values left and right step through, for a key that has them.
    /// These only offer values: `parse_value` still decides what is accepted.
    fn choices(&self) -> Option<&'static [&'static str]> {
        match self.kind {
            ConfigKind::Bool => Some(&["true", "false"]),
            ConfigKind::AutoReviewEffort => {
                Some(&["off", "none", "low", "medium", "high", "xhigh", "max"])
            }
            ConfigKind::ServiceTier => Some(&["fast", "priority", "ultrafast", "flex"]),
            ConfigKind::Str | ConfigKind::Port | ConfigKind::Secret => None,
        }
    }

    /// Read once at startup, so a running proxy keeps the old value until it restarts.
    fn needs_restart(&self) -> bool {
        matches!(self.path, "port" | "bindAddress")
    }
}

static CONFIG_KEYS: &[ConfigKey] = &[
    ConfigKey {
        path: "auth.autoRotate",
        kind: ConfigKind::Bool,
        env: &[],
        default: Some("false"),
        blurb: "switch saved accounts when one reaches a limit with a retry time",
    },
    ConfigKey {
        path: "auth.hourlyPing",
        kind: ConfigKind::Bool,
        env: &["CCP_HOURLY_PING"],
        default: Some("false"),
        blurb: "every hour, ping each saved account of the routed providers with a 5h window, to start it early",
    },
    ConfigKey {
        path: "port",
        kind: ConfigKind::Port,
        env: &["PORT"],
        default: Some("18765"),
        blurb: "listener port",
    },
    ConfigKey {
        path: "bindAddress",
        kind: ConfigKind::Str,
        env: &["CCP_BIND_ADDRESS"],
        default: Some("127.0.0.1"),
        blurb: "listener address",
    },
    ConfigKey {
        path: "autoReviewModel",
        kind: ConfigKind::Str,
        env: &["CCP_AUTO_REVIEW_MODEL"],
        default: None,
        blurb: "model for background review passes",
    },
    ConfigKey {
        path: "autoReviewEffort",
        kind: ConfigKind::AutoReviewEffort,
        env: &["CCP_AUTO_REVIEW_EFFORT"],
        default: None,
        blurb: "effort for routed Codex security reviews (none|low|medium|high|xhigh|max; off inherits)",
    },
    ConfigKey {
        path: "log.verbose",
        kind: ConfigKind::Bool,
        env: &["CCP_LOG_VERBOSE"],
        default: Some("false"),
        blurb: "verbose request logging",
    },
    ConfigKey {
        path: "log.stderr",
        kind: ConfigKind::Bool,
        env: &["CCP_LOG_STDERR"],
        default: Some("false"),
        blurb: "log to stderr instead of the log file",
    },
    ConfigKey {
        path: "opencode.apiKey",
        kind: ConfigKind::Secret,
        env: &["CCP_OPENCODE_API_KEY", "OPENCODE_API_KEY"],
        default: None,
        blurb: "OpenCode Go API key",
    },
    ConfigKey {
        path: "opencode.baseUrl",
        kind: ConfigKind::Str,
        env: &["CCP_OPENCODE_BASE_URL"],
        default: Some("https://opencode.ai/zen/go/v1"),
        blurb: "OpenCode Go API base URL",
    },
    ConfigKey {
        path: "opencodeZen.apiKey",
        kind: ConfigKind::Secret,
        env: &["CCP_OPENCODE_ZEN_API_KEY", "OPENCODE_ZEN_API_KEY"],
        default: None,
        blurb: "OpenCode Zen API key",
    },
    ConfigKey {
        path: "opencodeZen.baseUrl",
        kind: ConfigKind::Str,
        env: &["CCP_OPENCODE_ZEN_BASE_URL"],
        default: Some("https://opencode.ai/zen/v1"),
        blurb: "OpenCode Zen API base URL",
    },
    ConfigKey {
        path: "anthropic.apiKey",
        kind: ConfigKind::Secret,
        env: &["CCP_ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"],
        default: None,
        blurb: "Anthropic API key",
    },
    ConfigKey {
        path: "anthropic.baseUrl",
        kind: ConfigKind::Str,
        env: &["CCP_ANTHROPIC_BASE_URL"],
        default: Some("https://api.anthropic.com"),
        blurb: "Anthropic API base URL",
    },
    ConfigKey {
        path: "gemini.apiKey",
        kind: ConfigKind::Secret,
        env: &["CCP_GEMINI_API_KEY", "GEMINI_API_KEY", "GOOGLE_API_KEY"],
        default: None,
        blurb: "Gemini API key",
    },
    ConfigKey {
        path: "gemini.baseUrl",
        kind: ConfigKind::Str,
        env: &["CCP_GEMINI_BASE_URL"],
        default: Some("https://generativelanguage.googleapis.com/v1beta"),
        blurb: "Gemini API base URL",
    },
    ConfigKey {
        path: "codex.baseUrl",
        kind: ConfigKind::Str,
        env: &["CCP_CODEX_BASE_URL"],
        default: None,
        blurb: "Codex API base URL override",
    },
    ConfigKey {
        path: "codex.serviceTier",
        kind: ConfigKind::ServiceTier,
        env: &["CCP_CODEX_SERVICE_TIER"],
        default: None,
        blurb: "Codex Messages tier (fast|priority|ultrafast|flex)",
    },
    ConfigKey {
        path: "codex.fullLane",
        kind: ConfigKind::Bool,
        env: &["CCP_CODEX_FULL_LANE"],
        default: Some("false"),
        blurb: "escape Responses Lite for sol/terra",
    },
    ConfigKey {
        path: "codex.serverCompaction",
        kind: ConfigKind::Bool,
        env: &["CCP_CODEX_SERVER_COMPACTION"],
        default: Some("false"),
        blurb: "server-side conversation compaction",
    },
    ConfigKey {
        path: "codex.responsesApi",
        kind: ConfigKind::Bool,
        env: &["CCP_CODEX_RESPONSES_API"],
        default: Some("false"),
        blurb: "prefer the Responses API transport",
    },
    ConfigKey {
        path: "codex.imagesApi",
        kind: ConfigKind::Bool,
        env: &["CCP_CODEX_IMAGES_API"],
        default: Some("false"),
        blurb: "enable the images API",
    },
    ConfigKey {
        path: "codex.transcriptionsApi",
        kind: ConfigKind::Bool,
        env: &["CCP_CODEX_TRANSCRIPTIONS_API"],
        default: Some("false"),
        blurb: "enable the transcriptions API",
    },
    ConfigKey {
        path: "codex.previousResponseId",
        kind: ConfigKind::Bool,
        env: &["CCP_CODEX_PREVIOUS_RESPONSE_ID"],
        default: Some("false"),
        blurb: "chain previous_response_id across turns",
    },
];

fn find_key(name: &str) -> Result<&'static ConfigKey> {
    CONFIG_KEYS
        .iter()
        .find(|key| key.path == name)
        .with_context(|| {
            let known: Vec<_> = CONFIG_KEYS.iter().map(|key| key.path).collect();
            format!(
                "unknown config key `{name}`; see `cc-proxy config list`:\n{}",
                known.join(", ")
            )
        })
}

fn parse_bool(raw: &str) -> Result<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => anyhow::bail!("expected true/false, got `{raw}`"),
    }
}

fn parse_value(key: &ConfigKey, raw: &str) -> Result<serde_json::Value> {
    match key.kind {
        ConfigKind::Str | ConfigKind::Secret => {
            if raw.trim().is_empty() {
                anyhow::bail!("{} must not be empty", key.path);
            }
            Ok(serde_json::Value::String(raw.trim().to_string()))
        }
        ConfigKind::Port => {
            let port: u16 = raw
                .trim()
                .parse()
                .context(format!("{} must be a port 1-65535", key.path))?;
            if port == 0 {
                anyhow::bail!("{} must be a port 1-65535", key.path);
            }
            Ok(serde_json::Value::from(port))
        }
        ConfigKind::Bool => Ok(serde_json::Value::from(parse_bool(raw)?)),
        ConfigKind::AutoReviewEffort => {
            let value = raw.trim();
            if value != "off" {
                crate::providers::codex::translate::request::resolve_effort_override(
                    None,
                    Some(value),
                )?;
            }
            Ok(serde_json::Value::String(value.to_string()))
        }
        ConfigKind::ServiceTier => {
            let value = raw.trim();
            crate::providers::codex::translate::request::normalize_service_tier(value)?;
            Ok(serde_json::Value::String(value.to_string()))
        }
    }
}

fn get_dotted<'a>(root: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut current = root;
    for part in path.split('.') {
        current = current.get(part)?;
    }
    Some(current)
}

/// Set one dotted key. A missing or null step becomes an object. A step that
/// holds anything else (a string, a list) is the user's own data: it stays as
/// it is and the caller gets an error that names it.
fn set_dotted(root: &mut serde_json::Value, path: &str, value: serde_json::Value) -> Result<()> {
    set_under(root, path, path, value)
}

/// `rest` is the part of `full_path` still to walk from `node`.
fn set_under(
    node: &mut serde_json::Value,
    rest: &str,
    full_path: &str,
    value: serde_json::Value,
) -> Result<()> {
    if node.is_null() {
        *node = serde_json::json!({});
    }
    let Some(map) = node.as_object_mut() else {
        let walked = full_path
            .strip_suffix(rest)
            .map_or("", |walked| walked.trim_end_matches('.'));
        let holder = if walked.is_empty() {
            "config.json".to_string()
        } else {
            format!("`{walked}` in config.json")
        };
        anyhow::bail!(
            "{holder} isn't a JSON object, so `{full_path}` can't be saved. \
             Fix it with `cc-proxy config edit`, then try again."
        );
    };
    match rest.split_once('.') {
        None => {
            map.insert(rest.to_string(), value);
            Ok(())
        }
        Some((head, rest)) => {
            let child = map
                .entry(head.to_string())
                .or_insert(serde_json::Value::Null);
            set_under(child, rest, full_path, value)
        }
    }
}

/// Remove one dotted key, and any object it leaves empty.
fn remove_dotted(root: &mut serde_json::Value, path: &str) {
    let Some((head, rest)) = path.split_once('.') else {
        if let Some(map) = root.as_object_mut() {
            map.remove(path);
        }
        return;
    };
    let Some(child) = root.get_mut(head) else {
        return;
    };
    remove_dotted(child, rest);
    if child.as_object().is_some_and(serde_json::Map::is_empty)
        && let Some(map) = root.as_object_mut()
    {
        map.remove(head);
    }
}

fn scalar_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        serde_json::Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

/// First runtime env override for a key, if any is set and non-empty.
fn env_override(key: &ConfigKey) -> Option<(&'static str, String)> {
    key.env.iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.is_empty())
            .map(|value| (*name, value))
    })
}

fn display_value(key: &ConfigKey, file: Option<String>) -> String {
    if let Some((name, value)) = env_override(key) {
        return if key.kind == ConfigKind::Secret {
            format!("set (from ${name})")
        } else {
            format!("{value} (from ${name}; overrides config.json)")
        };
    }
    if key.path == "opencode.apiKey" && config::opencode_api_key_source() == Some("claude-settings")
    {
        return "set (from claude-settings)".to_string();
    }
    file_display_value(key, file)
}

/// The stored value and its default, without reading the environment or credentials.
fn file_display_value(key: &ConfigKey, file: Option<String>) -> String {
    if key.kind == ConfigKind::Secret {
        return if file.is_some() {
            "set (config.json)".to_string()
        } else {
            "unset".to_string()
        };
    }
    if let Some(value) = file {
        return value;
    }
    match key.default {
        Some(default) => format!("(default {default})"),
        None => "unset".to_string(),
    }
}

fn read_file() -> Result<serde_json::Value> {
    let dir = paths::config_dir();
    let current = dir.join("config.json");
    if current
        .try_exists()
        .with_context(|| format!("can't check for {}", current.display()))?
    {
        return config::read_config_json_at(&dir);
    }
    // No current file yet: honor a pre-fork config so reads see the same
    // settings the typed accessors resolve.
    let legacy = paths::legacy_config_dir(&paths::DirResolverEnv::default());
    if legacy != dir {
        return config::read_config_json_at(&legacy);
    }
    Ok(serde_json::json!({}))
}

pub fn run_config_get(name: &str) -> Result<()> {
    let key = find_key(name)?;
    let root = read_file()?;
    let file = get_dotted(&root, key.path).and_then(scalar_text);
    if file.is_none() && key.default.is_none() && env_override(key).is_none() {
        let secret_hint = if key.kind == ConfigKind::Secret {
            " (secrets never print; this only reports set/unset)"
        } else {
            ""
        };
        anyhow::bail!("{0} is not set{secret_hint}", key.path);
    }
    println!("{}", display_value(key, file));
    Ok(())
}

pub fn run_config_list() -> Result<()> {
    let root = read_file()?;
    let mut rows = Vec::new();
    let mut width = 0;
    for key in CONFIG_KEYS {
        let file = get_dotted(&root, key.path).and_then(scalar_text);
        let value = display_value(key, file);
        width = width.max(format!("{} = {value}", key.path).len());
        rows.push((key, value));
    }
    for (key, value) in rows {
        println!(
            "{:<width$}  # {}",
            format!("{} = {value}", key.path),
            key.blurb,
            width = width
        );
    }
    println!("\nValues live in config.json; $VAR overrides apply at runtime.");
    Ok(())
}

/// Write one dotted key to config.json, keeping every other key. Returns
/// the file written.
pub(crate) fn write_value(path: &str, value: serde_json::Value) -> Result<std::path::PathBuf> {
    write_values([(path, value)])
}

/// Remove dotted keys from config.json, keeping every other key.
pub(crate) fn remove_values<'a>(paths_to_remove: impl IntoIterator<Item = &'a str>) -> Result<()> {
    let mut root = read_file()?;
    for path in paths_to_remove {
        remove_dotted(&mut root, path);
    }
    config::write_config_json_at(&paths::config_dir(), &root)?;
    Ok(())
}

/// Write several dotted keys in one step, so a proxy that reads the file per
/// request never sees half of the change.
pub(crate) fn write_values<'a>(
    values: impl IntoIterator<Item = (&'a str, serde_json::Value)>,
) -> Result<std::path::PathBuf> {
    let mut root = read_file()?;
    for (path, value) in values {
        set_dotted(&mut root, path, value)?;
    }
    config::write_config_json_at(&paths::config_dir(), &root)
}

/// What a save did, for `config set` and the settings screen to word their own way.
struct Applied {
    path: std::path::PathBuf,
    /// The environment variable that still wins over the file at runtime.
    overridden_by: Option<&'static str>,
    restart: bool,
}

/// Check `raw` for `key` and write it to config.json.
fn apply(key: &ConfigKey, raw: &str) -> Result<Applied> {
    let value = parse_value(key, raw)?;
    Ok(Applied {
        path: write_value(key.path, value)?,
        overridden_by: env_override(key).map(|(name, _)| name),
        restart: key.needs_restart(),
    })
}

pub fn run_config_set(name: &str, raw: &str) -> Result<()> {
    let key = find_key(name)?;
    let applied = apply(key, raw)?;
    let saved = if key.kind == ConfigKind::Secret {
        format!("{} saved", key.path)
    } else {
        format!("{} = {}", key.path, raw.trim())
    };
    let mut lines = vec![saved, format!("Written to {}", applied.path.display())];
    if let Some(name) = applied.overridden_by {
        lines.push(format!("${name} is set and wins at runtime."));
    }
    if applied.restart {
        lines.push("Run `cc-proxy restart` to apply it to the running proxy.".into());
    }
    ui::print_note(Mood::Glad, &lines);
    Ok(())
}

pub fn run_config_edit() -> Result<()> {
    editor::open(None)
}

/// `cc-proxy config` with no subcommand: the settings screen in a terminal,
/// the list of settings anywhere else.
pub fn run_config_screen() -> Result<()> {
    if crate::prompt::is_interactive() {
        interactive::run()
    } else {
        run_config_list()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotted_get_and_set_round_trip() {
        let mut root = serde_json::json!({"port": 1});
        assert_eq!(
            get_dotted(&root, "opencode.apiKey").and_then(scalar_text),
            None
        );
        set_dotted(&mut root, "opencode.apiKey", serde_json::json!("k")).unwrap();
        assert_eq!(
            get_dotted(&root, "opencode.apiKey").and_then(scalar_text),
            Some("k".to_string())
        );
        assert_eq!(
            get_dotted(&root, "port").and_then(scalar_text),
            Some("1".to_string())
        );
        // A null step is as good as a missing one.
        let mut root = serde_json::json!({"log": null});
        set_dotted(&mut root, "log.verbose", serde_json::json!(true)).unwrap();
        assert_eq!(
            get_dotted(&root, "log.verbose").and_then(scalar_text),
            Some("true".to_string())
        );
    }

    #[test]
    fn setting_a_key_never_overwrites_a_value_that_is_not_an_object() {
        let mut root = serde_json::json!({"log": "nope", "port": 1});
        let error = set_dotted(&mut root, "log.verbose", serde_json::json!(true))
            .unwrap_err()
            .to_string();
        assert!(error.contains("`log` in config.json"), "{error}");
        assert!(error.contains("`log.verbose`"), "{error}");
        assert_eq!(root, serde_json::json!({"log": "nope", "port": 1}));

        let mut root = serde_json::json!(["not", "an", "object"]);
        let error = set_dotted(&mut root, "port", serde_json::json!(2))
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("config.json isn't a JSON object"),
            "{error}"
        );
        assert_eq!(root, serde_json::json!(["not", "an", "object"]));
    }

    #[test]
    fn removing_a_dotted_key_drops_the_objects_it_leaves_empty() {
        let mut root = serde_json::json!({
            "port": 1,
            "route": {"on": true, "tiers": {"codex": {"opus": "a", "haiku": "b"}}}
        });
        remove_dotted(&mut root, "route.tiers.codex.opus");
        assert_eq!(
            root["route"]["tiers"]["codex"],
            serde_json::json!({"haiku": "b"})
        );
        remove_dotted(&mut root, "route.tiers.codex");
        assert_eq!(root["route"], serde_json::json!({"on": true}));
        remove_dotted(&mut root, "route.tiers.codex");
        remove_dotted(&mut root, "missing.key");
        assert_eq!(root["port"], 1);
    }

    #[test]
    fn service_tier_config_key_reuses_wire_validation() {
        let key = find_key("codex.serviceTier").unwrap();
        assert_eq!(key.env, &["CCP_CODEX_SERVICE_TIER"]);
        for value in ["fast", "priority", "ultrafast", "flex"] {
            assert_eq!(parse_value(key, value).unwrap(), serde_json::json!(value));
        }
        for value in ["", "turbo", "Ultrafast"] {
            assert!(parse_value(key, value).is_err());
        }
    }

    #[test]
    fn auto_review_effort_config_key_validates_values_and_off() {
        let key = find_key("autoReviewEffort").unwrap();
        assert_eq!(key.env, &["CCP_AUTO_REVIEW_EFFORT"]);
        assert_eq!(key.default, None);
        for value in ["none", "low", "medium", "high", "xhigh", "max", "off"] {
            assert_eq!(parse_value(key, value).unwrap(), serde_json::json!(value));
        }
        for value in ["", "bogus", "LOW"] {
            assert!(parse_value(key, value).is_err());
        }
    }

    #[test]
    fn every_choice_is_accepted_by_its_own_validator() {
        for key in CONFIG_KEYS {
            for choice in key.choices().into_iter().flatten() {
                assert_eq!(
                    parse_value(key, choice).unwrap_or_else(|error| panic!(
                        "{} rejects its own choice `{choice}`: {error}",
                        key.path
                    )),
                    match key.kind {
                        ConfigKind::Bool => serde_json::json!(*choice == "true"),
                        _ => serde_json::json!(choice),
                    },
                    "{}",
                    key.path
                );
            }
        }
        assert!(find_key("port").unwrap().choices().is_none());
        assert!(find_key("opencode.apiKey").unwrap().choices().is_none());
    }

    #[test]
    fn only_startup_settings_ask_for_a_restart() {
        let restart: Vec<_> = CONFIG_KEYS
            .iter()
            .filter(|key| key.needs_restart())
            .map(|key| key.path)
            .collect();
        assert_eq!(restart, ["port", "bindAddress"]);
    }

    #[test]
    fn values_validate_per_kind() {
        let port = find_key("port").unwrap();
        assert!(parse_value(port, "8080").is_ok());
        assert!(parse_value(port, "abc").is_err());
        assert!(parse_value(port, "0").is_err());

        let flag = find_key("codex.fullLane").unwrap();
        assert_eq!(parse_value(flag, "ON").unwrap(), serde_json::json!(true));
        assert!(parse_value(flag, "maybe").is_err());

        let secret = find_key("opencode.apiKey").unwrap();
        assert!(parse_value(secret, "  ").is_err());

        assert!(find_key("nope.nothing").is_err());
    }
}
