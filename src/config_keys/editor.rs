//! Open config.json in the user's editor, on the line of one key when it can
//! be found.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::paths;

/// Opens config.json in `$VISUAL`, else `$EDITOR`, and waits for it to close.
/// With `key` (a dotted key such as `providers`) the editor starts on the line
/// where that key is written, if it takes a line on its command line.
pub(super) fn open(key: Option<&str>) -> Result<()> {
    let dir = paths::config_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("config.json");
    let editor = editor_words()?;
    let line = key.and_then(|key| line_of(&std::fs::read_to_string(&path).ok()?, key));
    let argv = with_file(editor, &path, line);
    let (program, args) = argv
        .split_first()
        .expect("with_file keeps the editor program");
    // `status()` hands the editor this terminal's stdin, stdout and stderr.
    let status = std::process::Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("failed to launch editor `{program}`"))?;
    if !status.success() {
        bail!("editor exited with {status}");
    }
    // Validate on close so a typo does not silently break the next start.
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    if !raw.trim().is_empty() {
        serde_json::from_str::<Value>(&raw)
            .with_context(|| format!("{} is not valid JSON after editing", path.display()))?;
    }
    Ok(())
}

/// The program and arguments in `$VISUAL`, else `$EDITOR`.
fn editor_words() -> Result<Vec<String>> {
    let raw = ["VISUAL", "EDITOR"]
        .into_iter()
        .find_map(|name| {
            std::env::var(name)
                .ok()
                .filter(|raw| !raw.trim().is_empty())
        })
        .context("set $VISUAL or $EDITOR to choose an editor")?;
    let words = split_words(&raw)?;
    if words.is_empty() {
        bail!("$VISUAL or $EDITOR names no program. Set it to an editor, such as `nano`.");
    }
    Ok(words)
}

/// Splits a command line into words the way a shell does for quotes and
/// backslashes, and no further: no variables, no pipes. The result is run as a
/// program with arguments, never through a shell.
fn split_words(raw: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (Some(open), ch) if ch == open => quote = None,
            // In double quotes a backslash only protects a quote or another backslash.
            (Some('"'), '\\') if !cfg!(windows) => match chars.next() {
                Some(next @ ('"' | '\\')) => word.push(next),
                Some(next) => word.extend(['\\', next]),
                None => bail!("$VISUAL or $EDITOR has an opening quote with no closing quote."),
            },
            (Some(_), ch) => word.push(ch),
            (None, '\'' | '"') => {
                quote = Some(ch);
                in_word = true;
            }
            // A Windows path is full of backslashes, so there they stay as typed.
            (None, '\\') if !cfg!(windows) => {
                word.push(
                    chars
                        .next()
                        .context("$VISUAL or $EDITOR ends with a backslash.")?,
                );
                in_word = true;
            }
            (None, ch) if ch.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            (None, ch) => {
                word.push(ch);
                in_word = true;
            }
        }
    }
    if quote.is_some() {
        bail!("$VISUAL or $EDITOR has an opening quote with no closing quote.");
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

/// The editor's command line with the file added, and the line to start on
/// for the editors that take one. An editor that isn't known gets only the
/// file: it opens at the top.
fn with_file(mut argv: Vec<String>, path: &Path, line: Option<usize>) -> Vec<String> {
    let program = Path::new(&argv[0])
        .file_stem()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let path = path.to_string_lossy().into_owned();
    match program.as_str() {
        "vi" | "vim" | "nvim" | "nano" | "emacs" => {
            argv.extend(line.map(|line| format!("+{line}")));
            argv.push(path);
        }
        // These return at once unless told to wait, and the screen would
        // read the file before anything was saved.
        "code" | "code-insiders" | "codium" | "cursor" | "windsurf" => {
            if !argv.iter().any(|word| word == "--wait" || word == "-w") {
                argv.push("--wait".to_string());
            }
            match line {
                Some(line) => argv.extend(["--goto".to_string(), format!("{path}:{line}")]),
                None => argv.push(path),
            }
        }
        _ => argv.push(path),
    }
    argv
}

/// The line (counting from 1) where `dotted` is written in `text`; where its
/// nearest parent is when the key isn't there. `None` when `text` isn't JSON or
/// the key is written twice, so the editor opens at the top instead of at a guess.
fn line_of(text: &str, dotted: &str) -> Option<usize> {
    serde_json::from_str::<Value>(text).ok()?;
    let mut path = dotted;
    loop {
        match key_lines(text, path).as_slice() {
            [line] => return Some(*line),
            [] => path = path.rsplit_once('.')?.0,
            _ => return None,
        }
    }
}

enum Frame {
    List,
    /// The key whose value is being read.
    Object(String),
}

/// Every line where a key with this dotted path is written. A plain scan of the
/// text, so the file is never reformatted to find a line.
fn key_lines(text: &str, dotted: &str) -> Vec<usize> {
    let mut frames: Vec<Frame> = Vec::new();
    // Inside an object, the next string is a key; after a value it is not.
    let mut want_key = false;
    let mut line = 1;
    let mut found = Vec::new();
    let mut chars = text.char_indices();
    while let Some((start, ch)) = chars.next() {
        match ch {
            '\n' => line += 1,
            '{' => {
                frames.push(Frame::Object(String::new()));
                want_key = true;
            }
            '[' => {
                frames.push(Frame::List);
                want_key = false;
            }
            '}' | ']' => {
                frames.pop();
                want_key = false;
            }
            ',' => want_key = matches!(frames.last(), Some(Frame::Object(_))),
            '"' => {
                let mut end = start;
                let mut escaped = false;
                for (index, ch) in chars.by_ref() {
                    end = index;
                    match (escaped, ch) {
                        (true, _) => escaped = false,
                        (false, '\\') => escaped = true,
                        (false, '"') => break,
                        _ => {}
                    }
                }
                if !want_key {
                    continue;
                }
                want_key = false;
                let Ok(name) = serde_json::from_str::<String>(&text[start..=end]) else {
                    continue;
                };
                if let Some(Frame::Object(current)) = frames.last_mut() {
                    *current = name;
                }
                if dotted_path(&frames).is_some_and(|path| path == dotted) {
                    found.push(line);
                }
            }
            _ => {}
        }
    }
    found
}

/// The keys from the top of the file down to the one being read. `None` inside a
/// list, where nothing has a dotted name.
fn dotted_path(frames: &[Frame]) -> Option<String> {
    let keys: Option<Vec<&str>> = frames
        .iter()
        .map(|frame| match frame {
            Frame::Object(key) => Some(key.as_str()),
            Frame::List => None,
        })
        .collect();
    Some(keys?.join("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(raw: &str) -> Vec<String> {
        split_words(raw).unwrap()
    }

    #[test]
    fn editor_words_split_like_a_shell_without_running_one() {
        assert_eq!(words("nano"), ["nano"]);
        assert_eq!(words("code --wait"), ["code", "--wait"]);
        assert_eq!(words("  vim   -u  NONE "), ["vim", "-u", "NONE"]);
        assert_eq!(
            words(r#""/Applications/Sublime Text.app/subl" -w"#),
            ["/Applications/Sublime Text.app/subl", "-w"]
        );
        assert_eq!(words("'my editor' --flag"), ["my editor", "--flag"]);
        assert_eq!(words("--name=''"), ["--name="]);
        assert_eq!(words(r#"say "a b" c"#), ["say", "a b", "c"]);
        assert_eq!(words(r#"a "" b"#), ["a", "", "b"]);
        // No expansion: these reach the program as text.
        assert_eq!(words("vim $HOME | cat"), ["vim", "$HOME", "|", "cat"]);
        if !cfg!(windows) {
            assert_eq!(words(r"my\ editor"), ["my editor"]);
            assert_eq!(words(r#""say \"hi\"""#), [r#"say "hi""#]);
            assert_eq!(words(r#""a\nb""#), [r"a\nb"]);
        }
        assert!(split_words("vim 'unfinished").is_err());
        assert!(split_words(r#"vim "unfinished"#).is_err());
        assert!(split_words("").unwrap().is_empty());
    }

    #[test]
    fn known_editors_get_the_line_and_the_rest_get_the_file() {
        let path = Path::new("/c/config.json");
        let run = |editor: &str, line| with_file(words(editor), path, line);

        assert_eq!(run("vim", Some(7)), ["vim", "+7", "/c/config.json"]);
        assert_eq!(
            run("/usr/bin/nano -w", Some(3)),
            ["/usr/bin/nano", "-w", "+3", "/c/config.json"]
        );
        assert_eq!(run("vim", None), ["vim", "/c/config.json"]);
        assert_eq!(
            run("code", Some(7)),
            ["code", "--wait", "--goto", "/c/config.json:7"]
        );
        assert_eq!(run("code -w", None), ["code", "-w", "/c/config.json"]);
        assert_eq!(
            run("cursor --wait", Some(2)),
            ["cursor", "--wait", "--goto", "/c/config.json:2"]
        );
        assert_eq!(run("helix", Some(7)), ["helix", "/c/config.json"]);
    }

    const FILE: &str = r#"{
  "port": 18765,
  "note": "\"providers\": not a key",
  "providers": {
    "ollama": { "baseUrl": "http://localhost:11434/v1" },
    "x\"y": 1
  },
  "route": { "providers": 1, "tiers": [ { "providers": 2 } ] }
}"#;

    #[test]
    fn a_key_is_found_by_its_whole_path() {
        assert_eq!(line_of(FILE, "port"), Some(2));
        assert_eq!(line_of(FILE, "providers"), Some(4));
        assert_eq!(line_of(FILE, "providers.ollama"), Some(5));
        assert_eq!(line_of(FILE, "providers.ollama.baseUrl"), Some(5));
        assert_eq!(line_of(FILE, "providers.x\"y"), Some(6));
        // The same name deeper in the file is a different key.
        assert_eq!(line_of(FILE, "route.providers"), Some(8));
    }

    #[test]
    fn a_missing_key_falls_back_to_its_nearest_parent() {
        assert_eq!(line_of(FILE, "providers.openrouter"), Some(4));
        assert_eq!(line_of(FILE, "providers.openrouter.apiKey"), Some(4));
        assert_eq!(line_of(FILE, "nothing"), None);
    }

    #[test]
    fn an_unsure_place_opens_at_the_top() {
        // Written twice: no way to say which one is meant.
        assert_eq!(line_of(r#"{"a": 1, "a": 2}"#, "a"), None);
        // Not JSON: a line number would be a guess.
        assert_eq!(line_of("{\"a\": 1,\n", "a"), None);
        // Nothing inside a list has a dotted name.
        assert_eq!(line_of(r#"{"list": [{"a": 1}]}"#, "list.a"), Some(1));
        assert_eq!(line_of("[{\"a\": 1}]", "a"), None);
        // Compact JSON is one line.
        assert_eq!(line_of(r#"{"a":{"b":1}}"#, "a.b"), Some(1));
    }
}
