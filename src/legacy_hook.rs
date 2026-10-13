//! Takes out the shell hook that older versions added.
//!
//! Versions up to 0.1.x put a `claude` function in the shell's startup file so
//! plain `claude` ran `cc-proxy claude`. Routing now lives in Claude Code's
//! settings, so the function has nothing left to call. This only removes it:
//! `cc-proxy start` does it once, and the old `cc-proxy claude` command does it
//! when a terminal that still has the function calls it.
//!
//! ponytail: delete this file, and the `claude` command that calls it, a few
//! releases after 0.2.0, when nobody still has the hook.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::{fsutil, paths};

/// Ends the line the old install added to the startup file.
const MARKER: &str = "# cc-proxy shell hook";

/// First line of the function files the old install wrote.
const HEADER: &str =
    "# Written by `cc-proxy shell install`. `cc-proxy shell uninstall` removes it.";

struct Home {
    home: PathBuf,
    zdotdir: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
}

impl Home {
    fn current() -> Self {
        let var = |name| std::env::var_os(name).filter(|value| !value.is_empty());
        Self {
            home: PathBuf::from(paths::DirResolverEnv::default().home),
            zdotdir: var("ZDOTDIR").map(PathBuf::from),
            xdg_config_home: var("XDG_CONFIG_HOME").map(PathBuf::from),
        }
    }

    /// Every file the old install may have touched, whatever shell is in use now.
    fn startup_files(&self) -> Vec<PathBuf> {
        let zshrc = self.zdotdir.as_ref().unwrap_or(&self.home).join(".zshrc");
        let mut files = vec![zshrc];
        files.extend(
            [
                ".zshrc",
                ".bashrc",
                ".bash_profile",
                ".bash_login",
                ".profile",
            ]
            .map(|name| self.home.join(name)),
        );
        files.dedup();
        files
    }

    fn fish_function(&self) -> PathBuf {
        self.xdg_config_home
            .clone()
            .unwrap_or_else(|| self.home.join(".config"))
            .join("fish/functions/claude.fish")
    }

    /// `~/.zshrc` rather than the full path, in what we print.
    fn show(&self, path: &Path) -> String {
        match path.strip_prefix(&self.home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        }
    }
}

/// `content` without any line the old install added.
fn without_hook(content: &str) -> String {
    content
        .split_inclusive('\n')
        .filter(|line| !line.trim_end().ends_with(MARKER))
        .collect()
}

fn read_or_empty(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error).with_context(|| format!("couldn't read {}", path.display())),
    }
}

/// Remove the hook from every file that has it. Returns the files it changed,
/// as the user would write them (`~/.zshrc`).
pub fn remove() -> Result<Vec<String>> {
    let home = Home::current();
    let mut removed = Vec::new();
    for file in home.startup_files() {
        let content = read_or_empty(&file)?;
        let updated = without_hook(&content);
        if updated != content {
            fsutil::write_atomic(&file, updated.as_bytes())
                .with_context(|| format!("couldn't write {}", file.display()))?;
            removed.push(home.show(&file));
        }
    }
    // Only a file the old install wrote: a claude.fish of the user's own stays.
    let fish = home.fish_function();
    if read_or_empty(&fish)?.starts_with(HEADER) {
        std::fs::remove_file(&fish)?;
        removed.push(home.show(&fish));
    }
    let function_file = paths::config_dir().join("shell").join("claude.sh");
    if function_file.exists() {
        std::fs::remove_file(&function_file)?;
        if let Some(dir) = function_file.parent() {
            // Fails, harmlessly, when something else is in the dir.
            let _ = std::fs::remove_dir(dir);
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_line_the_old_install_added_goes() {
        let line = format!(
            "[ -f '/home/ada/.config/cc-proxy/shell/claude.sh' ] && . '/home/ada/.config/cc-proxy/shell/claude.sh'  {MARKER}"
        );
        let original = "export PATH=\"$HOME/bin:$PATH\"\nalias ll='ls -l'\n";
        assert_eq!(without_hook(&format!("{original}{line}\n")), original);
        assert_eq!(without_hook(original), original);
        // The last line, with no newline after it.
        assert_eq!(without_hook(&format!("{original}{line}")), original);
    }

    #[test]
    fn startup_files_follow_zdotdir_first() {
        let home = Home {
            home: PathBuf::from("/home/ada"),
            zdotdir: Some(PathBuf::from("/home/ada/.config/zsh")),
            xdg_config_home: None,
        };
        let files = home.startup_files();
        assert_eq!(files[0], PathBuf::from("/home/ada/.config/zsh/.zshrc"));
        assert!(files.contains(&PathBuf::from("/home/ada/.bashrc")));
        assert_eq!(
            home.fish_function(),
            PathBuf::from("/home/ada/.config/fish/functions/claude.fish")
        );
    }
}
