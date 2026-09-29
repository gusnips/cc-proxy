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
