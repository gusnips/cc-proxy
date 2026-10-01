use std::sync::Arc;
use std::time::Duration;

use crate::auth::FileAuthStore;
use crate::providers::kimi::auth::constants::api_base_url;
use crate::providers::kimi::auth::headers::common_headers;
use crate::providers::kimi::auth::manager::KimiAuthManager;
use crate::providers::kimi::auth::token_store::{StoredAuth, file_store};
use crate::providers::kimi::translate::request::KimiChatRequest;
use crate::providers::upstream_error::{self, FailureKind};
use crate::retry::{self, MAX_RATE_LIMIT_RETRIES, compute_backoff_delay};

type AuthManager = KimiAuthManager<FileAuthStore<StoredAuth>>;

#[derive(Debug)]
pub struct KimiError {
    pub status: u16,
    pub detail: Option<String>,
    pub retry_after: Option<String>,
}

impl KimiError {
    pub(super) fn new(status: u16, detail: impl ToString) -> Self {
        Self {
            status,
            detail: Some(detail.to_string()),
            retry_after: None,
        }
    }
}

pub struct KimiHttpClient {
    client: reqwest::Client,
    auth_manager: Arc<AuthManager>,
}

impl Default for KimiHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl KimiHttpClient {
    pub fn new() -> Self {
        Self {
            // An idle bound, not a total one: a streamed answer can take
            // minutes to write and a total timeout cuts it off. 120s with no
            // byte at all still fails a hung upstream.
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .read_timeout(Duration::from_secs(120))
                .build()
                .expect("failed to create HTTP client"),
            auth_manager: Arc::new(KimiAuthManager::new(file_store())),
        }
    }

    /// Sends the request and returns the response once Kimi has accepted it
    /// with a 2xx, so the caller can read the body as it streams.
    pub async fn post_kimi(&self, body: &KimiChatRequest) -> Result<reqwest::Response, KimiError> {
        let mut auth = self.auth(AuthManager::get_auth).await?;
        let mut refreshed = false;
        let mut attempt = 0u32;
        loop {
            match self.attempt_post(&auth.access, body).await {
                // A token can be rejected before its expiry, for example after
                // a peer sharing the login (the Kimi CLI) refreshed it. One
                // refresh and replay recovers that; a second 401 is a real one.
                Err(KimiError { status: 401, .. }) if !refreshed => {
                    auth = self.auth(AuthManager::force_refresh).await?;
                    refreshed = true;
                }
                Err(err @ KimiError { status: 429, .. }) => {
                    let Some(wait_ms) = rate_limit_retry_wait(attempt, &err) else {
                        return Err(err);
                    };
                    retry::sleep(wait_ms).await;
                    attempt += 1;
                }
                result => return result,
            }
        }
    }

    /// The auth manager reads the token file and refreshes over a blocking
    /// client, so it runs on a blocking thread rather than stalling the
    /// runtime that carries every other stream.
    async fn auth(
        &self,
        load: fn(&AuthManager) -> anyhow::Result<StoredAuth>,
    ) -> Result<StoredAuth, KimiError> {
        let manager = self.auth_manager.clone();
        match tokio::task::spawn_blocking(move || load(&manager)).await {
            Ok(Ok(auth)) => Ok(auth),
            Ok(Err(error)) => Err(KimiError::new(401, error)),
            Err(error) => Err(KimiError::new(500, error)),
        }
    }

    async fn attempt_post(
        &self,
        access_token: &str,
        body: &KimiChatRequest,
    ) -> Result<reqwest::Response, KimiError> {
        let headers = common_headers().map_err(|e| KimiError::new(500, e))?;
        let mut request = self
            .client
            .post(format!("{}/chat/completions", api_base_url()))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("Authorization", format!("Bearer {access_token}"))
            .json(body);
        for (k, v) in &headers {
            if let Ok(name) = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                && let Ok(value) = reqwest::header::HeaderValue::from_str(v)
            {
                request = request.header(name, value);
            }
        }

        let response = request.send().await.map_err(|e| KimiError::new(0, e))?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let text = response.text().await.unwrap_or_default();
        Err(KimiError {
            status,
            detail: (!text.is_empty()).then_some(text),
            retry_after,
        })
    }
}

/// How long to wait before retrying a 429, or None when a retry cannot help.
/// A server that asks for longer than the retry budget was retried after 30s
/// anyway, into the same 429; and a spent balance or plan quota was retried
/// three times, which no wait inside one request clears. Both now return the
/// 429 so the caller sees the real wait.
fn rate_limit_retry_wait(attempt: u32, err: &KimiError) -> Option<u64> {
    if attempt >= MAX_RATE_LIMIT_RETRIES {
        return None;
    }
    let body = err.detail.as_deref().unwrap_or_default();
    if upstream_error::classify(Some(err.status), body) == FailureKind::Quota {
        return None;
    }
    let delay = compute_backoff_delay(attempt, err.retry_after.as_deref());
    (!delay.exceeds_budget).then_some(delay.wait_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate_limited(detail: &str, retry_after: Option<&str>) -> KimiError {
        KimiError {
            status: 429,
            detail: Some(detail.to_string()),
            retry_after: retry_after.map(str::to_string),
        }
    }

    #[test]
    fn a_429_is_retried_only_when_a_wait_can_clear_it() {
        let throttle = rate_limited(r#"{"error":{"message":"rate limit exceeded"}}"#, Some("2"));
        assert_eq!(rate_limit_retry_wait(0, &throttle), Some(2000));
        assert_eq!(
            rate_limit_retry_wait(MAX_RATE_LIMIT_RETRIES, &throttle),
            None
        );

        let long_wait = rate_limited("rate limit exceeded", Some("600"));
        assert_eq!(rate_limit_retry_wait(0, &long_wait), None);

        let quota = rate_limited(
            r#"{"error":{"message":"Your account balance is insufficient"}}"#,
            None,
        );
        assert_eq!(rate_limit_retry_wait(0, &quota), None);
    }
}
