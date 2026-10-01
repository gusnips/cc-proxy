use std::time::{SystemTime, UNIX_EPOCH};

use crate::providers::kimi::auth::constants::api_base_url;
use crate::providers::kimi::auth::headers::common_headers;
use crate::providers::kimi::auth::manager::KimiAuthManager;
use crate::providers::kimi::auth::token_store::{StoredAuth, file_store};
use crate::providers::kimi::translate::request::KimiChatRequest;
use crate::providers::upstream_error::{self, FailureKind};
use crate::retry::{MAX_RATE_LIMIT_RETRIES, compute_backoff_delay};

#[derive(Debug)]
pub struct KimiError {
    pub status: u16,
    pub detail: Option<String>,
    pub retry_after: Option<String>,
}

pub struct KimiResponse {
    pub body: Vec<u8>,
    pub status: u16,
    pub request_start_time: u64,
}

pub struct KimiHttpClient {
    client: reqwest::blocking::Client,
    auth_manager: KimiAuthManager<crate::auth::FileAuthStore<StoredAuth>>,
}

impl Default for KimiHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl KimiHttpClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .expect("failed to create HTTP client"),
            auth_manager: KimiAuthManager::new(file_store()),
        }
    }

    pub fn auth_manager(&self) -> &KimiAuthManager<crate::auth::FileAuthStore<StoredAuth>> {
        &self.auth_manager
    }

    pub fn post_kimi(&self, body: &KimiChatRequest) -> Result<KimiResponse, KimiError> {
        let mut auth = self.auth_manager.get_auth().map_err(|e| KimiError {
            status: 401,
            detail: Some(e.to_string()),
            retry_after: None,
        })?;

        let mut attempt = 0u32;
        loop {
            let result = self.attempt_post(&auth.access, body);

            match result {
                Ok(response) if response.status == 401 && attempt == 0 => {
                    // First 401: try refresh
                    match self.auth_manager.force_refresh() {
                        Ok(new_auth) => {
                            auth = new_auth;
                            attempt += 1;
                            continue;
                        }
                        Err(e) => {
                            return Err(KimiError {
                                status: 401,
                                detail: Some(e.to_string()),
                                retry_after: None,
                            });
                        }
                    }
                }
                Ok(response) => return Ok(response),
                Err(err @ KimiError { status: 429, .. }) => {
                    let Some(wait_ms) = rate_limit_retry_wait(attempt, &err) else {
                        return Err(err);
                    };
                    std::thread::sleep(std::time::Duration::from_millis(wait_ms));
                    attempt += 1;
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
    }

    fn attempt_post(
        &self,
        access_token: &str,
        body: &KimiChatRequest,
    ) -> Result<KimiResponse, KimiError> {
        let headers = common_headers().map_err(|e| KimiError {
            status: 500,
            detail: Some(e.to_string()),
            retry_after: None,
        })?;

        let url = format!("{}/chat/completions", api_base_url());
        let body_json = serde_json::to_string(body).map_err(|e| KimiError {
            status: 500,
            detail: Some(e.to_string()),
            retry_after: None,
        })?;

        let request_start_time = now_ms();

        let mut req_builder = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("Authorization", format!("Bearer {access_token}"));

        // Add common headers
        for (k, v) in &headers {
            if let Ok(name) = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                && let Ok(value) = reqwest::header::HeaderValue::from_str(v)
            {
                req_builder = req_builder.header(name, value);
            }
        }

        let resp = match req_builder.body(body_json).send() {
            Ok(r) => r,
            Err(e) => {
                return Err(KimiError {
                    status: 0,
                    detail: Some(e.to_string()),
                    retry_after: None,
                });
            }
        };

        let status = resp.status().as_u16();

        if status == 429 {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            let text = resp.text().unwrap_or_default();
            return Err(KimiError {
                status: 429,
                detail: if text.is_empty() { None } else { Some(text) },
                retry_after,
            });
        }

        if status == 401 || status == 403 {
            let text = resp.text().unwrap_or_default();
            return Err(KimiError {
                status,
                detail: if text.is_empty() { None } else { Some(text) },
                retry_after: None,
            });
        }

        if !resp.status().is_success() {
            let text = resp.text().unwrap_or_default();
            return Err(KimiError {
                status,
                detail: if text.is_empty() { None } else { Some(text) },
                retry_after: None,
            });
        }

        let body_bytes = resp.bytes().map(|b| b.to_vec()).unwrap_or_default();

        Ok(KimiResponse {
            body: body_bytes,
            status,
            request_start_time,
        })
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
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
