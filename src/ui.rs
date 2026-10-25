//! How cc-proxy looks: the colors the monitor and the commands share, the
//! cc-proxy face, and the animation while a command waits.
//!
//! Commands style their output only for a terminal. Piped or redirected
//! output, `NO_COLOR` and `TERM=dumb` get the same words as plain text, so
//! scripts, logs and tests never see an escape code.

use std::io::{self, IsTerminal, Write};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use crossterm::style::Stylize;
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

pub const TEAL: Color = Color::Rgb(78, 201, 176);
pub const WHITE: Color = Color::Rgb(240, 244, 248);
pub const DIM_WHITE: Color = Color::Rgb(180, 190, 200);
pub const GREEN: Color = Color::Rgb(120, 200, 120);
pub const BRIGHT_GREEN: Color = Color::Rgb(100, 230, 135);
pub const RED: Color = Color::Rgb(220, 120, 120);
pub const YELLOW: Color = Color::Rgb(220, 200, 100);
pub const DIM: Color = Color::Rgb(100, 104, 114);
/// The empty part of a meter.
const TRACK: Color = Color::Rgb(58, 60, 68);

/// One color per provider, the same in the monitor and in command output.
pub fn provider_color(provider: &str) -> Color {
    match provider {
        "codex" => TEAL,
        "glm" => Color::Rgb(110, 180, 240),
        "cursor" => Color::Rgb(150, 150, 245),
        "kimi" => Color::Rgb(200, 140, 230),
        "grok" => Color::Rgb(240, 130, 170),
        "opencode" | "opencode-go" | "opencode-zen" => Color::Rgb(240, 165, 100),
        "copilot" => Color::Rgb(160, 150, 255),
        "claude" | "anthropic" => Color::Rgb(230, 145, 110),
        "gemini" | "antigravity" => Color::Rgb(110, 170, 250),
        _ => DIM_WHITE,
    }
}

/// How people say a provider's name.
pub fn provider_name(provider: &str) -> &str {
    match provider {
        "codex" => "Codex",
        "glm" => "GLM",
        "copilot" => "GitHub Copilot",
        "cursor" => "Cursor",
        "kimi" => "Kimi",
        "grok" => "Grok",
        "claude" => "Claude subscription",
        "anthropic" => "Anthropic",
        "gemini" => "Gemini",
        "antigravity" => "Antigravity",
        "opencode" | "opencode-go" => "OpenCode Go",
        "opencode-zen" => "OpenCode Zen",
        other => other,
    }
}

/// The face's expressions. Each one shows what state the proxy is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mood {
    /// Running and ready.
    Awake,
    /// Something just worked: a reload, an install.
    Glad,
    /// Stopped, or off.
    Asleep,
    /// Needs a look: a proxy cc-proxy didn't start, a lost connection.
    Unsure,
    /// A command failed.
    Hurt,
}

impl Mood {
    pub fn face(self) -> &'static str {
        match self {
            Self::Awake => "•ω•",
            Self::Glad => "^ω^",
            Self::Asleep => "-ω-",
            Self::Unsure => "•_•",
            Self::Hurt => "×_×",
        }
    }

    pub fn color(self) -> Color {
        match self {
            Self::Awake => TEAL,
            Self::Glad => GREEN,
            Self::Asleep => DIM,
            Self::Unsure => YELLOW,
            Self::Hurt => RED,
        }
    }
}

/// Whether text written to `stream` gets colors and animation.
pub fn styled(stream: &impl IsTerminal) -> bool {
    stream.is_terminal()
        && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
        && std::env::var_os("TERM").is_none_or(|term| term != "dumb")
        && ansi_ready()
}

/// A Windows console reads escape codes only once they're switched on;
/// this switches them on, or says the console can't.
#[cfg(windows)]
fn ansi_ready() -> bool {
    crossterm::ansi_support::supports_ansi()
}

#[cfg(not(windows))]
fn ansi_ready() -> bool {
    true
}

/// Some terminals (and `script`) report zero columns; 80 stands in then.
fn terminal_width() -> usize {
    match crossterm::terminal::size() {
        Ok((width, _)) if width > 0 => usize::from(width),
        _ => 80,
    }
}

fn paint(text: &str, color: Color) -> String {
    text.with(color.into()).to_string()
}

fn bold(text: &str, color: Color) -> String {
    text.with(color.into()).bold().to_string()
}

/// `text` bold in `color` when `styled`, else as it is.
pub fn strong(text: &str, color: Color, styled: bool) -> String {
    if styled {
        bold(text, color)
    } else {
        text.to_string()
    }
}

/// `text` in `color`, with each `command` in teal and its backticks dropped.
fn with_code(text: &str, color: Color) -> String {
    text.split('`')
        .enumerate()
        .map(|(index, part)| paint(part, if index % 2 == 1 { TEAL } else { color }))
        .collect()
}

/// Lines of at most `width` columns, broken at spaces outside backticks so a
/// command stays on one line. Backticks take no column: they're dropped when
/// styled. A word wider than `width` gets a line of its own.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_code = false;
    for character in text.chars() {
        match character {
            ' ' if !in_code => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            '`' => {
                in_code = !in_code;
                word.push(character);
            }
            _ => word.push(character),
        }
    }
    if !word.is_empty() {
        words.push(word);
    }

    let columns = |text: &str| text.chars().filter(|&character| character != '`').count();
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in words {
        if !line.is_empty() && columns(&line) + 1 + columns(&word) > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(&word);
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

const FACE_WIDTH: usize = 7;
const GAP: &str = "  ";

/// The boxed face, its eyes `look` columns from the left edge (0 to 2).
fn face(mood: Mood, look: usize) -> [String; 3] {
    let color = mood.color();
    let eyes = if mood == Mood::Asleep {
        DIM_WHITE
    } else {
        WHITE
    };
    let inside = format!(
        "{}{}{}",
        " ".repeat(look),
        mood.face(),
        " ".repeat(2 - look)
    );
    [
        paint("╭─────╮", color),
        format!(
            "{}{}{}",
            paint("│", color),
            bold(&inside, eyes),
            paint("│", color)
        ),
        paint("╰─────╯", color),
    ]
}

/// The face beside `lines`, a headline and then details, wrapped to `width`
/// so the details stay beside the face. Plain, it's the lines alone.
pub fn card(mood: Mood, lines: &[String], styled: bool, width: usize) -> String {
    if !styled {
        return lines.join("\n");
    }
    let text_width = width.saturating_sub(FACE_WIDTH + GAP.len()).max(24);
    let rows = lines
        .iter()
        .enumerate()
        .flat_map(|(index, line)| {
            wrap(line, text_width).into_iter().map(move |part| {
                if index == 0 {
                    bold(&part, WHITE)
                } else {
                    with_code(&part, DIM_WHITE)
                }
            })
        })
        .collect::<Vec<_>>();
    let face = face(mood, 1);
    let blank = " ".repeat(FACE_WIDTH);
    (0..rows.len().max(face.len()))
        .map(|index| {
            let left = face.get(index).unwrap_or(&blank);
            match rows.get(index) {
                Some(row) => format!("{left}{GAP}{row}"),
                None => left.clone(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One headline led by the small face, then dimmer details under it. Plain,
/// it's the lines alone.
pub fn note(mood: Mood, lines: &[String], styled: bool, width: usize) -> String {
    if !styled {
        return lines.join("\n");
    }
    const INDENT: &str = "    ";
    let text_width = width.saturating_sub(INDENT.len()).max(24);
    let mut out = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        for part in wrap(line, text_width) {
            let lead = if out.is_empty() {
                format!("{} ", bold(mood.face(), mood.color()))
            } else {
                INDENT.to_string()
            };
            let color = if index == 0 { WHITE } else { DIM_WHITE };
            out.push(format!("{lead}{}", with_code(&part, color)));
        }
    }
    out.join("\n")
}

pub fn print_card(mood: Mood, lines: &[String]) {
    println!(
        "{}",
        card(mood, lines, styled(&io::stdout()), terminal_width())
    );
}

/// A dim tip under a card, in line with the card's text. Only a terminal gets
/// one: piped output stays exactly what the command printed.
pub fn print_tip(text: &str) {
    if styled(&io::stdout()) {
        println!("\n{}", tip(text, true, terminal_width()));
    }
}

pub fn tip(text: &str, styled: bool, width: usize) -> String {
    const LABEL: &str = "Tip: ";
    if !styled {
        return format!("{LABEL}{text}");
    }
    let indent = " ".repeat(FACE_WIDTH + GAP.len());
    let text_width = width.saturating_sub(indent.len() + LABEL.len()).max(24);
    wrap(text, text_width)
        .iter()
        .enumerate()
        .map(|(index, part)| {
            let lead = match index {
                0 => paint(LABEL, DIM),
                _ => " ".repeat(LABEL.len()),
            };
            format!("{indent}{lead}{}", with_code(part, DIM))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn print_note(mood: Mood, lines: &[String]) {
    println!(
        "{}",
        note(mood, lines, styled(&io::stdout()), terminal_width())
    );
}

pub fn eprint_note(mood: Mood, lines: &[String]) {
    eprintln!(
        "{}",
        note(mood, lines, styled(&io::stderr()), terminal_width())
    );
}

/// Prints a failed command's error and each cause under it. Plain, it's
/// Rust's own `Error: …` report.
pub fn print_error(error: &anyhow::Error) {
    if !styled(&io::stderr()) {
        eprintln!("Error: {error:?}");
        return;
    }
    let lines = error.chain().map(ToString::to_string).collect::<Vec<_>>();
    eprintln!("{}", card(Mood::Hurt, &lines, true, terminal_width()));
}

/// Color shows what remains: bright green from 80%, yellow at 30%, red at 10%.
pub fn remaining_color(percent: f64) -> Color {
    if percent <= 10.0 {
        RED
    } else if percent <= 30.0 {
        YELLOW
    } else if percent < 80.0 {
        GREEN
    } else {
        BRIGHT_GREEN
    }
}

/// A monitor usage row keeps its label and reset time neutral.
pub(crate) fn remaining_line(label: &str, percent: f64, detail: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw(format!("{label} ")),
        Span::styled(
            format!("{percent:.0}%"),
            Style::default().fg(remaining_color(percent)),
        ),
        Span::raw(format!(" left{detail}")),
    ])
}

/// A `percent`-used bar. A shorter allowance turns yellow, then red.
pub fn meter(percent: f64, cells: usize) -> String {
    let percent = percent.clamp(0.0, 100.0);
    let filled = ((percent / 100.0) * cells as f64).round() as usize;
    paint(&"━".repeat(filled), remaining_color(100.0 - percent))
        + &paint(&"━".repeat(cells - filled), TRACK)
}

/// A `percent`-left bar. A shorter bar means less allowance.
pub fn remaining_meter(percent: f64, cells: usize) -> String {
    let percent = percent.clamp(0.0, 100.0);
    let filled = ((percent / 100.0) * cells as f64).round() as usize;
    paint(&"━".repeat(filled), remaining_color(percent))
        + &paint(&"━".repeat(cells - filled), TRACK)
}

/// "2h14m", "3m05s", "5s".
pub fn duration(duration: Duration) -> String {
    let total = duration.as_secs();
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}h{minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

/// Work this quick never shows the animation, so it can't flicker.
const FIRST_FRAME_AFTER: Duration = Duration::from_millis(150);
const FRAME: Duration = Duration::from_millis(160);

/// Runs `work` while the face looks around beside `label` on stderr, and
/// wipes the animation before returning. Without a terminal, only `work`
/// runs.
pub fn waiting<T>(label: &str, work: impl FnOnce() -> T) -> T {
    if !styled(&io::stderr()) {
        return work();
    }
    let (done, finished) = mpsc::channel::<()>();
    std::thread::scope(|scope| {
        scope.spawn(move || animate(label, &finished));
        let result = work();
        drop(done);
        result
    })
}

fn animate(label: &str, finished: &mpsc::Receiver<()>) {
    if !matches!(
        finished.recv_timeout(FIRST_FRAME_AFTER),
        Err(RecvTimeoutError::Timeout)
    ) {
        return;
    }
    // Wider than the terminal, a line would wrap and the redraw would land
    // one row off.
    let room = terminal_width()
        .saturating_sub(FACE_WIDTH + GAP.len() + 3)
        .max(16);
    let label = label.chars().take(room).collect::<String>();
    let mut stderr = io::stderr();
    for tick in 0usize.. {
        let [top, middle, bottom] = face(Mood::Awake, [1, 0, 1, 2][tick % 4]);
        let text = paint(&format!("{label}{}", ".".repeat(tick % 4)), DIM_WHITE);
        if tick > 0 {
            let _ = write!(stderr, "\r\x1b[2A");
        }
        let _ = write!(
            stderr,
            "\x1b[2K{top}\n\x1b[2K{middle}{GAP}{text}\n\x1b[2K{bottom}"
        );
        let _ = stderr.flush();
        if !matches!(finished.recv_timeout(FRAME), Err(RecvTimeoutError::Timeout)) {
            break;
        }
    }
    let _ = write!(stderr, "\r\x1b[2A\x1b[J");
    let _ = stderr.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip_ansi(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(character) = chars.next() {
            if character == '\x1b' {
                chars
                    .by_ref()
                    .find(|character| character.is_ascii_alphabetic());
            } else {
                out.push(character);
            }
        }
        out
    }

    #[test]
    fn claude_subscription_is_distinct_from_direct_bypass() {
        assert_eq!(provider_name("claude"), "Claude subscription");
        assert_eq!(provider_name("anthropic"), "Anthropic");
        assert_eq!(provider_color("claude"), provider_color("anthropic"));
    }

    #[test]
    fn a_tip_hangs_under_the_card_text_and_keeps_its_command_in_teal() {
        let text = "`cc-proxy monitor` shows your accounts, how much quota each one has left and the requests running now.";
        assert_eq!(tip(text, false, 80), format!("Tip: {text}"));
        let styled = tip(text, true, 60);
        let plain = strip_ansi(&styled);
        let lines: Vec<_> = plain.lines().collect();
        assert!(lines.len() > 1, "{plain}");
        assert!(
            lines[0].starts_with("         Tip: cc-proxy monitor shows"),
            "{plain}"
        );
        assert!(lines[1].starts_with("              "), "{plain}");
        assert!(
            lines.iter().all(|line| line.chars().count() <= 60),
            "{plain}"
        );
        assert!(!plain.contains('`'), "{plain}");
    }

    #[test]
    fn plain_output_is_the_lines_alone() {
        let lines = [
            "cc-proxy started".to_string(),
            "Run `cc-proxy stop`.".into(),
        ];

        assert_eq!(
            card(Mood::Awake, &lines, false, 80),
            "cc-proxy started\nRun `cc-proxy stop`."
        );
        assert_eq!(
            note(Mood::Awake, &lines, false, 80),
            "cc-proxy started\nRun `cc-proxy stop`."
        );
    }

    #[test]
    fn a_card_puts_the_lines_beside_the_face_and_wraps_under_it() {
        let lines = [
            "cc-proxy started".to_string(),
            "Stop it with `cc-proxy stop` when you are done for the day.".into(),
        ];

        let text = strip_ansi(&card(Mood::Awake, &lines, true, 36));

        assert_eq!(
            text.lines().collect::<Vec<_>>(),
            [
                "╭─────╮  cc-proxy started",
                "│ •ω• │  Stop it with cc-proxy stop",
                "╰─────╯  when you are done for the",
                "         day.",
            ]
        );
    }

    #[test]
    fn wrapping_never_splits_a_command() {
        assert_eq!(
            wrap("run `cc-proxy use direct` now", 12),
            ["run", "`cc-proxy use direct`", "now"]
        );
    }

    #[test]
    fn a_meter_fills_in_proportion() {
        let bar = strip_ansi(&meter(45.0, 20));

        assert_eq!(bar.chars().count(), 20);
        assert!(meter(45.0, 20).starts_with(&paint(&"━".repeat(9), GREEN)));
        assert!(meter(95.0, 20).starts_with(&paint(&"━".repeat(19), RED)));
    }

    #[test]
    fn remaining_percentages_share_four_status_colors() {
        for (percent, color) in [
            (100.0, BRIGHT_GREEN),
            (80.0, BRIGHT_GREEN),
            (79.0, GREEN),
            (31.0, GREEN),
            (30.0, YELLOW),
            (11.0, YELLOW),
            (10.0, RED),
            (0.0, RED),
        ] {
            assert_eq!(remaining_color(percent), color);
            let line = remaining_line("5h", percent, ", resets in 1h");
            assert_eq!(line.spans[1].style.fg, Some(color));
            assert_eq!(line.spans[0].style.fg, None);
            assert_eq!(line.spans[2].style.fg, None);
            assert_eq!(
                line.to_string(),
                format!("5h {percent:.0}% left, resets in 1h")
            );
        }
        assert!(remaining_meter(100.0, 20).starts_with(&paint(&"━".repeat(20), BRIGHT_GREEN)));
    }

    #[test]
    fn a_remaining_meter_empties_and_turns_red_at_the_end() {
        assert_eq!(strip_ansi(&remaining_meter(45.0, 20)).chars().count(), 20);
        assert!(remaining_meter(45.0, 20).starts_with(&paint(&"━".repeat(9), GREEN)));
        assert!(remaining_meter(5.0, 20).starts_with(&paint("━", RED)));
        assert!(remaining_meter(25.0, 20).starts_with(&paint(&"━".repeat(5), YELLOW)));
    }
}
