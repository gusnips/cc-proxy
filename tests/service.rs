// Lifecycle tests for the background proxy service and for the routing it
// switches on and off in Claude Code's settings. Each test gets its own HOME,
// config dir, state dir and port, so none of them touches the real ones.
use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
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

    /// OpenCode Go signs in with a key in config.json, not an auth file.
    fn sign_in_with_key(&self) {
        std::fs::create_dir_all(self.config_dir()).unwrap();
        std::fs::write(
            self.config_dir().join("config.json"),
            r#"{"opencode":{"apiKey":"test-key"}}"#,
        )
        .unwrap();
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
fn restart_and_reload_keep_serving_and_name_the_provider() {
    let sandbox = Sandbox::new();
    sandbox.sign_in("codex");
    sandbox.cmd().args(sandbox.start_args()).assert().success();
    sandbox.wait_for_status(true);

    sandbox
        .cmd()
        .args(["restart", "--port", &sandbox.port.to_string()])
        .assert()
        .success()
        .stdout(contains("restarted"))
        .stdout(contains("Claude Code goes to Codex"));

    sandbox.wait_for_status(true);
    sandbox.assert_routed_to_proxy();

    sandbox
        .cmd()
        .arg("reload")
        .assert()
        .success()
        .stdout(contains("Claude Code goes to Codex"));
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
    assert_eq!(settings["modelPicker"]["replaceBuiltInOptions"], true);
    let options = settings["modelPicker"]["options"].as_array().unwrap();
    assert_eq!(options.len(), 4);
    assert!(
        options
            .iter()
            .all(|row| row["label"].as_str().unwrap().starts_with("codex/"))
    );
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
        .stdout(contains(
            "Claude Code uses its original settings and login, without the proxy",
        ));

    // Neutral values, not deleted keys: Claude Code never unsets a key it
    // stops seeing, so a deleted key would leave the window on the proxy.
    sandbox.assert_routed_to_anthropic();
    let settings = sandbox.settings();
    assert_eq!(settings["theme"], "dark");
    assert_eq!(settings["env"]["FOO"], "1");
    assert!(settings.get("modelPicker").is_none());
    assert_eq!(sandbox.route()["on"], false);
}

#[test]
fn use_switches_provider_and_direct_without_restarting_the_proxy() {
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
    assert_eq!(
        sandbox.route(),
        json!({"on": true, "provider": "kimi", "fallback": []})
    );
    sandbox.assert_routed_to_proxy();

    sandbox
        .cmd()
        .args(["use", "direct"])
        .assert()
        .success()
        .stdout(contains(
            "Claude Code uses its original settings and login, without the proxy",
        ));
    sandbox.assert_routed_to_anthropic();
    assert_eq!(
        sandbox.route(),
        json!({"on": false, "provider": "kimi", "fallback": []})
    );

    // The proxy ran through all of it, and picks the last provider up again.
    sandbox.cmd().args(["use", "codex"]).assert().success();
    sandbox.assert_routed_to_proxy();
    assert_eq!(sandbox.pid(), pid);
}

#[test]
fn direct_and_stop_restore_the_original_settings_and_login() {
    let sandbox = Sandbox::new();
    sandbox.sign_in("kimi");
    let original = json!({"theme": "dark", "env": {
        "ANTHROPIC_BASE_URL": "https://original.example.com",
        "CLAUDE_CODE_OAUTH_TOKEN": "original-login",
        "FOO": "keep"
    }});
    sandbox.write_settings(&original.to_string());

    for stop in [false, true] {
        sandbox.cmd().args(["use", "kimi"]).assert().success();
        sandbox.wait_for_status(true);
        let pid = sandbox.pid();
        sandbox.assert_routed_to_proxy();

        let mut command = sandbox.cmd();
        if stop {
            command.arg("stop");
        } else {
            command.args(["use", "direct"]);
        }
        command.assert().success().stdout(contains(
            "Claude Code uses its original settings and login, without the proxy",
        ));
        let restored = sandbox.settings();
        for key in ["ANTHROPIC_BASE_URL", "CLAUDE_CODE_OAUTH_TOKEN", "FOO"] {
            assert_eq!(restored["env"][key], original["env"][key]);
        }
        assert_eq!(restored["theme"], original["theme"]);
        assert_eq!(sandbox.route()["on"], false);
        assert_eq!(sandbox.route()["provider"], "kimi");
        sandbox.wait_for_status(!stop);
        if !stop {
            assert_eq!(sandbox.pid(), pid);
        }
    }
}

#[test]
fn direct_without_a_proxy_does_not_start_one_or_change_settings() {
    let sandbox = Sandbox::new();
    let original = "{\"theme\": \"dark\"}\n";
    sandbox.write_settings(original);
    sandbox.cmd().args(["use", " Direct "]).assert().success();
    assert_eq!(
        std::fs::read_to_string(sandbox.settings_path()).unwrap(),
        original
    );
    assert_eq!(sandbox.route(), Value::Null);
    sandbox.wait_for_status(false);
}

#[test]
fn direct_cannot_be_part_of_a_fallback_chain_and_changes_nothing() {
    let sandbox = Sandbox::new();
    let original = "{\"theme\": \"dark\"}\n";
    sandbox.write_settings(original);
    for targets in [["direct", "codex"], ["codex", " Direct "]] {
        sandbox
            .cmd()
            .arg("use")
            .args(targets)
            .assert()
            .failure()
            .stderr(contains("can't be part of a fallback chain"));
    }
    assert_eq!(
        std::fs::read_to_string(sandbox.settings_path()).unwrap(),
        original
    );
    assert_eq!(sandbox.route(), Value::Null);
    sandbox.wait_for_status(false);
}

#[test]
fn claude_and_direct_cannot_be_added_as_custom_providers() {
    let sandbox = Sandbox::new();
    for (name, reason) in [
        ("claude", "is a built-in provider name"),
        ("direct", "bypasses the proxy"),
    ] {
        sandbox
            .cmd()
            .args([
                "provider",
                "add",
                name,
                "http://127.0.0.1:9",
                "--model",
                "test-model",
            ])
            .assert()
            .failure()
            .stderr(contains(reason));
    }
    assert_eq!(sandbox.route(), Value::Null);
}

#[test]
fn picker_labels_follow_active_provider_maps_and_restore_the_original_picker() {
    let sandbox = Sandbox::new();
    sandbox.sign_in("grok");
    sandbox.sign_in("kimi");
    let original = json!({"theme": "dark", "modelPicker": {
        "replaceBuiltInOptions": false,
        "other": 7,
        "options": [{"model": "private-model", "label": "My model", "description": "Keep this row"}]
    }});
    sandbox.write_settings(&original.to_string());
    let opus_label = |settings: &Value| {
        settings["modelPicker"]["options"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["model"] == "opus")
            .unwrap()["label"]
            .clone()
    };

    sandbox.cmd().args(["use", "grok"]).assert().success();
    sandbox.wait_for_status(true);
    let pid = sandbox.pid();
    let settings = sandbox.settings();
    assert_eq!(opus_label(&settings), "grok/grok-4.7");
    assert_eq!(settings["modelPicker"]["replaceBuiltInOptions"], true);
    assert_eq!(
        settings["modelPicker"]["options"][0],
        original["modelPicker"]["options"][0]
    );
    assert_eq!(settings["modelPicker"]["other"], 7);

    sandbox
        .cmd()
        .args(["models", "set", "opus", "grok-4.5"])
        .assert()
        .success();
    assert_eq!(opus_label(&sandbox.settings()), "grok/grok-4.5");
    let active = std::fs::read(sandbox.settings_path()).unwrap();
    sandbox
        .cmd()
        .args(["models", "set", "opus", "kimi-k2.6", "--provider", "kimi"])
        .assert()
        .success();
    assert_eq!(std::fs::read(sandbox.settings_path()).unwrap(), active);

    sandbox
        .cmd()
        .args(["models", "reset", "opus"])
        .assert()
        .success();
    assert_eq!(opus_label(&sandbox.settings()), "grok/grok-4.7");
    sandbox.cmd().args(["use", "kimi"]).assert().success();
    assert_eq!(opus_label(&sandbox.settings()), "kimi/kimi-for-coding");
    assert_eq!(sandbox.pid(), pid);
    sandbox
        .cmd()
        .args(["models", "reset", "opus"])
        .assert()
        .success();
    assert_eq!(opus_label(&sandbox.settings()), "kimi/k3");

    sandbox.cmd().args(["use", "direct"]).assert().success();
    sandbox.assert_routed_to_anthropic();
    let restored = sandbox.settings();
    assert_eq!(restored["modelPicker"], original["modelPicker"]);
    assert_eq!(restored["theme"], original["theme"]);
    let off = std::fs::read(sandbox.settings_path()).unwrap();
    sandbox
        .cmd()
        .args(["models", "set", "opus", "grok-4.5", "--provider", "grok"])
        .assert()
        .success();
    assert_eq!(std::fs::read(sandbox.settings_path()).unwrap(), off);
}

#[test]
fn use_asks_a_reseller_for_each_claude_name_and_again_only_for_empty_ones() {
    let sandbox = Sandbox::new();
    sandbox.sign_in_with_key();

    // OpenCode Go resells other vendors' models, so cc-proxy picks none.
    // fable: a search, then a number. opus: a number from the first list.
    // sonnet: an exact name. haiku: Enter skips it.
    sandbox
        .cmd()
        .args(["use", "opencode"])
        .write_stdin("kimi\n1\n1\nopencode-go/glm-5.2\n\n")
        .assert()
        .success()
        .stdout(contains(
            "Which OpenCode Go model should `fable` use? Now: no model yet.",
        ))
        .stdout(contains("Claude Code goes to OpenCode Go"))
        .stdout(contains(
            "haiku has no model yet. Pick one with `cc-proxy models choose opencode`.",
        ));
    let kept = sandbox.route()["tiers"]["opencode"].clone();
    let model = |tier: &str| kept[tier].as_str().unwrap_or_default().to_string();
    assert!(model("fable").starts_with("opencode-go/kimi"), "{kept}");
    assert!(model("opus").starts_with("opencode-go/"), "{kept}");
    assert_eq!(model("sonnet"), "opencode-go/glm-5.2");
    assert!(kept.get("haiku").is_none(), "{kept}");

    // Only the name still without a model is asked again.
    sandbox
        .cmd()
        .args(["use", "opencode"])
        .write_stdin("\n")
        .assert()
        .success()
        .stdout(contains("should `haiku` use?"))
        .stdout(contains("should `fable` use?").not());
}

#[test]
fn use_keeps_nothing_when_nobody_answers_the_model_question() {
    let sandbox = Sandbox::new();
    sandbox.sign_in_with_key();

    sandbox
        .cmd()
        .args(["use", "opencode"])
        .write_stdin("")
        .assert()
        .success()
        .stdout(contains(
            "Choose the OpenCode Go model each Claude name uses",
        ))
        .stdout(contains("fable has no model yet"));
    assert!(
        sandbox.route().get("tiers").is_none(),
        "{}",
        sandbox.route()
    );
    sandbox.assert_routed_to_proxy();
}

#[test]
fn use_takes_an_api_key_for_a_provider_that_signs_in_with_one() {
    let sandbox = Sandbox::new();

    // Not signed in: it offers to, and the key line is the second answer.
    sandbox
        .cmd()
        .args(["use", "glm"])
        .write_stdin("y\ntest-key\n")
        .assert()
        .success()
        .stdout(contains("GLM isn't signed in. Sign in now?"))
        .stdout(contains("Claude Code goes to GLM"));
    sandbox.assert_routed_to_proxy();
    assert_eq!(sandbox.route()["provider"], "glm");
}

#[test]
fn models_show_set_and_reset_a_claude_name() {
    let sandbox = Sandbox::new();

    sandbox
        .cmd()
        .args(["models", "show", "grok"])
        .assert()
        .success()
        .stdout(contains("Claude names on Grok:"))
        .stdout(contains(
            "haiku   grok-composer-2.5-fast (picked by cc-proxy)",
        ));

    sandbox
        .cmd()
        .args(["models", "set", "Opus", "grok-4.5", "--provider", "grok"])
        .assert()
        .success()
        .stdout(contains("opus    grok-4.5 (your choice)"));
    assert_eq!(sandbox.route()["tiers"]["grok"]["opus"], "grok-4.5");

    sandbox
        .cmd()
        .args(["models", "reset", "opus", "--provider", "grok"])
        .assert()
        .success()
        .stdout(contains("opus    grok-4.7 (picked by cc-proxy)"));
    assert!(sandbox.route().is_null(), "{}", sandbox.route());
}

#[test]
fn models_set_names_what_is_wrong_with_what_you_typed() {
    let sandbox = Sandbox::new();

    sandbox
        .cmd()
        .args([
            "models",
            "set",
            "opus",
            "no-such-model",
            "--provider",
            "grok",
        ])
        .assert()
        .failure()
        .stderr(contains("doesn't serve a model called `no-such-model`"));
    sandbox
        .cmd()
        .args(["models", "set", "gemini", "grok-4.5", "--provider", "grok"])
        .assert()
        .failure()
        .stderr(contains("fable, opus, sonnet, haiku"));
    sandbox
        .cmd()
        .arg("models")
        .assert()
        .failure()
        .stderr(contains("No provider to use yet"));
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
        .stdout(contains("Couldn't restore Claude Code's original settings"));
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
