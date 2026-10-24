//! Google RPC details distinguish a short throttle from an exhausted quota.

use serde_json::Value;

use super::FailureKind;
use crate::provider::ProviderError;
use crate::rotation::RotationHint;

fn details(error: &Value) -> impl Iterator<Item = &Value> {
    error
        .get("details")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn is(detail: &Value, name: &str) -> bool {
    detail
        .get("@type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind == format!("type.googleapis.com/google.rpc.{name}"))
}

pub(super) fn kind(error: &Value) -> Option<FailureKind> {
    let mut rate_limited = false;
    for detail in details(error).filter(|detail| is(detail, "ErrorInfo")) {
        match detail["reason"].as_str() {
            Some("QUOTA_EXHAUSTED") => return Some(FailureKind::Quota),
            Some("RATE_LIMIT_EXCEEDED") => rate_limited = true,
            _ => {}
        }
    }
    // Gemini uses the same spent-quota prose for per-minute and per-day limits.
    // The violated quota IDs, not that prose or a short RetryInfo, decide.
    let ids: Vec<_> = details(error)
        .filter(|detail| is(detail, "QuotaFailure"))
        .filter_map(|detail| detail["violations"].as_array())
        .flatten()
        .filter_map(|violation| violation["quotaId"].as_str())
        .map(str::to_ascii_lowercase)
        .collect();
    if ids.iter().any(|id| id.contains("perday")) {
        Some(FailureKind::Quota)
    } else if rate_limited
        || (!ids.is_empty()
            && ids
                .iter()
                .all(|id| id.contains("perminute") || id.contains("persecond")))
    {
        Some(FailureKind::RateLimit)
    } else {
        None
    }
}

fn delay_ms(raw: &str) -> Option<u64> {
    let duration: jiff::SignedDuration = raw.parse().ok()?;
    let nanos = u64::try_from(duration.as_nanos()).ok()?;
    (nanos > 0).then(|| nanos.div_ceil(1_000_000))
}

fn reset_at(error: &Value, now: u64) -> Option<u64> {
    details(error)
        .flat_map(|detail| {
            let retry = is(detail, "RetryInfo")
                .then(|| detail["retryDelay"].as_str())
                .flatten()
                .and_then(delay_ms)
                .and_then(|delay| now.checked_add(delay));
            let metadata = is(detail, "ErrorInfo").then(|| &detail["metadata"]);
            let delay = metadata
                .and_then(|m| m["quotaResetDelay"].as_str())
                .and_then(delay_ms)
                .and_then(|delay| now.checked_add(delay));
            let timestamp = metadata
                .and_then(|m| m["quotaResetTimeStamp"].as_str())
                .and_then(|raw| raw.parse::<jiff::Timestamp>().ok())
                .and_then(|at| u64::try_from(at.as_millisecond()).ok());
            [retry, delay, timestamp].into_iter().flatten()
        })
        .filter(|until| *until > now)
        .max()
}

pub(super) fn normalize(failure: &mut ProviderError, error: &Value, provider: &str, now: u64) {
    if failure.status != http::StatusCode::TOO_MANY_REQUESTS {
        return;
    }
    if provider == "Antigravity" {
        failure.rotation_hint = Some(RotationHint::Stay);
    }
    if error["status"].as_str() != Some("RESOURCE_EXHAUSTED") && details(error).next().is_none() {
        return;
    }
    let until = reset_at(error, now);
    if failure.retry_after.is_none() {
        failure.retry_after = until.map(|until| (until - now).div_ceil(1000).to_string());
    }
    if provider == "Antigravity" {
        // Capacity failures keep their retry guidance but never rotate accounts.
        let quota = details(error).find(|detail| {
            is(detail, "ErrorInfo")
                && detail["domain"].as_str() == Some("cloudcode-pa.googleapis.com")
                && detail["reason"].as_str() == Some("QUOTA_EXHAUSTED")
        });
        let header_until = failure
            .retry_after
            .as_deref()
            .and_then(crate::retry::retry_after_ms)
            .and_then(|delay| now.checked_add(delay));
        let until = until
            .into_iter()
            .chain(header_until)
            .filter(|until| *until > now)
            .max();
        if let (Some(quota), Some(until_ms)) = (quota, until) {
            let per_model = quota
                .pointer("/metadata/model")
                .and_then(Value::as_str)
                .is_some_and(|model| !model.is_empty());
            failure.rotation_hint = Some(RotationHint::RetryAt {
                until_ms,
                per_model,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_fractional_and_compound_delays_without_guessing() {
        assert_eq!(delay_ms("6402.432658549s"), Some(6_402_433));
        assert_eq!(delay_ms("1h46m42.432658549s"), Some(6_402_433));
        assert_eq!(delay_ms("0.000000001s"), Some(1));
        for invalid in [
            "0s",
            "-1s",
            "NaNs",
            "infs",
            "broken",
            "999999999999999999999999s",
        ] {
            assert_eq!(delay_ms(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn timestamps_and_retry_headers_keep_the_longest_confirmed_wait() {
        let now = 1_800_000_000_000;
        let mut failure = ProviderError::new(
            http::StatusCode::TOO_MANY_REQUESTS,
            crate::provider::ProviderErrorKind::RateLimit,
            "quota",
        );
        let error = json!({"status":"RESOURCE_EXHAUSTED","details":[
            {"@type":"type.googleapis.com/google.rpc.ErrorInfo", "reason":"QUOTA_EXHAUSTED",
             "domain":"cloudcode-pa.googleapis.com", "metadata":{
                "quotaResetTimeStamp":jiff::Timestamp::from_millisecond(now as i64 + 30_000).unwrap().to_string()
             }}
        ]});
        normalize(&mut failure, &error, "Antigravity", now);
        assert_eq!(failure.retry_after.as_deref(), Some("30"));
        assert!(matches!(failure.rotation_hint,
            Some(RotationHint::RetryAt { until_ms, per_model: false }) if until_ms == now + 30_000));
        failure.retry_after = Some("60".into());
        normalize(&mut failure, &error, "Antigravity", now);
        assert!(matches!(failure.rotation_hint,
            Some(RotationHint::RetryAt { until_ms, .. }) if until_ms == now + 60_000));
    }

    #[test]
    fn unknown_or_missing_quota_evidence_never_rotates() {
        for (reason, domain, delay) in [
            ("RATE_LIMIT_EXCEEDED", "cloudcode-pa.googleapis.com", "1s"),
            ("QUOTA_EXHAUSTED", "another.googleapis.com", "1s"),
            ("QUOTA_EXHAUSTED", "cloudcode-pa.googleapis.com", "bad"),
            ("QUOTA_EXHAUSTED", "cloudcode-pa.googleapis.com", "0s"),
        ] {
            let mut failure = ProviderError::new(
                http::StatusCode::TOO_MANY_REQUESTS,
                crate::provider::ProviderErrorKind::RateLimit,
                "limit",
            );
            let error = json!({"status":"RESOURCE_EXHAUSTED","details":[
                {"@type":"type.googleapis.com/google.rpc.ErrorInfo", "reason":reason,
                 "domain":domain,"metadata":{"quotaResetDelay":delay}}
            ]});
            normalize(&mut failure, &error, "Antigravity", 10_000);
            assert!(matches!(failure.rotation_hint, Some(RotationHint::Stay)));
        }
    }

    #[test]
    fn daily_quotas_outrank_short_retry_info() {
        let error = json!({"details":[
            {"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"1s"},
            {"@type":"type.googleapis.com/google.rpc.QuotaFailure","violations":[
                {"quotaId":"GenerateRequestsPerMinutePerProjectPerModel"},
                {"quotaId":"GenerateRequestsPerDayPerProjectPerModel"}
            ]}
        ]});
        assert_eq!(kind(&error), Some(FailureKind::Quota));
    }
}
