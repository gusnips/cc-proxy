//! One classifier for an upstream failure, shared by every provider that does
//! not have its own.
//!
//! Providers file the same root cause under whatever status they like: a spent
//! balance arrives as 429, 402, 403 or 400, and a prompt that outgrew the
//! context window as 400, 413 or 429. Status alone gives Claude Code the wrong
//! advice. A context overflow mapped to a 5xx is retried until the retries run
//! out, and the conversation is never compacted. A spent balance sent back as a
//! throttle is retried against an empty account. So for the 4xx family the body
//! is read before the status, and within it context outranks entitlement,
//! entitlement outranks quota, and quota outranks auth: each earlier fix is
//! useless for the later ones.
//!
//! The pattern lists are ported from providerkit's `errors.ts`, which merged
//! five production classifiers. Keep them in step rather than inventing new
//! wordings here.

use std::sync::LazyLock;

use http::StatusCode;
use regex_lite::Regex;
use serde_json::Value;

use crate::provider::{ProviderError, ProviderErrorKind};

/// What failed, named by what fixes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// The prompt outgrew the context window. Compaction fixes it; a retry
    /// sends the same oversized prompt again.
    Context,
    /// The plan never included this API. Neither a retry nor a new key fixes it.
    Entitlement,
    /// The balance or usage window is spent. Waiting minutes does not fix it.
    Quota,
    /// The key or token is wrong, not the request.
    Auth,
    /// A 402 or 403 that names nothing more specific.
    Permission,
    /// A short throttle. Waiting fixes it.
    RateLimit,
    /// The upstream said it is overloaded. Retrying fixes it.
    Overloaded,
    /// Any other upstream fault: a 5xx, a timeout. Retrying usually fixes it.
    Server,
    /// Anything else in the 4xx family: something in the request itself.
    Invalid,
}

/// A failure that arrived as an HTTP status and a body. Status 0 is the
/// clients' spelling of "no response at all": the socket, DNS, a timeout.
pub fn from_http(
    status: u16,
    body: &str,
    retry_after: Option<&str>,
    provider: &str,
) -> ProviderError {
    let kind = if status == 0 {
        FailureKind::Server
    } else {
        classify(Some(status), body)
    };
    let message = upstream_message(body).unwrap_or_else(|| match status {
        0 => format!("{provider} upstream request failed"),
        _ => format!("{provider} upstream returned HTTP {status}"),
    });
    provider_error(kind, Some(status), message, retry_after)
}

/// A failure the upstream reported inside a stream that had already answered
/// 200. Without this the payload reads as a generic broken stream, and a
/// throttle or a spent balance loses the kind that says what to do about it.
pub fn from_stream(error: &Value, provider: &str) -> ProviderError {
    let status = error
        .get("code")
        .or_else(|| error.get("status"))
        .and_then(Value::as_u64)
        .and_then(|code| u16::try_from(code).ok())
        .filter(|code| (400..600).contains(code))
        .or_else(|| {
            error
                .get("type")
                .or_else(|| error.get("code"))
                .and_then(Value::as_str)
                .and_then(status_for_error_type)
        });
    let body = error.to_string();
    let message = upstream_message(&body)
        .or_else(|| error.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{provider} stream failed"));
    let retry_after = error
        .get("retry_after")
        .or_else(|| error.get("retry_after_seconds"))
        .and_then(|value| match value {
            Value::Number(number) => Some(number.to_string()),
            Value::String(text) => Some(text.clone()),
            _ => None,
        });
    provider_error(
        classify(status, &body),
        status,
        message,
        retry_after.as_deref(),
    )
}

fn provider_error(
    kind: FailureKind,
    upstream_status: Option<u16>,
    message: String,
    retry_after: Option<&str>,
) -> ProviderError {
    let upstream_status = upstream_status.and_then(|status| StatusCode::from_u16(status).ok());
    let (status, error_kind) = match kind {
        FailureKind::Context => (StatusCode::BAD_REQUEST, ProviderErrorKind::InvalidRequest),
        FailureKind::Entitlement => (StatusCode::FORBIDDEN, ProviderErrorKind::Permission),
        FailureKind::Quota | FailureKind::RateLimit => {
            (StatusCode::TOO_MANY_REQUESTS, ProviderErrorKind::RateLimit)
        }
        FailureKind::Auth => (StatusCode::UNAUTHORIZED, ProviderErrorKind::Authentication),
        FailureKind::Permission => (
            upstream_status
                .filter(|status| matches!(status.as_u16(), 402 | 403))
                .unwrap_or(StatusCode::FORBIDDEN),
            ProviderErrorKind::Permission,
        ),
        FailureKind::Overloaded => (
            StatusCode::from_u16(529).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
            ProviderErrorKind::Overloaded,
        ),
        FailureKind::Server => (StatusCode::BAD_GATEWAY, ProviderErrorKind::Api),
        FailureKind::Invalid => (
            upstream_status
                .filter(StatusCode::is_client_error)
                .unwrap_or(StatusCode::BAD_REQUEST),
            ProviderErrorKind::InvalidRequest,
        ),
    };
    // Claude Code starts compaction when a 400 says "prompt is too long".
    // Upstreams word the same overflow a dozen other ways, none of which it
    // recognizes, so the turn fails without the one fix that works.
    let message = if kind == FailureKind::Context
        && !message.to_ascii_lowercase().contains("prompt is too long")
    {
        format!("prompt is too long: {message}")
    } else {
        message
    };
    let mut error = ProviderError::new(status, error_kind, message);
    // The upstream's own wait, never an invented one: a made-up wait on a
    // spent balance turns one failure into a retry loop.
    error.retry_after = retry_after
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    // A spent balance does not refill on a backoff. `x-should-retry: false`
    // stops Claude Code's retry loop, the same signal the Codex quota wall
    // sends.
    if kind == FailureKind::Quota {
        error.should_retry = Some(false);
    }
    error
}

/// The classified upstream failure a stream translator stopped on, if that is
/// what it failed on rather than a broken stream.
pub fn carried(error: &anyhow::Error) -> Option<&ProviderError> {
    error.downcast_ref::<ProviderError>()
}

/// The error for a buffered response whose translation failed: the upstream's
/// own failure when the stream carried one, a 502 when the stream itself was
/// broken. Nothing has reached the client yet, so the real status still can.
pub fn translation_error(error: anyhow::Error, context: &str) -> ProviderError {
    error.downcast::<ProviderError>().unwrap_or_else(|error| {
        ProviderError::new(
            StatusCode::BAD_GATEWAY,
            ProviderErrorKind::Api,
            format!("{context}: {error}"),
        )
    })
}

/// The HTTP status an error `type` stands for, when an in-band error names a
/// type and no status. Anthropic's types first, then the OpenAI ones.
pub fn status_for_error_type(error_type: &str) -> Option<u16> {
    Some(match error_type {
        "invalid_request_error" => 400,
        "authentication_error" => 401,
        "permission_error" => 403,
        "not_found_error" => 404,
        "request_too_large" => 413,
        "rate_limit_error" => 429,
        "api_error" | "server_error" | "internal_server_error" => 500,
        "overloaded_error" => 529,
        _ => return None,
    })
}

/// The reason an upstream gave, from the JSON shapes providers use, or the raw
/// text when the body is not JSON.
fn upstream_message(body: &str) -> Option<String> {
    let body = body.trim();
    let message = match serde_json::from_str::<Value>(body) {
        Ok(value) => ["/error/message", "/message", "/error", "/msg", "/detail"]
            .iter()
            .find_map(|pointer| value.pointer(pointer).and_then(Value::as_str))
            .map(str::to_string),
        Err(_) => Some(body.chars().take(2_000).collect()),
    };
    message.filter(|message| !message.trim().is_empty())
}

/// The kind of failure, from the status (when there is one) and the body text.
pub fn classify(status: Option<u16>, text: &str) -> FailureKind {
    let client_error = status.is_none_or(|status| status < 500);
    if client_error && CONTEXT.is_match(text) {
        return FailureKind::Context;
    }
    if client_error && ENTITLEMENT.is_match(text) {
        return FailureKind::Entitlement;
    }
    if client_error && QUOTA.is_match(text) {
        return FailureKind::Quota;
    }
    // A missing model arrives as a 400, a 404, or in-band. It is a request
    // problem either way, and retrying it fails identically.
    if MODEL.is_match(text) {
        return FailureKind::Invalid;
    }
    if status == Some(401) || AUTH.is_match(text) {
        return FailureKind::Auth;
    }
    match status {
        Some(402 | 403) => return FailureKind::Permission,
        Some(404) => return FailureKind::Invalid,
        Some(429) => return FailureKind::RateLimit,
        _ => {}
    }
    if CONTENT.is_match(text) {
        return FailureKind::Invalid;
    }
    match status {
        Some(529) => return FailureKind::Overloaded,
        Some(status) if status >= 500 => {
            return if OVERLOAD.is_match(text) {
                FailureKind::Overloaded
            } else {
                FailureKind::Server
            };
        }
        // 408 is a timeout, 409 is how several gateways say the model is still
        // loading, and 425 asks for a replay. All three are worth another try.
        Some(408 | 409 | 425) => return FailureKind::Server,
        _ => {}
    }
    if RATE.is_match(text) {
        return FailureKind::RateLimit;
    }
    if OVERLOAD.is_match(text) {
        return FailureKind::Overloaded;
    }
    match status {
        Some(_) => FailureKind::Invalid,
        // An in-band error with nothing recognizable in it: a stream that dies
        // after its headers is a transient upstream fault, and calling it a
        // request problem would stop the retry that fixes it.
        None => FailureKind::Overloaded,
    }
}

fn pattern(source: &str) -> Regex {
    Regex::new(source).expect("upstream error pattern compiles")
}

// The request outgrew the model's window. Nothing about the account is wrong
// and waiting fixes nothing: the fix is to send less, which is compaction.
static CONTEXT: LazyLock<Regex> = LazyLock::new(|| {
    pattern(concat!(
        r"(?i)context[_\s]length[_\s]exceeded",
        r"|maximum context length",
        r"|context window",
        r"|prompt is too long",
        r"|input is too long",
        r"|too many (?:input )?tokens",
        r"|reduce the length of the (?:messages|prompt|input)",
        r"|input length and `?max_tokens`? exceed context limit",
        r"|exceed(?:s|ed)? (?:the )?context limit",
        // Scoped to what overflowed: a bare "exceeds the maximum" also covers
        // image counts and per-minute token rates, which compaction can't fix.
        r"|exceeds? the (?:model'?s? )?maximum (?:input |prompt |context )?(?:tokens?|length|context)",
    ))
});

// A plan that never included this API. Its wording overlaps quota ("plan"
// appears in both), so it is checked first.
static ENTITLEMENT: LazyLock<Regex> = LazyLock::new(|| {
    pattern(concat!(
        r"(?i)plan does(?:n't| not) (?:include|support)",
        r"|not included (?:in|with) your .{0,40}plan",
        r"|upgrade to [\w ]{1,30}(?:or higher|plan)",
        r"|requires? (?:the |a |an )?[\w\s]{1,30}plan",
        r"|requires? (?:a )?subscription",
        r"|subscription[_\s]?required",
        r"|upgrade for access",
        r"|no api access",
        r"|OAuth authentication is currently not allowed for this organization",
        r"|usage[_\s]not[_\s]included",
    ))
});

// Balance or usage window used up. Chinese-market providers report it in
// Chinese, which is why the literal strings are here.
static QUOTA: LazyLock<Regex> = LazyLock::new(|| {
    pattern(concat!(
        r"(?i)insufficient[_\s]quota",
        r"|exceeded your current quota",
        r"|insufficient (?:balance|credits?)",
        r"|credit balance is too low",
        r"|(?:no|any|out of) credits",
        r"|(?:credits?|balance) depleted",
        r"|usage limits? (?:reached|exceeded|hit)",
        r"|reached your (?:usage|weekly|monthly|daily) limit",
        r"|(?:weekly|monthly|daily|plan) usage limit",
        r"|purchase extra usage",
        r"|upgrade your plan",
        r"|quota\b[^.]{0,40}\b(?:exhausted|exceeded|will be refreshed)",
        r"|balance (?:is )?(?:too low|not enough|insufficient)",
        r"|billing_error",
        r"|\barrears?\b",
        r"|余额不足",
        r"|欠费",
        r"|额度(?:不足|已用完)",
    ))
});

static MODEL: LazyLock<Regex> = LazyLock::new(|| {
    pattern(concat!(
        r"(?i)(?:model|models/)[^.{\n]{0,40}(?:not found|does not exist|unknown)",
        r"|no model named",
        r"|unsupported model",
        r"|supported API model names are",
        r"|unknown model",
        r"|is not a valid model ID",
    ))
});

static AUTH: LazyLock<Regex> = LazyLock::new(|| {
    pattern(concat!(
        r"(?i)invalid[_\s]?(?:x-api-key|api[ _-]?key|token|credentials?)",
        r"|api[ _-]?key (?:is )?(?:not valid|invalid|incorrect)",
        r"|unauthorized|unauthenticated|permission_denied",
        r"|authentication[_\s](?:failed|invalid|error)",
        r"|account (?:disabled|suspended|deactivated|banned)",
        r"|unrecognizedclient|unauthorizedexception|accessdeniedexception",
        r"|expired[_\s]?token",
        r"|OAuth token has been revoked",
    ))
});

static CONTENT: LazyLock<Regex> = LazyLock::new(|| {
    pattern(concat!(
        r"(?i)content[_\s]filter",
        r"|content policy",
        r"|cyber[_\s]policy",
        r"|safety|prohibited_content|blocked|refusal|policy violation|misalignment",
        r"|maximum of \d+ PDF pages",
        r"|PDF specified is password protected",
        r"|images? exceeds? .* maximum",
        r"|image dimensions exceed",
    ))
});

// A throttle said in words. An in-band failure has no 429 to read, so this is
// the only evidence it leaves.
static RATE: LazyLock<Regex> = LazyLock::new(|| {
    pattern(concat!(
        r"(?i)rate[_\s-]?limit",
        r"|too[_\s-]?many[_\s-]?requests",
        // Google's per-minute quota code: a throttle, not a spent balance.
        r"|resource[_\s]?exhausted",
        r"|throttl(?:e|ed|ing)",
        r"|(?:requests?|tokens?) per (?:second|sec|minute|min)\b",
    ))
});

static OVERLOAD: LazyLock<Regex> = LazyLock::new(|| {
    pattern(concat!(
        r"(?i)overloaded",
        r"|\bunavailable\b",
        r"|internal error",
        r"|\bcapacity\b",
        r"|server is busy",
        r"|high demand",
        r#"|"code"\s*:\s*5\d\d"#,
        r"|(?:try|retry) your request again",
        // Google's status enum is upper-case by contract. Matched exactly so it
        // does not swallow the word in ordinary prose.
        r"|(?-i:\bINTERNAL\b)",
    ))
});

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn body_outranks_status_for_the_4xx_family() {
        let cases = [
            (
                400,
                r#"{"error":{"message":"This model's maximum context length is 262144 tokens"}}"#,
                FailureKind::Context,
            ),
            (
                429,
                r#"{"error":{"message":"Your prompt is too long, too many input tokens"}}"#,
                FailureKind::Context,
            ),
            (
                429,
                r#"{"error":{"message":"Your account balance is insufficient (余额不足)"}}"#,
                FailureKind::Quota,
            ),
            (
                429,
                r#"{"error":{"message":"套餐额度已用完"}}"#,
                FailureKind::Quota,
            ),
            (
                402,
                r#"{"error":{"type":"insufficient_quota"}}"#,
                FailureKind::Quota,
            ),
            (
                403,
                r#"{"error":{"message":"Your plan does not include API access"}}"#,
                FailureKind::Entitlement,
            ),
            (
                403,
                r#"{"error":{"message":"denied"}}"#,
                FailureKind::Permission,
            ),
            (401, "", FailureKind::Auth),
            (
                429,
                r#"{"error":{"message":"slow down"}}"#,
                FailureKind::RateLimit,
            ),
            (
                400,
                r#"{"error":{"message":"bad tool schema"}}"#,
                FailureKind::Invalid,
            ),
            (500, r#"{"error":{"message":"boom"}}"#, FailureKind::Server),
            (
                500,
                r#"{"error":{"status":"INTERNAL"}}"#,
                FailureKind::Overloaded,
            ),
            (
                500,
                r#"{"error":{"message":"an internal fault"}}"#,
                FailureKind::Server,
            ),
            (
                503,
                r#"{"error":{"message":"Model overloaded"}}"#,
                FailureKind::Overloaded,
            ),
            (529, "", FailureKind::Overloaded),
        ];
        for (status, body, expected) in cases {
            assert_eq!(classify(Some(status), body), expected, "{status} {body}");
        }
    }

    // Swap two checks and a spent balance is reported as a context overflow,
    // or an overflow as a bad key. The fix for each earlier kind is useless for
    // the later ones.
    #[test]
    fn context_outranks_entitlement_outranks_quota_outranks_auth() {
        let all =
            "prompt is too long; plan does not include this; insufficient_quota; invalid api key";
        assert_eq!(classify(Some(400), all), FailureKind::Context);
        let no_context = "plan does not include this; insufficient_quota; invalid api key";
        assert_eq!(classify(Some(400), no_context), FailureKind::Entitlement);
        assert_eq!(
            classify(Some(400), "insufficient_quota; invalid api key"),
            FailureKind::Quota
        );
        assert_eq!(classify(Some(400), "invalid api key"), FailureKind::Auth);
    }

    #[test]
    fn context_overflow_reaches_claude_code_as_a_compactable_400() {
        let failure = from_http(
            400,
            r#"{"error":{"message":"Input length exceeds the model's maximum context length"}}"#,
            None,
            "Kimi",
        );
        assert_eq!(failure.status, StatusCode::BAD_REQUEST);
        assert_eq!(failure.kind, ProviderErrorKind::InvalidRequest);
        assert!(failure.message.starts_with("prompt is too long: "));
    }

    #[test]
    fn quota_never_invents_a_wait_and_stops_the_retry_loop() {
        let failure = from_http(
            429,
            r#"{"error":{"message":"insufficient balance"}}"#,
            None,
            "Kimi",
        );
        let response = failure.response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(response.headers().get(http::header::RETRY_AFTER).is_none());
        assert_eq!(response.headers()["x-should-retry"], "false");

        let throttle = from_http(429, "", Some("12"), "GLM").response();
        assert_eq!(throttle.headers()[http::header::RETRY_AFTER], "12");
        assert!(throttle.headers().get("x-should-retry").is_none());
    }

    #[test]
    fn in_band_errors_keep_their_kind() {
        let quota = from_stream(
            &json!({"type":"rate_limit_error","message":"insufficient balance"}),
            "Kimi",
        );
        assert_eq!(quota.should_retry, Some(false));
        let throttle = from_stream(
            &json!({"type":"rate_limit_error","message":"slow down"}),
            "Kimi",
        );
        assert_eq!(throttle.kind, ProviderErrorKind::RateLimit);
        assert_eq!(throttle.should_retry, None);
        let invalid = from_stream(
            &json!({"type":"invalid_request_error","message":"bad schema"}),
            "OpenCode Go",
        );
        assert_eq!(invalid.status, StatusCode::BAD_REQUEST);
        let unknown = from_stream(&json!({"message":"stream reset"}), "Grok");
        assert_eq!(unknown.error_type(), "overloaded_error");
    }
}
