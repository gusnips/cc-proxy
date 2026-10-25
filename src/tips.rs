//! One short tip under the card of a few commands. A next step that belongs to
//! the command stays inside its card. A tip is something else worth knowing, so
//! it changes from run to run and is never about the command that just ran.

use rand::seq::SliceRandom;

use crate::ui;

/// Each tip with the command it is about. Every command named here must exist:
/// a test in `main.rs` parses them.
pub const TIPS: &[(&str, &str)] = &[
    (
        "monitor",
        "`cc-proxy monitor` shows your accounts, how much quota each one has left and the requests running now.",
    ),
    (
        "usage",
        "`cc-proxy usage` shows how much of each plan is left and when it resets.",
    ),
    (
        "use",
        "`cc-proxy use codex kimi` sends Claude Code to Codex first and moves to Kimi when Codex is spent.",
    ),
    (
        "use",
        "`cc-proxy use direct` sends Claude Code back to its own login. The proxy keeps running.",
    ),
    (
        "auth",
        "`cc-proxy auth add codex` saves a second login. With auto-rotate on, the proxy moves to it when the first one hits its limit.",
    ),
    (
        "auth",
        "`cc-proxy auth status codex` shows each saved account and how long it has to wait.",
    ),
    (
        "models",
        "`cc-proxy models choose` picks the model each Claude name uses, with search.",
    ),
    (
        "models",
        "`cc-proxy models list` shows every model a provider serves.",
    ),
    (
        "config",
        "`cc-proxy config` opens a settings screen, so you don't have to edit config.json.",
    ),
    ("update", "`cc-proxy update` installs the newest version."),
    (
        "demo",
        "`cc-proxy demo` opens the monitor with made-up traffic, so you can look around without signing in.",
    ),
];

/// A random tip that is not about `command`.
// ponytail: plain random, so the same tip can show twice in a row. Keep the
// last index in the state folder if that ever bothers anyone.
pub fn pick(command: &str) -> Option<&'static str> {
    let others: Vec<_> = TIPS
        .iter()
        .filter(|(about, _)| *about != command)
        .map(|(_, text)| *text)
        .collect();
    others.choose(&mut rand::thread_rng()).copied()
}

/// Prints a tip under the card the command just printed.
pub fn print(command: &str) {
    if let Some(text) = pick(command) {
        ui::print_tip(text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn about(tip: &str) -> &'static str {
        TIPS.iter().find(|(_, text)| *text == tip).unwrap().0
    }

    #[test]
    fn a_tip_is_never_about_the_command_that_just_ran_and_rotates() {
        for _ in 0..200 {
            assert_ne!(about(pick("use").unwrap()), "use");
        }
        let seen: std::collections::HashSet<_> = (0..200).filter_map(|_| pick("status")).collect();
        assert!(seen.len() > 3, "tips never rotated: {seen:?}");
    }

    #[test]
    fn tips_are_plain_sentences_with_balanced_code() {
        for (_, text) in TIPS {
            assert!(!text.contains('\u{2014}'), "{text}");
            assert_eq!(text.matches('`').count() % 2, 0, "{text}");
            assert!(text.ends_with('.'), "{text}");
        }
    }
}
