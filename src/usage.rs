//! `cc-proxy usage`: how much of each plan is used, read from each
//! provider's own usage endpoint with the sign-in the proxy already holds.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use clap::ValueEnum;
use jiff::{Timestamp, tz::TimeZone};
use ratatui::text::Line;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, RETRY_AFTER};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::accounts::Accounts;
use crate::auth::AuthStorage;
use crate::config;
use crate::providers::antigravity::Family;
use crate::providers::claude::auth::{
    AuthManager, RenewRejected, StoredAuth, USAGE_BETA, USAGE_URL,
};
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
/// How long to leave an account alone after a 429 that gives no `Retry-After`.
const RATE_LIMIT_FLOOR: Duration = Duration::from_secs(300);
/// The longest wait a `Retry-After` can ask for, so one odd answer can't switch
/// the automatic refresh off for a day. The `r` key still asks at once.
const RATE_LIMIT_CEILING: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum UsageProvider {
    Claude,
    Codex,
    Kimi,
    Antigravity,
    #[value(name = "opencode")]
    OpenCode,
}

impl UsageProvider {
    fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Kimi => "kimi",
            Self::Antigravity => "antigravity",
            Self::OpenCode => "opencode",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
            Self::Kimi => "Kimi",
            Self::Antigravity => "Antigravity",
            Self::OpenCode => "OpenCode Go",
        }
    }

    fn not_signed_in(self) -> &'static str {
        match self {
            Self::Claude => "claude: not signed in. Run `cc-proxy auth login claude`.",
            Self::Codex => "codex: not signed in. Run `cc-proxy auth login codex`.",
            Self::Kimi => "kimi: not signed in. Run `cc-proxy auth login kimi`.",
            Self::Antigravity => {
                "antigravity: not signed in. Run `cc-proxy auth login antigravity`."
            }
            Self::OpenCode => {
                "opencode: no API key set. Run `cc-proxy auth login opencode` or set OPENCODE_API_KEY."
            }
        }
    }

    fn describe(self, account: &str, failure: Failure) -> String {
        let name = self.name();
        let id = self.id();
        let replace = format!("cc-proxy auth login {id} --replace --account {account}");
        let detail = match failure {
            Failure::SignInUnusable => "cc-proxy couldn't read or renew this sign-in.".to_string(),
            Failure::Setup(detail) => format!(
                "cc-proxy couldn't build the usage request: {detail}. Check the provider settings."
            ),
            Failure::Rejected {
                status: 401 | 403, ..
            } => {
                format!("{name} didn't accept the sign-in when asked for usage.")
            }
            Failure::Rejected {
                status: 429,
                retry_after,
            } => format!(
                "{name} is limiting requests and turned down the usage request. Run \
                 `cc-proxy usage {id}` again {}.",
                match retry_after {
                    Some(seconds) => format!("in {}", format_time_left(seconds_i64(seconds))),
                    None => "later".to_string(),
                }
            ),
            Failure::Rejected {
                status: status @ 500..,
                ..
            } => format!(
                "The usage request failed with HTTP {status}, a problem on {name}'s \
                 side. Run `cc-proxy usage {id}` again in a few minutes."
            ),
            Failure::Rejected { status, .. } => format!(
                "{name} turned down the usage request with HTTP {status}. If you changed \
                 the {name} base URL, check it; if not, check for a cc-proxy update."
            ),
            Failure::TimedOut => format!(
                "{name} didn't answer the usage request within {} seconds. Check your \
                 connection and run `cc-proxy usage {id}` again.",
                REQUEST_TIMEOUT.as_secs()
            ),
            Failure::RenewalTimedOut => format!(
                "{name} didn't answer when cc-proxy renewed the sign-in. Check your \
                 connection and run `cc-proxy usage {id}` again."
            ),
            Failure::Waiting(left) => format!(
                "{name} asked cc-proxy to wait before the next usage request. Run \
                 `cc-proxy usage {id}` again in {}.",
                format_time_left(seconds_i64(left.as_secs()))
            ),
            Failure::Unreachable => format!(
                "cc-proxy couldn't reach {name} to ask for usage. Check your internet \
                 connection and run `cc-proxy usage {id}` again."
            ),
            Failure::Unreadable => format!(
                "{name}'s usage reply came back in a format cc-proxy doesn't recognize. \
                 Run `cc-proxy usage {id}` again later; if it keeps happening, check for a cc-proxy update."
            ),
        };
        format!("{name} account `{account}`: {detail} To replace this sign-in, run `{replace}`.")
    }
}

impl Failure {
    /// One short line for the monitor, which already names the account and
    /// offers sign-in. The command line keeps the longer `describe`.
    fn short(&self, name: &str) -> String {
        match self {
            Self::SignInUnusable => {
                "cc-proxy can't read or renew this sign-in. Choose Sign in to this account again."
                    .into()
            }
            Self::Setup(_) => {
                "cc-proxy can't build the usage request. Check the provider settings.".into()
            }
            Self::Rejected {
                status: 401 | 403, ..
            } => {
                format!("{name} rejected the sign-in. Choose Sign in to this account again.")
            }
            Self::Rejected {
                status: 429,
                retry_after,
            } => limited(name, rate_limit_wait(*retry_after)),
            Self::Waiting(left) => limited(name, *left),
            Self::Rejected {
                status: status @ 500..,
                ..
            } => {
                format!("{name} had a problem (HTTP {status}). Press r to retry in a few minutes.")
            }
            Self::Rejected { status, .. } => format!(
                "{name} refused the usage request (HTTP {status}). Check for a cc-proxy update."
            ),
            Self::TimedOut => format!(
                "{name} didn't answer in {} seconds. Press r to retry.",
                REQUEST_TIMEOUT.as_secs()
            ),
            Self::RenewalTimedOut => {
                format!("{name} didn't answer when cc-proxy renewed the sign-in. Press r to retry.")
            }
            Self::Unreachable => {
                format!("cc-proxy couldn't reach {name}. Check your connection, then press r.")
            }
            Self::Unreadable => format!(
                "{name} sent a usage reply cc-proxy can't read. Check for a cc-proxy update."
            ),
        }
    }
}

/// Why a usage lookup failed, before it is put into words.
#[derive(Debug)]
enum Failure {
    /// The saved sign-in is gone or unreadable, or the provider turned down its
    /// renewal. Only these mean "sign in again".
    SignInUnusable,
    /// A header couldn't be built from the saved sign-in or the config.
    Setup(String),
    /// The provider answered with an error. `retry_after` is its `Retry-After`
    /// in seconds, when it sent one.
    Rejected {
        status: u16,
        retry_after: Option<u64>,
    },
    TimedOut,
    /// The sign-in had to be renewed first, and Claude didn't answer that.
    RenewalTimedOut,
    Unreachable,
    Unreadable,
    /// An earlier 429 asked cc-proxy to wait, so nothing was sent this time.
    Waiting(Duration),
}

impl Failure {
    /// A fixed word for each cause, so a log line never carries a message that
    /// could hold a name or a token.
    fn category(&self) -> &'static str {
        match self {
            Self::SignInUnusable => "sign_in_unusable",
            Self::Setup(_) => "setup",
            Self::Rejected { .. } => "rejected",
            Self::TimedOut => "timed_out",
            Self::RenewalTimedOut => "renewal_timed_out",
            Self::Unreachable => "unreachable",
            Self::Unreadable => "unreadable",
            Self::Waiting(_) => "waiting",
        }
    }
}

/// How long to leave an account alone after a 429: what `Retry-After` said,
/// up to the ceiling, or the floor when it said nothing.
fn rate_limit_wait(retry_after: Option<u64>) -> Duration {
    retry_after
        .map_or(RATE_LIMIT_FLOOR, |seconds| {
            Duration::from_secs(seconds).min(RATE_LIMIT_CEILING)
        })
        .max(Duration::from_secs(1))
}

fn limited(name: &str, wait: Duration) -> String {
    format!(
        "{name} is limiting requests. Try again in {}.",
        format_time_left(seconds_i64(wait.as_secs()))
    )
}

fn seconds_i64(seconds: u64) -> i64 {
    i64::try_from(seconds).unwrap_or(i64::MAX)
}

/// When each account may be asked again, after a 429. In memory: a monitor
/// that is closed and opened again asks once, and learns the wait from the answer.
// ponytail: one table per process. Two monitors open at once each hit the 429
// once. A shared file would stop that, at the cost of a lock and a stale-file rule.
type Waits = HashMap<(&'static str, String), Instant>;

static WAITS: LazyLock<Mutex<Waits>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn lock_waits() -> MutexGuard<'static, Waits> {
    WAITS.lock().unwrap_or_else(|error| error.into_inner())
}

/// How much longer to leave `account` alone, if an earlier 429 asked for it.
fn wait_left(provider: UsageProvider, account: &str) -> Option<Duration> {
    let mut waits = lock_waits();
    let key = (provider.id(), account.to_string());
    let left = waits
        .get(&key)?
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero());
    if left.is_none() {
        waits.remove(&key);
    }
    left
}

/// A 429 makes the next automatic refresh skip the account.
fn start_wait(provider: UsageProvider, account: &str, retry_after: Option<u64>) {
    lock_waits().insert(
        (provider.id(), account.to_string()),
        Instant::now() + rate_limit_wait(retry_after),
    );
}

fn end_wait(provider: UsageProvider, account: &str) {
    lock_waits().remove(&(provider.id(), account.to_string()));
}

/// One line in proxy.log for a lookup that failed: the provider, the saved
/// account's id, the kind of failure, how long it took and, after a 429, when
/// cc-proxy asks again. Never a name, a token or a reply body. At `info`, so
/// the command line and the monitor screen don't print it.
fn log_failure(provider: UsageProvider, account: &str, failure: &Failure, took: Duration) {
    let mut fields = Map::from_iter([
        ("provider".into(), serde_json::json!(provider.id())),
        ("account".into(), serde_json::json!(account)),
        ("failure".into(), serde_json::json!(failure.category())),
        (
            "elapsedMs".into(),
            serde_json::json!(took.as_millis() as u64),
        ),
    ]);
    if let Failure::Rejected {
        status,
        retry_after,
    } = failure
    {
        fields.insert("status".into(), serde_json::json!(status));
        if *status == 429 {
            let wait = rate_limit_wait(*retry_after);
            fields.insert(
                "retryAtMs".into(),
                serde_json::json!(crate::rotation::now_ms() + wait.as_millis() as u64),
            );
        }
    }
    crate::logging::create_logger("usage").info("usage_lookup_failed", Some(fields));
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
    /// Providers that keep one allowance per model family list each here
    /// instead of in `five_hour` and `weekly`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    groups: Vec<GroupUsage>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct GroupUsage {
    family: Family,
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

impl Window {
    /// What is left to use, the way the source dashboards show it. The JSON
    /// contract keeps `used_percent`; only the words people read flip.
    fn remaining_percent(&self) -> f64 {
        (100.0 - self.used_percent).clamp(0.0, 100.0)
    }
}

#[derive(Debug, Serialize)]
struct AccountUsage {
    #[serde(skip)]
    id: String,
    name: String,
    current: bool,
    #[serde(flatten)]
    result: AccountResult,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum AccountResult {
    Usage {
        usage: PlanUsage,
    },
    Unavailable {
        error: String,
        /// The same failure in one line, for the monitor.
        #[serde(skip)]
        short: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UsageWindow {
    pub label: &'static str,
    pub remaining_percent: f64,
    pub resets_at_ms: Option<i64>,
}

impl UsageWindow {
    fn family(&self) -> &str {
        self.label.rsplit_once(' ').map_or("", |(family, _)| family)
    }

    /// A 5-hour window cannot be used while the same family's weekly or
    /// monthly allowance is at 0%, so it must not glow green.
    fn color(&self, siblings: &[Self]) -> ratatui::style::Color {
        if self.remaining_percent > 0.0
            && self.label.ends_with("5h")
            && siblings.iter().any(|other| {
                other.label != self.label
                    && other.family() == self.family()
                    && other.remaining_percent <= 0.0
            })
        {
            ui::DIM_WHITE
        } else {
            ui::remaining_color(self.remaining_percent)
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MonitorUsage {
    Reported(Vec<UsageWindow>),
    Unavailable(String),
    NotReported(&'static str),
}

impl MonitorUsage {
    pub fn lines(&self, now: Timestamp, tz: &TimeZone) -> Vec<Line<'static>> {
        match self {
            Self::Unavailable(message) => vec![message.clone().into()],
            Self::NotReported(message) => vec![(*message).into()],
            Self::Reported(windows) => windows
                .iter()
                .map(|window| {
                    let reset = window
                        .resets_at_ms
                        .and_then(|ms| Timestamp::from_millisecond(ms).ok())
                        .map_or(String::new(), |reset| {
                            format!(", {}", format_reset(reset, now, tz))
                        });
                    let mut line =
                        ui::remaining_line(window.label, window.remaining_percent, &reset);
                    line.spans[1].style.fg = Some(window.color(windows));
                    line
                })
                .collect(),
        }
    }

    pub fn compact_line(&self, now: Timestamp) -> Line<'static> {
        self.summary_line(now, true)
    }

    /// Window percentages without per-window reset countdowns, for rows that
    /// already lead with the account's wait timer.
    pub fn windows_line(&self, now: Timestamp) -> Line<'static> {
        self.summary_line(now, false)
    }

    fn summary_line(&self, now: Timestamp, with_reset_timers: bool) -> Line<'static> {
        use ratatui::{style::Style, text::Span};

        let windows = match self {
            Self::Unavailable(_) => return "unavailable".into(),
            Self::NotReported(_) => return "not reported".into(),
            Self::Reported(windows) if windows.is_empty() => return "not reported".into(),
            Self::Reported(windows) => windows,
        };
        let mut spans = Vec::new();
        for window in windows {
            if !spans.is_empty() {
                spans.push(Span::raw(" · "));
            }
            spans.push(Span::raw(format!("{} ", window.label)));
            // A passed reset is not proof that the next allowance is available.
            if window.remaining_percent == 0.0
                && window
                    .resets_at_ms
                    .is_some_and(|at| at <= now.as_millisecond())
            {
                spans.push(Span::raw("refresh needed"));
                continue;
            }
            spans.push(Span::styled(
                format!("{:.0}%", window.remaining_percent),
                Style::default().fg(window.color(windows)),
            ));
            if with_reset_timers
                && window.remaining_percent == 0.0
                && let Some(reset) = window.resets_at_ms
            {
                spans.push(Span::raw(format!(
                    " {} left",
                    format_time_left((reset - now.as_millisecond() + 999) / 1000)
                )));
            }
        }
        Line::from(spans)
    }
}

impl PlanUsage {
    fn monitor_usage(&self) -> MonitorUsage {
        let groups = self.groups.iter().flat_map(|group| {
            let [five_hour, weekly] = group.family.short_labels();
            [(five_hour, group.five_hour), (weekly, group.weekly)]
        });
        MonitorUsage::Reported(
            [("5h", self.five_hour), ("week", self.weekly)]
                .into_iter()
                .chain(groups)
                .filter_map(|(label, window)| {
                    window.map(|window| UsageWindow {
                        label,
                        remaining_percent: window.remaining_percent(),
                        resets_at_ms: window.resets_at_ms,
                    })
                })
                .collect(),
        )
    }

    fn exhausted_until_ms(&self, now_ms: u64) -> Option<u64> {
        [self.five_hour, self.weekly]
            .into_iter()
            .flatten()
            .filter(|window| window.remaining_percent() <= 0.0)
            .filter_map(|window| window.resets_at_ms.and_then(|ms| u64::try_from(ms).ok()))
            .filter(|until| *until > now_ms)
            .max()
    }
}

enum Report {
    Accounts(UsageProvider, Vec<AccountUsage>),
    /// OpenCode Go keeps its own shape: it has a monthly window and a status
    /// per window that the shared shape has no room for.
    OpenCode(Box<OpenCodeUsageResponse>),
}

impl Report {
    fn text(&self, now: Timestamp, tz: &TimeZone, styled: bool) -> String {
        match self {
            Self::Accounts(provider, accounts) => accounts
                .iter()
                .filter_map(|account| match &account.result {
                    AccountResult::Usage { usage } => {
                        let name = format!(
                            "{} `{}`{}",
                            provider.name(),
                            account.name,
                            if account.current { " [current]" } else { "" }
                        );
                        Some(format_plan(&name, usage, now, tz, styled))
                    }
                    AccountResult::Unavailable { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n\n"),
            Self::OpenCode(usage) => crate::providers::opencode::usage::format_text(usage, styled),
        }
    }

    fn json(&self) -> serde_json::Result<Value> {
        match self {
            Self::Accounts(_, accounts) => {
                Ok(serde_json::json!({"accounts": serde_json::to_value(accounts)?}))
            }
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
    let results = fetch_reports(&providers)?;

    let now = Timestamp::now();
    let tz = TimeZone::system();
    let styled = ui::styled(&std::io::stdout());
    let mut code = 0;
    let mut blocks = Vec::new();
    let mut by_provider = Map::new();
    for (provider, result) in providers.into_iter().zip(results) {
        match result {
            Ok(Some(report)) => {
                if let Report::Accounts(_, accounts) = &report {
                    for account in accounts {
                        if let AccountResult::Unavailable { error, .. } = &account.result {
                            eprintln!("{error}");
                            code = 2;
                        }
                    }
                }
                if json {
                    by_provider.insert(provider.id().to_string(), report.json()?);
                } else {
                    let text = report.text(now, &tz, styled);
                    if !text.is_empty() {
                        blocks.push(text);
                    }
                }
            }
            // Asked for by name, a missing sign-in fails the command.
            Ok(None) if only.is_some() => {
                eprintln!("{}", provider.not_signed_in());
                code = code.max(1);
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

/// OpenCode Go reports usage for its current login only.
pub(crate) fn headlines() -> Vec<(&'static str, MonitorUsage)> {
    if crate::accounts::saved_api_key("opencode", None)
        .or_else(config::opencode_api_key)
        .is_none()
    {
        return Vec::new();
    }
    let results = fetch_reports(&[UsageProvider::OpenCode]);
    let headline = match results {
        Ok(results) => match results.into_iter().next() {
            Some(Ok(Some(Report::OpenCode(response)))) => {
                let usage = &response.usage;
                MonitorUsage::Reported(
                    [
                        ("5h", &usage.rolling),
                        ("week", &usage.weekly),
                        ("month", &usage.monthly),
                    ]
                    .into_iter()
                    .filter_map(|(label, window)| {
                        let window = window.as_ref()?;
                        Some(UsageWindow {
                            label,
                            remaining_percent: (100.0 - window.percent?).clamp(0.0, 100.0),
                            resets_at_ms: window
                                .resets_at
                                .as_deref()
                                .and_then(|reset| reset_ms(&Value::from(reset))),
                        })
                    })
                    .collect(),
                )
            }
            Some(Ok(None)) => return Vec::new(),
            _ => MonitorUsage::Unavailable(
                "Usage unavailable. Press r to retry, or run cc-proxy usage opencode.".into(),
            ),
        },
        Err(error) => {
            tracing::warn!(%error, "Could not start the OpenCode usage lookup");
            MonitorUsage::Unavailable(
                "Usage unavailable. Press r to retry, or run cc-proxy usage opencode.".into(),
            )
        }
    };
    vec![("opencode", headline)]
}

/// Structured usage per saved account for the monitor. Call on its background
/// worker, never a request. UUIDs join the results without exposing credentials.
///
/// Asks every account, including the ones an earlier 429 told cc-proxy to leave
/// alone: this is the first read and the `r` key.
pub(crate) fn account_usage() -> HashMap<(String, String), MonitorUsage> {
    lock_waits().clear();
    account_usage_paced()
}

/// Like `account_usage`, for the refresh nobody asked for. An account that
/// answered 429 is skipped until its wait is over, and its row says how long.
pub(crate) fn account_usage_paced() -> HashMap<(String, String), MonitorUsage> {
    let mut usage = HashMap::new();
    for provider in ["claude", "codex", "kimi", "grok", "antigravity"] {
        let unavailable = if matches!(provider, "claude" | "codex" | "kimi" | "antigravity") {
            MonitorUsage::Unavailable(format!(
                "Usage unavailable. Press r to retry, or run cc-proxy usage {provider}."
            ))
        } else {
            MonitorUsage::NotReported(
                "This provider does not report usage. Confirmed wait timers still appear here.",
            )
        };
        match Accounts::new(provider).list() {
            Ok(accounts) => {
                for account in accounts {
                    usage.insert((provider.to_string(), account.id), unavailable.clone());
                }
            }
            Err(error) => {
                tracing::warn!(provider, %error, "Could not read saved accounts for usage");
            }
        }
    }
    let results = match fetch_reports(&[
        UsageProvider::Claude,
        UsageProvider::Codex,
        UsageProvider::Kimi,
        UsageProvider::Antigravity,
    ]) {
        Ok(results) => results,
        Err(error) => {
            tracing::warn!(%error, "Could not start saved-account usage lookups");
            return usage;
        }
    };
    for result in results {
        match result {
            Ok(Some(Report::Accounts(provider, accounts))) => {
                for account in accounts {
                    let report = match account.result {
                        AccountResult::Usage { usage } => usage.monitor_usage(),
                        AccountResult::Unavailable { short, .. } => {
                            MonitorUsage::Unavailable(short)
                        }
                    };
                    usage.insert((provider.id().to_string(), account.id), report);
                }
            }
            Err(error) => tracing::warn!(%error, "Could not read saved-account usage"),
            _ => {}
        }
    }
    remember_last_good(&last_good_path(), &usage);
    usage
}

/// Every label a window can carry, so a saved window gets its `&'static str` back.
const WINDOW_LABELS: [&str; 6] = [
    "5h",
    "week",
    "Gemini 5h",
    "Gemini week",
    "Claude 5h",
    "Claude week",
];

#[derive(Serialize, Deserialize)]
struct SavedWindow {
    label: String,
    remaining_percent: f64,
    resets_at_ms: Option<i64>,
}

/// Keyed `provider/account id`.
type SavedUsage = HashMap<String, Vec<SavedWindow>>;

fn last_good_path() -> PathBuf {
    crate::paths::state_dir().join("usage-last.json")
}

fn read_saved_usage(path: &Path) -> SavedUsage {
    fs::read(path)
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .unwrap_or_default()
}

/// The numbers from each account's last successful lookup. A failed refresh
/// shows these with a warning, instead of replacing them with an error.
pub(crate) fn last_good_usage() -> HashMap<(String, String), Vec<UsageWindow>> {
    restore_usage(read_saved_usage(&last_good_path()))
}

fn restore_usage(saved: SavedUsage) -> HashMap<(String, String), Vec<UsageWindow>> {
    saved
        .into_iter()
        .filter_map(|(key, windows)| {
            let (provider, id) = key.split_once('/')?;
            let windows = windows
                .into_iter()
                .filter_map(|window| {
                    Some(UsageWindow {
                        label: WINDOW_LABELS
                            .into_iter()
                            .find(|label| *label == window.label)?,
                        remaining_percent: window.remaining_percent,
                        resets_at_ms: window.resets_at_ms,
                    })
                })
                .collect();
            Some(((provider.to_string(), id.to_string()), windows))
        })
        .collect()
}

/// Saves the accounts that answered and forgets the ones that are gone.
// ponytail: read, change, write with no lock. Two monitors refreshing at the
// same moment can lose one account's entry until the next refresh.
fn remember_last_good(path: &Path, usage: &HashMap<(String, String), MonitorUsage>) {
    let mut saved = read_saved_usage(path);
    let present: HashSet<String> = usage
        .keys()
        .map(|(provider, id)| format!("{provider}/{id}"))
        .collect();
    saved.retain(|key, _| present.contains(key));
    for ((provider, id), usage) in usage {
        if let MonitorUsage::Reported(windows) = usage
            && !windows.is_empty()
        {
            saved.insert(
                format!("{provider}/{id}"),
                windows
                    .iter()
                    .map(|window| SavedWindow {
                        label: window.label.to_string(),
                        remaining_percent: window.remaining_percent,
                        resets_at_ms: window.resets_at_ms,
                    })
                    .collect(),
            );
        }
    }
    if let Some(folder) = path.parent()
        && fs::create_dir_all(folder).is_ok()
        && let Ok(raw) = serde_json::to_vec(&saved)
        && let Err(error) = crate::fsutil::write_atomic(path, &raw)
    {
        tracing::warn!(%error, "Could not save the last good usage");
    }
}

fn fetch_reports(
    providers: &[UsageProvider],
) -> anyhow::Result<Vec<Result<Option<Report>, String>>> {
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    Ok(runtime.block_on(futures_util::future::join_all(
        providers.iter().map(|provider| fetch(*provider, &client)),
    )))
}

/// None means there are no saved accounts (or no OpenCode API key).
async fn fetch(
    provider: UsageProvider,
    client: &reqwest::Client,
) -> Result<Option<Report>, String> {
    let accounts = match provider {
        UsageProvider::Claude => {
            Accounts::open(provider.id(), &crate::providers::claude::auth::CLI)
        }
        UsageProvider::Codex => Accounts::open(provider.id(), &crate::providers::codex::CODEX_CLI),
        UsageProvider::Kimi => Accounts::open(provider.id(), &crate::providers::kimi::KIMI_CLI),
        UsageProvider::Antigravity => Accounts::open(
            provider.id(),
            &crate::providers::antigravity::ANTIGRAVITY_CLI,
        ),
        UsageProvider::OpenCode => return opencode().await,
    }
    .map_err(|error| format!("{}: {error:#}", provider.name()))?;
    let list = accounts
        .list()
        .map_err(|error| format!("{}: {error:#}", provider.name()))?;
    if list.is_empty() {
        return Ok(None);
    }
    let current = accounts
        .current()
        .map_err(|error| format!("{}: {error:#}", provider.name()))?
        .map(|account| account.id);
    let usage = futures_util::future::join_all(list.into_iter().map(|account| {
        let is_current = current.as_ref() == Some(&account.id);
        async move {
            let started = Instant::now();
            let outcome = match wait_left(provider, &account.id) {
                Some(left) => Err(Failure::Waiting(left)),
                None => fetch_account(provider, &account.id, client).await,
            };
            let result = match outcome {
                Ok(usage) => {
                    end_wait(provider, &account.id);
                    // A spent family can't set or clear a wait for the whole account.
                    if usage.groups.is_empty() {
                        let _ = crate::rotation::Cooldowns::new().sync_account_window(
                            provider.id(),
                            &account.id,
                            usage.exhausted_until_ms(crate::rotation::now_ms()),
                        );
                    }
                    AccountResult::Usage { usage }
                }
                Err(failure) => {
                    if let Failure::Rejected {
                        status: 429,
                        retry_after,
                    } = failure
                    {
                        start_wait(provider, &account.id, retry_after);
                    }
                    // A skipped account asked nothing, so there is nothing to log.
                    if !matches!(failure, Failure::Waiting(_)) {
                        log_failure(provider, &account.id, &failure, started.elapsed());
                    }
                    AccountResult::Unavailable {
                        short: failure.short(provider.name()),
                        error: provider.describe(&account.name, failure),
                    }
                }
            };
            AccountUsage {
                id: account.id,
                name: account.name,
                current: is_current,
                result,
            }
        }
    }))
    .await;
    Ok(Some(Report::Accounts(provider, usage)))
}

async fn fetch_account(
    provider: UsageProvider,
    id: &str,
    client: &reqwest::Client,
) -> Result<PlanUsage, Failure> {
    let usage = match provider {
        UsageProvider::Claude => claude(client, id).await?,
        UsageProvider::Codex => codex(client, id).await?,
        UsageProvider::Kimi => kimi(client, id).await?,
        UsageProvider::Antigravity => antigravity(client, id).await?,
        UsageProvider::OpenCode => return Err(Failure::Unreadable),
    };
    // A free plan has no windows to count, and that is not a broken reply.
    let free = usage.plan.as_deref() == Some("free");
    if !free && usage.five_hour.is_none() && usage.weekly.is_none() && usage.groups.is_empty() {
        return Err(Failure::Unreadable);
    }
    Ok(usage)
}

async fn claude(client: &reqwest::Client, id: &str) -> Result<PlanUsage, Failure> {
    let store = Accounts::new("claude")
        .for_request::<StoredAuth>(Some(id))
        .map_err(|_| Failure::SignInUnusable)?;
    claude_usage(client, &AuthManager::new(store), USAGE_URL).await
}

/// Asks `url` for the usage of the login `manager` holds. A 401 means Claude
/// turned the access token down before its time ran out, so the login is
/// renewed once and the question asked once more. A 403 or a 429 would not
/// change after a renewal.
async fn claude_usage<S: AuthStorage<StoredAuth>>(
    client: &reqwest::Client,
    manager: &AuthManager<S>,
    url: &str,
) -> Result<PlanUsage, Failure> {
    let auth = manager.get_auth().await.map_err(renewal_failure)?;
    // A login saved before cc-proxy asked for the plan learns it here.
    let mut auth = manager.learn_plan(auth).await;
    // Claude's usage endpoint turns a free account away, which would read as a
    // broken sign-in. There is nothing to count: Claude Code can't use it.
    if auth.plan.as_deref() == Some("free") {
        return Ok(PlanUsage {
            plan: auth.plan,
            ..PlanUsage::default()
        });
    }
    let body = match get_json(client, url, claude_headers(&auth.access)?).await {
        Err(Failure::Rejected { status: 401, .. }) => {
            auth = manager
                .force_refresh(&auth.access)
                .await
                .map_err(renewal_failure)?;
            get_json(client, url, claude_headers(&auth.access)?).await?
        }
        body => body?,
    };
    Ok(parse_claude(&body, auth.plan))
}

fn claude_headers(access: &str) -> Result<HeaderMap, Failure> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {access}"))
            .map_err(|_| Failure::Setup("the saved token is not a valid header".into()))?,
    );
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert("anthropic-beta", HeaderValue::from_static(USAGE_BETA));
    Ok(headers)
}

/// Why a saved Claude sign-in couldn't be used. Only a login that is gone, or
/// that Claude turned down, means "sign in again". A Claude that is slow or out
/// of reach doesn't, and a 429 or a 5xx on the renewal is the same answer the
/// usage request would have got.
fn renewal_failure(error: anyhow::Error) -> Failure {
    if let Some(RenewRejected(status)) = error.downcast_ref::<RenewRejected>() {
        return match status.as_u16() {
            status @ (429 | 500..) => Failure::Rejected {
                status,
                retry_after: None,
            },
            _ => Failure::SignInUnusable,
        };
    }
    match error.downcast_ref::<reqwest::Error>() {
        Some(error) if error.is_timeout() => Failure::RenewalTimedOut,
        Some(_) => Failure::Unreachable,
        None => Failure::SignInUnusable,
    }
}

async fn codex(client: &reqwest::Client, id: &str) -> Result<PlanUsage, Failure> {
    let manager = CodexAuthManager::new(codex_tokens::account_store(id));
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
    Ok(parse_chatgpt(&body))
}

async fn kimi(client: &reqwest::Client, id: &str) -> Result<PlanUsage, Failure> {
    let manager = KimiAuthManager::new(
        kimi_tokens::for_request(Some(id)).map_err(|_| Failure::SignInUnusable)?,
    );
    // The Kimi auth manager renews over a blocking client, which must not
    // run on the async runtime.
    let auth = tokio::task::spawn_blocking(move || manager.get_auth())
        .await
        .map_err(|_| Failure::SignInUnusable)?
        .map_err(|_| Failure::SignInUnusable)?;
    let mut headers =
        common_headers(&auth.device_id).map_err(|error| Failure::Setup(error.to_string()))?;
    let bearer = HeaderValue::from_str(&format!("Bearer {}", auth.access))
        .map_err(|_| Failure::Setup("the saved token isn't a valid header".into()))?;
    headers.insert(AUTHORIZATION, bearer);
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    let url = format!("{}/usages", config::kimi_base_url());
    let body = get_json(client, &url, headers).await?;
    Ok(parse_kimi(&body))
}

async fn antigravity(client: &reqwest::Client, id: &str) -> Result<PlanUsage, Failure> {
    use crate::providers::antigravity::{IDE_USER_AGENT, load_valid_auth, quota_request};
    let auth = load_valid_auth(Some(id))
        .await
        .map_err(|_| Failure::SignInUnusable)?;
    let body = read_json(quota_request(client, &auth, IDE_USER_AGENT)).await?;
    Ok(parse_antigravity(&body, auth.plan))
}

async fn opencode() -> Result<Option<Report>, String> {
    let Some(api_key) =
        crate::accounts::saved_api_key("opencode", None).or_else(config::opencode_api_key)
    else {
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
    read_json(client.get(url).headers(headers)).await
}

async fn read_json(request: reqwest::RequestBuilder) -> Result<Value, Failure> {
    let transport_failure = |error: reqwest::Error| {
        if error.is_timeout() {
            Failure::TimedOut
        } else {
            Failure::Unreachable
        }
    };
    let response = request.send().await.map_err(transport_failure)?;
    let status = response.status();
    if !status.is_success() {
        // Seconds only. A date, or anything else, is left out.
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse().ok());
        return Err(Failure::Rejected {
            status: status.as_u16(),
            retry_after,
        });
    }
    let body = response.bytes().await.map_err(transport_failure)?;
    serde_json::from_slice(&body).map_err(|_| Failure::Unreadable)
}

// ---------------------------------------------------------------------------
// Parsers, ported from TabRunner's chrome/src/modules/providers/usage.ts.
// These endpoints are undocumented, so a window that can't be read is left
// out rather than failing the whole reply.
// ---------------------------------------------------------------------------

/// Claude's OAuth usage reports utilization as a percentage, not a fraction.
fn parse_claude(body: &Value, plan: Option<String>) -> PlanUsage {
    let window = |name| {
        let window = body.get(name)?;
        Some(Window {
            used_percent: number(window.get("utilization")?)?.clamp(0.0, 100.0),
            resets_at_ms: window.get("resets_at").and_then(reset_ms),
        })
    };
    PlanUsage {
        plan,
        five_hour: window("five_hour"),
        weekly: window("seven_day"),
        ..Default::default()
    }
}

/// Antigravity keeps a 5-hour and a weekly window for each model family.
fn parse_antigravity(body: &Value, plan: Option<String>) -> PlanUsage {
    let buckets = crate::providers::antigravity::parse_quota_summary(body);
    let window = |family, weekly| {
        let bucket = buckets
            .iter()
            .find(|bucket| bucket.family == family && bucket.weekly == weekly)?;
        Some(Window {
            used_percent: 100.0 - bucket.remaining_fraction * 100.0,
            resets_at_ms: bucket.reset_ms,
        })
    };
    let groups = [Family::Gemini, Family::Claude]
        .into_iter()
        .map(|family| GroupUsage {
            family,
            five_hour: window(family, false),
            weekly: window(family, true),
        })
        .filter(|group| group.five_hour.is_some() || group.weekly.is_some())
        .collect();
    PlanUsage {
        plan,
        groups,
        ..Default::default()
    }
}

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
        ..Default::default()
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
        ..Default::default()
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
    let rows = if usage.groups.is_empty() {
        vec![
            ("5-hour window".to_string(), &usage.five_hour),
            ("Weekly".to_string(), &usage.weekly),
        ]
    } else {
        usage
            .groups
            .iter()
            .flat_map(|group| {
                let name = group.family.name();
                [
                    (format!("{name} 5-hour"), &group.five_hour),
                    (format!("{name} weekly"), &group.weekly),
                ]
            })
            .collect()
    };
    for (label, window) in rows {
        output.push_str(&window_row(
            &label,
            window.as_ref().map(Window::remaining_percent),
            &format_window(window.as_ref(), now, tz),
            styled,
        ));
    }
    output
}

const METER_CELLS: usize = 20;

/// "\n  Weekly: 93% left, …". In a terminal the label is padded and a meter
/// goes before the words, so every window's meter lines up. The meter empties
/// and turns red as the allowance runs out.
pub(crate) fn window_row(label: &str, remaining: Option<f64>, text: &str, styled: bool) -> String {
    if !styled {
        return format!("\n  {label}: {text}");
    }
    let meter = remaining.map_or_else(
        || " ".repeat(METER_CELLS),
        |remaining| ui::remaining_meter(remaining, METER_CELLS),
    );
    let text = match (remaining, text.split_once('%')) {
        (Some(remaining), Some((percent, detail))) => format!(
            "{}{detail}",
            ui::strong(&format!("{percent}%"), ui::remaining_color(remaining), true)
        ),
        _ => text.to_string(),
    };
    format!("\n  {label:<18}{meter}  {text}")
}

fn format_window(window: Option<&Window>, now: Timestamp, tz: &TimeZone) -> String {
    let Some(window) = window else {
        return "not reported".to_string();
    };
    let left = format!("{:.0}% left", window.remaining_percent());
    match window
        .resets_at_ms
        .and_then(|ms| Timestamp::from_millisecond(ms).ok())
    {
        Some(reset) => format!("{left}, {}", format_reset(reset, now, tz)),
        None => left,
    }
}

/// "resets in 2h 10m (17:00)": the time left, then the local clock time,
/// with the weekday when it isn't today and the date when it is a week or
/// more away.
pub(crate) fn format_reset(reset: Timestamp, now: Timestamp, tz: &TimeZone) -> String {
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
pub(crate) fn format_time_left(seconds: i64) -> String {
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::{
        Json, Router,
        http::StatusCode,
        response::IntoResponse,
        routing::{get, post},
    };

    use super::*;
    use crate::auth::InMemoryAuthStore;

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
    fn claude_usage_keeps_reported_windows_and_real_reset_times() {
        let usage = parse_claude(
            &serde_json::json!({
                "five_hour": {"utilization": 20, "resets_at": "2026-10-25T12:00:00Z"},
                "seven_day": {"utilization": 100, "resets_at": "2026-10-28T12:00:00Z"}
            }),
            Some("max".into()),
        );
        assert_eq!(usage.five_hour, window(20.0, ms("2026-10-25T12:00:00Z")));
        assert_eq!(usage.weekly, window(100.0, ms("2026-10-28T12:00:00Z")));
        assert_eq!(
            usage.exhausted_until_ms(0),
            u64::try_from(ms("2026-10-28T12:00:00Z").unwrap()).ok()
        );
        let partial = parse_claude(
            &serde_json::json!({"five_hour": {"utilization": 0}, "seven_day": null}),
            None,
        );
        assert_eq!(partial.five_hour, window(0.0, None));
        assert!(partial.weekly.is_none());
        assert!(
            parse_claude(
                &serde_json::json!({"five_hour": {"utilization": "not a number"}}),
                None
            )
            .five_hour
            .is_none()
        );
    }

    #[test]
    fn antigravity_usage_lists_each_model_family_and_never_sets_an_account_wait() {
        let usage = parse_antigravity(
            &serde_json::json!({"groups": [
                {"displayName": "Gemini Models", "buckets": [
                    {"window": "weekly", "resetTime": "2026-10-30T12:00:00Z", "remainingFraction": 0.88},
                    {"window": "5h", "resetTime": "2026-10-25T12:00:00Z", "remainingFraction": 0}
                ]},
                {"displayName": "Claude and GPT models", "buckets": [
                    {"window": "5h", "resetTime": "2026-10-25T13:00:00Z", "remainingFraction": 1}
                ]},
                {"displayName": "Something new", "buckets": [
                    {"window": "5h", "remainingFraction": 0}
                ]}
            ]}),
            Some("pro".into()),
        );
        assert_eq!(usage.plan.as_deref(), Some("pro"));
        assert_eq!(usage.groups.len(), 2);
        assert_eq!(usage.groups[0].family, Family::Gemini);
        assert_eq!(
            usage.groups[0].five_hour,
            window(100.0, ms("2026-10-25T12:00:00Z"))
        );
        assert_eq!(
            usage.groups[0]
                .weekly
                .map(|weekly| weekly.remaining_percent().round()),
            Some(88.0)
        );
        assert_eq!(usage.groups[1].weekly, None);
        // Family waits are per model, so they never count as an account-wide wait.
        assert_eq!(usage.exhausted_until_ms(0), None);

        let MonitorUsage::Reported(windows) = usage.monitor_usage() else {
            panic!("expected reported usage");
        };
        let labels: Vec<_> = windows.iter().map(|window| window.label).collect();
        assert_eq!(
            labels,
            ["Gemini 5h", "Gemini week", "Claude 5h"],
            "{windows:?}"
        );

        let text = format_plan(
            "Antigravity `main`",
            &usage,
            Timestamp::from_millisecond(0).unwrap(),
            &TimeZone::UTC,
            false,
        );
        assert!(text.contains("\n  Gemini 5-hour: 0% left"), "{text}");
        assert!(text.contains("\n  Claude 5-hour: 100% left"), "{text}");
        assert!(text.contains("\n  Claude weekly: not reported"), "{text}");
        assert!(!text.contains("Weekly:"), "{text}");
        let json = serde_json::to_value(&usage).unwrap();
        assert_eq!(json["groups"][0]["family"], "gemini");
        assert_eq!(json["groups"][1]["family"], "claude");
    }

    #[test]
    fn chatgpt_maps_primary_and_secondary_windows_and_keeps_the_plan() {
        assert_eq!(
            parse_chatgpt(&fixture("chatgpt.json")),
            PlanUsage {
                plan: Some("plus".into()),
                five_hour: window(45.0, ms("2026-08-09T17:00:00Z")),
                weekly: window(7.0, ms("2026-08-15T00:00:00Z")),
                ..Default::default()
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
                ..Default::default()
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
            ..Default::default()
        };
        assert_eq!(
            format_plan("Codex", &usage, now, &tz, false),
            concat!(
                "Codex (plus)\n",
                "  5-hour window: 55% left, resets in 2h 10m (17:00)\n",
                "  Weekly: 93% left, resets in 4d 9h (Mon 00:00)"
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
            ..Default::default()
        };
        assert_eq!(
            format_plan("Kimi", &usage, now, &TimeZone::UTC, false),
            "Kimi\n  5-hour window: 88% left\n  Weekly: not reported"
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
    fn terminal_usage_colors_the_number_and_plain_output_stays_plain() {
        let plain = window_row("5-hour window", Some(0.0), "0% left, resets in 1h", false);
        assert_eq!(plain, "\n  5-hour window: 0% left, resets in 1h");
        let terminal = window_row("5-hour window", Some(0.0), "0% left, resets in 1h", true);
        assert!(terminal.contains(&ui::strong("0%", ui::RED, true)));
        assert!(terminal.ends_with(" left, resets in 1h"));
    }

    #[test]
    fn account_lines_show_what_is_left_without_guessing_missing_ones() {
        let now = "2026-08-12T14:50:00Z".parse().unwrap();
        let usage = PlanUsage {
            plan: Some("plus".into()),
            five_hour: window(45.0, ms("2026-08-12T17:00:00Z")),
            weekly: window(7.0, ms("2026-08-17T00:00:00Z")),
            ..Default::default()
        };
        assert_eq!(
            usage
                .monitor_usage()
                .lines(now, &TimeZone::UTC)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            [
                "5h 55% left, resets in 2h 10m (17:00)",
                "week 93% left, resets in 4d 9h (Mon 00:00)"
            ]
        );
        let usage = PlanUsage {
            five_hour: window(28.0, None),
            ..PlanUsage::default()
        };
        assert_eq!(
            usage
                .monitor_usage()
                .lines(now, &TimeZone::UTC)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["5h 72% left"]
        );
    }

    #[test]
    fn compact_usage_keeps_both_windows_and_counts_down_only_exhausted_ones() {
        let now: Timestamp = "2026-08-12T14:50:00Z".parse().unwrap();
        let usage = PlanUsage {
            five_hour: window(60.0, ms("2026-08-12T17:05:00Z")),
            weekly: window(90.0, ms("2026-08-17T00:00:00Z")),
            ..PlanUsage::default()
        };
        assert_eq!(
            usage.monitor_usage().compact_line(now).to_string(),
            "5h 40% · week 10%"
        );
        let usage = PlanUsage {
            five_hour: window(100.0, ms("2026-08-12T17:05:00Z")),
            weekly: window(100.0, ms("2026-08-12T18:00:00Z")),
            ..PlanUsage::default()
        };
        assert_eq!(
            usage.monitor_usage().compact_line(now).to_string(),
            "5h 0% 2h 15m left · week 0% 3h 10m left"
        );
        let usage = PlanUsage {
            five_hour: window(100.0, ms("2026-08-12T14:00:00Z")),
            ..PlanUsage::default()
        };
        assert_eq!(
            usage.monitor_usage().compact_line(now).to_string(),
            "5h refresh needed"
        );
        let usage = MonitorUsage::Unavailable("Press r to retry.".into());
        assert_eq!(usage.compact_line(now).to_string(), "unavailable");

        // When the weekly allowance is 0%, a full 5h window is dimmed rather than green.
        let blocked = PlanUsage {
            five_hour: window(0.0, ms("2026-08-12T17:05:00Z")),
            weekly: window(100.0, ms("2026-08-17T00:00:00Z")),
            ..PlanUsage::default()
        }
        .monitor_usage();
        let line = blocked.compact_line(now);
        assert_eq!(line.spans[1].style.fg, Some(ui::DIM_WHITE));
        assert_eq!(line.spans[4].style.fg, Some(ui::RED));
        assert_eq!(
            blocked.lines(now, &TimeZone::UTC)[0].spans[1].style.fg,
            Some(ui::DIM_WHITE)
        );
    }

    fn rejected(status: u16) -> Failure {
        Failure::Rejected {
            status,
            retry_after: None,
        }
    }

    #[test]
    fn the_monitor_gets_one_short_line_and_the_command_line_keeps_the_long_one() {
        let timed_out = UsageProvider::Claude.name();
        assert_eq!(
            Failure::TimedOut.short(timed_out),
            "Claude didn't answer in 10 seconds. Press r to retry."
        );
        assert_eq!(
            rejected(401).short("Codex"),
            "Codex rejected the sign-in. Choose Sign in to this account again."
        );
        for failure in [
            Failure::SignInUnusable,
            Failure::TimedOut,
            Failure::RenewalTimedOut,
            Failure::Unreachable,
            Failure::Unreadable,
            rejected(429),
            rejected(502),
            Failure::Waiting(Duration::from_secs(90)),
        ] {
            let short = failure.short("Kimi");
            assert!(short.len() < 90, "{short}");
            assert!(!short.contains("cc-proxy usage"), "{short}");
            assert!(!short.contains("auth login"), "{short}");
        }
    }

    #[test]
    fn a_429_is_waited_out_as_long_as_the_provider_says_but_never_more_than_an_hour() {
        assert_eq!(rate_limit_wait(Some(120)), Duration::from_secs(120));
        assert_eq!(rate_limit_wait(None), RATE_LIMIT_FLOOR);
        assert_eq!(rate_limit_wait(Some(86_400)), RATE_LIMIT_CEILING);
        assert_eq!(rate_limit_wait(Some(0)), Duration::from_secs(1));

        let told = Failure::Rejected {
            status: 429,
            retry_after: Some(120),
        };
        assert_eq!(
            told.short("Claude"),
            "Claude is limiting requests. Try again in 2m."
        );
        assert_eq!(
            Failure::Waiting(Duration::from_secs(61)).short("Claude"),
            "Claude is limiting requests. Try again in 2m."
        );
        assert!(
            UsageProvider::Claude
                .describe("work", told)
                .contains("again in 2m.")
        );

        // The table is what makes the next automatic refresh skip the account.
        let account = "wait-test-account";
        assert_eq!(wait_left(UsageProvider::Claude, account), None);
        start_wait(UsageProvider::Claude, account, Some(120));
        let left = wait_left(UsageProvider::Claude, account).unwrap();
        assert!(
            left > Duration::from_secs(110) && left <= Duration::from_secs(120),
            "{left:?}"
        );
        assert_eq!(wait_left(UsageProvider::Codex, account), None);
        end_wait(UsageProvider::Claude, account);
        assert_eq!(wait_left(UsageProvider::Claude, account), None);
    }

    #[test]
    fn only_a_login_that_is_gone_or_turned_down_says_to_sign_in_again() {
        let renewal = |status| {
            renewal_failure(anyhow::Error::new(RenewRejected(
                StatusCode::from_u16(status).unwrap(),
            )))
        };
        assert!(matches!(renewal(400), Failure::SignInUnusable));
        assert!(matches!(renewal(401), Failure::SignInUnusable));
        assert!(matches!(
            renewal(429),
            Failure::Rejected {
                status: 429,
                retry_after: None
            }
        ));
        assert!(matches!(
            renewal(502),
            Failure::Rejected { status: 502, .. }
        ));
        assert!(matches!(
            renewal_failure(anyhow::anyhow!("No Claude subscription is saved.")),
            Failure::SignInUnusable
        ));
    }

    #[tokio::test]
    async fn a_renewal_with_no_answer_is_a_timeout_and_not_a_sign_in_to_replace() {
        use anyhow::Context as _;
        // A server that takes the connection and never answers.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let silent = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let error = client
            .get(format!("http://{address}"))
            .send()
            .await
            .context("Claude sign-in could not be renewed. Check your connection and retry.")
            .unwrap_err();
        silent.abort();
        assert!(matches!(renewal_failure(error), Failure::RenewalTimedOut));
    }

    /// A stand-in for Claude. `/usage` accepts only the renewed token and turns
    /// every other one down with `rejects` and `Retry-After: 120`.
    async fn claude_stub(rejects: StatusCode) -> (String, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let renewals = Arc::new(AtomicUsize::new(0));
        let asked = Arc::new(AtomicUsize::new(0));
        let (renewed, counted) = (renewals.clone(), asked.clone());
        let app = Router::new()
            .route(
                "/token",
                post(move || {
                    let renewed = renewed.clone();
                    async move {
                        renewed.fetch_add(1, Ordering::SeqCst);
                        Json(serde_json::json!({
                            "access_token": "new-access",
                            "refresh_token": "new-refresh",
                            "expires_in": 3600
                        }))
                    }
                }),
            )
            .route(
                "/usage",
                get(move |headers: axum::http::HeaderMap| {
                    let counted = counted.clone();
                    async move {
                        counted.fetch_add(1, Ordering::SeqCst);
                        if headers["authorization"] == "Bearer new-access" {
                            (
                                StatusCode::OK,
                                Json(serde_json::json!({
                                    "five_hour": {"utilization": 20, "resets_at": "2026-10-25T12:00:00Z"},
                                    "seven_day": {"utilization": 50}
                                })),
                            )
                                .into_response()
                        } else {
                            (rejects, [("retry-after", "120")], Json(serde_json::json!({})))
                                .into_response()
                        }
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{address}"), renewals, asked)
    }

    fn saved_claude_login(base: &str) -> AuthManager<InMemoryAuthStore<StoredAuth>> {
        let store = InMemoryAuthStore::new();
        store
            .save(StoredAuth {
                access: "stale-access".into(),
                refresh: "refresh".into(),
                expires: 4_102_444_800_000,
                email: None,
                account_id: None,
                plan: Some("max".into()),
            })
            .unwrap();
        AuthManager::with_endpoints(store, format!("{base}/token"), format!("{base}/profile"))
    }

    #[tokio::test]
    async fn a_claude_usage_401_renews_the_login_once_and_asks_once_more() {
        let (base, renewals, asked) = claude_stub(StatusCode::UNAUTHORIZED).await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let manager = saved_claude_login(&base);

        let usage = claude_usage(&client, &manager, &format!("{base}/usage"))
            .await
            .unwrap();

        assert_eq!(usage.five_hour, window(20.0, ms("2026-10-25T12:00:00Z")));
        assert_eq!(renewals.load(Ordering::SeqCst), 1);
        assert_eq!(asked.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_claude_usage_403_or_429_keeps_the_login_and_the_retry_after() {
        for status in [StatusCode::FORBIDDEN, StatusCode::TOO_MANY_REQUESTS] {
            let (base, renewals, asked) = claude_stub(status).await;
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            let manager = saved_claude_login(&base);

            let failure = claude_usage(&client, &manager, &format!("{base}/usage"))
                .await
                .unwrap_err();

            assert!(
                matches!(
                    failure,
                    Failure::Rejected { status: got, retry_after: Some(120) } if got == status.as_u16()
                ),
                "{failure:?}"
            );
            assert_eq!(renewals.load(Ordering::SeqCst), 0);
            assert_eq!(asked.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn the_last_good_numbers_survive_a_failed_refresh_and_a_removed_account() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join("usage-last.json");
        let window = |remaining_percent| UsageWindow {
            label: "5h",
            remaining_percent,
            resets_at_ms: Some(1_700_000_000_000),
        };
        let key = |id: &str| ("codex".to_string(), id.to_string());
        let mut usage = HashMap::from([
            (key("a"), MonitorUsage::Reported(vec![window(58.0)])),
            (key("b"), MonitorUsage::Reported(vec![window(12.0)])),
        ]);
        remember_last_good(&path, &usage);

        // `a` fails the next time and `b` was removed.
        usage.remove(&key("b"));
        usage.insert(key("a"), MonitorUsage::Unavailable("Timed out.".into()));
        remember_last_good(&path, &usage);
        let restored = restore_usage(read_saved_usage(&path));
        assert_eq!(restored.get(&key("a")), Some(&vec![window(58.0)]));
        assert!(!restored.contains_key(&key("b")));

        // A file that isn't ours restores nothing and never panics.
        std::fs::write(&path, "not json").unwrap();
        assert!(restore_usage(read_saved_usage(&path)).is_empty());
    }

    #[test]
    fn failures_say_which_account_failed_and_how_to_replace_only_that_sign_in() {
        let message = UsageProvider::Codex.describe("work", rejected(401));
        assert_eq!(
            message,
            "Codex account `work`: Codex didn't accept the sign-in when asked for usage. To replace this sign-in, run `cc-proxy auth login codex --replace --account work`."
        );
        let message = UsageProvider::Kimi.describe("home", Failure::Unreadable);
        assert_eq!(
            message,
            "Kimi account `home`: Kimi's usage reply came back in a format cc-proxy doesn't recognize. Run `cc-proxy usage kimi` again later; if it keeps happening, check for a cc-proxy update. To replace this sign-in, run `cc-proxy auth login kimi --replace --account home`."
        );
    }
}
