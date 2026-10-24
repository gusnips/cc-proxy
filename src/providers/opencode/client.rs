use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::model::EndpointKind;
use crate::traffic::TrafficCapture;

const MAX_BUFFERED_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_USAGE_RESPONSE_BYTES: usize = 64 * 1024;
const USAGE_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const USER_AGENT: &str = concat!("cc-proxy/", env!("CARGO_PKG_VERSION"));
const OPENCODE_CLI_USER_AGENT: &str = "opencode/latest/2.0.32/cli";

/// Deterministically turns any conversation session ID into OpenCode's
/// `ses_<12 hex><14 base62>` identifier shape (`packages/schema/src/session-id.ts`).
pub fn forge_opencode_session_id(session_id: Option<&str>) -> String {
    use sha2::{Digest, Sha256};
    if let Some(existing) = session_id.filter(|s| s.starts_with("ses_") && s.len() == 30) {
        return existing.to_string();
    }
    let seed = session_id
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let digest = Sha256::digest(seed.as_bytes());
    let mut hex_part = String::with_capacity(12);
    for b in &digest[..6] {
        hex_part.push_str(&format!("{b:02x}"));
    }
    const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut tail = String::with_capacity(14);
    for b in &digest[6..20] {
        tail.push(BASE62[(*b as usize) % 62] as char);
    }
    format!("ses_{hex_part}{tail}")
}

pub struct OpenCodeClient {
    client: Arc<reqwest::Client>,
    base_url: reqwest::Url,
    /// Asked on every request, so a provider that reads config.json here
    /// picks up a key saved while the proxy runs.
    api_key: Box<dyn Fn() -> Option<String> + Send + Sync>,
}

pub struct OpenCodeResponse {
    response: reqwest::Response,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenCodeUsageResponse {
    pub usage: OpenCodeUsage,
    #[serde(flatten)]
    pub(super) extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenCodeUsage {
    pub rolling: Option<OpenCodeUsageWindow>,
    pub weekly: Option<OpenCodeUsageWindow>,
    pub monthly: Option<OpenCodeUsageWindow>,
    #[serde(flatten)]
    pub(super) extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenCodeUsageWindow {
    pub status: Option<String>,
    pub percent: Option<f64>,
    pub resets_at: Option<String>,
    #[serde(flatten)]
    pub(super) extra: BTreeMap<String, Value>,
}

impl OpenCodeUsage {
    fn has_known_data(&self) -> bool {
        [&self.rolling, &self.weekly, &self.monthly]
            .into_iter()
            .flatten()
            .any(OpenCodeUsageWindow::has_known_data)
    }
}

impl OpenCodeUsageWindow {
    fn has_known_data(&self) -> bool {
        self.status.is_some() || self.percent.is_some() || self.resets_at.is_some()
    }
}

#[derive(Debug)]
pub struct OpenCodeError {
    pub status: StatusCode,
    pub retry_after: Option<String>,
    pub message: String,
    /// The upstream's rejection body, redacted. Its codes and types (for
    /// example `insufficient_quota`) say what failed when the prose does not.
    pub body: Option<String>,
}

impl OpenCodeResponse {
    pub fn into_stream(
        self,
    ) -> impl futures_util::Stream<Item = Result<bytes::Bytes, OpenCodeError>> + Send {
        self.response.bytes_stream().map(|chunk| {
            chunk.map_err(|_| OpenCodeError {
                status: StatusCode::BAD_GATEWAY,
                retry_after: None,
                body: None,
                message: "OpenCode Go upstream stream failed".to_string(),
            })
        })
    }

    pub async fn into_bytes(self) -> Result<Vec<u8>, OpenCodeError> {
        self.into_bytes_with_limit(
            MAX_BUFFERED_RESPONSE_BYTES,
            "OpenCode Go upstream response exceeds the size limit",
        )
        .await
    }

    async fn into_bytes_with_limit(
        self,
        limit: usize,
        size_error: &'static str,
    ) -> Result<Vec<u8>, OpenCodeError> {
        let mut stream = self.into_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if bytes.len().saturating_add(chunk.len()) > limit {
                return Err(OpenCodeError {
                    status: StatusCode::BAD_GATEWAY,
                    retry_after: None,
                    body: None,
                    message: size_error.to_string(),
                });
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

impl OpenCodeClient {
    pub fn new(base_url: String, api_key: Option<String>) -> anyhow::Result<Self> {
        Self::with_key_source(base_url, move || api_key.clone())
    }

    /// This client, sending no key. OpenCode Zen's free tier needs none and
    /// refuses a stale one with "Invalid credential".
    pub fn anonymous(&self) -> Self {
        self.with_key(None)
    }

    /// This client, sending `api_key` on every request instead of whatever
    /// source it was built with. Used for the account a request selected.
    pub fn with_key(&self, api_key: Option<String>) -> Self {
        Self {
            client: self.client.clone(),
            base_url: self.base_url.clone(),
            api_key: Box::new(move || api_key.clone()),
        }
    }

    /// A client that reads its key from `api_key` on every request.
    pub fn with_key_source(
        base_url: String,
        api_key: impl Fn() -> Option<String> + Send + Sync + 'static,
    ) -> anyhow::Result<Self> {
        let base_url = reqwest::Url::parse(base_url.trim_end_matches('/'))?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .user_agent(USER_AGENT)
            .build()?;
        Ok(Self {
            client: Arc::new(client),
            base_url,
            api_key: Box::new(api_key),
        })
    }

    pub async fn post<T: Serialize + ?Sized>(
        &self,
        endpoint: EndpointKind,
        body: &T,
        stream: bool,
        traffic: Option<Arc<TrafficCapture>>,
        session_id: Option<&str>,
    ) -> Result<OpenCodeResponse, OpenCodeError> {
        self.post_for_provider(
            endpoint,
            body,
            stream,
            traffic,
            session_id,
            "opencode",
            "OpenCode Go",
            true,
            None,
        )
        .await
    }

    /// `anthropic_beta` is the client's `anthropic-beta` header. Only a
    /// Messages request sends it on.
    #[allow(clippy::too_many_arguments)]
    pub async fn post_for_provider<T: Serialize + ?Sized>(
        &self,
        endpoint: EndpointKind,
        body: &T,
        stream: bool,
        traffic: Option<Arc<TrafficCapture>>,
        session_id: Option<&str>,
        provider_slug: &str,
        provider_label: &str,
        require_key: bool,
        anthropic_beta: Option<&str>,
    ) -> Result<OpenCodeResponse, OpenCodeError> {
        let api_key = if require_key {
            Some(self.required_key(provider_slug, provider_label)?)
        } else {
            self.current_key()
        };
        let api_key = api_key.as_deref();
        let betas = anthropic_beta
            .filter(|_| endpoint == EndpointKind::Messages)
            .and_then(upstream_betas);
        let url = self.endpoint_url(endpoint);
        let accept = if stream {
            "text/event-stream"
        } else {
            "application/json"
        };

        let session_header_value = if provider_slug == "opencode-zen" {
            forge_opencode_session_id(session_id)
        } else {
            session_id
                .filter(|value| http::HeaderValue::from_str(value).is_ok())
                .map(str::to_string)
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
        };

        if let Some(capture) = traffic.as_ref() {
            let value = serde_json::to_value(body).unwrap_or(serde_json::Value::Null);
            capture.write_json("020-upstream-request", &value);
            let auth_header = match endpoint {
                EndpointKind::ChatCompletions | EndpointKind::Responses => "authorization",
                EndpointKind::Messages => "x-api-key",
                EndpointKind::Google => "x-goog-api-key",
            };
            capture.write_json(
                "021-upstream-request-metadata",
                &serde_json::json!({
                    "method": "POST",
                    "url": url.as_str(),
                    "provider": provider_slug,
                    "transport": "http",
                    "headers": {
                        "accept": accept,
                        "anthropic-beta": betas,
                        auth_header: if api_key.is_some() { "[redacted]" } else { "[none]" },
                        "content-type": "application/json",
                        "x-opencode-session": session_header_value
                    }
                }),
            );
        }

        let mut request = self
            .client
            .post(url)
            .header(http::header::ACCEPT, accept)
            .header(http::header::CONTENT_TYPE, "application/json")
            .header("x-opencode-session", &session_header_value)
            .json(body);
        if provider_slug == "opencode-zen" {
            request = request
                .header(http::header::USER_AGENT, OPENCODE_CLI_USER_AGENT)
                .header("x-opencode-session-id", &session_header_value)
                .header("x-session-affinity", &session_header_value)
                .header("x-session-id", &session_header_value)
                .header("x-opencode-project", "global")
                .header("x-opencode-client", "cli");
        }
        match endpoint {
            EndpointKind::ChatCompletions | EndpointKind::Responses => {
                if let Some(api_key) = api_key {
                    request =
                        request.header(http::header::AUTHORIZATION, format!("Bearer {api_key}"));
                }
            }
            EndpointKind::Messages => {
                request = request.header("anthropic-version", "2023-06-01");
                if let Some(betas) = betas.as_deref() {
                    request = request.header("anthropic-beta", betas);
                }
                if let Some(api_key) = api_key {
                    request = request.header("x-api-key", api_key);
                }
            }
            EndpointKind::Google => {
                if let Some(api_key) = api_key {
                    request = request
                        .header("x-goog-api-key", api_key)
                        .header(http::header::AUTHORIZATION, format!("Bearer {api_key}"));
                }
            }
        }
        let response = request.send().await.map_err(|_| OpenCodeError {
            status: StatusCode::BAD_GATEWAY,
            retry_after: None,
            body: None,
            message: format!("{provider_label} upstream request failed"),
        })?;

        if let Some(capture) = traffic.as_ref() {
            capture.write_json(
                "030-upstream-response-headers",
                &serde_json::json!({
                    "status": response.status().as_u16(),
                    "headers": safe_headers(response.headers())
                }),
            );
        }

        if !response.status().is_success() {
            return Err(rejected_response(response, api_key).await);
        }
        Ok(OpenCodeResponse { response })
    }

    pub async fn get_usage(&self) -> Result<OpenCodeUsageResponse, OpenCodeError> {
        let api_key = self.required_key("opencode", "OpenCode Go")?;
        let response = self
            .client
            .get(self.usage_url())
            .header(http::header::ACCEPT, "application/json")
            .header(http::header::AUTHORIZATION, format!("Bearer {api_key}"))
            .timeout(USAGE_REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|error| OpenCodeError {
                status: if error.is_timeout() {
                    StatusCode::GATEWAY_TIMEOUT
                } else {
                    StatusCode::BAD_GATEWAY
                },
                retry_after: None,
                body: None,
                message: if error.is_timeout() {
                    "OpenCode Go usage request timed out"
                } else {
                    "OpenCode Go usage request failed"
                }
                .to_string(),
            })?;
        if !response.status().is_success() {
            return Err(rejected_response(response, Some(&api_key)).await);
        }
        let bytes = OpenCodeResponse { response }
            .into_bytes_with_limit(
                MAX_USAGE_RESPONSE_BYTES,
                "OpenCode Go usage response exceeds the size limit",
            )
            .await?;
        let parsed: OpenCodeUsageResponse =
            serde_json::from_slice(&bytes).map_err(|_| OpenCodeError {
                status: StatusCode::BAD_GATEWAY,
                retry_after: None,
                body: None,
                message: "OpenCode Go usage response was invalid".to_string(),
            })?;
        if !parsed.usage.has_known_data() {
            return Err(OpenCodeError {
                status: StatusCode::BAD_GATEWAY,
                retry_after: None,
                body: None,
                message: "OpenCode Go usage response contained no recognized windows".to_string(),
            });
        }
        Ok(parsed)
    }

    fn current_key(&self) -> Option<String> {
        (self.api_key)().filter(|key| !key.is_empty())
    }

    fn required_key(
        &self,
        provider_slug: &str,
        provider_label: &str,
    ) -> Result<String, OpenCodeError> {
        self.current_key().ok_or_else(|| OpenCodeError {
            status: StatusCode::UNAUTHORIZED,
            retry_after: None,
            body: None,
            message: format!(
                "{provider_label} has no API key. Run `cc-proxy auth login {provider_slug}` to add one."
            ),
        })
    }

    fn endpoint_url(&self, endpoint: EndpointKind) -> reqwest::Url {
        let mut url = self.base_url.clone();
        let base_path = url.path().trim_end_matches('/');
        let new_path = match endpoint {
            EndpointKind::ChatCompletions => {
                if base_path.ends_with("/chat/completions") {
                    base_path.to_string()
                } else if base_path.is_empty() {
                    "/v1/chat/completions".to_string()
                } else {
                    format!("{base_path}/chat/completions")
                }
            }
            EndpointKind::Messages => {
                if base_path.ends_with("/messages") {
                    base_path.to_string()
                } else if base_path.ends_with("/v1") {
                    format!("{base_path}/messages")
                } else {
                    format!("{base_path}/v1/messages")
                }
            }
            EndpointKind::Responses => {
                if base_path.ends_with("/responses") {
                    base_path.to_string()
                } else if base_path.is_empty() {
                    "/v1/responses".to_string()
                } else {
                    format!("{base_path}/responses")
                }
            }
            EndpointKind::Google => format!("{base_path}/models"),
        };
        url.set_path(&new_path);
        url
    }

    pub fn google_url(&self, model_id: &str, stream: bool) -> reqwest::Url {
        let mut url = self.base_url.clone();
        let base_path = url.path().trim_end_matches('/');
        let action = if stream {
            "streamGenerateContent"
        } else {
            "generateContent"
        };
        let path = if base_path.ends_with("/zen/v1") || base_path.ends_with("/v1beta") {
            format!("{base_path}/models/{model_id}:{action}")
        } else if base_path.ends_with("/zen/go/v1") {
            let prefix = base_path.trim_end_matches("/zen/go/v1");
            format!("{prefix}/inference/go/google/v1beta/models/{model_id}:{action}")
        } else if base_path.is_empty() {
            format!("/v1beta/models/{model_id}:{action}")
        } else {
            format!("{base_path}/models/{model_id}:{action}")
        };
        url.set_path(&path);
        if stream {
            url.set_query(Some("alt=sse"));
        }
        url
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn post_google<T: Serialize + ?Sized>(
        &self,
        model_id: &str,
        body: &T,
        stream: bool,
        traffic: Option<Arc<TrafficCapture>>,
        session_id: Option<&str>,
        provider_slug: &str,
        provider_label: &str,
        require_key: bool,
        bearer_only: bool,
    ) -> Result<OpenCodeResponse, OpenCodeError> {
        let api_key = if require_key {
            Some(self.required_key(provider_slug, provider_label)?)
        } else {
            self.current_key()
        };
        let api_key = api_key.as_deref();
        let url = self.google_url(model_id, stream);
        let accept = if stream {
            "text/event-stream"
        } else {
            "application/json"
        };
        let session_header_value = if provider_slug == "opencode-zen" {
            forge_opencode_session_id(session_id)
        } else {
            session_id
                .filter(|value| http::HeaderValue::from_str(value).is_ok())
                .map(str::to_string)
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
        };

        if let Some(capture) = traffic.as_ref() {
            let value = serde_json::to_value(body).unwrap_or(serde_json::Value::Null);
            capture.write_json("020-upstream-request", &value);
            capture.write_json(
                "021-upstream-request-metadata",
                &serde_json::json!({
                    "method": "POST",
                    "url": url.as_str(),
                    "provider": provider_slug,
                    "transport": "http",
                    "headers": {
                        "accept": accept,
                        "authorization": if api_key.is_some() { "[redacted]" } else { "[none]" },
                        "content-type": "application/json",
                        "x-opencode-session": session_header_value
                    }
                }),
            );
        }

        let mut request = self
            .client
            .post(url)
            .header(http::header::ACCEPT, accept)
            .header(http::header::CONTENT_TYPE, "application/json")
            .json(body);
        if provider_slug.starts_with("opencode") {
            request = request.header("x-opencode-session", &session_header_value);
        }
        if provider_slug == "opencode-zen" {
            request = request
                .header(http::header::USER_AGENT, OPENCODE_CLI_USER_AGENT)
                .header("x-opencode-session-id", &session_header_value)
                .header("x-session-affinity", &session_header_value)
                .header("x-session-id", &session_header_value)
                .header("x-opencode-project", "global")
                .header("x-opencode-client", "cli");
        }
        if let Some(api_key) = api_key {
            if bearer_only || provider_slug == "opencode" {
                request = request.header(http::header::AUTHORIZATION, format!("Bearer {api_key}"));
            } else {
                request = request.header("x-goog-api-key", api_key);
            }
        }
        let response = request.send().await.map_err(|err| OpenCodeError {
            status: StatusCode::BAD_GATEWAY,
            retry_after: None,
            body: None,
            message: format!("{provider_label} upstream request failed: {err}"),
        })?;

        if let Some(capture) = traffic.as_ref() {
            capture.write_json(
                "030-upstream-response-headers",
                &serde_json::json!({
                    "status": response.status().as_u16(),
                    "headers": safe_headers(response.headers())
                }),
            );
        }

        if !response.status().is_success() {
            return Err(rejected_response(response, api_key).await);
        }
        Ok(OpenCodeResponse { response })
    }

    fn usage_url(&self) -> reqwest::Url {
        let mut url = self.base_url.clone();
        let base_path = url.path().trim_end_matches('/');
        url.set_path(&format!("{base_path}/usage"));
        url
    }
}

/// The betas to send Anthropic: Claude Code's own, minus `oauth-*`. Claude
/// Code signs in to cc-proxy, and cc-proxy sends Anthropic its own API key.
fn upstream_betas(header: &str) -> Option<String> {
    let betas: Vec<&str> = header
        .split(',')
        .map(str::trim)
        .filter(|beta| !beta.is_empty() && !beta.starts_with("oauth-"))
        .collect();
    (!betas.is_empty()).then(|| betas.join(","))
}

async fn rejected_response(response: reqwest::Response, secret: Option<&str>) -> OpenCodeError {
    let status = response.status();
    let retry_after = response
        .headers()
        .get(http::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while body.len() < 64 * 1024 {
        let Some(chunk) = stream.next().await else {
            break;
        };
        let Ok(chunk) = chunk else {
            break;
        };
        let remaining = 64 * 1024 - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    let mut message = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.get("message"))
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("OpenCode Go upstream returned HTTP {status}"));
    let mut body = String::from_utf8_lossy(&body).into_owned();
    if let Some(secret) = secret.filter(|secret| !secret.is_empty()) {
        message = message.replace(secret, "[redacted]");
        body = body.replace(secret, "[redacted]");
    }
    OpenCodeError {
        status,
        retry_after,
        message,
        body: Some(body),
    }
}

fn safe_headers(headers: &reqwest::header::HeaderMap) -> serde_json::Value {
    let mut result = serde_json::Map::new();
    for name in [
        "content-type",
        "content-length",
        "retry-after",
        "x-request-id",
    ] {
        if let Some(value) = headers.get(name).and_then(|value| value.to_str().ok()) {
            result.insert(
                name.to_string(),
                serde_json::Value::String(value.to_string()),
            );
        }
    }
    serde_json::Value::Object(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::{OriginalUri, State},
        http::HeaderMap,
        response::IntoResponse,
        routing::{get, post},
    };
    use std::sync::Mutex;

    #[derive(Debug)]
    struct SeenRequest {
        path: String,
        authorization: String,
        x_api_key: String,
        anthropic_version: String,
        anthropic_beta: String,
        x_opencode_session: String,
        body: serde_json::Value,
    }

    type Seen = Arc<Mutex<Vec<SeenRequest>>>;

    async fn capture_request(
        State(seen): State<Seen>,
        OriginalUri(uri): OriginalUri,
        headers: HeaderMap,
        Json(body): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        seen.lock().unwrap().push(SeenRequest {
            path: uri.path().to_string(),
            authorization: headers
                .get(http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string(),
            x_api_key: headers
                .get("x-api-key")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string(),
            anthropic_version: headers
                .get("anthropic-version")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string(),
            anthropic_beta: headers
                .get("anthropic-beta")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string(),
            x_opencode_session: headers
                .get("x-opencode-session")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string(),
            body,
        });
        Json(serde_json::json!({"ok": true}))
    }

    #[test]
    fn endpoint_urls_preserve_go_base_path() {
        let client = OpenCodeClient::new(
            "https://opencode.ai/zen/go/v1/".to_string(),
            Some("test".to_string()),
        )
        .unwrap();
        assert_eq!(
            client.endpoint_url(EndpointKind::ChatCompletions).as_str(),
            "https://opencode.ai/zen/go/v1/chat/completions"
        );
        assert_eq!(
            client.endpoint_url(EndpointKind::Messages).as_str(),
            "https://opencode.ai/zen/go/v1/messages"
        );
        assert_eq!(
            client.endpoint_url(EndpointKind::Responses).as_str(),
            "https://opencode.ai/zen/go/v1/responses"
        );
        assert_eq!(
            client.usage_url().as_str(),
            "https://opencode.ai/zen/go/v1/usage"
        );
    }

    #[tokio::test]
    async fn usage_uses_bearer_auth_and_parses_all_windows() {
        async fn usage(headers: HeaderMap) -> Json<serde_json::Value> {
            assert_eq!(
                headers
                    .get(http::header::AUTHORIZATION)
                    .and_then(|value| value.to_str().ok()),
                Some("Bearer test-key")
            );
            assert_eq!(
                headers
                    .get(http::header::USER_AGENT)
                    .and_then(|value| value.to_str().ok()),
                Some(USER_AGENT)
            );
            Json(serde_json::json!({
                "usage": {
                    "rolling": {"status":"ok", "percent":12.5, "resetsAt":"2026-09-10T12:00:00.000Z"},
                    "weekly": {"status":"ok", "percent":34, "resetsAt":"2026-09-14T00:00:00.000Z"},
                    "monthly": {"status":"rate-limited", "percent":100, "resetsAt":"2026-10-01T00:00:00.000Z"}
                }
            }))
        }

        let app = Router::new().route("/v1/usage", get(usage));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client =
            OpenCodeClient::new(format!("http://{address}/v1"), Some("test-key".to_string()))
                .unwrap();

        let response = client.get_usage().await.unwrap();
        assert_eq!(
            response
                .usage
                .rolling
                .as_ref()
                .and_then(|window| window.percent),
            Some(12.5)
        );
        assert_eq!(
            response
                .usage
                .weekly
                .as_ref()
                .and_then(|window| window.percent),
            Some(34.0)
        );
        assert_eq!(
            response
                .usage
                .monthly
                .as_ref()
                .and_then(|window| window.status.as_deref()),
            Some("rate-limited")
        );
        server.abort();
    }

    #[tokio::test]
    async fn usage_accepts_partial_windows_and_preserves_unknown_fields() {
        let app = Router::new().route(
            "/v1/usage",
            get(|| async {
                Json(serde_json::json!({
                    "usage": {
                        "rolling": {"percent":12.5, "futureWindowField":true},
                        "futureUsageField": "kept"
                    },
                    "futureRootField": {"kept": true}
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client =
            OpenCodeClient::new(format!("http://{address}/v1"), Some("test-key".to_string()))
                .unwrap();

        let response = client.get_usage().await.unwrap();
        assert!(response.usage.weekly.is_none());
        assert!(
            response
                .usage
                .rolling
                .as_ref()
                .and_then(|window| window.resets_at.as_ref())
                .is_none()
        );
        let serialized = serde_json::to_value(response).unwrap();
        assert_eq!(serialized["futureRootField"]["kept"], true);
        assert_eq!(serialized["usage"]["futureUsageField"], "kept");
        assert_eq!(serialized["usage"]["rolling"]["futureWindowField"], true);
        server.abort();
    }

    #[tokio::test]
    async fn usage_preserves_retry_after_on_rate_limit() {
        let app = Router::new().route(
            "/v1/usage",
            get(|| async {
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    [(http::header::RETRY_AFTER, "17")],
                    Json(serde_json::json!({"error":{"message":"try later test-key"}})),
                )
                    .into_response()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client =
            OpenCodeClient::new(format!("http://{address}/v1"), Some("test-key".to_string()))
                .unwrap();

        let error = client.get_usage().await.unwrap_err();
        assert_eq!(error.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(error.retry_after.as_deref(), Some("17"));
        assert_eq!(error.message, "try later [redacted]");
        server.abort();
    }

    #[tokio::test]
    async fn usage_requires_an_api_key() {
        let client = OpenCodeClient::new("https://example.com/v1".to_string(), None).unwrap();
        let error = client.get_usage().await.unwrap_err();
        assert_eq!(error.status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            error.message,
            "OpenCode Go has no API key. Run `cc-proxy auth login opencode` to add one."
        );
    }

    async fn serve_capture(seen: Seen) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let app = Router::new()
            .route("/v1/messages", post(capture_request))
            .route("/v1/chat/completions", post(capture_request))
            .with_state(seen);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (address, server)
    }

    #[tokio::test]
    async fn messages_send_claude_codes_betas_without_the_oauth_one() {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let (address, server) = serve_capture(seen.clone()).await;
        let client =
            OpenCodeClient::new(format!("http://{address}"), Some("test-key".to_string())).unwrap();
        let body = serde_json::json!({"model": "claude-opus-5-5", "messages": []});
        let claude_code = "oauth-2025-04-20, context-management-2025-06-27,advisor-tool-2026-03-01";
        for (endpoint, betas) in [
            (EndpointKind::Messages, Some(claude_code)),
            (EndpointKind::Messages, Some("oauth-2025-04-20")),
            (EndpointKind::ChatCompletions, Some(claude_code)),
        ] {
            client
                .post_for_provider(
                    endpoint,
                    &body,
                    false,
                    None,
                    None,
                    "anthropic",
                    "Anthropic",
                    true,
                    betas,
                )
                .await
                .unwrap()
                .into_bytes()
                .await
                .unwrap();
        }
        server.abort();

        let seen = seen.lock().unwrap();
        assert_eq!(
            seen[0].anthropic_beta,
            "context-management-2025-06-27,advisor-tool-2026-03-01"
        );
        // With the OAuth beta gone nothing is left, so no header goes out.
        assert!(seen[1].anthropic_beta.is_empty());
        // Betas only switch on fields of an Anthropic Messages body.
        assert!(seen[2].anthropic_beta.is_empty());
    }

    #[tokio::test]
    async fn a_key_saved_while_the_proxy_runs_is_used_on_the_next_request() {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let (address, server) = serve_capture(seen.clone()).await;
        let saved: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let source = saved.clone();
        let client = OpenCodeClient::with_key_source(format!("http://{address}"), move || {
            source.lock().unwrap().clone()
        })
        .unwrap();
        let body = serde_json::json!({"model": "claude-opus-5-5", "messages": []});
        let send = || {
            client.post_for_provider(
                EndpointKind::Messages,
                &body,
                false,
                None,
                None,
                "anthropic",
                "Anthropic",
                true,
                None,
            )
        };

        let Err(error) = send().await else {
            panic!("a request went out without a key");
        };
        assert_eq!(error.status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            error.message,
            "Anthropic has no API key. Run `cc-proxy auth login anthropic` to add one."
        );

        *saved.lock().unwrap() = Some("saved-later".to_string());
        send().await.unwrap().into_bytes().await.unwrap();
        server.abort();
        assert_eq!(seen.lock().unwrap()[0].x_api_key, "saved-later");
    }

    #[tokio::test]
    async fn a_request_can_carry_the_token_of_the_account_it_selected() {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let (address, server) = serve_capture(seen.clone()).await;
        let client =
            OpenCodeClient::new(format!("http://{address}"), Some("original".to_string())).unwrap();
        client
            .with_key(Some("account-token".to_string()))
            .post(
                EndpointKind::Messages,
                &serde_json::json!({"model": "claude-opus-5-5", "messages": []}),
                false,
                None,
                None,
            )
            .await
            .unwrap()
            .into_bytes()
            .await
            .unwrap();
        server.abort();

        assert_eq!(seen.lock().unwrap()[0].x_api_key, "account-token");
    }

    #[tokio::test]
    async fn invalid_usage_response_is_rejected() {
        let app = Router::new().route("/v1/usage", get(|| async { "not json" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client =
            OpenCodeClient::new(format!("http://{address}/v1"), Some("test-key".to_string()))
                .unwrap();

        let error = client.get_usage().await.unwrap_err();
        assert_eq!(error.status, StatusCode::BAD_GATEWAY);
        assert_eq!(error.message, "OpenCode Go usage response was invalid");
        server.abort();
    }

    #[tokio::test]
    async fn usage_response_without_recognized_window_data_is_rejected() {
        let app = Router::new().route(
            "/v1/usage",
            get(|| async { Json(serde_json::json!({"usage": {}})) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client =
            OpenCodeClient::new(format!("http://{address}/v1"), Some("test-key".to_string()))
                .unwrap();

        let error = client.get_usage().await.unwrap_err();
        assert_eq!(error.status, StatusCode::BAD_GATEWAY);
        assert_eq!(
            error.message,
            "OpenCode Go usage response contained no recognized windows"
        );
        server.abort();
    }

    #[tokio::test]
    async fn endpoints_use_protocol_native_auth_and_wire_model_ids() {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/v1/chat/completions", post(capture_request))
            .route("/v1/messages", post(capture_request))
            .route("/v1/responses", post(capture_request))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client =
            OpenCodeClient::new(format!("http://{address}/v1"), Some("test-key".to_string()))
                .unwrap();
        for (endpoint, model, session_id) in [
            (EndpointKind::ChatCompletions, "glm-5.2", Some("sess-abc")),
            (EndpointKind::Messages, "minimax-m3", None),
            (EndpointKind::Responses, "gpt-5.6-luna", Some("sess-xyz")),
        ] {
            client
                .post(
                    endpoint,
                    &serde_json::json!({"model": model, "messages": []}),
                    false,
                    None,
                    session_id,
                )
                .await
                .unwrap()
                .into_bytes()
                .await
                .unwrap();
        }
        server.abort();

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0].path, "/v1/chat/completions");
        assert_eq!(seen[1].path, "/v1/messages");
        assert_eq!(seen[2].path, "/v1/responses");
        assert_eq!(seen[0].authorization, "Bearer test-key");
        assert!(seen[0].x_api_key.is_empty());
        assert!(seen[1].authorization.is_empty());
        assert_eq!(seen[1].x_api_key, "test-key");
        assert_eq!(seen[2].authorization, "Bearer test-key");
        assert!(seen[2].x_api_key.is_empty());
        assert!(seen[0].anthropic_version.is_empty());
        assert_eq!(seen[1].anthropic_version, "2023-06-01");
        assert!(seen[2].anthropic_version.is_empty());
        assert_eq!(seen[0].body["model"], "glm-5.2");
        assert_eq!(seen[1].body["model"], "minimax-m3");
        assert_eq!(seen[2].body["model"], "gpt-5.6-luna");
        assert_eq!(seen[0].x_opencode_session, "sess-abc");
        assert!(!seen[1].x_opencode_session.is_empty());
        assert_ne!(seen[1].x_opencode_session, "sess-abc");
        assert_eq!(seen[2].x_opencode_session, "sess-xyz");
    }
}
