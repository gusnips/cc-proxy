//! `cc-proxy shell install|uninstall` and `cc-proxy on|off`: make plain
//! `claude` go through cc-proxy, then switch it without editing a file again.
//!
//! Install writes a `claude` shell function and adds one line that loads it
//! to the shell's startup file (fish loads a function file by itself). The
//! function exports nothing: each call runs `cc-proxy claude`, which reads
//! `claude.enabled` and either starts Claude Code on the proxy or runs it
//! untouched. So `on` and `off` reach every open terminal at once, and no
//! other program on the machine ever sees the proxy's address.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::ui::{self, Mood};
use crate::{config, config_keys, paths};

/// Set by the shell function, so `cc-proxy claude` knows `claude.enabled`
/// applies. Run by hand, `cc-proxy claude` always uses the proxy.
pub const HOOK_ENV: &str = "CCP_SHELL_HOOK";

/// Ends the one line install adds, so uninstall finds exactly that line.
const MARKER: &str = "# cc-proxy shell hook";

const HEADER: &str =
    "# Written by `cc-proxy shell install`. `cc-proxy shell uninstall` removes it.";

// Without cc-proxy on PATH (uninstalled, or a shell with a trimmed PATH),
// `claude` still runs Claude Code.
const POSIX_FUNCTION: &str = r#"claude() {
  if command -v cc-proxy >/dev/null 2>&1; then
    CCP_SHELL_HOOK=1 cc-proxy claude "$@"
  else
    command claude "$@"
  fi
}
"#;

const FISH_FUNCTION: &str =
    "function claude --wraps claude --description 'Claude Code, through cc-proxy when it is on'
    if command -q cc-proxy
        env CCP_SHELL_HOOK=1 cc-proxy claude $argv
    else
        command claude $argv
    end
end
";

/// Where the hook goes for one shell.
#[derive(Debug, PartialEq, Eq)]
enum Target {
    /// A startup file that gets one line loading the function file.
    Rc(PathBuf),
    /// fish loads every file in its functions dir by itself.
    Fish(PathBuf),
}

struct Env {
    home: PathBuf,
    shell: String,
    zdotdir: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
}

impl Env {
    fn current() -> Self {
        let var = |name| std::env::var_os(name).filter(|value| !value.is_empty());
        Self {
            home: PathBuf::from(paths::DirResolverEnv::default().home),
            shell: std::env::var("SHELL").unwrap_or_default(),
            zdotdir: var("ZDOTDIR").map(PathBuf::from),
            xdg_config_home: var("XDG_CONFIG_HOME").map(PathBuf::from),
        }
    }

    fn shell_name(&self) -> &str {
        Path::new(&self.shell)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
    }

    fn zshrc(&self) -> PathBuf {
        self.zdotdir.as_ref().unwrap_or(&self.home).join(".zshrc")
    }

    fn fish_function(&self) -> PathBuf {
        self.xdg_config_home
            .clone()
            .unwrap_or_else(|| self.home.join(".config"))
            .join("fish/functions/claude.fish")
    }

    /// A login bash reads only the first of these that exists, so a new
    /// `.bash_profile` next to an existing `.profile` would hide the `.profile`.
    fn bash_login_file(&self) -> PathBuf {
        let files = [".bash_profile", ".bash_login", ".profile"].map(|name| self.home.join(name));
        let existing = files.iter().find(|file| file.exists());
        existing.unwrap_or(&files[0]).clone()
    }

    /// The file each shell reads when a terminal opens. macOS Terminal opens
    /// login shells, and a login bash doesn't read `.bashrc`.
    fn target(&self, os: &str) -> Result<Target, String> {
        match self.shell_name() {
            "zsh" => Ok(Target::Rc(self.zshrc())),
            "bash" if os == "macos" => Ok(Target::Rc(self.bash_login_file())),
            "bash" => Ok(Target::Rc(self.home.join(".bashrc"))),
            "fish" => Ok(Target::Fish(self.fish_function())),
            "" => Err(
                "cc-proxy couldn't tell which shell you use, because $SHELL isn't set. \
                 Run `cc-proxy claude` instead of `claude`; it takes the same arguments."
                    .to_string(),
            ),
            other => Err(format!(
                "cc-proxy can't add the hook to {other} yet. It works with zsh, bash and fish. \
                 Run `cc-proxy claude` instead of `claude`; it takes the same arguments."
            )),
        }
    }

    /// Every file an earlier install may have touched, whatever $SHELL says
    /// now: the user may have switched shells since.
    fn rc_files(&self) -> Vec<PathBuf> {
        let mut files = vec![
            self.zshrc(),
            self.home.join(".zshrc"),
            self.home.join(".bashrc"),
            self.home.join(".bash_profile"),
            self.home.join(".bash_login"),
            self.home.join(".profile"),
        ];
        files.dedup();
        files
    }

    /// `~/.zshrc` rather than the full path, in what we print.
    fn show(&self, path: &Path) -> String {
        match path.strip_prefix(&self.home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        }
    }
}

fn function_file() -> PathBuf {
    paths::config_dir().join("shell").join("claude.sh")
}

/// Single-quoted for sh, so a path with spaces or `$` stays one word.
fn sh_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

fn hook_line(function_file: &Path) -> String {
    let quoted = sh_quote(function_file);
    format!("[ -f {quoted} ] && . {quoted}  {MARKER}")
}

/// `content` without any line install added.
fn without_hook(content: &str) -> String {
    content
        .split_inclusive('\n')
        .filter(|line| !line.trim_end().ends_with(MARKER))
        .collect()
}

/// `content` with `line` as its only hook line. Unchanged when it's already
/// there, so a second install doesn't touch the file.
fn with_hook(content: &str, line: &str) -> String {
    if content.lines().any(|existing| existing == line) {
        return content.to_string();
    }
    let mut out = without_hook(content);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(line);
    out.push('\n');
    out
}

/// Write through `fs::write`, never a rename: a startup file is often a
/// symlink into a dotfiles repo, and the link must survive.
fn rewrite(path: &Path, content: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, content).with_context(|| format!("couldn't write {}", path.display()))
}

fn read_or_empty(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error).with_context(|| format!("couldn't read {}", path.display())),
    }
}

fn set_enabled(enabled: bool) -> Result<()> {
    config_keys::write_value("claude.enabled", serde_json::Value::Bool(enabled))?;
    Ok(())
}

fn hook_installed(env: &Env) -> bool {
    function_file().exists() || env.fish_function().exists()
}

pub fn install() -> Result<()> {
    let env = Env::current();
    let target = match env.target(std::env::consts::OS) {
        Ok(target) => target,
        Err(message) => {
            ui::eprint_note(Mood::Unsure, &[message]);
            std::process::exit(1);
        }
    };
    let file = match &target {
        Target::Rc(rc) => {
            let function_file = function_file();
            rewrite(&function_file, &format!("{HEADER}\n{POSIX_FUNCTION}"))?;
            let content = read_or_empty(rc)?;
            let updated = with_hook(&content, &hook_line(&function_file));
            if updated != content {
                rewrite(rc, &updated)?;
            }
            rc
        }
        Target::Fish(function) => {
            rewrite(function, &format!("{HEADER}\n{FISH_FUNCTION}"))?;
            function
        }
    };
    set_enabled(true)?;
    ui::print_note(
        Mood::Glad,
        &[
            format!("Added the cc-proxy hook to {}.", env.show(file)),
            format!(
                "In new terminals, `claude` now starts Claude Code on the proxy. In a \
                 terminal that's already open, run `exec {}` first.",
                env.shell_name()
            ),
            "`cc-proxy off` goes back to plain Claude Code in every terminal; `cc-proxy on` \
             switches back."
                .into(),
        ],
    );
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let env = Env::current();
    let mut removed = Vec::new();
    for rc in env.rc_files() {
        let content = read_or_empty(&rc)?;
        let updated = without_hook(&content);
        if updated != content {
            rewrite(&rc, &updated)?;
            removed.push(env.show(&rc));
        }
    }
    // Only a file install wrote: a claude.fish of the user's own stays.
    let fish = env.fish_function();
    if read_or_empty(&fish)?.starts_with(HEADER) {
        fs::remove_file(&fish)?;
        removed.push(env.show(&fish));
    }
    let function_file = function_file();
    if function_file.exists() {
        fs::remove_file(&function_file)?;
        if let Some(dir) = function_file.parent() {
            // Fails, harmlessly, when something else is in the dir.
            let _ = fs::remove_dir(dir);
        }
    }
    // Terminals that are still open keep the function until they close;
    // off sends it to plain Claude Code.
    set_enabled(false)?;
    if removed.is_empty() {
        ui::print_note(
            Mood::Asleep,
            &["The cc-proxy hook isn't installed, so there was nothing to remove.".into()],
        );
    } else {
        ui::print_note(
            Mood::Asleep,
            &[
                format!("Removed the cc-proxy hook from {}.", removed.join(" and ")),
                "`claude` starts plain Claude Code again, in every terminal. \
                 `cc-proxy claude` still uses the proxy."
                    .into(),
            ],
        );
    }
    Ok(())
}

pub fn set(on: bool) -> Result<()> {
    set_enabled(on)?;
    let open_sessions =
        "Claude Code sessions that are already open keep their connection until you quit them.";
    let (mood, lines) = match (on, hook_installed(&Env::current())) {
        (true, true) => (
            Mood::Awake,
            [
                "cc-proxy is on.",
                "`claude` in any terminal now starts Claude Code on the proxy.",
            ],
        ),
        (true, false) => (
            Mood::Unsure,
            [
                "cc-proxy is on, but plain `claude` won't use it until you run \
                 `cc-proxy shell install`.",
                "`cc-proxy claude` uses the proxy either way.",
            ],
        ),
        (false, _) => (
            Mood::Asleep,
            [
                "cc-proxy is off.",
                "`claude` in any terminal now starts plain Claude Code. `cc-proxy claude` \
                 still uses the proxy.",
            ],
        ),
    };
    let lines = lines.into_iter().chain([open_sessions]).map(String::from);
    ui::print_note(mood, &lines.collect::<Vec<_>>());
    Ok(())
}

/// Whether plain `claude` goes through the proxy: the hook is in and it's on.
pub fn plain_claude_uses_proxy() -> bool {
    hook_installed(&Env::current()) && config::claude_enabled()
}

/// What plain `claude` does right now, for `cc-proxy status`.
pub fn plain_claude_line() -> &'static str {
    match (hook_installed(&Env::current()), config::claude_enabled()) {
        (true, true) => "Plain `claude` goes through it.",
        (true, false) => "Plain `claude` skips it. `cc-proxy on` sends it through.",
        (false, _) => "Plain `claude` skips it. `cc-proxy shell install` sends it through.",
    }
}

/// Through the hook, `claude.enabled` decides; run by hand, always the proxy.
pub fn wants_proxy() -> bool {
    std::env::var_os(HOOK_ENV).is_none() || config::claude_enabled()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(shell: &str) -> Env {
        Env {
            home: PathBuf::from("/home/ada"),
            shell: shell.to_string(),
            zdotdir: None,
            xdg_config_home: None,
        }
    }

    #[test]
    fn each_shell_gets_the_file_it_reads_when_a_terminal_opens() {
        let rc = |path: &str| Ok(Target::Rc(PathBuf::from(path)));
        assert_eq!(env("/bin/zsh").target("macos"), rc("/home/ada/.zshrc"));
        assert_eq!(
            env("/bin/bash").target("macos"),
            rc("/home/ada/.bash_profile")
        );
        assert_eq!(
            env("/usr/bin/bash").target("linux"),
            rc("/home/ada/.bashrc")
        );
        assert_eq!(
            env("/usr/bin/fish").target("linux"),
            Ok(Target::Fish(PathBuf::from(
                "/home/ada/.config/fish/functions/claude.fish"
            )))
        );
        let zdotdir = Env {
            zdotdir: Some(PathBuf::from("/home/ada/.config/zsh")),
            ..env("/bin/zsh")
        };
        assert_eq!(zdotdir.target("linux"), rc("/home/ada/.config/zsh/.zshrc"));
        assert!(
            env("/bin/tcsh")
                .target("linux")
                .unwrap_err()
                .contains("tcsh")
        );
        assert!(env("").target("windows").unwrap_err().contains("$SHELL"));
    }

    #[test]
    fn a_login_bash_keeps_reading_the_file_it_already_reads() {
        let home = tempfile::tempdir().unwrap();
        let bash = Env {
            home: home.path().to_path_buf(),
            ..env("/bin/bash")
        };
        fs::write(home.path().join(".profile"), "").unwrap();
        assert_eq!(
            bash.target("macos"),
            Ok(Target::Rc(home.path().join(".profile")))
        );
        fs::write(home.path().join(".bash_profile"), "").unwrap();
        assert_eq!(
            bash.target("macos"),
            Ok(Target::Rc(home.path().join(".bash_profile")))
        );
    }

    #[test]
    fn install_adds_one_line_and_uninstall_takes_exactly_it_back() {
        let line = hook_line(Path::new("/home/ada/.config/cc-proxy/shell/claude.sh"));
        let original = "export PATH=\"$HOME/bin:$PATH\"\nalias ll='ls -l'\n";

        let installed = with_hook(original, &line);
        assert_eq!(installed, format!("{original}{line}\n"));
        assert_eq!(with_hook(&installed, &line), installed);
        assert_eq!(without_hook(&installed), original);

        // A file without a final newline still gets the line on its own row.
        assert_eq!(
            with_hook("alias ll='ls -l'", &line),
            format!("alias ll='ls -l'\n{line}\n")
        );
        // A moved config dir replaces the old line instead of adding a second.
        let moved = hook_line(Path::new("/elsewhere/claude.sh"));
        assert_eq!(
            with_hook(&installed, &moved),
            format!("{original}{moved}\n")
        );
    }

    #[test]
    fn the_hook_line_keeps_an_awkward_path_in_one_piece() {
        assert_eq!(
            hook_line(Path::new("/Users/a b/it's/claude.sh")),
            r"[ -f '/Users/a b/it'\''s/claude.sh' ] && . '/Users/a b/it'\''s/claude.sh'  # cc-proxy shell hook"
        );
    }
}
