//! Questions on stdin and stdout, for `cc-proxy start`, `cc-proxy use` and the login flows.
//! In a terminal, lists use an arrow-key selector with live type-to-filter;
//! piped input answers them line by line.

use std::io::{BufRead, IsTerminal, Write};

use anyhow::{Result, bail};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode},
};

/// How many options `choose` prints. The rest can still be typed by name.
const MAX_SHOWN: usize = 12;

/// True when both stdin and stdout are attached to a terminal.
pub fn is_interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Strip terminal escape sequences (such as SGR mouse events `\x1b[<35;27;27M`
/// or bracketed-paste markers `\x1b[200~`) and control characters from a line
/// read from the terminal.
pub fn strip_terminal_noise(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i += 1;
            if i < bytes.len() && bytes[i] == b'[' {
                i += 1;
                while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
            } else if i < bytes.len() && bytes[i] == b']' {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            continue;
        }
        if bytes[i] < 0x20 || bytes[i] == 0x7f {
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).trim().to_string()
}

/// One line from stdin, trimmed. `None` when the input has ended.
fn read_line() -> std::io::Result<Option<String>> {
    std::io::stdout().flush()?;
    let mut buf = String::new();
    let read = std::io::stdin().lock().read_line(&mut buf)?;
    Ok((read > 0).then(|| strip_terminal_noise(&buf)))
}

pub fn read_visible_line() -> std::io::Result<String> {
    Ok(read_line()?.unwrap_or_default())
}

/// Read a line without echoing it, so a pasted API key never shows on
/// screen. Uses termios on unix (already a dependency via libc); elsewhere
/// falls back to a visible read with a warning.
#[cfg(unix)]
pub fn read_hidden_line(prompt: &str) -> std::io::Result<String> {
    use std::os::unix::io::AsRawFd;

    let fd = std::io::stdin().as_raw_fd();
    // SAFETY: tcgetattr/tcsetattr only touch the termios struct for our own
    // stdin fd, and the original flags are restored before returning.
    unsafe {
        let mut original: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut original) != 0 {
            print!("{prompt}");
            // Not a TTY (piped stdin in tests and scripts): plain read.
            return read_visible_line();
        }
        // Turn off any lingering terminal mouse reporting before prompting so
        // clicking the window to paste does not inject `\x1b[<...M` into stdin.
        print!("\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l{prompt}");
        std::io::stdout().flush()?;
        let mut hidden = original;
        hidden.c_lflag &= !libc::ECHO;
        if libc::tcsetattr(fd, libc::TCSANOW, &hidden) != 0 {
            return read_visible_line();
        }
        let line = read_visible_line();
        libc::tcsetattr(fd, libc::TCSANOW, &original);
        println!();
        line
    }
}

#[cfg(not(unix))]
pub fn read_hidden_line(prompt: &str) -> std::io::Result<String> {
    eprintln!("warning: hidden input is not supported on this platform; the key will echo.");
    print!("{prompt}");
    read_visible_line()
}

/// Print `options` and read the pick. In a terminal, uses ↑/↓ arrow keys and
/// live type-to-filter; on piped input, prints numbered rows and reads a number
/// or exact name. Enter takes the option at `default`. Returns its index.
pub fn choose(question: &str, options: &[String], default: usize) -> Result<usize> {
    if options.is_empty() {
        bail!("no options to choose from");
    }
    if is_interactive()
        && let Ok(Some(picked)) = tty_pick(question, options, default, false)
    {
        return Ok(picked);
    }
    println!("{question}");
    for (index, option) in options.iter().take(MAX_SHOWN).enumerate() {
        println!("  {}. {option}", index + 1);
    }
    if options.len() > MAX_SHOWN {
        println!(
            "  ...and {} more. Type a number or a name; `cc-proxy models list` shows them all.",
            options.len() - MAX_SHOWN
        );
    }
    loop {
        print!("Number [{}]: ", default + 1);
        let Some(answer) = read_line()? else {
            bail!("the input ended before you picked an option");
        };
        if answer.is_empty() {
            return Ok(default);
        }
        let by_number = answer
            .parse::<usize>()
            .ok()
            .and_then(|number| number.checked_sub(1))
            .filter(|index| *index < options.len());
        let by_name = options
            .iter()
            .position(|option| option.eq_ignore_ascii_case(&answer));
        match by_number.or(by_name) {
            Some(index) => return Ok(index),
            None => println!("Type a number from 1 to {}.", options.len()),
        }
    }
}

/// Pick one of `options` by searching, for lists too long to read through.
/// In a terminal, uses ↑/↓ arrow keys with live type-to-filter and a top
/// "Skip (keep as it is)" option; on piped input, reads a line at a time.
/// Returns the index picked, or `None` to keep.
pub fn search(question: &str, options: &[String]) -> Result<Option<usize>> {
    if is_interactive() {
        return tty_pick(question, options, 0, true);
    }
    println!("{question}");
    let mut shown: Vec<usize> = (0..options.len().min(MAX_SHOWN)).collect();
    print_numbered(options, &shown, options.len());
    loop {
        print!("Type to search, a number to pick, or Enter to keep: ");
        let Some(answer) = read_line()? else {
            println!();
            return Ok(None);
        };
        if answer.is_empty() {
            return Ok(None);
        };
        let by_number = answer
            .parse::<usize>()
            .ok()
            .and_then(|number| number.checked_sub(1))
            .and_then(|index| shown.get(index).copied());
        let by_name = options
            .iter()
            .position(|option| option.eq_ignore_ascii_case(&answer));
        if let Some(index) = by_number.or(by_name) {
            return Ok(Some(index));
        }
        let found = matching(options, &answer);
        if found.is_empty() {
            println!("Nothing matches \"{answer}\". Try another word, or press Enter to keep.");
            continue;
        }
        shown = found.iter().take(MAX_SHOWN).copied().collect();
        print_numbered(options, &shown, found.len());
    }
}

struct RawModeGuard;

impl RawModeGuard {
    fn enter() -> std::io::Result<Self> {
        enable_raw_mode()?;
        print!("\x1b[?25l");
        std::io::stdout().flush()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        print!("\x1b[?25h");
        let _ = std::io::stdout().flush();
    }
}

fn truncate_cols(s: &str, max_cols: usize) -> String {
    if max_cols <= 4 {
        return String::new();
    }
    let mut out = String::new();
    for (count, ch) in s.chars().enumerate() {
        if count + 1 >= max_cols {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

fn clear_drawn(drawn_lines: usize) {
    if drawn_lines > 0 {
        print!("\x1b[{drawn_lines}A\r\x1b[J");
    } else {
        print!("\r\x1b[J");
    }
}

/// Interactive ↑/↓ arrow-key and live-search selector for terminals.
fn tty_pick(
    question: &str,
    options: &[String],
    default: usize,
    allow_skip: bool,
) -> Result<Option<usize>> {
    let guard = RawModeGuard::enter()?;
    let mut query = String::new();
    let mut selected = if allow_skip {
        0
    } else {
        default.min(options.len().saturating_sub(1))
    };
    let mut window_start = 0usize;
    let mut drawn_lines = 0usize;

    loop {
        let items: Vec<Option<usize>> = if query.is_empty() {
            let mut list = Vec::with_capacity(options.len() + usize::from(allow_skip));
            if allow_skip {
                list.push(None);
            }
            list.extend((0..options.len()).map(Some));
            list
        } else {
            matching(options, &query).into_iter().map(Some).collect()
        };

        if items.is_empty() {
            selected = 0;
            window_start = 0;
        } else {
            if selected >= items.len() {
                selected = items.len() - 1;
            }
            if selected < window_start {
                window_start = selected;
            } else if selected >= window_start + MAX_SHOWN {
                window_start = selected + 1 - MAX_SHOWN;
            }
        }

        let cols = crossterm::terminal::size()
            .map(|(w, _)| usize::from(w))
            .unwrap_or(80);
        clear_drawn(drawn_lines);
        let mut lines = 0usize;

        let header = if query.is_empty() {
            truncate_cols(question, cols)
        } else {
            truncate_cols(&format!("{question} [search: {query}]"), cols)
        };
        print!("{header}\r\n");
        lines += 1;

        if items.is_empty() {
            let empty_msg = truncate_cols(
                "  No matches. Press Backspace to edit or Esc to skip.",
                cols,
            );
            print!("\x1b[2m{empty_msg}\x1b[0m\r\n");
            lines += 1;
        } else {
            for (row_idx, item) in items.iter().enumerate().skip(window_start).take(MAX_SHOWN) {
                let label = match item {
                    None => "Skip (keep as it is)",
                    Some(idx) => options[*idx].as_str(),
                };
                let text = truncate_cols(label, cols.saturating_sub(4));
                if row_idx == selected {
                    print!("\x1b[36;1m❯ {text}\x1b[0m\r\n");
                } else {
                    print!("  {text}\r\n");
                }
                lines += 1;
            }
        }

        let hint = if items.len() > MAX_SHOWN {
            format!(
                "  ({}/{}) ↑/↓ move · Enter pick · type to filter · Esc {}",
                selected + 1,
                items.len(),
                if allow_skip { "skip" } else { "cancel" }
            )
        } else if allow_skip {
            "  ↑/↓ move · Enter pick · type to filter · Esc skip".to_string()
        } else {
            "  ↑/↓ move · Enter pick · type to filter · Esc cancel".to_string()
        };
        print!("\x1b[2m{}\x1b[0m\r\n", truncate_cols(&hint, cols));
        lines += 1;
        drawn_lines = lines;
        std::io::stdout().flush()?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                clear_drawn(drawn_lines);
                drop(guard);
                bail!("cancelled");
            }
            (KeyCode::Esc, _) => {
                if !query.is_empty() {
                    query.clear();
                    selected = 0;
                    window_start = 0;
                    continue;
                }
                clear_drawn(drawn_lines);
                drop(guard);
                if allow_skip {
                    println!("{question} skipped");
                    return Ok(None);
                }
                bail!("cancelled");
            }
            (KeyCode::Up, _) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                if !items.is_empty() {
                    selected = if selected == 0 {
                        items.len() - 1
                    } else {
                        selected - 1
                    };
                }
            }
            (KeyCode::Down, _) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                if !items.is_empty() {
                    selected = (selected + 1) % items.len();
                }
            }
            (KeyCode::PageUp, _) => {
                selected = selected.saturating_sub(MAX_SHOWN);
            }
            (KeyCode::PageDown, _) => {
                if !items.is_empty() {
                    selected = (selected + MAX_SHOWN).min(items.len() - 1);
                }
            }
            (KeyCode::Enter, _) => {
                let Some(&pick) = items.get(selected) else {
                    if allow_skip {
                        clear_drawn(drawn_lines);
                        drop(guard);
                        println!("{question} skipped");
                        return Ok(None);
                    }
                    continue;
                };
                clear_drawn(drawn_lines);
                drop(guard);
                match pick {
                    Some(idx) => {
                        println!("{question} {}", options[idx]);
                        return Ok(Some(idx));
                    }
                    None => {
                        println!("{question} kept as it is");
                        return Ok(None);
                    }
                }
            }
            (KeyCode::Backspace, _) => {
                query.pop();
                selected = 0;
                window_start = 0;
            }
            (KeyCode::Char(ch), m)
                if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
            {
                query.push(ch);
                selected = 0;
                window_start = 0;
            }
            _ => {}
        }
    }
}

/// The options that hold every word of `query`, in any case, in list order.
pub(crate) fn matching(options: &[String], query: &str) -> Vec<usize> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    options
        .iter()
        .enumerate()
        .filter(|(_, option)| {
            let option = option.to_lowercase();
            words.iter().all(|word| option.contains(word.as_str()))
        })
        .map(|(index, _)| index)
        .collect()
}

fn print_numbered(options: &[String], shown: &[usize], total: usize) {
    for (number, index) in shown.iter().enumerate() {
        println!("  {}. {}", number + 1, options[*index]);
    }
    if total > shown.len() {
        println!(
            "  ...and {} more. Type part of a name to search.",
            total - shown.len()
        );
    }
}

/// A yes/no question. Enter, or input that has ended, takes `default`.
pub fn confirm(question: &str, default: bool) -> Result<bool> {
    Ok(ask_yes_no(question, default)?.unwrap_or(default))
}

/// Like `confirm`, but `None` when the input has ended, for a question whose
/// answer gets saved: no answer is not a yes.
pub fn ask_yes_no(question: &str, default: bool) -> Result<Option<bool>> {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    loop {
        print!("{question} {hint} ");
        let Some(answer) = read_line()? else {
            return Ok(None);
        };
        match answer.to_ascii_lowercase().as_str() {
            "" => return Ok(Some(default)),
            "y" | "yes" => return Ok(Some(true)),
            "n" | "no" => return Ok(Some(false)),
            _ => println!("Answer y or n."),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_matches_every_word_in_any_case_and_order() {
        let options: Vec<String> = [
            "openrouter/anthropic/claude-opus-4.1",
            "openrouter/google/gemini-2.5-pro",
            "openrouter/anthropic/claude-sonnet-4.5",
            "openrouter/meta-llama/llama-3.3-70b-instruct:free",
        ]
        .map(String::from)
        .into();
        assert_eq!(matching(&options, "claude"), [0, 2]);
        assert_eq!(matching(&options, "OPUS claude"), [0]);
        assert_eq!(matching(&options, "free llama"), [3]);
        assert!(matching(&options, "gpt").is_empty());
    }
}
