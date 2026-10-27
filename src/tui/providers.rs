//! Local saved accounts and usage. Network lookups run only on the panel's worker.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph, Wrap},
};

use super::controls::Action;
use super::{BG, DIM_WHITE, GREEN, SELECTED_BG, TEAL, WHITE, YELLOW, panel};
use crate::accounts::Accounts;
use crate::monitor::{
    EndpointKind, RequestStatus,
    accounting::{AccountUsage, UsageTotals},
    snapshot::MonitorSnapshot,
};
use crate::prompt::{InputLine, InputResult};
use crate::provider::AuthState;
use crate::registry::Registry;
use crate::usage::{MonitorUsage, UsageWindow};
use crate::{config, config::PoolEntry, pool, rotation::Cooldowns, ui};

const REFRESH_AFTER: Duration = Duration::from_secs(60);

/// The menu line that starts moving the row. Up and Down move one place,
/// Left moves to the top, Right moves to the bottom, Enter saves, Esc puts the
/// old order back.
const MOVE_ROW: &str = "Move...";

/// The menu line that opens the name field. Like removing, it is a step, not an
/// action that runs by itself: Enter on it opens the field.
const RENAME_ACCOUNT: &str = "Rename account...";

/// The menu line that asks before it deletes. Choosing it opens a second menu
/// with one line, so removing a login is two deliberate presses of Enter.
const REMOVE_ACCOUNT: &str = "Remove saved account...";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    Local,
    Remote,
    Demo,
}

#[derive(Debug, Clone, PartialEq)]
struct AccountRow {
    id: String,
    name: String,
    current: bool,
    state: String,
    usage: Option<MonitorUsage>,
    usage_error: Option<String>,
    waits: Vec<(Option<String>, u64)>,
    /// The account-wide wait is a spent balance: its time is a recheck, not a reset.
    spent: bool,
    /// What the account's plan can't do, in words (see `AccountSupport::note`).
    note: Option<String>,
    /// A better name than `main` or `account-2`, when the sign-in says an email.
    suggested_name: Option<String>,
}

impl AccountRow {
    fn waiting_until(&self, now: u64) -> Option<u64> {
        self.waits
            .iter()
            .filter(|(model, until)| model.is_none() && *until > now)
            .map(|(_, until)| *until)
            .max()
    }

    /// When cc-proxy tries this account again, if its balance is spent.
    fn out_of_credit_until(&self, now: u64) -> Option<u64> {
        self.waiting_until(now).filter(|_| self.spent)
    }

    fn usage_summary(&self, now: u64) -> Line<'static> {
        let mut summary = self.usage.as_ref().map_or_else(
            || "checking...".into(),
            |usage| usage.compact_line(jiff::Timestamp::now()),
        );
        let reset_is_shown = match &self.usage {
            Some(MonitorUsage::Reported(windows)) => windows.iter().any(|window| {
                window.remaining_percent == 0.0
                    && window.resets_at_ms.is_some_and(|reset| reset > now as i64)
            }),
            _ => false,
        };
        if let Some(until) = self.waiting_until(now)
            && (self.spent || !reset_is_shown)
        {
            let replaces = match &self.usage {
                None | Some(MonitorUsage::Unavailable(_)) => true,
                // "out of credit" says more than "not reported".
                Some(MonitorUsage::NotReported(_)) => self.spent,
                Some(MonitorUsage::Reported(_)) => false,
            };
            if replaces {
                summary = Line::default();
            } else {
                summary.spans.push(Span::raw(" · "));
            }
            let left = ui::duration(Duration::from_millis(until - now));
            if self.spent {
                summary.spans.extend([
                    Span::styled(
                        "out of credit",
                        Style::default().fg(ui::remaining_color(0.0)),
                    ),
                    Span::raw(format!(" · tries again in {left}")),
                ]);
            } else {
                summary.spans.extend([
                    Span::styled("waiting", Style::default().fg(YELLOW)),
                    Span::raw(format!(" · {left} left")),
                ]);
            }
        }
        summary
    }
}

#[derive(Debug, Clone, PartialEq)]
struct ProviderRow {
    id: String,
    state: AuthState,
    usage: Option<MonitorUsage>,
    usage_error: Option<String>,
    config_key_hash: Option<[u8; 32]>,
    accounts: Vec<AccountRow>,
    supports_accounts: bool,
    pinned: Option<String>,
    error: Option<String>,
    /// For a provider that holds one credential: when it waits. The same shape
    /// as `AccountRow::waits`.
    waits: Vec<(Option<String>, u64)>,
    spent: bool,
}

impl ProviderRow {
    /// A provider with one credential is one row of the pool, with no saved
    /// account to name. This gives it the shape of an account, so every row is
    /// drawn the same way. Its `id` is empty: it has none.
    fn as_account(&self) -> AccountRow {
        AccountRow {
            id: String::new(),
            name: ui::provider_name(&self.id).to_string(),
            current: false,
            state: self.state.describe(),
            usage: self.usage.clone(),
            usage_error: self.usage_error.clone(),
            waits: self.waits.clone(),
            spent: self.spent,
            note: None,
            suggested_name: None,
        }
    }

    fn retain_usage(&mut self, previous: &Self) {
        if self.pinned != previous.pinned {
            return;
        }
        // Provider-wide usage belongs to the current login, not the provider name.
        let same_current = self.accounts.iter().any(|account| {
            account.current
                && previous
                    .accounts
                    .iter()
                    .any(|old| old.current && old.id == account.id)
        });
        let same_config_key =
            self.config_key_hash.is_some() && self.config_key_hash == previous.config_key_hash;
        if self.state != AuthState::Missing && (same_current || same_config_key) {
            self.usage_error =
                retain_previous_usage(&mut self.usage, &previous.usage, &previous.usage_error);
        }
        for account in &mut self.accounts {
            if let Some(old) = previous.accounts.iter().find(|old| old.id == account.id) {
                // A failure that already kept the last saved numbers stays.
                let saved_failure = account.usage_error.take();
                account.usage_error =
                    retain_previous_usage(&mut account.usage, &old.usage, &old.usage_error)
                        .or(saved_failure);
            }
        }
    }
}

fn entry_of(row: &PoolEntry) -> Entry {
    match &row.account_id {
        Some(id) => Entry::Account(row.provider.clone(), id.clone()),
        None => Entry::Provider(row.provider.clone()),
    }
}

/// What one read of this computer gives the list: every provider, and the
/// order of the request pool.
struct Loaded {
    rows: Vec<ProviderRow>,
    pool: Vec<PoolEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Entry {
    Direct,
    Provider(String),
    Account(String, String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Serving {
    provider: String,
    account_id: Option<String>,
    account: Option<String>,
    model: Option<String>,
}

impl Serving {
    fn matches(&self, entry: &Entry) -> bool {
        match entry {
            Entry::Account(provider, id) => {
                &self.provider == provider && self.account_id.as_ref() == Some(id)
            }
            Entry::Provider(provider) => &self.provider == provider && self.account_id.is_none(),
            Entry::Direct => false,
        }
    }
}

/// A new name being typed for one saved account. It lives in the details area
/// like the action list, and the account row stays selected under it.
struct RenameDraft {
    provider: String,
    id: String,
    old_name: String,
    input: InputLine,
    /// The field started with the email the sign-in uses, not the old name.
    suggested: bool,
    /// Why the last save failed. The typed name stays, so it can be fixed.
    error: Option<String>,
    /// The name was sent and the answer has not come yet.
    saving: bool,
}

/// A row being moved in the list. The changes stay in memory until Enter
/// saves them; Escape puts the old order back.
struct MoveDraft {
    provider: String,
    account: Option<String>,
    before: Vec<PoolEntry>,
    saving: bool,
    error: Option<String>,
}

pub(super) struct ProvidersPanel {
    mode: Mode,
    rows: Vec<ProviderRow>,
    /// The request pool in its saved order. Rows with an `off` tag are in it too.
    pool: Vec<PoolEntry>,
    route: config::Route,
    auto_rotate: bool,
    /// Every request that reached generation, across all chats.
    active: Vec<Serving>,
    last_used: Option<Serving>,
    /// What each saved account used since the proxy started, by provider and
    /// immutable account id, so a rename never splits it.
    spend: HashMap<(String, String), UsageTotals>,
    loaded_at: Option<Instant>,
    pending: Option<mpsc::Receiver<Loaded>>,
    /// The pending read also fetches usage.
    pending_usage: bool,
    /// Set while a worker is still fetching usage, even when nothing waits for it.
    usage_busy: Arc<AtomicBool>,
    selected: Entry,
    list: ListState,
    menu: Option<Vec<(String, Action)>>,
    menu_state: ListState,
    notice: Option<(String, bool)>,
    detail_scroll: u16,
    rename: Option<RenameDraft>,
    moving: Option<MoveDraft>,
}

impl ProvidersPanel {
    pub fn new(mode: Mode) -> Self {
        let mut panel = Self {
            mode,
            rows: Vec::new(),
            pool: Vec::new(),
            route: config::Route::default(),
            auto_rotate: false,
            active: Vec::new(),
            last_used: None,
            spend: HashMap::new(),
            loaded_at: None,
            pending: None,
            pending_usage: false,
            usage_busy: Arc::new(AtomicBool::new(false)),
            selected: Entry::Direct,
            list: ListState::default(),
            menu: None,
            menu_state: ListState::default(),
            notice: None,
            detail_scroll: 0,
            rename: None,
            moving: None,
        };
        if mode == Mode::Demo {
            panel.rows = demo_rows();
            panel.pool = demo_pool();
            panel.route.on = true;
            panel.route.provider = Some("codex".into());
            panel.route.fallback = vec!["kimi".into()];
            panel.auto_rotate = true;
            panel.selected = Entry::Account("codex".into(), "demo-work".into());
        }
        panel
    }

    pub fn refresh(&mut self) {
        if self.mode != Mode::Local {
            return;
        }
        self.route = config::route();
        self.auto_rotate = config::auth_auto_rotate();
        if let Some(pending) = &self.pending {
            let mut batches = Vec::new();
            let finished = loop {
                match pending.try_recv() {
                    Ok(rows) => batches.push(rows),
                    Err(mpsc::TryRecvError::Empty) => break false,
                    Err(mpsc::TryRecvError::Disconnected) => break true,
                }
            };
            for loaded in batches {
                self.take_rows(loaded);
            }
            if !finished {
                return;
            }
            self.pending = None;
            if self.pending_usage {
                self.loaded_at = Some(Instant::now());
            }
            if !self.entries().contains(&self.selected) {
                self.selected = self.nearest_entry();
                self.menu = None;
            }
        }
        if self
            .loaded_at
            .is_none_or(|loaded| loaded.elapsed() >= REFRESH_AFTER)
            && !self.usage_busy.load(Ordering::SeqCst)
        {
            // The first read and the `r` key leave `loaded_at` empty.
            self.spawn_load(if self.loaded_at.is_some() {
                Usage::Paced
            } else {
                Usage::Ask
            });
        }
    }

    /// Starts a worker that reads the saved accounts and the order, and `usage`.
    /// A dropped receiver does not stop a worker, so `usage_busy` keeps a second
    /// usage read from starting beside it.
    fn spawn_load(&mut self, usage: Usage) {
        let with_usage = usage != Usage::No;
        let (send, receive) = mpsc::channel();
        let busy = with_usage.then(|| {
            self.usage_busy.store(true, Ordering::SeqCst);
            BusyGuard(self.usage_busy.clone())
        });
        std::thread::spawn(move || {
            load(&send, usage);
            drop(busy);
        });
        self.pending = Some(receive);
        self.pending_usage = with_usage;
    }

    /// Reads the saved accounts and the order again after a change, not their
    /// usage: a sign-in or a routing toggle has no new numbers to fetch.
    pub fn reload_saved(&mut self) {
        if self.mode == Mode::Local {
            self.spawn_load(Usage::No);
        }
    }

    fn take_rows(&mut self, loaded: Loaded) {
        let Loaded { mut rows, pool } = loaded;
        for row in &mut rows {
            if let Some(previous) = self.rows.iter().find(|old| old.id == row.id) {
                row.retain_usage(previous);
            }
        }
        let first_load = self.rows.is_empty();
        self.rows = rows;
        self.pool = pool;
        if first_load {
            self.selected = self
                .pool
                .iter()
                .find(|row| row.enabled)
                .map(entry_of)
                .unwrap_or_else(|| self.entries()[0].clone());
        }
    }

    /// Follow requests that reached generation. A provider being tried is not
    /// proof it served anything, and a token-count request is not a generation.
    pub fn observe(&mut self, snapshot: &MonitorSnapshot) {
        self.spend = snapshot
            .account_usage
            .iter()
            .filter_map(
                |AccountUsage {
                     provider,
                     account_id,
                     usage,
                 }| {
                    Some(((provider.clone(), account_id.clone()?), usage.clone()))
                },
            )
            .collect();
        self.active = snapshot
            .active
            .iter()
            .filter(|request| {
                request.endpoint != EndpointKind::CountTokens
                    && request.generation_started_at.is_some()
            })
            .filter_map(|request| {
                Some(Serving {
                    provider: request.provider.clone()?,
                    account_id: request.account_id.clone(),
                    account: request.account.clone(),
                    model: request.model.clone(),
                })
            })
            .collect();
        self.last_used = snapshot
            .recent
            .iter()
            .filter(|request| {
                request.endpoint != EndpointKind::CountTokens
                    && request.status == RequestStatus::Completed
                    && request.provider.is_some()
            })
            .max_by_key(|request| request.finished_at)
            .map(|request| Serving {
                provider: request.provider.clone().unwrap(),
                account_id: request.account_id.clone(),
                account: request.account.clone(),
                model: request.model.clone(),
            });
    }

    pub fn retry(&mut self) {
        self.loaded_at = None;
        self.refresh();
    }

    /// Where the cursor goes when its row is gone: another row of the same
    /// provider, else the top of the list.
    fn nearest_entry(&self) -> Entry {
        let entries = self.entries();
        let provider = match &self.selected {
            Entry::Provider(id) | Entry::Account(id, _) => Some(id),
            Entry::Direct => None,
        };
        provider
            .and_then(|id| {
                entries.iter().find(|entry| {
                    matches!(entry, Entry::Provider(other) | Entry::Account(other, _) if other == id)
                })
            })
            .unwrap_or(&entries[0])
            .clone()
    }

    /// Activity across all windows, not the most recent request's route.
    fn serving_line(&self) -> Line<'static> {
        if !self.active.is_empty() {
            let accounts = self
                .active
                .iter()
                .map(|request| (&request.provider, &request.account_id))
                .collect::<HashSet<_>>()
                .len();
            return Line::styled(
                format!(
                    " Serving: {} request{} on {accounts} account{}. ● active",
                    self.active.len(),
                    if self.active.len() == 1 { "" } else { "s" },
                    if accounts == 1 { "" } else { "s" },
                ),
                Style::default().fg(TEAL),
            );
        }
        let now = crate::rotation::now_ms();
        let next_reset = self
            .auto_rotate
            .then(|| self.first_reset(now))
            .flatten()
            .map(|(first, until)| {
                format!(
                    "Next reset: {} in {}",
                    self.label_of(&first),
                    ui::duration(Duration::from_millis(until.saturating_sub(now)))
                )
            });
        let base = match &self.last_used {
            Some(serving) => {
                let account = serving
                    .account
                    .as_ref()
                    .map(|name| format!(" / {name}"))
                    .unwrap_or_default();
                format!(
                    " ○ Last used: {}{account}",
                    ui::provider_name(&serving.provider)
                )
            }
            None => " No request served yet in this run.".into(),
        };
        let mut spans = vec![Span::styled(base, Style::default().fg(DIM_WHITE))];
        if let Some(next_reset) = next_reset {
            spans.push(Span::styled(" · ", Style::default().fg(DIM_WHITE)));
            spans.push(Span::styled(
                next_reset,
                Style::default().fg(YELLOW).add_modifier(Modifier::BOLD),
            ));
        }
        Line::from(spans)
    }

    fn active_count(&self, entry: &Entry) -> usize {
        self.active
            .iter()
            .filter(|request| request.matches(entry))
            .count()
    }

    fn was_last_used(&self, entry: &Entry) -> bool {
        self.active.is_empty()
            && self
                .last_used
                .as_ref()
                .is_some_and(|last| last.matches(entry))
    }

    fn activity_lines(&self, entry: &Entry) -> Vec<Line<'static>> {
        let mut models = BTreeMap::new();
        for request in self.active.iter().filter(|request| request.matches(entry)) {
            *models
                .entry(request.model.as_deref().unwrap_or("model not reported"))
                .or_insert(0) += 1;
        }
        models
            .into_iter()
            .map(|(model, count)| {
                Line::styled(
                    format!("● {model}: {count} active"),
                    Style::default().fg(TEAL),
                )
            })
            .collect()
    }

    /// What this account used since the proxy started, if it served anything.
    fn spent_since_start(&self, provider: &str, account: &AccountRow) -> Option<&UsageTotals> {
        self.spend
            .get(&(provider.to_string(), account.id.clone()))
            .filter(|totals| totals.observed_requests > 0)
    }

    /// The row's usage text. An account that reports no quota (an API key)
    /// shows what it used instead of "not reported". A wait still wins.
    fn row_summary(&self, provider: &str, account: &AccountRow, now: u64) -> Line<'static> {
        let summary = account.usage_summary(now);
        match (&account.usage, self.spent_since_start(provider, account)) {
            (Some(MonitorUsage::NotReported(_)), Some(totals))
                if summary.to_string() == "not reported" =>
            {
                Line::styled(totals.compact(), Style::default().fg(DIM_WHITE))
            }
            _ => summary,
        }
    }

    /// One flat list: the pool rows in their saved order, whatever their
    /// provider. Providers with no row in the pool (not set up) wait below.
    fn entries(&self) -> Vec<Entry> {
        let mut entries: Vec<Entry> = self
            .pool
            .iter()
            .map(entry_of)
            .filter(|entry| self.row_parts(entry).is_some())
            .collect();
        let mut waiting: Vec<&ProviderRow> = self
            .rows
            .iter()
            .filter(|row| !self.pool.iter().any(|entry| entry.provider == row.id))
            .collect();
        waiting
            .sort_by_cached_key(|row| (ui::provider_name(&row.id).to_lowercase(), row.id.clone()));
        entries.extend(
            waiting
                .into_iter()
                .map(|row| Entry::Provider(row.id.clone())),
        );
        entries.push(Entry::Direct);
        entries
    }

    /// The provider and the account a row stands for. A provider that holds one
    /// credential has no saved account, so it gets one made from its own state.
    fn row_parts(&self, entry: &Entry) -> Option<(&ProviderRow, Cow<'_, AccountRow>)> {
        match entry {
            Entry::Direct => None,
            Entry::Provider(id) => {
                let row = self.rows.iter().find(|row| &row.id == id)?;
                Some((row, Cow::Owned(row.as_account())))
            }
            Entry::Account(provider, id) => {
                let row = self.rows.iter().find(|row| &row.id == provider)?;
                let account = row.accounts.iter().find(|account| &account.id == id)?;
                Some((row, Cow::Borrowed(account)))
            }
        }
    }

    /// Where a row stands in the pool, and whether it is on.
    fn pool_row(&self, entry: &Entry) -> Option<(usize, &PoolEntry)> {
        let (provider, account) = match entry {
            Entry::Provider(id) => (id.as_str(), None),
            Entry::Account(provider, id) => (provider.as_str(), Some(id.as_str())),
            Entry::Direct => return None,
        };
        self.pool
            .iter()
            .enumerate()
            .find(|(_, row)| row.provider == provider && row.account_id.as_deref() == account)
    }

    /// `Codex / work`, or the provider alone when it holds one credential.
    fn label_of(&self, entry: &Entry) -> String {
        match (entry, self.row_parts(entry)) {
            (Entry::Account(provider, _), Some((_, account))) => {
                format!("{} / {}", ui::provider_name(provider), account.name)
            }
            (Entry::Provider(id), _) => ui::provider_name(id).to_string(),
            _ => String::new(),
        }
    }

    /// When every row that is on waits, the one that frees up first. A spent
    /// balance has no reset, so it never counts.
    fn first_reset(&self, now: u64) -> Option<(Entry, u64)> {
        let mut first: Option<(Entry, u64)> = None;
        for row in self.pool.iter().filter(|row| row.enabled) {
            let entry = entry_of(row);
            let (_, account) = self.row_parts(&entry)?;
            let until = account.waiting_until(now)?;
            if !account.spent && first.as_ref().is_none_or(|(_, best)| until < *best) {
                first = Some((entry, until));
            }
        }
        first
    }

    /// Every row that is on has run out of credit, and at least one is on.
    fn all_out_of_credit(&self, now: u64) -> bool {
        let mut on = self.pool.iter().filter(|row| row.enabled).peekable();
        on.peek().is_some()
            && on.all(|row| {
                self.row_parts(&entry_of(row))
                    .is_some_and(|(_, account)| account.out_of_credit_until(now).is_some())
            })
    }

    fn selected_provider(&self) -> Option<&ProviderRow> {
        let id = match &self.selected {
            Entry::Provider(id) | Entry::Account(id, _) => id,
            _ => return None,
        };
        self.rows.iter().find(|row| &row.id == id)
    }

    fn selected_account(&self) -> Option<&AccountRow> {
        let Entry::Account(_, id) = &self.selected else {
            return None;
        };
        self.selected_provider()?
            .accounts
            .iter()
            .find(|account| &account.id == id)
    }

    /// Replaces the actions with one line that does the deleting, and says what
    /// it deletes. Escape keeps the account.
    fn confirm_removal(&mut self, provider: String, id: String) {
        let Some(name) = self.selected_account().map(|account| account.name.clone()) else {
            return;
        };
        self.notice = Some((
            format!(
                "Delete {} account `{name}`? Its saved login is removed from this computer, and you will need to sign in again to add it back. Press Enter to delete it, or Esc to keep it.",
                ui::provider_name(&provider)
            ),
            true,
        ));
        self.menu = Some(vec![(
            format!("Delete `{name}` and its saved login"),
            Action::RemoveAccount { provider, id },
        )]);
        self.menu_state.select(Some(0));
    }

    pub fn close_menu(&mut self) -> bool {
        // A question that was never answered must not stay on screen, and a
        // cancelled move puts the old order back.
        let menu = self.menu.take().is_some();
        let rename = self.rename.take().is_some();
        let moving = if let Some(draft) = self.moving.take() {
            self.pool = draft.before;
            self.route_from_pool();
            true
        } else {
            false
        };
        let was_open = menu || rename || moving;
        if was_open {
            self.notice = None;
        }
        was_open
    }

    /// An action list, a name field or move mode is open over the account list.
    pub fn menu_open(&self) -> bool {
        self.menu.is_some() || self.rename.is_some() || self.moving.is_some()
    }

    /// A row is being moved in the list.
    pub fn moving(&self) -> bool {
        self.moving.is_some()
    }

    /// A name is being typed: every key, even `q`, goes to the field.
    pub fn renaming(&self) -> bool {
        self.rename.is_some()
    }

    /// Typing in the name field. Enter hands the new name back to be saved;
    /// Escape drops it and keeps the old name.
    pub fn press_rename(&mut self, key: KeyEvent) -> Option<Action> {
        let draft = self.rename.as_mut()?;
        match draft.input.press(key) {
            InputResult::Editing => None,
            InputResult::Cancelled => {
                self.close_menu();
                None
            }
            InputResult::Submitted => {
                let name = draft.input.value();
                let typed = name.trim().to_ascii_lowercase();
                if typed.is_empty() {
                    draft.error = Some(format!(
                        "Type a name, or press Esc to keep `{}`.",
                        draft.old_name
                    ));
                    return None;
                }
                if typed == draft.old_name {
                    self.close_menu();
                    self.notice =
                        Some(("Nothing changed. That is the current name.".into(), false));
                    return None;
                }
                draft.error = None;
                Some(Action::RenameAccount {
                    provider: draft.provider.clone(),
                    id: draft.id.clone(),
                    name,
                })
            }
        }
    }

    /// Opens the name field on the selected account, started with the email
    /// its sign-in uses when its name is only a number.
    fn open_rename(&mut self, provider: String, id: String) {
        let Some(account) = self.selected_account().filter(|account| account.id == id) else {
            return;
        };
        let started_with = account
            .suggested_name
            .clone()
            .unwrap_or_else(|| account.name.clone());
        self.rename = Some(RenameDraft {
            provider,
            id,
            old_name: account.name.clone(),
            input: InputLine::new(&started_with),
            suggested: account.suggested_name.is_some(),
            error: None,
            saving: false,
        });
        self.menu = None;
        self.moving = None;
        self.notice = None;
        self.detail_scroll = 0;
    }

    fn open_move(&mut self, provider: String, account: Option<String>) {
        self.moving = Some(MoveDraft {
            provider,
            account,
            before: self.pool.clone(),
            saving: false,
            error: None,
        });
        self.menu = None;
        self.rename = None;
        self.notice = None;
        self.detail_scroll = 0;
    }

    /// Keeps the name field open with what was typed, and says why it was refused.
    fn refuse_name(&mut self, reason: String) {
        if let Some(draft) = &mut self.rename {
            draft.error = Some(reason);
            draft.saving = false;
        }
    }

    /// Says at once that a change is being saved, while it runs behind the
    /// monitor. The action list or the name field stays open until the answer.
    pub fn show_saving(&mut self) {
        self.notice = Some(("Saving...".into(), false));
        if let Some(draft) = &mut self.rename {
            draft.saving = true;
            draft.error = None;
        }
        if let Some(draft) = &mut self.moving {
            draft.saving = true;
            draft.error = None;
        }
    }

    /// Shows a new name at once, without waiting for the next read. Returns
    /// why the name can't be used, so the field can say so.
    pub fn rename_account(&mut self, provider: &str, id: &str, name: &str) -> Result<(), String> {
        let name = crate::accounts::clean_name(name).map_err(|error| error.to_string())?;
        if let Some(account) = self
            .rows
            .iter_mut()
            .find(|row| row.id == provider)
            .and_then(|row| row.accounts.iter_mut().find(|account| account.id == id))
        {
            account.name = name;
            account.suggested_name = None;
        }
        Ok(())
    }

    pub fn press(&mut self, key: KeyCode) -> Option<Action> {
        if self.mode == Mode::Remote {
            return None;
        }
        if let Some(draft) = &self.moving {
            let provider = draft.provider.clone();
            let account = draft.account.clone();
            match key {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.move_row(&provider, account.as_deref(), -1);
                    self.route_from_pool();
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.move_row(&provider, account.as_deref(), 1);
                    self.route_from_pool();
                }
                KeyCode::Left | KeyCode::Home => {
                    self.move_row(&provider, account.as_deref(), isize::MIN);
                    self.route_from_pool();
                }
                KeyCode::Right | KeyCode::End => {
                    self.move_row(&provider, account.as_deref(), isize::MAX);
                    self.route_from_pool();
                }
                KeyCode::Enter => {
                    let unchanged = self
                        .moving
                        .as_ref()
                        .is_some_and(|draft| self.pool == draft.before);
                    if unchanged {
                        self.moving = None;
                        self.notice =
                            Some(("Nothing changed. The order is the same.".into(), false));
                        return None;
                    }
                    if let Some(draft) = &mut self.moving {
                        draft.error = None;
                    }
                    let at = self
                        .pool
                        .iter()
                        .position(|row| row.provider == provider && row.account_id == account)
                        .unwrap_or(0);
                    return Some(Action::SavePool {
                        provider,
                        account,
                        at,
                        pool: self.pool.clone(),
                    });
                }
                _ => {}
            }
            return None;
        }
        match key {
            KeyCode::Up | KeyCode::Char('k') => self.step(false),
            KeyCode::Down | KeyCode::Char('j') => self.step(true),
            KeyCode::PageDown | KeyCode::Char('d') | KeyCode::Char(']') => {
                self.detail_scroll = self.detail_scroll.saturating_add(3)
            }
            KeyCode::PageUp | KeyCode::Char('u') | KeyCode::Char('[') => {
                self.detail_scroll = self.detail_scroll.saturating_sub(3)
            }
            KeyCode::Home if self.menu.is_none() => self.selected = self.entries()[0].clone(),
            KeyCode::End if self.menu.is_none() => {
                self.selected = self.entries().last().unwrap().clone()
            }
            KeyCode::Char('r') => self.retry(),
            // Right goes one level in, Left one level out. Only Enter runs a choice.
            KeyCode::Right if self.menu.is_none() => self.open_actions(),
            KeyCode::Left if self.menu.is_some() => {
                self.close_menu();
            }
            KeyCode::Enter => {
                if let Some(menu) = &self.menu {
                    let chosen = menu.get(self.menu_state.selected().unwrap_or(0)).cloned();
                    return match chosen {
                        Some((
                            label,
                            Action::SavePool {
                                provider, account, ..
                            },
                        )) if label == MOVE_ROW => {
                            self.open_move(provider, account);
                            None
                        }
                        Some((label, Action::RemoveAccount { provider, id }))
                            if label == REMOVE_ACCOUNT =>
                        {
                            self.confirm_removal(provider, id);
                            None
                        }
                        Some((label, Action::RenameAccount { provider, id, .. }))
                            if label == RENAME_ACCOUNT =>
                        {
                            self.open_rename(provider, id);
                            None
                        }
                        chosen => chosen.map(|(_, action)| action),
                    };
                }
                self.open_actions();
            }
            _ => {}
        }
        None
    }

    fn open_actions(&mut self) {
        self.menu = Some(self.actions());
        self.menu_state.select(Some(0));
        self.detail_scroll = 0;
    }

    fn step(&mut self, down: bool) {
        self.detail_scroll = 0;
        if let Some(menu) = &self.menu {
            let at = self.menu_state.selected().unwrap_or(0);
            self.menu_state.select(Some(if down {
                (at + 1).min(menu.len().saturating_sub(1))
            } else {
                at.saturating_sub(1)
            }));
        } else {
            let entries = self.entries();
            let at = entries
                .iter()
                .position(|entry| entry == &self.selected)
                .unwrap_or(0);
            self.selected = entries[if down {
                (at + 1).min(entries.len() - 1)
            } else {
                at.saturating_sub(1)
            }]
            .clone();
            self.notice = None;
        }
    }

    fn actions(&self) -> Vec<(String, Action)> {
        if self.selected == Entry::Direct {
            return vec![if self.route.target().is_some() {
                ("Disable proxy routing".into(), Action::DisableRouting)
            } else {
                (
                    "Send Claude Code through the proxy".into(),
                    Action::EnableRouting,
                )
            }];
        }
        let Some(row) = self.selected_provider() else {
            return Vec::new();
        };
        let account = self.selected_account();
        let mut actions = Vec::new();
        // A row in the pool can move and be turned off. A provider that is not
        // set up is not in it.
        if let Some((_, place)) = self.pool_row(&self.selected) {
            let target = (row.id.clone(), account.map(|account| account.id.clone()));
            if self.pool.len() > 1 {
                actions.push((
                    MOVE_ROW.into(),
                    Action::SavePool {
                        provider: target.0.clone(),
                        account: target.1.clone(),
                        at: 0,
                        pool: Vec::new(),
                    },
                ));
            }
            actions.push((
                if place.enabled { "Disable" } else { "Enable" }.into(),
                Action::SetEnabled {
                    provider: target.0.clone(),
                    account: target.1.clone(),
                    enabled: !place.enabled,
                },
            ));
        }
        actions.push((
            if account.is_some() {
                "Sign in to this account again".into()
            } else if row.supports_accounts && !row.accounts.is_empty() {
                "Add another account".into()
            } else {
                "Sign in".into()
            },
            Action::SignIn {
                provider: row.id.clone(),
                account: account.map(|account| account.id.clone()),
                add: account.is_none() && row.supports_accounts && !row.accounts.is_empty(),
            },
        ));
        if let Some(account) = account {
            actions.push((
                RENAME_ACCOUNT.into(),
                Action::RenameAccount {
                    provider: row.id.clone(),
                    id: account.id.clone(),
                    name: String::new(),
                },
            ));
            actions.push((
                REMOVE_ACCOUNT.into(),
                Action::RemoveAccount {
                    provider: row.id.clone(),
                    id: account.id.clone(),
                },
            ));
        }
        actions
    }

    pub fn finish_action(&mut self, result: anyhow::Result<String>) {
        match result {
            Ok(message) => {
                self.notice = Some((message, false));
                self.menu = None;
                self.rename = None;
                self.moving = None;
            }
            // A name that was refused stays in its field, with the reason beside it.
            Err(error) if self.rename.is_some() => self.refuse_name(format!("{error:#}")),
            Err(error) if let Some(draft) = &mut self.moving => {
                draft.saving = false;
                draft.error = Some(format!(
                    "Couldn't save the order: {error:#}. Press Enter to retry, or Esc to put it back."
                ));
            }
            Err(error) => {
                self.notice = Some((
                    format!(
                        "Couldn't save the change: {error:#}. Press Enter to retry, or Esc to go back."
                    ),
                    true,
                ))
            }
        }
    }

    /// Like `finish_action`, but a failed start retries with `s`, not Enter.
    pub fn finish_start(&mut self, result: anyhow::Result<String>) {
        match result {
            Ok(_) => self.finish_action(result),
            Err(error) => {
                self.notice = Some((
                    format!("Couldn't start the proxy: {error:#}. Press s to try again."),
                    true,
                ))
            }
        }
    }

    /// Takes a removed account off the list at once, without waiting for the
    /// next read. The cursor goes to the row that takes its place.
    pub fn forget_account(&mut self, provider: &str, id: &str) {
        let gone = Entry::Account(provider.to_string(), id.to_string());
        let at = self
            .entries()
            .iter()
            .position(|entry| entry == &gone)
            .unwrap_or(0);
        if let Some(row) = self.rows.iter_mut().find(|row| row.id == provider) {
            let was_current = row
                .accounts
                .iter()
                .any(|account| account.id == id && account.current);
            row.accounts.retain(|account| account.id != id);
            if was_current && let Some(first) = row.accounts.first_mut() {
                first.current = true;
            }
        }
        self.pool
            .retain(|row| !(row.provider == provider && row.account_id.as_deref() == Some(id)));
        let entries = self.entries();
        self.selected = entries[at.min(entries.len() - 1)].clone();
    }

    /// Shows a change to the pool at once, without waiting for the next read.
    /// The next read confirms it.
    pub fn apply_pool_action(&mut self, action: &Action) {
        match action {
            Action::SavePool { pool, .. } => self.pool = pool.clone(),
            Action::SetEnabled {
                provider,
                account,
                enabled,
            } => self.set_row_enabled(provider, account.as_deref(), *enabled),
            _ => return,
        }
        self.route_from_pool();
    }

    fn move_row(&mut self, provider: &str, account: Option<&str>, offset: isize) {
        let Some(at) = self
            .pool
            .iter()
            .position(|row| row.provider == provider && row.account_id.as_deref() == account)
        else {
            return;
        };
        let to = at.saturating_add_signed(offset).min(self.pool.len() - 1);
        let row = self.pool.remove(at);
        self.pool.insert(to, row);
    }

    fn set_row_enabled(&mut self, provider: &str, account: Option<&str>, enabled: bool) {
        if let Some(row) = self
            .pool
            .iter_mut()
            .find(|row| row.provider == provider && row.account_id.as_deref() == account)
        {
            row.enabled = enabled;
        }
    }

    /// The first provider that is on and the others after it, as the pool says.
    fn route_from_pool(&mut self) {
        let mut names: Vec<String> = Vec::new();
        for row in self.pool.iter().filter(|row| row.enabled) {
            if !names.contains(&row.provider) {
                names.push(row.provider.clone());
            }
        }
        self.route.provider = (!names.is_empty()).then(|| names.remove(0));
        self.route.fallback = names;
    }

    pub fn preview_action(&mut self, action: &Action) {
        match action {
            Action::StartProxy => {}
            Action::DisableRouting => self.route.on = false,
            Action::EnableRouting => {
                self.route.on = true;
                self.route_from_pool();
            }
            Action::SavePool { .. } | Action::SetEnabled { .. } => {
                self.apply_pool_action(action);
                self.moving = None;
            }
            Action::RemoveAccount { provider, id } => self.forget_account(provider, id),
            Action::RenameAccount { provider, id, name } => {
                if let Err(reason) = self.rename_account(provider, id, name) {
                    self.refuse_name(reason);
                    return;
                }
                self.rename = None;
            }
            Action::SignIn { .. } => {
                self.notice = Some((
                    "Demo only. Run cc-proxy open to sign in to a real account.".into(),
                    false,
                ));
                self.menu = None;
                return;
            }
        }
        self.menu = None;
        self.notice = Some(("Demo only. Nothing on this computer changed.".into(), false));
    }

    fn entry_item(&self, entry: &Entry, width: u16) -> ListItem<'static> {
        let selected = entry == &self.selected;
        let Some((row, account)) = self.row_parts(entry) else {
            let text = if self.route.target().is_none() {
                "Proxy routing is off [in use]"
            } else {
                "Disable proxy routing"
            };
            return ListItem::new(Line::from(Span::styled(
                text,
                Style::default().fg(if selected { TEAL } else { WHITE }),
            )));
        };
        let now = crate::rotation::now_ms();
        let place = self.pool_row(entry).map(|(_, place)| place);
        let active = self.active_count(entry);
        let last_used = self.was_last_used(entry);
        let first_reset = self.auto_rotate
            && self
                .first_reset(now)
                .is_some_and(|(first, _)| &first == entry);
        let summary = match entry {
            // A provider with one credential shows nothing until it has numbers or a wait.
            Entry::Provider(_) if account.usage.is_none() && account.waits.is_empty() => {
                Line::default()
            }
            _ => self.row_summary(&row.id, &account, now),
        };
        let label = self.label_of(entry);
        let off = match place {
            Some(place) if !place.enabled => Some("off"),
            Some(_) => None,
            None if row.state == AuthState::Missing => Some("not set up"),
            // Signed in with a key in config.json but no saved login: not a row yet.
            None => Some("not in the list"),
        };
        let mut info = vec![Span::raw("  ")];
        if active > 0 {
            info.push(Span::styled(
                format!("{active} active · "),
                Style::default().fg(TEAL),
            ));
        }
        info.extend(summary.spans);
        info.extend(row_tags(&account, first_reset, off));
        ListItem::new(vec![
            Line::from(vec![
                row_marker(active > 0, last_used),
                Span::styled(
                    super::ellipsize(&label, usize::from(width).saturating_sub(2)),
                    row_name_style(active > 0, selected, first_reset, off.is_none()),
                ),
            ]),
            Line::from(info),
        ])
    }

    /// Where the selected row stands in the list, and what that means.
    fn placement_line(&self) -> Option<Line<'static>> {
        let (at, place) = self.pool_row(&self.selected)?;
        let state = if place.enabled {
            "On."
        } else {
            "Off. New chats skip it."
        };
        Some(Line::styled(
            format!(
                "Number {} of {} in the list. {state}",
                at + 1,
                self.pool.len()
            ),
            Style::default().fg(DIM_WHITE),
        ))
    }

    fn detail_lines(&self) -> Vec<Line<'static>> {
        let mut text = Vec::new();
        match &self.selected {
            Entry::Direct => {
                if self.route.target().is_some() {
                    text.push(
                        "Restore Claude Code's original settings and login, without the proxy."
                            .into(),
                    );
                    text.push(
                        "The proxy keeps running. You can send Claude Code through it again from here."
                            .into(),
                    );
                } else {
                    text.push("Claude Code uses its own settings and login now.".into());
                    text.push(
                        "Press Enter to send it through the proxy again. The list keeps its order."
                            .into(),
                    );
                }
            }
            _ => {
                if let Some(row) = self.selected_provider() {
                    if self.selected_account().is_none() {
                        text.push(ui::provider_name(&row.id).to_string().into());
                    }
                    if let Some(pinned) = &row.pinned {
                        text.push(format!("${pinned} supplies the login. Saved account choices do not apply until you unset it in the proxy service.").into());
                    }
                    if let Some(error) = &row.error {
                        text.push(error.clone().into());
                    }
                    if let Some(account) = self.selected_account() {
                        text.push(Line::from(vec![
                            row_marker(
                                self.active_count(&self.selected) > 0,
                                self.was_last_used(&self.selected),
                            ),
                            Span::raw(account.name.clone()),
                        ]));
                        text.extend(account.usage.as_ref().map_or_else(
                            || vec!["Checking usage...".into()],
                            |usage| {
                                usage.lines(jiff::Timestamp::now(), &jiff::tz::TimeZone::system())
                            },
                        ));
                        let now = crate::rotation::now_ms();
                        for (model, until) in &account.waits {
                            if *until > now {
                                if model.is_none() && account.spent {
                                    text.push(Line::styled(
                                        format!(
                                            "Out of credit. cc-proxy tries this account again in {}. After you add credit, cc-proxy retries it at that time.",
                                            ui::duration(Duration::from_millis(until - now))
                                        ),
                                        Style::default().fg(ui::remaining_color(0.0)),
                                    ));
                                    continue;
                                }
                                let scope = model
                                    .as_ref()
                                    .map_or(String::new(), |model| format!(" for {model}"));
                                text.push(Line::styled(
                                    format!(
                                        "At its limit{scope}. Retry in {}.",
                                        ui::duration(Duration::from_millis(until - now))
                                    ),
                                    Style::default().fg(YELLOW),
                                ));
                            }
                        }
                        text.push(account.state.clone().into());
                        text.extend(self.placement_line());
                        if let Some(totals) = self.spent_since_start(&row.id, account) {
                            text.push("".into());
                            text.push(Line::styled(
                                format!("Since the proxy started: {}", totals.tokens_label()),
                                Style::default().fg(DIM_WHITE),
                            ));
                            if let Some(cache) = totals.cache_label() {
                                text.push(Line::styled(
                                    format!("Cached input: {cache}"),
                                    Style::default().fg(DIM_WHITE),
                                ));
                            }
                            text.push(Line::styled(
                                format!("Cost: {}", totals.cost_detail()),
                                Style::default().fg(DIM_WHITE),
                            ));
                        }
                        if let Some(note) = &account.note {
                            text.push(Line::styled(note.clone(), Style::default().fg(YELLOW)));
                        }
                        text.push("".into());
                        text.push(
                            "Move changes the order. Enable and Disable control which rows new chats can use. A chat keeps its account until it hits a limit."
                                .into(),
                        );
                    } else {
                        text.push(row.state.describe().into());
                        text.extend(self.placement_line());
                        if let Some(usage) = &row.usage {
                            text.extend(
                                usage.lines(jiff::Timestamp::now(), &jiff::tz::TimeZone::system()),
                            );
                        }
                        text.push("".into());
                        if row.state == AuthState::Missing {
                            text.push("Press Enter, then Sign in to save a login.".into());
                        } else {
                            text.push("Press Enter to move this row or turn it on or off.".into());
                        }
                    }
                    let usage_error = self
                        .selected_account()
                        .map_or(&row.usage_error, |account| &account.usage_error);
                    if let Some(error) = usage_error {
                        let retry = if error.contains("Press r") {
                            ""
                        } else {
                            " Press r to retry."
                        };
                        text.push(Line::styled(
                            format!(
                                "* Refresh failed. Last values may be out of date. {error}{retry}"
                            ),
                            Style::default().fg(YELLOW),
                        ));
                    }
                    if self.all_out_of_credit(crate::rotation::now_ms()) {
                        text.push(Line::styled(
                            "Every row that is on is out of credit. Add credit to one, or enable another account.",
                            Style::default().fg(ui::remaining_color(0.0)),
                        ));
                    }
                    if self.auto_rotate
                        && let Some((first, until)) = self.first_reset(crate::rotation::now_ms())
                        && first == self.selected
                        && let Ok(until) = i64::try_from(until)
                        && let Ok(reset) = jiff::Timestamp::from_millisecond(until)
                    {
                        let reset = crate::usage::format_reset(
                            reset,
                            jiff::Timestamp::now(),
                            &jiff::tz::TimeZone::system(),
                        );
                        text.push(Line::styled(
                            format!(
                                "Every row that is on is waiting. {} {reset}.",
                                self.label_of(&first)
                            ),
                            Style::default().fg(YELLOW),
                        ));
                        text.push("The next request rechecks each account's limits.".into());
                    }
                    text.extend(self.activity_lines(&self.selected));
                }
            }
        }
        if let Some((message, problem)) = &self.notice {
            text.insert(
                0,
                Line::styled(
                    message.clone(),
                    Style::default().fg(if *problem { YELLOW } else { GREEN }),
                ),
            );
            text.insert(1, Line::raw(""));
        }
        text
    }

    pub fn render(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        if self.mode == Mode::Remote {
            frame.render_widget(Paragraph::new("Accounts are managed on the computer running the proxy. This --url connection is read-only.\n\nRun cc-proxy open on that computer to choose providers, accounts or rotation. Press 2 for activity.")
                .style(Style::default().fg(DIM_WHITE)).wrap(Wrap { trim: false }).block(panel("Remote proxy", false)), area);
            return;
        }
        let [summary, body] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).areas(area);
        let primary = self
            .route
            .target()
            .map(ui::provider_name)
            .unwrap_or("Claude Code login (no proxy)");
        let note = if self.route.target().is_none() {
            " Routing is off. Claude Code uses its own login."
        } else if !self.auto_rotate {
            " Auto-rotate is off, so the first row that is on answers every request."
        } else {
            " New chats share the first provider's ready accounts. A chat stays on its account until it hits a limit."
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled(" Claude Code: ", Style::default().fg(DIM_WHITE)),
                    Span::styled(
                        primary.to_string(),
                        Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        if self.mode == Mode::Demo {
                            "  [demo]"
                        } else {
                            ""
                        },
                        Style::default().fg(YELLOW),
                    ),
                ]),
                Line::styled(note, Style::default().fg(DIM_WHITE)),
                self.serving_line(),
            ]),
            summary,
        );
        let [list_area, detail_area] = if area.width >= 100 {
            Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(body)
        } else {
            Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(body)
        };
        let entries = self.entries();
        let items: Vec<_> = entries
            .iter()
            .map(|entry| self.entry_item(entry, list_area.width.saturating_sub(4)))
            .collect();
        self.list
            .select(entries.iter().position(|entry| entry == &self.selected));
        frame.render_stateful_widget(
            List::new(items)
                .block(panel(
                    if self.moving.is_some() {
                        "Request order · moving"
                    } else if self.pending.is_some() || self.usage_busy.load(Ordering::SeqCst) {
                        "Request order · refreshing"
                    } else {
                        "Request order"
                    },
                    self.menu.is_none() && self.rename.is_none(),
                ))
                .highlight_symbol("❯ ")
                .style(Style::default().fg(DIM_WHITE))
                .highlight_style(
                    Style::default()
                        .bg(SELECTED_BG)
                        .add_modifier(Modifier::BOLD),
                ),
            list_area,
            &mut self.list,
        );
        let detail = panel(
            if self.rename.is_some() {
                "Rename account"
            } else if self.moving.is_some() {
                "Move row"
            } else if self.menu.is_some() {
                "Choose an action"
            } else {
                "Details"
            },
            self.menu_open(),
        );
        let inner = detail.inner(detail_area);
        frame.render_widget(detail, detail_area);
        if let Some(draft) = &self.rename {
            render_rename(frame, inner, draft);
        } else if let Some(draft) = &self.moving {
            self.render_move(frame, inner, draft);
        } else if let Some(menu) = &self.menu {
            let notice_height = if let Some((message, _)) = &self.notice {
                Paragraph::new(message.as_str())
                    .wrap(Wrap { trim: false })
                    .line_count(inner.width)
                    .min(usize::from(inner.height / 2)) as u16
            } else {
                0
            };
            let [choices, notice] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(notice_height)])
                    .areas(inner);
            frame.render_stateful_widget(
                List::new(menu.iter().map(|(label, _)| ListItem::new(label.clone())))
                    .highlight_symbol("❯ ")
                    .highlight_style(
                        Style::default()
                            .fg(TEAL)
                            .bg(SELECTED_BG)
                            .add_modifier(Modifier::BOLD),
                    ),
                choices,
                &mut self.menu_state,
            );
            if let Some((message, _)) = &self.notice {
                frame.render_widget(
                    Paragraph::new(message.as_str())
                        .style(Style::default().fg(YELLOW))
                        .wrap(Wrap { trim: false }),
                    notice,
                );
            }
        } else {
            let mut lines = self.detail_lines();
            if self.rows.is_empty() {
                lines.insert(
                    0,
                    Line::styled(
                        "Checking local saved accounts...",
                        Style::default().fg(TEAL),
                    ),
                );
            }
            let paragraph = Paragraph::new(lines)
                .style(Style::default().fg(DIM_WHITE).bg(BG))
                .wrap(Wrap { trim: false });
            self.detail_scroll = self.detail_scroll.min(
                paragraph
                    .line_count(inner.width)
                    .saturating_sub(usize::from(inner.height))
                    .min(usize::from(u16::MAX)) as u16,
            );
            frame.render_widget(paragraph.scroll((self.detail_scroll, 0)), inner);
        }
    }

    fn render_move(&self, frame: &mut ratatui::Frame<'_>, area: Rect, draft: &MoveDraft) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let label = self.label_of(&self.selected);
        let at = self.pool_row(&self.selected).map_or(0, |(at, _)| at);
        let mut lines = vec![
            Line::styled(
                format!(
                    "Move {}",
                    super::ellipsize(&label, usize::from(area.width).saturating_sub(6))
                ),
                Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
            ),
            Line::styled(
                format!("Number {} of {} in the list.", at + 1, self.pool.len()),
                Style::default().fg(TEAL),
            ),
            Line::raw(""),
        ];
        if draft.saving {
            lines.push(Line::styled("Saving...", Style::default().fg(TEAL)));
            lines.push(Line::raw(""));
        }
        if let Some(error) = &draft.error {
            lines.push(Line::styled(error.clone(), Style::default().fg(YELLOW)));
            lines.push(Line::raw(""));
        }
        lines.push("↑ and ↓ move one place.".into());
        lines.push("← moves to the top. → moves to the bottom.".into());
        lines.push("Press Enter to save, or Esc to put it back.".into());
        frame.render_widget(
            Paragraph::new(lines)
                .style(Style::default().fg(DIM_WHITE).bg(BG))
                .wrap(Wrap { trim: false }),
            area,
        );
    }
}

/// The name field in the details area: the old name, the field with its
/// cursor, what went wrong, and what stays the same.
fn render_rename(frame: &mut ratatui::Frame<'_>, area: Rect, draft: &RenameDraft) {
    const LABEL: &str = "New name: ";
    if area.width == 0 || area.height == 0 {
        return;
    }
    let room = usize::from(area.width).saturating_sub(LABEL.len() + 1);
    let (shown, at) = draft.input.window(room);
    let mut lines = vec![
        Line::styled(
            format!(
                "Rename `{}`",
                super::ellipsize(&draft.old_name, usize::from(area.width).saturating_sub(10))
            ),
            Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
        ),
        Line::from(vec![
            Span::styled(LABEL, Style::default().fg(DIM_WHITE)),
            Span::styled(
                shown,
                Style::default()
                    .fg(WHITE)
                    .add_modifier(Modifier::UNDERLINED),
            ),
        ]),
        Line::raw(""),
    ];
    if draft.saving {
        lines.push(Line::styled("Saving...", Style::default().fg(TEAL)));
        lines.push(Line::raw(""));
    }
    if let Some(error) = &draft.error {
        lines.push(Line::styled(error.clone(), Style::default().fg(YELLOW)));
        lines.push(Line::styled(
            format!(
                "Fix the name and press Enter, or press Esc to keep `{}`.",
                draft.old_name
            ),
            Style::default().fg(YELLOW),
        ));
        lines.push(Line::raw(""));
    }
    if draft.suggested {
        lines.push(
            "Started with the email this sign-in uses. Edit it, or press Ctrl-U to clear it."
                .into(),
        );
    }
    lines.push("A name can use letters, numbers, - and _. An email works too.".into());
    lines.push("The saved sign-in stays the same.".into());
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(DIM_WHITE).bg(BG))
            .wrap(Wrap { trim: false }),
        area,
    );
    let column = u16::try_from(LABEL.len() + at).unwrap_or(u16::MAX);
    frame.set_cursor_position((
        area.x.saturating_add(column).min(area.right() - 1),
        area.y.saturating_add(1).min(area.bottom() - 1),
    ));
}

/// Every account generating a reply gets a teal bullet. When all are idle,
/// a hollow circle marks the last account used.
fn row_marker(active: bool, last_used: bool) -> Span<'static> {
    if active {
        Span::styled("● ", Style::default().fg(TEAL).add_modifier(Modifier::BOLD))
    } else if last_used {
        Span::styled("○ ", Style::default().fg(DIM_WHITE))
    } else {
        Span::raw("  ")
    }
}

/// A row that is off is dim. The one answering requests is bold teal, the one
/// under the cursor is teal, and the next account to reset when all wait is yellow.
fn row_name_style(serving: bool, selected: bool, first_reset: bool, on: bool) -> Style {
    let color = if selected || serving {
        TEAL
    } else if first_reset {
        YELLOW
    } else if on {
        WHITE
    } else {
        DIM_WHITE
    };
    let style = Style::default().fg(color);
    if serving || first_reset {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

/// What follows a row's numbers: `off` or `not set up` in words, `[first reset]`
/// when every row waits and this one frees up first, and a yellow `*` when the
/// usage refresh failed (Details says why).
fn row_tags(account: &AccountRow, first_reset: bool, off: Option<&str>) -> Vec<Span<'static>> {
    let mut tags = Vec::new();
    if let Some(off) = off {
        tags.push(Span::styled(
            format!(" {off}"),
            Style::default().fg(DIM_WHITE),
        ));
    }
    if first_reset {
        tags.push(Span::styled(
            " [first reset]",
            Style::default().fg(YELLOW).add_modifier(Modifier::BOLD),
        ));
    }
    if account.usage_error.is_some() {
        tags.push(Span::styled(
            " *",
            Style::default().fg(YELLOW).add_modifier(Modifier::BOLD),
        ));
    }
    tags
}

fn retain_previous_usage(
    next: &mut Option<MonitorUsage>,
    previous: &Option<MonitorUsage>,
    previous_error: &Option<String>,
) -> Option<String> {
    match (next.as_ref(), previous) {
        (None, _) => {
            *next = previous.clone();
            previous_error.clone()
        }
        (Some(MonitorUsage::Unavailable(error)), Some(MonitorUsage::Reported(windows)))
            if !windows.is_empty() =>
        {
            let error = error.clone();
            *next = previous.clone();
            Some(error)
        }
        _ => None,
    }
}

fn account_rows(
    id: &str,
    support: &dyn crate::accounts::AccountSupport,
) -> anyhow::Result<Vec<AccountRow>> {
    let accounts = Accounts::open(id, support)?;
    let current = accounts.current()?.map(|account| account.id);
    accounts.list()?.into_iter().map(|account| {
        let mut state = match accounts.login(&account.name) {
            Ok(Some(login)) => support.state(&login).describe(),
            Ok(None) => format!("No saved login. Run cc-proxy login {id} --replace --account {}", account.name),
            Err(error) => format!("Can't read the login: {error:#}. Run cc-proxy login {id} --replace --account {}", account.name),
        };
        let waits = match Cooldowns::new().waits(id, &account.id) {
            Ok(waits) => waits,
            Err(error) => { state.push_str(&format!(". Can't read the wait timer: {error:#}")); Vec::new() }
        };
        let spent = waits.iter().any(|wait| wait.model.is_none() && wait.spent);
        let waits = waits.into_iter().map(|wait| (wait.model, wait.until_ms)).collect();
        let note = accounts.login(&account.name).ok().flatten().and_then(|login| support.note(&login));
        let suggested_name = accounts.suggest_name(support, &account);
        Ok(AccountRow { current: current.as_ref() == Some(&account.id), waits, spent, id: account.id, name: account.name, state, usage: None, usage_error: None, note, suggested_name })
    }).collect()
}

/// What a read fetches besides the saved accounts and the order.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Usage {
    /// No numbers: a change was saved, and nothing new is left to fetch.
    No,
    /// Every account, now. The first read and the `r` key.
    Ask,
    /// The read every minute: an account that said 429 is left alone until its
    /// wait is over.
    Paced,
}

/// Clears `usage_busy` when the usage worker ends, even if it panics.
struct BusyGuard(Arc<AtomicBool>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn load(send: &mpsc::Sender<Loaded>, usage: Usage) {
    let registry = Registry::new();
    let mut rows: Vec<ProviderRow> = crate::route::routable_providers(&registry)
        .into_iter()
        .filter_map(|id| {
            let provider = registry.provider(id)?;
            let cli = provider.cli();
            let mut row = ProviderRow {
                state: cli.auth_state(),
                id: id.into(),
                usage: None,
                usage_error: None,
                config_key_hash: None,
                accounts: Vec::new(),
                supports_accounts: cli.accounts().is_some(),
                pinned: cli
                    .accounts()
                    .and_then(|support| support.pinned_credentials())
                    .map(str::to_string),
                error: None,
                waits: Vec::new(),
                spent: false,
            };
            if let Some(support) = cli.accounts() {
                match account_rows(id, support) {
                    Ok(accounts) => row.accounts = accounts,
                    Err(error) => {
                        row.error = Some(format!(
                            "Can't read accounts: {error:#}. Run cc-proxy auth status {id}"
                        ))
                    }
                }
            }
            // A provider with one credential waits under a fixed name, not an account id.
            if row.accounts.is_empty()
                && row.state != AuthState::Missing
                && let Ok(waits) = Cooldowns::new().waits(id, "default")
            {
                row.spent = waits.iter().any(|wait| wait.model.is_none() && wait.spent);
                row.waits = waits
                    .into_iter()
                    .map(|wait| (wait.model, wait.until_ms))
                    .collect();
            }
            if id == "opencode" && !row.accounts.iter().any(|account| account.current) {
                use sha2::{Digest, Sha256};
                row.config_key_hash =
                    config::opencode_api_key().map(|key| Sha256::digest(key.as_bytes()).into());
            }
            Some(row)
        })
        .collect();
    // Read once: the pool does not change while the usage numbers are fetched.
    let pool = pool::entries(&config::route(), &registry).unwrap_or_default();
    let first = Loaded {
        rows: rows.clone(),
        pool: pool.clone(),
    };
    if send.send(first).is_err() || usage == Usage::No {
        return;
    }
    for (id, headline) in crate::usage::headlines() {
        if let Some(row) = rows.iter_mut().find(|row| row.id == id) {
            row.usage = Some(headline);
        }
    }
    let accounts = match usage {
        Usage::Paced => crate::usage::account_usage_paced(),
        _ => crate::usage::account_usage(),
    };
    apply_account_usage(&mut rows, &accounts);
    keep_last_good(&mut rows, &crate::usage::last_good_usage());
    let _ = send.send(Loaded { rows, pool });
}

/// An account whose lookup failed keeps the numbers from its last good one,
/// even after the monitor was closed and opened again. Details says why it
/// failed.
fn keep_last_good(rows: &mut [ProviderRow], saved: &HashMap<(String, String), Vec<UsageWindow>>) {
    let now = crate::rotation::now_ms();
    for row in rows {
        for account in &mut row.accounts {
            if let Some(MonitorUsage::Unavailable(reason)) = &account.usage
                && let Some(windows) = saved.get(&(row.id.clone(), account.id.clone()))
                && !windows.is_empty()
            {
                account.usage_error = Some(reason.clone());
                if row.id != "antigravity" {
                    sync_exhausted_wait(account, windows, now);
                }
                account.usage = Some(MonitorUsage::Reported(windows.clone()));
            }
        }
    }
}

fn sync_exhausted_wait(account: &mut AccountRow, windows: &[UsageWindow], now: u64) {
    let exhausted_until = windows
        .iter()
        .filter(|window| window.remaining_percent <= 0.0)
        .filter_map(|window| window.resets_at_ms.and_then(|ms| u64::try_from(ms).ok()))
        .filter(|until| *until > now)
        .max();
    // A spent balance stays: the usage windows do not know about it.
    let spent = account.spent;
    account.waits.retain(|(model, _)| model.is_some() || spent);
    if let Some(until) = exhausted_until {
        account.waits.push((None, until));
    }
}

fn apply_account_usage(rows: &mut [ProviderRow], usage: &HashMap<(String, String), MonitorUsage>) {
    let now = crate::rotation::now_ms();
    for row in rows {
        for account in &mut row.accounts {
            let lines = usage.get(&(row.id.clone(), account.id.clone())).cloned().unwrap_or_else(|| {
                match row.id.as_str() {
                    "opencode" if account.current => row.usage.clone().unwrap_or_else(|| MonitorUsage::Unavailable("Usage unavailable. Press r to retry, or run cc-proxy usage opencode.".into())),
                    "opencode" => MonitorUsage::NotReported("OpenCode Go reports usage for the current account only."),
                    "claude" | "codex" | "kimi" | "antigravity" => MonitorUsage::Unavailable(format!("Usage unavailable. Press r to retry, or run cc-proxy usage {}.", row.id)),
                    _ => MonitorUsage::NotReported("This provider does not report a balance or usage. With auto-rotate on, cc-proxy moves to the next account when the provider says this one is out of credit."),
                }
            });
            if let MonitorUsage::Reported(windows) = &lines
                && row.id != "antigravity"
            {
                sync_exhausted_wait(account, windows, now);
            }
            account.usage = Some(lines);
        }
    }
}

/// The demo's pool: Codex `work` first, then `home`, then Kimi. Order is the
/// order new chats try them.
fn demo_pool() -> Vec<PoolEntry> {
    let on = |provider: &str, id: &str| PoolEntry {
        provider: provider.into(),
        account_id: Some(id.into()),
        enabled: true,
    };
    vec![
        on("codex", "demo-work"),
        on("codex", "demo-home"),
        on("kimi", "demo-kimi"),
    ]
}

fn demo_rows() -> Vec<ProviderRow> {
    vec![
        ProviderRow {
            id: "codex".into(),
            state: AuthState::SignedIn {
                account: None,
                expires_ms: None,
            },
            usage: None,
            usage_error: None,
            config_key_hash: None,
            error: None,
            supports_accounts: true,
            pinned: None,
            waits: Vec::new(),
            spent: false,
            accounts: vec![
                AccountRow {
                    id: "demo-home".into(),
                    name: "home".into(),
                    current: false,
                    state: "signed in".into(),
                    usage: Some(MonitorUsage::Reported(vec![UsageWindow {
                        label: "5h",
                        remaining_percent: 0.0,
                        resets_at_ms: Some(jiff::Timestamp::now().as_millisecond() + 3_600_000),
                    }])),
                    usage_error: None,
                    waits: vec![(None, crate::rotation::now_ms() + 3_600_000)],
                    spent: false,
                    note: None,
                    suggested_name: None,
                },
                AccountRow {
                    id: "demo-work".into(),
                    name: "work".into(),
                    current: true,
                    state: "signed in".into(),
                    usage: Some(MonitorUsage::Reported(vec![
                        UsageWindow {
                            label: "5h",
                            remaining_percent: 58.0,
                            resets_at_ms: Some(jiff::Timestamp::now().as_millisecond() + 7_200_000),
                        },
                        UsageWindow {
                            label: "week",
                            remaining_percent: 88.0,
                            resets_at_ms: Some(
                                jiff::Timestamp::now().as_millisecond() + 345_600_000,
                            ),
                        },
                    ])),
                    usage_error: None,
                    waits: Vec::new(),
                    spent: false,
                    note: None,
                    suggested_name: None,
                },
            ],
        },
        ProviderRow {
            id: "kimi".into(),
            state: AuthState::SignedIn {
                account: None,
                expires_ms: None,
            },
            usage: None,
            usage_error: None,
            config_key_hash: None,
            error: None,
            supports_accounts: true,
            pinned: None,
            waits: Vec::new(),
            spent: false,
            accounts: vec![AccountRow {
                id: "demo-kimi".into(),
                name: "personal".into(),
                current: true,
                state: "signed in".into(),
                usage: Some(MonitorUsage::Reported(vec![UsageWindow {
                    label: "5h",
                    remaining_percent: 72.0,
                    resets_at_ms: Some(jiff::Timestamp::now().as_millisecond() + 10_800_000),
                }])),
                usage_error: None,
                waits: Vec::new(),
                spent: false,
                note: None,
                suggested_name: None,
            }],
        },
        ProviderRow {
            id: "opencode-zen".into(),
            state: AuthState::Missing,
            usage: None,
            usage_error: None,
            config_key_hash: None,
            accounts: Vec::new(),
            supports_accounts: true,
            pinned: None,
            error: None,
            waits: Vec::new(),
            spent: false,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    /// A batch whose pool lists every provider's accounts in the order given,
    /// all on, and a provider with one credential as one row.
    impl From<Vec<ProviderRow>> for Loaded {
        fn from(rows: Vec<ProviderRow>) -> Self {
            let mut pool = Vec::new();
            for row in &rows {
                let on = |account_id| PoolEntry {
                    provider: row.id.clone(),
                    account_id,
                    enabled: true,
                };
                if row.accounts.is_empty() {
                    if row.state != AuthState::Missing {
                        pool.push(on(None));
                    }
                } else {
                    let mut accounts: Vec<_> = row.accounts.iter().collect();
                    accounts.sort_by_key(|account| !account.current);
                    pool.extend(accounts.into_iter().map(|a| on(Some(a.id.clone()))));
                }
            }
            Self { rows, pool }
        }
    }

    fn render(panel: &mut ProvidersPanel, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| panel.render(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn pending_refresh(panel: &mut ProvidersPanel) -> mpsc::Sender<Loaded> {
        let (send, receive) = mpsc::channel();
        panel.mode = Mode::Local;
        panel.pending = Some(receive);
        panel.pending_usage = true;
        send
    }

    fn metadata(mut rows: Vec<ProviderRow>) -> Vec<ProviderRow> {
        for row in &mut rows {
            row.usage = None;
            row.usage_error = None;
            for account in &mut row.accounts {
                account.usage = None;
                account.usage_error = None;
            }
        }
        rows
    }

    #[test]
    fn refresh_keeps_quota_and_reset_times_in_rows_and_details() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        panel.selected = Entry::Account("codex".into(), "demo-work".into());
        let previous = panel.rows[0].accounts[1].usage.clone();
        let send = pending_refresh(&mut panel);
        panel.refresh();
        send.send(Loaded::from(metadata(demo_rows()))).unwrap();
        panel.refresh();
        assert_eq!(panel.rows[0].accounts[1].usage, previous);
        for (width, height) in [(160, 30), (80, 24), (40, 24)] {
            let text = render(&mut panel, width, height);
            for expected in ["refreshing", "58%", "88%", "5h 58% left", "resets in 2h"] {
                assert!(text.contains(expected), "missing {expected}: {text}");
            }
            assert!(!text.to_ascii_lowercase().contains("checking"), "{text}");
        }
    }

    #[test]
    fn successful_refresh_replaces_cached_values_and_clears_stale_feedback() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        panel.selected = Entry::Account("codex".into(), "demo-work".into());
        panel.rows[0].accounts[1].usage_error = Some("Press r to retry.".into());
        let send = pending_refresh(&mut panel);
        let mut rows = demo_rows();
        let next = Some(MonitorUsage::Reported(vec![UsageWindow {
            label: "5h",
            remaining_percent: 17.0,
            resets_at_ms: Some(jiff::Timestamp::now().as_millisecond() + 300_000),
        }]));
        rows[0].accounts[1].usage = next.clone();
        send.send(Loaded::from(rows)).unwrap();
        drop(send);
        panel.refresh();
        assert_eq!(panel.rows[0].accounts[1].usage, next);
        assert!(panel.rows[0].accounts[1].usage_error.is_none());
        assert!(panel.pending.is_none());
        let text = render(&mut panel, 160, 30);
        for expected in ["5h 17%", "5h 17% left", "resets in 5m"] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        for old in ["58%", "88%", "[refresh failed]", "refreshing"] {
            assert!(!text.contains(old), "{text}");
        }
    }

    #[test]
    fn failed_refresh_keeps_last_values_and_marks_them_stale_with_a_retry() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        panel.selected = Entry::Account("codex".into(), "demo-work".into());
        let previous = panel.rows[0].accounts[1].usage.clone();
        let send = pending_refresh(&mut panel);
        let mut rows = metadata(demo_rows());
        apply_account_usage(&mut rows, &HashMap::new());
        send.send(Loaded::from(rows)).unwrap();
        drop(send);
        panel.refresh();
        assert_eq!(panel.rows[0].accounts[1].usage, previous);
        assert!(panel.rows[0].accounts[1].usage_error.is_some());
        let text = render(&mut panel, 160, 30);
        for expected in [
            "58%",
            "88%",
            "resets in 2h",
            "* Refresh failed. Last values may be out of date.",
            "Press r to retry",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        assert!(!text.contains("[refresh failed]"), "{text}");
        assert!(!text.to_ascii_lowercase().contains("checking"), "{text}");
        assert!(!text.contains("refreshing"), "{text}");

        let send = pending_refresh(&mut panel);
        panel.press(KeyCode::Char('r'));
        send.send(Loaded::from(metadata(demo_rows()))).unwrap();
        panel.refresh();
        assert_eq!(panel.rows[0].accounts[1].usage, previous);
        let text = render(&mut panel, 160, 30);
        for expected in ["refreshing", "* Refresh failed", "58%", "Press r to retry"] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
    }

    #[test]
    fn a_failed_lookup_keeps_the_last_saved_numbers_and_warns_at_the_end() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        let mut rows = metadata(demo_rows());
        // Nothing answered, as in a monitor opened during an outage.
        apply_account_usage(&mut rows, &HashMap::new());
        let saved = HashMap::from([(
            ("codex".to_string(), "demo-work".to_string()),
            vec![UsageWindow {
                label: "5h",
                remaining_percent: 41.0,
                resets_at_ms: None,
            }],
        )]);
        keep_last_good(&mut rows, &saved);
        // An account with nothing saved keeps the plain message and no warning.
        assert!(rows[0].accounts[0].usage_error.is_none());
        assert!(matches!(
            rows[0].accounts[0].usage,
            Some(MonitorUsage::Unavailable(_))
        ));
        // The rows replace a monitor that already showed other numbers.
        panel.take_rows(Loaded::from(rows));
        panel.selected = Entry::Account("codex".into(), "demo-work".into());
        let text = render(&mut panel, 160, 30);
        for expected in [
            "5h 41%",
            "* Refresh failed. Last values may be out of date.",
            "Press r to retry",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
    }

    #[test]
    fn first_load_checks_until_a_result_and_never_turns_failure_into_zero() {
        let mut panel = ProvidersPanel::new(Mode::Local);
        let send = pending_refresh(&mut panel);
        send.send(Loaded::from(metadata(demo_rows()))).unwrap();
        panel.refresh();
        panel.selected = Entry::Account("codex".into(), "demo-work".into());
        let text = render(&mut panel, 160, 30);
        assert!(text.contains("checking..."), "{text}");
        assert!(text.contains("Checking usage..."), "{text}");
        assert!(!text.contains('%'), "{text}");

        let mut rows = metadata(demo_rows());
        apply_account_usage(&mut rows, &HashMap::new());
        send.send(Loaded::from(rows)).unwrap();
        drop(send);
        panel.refresh();
        let text = render(&mut panel, 160, 30);
        for expected in ["unavailable", "Press r to retry"] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        for absent in ["checking...", "Checking usage...", "Refresh failed", "%"] {
            assert!(!text.contains(absent), "{text}");
        }
        assert!(panel.rows[0].accounts[1].usage_error.is_none());

        let send = pending_refresh(&mut panel);
        send.send(Loaded::from(metadata(demo_rows()))).unwrap();
        panel.refresh();
        assert!(matches!(
            panel.rows[0].accounts[1].usage,
            Some(MonitorUsage::Unavailable(_))
        ));
        send.send(Loaded::from(demo_rows())).unwrap();
        drop(send);
        panel.refresh();
        let text = render(&mut panel, 160, 30);
        assert!(text.contains("5h 58% left"), "{text}");
        assert!(!text.contains("unavailable"), "{text}");
    }

    #[test]
    fn refresh_caches_follow_provider_and_account_ids_not_names_or_order() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        panel.selected = Entry::Account("codex".into(), "demo-work".into());
        let previous = panel.rows[0].accounts[1].usage.clone();
        let other_account = panel.rows[0].accounts[0].usage.clone();
        let send = pending_refresh(&mut panel);
        let mut rows = metadata(demo_rows());
        rows[0].accounts.swap(0, 1);
        rows[0].accounts[0].name = "renamed".into();
        send.send(Loaded::from(rows.clone())).unwrap();
        panel.refresh();
        assert_eq!(panel.rows[0].accounts[0].usage, previous);
        assert_eq!(panel.rows[0].accounts[0].name, "renamed");

        // A new login gets a new UUID even when its saved name is unchanged.
        rows[0].accounts[0].id = "replacement".into();
        rows[0].accounts[1].name = "work".into();
        let mut other_provider = rows[0].clone();
        other_provider.id = "another-provider".into();
        rows.push(other_provider);
        send.send(Loaded::from(rows)).unwrap();
        drop(send);
        panel.refresh();
        assert!(panel.rows[0].accounts[0].usage.is_none());
        assert_eq!(panel.rows[0].accounts[1].usage, other_account);
        assert!(
            panel.rows[3]
                .accounts
                .iter()
                .all(|account| account.usage.is_none())
        );
        // The old login is gone, so the cursor is never left on a missing row.
        // (Whether it follows the new login depends on the saved route, which
        // `refresh` reads from the machine, so this test does not assert it.)
        assert!(panel.entries().contains(&panel.selected));
    }

    #[test]
    fn provider_headline_cache_stays_with_its_current_account() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        panel.rows[0].id = "opencode".into();
        panel.rows[0].usage = panel.rows[0].accounts[1].usage.clone();
        panel.selected = Entry::Provider("opencode".into());
        let previous = panel.rows[0].usage.clone();
        let send = pending_refresh(&mut panel);
        let mut rows = metadata(panel.rows.clone());
        send.send(Loaded::from(rows.clone())).unwrap();
        panel.refresh();
        assert_eq!(panel.rows[0].usage, previous);
        rows[0].usage = Some(MonitorUsage::Unavailable("Press r to retry.".into()));
        send.send(Loaded::from(rows.clone())).unwrap();
        panel.refresh();
        assert_eq!(panel.rows[0].usage, previous);
        let text = render(&mut panel, 160, 30);
        for expected in ["5h 58% left", "Refresh failed", "Press r to retry"] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }

        rows[0].usage = None;
        rows[0].accounts[0].current = true;
        rows[0].accounts[1].current = false;
        send.send(Loaded::from(rows.clone())).unwrap();
        panel.refresh();
        assert!(panel.rows[0].usage.is_none());
        assert!(panel.rows[0].usage_error.is_none());

        rows[0].accounts[0].id = "replacement".into();
        rows[0].accounts.remove(1);
        send.send(Loaded::from(rows)).unwrap();
        drop(send);
        panel.refresh();
        assert!(panel.rows[0].usage.is_none());
        assert!(panel.rows[0].accounts[0].usage.is_none());

        let mut old = demo_rows().remove(0);
        old.usage = previous;
        let mut changed = metadata(vec![old.clone()]).remove(0);
        changed.pinned = Some("OTHER_KEY".into());
        changed.retain_usage(&old);
        assert!(changed.usage.is_none());
        assert!(
            changed
                .accounts
                .iter()
                .all(|account| account.usage.is_none())
        );

        // Without an account UUID, matching provider names do not prove identity.
        old.accounts.clear();
        let mut unidentified = metadata(vec![old.clone()]).remove(0);
        unidentified.retain_usage(&old);
        assert!(unidentified.usage.is_none());

        // Config-only usage can be reused only while the key digest matches.
        old.config_key_hash = Some([1; 32]);
        let mut configured = metadata(vec![old.clone()]).remove(0);
        configured.retain_usage(&old);
        assert_eq!(configured.usage, old.usage);
        configured.usage = Some(MonitorUsage::Unavailable("Press r to retry.".into()));
        configured.retain_usage(&old);
        assert_eq!(configured.usage, old.usage);
        assert!(configured.usage_error.is_some());
        let mut replaced = metadata(vec![old.clone()]).remove(0);
        replaced.config_key_hash = Some([2; 32]);
        replaced.retain_usage(&old);
        assert!(replaced.usage.is_none());
        assert!(replaced.usage_error.is_none());
    }

    #[test]
    fn selected_account_shows_usage_and_waits_without_color_alone() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        panel.selected = Entry::Account("codex".into(), "demo-home".into());
        let text = render(&mut panel, 110, 25);
        for expected in [
            "home",
            "58%",
            "5h 0% left",
            "At its limit",
            "Retry in",
            "Number 2 of 3 in the list. On.",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
    }

    #[test]
    fn enter_opens_actions_and_escape_returns_without_a_change() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        panel.selected = Entry::Account("codex".into(), "demo-home".into());
        assert_eq!(panel.press(KeyCode::Enter), None);
        assert!(render(&mut panel, 110, 25).contains(MOVE_ROW));
        assert!(panel.close_menu());
        panel.press(KeyCode::Enter);
        // Enter on Move... opens move mode instead of saving right away.
        assert_eq!(panel.press(KeyCode::Enter), None);
        assert!(panel.moving());
        assert!(render(&mut panel, 110, 25).contains("Request order · moving"));
        // Escape puts the old order back without saving.
        assert_eq!(panel.press(KeyCode::Up), None);
        assert_eq!(panel.pool[0].account_id.as_deref(), Some("demo-home"));
        assert!(panel.close_menu());
        assert!(!panel.moving());
        assert_eq!(panel.pool[0].account_id.as_deref(), Some("demo-work"));
        // Moving and pressing Enter saves the new order.
        panel.press(KeyCode::Enter);
        panel.press(KeyCode::Enter);
        assert_eq!(panel.press(KeyCode::Up), None);
        let action = panel.press(KeyCode::Enter).unwrap();
        assert!(matches!(
            &action,
            Action::SavePool {
                provider,
                account: Some(id),
                at: 0,
                ..
            } if provider == "codex" && id == "demo-home"
        ));
        panel.preview_action(&action);
        assert!(!panel.moving());
        // Reordering does not change the saved login.
        assert!(!panel.rows[0].accounts[0].current);
        // The row is first in the list now, and routing still goes to Codex.
        assert_eq!(panel.pool[0].account_id.as_deref(), Some("demo-home"));
        assert_eq!(panel.route.target(), Some("codex"));
        assert!(render(&mut panel, 110, 25).contains("Nothing on this computer changed"));
    }

    #[test]
    fn removal_is_scoped_to_a_saved_login_and_the_cursor_takes_the_next_row() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        panel.selected = Entry::Provider("opencode-zen".into());
        assert!(
            !panel
                .actions()
                .iter()
                .any(|(_, action)| matches!(action, Action::RemoveAccount { .. }))
        );
        panel.selected = Entry::Account("codex".into(), "demo-work".into());
        panel.press(KeyCode::Enter);
        let (at, action) = panel
            .menu
            .as_ref()
            .unwrap()
            .iter()
            .enumerate()
            .find_map(|(at, (_, action))| {
                matches!(action, Action::RemoveAccount { .. }).then_some((at, action.clone()))
            })
            .unwrap();
        // The question stays inside the monitor: no screen change for a delete.
        assert!(!action.needs_terminal());
        panel.menu_state.select(Some(at));
        // The first Enter asks. Nothing is returned to run, and nothing is gone yet.
        assert_eq!(panel.press(KeyCode::Enter), None);
        assert_eq!(panel.rows[0].accounts.len(), 2);
        // Wide enough that the question does not wrap inside the panel.
        let asking = render(&mut panel, 400, 30);
        for expected in [
            "Delete Codex account `work`",
            "sign in again to add it back",
            "Esc to keep it",
            "Delete `work` and its saved login",
        ] {
            assert!(asking.contains(expected), "{expected}: {asking}");
        }
        // Escape keeps the account and leaves no stale question behind.
        assert!(panel.close_menu());
        assert!(!render(&mut panel, 400, 30).contains("Delete Codex account"));
        panel.press(KeyCode::Enter);
        panel.menu_state.select(Some(at));
        panel.press(KeyCode::Enter);
        // The second Enter is the answer.
        assert_eq!(panel.press(KeyCode::Enter), Some(action.clone()));
        panel.preview_action(&action);
        assert_eq!(panel.rows[0].accounts.len(), 1);
        assert!(panel.rows[0].accounts[0].current);
        assert!(
            !panel
                .pool
                .iter()
                .any(|row| row.account_id.as_deref() == Some("demo-work"))
        );
        // The cursor stays at the same place in the list: on the row that moved up.
        assert_eq!(
            panel.selected,
            Entry::Account("codex".into(), "demo-home".into())
        );
        assert!(render(&mut panel, 110, 25).contains("Nothing on this computer changed"));
    }

    /// A snapshot where a request reached generation on `provider` / `account`.
    fn served(provider: &str, account_id: &str, endpoint: EndpointKind) -> MonitorSnapshot {
        let monitor = crate::monitor::MonitorHandle::default();
        monitor.request_started("r", Some("s".into()), Some(1), endpoint);
        monitor.provider_selected("r", provider, "model", None);
        monitor.account_selected("r", account_id, "name");
        monitor.generation_started("r");
        monitor.snapshot().into()
    }

    #[test]
    fn the_account_answering_is_highlighted_without_moving_the_cursor() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        let selected = Entry::Account("codex".into(), "demo-work".into());
        panel.selected = selected.clone();
        panel.observe(&served("kimi", "demo-kimi", EndpointKind::Messages));
        assert_eq!(panel.selected, selected);
        let text = render(&mut panel, 160, 30);
        assert!(text.contains("Serving: 1 request on 1 account"), "{text}");
        assert!(text.contains("● Kimi / personal"), "{text}");
        assert!(!text.contains("● Codex / work"), "{text}");
        panel.selected = Entry::Account("kimi".into(), "demo-kimi".into());
        assert!(render(&mut panel, 160, 30).contains("● model: 1 active"));
    }

    #[test]
    fn a_cursor_the_person_moved_stays_and_token_counts_do_not_count_as_serving() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        panel.selected = Entry::Account("codex".into(), "demo-home".into());
        panel.observe(&served("kimi", "demo-kimi", EndpointKind::Messages));
        assert_eq!(
            panel.selected,
            Entry::Account("codex".into(), "demo-home".into())
        );
        // A local token count is not a request any provider answered.
        let mut panel = ProvidersPanel::new(Mode::Demo);
        panel.observe(&served("kimi", "demo-kimi", EndpointKind::CountTokens));
        assert!(panel.active.is_empty());
        assert!(panel.last_used.is_none());
        assert!(render(&mut panel, 160, 30).contains("No request served yet in this run."));
    }

    #[test]
    fn concurrent_accounts_stay_marked_when_another_request_finishes() {
        let monitor = crate::monitor::MonitorHandle::default();
        for (id, provider, account, model) in [
            ("a", "codex", "demo-work", "gpt-test"),
            ("b", "kimi", "demo-kimi", "kimi-test"),
            ("c", "codex", "demo-work", "other-test"),
        ] {
            monitor.request_started(id, Some(id.into()), Some(1), EndpointKind::Messages);
            monitor.provider_selected(id, provider, model, None);
            monitor.account_selected(id, account, "name");
            monitor.generation_started(id);
        }
        monitor.stream_progress("a", 10, 1, None, Some(2));
        monitor.request_started("count", None, None, EndpointKind::CountTokens);
        monitor.provider_selected("count", "codex", "gpt-test", None);
        monitor.account_selected("count", "demo-home", "home");
        monitor.generation_started("count");
        monitor.request_completed("c", 200, None, None);
        let mut panel = ProvidersPanel::new(Mode::Demo);
        panel.observe(&monitor.snapshot().into());
        let text = render(&mut panel, 160, 30);
        for expected in [
            "Serving: 2 requests on 2 accounts",
            "● Codex / work",
            "● Kimi / personal",
        ] {
            assert!(text.contains(expected), "{expected}: {text}");
        }
        assert!(!text.contains("● Codex / home"), "{text}");
        assert_eq!(
            panel.active_count(&Entry::Account("codex".into(), "demo-work".into())),
            1
        );
        monitor.request_completed("a", 200, None, None);
        monitor.request_completed("b", 200, None, None);
        panel.observe(&monitor.snapshot().into());
        let text = render(&mut panel, 160, 30);
        assert!(text.contains("○ Kimi / personal"), "{text}");
        assert!(!text.contains("● Codex / work"), "{text}");
        assert!(text.contains("Last used: Kimi / name"), "{text}");
    }

    #[test]
    fn an_unknown_account_never_highlights_another_account_of_the_same_provider() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        panel.observe(&served("codex", "removed-account", EndpointKind::Messages));
        let text = render(&mut panel, 160, 30);
        assert!(!text.contains("● Codex /"), "{text}");
    }

    #[test]
    fn each_account_gets_two_lines_with_room_for_its_name() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        panel.rows[0].accounts[1].name = "work@example.com".into();
        for (width, height) in [(160, 30), (80, 24), (40, 24)] {
            let text = render(&mut panel, width, height);
            let lines: Vec<_> = text.lines().collect();
            let at = lines
                .iter()
                .position(|line| line.contains("Codex / work@example.com"))
                .unwrap();
            assert!(!lines[at].contains("58%"), "{text}");
            assert!(lines[at + 1].contains("5h 58%"), "{text}");
            assert!(
                !text.contains("● Codex /"),
                "no account is active yet: {text}"
            );
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    fn type_name(panel: &mut ProvidersPanel, text: &str) {
        for ch in text.chars() {
            assert_eq!(panel.press_rename(key(KeyCode::Char(ch))), None);
        }
    }

    /// Opens the actions on the selected account and picks Rename.
    fn start_rename(panel: &mut ProvidersPanel) {
        panel.press(KeyCode::Enter);
        let at = panel
            .menu
            .as_ref()
            .unwrap()
            .iter()
            .position(|(label, _)| label == RENAME_ACCOUNT)
            .expect("an account can be renamed");
        panel.menu_state.select(Some(at));
        assert_eq!(panel.press(KeyCode::Enter), None);
        assert!(panel.renaming());
    }

    #[test]
    fn rename_starts_with_the_email_and_keeps_what_was_typed_until_it_is_saved() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        panel.rows[0].accounts[0].name = "account-2".into();
        panel.rows[0].accounts[0].suggested_name = Some("ana@example.com".into());
        panel.selected = Entry::Account("codex".into(), "demo-home".into());

        start_rename(&mut panel);
        let field = render(&mut panel, 120, 25);
        for expected in [
            "Rename account",
            "Rename `account-2`",
            "New name: ana@example.com",
            "Started with the email this sign-in uses",
            "The saved sign-in stays the same",
        ] {
            assert!(field.contains(expected), "{expected}: {field}");
        }

        // Escape keeps the old name, on the same account.
        assert!(panel.close_menu());
        assert!(!panel.renaming());
        assert_eq!(panel.rows[0].accounts[0].name, "account-2");
        assert_eq!(
            panel.selected,
            Entry::Account("codex".into(), "demo-home".into())
        );

        // The person clears the email and types their own name.
        start_rename(&mut panel);
        let ctrl_u = KeyEvent::new(KeyCode::Char('u'), crossterm::event::KeyModifiers::CONTROL);
        assert_eq!(panel.press_rename(ctrl_u), None);
        type_name(&mut panel, "Spare");
        let save = panel.press_rename(key(KeyCode::Enter));
        assert_eq!(
            save,
            Some(Action::RenameAccount {
                provider: "codex".into(),
                id: "demo-home".into(),
                name: "Spare".into(),
            })
        );
        assert!(!save.unwrap().needs_terminal());

        // The save was refused: the field stays open with the text and the reason.
        panel.finish_action(Err(anyhow::anyhow!(
            "Codex already has an account called `spare`. Pick another name."
        )));
        assert!(panel.renaming());
        let refused = render(&mut panel, 120, 25);
        for expected in [
            "New name: Spare",
            "already has an account called `spare`",
            "Fix the name and press Enter, or press Esc to keep `account-2`",
        ] {
            assert!(refused.contains(expected), "{expected}: {refused}");
        }

        // Fixing the name and saving again works, and the row follows at once.
        type_name(&mut panel, "2");
        let again = panel.press_rename(key(KeyCode::Enter)).unwrap();
        panel.preview_action(&again);
        assert!(!panel.renaming());
        assert_eq!(panel.rows[0].accounts[0].name, "spare2");
        assert_eq!(panel.rows[0].accounts[0].suggested_name, None);
        assert_eq!(
            panel.selected,
            Entry::Account("codex".into(), "demo-home".into())
        );
    }

    #[test]
    fn a_change_that_is_saving_says_so_and_keeps_its_field_until_the_answer() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        panel.selected = Entry::Account("codex".into(), "demo-home".into());
        start_rename(&mut panel);
        panel.show_saving();
        assert!(panel.renaming());
        assert!(render(&mut panel, 120, 25).contains("Saving..."));
        panel.finish_action(Err(anyhow::anyhow!(
            "Codex already has an account called `spare`. Pick another name."
        )));
        let refused = render(&mut panel, 120, 25);
        assert!(refused.contains("already has an account"), "{refused}");
        assert!(!refused.contains("Saving..."), "{refused}");

        // An action in the list shows it too, and the list stays open.
        panel.close_menu();
        panel.press(KeyCode::Enter);
        panel.show_saving();
        assert!(panel.menu_open());
        assert!(render(&mut panel, 120, 25).contains("Saving..."));
    }

    #[test]
    fn a_usage_read_that_is_still_running_is_never_started_a_second_time() {
        let mut panel = ProvidersPanel::new(Mode::Local);
        panel.usage_busy.store(true, Ordering::SeqCst);
        panel.refresh();
        panel.retry();
        assert!(panel.pending.is_none());
        // The title keeps saying so, even though nothing waits for the answer.
        assert!(render(&mut panel, 120, 25).contains("refreshing"));
    }

    #[test]
    fn rename_says_so_when_the_name_is_empty_or_unchanged_and_never_for_a_provider() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        panel.selected = Entry::Provider("opencode-zen".into());
        assert!(
            !panel
                .actions()
                .iter()
                .any(|(_, action)| matches!(action, Action::RenameAccount { .. }))
        );

        panel.selected = Entry::Account("codex".into(), "demo-work".into());
        start_rename(&mut panel);
        // Nothing to suggest: the field starts with the current name.
        assert!(render(&mut panel, 120, 25).contains("New name: work"));
        assert!(!render(&mut panel, 120, 25).contains("Started with the email"));
        assert_eq!(panel.press_rename(key(KeyCode::Enter)), None);
        assert!(!panel.renaming());
        assert!(render(&mut panel, 120, 25).contains("Nothing changed. That is the current name."));

        start_rename(&mut panel);
        let ctrl_u = KeyEvent::new(KeyCode::Char('u'), crossterm::event::KeyModifiers::CONTROL);
        panel.press_rename(ctrl_u);
        assert_eq!(panel.press_rename(key(KeyCode::Enter)), None);
        assert!(panel.renaming());
        assert!(render(&mut panel, 120, 25).contains("Type a name, or press Esc to keep `work`."));
        assert_eq!(panel.rows[0].accounts[1].name, "work");

        // A name the real rule refuses never reaches the disk in the demo either.
        type_name(&mut panel, "a b");
        let bad = panel.press_rename(key(KeyCode::Enter)).unwrap();
        panel.preview_action(&bad);
        assert!(panel.renaming());
        assert!(render(&mut panel, 120, 25).contains("can't be an account name"));
    }

    #[test]
    fn an_account_shows_what_it_used_since_the_proxy_started() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        panel.rows[1].accounts[0].usage = Some(MonitorUsage::NotReported("No balance."));
        panel.selected = Entry::Account("kimi".into(), "demo-kimi".into());
        // Nothing served yet: the row says so and Details stay quiet.
        assert!(render(&mut panel, 160, 30).contains("not reported"));
        assert!(!render(&mut panel, 160, 30).contains("Since the proxy started"));

        let monitor = crate::monitor::MonitorHandle::default();
        monitor.request_started("r", Some("s".into()), Some(1), EndpointKind::Messages);
        monitor.provider_selected("r", "kimi", "kimi-k2.6", None);
        monitor.account_selected("r", "demo-kimi", "personal");
        monitor.usage_updated("r", Some(1_200), Some(300));
        monitor.request_completed("r", 200, Some(1_200), Some(300));
        panel.observe(&monitor.snapshot().into());

        let text = render(&mut panel, 160, 30);
        // An API-key row shows the use instead of "not reported".
        assert!(text.contains("1.2k in / 300 out"), "{text}");
        assert!(!text.contains("not reported"), "{text}");
        // Details say what the number is, and say when no price is known.
        assert!(
            text.contains("Since the proxy started: 1.2k in / 300 out"),
            "{text}"
        );
        assert!(text.contains("no price for this model"), "{text}");
        // The totals follow the immutable id: a rename keeps them.
        panel.rows[1].accounts[0].name = "renamed".into();
        assert!(render(&mut panel, 160, 30).contains("1.2k in / 300 out"));
    }

    #[test]
    fn the_row_serving_requests_stands_out_from_across_the_list() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        panel.set_row_enabled("kimi", Some("demo-kimi"), false);
        panel.observe(&served("codex", "demo-work", EndpointKind::Messages));
        let mut terminal = Terminal::new(TestBackend::new(160, 30)).unwrap();
        terminal
            .draw(|frame| panel.render(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let find = |needle: &str| {
            (0..30)
                .find_map(|y| {
                    let row = (0..160)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>();
                    row.find(needle)
                        .map(|x| (row[..x].chars().count() as u16, y))
                })
                .unwrap_or_else(|| panic!("no {needle}"))
        };
        // Only an account that reached generation gets a bullet and bold teal name.
        let (x, y) = find("● Codex / work");
        assert_eq!(buffer[(x, y)].fg, TEAL);
        assert!(buffer[(x, y)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(x + 2, y)].fg, TEAL);
        // Another row that is on has no bullet and a plain name.
        let (home_x, home_y) = find("Codex / home");
        assert_eq!(buffer[(home_x - 2, home_y)].symbol(), " ");
        assert!(!buffer[(home_x, home_y)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(home_x, home_y)].fg, WHITE);
        // A row that is off stays in its place, dim, and says so in words.
        let (kimi_x, kimi_y) = find("Kimi / personal");
        assert_eq!(buffer[(kimi_x, kimi_y)].fg, DIM_WHITE);
        let row = (0..64)
            .map(|x| buffer[(x, kimi_y + 1)].symbol())
            .collect::<String>();
        assert!(row.contains(" off"), "{row}");
        // The old order tags are gone: the list itself is the order.
        let everything = render(&mut panel, 160, 30);
        for gone in ["[primary]", "[fallback", "[current]", "Fallback:"] {
            assert!(!everything.contains(gone), "{gone}: {everything}");
        }
        // Narrow terminals keep the bullet on the name's line.
        for (width, height) in [(80, 24), (40, 24)] {
            let text = render(&mut panel, width, height);
            assert!(text.contains("● Codex / work"), "{width}: {text}");
        }
        // A failed refresh puts a yellow * on the row. Details say what failed.
        panel.rows[0].accounts[1].usage_error = Some("Press r to retry.".into());
        let mut terminal = Terminal::new(TestBackend::new(160, 30)).unwrap();
        terminal
            .draw(|frame| panel.render(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let list_row = (0..64)
            .map(|x| buffer[(x, y + 1)].symbol())
            .collect::<String>();
        let at = list_row.find('*').unwrap();
        assert_eq!(
            buffer[(list_row[..at].chars().count() as u16, y + 1)].fg,
            YELLOW
        );
        assert!(!list_row.contains("refresh failed"), "{list_row}");
    }

    #[test]
    fn right_opens_the_actions_and_left_goes_back_to_the_same_account() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        let work = Entry::Account("codex".into(), "demo-work".into());
        panel.selected = work.clone();
        assert!(panel.press(KeyCode::Right).is_none());
        assert!(panel.menu_open());
        // Inside the menu Right runs nothing. Only Enter does.
        assert!(panel.press(KeyCode::Right).is_none());
        assert!(panel.menu_open());
        assert!(panel.press(KeyCode::Left).is_none());
        assert!(!panel.menu_open());
        assert_eq!(panel.selected, work);
        // With nothing open there is nowhere further back to go.
        assert!(panel.press(KeyCode::Left).is_none());
        assert_eq!(panel.selected, work);
    }

    #[test]
    fn refresh_keeps_the_cursor_on_its_account_when_the_order_changes() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        panel.route = config::Route {
            on: true,
            provider: Some("codex".into()),
            ..Default::default()
        };
        // A read where `first` leads the pool.
        let first_is = |first: &str| {
            let mut loaded = Loaded::from(demo_rows());
            loaded
                .pool
                .sort_by_key(|row| row.account_id.as_deref() != Some(first));
            loaded
        };
        let work = Entry::Account("codex".into(), "demo-work".into());
        let kimi = Entry::Account("kimi".into(), "demo-kimi".into());

        panel.selected = work.clone();
        panel.take_rows(first_is("demo-home"));
        assert_eq!(panel.selected, work);

        // Somewhere else, or inside a menu, the cursor stays put.
        panel.selected = kimi.clone();
        panel.take_rows(first_is("demo-work"));
        assert_eq!(panel.selected, kimi);
        panel.selected = work.clone();
        panel.menu = Some(panel.actions());
        panel.take_rows(first_is("demo-home"));
        assert_eq!(panel.selected, work);
    }

    #[test]
    fn only_actions_that_may_ask_a_question_leave_the_monitor() {
        let account = |action: fn(String, String) -> Action| action("codex".into(), "id".into());
        assert!(!account(|provider, id| Action::RemoveAccount { provider, id }).needs_terminal());
        for action in [
            Action::SavePool {
                provider: "codex".into(),
                account: Some("id".into()),
                at: 0,
                pool: Vec::new(),
            },
            Action::SetEnabled {
                provider: "codex".into(),
                account: None,
                enabled: false,
            },
        ] {
            assert!(!action.needs_terminal());
        }
        // Going back to Claude Code's own login never asks anything.
        assert!(!Action::DisableRouting.needs_terminal());
        assert!(!Action::EnableRouting.needs_terminal());
        // Signing in always does: a browser, a code, a key.
        assert!(
            Action::SignIn {
                provider: "codex".into(),
                account: None,
                add: false
            }
            .needs_terminal()
        );
        assert!(Action::StartProxy.needs_terminal());
    }

    #[test]
    fn navigation_keeps_selection_visible_and_actions_are_scoped() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        for _ in 0..12 {
            panel.press(KeyCode::Down);
        }
        assert_eq!(panel.selected, Entry::Direct);
        panel.press(KeyCode::Up);
        assert_eq!(panel.selected, Entry::Provider("opencode-zen".into()));
        assert!(render(&mut panel, 40, 14).contains("OpenCode Zen"));
        let labels = |panel: &ProvidersPanel| {
            panel
                .actions()
                .into_iter()
                .map(|(label, _)| label)
                .collect::<Vec<_>>()
        };
        // A provider that is not set up is not in the pool: it can't move or turn on.
        for label in [MOVE_ROW, "Enable", "Disable"] {
            assert!(!labels(&panel).iter().any(|l| l == label), "{label}");
        }
        // Every row in the pool offers Move... and Enable/Disable.
        panel.selected = Entry::Account("codex".into(), "demo-work".into());
        let first = labels(&panel);
        assert!(
            !first
                .iter()
                .any(|l| l == "Use now" || l == "Use provider for Claude Code")
        );
        assert!(first.iter().any(|l| l == MOVE_ROW));
        assert!(first.iter().any(|l| l == "Disable"));
        panel.selected = Entry::Account("kimi".into(), "demo-kimi".into());
        let last = labels(&panel);
        assert!(last.iter().any(|l| l == MOVE_ROW));
        // A row that is off offers to turn it on.
        panel.pool[2].enabled = false;
        assert!(labels(&panel).iter().any(|l| l == "Enable"));
        // The old fallback actions are gone for every row.
        assert!(!labels(&panel).iter().any(|l| l.contains("fallback")));
    }

    #[test]
    fn remote_mode_never_loads_local_accounts_or_offers_actions() {
        let mut panel = ProvidersPanel::new(Mode::Remote);
        panel.refresh();
        assert!(panel.pending.is_none());
        assert!(panel.rows.is_empty());
        assert_eq!(panel.press(KeyCode::Enter), None);
        let text = render(&mut panel, 80, 20);
        assert!(text.contains("read-only"));
        assert!(text.contains("Press 2 for activity"));
    }

    #[test]
    fn failed_action_keeps_the_action_ready_to_retry() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        panel.press(KeyCode::Enter);
        panel.press(KeyCode::Down);
        let action = panel.press(KeyCode::Enter).unwrap();
        panel.finish_action(Err(anyhow::anyhow!("config.json is not writable")));
        assert_eq!(panel.press(KeyCode::Enter), Some(action));
        assert!(render(&mut panel, 110, 25).contains("config.json is not writable"));
    }

    #[test]
    fn usage_is_joined_by_immutable_id_not_account_name_or_position() {
        let mut rows = demo_rows();
        apply_account_usage(
            &mut rows,
            &HashMap::from([(
                ("codex".into(), "demo-work".into()),
                MonitorUsage::Reported(vec![UsageWindow {
                    label: "5h",
                    remaining_percent: 93.0,
                    resets_at_ms: None,
                }]),
            )]),
        );
        assert!(matches!(
            rows[0].accounts[0].usage,
            Some(MonitorUsage::Unavailable(_))
        ));
        assert_eq!(
            rows[0].accounts[1].usage,
            Some(MonitorUsage::Reported(vec![UsageWindow {
                label: "5h",
                remaining_percent: 93.0,
                resets_at_ms: None
            }])),
        );
        assert!(rows[0].accounts[1].waits.is_empty());
    }

    #[test]
    fn remaining_percentages_keep_their_color_in_the_rendered_monitor() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        panel.selected = Entry::Account("codex".into(), "demo-work".into());
        panel.rows[0].accounts[1].usage = Some(MonitorUsage::Reported(
            [100.0, 55.0, 20.0, 0.0]
                .into_iter()
                .map(|percent| UsageWindow {
                    label: "5h",
                    remaining_percent: percent,
                    resets_at_ms: None,
                })
                .collect(),
        ));
        let mut terminal = Terminal::new(TestBackend::new(160, 30)).unwrap();
        terminal
            .draw(|frame| panel.render(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        for percent in [100.0, 55.0, 20.0, 0.0] {
            let text = format!("5h {percent:.0}% left");
            let (x, y) = (0..30)
                .find_map(|y| {
                    let row = (0..160)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>();
                    row.find(&text)
                        .map(|x| (row[..x].chars().count() as u16, y))
                })
                .unwrap();
            assert_eq!(buffer[(x + 3, y)].fg, ui::remaining_color(percent));
        }
    }

    #[test]
    fn selected_account_keeps_its_quota_color() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        panel.selected = Entry::Account("codex".into(), "demo-home".into());
        let mut terminal = Terminal::new(TestBackend::new(160, 30)).unwrap();
        terminal
            .draw(|frame| panel.render(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let (x, y) = (0..30)
            .find_map(|y| {
                let row = (0..160)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>();
                row.find("5h 0% ")
                    .map(|x| (row[..x].chars().count() as u16 + 3, y))
            })
            .unwrap();
        assert_eq!(buffer[(x, y)].fg, ui::remaining_color(0.0));
    }

    #[test]
    fn when_every_row_that_is_on_waits_the_one_that_frees_first_is_marked() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        let now = crate::rotation::now_ms();
        let home = Entry::Account("codex".into(), "demo-home".into());
        panel.rows[0].accounts[0].waits = vec![(None, now + 3_600_000)];
        panel.rows[0].accounts[1].waits = vec![(None, now + 7_200_000)];
        panel.selected = home.clone();
        // Kimi's row is on and ready, so nothing is waiting as a whole yet.
        assert!(panel.first_reset(now).is_none());
        let text = render(&mut panel, 160, 30);
        assert!(!text.contains("[first reset]"), "{text}");

        panel.rows[1].accounts[0].waits = vec![(None, now + 10_800_000)];
        assert_eq!(panel.first_reset(now), Some((home, now + 3_600_000)));
        let text = render(&mut panel, 160, 30);
        for expected in [
            "0% ",
            "[first reset]",
            "Next reset: Codex / home in",
            "Every row that is on is waiting. Codex / home resets in",
            "next request",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        assert_eq!(
            panel.pool.iter().map(|row| row.enabled).collect::<Vec<_>>(),
            [true, true, true],
            "Rendering must not change the pool"
        );
        panel.rows[0].accounts[0].waits.clear();
        assert!(!render(&mut panel, 160, 30).contains("Every row that is on is waiting"));
    }

    #[test]
    fn model_specific_waits_never_claim_every_model_is_waiting() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        for account in &mut panel.rows[0].accounts {
            account.waits = vec![(Some("one-model".into()), crate::rotation::now_ms() + 60_000)];
        }
        assert!(panel.first_reset(crate::rotation::now_ms()).is_none());
        assert!(!render(&mut panel, 160, 30).contains("Every row that is on is waiting"));
    }

    #[test]
    fn opencode_usage_stays_with_the_current_account() {
        let mut rows = demo_rows();
        rows[0].id = "opencode".into();
        rows[0].usage = Some(MonitorUsage::Reported(vec![UsageWindow {
            label: "5h",
            remaining_percent: 58.0,
            resets_at_ms: None,
        }]));
        apply_account_usage(&mut rows, &HashMap::new());
        assert!(matches!(
            rows[0].accounts[0].usage,
            Some(MonitorUsage::NotReported(_))
        ));
        assert_eq!(rows[0].accounts[1].usage, rows[0].usage);
    }

    #[test]
    fn usage_stays_visible_in_a_small_terminal() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        panel.selected = Entry::Account("codex".into(), "demo-work".into());
        let text = render(&mut panel, 40, 13);
        assert!(text.contains("5h 58% left"), "{text}");
        assert!(text.contains("week 88% left"), "{text}");
        panel.rows[0].accounts[1].name = "a-long-account-name-for-a-team".into();
        for _ in 0..8 {
            panel.press(KeyCode::PageDown);
        }
        assert!(render(&mut panel, 40, 13).contains("hits a"));
        panel.press(KeyCode::Enter);
        assert!(render(&mut panel, 40, 13).contains(MOVE_ROW));
    }

    #[test]
    fn a_wait_timer_never_invents_zero_quota() {
        let mut account = demo_rows().remove(0).accounts.remove(1);
        let now = crate::rotation::now_ms();
        account.waits = vec![(None, now + 60_000)];
        assert_eq!(
            account.usage_summary(now).to_string(),
            "5h 58% · week 88% · waiting · 1m00s left"
        );
        account.usage = None;
        assert_eq!(
            account.usage_summary(now).to_string(),
            "waiting · 1m00s left"
        );
        account.waits = vec![(Some("one-model".into()), now + 60_000)];
        assert_eq!(account.usage_summary(now).to_string(), "checking...");
    }

    #[test]
    fn antigravity_keeps_each_family_visible_across_refreshes() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        panel.rows.truncate(1);
        panel.rows[0].id = "antigravity".into();
        panel.rows[0].accounts.truncate(1);
        panel.selected = Entry::Account("antigravity".into(), "demo-home".into());
        let now = crate::rotation::now_ms();
        let windows: Vec<_> = [
            ("Gemini 5h", 100.0),
            ("Gemini week", 95.0),
            ("Claude 5h", 100.0),
            ("Claude week", 0.0),
        ]
        .into_iter()
        .map(|(label, remaining_percent)| UsageWindow {
            label,
            remaining_percent,
            resets_at_ms: Some(now as i64 + 345_600_000),
        })
        .collect();
        let usage = HashMap::from([(
            ("antigravity".into(), "demo-home".into()),
            MonitorUsage::Reported(windows),
        )]);
        panel.rows[0].accounts[0].waits = vec![(Some("claude-model".into()), now + 60_000)];
        apply_account_usage(&mut panel.rows, &usage);
        assert!(panel.rows[0].accounts[0].waiting_until(now).is_none());
        let previous = panel.rows[0].clone();
        let mut refreshing = metadata(panel.rows.clone());
        refreshing[0].retain_usage(&previous);
        panel.rows = refreshing;
        for (width, height) in [(180, 36), (80, 36), (40, 36)] {
            let text = render(&mut panel, width, height);
            for expected in [
                "Gemini 5h 100%",
                "Gemini week 95%",
                "Claude 5h 100%",
                "Claude week 0%",
            ] {
                assert!(text.contains(expected), "missing {expected}: {text}");
            }
            assert!(!text.contains("All accounts are waiting"), "{text}");
        }
    }

    #[test]
    fn a_global_wait_keeps_all_reported_windows_in_the_list() {
        let mut account = demo_rows().remove(0).accounts.remove(1);
        let now = crate::rotation::now_ms();
        account.waits = vec![(None, now + 60_000)];
        if let Some(MonitorUsage::Reported(windows)) = &mut account.usage {
            windows[0].remaining_percent = 0.0;
        }
        let summary = account.usage_summary(now).to_string();
        assert!(summary.contains("5h 0%"), "{summary}");
        assert!(summary.contains("week 88%"), "{summary}");
    }

    #[test]
    fn an_account_out_of_credit_says_so_and_never_claims_a_reset() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        let now = crate::rotation::now_ms();
        let recheck = now + 30 * 60_000;
        for account in &mut panel.rows[0].accounts {
            account.usage = Some(MonitorUsage::NotReported("no balance"));
            account.waits = vec![(None, recheck)];
            account.spent = true;
        }
        panel.selected = Entry::Account("codex".into(), "demo-home".into());
        for (width, height) in [(160, 30), (80, 24), (40, 24)] {
            let text = render(&mut panel, width, height);
            assert!(text.contains("out of credit"), "{width}: {text}");
            assert!(text.contains("tries again in"), "{width}: {text}");
            for never in ["[first reset]", "waiting", "resets in", "not reported"] {
                assert!(!text.contains(never), "{width}: {never}: {text}");
            }
        }
        // Wide enough that the sentence does not wrap inside the details panel.
        let wide = render(&mut panel, 400, 30);
        assert!(wide.contains("Out of credit. cc-proxy tries this account again in"));
        assert!(wide.contains("After you add credit, cc-proxy retries it at that time"));
        assert!(!wide.contains("At its limit"), "{wide}");

        // Kimi's row is on and fine, so the list as a whole is not out of credit.
        assert!(
            !wide.contains("Every row that is on is out of credit"),
            "{wide}"
        );
        for account in &mut panel.rows[1].accounts {
            account.usage = Some(MonitorUsage::NotReported("no balance"));
            account.waits = vec![(None, recheck)];
            account.spent = true;
        }
        // Once every row that is on is spent, Details says what to do.
        let all = render(&mut panel, 160, 30);
        assert!(
            all.contains("Every row that is on is out of credit"),
            "{all}"
        );
        assert!(!all.contains("Every row that is on is waiting"), "{all}");
    }

    #[test]
    fn a_spent_account_keeps_its_quota_windows_and_a_real_reset_still_counts_as_first() {
        let now = crate::rotation::now_ms();
        let mut panel = ProvidersPanel::new(Mode::Demo);
        // Kimi's row is off, so only Codex's two rows decide.
        panel.set_row_enabled("kimi", Some("demo-kimi"), false);
        let codex = &mut panel.rows[0];
        // Windows stay beside the note: a wait never hides measured quota.
        codex.accounts[1].waits = vec![(None, now + 1_800_000)];
        codex.accounts[1].spent = true;
        let summary = codex.accounts[1].usage_summary(now).to_string();
        assert!(summary.starts_with("5h 58% · week 88% · out of credit · tries again in"));

        // One account out of credit and one with a real reset: the real one is first.
        codex.accounts[0].waits = vec![(None, now + 3_600_000)];
        assert_eq!(
            panel.first_reset(now),
            Some((
                Entry::Account("codex".into(), "demo-home".into()),
                now + 3_600_000
            ))
        );
        // All rows out of credit: nobody resets, so nobody is first.
        panel.rows[0].accounts[0].spent = true;
        assert!(panel.first_reset(now).is_none());
        assert!(panel.all_out_of_credit(now));
        // A wait that ended is not out of credit any more.
        assert!(!panel.all_out_of_credit(now + 3_600_001));
    }

    #[test]
    fn refresh_keeps_an_out_of_credit_wait_when_usage_windows_arrive() {
        let now = crate::rotation::now_ms();
        let mut rows = demo_rows();
        rows[0].accounts[1].waits = vec![(None, now + 1_800_000)];
        rows[0].accounts[1].spent = true;
        rows[0].accounts[0].waits = vec![(None, now + 600_000)];
        apply_account_usage(
            &mut rows,
            &HashMap::from([
                (
                    ("codex".into(), "demo-work".into()),
                    MonitorUsage::Reported(vec![UsageWindow {
                        label: "5h",
                        remaining_percent: 40.0,
                        resets_at_ms: None,
                    }]),
                ),
                (
                    ("codex".into(), "demo-home".into()),
                    MonitorUsage::Reported(vec![UsageWindow {
                        label: "5h",
                        remaining_percent: 40.0,
                        resets_at_ms: None,
                    }]),
                ),
            ]),
        );
        // The usage lookup clears a reset wait it no longer finds, not a spent balance.
        assert!(rows[0].accounts[0].waits.is_empty());
        assert_eq!(rows[0].accounts[1].waits, vec![(None, now + 1_800_000)]);
    }

    #[test]
    fn a_plan_note_shows_in_the_details_of_that_account_only() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        panel.rows[0].accounts[0].note = Some("Free plan. A full bar is not model access.".into());
        panel.selected = Entry::Account("codex".into(), "demo-home".into());
        assert!(render(&mut panel, 400, 30).contains("Free plan. A full bar is not model access."));
        panel.selected = Entry::Account("codex".into(), "demo-work".into());
        assert!(!render(&mut panel, 400, 30).contains("Free plan."));
    }

    #[test]
    fn the_list_follows_the_pool_and_providers_not_set_up_wait_below_it() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        for (id, state) in [
            ("grok", AuthState::Missing),
            ("anthropic", AuthState::KeySaved),
            ("gemini", AuthState::KeySaved),
            ("cursor", AuthState::Missing),
        ] {
            let mut row = panel.rows[2].clone();
            row.id = id.into();
            row.state = state;
            panel.rows.push(row);
        }
        // Providers that hold one credential are rows of the pool too.
        for id in ["anthropic", "gemini"] {
            panel.pool.push(PoolEntry {
                provider: id.into(),
                account_id: None,
                enabled: false,
            });
        }
        let names = |panel: &ProvidersPanel| {
            panel
                .entries()
                .into_iter()
                .map(|entry| match entry {
                    Entry::Provider(id) => id,
                    Entry::Account(provider, id) => format!("{provider}/{id}"),
                    Entry::Direct => "direct".into(),
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(&panel),
            [
                "codex/demo-work",
                "codex/demo-home",
                "kimi/demo-kimi",
                "anthropic",
                "gemini",
                "cursor",
                "grok",
                "opencode-zen",
                "direct"
            ]
        );
        // Moving a row changes where it stands, whatever its provider.
        panel.move_row("gemini", None, isize::MIN);
        panel.move_row("kimi", Some("demo-kimi"), -1);
        assert_eq!(
            names(&panel)[..5],
            [
                "gemini",
                "codex/demo-work",
                "kimi/demo-kimi",
                "codex/demo-home",
                "anthropic"
            ]
        );
        panel.route.on = false;
        assert_eq!(names(&panel).last().unwrap(), "direct");
    }

    #[test]
    fn accounts_of_different_providers_mix_in_one_list() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        let mut claude = panel.rows[0].clone();
        claude.id = "claude".into();
        panel.rows.push(claude);
        for id in ["demo-home", "demo-work"] {
            panel.pool.push(PoolEntry {
                provider: "claude".into(),
                account_id: Some(id.into()),
                enabled: true,
            });
        }
        let labels = |panel: &ProvidersPanel| {
            panel
                .entries()
                .iter()
                .map(|entry| panel.label_of(entry))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            labels(&panel)[..5],
            [
                "Codex / work",
                "Codex / home",
                "Kimi / personal",
                "Claude / home",
                "Claude / work"
            ]
        );
        // One Claude account between the Codex ones: the list is not grouped.
        panel.move_row("claude", Some("demo-work"), -3);
        assert_eq!(
            labels(&panel)[..4],
            [
                "Codex / work",
                "Claude / work",
                "Codex / home",
                "Kimi / personal"
            ]
        );
        panel.selected = Entry::Account("claude".into(), "demo-work".into());
        assert!(render(&mut panel, 160, 35).contains("5h 58% left"));
        let order = labels(&panel);
        panel.selected = Entry::Direct;
        assert_eq!(panel.actions()[0].1, Action::DisableRouting);
        panel.preview_action(&Action::DisableRouting);
        assert!(panel.route.target().is_none());
        assert_eq!(labels(&panel), order);
        // With routing off, the same row sends Claude Code through the proxy again.
        assert_eq!(panel.actions()[0].1, Action::EnableRouting);
        panel.preview_action(&Action::EnableRouting);
        assert_eq!(panel.route.target(), Some("codex"));
        assert_eq!(labels(&panel), order);
        panel.preview_action(&Action::DisableRouting);
        let text = render(&mut panel, 160, 35);
        assert!(text.contains("Proxy routing is off [in use]"), "{text}");
        assert!(
            text.contains("uses its own settings and login now"),
            "{text}"
        );
        panel.press(KeyCode::Home);
        assert_eq!(
            panel.selected,
            Entry::Account("codex".into(), "demo-work".into())
        );
        panel.press(KeyCode::End);
        assert_eq!(panel.selected, Entry::Direct);
    }

    #[test]
    fn account_list_shows_quota_for_unselected_accounts_and_long_names() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        panel.rows[0].accounts[1].name = "a-very-long-email-address-for-work@example.com".into();
        for (width, height) in [(160, 30), (80, 24), (40, 24)] {
            let text = render(&mut panel, width, height);
            assert!(text.contains("58%"), "{text}");
            assert!(text.contains("88%"), "{text}");
            assert!(text.contains("0%"), "{text}");
            assert!(text.contains("left"), "{text}");
        }
        if let Some(MonitorUsage::Reported(windows)) = &mut panel.rows[0].accounts[1].usage {
            for window in windows {
                window.remaining_percent = 0.0;
            }
        }
        let text = render(&mut panel, 40, 30);
        assert!(text.contains("5h 0%"), "{text}");
        assert!(text.contains("week 0%"), "{text}");
    }

    #[test]
    fn move_and_enable_change_the_list_at_once_without_switching_logins() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        let order = |panel: &ProvidersPanel| {
            panel
                .entries()
                .iter()
                .filter(|entry| **entry != Entry::Direct)
                .map(|entry| panel.label_of(entry))
                .take(3)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            order(&panel),
            ["Codex / work", "Codex / home", "Kimi / personal"]
        );

        // Move...: Up/Down move one place, Right jumps to the bottom, Left jumps
        // to the top, Esc puts the old order back, and Enter saves.
        let work = Entry::Account("codex".into(), "demo-work".into());
        panel.selected = work.clone();
        panel.press(KeyCode::Enter);
        panel.press(KeyCode::Enter);
        assert!(panel.moving());
        panel.press(KeyCode::Right);
        assert_eq!(
            order(&panel),
            ["Codex / home", "Kimi / personal", "Codex / work"]
        );
        panel.press(KeyCode::Left);
        assert_eq!(
            order(&panel),
            ["Codex / work", "Codex / home", "Kimi / personal"]
        );
        panel.press(KeyCode::Down);
        let save = panel.press(KeyCode::Enter).unwrap();
        panel.preview_action(&save);
        assert!(!panel.moving());
        assert_eq!(
            order(&panel),
            ["Codex / home", "Codex / work", "Kimi / personal"]
        );
        assert_eq!(panel.selected, work);
        // The header names the first provider that is on.
        assert_eq!(panel.route.target(), Some("codex"));

        // Turning the last row off keeps its place and dims it.
        panel.selected = Entry::Account("kimi".into(), "demo-kimi".into());
        let off = panel
            .actions()
            .into_iter()
            .find(|(label, _)| label == "Disable")
            .unwrap()
            .1;
        panel.preview_action(&off);
        assert_eq!(
            order(&panel),
            ["Codex / home", "Codex / work", "Kimi / personal"]
        );
        assert!(!panel.pool[2].enabled);
        assert_eq!(panel.route.fallback, Vec::<String>::new());

        // Enabling the row keeps its place and does not change the order.
        let enable = panel
            .actions()
            .into_iter()
            .find(|(label, _)| label == "Enable")
            .unwrap()
            .1;
        panel.preview_action(&enable);
        assert_eq!(
            order(&panel),
            ["Codex / home", "Codex / work", "Kimi / personal"]
        );
        assert!(panel.pool[2].enabled);
        assert_eq!(panel.route.target(), Some("codex"));
        assert!(render(&mut panel, 120, 25).contains("Demo only"));
    }

    #[test]
    fn the_header_says_how_new_chats_pick_a_row() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        let text = render(&mut panel, 160, 30);
        assert!(
            text.contains("New chats share the first provider's ready accounts. A chat stays on its account until it hits a limit."),
            "{text}"
        );
        assert!(text.contains("Claude Code: Codex"), "{text}");
        assert!(!text.contains("Fallback:"), "{text}");
        // Without auto-rotate the first row that is on answers everything.
        panel.auto_rotate = false;
        assert!(
            render(&mut panel, 160, 30).contains("the first row that is on answers every request")
        );
        // With routing off, it says where Claude Code goes instead.
        panel.route.on = false;
        let off = render(&mut panel, 160, 30);
        assert!(
            off.contains("Routing is off. Claude Code uses its own login."),
            "{off}"
        );
    }

    #[test]
    fn a_provider_with_one_credential_is_a_row_with_its_own_wait() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        let mut zen = panel.rows[2].clone();
        zen.id = "anthropic".into();
        zen.state = AuthState::KeySaved;
        zen.waits = vec![(None, crate::rotation::now_ms() + 600_000)];
        panel.rows.push(zen);
        panel.pool.push(PoolEntry {
            provider: "anthropic".into(),
            account_id: None,
            enabled: true,
        });
        let row = Entry::Provider("anthropic".into());
        assert!(panel.entries().contains(&row));
        panel.selected = row;
        let text = render(&mut panel, 160, 30);
        // It is named by its provider alone, and a wait still shows.
        assert!(text.contains("Anthropic"), "{text}");
        assert!(text.contains("waiting"), "{text}");
        assert!(text.contains("Number 4 of 4 in the list. On."), "{text}");
        let labels: Vec<String> = panel
            .actions()
            .into_iter()
            .map(|(label, _)| label)
            .collect();
        assert!(!labels.contains(&"Use provider for Claude Code".to_string()));
        assert!(labels.contains(&MOVE_ROW.to_string()));
        assert!(
            !labels
                .iter()
                .any(|l| l.contains("Rename") || l.contains("Remove"))
        );
    }

    #[test]
    fn a_pool_row_with_no_saved_login_behind_it_is_not_listed() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        panel.pool.push(PoolEntry {
            provider: "codex".into(),
            account_id: Some("signed-out-long-ago".into()),
            enabled: true,
        });
        assert!(!panel.entries().contains(&Entry::Account(
            "codex".into(),
            "signed-out-long-ago".into()
        )));
    }

    #[test]
    fn small_and_narrow_screens_render_browse_and_actions() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        for (width, height) in [(160, 40), (80, 24), (40, 16), (20, 8), (3, 2)] {
            render(&mut panel, width, height);
            panel.press(KeyCode::Enter);
            render(&mut panel, width, height);
            panel.close_menu();
        }
    }
}
