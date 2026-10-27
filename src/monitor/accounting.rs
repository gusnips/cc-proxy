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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceTier {
    Standard,
    Fast,
    Flex,
    Other,
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
    pub service_tier: Option<ServiceTier>,
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
            service_tier: service_tier(value),
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
            cache_write_tokens: value
                .pointer("/input_tokens_details/cache_write_tokens")
                .and_then(Value::as_u64),
            service_tier: service_tier(value),
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
        latest!(fast, us_inference, service_tier);
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

fn service_tier(value: &Value) -> Option<ServiceTier> {
    value
        .get("service_tier")
        .and_then(Value::as_str)
        .map(|tier| match tier {
            "standard" | "default" | "auto" => ServiceTier::Standard,
            "fast" | "priority" => ServiceTier::Fast,
            "flex" => ServiceTier::Flex,
            _ => ServiceTier::Other,
        })
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
    fast_multiplier: f64,
    flex_multiplier: f64,
}

// ponytail: a dated table ported from ccusage (models.dev, LiteLLM, and
// service-tier multiplier overrides), checked 2026-10-27. Unknown models stay
// unpriced. Refresh this table as prices change.
fn price(model: &str, input_tokens: u64) -> Option<Price> {
    let canonical = canonical_model_id(model)?;
    let (input, output, read, write) = base_rates(&canonical, input_tokens)?;
    Some(Price {
        input,
        output,
        read,
        write,
        write_1h: input * 2.0,
        fast_multiplier: fast_multiplier(&canonical).unwrap_or(1.0),
        flex_multiplier: flex_multiplier(&canonical).unwrap_or(1.0),
    })
}

fn base_rates(model: &str, input_tokens: u64) -> Option<(f64, f64, f64, f64)> {
    let rates = match model {
        "claude-haiku-5-5" if input_tokens > 100_000 => (0.5, 2.5, 0.05, 0.625),
        "claude-sonnet-4" if input_tokens > 200_000 => (6.0, 22.5, 0.6, 7.5),
        "gemini-2-5-computer-use-preview-10-2025" if input_tokens > 200_000 => {
            (2.5, 15.0, 0.125, 3.125)
        }
        "gemini-2-5-pro" if input_tokens > 200_000 => (2.5, 15.0, 0.25, 3.125),
        "gemini-3-1-pro"
        | "gemini-3-1-pro-preview"
        | "gemini-3-1-pro-preview-customtools"
        | "gemini-3-pro"
        | "gemini-pro-latest"
            if input_tokens > 200_000 =>
        {
            (4.0, 18.0, 0.4, 5.0)
        }
        "gemini-3-pro-preview" if input_tokens > 200_000 => (4.0, 18.0, 0.2, 5.0),
        "glm-4-6v" if input_tokens > 32_000 => (0.29879, 0.89637, 0.05976, 0.0),
        "glm-5v-turbo" if input_tokens > 32_000 => (1.04577, 3.88427, 0.26891, 0.0),
        "gpt-5-4" if input_tokens > 272_000 => (5.0, 22.5, 0.5, 5.0),
        "gpt-5-4-pro" | "gpt-5-5-pro" if input_tokens > 272_000 => (60.0, 270.0, 3.0, 75.0),
        "gpt-5-5" if input_tokens > 272_000 => (10.0, 45.0, 1.0, 10.0),
        "gpt-5-6" | "gpt-5-6-sol" | "gpt-daybreak-blue-latest" if input_tokens > 272_000 => {
            (8.0, 30.0, 0.8, 10.0)
        }
        "gpt-5-6-luna" if input_tokens > 272_000 => (0.4, 1.8, 0.04, 0.5),
        "gpt-5-6-terra" if input_tokens > 272_000 => (4.0, 18.0, 0.4, 5.0),
        "gpt-6-1-sol" if input_tokens > 272_000 => (4.0, 15.0, 0.2, 5.0),
        "gpt-6-1-terra" if input_tokens > 272_000 => (2.0, 7.5, 0.1, 2.5),
        "gpt-6-astra" if input_tokens > 272_000 => (20.0, 75.0, 2.0, 25.0),
        "gpt-6-luna" if input_tokens > 272_000 => (0.2, 0.75, 0.02, 0.25),
        "gpt-6-sol" if input_tokens > 272_000 => (4.0, 15.0, 0.4, 5.0),
        "grok-4-2-beta" if input_tokens > 200_000 => (4.0, 12.0, 0.4, 4.0),
        "grok-4-20" | "grok-4-20-multi-agent" if input_tokens > 200_000 => {
            (2.83, 5.67, 0.45, 3.5375)
        }
        "grok-4-20-0309-non-reasoning"
        | "grok-4-20-0309-reasoning"
        | "grok-4-20-beta-0309-non-reasoning"
        | "grok-4-20-beta-0309-reasoning"
        | "grok-4-20-multi-agent-0309"
        | "grok-4-3"
            if input_tokens > 200_000 =>
        {
            (2.5, 5.0, 0.4, 3.125)
        }
        "grok-4-5" if input_tokens > 200_000 => (4.0, 12.0, 0.6, 5.0),
        "grok-4-6" | "grok-4-7" if input_tokens > 200_000 => (4.0, 12.0, 1.0, 5.0),
        "grok-build-0-1" if input_tokens > 200_000 => (2.0, 4.0, 0.4, 2.5),
        "minimax-m3" if input_tokens > 512_000 => (0.6, 2.4, 0.12, 0.75),
        "qwen-flash" if input_tokens > 256_000 => (0.25, 2.0, 0.05, 0.3125),
        "qwen-plus" if input_tokens > 256_000 => (1.2, 3.6, 0.24, 1.5),
        "qwen3-5-plus" if input_tokens > 256_000 => (0.5, 3.0, 0.04, 0.625),
        "qwen3-6-flash" if input_tokens > 256_000 => (1.0, 4.0, 0.1, 1.25),
        "qwen3-6-plus" if input_tokens > 256_000 => (2.0, 6.0, 0.2, 2.5),
        "qwen3-7-flash" if input_tokens > 32_000 => (0.1, 0.4, 0.02, 0.125),
        "qwen3-7-plus" if input_tokens > 256_000 => (1.2, 4.8, 0.24, 1.5),
        "qwen3-coder-30b-a3b-instruct" if input_tokens > 32_000 => (0.75, 3.75, 0.045, 0.9375),
        "qwen3-coder-480b-a35b-instruct" if input_tokens > 32_000 => (2.7, 13.5, 0.15, 3.375),
        "qwen3-coder-flash" if input_tokens > 32_000 => (0.5, 2.5, 0.1, 0.625),
        "qwen3-max-preview" if input_tokens > 32_000 => {
            (1.44927536, 5.79710145, 0.08695652, 1.8115942)
        }
        "qwen3-vl-plus" if input_tokens > 32_000 => (0.3, 2.4, 0.06, 0.375),
        "claude-3-5-haiku" => (0.8, 4.0, 0.08, 1.0),
        "claude-3-5-sonnet"
        | "claude-3-5-sonnet-v2"
        | "claude-3-7-sonnet"
        | "claude-3-sonnet"
        | "claude-sonnet-4"
        | "claude-sonnet-4-5"
        | "claude-sonnet-4-6"
        | "grok-4-0709" => (3.0, 15.0, 0.3, 3.75),
        "claude-3-haiku" => (0.25, 1.25, 0.03, 0.3),
        "claude-3-opus"
        | "claude-opus-4"
        | "claude-opus-4-1"
        | "claude-opus-4-1-20250805-thinking" => (15.0, 75.0, 1.5, 18.75),
        "claude-fable-5"
        | "claude-mythos-5"
        | "claude-opus-4-8-fast"
        | "claude-opus-5-fast"
        | "gpt-6-astra" => (10.0, 50.0, 1.0, 12.5),
        "claude-fable-5-1" | "claude-mythos-5-1" => (10.0, 50.0, 0.25, 12.5),
        "claude-haiku-4-5" | "qwen3-coder-plus" => (1.0, 5.0, 0.1, 1.25),
        "claude-haiku-5-5" | "gpt-6-luna" => (0.1, 0.5, 0.01, 0.125),
        "claude-opus-4-5" | "claude-opus-4-6" | "claude-opus-4-7" | "claude-opus-4-8"
        | "claude-opus-5" => (5.0, 25.0, 0.5, 6.25),
        "claude-opus-5-5" | "gemini-4-argon-contributor" | "muse-wonder-1-0" => {
            (4.0, 20.0, 0.2, 5.0)
        }
        "claude-opus-5-5-fast" => (8.0, 40.0, 0.4, 10.0),
        "claude-sonnet-5" | "gpt-6-sol" | "grok-4-1" => (2.0, 10.0, 0.2, 2.5),
        "claude-sonnet-5-5" | "gemini-4-argon" | "gpt-6-1-sol" => (2.0, 10.0, 0.1, 2.5),
        "codestral-2" | "codestral-2501" | "codestral-2508" | "codestral-latest" => {
            (0.3, 0.9, 0.03, 0.375)
        }
        "deepseek-3-2" => (0.5, 1.6, 0.15, 0.625),
        "deepseek-4-flash" | "deepseek-v4-flash-speed" => (0.14, 0.28, 0.028, 0.175),
        "deepseek-chat" | "deepseek-chat-cheaper" => (0.1, 0.425, 0.05, 0.125),
        "deepseek-flash" | "deepseek-v4-flash" | "deepseek-v4-flash-vision-exp" => {
            (0.15, 0.6, 0.003, 0.1875)
        }
        "deepseek-r1" => (0.574, 2.294, 0.0574, 0.7175),
        "deepseek-r1-0528" => (0.69552, 2.78208, 0.185472, 0.8694),
        "deepseek-r1-distill-llama-70b" | "deepseek-r1-distill-qwen-32b" => {
            (0.287, 0.861, 0.0287, 0.35875)
        }
        "deepseek-r1-distill-qwen-14b" => (0.144, 0.431, 0.0144, 0.18),
        "deepseek-r1-distill-qwen-7b" => (0.072, 0.144, 0.0072, 0.09),
        "deepseek-reasoner" | "deepseek-v3" => (0.56, 1.68, 0.07, 0.7),
        "deepseek-tng-r1t2-chimera" | "minimax-m2" | "minimax-m2-1" | "minimax-m2-5" => {
            (0.3, 1.2, 0.03, 0.375)
        }
        "deepseek-v3-0324" => (0.2, 0.77, 0.135, 0.25),
        "deepseek-v3-1" => (0.574, 1.721, 0.0574, 0.7175),
        "deepseek-v3-1-terminus" => (0.27, 1.0, 0.216, 0.3375),
        "deepseek-v3-2" => (0.28, 0.42, 0.03, 0.35),
        "deepseek-v3-2-exp" => (0.287, 0.431, 0.0287, 0.35875),
        "deepseek-v3-2-speciale" => (0.58, 1.68, 0.058, 0.725),
        "deepseek-v4-1-flash" => (0.31, 1.23, 0.01, 0.3875),
        "deepseek-v4-1-flash-1017" => (0.203, 0.609, 0.006, 0.25375),
        "deepseek-v4-1-flash-552b" => (0.280375, 1.1215, 0.11215, 0.35046875),
        "deepseek-v4-1-flash-fast" => (0.25, 1.2, 0.005, 0.0),
        "deepseek-v4-1-flash-flex" => (0.0975, 0.39, 0.00975, 0.121875),
        "deepseek-v4-flash-0731" => (0.44, 1.32, 0.088, 0.55),
        "deepseek-v4-flash-0731-fast" => (0.28, 1.4, 0.07, 0.35),
        "deepseek-v4-flash-flex" => (0.091, 0.182, 0.0182, 0.11375),
        "deepseek-v4-flash-ga-260731" => (0.4453, 1.3359, 0.01484, 0.556625),
        "deepseek-v4-pro" => (0.66, 1.98, 0.022, 0.825),
        "deepseek-v4-pro-0813" => (1.35, 3.0, 0.05, 1.6875),
        "deepseek-v4-pro-ga-260813" => (1.3359, 4.00771, 0.04453, 1.669875),
        "devstral-2-123b-instruct-2512" => (0.57, 2.736, 0.057, 0.7125),
        "devstral-2512"
        | "devstral-latest"
        | "devstral-medium-latest"
        | "mistral-devstral-2-123b"
        | "mistral-medium-2505"
        | "mistral-medium-2508"
        | "mistral-medium-3" => (0.4, 2.0, 0.04, 0.5),
        "gemini-2-0-flash" => (0.1, 0.42, 0.01, 0.125),
        "gemini-2-0-flash-lite" => (0.075, 0.3, 0.0075, 0.09375),
        "gemini-2-0-pro-exp-02-05" => (1.989, 7.956, 0.49725, 2.48625),
        "gemini-2-0-pro-reasoner" => (1.292, 4.998, 0.323, 1.615),
        "gemini-2-5-computer-use-preview-10-2025"
        | "gemini-2-5-pro"
        | "gpt-5-1-chat-latest"
        | "gpt-5-chat-latest"
        | "gpt-5-codex" => (1.25, 10.0, 0.125, 1.5625),
        "gemini-2-5-flash"
        | "gemini-2-5-flash-nothink"
        | "gemini-2-5-flash-nothinking"
        | "gemini-2-5-flash-preview-09-2025"
        | "gemini-3-5-flash-lite"
        | "gemini-flash-lite-latest" => (0.3, 2.5, 0.03, 0.375),
        "gemini-2-5-flash-lite" | "gemini-2-5-flash-lite-preview-09-2025" | "qwen3-5-flash" => {
            (0.1, 0.4, 0.01, 0.125)
        }
        "gemini-2-5-flash-lite-preview-06-17"
        | "gemini-2-5-flash-preview-04-17"
        | "minimax-m3-1-flash"
        | "mistral-small-2603"
        | "mistral-small-latest"
        | "qwen3-vl-30b-a3b-instruct" => (0.15, 0.6, 0.015, 0.1875),
        "gemini-2-5-flash-preview-05-20" => (0.1425, 3.325, 0.0285, 0.178125),
        "gemini-2-5-pro-exp-03-25"
        | "gemini-2-5-pro-preview-03-25"
        | "gemini-2-5-pro-preview-05-06" => (2.5, 10.0, 0.25, 3.125),
        "gemini-2-5-pro-preview-06-05" => (1.1875, 9.5, 0.1187, 1.484375),
        "gemini-3-1-flash-lite" | "qwen3-6-flash" => (0.25, 1.5, 0.025, 0.3125),
        "gemini-3-1-flash-lite-preview" => (0.25, 1.5, 0.025, 1.0),
        "gemini-3-1-pro"
        | "gemini-3-1-pro-preview"
        | "gemini-3-1-pro-preview-customtools"
        | "gemini-3-pro"
        | "gemini-3-pro-preview"
        | "gemini-pro-latest"
        | "gpt-5-6-terra"
        | "gpt-5-6-terra-pro" => (2.0, 12.0, 0.2, 2.5),
        "gemini-3-5-flash" => (1.5, 9.0, 0.15, 1.875),
        "gemini-3-6-flash" | "gemini-3-7-flash" | "gemini-3-8-flash" | "gemini-flash-latest" => {
            (0.75, 3.75, 0.075, 0.9375)
        }
        "gemini-3-flash" | "gemini-3-flash-preview" | "qwen3-6-plus" => (0.5, 3.0, 0.05, 0.625),
        "gemini-exp-1206" => (1.258, 4.998, 0.629, 1.5725),
        "glm-4-1v-thinking-flash" | "glm-4-1v-thinking-flashx" => (0.3, 0.3, 0.15, 0.0),
        "glm-4-32b-0414-128k" => (0.1, 0.1, 0.0, 0.0),
        "glm-4-5" | "glm-4-6" | "glm-4-7" => (0.6, 2.2, 0.11, 0.0),
        "glm-4-5-air" => (0.2, 1.1, 0.03, 0.0),
        "glm-4-5-airx" => (1.1, 4.5, 0.22, 0.0),
        "glm-4-5-flash" => (0.0, 0.0, 0.0, 0.0),
        "glm-4-5-x" => (2.2, 8.9, 0.45, 0.0),
        "glm-4-5v" => (0.6, 1.8, 0.11, 0.0),
        "glm-4-6v" => (0.1494, 0.44819, 0.02988, 0.0),
        "glm-4-6v-flashx" => (0.04, 0.4, 0.004, 0.0),
        "glm-4-7-flash" => (0.06, 0.4, 0.01, 0.0),
        "glm-4-7-flashx" => (0.07, 0.4, 0.01, 0.0),
        "glm-4-air-0111" => (0.1394, 0.1394, 0.0697, 0.0),
        "glm-4-long" => (0.2006, 0.2006, 0.1003, 0.0),
        "glm-4-plus-0111" => (9.996, 9.996, 4.998, 0.0),
        "glm-5" => (1.0, 3.2, 0.2, 0.0),
        "glm-5-1" | "glm-5-3" => (1.4, 4.4, 0.26, 0.0),
        "glm-5-2" => (1.4, 4.4, 0.28, 0.0),
        "glm-5-2-260617" => (1.18747, 4.15615, 0.29687, 0.0),
        "glm-5-2-fast" => (2.31, 7.26, 0.231, 0.0),
        "glm-5-2-nitro" => (0.8, 2.4, 0.08, 0.0),
        "glm-5-3-fast" => (2.8, 8.8, 0.56, 0.0),
        "glm-5-3-flash" => (0.15, 0.5, 0.03, 0.0),
        "glm-5-3-flash-260828" => (0.11875, 0.41563, 0.03414, 0.0),
        "glm-5-3-flash-flex" => (0.0975, 0.325, 0.0195, 0.0),
        "glm-5-3-flashx" => (0.37, 1.25, 0.075, 0.0),
        "glm-5-3-flex" => (0.9425, 2.925, 0.09425, 0.0),
        "glm-5-turbo" => (1.2, 4.0, 0.24, 0.0),
        "glm-5v-turbo" => (0.74698, 3.28669, 0.17927, 0.0),
        "glm-flash-latest" => (0.2311, 0.7511, 0.0578, 0.0),
        "glm-latest" => (1.791, 6.6326, 0.1733, 0.0),
        "glm-z1-airx" => (0.7, 0.7, 0.35, 0.0),
        "gpt-3-5-turbo" => (0.5, 1.5, 0.0, 0.625),
        "gpt-3-5-turbo-0125"
        | "mistral-large-2512"
        | "mistral-large-latest"
        | "mistral-magistral-small-2509"
        | "mistral-mistral-large-3-675b-instruct" => (0.5, 1.5, 0.05, 0.625),
        "gpt-3-5-turbo-1106" => (1.0, 2.0, 0.1, 1.25),
        "gpt-3-5-turbo-instruct" => (1.5, 2.0, 0.15, 1.875),
        "gpt-4" => (30.0, 60.0, 3.0, 37.5),
        "gpt-4-1" | "o3" => (2.0, 8.0, 0.5, 2.5),
        "gpt-4-1-mini" => (0.4, 1.6, 0.1, 0.5),
        "gpt-4-1-nano" => (0.1, 0.4, 0.025, 0.125),
        "gpt-4-turbo" | "gpt-4-turbo-vision" => (10.0, 30.0, 1.0, 12.5),
        "gpt-4o" => (2.5, 10.0, 1.25, 2.5),
        "gpt-4o-mini" => (0.15, 0.6, 0.075, 0.15),
        "gpt-5" | "gpt-5-1" | "gpt-5-1-codex" | "gpt-5-1-codex-max" => (1.25, 10.0, 0.125, 1.25),
        "gpt-5-1-codex-mini" | "gpt-5-mini" => (0.25, 2.0, 0.025, 0.25),
        "gpt-5-2" | "gpt-5-2-codex" | "gpt-5-3-codex" | "gpt-5-3-codex-spark" => {
            (1.75, 14.0, 0.175, 1.75)
        }
        "gpt-5-2-chat-latest" | "gpt-5-3-chat-latest" | "gpt-5-3-codex-xhigh" => {
            (1.75, 14.0, 0.175, 2.1875)
        }
        "gpt-5-2-pro" => (21.0, 168.0, 2.1, 26.25),
        "gpt-5-4" => (2.5, 15.0, 0.25, 2.5),
        "gpt-5-4-mini" => (0.75, 4.5, 0.075, 0.75),
        "gpt-5-4-nano" => (0.2, 1.25, 0.02, 0.2),
        "gpt-5-4-pro" | "gpt-5-5-pro" => (30.0, 180.0, 3.0, 37.5),
        "gpt-5-5" => (5.0, 30.0, 0.5, 5.0),
        "gpt-5-6" | "gpt-5-6-sol" | "gpt-daybreak-blue-latest" => (4.0, 20.0, 0.4, 5.0),
        "gpt-5-6-luna" | "gpt-5-6-luna-pro" => (0.2, 1.2, 0.02, 0.25),
        "gpt-5-6-sol-pro" | "gpt-chat-latest" => (5.0, 30.0, 0.5, 6.25),
        "gpt-5-nano" => (0.05, 0.4, 0.005, 0.05),
        "gpt-5-pro" => (15.0, 120.0, 1.5, 18.75),
        "gpt-6-1-terra" => (1.0, 5.0, 0.05, 1.25),
        "gpt-daybreak-red-latest" => (12.5, 75.0, 1.25, 15.625),
        "gpt-oss-120b" => (0.35, 0.75, 0.35, 0.4375),
        "gpt-oss-20b" => (0.036, 0.15428571, 0.00371429, 0.045),
        "grok-3" | "grok-4" => (3.0, 15.0, 0.75, 3.75),
        "grok-3-mini" => (0.3, 0.5, 0.075, 0.375),
        "grok-4-1-fast-non-reasoning"
        | "grok-4-1-fast-reasoning"
        | "grok-4-fast-non-reasoning"
        | "grok-4-fast-reasoning" => (0.2, 0.5, 0.05, 0.25),
        "grok-4-2-beta" => (2.0, 6.0, 0.2, 2.0),
        "grok-4-20" | "grok-4-20-multi-agent" => (1.42, 2.83, 0.23, 1.775),
        "grok-4-20-0309-non-reasoning"
        | "grok-4-20-0309-reasoning"
        | "grok-4-20-multi-agent-0309"
        | "grok-4-3" => (1.25, 2.5, 0.2, 1.5625),
        "grok-4-20-beta-0309-non-reasoning"
        | "grok-4-20-beta-0309-reasoning"
        | "grok-4-20-non-reasoning"
        | "grok-4-20-reasoning"
        | "mistral-large-2411"
        | "mistral-large2" => (2.0, 6.0, 0.2, 2.5),
        "grok-4-5" => (2.0, 6.0, 0.3, 2.5),
        "grok-4-6" | "grok-4-7" | "qwen3-8-2-4t-a95b" => (2.0, 6.0, 0.5, 2.5),
        "grok-build-0-1" => (1.0, 2.0, 0.2, 1.25),
        "grok-code-fast-1" => (0.2, 1.5, 0.02, 0.25),
        "hy-mt2-plus" => (0.074, 0.295, 0.0074, 0.0925),
        "hy3" => (0.1562, 0.6248, 0.03905, 0.19525),
        "hy3-preview" => (0.17, 0.566661, 0.051, 0.2125),
        "hy4-preview" => (0.845, 2.535, 0.04225, 1.05625),
        "kimi-k2" => (0.57, 2.3, 0.5, 0.7125),
        "kimi-k2-0711" => (0.57, 2.3, 0.057, 0.7125),
        "kimi-k2-0905" => (0.5, 2.0, 0.4, 0.625),
        "kimi-k2-0905-preview" => (0.62854, 2.5146, 0.062854, 0.785675),
        "kimi-k2-5" => (0.6, 3.0, 0.1, 0.75),
        "kimi-k2-6" => (0.95, 4.0, 0.16, 1.1875),
        "kimi-k2-6-fast" => (1.655, 8.778, 0.1655, 2.06875),
        "kimi-k2-6-nitro" | "kimi-k2-7-code-nitro" => (0.275, 1.1, 0.0275, 0.34375),
        "kimi-k2-7-code" => (0.95, 4.0, 0.19, 1.1875),
        "kimi-k2-7-code-fast" => (0.95, 4.0, 0.095, 1.1875),
        "kimi-k2-7-code-flex" => (0.6175, 2.6, 0.06175, 0.771875),
        "kimi-k2-7-code-highspeed" => (1.9, 8.0, 0.38, 2.375),
        "kimi-k2-instruct-fast" => (0.4, 1.8, 0.2, 0.5),
        "kimi-k2-turbo-preview" => (0.15, 8.0, 0.015, 0.1875),
        "kimi-k3" => (3.0, 15.0, 0.3, 3.0),
        "kimi-k3-1-pro" => (2.0, 10.0, 0.2, 2.0),
        "kimi-k3-1-pro-highspeed" => (4.0, 20.0, 0.2, 4.0),
        "kimi-k3-fast" | "kimi-k3-fast-api" => (4.5, 22.5, 0.45, 5.625),
        "kimi-k3-flex" => (1.95, 9.75, 0.195, 2.4375),
        "kimi-latest" => (1.791, 8.9436, 0.1733, 2.23875),
        "longcat-2-0" => (0.7746, 3.0984, 0.015492, 0.96825),
        "longcat-2-5-preview" => (0.3, 1.2, 0.006, 0.375),
        "magistral-medium-latest" => (2.0, 5.0, 0.2, 2.5),
        "mimo-v2-5" | "mimo-v2-6-flash" => (0.14, 0.28, 0.0028, 0.175),
        "mimo-v2-5-pro" | "mimo-v2-6-pro" => (0.435, 0.87, 0.0036, 0.54375),
        "mimo-v2-5-pro-ultraspeed" => (1.305, 2.61, 0.0108, 1.63125),
        "mimo-v2-6-flash-rl" | "nemotron-3-nano-omni-30b-a3b-reasoning" => (0.1, 0.25, 0.01, 0.125),
        "mimo-v2-6-pro-flex" => (0.5655, 1.131, 0.0234, 0.706875),
        "mimo-v2-6-pro-rl" => (0.4, 0.8, 0.04, 0.5),
        "mimo-v2-6-pro-ultraspeed" => (4.35, 8.7, 0.036, 5.4375),
        "mimo-v2-flash"
        | "mistral-small-2503"
        | "mistral-small-2506"
        | "mistral-voxtral-small-24b-2507" => (0.1, 0.3, 0.01, 0.125),
        "mimo-v2-pro" => (1.0, 3.0, 0.2, 1.25),
        "minimax-m1" => (0.1394, 1.3328, 0.0697, 0.17425),
        "minimax-m2-1-lightning" => (0.12, 0.48, 0.012, 0.15),
        "minimax-m2-5-highspeed" => (0.6, 2.4, 0.03, 0.375),
        "minimax-m2-7" | "minimax-m3" | "minimax-m3-preview" => (0.3, 1.2, 0.06, 0.375),
        "minimax-m2-7-highspeed" => (0.6, 2.4, 0.06, 0.375),
        "minimax-m25" => (0.27, 0.95, 0.03, 0.3375),
        "minimax-m27" => (0.375, 1.5, 0.06875, 0.46875),
        "minimax-text-01" => (0.2, 1.1, 0.02, 0.25),
        "ministral-14b-2512" | "mistral-ministral-3-14b-instruct" => (0.2, 0.2, 0.02, 0.25),
        "ministral-3b" | "mistral-voxtral-mini-3b-2507" => (0.04, 0.04, 0.004, 0.05),
        "ministral-3b-2512" | "ministral-3b-latest" | "mistral-ministral-3-3b-instruct" => {
            (0.1, 0.1, 0.01, 0.125)
        }
        "ministral-8b-2512"
        | "ministral-8b-latest"
        | "mistral-ministral-3-8b-instruct"
        | "pixtral-12b" => (0.15, 0.15, 0.015, 0.1875),
        "mistral-3-14b" => (0.2, 0.2, 0.04, 0.25),
        "mistral-7b-instruct-v0-2" => (0.159, 0.219, 0.0159, 0.19875),
        "mistral-code-agent-latest" => (0.4, 2.0, 0.2, 0.5),
        "mistral-code-latest" => (0.3, 0.9, 0.15, 0.375),
        "mistral-large-2402" => (4.284, 12.952, 0.4284, 5.355),
        "mistral-large-4" => (1.36, 4.18, 0.14, 1.7),
        "mistral-large-4-0" => (0.765, 2.351, 0.079, 0.95625),
        "mistral-medium-2604" | "mistral-medium-latest" | "qwen3-coder-480b-a35b-instruct" => {
            (1.5, 7.5, 0.15, 1.875)
        }
        "mistral-medium-3-5" => (1.532, 7.843, 0.154, 1.915),
        "mistral-medium-3-5-128b" => (2.052, 10.26, 0.2052, 2.565),
        "mistral-nemo" => (20.0, 40.0, 2.0, 25.0),
        "mistral-nemo-instruct-2407" => (0.145, 0.145, 0.014, 0.18125),
        "mistral-small" => (0.075, 0.2, 0.0075, 0.09375),
        "mistral-small-3-2-24b-instruct" => (0.09375, 0.25, 0.009375, 0.1171875),
        "mistral-small-3-2-24b-instruct-2506" => (0.1, 0.312, 0.01, 0.125),
        "mistral-small-4" => (0.15, 0.6, 0.15, 0.1875),
        "mistral-small-4-119b" => (0.56075, 2.35515, 0.056075, 0.7009375),
        "mistral4-119b" => (0.2243, 0.6729, 0.02243, 0.280375),
        "muse-glimmer-30b" => (0.2, 0.8, 0.05, 0.25),
        "muse-glimmer-30b-tr" => (2.0, 5.0, 0.5, 2.5),
        "muse-spark-1-1" | "muse-spark-1-2" | "muse-spark-1-3" | "muse-spark-open" => {
            (1.25, 4.25, 0.15, 1.5625)
        }
        "muse-spark-1-2-contributor" | "muse-spark-1-3-contributor" => (0.1, 0.2, 0.002, 0.125),
        "muse-wonder" => (5.0, 25.0, 0.25, 6.25),
        "nemotron-3-5-lightning" => (0.08, 0.2, 0.04, 0.1),
        "nemotron-3-5-super-vl" => (0.3, 0.65, 0.06, 0.375),
        "nemotron-3-nano-omni" => (0.5, 0.9, 0.1, 0.625),
        "nemotron-3-super" => (0.015, 0.6, 0.015, 0.01875),
        "nemotron-3-super-120b-a12b" => (0.09714286, 0.48, 0.00971429, 0.12142857),
        "nemotron-3-ultra" => (0.1, 3.0, 0.1, 0.125),
        "nemotron-3-ultra-550b" => (0.9, 1.7, 0.18, 1.125),
        "nemotron-3-ultra-550b-a55b" => (0.68571429, 4.28571429, 0.06857143, 0.85714286),
        "nemotron-3-ultra-nvfp4" => (0.6, 2.4, 0.12, 0.75),
        "nemotron-lightning-3-5-30b-a3b" => (0.05, 0.2, 0.01, 0.0625),
        "nemotron-nano-12b-v2-vl" => (0.2, 0.6, 0.04, 0.25),
        "nemotron-nano-v2-12b" => (0.24, 0.707, 0.024, 0.3),
        "o1" => (15.0, 60.0, 7.5, 18.75),
        "o1-mini" | "o3-mini" => (1.1, 4.4, 0.55, 1.375),
        "o1-pro" => (150.0, 600.0, 15.0, 187.5),
        "o3-pro" => (20.0, 80.0, 2.0, 25.0),
        "o4-mini" => (1.1, 4.4, 0.275, 1.375),
        "pixtral-12b-2409" => (0.285, 0.285, 0.0285, 0.35625),
        "pixtral-large-2502" => (1.993, 5.978, 0.1993, 2.49125),
        "qwen-2-5-coder-32b" => (0.79, 0.79, 0.079, 0.9875),
        "qwen-3-8-27b" => (0.99, 1.49, 0.99, 1.2375),
        "qwen-3-8-27b-flex" => (0.2925, 2.08, 0.1625, 0.365625),
        "qwen-flash" => (0.05, 0.4, 0.01, 0.0625),
        "qwen-long" => (0.072, 0.287, 0.0072, 0.09),
        "qwen-max" => (1.6, 6.4, 0.16, 2.0),
        "qwen-mt-plus" => (2.46, 7.37, 0.246, 3.075),
        "qwen-mt-turbo" => (0.16, 0.49, 0.016, 0.2),
        "qwen-plus" | "qwen-plus-latest" => (0.4, 1.2, 0.08, 0.5),
        "qwen-qwen3-coder-next" => (0.5, 1.2, 0.05, 0.625),
        "qwen-qwen3-next-80b-a3b" => (0.15, 1.2, 0.015, 0.1875),
        "qwen-qwen3-vl-235b-a22b" => (0.53, 2.66, 0.053, 0.6625),
        "qwen-turbo" => (0.05, 0.2, 0.005, 0.0625),
        "qwen-vl-max" => (0.8, 3.2, 0.08, 1.0),
        "qwen-vl-plus" => (0.21, 0.63, 0.021, 0.2625),
        "qwen2-5-coder-7b-fast" => (0.03, 0.09, 0.003, 0.0375),
        "qwen2-5-vl-72b-instruct" => (1.014, 1.014, 0.1014, 1.2675),
        "qwen3-14b" => (0.35, 1.4, 0.035, 0.4375),
        "qwen3-235b-a22b" | "qwen3-32b" | "qwen3-vl-235b-a22b" => (0.7, 2.8, 0.07, 0.875),
        "qwen3-235b-a22b-fp8" | "qwen3-vl-30b-a3b" => (0.2, 0.8, 0.02, 0.25),
        "qwen3-235b-a22b-instruct-2507" => (0.199, 0.598, 0.018, 0.24875),
        "qwen3-235b-a22b-thinking-2507" => (0.3, 3.0, 0.03, 0.375),
        "qwen3-30b-a3b" => (0.11, 1.08, 0.011, 0.1375),
        "qwen3-30b-a3b-instruct-2507" => (0.099, 0.299, 0.0099, 0.12375),
        "qwen3-30b-a3b-thinking-2507" => (0.10869565, 1.08695652, 0.01086957, 0.13586956),
        "qwen3-5-122b" => (1.1215, 4.7103, 0.11215, 1.401875),
        "qwen3-5-122b-a10b" => (0.4, 3.2, 0.04, 0.5),
        "qwen3-5-27b" => (0.3, 2.4, 0.03, 0.375),
        "qwen3-5-2b" => (0.02, 0.1, 0.002, 0.025),
        "qwen3-5-35b-a3b" => (0.25, 2.0, 0.025, 0.3125),
        "qwen3-5-397b-a17b" | "qwen3-6-27b" => (0.6, 3.6, 0.06, 0.75),
        "qwen3-5-4b" => (0.04, 0.07, 0.02, 0.05),
        "qwen3-5-9b" => (0.09, 0.13, 0.045, 0.1125),
        "qwen3-5-plus" => (0.4, 2.4, 0.04, 0.5),
        "qwen3-6" => (0.04, 0.35, 0.02, 0.05),
        "qwen3-6-35b" | "qwen3-6-35b-fast" => (0.29, 1.15, 0.029, 0.3625),
        "qwen3-6-35b-a3b" => (0.375, 2.25, 0.0375, 0.46875),
        "qwen3-6-35b-flex" => (0.1885, 0.7475, 0.01885, 0.235625),
        "qwen3-6-max-preview" => (1.3, 7.8, 0.13, 1.625),
        "qwen3-7-flash" => (0.03, 0.13, 0.006, 0.0375),
        "qwen3-7-max" => (2.5, 7.5, 0.5, 3.125),
        "qwen3-7-plus" => (0.4, 1.6, 0.08, 0.5),
        "qwen3-8-27b" => (0.33645, 2.4673, 0.11215, 0.4205625),
        "qwen3-8-flash" => (0.15, 0.47, 0.016, 0.2),
        "qwen3-8-flash-next" => (0.201, 0.5, 0.05, 0.25125),
        "qwen3-8-max" => (2.0, 6.0, 0.25, 2.5),
        "qwen3-8-max-0902" => (2.0, 6.0, 2.0, 2.5),
        "qwen3-8-max-preview" => (0.338, 1.014, 0.0676, 0.4225),
        "qwen3-8-omni-flash" => (0.15, 0.47, 0.016, 0.1875),
        "qwen3-8b" => (0.18, 0.7, 0.018, 0.225),
        "qwen3-coder" => (0.22, 0.95, 0.022, 0.275),
        "qwen3-coder-30b-a3b-instruct" => (0.45, 2.25, 0.045, 0.5625),
        "qwen3-coder-480b-a35b-instruct-turbo" => (0.35, 1.5, 0.04, 0.4375),
        "qwen3-coder-flash" => (0.3, 1.5, 0.06, 0.375),
        "qwen3-coder-next" => (0.108, 0.675, 0.06, 0.135),
        "qwen3-max" => (1.2, 6.0, 0.12, 1.5),
        "qwen3-max-preview" => (0.86956522, 3.47826087, 0.08695652, 1.08695652),
        "qwen3-next-80b" => (0.35, 1.9, 0.035, 0.4375),
        "qwen3-next-80b-a3b-instruct" => (0.5, 2.0, 0.05, 0.625),
        "qwen3-next-80b-a3b-thinking" => (0.5, 6.0, 0.05, 0.625),
        "qwen3-vl-235b-a22b-instruct" => (0.2, 0.88, 0.11, 0.25),
        "qwen3-vl-235b-a22b-instruct-original" => (0.3, 1.9, 0.15, 0.375),
        "qwen3-vl-plus" => (0.2, 1.6, 0.04, 0.25),
        "reflection-beam" => (0.4, 1.2, 0.04, 0.5),
        "space-bunny" => (0.15, 0.6, 0.03, 0.0),
        _ => return None,
    };
    Some(rates)
}

fn fast_multiplier(model: &str) -> Option<f64> {
    match model {
        "gpt-6-astra" | "gpt-6-1-sol" | "gpt-6-sol" | "gpt-6-1-terra" | "gpt-6-luna"
        | "gpt-5-6-sol" | "gpt-5-6-terra" | "gpt-5-6-luna" | "gpt-5-6" | "gpt-5-4"
        | "gpt-5-3-codex" | "claude-opus-5-5" | "claude-opus-5" | "claude-opus-4-8" => Some(2.0),
        "gpt-5-5" => Some(2.5),
        "claude-opus-4-6" | "claude-opus-4-7" => Some(6.0),
        _ => None,
    }
}

fn flex_multiplier(model: &str) -> Option<f64> {
    match model {
        "gpt-6-astra" | "gpt-6-1-sol" | "gpt-6-sol" | "gpt-6-1-terra" | "gpt-6-luna"
        | "gpt-5-6-sol" | "gpt-5-6-terra" | "gpt-5-6-luna" | "gpt-5-6" | "gpt-5-5"
        | "gpt-5-5-pro" | "gpt-5-4" | "gpt-5-4-mini" | "gpt-5-4-nano" | "gpt-5-4-pro"
        | "gpt-5-2" | "gpt-5-1" | "gpt-5" | "gpt-5-mini" | "gpt-5-nano" | "o3" | "o4-mini" => {
            Some(0.5)
        }
        _ => None,
    }
}

struct CanonicalRequest<'a> {
    model: &'a str,
    suffix_tier: Option<ServiceTier>,
}

fn parse_model_request(raw: &str) -> Option<CanonicalRequest<'_>> {
    let mut s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if s.len() >= 4 && s[s.len() - 4..].eq_ignore_ascii_case("[1m]") {
        s = &s[..s.len() - 4];
    }
    for prefix in ["cursor-agent:", "cursor-plan:", "cursor-ask:", "cursor:"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest;
            break;
        }
    }
    if let Some((_, rest)) = s.rsplit_once('/') {
        s = rest;
    }
    for prefix in [
        "us.anthropic.",
        "eu.anthropic.",
        "global.anthropic.",
        "jp.anthropic.",
        "au.anthropic.",
        "apac.anthropic.",
        "in.anthropic.",
        "anthropic.",
        "global.openai.",
        "us.openai.",
        "in.openai.",
        "openai.",
        "global.xai.",
        "us.xai.",
        "xai.",
        "global.zai.",
        "us.zai.",
        "zai.",
        "moonshotai.",
        "moonshot.",
        "minimax.",
    ] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest;
            break;
        }
    }
    let mut suffix_tier = None;
    if let Some(base) = s.strip_suffix("-ultrafast") {
        if base.starts_with("gpt-") {
            if base.ends_with("-fast") || base.ends_with("-ultrafast") {
                return None;
            }
            s = base;
            suffix_tier = Some(ServiceTier::Fast);
        }
    } else if let Some(base) = s.strip_suffix("-fast")
        && base.starts_with("gpt-")
    {
        if base.ends_with("-fast") || base.ends_with("-ultrafast") {
            return None;
        }
        s = base;
        suffix_tier = Some(ServiceTier::Fast);
    }
    if s.is_empty() {
        return None;
    }
    Some(CanonicalRequest {
        model: s,
        suffix_tier,
    })
}

fn canonical_model_id(raw: &str) -> Option<String> {
    let parsed = parse_model_request(raw)?;
    let mut norm = snapshot_base(parsed.model)
        .to_ascii_lowercase()
        .replace(['.', '@'], "-");
    if let Some((base, suffix)) = norm.rsplit_once(':')
        && matches!(suffix, "thinking" | "0" | "1" | "fast" | "official")
    {
        if suffix == "fast" {
            norm = format!("{base}-fast");
        } else {
            norm = base.to_string();
        }
    }
    if let Some(base) = norm.strip_suffix("-v1")
        && base.starts_with("claude-")
    {
        norm = snapshot_base(base).to_string();
    }
    norm = match norm.as_str() {
        "kimi-for-coding" | "k2-6" | "k2p6" => "kimi-k2-6".to_string(),
        "k3" => "kimi-k3".to_string(),
        "gpt-reserve" => "gpt-5-6-luna".to_string(),
        "gpt-5-3-spark" => "gpt-5-3-codex-spark".to_string(),
        "gemini-3-pro-high" | "gemini-3-pro-low" => "gemini-3-pro-preview".to_string(),
        "gemini-3-1-pro-high" | "gemini-3-1-pro-low" => "gemini-3-1-pro".to_string(),
        "beam" => "reflection-beam".to_string(),
        _ => norm,
    };
    if let Some(base) = norm.strip_suffix("-free")
        && !matches!(
            norm.as_str(),
            "longcat-2-5-preview-free"
                | "space-bunny-free"
                | "glm-5-3-flash-free"
                | "deepseek-v4-1-flash-free"
                | "beam-free"
                | "ll-alpha-free"
                | "exo-free"
                | "mimo-v2-6-flash-free"
                | "mimo-v2-5-free"
                | "nemotron-3-ultra-free"
                | "nemotron-3-5-lightning-free"
                | "nemotron-3-5-super-vl-free"
        )
    {
        norm = base.to_string();
    }
    for suffix in ["-low", "-medium", "-high"] {
        if let Some(stem) = norm.strip_suffix(suffix)
            && (stem.starts_with("gemini-")
                || stem.starts_with("claude-")
                || stem == "gpt-oss-120b")
        {
            norm = stem.to_string();
            break;
        }
    }
    for suffix in ["-thinking", "-think"] {
        if let Some(stem) = norm.strip_suffix(suffix)
            && base_rates(stem, 0).is_some()
        {
            norm = stem.to_string();
            break;
        }
    }
    if let Some(stem) = norm.strip_suffix("-build")
        && stem.starts_with("grok-")
        && base_rates(stem, 0).is_some()
    {
        norm = stem.to_string();
    }
    Some(snapshot_base(&norm).to_string())
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
    let parsed = parse_model_request(model)?;
    let rates = price(model, input)?;
    let effective_tier = usage.service_tier.or(parsed.suffix_tier);
    let tier_multiplier = match effective_tier {
        None | Some(ServiceTier::Standard) => 1.0,
        Some(ServiceTier::Fast) => {
            if rates.fast_multiplier <= 1.0 {
                return None;
            }
            rates.fast_multiplier
        }
        Some(ServiceTier::Flex) => {
            if rates.flex_multiplier == 1.0 {
                return None;
            }
            rates.flex_multiplier
        }
        Some(ServiceTier::Other) => return None,
    };
    let speed_multiplier = if usage.fast == Some(true) && effective_tier != Some(ServiceTier::Fast)
    {
        if rates.fast_multiplier <= 1.0 {
            return None;
        }
        rates.fast_multiplier
    } else {
        1.0
    };
    let geo = if usage.us_inference == Some(true) {
        1.1
    } else {
        1.0
    };
    let fresh = input.saturating_sub(read).saturating_sub(write);
    let long_write = usage.cache_write_1h_tokens.unwrap_or(0).min(write);
    Some(
        (fresh as f64 * rates.input
            + output as f64 * rates.output
            + read as f64 * rates.read
            + write.saturating_sub(long_write) as f64 * rates.write
            + long_write as f64 * rates.write_1h)
            / 1_000_000.0
            * tier_multiplier
            * speed_multiplier
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
        // Codex reports cache writes beside the reads, both inside the input.
        let codex = UsageObservation::openai(
            &json!({"input_tokens": 100, "output_tokens": 10, "input_tokens_details": {"cached_tokens": 40, "cache_write_tokens": 50}}),
        )
        .totals(Some("gpt-5.4"), true);
        assert_eq!(
            (
                codex.input_tokens,
                codex.cache_read_tokens,
                codex.cache_write_tokens
            ),
            (100, 40, 50)
        );
    }

    #[test]
    fn prices_routed_models_prefixes_and_service_tiers_like_ccusage() {
        let observation =
            UsageObservation::openai(&json!({"input_tokens": 100, "output_tokens": 10}));
        for model in [
            "gpt-6-astra",
            "gpt-6-astra-fast",
            "gpt-6-astra-ultrafast",
            "gpt-6.1-sol",
            "gpt-6.1-sol-fast",
            "gpt-6-sol",
            "gpt-6-sol-fast",
            "gpt-6.1-terra",
            "gpt-6-luna",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "gpt-5.5",
            "gpt-5.4",
            "gpt-5.4-fast",
            "gpt-5.4-2026-03-05",
            "codex/gpt-6.1-sol",
            "kimi-for-coding",
            "kimi-k2.6",
            "kimi-k2.5",
            "kimi-k3",
            "k2.6",
            "k3",
            "kimi/kimi-k3",
            "grok-4.7",
            "grok-4.6",
            "grok-4.5",
            "grok-4.3",
            "grok-4.20",
            "grok-4",
            "grok/grok-4.7",
            "gemini-4-argon",
            "gemini-3.8-flash",
            "gemini-3.7-flash",
            "gemini-3.6-flash",
            "gemini-3.5-flash",
            "gemini-3.5-flash-lite",
            "gemini-3.1-pro-preview",
            "gemini-3-flash-preview",
            "gemini-2.5-pro",
            "gemini-2.5-flash",
            "gemini/gemini-4-argon",
            "antigravity/gemini-4-argon-high",
            "antigravity/claude-opus-5-5-medium",
            "antigravity/gpt-oss-120b-medium",
            "glm-5.3",
            "glm-5.3-flash",
            "glm-5.2",
            "glm-5.1",
            "glm-5",
            "glm-4.7",
            "glm/glm-5.3",
            "copilot/claude-opus-4.8",
            "copilot/gpt-5.5",
            "cursor:gpt-5.5",
            "opencode-go/kimi-k2.7-code",
            "opencode-go/minimax-m3",
            "opencode-go/minimax-m2.7",
            "opencode-go/qwen3.8-max",
            "opencode-go/deepseek-v4.1-flash",
            "opencode-go/mimo-v2.6-pro",
            "opencode-zen/claude-fable-5-1",
            "opencode-zen/mistral-large-4",
            "anthropic/claude-sonnet-5-5",
            "claude/claude-opus-5-5",
        ] {
            assert!(
                observation
                    .totals(Some(model), true)
                    .estimated_cost_usd
                    .is_some(),
                "{model}"
            );
        }

        let base_sol = observation
            .totals(Some("gpt-6.1-sol"), true)
            .estimated_cost_usd
            .unwrap();
        let fast_suffix = observation
            .totals(Some("gpt-6.1-sol-fast"), true)
            .estimated_cost_usd
            .unwrap();
        let priority_tier = UsageObservation::openai(
            &json!({"input_tokens": 100, "output_tokens": 10, "service_tier": "priority"}),
        )
        .totals(Some("gpt-6.1-sol"), true)
        .estimated_cost_usd
        .unwrap();
        let flex_tier = UsageObservation::openai(
            &json!({"input_tokens": 100, "output_tokens": 10, "service_tier": "flex"}),
        )
        .totals(Some("gpt-6.1-sol"), true)
        .estimated_cost_usd
        .unwrap();
        assert!((fast_suffix - base_sol * 2.0).abs() < 1e-12);
        assert!((priority_tier - base_sol * 2.0).abs() < 1e-12);
        assert!((flex_tier - base_sol * 0.5).abs() < 1e-12);
    }

    #[test]
    fn unknown_siblings_and_non_text_requests_have_no_estimate() {
        let observation =
            UsageObservation::openai(&json!({"input_tokens": 100, "output_tokens": 10}));
        for model in [
            "claude-opus-5-6",
            "claude-sonnet-5-5-turbo",
            "gpt-5.4-mini-fast",
            "gpt-6-astra-fast-fast",
            "cursor-composer",
            "grok-composer-2.5-fast",
            "glm-5.3-highspeed",
            "opencode-zen/big-pickle",
        ] {
            assert_eq!(
                observation.totals(Some(model), true).estimated_cost_usd,
                None,
                "{model}"
            );
        }
        // A model with no published fast or flex multiplier gets no estimate on that tier.
        let unpriced_fast = UsageObservation::openai(
            &json!({"input_tokens": 100, "output_tokens": 10, "service_tier": "priority"}),
        );
        assert_eq!(
            unpriced_fast
                .totals(Some("gpt-5.4-mini"), true)
                .estimated_cost_usd,
            None
        );
        let unknown_tier = UsageObservation::openai(
            &json!({"input_tokens": 100, "output_tokens": 10, "service_tier": "scale"}),
        );
        assert_eq!(
            unknown_tier
                .totals(Some("gpt-5.4"), true)
                .estimated_cost_usd,
            None
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
