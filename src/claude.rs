//! `cc-proxy claude`: start Claude Code on the proxy.
//!
//! Resume, worktree and other launch flags keep working. The connection rides
//! in one `--settings` JSON, merged with the caller's own `--settings` if any.
//! Claude Code ranks it above the user's and the project's settings files,
//! so saved connection values can't send the session elsewhere. No file changes.

use std::ffi::{OsStr, OsString};
use std::process::Command;

use anyhow::{Context, Result, ensure};

use crate::ui::{self, Mood};
use crate::{config, daemon};

/// Claude Code's connection values for this session, sent in `--settings`.
fn env(base_url: &str) -> Vec<(&'static str, String)> {
    let mut env = vec![
        ("ANTHROPIC_BASE_URL", base_url.to_string()),
        // Claude Code asks for a credential; the proxy never forwards it. An
        // OAuth token, not ANTHROPIC_AUTH_TOKEN: a session that sets that one is
        // not a claude.ai subscriber, so Claude Code ignores the usage-limit
        // headers. With this one it reads them, shows when the limit resets and
        // can continue the task by itself once it has.
        ("CLAUDE_CODE_OAUTH_TOKEN", "unused".to_string()),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1".to_string()),
        // Without it, a stream that fails part-way is sent again without
        // streaming, which can run its tool calls twice.
        ("CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK", "1".to_string()),
        (
            "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY",
            "1".to_string(),
        ),
    ];
    if let Some(model) = config::claude_model() {
        env.push(("ANTHROPIC_MODEL", model));
    }
    if let Some(model) = config::claude_fast_model() {
        env.push(("ANTHROPIC_DEFAULT_HAIKU_MODEL", model));
    }
    env
}

/// One `--settings` flag: Claude Code uses only the last occurrence. Preserve
/// the caller's settings, then override just the proxy connection values.
fn proxy_args(args: Vec<OsString>, base_url: &str) -> Result<Vec<OsString>> {
    let mut settings_source = None;
    let mut forwarded = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--" {
            forwarded.push(arg);
            forwarded.extend(args);
            break;
        }
        let source = if arg == "--settings" {
            Some(
                args.next()
                    .context("`--settings` needs a JSON string or a file path.")?,
            )
        } else {
            arg.to_str()
                .and_then(|arg| arg.strip_prefix("--settings="))
                .map(OsString::from)
        };
        if let Some(source) = source {
            settings_source = Some(source);
        } else {
            forwarded.push(arg);
        }
    }
    let mut settings = settings_source
        .as_deref()
        .map(read_settings)
        .transpose()?
        .unwrap_or_default();
    let settings_env = settings
        .entry("env")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .context(
            "The `env` in your `--settings` must be a JSON object. Use named values such as {\"DEBUG\":\"1\"}.",
        )?;
    settings_env.extend(
        env(base_url)
            .into_iter()
            .map(|(name, value)| (name.into(), value.into())),
    );
    let mut out = vec![
        OsString::from("--settings"),
        serde_json::Value::Object(settings).to_string().into(),
    ];
    out.extend(forwarded);
    Ok(out)
}

fn read_settings(source: &OsStr) -> Result<serde_json::Map<String, serde_json::Value>> {
    if let Some(json) = source
        .to_str()
        .filter(|text| text.trim_start().starts_with('{'))
    {
        return serde_json::from_str(json)
            .context("Couldn't read your `--settings` JSON. Check its syntax and try again.");
    }
    let path = std::path::Path::new(source);
    let metadata = std::fs::metadata(path).with_context(|| {
        format!(
            "Couldn't open {} for `--settings`. Check the file path and try again.",
            path.display()
        )
    })?;
    ensure!(
        metadata.is_file() && metadata.len() <= 2 * 1024 * 1024,
        "`--settings` needs a regular JSON file no larger than 2 MiB. Choose a smaller file or pass an inline JSON object."
    );
    let file = std::fs::File::open(path)
        .with_context(|| format!("Couldn't read {} for `--settings`.", path.display()))?;
    serde_json::from_reader(file).with_context(|| {
        format!(
            "Couldn't read {} as `--settings` JSON. Check its syntax and try again.",
            path.display()
        )
    })
}

/// The URL of the running proxy. Starts the background service when nothing
/// answers, so `cc-proxy claude` is the only command a session needs.
fn proxy_url() -> Result<String> {
    match daemon::describe() {
        daemon::DaemonStatus::Running(info) => Ok(info.listen_url()),
        // `describe` found it by probing loopback on this port.
        daemon::DaemonStatus::Unmanaged { port } => Ok(format!("http://127.0.0.1:{port}")),
        daemon::DaemonStatus::Stopped => {
            match ui::waiting("starting cc-proxy", || daemon::serve_background(None))? {
                daemon::ServeOutcome::Started(info) => {
                    let url = info.listen_url();
                    ui::eprint_note(Mood::Awake, &[format!("Started cc-proxy on {url}.")]);
                    Ok(url)
                }
                daemon::ServeOutcome::AlreadyRunning(info) => Ok(info.listen_url()),
            }
        }
    }
}

pub fn run(args: Vec<OsString>) -> Result<()> {
    let mut command = Command::new("claude");
    // Off, through the shell hook: plain claude, no proxy and no settings.
    let args = if crate::shell::wants_proxy() {
        proxy_args(args, &proxy_url()?)?
    } else {
        args
    };
    command.args(args).env_remove(crate::shell::HOOK_ENV);
    let error = exec(command);
    if error.kind() == std::io::ErrorKind::NotFound {
        ui::eprint_note(
            Mood::Hurt,
            &[
                "cc-proxy couldn't start claude: there's no `claude` on your PATH. \
               Install Claude Code, or add it to PATH."
                    .into(),
            ],
        );
        std::process::exit(127);
    }
    Err(anyhow::anyhow!("cc-proxy couldn't start claude: {error}"))
}

/// Replace this process with claude, so signals, the terminal and the exit
/// code are claude's own. Returns only when claude couldn't start.
#[cfg(unix)]
fn exec(mut command: Command) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    command.exec()
}

/// Windows has no exec: run claude, then exit with its code.
#[cfg(not(unix))]
fn exec(mut command: Command) -> std::io::Error {
    match command.status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(error) => error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caller_settings_keep_their_values_but_not_a_conflicting_connection() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let caller = serde_json::json!({
            "env": {"ANTHROPIC_BASE_URL":"http://127.0.0.1:1", "UNRELATED":"keep"},
            "permissions": {"defaultMode":"bypassPermissions"},
            "hooks": {"SessionStart":[]}
        })
        .to_string();
        std::fs::write(file.path(), &caller).unwrap();
        for setting_args in [
            vec!["--settings".into(), caller.clone().into()],
            vec![format!("--settings={caller}").into()],
            vec!["--settings".into(), file.path().as_os_str().to_owned()],
        ] {
            let mut args: Vec<OsString> = vec!["--dangerously-skip-permissions".into()];
            args.extend(setting_args);
            args.extend([
                "--resume".into(),
                "abc".into(),
                "-p".into(),
                "hi there".into(),
            ]);
            let out = proxy_args(args, "http://127.0.0.1:18765").unwrap();
            let settings: serde_json::Value =
                serde_json::from_str(out[1].to_str().unwrap()).unwrap();
            assert_eq!(
                settings["env"]["ANTHROPIC_BASE_URL"],
                "http://127.0.0.1:18765"
            );
            assert_eq!(settings["env"]["UNRELATED"], "keep");
            assert_eq!(settings["permissions"]["defaultMode"], "bypassPermissions");
            assert_eq!(settings["hooks"]["SessionStart"], serde_json::json!([]));
            assert_eq!(
                out[2..],
                [
                    "--dangerously-skip-permissions",
                    "--resume",
                    "abc",
                    "-p",
                    "hi there"
                ]
                .map(OsString::from)
            );
        }
        assert_eq!(std::fs::read_to_string(file.path()).unwrap(), caller);
    }

    #[test]
    fn repeated_settings_keep_claudes_last_value_and_stop_at_the_delimiter() {
        let out = proxy_args(
            vec![
                "--settings=/overridden-file-that-does-not-exist.json".into(),
                "--settings".into(),
                "{\"last\":true}".into(),
                "--".into(),
                "--settings".into(),
                "this is a prompt".into(),
            ],
            "http://127.0.0.1:18765",
        )
        .unwrap();
        let settings: serde_json::Value = serde_json::from_str(out[1].to_str().unwrap()).unwrap();
        assert_eq!(settings["last"], true);
        assert!(settings.get("first").is_none());
        assert_eq!(
            out[2..],
            ["--", "--settings", "this is a prompt"].map(OsString::from)
        );
        for args in [
            vec!["--settings".into()],
            vec!["--settings".into(), "{not json}".into()],
            vec!["--settings={\"env\":false}".into()],
        ] {
            assert!(proxy_args(args, "http://127.0.0.1:18765").is_err());
        }
    }

    #[test]
    fn settings_carry_the_proxy_url_and_no_legacy_names() {
        let args = proxy_args(Vec::new(), "http://127.0.0.1:18765").unwrap();
        let settings: serde_json::Value = serde_json::from_str(args[1].to_str().unwrap()).unwrap();
        let env = &settings["env"];
        assert_eq!(env["ANTHROPIC_BASE_URL"], "http://127.0.0.1:18765");
        assert_eq!(env["CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK"], "1");
        assert!(env.get("ANTHROPIC_SMALL_FAST_MODEL").is_none());
        // ANTHROPIC_AUTH_TOKEN makes Claude Code drop every `anthropic-ratelimit-*`
        // header, so the reset countdown and the wait-and-continue offer never show.
        assert_eq!(env["CLAUDE_CODE_OAUTH_TOKEN"], "unused");
        assert!(env.get("ANTHROPIC_AUTH_TOKEN").is_none());
    }
}
