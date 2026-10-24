mod common;

use std::{
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    process::{Command, Output},
    sync::Arc,
};

use axum::{body::Body, http::Request};
use cc_proxy::{Registry, claude_settings, config, providers, route, server, tiers};
use common::{EnvGuard, env_lock};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

fn cli(config: &std::path::Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cc-proxy"));
    command
        .env_clear()
        .env("HOME", config)
        .env("CCP_CONFIG_DIR", config)
        .env("CLAUDE_CONFIG_DIR", config.join("claude"))
        .env("XDG_STATE_HOME", config.join("state"))
        .env("CODEX_HOME", config)
        .args(args);
    for (name, value) in env {
        command.env(name, value);
    }
    command.output().unwrap()
}

fn mock(responses: Vec<(u16, Value)>) -> (String, std::thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for (status, body) in responses {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&stream);
            let mut request = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                request.push_str(&line);
            }
            requests.push(request);
            let body = body.to_string();
            write!(stream, "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
        requests
    });
    (url, handle)
}

fn cache(config: &std::path::Path, provider: &str, ids: &[&str]) {
    let path = config.join(provider).join("models.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_vec(ids).unwrap()).unwrap();
}

#[test]
fn gemini_refresh_reads_every_page_and_keeps_the_last_good_list_on_failure() {
    let config = tempfile::tempdir().unwrap();
    cache(config.path(), "gemini", &["gemini-retired-pro"]);
    let (url, pages) = mock(vec![
        (
            200,
            json!({"models": [
            {"name": "models/gemini-9-pro", "supportedGenerationMethods": ["generateContent"]},
            {"name": "models/gemini-9-tts", "supportedGenerationMethods": ["generateContent"]},
            {"name": "models/gemini-embedding", "supportedGenerationMethods": ["embedContent"]}
        ], "nextPageToken": "page two"}),
        ),
        (
            200,
            json!({"models": [
                {"name": "models/gemini-9-flash", "supportedGenerationMethods": ["generateContent"]}
            ]}),
        ),
    ]);
    let base = format!("{url}/v1beta");
    let env = [
        ("CCP_GEMINI_BASE_URL", base.as_str()),
        ("CCP_GEMINI_API_KEY", "test-key"),
    ];
    let output = cli(config.path(), &["models", "refresh", "gemini"], &env);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = pages.join().unwrap();
    assert!(requests[0].starts_with("GET /v1beta/models?"));
    assert!(requests[0].contains("x-goog-api-key: test-key"));
    assert!(requests[1].contains("pageToken=page+two"));
    let saved = config.path().join("gemini/models.json");
    let before = std::fs::read(&saved).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&before).unwrap(),
        json!(["gemini-9-flash", "gemini-9-pro"])
    );
    let listing = cli(config.path(), &["models", "list", "gemini"], &[]);
    let listing = String::from_utf8_lossy(&listing.stdout);
    assert!(listing.contains("gemini-9-pro"));
    assert!(!listing.contains("gemini-retired-pro"));
    assert!(!listing.contains("gemini-4-argon"));

    let (url, failed) = mock(vec![(503, json!({"error": "unavailable"}))]);
    let base = format!("{url}/v1beta");
    let output = cli(
        config.path(),
        &["models", "refresh", "gemini"],
        &[
            ("CCP_GEMINI_BASE_URL", &base),
            ("CCP_GEMINI_API_KEY", "test-key"),
        ],
    );
    assert!(!output.status.success());
    failed.join().unwrap();
    assert_eq!(std::fs::read(saved).unwrap(), before);
}

#[test]
fn all_refresh_reports_failures_and_still_updates_other_providers() {
    let config = tempfile::tempdir().unwrap();
    cache(config.path(), "opencode", &["kept-model"]);
    let (go, failed) = mock(vec![(503, json!({"error": "unavailable"}))]);
    let (zen, success) = mock(vec![(200, json!({"data": [{"id": "new-model-free"}]}))]);
    let output = cli(
        config.path(),
        &["models", "refresh", "all"],
        &[
            ("CCP_OPENCODE_BASE_URL", &format!("{go}/v1")),
            ("CCP_OPENCODE_ZEN_BASE_URL", &format!("{zen}/v1")),
        ],
    );
    failed.join().unwrap();
    success.join().unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("live model discovery is not supported yet")
    );
    assert_eq!(
        std::fs::read(config.path().join("opencode/models.json")).unwrap(),
        b"[\"kept-model\"]"
    );
    assert_eq!(
        std::fs::read(config.path().join("opencode-zen/models.json")).unwrap(),
        b"[\"new-model-free\"]"
    );
}

#[test]
fn running_registry_and_picker_follow_refreshed_catalogs_and_new_versions() {
    let _lock = env_lock();
    let dir = tempfile::tempdir().unwrap();
    let _env = [
        EnvGuard::set("CCP_CONFIG_DIR", dir.path()),
        EnvGuard::set("CLAUDE_CONFIG_DIR", dir.path()),
        EnvGuard::set("HOME", dir.path()),
        EnvGuard::set("CODEX_HOME", dir.path()),
        EnvGuard::set("XDG_STATE_HOME", dir.path()),
    ];
    std::fs::write(
        dir.path().join("config.json"),
        json!({
            "route": {"on": true, "provider": "antigravity"},
            "providers": {
                "openrouter": {
                    "baseUrl": "http://127.0.0.1:1/v1",
                    "format": "openai",
                    "models": ["old-custom-model"]
                }
            }
        })
        .to_string(),
    )
    .unwrap();
    cache(dir.path(), "antigravity", &["retired-model"]);
    let registry = Arc::new(Registry::new());
    let app = server::app(registry.clone());
    let mut ids = Vec::new();
    for base in [
        "claude-opus-9-1",
        "claude-sonnet-9-1",
        "gemini-9-argon",
        "gemini-9-flash",
    ] {
        for effort in ["low", "medium", "high"] {
            ids.push(format!("{base}-{effort}"));
        }
    }
    cache(
        dir.path(),
        "antigravity",
        &ids.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    let derived = tiers::for_provider(&registry, "antigravity");
    for (tier, model) in [
        ("fable", "antigravity/claude-opus-9-1"),
        ("opus", "antigravity/claude-sonnet-9-1"),
        ("sonnet", "antigravity/gemini-9-argon"),
        ("haiku", "antigravity/gemini-9-flash"),
    ] {
        assert_eq!(derived[tier].model, model);
        assert_eq!(tiers::ranked(&registry, "antigravity", tier)[0], model);
        assert_eq!(
            tiers::resolve(&registry, &config::route(), tier).unwrap(),
            model
        );
    }
    let mut chosen = config::route();
    chosen
        .tiers
        .entry("antigravity".into())
        .or_default()
        .insert("opus".into(), "antigravity/gemini-9-flash".into());
    assert_eq!(
        tiers::resolve(&registry, &chosen, "opus").unwrap(),
        "antigravity/gemini-9-flash"
    );
    assert_eq!(
        registry
            .provider_for_model("antigravity/some-future-model[1m]")
            .unwrap()
            .name(),
        "antigravity"
    );
    let (custom_url, custom_mock) =
        mock(vec![(200, json!({"data": [{"id": "new-custom-model"}]}))]);
    config::save_custom_provider(
        "openrouter",
        &config::CustomProviderConfig {
            base_url: format!("{custom_url}/v1"),
            format: config::CustomFormat::OpenAi,
            api_key: None,
            api_key_env: None,
            models: vec!["old-custom-model".into()],
        },
    )
    .unwrap();
    assert_eq!(
        providers::custom::refresh_models("openrouter").unwrap(),
        ["new-custom-model"]
    );
    custom_mock.join().unwrap();
    assert_eq!(
        registry.catalog_for("openrouter"),
        ["openrouter/new-custom-model"]
    );
    assert_eq!(
        registry
            .provider_for_model("new-custom-model")
            .unwrap()
            .name(),
        "openrouter"
    );
    claude_settings::claim("http://127.0.0.1:18765").unwrap();
    route::sync_model_labels(&registry, &config::route(), None).unwrap();
    let settings: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("settings.json")).unwrap()).unwrap();
    assert!(
        settings["modelPicker"]["options"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["model"] == "antigravity/claude-opus-9-1")
    );
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let response = runtime
        .block_on(
            app.oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .body(Body::empty())
                    .unwrap(),
            ),
        )
        .unwrap();
    let bytes = runtime
        .block_on(response.into_body().collect())
        .unwrap()
        .to_bytes();
    let models: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        models["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == "antigravity/claude-opus-9-1")
    );
    assert!(
        !models["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == "antigravity/retired-model")
    );

    cache(dir.path(), "opencode", &["new-chat-model"]);
    assert_eq!(
        registry
            .provider_for_model("new-chat-model")
            .unwrap()
            .name(),
        "opencode"
    );
    assert!(providers::opencode::model::resolve("new-chat-model").is_some());
}
