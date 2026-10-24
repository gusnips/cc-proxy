//! Retry a Messages request on another saved account, before output starts.
//! Only a 429 with a future retry time makes an account wait. No guessed windows.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::response::Response;
use http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::accounts::{Account, Accounts};
use crate::anthropic::{MessagesRequest, json_error};
use crate::provider::{Provider, RequestContext};
use crate::registry::Registry;
use crate::request_identity::ConversationIdentity;

/// Provider-confirmed limits can use a reset clock without adding client retry headers.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RotationHint {
    Stay,
    RetryAt { until_ms: u64, per_model: bool },
}

/// Rides on the 429 answered when every account (and, for a routed chain,
/// every provider) is spent: the earliest moment one frees up. The server
/// reads it to decide whether a fallback provider is worth trying.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ExhaustedAt {
    pub until_ms: u64,
}

#[derive(Serialize, Deserialize)]
struct Cooldown {
    provider: String,
    account: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    until_ms: u64,
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

    pub(crate) fn waits(&self, provider: &str, id: &str) -> Result<Vec<(Option<String>, u64)>> {
        Ok(self
            .read()?
            .into_iter()
            .filter(|c| c.provider == provider && c.account == id && c.until_ms > now_ms())
            .map(|c| (c.model, c.until_ms))
            .collect())
    }

    #[cfg(test)]
    fn until(&self, provider: &str, id: &str) -> Result<Option<u64>> {
        Ok(self
            .waits(provider, id)?
            .into_iter()
            .map(|(_, until)| until)
            .max())
    }

    fn mark(&self, provider: &str, id: &str, model: Option<&str>, until_ms: u64) -> Result<()> {
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
            Some(entry) => entry.until_ms = entry.until_ms.max(until_ms),
            None => entries.push(Cooldown {
                provider: provider.into(),
                account: id.into(),
                model: model.map(str::to_string),
                until_ms,
            }),
        }
        crate::fsutil::write_atomic(&self.path, &serde_json::to_vec_pretty(&entries)?)?;
        Ok(())
    }
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn retry_deadline(response: &Response, now: u64) -> Option<(u64, bool)> {
    if response.status() != StatusCode::TOO_MANY_REQUESTS {
        return None;
    }
    if let Some(hint) = response.extensions().get::<RotationHint>() {
        return match *hint {
            RotationHint::Stay => None,
            RotationHint::RetryAt {
                until_ms,
                per_model,
            } => (until_ms > now).then_some((until_ms, per_model)),
        };
    }
    let wait = crate::retry::retry_after_ms(
        response
            .headers()
            .get(http::header::RETRY_AFTER)?
            .to_str()
            .ok()?,
    )?;
    now.checked_add(wait)
        .filter(|until| *until > now)
        .map(|until| (until, false))
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
    let deadlines: Option<Vec<_>> = list
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
                .map(|c| c.until_ms)
                .max()
        })
        .collect();
    let first = deadlines?.into_iter().min()?;
    Some(exhausted_response(
        first,
        &format!(
            "{}. Try again in {}, or add an account with `cc-proxy auth add {provider}`.",
            who_is_limited(list, provider),
            crate::ui::duration(std::time::Duration::from_millis(first - now)),
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
    response.extensions_mut().insert(ExhaustedAt { until_ms });
    response
}

pub(crate) async fn execute(
    provider: &dyn Provider,
    body: MessagesRequest,
    context: RequestContext,
    identity: Option<ConversationIdentity>,
) -> Response {
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

/// A routed request: the provider Claude Code points at first, then the
/// `route.fallback` list in order, each one once. A provider is skipped the
/// moment it answers "every account is spent", so the request lands on the
/// first provider with room. When every one of them is spent, the answer
/// names the one that frees up first and when, which is what lets Claude Code
/// count down and resume by itself.
pub(crate) async fn execute_routed(
    registry: &Registry,
    route: &crate::config::Route,
    requested: &str,
    primary: Arc<dyn Provider>,
    mut body: MessagesRequest,
    mut context: RequestContext,
    identity: Option<ConversationIdentity>,
) -> Response {
    let mut response = execute(
        primary.as_ref(),
        body.clone(),
        context.clone(),
        identity.clone(),
    )
    .await;
    let mut earliest: Option<(u64, &str)> = exhausted_by(&response, primary.name());
    for fallback in route.fallback() {
        if fallback == primary.name() {
            continue;
        }
        if response.extensions().get::<ExhaustedAt>().is_none() {
            return response;
        }
        let Ok(model) = crate::tiers::resolve_for(registry, route, fallback, requested) else {
            continue;
        };
        let Some(next) = registry.provider_for_model(&model) else {
            continue;
        };
        // A provider nobody signed in to would answer with an auth error and
        // hide the limit this request is really about.
        if next.name() != fallback || next.cli().auth_state() == crate::provider::AuthState::Missing
        {
            continue;
        }
        crate::logging::create_logger("auth").info(
            "provider_switched",
            Some(serde_json::Map::from_iter([
                ("from".into(), serde_json::json!(primary.name())),
                ("to".into(), serde_json::json!(fallback)),
                ("model".into(), serde_json::json!(&model)),
                ("reqId".into(), serde_json::json!(&context.req_id)),
            ])),
        );
        body.model = Some(model);
        context.provider = fallback.to_string();
        response = execute(
            next.as_ref(),
            body.clone(),
            context.clone(),
            identity.clone(),
        )
        .await;
        if let Some((until, name)) = exhausted_by(&response, fallback)
            && earliest.is_none_or(|(first, _)| until < first)
        {
            earliest = Some((until, name));
        }
    }
    match (
        response.extensions().get::<ExhaustedAt>().copied(),
        earliest,
    ) {
        (Some(last), Some((until, fallback))) if until <= last.until_ms => exhausted_response(
            until,
            &format!(
                "Every routed provider is at its limit. {} frees up first, in {}. \
                 Use `cc-proxy auth add` to add an account sooner.",
                crate::ui::provider_name(fallback),
                crate::ui::duration(std::time::Duration::from_millis(until - now_ms())),
            ),
        ),
        _ => response,
    }
}

fn exhausted_by<'a>(response: &Response, provider: &'a str) -> Option<(u64, &'a str)> {
    response
        .extensions()
        .get::<ExhaustedAt>()
        .map(|at| (at.until_ms, provider))
}

/// Milliseconds from `now_ms` until the next top of the hour (`XX:00:00.000`).
fn ms_until_next_hour(now_ms: u64) -> u64 {
    const HOUR_MS: u64 = 3_600_000;
    HOUR_MS - (now_ms % HOUR_MS)
}

/// Sends a one-token Messages request at the start of every hour (`XX:00`) to
/// every signed-in provider account that is not currently waiting on a limit,
/// so rolling 5-hour quota windows start counting down early while `cc-proxy`
/// is running. Controlled by `auth.hourlyPing` (read on each tick).
pub(crate) fn spawn_hourly_ping(registry: Arc<Registry>) {
    tokio::spawn(async move {
        loop {
            let wait_ms = ms_until_next_hour(now_ms());
            tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
            if crate::config::auth_hourly_ping() {
                ping_signed_in_providers(&registry, &Cooldowns::new()).await;
            }
        }
    });
}

async fn ping_signed_in_providers(registry: &Registry, cooldowns: &Cooldowns) {
    let route = crate::config::route();
    let log = crate::logging::create_logger("auth");
    for name in registry.list_provider_names() {
        let Some(provider) = registry.provider(&name) else {
            continue;
        };
        if provider.cli().auth_state() == crate::provider::AuthState::Missing {
            continue;
        }
        let Some(model) = ping_model_for(registry, &route, &name) else {
            continue;
        };
        let targets: Vec<Option<String>> = match provider
            .cli()
            .accounts()
            .filter(|support| support.pinned_credentials().is_none())
            .and_then(|support| Accounts::open(&name, support).ok())
            .and_then(|accounts| accounts.list().ok())
        {
            Some(list) if !list.is_empty() => {
                let snapshot = cooldowns.read().unwrap_or_default();
                let now = now_ms();
                list.into_iter()
                    .filter(|account| {
                        !snapshot.iter().any(|c| {
                            c.provider == name
                                && c.account == account.id
                                && c.model.as_deref().is_none_or(|m| m == model)
                                && c.until_ms > now
                        })
                    })
                    .map(|account| Some(account.id))
                    .collect()
            }
            _ => vec![None],
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
            if let (Some(id), Some((until, per_model))) =
                (account.as_deref(), retry_deadline(&response, now_ms()))
            {
                let _ = cooldowns.mark(&name, id, per_model.then_some(model.as_str()), until);
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
            let list = accounts.list()?;
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
            if let Err(error) = accounts.switch_if_current(&current.id, &next.id) {
                return storage_error(error);
            }
            crate::logging::create_logger("auth").info(
                "account_switched",
                Some(serde_json::Map::from_iter([
                    ("provider".into(), serde_json::json!(provider.name())),
                    ("account".into(), serde_json::json!(&next.name)),
                    ("reqId".into(), serde_json::json!(&context.req_id)),
                ])),
            );
        }
        tried.insert(next.id.clone());
        let mut attempt = context.clone();
        attempt.account = Some(next.id.clone());
        let response = provider
            .handle_messages_with_conversation_identity(body.clone(), attempt, identity.clone())
            .await;
        let Some((until, per_model)) = retry_deadline(&response, now_ms()) else {
            return response;
        };
        if let Err(error) =
            cooldowns.mark(provider.name(), &next.id, per_model.then_some(model), until)
        {
            return storage_error(error);
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
        let Some(ExhaustedAt { until_ms }) = response.extensions().get::<ExhaustedAt>().copied()
        else {
            panic!("an all-limited answer must name the first release");
        };
        assert_eq!(reset, until_ms / 1_000);
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
        assert_eq!(retry_deadline(&response, 10_000), Some((20_000, false)));
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
    }

    struct ChainFake {
        name: &'static str,
        spent_until: Option<u64>,
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
            match self.spent_until {
                Some(until) => exhausted_response(until, &format!("{} is spent", self.name)),
                None => Response::new(axum::body::Body::from(self.name)),
            }
        }
    }

    fn chain_route(fallback: &[&str]) -> crate::config::Route {
        crate::config::Route {
            on: true,
            provider: Some("codex".into()),
            fallback: fallback.iter().map(|name| (*name).to_string()).collect(),
            tiers: [(
                "antigravity".to_string(),
                [("opus".to_string(), "antigravity/test-1".to_string())].into(),
            )]
            .into(),
        }
    }

    async fn routed_call(
        codex_until: Option<u64>,
        antigravity_until: Option<u64>,
        route: &crate::config::Route,
    ) -> Response {
        let registry = Registry::from_providers([
            Arc::new(ChainFake {
                name: "codex",
                spent_until: codex_until,
            }) as Arc<dyn Provider>,
            Arc::new(ChainFake {
                name: "antigravity",
                spent_until: antigravity_until,
            }),
        ]);
        execute_routed(
            &registry,
            route,
            "claude-opus-4-8",
            registry.provider("codex").unwrap(),
            serde_json::from_value(json!({"messages":[],"model":"claude-opus-4-8"})).unwrap(),
            RequestContext {
                req_id: "chain-test".into(),
                session_id: None,
                session_seq: None,
                provider: "codex".into(),
                account: None,
                traffic: None,
                monitor: None,
            },
            None,
        )
        .await
    }

    #[tokio::test]
    async fn a_spent_provider_moves_the_request_to_the_next_one_in_the_chain() {
        let response = routed_call(
            Some(now_ms() + 60 * 60 * 1_000),
            None,
            &chain_route(&["antigravity"]),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(&body[..], b"antigravity");
    }

    #[tokio::test]
    async fn a_spent_chain_names_the_provider_that_frees_up_first() {
        let now = now_ms();
        let response = routed_call(
            Some(now + 4 * 60 * 60 * 1_000),
            Some(now + 2 * 60 * 60 * 1_000),
            &chain_route(&["antigravity"]),
        )
        .await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let reset: u64 = response.headers()["anthropic-ratelimit-unified-reset"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        // Antigravity frees up two hours before Codex does.
        assert_eq!(reset, (now + 2 * 60 * 60 * 1_000) / 1_000);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Antigravity"),
            "{body}"
        );
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
        let registry = Registry::from_providers([Arc::new(ChainFake {
            name: "codex",
            spent_until: None,
        }) as Arc<dyn Provider>]);
        ping_signed_in_providers(&registry, &cooldowns).await;
    }
}
