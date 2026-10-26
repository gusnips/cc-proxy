//! Questions on stdin and stdout, for `cc-proxy start`, `cc-proxy use` and the login flows.
//! In a terminal, lists use an arrow-key selector with live type-to-filter;
//! piped input answers them line by line.

use std::io::{BufRead, IsTerminal, Write};

use anyhow::{Context, Result, bail};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
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

/// One visible line being edited. Names in a prompt and in the monitor share
/// the same keys, including Ctrl-U to clear the suggested name.
#[derive(Clone, Debug)]
pub struct InputLine {
    chars: Vec<char>,
    cursor: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum InputResult {
    Editing,
    Submitted,
    Cancelled,
}

impl InputLine {
    pub fn new(initial: &str) -> Self {
        let chars: Vec<_> = initial.chars().collect();
        Self {
            cursor: chars.len(),
            chars,
        }
    }

    pub fn value(&self) -> String {
        self.chars.iter().collect()
    }

    /// What fits in `room` columns, scrolled so the cursor stays in view, and
    /// the cursor's column inside it.
    pub fn window(&self, room: usize) -> (String, usize) {
        let from = self.cursor.saturating_sub(room.saturating_sub(1));
        let shown = self.chars.iter().skip(from).take(room).collect();
        (shown, self.cursor - from)
    }

    pub fn press(&mut self, key: KeyEvent) -> InputResult {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Enter => return InputResult::Submitted,
            KeyCode::Esc => return InputResult::Cancelled,
            KeyCode::Char('c') if control => return InputResult::Cancelled,
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.chars.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.chars.len(),
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.chars.remove(self.cursor);
            }
            KeyCode::Delete if self.cursor < self.chars.len() => {
                self.chars.remove(self.cursor);
            }
            KeyCode::Char('u') if control => {
                self.chars.clear();
                self.cursor = 0;
            }
            // Windows reports AltGr (`@`, `{`) as Ctrl and Alt together.
            KeyCode::Char(ch) if control == alt => {
                self.chars.insert(self.cursor, ch);
                self.cursor += 1;
            }
            _ => {}
        }
        InputResult::Editing
    }
}

/// A visible, editable line that starts with `initial`. Escape or Ctrl-C gives
/// `None`. Only call it in a terminal: it needs raw mode, and it fails clearly
/// when there is none.
pub fn read_line_with_initial(prompt: &str, initial: &str) -> std::io::Result<Option<String>> {
    let guard = RawModeGuard::enter_typing()?;
    let mut input = InputLine::new(initial);
    let result = loop {
        let columns = crossterm::terminal::size().map_or(80, |(columns, _)| columns);
        let room = usize::from(columns).saturating_sub(prompt.chars().count() + 1);
        let (shown, at) = input.window(room);
        print!(
            "\r\x1b[K{prompt}{shown}\r\x1b[{}C",
            prompt.chars().count() + at
        );
        std::io::stdout().flush()?;
        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match input.press(key) {
                InputResult::Editing => {}
                InputResult::Submitted => break Some(input.value()),
                InputResult::Cancelled => break None,
            }
        }
    };
    drop(guard);
    println!();
    Ok(result)
}

/// What one key press did to a secret being typed.
#[derive(Debug, PartialEq, Eq)]
enum SecretKey {
    Typing,
    Done,
    Cancelled,
}

fn apply_secret_key(secret: &mut String, key: KeyEvent) -> SecretKey {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Enter => return SecretKey::Done,
        KeyCode::Esc => return SecretKey::Cancelled,
        KeyCode::Char('c') if control => return SecretKey::Cancelled,
        KeyCode::Char('u') if control => secret.clear(),
        KeyCode::Backspace => {
            secret.pop();
        }
        // Windows reports AltGr (`@`, `{`) as Ctrl and Alt together.
        KeyCode::Char(ch) if control == alt => secret.push(ch),
        _ => {}
    }
    SecretKey::Typing
}

/// The prompt line while a secret is typed: one `*` for each character. A long
/// secret stops at the edge of the terminal, so the line never wraps and the
/// next redraw always clears all of it.
fn masked_line(prompt: &str, typed: usize, columns: u16) -> String {
    let room = usize::from(columns).saturating_sub(prompt.chars().count() + 1);
    format!("\r\x1b[K{prompt}{}", "*".repeat(typed.min(room)))
}

/// Read a secret such as an API key. Each character shows as `*`, so a paste
/// is seen to land and the key itself never shows on screen.
pub fn read_hidden_line(prompt: &str) -> std::io::Result<String> {
    if !std::io::stdin().is_terminal() {
        // Piped input (tests and scripts): nothing to hide or draw.
        print!("{prompt}");
        return read_visible_line();
    }
    // Turn off any lingering terminal mouse reporting before prompting so
    // clicking the window to paste does not inject `\x1b[<...M` into stdin.
    print!("\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l");
    let guard = RawModeGuard::enter_typing()?;
    let mut secret = String::new();
    let outcome = 'typing: loop {
        let columns = crossterm::terminal::size().map_or(80, |(columns, _)| columns);
        print!("{}", masked_line(prompt, secret.chars().count(), columns));
        std::io::stdout().flush()?;
        // A paste arrives as one burst of keys: draw once after the burst.
        loop {
            if let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                match apply_secret_key(&mut secret, key) {
                    SecretKey::Typing => {}
                    finished => break 'typing finished,
                }
            }
            if !event::poll(std::time::Duration::ZERO)? {
                break;
            }
        }
    };
    drop(guard);
    println!();
    match outcome {
        SecretKey::Cancelled => Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "Cancelled. Nothing was saved.",
        )),
        _ => Ok(secret.trim().to_string()),
    }
}

/// Print `options` and read the pick. In a terminal, uses ↑/↓ arrow keys and
/// live type-to-filter; on piped input, prints numbered rows and reads a number
/// or exact name. Enter takes the option at `default`. Returns its index.
pub fn choose(question: &str, options: &[String], default: usize) -> Result<usize> {
    if options.is_empty() {
        bail!("no options to choose from");
    }
    if is_interactive() {
        return tty_pick(question, options, default, false)?
            .context("No option was picked. Run the command again to choose.");
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

struct RawModeGuard {
    hides_cursor: bool,
}

impl RawModeGuard {
    fn enter() -> std::io::Result<Self> {
        enable_raw_mode()?;
        print!("\x1b[?25l");
        std::io::stdout().flush()?;
        Ok(Self { hides_cursor: true })
    }

    /// For typed text: the cursor stays visible.
    fn enter_typing() -> std::io::Result<Self> {
        enable_raw_mode()?;
        Ok(Self {
            hides_cursor: false,
        })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        if self.hides_cursor {
            print!("\x1b[?25h");
            let _ = std::io::stdout().flush();
        }
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

    fn type_into(input: &mut InputLine, keys: &[(KeyCode, KeyModifiers)]) -> InputResult {
        let mut last = InputResult::Editing;
        for (code, modifiers) in keys {
            last = input.press(KeyEvent::new(*code, *modifiers));
        }
        last
    }

    #[test]
    fn an_input_line_edits_in_the_middle_and_ends_on_enter_or_escape() {
        let none = KeyModifiers::NONE;
        let mut input = InputLine::new("work");
        assert_eq!(input.value(), "work");
        type_into(&mut input, &[(KeyCode::Left, none), (KeyCode::Left, none)]);
        type_into(
            &mut input,
            &[(KeyCode::Char('-'), none), (KeyCode::Char('x'), none)],
        );
        assert_eq!(input.value(), "wo-xrk");
        type_into(
            &mut input,
            &[(KeyCode::Backspace, none), (KeyCode::Delete, none)],
        );
        assert_eq!(input.value(), "wo-k");
        type_into(
            &mut input,
            &[(KeyCode::Home, none), (KeyCode::Delete, none)],
        );
        assert_eq!(input.value(), "o-k");
        // An AltGr character counts as typing, like in the secret field.
        let alt_gr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        type_into(
            &mut input,
            &[(KeyCode::End, none), (KeyCode::Char('@'), alt_gr)],
        );
        assert_eq!(input.value(), "o-k@");
        // A narrow window scrolls to keep the cursor in view.
        assert_eq!(input.window(10), ("o-k@".to_string(), 4));
        assert_eq!(input.window(3), ("k@".to_string(), 2));

        assert_eq!(
            type_into(&mut input, &[(KeyCode::Char('u'), KeyModifiers::CONTROL)]),
            InputResult::Editing
        );
        assert_eq!(input.value(), "");
        assert_eq!(
            type_into(&mut input, &[(KeyCode::Enter, none)]),
            InputResult::Submitted
        );
        assert_eq!(
            type_into(&mut input, &[(KeyCode::Esc, none)]),
            InputResult::Cancelled
        );
        assert_eq!(
            type_into(&mut input, &[(KeyCode::Char('c'), KeyModifiers::CONTROL)]),
            InputResult::Cancelled
        );
    }

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

    fn type_keys(secret: &mut String, keys: &[(KeyCode, KeyModifiers)]) -> SecretKey {
        let mut last = SecretKey::Typing;
        for (code, modifiers) in keys {
            last = apply_secret_key(secret, KeyEvent::new(*code, *modifiers));
        }
        last
    }

    #[test]
    fn a_secret_collects_typed_characters_and_edits_with_backspace_and_ctrl_u() {
        let none = KeyModifiers::NONE;
        let mut secret = String::new();
        let typed = "sk-ab12".chars().map(|ch| (KeyCode::Char(ch), none));
        assert_eq!(
            type_keys(&mut secret, &typed.collect::<Vec<_>>()),
            SecretKey::Typing
        );
        assert_eq!(secret, "sk-ab12");
        type_keys(&mut secret, &[(KeyCode::Backspace, none)]);
        assert_eq!(secret, "sk-ab1");
        // A shifted letter and an AltGr character both count as typing.
        type_keys(
            &mut secret,
            &[
                (KeyCode::Char('Z'), KeyModifiers::SHIFT),
                (
                    KeyCode::Char('@'),
                    KeyModifiers::CONTROL | KeyModifiers::ALT,
                ),
            ],
        );
        assert_eq!(secret, "sk-ab1Z@");
        // Other Ctrl and Alt combinations and arrow keys type nothing.
        type_keys(
            &mut secret,
            &[
                (KeyCode::Char('a'), KeyModifiers::CONTROL),
                (KeyCode::Char('b'), KeyModifiers::ALT),
                (KeyCode::Left, none),
            ],
        );
        assert_eq!(secret, "sk-ab1Z@");
        type_keys(&mut secret, &[(KeyCode::Char('u'), KeyModifiers::CONTROL)]);
        assert!(secret.is_empty());
        // Backspace on an empty secret is harmless.
        type_keys(&mut secret, &[(KeyCode::Backspace, none)]);
        assert!(secret.is_empty());
    }

    #[test]
    fn enter_finishes_and_escape_or_ctrl_c_cancel_a_secret() {
        let mut secret = String::from("sk-123");
        let press = |secret: &mut String, code, modifiers| {
            apply_secret_key(secret, KeyEvent::new(code, modifiers))
        };
        assert_eq!(
            press(&mut secret, KeyCode::Enter, KeyModifiers::NONE),
            SecretKey::Done
        );
        assert_eq!(
            press(&mut secret, KeyCode::Esc, KeyModifiers::NONE),
            SecretKey::Cancelled
        );
        assert_eq!(
            press(&mut secret, KeyCode::Char('c'), KeyModifiers::CONTROL),
            SecretKey::Cancelled
        );
        assert_eq!(secret, "sk-123");
    }

    #[test]
    fn the_masked_line_shows_one_star_per_character_and_never_the_secret() {
        assert_eq!(masked_line("API key: ", 0, 80), "\r\x1b[KAPI key: ");
        assert_eq!(masked_line("API key: ", 5, 80), "\r\x1b[KAPI key: *****");
        // A key longer than the terminal stops one column short of the edge.
        let long = masked_line("API key: ", 200, 40);
        assert_eq!(long.matches('*').count(), 40 - "API key: ".len() - 1);
        // A terminal narrower than the prompt draws no stars and never underflows.
        assert_eq!(masked_line("API key: ", 7, 5), "\r\x1b[KAPI key: ");
    }
}
