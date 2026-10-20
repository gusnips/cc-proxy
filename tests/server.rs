use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::response::IntoResponse;
use cc_proxy::{
    MessagesRequest,
    anthropic::MAX_ANTHROPIC_REQUEST_BYTES,
    monitor::{MonitorHandle, RequestStatus},
    provider::{CliHandlers, Generation, GenerationBody, Provider, ProviderError, RequestContext},
    registry::Registry,
    request_identity::ConversationIdentity,
    server::{
        AppFeatures, app, app_with_features, app_with_monitor, app_with_options,
        bind_proxy_listener,
    },
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tower::util::ServiceExt;

mod common;
use common::routed_to;

fn body_string(json: &str) -> Body {
    Body::from(json.to_string())
}

struct FakeCli;

impl CliHandlers for FakeCli {
    fn auth_state(&self) -> cc_proxy::provider::AuthState {
        cc_proxy::provider::AuthState::Missing
    }

    fn login(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn device(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn status(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn logout(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

static FAKE_CLI: FakeCli = FakeCli;

struct FakeProvider {
    name: &'static str,
    models: Vec<String>,
}

#[async_trait]
impl Provider for FakeProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn supported_models(&self) -> Vec<String> {
        self.models.clone()
    }

    fn cli(&self) -> &dyn CliHandlers {
        &FAKE_CLI
    }

    async fn handle_messages(
        &self,
        _body: MessagesRequest,
        _ctx: RequestContext,
    ) -> axum::response::Response {
        (StatusCode::NOT_IMPLEMENTED, "unused").into_response()
    }

    async fn handle_count_tokens(
        &self,
        _body: MessagesRequest,
        _ctx: RequestContext,
    ) -> axum::response::Response {
        (StatusCode::NOT_IMPLEMENTED, "unused").into_response()
    }

    async fn generate_anthropic_stream(
        &self,
        body: MessagesRequest,
        _ctx: RequestContext,
    ) -> Result<Generation, ProviderError> {
        let model = body.model.unwrap_or_default();
        let sse = format!(
            "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_fake\",\"model\":{model:?},\"usage\":{{\"input_tokens\":2}}}}}}\n\nevent: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\nevent: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{name}\"}}}}\n\nevent: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\nevent: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":1}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
            name = self.name,
        );
        Ok(Generation {
            body: GenerationBody::BufferedSse(sse.into()),
            resolved_model: model,
        })
    }
}

struct TranslatingProvider {
    name: &'static str,
    model: &'static str,
    captured: Arc<Mutex<Option<Value>>>,
}

#[async_trait]
impl Provider for TranslatingProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn supported_models(&self) -> Vec<String> {
        vec![self.model.to_string()]
    }

    fn cli(&self) -> &dyn CliHandlers {
        &FAKE_CLI
    }

    async fn handle_messages(
        &self,
        _body: MessagesRequest,
        _ctx: RequestContext,
    ) -> axum::response::Response {
        (StatusCode::NOT_IMPLEMENTED, "unused").into_response()
    }

    async fn handle_count_tokens(
        &self,
        _body: MessagesRequest,
        _ctx: RequestContext,
    ) -> axum::response::Response {
        (StatusCode::NOT_IMPLEMENTED, "unused").into_response()
    }

    async fn generate_anthropic_stream(
        &self,
        body: MessagesRequest,
        _ctx: RequestContext,
    ) -> Result<Generation, ProviderError> {
        let translated = match self.name {
            "kimi" => serde_json::to_value(
                cc_proxy::providers::kimi::translate::request::translate_request(
                    &body,
                    cc_proxy::providers::kimi::translate::request::TranslateOptions {
                        session_id: None,
                    },
                )
                .unwrap(),
            )
            .unwrap(),
            "grok" => serde_json::to_value(
                cc_proxy::providers::grok::translate::request::translate_request(
                    &body,
                    self.model.to_string(),
                )
                .unwrap(),
            )
            .unwrap(),
            _ => unreachable!(),
        };
        *self.captured.lock().unwrap() = Some(translated);
        let sse = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_fake\",\"model\":\"test\",\"usage\":{\"input_tokens\":1}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        );
        Ok(Generation {
            body: GenerationBody::BufferedSse(sse.into()),
            resolved_model: self.model.to_string(),
        })
    }
}

fn translating_registry(
    name: &'static str,
    model: &'static str,
    captured: Arc<Mutex<Option<Value>>>,
) -> Arc<Registry> {
    Arc::new(Registry::from_providers(vec![
        Arc::new(TranslatingProvider {
            name,
            model,
            captured,
        }) as Arc<dyn Provider>,
    ]))
}

type CapturedIdentity = (Option<ConversationIdentity>, Option<String>);

#[derive(Default)]
struct IdentityCaptureProvider {
    captured: Arc<Mutex<Vec<CapturedIdentity>>>,
    bodies: Mutex<Vec<MessagesRequest>>,
}

#[async_trait]
impl Provider for IdentityCaptureProvider {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn supported_models(&self) -> Vec<String> {
        vec!["gpt-5.5".to_string(), "gpt-6-luna".to_string()]
    }

    fn catalog(&self) -> Vec<String> {
        cc_proxy::providers::codex::translate::model_allowlist::tier_catalog()
    }

    fn cli(&self) -> &dyn CliHandlers {
        &FAKE_CLI
    }

    async fn handle_messages(
        &self,
        _body: MessagesRequest,
        _ctx: RequestContext,
    ) -> axum::response::Response {
        (StatusCode::INTERNAL_SERVER_ERROR, "legacy path").into_response()
    }

    async fn handle_messages_with_conversation_identity(
        &self,
        body: MessagesRequest,
        ctx: RequestContext,
        conversation_identity: Option<ConversationIdentity>,
    ) -> axum::response::Response {
        self.bodies.lock().unwrap().push(body);
        self.captured
            .lock()
            .unwrap()
            .push((conversation_identity, ctx.session_id));
        (StatusCode::OK, "captured").into_response()
    }

    async fn handle_count_tokens(
        &self,
        body: MessagesRequest,
        _ctx: RequestContext,
    ) -> axum::response::Response {
        self.bodies.lock().unwrap().push(body);
        (StatusCode::OK, "counted").into_response()
    }
}

fn routed_registry() -> Arc<Registry> {
    Arc::new(Registry::from_providers(vec![
        Arc::new(FakeProvider {
            name: "kimi",
            models: vec!["kimi-k2.6".to_string()],
        }) as Arc<dyn Provider>,
        Arc::new(FakeProvider {
            name: "grok",
            models: vec!["grok-4.5".to_string()],
        }),
        Arc::new(FakeProvider {
            name: "cursor",
            models: vec!["cursor".to_string()],
        }),
    ]))
}

async fn call_identity_ingress(
    app: &axum::Router,
    path: &str,
    headers: &[(&str, &str)],
    body: Value,
) -> StatusCode {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    app.clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn messages_ingress_forwards_only_strict_conversation_identity() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(IdentityCaptureProvider {
        captured: captured.clone(),
        ..Default::default()
    }) as Arc<dyn Provider>;
    let app = app(Arc::new(Registry::from_providers([provider])));
    let normal_body = || {
        json!({
            "model": "gpt-5.5",
            "max_tokens": 32,
            "messages": [{"role": "user", "content": "hello"}]
        })
    };

    let cases = [
        (
            vec![("x-claude-code-session-id", "session-main")],
            normal_body(),
        ),
        (
            vec![
                ("x-claude-code-session-id", "session-agent"),
                ("x-claude-code-agent-id", "agent-direct"),
            ],
            normal_body(),
        ),
        (
            vec![
                ("x-claude-code-session-id", "session-nested"),
                ("x-claude-code-agent-id", "agent-child"),
                ("x-claude-code-parent-agent-id", "agent-parent"),
            ],
            normal_body(),
        ),
        (
            vec![
                ("x-claude-code-session-id", "session-malformed-agent"),
                ("x-claude-code-agent-id", "malformed agent"),
            ],
            normal_body(),
        ),
        (vec![], normal_body()),
        (
            vec![("x-claude-code-session-id", " \tsession-trimmed\t ")],
            normal_body(),
        ),
        (
            vec![
                ("x-claude-code-session-id", "session-auto-review"),
                ("x-claude-code-agent-id", "agent-auto-review"),
            ],
            json!({
                "model": "gpt-5.5",
                "max_tokens": 32,
                "messages": [{"role": "user", "content": "review"}],
                "system": [{
                    "type": "text",
                    "text": "You are a security monitor for autonomous AI coding agents. Review this turn."
                }]
            }),
        ),
    ];

    for (headers, body) in cases {
        assert_eq!(
            call_identity_ingress(&app, "/v1/messages", &headers, body).await,
            StatusCode::OK
        );
    }
    assert_eq!(
        call_identity_ingress(
            &app,
            "/v1/messages/count_tokens",
            &[("x-claude-code-session-id", "session-count")],
            normal_body(),
        )
        .await,
        StatusCode::OK
    );

    assert_eq!(
        *captured.lock().unwrap(),
        vec![
            (
                Some(ConversationIdentity::Main("session-main".to_string())),
                Some("session-main".to_string()),
            ),
            (
                Some(ConversationIdentity::Agent(
                    "session-agent".to_string(),
                    "agent-direct".to_string(),
                )),
                Some("session-agent".to_string()),
            ),
            (
                Some(ConversationIdentity::Agent(
                    "session-nested".to_string(),
                    "agent-child".to_string(),
                )),
                Some("session-nested".to_string()),
            ),
            (None, Some("session-malformed-agent".to_string())),
            (None, None),
            (
                Some(ConversationIdentity::Main("session-trimmed".to_string())),
                Some(" \tsession-trimmed\t ".to_string()),
            ),
            (None, Some("session-auto-review".to_string())),
        ]
    );
}

#[tokio::test]
async fn native_model_labels_keep_exact_ids_and_leave_dispatch_and_background_checks_alone() {
    let _routed = routed_to("codex");
    let config_dir = std::path::PathBuf::from(std::env::var_os("CCP_CONFIG_DIR").unwrap());
    let isolated = tempfile::tempdir().unwrap();
    let _env = [
        common::EnvGuard::set("CLAUDE_CONFIG_DIR", isolated.path().join("claude")),
        common::EnvGuard::set("HOME", isolated.path()),
        common::EnvGuard::set("CODEX_HOME", isolated.path()),
        common::EnvGuard::set("XDG_STATE_HOME", isolated.path().join("state")),
        common::EnvGuard::unset("CCP_CODEX_MODEL"),
        common::EnvGuard::set("CCP_AUTO_REVIEW_MODEL", "gpt-6-luna"),
    ];
    std::fs::write(
        config_dir.join("config.json"),
        json!({"route": {"on": true, "provider": "codex", "tiers": {"codex": {
            "fable": "gpt-5.5", "opus": "gpt-5.5", "sonnet": "gpt-5.5", "haiku": "gpt-5.5"
        }}}})
        .to_string(),
    )
    .unwrap();
    cc_proxy::claude_settings::claim("http://127.0.0.1:19876").unwrap();
    let settings_path = cc_proxy::claude_settings::path();
    let provider = Arc::new(IdentityCaptureProvider::default());
    let app = app(Arc::new(Registry::from_providers([
        provider.clone() as Arc<dyn Provider>
    ])));
    for (requested, dispatched) in [
        ("claude-fable-native[1m]", "gpt-5.5"),
        ("claude-opus-native[1m]", "gpt-5.5"),
        ("claude-sonnet-native[1m]", "gpt-5.5"),
        ("claude-haiku-native[1m]", "gpt-5.5"),
        ("gpt-5.5-fast[1M]", "gpt-5.5-fast"),
    ] {
        let body = json!({
            "model": requested,
            "max_tokens": 8192,
            "thinking": {"type": "enabled", "budget_tokens": 4096},
            "output_config": {"effort": "high"},
            "messages": [{"role": "user", "content": "keep this request unchanged"}]
        });
        assert_eq!(
            call_identity_ingress(
                &app,
                "/v1/messages",
                &[("x-claude-code-session-id", "native-model-labels")],
                body.clone(),
            )
            .await,
            StatusCode::OK
        );
        let sent = provider.bodies.lock().unwrap().last().unwrap().clone();
        assert_eq!(sent.model.as_deref(), Some(dispatched));
        assert_eq!(sent.max_tokens, Some(8192));
        assert_eq!(sent.extra["thinking"], body["thinking"]);
        assert_eq!(sent.extra["output_config"], body["output_config"]);
        assert_eq!(
            serde_json::to_value(sent.messages).unwrap(),
            body["messages"]
        );
        let settings: Value =
            serde_json::from_slice(&std::fs::read(&settings_path).unwrap()).unwrap();
        assert_eq!(settings["modelPicker"]["replaceBuiltInOptions"], true);
        let row = settings["modelPicker"]["options"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["model"] == requested)
            .unwrap();
        assert_eq!(row["label"], "codex/gpt-5.5");
        let description = row["description"].as_str().unwrap();
        assert!(description.contains(requested));
        let normalized = cc_proxy::registry::normalize_incoming_model(requested);
        assert_eq!(
            description.starts_with("Answers "),
            cc_proxy::registry::anthropic_model_family(&normalized).is_some()
        );
    }

    let before_reset = std::fs::read(&settings_path).unwrap();
    let config_path = config_dir.join("config.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    config["route"]["tiers"]["codex"]
        .as_object_mut()
        .unwrap()
        .remove("opus");
    std::fs::write(config_path, config.to_string()).unwrap();
    std::fs::write(
        isolated.path().join("models_cache.json"),
        json!({"models": [{"slug": "gpt-99-sol", "supported_in_api": true, "visibility": "list"}]})
            .to_string(),
    )
    .unwrap();
    assert_eq!(std::fs::read(&settings_path).unwrap(), before_reset);
    let observed = "claude-opus-native[1m]";
    assert_eq!(
        call_identity_ingress(
            &app,
            "/v1/messages",
            &[("x-claude-code-session-id", "native-model-labels")],
            json!({"model": observed, "max_tokens": 8192,
                "messages": [{"role": "user", "content": "use the refreshed catalog"}]}),
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        provider
            .bodies
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .model
            .as_deref(),
        Some("gpt-99-sol")
    );
    let refreshed: Value = serde_json::from_slice(&std::fs::read(&settings_path).unwrap()).unwrap();
    let row = refreshed["modelPicker"]["options"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["model"] == observed)
        .unwrap();
    assert_eq!(row["model"], observed);
    assert_eq!(row["label"], "codex/gpt-99-sol");
    assert!(row["description"].as_str().unwrap().contains(observed));

    let before = std::fs::read(&settings_path).unwrap();
    for (path, requested, system, headers, dispatched) in [
        (
            "/v1/messages/count_tokens",
            "claude-opus-count-only[1m]",
            json!([]),
            vec![("x-claude-code-session-id", "native-count-only")],
            "gpt-99-sol",
        ),
        (
            "/v1/messages",
            "claude-opus-auto-review[1m]",
            json!([{"type": "text", "text": "You are a security monitor for autonomous AI coding agents. Review this turn."}]),
            vec![("x-claude-code-session-id", "native-auto-review")],
            "gpt-6-luna",
        ),
        (
            "/v1/messages",
            "claude-opus-no-session[1m]",
            json!([]),
            vec![],
            "gpt-99-sol",
        ),
    ] {
        assert_eq!(
            call_identity_ingress(
                &app,
                path,
                &headers,
                json!({
                    "model": requested, "max_tokens": 32, "system": system,
                    "messages": [{"role": "user", "content": "check"}]
                })
            )
            .await,
            StatusCode::OK
        );
        assert_eq!(
            provider
                .bodies
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .model
                .as_deref(),
            Some(dispatched)
        );
        assert_eq!(
            std::fs::read(&settings_path).unwrap(),
            before,
            "{requested}"
        );
    }
}

#[tokio::test]
async fn model_label_settings_errors_do_not_fail_a_valid_request() {
    let _routed = routed_to("codex");
    let config_dir = std::path::PathBuf::from(std::env::var_os("CCP_CONFIG_DIR").unwrap());
    let isolated = tempfile::tempdir().unwrap();
    let _env = [
        common::EnvGuard::set("CLAUDE_CONFIG_DIR", isolated.path().join("claude")),
        common::EnvGuard::set("HOME", isolated.path()),
        common::EnvGuard::set("XDG_STATE_HOME", isolated.path().join("state")),
        common::EnvGuard::unset("CCP_CODEX_MODEL"),
    ];
    std::fs::write(
        config_dir.join("config.json"),
        json!({"route": {"on": true, "provider": "codex", "tiers": {
            "codex": {"opus": "gpt-5.5"}
        }}})
        .to_string(),
    )
    .unwrap();
    cc_proxy::claude_settings::claim("http://127.0.0.1:19876").unwrap();
    let settings_path = cc_proxy::claude_settings::path();
    let broken = "{ not json";
    std::fs::write(&settings_path, broken).unwrap();
    let provider = Arc::new(IdentityCaptureProvider::default());
    let app = app(Arc::new(Registry::from_providers([
        provider.clone() as Arc<dyn Provider>
    ])));
    assert_eq!(
        call_identity_ingress(
            &app,
            "/v1/messages",
            &[("x-claude-code-session-id", "broken-model-label-settings")],
            json!({"model": "claude-opus-native[1m]", "max_tokens": 32,
                "messages": [{"role": "user", "content": "hello"}]}),
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        provider.bodies.lock().unwrap()[0].model.as_deref(),
        Some("gpt-5.5")
    );
    assert_eq!(std::fs::read_to_string(settings_path).unwrap(), broken);
}

#[tokio::test]
async fn messages_ingress_hands_the_anthropic_beta_header_to_the_provider() {
    let provider = Arc::new(IdentityCaptureProvider::default());
    let app = app(Arc::new(Registry::from_providers([
        provider.clone() as Arc<dyn Provider>
    ])));
    let body = json!({"model": "gpt-5.5", "max_tokens": 32,
        "messages": [{"role": "user", "content": "hello"}]});
    for headers in [
        &[("anthropic-beta", "context-management-2025-06-27")][..],
        &[],
    ] {
        assert_eq!(
            call_identity_ingress(&app, "/v1/messages", headers, body.clone()).await,
            StatusCode::OK
        );
    }

    let bodies = provider.bodies.lock().unwrap();
    assert_eq!(
        bodies[0].anthropic_beta.as_deref(),
        Some("context-management-2025-06-27")
    );
    assert!(bodies[1].anthropic_beta.is_none());
}

#[tokio::test]
async fn thread_requests_are_rejected_before_any_provider_call() {
    let provider = Arc::new(IdentityCaptureProvider::default());
    let app = app(Arc::new(Registry::from_providers([
        provider.clone() as Arc<dyn Provider>
    ])));
    for path in ["/v1/messages", "/v1/messages/count_tokens"] {
        for stream in [false, true] {
            for thread in [
                json!({"type": "create"}),
                json!({"type": "continue", "previous_message_id": "msg_elsewhere"}),
                json!({"type": "future-thread-mode"}),
            ] {
                let body = json!({
                    "model": "gpt-5.5",
                    "max_tokens": 32,
                    "stream": stream,
                    "thread": thread,
                    "messages": [{"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "call_before_switch", "content": "done"}
                    ]}]
                });
                let response = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method(Method::POST)
                            .uri(path)
                            .header("content-type", "application/json")
                            .body(Body::from(body.to_string()))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}, {body}");
                assert!(!request_id(&response).is_empty());
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let error: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(error["type"], "error");
                assert_eq!(error["error"]["type"], "invalid_request_error");
                assert_eq!(
                    error["error"]["details"]["error_code"],
                    "thread_unsupported_request"
                );
                assert!(provider.bodies.lock().unwrap().is_empty());
            }
        }
    }
}

#[tokio::test]
async fn thread_stateless_retries_keep_history_system_and_tools() {
    let provider = Arc::new(IdentityCaptureProvider::default());
    let app = app(Arc::new(Registry::from_providers([
        provider.clone() as Arc<dyn Provider>
    ])));
    let full = json!({
        "model": "gpt-5.5",
        "max_tokens": 32,
        "system": [{"type": "text", "text": "Preserve the original task."}],
        "tools": [{"name": "Read", "input_schema": {"type": "object"}}],
        "diagnostics": {"previous_message_id": "msg_diagnostic_only"},
        "messages": [
            {"role": "user", "content": "Original task: remember marker THREAD_REPLAY_CHECK."},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "call_before_switch", "name": "Read", "input": {"file_path": "marker.txt"}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_before_switch", "content": "THREAD_REPLAY_CHECK"}]},
            {"role": "assistant", "content": "I read the marker."},
            {"role": "user", "content": "Continue the original task."}
        ]
    });
    for agent in [None, Some("agent-child")] {
        for stream in [false, true] {
            let mut body = full.clone();
            body["stream"] = json!(stream);
            if agent.is_some() {
                body["thread"] = Value::Null;
            }
            let mut request = Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("x-claude-code-session-id", "session-replay")
                .header("anthropic-beta", "message-threads-2026-08-12");
            if let Some(agent) = agent {
                request = request.header("x-claude-code-agent-id", agent);
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::from(body.to_string())).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let sent = provider.bodies.lock().unwrap().last().unwrap().clone();
            assert_eq!(
                serde_json::to_value(sent.messages).unwrap(),
                full["messages"]
            );
            assert_eq!(sent.extra["system"], full["system"]);
            assert_eq!(sent.extra["tools"], full["tools"]);
            assert_eq!(sent.extra["diagnostics"], full["diagnostics"]);
            assert_eq!(sent.stream, stream);
        }
    }
    assert_eq!(provider.bodies.lock().unwrap().len(), 4);
    let captured = provider.captured.lock().unwrap();
    assert_eq!(
        captured[0].0,
        Some(ConversationIdentity::Main("session-replay".into()))
    );
    assert_eq!(
        captured[2].0,
        Some(ConversationIdentity::Agent(
            "session-replay".into(),
            "agent-child".into()
        ))
    );
}

#[tokio::test]
async fn bind_error_names_address_and_port() {
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = occupied.local_addr().unwrap().port();

    let err = bind_proxy_listener("127.0.0.1", port)
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains(&format!("127.0.0.1:{port}")));
    assert!(err.contains("failed to bind proxy listener"));
}

#[tokio::test]
async fn configurable_bind_address_accepts_all_interfaces() {
    let listener = bind_proxy_listener("0.0.0.0", 0).await.unwrap();
    assert_eq!(listener.local_addr().unwrap().ip().to_string(), "0.0.0.0");
}

#[tokio::test]
async fn invalid_bind_address_is_actionable() {
    let err = bind_proxy_listener("not-an-ip", 18765)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("invalid proxy bind address"));
    assert!(err.contains("not-an-ip"));
}

#[tokio::test]
async fn healthz_returns_ok() {
    let app = app(Arc::new(Registry::new()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap();
    assert_eq!(
        body,
        json!({"ok": true, "version": env!("CARGO_PKG_VERSION")})
    );
}

#[tokio::test]
async fn invalid_json_request_is_json_error() {
    let app = app(Arc::new(Registry::new()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .body(body_string("{"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let value: Value = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap();
    let error_type = value["error"]["type"].as_str().unwrap_or("");
    assert_eq!(error_type, "invalid_request_error");
}

#[tokio::test]
async fn empty_body_is_invalid_json() {
    let app = app(Arc::new(Registry::new()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn unknown_model_returns_400_with_summary() {
    let app = app(Arc::new(Registry::new()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"messages":[{"role":"user","content":"hello"}],"model":"not-a-model"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap();
    let message = body["error"]["message"].as_str().unwrap_or("");
    assert!(message.contains("Unknown model \"not-a-model\""));
    assert!(message.contains("Supported:"));
}

fn request_id(response: &axum::response::Response) -> &str {
    response
        .headers()
        .get("request-id")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

async fn messages_response(app: axum::Router, model: &str) -> axum::response::Response {
    app.oneshot(
        Request::builder()
            .method(Method::POST)
            .uri("/v1/messages")
            .header("content-type", "application/json")
            .body(body_string(
                &json!({"model": model, "messages": [{"role": "user", "content": "hello"}]})
                    .to_string(),
            ))
            .unwrap(),
    )
    .await
    .unwrap()
}

// Claude Code populates the `requestId` field of every transcript record from
// the `request-id` response header. Consumers that de-duplicate those records
// by request id count each request twice without it, because a transcript
// legitimately repeats a record and the id is what resolves the repeat.
#[tokio::test]
async fn successful_response_carries_a_request_id() {
    let registry = || {
        Arc::new(Registry::from_providers([
            Arc::new(IdentityCaptureProvider::default()) as Arc<dyn Provider>,
        ]))
    };
    let first = messages_response(app(registry()), "gpt-5.5").await;
    let second = messages_response(app(registry()), "gpt-5.5").await;

    assert_eq!(first.status(), StatusCode::OK);
    assert!(!request_id(&first).is_empty());
    assert_ne!(request_id(&first), request_id(&second));
}

// Errors are de-duplicated by the same key as successes, and a failed turn is
// the record most worth correlating with a proxy log.
#[tokio::test]
async fn rejected_request_carries_a_request_id() {
    let response = messages_response(app(Arc::new(Registry::new())), "not-a-model").await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(!request_id(&response).is_empty());
}

// A provider that answers with a failure status leaves through a different
// return path than a success.
#[tokio::test]
async fn provider_failure_response_carries_a_request_id() {
    let response = messages_response(app(routed_registry()), "kimi-k2.6").await;

    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    assert!(!request_id(&response).is_empty());
}

#[tokio::test]
async fn missing_model_returns_400() {
    let app = app(Arc::new(Registry::new()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages/count_tokens")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap();
    let error_type = body["error"]["type"].as_str().unwrap_or("");
    assert_eq!(error_type, "invalid_request_error");
}

async fn error_body(response: axum::response::Response) -> Value {
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap()
}

// Builds a valid JSON /v1/messages body of exactly `total_len` bytes that has
// no "model", so the handler parses it and then fails on the missing model.
// That failure proves the body cleared the size gate without touching a
// provider.
fn padded_messages_body_without_model(total_len: usize) -> String {
    let prefix = r#"{"messages":[{"role":"user","content":"hello"}],"padding":""#;
    let suffix = r#""}"#;
    let padding = total_len - prefix.len() - suffix.len();
    let mut body = String::with_capacity(total_len);
    body.push_str(prefix);
    body.extend(std::iter::repeat_n('a', padding));
    body.push_str(suffix);
    assert_eq!(body.len(), total_len);
    body
}

#[tokio::test]
async fn messages_body_over_16mib_clears_size_gate() {
    const OLD_LIMIT: usize = 16 * 1024 * 1024;
    const { assert!(MAX_ANTHROPIC_REQUEST_BYTES > OLD_LIMIT) };
    let app = app(Arc::new(Registry::new()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(padded_messages_body_without_model(
                    OLD_LIMIT + 1,
                )))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = error_body(response).await;
    let message = body["error"]["message"].as_str().unwrap_or("");
    assert!(
        message.starts_with("Missing \"model\""),
        "body over 16 MiB should reach model validation, got: {message}"
    );
}

#[tokio::test]
async fn messages_body_at_limit_clears_size_gate() {
    let app = app(Arc::new(Registry::new()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(padded_messages_body_without_model(
                    MAX_ANTHROPIC_REQUEST_BYTES,
                )))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = error_body(response).await;
    let message = body["error"]["message"].as_str().unwrap_or("");
    assert!(
        message.starts_with("Missing \"model\""),
        "body at the limit should reach model validation, got: {message}"
    );
}

#[tokio::test]
async fn anthropic_bodies_over_limit_return_request_too_large() {
    for path in ["/v1/messages", "/v1/messages/count_tokens"] {
        let response = app(Arc::new(Registry::new()))
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(Body::from(padded_messages_body_without_model(
                        MAX_ANTHROPIC_REQUEST_BYTES + 1,
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body = error_body(response).await;
        assert_eq!(body["error"]["type"], "request_too_large");
    }
}

#[tokio::test]
async fn known_model_reaches_codex_provider() {
    let app = app(Arc::new(Registry::new()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"model":"gpt-5.4","messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    // Codex provider is now concrete, so it should attempt auth before returning 501
    let status = response.status();
    assert!(
        status != StatusCode::NOT_IMPLEMENTED,
        "codex should no longer be a placeholder provider"
    );
}

#[tokio::test]
async fn count_tokens_routes_to_provider() {
    let app = app(Arc::new(Registry::new()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages/count_tokens")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"model":"gpt-5.4","messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    // Codex provider is now concrete, so count_tokens should succeed
    let status = response.status();
    assert!(
        status != StatusCode::NOT_IMPLEMENTED,
        "count_tokens should no longer return 501 for codex models"
    );
}

#[tokio::test]
async fn context_window_hint_is_removed_before_provider_dispatch() {
    let app = app(Arc::new(Registry::new()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages/count_tokens")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"model":"gpt-5.6-luna[1m]","messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn opus_5_alias_routes_to_provider() {
    let _routed = routed_to("codex");
    let app = app(Arc::new(Registry::new()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages/count_tokens")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"model":"claude-opus-5","messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn image_routes_reject_variations_wrong_media_and_oversized_generation() {
    let features = AppFeatures {
        responses_api: false,
        images_api: true,
        transcriptions_api: false,
    };
    let variation = app_with_features(Arc::new(Registry::new()), None, features)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/images/variations")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(variation.status(), StatusCode::NOT_FOUND);

    let wrong_media = app_with_features(Arc::new(Registry::new()), None, features)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/images/generations")
                .header("content-type", "multipart/form-data; boundary=x")
                .body(Body::from("--x--\r\n"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(wrong_media.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

    let oversized = app_with_features(Arc::new(Registry::new()), None, features)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/images/generations")
                .header("content-type", "application/json")
                .body(Body::from(vec![b'x'; 256 * 1024 + 1]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn monitor_tracks_image_endpoint_without_session_affinity() {
    let monitor = MonitorHandle::new(10);
    let app = app_with_features(
        Arc::new(Registry::new()),
        Some(monitor.clone()),
        AppFeatures {
            responses_api: false,
            images_api: true,
            transcriptions_api: false,
        },
    );
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/images/generations")
                .header("content-type", "application/json")
                .body(body_string("{"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let _ = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();

    let state = monitor.snapshot();
    assert_eq!(state.recent.len(), 1);
    assert_eq!(state.recent[0].endpoint.label(), "images");
    assert_eq!(state.recent[0].status, RequestStatus::Failed);
    assert!(state.recent[0].session_seq.is_none());
    assert!(state.recent[0].traffic_capture_path.is_none());
}

#[tokio::test]
async fn image_edit_accepts_multipart_and_validates_fields() {
    let app = app_with_features(
        Arc::new(Registry::new()),
        None,
        AppFeatures {
            responses_api: false,
            images_api: true,
            transcriptions_api: false,
        },
    );
    let boundary = "ccp-image-test";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"input.png\"\r\nContent-Type: image/png\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/images/edits")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["error"]["param"], "prompt");
}

#[tokio::test]
async fn image_routes_are_independently_opt_in() {
    let disabled = app_with_features(
        Arc::new(Registry::new()),
        None,
        AppFeatures {
            responses_api: false,
            images_api: false,
            transcriptions_api: false,
        },
    );
    let response = disabled
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/images/generations")
                .header("content-type", "application/json")
                .body(body_string("{"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let enabled = app_with_features(
        Arc::new(Registry::new()),
        None,
        AppFeatures {
            responses_api: false,
            images_api: true,
            transcriptions_api: false,
        },
    );
    let response = enabled
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/images/generations")
                .header("content-type", "application/json")
                .body(body_string("{"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn transcription_route_is_independently_opt_in_and_validates_multipart() {
    let disabled = app_with_features(
        Arc::new(Registry::new()),
        None,
        AppFeatures {
            responses_api: false,
            images_api: false,
            transcriptions_api: false,
        },
    );
    let response = disabled
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/audio/transcriptions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let enabled = app_with_features(
        Arc::new(Registry::new()),
        None,
        AppFeatures {
            responses_api: false,
            images_api: false,
            transcriptions_api: true,
        },
    );
    let boundary = "ccp-transcription-test";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\ngpt-4o-mini-transcribe\r\n--{boundary}--\r\n"
    );
    let response = enabled
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/audio/transcriptions")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["error"]["param"], "file");
}

#[tokio::test]
async fn transcription_route_rejects_non_audio_uploads() {
    let app = app_with_features(
        Arc::new(Registry::new()),
        None,
        AppFeatures {
            responses_api: false,
            images_api: false,
            transcriptions_api: true,
        },
    );
    let boundary = "ccp-transcription-type-test";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"notes.txt\"\r\nContent-Type: text/plain\r\n\r\nnot audio\r\n--{boundary}--\r\n"
    );
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/audio/transcriptions")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn native_responses_route_is_disabled_by_default_option() {
    let app = app_with_options(Arc::new(Registry::new()), None, false);
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(body_string(r#"{"model":"gpt-5.4","input":"hello"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn enabled_native_responses_route_uses_openai_errors() {
    let app = app_with_options(Arc::new(Registry::new()), None, true);
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(body_string("{"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap();
    assert!(body.get("type").is_none());
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["code"], "invalid_json");
}

#[tokio::test]
async fn chat_completions_route_uses_responses_api_gate() {
    let disabled = app_with_options(Arc::new(Registry::new()), None, false);
    let response = disabled
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"model":"gpt-5.6-sol","messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let enabled = app_with_options(Arc::new(Registry::new()), None, true);
    let response = enabled
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(body_string("{"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["code"], "invalid_json");
}

#[tokio::test]
async fn openai_routes_select_non_codex_providers_and_aliases() {
    let _routed = routed_to("kimi");
    for (uri, request, expected) in [
        (
            "/v1/chat/completions",
            json!({"model":"kimi-k2.6","messages":[{"role":"user","content":"hello"}]}),
            "kimi",
        ),
        (
            "/v1/responses",
            json!({"model":"grok-4.5","input":"hello"}),
            "grok",
        ),
        (
            "/v1/chat/completions",
            json!({"model":"cursor:gpt-5.5","messages":[{"role":"user","content":"hello"}]}),
            "cursor",
        ),
        (
            "/v1/responses",
            json!({"model":"sonnet","input":"hello"}),
            "kimi",
        ),
    ] {
        let response = app_with_options(routed_registry(), None, true)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(body_string(&request.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{uri} {request}");
        let value: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        let text = if uri.ends_with("responses") {
            value["output"][0]["content"][0]["text"].as_str()
        } else {
            value["choices"][0]["message"]["content"].as_str()
        };
        assert_eq!(text, Some(expected));
    }
}

/// Ask `/v1/responses` for `model`. The fake providers answer with their own name.
async fn responses_answer(app: &axum::Router, model: &str) -> (StatusCode, String) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(body_string(
                    &json!({"model": model, "input": "hello"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap();
    let text = value["output"][0]["content"][0]["text"]
        .as_str()
        .or_else(|| value["error"]["message"].as_str())
        .unwrap_or_default()
        .to_string();
    (status, text)
}

#[tokio::test]
async fn claude_names_follow_the_route_with_no_restart() {
    let routed = routed_to("kimi");
    let app = app_with_options(routed_registry(), None, true);

    assert_eq!(
        responses_answer(&app, "sonnet").await,
        (StatusCode::OK, "kimi".to_string())
    );

    // `cc-proxy use grok` writes this file. The same app picks it up.
    routed.set(true, "grok");
    assert_eq!(
        responses_answer(&app, "sonnet").await,
        (StatusCode::OK, "grok".to_string())
    );

    // `cc-proxy stop` and `cc-proxy use claude` write this.
    routed.set(false, "grok");
    let (status, message) = responses_answer(&app, "sonnet").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(message.contains("routing is off"), "{message}");

    // A model named in full never needed the route.
    assert_eq!(
        responses_answer(&app, "kimi-k2.6").await,
        (StatusCode::OK, "kimi".to_string())
    );
}

#[tokio::test]
async fn routing_is_off_until_a_command_turns_it_on() {
    // No config.json at all, which is a fresh install.
    let _routed = routed_to("kimi");
    std::fs::remove_file(
        std::path::Path::new(&std::env::var("CCP_CONFIG_DIR").unwrap()).join("config.json"),
    )
    .unwrap();
    let app = app_with_options(routed_registry(), None, true);

    let (status, message) = responses_answer(&app, "claude-sonnet-4-5").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(message.contains("routing is off"), "{message}");
}

#[tokio::test]
async fn openai_routes_preserve_serial_tool_calls_upstream() {
    for (provider, model, uri, body, expected_choice) in [
        (
            "kimi",
            "kimi-k2.6",
            "/v1/chat/completions",
            json!({
                "model":"kimi-k2.6",
                "messages":[{"role":"user","content":"look up x"}],
                "tools":[{"type":"function","function":{"name":"lookup","parameters":{"type":"object"}}}],
                "tool_choice":{"type":"function","function":{"name":"lookup"}},
                "parallel_tool_calls":false
            }),
            json!({"type":"function","function":{"name":"lookup"}}),
        ),
        (
            "grok",
            "grok-4.5",
            "/v1/responses",
            json!({
                "model":"grok-4.5",
                "input":"look up x",
                "tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}}],
                "tool_choice":"none",
                "parallel_tool_calls":false
            }),
            json!("none"),
        ),
    ] {
        let captured = Arc::new(Mutex::new(None));
        let response = app_with_options(
            translating_registry(provider, model, captured.clone()),
            None,
            true,
        )
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(uri)
                .header("content-type", "application/json")
                .body(body_string(&body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let translated = captured.lock().unwrap().clone().unwrap();
        assert_eq!(translated["parallel_tool_calls"], false);
        assert_eq!(translated["tool_choice"], expected_choice);
    }
}

#[tokio::test]
async fn routed_openai_streams_use_surface_specific_events() {
    let chat = app_with_options(routed_registry(), None, true)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"model":"kimi-k2.6","stream":true,"stream_options":{"include_usage":true},"messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let chat = String::from_utf8(
        axum::body::to_bytes(chat.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(chat.contains("chat.completion.chunk"));
    assert!(chat.contains("\"total_tokens\":3"));
    assert!(chat.ends_with("data: [DONE]\n\n"));

    let responses = app_with_options(routed_registry(), None, true)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"model":"grok-4.5","stream":true,"input":"hello"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let responses = String::from_utf8(
        axum::body::to_bytes(responses.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(responses.contains("event: response.created"));
    assert!(responses.contains("event: response.completed"));
    assert!(responses.contains("\"sequence_number\":0"));
}

#[tokio::test]
async fn non_codex_validation_uses_openai_errors_before_generation() {
    let monitor = MonitorHandle::new(10);
    let response = app_with_options(routed_registry(), Some(monitor.clone()), true)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .header("x-client-request-id", "invalid-routed-request")
                .body(body_string(
                    r#"{"model":"kimi-k2.6","messages":[{"role":"user","content":"hello"}],"temperature":0.5}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let value: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(value["error"]["param"], "temperature");
    assert_eq!(value["error"]["code"], "unsupported_parameter");
    assert!(cc_proxy::session::existing_session_now(Some("invalid-routed-request")).is_none());
    let snapshot = monitor.snapshot();
    assert_eq!(snapshot.recent[0].session_seq, None);
    assert_eq!(snapshot.recent[0].provider, None);
}

#[tokio::test]
async fn chat_completions_validation_returns_openai_parameter_errors() {
    let app = app_with_options(Arc::new(Registry::new()), None, true);
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"model":"gpt-5.6-sol","messages":[{"role":"user","content":"hello"}],"max_tokens":100}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["error"]["param"], "max_tokens");
    assert_eq!(body["error"]["code"], "unsupported_parameter");
}

#[tokio::test]
async fn unknown_routes_use_anthropic_not_found_error() {
    let app = app(Arc::new(Registry::new()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/nope")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: Value = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap();
    assert_eq!(body["type"].as_str().unwrap_or(""), "error");
}

#[tokio::test]
async fn monitor_records_successful_request_events() {
    let monitor = MonitorHandle::new(10);
    let app = app_with_monitor(Arc::new(Registry::new()), Some(monitor.clone()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages/count_tokens")
                .header("content-type", "application/json")
                .header("x-claude-code-session-id", "project-session")
                .body(body_string(
                    r##"{"model":"gpt-5.4","messages":[{"role":"user","content":"hello"}],"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.177.45c"},{"type":"text","text":"You are a Claude agent, built on Anthropic's Claude Agent SDK.","cache_control":{"type":"ephemeral"}},{"type":"text","text":"\nYou are an interactive agent.\n\n# Environment\nYou have been invoked in the following environment: \n - Primary working directory: /projects/example\n - Is a git repository: true","cache_control":{"type":"ephemeral"}}],"output_config":{"effort":"high"}}"##,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let state = monitor.snapshot();
    assert_eq!(state.active.len(), 1);
    assert!(state.recent.is_empty());

    let _body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let state = monitor.snapshot();
    assert!(state.active.is_empty());
    assert_eq!(state.recent.len(), 1);
    assert_eq!(state.recent[0].status, RequestStatus::Completed);
    assert_eq!(state.recent[0].http_status, Some(200));
    assert_eq!(
        state.recent[0].session_id.as_deref(),
        Some("project-session")
    );
    assert!(state.recent[0].session_seq.is_some());
    assert_eq!(state.recent[0].project.as_deref(), Some("example"));
    assert_eq!(state.sessions[0].project.as_deref(), Some("example"));
    assert_eq!(state.recent[0].provider.as_deref(), Some("codex"));
    assert_eq!(state.recent[0].model.as_deref(), Some("gpt-5.4"));
    assert_eq!(state.recent[0].effort.as_deref(), Some("high"));
    assert!(state.recent[0].input_tokens.is_some());
}

#[tokio::test]
async fn monitor_records_invalid_json_failure() {
    let monitor = MonitorHandle::new(10);
    let app = app_with_monitor(Arc::new(Registry::new()), Some(monitor.clone()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .body(body_string("{"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let state = monitor.snapshot();
    assert!(state.active.is_empty());
    assert_eq!(state.recent[0].status, RequestStatus::Failed);
    assert_eq!(state.recent[0].http_status, Some(400));
    let error = state.recent[0].error.as_deref().unwrap_or("");
    assert!(error.starts_with("Invalid JSON:"));
}

#[tokio::test]
async fn monitor_records_unknown_model_failure() {
    let monitor = MonitorHandle::new(10);
    let app = app_with_monitor(Arc::new(Registry::new()), Some(monitor.clone()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"messages":[{"role":"user","content":"hello"}],"model":"not-a-model"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let state = monitor.snapshot();
    assert!(state.active.is_empty());
    assert_eq!(state.recent[0].status, RequestStatus::Failed);
    assert_eq!(state.recent[0].http_status, Some(400));
    let error = state.recent[0].error.as_deref().unwrap_or("");
    assert!(error.starts_with("Unknown model \"not-a-model\""));
    assert!(error.contains("Supported:"));
}

async fn get_models(app: axum::Router, uri: &str) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    (status, value)
}

#[tokio::test]
async fn models_endpoint_lists_supported_models() {
    let app = app(Arc::new(Registry::new()));
    let (status, value) = get_models(app, "/v1/models").await;

    assert_eq!(status, StatusCode::OK);
    let data = value["data"].as_array().unwrap();
    assert!(!data.is_empty());
    let ids: Vec<&str> = data.iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"gpt-5.6-sol"));
    assert!(ids.contains(&"grok-4.6"));
    for entry in data {
        assert_eq!(entry["type"], "model");
        assert!(entry["display_name"].as_str().is_some());
    }
    assert_eq!(value["has_more"], json!(false));
    assert_eq!(value["first_id"], data[0]["id"]);
    assert_eq!(value["last_id"], data[data.len() - 1]["id"]);
}

#[tokio::test]
async fn models_endpoint_leads_with_claude_names_while_routing_is_on() {
    // Claude Code's gateway model discovery ignores ids that don't start with
    // "claude" or "anthropic", so these entries are what make
    // CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1 useful at all.
    let _routed = routed_to("codex");
    let _override = common::EnvGuard::unset("CCP_CODEX_MODEL");
    let registry = Arc::new(Registry::from_providers([
        Arc::new(IdentityCaptureProvider::default()) as Arc<dyn Provider>,
    ]));
    let mappings = cc_proxy::tiers::for_provider(&registry, "codex");
    let (status, value) = get_models(app(registry), "/v1/models?limit=1000").await;

    assert_eq!(status, StatusCode::OK);
    let data = value["data"].as_array().unwrap();
    let ids: Vec<&str> = data.iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert_eq!(
        ids[..4],
        [
            "claude-fable",
            "claude-opus",
            "claude-sonnet",
            "claude-haiku"
        ]
    );
    for (entry, tier) in data[..4].iter().zip(cc_proxy::tiers::TIERS) {
        assert_eq!(
            entry["display_name"],
            format!("codex/{}", mappings[tier].model)
        );
        let description = entry["description"].as_str().unwrap();
        assert!(description.starts_with("Answers "));
        assert!(description.contains(entry["id"].as_str().unwrap()));
    }
}

#[tokio::test]
async fn models_endpoint_has_no_claude_names_while_routing_is_off() {
    let routed = routed_to("codex");
    routed.set(false, "codex");
    let app = app(Arc::new(Registry::new()));
    let (status, value) = get_models(app, "/v1/models?limit=1000").await;

    assert_eq!(status, StatusCode::OK);
    let data = value["data"].as_array().unwrap();
    assert!(
        data.iter()
            .all(|m| !m["id"].as_str().unwrap().starts_with("claude-"))
    );
}

#[tokio::test]
async fn models_endpoint_respects_limit() {
    let app = app(Arc::new(Registry::new()));
    let (status, value) = get_models(app, "/v1/models?limit=2").await;

    assert_eq!(status, StatusCode::OK);
    let data = value["data"].as_array().unwrap();
    assert_eq!(data.len(), 2);
    assert_eq!(value["has_more"], json!(true));
    assert_eq!(value["last_id"], data[1]["id"]);
}

#[tokio::test]
async fn models_endpoint_tolerates_unknown_query_params() {
    let app = app(Arc::new(Registry::new()));
    let (status, _) = get_models(app, "/v1/models?limit=1000&after_id=x").await;
    assert_eq!(status, StatusCode::OK);
}
