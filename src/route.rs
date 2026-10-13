//! Sending Claude Code through the proxy, and back.
//!
//! Routing has two halves that have to agree. `route` in config.json tells the
//! proxy where Claude's own model names go, and the proxy reads it on every
//! request, so a switch needs no restart. The `env` block in Claude Code's
//! settings (see `claude_settings`) is what makes a Claude Code window talk to
//! the proxy at all.
//!
//! Routing starts only when the user runs `cc-proxy start` or `cc-proxy use`.
//! It ends when they run `cc-proxy stop` or `cc-proxy use claude`, or when the
//! proxy turns out to be gone ([`heal`]). Nothing else turns it back on.

use std::time::Duration;

use anyhow::Result;

use crate::ui::{self, Mood};
use crate::{claude_settings, config, config_keys, daemon};

/// The providers `start` and `use` can send Claude Code to.
pub const PROVIDERS: [&str; 7] = [
    "codex", "kimi", "cursor", "grok", "copilot", "glm", "opencode",
];

/// What `cc-proxy use` takes to mean "no routing".
pub const CLAUDE: &str = "claude";

/// Fails before anything starts when Claude Code's settings can't be edited,
/// so a refused `start` doesn't leave a proxy running that nothing points at.
pub fn preflight() -> Result<()> {
    claude_settings::inspect().map(drop).map_err(|error| {
        anyhow::anyhow!("{error:#}. cc-proxy changed nothing. Fix the file, then run it again.")
    })
}

/// Send Claude Code through the proxy at `base_url`, to `provider`.
pub fn activate(provider: &str, base_url: &str) -> Result<()> {
    // The settings file is the step that can refuse (a file we can't read as
    // JSON), so it goes first and nothing else changes when it does.
    claude_settings::claim(base_url)?;
    config_keys::write_values([
        ("route.on", serde_json::Value::Bool(true)),
        ("route.provider", provider.into()),
    ])?;
    Ok(())
}

/// Send Claude Code back to Anthropic. Reports a settings file it couldn't
/// change, after the proxy has stopped serving Claude's names either way.
pub fn deactivate() -> Result<()> {
    let config = if config::route().on {
        config_keys::write_value("route.on", serde_json::Value::Bool(false)).map(drop)
    } else {
        Ok(())
    };
    let settings = claude_settings::release().map(drop);
    settings.and(config)
}

/// Point Claude Code at the proxy's new address after a restart moved it.
/// Does nothing while routing is off. Returns whether it wrote the settings.
pub fn refresh(base_url: &str) -> Result<bool> {
    match config::route().target() {
        Some(_) => claude_settings::claim(base_url),
        None => Ok(false),
    }
}

/// Routing that outlived its proxy (a crash, a reboot, a kill) leaves every
/// Claude Code window pointed at nothing. Put them back on Anthropic.
pub fn heal() {
    let Ok(found) = claude_settings::inspect() else {
        return;
    };
    let Some(url) = found.routed_to else {
        return;
    };
    if proxy_answers(&url) {
        return;
    }
    match deactivate() {
        Ok(()) => ui::eprint_note(
            Mood::Asleep,
            &[
                "cc-proxy wasn't running, so Claude Code goes back to your Claude subscription."
                    .into(),
                "Run `cc-proxy start` to route it again.".into(),
            ],
        ),
        Err(error) => ui::eprint_note(Mood::Unsure, &[error.to_string()]),
    }
}

/// Whether something serves `url`. Asks a few times: a `restart` in another
/// terminal is down for a moment, and that is not a reason to switch routing off.
fn proxy_answers(url: &str) -> bool {
    let claimed_port = url::Url::parse(url)
        .ok()
        .and_then(|url| url.port_or_known_default());
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(500));
        }
        let managed = !matches!(daemon::describe(), daemon::DaemonStatus::Stopped);
        if managed || claimed_port.is_some_and(daemon::probe_port_health) {
            return true;
        }
    }
    false
}

/// The lines `cc-proxy status` shows about routing.
pub fn status_lines() -> Vec<String> {
    let route = config::route();
    let found = match claude_settings::inspect() {
        Ok(found) => found,
        Err(error) => return vec![error.to_string()],
    };
    let mut lines = Vec::new();
    match (route.target(), found.routed_to) {
        (Some(provider), Some(_)) => lines.push(format!(
            "Claude Code goes to {}. `cc-proxy use claude` switches back to your subscription.",
            ui::provider_name(provider)
        )),
        (Some(provider), None) => lines.push(format!(
            "Routing is on for {}, but Claude Code's settings don't point at the proxy. \
             `cc-proxy use {provider}` fixes that.",
            ui::provider_name(provider)
        )),
        (None, Some(url)) => lines.push(format!(
            "Claude Code's settings point at {url}, but routing is off. `cc-proxy use claude` \
             points them back at Anthropic."
        )),
        (None, None) => lines.push(
            "Claude Code uses your Claude subscription. `cc-proxy start` routes it through a \
             provider."
                .into(),
        ),
    }
    lines.extend(found.warnings);
    lines
}

/// A note for when the running proxy is not the version of this binary,
/// which happens after an upgrade until the proxy restarts.
pub fn version_note(port: u16) -> Option<String> {
    let ours = env!("CARGO_PKG_VERSION");
    match daemon::running_version(port) {
        Some(running) if running == ours => None,
        Some(running) => Some(format!(
            "The running proxy is {running}, and this is {ours}. `cc-proxy restart` updates it."
        )),
        // Answers, but doesn't say its version: it predates 0.2.0, so it also
        // ignores the provider picked here.
        None if daemon::probe_port_health(port) => Some(format!(
            "The running proxy is older than {ours} and ignores the provider you pick. \
             `cc-proxy restart` updates it."
        )),
        None => None,
    }
}
