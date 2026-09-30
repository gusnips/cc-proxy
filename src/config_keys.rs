//! `cc-proxy config get/set/list/edit`: inspect and change config.json
//! through dotted keys (`port`, `opencode.apiKey`, `codex.fullLane`).
//!
//! Reads show the FILE value — the thing `set` manages. When an environment
//! variable overrides a key at runtime, get/list say so instead of
//! pretending the file wins. Secret keys never print their value.

use anyhow::{Context, Result};

use crate::{config, paths};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigKind {
    /// Non-empty string.
    Str,
    /// TCP port 1-65535.
    Port,
    /// true/false (also 1/0, yes/no, on/off).
    Bool,
    /// One of a fixed set.
    Provider,
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

const PROVIDERS: &[&str] = &["codex", "kimi"];

static CONFIG_KEYS: &[ConfigKey] = &[
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
        path: "aliasProvider",
        kind: ConfigKind::Provider,
        env: &["CCP_ALIAS_PROVIDER"],
        default: Some("codex"),
        blurb: "provider for bare model IDs (codex|kimi)",
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
        ConfigKind::Provider => {
            let value = raw.trim().to_string();
            if !PROVIDERS.contains(&value.as_str()) {
                anyhow::bail!(
                    "{} must be one of {}; got `{raw}`",
                    key.path,
                    PROVIDERS.join("|")
                );
            }
            Ok(serde_json::Value::String(value))
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

fn set_dotted(root: &mut serde_json::Value, path: &str, value: serde_json::Value) {
    let mut parts: Vec<&str> = path.split('.').collect();
    let Some(last) = parts.pop() else {
        return;
    };
    let mut current = root;
    for part in parts {
        if !current.is_object() {
            *current = serde_json::json!({});
        }
        current = match current {
            serde_json::Value::Object(map) => map
                .entry(part.to_string())
                .or_insert(serde_json::Value::Null),
            _ => unreachable!("ensured object above"),
        };
    }
    if !current.is_object() {
        *current = serde_json::json!({});
    }
    if let serde_json::Value::Object(map) = current {
        map.insert(last.to_string(), value);
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
    if key.kind == ConfigKind::Secret {
        if let Some((name, _)) = env_override(key) {
            return format!("set (from ${name})");
        }
        if key.path == "opencode.apiKey"
            && let Some(source) = config::opencode_api_key_source()
            && source == "claude-settings"
        {
            return "set (from claude-settings)".to_string();
        }
        return if file.is_some() {
            "set (config.json)".to_string()
        } else {
            "unset".to_string()
        };
    }
    if let Some((name, value)) = env_override(key) {
        return format!("{value} (from ${name}; overrides config.json)");
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
    if dir.join("config.json").exists() {
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

pub fn run_config_set(name: &str, raw: &str) -> Result<()> {
    let key = find_key(name)?;
    let value = parse_value(key, raw)?;
    let dir = paths::config_dir();
    let mut root = read_file()?;
    if !root.is_object() {
        root = serde_json::json!({});
    }
    set_dotted(&mut root, key.path, value);
    let path = config::write_config_json_at(&dir, &root)?;
    if key.kind == ConfigKind::Secret {
        println!("{} saved to {}.", key.path, path.display());
    } else {
        println!("{} = {} ({}).", key.path, raw.trim(), path.display());
    }
    for name in key.env {
        if std::env::var(name).ok().filter(|v| !v.is_empty()).is_some() {
            println!("Note: ${name} is set and takes precedence at runtime.");
            break;
        }
    }
    if key.path == "port" {
        println!("Restart the service (`cc-proxy restart`) to rebind.");
    }
    Ok(())
}

pub fn run_config_edit() -> Result<()> {
    let dir = paths::config_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("config.json");
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .context("set $VISUAL or $EDITOR to choose an editor")?;
    let status = std::process::Command::new(&editor)
        .arg(&path)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .with_context(|| format!("failed to launch editor `{editor}`"))?;
    if !status.success() {
        anyhow::bail!("editor exited with {status}");
    }
    // Validate on close so a typo does not silently break the next start.
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    if !raw.trim().is_empty() {
        serde_json::from_str::<serde_json::Value>(&raw)
            .with_context(|| format!("{} is not valid JSON after editing", path.display()))?;
    }
    Ok(())
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
        set_dotted(&mut root, "opencode.apiKey", serde_json::json!("k"));
        assert_eq!(
            get_dotted(&root, "opencode.apiKey").and_then(scalar_text),
            Some("k".to_string())
        );
        assert_eq!(
            get_dotted(&root, "port").and_then(scalar_text),
            Some("1".to_string())
        );
        // Non-object intermediate values are replaced, not merged into.
        let mut root = serde_json::json!({"log": "nope"});
        set_dotted(&mut root, "log.verbose", serde_json::json!(true));
        assert_eq!(
            get_dotted(&root, "log.verbose").and_then(scalar_text),
            Some("true".to_string())
        );
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
    fn values_validate_per_kind() {
        let port = find_key("port").unwrap();
        assert!(parse_value(port, "8080").is_ok());
        assert!(parse_value(port, "abc").is_err());
        assert!(parse_value(port, "0").is_err());

        let flag = find_key("codex.fullLane").unwrap();
        assert_eq!(parse_value(flag, "ON").unwrap(), serde_json::json!(true));
        assert!(parse_value(flag, "maybe").is_err());

        let provider = find_key("aliasProvider").unwrap();
        assert!(parse_value(provider, "kimi").is_ok());
        assert!(parse_value(provider, "Muse").is_err());

        let secret = find_key("opencode.apiKey").unwrap();
        assert!(parse_value(secret, "  ").is_err());

        assert!(find_key("nope.nothing").is_err());
    }
}
