//! A routed request on the request pool (`crate::pool`): the enabled rows in
//! their saved order, accounts of different providers mixed.
//!
//! A new chat takes the first row that is ready. A chat that already started
//! keeps its provider and account while they can still serve it, so the
//! provider's cache is reused, even after a row above recovers. When that row
//! hits a limit, is turned off or is removed, the chat moves down the pool.
//! Running requests are pinned to the account they started on.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::response::Response;
use http::StatusCode;
use once_cell::sync::Lazy;

use super::{
    Cooldown, Cooldowns, MAX_ROTATION_WAIT_MS, ServedBy, all_limited, execute, exhausted_response,
    now_ms, retry_deadline, spent_response, storage_error,
};
use crate::accounts::{Account, Accounts};
use crate::anthropic::{MessagesRequest, json_error};
use crate::config::{PoolEntry, Route};
use crate::provider::{AuthState, Provider, RequestContext};
use crate::registry::{Registry, anthropic_model_family};
use crate::request_identity::ConversationIdentity;

/// A provider and one of its saved accounts, or the provider alone.
type Place = (String, Option<String>);

/// How long a chat keeps its place after its last request.
const CHAT_MEMORY: Duration = Duration::from_secs(24 * 60 * 60);
/// Most chats remembered at once. The oldest goes first.
const CHAT_LIMIT: usize = 2_048;

static CHATS: Lazy<Mutex<HashMap<String, (Place, Instant)>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

fn place_of(chat: &str) -> Option<Place> {
    let chats = CHATS.lock().ok()?;
    let (place, at) = chats.get(chat)?;
    (at.elapsed() < CHAT_MEMORY).then(|| place.clone())
}

fn remember(chat: &str, place: Place) {
    let Ok(mut chats) = CHATS.lock() else {
        return;
    };
    if chats.len() >= CHAT_LIMIT && !chats.contains_key(chat) {
        chats.retain(|_, (_, at)| at.elapsed() < CHAT_MEMORY);
        let oldest = chats
            .iter()
            .min_by_key(|(_, (_, at))| *at)
            .map(|(chat, _)| chat.clone());
        if chats.len() >= CHAT_LIMIT
            && let Some(oldest) = oldest
        {
            chats.remove(&oldest);
        }
    }
    chats.insert(chat.to_string(), (place, Instant::now()));
}

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
    if anthropic_model_family(requested).is_none() {
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
    let tiered = anthropic_model_family(requested).is_some();
    let mut plans = Vec::new();
    let mut refusal = None;
    for row in rows {
        let (provider, model) = if tiered {
            match crate::tiers::resolve_for(registry, route, &row.provider, requested) {
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

/// The chat's own row when it can serve, else the first row that is ready.
fn pick(
    plans: &[Plan],
    chat: Option<&Place>,
    tried: &HashSet<usize>,
    snapshot: &[Cooldown],
    now: u64,
) -> Option<usize> {
    let ready = |at: usize| !tried.contains(&at) && plans[at].wait(snapshot, now).is_none();
    chat.and_then(|place| plans.iter().position(|plan| &plan.place() == place))
        .filter(|at| ready(*at))
        .or_else(|| (0..plans.len()).find(|at| ready(*at)))
}

fn first_release(plans: &[Plan], snapshot: &[Cooldown], now: u64) -> Option<u64> {
    plans
        .iter()
        .filter_map(|plan| plan.wait(snapshot, now))
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
    cooldowns: Cooldowns,
}

impl Pool<'_> {
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
        let (auto, cooldowns) = (self.auto, &self.cooldowns);
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
        let chat = context.session_id.clone();
        let remembered = chat.as_deref().and_then(place_of);
        let mut tried: HashSet<usize> = HashSet::new();
        let mut last_response = None;
        let mut waited = false;
        let mut shown = self.primary.name().to_string();
        loop {
            // Without auto-rotate there are no saved waits to honor: the first row answers.
            let snapshot = if auto {
                match cooldowns.read() {
                    Ok(snapshot) => snapshot,
                    Err(error) => return storage_error(error),
                }
            } else {
                Vec::new()
            };
            let now = now_ms();
            let Some(at) = pick(&plans, remembered.as_ref(), &tried, &snapshot, now) else {
                // Nothing ready: hold the request until the first row frees up
                // (once per request, bounded), so the turn continues on it
                // instead of dying on a 429.
                if auto
                    && !waited
                    && let Some(until) = first_release(&plans, &snapshot, now)
                    && until - now <= MAX_ROTATION_WAIT_MS
                {
                    waited = true;
                    log.info(
                        "account_wait",
                        Some(serde_json::Map::from_iter([
                            ("provider".into(), serde_json::json!(self.primary.name())),
                            ("waitMs".into(), serde_json::json!(until - now)),
                            ("reqId".into(), serde_json::json!(&context.req_id)),
                        ])),
                    );
                    tokio::time::sleep(Duration::from_millis(until - now)).await;
                    let fresh = match cooldowns.read() {
                        Ok(entries) => entries,
                        Err(error) => return storage_error(error),
                    };
                    let now = now_ms();
                    tried.retain(|at| plans[*at].wait(&fresh, now).is_some());
                    continue;
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
                    // A chat keeps its place unless that row is gone, turned off
                    // or waiting. A row that only has no model for this request
                    // (the chat's other tiers) stays its place.
                    let kept = remembered.as_ref().is_some_and(|place| {
                        enabled.contains(place) && !plans.iter().any(|p| &p.place() == place)
                    });
                    if !kept {
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
            if let Err(error) = cooldowns.sit_out(provider, plan.wait_id(), &plan.model, &sitout) {
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
                session_id: chat.map(str::to_string),
                session_seq: None,
                provider: first,
                account: None,
                traffic: None,
                monitor,
            };
            pool.drive(rows.to_vec(), body, context, None).await
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

    #[test]
    fn a_chat_is_remembered_and_its_place_can_change() {
        remember("memory-test-a", place("claude", Some("x")));
        assert_eq!(place_of("memory-test-a"), Some(place("claude", Some("x"))));
        assert_eq!(place_of("memory-test-unknown"), None);
        remember("memory-test-a", place("codex", None));
        assert_eq!(place_of("memory-test-a"), Some(place("codex", None)));
    }
}
