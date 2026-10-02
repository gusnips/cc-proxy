use assert_cmd::Command;
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
fn models_prints_all_providers() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("cc-proxy")?;
    cmd.arg("models");
    let out = String::from_utf8(cmd.output()?.stdout)?;
    assert!(out.contains("codex:"));
    assert!(out.contains("kimi:"));
    assert!(out.contains("opencode:"));
    assert!(out.contains("cursor:"));

    let mut cmd = Command::cargo_bin("cc-proxy")?;
    cmd.args(["models", "--full"]);
    cmd.output()?;
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
        "Sign in to a provider, pick your models and get ready to run claude",
        "Start Claude Code on the proxy, passing every argument to claude",
        "Add or remove the hook that sends plain `claude` through cc-proxy",
        "Send plain `claude` through cc-proxy, in every terminal",
        "Run plain `claude` without the proxy again, in every terminal",
        "Start the proxy as a background service",
        "Stop the background proxy service",
        "Show whether the background proxy service is running",
        "Restart the background proxy service",
        "Validate the config file and ask a running service to reload it",
        "Attach a read-only dashboard to a running proxy",
        "List supported provider models",
        "Manage Codex authentication",
        "Manage Kimi authentication",
        "Manage GitHub Copilot authentication",
        "Manage Cursor authentication",
        "Manage Grok authentication",
        "Manage GLM authentication",
        "Manage the OpenCode Go API key",
        "Show how much of your Codex, Kimi and OpenCode Go plans you have used",
    ] {
        assert!(stdout.contains(description), "missing: {description}");
    }
    assert!(!stdout.contains("demo"));
    assert!(!stdout.contains("mock data and no proxy server"));
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
    cmd.args(["cursor", "auth", "device"]);
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
    cmd.args(["kimi", "auth", "logout"]);
    cmd.env("CCP_CONFIG_DIR", temp.path());
    cmd.assert().success();
    Ok(())
}

#[test]
fn models_output_is_stable_order() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("cc-proxy")?;
    cmd.args(["models", "--full"]);
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
    cmd.args(["kimi", "auth", "status"]);
    cmd.env("CCP_CONFIG_DIR", temp.path());
    cmd.assert().success().stdout(contains("User: u"));
    Ok(())
}

fn isolated_opencode_env(cmd: &mut Command, temp: &TempDir) {
    cmd.env("CCP_CONFIG_DIR", temp.path().join("config"))
        .env("HOME", temp.path())
        .env("USERPROFILE", temp.path())
        .env_remove("CCP_OPENCODE_API_KEY")
        .env_remove("OPENCODE_API_KEY");
}

#[test]
fn opencode_auth_status_reports_config_source() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let mut cmd = Command::cargo_bin("cc-proxy")?;
    isolated_opencode_env(&mut cmd, &temp);
    cmd.args(["opencode", "auth", "status"])
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
    cmd.args(["opencode", "auth", "status"])
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
    cmd.args(["opencode", "auth", "login"])
        .write_stdin("login-key\n")
        .assert()
        .success()
        .stdout(contains("saved"));
    let raw = std::fs::read_to_string(temp.path().join("config/config.json"))?;
    assert!(raw.contains("\"apiKey\""));
    assert!(raw.contains("login-key"));
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
    run(&["config", "set", "aliasProvider", "Muse"]).failure();
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
        serde_json::json!({"access": access, "refresh": "test-refresh", "expires": expires, "scope": "openid", "userId": "user_test"})
    };
    std::fs::write(dir.join("auth.json"), serde_json::to_vec(&auth).unwrap()).unwrap();
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
        if !sent("authorization", "Bearer test-access") || !sent(required.0, required.1) {
            return (StatusCode::UNAUTHORIZED, String::new());
        }
        let path = format!(
            "{}/tests/fixtures/usage/{fixture}",
            env!("CARGO_MANIFEST_DIR")
        );
        (StatusCode::OK, std::fs::read_to_string(path).unwrap())
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
            "Codex (plus)\n",
            "  5-hour window: 45% used, already reset (Aug 9 17:00)\n",
            "  Weekly: 7% used, already reset (Aug 15 00:00)\n",
            "\n",
            "Kimi (allegretto)\n",
            "  5-hour window: 28% used, already reset (Aug 9 09:00)\n",
            "  Weekly: 12% used, already reset (Aug 14 16:00)\n",
            "\n",
            "opencode: no API key set. Run `cc-proxy opencode auth login` or set OPENCODE_API_KEY.\n",
        )
    );

    let output = usage_command(&temp, &base_url)
        .args(["usage", "--json"])
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stderr)?,
        "opencode: no API key set. Run `cc-proxy opencode auth login` or set OPENCODE_API_KEY.\n"
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(
        json,
        serde_json::json!({
            "codex": {
                "plan": "plus",
                "five_hour": {"used_percent": 45.0, "resets_at_ms": 1786294800000_i64},
                "weekly": {"used_percent": 7.0, "resets_at_ms": 1786752000000_i64}
            },
            "kimi": {
                "plan": "allegretto",
                "five_hour": {"used_percent": 28.0, "resets_at_ms": 1786266000000_i64},
                "weekly": {"used_percent": 12.0, "resets_at_ms": 1786723200000_i64}
            }
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
            "codex",
            "codex: not signed in. Run `cc-proxy codex auth login`.\n",
        ),
        (
            "kimi",
            "kimi: not signed in. Run `cc-proxy kimi auth login`.\n",
        ),
        (
            "opencode",
            "opencode: no API key set. Run `cc-proxy opencode auth login` or set OPENCODE_API_KEY.\n",
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
            "Codex didn't accept the sign-in when asked for usage. Run `cc-proxy codex auth login` to sign in again.\n",
        );
    Ok(())
}

/// A z.ai stand-in that answers every message with `status`.
fn serve_glm(status: u16) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move || async move {
            (axum::http::StatusCode::from_u16(status).unwrap(), "{}")
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

fn setup_command(temp: &TempDir, glm_base_url: &str) -> Command {
    let mut cmd = Command::cargo_bin("cc-proxy").unwrap();
    isolated_opencode_env(&mut cmd, temp);
    cmd.arg("setup")
        .env("SHELL", "/bin/zsh")
        .env_remove("CCP_GLM_API_KEY")
        .env_remove("GLM_API_KEY")
        .env("CCP_GLM_BASE_URL", glm_base_url)
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost");
    cmd
}

/// An API key, GLM, the key, then Enter twice for the models and "n" for
/// the shell hook.
const GLM_SETUP_INPUT: &str = "2\n1\nglm-test-key\n\n\nn\n";

#[test]
fn setup_tests_the_glm_key_then_saves_it_and_the_models() -> Result<(), Box<dyn std::error::Error>>
{
    let temp = TempDir::new()?;
    setup_command(&temp, &serve_glm(200))
        .write_stdin(GLM_SETUP_INPUT)
        .assert()
        .success()
        .stdout(contains("GLM key saved"))
        .stdout(contains(
            "Run `cc-proxy claude` to start Claude Code on glm-5.3.",
        ));
    let key = std::fs::read_to_string(temp.path().join("config/glm/auth.json"))?;
    assert!(key.contains("glm-test-key"));
    let config = std::fs::read_to_string(temp.path().join("config/config.json"))?;
    assert!(config.contains("\"model\": \"glm-5.3\""), "{config}");
    assert!(config.contains("\"fastModel\": \"glm-5.3\""), "{config}");
    Ok(())
}

#[test]
fn setup_refuses_to_save_a_key_the_provider_rejects() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    setup_command(&temp, &serve_glm(401))
        .write_stdin(GLM_SETUP_INPUT)
        .assert()
        .failure()
        .stderr(contains("GLM refused the key (HTTP 401)"))
        .stderr(contains("didn't save it"));
    assert!(!temp.path().join("config/glm/auth.json").exists());
    assert!(!temp.path().join("config/config.json").exists());
    Ok(())
}

#[test]
fn setup_saves_the_key_with_a_warning_when_the_provider_is_offline()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    // A port nothing listens on.
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        format!("http://{}", listener.local_addr()?)
    };
    setup_command(&temp, &closed)
        .write_stdin(GLM_SETUP_INPUT)
        .assert()
        .success()
        .stdout(contains("couldn't check the GLM key"))
        .stdout(contains("Run `cc-proxy claude`"));
    assert!(temp.path().join("config/glm/auth.json").exists());
    Ok(())
}
