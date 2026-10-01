// Lifecycle tests for the background proxy service: serve, status, restart,
// stop, and reload against an isolated state directory.
use assert_cmd::Command;
use predicates::str::contains;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn isolated_env(cmd: &mut Command, temp: &TempDir, port: u16) {
    cmd.env("CCP_CONFIG_DIR", temp.path().join("config"));
    cmd.env("XDG_STATE_HOME", temp.path().join("state"));
    cmd.env("HOME", temp.path());
    // Pin the probe port so status/stop/reload never touch the machine's
    // real proxy port (or another test's) when no daemon is tracked.
    cmd.env("PORT", port.to_string());
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Runs `cc-proxy stop` on drop so a failed test does not leave a daemon
/// behind on a test port.
struct DaemonGuard {
    env: Vec<(String, String)>,
}

impl DaemonGuard {
    fn new(temp: &TempDir, port: u16) -> Self {
        let env = [
            (
                "CCP_CONFIG_DIR".to_string(),
                temp.path().join("config").to_string_lossy().into_owned(),
            ),
            (
                "XDG_STATE_HOME".to_string(),
                temp.path().join("state").to_string_lossy().into_owned(),
            ),
            (
                "HOME".to_string(),
                temp.path().to_string_lossy().into_owned(),
            ),
            ("PORT".to_string(), port.to_string()),
        ]
        .into_iter()
        .collect();
        Self { env }
    }

    fn stop(&self) {
        let mut cmd = Command::cargo_bin("cc-proxy").unwrap();
        for (key, value) in &self.env {
            cmd.env(key, value);
        }
        let _ = cmd.arg("stop").output();
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

fn wait_for_status(guard: &DaemonGuard, running: bool) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let mut cmd = Command::cargo_bin("cc-proxy")?;
        for (key, value) in &guard.env {
            cmd.env(key, value);
        }
        let output = cmd.arg("status").output()?;
        if output.status.success() == running {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "status did not become running={running}: {}",
                String::from_utf8_lossy(&output.stdout)
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn status_reports_not_running_without_daemon() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let mut cmd = Command::cargo_bin("cc-proxy")?;
    isolated_env(&mut cmd, &temp, free_port());
    cmd.arg("status")
        .assert()
        .failure()
        .code(1)
        .stdout(contains("not running"));
    Ok(())
}

#[test]
fn stop_without_daemon_is_success() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let mut cmd = Command::cargo_bin("cc-proxy")?;
    isolated_env(&mut cmd, &temp, free_port());
    cmd.arg("stop")
        .assert()
        .success()
        .stdout(contains("not running"));
    Ok(())
}

#[test]
fn reload_without_daemon_validates_config() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let mut cmd = Command::cargo_bin("cc-proxy")?;
    isolated_env(&mut cmd, &temp, free_port());
    cmd.arg("reload")
        .assert()
        .failure()
        .stdout(contains("config file is valid"));
    Ok(())
}

#[test]
fn reload_rejects_invalid_config() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let config_dir = temp.path().join("config");
    std::fs::create_dir_all(&config_dir)?;
    std::fs::write(config_dir.join("config.json"), "{not json")?;
    let mut cmd = Command::cargo_bin("cc-proxy")?;
    isolated_env(&mut cmd, &temp, free_port());
    cmd.arg("reload")
        .assert()
        .failure()
        .stderr(contains("invalid config"));
    Ok(())
}

#[test]
fn serve_status_and_stop_round_trip() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let port = free_port();
    let guard = DaemonGuard::new(&temp, port);

    let mut serve = Command::cargo_bin("cc-proxy")?;
    for (key, value) in &guard.env {
        serve.env(key, value);
    }
    serve
        .args(["serve", "--port", &port.to_string()])
        .assert()
        .success()
        .stdout(contains("started"));

    wait_for_status(&guard, true)?;

    // A second serve refuses to double-start.
    let mut again = Command::cargo_bin("cc-proxy")?;
    for (key, value) in &guard.env {
        again.env(key, value);
    }
    again
        .args(["serve", "--port", &port.to_string()])
        .assert()
        .success()
        .stdout(contains("already running"));

    let mut stop = Command::cargo_bin("cc-proxy")?;
    for (key, value) in &guard.env {
        stop.env(key, value);
    }
    stop.arg("stop")
        .assert()
        .success()
        .stdout(contains("stopped"));

    wait_for_status(&guard, false)?;
    Ok(())
}

#[test]
fn restart_keeps_serving() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let port = free_port();
    let guard = DaemonGuard::new(&temp, port);

    let mut serve = Command::cargo_bin("cc-proxy")?;
    for (key, value) in &guard.env {
        serve.env(key, value);
    }
    serve
        .args(["serve", "--port", &port.to_string()])
        .assert()
        .success();
    wait_for_status(&guard, true)?;

    let mut restart = Command::cargo_bin("cc-proxy")?;
    for (key, value) in &guard.env {
        restart.env(key, value);
    }
    restart
        .args(["restart", "--port", &port.to_string()])
        .assert()
        .success()
        .stdout(contains("restarted"));

    wait_for_status(&guard, true)?;
    Ok(())
}

/// A stand-in `claude` that writes each argument it got on its own line.
#[cfg(unix)]
fn fake_claude(temp: &TempDir) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;
    let bin = temp.path().join("bin");
    std::fs::create_dir_all(&bin)?;
    let script = bin.join("claude");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$CLAUDE_ARGS_OUT\"\n",
    )?;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
    Ok(bin)
}

#[cfg(unix)]
#[test]
fn claude_starts_the_proxy_and_passes_every_argument() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let port = free_port();
    let guard = DaemonGuard::new(&temp, port);
    let config_dir = temp.path().join("config");
    std::fs::create_dir_all(&config_dir)?;
    std::fs::write(
        config_dir.join("config.json"),
        r#"{"claude":{"model":"k3[1m]","fastModel":"k3"}}"#,
    )?;
    let bin = fake_claude(&temp)?;
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )?;
    let out = temp.path().join("args.txt");

    let mut cmd = Command::cargo_bin("cc-proxy")?;
    for (key, value) in &guard.env {
        cmd.env(key, value);
    }
    cmd.env("PATH", path)
        .env("CLAUDE_ARGS_OUT", &out)
        .args(["claude", "--resume", "abc", "-p", "hi there", "--worktree"])
        .assert()
        .success()
        .stderr(contains("Started cc-proxy"));

    let recorded = std::fs::read_to_string(&out)?;
    let args: Vec<&str> = recorded.lines().collect();
    assert_eq!(args[0], "--settings");
    let settings: serde_json::Value = serde_json::from_str(args[1])?;
    let env = &settings["env"];
    assert_eq!(
        env["ANTHROPIC_BASE_URL"],
        format!("http://127.0.0.1:{port}")
    );
    assert_eq!(env["ANTHROPIC_MODEL"], "k3[1m]");
    assert_eq!(env["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "k3");
    assert_eq!(
        args[2..],
        ["--resume", "abc", "-p", "hi there", "--worktree"]
    );
    wait_for_status(&guard, true)?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn claude_missing_from_path_says_how_to_fix_it() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let port = free_port();
    let guard = DaemonGuard::new(&temp, port);
    let empty = temp.path().join("empty");
    std::fs::create_dir_all(&empty)?;

    let mut cmd = Command::cargo_bin("cc-proxy")?;
    for (key, value) in &guard.env {
        cmd.env(key, value);
    }
    cmd.env("PATH", &empty)
        .arg("claude")
        .assert()
        .failure()
        .code(127)
        .stderr(contains("there's no `claude` on your PATH"));
    Ok(())
}

/// `cc-proxy` with this test's isolated state, plus `extra` env.
#[cfg(unix)]
fn run_isolated(
    guard: &DaemonGuard,
    extra: &[(&str, &std::ffi::OsStr)],
    args: &[&str],
) -> Result<std::process::Output, Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("cc-proxy")?;
    for (key, value) in &guard.env {
        cmd.env(key, value);
    }
    for (key, value) in extra {
        cmd.env(key, value);
    }
    Ok(cmd.env_remove("ZDOTDIR").args(args).output()?)
}

#[cfg(unix)]
#[test]
fn shell_install_adds_one_line_and_uninstall_restores_the_file()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let guard = DaemonGuard::new(&temp, free_port());
    let zsh = [("SHELL", std::ffi::OsStr::new("/bin/zsh"))];
    let zshrc = temp.path().join(".zshrc");
    let original = "export EDITOR=vim\nalias ll='ls -l'\n";
    std::fs::write(&zshrc, original)?;

    for _ in 0..2 {
        let output = run_isolated(&guard, &zsh, &["shell", "install"])?;
        assert!(output.status.success(), "{output:?}");
    }
    let installed = std::fs::read_to_string(&zshrc)?;
    assert!(installed.starts_with(original));
    assert_eq!(installed.matches("# cc-proxy shell hook").count(), 1);
    let function = temp.path().join("config/shell/claude.sh");
    assert!(function.exists());

    let output = run_isolated(&guard, &zsh, &["shell", "uninstall"])?;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(std::fs::read_to_string(&zshrc)?, original);
    assert!(!function.exists());
    Ok(())
}

#[cfg(unix)]
#[test]
fn the_hook_runs_plain_claude_when_cc_proxy_is_off() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let guard = DaemonGuard::new(&temp, free_port());
    let zsh = [("SHELL", std::ffi::OsStr::new("/bin/zsh"))];
    for args in [&["shell", "install"][..], &["off"]] {
        let output = run_isolated(&guard, &zsh, args)?;
        assert!(output.status.success(), "{output:?}");
    }

    // Load the function the way the startup file does, then call it.
    let cc_proxy_dir = assert_cmd::cargo::cargo_bin("cc-proxy")
        .parent()
        .ok_or("cc-proxy has no parent dir")?
        .to_path_buf();
    let path = std::env::join_paths(
        [fake_claude(&temp)?, cc_proxy_dir]
            .into_iter()
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )?;
    let out = temp.path().join("args.txt");
    let mut shell = std::process::Command::new("sh");
    shell
        .arg("-c")
        .arg(". \"$1\" && claude --resume abc -p 'hi there'")
        .arg("sh")
        .arg(temp.path().join("config/shell/claude.sh"))
        .env("PATH", path)
        .env("CLAUDE_ARGS_OUT", &out);
    for (key, value) in &guard.env {
        shell.env(key, value);
    }
    assert!(shell.status()?.success());

    let recorded = std::fs::read_to_string(&out)?;
    assert_eq!(
        recorded.lines().collect::<Vec<_>>(),
        ["--resume", "abc", "-p", "hi there"]
    );
    Ok(())
}
