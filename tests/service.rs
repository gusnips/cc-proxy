// Lifecycle tests for the background proxy service and for the routing it
// switches on and off in Claude Code's settings. Each test gets its own HOME,
// config dir, state dir and port, so none of them touches the real ones.
use assert_cmd::Command;
use predicates::str::contains;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tempfile::TempDir;

mod common;

const NEUTRAL_URL: &str = "https://api.anthropic.com";

/// An isolated machine. Dropping it runs `cc-proxy stop`, so a failed test
/// leaves no daemon behind on a test port.
struct Sandbox {
    temp: TempDir,
    port: u16,
}

impl Sandbox {
    fn new() -> Self {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        Self {
            temp: TempDir::new().unwrap(),
            port,
        }
    }

    /// `cc-proxy` in this machine. The port is pinned so status, stop and
    /// reload never touch the real proxy port when no daemon is tracked.
    fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("cc-proxy").unwrap();
        cmd.env("CCP_CONFIG_DIR", self.config_dir())
            .env("XDG_STATE_HOME", self.temp.path().join("state"))
            .env("HOME", self.temp.path())
            .env("PORT", self.port.to_string())
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("ZDOTDIR")
            .env_remove("XDG_CONFIG_HOME");
        cmd
    }

    fn start_args(&self) -> [String; 3] {
        ["start".into(), "--port".into(), self.port.to_string()]
    }

    fn config_dir(&self) -> PathBuf {
        self.temp.path().join("config")
    }

    fn sign_in(&self, provider: &str) {
        common::write_auth(&self.config_dir(), provider);
    }

    fn settings_path(&self) -> PathBuf {
        self.temp.path().join(".claude/settings.json")
    }

    fn write_settings(&self, text: &str) {
        let path = self.settings_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn settings(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.settings_path()).unwrap()).unwrap()
    }

    /// The `route` block in config.json, or null before anything wrote it.
    fn route(&self) -> Value {
        std::fs::read_to_string(self.config_dir().join("config.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .map(|config| config["route"].clone())
            .unwrap_or(Value::Null)
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn pid(&self) -> u32 {
        let pidfile = self.temp.path().join("state/cc-proxy/proxy.pid");
        let info: Value = serde_json::from_str(&std::fs::read_to_string(pidfile).unwrap()).unwrap();
        info["pid"].as_u64().unwrap() as u32
    }

    fn wait_for_status(&self, running: bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let output = self.cmd().arg("status").output().unwrap();
            if output.status.success() == running {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "status never became running={running}: {}",
                String::from_utf8_lossy(&output.stdout)
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn assert_routed_to_proxy(&self) {
        let settings = self.settings();
        assert_eq!(settings["env"]["ANTHROPIC_BASE_URL"], self.url());
        assert_eq!(settings["env"]["CLAUDE_CODE_OAUTH_TOKEN"], "unused");
    }

    fn assert_routed_to_anthropic(&self) {
        let settings = self.settings();
        assert_eq!(settings["env"]["ANTHROPIC_BASE_URL"], NEUTRAL_URL);
        assert_eq!(settings["env"]["CLAUDE_CODE_OAUTH_TOKEN"], "");
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = self.cmd().arg("stop").output();
    }
}

#[test]
fn status_reports_not_running_without_daemon() {
    let sandbox = Sandbox::new();
    sandbox
        .cmd()
        .arg("status")
        .assert()
        .failure()
        .code(1)
        .stdout(contains("not running"));
}

#[test]
fn stop_without_daemon_is_success() {
    let sandbox = Sandbox::new();
    sandbox
        .cmd()
        .arg("stop")
        .assert()
        .success()
        .stdout(contains("not running"));
}

#[test]
fn reload_without_daemon_validates_config() {
    let sandbox = Sandbox::new();
    sandbox
        .cmd()
        .arg("reload")
        .assert()
        .failure()
        .stdout(contains("config file is valid"));
}

#[test]
fn reload_rejects_invalid_config() {
    let sandbox = Sandbox::new();
    std::fs::create_dir_all(sandbox.config_dir()).unwrap();
    std::fs::write(sandbox.config_dir().join("config.json"), "{not json").unwrap();
    sandbox
        .cmd()
        .arg("reload")
        .assert()
        .failure()
        .stderr(contains("invalid config"));
}

#[test]
fn start_status_and_stop_round_trip() {
    let sandbox = Sandbox::new();
    sandbox.sign_in("codex");

    sandbox
        .cmd()
        .args(sandbox.start_args())
        .assert()
        .success()
        .stdout(contains("started"));
    sandbox.wait_for_status(true);

    // A second start refuses to double-start.
    sandbox
        .cmd()
        .args(sandbox.start_args())
        .assert()
        .success()
        .stdout(contains("already running"));

    sandbox
        .cmd()
        .arg("stop")
        .assert()
        .success()
        .stdout(contains("stopped"));
    sandbox.wait_for_status(false);
}

#[test]
fn restart_keeps_serving() {
    let sandbox = Sandbox::new();
    sandbox.sign_in("codex");
    sandbox.cmd().args(sandbox.start_args()).assert().success();
    sandbox.wait_for_status(true);

    sandbox
        .cmd()
        .args(["restart", "--port", &sandbox.port.to_string()])
        .assert()
        .success()
        .stdout(contains("restarted"));

    sandbox.wait_for_status(true);
    sandbox.assert_routed_to_proxy();
}

#[test]
fn start_routes_claude_code_through_settings_and_stop_switches_it_off() {
    let sandbox = Sandbox::new();
    sandbox.sign_in("codex");
    sandbox.write_settings(r#"{ "theme": "dark", "env": { "FOO": "1" } }"#);

    sandbox
        .cmd()
        .args(sandbox.start_args())
        .assert()
        .success()
        .stdout(contains("Claude Code goes to Codex"));
    sandbox.wait_for_status(true);

    let settings = sandbox.settings();
    assert_eq!(settings["theme"], "dark");
    assert_eq!(settings["env"]["FOO"], "1");
    sandbox.assert_routed_to_proxy();
    assert_eq!(sandbox.route(), json!({"on": true, "provider": "codex"}));
    sandbox
        .cmd()
        .arg("status")
        .assert()
        .success()
        .stdout(contains("Claude Code goes to Codex"));

    sandbox
        .cmd()
        .arg("stop")
        .assert()
        .success()
        .stdout(contains("Claude Code uses your Claude subscription"));

    // Neutral values, not deleted keys: Claude Code never unsets a key it
    // stops seeing, so a deleted key would leave the window on the proxy.
    sandbox.assert_routed_to_anthropic();
    let settings = sandbox.settings();
    assert_eq!(settings["theme"], "dark");
    assert_eq!(settings["env"]["FOO"], "1");
    assert_eq!(sandbox.route()["on"], false);
}

#[test]
fn use_switches_provider_and_claude_without_restarting_the_proxy() {
    let sandbox = Sandbox::new();
    sandbox.sign_in("codex");
    sandbox.sign_in("kimi");

    // Two providers are signed in, so `start` asks. Enter takes the first.
    sandbox
        .cmd()
        .args(sandbox.start_args())
        .write_stdin("\n")
        .assert()
        .success()
        .stdout(contains("Claude Code goes to Codex"));
    sandbox.wait_for_status(true);
    let pid = sandbox.pid();

    sandbox
        .cmd()
        .args(["use", "kimi"])
        .assert()
        .success()
        .stdout(contains("Claude Code goes to Kimi"));
    assert_eq!(sandbox.route(), json!({"on": true, "provider": "kimi"}));
    sandbox.assert_routed_to_proxy();

    sandbox
        .cmd()
        .args(["use", "claude"])
        .assert()
        .success()
        .stdout(contains("Claude Code uses your Claude subscription"));
    sandbox.assert_routed_to_anthropic();
    assert_eq!(sandbox.route(), json!({"on": false, "provider": "kimi"}));

    // The proxy ran through all of it, and picks the last provider up again.
    sandbox.cmd().args(["use", "codex"]).assert().success();
    sandbox.assert_routed_to_proxy();
    assert_eq!(sandbox.pid(), pid);
}

#[test]
fn use_refuses_a_provider_it_cannot_route_to() {
    let sandbox = Sandbox::new();
    sandbox
        .cmd()
        .args(["use", "nobody"])
        .assert()
        .failure()
        .stderr(contains("can't send Claude Code to `nobody`"));
    assert!(!sandbox.settings_path().exists());
}

#[cfg(unix)]
#[test]
fn a_killed_proxy_is_healed_by_the_next_command() {
    let sandbox = Sandbox::new();
    sandbox.sign_in("codex");
    sandbox.cmd().args(sandbox.start_args()).assert().success();
    sandbox.wait_for_status(true);
    sandbox.assert_routed_to_proxy();

    let killed = std::process::Command::new("kill")
        .args(["-9", &sandbox.pid().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    while std::net::TcpStream::connect(("127.0.0.1", sandbox.port)).is_ok() {
        std::thread::sleep(Duration::from_millis(50));
    }

    sandbox
        .cmd()
        .arg("models")
        .assert()
        .success()
        .stderr(contains("cc-proxy wasn't running"));
    sandbox.assert_routed_to_anthropic();
    assert_eq!(sandbox.route()["on"], false);

    // Healed once. The next command has nothing to say.
    sandbox
        .cmd()
        .arg("models")
        .assert()
        .success()
        .stderr(predicates::str::is_empty());
}

#[cfg(unix)]
#[test]
fn start_no_monitor_only_serves_and_leaves_settings_alone() {
    let sandbox = Sandbox::new();
    let before = "{\n  \"env\": { \"FOO\": \"1\" },\n  \"theme\": \"dark\"\n}\n";
    sandbox.write_settings(before);

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_cc-proxy"))
        .args(["start", "--no-monitor", "--port", &sandbox.port.to_string()])
        .env("CCP_CONFIG_DIR", sandbox.config_dir())
        .env("XDG_STATE_HOME", sandbox.temp.path().join("state"))
        .env("HOME", sandbox.temp.path())
        .env_remove("CLAUDE_CONFIG_DIR")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while std::net::TcpStream::connect(("127.0.0.1", sandbox.port)).is_err() {
        assert!(Instant::now() < deadline, "the service never came up");
        std::thread::sleep(Duration::from_millis(20));
    }

    // A reboot starts this command, and a reboot must not route anything.
    let after = std::fs::read_to_string(sandbox.settings_path()).unwrap();
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(after, before);
    assert_eq!(sandbox.route(), Value::Null);
}

#[test]
fn a_settings_file_that_is_not_json_stops_start_before_anything_runs() {
    let sandbox = Sandbox::new();
    sandbox.sign_in("codex");
    sandbox.write_settings("{ not json");

    sandbox
        .cmd()
        .args(sandbox.start_args())
        .assert()
        .failure()
        .stderr(contains("isn't valid JSON"))
        .stderr(contains("changed nothing"));

    assert_eq!(
        std::fs::read_to_string(sandbox.settings_path()).unwrap(),
        "{ not json"
    );
    sandbox
        .cmd()
        .arg("status")
        .assert()
        .failure()
        .stdout(contains("not running"));
}

#[test]
fn stop_still_succeeds_when_the_settings_file_is_not_json() {
    let sandbox = Sandbox::new();
    sandbox.write_settings("{ not json");

    sandbox
        .cmd()
        .arg("stop")
        .assert()
        .success()
        .stdout(contains("Couldn't switch Claude Code back"));
    assert_eq!(
        std::fs::read_to_string(sandbox.settings_path()).unwrap(),
        "{ not json"
    );
}

#[cfg(unix)]
#[test]
fn the_old_claude_command_removes_the_hook_and_says_what_to_do() {
    let sandbox = Sandbox::new();
    let zshrc = sandbox.temp.path().join(".zshrc");
    let original = "export PATH=\"$HOME/bin:$PATH\"\n";
    let hook = "[ -f '/x/claude.sh' ] && . '/x/claude.sh'  # cc-proxy shell hook\n";
    std::fs::write(&zshrc, format!("{original}{hook}")).unwrap();

    sandbox
        .cmd()
        .args(["claude", "-p", "hi"])
        .assert()
        .failure()
        .stderr(contains("The shell hook is gone"))
        .stderr(contains("~/.zshrc"))
        .stderr(contains("run `claude` directly"));

    assert_eq!(std::fs::read_to_string(zshrc).unwrap(), original);
    assert!(!sandbox.settings_path().exists());
}
