//! Retry a Messages request on another saved account, before output starts.
//! A 429 with a future retry time makes an account wait. A spent balance has no
//! reset time to wait for, so that account sits out and is tried again later.
//! No guessed windows: that later try is a recheck, never a claimed reset.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use axum::response::Response;
use http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::accounts::{Account, AccountSupport, Accounts};
use crate::anthropic::{MessagesRequest, json_error};
use crate::provider::{Provider, RequestContext};
use crate::registry::Registry;
use crate::request_identity::ConversationIdentity;

mod routed;
pub(crate) use routed::execute_routed;

/// How long an account whose balance is spent sits out before one request
/// tries it again. A top-up is invisible to cc-proxy, so this is a recheck, not
/// a reset. A failed recheck costs one quick error that the next account
/// absorbs in the same request.
// ponytail: one fixed interval for every provider. Upgrade path: read the
// balance where a provider offers an endpoint for it (OpenRouter's `/key`).
const SPENT_RECHECK_MS: u64 = 30 * 60 * 1000;

/// Provider-confirmed limits can use a reset clock without adding client retry headers.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RotationHint {
    Stay,
    RetryAt {
        until_ms: u64,
        per_model: bool,
    },
    /// The balance is spent and the provider named no reset: a payment error,
    /// "credit balance is too low", a quota with no clock.
    Spent,
}

/// Set on a response that a fallback provider answered. The server reads it so
/// the log and the monitor name the provider that really served the request,
/// not the one that gave up.
#[derive(Debug, Clone)]
pub(crate) struct ServedBy {
    pub provider: String,
    pub model: String,
}

#[derive(Serialize, Deserialize)]
struct Cooldown {
    provider: String,
    account: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    until_ms: u64,
    /// The balance is spent: `until_ms` is when it is tried again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    spent: bool,
}

/// One saved wait on an account, as the monitor and `auth status` show it.
pub(crate) struct Wait {
    pub model: Option<String>,
    pub until_ms: u64,
    /// The balance is spent. `until_ms` is a recheck, not a reset.
    pub spent: bool,
}

/// How long an account sits out after a failed request, and why.
#[derive(Debug, PartialEq, Eq)]
struct Sitout {
    until_ms: u64,
    per_model: bool,
    spent: bool,
}

pub(crate) struct Cooldowns {
    path: PathBuf,
}

impl Cooldowns {
    pub(crate) fn new() -> Self {
        Self {
            path: crate::paths::state_dir().join("rotation.json"),
        }
    }

    fn read(&self) -> Result<Vec<Cooldown>> {
        let raw = match fs::read(&self.path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "Can't read {}. Check its permissions and try again.",
                        self.path.display()
                    )
                });
            }
        };
        serde_json::from_slice(&raw).with_context(|| {
            format!(
                "Invalid cooldown file at {}. Fix or remove it, then try again.",
                self.path.display()
            )
        })
    }

    pub(crate) fn waits(&self, provider: &str, id: &str) -> Result<Vec<Wait>> {
        Ok(self
            .read()?
            .into_iter()
            .filter(|c| c.provider == provider && c.account == id && c.until_ms > now_ms())
            .map(|c| Wait {
                model: c.model,
                until_ms: c.until_ms,
                spent: c.spent,
            })
            .collect())
    }

    #[cfg(test)]
    fn until(&self, provider: &str, id: &str) -> Result<Option<u64>> {
        Ok(self
            .waits(provider, id)?
            .into_iter()
            .map(|wait| wait.until_ms)
            .max())
    }

    /// Sits an account out after a failed request. `model` only scopes the
    /// wait when the provider limited that model alone.
    fn sit_out(&self, provider: &str, id: &str, model: &str, sitout: &Sitout) -> Result<()> {
        self.mark_with(
            provider,
            id,
            sitout.per_model.then_some(model),
            sitout.until_ms,
            sitout.spent,
        )
    }

    fn mark_with(
        &self,
        provider: &str,
        id: &str,
        model: Option<&str>,
        until_ms: u64,
        spent: bool,
    ) -> Result<()> {
        fs::create_dir_all(
            self.path
                .parent()
                .context("The cooldown path has no parent folder")?,
        )?;
        let lock = File::create(self.path.with_extension("lock"))?;
        lock.lock()?;
        let mut entries = self.read()?;
        entries.retain(|c| c.until_ms > now_ms());
        match entries
            .iter_mut()
            .find(|c| c.provider == provider && c.account == id && c.model.as_deref() == model)
        {
            Some(entry) if until_ms >= entry.until_ms => {
                entry.until_ms = until_ms;
                entry.spent = spent;
            }
            Some(_) => {}
            None => entries.push(Cooldown {
                provider: provider.into(),
                account: id.into(),
                model: model.map(str::to_string),
                until_ms,
                spent,
            }),
        }
        crate::fsutil::write_atomic(&self.path, &serde_json::to_vec_pretty(&entries)?)?;
        Ok(())
    }

    /// Aligns an account's saved wait timer with its live quota windows:
    /// clears the account-wide wait when every window has quota left, or
    /// updates `until_ms` when the provider moved the reset time.
    pub(crate) fn sync_account_window(
        &self,
        provider: &str,
        id: &str,
        exhausted_until_ms: Option<u64>,
    ) -> Result<()> {
        if !self.path.exists() && exhausted_until_ms.is_none() {
            return Ok(());
        }
        fs::create_dir_all(
            self.path
                .parent()
                .context("The cooldown path has no parent folder")?,
        )?;
        let lock = File::create(self.path.with_extension("lock"))?;
        lock.lock()?;
        let now = now_ms();
        let mut entries = self.read()?;
        entries.retain(|c| {
            c.until_ms > now && !(c.provider == provider && c.account == id && c.model.is_none())
        });
        if let Some(until_ms) = exhausted_until_ms.filter(|until| *until > now) {
            entries.push(Cooldown {
                provider: provider.into(),
                account: id.into(),
                model: None,
                until_ms,
                spent: false,
            });
        }
        crate::fsutil::write_atomic(&self.path, &serde_json::to_vec_pretty(&entries)?)?;
        Ok(())
    }

    pub(crate) fn clear_account(&self, provider: &str, id: &str) -> Result<()> {
        if !self.path.exists() {
            return Ok(());
        }
        fs::create_dir_all(
            self.path
                .parent()
                .context("The cooldown path has no parent folder")?,
        )?;
        let lock = File::create(self.path.with_extension("lock"))?;
        lock.lock()?;
        let now = now_ms();
        let mut entries = self.read()?;
        let before = entries.len();
        entries.retain(|c| c.until_ms > now && !(c.provider == provider && c.account == id));
        if entries.len() != before {
            crate::fsutil::write_atomic(&self.path, &serde_json::to_vec_pretty(&entries)?)?;
        }
        Ok(())
    }
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn retry_deadline(response: &Response, now: u64) -> Option<Sitout> {
    let hint = response.extensions().get::<RotationHint>().copied();
    // A spent balance reaches the client as a 402, every other limit as a 429.
    let status = response.status();
    let spent_balance =
        matches!(hint, Some(RotationHint::Spent)) && status == StatusCode::PAYMENT_REQUIRED;
    if status != StatusCode::TOO_MANY_REQUESTS && !spent_balance {
        return None;
    }
    let wait = |until_ms, per_model| Sitout {
        until_ms,
        per_model,
        spent: false,
    };
    match hint {
        Some(RotationHint::Stay) => return None,
        Some(RotationHint::RetryAt {
            until_ms,
            per_model,
        }) => return (until_ms > now).then(|| wait(until_ms, per_model)),
        Some(RotationHint::Spent) | None => {}
    }
    // A reset the provider named beats our recheck, even for a spent balance.
    let named = response
        .headers()
        .get(http::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(crate::retry::retry_after_ms)
        .and_then(|delay| now.checked_add(delay))
        .filter(|until| *until > now);
    match (named, hint) {
        (Some(until), _) => Some(wait(until, false)),
        (None, Some(RotationHint::Spent)) => Some(Sitout {
            until_ms: now.saturating_add(SPENT_RECHECK_MS),
            per_model: false,
            spent: true,
        }),
        (None, _) => None,
    }
}

/// Longest one request waits for the first cooling account instead of
/// answering 429. Short resets ride out inside the request so the agent turn
/// keeps going; anything longer surfaces as a 429 with the earliest reset,
/// which the client retries on its own.
const MAX_ROTATION_WAIT_MS: u64 = 120_000;

/// Current account first, then the saved order. Every account gets at most one
/// provider call per request, even if a very short cooldown expires meanwhile.
fn eligible<'a>(
    list: &'a [Account],
    current: &str,
    tried: &HashSet<String>,
    cooldowns: &[Cooldown],
    provider: &str,
    model: &str,
    now: u64,
) -> Option<&'a Account> {
    let ready = |account: &&Account| {
        !tried.contains(&account.id)
            && !cooldowns.iter().any(|c| {
                c.provider == provider
                    && c.account == account.id
                    && c.model.as_deref().is_none_or(|m| m == model)
                    && c.until_ms > now
            })
    };
    list.iter()
        .filter(ready)
        .find(|a| a.id == current)
        .or_else(|| list.iter().find(ready))
}

/// Earliest time any account stops cooling down, or `None` when nothing is
/// cooling. Per account the longest matching cooldown wins; across accounts
/// the first one to lapse wins.
fn first_release(
    list: &[Account],
    cooldowns: &[Cooldown],
    provider: &str,
    model: &str,
    now: u64,
) -> Option<u64> {
    list.iter()
        .filter_map(|a| {
            cooldowns
                .iter()
                .filter(|c| {
                    c.provider == provider
                        && c.account == a.id
                        && c.model.as_deref().is_none_or(|m| m == model)
                        && c.until_ms > now
                })
                .map(|c| c.until_ms)
                .max()
        })
        .min()
}

fn all_limited(
    list: &[Account],
    cooldowns: &[Cooldown],
    provider: &str,
    model: &str,
    now: u64,
) -> Option<Response> {
    // Per account, the cooldown that lasts longest decides when it is free.
    let governing = list
        .iter()
        .map(|a| {
            cooldowns
                .iter()
                .filter(|c| {
                    c.provider == provider
                        && c.account == a.id
                        && c.model.as_deref().is_none_or(|m| m == model)
                        && c.until_ms > now
                })
                .max_by_key(|c| c.until_ms)
        })
        .collect::<Option<Vec<_>>>()?;
    let first = governing.iter().map(|c| c.until_ms).min()?;
    let wait = crate::ui::duration(std::time::Duration::from_millis(first - now));
    if governing.iter().all(|c| c.spent) {
        let label = crate::ui::provider_name(provider);
        let message = match list {
            [only] => format!(
                "The saved {label} account is out of credit. Add credit, then run \
                 `cc-proxy auth use {provider} {}` to try it now. cc-proxy tries it again in {wait}.",
                only.name
            ),
            _ => format!(
                "All {} saved {label} accounts are out of credit. Add credit to one, then run \
                 `cc-proxy auth use {provider} <account>` to try it now. cc-proxy tries them \
                 again in {wait}, or you can add an account with `cc-proxy auth add {provider}`.",
                list.len()
            ),
        };
        return Some(spent_response(&message));
    }
    Some(exhausted_response(
        first,
        &format!(
            "{}. Try again in {wait}, or add an account with `cc-proxy auth add {provider}`.",
            who_is_limited(list, provider),
        ),
    ))
}

fn who_is_limited(list: &[Account], provider: &str) -> String {
    if list.len() == 1 {
        format!(
            "The saved {} account is at the limit",
            crate::ui::provider_name(provider)
        )
    } else {
        format!(
            "All {} saved {} accounts are at the limit",
            list.len(),
            crate::ui::provider_name(provider)
        )
    }
}

/// A 429 that Claude Code reads as a spent usage window with a reset time, so
/// it counts down and resumes the turn by itself instead of erroring out.
pub(crate) fn exhausted_response(until_ms: u64, message: &str) -> Response {
    let mut response = json_error(StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", message);
    response
        .headers_mut()
        .extend(crate::anthropic::error::usage_limit_headers(
            Some(until_ms),
            None,
        ));
    response
}

/// A 402 for accounts with no credit left. It names no reset, so Claude Code
/// gets no countdown to resume on: it stops and shows the message, not the
/// "temporarily limiting requests" wording a 429 gets.
fn spent_response(message: &str) -> Response {
    let mut response = json_error(StatusCode::PAYMENT_REQUIRED, "billing_error", message);
    response
        .headers_mut()
        .insert("x-should-retry", http::HeaderValue::from_static("false"));
    response
}

pub(crate) async fn execute(
    provider: &dyn Provider,
    mut body: MessagesRequest,
    context: RequestContext,
    identity: Option<ConversationIdentity>,
) -> Response {
    crate::anthropic::request::prepare_for_provider(&mut body, provider.name());
    let Some(support) = provider.cli().accounts() else {
        return provider
            .handle_messages_with_conversation_identity(body, context, identity)
            .await;
    };
    if !crate::config::auth_auto_rotate() || support.pinned_credentials().is_some() {
        return provider
            .handle_messages_with_conversation_identity(body, context, identity)
            .await;
    }
    let accounts = match Accounts::open(provider.name(), support) {
        Ok(accounts) => accounts,
        Err(error) => return storage_error(error),
    };
    execute_with(
        provider,
        body,
        context,
        identity,
        &accounts,
        &Cooldowns::new(),
    )
    .await
}

/// Milliseconds from `now_ms` until the next top of the hour (`XX:00:00.000`).
fn ms_until_next_hour(now_ms: u64) -> u64 {
    const HOUR_MS: u64 = 3_600_000;
    HOUR_MS - (now_ms % HOUR_MS)
}

/// Providers whose plans run a rolling window (5 hours, then a week) that
/// starts with the first request. Only these gain from an early start.
/// A pay-as-you-go key (Anthropic, Gemini, OpenCode Zen, a custom provider)
/// would be charged for every ping, and a monthly plan (Cursor, Copilot) has no
/// window to start. A provider missing here is never pinged, which costs
/// nothing; one wrongly added would cost money.
const WINDOWED_PLANS: &[&str] = &["antigravity", "claude", "codex", "glm", "kimi", "opencode"];

/// The providers one hourly ping reaches: the providers with an enabled row in
/// the pool, in pool order, when their plan has a window.
fn pinged_providers(route: &crate::config::Route) -> Vec<&str> {
    // Routing off means Claude Code bypasses the proxy: there is no pool.
    let Some(primary) = route.target() else {
        return Vec::new();
    };
    let mut pool: Vec<&str> = Vec::new();
    for name in std::iter::once(primary).chain(route.fallback().iter().map(String::as_str)) {
        if WINDOWED_PLANS.contains(&name) && !pool.contains(&name) {
            pool.push(name);
        }
    }
    pool
}

/// Sends a one-token Messages request at the start of every hour (`XX:00`) to
/// every enabled account of the request pool, not only the one in use, for each
/// provider whose plan has a rolling window (see `WINDOWED_PLANS`). The windows
/// start counting down early while `cc-proxy` is running. Controlled by
/// `auth.hourlyPing` (read on each tick).
///
/// A saved wait never stops a ping. A window that ends at the top of the hour
/// is still "waiting" for a moment when the tick fires, and skipping it would
/// leave the next window unstarted for a whole hour. The provider answers: a
/// limited account says 429, which also refreshes its wait.
pub(crate) fn spawn_hourly_ping(registry: Arc<Registry>) {
    tokio::spawn(async move {
        loop {
            let wait_ms = ms_until_next_hour(now_ms());
            tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
            if crate::config::auth_hourly_ping() {
                ping_routed_providers(&registry, &crate::config::route(), &Cooldowns::new()).await;
            }
        }
    });
}

/// Every saved account, not only the current one: a window only starts on the
/// account that is asked. Saved waits are not an input on purpose.
fn ping_targets(list: Vec<Account>) -> Vec<Option<String>> {
    list.into_iter().map(|account| Some(account.id)).collect()
}

async fn ping_routed_providers(
    registry: &Registry,
    route: &crate::config::Route,
    cooldowns: &Cooldowns,
) {
    let log = crate::logging::create_logger("auth");
    let rows = crate::pool::active(route, registry);
    for name in pinged_providers(route).into_iter().map(str::to_string) {
        let Some(provider) = registry.provider(&name) else {
            continue;
        };
        if provider.cli().auth_state() == crate::provider::AuthState::Missing {
            continue;
        }
        let Some(model) = ping_model_for(registry, route, &name) else {
            continue;
        };
        let saved = provider
            .cli()
            .accounts()
            .filter(|support| support.pinned_credentials().is_none())
            .and_then(|support| {
                let accounts = Accounts::open(&name, support).ok()?;
                Some(servable(Some(support), &accounts, accounts.list().ok()?))
            });
        let enabled = |id: Option<&str>| {
            rows.iter()
                .any(|row| row.provider == name && row.account_id.as_deref() == id)
        };
        let targets: Vec<Option<String>> = match saved {
            Some(list) if !list.is_empty() => ping_targets(
                list.into_iter()
                    .filter(|account| enabled(Some(&account.id)))
                    .collect(),
            ),
            _ if enabled(None) => vec![None],
            _ => Vec::new(),
        };
        for account in targets {
            let Ok(body) = serde_json::from_value::<MessagesRequest>(serde_json::json!({
                "model": &model,
                "max_tokens": 1,
                "stream": false,
                "messages": [{"role": "user", "content": "hi"}]
            })) else {
                continue;
            };
            let req_id = format!("hourly-ping-{}", uuid::Uuid::new_v4());
            let context = RequestContext {
                account: account.clone(),
                req_id: req_id.clone(),
                session_id: None,
                session_seq: None,
                provider: name.clone(),
                traffic: None,
                monitor: None,
            };
            let response = provider.handle_messages(body, context).await;
            let status = response.status().as_u16();
            if let (Some(id), Some(sitout)) =
                (account.as_deref(), retry_deadline(&response, now_ms()))
            {
                let _ = cooldowns.sit_out(&name, id, &model, &sitout);
            }
            log.info(
                "hourly_ping",
                Some(serde_json::Map::from_iter([
                    ("provider".into(), serde_json::json!(&name)),
                    ("model".into(), serde_json::json!(&model)),
                    ("account".into(), serde_json::json!(account)),
                    ("status".into(), serde_json::json!(status)),
                    ("reqId".into(), serde_json::json!(req_id)),
                ])),
            );
        }
    }
}

fn ping_model_for(
    registry: &Registry,
    route: &crate::config::Route,
    provider: &str,
) -> Option<String> {
    for tier in ["claude-haiku-5-5", "claude-sonnet-5-5", "claude-opus-5-5"] {
        if let Ok(model) = crate::tiers::resolve_for(registry, route, provider, tier) {
            return Some(model);
        }
    }
    registry
        .catalog_for(provider)
        .into_iter()
        .next()
        .or_else(|| registry.supported_models_for(provider).into_iter().next())
}

fn storage_error(error: anyhow::Error) -> Response {
    json_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "api_error",
        format!("Can't choose a saved account: {error:#}"),
    )
}

/// Accounts whose plan can't serve requests at all (a free Claude plan) take
/// no part in rotation. If that would leave none, every account stays, so the
/// provider's own error reaches the person instead of a made-up one.
fn servable(
    support: Option<&dyn AccountSupport>,
    accounts: &Accounts,
    list: Vec<Account>,
) -> Vec<Account> {
    let Some(support) = support else { return list };
    let usable: Vec<Account> = list
        .iter()
        .filter(|account| accounts.unusable(support, account).is_none())
        .cloned()
        .collect();
    if usable.is_empty() { list } else { usable }
}

/// Who moves the saved account.
pub(crate) enum Switch<'a> {
    /// A person: `auth use` or the monitor. Their choice wins, unless the
    /// account's plan can't serve requests at all.
    Chosen(&'a dyn AccountSupport),
    /// A request found the account it saw as current spent or limited. It
    /// yields to any newer choice.
    Rotated { from: &'a str, req_id: &'a str },
}

/// The one way the saved account changes, for a person and for rotation alike.
/// Requests already running keep the account they started on. The next one
/// reads the new choice.
pub(crate) fn switch_account(
    provider: &str,
    accounts: &Accounts,
    cooldowns: &Cooldowns,
    to: &Account,
    switch: Switch<'_>,
) -> Result<()> {
    let (by, req_id) = match switch {
        Switch::Chosen(support) => {
            if let Some(reason) = accounts.unusable(support, to) {
                bail!(
                    "cc-proxy won't use `{}`: {reason}. Choose another account.",
                    to.name
                );
            }
            // Clear the sit-out first. Saved the other way round, a request
            // between the two steps sees the chosen account cooling and moves
            // off it again.
            let _ = cooldowns.clear_account(provider, &to.id);
            accounts.set_current(&to.name)?;
            ("user", None)
        }
        Switch::Rotated { from, req_id } => {
            if !accounts.switch_if_current(from, &to.id)? {
                return Ok(());
            }
            ("rotation", Some(req_id))
        }
    };
    let mut fields = serde_json::Map::from_iter([
        ("provider".into(), serde_json::json!(provider)),
        ("account".into(), serde_json::json!(&to.name)),
        ("by".into(), serde_json::json!(by)),
    ]);
    if let Some(req_id) = req_id {
        fields.insert("reqId".into(), serde_json::json!(req_id));
    }
    crate::logging::create_logger("auth").info("account_switched", Some(fields));
    Ok(())
}

async fn execute_with(
    provider: &dyn Provider,
    body: MessagesRequest,
    context: RequestContext,
    identity: Option<ConversationIdentity>,
    accounts: &Accounts,
    cooldowns: &Cooldowns,
) -> Response {
    let mut tried = HashSet::new();
    let mut last_response = None;
    let mut waited = false;
    let model = body.model.as_deref().unwrap_or_default();
    loop {
        let selection = (|| -> Result<_> {
            let list = servable(provider.cli().accounts(), accounts, accounts.list()?);
            let Some(current) = accounts.current()? else {
                return Ok(None);
            };
            let snapshot = cooldowns.read()?;
            let now = now_ms();
            let next = eligible(
                &list,
                &current.id,
                &tried,
                &snapshot,
                provider.name(),
                model,
                now,
            )
            .cloned();
            Ok(Some((list, current, snapshot, now, next)))
        })();
        let (list, current, snapshot, now, next) = match selection {
            Ok(Some(selected)) => selected,
            Ok(None) => {
                return match last_response {
                    Some(response) => response,
                    None => {
                        provider
                            .handle_messages_with_conversation_identity(body, context, identity)
                            .await
                    }
                };
            }
            Err(error) => return storage_error(error),
        };
        let Some(next) = next else {
            // Nothing ready: hold the request until the first account frees
            // up (once per request, bounded), so the turn continues on it
            // instead of dying on a 429.
            if !waited {
                let now = now_ms();
                if let Some(until) = first_release(&list, &snapshot, provider.name(), model, now)
                    && until > now
                {
                    let wait = until - now;
                    if wait <= MAX_ROTATION_WAIT_MS {
                        waited = true;
                        crate::logging::create_logger("auth").info(
                            "account_wait",
                            Some(serde_json::Map::from_iter([
                                ("provider".into(), serde_json::json!(provider.name())),
                                ("waitMs".into(), serde_json::json!(wait)),
                                ("reqId".into(), serde_json::json!(&context.req_id)),
                            ])),
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
                        let fresh = match cooldowns.read() {
                            Ok(entries) => entries,
                            Err(error) => return storage_error(error),
                        };
                        let now = now_ms();
                        tried.retain(|id| {
                            fresh.iter().any(|c| {
                                c.provider == provider.name()
                                    && c.account == *id
                                    && c.model.as_deref().is_none_or(|m| m == model)
                                    && c.until_ms > now
                            })
                        });
                        continue;
                    }
                }
            }
            return all_limited(&list, &snapshot, provider.name(), model, now)
                .or(last_response)
                .unwrap_or_else(|| {
                    json_error(
                        StatusCode::TOO_MANY_REQUESTS,
                        "rate_limit_error",
                        "Every saved account was tried. Try the request again later.",
                    )
                });
        };
        if next.id != current.id {
            let switch = Switch::Rotated {
                from: &current.id,
                req_id: &context.req_id,
            };
            if let Err(error) = switch_account(provider.name(), accounts, cooldowns, &next, switch)
            {
                return storage_error(error);
            }
        }
        tried.insert(next.id.clone());
        let mut attempt = context.clone();
        attempt.account = Some(next.id.clone());
        let response = provider
            .handle_messages_with_conversation_identity(body.clone(), attempt, identity.clone())
            .await;
        let Some(sitout) = retry_deadline(&response, now_ms()) else {
            return response;
        };
        if let Err(error) = cooldowns.sit_out(provider.name(), &next.id, model, &sitout) {
            return storage_error(error);
        }
        if sitout.spent {
            crate::logging::create_logger("auth").info(
                "account_out_of_credit",
                Some(serde_json::Map::from_iter([
                    ("provider".into(), serde_json::json!(provider.name())),
                    ("account".into(), serde_json::json!(&next.name)),
                    ("reqId".into(), serde_json::json!(&context.req_id)),
                ])),
            );
        }
        last_response = Some(response);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{AuthState, CliHandlers};
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::Mutex;

    impl Cooldowns {
        /// A reset the provider named, for tests that need a wait.
        fn mark(&self, provider: &str, id: &str, model: Option<&str>, until_ms: u64) -> Result<()> {
            self.mark_with(provider, id, model, until_ms, false)
        }
    }

    struct Fake {
        calls: Mutex<Vec<String>>,
        limited: Vec<String>,
        once: Vec<String>,
        seen: Mutex<HashSet<String>>,
        retry: Option<&'static str>,
        status: StatusCode,
    }
    impl CliHandlers for Fake {
        fn auth_state(&self) -> AuthState {
            AuthState::Missing
        }
        fn login(&self) -> Result<()> {
            unreachable!()
        }
        fn device(&self) -> Result<()> {
            unreachable!()
        }
        fn status(&self) -> Result<()> {
            unreachable!()
        }
        fn logout(&self) -> Result<()> {
            unreachable!()
        }
    }
    #[async_trait]
    impl Provider for Fake {
        fn name(&self) -> &'static str {
            "codex"
        }
        fn supported_models(&self) -> Vec<String> {
            vec![]
        }
        fn cli(&self) -> &dyn CliHandlers {
            self
        }
        async fn handle_count_tokens(&self, _: MessagesRequest, _: RequestContext) -> Response {
            unreachable!()
        }
        async fn handle_messages(&self, _: MessagesRequest, ctx: RequestContext) -> Response {
            let id = ctx.account.unwrap();
            self.calls.lock().unwrap().push(id.clone());
            let mut fail = self.limited.contains(&id);
            if self.once.contains(&id) && !self.seen.lock().unwrap().contains(&id) {
                self.seen.lock().unwrap().insert(id.clone());
                fail = true;
            }
            let mut response = Response::new(axum::body::Body::from("reply"));
            if fail {
                *response.status_mut() = self.status;
                if let Some(retry) = self.retry {
                    response
                        .headers_mut()
                        .insert(http::header::RETRY_AFTER, retry.parse().unwrap());
                }
            }
            response
        }
    }
    fn fixture() -> (tempfile::TempDir, Accounts, Cooldowns, Vec<Account>) {
        let dir = tempfile::tempdir().unwrap();
        let accounts = Accounts::in_dir("codex", dir.path().join("codex"));
        for name in ["a", "b"] {
            accounts
                .install(name, None, &json!({"access":name}), None)
                .unwrap();
        }
        let list = accounts.list().unwrap();
        let cooldowns = Cooldowns {
            path: dir.path().join("rotation.json"),
        };
        (dir, accounts, cooldowns, list)
    }
    async fn call(fake: &Fake, accounts: &Accounts, cooldowns: &Cooldowns) -> Response {
        let ctx = RequestContext {
            req_id: "rotation-test".into(),
            session_id: None,
            session_seq: None,
            provider: "codex".into(),
            account: None,
            traffic: None,
            monitor: None,
        };
        execute_with(
            fake,
            serde_json::from_value(json!({"messages":[],"model":"test"})).unwrap(),
            ctx,
            None,
            accounts,
            cooldowns,
        )
        .await
    }
    fn fake(limited: Vec<String>, retry: Option<&'static str>, status: StatusCode) -> Fake {
        Fake {
            calls: Mutex::new(vec![]),
            limited,
            once: Vec::new(),
            seen: Mutex::new(HashSet::new()),
            retry,
            status,
        }
    }

    #[tokio::test]
    async fn a_limit_moves_to_b_and_the_next_request_stays_there() {
        let (_dir, accounts, cooldowns, list) = fixture();
        let fake = fake(
            vec![list[0].id.clone()],
            Some("3600"),
            StatusCode::TOO_MANY_REQUESTS,
        );
        assert_eq!(
            call(&fake, &accounts, &cooldowns).await.status(),
            StatusCode::OK
        );
        assert_eq!(accounts.current().unwrap().unwrap().name, "b");
        assert!(cooldowns.until("codex", &list[0].id).unwrap().is_some());
        assert_eq!(
            call(&fake, &accounts, &cooldowns).await.status(),
            StatusCode::OK
        );
        assert_eq!(
            *fake.calls.lock().unwrap(),
            [list[0].id.clone(), list[1].id.clone(), list[1].id.clone()]
        );
    }

    #[tokio::test]
    async fn all_limited_waits_for_the_first_release_and_continues() {
        let (_dir, accounts, cooldowns, list) = fixture();
        let fake = Fake {
            calls: Mutex::new(vec![]),
            limited: Vec::new(),
            once: list.iter().map(|a| a.id.clone()).collect(),
            seen: Mutex::new(HashSet::new()),
            retry: Some("2"),
            status: StatusCode::TOO_MANY_REQUESTS,
        };
        let response = call(&fake, &accounts, &cooldowns).await;
        assert_eq!(response.status(), StatusCode::OK);
        // Both accounts 429 once, the request waits ~2s for the first
        // release, then continues on the freed account.
        assert_eq!(fake.calls.lock().unwrap().len(), 3);
    }

    /// Logins whose `access` is listed are on a plan that can't serve requests.
    struct Plans(&'static [&'static str]);
    impl AccountSupport for Plans {
        fn sign_in(&self, _: bool) -> Result<serde_json::Value> {
            unreachable!()
        }
        fn identity(&self, _: &serde_json::Value) -> Option<String> {
            None
        }
        fn state(&self, _: &serde_json::Value) -> AuthState {
            AuthState::Missing
        }
        fn single_login(&self) -> Box<dyn crate::auth::AuthStorage<serde_json::Value>> {
            Box::new(crate::accounts::NoLogin)
        }
        fn unusable(&self, login: &serde_json::Value) -> Option<&'static str> {
            self.0
                .contains(&login["access"].as_str()?)
                .then_some("it is on a free plan")
        }
    }

    #[test]
    fn a_plan_that_cannot_serve_takes_no_part_in_rotation_and_cannot_be_chosen() {
        let (_dir, accounts, cooldowns, list) = fixture();
        let free_a = Plans(&["a"]);

        // Rotation and the hourly ping see only `b`.
        let ids = |list: &[Account]| list.iter().map(|a| a.name.clone()).collect::<Vec<_>>();
        assert_eq!(
            ids(&servable(Some(&free_a), &accounts, list.clone())),
            ["b"]
        );
        // With no support to ask, or when every account is unusable, nothing is hidden.
        assert_eq!(ids(&servable(None, &accounts, list.clone())), ["a", "b"]);
        let all_free = Plans(&["a", "b"]);
        assert_eq!(
            ids(&servable(Some(&all_free), &accounts, list.clone())),
            ["a", "b"]
        );

        // A person can't choose it, and nothing changes.
        accounts.set_current("b").unwrap();
        let error = switch_account(
            "codex",
            &accounts,
            &cooldowns,
            &list[0],
            Switch::Chosen(&free_a),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("won't use `a`"), "{error}");
        assert!(error.contains("free plan"), "{error}");
        assert_eq!(accounts.current().unwrap().unwrap().name, "b");
    }

    #[test]
    fn a_chosen_account_is_ready_at_once_and_a_late_rotation_cannot_undo_it() {
        let (_dir, accounts, cooldowns, list) = fixture();
        accounts
            .install("c", None, &json!({"access":"c"}), None)
            .unwrap();
        let third = accounts.require("c").unwrap();
        cooldowns
            .mark("codex", &list[1].id, None, now_ms() + 3_600_000)
            .unwrap();

        switch_account(
            "codex",
            &accounts,
            &cooldowns,
            &list[1],
            Switch::Chosen(&Plans(&[])),
        )
        .unwrap();
        assert_eq!(accounts.current().unwrap().unwrap().id, list[1].id);
        assert!(cooldowns.read().unwrap().is_empty());

        // A request that saw `a` as current finishes after the choice.
        let late = Switch::Rotated {
            from: &list[0].id,
            req_id: "late",
        };
        switch_account("codex", &accounts, &cooldowns, &third, late).unwrap();
        assert_eq!(accounts.current().unwrap().unwrap().id, list[1].id);
    }

    #[tokio::test]
    async fn earliest_reset_resumes_on_non_current_account_after_a_short_wait() {
        let (_dir, accounts, cooldowns, list) = fixture();
        accounts.set_current("b").unwrap();
        let now = now_ms();
        cooldowns
            .mark("codex", &list[0].id, None, now + 100)
            .unwrap();
        cooldowns
            .mark("codex", &list[1].id, None, now + 3_600_000)
            .unwrap();
        let fake = fake(Vec::new(), None, StatusCode::OK);

        assert_eq!(
            call(&fake, &accounts, &cooldowns).await.status(),
            StatusCode::OK
        );
        assert!(now_ms() >= now + 100);
        assert_eq!(*fake.calls.lock().unwrap(), [list[0].id.clone()]);
        assert_eq!(accounts.current().unwrap().unwrap().id, list[0].id);
    }

    #[tokio::test]
    async fn fresh_retry_uses_earliest_reset_not_the_later_current_account() {
        let (_dir, accounts, cooldowns, list) = fixture();
        accounts.set_current("b").unwrap();
        let now = now_ms();
        let first_reset = now + 3_600_000;
        cooldowns
            .mark("codex", &list[0].id, None, first_reset)
            .unwrap();
        cooldowns
            .mark("codex", &list[1].id, None, now + 7_200_000)
            .unwrap();
        let fake = fake(Vec::new(), None, StatusCode::OK);
        let response = call(&fake, &accounts, &cooldowns).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response.headers()["anthropic-ratelimit-unified-reset"],
            (first_reset / 1_000).to_string().as_str()
        );
        assert!(fake.calls.lock().unwrap().is_empty());
        assert_eq!(accounts.current().unwrap().unwrap().id, list[1].id);

        // Simulate the client retrying after only the earlier account resets.
        let mut entries = cooldowns.read().unwrap();
        entries
            .iter_mut()
            .find(|entry| entry.account == list[0].id)
            .unwrap()
            .until_ms = now_ms() - 1;
        fs::write(&cooldowns.path, serde_json::to_vec(&entries).unwrap()).unwrap();
        assert_eq!(
            call(&fake, &accounts, &cooldowns).await.status(),
            StatusCode::OK
        );
        assert_eq!(*fake.calls.lock().unwrap(), [list[0].id.clone()]);
        assert_eq!(accounts.current().unwrap().unwrap().id, list[0].id);
    }

    #[tokio::test]
    async fn all_limited_returns_the_first_deadline_without_calling_again() {
        let (_dir, accounts, cooldowns, list) = fixture();
        let fake = fake(
            list.iter().map(|a| a.id.clone()).collect(),
            Some("3600"),
            StatusCode::TOO_MANY_REQUESTS,
        );
        let response = call(&fake, &accounts, &cooldowns).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let headers = response.headers();
        assert_eq!(headers["anthropic-ratelimit-unified-status"], "rejected");
        assert_eq!(
            headers["anthropic-ratelimit-unified-overage-status"],
            "rejected"
        );
        let reset: u64 = headers["anthropic-ratelimit-unified-reset"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!(reset > now_ms() / 1_000);
        call(&fake, &accounts, &cooldowns).await;
        assert_eq!(fake.calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn only_a_429_with_a_future_retry_time_rotates() {
        for (retry, status) in [
            (None, 429),
            (Some("bad"), 429),
            (Some("-1"), 429),
            (Some("0"), 429),
            (Some("Wed, 01 Jan 2020 00:00:00 GMT"), 429),
            (Some("3600"), 503),
            (Some("3600"), 200),
        ] {
            let (_dir, accounts, cooldowns, list) = fixture();
            let fake = fake(
                vec![list[0].id.clone()],
                retry,
                StatusCode::from_u16(status).unwrap(),
            );
            assert_eq!(
                call(&fake, &accounts, &cooldowns).await.status().as_u16(),
                status
            );
            assert_eq!(fake.calls.lock().unwrap().len(), 1);
            assert_eq!(accounts.current().unwrap().unwrap().name, "a");
        }
    }

    #[test]
    fn expiry_is_checked_lazily_and_attempts_are_never_repeated() {
        let (_dir, _accounts, _store, list) = fixture();
        let entries = vec![Cooldown {
            provider: "codex".into(),
            account: list[0].id.clone(),
            model: None,
            until_ms: 200,
            spent: false,
        }];
        assert_eq!(
            eligible(
                &list,
                &list[0].id,
                &HashSet::new(),
                &entries,
                "codex",
                "test",
                100
            )
            .unwrap()
            .name,
            "b"
        );
        assert_eq!(
            eligible(
                &list,
                &list[0].id,
                &HashSet::new(),
                &entries,
                "codex",
                "test",
                200
            )
            .unwrap()
            .name,
            "a"
        );
        let tried = list.iter().map(|a| a.id.clone()).collect();
        assert!(eligible(&list, &list[0].id, &tried, &[], "codex", "test", 300).is_none());
    }

    #[test]
    fn provider_hints_do_not_change_the_client_retry_headers() {
        let mut response = Response::new(axum::body::Body::empty());
        *response.status_mut() = StatusCode::TOO_MANY_REQUESTS;
        response.extensions_mut().insert(RotationHint::RetryAt {
            until_ms: 20_000,
            per_model: false,
        });
        assert_eq!(
            retry_deadline(&response, 10_000),
            Some(Sitout {
                until_ms: 20_000,
                per_model: false,
                spent: false
            })
        );
        assert!(!response.headers().contains_key(http::header::RETRY_AFTER));
        response
            .headers_mut()
            .insert(http::header::RETRY_AFTER, "10".parse().unwrap());
        response.extensions_mut().insert(RotationHint::Stay);
        assert_eq!(retry_deadline(&response, 10_000), None);
        response.extensions_mut().insert(RotationHint::RetryAt {
            until_ms: 10_000,
            per_model: false,
        });
        assert_eq!(retry_deadline(&response, 10_000), None);
        *response.status_mut() = StatusCode::OK;
        response.extensions_mut().insert(RotationHint::RetryAt {
            until_ms: 20_000,
            per_model: false,
        });
        assert_eq!(retry_deadline(&response, 10_000), None);
    }

    #[test]
    fn model_limits_never_block_other_models_or_providers() {
        let (_dir, _accounts, cooldowns, list) = fixture();
        let now = now_ms();
        cooldowns
            .mark("codex", &list[0].id, Some("model-a"), now + 60_000)
            .unwrap();
        cooldowns
            .mark("codex", &list[1].id, Some("model-a"), now + 90_000)
            .unwrap();
        let snapshot = cooldowns.read().unwrap();
        assert!(
            eligible(
                &list,
                &list[0].id,
                &HashSet::new(),
                &snapshot,
                "codex",
                "model-a",
                now
            )
            .is_none()
        );
        assert_eq!(
            eligible(
                &list,
                &list[0].id,
                &HashSet::new(),
                &snapshot,
                "codex",
                "model-b",
                now
            )
            .unwrap()
            .id,
            list[0].id
        );
        assert!(
            eligible(
                &list,
                &list[0].id,
                &HashSet::new(),
                &snapshot,
                "other",
                "model-a",
                now
            )
            .is_some()
        );
        let response = all_limited(&list, &snapshot, "codex", "model-a", now).unwrap();
        let reset: u64 = response.headers()["anthropic-ratelimit-unified-reset"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(reset, (now + 60_000) / 1_000);
        assert_eq!(response.headers()["x-should-retry"], "false");
        assert!(all_limited(&list, &snapshot, "codex", "model-b", now).is_none());
    }

    #[test]
    fn concurrent_marks_keep_every_account_and_the_longest_deadline() {
        let (dir, _accounts, cooldowns, list) = fixture();
        let until = now_ms() + 60_000;
        std::thread::scope(|scope| {
            for account in &list {
                let path = cooldowns.path.clone();
                scope.spawn(move || {
                    Cooldowns { path }
                        .mark("codex", &account.id, None, until)
                        .unwrap()
                });
            }
        });
        cooldowns
            .mark("codex", &list[0].id, None, until - 1000)
            .unwrap();
        let reopened = Cooldowns {
            path: dir.path().join("rotation.json"),
        };
        assert_eq!(reopened.until("codex", &list[0].id).unwrap(), Some(until));
        assert_eq!(reopened.until("codex", &list[1].id).unwrap(), Some(until));

        reopened
            .sync_account_window("codex", &list[0].id, Some(until - 30_000))
            .unwrap();
        assert_eq!(
            reopened.until("codex", &list[0].id).unwrap(),
            Some(until - 30_000)
        );
        reopened
            .sync_account_window("codex", &list[0].id, None)
            .unwrap();
        assert_eq!(reopened.until("codex", &list[0].id).unwrap(), None);
        assert_eq!(reopened.until("codex", &list[1].id).unwrap(), Some(until));
    }

    /// A payment error as the shared classifier answers it: a 429 with no
    /// reset named.
    fn spent_balance() -> Response {
        crate::providers::upstream_error::from_http(
            402,
            r#"{"type":"error","error":{"type":"billing_error","message":"Your credit balance is too low"}}"#,
            None,
            "Anthropic",
        )
        .response()
    }

    /// Answers 200, except for the accounts whose balance is spent.
    struct Broke {
        calls: Mutex<Vec<String>>,
        spent: Vec<String>,
    }
    impl CliHandlers for Broke {
        fn auth_state(&self) -> AuthState {
            AuthState::Missing
        }
        fn login(&self) -> Result<()> {
            unreachable!()
        }
        fn device(&self) -> Result<()> {
            unreachable!()
        }
        fn status(&self) -> Result<()> {
            unreachable!()
        }
        fn logout(&self) -> Result<()> {
            unreachable!()
        }
    }
    #[async_trait]
    impl Provider for Broke {
        fn name(&self) -> &'static str {
            "codex"
        }
        fn supported_models(&self) -> Vec<String> {
            vec![]
        }
        fn cli(&self) -> &dyn CliHandlers {
            self
        }
        async fn handle_count_tokens(&self, _: MessagesRequest, _: RequestContext) -> Response {
            unreachable!()
        }
        async fn handle_messages(&self, _: MessagesRequest, ctx: RequestContext) -> Response {
            let id = ctx.account.unwrap();
            self.calls.lock().unwrap().push(id.clone());
            if self.spent.contains(&id) {
                spent_balance()
            } else {
                Response::new(axum::body::Body::from("reply"))
            }
        }
    }
    fn broke(spent: Vec<String>) -> Broke {
        Broke {
            calls: Mutex::new(vec![]),
            spent,
        }
    }
    async fn call_broke(broke: &Broke, accounts: &Accounts, cooldowns: &Cooldowns) -> Response {
        let ctx = RequestContext {
            req_id: "spent-test".into(),
            session_id: None,
            session_seq: None,
            provider: "codex".into(),
            account: None,
            traffic: None,
            monitor: None,
        };
        execute_with(
            broke,
            serde_json::from_value(json!({"messages":[],"model":"test"})).unwrap(),
            ctx,
            None,
            accounts,
            cooldowns,
        )
        .await
    }

    #[test]
    fn a_spent_balance_sits_out_for_a_recheck_and_a_named_reset_beats_it() {
        let mut response = spent_balance();
        // A spent balance reaches the client as a payment error, not a throttle.
        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
        assert!(!response.headers().contains_key(http::header::RETRY_AFTER));
        assert_eq!(
            retry_deadline(&response, 10_000),
            Some(Sitout {
                until_ms: 10_000 + SPENT_RECHECK_MS,
                per_model: false,
                spent: true
            })
        );
        // The provider's own clock is a real reset, not a recheck.
        response
            .headers_mut()
            .insert(http::header::RETRY_AFTER, "60".parse().unwrap());
        assert_eq!(
            retry_deadline(&response, 10_000),
            Some(Sitout {
                until_ms: 70_000,
                per_model: false,
                spent: false
            })
        );
        // A success never sits an account out, whatever hint rides along.
        *response.status_mut() = StatusCode::OK;
        assert_eq!(retry_deadline(&response, 10_000), None);
        // Neither does a 402 nobody marked as a spent balance.
        *response.status_mut() = StatusCode::PAYMENT_REQUIRED;
        response.extensions_mut().remove::<RotationHint>();
        assert_eq!(retry_deadline(&response, 10_000), None);
    }

    #[tokio::test]
    async fn a_spent_key_moves_to_the_next_key_and_stays_out_until_rechecked() {
        let (_dir, accounts, cooldowns, list) = fixture();
        let broke = broke(vec![list[0].id.clone()]);
        assert_eq!(
            call_broke(&broke, &accounts, &cooldowns).await.status(),
            StatusCode::OK
        );
        assert_eq!(accounts.current().unwrap().unwrap().name, "b");
        let waits = cooldowns.waits("codex", &list[0].id).unwrap();
        assert_eq!(waits.len(), 1);
        assert!(waits[0].spent && waits[0].model.is_none());
        let recheck = waits[0].until_ms - now_ms();
        assert!(
            recheck > SPENT_RECHECK_MS - 60_000 && recheck <= SPENT_RECHECK_MS,
            "{recheck}"
        );
        // The next request goes straight to the key that still has credit.
        assert_eq!(
            call_broke(&broke, &accounts, &cooldowns).await.status(),
            StatusCode::OK
        );
        assert_eq!(
            *broke.calls.lock().unwrap(),
            [list[0].id.clone(), list[1].id.clone(), list[1].id.clone()]
        );
    }

    #[tokio::test]
    async fn when_every_key_is_spent_the_answer_says_how_to_fix_it_with_no_countdown() {
        let (_dir, accounts, cooldowns, list) = fixture();
        let broke = broke(list.iter().map(|a| a.id.clone()).collect());
        let response = call_broke(&broke, &accounts, &cooldowns).await;
        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
        // No usage-limit headers: there is no reset for Claude Code to count down to.
        assert!(
            !response
                .headers()
                .contains_key("anthropic-ratelimit-unified-reset")
        );
        assert_eq!(response.headers()["x-should-retry"], "false");
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let message = body["error"]["message"].as_str().unwrap();
        for expected in [
            "out of credit",
            "cc-proxy auth use codex",
            "tries them again in",
            "cc-proxy auth add codex",
        ] {
            assert!(message.contains(expected), "{expected}: {message}");
        }
        assert_eq!(broke.calls.lock().unwrap().len(), 2);

        // The next request does not call a spent key again.
        let again = call_broke(&broke, &accounts, &cooldowns).await;
        assert_eq!(again.status(), StatusCode::PAYMENT_REQUIRED);
        assert_eq!(broke.calls.lock().unwrap().len(), 2);

        // After a top-up, choosing the account clears its wait and it is tried at once.
        cooldowns.clear_account("codex", &list[0].id).unwrap();
        accounts.set_current("a").unwrap();
        let topped_up = self::broke(vec![list[1].id.clone()]);
        assert_eq!(
            call_broke(&topped_up, &accounts, &cooldowns).await.status(),
            StatusCode::OK
        );
        assert_eq!(*topped_up.calls.lock().unwrap(), [list[0].id.clone()]);
    }

    #[test]
    fn one_spent_key_names_itself_and_a_mix_with_a_real_reset_stays_a_countdown() {
        let (_dir, _accounts, cooldowns, list) = fixture();
        let now = now_ms();
        cooldowns
            .mark_with("codex", &list[0].id, None, now + 60_000, true)
            .unwrap();
        let snapshot = cooldowns.read().unwrap();
        // One of two keys is spent and the other is free: not all limited.
        assert!(all_limited(&list, &snapshot, "codex", "test", now).is_none());
        let one = all_limited(&list[..1], &snapshot, "codex", "test", now).unwrap();
        assert_eq!(one.status(), StatusCode::PAYMENT_REQUIRED);

        cooldowns
            .mark("codex", &list[1].id, None, now + 3_600_000)
            .unwrap();
        let snapshot = cooldowns.read().unwrap();
        let mixed = all_limited(&list, &snapshot, "codex", "test", now).unwrap();
        assert_eq!(mixed.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(
            mixed
                .headers()
                .contains_key("anthropic-ratelimit-unified-reset")
        );
    }

    #[test]
    fn an_old_cooldown_file_without_the_spent_field_still_reads() {
        let dir = tempfile::tempdir().unwrap();
        let cooldowns = Cooldowns {
            path: dir.path().join("rotation.json"),
        };
        let until = now_ms() + 60_000;
        fs::write(
            &cooldowns.path,
            format!(r#"[{{"provider":"codex","account":"x","until_ms":{until}}}]"#),
        )
        .unwrap();
        let waits = cooldowns.waits("codex", "x").unwrap();
        assert_eq!(waits.len(), 1);
        assert!(!waits[0].spent);
        // A reset wait is written without the field, so an older build reads it too.
        cooldowns.mark("codex", "y", None, until).unwrap();
        assert!(
            !fs::read_to_string(&cooldowns.path)
                .unwrap()
                .contains("spent")
        );
        cooldowns
            .mark_with("codex", "z", None, until, true)
            .unwrap();
        assert!(cooldowns.waits("codex", "z").unwrap()[0].spent);
    }

    struct ChainFake {
        name: &'static str,
    }

    impl CliHandlers for ChainFake {
        fn auth_state(&self) -> AuthState {
            AuthState::SignedIn {
                account: None,
                expires_ms: None,
            }
        }
        fn login(&self) -> Result<()> {
            unreachable!()
        }
        fn device(&self) -> Result<()> {
            unreachable!()
        }
        fn status(&self) -> Result<()> {
            unreachable!()
        }
        fn logout(&self) -> Result<()> {
            unreachable!()
        }
    }

    #[async_trait]
    impl Provider for ChainFake {
        fn name(&self) -> &'static str {
            self.name
        }
        fn supported_models(&self) -> Vec<String> {
            vec![]
        }
        fn cli(&self) -> &dyn CliHandlers {
            self
        }
        async fn handle_count_tokens(&self, _: MessagesRequest, _: RequestContext) -> Response {
            unreachable!()
        }
        async fn handle_messages(&self, _: MessagesRequest, _: RequestContext) -> Response {
            Response::new(axum::body::Body::from(self.name))
        }
    }

    #[tokio::test]
    async fn hourly_ping_ticks_on_the_hour_and_pings_signed_in_providers() {
        assert_eq!(ms_until_next_hour(0), 3_600_000);
        assert_eq!(ms_until_next_hour(1_000), 3_599_000);
        assert_eq!(ms_until_next_hour(3_600_000), 3_600_000);

        let dir = tempfile::tempdir().unwrap();
        let cooldowns = Cooldowns {
            path: dir.path().join("rotation.json"),
        };
        let registry =
            Registry::from_providers([Arc::new(ChainFake { name: "codex" }) as Arc<dyn Provider>]);
        ping_routed_providers(&registry, &crate::config::Route::default(), &cooldowns).await;
    }

    /// Answers 200 and counts the calls. It has a model, so a ping reaches it.
    struct Counting {
        name: &'static str,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl CliHandlers for Counting {
        fn auth_state(&self) -> AuthState {
            AuthState::SignedIn {
                account: None,
                expires_ms: None,
            }
        }
        fn login(&self) -> Result<()> {
            unreachable!()
        }
        fn device(&self) -> Result<()> {
            unreachable!()
        }
        fn status(&self) -> Result<()> {
            unreachable!()
        }
        fn logout(&self) -> Result<()> {
            unreachable!()
        }
    }
    #[async_trait]
    impl Provider for Counting {
        fn name(&self) -> &'static str {
            self.name
        }
        fn supported_models(&self) -> Vec<String> {
            vec![format!("{}-model", self.name)]
        }
        fn cli(&self) -> &dyn CliHandlers {
            self
        }
        async fn handle_count_tokens(&self, _: MessagesRequest, _: RequestContext) -> Response {
            unreachable!()
        }
        async fn handle_messages(&self, _: MessagesRequest, _: RequestContext) -> Response {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Response::new(axum::body::Body::from("pong"))
        }
    }

    fn route_to(primary: &str, fallback: &[&str]) -> crate::config::Route {
        crate::config::Route {
            on: true,
            provider: Some(primary.into()),
            fallback: fallback.iter().map(|name| (*name).to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn the_hourly_ping_reaches_only_routed_providers_with_a_window() {
        // Primary first, then fallbacks in order. Pay-as-you-go keys and monthly
        // plans are left out, and so is a provider named twice.
        let route = route_to(
            "codex",
            &[
                "antigravity",
                "anthropic",
                "cursor",
                "claude",
                "codex",
                "gemini",
            ],
        );
        assert_eq!(pinged_providers(&route), ["codex", "antigravity", "claude"]);
        // A key that bills every token is never the target, even as the primary.
        assert!(pinged_providers(&route_to("anthropic", &[])).is_empty());
        // Routing off: Claude Code bypasses the proxy, so there is no pool.
        let mut off = route_to("codex", &["kimi"]);
        off.on = false;
        assert!(pinged_providers(&off).is_empty());
        // Flat plans that sign in with a key still count: their windows are the point.
        assert_eq!(
            pinged_providers(&route_to("opencode", &["glm"])),
            ["opencode", "glm"]
        );
        for billed in ["anthropic", "gemini", "opencode-zen", "cursor", "copilot"] {
            assert!(!WINDOWED_PLANS.contains(&billed), "{billed}");
        }
    }

    #[test]
    fn the_hourly_ping_asks_every_account_even_one_a_saved_wait_calls_limited() {
        let (_dir, accounts, cooldowns, list) = fixture();
        accounts.set_current("b").unwrap();
        // A wait that ends a moment after the tick must not hide the account:
        // the window it belongs to is over, and the next one has to start.
        cooldowns
            .mark_with("codex", &list[0].id, None, now_ms() + 500, false)
            .unwrap();
        cooldowns
            .mark_with("codex", &list[1].id, None, now_ms() + 60_000, true)
            .unwrap();
        let ids: Vec<_> = ping_targets(list.clone()).into_iter().flatten().collect();
        assert_eq!(ids, [list[0].id.clone(), list[1].id.clone()]);
    }

    #[tokio::test]
    async fn the_hourly_ping_calls_each_pooled_provider_and_nothing_outside_it() {
        let count = |name| {
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            (
                Arc::new(Counting {
                    name,
                    calls: calls.clone(),
                }) as Arc<dyn Provider>,
                calls,
            )
        };
        let (codex, codex_calls) = count("codex");
        let (kimi, kimi_calls) = count("kimi");
        let (anthropic, anthropic_calls) = count("anthropic");
        let (antigravity, antigravity_calls) = count("antigravity");
        let registry = Registry::from_providers([codex, kimi, anthropic, antigravity]);
        let dir = tempfile::tempdir().unwrap();
        let cooldowns = Cooldowns {
            path: dir.path().join("rotation.json"),
        };
        // Kimi is signed in but not routed. Anthropic is routed but bills per token.
        ping_routed_providers(
            &registry,
            &route_to("codex", &["anthropic", "antigravity"]),
            &cooldowns,
        )
        .await;
        let calls = |counter: &Arc<std::sync::atomic::AtomicUsize>| {
            counter.load(std::sync::atomic::Ordering::SeqCst)
        };
        assert_eq!(calls(&codex_calls), 1);
        assert_eq!(calls(&antigravity_calls), 1);
        assert_eq!(calls(&kimi_calls), 0);
        assert_eq!(calls(&anthropic_calls), 0);
    }
}
