//! `cc-proxy usage`: how much of each plan is used, read from each
//! provider's own usage endpoint with the sign-in the proxy already holds.

use std::time::Duration;

use clap::ValueEnum;
use jiff::{Timestamp, tz::TimeZone};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::config;
use crate::providers::codex::auth::constants::CODEX_API_ENDPOINT;
use crate::providers::codex::auth::manager::CodexAuthManager;
use crate::providers::codex::auth::token_store as codex_tokens;
use crate::providers::codex::client::{build_codex_usage_headers, codex_usage_endpoint};
use crate::providers::kimi::auth::headers::common_headers;
use crate::providers::kimi::auth::manager::KimiAuthManager;
use crate::providers::kimi::auth::token_store as kimi_tokens;
use crate::providers::opencode::client::{OpenCodeClient, OpenCodeUsageResponse};
use crate::ui;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum UsageProvider {
    Codex,
    Kimi,
    #[value(name = "opencode")]
    OpenCode,
}

impl UsageProvider {
    fn id(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Kimi => "kimi",
            Self::OpenCode => "opencode",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Kimi => "Kimi",
            Self::OpenCode => "OpenCode Go",
        }
    }

    fn not_signed_in(self) -> &'static str {
        match self {
            Self::Codex => "codex: not signed in. Run `cc-proxy codex auth login`.",
            Self::Kimi => "kimi: not signed in. Run `cc-proxy kimi auth login`.",
            Self::OpenCode => {
                "opencode: no API key set. Run `cc-proxy opencode auth login` or set OPENCODE_API_KEY."
            }
        }
    }

    fn describe(self, failure: Failure) -> String {
        let name = self.name();
        let id = self.id();
        match failure {
            Failure::SignInUnusable => format!(
                "cc-proxy couldn't use the saved {name} sign-in. It may have expired or been \
                 revoked. Run `cc-proxy {id} auth login` to sign in again."
            ),
            Failure::Setup(detail) => {
                format!("cc-proxy couldn't build the {name} usage request: {detail}.")
            }
            Failure::Rejected(401 | 403) => format!(
                "{name} didn't accept the sign-in when asked for usage. Run \
                 `cc-proxy {id} auth login` to sign in again."
            ),
            Failure::Rejected(429) => format!(
                "{name} is limiting requests and turned down the usage request. Wait a \
                 minute and try again."
            ),
            Failure::Rejected(status @ 500..) => format!(
                "{name}'s usage request failed with HTTP {status}, a problem on {name}'s \
                 side. Try again in a few minutes."
            ),
            Failure::Rejected(status) => format!(
                "{name} turned down the usage request with HTTP {status}. If you changed \
                 the {name} base URL, check it; if not, check for a cc-proxy update."
            ),
            Failure::TimedOut => format!(
                "{name} didn't answer the usage request within {} seconds. Check your \
                 connection and try again.",
                REQUEST_TIMEOUT.as_secs()
            ),
            Failure::Unreachable => format!(
                "cc-proxy couldn't reach {name} to ask for usage. Check your internet \
                 connection and try again."
            ),
            Failure::Unreadable => format!(
                "{name}'s usage reply came back in a format cc-proxy doesn't recognize. \
                 Try again later; if it keeps happening, check for a cc-proxy update."
            ),
        }
    }
}

/// Why a Codex or Kimi usage lookup failed, before it is put into words.
#[derive(Debug)]
enum Failure {
    /// The saved sign-in couldn't be read, or couldn't be renewed.
    SignInUnusable,
    /// A header couldn't be built from the saved sign-in or the config.
    Setup(String),
    Rejected(u16),
    TimedOut,
    Unreachable,
    Unreadable,
}

/// One plan's usage, in the same shape for every provider.
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
struct PlanUsage {
    #[serde(skip_serializing_if = "Option::is_none")]
    plan: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    five_hour: Option<Window>,
    #[serde(skip_serializing_if = "Option::is_none")]
    weekly: Option<Window>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
struct Window {
    used_percent: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    resets_at_ms: Option<i64>,
}

enum Report {
    Plan(UsageProvider, PlanUsage),
    /// OpenCode Go keeps its own shape: it has a monthly window and a status
    /// per window that the shared shape has no room for.
    OpenCode(Box<OpenCodeUsageResponse>),
}

impl Report {
    fn text(&self, now: Timestamp, tz: &TimeZone, styled: bool) -> String {
        match self {
            Self::Plan(provider, usage) => format_plan(provider.name(), usage, now, tz, styled),
            Self::OpenCode(usage) => crate::providers::opencode::usage::format_text(usage, styled),
        }
    }

    fn json(&self) -> serde_json::Result<Value> {
        match self {
            Self::Plan(_, usage) => serde_json::to_value(usage),
            Self::OpenCode(usage) => serde_json::to_value(usage),
        }
    }
}

/// Prints usage for `only`, or for every provider when it is None, and
/// returns the exit code: 0 when every lookup worked, 1 when the named
/// provider isn't signed in, 2 when a lookup failed.
pub fn run(only: Option<UsageProvider>, json: bool) -> anyhow::Result<i32> {
    let providers = match only {
        Some(provider) => vec![provider],
        None => UsageProvider::value_variants().to_vec(),
    };
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let results = runtime.block_on(futures_util::future::join_all(
        providers.iter().map(|provider| fetch(*provider, &client)),
    ));

    let now = Timestamp::now();
    let tz = TimeZone::system();
    let styled = ui::styled(&std::io::stdout());
    let mut code = 0;
    let mut blocks = Vec::new();
    let mut by_provider = Map::new();
    for (provider, result) in providers.into_iter().zip(results) {
        match result {
            Ok(Some(report)) if json => {
                by_provider.insert(provider.id().to_string(), report.json()?);
            }
            Ok(Some(report)) => blocks.push(report.text(now, &tz, styled)),
            // Asked for by name, a missing sign-in fails the command.
            Ok(None) if only.is_some() => {
                eprintln!("{}", provider.not_signed_in());
                code = 1;
            }
            // Keep stdout pure JSON for scripts.
            Ok(None) if json => eprintln!("{}", provider.not_signed_in()),
            Ok(None) => blocks.push(provider.not_signed_in().to_string()),
            Err(message) => {
                eprintln!("{message}");
                code = 2;
            }
        }
    }

    if json && (only.is_none() || !by_provider.is_empty()) {
        println!("{}", serde_json::to_string_pretty(&by_provider)?);
    } else if !blocks.is_empty() {
        println!("{}", blocks.join("\n\n"));
    }
    Ok(code)
}

/// None means the provider isn't signed in.
async fn fetch(
    provider: UsageProvider,
    client: &reqwest::Client,
) -> Result<Option<Report>, String> {
    let usage = match provider {
        UsageProvider::Codex => codex(client).await,
        UsageProvider::Kimi => kimi(client).await,
        // OpenCode Go reports in its own shape and its own words.
        UsageProvider::OpenCode => return opencode().await,
    };
    match usage {
        Ok(Some(usage)) if usage.five_hour.is_none() && usage.weekly.is_none() => {
            Err(provider.describe(Failure::Unreadable))
        }
        Ok(usage) => Ok(usage.map(|usage| Report::Plan(provider, usage))),
        Err(failure) => Err(provider.describe(failure)),
    }
}

async fn codex(client: &reqwest::Client) -> Result<Option<PlanUsage>, Failure> {
    let manager = CodexAuthManager::new(codex_tokens::file_store());
    if manager
        .store
        .load_auth()
        .map_err(|_| Failure::SignInUnusable)?
        .is_none()
    {
        return Ok(None);
    }
    // ponytail: a 401 is reported, not refreshed and retried. get_auth()
    // already renews a token that is about to expire, so a 401 here means
    // the sign-in itself is gone.
    let auth = manager
        .get_auth()
        .await
        .map_err(|_| Failure::SignInUnusable)?;
    let headers =
        build_codex_usage_headers(&auth).map_err(|error| Failure::Setup(error.message))?;
    let url = codex_usage_endpoint(&config::codex_base_url(CODEX_API_ENDPOINT));
    let body = get_json(client, &url, headers).await?;
    Ok(Some(parse_chatgpt(&body)))
}

async fn kimi(client: &reqwest::Client) -> Result<Option<PlanUsage>, Failure> {
    let manager = KimiAuthManager::new(kimi_tokens::file_store());
    if manager
        .store
        .load_auth()
        .map_err(|_| Failure::SignInUnusable)?
        .is_none()
    {
        return Ok(None);
    }
    // The Kimi auth manager renews over a blocking client, which must not
    // run on the async runtime.
    let auth = tokio::task::spawn_blocking(move || manager.get_auth())
        .await
        .map_err(|_| Failure::SignInUnusable)?
        .map_err(|_| Failure::SignInUnusable)?;
    let mut headers = common_headers().map_err(|error| Failure::Setup(error.to_string()))?;
    let bearer = HeaderValue::from_str(&format!("Bearer {}", auth.access))
        .map_err(|_| Failure::Setup("the saved token isn't a valid header".into()))?;
    headers.insert(AUTHORIZATION, bearer);
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    let url = format!("{}/usages", config::kimi_base_url());
    let body = get_json(client, &url, headers).await?;
    Ok(Some(parse_kimi(&body)))
}

async fn opencode() -> Result<Option<Report>, String> {
    let Some(api_key) = config::opencode_api_key() else {
        return Ok(None);
    };
    let client = OpenCodeClient::new(config::opencode_base_url(), Some(api_key))
        .map_err(|error| error.to_string())?;
    let usage = client.get_usage().await.map_err(|error| error.message)?;
    Ok(Some(Report::OpenCode(Box::new(usage))))
}

async fn get_json(
    client: &reqwest::Client,
    url: &str,
    headers: HeaderMap,
) -> Result<Value, Failure> {
    let transport_failure = |error: reqwest::Error| {
        if error.is_timeout() {
            Failure::TimedOut
        } else {
            Failure::Unreachable
        }
    };
    let response = client
        .get(url)
        .headers(headers)
        .send()
        .await
        .map_err(transport_failure)?;
    let status = response.status();
    if !status.is_success() {
        return Err(Failure::Rejected(status.as_u16()));
    }
    let body = response.bytes().await.map_err(transport_failure)?;
    serde_json::from_slice(&body).map_err(|_| Failure::Unreadable)
}

// ---------------------------------------------------------------------------
// Parsers, ported from TabRunner's chrome/src/modules/providers/usage.ts.
// These endpoints are undocumented, so a window that can't be read is left
// out rather than failing the whole reply.
// ---------------------------------------------------------------------------

/// ChatGPT's `wham/usage`: `plan_type`, and `rate_limit.primary_window`
/// (5 hours) and `secondary_window` (weekly), each with a `used_percent`.
fn parse_chatgpt(body: &Value) -> PlanUsage {
    let window = |key: &str| {
        let window = body.get("rate_limit")?.get(key)?;
        Some(Window {
            used_percent: number(window.get("used_percent")?)?,
            resets_at_ms: first_present(window, &["resets_at", "reset_at"]).and_then(reset_ms),
        })
    };
    PlanUsage {
        plan: text(body.get("plan_type")),
        five_hour: window("primary_window"),
        weekly: window("secondary_window"),
    }
}

/// Kimi's `/usages`: the plan in `user.membership.level` (or `subType`),
/// the weekly quota in `usage`, and the 5-hour quota in the `detail` of a
/// `limits` entry. Counts arrive as strings.
fn parse_kimi(body: &Value) -> PlanUsage {
    let limits = body
        .get("limits")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    PlanUsage {
        plan: text(body.pointer("/user/membership/level")).or_else(|| text(body.get("subType"))),
        five_hour: five_hour_limit(limits).and_then(|limit| kimi_quota(limit.get("detail")?)),
        weekly: body.get("usage").and_then(kimi_quota),
    }
}

/// TabRunner takes `limits[0]` as the 5-hour window. This picks the entry
/// whose window is 5 hours instead, so a reordered list can't pass another
/// window off as the 5-hour one. The first entry is used only when no entry
/// says what window it covers.
fn five_hour_limit(limits: &[Value]) -> Option<&Value> {
    let says_window = limits
        .iter()
        .any(|limit| limit.get("window").is_some_and(Value::is_object));
    if !says_window {
        return limits.first();
    }
    limits
        .iter()
        .find(|limit| limit.get("window").is_some_and(is_five_hours))
}

fn is_five_hours(window: &Value) -> bool {
    let duration = window.get("duration").and_then(number);
    let unit = window
        .get("timeUnit")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_uppercase();
    (unit.contains("MINUTE") && duration == Some(300.0))
        || (unit.contains("HOUR") && duration == Some(5.0))
}

/// `{limit, used | remaining, resetTime | reset_at}` as a used percentage,
/// rounded and held to 0-100.
fn kimi_quota(quota: &Value) -> Option<Window> {
    let limit = quota
        .get("limit")
        .and_then(number)
        .filter(|limit| *limit > 0.0)?;
    let used = match quota.get("used").and_then(number) {
        Some(used) => used,
        None => limit - quota.get("remaining").and_then(number)?,
    };
    Some(Window {
        used_percent: (used / limit * 100.0).round().clamp(0.0, 100.0),
        resets_at_ms: first_present(quota, &["resetTime", "reset_at"]).and_then(reset_ms),
    })
}

/// A number, or a string holding one.
fn number(value: &Value) -> Option<f64> {
    let number = match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    };
    number.filter(|number| number.is_finite())
}

fn text(value: Option<&Value>) -> Option<String> {
    value?
        .as_str()
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// The first of `keys` that is set and not null.
fn first_present<'a>(object: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter()
        .find_map(|key| object.get(*key).filter(|value| !value.is_null()))
}

/// A reset time sent as an RFC 3339 string, or as epoch seconds or
/// milliseconds. A number below 10^12 is seconds: as milliseconds it would
/// fall before September 2001.
fn reset_ms(value: &Value) -> Option<i64> {
    if let Some(at) = value
        .as_str()
        .and_then(|text| text.parse::<Timestamp>().ok())
    {
        return Some(at.as_millisecond());
    }
    let epoch = number(value).filter(|epoch| *epoch > 0.0)?;
    let ms = if epoch < 1_000_000_000_000.0 {
        epoch * 1000.0
    } else {
        epoch
    };
    Some(ms.round() as i64)
}

// ---------------------------------------------------------------------------
// Text output
// ---------------------------------------------------------------------------

fn format_plan(
    name: &str,
    usage: &PlanUsage,
    now: Timestamp,
    tz: &TimeZone,
    styled: bool,
) -> String {
    let heading = match &usage.plan {
        Some(plan) => format!("{name} ({plan})"),
        None => name.to_string(),
    };
    let mut output = ui::strong(&heading, ui::WHITE, styled);
    for (label, window) in [
        ("5-hour window", &usage.five_hour),
        ("Weekly", &usage.weekly),
    ] {
        output.push_str(&window_row(
            label,
            window.as_ref().map(|window| window.used_percent),
            &format_window(window.as_ref(), now, tz),
            styled,
        ));
    }
    output
}

const METER_CELLS: usize = 20;

/// "\n  Weekly: 7% used, …". In a terminal the label is padded and a meter
/// goes before the words, so every window's meter lines up.
pub(crate) fn window_row(label: &str, percent: Option<f64>, text: &str, styled: bool) -> String {
    if !styled {
        return format!("\n  {label}: {text}");
    }
    let meter = percent.map_or_else(
        || " ".repeat(METER_CELLS),
        |percent| ui::meter(percent, METER_CELLS),
    );
    format!("\n  {label:<18}{meter}  {text}")
}

fn format_window(window: Option<&Window>, now: Timestamp, tz: &TimeZone) -> String {
    let Some(window) = window else {
        return "not reported".to_string();
    };
    let used = format!("{}% used", window.used_percent);
    match window
        .resets_at_ms
        .and_then(|ms| Timestamp::from_millisecond(ms).ok())
    {
        Some(reset) => format!("{used}, {}", format_reset(reset, now, tz)),
        None => used,
    }
}

/// "resets in 2h 10m (17:00)": the time left, then the local clock time,
/// with the weekday when it isn't today and the date when it is a week or
/// more away.
fn format_reset(reset: Timestamp, now: Timestamp, tz: &TimeZone) -> String {
    let at = reset.to_zoned(tz.clone());
    let days_ahead = now
        .to_zoned(tz.clone())
        .date()
        .until(at.date())
        .map_or(i32::MAX, |span| span.get_days());
    let clock = at.strftime(match days_ahead {
        0 => "%H:%M",
        1..=6 => "%a %H:%M",
        _ => "%b %-d %H:%M",
    });
    let seconds = reset.as_second() - now.as_second();
    if seconds <= 0 {
        return format!("already reset ({clock})");
    }
    format!("resets in {} ({clock})", format_time_left(seconds))
}

/// The two largest units, rounded up to the minute: "5d 3h", "2h 10m", "4m".
fn format_time_left(seconds: i64) -> String {
    let minutes = (seconds + 59) / 60;
    let (days, hours, minutes) = (minutes / 1440, minutes % 1440 / 60, minutes % 60);
    match (days, hours, minutes) {
        (0, 0, minutes) => format!("{minutes}m"),
        (0, hours, 0) => format!("{hours}h"),
        (0, hours, minutes) => format!("{hours}h {minutes}m"),
        (days, 0, _) => format!("{days}d"),
        (days, hours, _) => format!("{days}d {hours}h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/usage/{name}", env!("CARGO_MANIFEST_DIR"));
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"));
        serde_json::from_str(&raw).unwrap_or_else(|error| panic!("{path}: {error}"))
    }

    fn ms(rfc3339: &str) -> Option<i64> {
        Some(rfc3339.parse::<Timestamp>().unwrap().as_millisecond())
    }

    fn window(used_percent: f64, resets_at_ms: Option<i64>) -> Option<Window> {
        Some(Window {
            used_percent,
            resets_at_ms,
        })
    }

    #[test]
    fn chatgpt_maps_primary_and_secondary_windows_and_keeps_the_plan() {
        assert_eq!(
            parse_chatgpt(&fixture("chatgpt.json")),
            PlanUsage {
                plan: Some("plus".into()),
                five_hour: window(45.0, ms("2026-08-09T17:00:00Z")),
                weekly: window(7.0, ms("2026-08-15T00:00:00Z")),
            }
        );
    }

    #[test]
    fn chatgpt_reads_epoch_second_resets_and_missing_windows() {
        assert_eq!(
            parse_chatgpt(&fixture("chatgpt-epoch-seconds.json")),
            PlanUsage {
                five_hour: window(100.0, Some(1_786_000_000_000)),
                ..PlanUsage::default()
            }
        );
    }

    #[test]
    fn kimi_reads_string_counts_from_the_5_hour_detail_and_weekly_usage() {
        assert_eq!(
            parse_kimi(&fixture("kimi.json")),
            PlanUsage {
                plan: Some("allegretto".into()),
                five_hour: window(28.0, ms("2026-08-09T17:00:00+08:00")),
                weekly: window(12.0, ms("2026-08-15T00:00:00+08:00")),
            }
        );
    }

    #[test]
    fn kimi_derives_used_from_remaining() {
        assert_eq!(
            parse_kimi(&fixture("kimi-remaining.json")).weekly,
            window(50.0, None)
        );
    }

    #[test]
    fn kimi_picks_the_5_hour_limit_by_its_window_not_its_position() {
        let usage = parse_kimi(&fixture("kimi-5h-not-first.json"));
        assert_eq!(
            usage.five_hour,
            window(28.0, ms("2026-08-09T17:00:00+08:00"))
        );
    }

    #[test]
    fn kimi_falls_back_to_the_first_limit_only_without_window_info() {
        let first_without_window = serde_json::json!({
            "limits": [{"detail": {"limit": "10", "used": "4"}}, {"detail": {"limit": "10", "used": "9"}}]
        });
        assert_eq!(
            parse_kimi(&first_without_window).five_hour,
            window(40.0, None)
        );

        let no_5_hour_window = serde_json::json!({
            "limits": [{"window": {"duration": 7, "timeUnit": "DAY"}, "detail": {"limit": "10", "used": "4"}}]
        });
        assert_eq!(parse_kimi(&no_5_hour_window).five_hour, None);

        let hours = serde_json::json!({
            "limits": [{"window": {"duration": 5, "timeUnit": "TIME_UNIT_HOUR"}, "detail": {"limit": "4", "used": "1"}}]
        });
        assert_eq!(parse_kimi(&hours).five_hour, window(25.0, None));
    }

    #[test]
    fn kimi_falls_back_to_sub_type_and_skips_unreadable_windows() {
        assert_eq!(
            parse_kimi(&serde_json::json!({"subType": "moderato", "limits": "later"})),
            PlanUsage {
                plan: Some("moderato".into()),
                ..PlanUsage::default()
            }
        );
    }

    #[test]
    fn kimi_percent_is_rounded_and_held_to_0_to_100() {
        let over = serde_json::json!({"usage": {"limit": 3, "used": 4}});
        assert_eq!(parse_kimi(&over).weekly, window(100.0, None));
        let third = serde_json::json!({"usage": {"limit": 3, "used": 1}});
        assert_eq!(parse_kimi(&third).weekly, window(33.0, None));
        let no_limit = serde_json::json!({"usage": {"limit": 0, "used": 1}});
        assert_eq!(parse_kimi(&no_limit).weekly, None);
    }

    #[test]
    fn reset_accepts_rfc3339_epoch_seconds_and_epoch_milliseconds() {
        assert_eq!(
            reset_ms(&Value::from("2026-08-09T17:00:00Z")),
            ms("2026-08-09T17:00:00Z")
        );
        assert_eq!(
            reset_ms(&Value::from(1_786_000_000)),
            Some(1_786_000_000_000)
        );
        assert_eq!(
            reset_ms(&Value::from("1786000000")),
            Some(1_786_000_000_000)
        );
        assert_eq!(
            reset_ms(&Value::from(1_786_000_000_123_i64)),
            Some(1_786_000_000_123)
        );
        assert_eq!(reset_ms(&Value::from(0)), None);
        assert_eq!(reset_ms(&Value::from("soon")), None);
    }

    #[test]
    fn text_shows_time_left_and_local_clock_time() {
        let tz = TimeZone::UTC;
        // A Wednesday.
        let now: Timestamp = "2026-08-12T14:50:00Z".parse().unwrap();
        let usage = PlanUsage {
            plan: Some("plus".into()),
            five_hour: window(45.0, ms("2026-08-12T17:00:00Z")),
            weekly: window(7.0, ms("2026-08-17T00:00:00Z")),
        };
        assert_eq!(
            format_plan("Codex", &usage, now, &tz, false),
            concat!(
                "Codex (plus)\n",
                "  5-hour window: 45% used, resets in 2h 10m (17:00)\n",
                "  Weekly: 7% used, resets in 4d 9h (Mon 00:00)"
            )
        );
    }

    #[test]
    fn text_marks_missing_windows_and_resets() {
        let now: Timestamp = "2026-08-12T14:50:00Z".parse().unwrap();
        let usage = PlanUsage {
            plan: None,
            five_hour: window(12.5, None),
            weekly: None,
        };
        assert_eq!(
            format_plan("Kimi", &usage, now, &TimeZone::UTC, false),
            "Kimi\n  5-hour window: 12.5% used\n  Weekly: not reported"
        );
    }

    #[test]
    fn reset_text_dates_far_and_past_resets() {
        let tz = TimeZone::UTC;
        let now: Timestamp = "2026-08-12T14:50:00Z".parse().unwrap();
        let at = |rfc3339: &str| rfc3339.parse::<Timestamp>().unwrap();
        assert_eq!(
            format_reset(at("2026-08-19T14:50:00Z"), now, &tz),
            "resets in 7d (Aug 19 14:50)"
        );
        assert_eq!(
            format_reset(at("2026-08-12T14:50:30Z"), now, &tz),
            "resets in 1m (14:50)"
        );
        assert_eq!(
            format_reset(at("2026-08-09T17:00:00Z"), now, &tz),
            "already reset (Aug 9 17:00)"
        );
    }

    #[test]
    fn usage_endpoint_sits_beside_the_codex_endpoint() {
        assert_eq!(
            codex_usage_endpoint(CODEX_API_ENDPOINT),
            "https://chatgpt.com/backend-api/wham/usage"
        );
        assert_eq!(
            codex_usage_endpoint("http://127.0.0.1:9/backend-api/codex/responses/"),
            "http://127.0.0.1:9/backend-api/wham/usage"
        );
    }

    #[test]
    fn failures_say_what_failed_and_what_to_do() {
        let message = UsageProvider::Codex.describe(Failure::Rejected(401));
        assert_eq!(
            message,
            "Codex didn't accept the sign-in when asked for usage. Run `cc-proxy codex auth login` to sign in again."
        );
        let message = UsageProvider::Kimi.describe(Failure::Unreadable);
        assert_eq!(
            message,
            "Kimi's usage reply came back in a format cc-proxy doesn't recognize. Try again later; if it keeps happening, check for a cc-proxy update."
        );
    }
}
