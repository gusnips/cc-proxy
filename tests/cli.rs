use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use std::env;
#[cfg(unix)]
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Child, ExitStatus, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

#[test]
fn version_aliases_print_expected_version() -> Result<(), Box<dyn std::error::Error>> {
    let expected = format!("cc-proxy {}", env!("CARGO_PKG_VERSION"));

    for arg in ["--version", "-v", "version"] {
        let mut cmd = Command::cargo_bin("cc-proxy")?;
        cmd.arg(arg)
            .assert()
            .success()
            .stdout(contains(expected.clone()));
    }
    Ok(())
}

#[test]
fn models_list_prints_all_providers() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("cc-proxy")?;
    cmd.args(["models", "list"]);
    let out = String::from_utf8(cmd.output()?.stdout)?;
    assert!(out.contains("codex:"));
    assert!(out.contains("kimi:"));
    assert!(out.contains("opencode:"));
    assert!(out.contains("cursor:"));

    let mut cmd = Command::cargo_bin("cc-proxy")?;
    cmd.args(["models", "list", "--full"]);
    cmd.assert().success();
    Ok(())
}

#[test]
fn help_describes_visible_commands_and_hides_demo() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("cc-proxy")?;
    cmd.arg("--help");
    let output = cmd.output()?;
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout)?;
    for description in [
        "Print version information",
        "Start the proxy as a background service and send Claude Code through it",
        "Put a provider first, set the order of several, or use `direct` to bypass the proxy",
        "See the order requests try providers and accounts, turn them on or off, and move them",
        "Switch routing off and stop the background proxy service",
        "Show whether the proxy is running and where Claude Code goes",
        "Restart the background proxy service",
        "Validate the config file and ask a running service to reload it",
        "Open accounts, settings and live activity. If the proxy is off, press s to start it",
        "List every provider and where Claude Code goes, or add and remove your own",
        "Sign in to providers, or see who is signed in (login, device, status, logout)",
        "See or choose the model each Claude name uses, and list provider models",
        "Manage the background proxy service (start, stop, status, restart, reload)",
        "Show how much of your Codex, Kimi and OpenCode Go plans you have used",
    ] {
        assert!(stdout.contains(description), "missing: {description}");
    }
    assert!(!stdout.contains("demo"));
    assert!(!stdout.contains("mock data and no proxy server"));
    // The old launcher, hook, and top-level provider commands are gone; the `claude` tombstone is hidden.
    for gone in [
        "shell",
        "setup",
        "Send plain `claude`",
        "hook",
        "Manage Codex authentication",
        "Manage Kimi authentication",
    ] {
        assert!(!stdout.contains(gone), "still listed: {gone}");
    }
    Ok(())
}

#[test]
fn invalid_command_exits_two() -> Result<(), Box<dyn std::error::Error>> {
    Command::cargo_bin("cc-proxy")?
        .arg("definitely-not-a-command")
        .assert()
        .failure()
        .code(2);
    Ok(())
}

#[test]
fn unsupported_provider_auth_command_exits_two() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("cc-proxy")?;
    cmd.args(["auth", "device", "cursor"]);
    let output = cmd.output()?;
    assert_eq!(output.status.code(), Some(2));
    let out = String::from_utf8(output.stderr)?;
    assert!(out.contains("not yet implemented") || out.contains("unsupported"));
    Ok(())
}

#[test]
fn provider_logout_without_auth_is_success() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let mut cmd = Command::cargo_bin("cc-proxy")?;
    cmd.args(["auth", "logout", "kimi"]);
    cmd.env("CCP_CONFIG_DIR", temp.path());
    cmd.assert().success();
    Ok(())
}

#[test]
fn models_output_is_stable_order() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("cc-proxy")?;
    cmd.args(["models", "list", "--full"]);
    let output = cmd.output()?;
    let out = String::from_utf8(output.stdout)?;
    let codex_pos = out.find("codex:").unwrap_or(0);
    let kimi_pos = out.find("kimi:").unwrap_or(0);
    let cursor_pos = out.find("cursor:").unwrap_or(0);
    assert!(codex_pos < kimi_pos);
    assert!(kimi_pos < cursor_pos);
    Ok(())
}

#[cfg(unix)]
struct ChildGuard(Child);

#[cfg(unix)]
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(unix)]
fn wait_for_service(
    child: &mut ChildGuard,
    port: u16,
) -> Result<TcpStream, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(stream) => return Ok(stream),
            Err(error) if Instant::now() < deadline => {
                if let Some(status) = child.0.try_wait()? {
                    let mut stderr = String::new();
                    if let Some(mut pipe) = child.0.stderr.take() {
                        pipe.read_to_string(&mut stderr)?;
                    }
                    return Err(format!("service exited with {status}: {stderr}").into());
                }
                let _ = error;
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(unix)]
fn send_signal(child: &ChildGuard, signal: &str) -> Result<(), Box<dyn std::error::Error>> {
    let status = std::process::Command::new("kill")
        .args([signal, &child.0.id().to_string()])
        .status()?;
    if !status.success() {
        return Err(format!("kill {signal} failed with {status}").into());
    }
    Ok(())
}

#[cfg(unix)]
fn wait_for_exit(
    child: &mut ChildGuard,
    timeout: Duration,
) -> Result<ExitStatus, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.0.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err("plain service did not exit after the second signal".into());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
fn plain_service_exits_on_second_signal(signal: &str) -> Result<(), Box<dyn std::error::Error>> {
    let upstream = TcpListener::bind("127.0.0.1:0")?;
    let upstream_url = format!("http://{}", upstream.local_addr()?);
    let (accepted_tx, accepted_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let fixture = thread::spawn(move || {
        let (stream, _) = upstream.accept().unwrap();
        accepted_tx.send(()).unwrap();
        let _ = release_rx.recv();
        drop(stream);
    });

    let config = TempDir::new()?;
    let auth_dir = config.path().join("kimi");
    std::fs::create_dir_all(&auth_dir)?;
    std::fs::write(
        auth_dir.join("auth.json"),
        r#"{"access":"test","refresh":"test","expires":4102444800000,"scope":"openid","userId":"test"}"#,
    )?;
    let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_cc-proxy"))
        .args(["start", "--no-monitor", "--port", &port.to_string()])
        .env("CCP_CONFIG_DIR", config.path())
        .env("CCP_KIMI_BASE_URL", upstream_url)
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut child = ChildGuard(child);
    let mut downstream = wait_for_service(&mut child, port)?;
    let body = br#"{"model":"kimi-for-coding","max_tokens":64,"messages":[{"role":"user","content":"hello"}]}"#;
    write!(
        downstream,
        "POST /v1/messages HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )?;
    downstream.write_all(body)?;
    accepted_rx.recv_timeout(Duration::from_secs(20))?;

    send_signal(&child, signal)?;
    thread::sleep(Duration::from_millis(200));
    assert!(child.0.try_wait()?.is_none());
    send_signal(&child, signal)?;
    let status = wait_for_exit(&mut child, Duration::from_secs(2));
    let _ = release_tx.send(());
    fixture.join().unwrap();

    assert_eq!(status?.code(), Some(130));
    Ok(())
}

#[cfg(unix)]
#[test]
fn plain_service_exits_on_second_ctrl_c() -> Result<(), Box<dyn std::error::Error>> {
    plain_service_exits_on_second_signal("-INT")
}

#[cfg(unix)]
#[test]
fn plain_service_exits_on_second_sigterm() -> Result<(), Box<dyn std::error::Error>> {
    plain_service_exits_on_second_signal("-TERM")
}

#[test]
fn kimi_auth_status_reads_stored_auth() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let auth_dir = temp.path().join("kimi");
    std::fs::create_dir_all(&auth_dir)?;
    std::fs::write(
        auth_dir.join("auth.json"),
        r#"{"access":"a","refresh":"r","expires":4102444800000,"scope":"openid","userId":"u"}"#,
    )?;
    let mut cmd = Command::cargo_bin("cc-proxy")?;
    cmd.args(["auth", "status", "kimi"]);
    cmd.env("CCP_CONFIG_DIR", temp.path());
    cmd.assert().success().stdout(contains("signed in as u"));

    let mut cmd = Command::cargo_bin("cc-proxy")?;
    cmd.arg("auth");
    cmd.env("CCP_CONFIG_DIR", temp.path());
    cmd.assert().success().stdout(contains("kimi: signed in"));
    Ok(())
}

fn isolated_opencode_env(cmd: &mut Command, temp: &TempDir) {
    cmd.env("CCP_CONFIG_DIR", temp.path().join("config"))
        .env("HOME", temp.path())
        .env("USERPROFILE", temp.path())
        .env_remove("CCP_OPENCODE_API_KEY")
        .env_remove("OPENCODE_API_KEY");
}

/// Every saved login file of a provider, joined. Empty when none is saved.
fn saved_logins(temp: &TempDir, provider: &str) -> String {
    let dir = temp.path().join("config").join(provider).join("accounts");
    let Ok(files) = std::fs::read_dir(dir) else {
        return String::new();
    };
    files
        .filter_map(|file| std::fs::read_to_string(file.ok()?.path()).ok())
        .collect()
}

#[test]
fn opencode_auth_status_reports_config_source() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let mut cmd = Command::cargo_bin("cc-proxy")?;
    isolated_opencode_env(&mut cmd, &temp);
    cmd.args(["auth", "status", "opencode"])
        .assert()
        .failure()
        .code(1)
        .stdout(contains("Not authenticated"));

    std::fs::create_dir_all(temp.path().join("config"))?;
    std::fs::write(
        temp.path().join("config/config.json"),
        r#"{"opencode":{"apiKey":"file-key"}}"#,
    )?;
    let mut cmd = Command::cargo_bin("cc-proxy")?;
    isolated_opencode_env(&mut cmd, &temp);
    cmd.args(["auth", "status", "opencode"])
        .assert()
        .success()
        .stdout(contains("config.json"));
    Ok(())
}

#[test]
fn opencode_auth_login_stores_key_from_stdin() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let mut cmd = Command::cargo_bin("cc-proxy")?;
    isolated_opencode_env(&mut cmd, &temp);
    cmd.args(["auth", "login", "opencode"])
        .write_stdin("login-key\n")
        .assert()
        .success()
        .stdout(contains("Signed in to OpenCode Go as `main`"));
    let raw = saved_logins(&temp, "opencode");
    assert!(raw.contains("\"apiKey\""));
    assert!(raw.contains("login-key"));
    Ok(())
}

#[test]
fn top_level_login_and_logout_use_the_auth_handlers() -> Result<(), Box<dyn std::error::Error>> {
    for login in [vec!["login", "opencode"], vec!["auth", "login", "opencode"]] {
        let temp = TempDir::new()?;
        let mut cmd = Command::cargo_bin("cc-proxy")?;
        isolated_opencode_env(&mut cmd, &temp);
        cmd.args(login).write_stdin("fake-key\n").assert().success();
        let mut cmd = Command::cargo_bin("cc-proxy")?;
        isolated_opencode_env(&mut cmd, &temp);
        cmd.args(["logout", "opencode"]).assert().success();
        assert!(!saved_logins(&temp, "opencode").contains("fake-key"));
    }
    Ok(())
}

#[test]
fn saved_accounts_can_be_switched_and_removed_without_signing_in()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let config = temp.path().join("config/codex");
    std::fs::create_dir_all(config.join("accounts"))?;
    let a = uuid::Uuid::new_v4().to_string();
    let b = uuid::Uuid::new_v4().to_string();
    let index = serde_json::json!({"current":"home", "accounts":[
        {"id":a,"name":"home","identity":"acct-home"},
        {"id":b,"name":"work","identity":"acct-work"}
    ]});
    std::fs::write(config.join("accounts.json"), index.to_string())?;
    for (id, identity) in [(&a, "acct-home"), (&b, "acct-work")] {
        std::fs::write(config.join("accounts").join(format!("{id}.json")), serde_json::json!({
            "access":"do-not-print-me","refresh":"do-not-print-me","expires":4102444800000_u64,"accountId":identity
        }).to_string())?;
    }
    let run = |args: &[&str]| {
        let mut cmd = Command::cargo_bin("cc-proxy").unwrap();
        isolated_opencode_env(&mut cmd, &temp);
        cmd.env("XDG_STATE_HOME", temp.path().join("state"));
        cmd.args(args).assert()
    };
    run(&["auth", "status", "codex"])
        .success()
        .stdout(contains("2 accounts"))
        .stdout(contains("do-not-print-me").not());
    run(&["login", "codex"])
        .failure()
        .stderr(contains("auth add codex"))
        .stderr(contains("--replace"));
    run(&["auth", "use", "codex", "work"])
        .success()
        .stdout(contains("now uses `work`"));
    run(&["logout", "codex", "work"])
        .success()
        .stdout(contains("now uses `home`"));
    assert!(!config.join("accounts").join(format!("{b}.json")).exists());
    assert!(config.join("accounts").join(format!("{a}.json")).exists());
    run(&["logout", "codex", "--all"]).success();
    run(&["auth", "status", "codex"]).failure().code(1);
    Ok(())
}

#[test]
fn pool_lists_turns_rows_on_and_off_and_moves_them() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let config = temp.path().join("config");
    let ids = [
        uuid::Uuid::new_v4().to_string(),
        uuid::Uuid::new_v4().to_string(),
    ];
    std::fs::create_dir_all(config.join("codex/accounts"))?;
    std::fs::write(
        config.join("codex/accounts.json"),
        serde_json::json!({"current":"home", "accounts":[
            {"id":ids[0],"name":"home","identity":"acct-home"},
            {"id":ids[1],"name":"work","identity":"acct-work"}
        ]})
        .to_string(),
    )?;
    for (id, identity) in ids.iter().zip(["acct-home", "acct-work"]) {
        std::fs::write(
            config.join("codex/accounts").join(format!("{id}.json")),
            serde_json::json!({
                "access":"x","refresh":"x","expires":4102444800000_u64,"accountId":identity
            })
            .to_string(),
        )?;
    }
    std::fs::write(
        config.join("config.json"),
        r#"{"port": 4321, "route": {"on": true, "provider": "codex"}}"#,
    )?;
    let run = |args: &[&str]| {
        let mut cmd = Command::cargo_bin("cc-proxy").unwrap();
        isolated_opencode_env(&mut cmd, &temp);
        cmd.env("XDG_STATE_HOME", temp.path().join("state"));
        cmd.args(args).assert()
    };
    let out = |assert: assert_cmd::assert::Assert| {
        String::from_utf8(assert.get_output().stdout.clone()).unwrap()
    };

    // The older settings read as rows, and listing writes nothing.
    let listed = out(run(&["pool"]).success());
    assert!(listed.find("home").unwrap() < listed.find("work").unwrap());
    assert!(!std::fs::read_to_string(config.join("config.json"))?.contains("pool"));

    run(&["pool", "off", "codex", "work"])
        .success()
        .stdout(contains("Codex `work` is off"));
    run(&["pool", "move", "codex", "work", "up"])
        .success()
        .stdout(contains("now number 1"));
    run(&["pool", "move", "codex", "up"])
        .failure()
        .stderr(contains("Say which one"));
    run(&["pool", "move", "codex", "work", "sideways"])
        .failure()
        .stderr(contains("`up` or `down`"));
    run(&["pool", "on", "kimi"])
        .failure()
        .stderr(contains("cc-proxy login kimi"));

    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(config.join("config.json"))?)?;
    assert_eq!(saved["port"], 4321);
    assert!(saved["route"].get("provider").is_none());
    let rows: Vec<(&str, bool)> = saved["route"]["pool"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["accountId"].as_str().unwrap(),
                row["enabled"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(rows, [(ids[1].as_str(), false), (ids[0].as_str(), true)]);
    Ok(())
}

#[test]
fn config_set_get_list_round_trip() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let run = |args: &[&str]| {
        let mut cmd = Command::cargo_bin("cc-proxy").unwrap();
        isolated_opencode_env(&mut cmd, &temp);
        cmd.args(args).assert()
    };

    run(&["config", "set", "port", "18080"]).success();
    run(&["config", "get", "port"])
        .success()
        .stdout(contains("18080"));
    run(&["config", "set", "codex.fullLane", "true"]).success();
    run(&["config", "list"])
        .success()
        .stdout(contains("port"))
        .stdout(contains("18080"))
        .stdout(contains("codex.fullLane"));

    // Secrets never print.
    run(&["config", "set", "opencode.apiKey", "shh"]).success();
    let mut cmd = Command::cargo_bin("cc-proxy")?;
    isolated_opencode_env(&mut cmd, &temp);
    let assert = cmd
        .args(["config", "get", "opencode.apiKey"])
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone())?;
    assert!(out.contains("set") && !out.contains("shh"));

    run(&["config", "set", "port", "abc"]).failure();
    run(&["config", "set", "aliasProvider", "codex"]).failure();
    run(&["config", "set", "claude.enabled", "true"]).failure();
    run(&["config", "set", "nope.x", "1"]).failure();
    run(&["config", "get", "log.verbose"])
        .success()
        .stdout(contains("false"));
    Ok(())
}

#[test]
fn config_edit_uses_editor() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let mut cmd = Command::cargo_bin("cc-proxy")?;
    isolated_opencode_env(&mut cmd, &temp);
    cmd.env("EDITOR", "true").env_remove("VISUAL");
    cmd.args(["config", "edit"]).assert().success();
    Ok(())
}

#[cfg(unix)]
#[test]
fn config_edit_runs_an_editor_that_has_arguments_and_a_path_with_spaces()
-> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new()?;
    let editors = temp.path().join("my editors");
    std::fs::create_dir_all(&editors)?;
    let seen = temp.path().join("seen.txt");
    let editor = editors.join("ed.sh");
    std::fs::write(
        &editor,
        format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n", seen.display()),
    )?;
    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755))?;

    let mut cmd = Command::cargo_bin("cc-proxy")?;
    isolated_opencode_env(&mut cmd, &temp);
    cmd.env("VISUAL", "").env(
        "EDITOR",
        format!("\"{}\" -w --name 'a b'", editor.display()),
    );
    cmd.args(["config", "edit"]).assert().success();

    let config = temp.path().join("config").join("config.json");
    assert_eq!(
        std::fs::read_to_string(seen)?.lines().collect::<Vec<_>>(),
        ["-w", "--name", "a b", config.to_str().unwrap()]
    );
    Ok(())
}

#[test]
fn bare_config_prints_the_settings_list_when_it_is_not_a_terminal()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let mut cmd = Command::cargo_bin("cc-proxy")?;
    isolated_opencode_env(&mut cmd, &temp);
    cmd.arg("config")
        .write_stdin("")
        .assert()
        .success()
        .stdout(contains("port = (default 18765)"))
        .stdout(contains("codex.fullLane"))
        .stdout(contains("\x1b").not());
    Ok(())
}

#[test]
fn update_check_with_pinned_version_is_offline() -> Result<(), Box<dyn std::error::Error>> {
    let current = format!("v{}", env!("CARGO_PKG_VERSION"));
    let mut cmd = Command::cargo_bin("cc-proxy")?;
    cmd.args(["update", "--check", "--version", &current])
        .assert()
        .success()
        .stdout(contains("is up to date"));

    let mut cmd = Command::cargo_bin("cc-proxy")?;
    cmd.args(["update", "--check", "--version", "v9.9.9"])
        .assert()
        .success()
        .stdout(contains("is out"));
    Ok(())
}

/// Writes a sign-in that never needs renewing, the way `auth login` would.
fn write_usage_auth(config_dir: &std::path::Path, provider: &str, access: &str) {
    let dir = config_dir.join(provider);
    std::fs::create_dir_all(&dir).unwrap();
    let expires = 4102444800000_i64;
    let auth = if provider == "codex" {
        serde_json::json!({"access": access, "refresh": "test-refresh", "expires": expires, "account_id": "acct_test"})
    } else {
        serde_json::json!({"access": access, "refresh": "test-refresh", "expires": expires, "scope": "openid", "userId": "user_test", "deviceId": "device_test"})
    };
    std::fs::write(dir.join("auth.json"), serde_json::to_vec(&auth).unwrap()).unwrap();
}

/// Writes independent saved sign-ins without changing the current account.
fn write_usage_accounts(
    config_dir: &std::path::Path,
    provider: &str,
    accounts: &[(&str, &str)],
    current: &str,
) {
    let dir = config_dir.join(provider);
    std::fs::create_dir_all(dir.join("accounts")).unwrap();
    let saved: Vec<_> = accounts
        .iter()
        .map(|(name, access)| {
            let id = uuid::Uuid::new_v4().to_string();
            let second = *access == "second-access";
            let identity = if second { "acct_second" } else { "acct_test" };
            let auth = if provider == "codex" {
                serde_json::json!({
                    "access": access, "refresh": "test-refresh", "expires": 4102444800000_i64,
                    "accountId": identity
                })
            } else {
                serde_json::json!({
                    "access": access, "refresh": "test-refresh", "expires": 4102444800000_i64,
                    "userId": identity, "deviceId": if second { "device_second" } else { "device_test" }
                })
            };
            std::fs::write(
                dir.join("accounts").join(format!("{id}.json")),
                auth.to_string(),
            )
            .unwrap();
            serde_json::json!({"id": id, "name": name, "identity": identity})
        })
        .collect();
    std::fs::write(
        dir.join("accounts.json"),
        serde_json::json!({"current": current, "accounts": saved}).to_string(),
    )
    .unwrap();
}

/// Serves the usage fixtures where ChatGPT and Kimi serve usage, but only to
/// a caller that sends the test sign-in and the provider's own headers.
fn serve_usage_fixtures() -> String {
    use axum::{
        http::{HeaderMap, StatusCode},
        routing::get,
    };

    fn reply(headers: &HeaderMap, fixture: &str, required: (&str, &str)) -> (StatusCode, String) {
        let sent = |name: &str, value: &str| headers.get(name).is_some_and(|sent| sent == value);
        let second = sent("authorization", "Bearer second-access");
        let required_value = if second && required.0 == "chatgpt-account-id" {
            "acct_second"
        } else {
            required.1
        };
        if (!sent("authorization", "Bearer test-access") && !second)
            || !sent(required.0, required_value)
            || (required.0 == "x-msh-platform"
                && !sent(
                    "x-msh-device-id",
                    if second {
                        "device_second"
                    } else {
                        "device_test"
                    },
                ))
        {
            return (StatusCode::UNAUTHORIZED, String::new());
        }
        let path = format!(
            "{}/tests/fixtures/usage/{fixture}",
            env!("CARGO_MANIFEST_DIR")
        );
        let mut body: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        if second {
            if fixture == "chatgpt.json" {
                body["rate_limit"]["primary_window"]["used_percent"] = 85.into();
                body["rate_limit"]["secondary_window"]["used_percent"] = 30.into();
            } else {
                body["limits"][0]["detail"]["used"] = "62".into();
                body["usage"]["used"] = "410".into();
            }
        }
        (StatusCode::OK, body.to_string())
    }

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new()
        .route(
            "/backend-api/wham/usage",
            get(|headers: HeaderMap| async move {
                reply(
                    &headers,
                    "chatgpt.json",
                    ("chatgpt-account-id", "acct_test"),
                )
            }),
        )
        .route(
            "/coding/v1/usages",
            get(|headers: HeaderMap| async move {
                reply(&headers, "kimi.json", ("x-msh-platform", "kimi_cli"))
            }),
        );
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                axum::serve(listener, app).await.unwrap();
            });
    });
    base_url
}

fn usage_command(temp: &TempDir, base_url: &str) -> Command {
    let mut cmd = Command::cargo_bin("cc-proxy").unwrap();
    isolated_opencode_env(&mut cmd, temp);
    cmd.env(
        "CCP_CODEX_BASE_URL",
        format!("{base_url}/backend-api/codex/responses"),
    )
    .env("CCP_KIMI_BASE_URL", format!("{base_url}/coding/v1"))
    .env("NO_PROXY", "127.0.0.1,localhost")
    .env("no_proxy", "127.0.0.1,localhost")
    // The fixtures' resets are in the past, so the text never changes.
    .env("TZ", "UTC");
    cmd
}

#[test]
fn usage_shows_every_signed_in_plan_and_how_to_sign_in_to_the_rest()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let config = temp.path().join("config");
    write_usage_auth(&config, "codex", "test-access");
    write_usage_auth(&config, "kimi", "test-access");
    let base_url = serve_usage_fixtures();

    let output = usage_command(&temp, &base_url).arg("usage").output()?;
    assert_eq!(String::from_utf8(output.stderr)?, "");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stdout)?,
        concat!(
            "claude: not signed in. Run `cc-proxy auth login claude`.\n",
            "\n",
            "Codex `main` [current] (plus)\n",
            "  5-hour window: 55% left, already reset (Aug 9 17:00)\n",
            "  Weekly: 93% left, already reset (Aug 15 00:00)\n",
            "\n",
            "Kimi `main` [current] (allegretto)\n",
            "  5-hour window: 72% left, already reset (Aug 9 09:00)\n",
            "  Weekly: 88% left, already reset (Aug 14 16:00)\n",
            "\n",
            "antigravity: not signed in. Run `cc-proxy auth login antigravity`.\n",
            "\n",
            "opencode: no API key set. Run `cc-proxy auth login opencode` or set OPENCODE_API_KEY.\n",
        )
    );

    let output = usage_command(&temp, &base_url)
        .args(["usage", "--json"])
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stderr)?,
        concat!(
            "claude: not signed in. Run `cc-proxy auth login claude`.\n",
            "antigravity: not signed in. Run `cc-proxy auth login antigravity`.\n",
            "opencode: no API key set. Run `cc-proxy auth login opencode` or set OPENCODE_API_KEY.\n",
        )
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(
        json,
        serde_json::json!({
            "codex": {"accounts": [{
                "name": "main", "current": true,
                "usage": {
                    "plan": "plus",
                    "five_hour": {"used_percent": 45.0, "resets_at_ms": 1786294800000_i64},
                    "weekly": {"used_percent": 7.0, "resets_at_ms": 1786752000000_i64}
                }
            }]},
            "kimi": {"accounts": [{
                "name": "main", "current": true,
                "usage": {
                    "plan": "allegretto",
                    "five_hour": {"used_percent": 28.0, "resets_at_ms": 1786266000000_i64},
                    "weekly": {"used_percent": 12.0, "resets_at_ms": 1786723200000_i64}
                }
            }]}
        })
    );
    Ok(())
}

#[test]
fn usage_for_a_provider_that_is_not_signed_in_names_the_login_and_exits_one()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    for (provider, line) in [
        (
            "claude",
            "claude: not signed in. Run `cc-proxy auth login claude`.\n",
        ),
        (
            "codex",
            "codex: not signed in. Run `cc-proxy auth login codex`.\n",
        ),
        (
            "kimi",
            "kimi: not signed in. Run `cc-proxy auth login kimi`.\n",
        ),
        (
            "antigravity",
            "antigravity: not signed in. Run `cc-proxy auth login antigravity`.\n",
        ),
        (
            "opencode",
            "opencode: no API key set. Run `cc-proxy auth login opencode` or set OPENCODE_API_KEY.\n",
        ),
    ] {
        let mut cmd = Command::cargo_bin("cc-proxy")?;
        isolated_opencode_env(&mut cmd, &temp);
        cmd.args(["usage", provider])
            .assert()
            .failure()
            .code(1)
            .stdout("")
            .stderr(line);
    }
    Ok(())
}

#[test]
fn usage_with_a_rejected_sign_in_says_how_to_fix_it_and_exits_two()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    write_usage_auth(&temp.path().join("config"), "codex", "revoked-access");
    let base_url = serve_usage_fixtures();

    usage_command(&temp, &base_url)
        .args(["usage", "codex"])
        .assert()
        .failure()
        .code(2)
        .stdout("")
        .stderr(
            "Codex account `main`: Codex didn't accept the sign-in when asked for usage. To replace this sign-in, run `cc-proxy auth login codex --replace --account main`.\n",
        );
    Ok(())
}

#[test]
fn usage_reads_each_saved_accounts_windows_without_switching_current()
-> Result<(), Box<dyn std::error::Error>> {
    let base_url = serve_usage_fixtures();
    for (provider, name, first_used, second_used, reset) in [
        ("codex", "Codex", 45.0, 85.0, 1786294800000_i64),
        ("kimi", "Kimi", 28.0, 62.0, 1786266000000_i64),
    ] {
        let temp = TempDir::new()?;
        let config = temp.path().join("config");
        write_usage_accounts(
            &config,
            provider,
            &[("home", "test-access"), ("work", "second-access")],
            "work",
        );
        let index = config.join(provider).join("accounts.json");
        let before = std::fs::read(&index)?;
        let output = usage_command(&temp, &base_url)
            .args(["usage", provider, "--json"])
            .output()?;
        assert!(output.status.success(), "{:?}", output);
        assert!(output.stderr.is_empty(), "{:?}", output.stderr);
        let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        let accounts = json[provider]["accounts"].as_array().unwrap();
        assert_eq!(accounts.len(), 2);
        for (account, saved_name, current, used) in [
            (&accounts[0], "home", false, first_used),
            (&accounts[1], "work", true, second_used),
        ] {
            assert_eq!(account["name"], saved_name);
            assert_eq!(account["current"], current);
            assert_eq!(account["usage"]["five_hour"]["used_percent"], used);
            assert_eq!(account["usage"]["five_hour"]["resets_at_ms"], reset);
            assert!(account["usage"]["weekly"]["resets_at_ms"].is_i64());
        }
        let text = usage_command(&temp, &base_url)
            .args(["usage", provider])
            .output()?;
        assert!(text.status.success(), "{:?}", text);
        let text = String::from_utf8(text.stdout)?;
        assert!(text.contains(&format!("{name} `home` (")), "{text}");
        assert!(
            text.contains(&format!("{name} `work` [current] (")),
            "{text}"
        );
        assert!(
            text.contains(&format!("{:.0}% left", 100.0 - first_used)),
            "{text}"
        );
        assert!(
            text.contains(&format!("{:.0}% left", 100.0 - second_used)),
            "{text}"
        );
        for hidden in [
            "test-access",
            "second-access",
            "acct_test",
            "acct_second",
            "device_test",
            "device_second",
        ] {
            assert!(!text.contains(hidden), "leaked {hidden}: {text}");
            assert!(!String::from_utf8_lossy(&output.stdout).contains(hidden));
        }
        assert_eq!(std::fs::read(index)?, before);
    }
    Ok(())
}

#[test]
fn usage_keeps_healthy_accounts_when_a_saved_sign_in_is_rejected_missing_or_unreadable()
-> Result<(), Box<dyn std::error::Error>> {
    let base_url = serve_usage_fixtures();
    for provider in ["codex", "kimi"] {
        for broken in ["rejected", "missing", "unreadable"] {
            let temp = TempDir::new()?;
            let config = temp.path().join("config");
            write_usage_accounts(
                &config,
                provider,
                &[("home", "revoked-access"), ("work", "second-access")],
                "home",
            );
            let index = config.join(provider).join("accounts.json");
            let before = std::fs::read(&index)?;
            let saved: serde_json::Value = serde_json::from_slice(&before)?;
            let id = saved["accounts"][0]["id"].as_str().unwrap();
            let login = config
                .join(provider)
                .join("accounts")
                .join(format!("{id}.json"));
            match broken {
                "missing" => std::fs::remove_file(login)?,
                "unreadable" => std::fs::write(login, "{not json")?,
                _ => {}
            }
            let replace = format!("cc-proxy auth login {provider} --replace --account home");
            let text = usage_command(&temp, &base_url)
                .args(["usage", provider])
                .output()?;
            assert_eq!(text.status.code(), Some(2), "{provider} {broken}");
            assert!(String::from_utf8(text.stdout)?.contains("`work`"));
            let error = String::from_utf8(text.stderr)?;
            assert!(error.contains("account `home`"), "{error}");
            assert!(error.contains(&replace), "{error}");
            let output = usage_command(&temp, &base_url)
                .args(["usage", provider, "--json"])
                .output()?;
            assert_eq!(output.status.code(), Some(2), "{provider} {broken}");
            let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;
            let accounts = json[provider]["accounts"].as_array().unwrap();
            assert_eq!(accounts.len(), 2);
            assert_eq!(accounts[0]["name"], "home");
            assert_eq!(accounts[0]["current"], true);
            assert!(accounts[0]["error"].as_str().unwrap().contains(&replace));
            assert!(accounts[0].get("usage").is_none());
            assert_eq!(accounts[1]["name"], "work");
            assert_eq!(accounts[1]["current"], false);
            assert!(accounts[1]["usage"]["five_hour"]["used_percent"].is_f64());
            assert_eq!(std::fs::read(index)?, before);
        }
    }
    Ok(())
}

/// Answers one `GET .../models` with an OpenAI-style list, like OpenRouter's.
#[cfg(unix)]
fn serve_model_list_once(ids: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let base_url = format!("http://{}/v1", listener.local_addr()?);
    let body = serde_json::json!({
        "data": ids.iter().map(|id| serde_json::json!({"id": id})).collect::<Vec<_>>()
    })
    .to_string();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut chunk = [0; 1024];
        while !request.windows(4).any(|end| end == b"\r\n\r\n") {
            let read = stream.read(&mut chunk).unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
        }
        assert!(String::from_utf8_lossy(&request).starts_with("GET /v1/models "));
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
             connection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    });
    Ok(base_url)
}

#[cfg(unix)]
#[test]
fn models_refresh_fetches_and_saves_a_custom_providers_list()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let run = |args: &[&str]| {
        let mut cmd = Command::cargo_bin("cc-proxy").unwrap();
        isolated_opencode_env(&mut cmd, &temp);
        cmd.env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .args(args);
        cmd
    };
    let base_url = serve_model_list_once(&["vendor/model-a", "vendor/model-b"])?;
    run(&[
        "provider",
        "add",
        "local",
        &base_url,
        "--model",
        "placeholder",
    ])
    .assert()
    .success();

    run(&["models", "refresh", "local"])
        .assert()
        .success()
        .stdout(contains("local lists 2 models now."));
    run(&["models", "list", "local"])
        .assert()
        .success()
        .stdout(contains("local/vendor/model-a"))
        .stdout(contains("placeholder").not());

    // A reseller's names start empty, and bare `models` needs a provider.
    run(&["models", "show", "local"])
        .assert()
        .success()
        .stdout(contains("opus    no model yet"));
    run(&["models"])
        .assert()
        .failure()
        .stderr(contains("No provider to use yet"));
    Ok(())
}

#[test]
fn custom_provider_add_auth_models_and_remove_round_trip() -> Result<(), Box<dyn std::error::Error>>
{
    let temp = TempDir::new()?;
    let run = |args: &[&str]| {
        let mut cmd = Command::cargo_bin("cc-proxy").unwrap();
        isolated_opencode_env(&mut cmd, &temp);
        cmd.env_remove("CCP_ANTHROPIC_API_KEY")
            .env_remove("ANTHROPIC_API_KEY");
        cmd.args(args);
        cmd
    };

    run(&[
        "provider",
        "add",
        "openrouter",
        "https://openrouter.ai/api/v1",
        "--format",
        "openai",
        "--model",
        "deepseek-r1",
    ])
    .assert()
    .success()
    .stdout(contains("Added provider `openrouter`"));

    run(&["auth", "login", "openrouter"])
        .write_stdin("or-secret-key\n")
        .assert()
        .success()
        .stdout(contains("Signed in to openrouter as `main`"));

    run(&["auth", "status", "openrouter"])
        .assert()
        .success()
        .stdout(contains("openrouter `main` (current)"));

    run(&["models", "list"])
        .assert()
        .success()
        .stdout(contains("anthropic:"))
        .stdout(contains("openrouter: deepseek-r1, openrouter/deepseek-r1"));

    run(&["provider", "list"])
        .assert()
        .success()
        .stdout(contains(
            "openrouter: custom (openai, https://openrouter.ai/api/v1)",
        ))
        .stdout(contains("`cc-proxy use <provider>` sends it through one"));

    run(&["provider", "remove", "openrouter"])
        .assert()
        .success()
        .stdout(contains("Removed provider `openrouter`"));

    run(&["auth", "login", "anthropic"])
        .write_stdin("sk-ant-test\n")
        .assert()
        .success()
        .stdout(contains("Signed in to Anthropic as `main`"));

    run(&["auth", "status", "anthropic"])
        .assert()
        .success()
        .stdout(contains("Anthropic `main` (current)"));

    Ok(())
}
