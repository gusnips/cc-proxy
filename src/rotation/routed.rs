//! A routed request on the request pool (`crate::pool`): the enabled rows in
//! their saved order, accounts of different providers mixed.
//!
//! A new chat takes the first row that is ready. A chat that already started
//! keeps its provider and account while they can still serve it, so the
//! provider's cache is reused, even after a row above recovers. When that row
//! hits a limit, is turned off or is removed, the chat moves down the pool.
//! Running requests are pinned to the account they started on. A chat's place is
//! saved (see `chats`), so a restart or an update keeps it too.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use axum::response::Response;
use http::StatusCode;

use super::chats::{Place, assign, place_of, remember};
use super::{
    Cooldown, Cooldowns, MAX_ROTATION_WAIT_MS, RESET_WAIT_SLICE_MS, ServedBy, all_limited, execute,
    exhausted_response, now_ms, retry_deadline, spent_response, storage_error, wait_while_paused,
};
use crate::accounts::{Account, Accounts};
use crate::anthropic::{MessagesRequest, json_error};
use crate::config::{PoolEntry, Route};
use crate::provider::{AuthState, Provider, RequestContext};
use crate::registry::{Registry, anthropic_model_family, normalize_incoming_model};
use crate::request_identity::ConversationIdentity;

/// One row that can serve this request: who answers, and with which model.
struct Plan {
    row: PoolEntry,
    provider: Arc<dyn Provider>,
    model: String,
}

impl Plan {
    fn place(&self) -> Place {
        (self.row.provider.clone(), self.row.account_id.clone())
    }

    /// The id a wait is saved under. A provider with one credential has no
    /// account id, so its waits use a fixed name.
    fn wait_id(&self) -> &str {
        self.row.account_id.as_deref().unwrap_or("default")
    }

    /// The longest wait that applies to this request on this row.
    fn wait<'a>(&self, snapshot: &'a [Cooldown], now: u64) -> Option<&'a Cooldown> {
        snapshot
            .iter()
            .filter(|c| {
                c.provider == self.row.provider
                    && c.account == self.wait_id()
                    && c.model.as_deref().is_none_or(|m| m == self.model)
                    && c.until_ms > now
            })
            .max_by_key(|c| c.until_ms)
    }
}

/// The saved accounts of one provider, read once for a request.
#[derive(Default)]
pub(super) struct Saved {
    accounts: Vec<Account>,
    /// Accounts whose plan can't serve requests at all (a free Claude plan).
    /// Left empty when every account is like that, so the provider's own error
    /// reaches the person instead of a made-up one.
    pub(super) unusable: HashSet<String>,
}

impl Saved {
    pub(super) fn read(provider: &dyn Provider) -> Self {
        let Some(support) = provider
            .cli()
            .accounts()
            .filter(|support| support.pinned_credentials().is_none())
        else {
            return Self::default();
        };
        let Ok(accounts) = Accounts::open(provider.name(), support) else {
            return Self::default();
        };
        let Ok(list) = accounts.list() else {
            return Self::default();
        };
        let unusable: HashSet<String> = list
            .iter()
            .filter(|account| accounts.unusable(support, account).is_some())
            .map(|account| account.id.clone())
            .collect();
        Self {
            unusable: if unusable.len() == list.len() {
                HashSet::new()
            } else {
                unusable
            },
            accounts: list,
        }
    }

    fn name_of(&self, id: &str) -> String {
        self.accounts
            .iter()
            .find(|account| account.id == id)
            .map_or_else(|| id.to_string(), |account| account.name.clone())
    }
}

/// The rows this request may use, or `None` when the pool does not decide it.
/// A Claude name (`opus`, `sonnet`) can go to any enabled row. A name that
/// belongs to one provider (`gpt-6`, `kimi-k2.6`) only uses that provider's
/// enabled rows, and answers as before when it has none: asking for a model by
/// name is a choice of provider. With no row at all (nothing is signed in) the
/// provider the server chose answers, so the person reads its own sign-in help.
///
/// Claude Code's `[1m]` suffix asks for the long context window and does not
/// change the model. Read with it, `claude-opus-4-8[1m]` would look like one
/// provider's own model, and a chat warm on a fallback would go back to the
/// first provider. So the names are read without it here and in `plan_rows`.
fn pool_rows(
    registry: &Registry,
    route: &Route,
    primary: &str,
    requested: &str,
) -> Option<Vec<PoolEntry>> {
    if !route.on {
        return None;
    }
    let mut rows = crate::pool::active(route, registry);
    if anthropic_model_family(&normalize_incoming_model(requested)).is_none() {
        rows.retain(|row| row.provider == primary);
    }
    (!rows.is_empty()).then_some(rows)
}

/// Which rows can answer, in pool order. The text is why the first row that
/// had to be left out could not answer.
fn plan_rows(
    registry: &Registry,
    route: &Route,
    requested: &str,
    primary: &Arc<dyn Provider>,
    model: &str,
    rows: Vec<PoolEntry>,
    saved: &mut HashMap<String, Saved>,
) -> (Vec<Plan>, Option<String>) {
    let requested = normalize_incoming_model(requested);
    let tiered = anthropic_model_family(&requested).is_some();
    let mut plans = Vec::new();
    let mut refusal = None;
    for row in rows {
        let (provider, model) = if tiered {
            match crate::tiers::resolve_for(registry, route, &row.provider, &requested) {
                Ok(model) => match registry.provider_for_model(&model) {
                    Some(provider) if provider.name() == row.provider => (provider, model),
                    _ => continue,
                },
                Err(message) => {
                    refusal.get_or_insert(message);
                    continue;
                }
            }
        } else {
            (primary.clone(), model.to_string())
        };
        let usable = match &row.account_id {
            Some(id) => !saved
                .entry(row.provider.clone())
                .or_insert_with(|| Saved::read(provider.as_ref()))
                .unusable
                .contains(id),
            // A provider nobody signed in to would answer with an auth error
            // and hide the limit this request is really about.
            None => !matches!(provider.cli().auth_state(), AuthState::Missing),
        };
        if usable {
            plans.push(Plan {
                row,
                provider,
                model,
            });
        }
    }
    (plans, refusal)
}

/// The key a request's chat is remembered under, and the main chat's key when
/// the request is a helper agent's. The main chat and its helper agents share
/// one session header, so each helper gets a key of its own: it starts where the
/// main chat is, and whatever it does never moves the main chat. A request with
/// no valid identity is not remembered, so a malformed header cannot take over
/// a real chat's place.
fn chat_keys(identity: Option<&ConversationIdentity>) -> (Option<String>, Option<String>) {
    match identity {
        None => (None, None),
        Some(ConversationIdentity::Main(session)) => (Some(session.clone()), None),
        Some(agent @ ConversationIdentity::Agent(session, _)) => (
            Some(agent.search_session_id().into_owned()),
            Some(session.clone()),
        ),
    }
}

fn ready(
    plans: &[Plan],
    at: usize,
    tried: &HashSet<usize>,
    snapshot: &[Cooldown],
    now: u64,
) -> bool {
    !tried.contains(&at) && plans[at].wait(snapshot, now).is_none()
}

/// The chat's own row when it can serve, else the first row that is ready.
fn pick(
    plans: &[Plan],
    chat: Option<&Place>,
    tried: &HashSet<usize>,
    snapshot: &[Cooldown],
    now: u64,
) -> Option<usize> {
    let is_ready = |at: usize| ready(plans, at, tried, snapshot, now);
    chat.and_then(|place| plans.iter().position(|plan| &plan.place() == place))
        .filter(|at| is_ready(*at))
        .or_else(|| (0..plans.len()).find(|at| is_ready(*at)))
}

/// A chat with no row of its own that can serve takes the first row that is
/// ready, `first`, or any other of the first `limit` ready rows, whatever their
/// provider: the one with the fewest recent chats. The order of the list still
/// ranks the rows: a row past those takes new chats only once some of them are
/// waiting or turned off. The choice is saved before the request starts, so new
/// chats that arrive together do not all pick the same account.
fn spread(
    plans: &[Plan],
    first: usize,
    chat: &str,
    tried: &HashSet<usize>,
    snapshot: &[Cooldown],
    now: u64,
    limit: usize,
) -> usize {
    let group: Vec<usize> = (first..plans.len())
        .filter(|at| ready(plans, *at, tried, snapshot, now))
        .take(limit)
        .collect();
    if group.len() < 2 {
        return first;
    }
    let places: Vec<Place> = group.iter().map(|at| plans[*at].place()).collect();
    assign(chat, &places)
        .and_then(|place| group.into_iter().find(|at| plans[*at].place() == place))
        .unwrap_or(first)
}

fn first_release(plans: &[Plan], snapshot: &[Cooldown], now: u64) -> Option<u64> {
    plans
        .iter()
        .filter_map(|plan| plan.wait(snapshot, now))
        .filter(|wait| !wait.spent)
        .map(|wait| wait.until_ms)
        .min()
}

/// A routed request: the enabled rows of the pool in order, each at most once.
/// A row is skipped the moment its provider says its account is spent or at a
/// limit with a retry time. When every row waits, the answer names the one that
/// frees up first and when, which is what lets Claude Code count down and
/// resume by itself.
pub(crate) async fn execute_routed(
    registry: &Registry,
    route: &Route,
    requested: &str,
    primary: Arc<dyn Provider>,
    body: MessagesRequest,
    context: RequestContext,
    identity: Option<ConversationIdentity>,
) -> Response {
    let Some(rows) = pool_rows(registry, route, primary.name(), requested) else {
        return execute(primary.as_ref(), body, context, identity).await;
    };
    let pool = Pool {
        registry,
        route,
        requested,
        primary,
        saved: HashMap::new(),
        auto: crate::config::auth_auto_rotate(),
        spread: crate::config::auth_spread_chats(),
        wait_for_reset: None,
        cooldowns: Cooldowns::new(),
    };
    pool.drive(rows, body, context, identity).await
}

/// What one routed request works from.
struct Pool<'a> {
    registry: &'a Registry,
    route: &'a Route,
    /// The model name Claude Code sent.
    requested: &'a str,
    /// The provider the server first chose for the request.
    primary: Arc<dyn Provider>,
    /// The saved accounts of the providers, read when a row needs them.
    saved: HashMap<String, Saved>,
    /// `auth.autoRotate`: move on from a row that hits a limit.
    auto: bool,
    /// `auth.spreadChats`: how many of the top ready rows share new chats.
    spread: usize,
    /// `None` reads `auth.waitForReset` live; tests pass `Some` to stay in memory.
    wait_for_reset: Option<bool>,
    cooldowns: Cooldowns,
}

impl Pool<'_> {
    fn hold_until_reset(&self) -> bool {
        self.wait_for_reset
            .unwrap_or_else(crate::config::auth_wait_for_reset)
    }

    /// Tries the enabled `rows` in order. See [`execute_routed`].
    async fn drive(
        mut self,
        rows: Vec<PoolEntry>,
        body: MessagesRequest,
        context: RequestContext,
        identity: Option<ConversationIdentity>,
    ) -> Response {
        let enabled: Vec<Place> = rows
            .iter()
            .map(|row| (row.provider.clone(), row.account_id.clone()))
            .collect();
        let (plans, refusal) = plan_rows(
            self.registry,
            self.route,
            self.requested,
            &self.primary,
            body.model.as_deref().unwrap_or_default(),
            rows,
            &mut self.saved,
        );
        let auto = self.auto;
        if plans.is_empty() {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                refusal.unwrap_or_else(|| {
                    "No account in the pool can serve this request. Run `cc-proxy open` to turn on \
                 an account, or `cc-proxy login <provider>` to add one."
                        .to_string()
                }),
            );
        }

        let log = crate::logging::create_logger("auth");
        let (chat, root) = chat_keys(identity.as_ref());
        let own = chat.as_deref().and_then(place_of);
        let root_place = root.as_deref().and_then(place_of);
        let remembered = own.clone().or_else(|| root_place.clone());
        // A chat keeps its place unless that row is gone, turned off or
        // waiting. A row that only has no model for this request (the chat's
        // other tiers) stays its place, and this request must not replace it.
        let kept = remembered.as_ref().is_some_and(|place| {
            enabled.contains(place) && !plans.iter().any(|p| &p.place() == place)
        });
        let mut tried: HashSet<usize> = HashSet::new();
        let mut last_response = None;
        let mut waited = false;
        let mut shown = self.primary.name().to_string();
        loop {
            wait_while_paused(&context.req_id, context.monitor.as_ref()).await;
            // Without auto-rotate there are no saved waits to honor: the first row answers.
            let snapshot = if auto {
                match self.cooldowns.read() {
                    Ok(snapshot) => snapshot,
                    Err(error) => return storage_error(error),
                }
            } else {
                Vec::new()
            };
            let now = now_ms();
            let Some(mut at) = pick(&plans, remembered.as_ref(), &tried, &snapshot, now) else {
                // Nothing ready: hold the request until the first row frees up
                // (bounded by default, or until reset when `auth.waitForReset`
                // is on), so the turn continues on it instead of dying on a 429.
                let hold_until_reset = self.hold_until_reset();
                if auto
                    && let Some(mut until) = first_release(&plans, &snapshot, now)
                    && until > now
                    && ((!waited && until - now <= MAX_ROTATION_WAIT_MS) || hold_until_reset)
                {
                    let short_wait = !waited && until - now <= MAX_ROTATION_WAIT_MS;
                    waited = true;
                    log.info(
                        "account_wait",
                        Some(serde_json::Map::from_iter([
                            ("provider".into(), serde_json::json!(self.primary.name())),
                            ("waitMs".into(), serde_json::json!(until - now)),
                            ("reqId".into(), serde_json::json!(&context.req_id)),
                        ])),
                    );
                    if let Some(monitor) = context.monitor.as_ref() {
                        monitor.request_held(&context.req_id, Some(until));
                    }
                    loop {
                        let now = now_ms();
                        if now >= until {
                            break;
                        }
                        if !short_wait && !self.hold_until_reset() {
                            break;
                        }
                        let slice = (until - now).clamp(1, RESET_WAIT_SLICE_MS);
                        tokio::time::sleep(Duration::from_millis(slice)).await;
                        let fresh = match self.cooldowns.read() {
                            Ok(entries) => entries,
                            Err(error) => {
                                if let Some(monitor) = context.monitor.as_ref() {
                                    monitor.request_released(&context.req_id);
                                }
                                return storage_error(error);
                            }
                        };
                        let now = now_ms();
                        tried.retain(|at| plans[*at].wait(&fresh, now).is_some());
                        if pick(&plans, remembered.as_ref(), &tried, &fresh, now).is_some() {
                            break;
                        }
                        let Some(next_until) = first_release(&plans, &fresh, now) else {
                            break;
                        };
                        if next_until != until {
                            until = next_until;
                            if let Some(monitor) = context.monitor.as_ref() {
                                monitor.request_held(&context.req_id, Some(until));
                            }
                        }
                    }
                    if let Some(monitor) = context.monitor.as_ref() {
                        monitor.request_released(&context.req_id);
                    }
                    let fresh = match self.cooldowns.read() {
                        Ok(entries) => entries,
                        Err(error) => return storage_error(error),
                    };
                    let now = now_ms();
                    tried.retain(|at| plans[*at].wait(&fresh, now).is_some());
                    if pick(&plans, remembered.as_ref(), &tried, &fresh, now).is_some() {
                        continue;
                    }
                }
                return all_limited_pool(&plans, &snapshot, now, &self.saved)
                    .or(last_response)
                    .unwrap_or_else(|| {
                        json_error(
                            StatusCode::TOO_MANY_REQUESTS,
                            "rate_limit_error",
                            "Every account in the pool was tried. Try the request again later.",
                        )
                    });
            };
            // Without auto-rotate the first row answers everything, so nothing spreads.
            if auto
                && !kept
                && let Some(chat) = chat.as_deref()
                && remembered.as_ref() != Some(&plans[at].place())
            {
                at = spread(&plans, at, chat, &tried, &snapshot, now, self.spread);
            }
            tried.insert(at);
            let plan = &plans[at];
            let provider = plan.provider.name();
            if provider != shown {
                log.info(
                    "provider_switched",
                    Some(serde_json::Map::from_iter([
                        ("from".into(), serde_json::json!(&shown)),
                        ("to".into(), serde_json::json!(provider)),
                        ("model".into(), serde_json::json!(&plan.model)),
                        ("reqId".into(), serde_json::json!(&context.req_id)),
                    ])),
                );
                if let Some(monitor) = &context.monitor {
                    monitor.provider_switched(&context.req_id, provider, &plan.model);
                }
                shown = provider.to_string();
            }
            let mut attempt_body = body.clone();
            attempt_body.model = Some(plan.model.clone());
            crate::anthropic::request::prepare_for_provider(&mut attempt_body, provider);
            let mut attempt = context.clone();
            attempt.provider = provider.to_string();
            attempt.account = plan.row.account_id.clone();
            let mut response = plan
                .provider
                .handle_messages_with_conversation_identity(attempt_body, attempt, identity.clone())
                .await;
            let sitout = auto.then(|| retry_deadline(&response, now_ms())).flatten();
            let Some(sitout) = sitout else {
                if let Some(chat) = chat.as_deref() {
                    // A helper agent that stays where the main chat is needs
                    // no place of its own.
                    let follows_root = own.is_none() && root_place.as_ref() == Some(&plan.place());
                    if !kept && !follows_root {
                        remember(chat, plan.place());
                    }
                }
                if provider != self.primary.name() {
                    response.extensions_mut().insert(ServedBy {
                        provider: provider.to_string(),
                        model: plan.model.clone(),
                    });
                }
                return response;
            };
            if let Err(error) =
                self.cooldowns
                    .sit_out(provider, plan.wait_id(), &plan.model, &sitout)
            {
                return storage_error(error);
            }
            if sitout.spent {
                log.info(
                    "account_out_of_credit",
                    Some(serde_json::Map::from_iter([
                        ("provider".into(), serde_json::json!(provider)),
                        ("account".into(), serde_json::json!(self.account_name(plan))),
                        ("reqId".into(), serde_json::json!(&context.req_id)),
                    ])),
                );
            }
            last_response = Some(response);
        }
    }

    /// What the logs call the row's account: its saved name, else the provider.
    fn account_name(&self, plan: &Plan) -> String {
        let provider = plan.provider.name();
        match &plan.row.account_id {
            Some(id) => self
                .saved
                .get(provider)
                .map_or_else(|| id.clone(), |saved| saved.name_of(id)),
            None => provider.to_string(),
        }
    }
}

/// The answer when every row waits: the earliest reset, or when a spent
/// balance is tried again. `None` while any row is free.
fn all_limited_pool(
    plans: &[Plan],
    snapshot: &[Cooldown],
    now: u64,
    saved: &HashMap<String, Saved>,
) -> Option<Response> {
    let waits = plans
        .iter()
        .map(|plan| plan.wait(snapshot, now).map(|wait| (plan, wait)))
        .collect::<Option<Vec<_>>>()?;
    let only = plans[0].provider.name();
    if plans.iter().all(|plan| plan.provider.name() == only) {
        // One provider: its own message names the accounts to fix.
        let accounts: Vec<Account> = saved.get(only).map_or_else(Vec::new, |saved| {
            let in_pool = |account: &&Account| {
                plans
                    .iter()
                    .any(|plan| plan.row.account_id.as_deref() == Some(account.id.as_str()))
            };
            saved.accounts.iter().filter(in_pool).cloned().collect()
        });
        if let Some(response) = all_limited(&accounts, snapshot, only, &plans[0].model, now) {
            return Some(response);
        }
    }
    let (plan, first) = waits.iter().min_by_key(|(_, wait)| wait.until_ms)?;
    let wait = crate::ui::duration(Duration::from_millis(first.until_ms.saturating_sub(now)));
    let provider = crate::ui::provider_name(&plan.row.provider);
    Some(if first.spent {
        spent_response(&format!(
            "Every account in the pool is out of credit or at its limit. cc-proxy tries \
             {provider} again in {wait}. Add credit, or use `cc-proxy auth add` to add an \
             account."
        ))
    } else {
        exhausted_response(
            first.until_ms,
            &format!(
                "Every account in the pool is at its limit. {provider} frees up first, in \
                 {wait}. Use `cc-proxy auth add` to add an account sooner."
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::{EndpointKind, MonitorHandle};
    use crate::provider::CliHandlers;
    use async_trait::async_trait;
    use std::sync::Mutex;

    type Calls = Arc<Mutex<Vec<Place>>>;

    /// What each fake answers: a limit with a retry time, or a spent balance.
    #[derive(Clone, Default)]
    struct Rules {
        limited: Arc<Mutex<HashSet<Place>>>,
        broke: Arc<Mutex<HashSet<Place>>>,
    }

    struct Fake {
        name: &'static str,
        calls: Calls,
        rules: Rules,
    }

    impl CliHandlers for Fake {
        fn auth_state(&self) -> AuthState {
            AuthState::SignedIn {
                account: None,
                expires_ms: None,
            }
        }
        fn login(&self) -> anyhow::Result<()> {
            unreachable!()
        }
        fn device(&self) -> anyhow::Result<()> {
            unreachable!()
        }
        fn status(&self) -> anyhow::Result<()> {
            unreachable!()
        }
        fn logout(&self) -> anyhow::Result<()> {
            unreachable!()
        }
    }

    #[async_trait]
    impl Provider for Fake {
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
        async fn handle_messages(&self, _: MessagesRequest, ctx: RequestContext) -> Response {
            let place = (self.name.to_string(), ctx.account.clone());
            self.calls.lock().unwrap().push(place.clone());
            if self.rules.limited.lock().unwrap().contains(&place) {
                let mut response = json_error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "rate_limit_error",
                    "limit reached",
                );
                response
                    .extensions_mut()
                    .insert(super::super::RotationHint::RetryAt {
                        until_ms: now_ms() + 3_600_000,
                        per_model: false,
                    });
                return response;
            }
            if self.rules.broke.lock().unwrap().contains(&place) {
                let mut response = json_error(
                    StatusCode::PAYMENT_REQUIRED,
                    "billing_error",
                    "credit balance is too low",
                );
                response
                    .extensions_mut()
                    .insert(super::super::RotationHint::Spent);
                return response;
            }
            Response::new(axum::body::Body::from(format!(
                "{}:{}",
                self.name,
                ctx.account.unwrap_or_default()
            )))
        }
    }

    const CLAUDE_PRO: &str = "claude-pro";
    const CODEX_ONE: &str = "codex-one";
    const CLAUDE_MAX: &str = "claude-max";

    fn row(provider: &str, id: Option<&str>) -> PoolEntry {
        PoolEntry {
            provider: provider.into(),
            account_id: id.map(str::to_string),
            enabled: true,
        }
    }

    fn place(provider: &str, id: Option<&str>) -> Place {
        (provider.to_string(), id.map(str::to_string))
    }

    /// Claude (two accounts) and Codex (one) with fakes behind them.
    struct World {
        registry: Registry,
        route: Route,
        calls: Calls,
        rules: Rules,
        /// `auth.spreadChats` for the requests this world asks. It starts at 1,
        /// so a new chat takes the first ready row and the tests that follow
        /// the order of the list stay exact. The spread tests turn it up.
        spread: usize,
        _dir: tempfile::TempDir,
        path: std::path::PathBuf,
    }

    fn world() -> World {
        let calls = Calls::default();
        let rules = Rules::default();
        let fake = |name| {
            Arc::new(Fake {
                name,
                calls: calls.clone(),
                rules: rules.clone(),
            }) as Arc<dyn Provider>
        };
        let dir = tempfile::tempdir().unwrap();
        let tiers = ["claude", "codex"]
            .into_iter()
            .map(|name| {
                (
                    name.to_string(),
                    [("opus".to_string(), format!("{name}/test-1"))].into(),
                )
            })
            .collect();
        World {
            registry: Registry::from_providers([fake("claude"), fake("codex")]),
            route: Route {
                on: true,
                tiers,
                ..Route::default()
            },
            calls,
            rules,
            spread: 1,
            path: dir.path().join("rotation.json"),
            _dir: dir,
        }
    }

    fn account(id: &str) -> Account {
        Account {
            id: id.into(),
            name: id.into(),
            identity: None,
        }
    }

    impl World {
        fn limit(&self, provider: &str, id: Option<&str>) {
            self.rules
                .limited
                .lock()
                .unwrap()
                .insert(place(provider, id));
        }

        fn recover(&self, provider: &str, id: &str) {
            self.rules
                .limited
                .lock()
                .unwrap()
                .remove(&place(provider, Some(id)));
            Cooldowns {
                path: self.path.clone(),
            }
            .clear_account(provider, id)
            .unwrap();
        }

        /// One request for `opus` against `rows` (the enabled ones, in order).
        /// Returns who answered, from the fake's own body.
        async fn ask(&self, rows: &[PoolEntry], chat: Option<&str>) -> Response {
            self.ask_with(rows, chat, true, "claude-opus-4-8", None)
                .await
        }

        async fn ask_with(
            &self,
            rows: &[PoolEntry],
            chat: Option<&str>,
            auto: bool,
            requested: &str,
            monitor: Option<MonitorHandle>,
        ) -> Response {
            let identity = chat.map(|chat| ConversationIdentity::Main(chat.to_string()));
            self.ask_as(rows, identity, auto, requested, monitor).await
        }

        /// A request from this conversation: the main chat or a helper agent.
        async fn ask_as(
            &self,
            rows: &[PoolEntry],
            identity: Option<ConversationIdentity>,
            auto: bool,
            requested: &str,
            monitor: Option<MonitorHandle>,
        ) -> Response {
            let saved = [
                (
                    "claude".to_string(),
                    Saved {
                        accounts: vec![account(CLAUDE_PRO), account(CLAUDE_MAX)],
                        unusable: HashSet::new(),
                    },
                ),
                (
                    "codex".to_string(),
                    Saved {
                        accounts: vec![account(CODEX_ONE)],
                        unusable: HashSet::new(),
                    },
                ),
            ]
            .into();
            let first = rows
                .first()
                .map_or("claude", |row| row.provider.as_str())
                .to_string();
            let primary = self.registry.provider(&first).unwrap();
            let pool = Pool {
                registry: &self.registry,
                route: &self.route,
                requested,
                primary,
                saved,
                auto,
                spread: self.spread,
                wait_for_reset: Some(false),
                cooldowns: Cooldowns {
                    path: self.path.clone(),
                },
            };
            let body = serde_json::from_value(
                serde_json::json!({"messages": [], "model": format!("{first}/test-1")}),
            )
            .unwrap();
            let context = RequestContext {
                req_id: "pool-test".into(),
                session_id: None,
                session_seq: None,
                provider: first,
                account: None,
                traffic: None,
                monitor,
            };
            pool.drive(rows.to_vec(), body, context, identity).await
        }

        fn served(&self) -> Vec<Place> {
            std::mem::take(&mut *self.calls.lock().unwrap())
        }
    }

    async fn text(response: Response) -> String {
        let body = axum::body::to_bytes(response.into_body(), 8192)
            .await
            .unwrap();
        String::from_utf8_lossy(&body).into_owned()
    }

    fn mixed() -> Vec<PoolEntry> {
        vec![
            row("claude", Some(CLAUDE_PRO)),
            row("codex", Some(CODEX_ONE)),
            row("claude", Some(CLAUDE_MAX)),
        ]
    }

    #[tokio::test]
    async fn accounts_of_different_providers_are_tried_in_the_saved_order() {
        let world = world();
        world.limit("claude", Some(CLAUDE_PRO));
        let response = world.ask(&mixed(), None).await;
        assert_eq!(text(response).await, format!("codex:{CODEX_ONE}"));
        assert_eq!(
            world.served(),
            [
                place("claude", Some(CLAUDE_PRO)),
                place("codex", Some(CODEX_ONE))
            ]
        );
        // The limited account sits out, so the next request goes straight on.
        world.limit("codex", Some(CODEX_ONE));
        let response = world.ask(&mixed(), None).await;
        assert_eq!(text(response).await, format!("claude:{CLAUDE_MAX}"));
        assert_eq!(
            world.served(),
            [
                place("codex", Some(CODEX_ONE)),
                place("claude", Some(CLAUDE_MAX))
            ]
        );
    }

    #[tokio::test]
    async fn a_new_chat_takes_the_first_row_that_is_ready_once_it_recovers() {
        let world = world();
        world.limit("claude", Some(CLAUDE_PRO));
        let first = world.ask(&mixed(), Some("chat-a")).await;
        assert_eq!(text(first).await, format!("codex:{CODEX_ONE}"));
        world.served();
        world.recover("claude", CLAUDE_PRO);
        // The chat that started on Codex stays there: its cache is warm.
        let same = world.ask(&mixed(), Some("chat-a")).await;
        assert_eq!(text(same).await, format!("codex:{CODEX_ONE}"));
        // A new chat, and a request with no chat, start from the top again.
        let fresh = world.ask(&mixed(), Some("chat-b")).await;
        assert_eq!(text(fresh).await, format!("claude:{CLAUDE_PRO}"));
        let none = world.ask(&mixed(), None).await;
        assert_eq!(text(none).await, format!("claude:{CLAUDE_PRO}"));
    }

    #[tokio::test]
    async fn a_chat_moves_down_the_pool_when_its_row_is_limited_turned_off_or_removed() {
        let world = world();
        let started = world.ask(&mixed()[1..], Some("chat-c")).await;
        assert_eq!(text(started).await, format!("codex:{CODEX_ONE}"));
        world.served();
        // Its row hits a limit: the request moves on and the chat follows.
        world.limit("codex", Some(CODEX_ONE));
        let moved = world.ask(&mixed(), Some("chat-c")).await;
        assert_eq!(text(moved).await, format!("claude:{CLAUDE_PRO}"));
        world.served();
        world.recover("codex", CODEX_ONE);
        let kept = world.ask(&mixed(), Some("chat-c")).await;
        assert_eq!(text(kept).await, format!("claude:{CLAUDE_PRO}"));
        // Its row turned off or removed: the first ready row answers.
        let rows = vec![
            row("codex", Some(CODEX_ONE)),
            row("claude", Some(CLAUDE_MAX)),
        ];
        let off = world.ask(&rows, Some("chat-c")).await;
        assert_eq!(text(off).await, format!("codex:{CODEX_ONE}"));
    }

    #[tokio::test]
    async fn a_chat_keeps_its_place_when_a_request_only_lacks_a_model_for_it() {
        let world = world();
        let started = world.ask(&mixed()[1..], Some("chat-d")).await;
        assert_eq!(text(started).await, format!("codex:{CODEX_ONE}"));
        world.served();
        // No `haiku` model is set for Codex here, so Claude answers this one.
        let other = world
            .ask_with(&mixed(), Some("chat-d"), true, "claude-haiku-4-5", None)
            .await;
        assert!(text(other).await.starts_with("claude:"));
        // The chat is still on Codex for its next `opus` request.
        let back = world.ask(&mixed(), Some("chat-d")).await;
        assert_eq!(text(back).await, format!("codex:{CODEX_ONE}"));
    }

    #[tokio::test]
    async fn a_helper_agent_never_moves_the_main_chat() {
        let world = world();
        let main = || Some(ConversationIdentity::Main("chat-main".into()));
        let helper = |agent: &str| {
            Some(ConversationIdentity::Agent(
                "chat-main".into(),
                agent.into(),
            ))
        };
        let started = world
            .ask_as(&mixed(), main(), true, "claude-opus-4-8", None)
            .await;
        assert_eq!(text(started).await, format!("claude:{CLAUDE_PRO}"));
        world.served();

        // A helper that asks for a Codex model uses Codex only: that is its own
        // choice, and the main chat stays on Claude with its warm cache.
        let codex_only = vec![row("codex", Some(CODEX_ONE))];
        let child = world
            .ask_as(
                &codex_only,
                helper("agent-1"),
                true,
                "claude-opus-4-8",
                None,
            )
            .await;
        assert_eq!(text(child).await, format!("codex:{CODEX_ONE}"));
        world.served();
        let again = world
            .ask_as(&mixed(), main(), true, "claude-opus-4-8", None)
            .await;
        assert_eq!(text(again).await, format!("claude:{CLAUDE_PRO}"));
        world.served();

        // A new helper starts where the main chat is, and keeps no place of its own.
        let fresh = world
            .ask_as(&mixed(), helper("agent-2"), true, "claude-opus-4-8", None)
            .await;
        assert_eq!(text(fresh).await, format!("claude:{CLAUDE_PRO}"));
        assert!(
            place_of(
                &ConversationIdentity::Agent("chat-main".into(), "agent-2".into())
                    .search_session_id()
            )
            .is_none()
        );
        // The helper that left the main chat's place stays where it went.
        world.served();
        let stay = world
            .ask_as(&mixed(), helper("agent-1"), true, "claude-opus-4-8", None)
            .await;
        assert_eq!(text(stay).await, format!("codex:{CODEX_ONE}"));
    }

    #[tokio::test]
    async fn the_long_context_suffix_does_not_pull_a_chat_off_its_fallback() {
        let world = world();
        world.limit("claude", Some(CLAUDE_PRO));
        let started = world.ask(&mixed(), Some("chat-1m")).await;
        assert_eq!(text(started).await, format!("codex:{CODEX_ONE}"));
        world.served();
        world.recover("claude", CLAUDE_PRO);
        // `[1m]` is the same Claude name: every row stays open and the chat
        // stays on Codex, as it does without the suffix.
        let same = world
            .ask_with(&mixed(), Some("chat-1m"), true, "claude-opus-4-8[1m]", None)
            .await;
        assert_eq!(text(same).await, format!("codex:{CODEX_ONE}"));
        let mut route = world.route.clone();
        route.pool = Some(vec![row("claude", None), row("codex", None)]);
        assert_eq!(
            pool_rows(&world.registry, &route, "claude", "claude-opus-4-8[1m]"),
            pool_rows(&world.registry, &route, "claude", "claude-opus-4-8")
        );
    }

    #[tokio::test]
    async fn every_row_waiting_answers_with_the_first_reset() {
        let world = world();
        for row in mixed() {
            world.limit(&row.provider, row.account_id.as_deref());
        }
        let response = world.ask(&mixed(), None).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(
            response
                .headers()
                .contains_key("anthropic-ratelimit-unified-reset")
        );
        let message = text(response).await;
        assert!(message.contains("at its limit"), "{message}");
        assert_eq!(world.served().len(), 3);
        // The next request calls nobody: every row is waiting.
        let again = world.ask(&mixed(), None).await;
        assert_eq!(again.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(world.served().is_empty());
    }

    #[tokio::test]
    async fn every_balance_spent_says_how_to_fix_it_with_no_countdown() {
        let world = world();
        for row in mixed() {
            world
                .rules
                .broke
                .lock()
                .unwrap()
                .insert(place(&row.provider, row.account_id.as_deref()));
        }
        let response = world.ask(&mixed(), None).await;
        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
        assert!(
            !response
                .headers()
                .contains_key("anthropic-ratelimit-unified-reset")
        );
        let message = text(response).await;
        for expected in ["out of credit", "tries", "again in", "cc-proxy auth add"] {
            assert!(message.contains(expected), "{expected}: {message}");
        }
    }

    #[tokio::test]
    async fn one_provider_in_the_pool_keeps_the_message_that_names_its_accounts() {
        let world = world();
        let rows = vec![
            row("claude", Some(CLAUDE_PRO)),
            row("claude", Some(CLAUDE_MAX)),
        ];
        world.limit("claude", Some(CLAUDE_PRO));
        world.limit("claude", Some(CLAUDE_MAX));
        let response = world.ask(&rows, None).await;
        let message = text(response).await;
        assert!(message.contains("All 2 saved Claude"), "{message}");
        assert!(message.contains("cc-proxy auth add claude"), "{message}");
    }

    #[tokio::test]
    async fn without_auto_rotate_the_first_row_answers_whatever_it_says() {
        let world = world();
        world.limit("claude", Some(CLAUDE_PRO));
        let response = world
            .ask_with(&mixed(), None, false, "claude-opus-4-8", None)
            .await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(world.served(), [place("claude", Some(CLAUDE_PRO))]);
        // No wait was saved, so nothing else changes.
        assert!(
            Cooldowns {
                path: world.path.clone()
            }
            .read()
            .unwrap()
            .is_empty()
        );
    }

    #[tokio::test]
    async fn a_provider_with_one_credential_waits_under_a_fixed_name_and_is_skipped_next_time() {
        let world = world();
        let rows = vec![row("codex", None), row("claude", Some(CLAUDE_PRO))];
        world.limit("codex", None);
        let response = world.ask(&rows, None).await;
        assert_eq!(text(response).await, format!("claude:{CLAUDE_PRO}"));
        world.served();
        let again = world.ask(&rows, None).await;
        assert!(text(again).await.starts_with("claude:"));
        assert_eq!(world.served(), [place("claude", Some(CLAUDE_PRO))]);
    }

    #[tokio::test]
    async fn the_provider_that_answered_is_named_in_the_monitor_and_on_the_response() {
        let world = world();
        let monitor = MonitorHandle::default();
        monitor.request_started("pool-test", None, None, EndpointKind::Messages);
        monitor.provider_selected("pool-test", "claude", "claude/test-1", None);
        world.limit("claude", Some(CLAUDE_PRO));
        let response = world
            .ask_with(
                &mixed(),
                None,
                true,
                "claude-opus-4-8",
                Some(monitor.clone()),
            )
            .await;
        let by = response.extensions().get::<ServedBy>().unwrap();
        assert_eq!(
            (by.provider.as_str(), by.model.as_str()),
            ("codex", "codex/test-1")
        );
        let live = monitor.snapshot();
        assert_eq!(live.active[0].provider.as_deref(), Some("codex"));
        assert_eq!(live.active[0].model.as_deref(), Some("codex/test-1"));
        // Nothing to name when the first provider answers itself.
        world.recover("claude", CLAUDE_PRO);
        let own = world.ask(&mixed(), None).await;
        assert!(own.extensions().get::<ServedBy>().is_none());
    }

    #[test]
    fn a_name_that_belongs_to_one_provider_only_uses_that_providers_rows() {
        let world = world();
        let mut route = world.route.clone();
        route.pool = Some(vec![row("claude", None), row("codex", None)]);
        // A Claude name can go to any enabled row.
        let any = pool_rows(&world.registry, &route, "claude", "claude-opus-4-8").unwrap();
        assert_eq!(any.len(), 2);
        // A provider's own model name uses that provider's rows.
        let own = pool_rows(&world.registry, &route, "codex", "codex/test-1").unwrap();
        assert_eq!(own, [row("codex", None)]);
        // With none of its rows enabled it answers as before, and a turned
        // off route leaves the pool out of it.
        route.pool = Some(vec![
            row("claude", None),
            PoolEntry {
                enabled: false,
                ..row("codex", None)
            },
        ]);
        assert!(pool_rows(&world.registry, &route, "codex", "codex/test-1").is_none());
        route.on = false;
        assert!(pool_rows(&world.registry, &route, "claude", "claude-opus-4-8").is_none());
        // An empty pool is not a pool of one: nothing is enabled, so the
        // provider the server chose answers with its own message.
        route.on = true;
        route.pool = Some(Vec::new());
        assert!(pool_rows(&world.registry, &route, "claude", "claude-opus-4-8").is_none());
    }

    #[tokio::test]
    async fn a_chat_is_still_on_its_account_after_a_restart_and_a_missing_row_falls_through() {
        let world = world();
        world.limit("claude", Some(CLAUDE_PRO));
        let started = world.ask(&mixed(), Some("chat-restart")).await;
        assert_eq!(text(started).await, format!("codex:{CODEX_ONE}"));
        world.served();
        world.recover("claude", CLAUDE_PRO);

        // The proxy restarts: the places are read from the file again. The
        // chat stays on Codex even though the first row is ready.
        super::super::chats::forget_loaded();
        let same = world.ask(&mixed(), Some("chat-restart")).await;
        assert_eq!(text(same).await, format!("codex:{CODEX_ONE}"));
        assert_eq!(world.served(), [place("codex", Some(CODEX_ONE))]);

        // Its row was removed or turned off while the proxy was down: it takes
        // a ready row of the first provider, as a new chat does.
        super::super::chats::forget_loaded();
        let rows = vec![
            row("claude", Some(CLAUDE_PRO)),
            row("claude", Some(CLAUDE_MAX)),
        ];
        let moved = world.ask(&rows, Some("chat-restart")).await;
        assert!(text(moved).await.starts_with("claude:"));
    }

    #[tokio::test]
    async fn new_chats_spread_over_the_top_ready_accounts_of_any_provider() {
        let mut world = world();
        world.spread = 5;
        // Names of their own: places are counted across every chat in the process.
        let rows = vec![
            row("claude", Some("top-a")),
            row("codex", Some("top-b")),
            row("claude", Some("top-c")),
        ];
        let answer = |chat: &'static str| {
            let (world, rows) = (&world, &rows);
            async move { text(world.ask(rows, Some(chat)).await).await }
        };
        // Three new chats, three accounts, two providers, in the order of the list.
        assert_eq!(answer("top-1").await, "claude:top-a");
        assert_eq!(answer("top-2").await, "codex:top-b");
        assert_eq!(answer("top-3").await, "claude:top-c");
        assert_eq!(answer("top-4").await, "claude:top-a");
        // A chat stays where it started, even when the others are lighter.
        assert_eq!(answer("top-1").await, "claude:top-a");
        assert_eq!(answer("top-2").await, "codex:top-b");
        // An account that is waiting takes no new chat.
        world.limit("codex", Some("top-b"));
        assert_eq!(answer("top-5").await, "claude:top-c");
        // A chat whose account is waiting moves to a ready one, of any provider.
        assert_eq!(answer("top-2").await, "claude:top-a");
    }

    #[tokio::test]
    async fn spread_chats_stops_at_the_top_ready_rows() {
        let mut world = world();
        let rows = vec![
            row("claude", Some("cap-a")),
            row("codex", Some("cap-b")),
            row("claude", Some("cap-c")),
        ];
        // Two rows share new chats. The third row takes none while both are ready.
        world.spread = 2;
        for (chat, who) in [
            ("cap-1", "claude:cap-a"),
            ("cap-2", "codex:cap-b"),
            ("cap-3", "claude:cap-a"),
            ("cap-4", "codex:cap-b"),
        ] {
            assert_eq!(text(world.ask(&rows, Some(chat)).await).await, who);
        }
        // The third row counts once one of the top two is waiting.
        world.limit("claude", Some("cap-a"));
        assert_eq!(
            text(world.ask(&rows, Some("cap-5")).await).await,
            "claude:cap-c"
        );
        // One means the first ready row answers every new chat.
        world.spread = 1;
        for chat in ["one-1", "one-2", "one-3"] {
            let rows = [row("claude", Some("one-a")), row("codex", Some("one-b"))];
            assert_eq!(
                text(world.ask(&rows, Some(chat)).await).await,
                "claude:one-a"
            );
        }
    }

    #[tokio::test]
    async fn auto_rotate_off_spreads_nothing() {
        let world = world();
        // Without auto-rotate the first row answers every request.
        let rows = vec![row("claude", Some("off-a")), row("claude", Some("off-b"))];
        for chat in ["off-1", "off-2", "off-3"] {
            let answer = world
                .ask_with(&rows, Some(chat), false, "claude-opus-4-8", None)
                .await;
            assert_eq!(text(answer).await, "claude:off-a");
        }
        // A request with no valid identity is not remembered, so it takes the first row.
        let loose = world.ask(&rows, None).await;
        assert_eq!(text(loose).await, "claude:off-a");
    }

    #[tokio::test]
    async fn an_empty_pool_says_what_to_turn_on_instead_of_picking_a_provider() {
        let world = world();
        let response = world.ask(&[], None).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let message = text(response).await;
        assert!(message.contains("cc-proxy open"), "{message}");
        assert!(world.served().is_empty());
    }

    #[tokio::test]
    async fn an_account_whose_plan_cannot_serve_is_skipped() {
        let world = world();
        let saved = Saved {
            accounts: vec![account(CLAUDE_PRO), account(CLAUDE_MAX)],
            unusable: HashSet::from([CLAUDE_PRO.to_string()]),
        };
        let mut map = HashMap::from([("claude".to_string(), saved)]);
        let (plans, _) = plan_rows(
            &world.registry,
            &world.route,
            "claude-opus-4-8",
            &world.registry.provider("claude").unwrap(),
            "claude/test-1",
            vec![
                row("claude", Some(CLAUDE_PRO)),
                row("claude", Some(CLAUDE_MAX)),
            ],
            &mut map,
        );
        assert_eq!(
            plans.iter().map(Plan::place).collect::<Vec<_>>(),
            [place("claude", Some(CLAUDE_MAX))]
        );
    }

    #[tokio::test]
    async fn wait_for_reset_holds_through_a_long_window_and_skips_spent_balances() {
        let world = world();
        let monitor = MonitorHandle::default();
        monitor.request_started("pool-wait", None, None, EndpointKind::Messages);
        let cooldowns = Cooldowns {
            path: world.path.clone(),
        };
        // A reset 10 minutes out is past the 2-minute default, so wait_for_reset
        // is what holds the request until the account frees up.
        let until = now_ms() + 600_000;
        cooldowns
            .mark_with("claude", CLAUDE_PRO, None, until, false)
            .unwrap();
        let rows = vec![row("claude", Some(CLAUDE_PRO))];
        let saved = HashMap::from([(
            "claude".to_string(),
            Saved {
                accounts: vec![account(CLAUDE_PRO)],
                unusable: HashSet::new(),
            },
        )]);
        let primary = world.registry.provider("claude").unwrap();
        let pool = Pool {
            registry: &world.registry,
            route: &world.route,
            requested: "claude-opus-4-8",
            primary,
            saved,
            auto: true,
            spread: world.spread,
            wait_for_reset: Some(true),
            cooldowns: Cooldowns {
                path: world.path.clone(),
            },
        };
        let body =
            serde_json::from_value(serde_json::json!({"messages": [], "model": "claude/test-1"}))
                .unwrap();
        let context = RequestContext {
            req_id: "pool-wait".into(),
            session_id: None,
            session_seq: None,
            provider: "claude".into(),
            account: None,
            traffic: None,
            monitor: Some(monitor.clone()),
        };
        let release_path = world.path.clone();
        let release_monitor = monitor.clone();
        let releaser = tokio::spawn(async move {
            for _ in 0..40 {
                let state = release_monitor.snapshot();
                if state
                    .active
                    .first()
                    .is_some_and(|req| req.wait_until_ms == Some(until))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Cooldowns { path: release_path }
                .clear_account("claude", CLAUDE_PRO)
                .unwrap();
        });
        let response = pool.drive(rows, body, context, None).await;
        releaser.await.unwrap();
        assert_eq!(text(response).await, format!("claude:{CLAUDE_PRO}"));
        assert_eq!(monitor.snapshot().active[0].wait_until_ms, None);

        // A spent balance has no reset time, so wait_for_reset never hangs on it.
        cooldowns
            .mark_with("claude", CLAUDE_PRO, None, now_ms() + 60_000, true)
            .unwrap();
        assert_eq!(
            first_release(
                &[Plan {
                    row: row("claude", Some(CLAUDE_PRO)),
                    provider: world.registry.provider("claude").unwrap(),
                    model: "claude/test-1".into(),
                }],
                &cooldowns.read().unwrap(),
                now_ms()
            ),
            None
        );
    }

    #[tokio::test]
    async fn manual_pause_holds_new_requests_and_releases_when_resumed() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let monitor = MonitorHandle::default();
        monitor.request_started("req-pause", None, None, EndpointKind::Messages);
        let paused = Arc::new(AtomicBool::new(true));
        let flag = paused.clone();
        let monitor_clone = monitor.clone();
        let waiter = tokio::spawn(async move {
            super::super::wait_while_paused_with(
                "req-pause",
                Some(&monitor_clone),
                move || flag.load(Ordering::SeqCst),
                Duration::from_millis(5),
            )
            .await;
        });
        for _ in 0..40 {
            let state = monitor.snapshot();
            if state
                .active
                .first()
                .is_some_and(|req| req.status == crate::monitor::RequestStatus::Paused)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            monitor.snapshot().active[0].status,
            crate::monitor::RequestStatus::Paused
        );
        paused.store(false, Ordering::SeqCst);
        waiter.await.unwrap();
        assert_eq!(
            monitor.snapshot().active[0].status,
            crate::monitor::RequestStatus::Started
        );
    }
}
