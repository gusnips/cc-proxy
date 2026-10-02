use std::time::Duration;

use http::StatusCode;

use super::auth::{self, StoredAuth, copilot_headers};
use crate::config;
use crate::provider::{ProviderError, ProviderErrorKind};
use crate::providers::opencode::chat::ChatRequest;
use crate::providers::upstream_error;

const NAME: &str = "GitHub Copilot";

pub struct CopilotClient {
    client: reqwest::Client,
}

impl Default for CopilotClient {
    fn default() -> Self {
        Self::new()
    }
}

impl CopilotClient {
    pub fn new() -> Self {
        Self {
            // An idle bound, not a total one: a streamed answer can take
            // minutes to write. 120s with no byte at all fails a hung upstream.
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .read_timeout(Duration::from_secs(120))
                .build()
                .expect("failed to create HTTP client"),
        }
    }

    /// Sends the request and returns the response once Copilot has accepted it
    /// with a 2xx, so the caller can read the body as it streams.
    pub async fn post_chat(
        &self,
        body: &ChatRequest,
        initiator: &str,
    ) -> Result<reqwest::Response, ProviderError> {
        let mut auth = blocking_auth(auth::current).await?;
        match self.attempt(&auth, body, initiator).await {
            // The token can be rejected before its expiry. One fresh token and
            // one replay recovers that; a second 401 is a real one.
            Err(error) if error.status == StatusCode::UNAUTHORIZED => {
                let rejected = auth.copilot_token;
                auth = blocking_auth(move || auth::renew(&rejected)).await?;
                self.attempt(&auth, body, initiator).await
            }
            result => result,
        }
    }

    async fn attempt(
        &self,
        auth: &StoredAuth,
        body: &ChatRequest,
        initiator: &str,
    ) -> Result<reqwest::Response, ProviderError> {
        let host = config::copilot_base_url().unwrap_or_else(|| auth.host.clone());
        let mut request = self
            .client
            .post(format!("{}/chat/completions", host.trim_end_matches('/')))
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .header(
                "User-Agent",
                concat!("cc-proxy/", env!("CARGO_PKG_VERSION")),
            )
            .bearer_auth(&auth.copilot_token)
            .json(body);
        for (name, value) in copilot_headers(initiator) {
            request = request.header(name, value);
        }
        let response = request
            .send()
            .await
            .map_err(|error| upstream_error::from_http(0, &error.to_string(), None, NAME))?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let text = response.text().await.unwrap_or_default();
        Err(upstream_error::from_http(
            status,
            &text,
            retry_after.as_deref(),
            NAME,
        ))
    }
}

/// The auth code reads files and calls GitHub over a blocking client, so it
/// runs on a blocking thread rather than stalling the runtime that carries
/// every other stream.
async fn blocking_auth(
    load: impl FnOnce() -> anyhow::Result<StoredAuth> + Send + 'static,
) -> Result<StoredAuth, ProviderError> {
    match tokio::task::spawn_blocking(load).await {
        Ok(Ok(auth)) => Ok(auth),
        Ok(Err(error)) => Err(ProviderError::new(
            StatusCode::UNAUTHORIZED,
            ProviderErrorKind::Authentication,
            error.to_string(),
        )),
        Err(error) => Err(ProviderError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            ProviderErrorKind::Api,
            error.to_string(),
        )),
    }
}
