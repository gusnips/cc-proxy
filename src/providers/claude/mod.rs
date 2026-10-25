//! Claude subscription provider, separate from the Anthropic API-key provider.

pub mod auth;
mod local;
mod login;
mod wire;

use std::time::Duration;

use async_trait::async_trait;
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use serde_json::Value;

use crate::accounts::Accounts;
use crate::anthropic::{CountTokensResponse, MessagesRequest};
use crate::provider::{
    CliHandlers, Generation, GenerationBody, Provider, ProviderError, ProviderErrorKind,
    RequestContext,
};
use crate::providers::{
    custom::ANTHROPIC_MODELS,
    kimi::count_tokens,
    opencode::{messages, sse_response, update_buffered_usage},
    upstream_error,
};
use crate::rotation::{RotationHint, now_ms};

pub struct ClaudeProvider {
    client: reqwest::Client,
    base_url: String,
}

impl Default for ClaudeProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeProvider {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .read_timeout(Duration::from_secs(120))
                .user_agent(concat!("cc-proxy/", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("failed to build Claude client"),
            base_url: "https://api.anthropic.com".into(),
        }
    }

    fn model(&self, body: &MessagesRequest) -> Result<String, ProviderError> {
        body.model
            .as_deref()
            .and_then(|model| model.strip_prefix("claude/"))
            .filter(|model| !model.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                ProviderError::new(
                    StatusCode::BAD_REQUEST,
                    ProviderErrorKind::InvalidRequest,
                    "Choose a Claude model with `claude/<model-id>` or route a Claude name with `cc-proxy use claude`.",
                )
            })
    }

    async fn send(
        &self,
        body: &MessagesRequest,
        ctx: &mut RequestContext,
    ) -> Result<(reqwest::Response, String), ProviderError> {
        ctx.pin_account(&auth::CLI)?;
        let accounts = Accounts::open("claude", &auth::CLI).map_err(auth_error)?;
        let store = accounts
            .for_request::<auth::StoredAuth>(ctx.account.as_deref())
            .map_err(auth_error)?;
        let manager = auth::AuthManager::new(store);
        self.send_authenticated(body, ctx, &manager).await
    }

    async fn send_authenticated<S: crate::auth::AuthStorage<auth::StoredAuth>>(
        &self,
        body: &MessagesRequest,
        ctx: &RequestContext,
        manager: &auth::AuthManager<S>,
    ) -> Result<(reqwest::Response, String), ProviderError> {
        let mut auth = manager.get_auth().await.map_err(auth_error)?;
        let model = self.model(body)?;
        if let Some(monitor) = &ctx.monitor {
            monitor.model_resolved(&ctx.req_id, &model);
            monitor.upstream_started(&ctx.req_id);
        }
        for attempt in 0..2 {
            let wire = wire::prepare(body, &model, auth.account_id.as_deref()).map_err(|_| {
                ProviderError::new(
                    StatusCode::BAD_REQUEST,
                    ProviderErrorKind::InvalidRequest,
                    "Could not prepare the Claude request. Retry the request.",
                )
            })?;
            if let Some(traffic) = &ctx.traffic {
                traffic.write_json("020-upstream-request", &wire);
            }
            let mut betas = vec![auth::OAUTH_BETA.to_string()];
            if let Some(caller) = &body.anthropic_beta {
                for beta in caller.split(',').map(str::trim).filter(|beta| {
                    !beta.is_empty()
                        && !beta.starts_with("oauth-")
                        && !beta.starts_with("claude-code-")
                }) {
                    if !betas
                        .iter()
                        .any(|item| item.split(',').any(|item| item == beta))
                    {
                        betas.push(beta.to_string());
                    }
                }
            }
            let response = self
                .client
                .post(format!("{}/v1/messages", self.base_url))
                .bearer_auth(&auth.access)
                .header("anthropic-version", "2023-06-01")
                .header("anthropic-beta", betas.join(","))
                .header(
                    "accept",
                    if body.stream {
                        "text/event-stream"
                    } else {
                        "application/json"
                    },
                )
                .header("x-client-request-id", &ctx.req_id)
                .json(&wire)
                .send()
                .await
                .map_err(|_| {
                    ProviderError::new(
                        StatusCode::BAD_GATEWAY,
                        ProviderErrorKind::Api,
                        "Could not reach Claude. Check your connection and retry.",
                    )
                })?;
            if response.status() == StatusCode::UNAUTHORIZED && attempt == 0 {
                auth = manager
                    .force_refresh(&auth.access)
                    .await
                    .map_err(auth_error)?;
                continue;
            }
            if !response.status().is_success() {
                return Err(rejected(response, &auth.access).await);
            }
            return Ok((response, model));
        }
        unreachable!("the second attempt returns its response")
    }
}

fn auth_error(error: anyhow::Error) -> ProviderError {
    ProviderError::new(
        StatusCode::UNAUTHORIZED,
        ProviderErrorKind::Authentication,
        error.to_string(),
    )
}

async fn read_body(response: reqwest::Response) -> Result<Vec<u8>, ProviderError> {
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| {
            ProviderError::new(
                StatusCode::BAD_GATEWAY,
                ProviderErrorKind::Api,
                "Claude stopped before the answer finished. Retry the request.",
            )
        })?;
        if bytes.len().saturating_add(chunk.len()) > 8 * 1024 * 1024 {
            return Err(ProviderError::new(
                StatusCode::BAD_GATEWAY,
                ProviderErrorKind::Api,
                "Claude's reply exceeded 8 MB. Try a smaller request.",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn rejected(response: reqwest::Response, token: &str) -> ProviderError {
    let status = response.status();
    let retry = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let reset = quota_reset(response.headers(), now_ms());
    let body = read_body(response).await.unwrap_or_default();
    let safe = String::from_utf8_lossy(&body).replace(token, "[redacted]");
    let mut error = upstream_error::from_http(
        status.as_u16(),
        &safe,
        retry.as_deref(),
        "Claude subscription",
    );
    if status == StatusCode::TOO_MANY_REQUESTS {
        error.rotation_hint = Some(match reset {
            Some(until_ms) => RotationHint::RetryAt {
                until_ms,
                per_model: false,
            },
            None => RotationHint::Stay,
        });
        if reset.is_some() {
            error.should_retry = Some(false);
        }
    }
    error
}

fn quota_reset(headers: &http::HeaderMap, now: u64) -> Option<u64> {
    let value = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    let mut resets = Vec::new();
    for window in ["5h", "7d"] {
        if value(&format!("anthropic-ratelimit-unified-{window}-status")) == Some("rejected") {
            if let Some(reset) = value(&format!("anthropic-ratelimit-unified-{window}-reset")) {
                resets.push(reset);
            }
        }
    }
    if resets.is_empty() && value("anthropic-ratelimit-unified-status") == Some("rejected") {
        if let Some(reset) = value("anthropic-ratelimit-unified-reset") {
            resets.push(reset);
        }
    }
    resets
        .into_iter()
        .filter_map(|reset| reset.parse::<u64>().ok()?.checked_mul(1000))
        .filter(|until| *until > now)
        .max()
}

#[async_trait]
impl Provider for ClaudeProvider {
    fn name(&self) -> &'static str {
        "claude"
    }
    fn supported_models(&self) -> Vec<String> {
        ANTHROPIC_MODELS
            .iter()
            .map(|model| format!("claude/{model}"))
            .collect()
    }
    fn cli(&self) -> &dyn CliHandlers {
        &auth::CLI
    }

    async fn handle_messages(&self, body: MessagesRequest, mut ctx: RequestContext) -> Response {
        if body.stream {
            return match self.generate_anthropic_stream(body, ctx).await {
                Ok(generation) => sse_response(generation.body),
                Err(error) => error.response(),
            };
        }
        let result = async {
            let (response, _) = self.send(&body, &mut ctx).await?;
            let bytes = read_body(response).await?;
            let mut value: Value = serde_json::from_slice(&bytes).map_err(|_| {
                ProviderError::new(
                    StatusCode::BAD_GATEWAY,
                    ProviderErrorKind::Api,
                    "Claude returned an unreadable reply. Retry the request.",
                )
            })?;
            if let Some(traffic) = &ctx.traffic {
                traffic.write_json("040-upstream-response", &value);
            }
            wire::unprefix(&mut value);
            update_buffered_usage(&ctx, &value);
            Ok::<_, ProviderError>(value)
        }
        .await;
        match result {
            Ok(value) => Json(value).into_response(),
            Err(error) => error.response(),
        }
    }

    async fn handle_count_tokens(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let input_tokens = count_tokens::count_tokens(&body);
        if let Some(monitor) = &ctx.monitor {
            monitor.usage_updated(&ctx.req_id, Some(input_tokens), None);
        }
        Json(CountTokensResponse { input_tokens }).into_response()
    }

    async fn generate_anthropic_stream(
        &self,
        mut body: MessagesRequest,
        mut ctx: RequestContext,
    ) -> Result<Generation, ProviderError> {
        body.stream = true;
        let (response, model) = self.send(&body, &mut ctx).await?;
        Ok(Generation {
            body: GenerationBody::LiveSse(messages::stream_body_with_transform(
                response.bytes_stream(),
                "Claude subscription",
                ctx.monitor,
                ctx.req_id,
                ctx.traffic,
                Some(wire::unprefix),
            )),
            resolved_model: model,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthStorage, InMemoryAuthStore};
    use axum::{Router, extract::State, routing::post};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn oauth_request_uses_bearer_only_and_returns_confirmed_quota_for_rotation() {
        type Seen = Arc<Mutex<Option<(http::HeaderMap, Value)>>>;
        async fn handler(
            State(seen): State<Seen>,
            headers: http::HeaderMap,
            Json(body): Json<Value>,
        ) -> Response {
            *seen.lock().unwrap() = Some((headers, body));
            (
                StatusCode::TOO_MANY_REQUESTS,
                [
                    ("anthropic-ratelimit-unified-status", "rejected"),
                    ("anthropic-ratelimit-unified-reset", "4102444800"),
                ],
                Json(json!({"error":{"type":"rate_limit_error","message":"Usage limit reached"}})),
            )
                .into_response()
        }
        let seen = Seen::default();
        let app = Router::new()
            .route("/v1/messages", post(handler))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let provider = ClaudeProvider {
            base_url: format!("http://{address}"),
            ..ClaudeProvider::new()
        };
        let store = InMemoryAuthStore::new();
        store
            .save(auth::StoredAuth {
                access: "subscription-token".into(),
                refresh: "refresh-token".into(),
                expires: u64::MAX,
                email: None,
                account_id: Some("selected-account".into()),
                plan: None,
            })
            .unwrap();
        let manager = auth::AuthManager::new(store);
        let mut body: MessagesRequest = serde_json::from_value(json!({
            "model":"claude/claude-sonnet-5-5", "stream":true,
            "messages":[{"role":"user","content":"hello"}],
            "tools":[{"name":"Read","input_schema":{"type":"object"}}]
        }))
        .unwrap();
        body.anthropic_beta = Some("context-management-2025-06-27".into());
        let ctx = RequestContext {
            req_id: "request".into(),
            session_id: None,
            session_seq: None,
            provider: "claude".into(),
            account: None,
            monitor: None,
            traffic: None,
        };
        let error = match provider.send_authenticated(&body, &ctx, &manager).await {
            Ok(_) => panic!("quota rejection must fail before a stream is returned"),
            Err(error) => error,
        };
        assert!(matches!(
            error.rotation_hint,
            Some(RotationHint::RetryAt {
                until_ms: 4_102_444_800_000,
                per_model: false
            })
        ));
        let captured = seen.lock().unwrap();
        let (headers, body) = captured.as_ref().unwrap();
        assert_eq!(headers["authorization"], "Bearer subscription-token");
        assert!(!headers.contains_key("x-api-key"));
        assert!(
            headers["anthropic-beta"]
                .to_str()
                .unwrap()
                .contains(auth::OAUTH_BETA)
        );
        assert!(
            headers["anthropic-beta"]
                .to_str()
                .unwrap()
                .contains("context-management-2025-06-27")
        );
        assert_eq!(body["tools"][0]["name"], "custom_Read");
        assert_eq!(body["model"], "claude-sonnet-5-5");
        server.abort();
    }

    #[test]
    fn only_confirmed_exhausted_windows_rotate_not_a_short_throttle() {
        let headers = http::HeaderMap::from_iter([
            (http::header::RETRY_AFTER, "2".parse().unwrap()),
            (
                http::HeaderName::from_static("anthropic-ratelimit-unified-status"),
                "allowed".parse().unwrap(),
            ),
            (
                http::HeaderName::from_static("anthropic-ratelimit-unified-reset"),
                "2000000000".parse().unwrap(),
            ),
        ]);
        assert_eq!(quota_reset(&headers, 0), None);
        let mut headers = headers;
        headers.insert(
            "anthropic-ratelimit-unified-status",
            "rejected".parse().unwrap(),
        );
        assert_eq!(quota_reset(&headers, 0), Some(2_000_000_000_000));
        headers.insert(
            "anthropic-ratelimit-unified-7d-status",
            "rejected".parse().unwrap(),
        );
        headers.insert(
            "anthropic-ratelimit-unified-7d-reset",
            "2000000100".parse().unwrap(),
        );
        assert_eq!(quota_reset(&headers, 0), Some(2_000_000_100_000));
    }
}
