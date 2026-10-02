//! Questions on stdin and stdout, for `cc-proxy setup` and the login flows.
//! Piped input answers them the same way a keyboard does.

use std::io::{BufRead, Write};

use anyhow::{Result, bail};

/// How many options `choose` prints. The rest can still be typed by name.
const MAX_SHOWN: usize = 12;

/// One line from stdin, trimmed. `None` when the input has ended.
fn read_line() -> std::io::Result<Option<String>> {
    std::io::stdout().flush()?;
    let mut buf = String::new();
    let read = std::io::stdin().lock().read_line(&mut buf)?;
    Ok((read > 0).then(|| buf.trim().to_string()))
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

    print!("{prompt}");
    std::io::stdout().flush()?;
    let fd = std::io::stdin().as_raw_fd();
    // SAFETY: tcgetattr/tcsetattr only touch the termios struct for our own
    // stdin fd, and the original flags are restored before returning.
    unsafe {
        let mut original: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut original) != 0 {
            // Not a TTY (piped stdin in tests and scripts): plain read.
            return read_visible_line();
        }
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
            "  ...and {} more. Type a number or a name; `cc-proxy models` lists them all.",
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

/// A yes/no question. Enter, or input that has ended, takes `default`.
pub fn confirm(question: &str, default: bool) -> Result<bool> {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    loop {
        print!("{question} {hint} ");
        let Some(answer) = read_line()? else {
            return Ok(default);
        };
        match answer.to_ascii_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => println!("Answer y or n."),
        }
    }
}
