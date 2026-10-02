//! `cc-proxy claude`: start Claude Code on the proxy.
//!
//! Every argument goes to `claude` unchanged, so `--resume`, `--worktree`,
//! `-p` and a pasted resume command all keep working. The connection rides
//! in one `--settings` JSON. Claude Code ranks it above the user's and the
//! project's settings files, so an `env` entry in `settings.json` can't send
//! the session somewhere else, and nothing on disk is changed.

use std::ffi::OsString;
use std::process::Command;

use anyhow::Result;

use crate::ui::{self, Mood};
use crate::{config, daemon};

/// The Claude Code environment that sends every request to the proxy at
/// `base_url`. One list for `--settings`, the serve banner and the monitor.
pub fn env(base_url: &str) -> Vec<(&'static str, String)> {
    let mut env = vec![
        ("ANTHROPIC_BASE_URL", base_url.to_string()),
        // Claude Code asks for a credential; the proxy never forwards it.
        ("ANTHROPIC_AUTH_TOKEN", "unused".to_string()),
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

/// The `--settings` value: `{"env": {...}}`.
pub fn settings_json(base_url: &str) -> String {
    let env: serde_json::Map<String, serde_json::Value> = env(base_url)
        .into_iter()
        .map(|(name, value)| (name.to_string(), value.into()))
        .collect();
    serde_json::json!({ "env": env }).to_string()
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
    if crate::shell::wants_proxy() {
        command.arg("--settings").arg(settings_json(&proxy_url()?));
    }
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
    fn settings_carry_the_proxy_url_and_no_legacy_names() {
        let settings: serde_json::Value =
            serde_json::from_str(&settings_json("http://127.0.0.1:18765")).unwrap();
        let env = &settings["env"];
        assert_eq!(env["ANTHROPIC_BASE_URL"], "http://127.0.0.1:18765");
        assert_eq!(env["CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK"], "1");
        assert!(env.get("ANTHROPIC_SMALL_FAST_MODEL").is_none());
    }
}
