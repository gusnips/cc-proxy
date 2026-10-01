use std::time::Duration;

use crate::config;

/// How long one read may wait for the next bytes. It bounds silence, not the
/// answer: a stream that keeps sending runs as long as it needs, and one that
/// goes quiet this long fails. The 300s limit on the whole request it replaces
/// cut every longer answer off partway through. A reply that is not streamed
/// sends nothing until it is done, so it still gets 300s in total.
const READ_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug)]
pub struct GlmError {
    pub status: u16,
    pub detail: Option<String>,
    pub retry_after: Option<String>,
}

/// Async HTTP client for the z.ai Anthropic-compatible endpoint.
///
/// z.ai speaks the Anthropic Messages API natively, so the request body is
/// forwarded verbatim (no translation) and the upstream Anthropic SSE/JSON
/// reply is piped straight back to Claude Code.
pub struct GlmHttpClient {
    client: reqwest::Client,
}

impl GlmHttpClient {
    pub fn new() -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(READ_TIMEOUT)
            .build()?;
        Ok(Self { client })
    }

    /// Sends the request and returns the reply once its status is a success.
    /// The body is left unread, so a stream reaches Claude Code as it arrives.
    pub async fn post_messages(
        &self,
        api_key: &str,
        body: &[u8],
    ) -> Result<reqwest::Response, GlmError> {
        let base = config::glm_base_url();
        let url = format!("{}/v1/messages", base.trim_end_matches('/'));

        let resp = self
            .client
            .post(&url)
            .header("authorization", format!("Bearer {api_key}"))
            .header("anthropic-version", "2023-06-01")
            .header("accept", "application/json")
            .header("content-type", "application/json")
            .body(body.to_vec())
            .send()
            .await
            .map_err(|e| GlmError {
                status: 0,
                detail: Some(e.to_string()),
                retry_after: None,
            })?;

        let status = resp.status().as_u16();

        if status == 429 {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            let text = resp.text().await.unwrap_or_default();
            return Err(GlmError {
                status: 429,
                detail: if text.is_empty() { None } else { Some(text) },
                retry_after,
            });
        }

        if status == 401 || status == 403 {
            let text = resp.text().await.unwrap_or_default();
            return Err(GlmError {
                status,
                detail: if text.is_empty() { None } else { Some(text) },
                retry_after: None,
            });
        }

        if !(200..300).contains(&status) {
            let text = resp.text().await.unwrap_or_default();
            return Err(GlmError {
                status,
                detail: if text.is_empty() { None } else { Some(text) },
                retry_after: None,
            });
        }

        Ok(resp)
    }
}
