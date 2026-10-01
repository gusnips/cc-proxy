use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rand::Rng;

pub const RETRY_INITIAL_DELAY_MS: u64 = 2000;
pub const RETRY_MAX_DELAY_MS: u64 = 30_000;
pub const RETRY_BACKOFF_FACTOR: u64 = 2;
pub const MAX_RATE_LIMIT_RETRIES: u32 = 3;

#[derive(Debug, Clone, Copy)]
pub struct BackoffOutcome {
    pub wait_ms: u64,
    pub exceeds_budget: bool,
}

/// 529 is Anthropic's overloaded status, which Anthropic-shaped upstreams
/// return for the same capacity shortfall a 503 reports.
pub fn should_retry_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504 | 529)
}

pub fn compute_backoff_delay(attempt: u32, retry_after: Option<&str>) -> BackoffOutcome {
    if let Some(target_ms) = retry_after.and_then(retry_after_ms) {
        return BackoffOutcome {
            wait_ms: target_ms.min(RETRY_MAX_DELAY_MS),
            exceeds_budget: target_ms > RETRY_MAX_DELAY_MS,
        };
    }

    // Full jitter: a random wait up to the exponential ceiling. A fixed
    // fraction of it sent every session that hit the same limit back in the
    // same instant, rebuilding the burst that caused the limit.
    let ceiling = RETRY_INITIAL_DELAY_MS
        .saturating_mul(RETRY_BACKOFF_FACTOR.saturating_pow(attempt))
        .min(RETRY_MAX_DELAY_MS);
    BackoffOutcome {
        wait_ms: rand::thread_rng().gen_range(0..=ceiling),
        exceeds_budget: false,
    }
}

/// Retry-After is either a delay in seconds or an HTTP-date (RFC 9110).
/// Reading only the number dropped a dated header on the floor and retried
/// on our own clock, before the time the server had named.
fn retry_after_ms(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    let seconds = match raw.parse::<f64>() {
        Ok(seconds) => seconds,
        Err(_) => {
            let at =
                time::OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc2822)
                    .ok()?;
            (at - time::OffsetDateTime::now_utc())
                .as_seconds_f64()
                .max(0.0)
        }
    };
    (seconds.is_finite() && seconds >= 0.0).then(|| (seconds * 1000.0).ceil() as u64)
}

static ZERO_RETRY_DELAY_FOR_TESTS: AtomicBool = AtomicBool::new(false);

/// Make retry sleeps return immediately so exhaustion paths can be exercised
/// in tests without waiting out the real backoff schedule.
pub fn set_zero_retry_delay_for_tests(enabled: bool) {
    ZERO_RETRY_DELAY_FOR_TESTS.store(enabled, Ordering::SeqCst);
}

pub async fn sleep(ms: u64) {
    if ZERO_RETRY_DELAY_FOR_TESTS.load(Ordering::SeqCst) {
        return;
    }
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

#[cfg(test)]
pub async fn retry_on_statuses<T, E, F>(mut next: F) -> Result<T, E>
where
    E: std::fmt::Debug,
    F: FnMut(u32) -> Result<T, E>,
{
    let mut attempt = 0;
    loop {
        attempt += 1;
        if attempt > MAX_RATE_LIMIT_RETRIES + 1 {
            break;
        }
        match next(attempt) {
            Ok(value) => return Ok(value),
            Err(err) if attempt <= MAX_RATE_LIMIT_RETRIES + 1 => {
                if attempt > MAX_RATE_LIMIT_RETRIES {
                    return Err(err);
                }
                sleep(compute_backoff_delay(attempt, None).wait_ms).await;
            }
            Err(err) => return Err(err),
        }
    }
    unreachable!()
}
