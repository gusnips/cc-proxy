//! Questions on stdin and stdout, for `cc-proxy start`, `cc-proxy use` and the login flows.
//! Piped input answers them the same way a keyboard does.

use std::io::{BufRead, Write};

use anyhow::{Result, bail};

/// How many options `choose` prints. The rest can still be typed by name.
const MAX_SHOWN: usize = 12;

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

/// Print `options` numbered and read the pick: a number, or an option's
/// exact name. Enter takes the option at `default`. Returns its index.
pub fn choose(question: &str, options: &[String], default: usize) -> Result<usize> {
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
/// Shows the first options, then reads a line: a number picks from the list
/// shown, an option's exact name picks it, other text searches every option,
/// and Enter keeps what is there. Input that has ended keeps it too. Returns
/// the index picked, or `None` to keep.
pub fn search(question: &str, options: &[String]) -> Result<Option<usize>> {
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
        }
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

/// The options that hold every word of `query`, in any case, in list order.
fn matching(options: &[String], query: &str) -> Vec<usize> {
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
