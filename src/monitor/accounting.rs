//! Token usage seen by this proxy, not a provider's invoice.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageTotals {
    /// Full input, including cache reads and writes.
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub observed_requests: u64,
    pub unpriced_requests: u64,
    pub estimated_requests: u64,
    pub reported_requests: u64,
    pub estimated_cost_usd: Option<f64>,
    pub reported_cost_usd: Option<f64>,
}

impl UsageTotals {
    /// A request's input and output as shown: what the monitor measured when it
    /// observed any (input then includes the cached share), else the count the
    /// provider reported before that.
    pub fn input_or(&self, reported: Option<u64>) -> Option<u64> {
        if self.observed_requests > 0 {
            Some(self.input_tokens)
        } else {
            reported
        }
    }

    pub fn output_or(&self, reported: Option<u64>) -> Option<u64> {
        if self.observed_requests > 0 {
            Some(self.output_tokens)
        } else {
            reported
        }
    }

    /// `12.4k in / 830 out`, then the cost when there is one.
    pub fn compact(&self) -> String {
        match self.cost_label() {
            Some(cost) => format!("{} · {cost}", self.tokens_label()),
            None => self.tokens_label(),
        }
    }

    pub fn tokens_label(&self) -> String {
        format!(
            "{} in / {} out",
            compact_tokens(self.input_tokens),
            compact_tokens(self.output_tokens)
        )
    }

    /// The cached share of the input, when the provider reported one.
    pub fn cache_label(&self) -> Option<String> {
        (self.cache_read_tokens > 0 || self.cache_write_tokens > 0).then(|| {
            format!(
                "{} read, {} written",
                compact_tokens(self.cache_read_tokens),
                compact_tokens(self.cache_write_tokens)
            )
        })
    }

    /// Dollars, or `None` when nothing was priced. A total that leaves out
    /// requests with no known price says `(partial)`.
    pub fn cost_label(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(cost) = self.reported_cost_usd {
            parts.push(format!("{} reported", money(cost)));
        }
        if let Some(cost) = self.estimated_cost_usd {
            parts.push(format!("{} est.", money(cost)));
        }
        if parts.is_empty() {
            return None;
        }
        let partial = if self.unpriced_requests > 0 {
            " (partial)"
        } else {
            ""
        };
        Some(format!("{}{partial}", parts.join(" + ")))
    }

    /// For a detail row: the cost, or why there is none.
    pub fn cost_detail(&self) -> String {
        match (self.cost_label(), self.observed_requests) {
            (Some(cost), _) => format!("{cost}, at API prices"),
            (None, 0) => "-".into(),
            (None, _) => "no price for this model".into(),
        }
    }
    pub(super) fn replace(&mut self, old: &Self, new: &Self) {
        macro_rules! replace_count {
            ($($field:ident),+ $(,)?) => { $(self.$field = self.$field.saturating_sub(old.$field).saturating_add(new.$field);)+ };
        }
        replace_count!(
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
            observed_requests,
            unpriced_requests,
            estimated_requests,
            reported_requests
        );
        self.estimated_cost_usd = replace_cost(
            self.estimated_cost_usd,
            old.estimated_cost_usd,
            new.estimated_cost_usd,
            self.estimated_requests,
        );
        self.reported_cost_usd = replace_cost(
            self.reported_cost_usd,
            old.reported_cost_usd,
            new.reported_cost_usd,
            self.reported_requests,
        );
    }

    pub(super) fn add(&mut self, other: &Self) {
        self.replace(&Self::default(), other);
    }
}

fn replace_cost(
    total: Option<f64>,
    old: Option<f64>,
    new: Option<f64>,
    samples: u64,
) -> Option<f64> {
    (samples > 0).then(|| (total.unwrap_or(0.0) - old.unwrap_or(0.0) + new.unwrap_or(0.0)).max(0.0))
}

pub fn compact_tokens(tokens: u64) -> String {
    match tokens {
        0..=999 => tokens.to_string(),
        1_000..=999_999 => format!("{:.1}k", tokens as f64 / 1_000.0),
        _ => format!("{:.1}M", tokens as f64 / 1_000_000.0),
    }
}

fn money(cost: f64) -> String {
    if (1e-6..0.01).contains(&cost) {
        "<$0.01".into()
    } else {
        format!("${cost:.2}")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountUsage {
    pub provider: String,
    pub account_id: Option<String>,
    pub usage: UsageTotals,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct AccountKey {
    pub provider: String,
    pub account_id: Option<String>,
}

/// Cumulative observations for one provider attempt. Omitted values keep the
/// previous observation. Anthropic's input is fresh; OpenAI's is full input.
#[derive(Debug, Clone, Default)]
pub struct UsageObservation {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub cache_write_1h_tokens: Option<u64>,
    pub input_is_fresh: bool,
    pub fast: Option<bool>,
    pub us_inference: Option<bool>,
    pub standard_tier: Option<bool>,
    /// Set only by a source whose cost field is known to mean USD for this call.
    pub reported_cost_usd: Option<f64>,
}

impl UsageObservation {
    pub fn anthropic(value: &Value) -> Self {
        Self {
            input_tokens: value.get("input_tokens").and_then(Value::as_u64),
            output_tokens: value.get("output_tokens").and_then(Value::as_u64),
            cache_read_tokens: value.get("cache_read_input_tokens").and_then(Value::as_u64),
            cache_write_tokens: value
                .get("cache_creation_input_tokens")
                .and_then(Value::as_u64),
            cache_write_1h_tokens: value
                .pointer("/cache_creation/ephemeral_1h_input_tokens")
                .and_then(Value::as_u64),
            input_is_fresh: true,
            fast: value
                .get("speed")
                .and_then(Value::as_str)
                .map(|speed| speed == "fast"),
            us_inference: value
                .get("inference_geo")
                .and_then(Value::as_str)
                .map(|geo| geo == "us"),
            standard_tier: standard_tier(value),
            ..Self::default()
        }
    }

    pub fn openai(value: &Value) -> Self {
        Self {
            input_tokens: value
                .get("input_tokens")
                .or_else(|| value.get("prompt_tokens"))
                .and_then(Value::as_u64),
            output_tokens: value
                .get("output_tokens")
                .or_else(|| value.get("completion_tokens"))
                .and_then(Value::as_u64),
            cache_read_tokens: value
                .pointer("/input_tokens_details/cached_tokens")
                .or_else(|| value.pointer("/prompt_tokens_details/cached_tokens"))
                .or_else(|| value.get("cached_tokens"))
                .and_then(Value::as_u64),
            cache_write_tokens: None,
            standard_tier: standard_tier(value),
            ..Self::default()
        }
    }

    pub fn has_tokens(&self) -> bool {
        self.input_tokens.is_some() || self.output_tokens.is_some()
    }

    fn merge(&mut self, incoming: Self) {
        // Counts only grow within one attempt, so a repeated or partial event
        // (a gateway that sends `input_tokens: 0` in its last delta) never
        // lowers what an earlier one reported.
        macro_rules! grow {
            ($($field:ident),+ $(,)?) => { $(if let Some(value) = incoming.$field {
                self.$field = Some(self.$field.map_or(value, |current| current.max(value)));
            })+ };
        }
        grow!(
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
            cache_write_1h_tokens
        );
        macro_rules! latest {
            ($($field:ident),+ $(,)?) => { $(if incoming.$field.is_some() { self.$field = incoming.$field; })+ };
        }
        latest!(fast, us_inference, standard_tier);
        if incoming
            .reported_cost_usd
            .is_some_and(|cost| cost.is_finite() && cost >= 0.0)
        {
            self.reported_cost_usd = incoming.reported_cost_usd;
        }
        self.input_is_fresh = incoming.input_is_fresh;
    }

    fn totals(&self, model: Option<&str>, text_endpoint: bool) -> UsageTotals {
        if !self.has_tokens() && self.reported_cost_usd.is_none() {
            return UsageTotals::default();
        }
        let read = self.cache_read_tokens.unwrap_or(0);
        let write = self.cache_write_tokens.unwrap_or(0);
        let full_input = if self.input_is_fresh {
            self.input_tokens
                .unwrap_or(0)
                .saturating_add(read)
                .saturating_add(write)
        } else {
            self.input_tokens.unwrap_or(0)
        };
        let read = read.min(full_input);
        let write = write.min(full_input.saturating_sub(read));
        let estimate = if self.reported_cost_usd.is_none()
            && text_endpoint
            && self.input_tokens.is_some()
            && self.output_tokens.is_some()
        {
            model.and_then(|model| {
                estimate(
                    model,
                    full_input,
                    self.output_tokens.unwrap_or(0),
                    read,
                    write,
                    self,
                )
            })
        } else {
            None
        };
        let reported = self.reported_cost_usd;
        UsageTotals {
            input_tokens: full_input,
            output_tokens: self.output_tokens.unwrap_or(0),
            cache_read_tokens: read,
            cache_write_tokens: write,
            observed_requests: 1,
            unpriced_requests: u64::from(estimate.is_none() && reported.is_none()),
            estimated_requests: u64::from(estimate.is_some()),
            reported_requests: u64::from(reported.is_some()),
            estimated_cost_usd: estimate,
            reported_cost_usd: reported,
        }
    }
}

fn standard_tier(value: &Value) -> Option<bool> {
    value
        .get("service_tier")
        .and_then(Value::as_str)
        .map(|tier| matches!(tier, "standard" | "default" | "auto"))
}

pub(super) struct Recount {
    pub old: UsageTotals,
    pub new: UsageTotals,
    pub old_account: Option<AccountKey>,
    pub account: Option<AccountKey>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct RequestAccounting {
    observation: UsageObservation,
    rich: bool,
    pub contribution: UsageTotals,
    pub prior: UsageTotals,
    pub account: Option<AccountKey>,
}

impl RequestAccounting {
    pub fn observe(&mut self, usage: UsageObservation, rich: bool) {
        if rich {
            if !self.rich {
                self.observation = UsageObservation::default();
            }
            self.rich = true;
            self.observation.merge(usage);
        } else if !self.rich {
            self.observation.merge(usage);
        }
    }

    pub fn totals(&self, model: Option<&str>, text_endpoint: bool) -> UsageTotals {
        self.observation.totals(model, text_endpoint)
    }

    /// What this request now adds to its account and session, as the
    /// `(old, new)` contributions to swap, or `None` when nothing changed.
    pub fn recount(
        &mut self,
        model: Option<&str>,
        text_endpoint: bool,
        legacy: UsageObservation,
        account: Option<AccountKey>,
    ) -> Option<Recount> {
        self.observe(legacy, false);
        let next = self.totals(model, text_endpoint);
        if next == self.contribution && account == self.account {
            return None;
        }
        Some(Recount {
            old: std::mem::replace(&mut self.contribution, next.clone()),
            new: next,
            old_account: std::mem::replace(&mut self.account, account.clone()),
            account,
        })
    }

    pub fn all_totals(&self) -> UsageTotals {
        let mut usage = self.prior.clone();
        usage.add(&self.contribution);
        usage
    }

    pub fn seal(&mut self) {
        self.prior.add(&self.contribution);
        self.contribution = UsageTotals::default();
        self.observation = UsageObservation::default();
        self.rich = false;
        self.account = None;
    }
}

#[derive(Clone, Copy)]
struct Price {
    input: f64,
    output: f64,
    read: f64,
    write: f64,
    write_1h: f64,
}

// ponytail: a dated table, checked 2026-10-26 against the official pricing
// pages below. Unknown models stay unpriced. Refresh this table as prices change.
// https://platform.claude.com/docs/en/about-claude/pricing
// https://developers.openai.com/api/docs/pricing
fn price(model: &str, input_tokens: u64) -> Option<Price> {
    let model = snapshot_base(model);
    let (input, output, read, write, write_1h) = match model {
        "claude-fable-5-1" | "claude-mythos-5-1" => (10.0, 50.0, 0.25, 12.5, 20.0),
        "claude-fable-5" => (10.0, 50.0, 1.0, 12.5, 20.0),
        "claude-opus-5-5" => (4.0, 20.0, 0.2, 5.0, 8.0),
        "claude-opus-5" | "claude-opus-4-8" | "claude-opus-4-7" | "claude-opus-4-6"
        | "claude-opus-4-5" => (5.0, 25.0, 0.5, 6.25, 10.0),
        "claude-sonnet-5-5" => (2.0, 10.0, 0.1, 2.5, 4.0),
        "claude-sonnet-5" => (2.0, 10.0, 0.2, 2.5, 4.0),
        "claude-sonnet-4-6" => (3.0, 15.0, 0.3, 3.75, 6.0),
        "claude-haiku-5-5" if input_tokens > 100_000 => (0.5, 2.5, 0.05, 0.625, 1.0),
        "claude-haiku-5-5" => (0.1, 0.5, 0.01, 0.125, 0.2),
        "claude-haiku-4-5" => (1.0, 5.0, 0.1, 1.25, 2.0),
        "gpt-5.5" if input_tokens > 272_000 => (10.0, 45.0, 1.0, 10.0, 10.0),
        "gpt-5.5" => (5.0, 30.0, 0.5, 5.0, 5.0),
        "gpt-5.4" if input_tokens > 272_000 => (5.0, 22.5, 0.5, 5.0, 5.0),
        "gpt-5.4" => (2.5, 15.0, 0.25, 2.5, 2.5),
        "gpt-5.4-mini" => (0.75, 4.5, 0.075, 0.75, 0.75),
        "gpt-5.3-codex" => (1.75, 14.0, 0.175, 1.75, 1.75),
        "gpt-5" => (1.25, 10.0, 0.125, 1.25, 1.25),
        "gpt-5-mini" => (0.25, 2.0, 0.025, 0.25, 0.25),
        "gpt-4o" => (2.5, 10.0, 1.25, 2.5, 2.5),
        "gpt-4o-mini" => (0.15, 0.6, 0.075, 0.15, 0.15),
        _ => return None,
    };
    Some(Price {
        input,
        output,
        read,
        write,
        write_1h,
    })
}

fn snapshot_base(model: &str) -> &str {
    if model.len() > 9 {
        let (base, date) = model.split_at(model.len() - 9);
        if date.starts_with('-') && date[1..].bytes().all(|c| c.is_ascii_digit()) {
            return base;
        }
    }
    if model.len() > 11 {
        let (base, date) = model.split_at(model.len() - 11);
        if date.starts_with('-')
            && date.as_bytes()[5] == b'-'
            && date.as_bytes()[8] == b'-'
            && date[1..]
                .bytes()
                .enumerate()
                .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
        {
            return base;
        }
    }
    model
}

fn estimate(
    model: &str,
    input: u64,
    output: u64,
    read: u64,
    write: u64,
    usage: &UsageObservation,
) -> Option<f64> {
    if usage.standard_tier == Some(false) {
        return None;
    }
    let rates = price(model, input)?;
    let fresh = input.saturating_sub(read).saturating_sub(write);
    let long_write = usage.cache_write_1h_tokens.unwrap_or(0).min(write);
    let fast = if usage.fast == Some(true) {
        if !matches!(
            snapshot_base(model),
            "claude-opus-5-5" | "claude-opus-5" | "claude-opus-4-8"
        ) {
            return None;
        }
        2.0
    } else {
        1.0
    };
    let geo = if usage.us_inference == Some(true) {
        1.1
    } else {
        1.0
    };
    Some(
        (fresh as f64 * rates.input
            + output as f64 * rates.output
            + read as f64 * rates.read
            + write.saturating_sub(long_write) as f64 * rates.write
            + long_write as f64 * rates.write_1h)
            / 1_000_000.0
            * fast
            * geo,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::{EndpointKind, MonitorHandle};
    use serde_json::json;

    #[test]
    fn normalized_input_and_cache_rates_do_not_double_count() {
        let anthropic = UsageObservation::anthropic(
            &json!({"input_tokens": 20, "output_tokens": 10, "cache_read_input_tokens": 70, "cache_creation_input_tokens": 10}),
        );
        let usage = anthropic.totals(Some("claude-opus-5-5"), true);
        assert_eq!(
            (
                usage.input_tokens,
                usage.cache_read_tokens,
                usage.cache_write_tokens
            ),
            (100, 70, 10)
        );
        assert!((usage.estimated_cost_usd.unwrap() - 0.000344).abs() < 1e-10);
        let openai = UsageObservation::openai(
            &json!({"input_tokens": 100, "output_tokens": 10, "input_tokens_details": {"cached_tokens": 70}, "output_tokens_details": {"reasoning_tokens": 6}}),
        );
        let usage = openai.totals(Some("gpt-5.4"), true);
        assert_eq!(
            (
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_tokens
            ),
            (100, 10, 70)
        );
        assert!((usage.estimated_cost_usd.unwrap() - 0.0002425).abs() < 1e-10);
    }

    #[test]
    fn unknown_siblings_and_non_text_requests_have_no_estimate() {
        let observation =
            UsageObservation::openai(&json!({"input_tokens": 100, "output_tokens": 10}));
        for model in [
            "gpt-6.1-sol",
            "gpt-5.4-fast",
            "claude-opus-5-6",
            "claude-sonnet-5-5-turbo",
        ] {
            assert_eq!(
                observation.totals(Some(model), true).estimated_cost_usd,
                None,
                "{model}"
            );
        }
        assert!(
            observation
                .totals(Some("gpt-5.4-2026-03-05"), true)
                .estimated_cost_usd
                .is_some()
        );
        assert_eq!(
            observation
                .totals(Some("gpt-5.4"), false)
                .estimated_cost_usd,
            None
        );
    }

    #[test]
    fn a_late_cache_split_and_report_replace_not_add_to_the_estimate() {
        let mut record = RequestAccounting::default();
        record.observe(
            UsageObservation::openai(&json!({"input_tokens": 100, "output_tokens": 10})),
            true,
        );
        let first = record.totals(Some("gpt-5.4"), true);
        let mut aggregate = UsageTotals::default();
        aggregate.add(&first);
        record.observe(UsageObservation::openai(&json!({"input_tokens": 100, "output_tokens": 10, "input_tokens_details": {"cached_tokens": 90}})), true);
        let cached = record.totals(Some("gpt-5.4"), true);
        aggregate.replace(&first, &cached);
        assert!(aggregate.estimated_cost_usd < first.estimated_cost_usd);
        assert_eq!(aggregate.observed_requests, 1);
        record.observe(
            UsageObservation {
                reported_cost_usd: Some(0.12),
                ..UsageObservation::openai(&json!({}))
            },
            true,
        );
        let reported = record.totals(Some("gpt-5.4"), true);
        aggregate.replace(&cached, &reported);
        assert_eq!(aggregate.estimated_cost_usd, None);
        assert_eq!(aggregate.reported_cost_usd, Some(0.12));
    }

    #[test]
    fn clamps_cache_slices_and_accounts_for_known_tiers() {
        let usage = UsageObservation::openai(
            &json!({"input_tokens": 10, "output_tokens": 1, "input_tokens_details": {"cached_tokens": 900}}),
        );
        assert_eq!(usage.totals(Some("gpt-5.4"), true).cache_read_tokens, 10);
        let short =
            UsageObservation::anthropic(&json!({"input_tokens": 100_000, "output_tokens": 0}));
        let long =
            UsageObservation::anthropic(&json!({"input_tokens": 100_001, "output_tokens": 0}));
        assert!(
            long.totals(Some("claude-haiku-5-5"), true)
                .estimated_cost_usd
                .unwrap()
                > short
                    .totals(Some("claude-haiku-5-5"), true)
                    .estimated_cost_usd
                    .unwrap()
                    * 4.0
        );
        let long_write = UsageObservation::anthropic(
            &json!({"input_tokens": 0, "output_tokens": 0, "cache_creation_input_tokens": 100, "cache_creation": {"ephemeral_1h_input_tokens": 100}}),
        );
        let cost = long_write
            .totals(Some("claude-opus-5-5"), true)
            .estimated_cost_usd
            .unwrap();
        assert!((cost - 0.0008).abs() < 1e-12);
    }

    fn started(monitor: &MonitorHandle, id: &str, session: &str, endpoint: EndpointKind) {
        monitor.request_started(id, Some(session.into()), None, endpoint);
    }

    fn account(monitor: &MonitorHandle, provider: &str) -> UsageTotals {
        monitor
            .snapshot()
            .account_usage
            .into_iter()
            .find(|usage| usage.provider == provider)
            .map(|usage| usage.usage)
            .unwrap_or_default()
    }

    #[test]
    fn repeated_stream_observations_count_once_per_account_and_session() {
        let monitor = MonitorHandle::new(10);
        started(&monitor, "r1", "s1", EndpointKind::Messages);
        monitor.provider_selected("r1", "claude", "claude-opus-5-5", None);
        monitor.account_selected("r1", "acct-1", "work");
        monitor.model_resolved("r1", "claude-opus-5-5");
        let usage = UsageObservation::anthropic(
            &json!({"input_tokens": 20, "output_tokens": 10, "cache_read_input_tokens": 70, "cache_creation_input_tokens": 10}),
        );
        for _ in 0..3 {
            monitor.usage_observed("r1", usage.clone());
            monitor.stream_progress("r1", 10, 1, Some(20), Some(10));
        }
        monitor.request_completed("r1", 200, Some(20), Some(10));

        let state = monitor.snapshot();
        assert_eq!(state.sessions[0].usage.input_tokens, 100);
        assert_eq!(state.sessions[0].usage.observed_requests, 1);
        let shown = crate::monitor::snapshot::MonitorSnapshot::from(state.clone());
        // The rows show the full input, the cached share included.
        assert_eq!(shown.recent[0].input_tokens, Some(100));
        assert_eq!(shown.recent[0].output_tokens, Some(10));
        assert_eq!(state.account_usage.len(), 1);
        assert_eq!(state.account_usage[0].account_id.as_deref(), Some("acct-1"));
        assert_eq!(state.account_usage[0].usage.cache_read_tokens, 70);
        assert!(state.account_usage[0].usage.estimated_cost_usd.is_some());
    }

    #[test]
    fn count_tokens_is_not_spend_and_does_not_turn_into_session_tokens() {
        let monitor = MonitorHandle::new(10);
        started(&monitor, "count", "s1", EndpointKind::CountTokens);
        monitor.provider_selected("count", "claude", "claude-opus-5-5", None);
        monitor.model_resolved("count", "claude-opus-5-5");
        monitor.usage_updated("count", Some(5_000), None);
        monitor.request_completed("count", 200, Some(5_000), None);

        let state = monitor.snapshot();
        assert!(state.account_usage.is_empty());
        assert_eq!(state.sessions[0].usage.input_tokens, 0);
        assert_eq!(state.recent[0].input_tokens, Some(5_000));
    }

    #[test]
    fn a_late_cache_split_lowers_the_request_and_its_totals() {
        let monitor = MonitorHandle::new(10);
        started(&monitor, "r1", "s1", EndpointKind::Responses);
        monitor.provider_selected("r1", "codex", "gpt-5.4", None);
        monitor.account_selected("r1", "codex-1", "main");
        monitor.model_resolved("r1", "gpt-5.4");
        monitor.stream_progress("r1", 1, 1, Some(100), Some(10));
        monitor.request_completed("r1", 200, None, None);
        let before = account(&monitor, "codex").estimated_cost_usd.unwrap();

        monitor.usage_observed(
            "r1",
            UsageObservation::openai(
                &json!({"input_tokens": 100, "output_tokens": 10, "input_tokens_details": {"cached_tokens": 90}}),
            ),
        );
        let after = account(&monitor, "codex");
        assert!(after.estimated_cost_usd.unwrap() < before);
        assert_eq!((after.input_tokens, after.cache_read_tokens), (100, 90));
        assert_eq!(after.observed_requests, 1);
    }

    #[test]
    fn totals_survive_request_eviction_and_a_renamed_account() {
        let monitor = MonitorHandle::new(1);
        for (id, tokens) in [("r1", 100), ("r2", 200)] {
            started(&monitor, id, "s1", EndpointKind::Messages);
            monitor.provider_selected(id, "claude", "claude-opus-5-5", None);
            monitor.account_selected(
                id,
                "acct-1",
                if id == "r1" { "old name" } else { "new name" },
            );
            monitor.model_resolved(id, "claude-opus-5-5");
            monitor.usage_observed(
                id,
                UsageObservation::anthropic(&json!({"input_tokens": tokens, "output_tokens": 1})),
            );
            monitor.request_completed(id, 200, None, None);
        }

        let state = monitor.snapshot();
        assert_eq!(state.recent.len(), 1);
        assert_eq!(state.account_usage.len(), 1);
        assert_eq!(state.account_usage[0].usage.input_tokens, 300);
        assert_eq!(state.account_usage[0].usage.observed_requests, 2);
    }

    #[test]
    fn a_fallback_cost_belongs_to_the_provider_that_answered() {
        let monitor = MonitorHandle::new(10);
        started(&monitor, "r1", "s1", EndpointKind::Messages);
        monitor.provider_selected("r1", "codex", "claude-opus-5-5", None);
        monitor.account_selected("r1", "codex-1", "main");
        monitor.model_resolved("r1", "gpt-5.4");
        monitor.provider_switched("r1", "claude", "claude-opus-5-5");
        monitor.account_selected("r1", "claude-1", "me@example.com");
        monitor.model_resolved("r1", "claude-opus-5-5");
        monitor.usage_observed(
            "r1",
            UsageObservation::anthropic(&json!({"input_tokens": 1000, "output_tokens": 100})),
        );
        monitor.request_completed("r1", 200, None, None);

        assert_eq!(account(&monitor, "codex"), UsageTotals::default());
        assert_eq!(account(&monitor, "claude").input_tokens, 1_000);
    }

    #[test]
    fn a_provider_that_never_names_its_model_gets_no_price() {
        let monitor = MonitorHandle::new(10);
        started(&monitor, "r1", "s1", EndpointKind::Messages);
        monitor.provider_selected("r1", "kimi", "claude-opus-5-5", None);
        monitor.usage_updated("r1", Some(1_000), Some(100));
        monitor.request_completed("r1", 200, None, None);

        let usage = account(&monitor, "kimi");
        assert_eq!(usage.estimated_cost_usd, None);
        assert_eq!((usage.input_tokens, usage.unpriced_requests), (1_000, 1));
        assert_eq!(usage.cost_detail(), "no price for this model");
    }

    #[test]
    fn totals_round_trip_and_old_snapshots_default() {
        let monitor = MonitorHandle::new(10);
        started(&monitor, "r1", "s1", EndpointKind::Messages);
        monitor.provider_selected("r1", "claude", "claude-opus-5-5", None);
        monitor.model_resolved("r1", "claude-opus-5-5");
        monitor.usage_observed(
            "r1",
            UsageObservation::anthropic(&json!({"input_tokens": 10, "output_tokens": 1})),
        );
        let snapshot = crate::monitor::snapshot::MonitorSnapshot::from(monitor.snapshot());
        let decoded: crate::monitor::snapshot::MonitorSnapshot =
            serde_json::from_str(&serde_json::to_string(&snapshot).unwrap()).unwrap();
        assert_eq!(decoded, snapshot);

        let mut legacy = serde_json::to_value(&snapshot).unwrap();
        legacy.as_object_mut().unwrap().remove("account_usage");
        legacy["sessions"][0]
            .as_object_mut()
            .unwrap()
            .remove("usage");
        let decoded: crate::monitor::snapshot::MonitorSnapshot =
            serde_json::from_value(legacy).unwrap();
        assert!(decoded.account_usage.is_empty());
        assert_eq!(decoded.sessions[0].usage, UsageTotals::default());
    }
}
