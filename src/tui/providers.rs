//! Local saved accounts and usage. Network lookups run only on the panel's worker.

use std::collections::HashMap;
use std::sync::mpsc;
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
use crate::{config, rotation::Cooldowns, route, ui};

const REFRESH_AFTER: Duration = Duration::from_secs(60);

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
}

impl ProviderRow {
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

    /// Set only when every account waits. A spent balance has no reset, so it
    /// never counts as the first one to reset.
    fn first_reset(&self, now: u64) -> Option<(&AccountRow, u64)> {
        self.accounts
            .iter()
            .map(|account| account.waiting_until(now).map(|until| (account, until)))
            .collect::<Option<Vec<_>>>()?
            .into_iter()
            .filter(|(account, _)| !account.spent)
            .min_by_key(|(_, until)| *until)
    }

    fn all_out_of_credit(&self, now: u64) -> bool {
        !self.accounts.is_empty()
            && self
                .accounts
                .iter()
                .all(|account| account.out_of_credit_until(now).is_some())
    }
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
    active: bool,
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
}

pub(super) struct ProvidersPanel {
    mode: Mode,
    rows: Vec<ProviderRow>,
    route: config::Route,
    auto_rotate: bool,
    /// Last request that reached generation, not the route's first choice.
    serving: Option<Serving>,
    /// What each saved account used since the proxy started, by provider and
    /// immutable account id, so a rename never splits it.
    spend: HashMap<(String, String), UsageTotals>,
    loaded_at: Option<Instant>,
    pending: Option<mpsc::Receiver<Vec<ProviderRow>>>,
    selected: Entry,
    list: ListState,
    menu: Option<Vec<(String, Action)>>,
    menu_state: ListState,
    notice: Option<(String, bool)>,
    detail_scroll: u16,
    rename: Option<RenameDraft>,
}

impl ProvidersPanel {
    pub fn new(mode: Mode) -> Self {
        let mut panel = Self {
            mode,
            rows: Vec::new(),
            route: config::Route::default(),
            auto_rotate: false,
            serving: None,
            spend: HashMap::new(),
            loaded_at: None,
            pending: None,
            selected: Entry::Direct,
            list: ListState::default(),
            menu: None,
            menu_state: ListState::default(),
            notice: None,
            detail_scroll: 0,
            rename: None,
        };
        if mode == Mode::Demo {
            panel.rows = demo_rows();
            panel.route.on = true;
            panel.route.provider = Some("codex".into());
            panel.route.fallback = vec!["kimi".into()];
            panel.auto_rotate = true;
            panel.selected = Entry::Provider("codex".into());
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
            for rows in batches {
                self.take_rows(rows);
            }
            if !finished {
                return;
            }
            self.pending = None;
            self.loaded_at = Some(Instant::now());
            if !self.entries().contains(&self.selected) {
                self.selected = self
                    .selected_provider()
                    .map(|row| Entry::Provider(row.id.clone()))
                    .unwrap_or_else(|| self.entries()[0].clone());
                self.menu = None;
            }
        }
        if self
            .loaded_at
            .is_none_or(|loaded| loaded.elapsed() >= REFRESH_AFTER)
        {
            let (send, receive) = mpsc::channel();
            std::thread::spawn(move || load(&send));
            self.pending = Some(receive);
        }
    }

    fn take_rows(&mut self, mut rows: Vec<ProviderRow>) {
        for row in &mut rows {
            if let Some(previous) = self.rows.iter().find(|old| old.id == row.id) {
                row.retain_usage(previous);
            }
        }
        let first_load = self.rows.is_empty();
        let followed = self.primary_entry();
        self.rows = rows;
        if first_load {
            self.selected = self
                .primary_entry()
                .unwrap_or_else(|| self.entries()[0].clone());
        } else if self.menu.is_none()
            && followed.as_ref() == Some(&self.selected)
            && let Some(now) = self.primary_entry()
        {
            // Auto-rotate moved the saved account while the cursor sat on it,
            // so the cursor goes along. A cursor the person moved somewhere
            // else stays where it is.
            self.selected = now;
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
        let before = self.primary_entry();
        self.serving = snapshot
            .active
            .iter()
            .filter(|request| request.endpoint != EndpointKind::CountTokens)
            .filter_map(|request| {
                Some((
                    request.generation_started_at?,
                    Serving {
                        provider: request.provider.clone()?,
                        account_id: request.account_id.clone(),
                        account: request.account.clone(),
                        active: true,
                    },
                ))
            })
            .chain(snapshot.recent.iter().filter_map(|request| {
                if request.endpoint == EndpointKind::CountTokens
                    || request.status != RequestStatus::Completed
                {
                    return None;
                }
                Some((
                    request.generation_started_at.unwrap_or(request.finished_at),
                    Serving {
                        provider: request.provider.clone()?,
                        account_id: request.account_id.clone(),
                        account: request.account.clone(),
                        active: false,
                    },
                ))
            }))
            .max_by_key(|(at, _)| *at)
            .map(|(_, serving)| serving);
        if self.menu.is_none()
            && before.as_ref() == Some(&self.selected)
            && let Some(now) = self.primary_entry()
        {
            self.selected = now;
        }
    }

    pub fn retry(&mut self) {
        self.loaded_at = None;
        self.refresh();
    }

    /// Prefer the account that served a request over the route's first choice.
    fn primary_entry(&self) -> Option<Entry> {
        if let Some(serving) = &self.serving {
            let row = self.rows.iter().find(|row| row.id == serving.provider)?;
            let account = row
                .accounts
                .iter()
                .find(|account| serving.account_id.as_deref() == Some(account.id.as_str()));
            return Some(account.map_or_else(
                || Entry::Provider(row.id.clone()),
                |account| Entry::Account(row.id.clone(), account.id.clone()),
            ));
        }
        let row = self
            .rows
            .iter()
            .find(|row| self.route.target() == Some(row.id.as_str()))?;
        Some(
            row.accounts
                .iter()
                .find(|account| account.current)
                .map_or_else(
                    || Entry::Provider(row.id.clone()),
                    |account| Entry::Account(row.id.clone(), account.id.clone()),
                ),
        )
    }

    /// Who answered the newest request, in words, so the teal in the list
    /// is never the only sign.
    fn serving_line(&self) -> Line<'static> {
        let Some(serving) = &self.serving else {
            return Line::styled(
                " No request served yet in this run.",
                Style::default().fg(DIM_WHITE),
            );
        };
        let account = serving
            .account
            .as_ref()
            .map(|name| format!(" / {name}"))
            .unwrap_or_default();
        Line::styled(
            format!(
                " {}: {}{account}",
                if serving.active {
                    "Serving"
                } else {
                    "Last used"
                },
                ui::provider_name(&serving.provider)
            ),
            Style::default().fg(TEAL),
        )
    }

    /// Whether `provider` is the one answering requests. Before any request it
    /// is the route's first choice, which is what the next request will use.
    fn serves(&self, provider: &str) -> bool {
        match &self.serving {
            Some(serving) => serving.provider == provider,
            None => self.route.target() == Some(provider),
        }
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

    fn serves_account(&self, provider: &str, account: &AccountRow) -> bool {
        match &self.serving {
            Some(serving) => {
                serving.provider == provider
                    && serving.account_id.as_deref() == Some(account.id.as_str())
            }
            None => account.current && self.route.target() == Some(provider),
        }
    }

    fn entries(&self) -> Vec<Entry> {
        let mut entries = Vec::new();
        let mut rows: Vec<_> = self.rows.iter().collect();
        rows.sort_by_cached_key(|row| {
            let role = if self.route.target() == Some(row.id.as_str()) {
                (0, 0)
            } else if let Some(at) = self.route.fallback.iter().position(|id| id == &row.id) {
                (1, at)
            } else if row.state != AuthState::Missing {
                (2, 0)
            } else {
                (3, 0)
            };
            (role, ui::provider_name(&row.id).to_lowercase(), &row.id)
        });
        for row in rows {
            entries.push(Entry::Provider(row.id.clone()));
            entries.extend(
                row.accounts
                    .iter()
                    .map(|account| Entry::Account(row.id.clone(), account.id.clone())),
            );
        }
        entries.push(Entry::Direct);
        entries
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
        // A question that was never answered must not stay on screen.
        let menu = self.menu.take().is_some();
        let rename = self.rename.take().is_some();
        let was_open = menu || rename;
        if was_open {
            self.notice = None;
        }
        was_open
    }

    /// An action list or a name field is open over the account list.
    pub fn menu_open(&self) -> bool {
        self.menu.is_some() || self.rename.is_some()
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
        });
        self.menu = None;
        self.notice = None;
        self.detail_scroll = 0;
    }

    /// Keeps the name field open with what was typed, and says why it was refused.
    fn refuse_name(&mut self, reason: String) {
        if let Some(draft) = &mut self.rename {
            draft.error = Some(reason);
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
            return vec![(
                "Disable proxy routing".into(),
                Action::UseProvider("direct".into()),
            )];
        }
        let Some(row) = self.selected_provider() else {
            return Vec::new();
        };
        let mut actions = Vec::new();
        if let Some(account) = self.selected_account()
            && row.pinned.is_none()
        {
            actions.push((
                "Use account for new requests".into(),
                Action::UseAccount {
                    provider: row.id.clone(),
                    id: account.id.clone(),
                },
            ));
        }
        actions.push((
            "Use provider for Claude Code".into(),
            Action::UseProvider(row.id.clone()),
        ));
        if self.route.target() != Some(row.id.as_str()) {
            let mut fallback = self.route.fallback.clone();
            if let Some(at) = fallback.iter().position(|id| id == &row.id) {
                fallback.remove(at);
                actions.push(("Remove from fallback".into(), Action::Fallback(fallback)));
                if at > 0 {
                    let mut fallback = self.route.fallback.clone();
                    fallback.swap(at, at - 1);
                    actions.push((
                        "Try earlier in fallback order".into(),
                        Action::Fallback(fallback),
                    ));
                }
            } else if row.state != AuthState::Missing {
                fallback.push(row.id.clone());
                actions.push((
                    "Add to end of fallback order".into(),
                    Action::Fallback(fallback),
                ));
            }
        }
        let account = self.selected_account();
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
                // Drop an old read so it cannot replace the result of this action.
                self.pending = None;
                self.retry();
            }
            // A name that was refused stays in its field, with the reason beside it.
            Err(error) if self.rename.is_some() => self.refuse_name(format!("{error:#}")),
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
    /// next read, and hands its place as the saved choice to the first one left.
    pub fn forget_account(&mut self, provider: &str, id: &str) {
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
        self.selected = Entry::Provider(provider.to_string());
    }

    pub fn preview_action(&mut self, action: &Action) {
        match action {
            Action::StartProxy => {}
            Action::UseProvider(id) => {
                if id == "direct" {
                    self.route.on = false;
                } else {
                    self.route.fallback = route::promoted(&self.route, id);
                    self.route.on = true;
                    self.route.provider = Some(id.clone());
                }
            }
            Action::UseAccount { provider, id } => {
                if let Some(row) = self.rows.iter_mut().find(|row| &row.id == provider) {
                    for account in &mut row.accounts {
                        account.current = &account.id == id;
                    }
                }
            }
            Action::RemoveAccount { provider, id } => self.forget_account(provider, id),
            Action::RenameAccount { provider, id, name } => {
                if let Err(reason) = self.rename_account(provider, id, name) {
                    self.refuse_name(reason);
                    return;
                }
                self.rename = None;
            }
            Action::Fallback(ids) => self.route.fallback = ids.clone(),
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
        let name_color = if entry == &self.selected { TEAL } else { WHITE };
        let text = match entry {
            Entry::Direct => format!(
                "Disable proxy routing{}",
                if self.route.target().is_none() {
                    " [in use]"
                } else {
                    ""
                }
            ),
            Entry::Provider(id) => {
                let row = self.rows.iter().find(|row| &row.id == id).unwrap();
                let routed = self.route.target() == Some(id.as_str());
                let serving = self.serves(id);
                let name_style = if serving {
                    Style::default().fg(TEAL).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(name_color)
                };
                let role = if routed {
                    Span::styled(" [primary]", Style::default().fg(DIM_WHITE))
                } else if let Some(at) = self.route.fallback.iter().position(|name| name == id) {
                    Span::styled(
                        format!(" [fallback {}]", at + 1),
                        Style::default().fg(DIM_WHITE),
                    )
                } else if row.state == AuthState::Missing {
                    Span::styled("  not set up", Style::default().fg(DIM_WHITE))
                } else {
                    Span::raw("")
                };
                return ListItem::new(Line::from(vec![
                    Span::styled(ui::provider_name(id).to_string(), name_style),
                    role,
                ]));
            }
            Entry::Account(provider, id) => {
                let row = self.rows.iter().find(|row| &row.id == provider).unwrap();
                let account = row
                    .accounts
                    .iter()
                    .find(|account| &account.id == id)
                    .unwrap();
                let now = crate::rotation::now_ms();
                let serving = self.serves_account(provider, account);
                let first_reset = self.auto_rotate
                    && row.pinned.is_none()
                    && row
                        .first_reset(now)
                        .is_some_and(|(first, _)| first.id == account.id);
                let summary = self.row_summary(provider, account, now);
                let tags = account_tags(account, first_reset);
                let tags_width: usize = tags.iter().map(Span::width).sum();
                let marker = account_marker(account, serving);
                let name_style = account_name_style(account, serving, entry == &self.selected);
                let name_width =
                    usize::from(width).saturating_sub(4 + summary.width() + tags_width);
                if name_width < 8 {
                    let name_width = usize::from(width).saturating_sub(2 + tags_width);
                    let summaries = match &account.usage {
                        Some(MonitorUsage::Reported(windows))
                            if summary.width() + 4 > usize::from(width) =>
                        {
                            windows
                                .iter()
                                .map(|window| {
                                    MonitorUsage::Reported(vec![window.clone()])
                                        .compact_line(jiff::Timestamp::now())
                                })
                                .collect()
                        }
                        _ => vec![summary],
                    };
                    let mut title = vec![
                        marker,
                        Span::styled(super::ellipsize(&account.name, name_width), name_style),
                    ];
                    title.extend(tags);
                    let mut lines = vec![Line::from(title)];
                    lines.extend(summaries.into_iter().map(|summary| {
                        Line::from(
                            std::iter::once(Span::raw("    "))
                                .chain(summary.spans)
                                .collect::<Vec<_>>(),
                        )
                    }));
                    return ListItem::new(lines);
                }
                let mut spans = vec![
                    marker,
                    Span::styled(super::ellipsize(&account.name, name_width), name_style),
                    Span::raw("  "),
                ];
                spans.extend(summary.spans);
                spans.extend(tags);
                return ListItem::new(Line::from(spans));
            }
        };
        ListItem::new(Line::from(Span::styled(
            text,
            Style::default().fg(name_color),
        )))
    }

    fn detail_lines(&self) -> Vec<Line<'static>> {
        let mut text = Vec::new();
        match &self.selected {
            Entry::Direct => {
                text.push(
                    "Restore Claude Code's original settings and login, without the proxy.".into(),
                );
                text.push(
                    "The proxy keeps running. You can switch back to a provider here.".into(),
                );
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
                        text.push(
                            format!(
                                "{}{}",
                                if account.current { "● " } else { "" },
                                account.name
                            )
                            .into(),
                        );
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
                                            "Out of credit. cc-proxy tries this account again in {}. After you add credit, choose Use account for new requests to try it now.",
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
                            "New requests for this provider use the account you choose.".into(),
                        );
                    } else {
                        text.push(row.state.describe().into());
                        if let Some(usage) = &row.usage {
                            text.extend(
                                usage.lines(jiff::Timestamp::now(), &jiff::tz::TimeZone::system()),
                            );
                        }
                        text.push("".into());
                        text.push(if row.accounts.is_empty() {
                            "Press Enter, then Sign in to save a login.".into()
                        } else {
                            "Choose a saved account below this provider to check usage or switch accounts.".into()
                        });
                        if row.state != AuthState::Missing {
                            text.push("Use provider sends Claude Code here. Open windows follow within a few seconds.".into());
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
                    if row.all_out_of_credit(crate::rotation::now_ms()) {
                        text.push(Line::styled(
                            "Every saved account is out of credit. Add credit to one, then choose it and Use account for new requests.",
                            Style::default().fg(ui::remaining_color(0.0)),
                        ));
                    }
                    if self.auto_rotate
                        && row.pinned.is_none()
                        && let Some((account, until)) = row.first_reset(crate::rotation::now_ms())
                        && let Ok(until) = i64::try_from(until)
                        && let Ok(reset) = jiff::Timestamp::from_millisecond(until)
                    {
                        let reset = crate::usage::format_reset(
                            reset,
                            jiff::Timestamp::now(),
                            &jiff::tz::TimeZone::system(),
                        );
                        text.push(Line::styled(
                            format!("All accounts are waiting. {} {reset}.", account.name),
                            Style::default().fg(YELLOW),
                        ));
                        text.push("The next request rechecks each account's limits.".into());
                    }
                    if row.pinned.is_none()
                        && self
                            .selected_account()
                            .is_some_and(|account| account.current)
                    {
                        text.push(if self.auto_rotate {
                            "The ● marks the saved choice. Auto-rotate skips it while it is waiting."
                        } else {
                            "The ● marks the saved choice. Auto-rotate is off; choose another account to switch."
                        }.into());
                    }
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
        let fallback = if self.route.fallback.is_empty() {
            "none".into()
        } else {
            self.route
                .fallback
                .iter()
                .map(|id| ui::provider_name(id))
                .collect::<Vec<_>>()
                .join(" → ")
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
                Line::styled(
                    format!(
                        " Fallback: {fallback}{}",
                        if !self.auto_rotate {
                            " (account rotation off)"
                        } else if self.route.target().is_none() {
                            " (routing is off)"
                        } else {
                            ""
                        }
                    ),
                    Style::default().fg(DIM_WHITE),
                ),
                self.serving_line(),
            ]),
            summary,
        );
        let [list_area, detail_area] = if area.width >= 100 {
            Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)]).areas(body)
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
                    if self.pending.is_some() {
                        "Providers and accounts · refreshing"
                    } else {
                        "Providers and accounts"
                    },
                    !self.menu_open(),
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

/// Saved choices remain white. A request's actual account gets the teal bullet,
/// even if another request has since changed the saved choice.
fn account_marker(account: &AccountRow, serving: bool) -> Span<'static> {
    if !account.current && !serving {
        return Span::raw("  ");
    }
    let color = if serving { TEAL } else { WHITE };
    Span::styled(
        "● ",
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

fn account_name_style(account: &AccountRow, serving: bool, selected: bool) -> Style {
    let style = Style::default().fg(if selected || serving { TEAL } else { WHITE });
    if account.current || serving {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

/// What follows an account's numbers. The saved account has no tag: its bullet
/// says it, and Details spells it out. A `*` means the usage refresh failed;
/// Details says why.
fn account_tags(account: &AccountRow, first_reset: bool) -> Vec<Span<'static>> {
    let mut tags = Vec::new();
    if first_reset {
        tags.push(Span::styled(
            " [first reset]",
            Style::default().fg(DIM_WHITE),
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

fn load(send: &mpsc::Sender<Vec<ProviderRow>>) {
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
            if id == "opencode" && !row.accounts.iter().any(|account| account.current) {
                use sha2::{Digest, Sha256};
                row.config_key_hash =
                    config::opencode_api_key().map(|key| Sha256::digest(key.as_bytes()).into());
            }
            Some(row)
        })
        .collect();
    if send.send(rows.clone()).is_err() {
        return;
    }
    for (id, headline) in crate::usage::headlines() {
        if let Some(row) = rows.iter_mut().find(|row| row.id == id) {
            row.usage = Some(headline);
        }
    }
    apply_account_usage(&mut rows, &crate::usage::account_usage());
    keep_last_good(&mut rows, &crate::usage::last_good_usage());
    let _ = send.send(rows);
}

/// An account whose lookup failed keeps the numbers from its last good one,
/// even after the monitor was closed and opened again. Details says why it
/// failed.
fn keep_last_good(rows: &mut [ProviderRow], saved: &HashMap<(String, String), Vec<UsageWindow>>) {
    for row in rows {
        for account in &mut row.accounts {
            if let Some(MonitorUsage::Unavailable(reason)) = &account.usage
                && let Some(windows) = saved.get(&(row.id.clone(), account.id.clone()))
                && !windows.is_empty()
            {
                account.usage_error = Some(reason.clone());
                account.usage = Some(MonitorUsage::Reported(windows.clone()));
            }
        }
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
            account.usage = Some(lines);
        }
    }
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
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

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

    fn pending_refresh(panel: &mut ProvidersPanel) -> mpsc::Sender<Vec<ProviderRow>> {
        let (send, receive) = mpsc::channel();
        panel.mode = Mode::Local;
        panel.pending = Some(receive);
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
        send.send(metadata(demo_rows())).unwrap();
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
        send.send(rows).unwrap();
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
        send.send(rows).unwrap();
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
        send.send(metadata(demo_rows())).unwrap();
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
        panel.take_rows(rows);
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
        send.send(metadata(demo_rows())).unwrap();
        panel.refresh();
        panel.selected = Entry::Account("codex".into(), "demo-work".into());
        let text = render(&mut panel, 160, 30);
        assert!(text.contains("checking..."), "{text}");
        assert!(text.contains("Checking usage..."), "{text}");
        assert!(!text.contains('%'), "{text}");

        let mut rows = metadata(demo_rows());
        apply_account_usage(&mut rows, &HashMap::new());
        send.send(rows).unwrap();
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
        send.send(metadata(demo_rows())).unwrap();
        panel.refresh();
        assert!(matches!(
            panel.rows[0].accounts[1].usage,
            Some(MonitorUsage::Unavailable(_))
        ));
        send.send(demo_rows()).unwrap();
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
        send.send(rows.clone()).unwrap();
        panel.refresh();
        assert_eq!(panel.rows[0].accounts[0].usage, previous);
        assert_eq!(panel.rows[0].accounts[0].name, "renamed");

        // A new login gets a new UUID even when its saved name is unchanged.
        rows[0].accounts[0].id = "replacement".into();
        rows[0].accounts[1].name = "work".into();
        let mut other_provider = rows[0].clone();
        other_provider.id = "another-provider".into();
        rows.push(other_provider);
        send.send(rows).unwrap();
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
        // The cursor was on the saved account. A new login takes its place as
        // the saved account, so the cursor goes along to it.
        assert_eq!(
            panel.selected,
            Entry::Account("codex".into(), "replacement".into())
        );
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
        send.send(rows.clone()).unwrap();
        panel.refresh();
        assert_eq!(panel.rows[0].usage, previous);
        rows[0].usage = Some(MonitorUsage::Unavailable("Press r to retry.".into()));
        send.send(rows.clone()).unwrap();
        panel.refresh();
        assert_eq!(panel.rows[0].usage, previous);
        let text = render(&mut panel, 160, 30);
        for expected in ["5h 58% left", "Refresh failed", "Press r to retry"] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }

        rows[0].usage = None;
        rows[0].accounts[0].current = true;
        rows[0].accounts[1].current = false;
        send.send(rows.clone()).unwrap();
        panel.refresh();
        assert!(panel.rows[0].usage.is_none());
        assert!(panel.rows[0].usage_error.is_none());

        rows[0].accounts[0].id = "replacement".into();
        rows[0].accounts.remove(1);
        send.send(rows).unwrap();
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
            "[primary]",
            "[fallback 1]",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
    }

    #[test]
    fn enter_opens_actions_and_escape_returns_without_a_change() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        panel.selected = Entry::Account("codex".into(), "demo-home".into());
        assert_eq!(panel.press(KeyCode::Enter), None);
        assert!(render(&mut panel, 110, 25).contains("Use account for new requests"));
        assert!(panel.close_menu());
        assert!(!panel.rows[0].accounts[0].current);
        panel.press(KeyCode::Enter);
        let action = panel.press(KeyCode::Enter).unwrap();
        assert_eq!(
            action,
            Action::UseAccount {
                provider: "codex".into(),
                id: "demo-home".into()
            }
        );
        panel.preview_action(&action);
        assert!(panel.rows[0].accounts[0].current);
        assert_eq!(panel.route.target(), Some("codex"));
        assert!(render(&mut panel, 110, 25).contains("Nothing on this computer changed"));
    }

    #[test]
    fn removal_is_scoped_to_a_saved_login_and_demo_hands_over_the_current_account() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
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
        assert_eq!(panel.selected, Entry::Provider("codex".into()));
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
    fn the_provider_and_account_that_answered_are_highlighted_and_followed() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        // The cursor sits on the saved account of the primary provider.
        panel.selected = Entry::Account("codex".into(), "demo-work".into());
        // Codex gave up and Kimi answered: that is who is serving now.
        panel.observe(&served("kimi", "demo-kimi", EndpointKind::Messages));
        assert_eq!(
            panel.selected,
            Entry::Account("kimi".into(), "demo-kimi".into())
        );
        let text = render(&mut panel, 160, 30);
        assert!(text.contains("Serving: Kimi / name"), "{text}");
        let mut terminal = Terminal::new(TestBackend::new(160, 30)).unwrap();
        terminal
            .draw(|frame| panel.render(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let at = |needle: &str| {
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
        // Codex is still the first in the route, but only Kimi is teal.
        let (x, y) = at("Kimi [fallback 1]");
        assert_eq!(buffer[(x, y)].fg, TEAL);
        let (x, y) = at("Codex [primary]");
        assert_ne!(buffer[(x, y)].fg, TEAL);
        // Codex's saved account keeps its bullet, now in white.
        let (x, y) = at("● work");
        assert_eq!(buffer[(x, y)].fg, WHITE);
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
        assert!(panel.serving.is_none());
        assert!(render(&mut panel, 160, 30).contains("No request served yet in this run."));
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
    fn rename_says_so_when_the_name_is_empty_or_unchanged_and_never_for_a_provider() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
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
    fn the_account_serving_requests_stands_out_from_across_the_list() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
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
        // Codex is the routed provider and `work` is its saved choice: a
        // bullet and a bold teal name, with no second tag saying the same.
        let (x, y) = find("● work");
        assert_eq!(buffer[(x, y)].fg, TEAL);
        assert!(buffer[(x, y)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(x + 2, y)].fg, TEAL);
        let (provider_x, provider_y) = find("Codex [primary]");
        assert_eq!(buffer[(provider_x, provider_y)].fg, TEAL);
        assert!(
            buffer[(provider_x, provider_y)]
                .modifier
                .contains(Modifier::BOLD)
        );
        // Another provider's saved choice is still marked, in white. An account
        // that is not the saved choice has no bullet.
        let (kimi_x, kimi_y) = find("● personal");
        assert_eq!(buffer[(kimi_x, kimi_y)].fg, WHITE);
        let (home_x, home_y) = find("home");
        assert_eq!(buffer[(home_x - 2, home_y)].symbol(), " ");
        assert!(!buffer[(home_x, home_y)].modifier.contains(Modifier::BOLD));
        // The route's order tags stay quiet: teal belongs to who serves.
        let (primary_x, primary_y) = find("[primary]");
        assert_eq!(buffer[(primary_x, primary_y)].fg, DIM_WHITE);
        let (fallback_x, fallback_y) = find("[fallback 1]");
        assert_eq!(buffer[(fallback_x, fallback_y)].fg, DIM_WHITE);
        // Narrow terminals keep the bullet on the name's line.
        for (width, height) in [(80, 24), (40, 24)] {
            let text = render(&mut panel, width, height);
            assert!(text.contains("● work"), "{width}: {text}");
            assert!(!text.contains("[current]"), "{width}: {text}");
        }
        // A failed refresh puts a yellow * on the row. Details say what failed.
        panel.rows[0].accounts[1].usage_error = Some("Press r to retry.".into());
        let mut terminal = Terminal::new(TestBackend::new(160, 30)).unwrap();
        terminal
            .draw(|frame| panel.render(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let list_row = (0..64).map(|x| buffer[(x, y)].symbol()).collect::<String>();
        let at = list_row.find('*').unwrap();
        assert_eq!(
            buffer[(list_row[..at].chars().count() as u16, y)].fg,
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
    fn choosing_a_provider_puts_the_old_primary_first_among_the_fallbacks() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        panel.route = config::Route {
            on: true,
            provider: Some("codex".into()),
            fallback: vec!["kimi".into(), "glm".into()],
            ..Default::default()
        };
        panel.preview_action(&Action::UseProvider("glm".into()));
        assert_eq!(panel.route.provider.as_deref(), Some("glm"));
        assert_eq!(panel.route.fallback, ["codex", "kimi"]);
    }

    #[test]
    fn the_cursor_follows_the_account_rotation_picks_only_while_it_sits_on_it() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        panel.route = config::Route {
            on: true,
            provider: Some("codex".into()),
            ..Default::default()
        };
        let rotated = || {
            let mut rows = demo_rows();
            rows[0].accounts[0].current = true;
            rows[0].accounts[1].current = false;
            rows
        };
        let home = Entry::Account("codex".into(), "demo-home".into());
        let work = Entry::Account("codex".into(), "demo-work".into());

        panel.selected = work.clone();
        panel.take_rows(rotated());
        assert_eq!(panel.selected, home);

        // Somewhere else, or inside a menu, the cursor stays put.
        panel.selected = Entry::Provider("kimi".into());
        panel.take_rows(demo_rows());
        assert_eq!(panel.selected, Entry::Provider("kimi".into()));
        panel.selected = work.clone();
        panel.menu = Some(panel.actions());
        panel.take_rows(rotated());
        assert_eq!(panel.selected, work);
    }

    #[test]
    fn only_actions_that_may_ask_a_question_leave_the_monitor() {
        let account = |action: fn(String, String) -> Action| action("codex".into(), "id".into());
        assert!(!account(|provider, id| Action::UseAccount { provider, id }).needs_terminal());
        assert!(!account(|provider, id| Action::RemoveAccount { provider, id }).needs_terminal());
        assert!(!Action::Fallback(vec!["kimi".into()]).needs_terminal());
        // Going back to Claude Code's own login never asks anything.
        assert!(!Action::UseProvider("direct".into()).needs_terminal());
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
        assert!(
            !panel
                .actions()
                .iter()
                .any(|(_, action)| matches!(action, Action::Fallback(_)))
        );
        panel.selected = Entry::Provider("kimi".into());
        assert!(
            panel
                .actions()
                .iter()
                .any(|(label, _)| label == "Remove from fallback")
        );
        panel.selected = Entry::Provider("codex".into());
        assert!(
            !panel
                .actions()
                .iter()
                .any(|(_, action)| matches!(action, Action::Fallback(_)))
        );
        panel.rows[0].pinned = Some("CODEX_TOKEN".into());
        panel.selected = Entry::Account("codex".into(), "demo-home".into());
        assert!(
            !panel
                .actions()
                .iter()
                .any(|(_, action)| matches!(action, Action::UseAccount { .. }))
        );
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
    fn waiting_current_is_not_presented_as_the_first_account_to_reset() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        let now = crate::rotation::now_ms();
        panel.rows[0].accounts[0].waits = vec![(None, now + 3_600_000)];
        panel.rows[0].accounts[1].waits = vec![(None, now + 7_200_000)];
        panel.selected = Entry::Provider("codex".into());
        let text = render(&mut panel, 160, 30);
        for expected in [
            "0% ",
            "[first reset]",
            "● work",
            "All accounts are waiting. home resets in",
            "next request",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        assert!(
            panel.rows[0].accounts[1].current,
            "Rendering must not change the saved account"
        );
        panel.rows[0].accounts[0].waits.clear();
        assert!(!render(&mut panel, 160, 30).contains("All accounts are waiting"));
    }

    #[test]
    fn model_specific_waits_never_claim_every_model_is_waiting() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        for account in &mut panel.rows[0].accounts {
            account.waits = vec![(Some("one-model".into()), crate::rotation::now_ms() + 60_000)];
        }
        assert!(
            panel.rows[0]
                .first_reset(crate::rotation::now_ms())
                .is_none()
        );
        assert!(!render(&mut panel, 160, 30).contains("All accounts are waiting"));
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
        assert!(render(&mut panel, 40, 13).contains("saved choice"));
        panel.press(KeyCode::Enter);
        assert!(render(&mut panel, 40, 13).contains("Use account for new requests"));
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
        assert!(wide.contains("choose Use account for new requests to try it now"));
        assert!(!wide.contains("At its limit"), "{wide}");

        // The provider row says what to do once every account is spent.
        panel.selected = Entry::Provider("codex".into());
        let provider = render(&mut panel, 160, 30);
        assert!(
            provider.contains("Every saved account is out of credit"),
            "{provider}"
        );
        assert!(!provider.contains("All accounts are waiting"), "{provider}");
    }

    #[test]
    fn a_spent_account_keeps_its_quota_windows_and_a_real_reset_still_counts_as_first() {
        let now = crate::rotation::now_ms();
        let mut rows = demo_rows();
        let codex = &mut rows[0];
        // Windows stay beside the note: a wait never hides measured quota.
        codex.accounts[1].waits = vec![(None, now + 1_800_000)];
        codex.accounts[1].spent = true;
        let summary = codex.accounts[1].usage_summary(now).to_string();
        assert!(summary.starts_with("5h 58% · week 88% · out of credit · tries again in"));

        // One account out of credit and one with a real reset: the real one is first.
        codex.accounts[0].waits = vec![(None, now + 3_600_000)];
        let (first, until) = codex.first_reset(now).unwrap();
        assert_eq!((first.id.as_str(), until), ("demo-home", now + 3_600_000));
        // All accounts out of credit: nobody resets, so nobody is first.
        codex.accounts[0].spent = true;
        assert!(codex.first_reset(now).is_none());
        assert!(codex.all_out_of_credit(now));
        // A wait that ended is not out of credit any more.
        assert!(!codex.all_out_of_credit(now + 3_600_001));
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
    fn providers_follow_route_then_connection_and_alphabetical_order() {
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
        let providers = |panel: &ProvidersPanel| {
            panel
                .entries()
                .into_iter()
                .filter_map(|entry| match entry {
                    Entry::Provider(id) => Some(id),
                    Entry::Direct => Some("direct".into()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            providers(&panel),
            [
                "codex",
                "kimi",
                "anthropic",
                "gemini",
                "cursor",
                "grok",
                "opencode-zen",
                "direct"
            ]
        );
        panel.route.provider = Some("gemini".into());
        panel.route.fallback = vec!["kimi".into(), "codex".into()];
        assert_eq!(
            providers(&panel),
            [
                "gemini",
                "kimi",
                "codex",
                "anthropic",
                "cursor",
                "grok",
                "opencode-zen",
                "direct"
            ]
        );
        panel.route.on = false;
        assert_eq!(providers(&panel).last().unwrap(), "direct");
    }

    #[test]
    fn claude_subscription_follows_the_route_and_keeps_its_own_accounts() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        let mut claude = panel.rows[0].clone();
        claude.id = "claude".into();
        panel.rows.push(claude);
        let providers = |panel: &ProvidersPanel| {
            panel
                .entries()
                .into_iter()
                .filter_map(|entry| {
                    if let Entry::Provider(id) = entry {
                        Some(id)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            providers(&panel),
            ["codex", "kimi", "claude", "opencode-zen"]
        );
        panel.route.fallback = vec!["claude".into(), "kimi".into()];
        assert_eq!(
            providers(&panel),
            ["codex", "claude", "kimi", "opencode-zen"]
        );
        panel.selected = Entry::Account("claude".into(), "demo-work".into());
        assert!(render(&mut panel, 160, 35).contains("5h 58% left"));
        panel.preview_action(&Action::UseProvider("claude".into()));
        assert_eq!(panel.route.target(), Some("claude"));
        assert_eq!(
            providers(&panel),
            ["claude", "codex", "kimi", "opencode-zen"]
        );
        panel.selected = Entry::Direct;
        assert_eq!(panel.actions()[0].1, Action::UseProvider("direct".into()));
        panel.preview_action(&Action::UseProvider("direct".into()));
        assert!(panel.route.target().is_none());
        assert_eq!(panel.route.provider.as_deref(), Some("claude"));
        let text = render(&mut panel, 160, 35);
        assert!(text.contains("Disable proxy routing [in use]"), "{text}");
        assert!(text.contains("original settings and login"), "{text}");
        panel.press(KeyCode::Home);
        assert_eq!(panel.selected, Entry::Provider("codex".into()));
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
