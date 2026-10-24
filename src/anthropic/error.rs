use axum::{
    Json,
    http::{HeaderMap, HeaderValue, StatusCode, header::HeaderName},
    response::{IntoResponse, Response},
};
use serde::Serialize;

/// The headers Claude Code reads to tell a spent usage window from a short
/// throttle. Without them it answers a 429 with "Server is temporarily
/// limiting requests (not your usage limit)" and retries on a backoff; with
/// them it names the limit, counts down, and picks the turn back up when the
/// window reopens.
pub fn usage_limit_headers(until_ms: Option<u64>, claim: Option<&str>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("x-should-retry"),
        HeaderValue::from_static("false"),
    );
    headers.insert(
        HeaderName::from_static("anthropic-ratelimit-unified-status"),
        HeaderValue::from_static("rejected"),
    );
    if let Some(seconds) = until_ms.map(|until| until / 1_000)
        && let Ok(value) = HeaderValue::from_str(&seconds.to_string())
    {
        headers.insert(
            HeaderName::from_static("anthropic-ratelimit-unified-reset"),
            value,
        );
    }
    match claim {
        Some(claim) => {
            if let Ok(value) = HeaderValue::from_str(claim) {
                headers.insert(
                    HeaderName::from_static("anthropic-ratelimit-unified-representative-claim"),
                    value,
                );
            }
        }
        // Without a claim Claude Code takes the 429 for a server throttle and
        // shows no reset time. A rejected extra usage status makes it say
        // "You've hit your limit" without naming a window we don't know.
        None => {
            headers.insert(
                HeaderName::from_static("anthropic-ratelimit-unified-overage-status"),
                HeaderValue::from_static("rejected"),
            );
        }
    }
    headers
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorEnvelope {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub error: ErrorDetail,
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorDetail {
    #[serde(rename = "type")]
    pub kind: String,
    pub message: String,
}

pub fn json_error(
    status: StatusCode,
    kind: impl Into<String>,
    message: impl Into<String>,
) -> Response {
    (
        status,
        Json(ErrorEnvelope {
            kind: "error",
            error: ErrorDetail {
                kind: kind.into(),
                message: message.into(),
            },
        }),
    )
        .into_response()
}
