//! Local saved accounts and usage. Network lookups run only on the panel's worker.

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossterm::event::KeyCode;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph, Wrap},
};

use super::controls::Action;
use super::{BG, DIM_WHITE, GREEN, SELECTED_BG, TEAL, WHITE, YELLOW, panel};
use crate::accounts::Accounts;
use crate::provider::AuthState;
use crate::registry::Registry;
use crate::usage::{MonitorUsage, UsageWindow};
use crate::{config, rotation::Cooldowns, ui};

const REFRESH_AFTER: Duration = Duration::from_secs(60);

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
}

impl AccountRow {
    fn waiting_until(&self, now: u64) -> Option<u64> {
        self.waits
            .iter()
            .filter(|(model, until)| model.is_none() && *until > now)
            .map(|(_, until)| *until)
            .max()
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
            && !reset_is_shown
        {
            if self.usage.is_none() || matches!(self.usage, Some(MonitorUsage::Unavailable(_))) {
                summary = Line::default();
            } else {
                summary.spans.push(Span::raw(" · "));
            }
            summary.spans.extend([
                Span::styled("waiting", Style::default().fg(YELLOW)),
                Span::raw(format!(
                    " · {} left",
                    ui::duration(Duration::from_millis(until - now))
                )),
            ]);
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
                account.usage_error =
                    retain_previous_usage(&mut account.usage, &old.usage, &old.usage_error);
            }
        }
    }

    fn first_reset(&self, now: u64) -> Option<(&AccountRow, u64)> {
        self.accounts
            .iter()
            .map(|account| account.waiting_until(now).map(|until| (account, until)))
            .collect::<Option<Vec<_>>>()?
            .into_iter()
            .min_by_key(|(_, until)| *until)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Entry {
    Rotate,
    Direct,
    Provider(String),
    Account(String, String),
}

pub(super) struct ProvidersPanel {
    mode: Mode,
    rows: Vec<ProviderRow>,
    route: config::Route,
    auto_rotate: bool,
    loaded_at: Option<Instant>,
    pending: Option<mpsc::Receiver<Vec<ProviderRow>>>,
    selected: Entry,
    list: ListState,
    menu: Option<Vec<(String, Action)>>,
    menu_state: ListState,
    notice: Option<(String, bool)>,
    detail_scroll: u16,
}

impl ProvidersPanel {
    pub fn new(mode: Mode) -> Self {
        let mut panel = Self {
            mode,
            rows: Vec::new(),
            route: config::Route::default(),
            auto_rotate: false,
            loaded_at: None,
            pending: None,
            selected: Entry::Rotate,
            list: ListState::default(),
            menu: None,
            menu_state: ListState::default(),
            notice: None,
            detail_scroll: 0,
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
            loop {
                match pending.try_recv() {
                    Ok(mut rows) => {
                        for row in &mut rows {
                            if let Some(previous) = self.rows.iter().find(|old| old.id == row.id) {
                                row.retain_usage(previous);
                            }
                        }
                        if self.rows.is_empty() {
                            self.selected = rows
                                .iter()
                                .find(|row| self.route.target() == Some(row.id.as_str()))
                                .map(|row| {
                                    row.accounts
                                        .iter()
                                        .find(|account| account.current)
                                        .map(|account| {
                                            Entry::Account(row.id.clone(), account.id.clone())
                                        })
                                        .unwrap_or_else(|| Entry::Provider(row.id.clone()))
                                })
                                .unwrap_or(Entry::Rotate);
                        }
                        self.rows = rows;
                    }
                    Err(mpsc::TryRecvError::Empty) => return,
                    Err(mpsc::TryRecvError::Disconnected) => break,
                }
            }
            self.pending = None;
            self.loaded_at = Some(Instant::now());
            if !self.entries().contains(&self.selected) {
                self.selected = Entry::Rotate;
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

    pub fn retry(&mut self) {
        self.loaded_at = None;
        self.refresh();
    }

    fn entries(&self) -> Vec<Entry> {
        let mut entries = vec![Entry::Direct];
        let mut rows: Vec<_> = self.rows.iter().collect();
        rows.sort_by_cached_key(|row| {
            let role = if row.id == "claude" {
                (0, 0)
            } else if self.route.target() == Some(row.id.as_str()) {
                (1, 0)
            } else if let Some(at) = self.route.fallback.iter().position(|id| id == &row.id) {
                (2, at)
            } else if row.state != AuthState::Missing {
                (3, 0)
            } else {
                (4, 0)
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
        entries.push(Entry::Rotate);
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

    pub fn close_menu(&mut self) -> bool {
        self.menu.take().is_some()
    }

    pub fn menu_open(&self) -> bool {
        self.menu.is_some()
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
            KeyCode::Home if self.menu.is_none() => self.selected = Entry::Direct,
            KeyCode::End if self.menu.is_none() => {
                self.selected = self.entries().last().unwrap().clone()
            }
            KeyCode::Char('r') => self.retry(),
            KeyCode::Enter => {
                if let Some(menu) = &self.menu {
                    return menu
                        .get(self.menu_state.selected().unwrap_or(0))
                        .map(|(_, action)| action.clone());
                }
                self.menu = Some(self.actions());
                self.menu_state.select(Some(0));
                self.detail_scroll = 0;
            }
            _ => {}
        }
        None
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
        match &self.selected {
            Entry::Rotate => {
                return vec![(
                    format!(
                        "Turn {} auto-rotate",
                        if self.auto_rotate { "off" } else { "on" }
                    ),
                    Action::Rotate(!self.auto_rotate),
                )];
            }
            Entry::Direct => {
                return vec![(
                    "Disable proxy routing".into(),
                    Action::UseProvider("direct".into()),
                )];
            }
            _ => {}
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
        actions
    }

    pub fn finish_action(&mut self, result: anyhow::Result<String>) {
        match result {
            Ok(message) => {
                self.notice = Some((message, false));
                self.menu = None;
                // Drop an old read so it cannot replace the result of this action.
                self.pending = None;
                self.retry();
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

    pub fn preview_action(&mut self, action: &Action) {
        match action {
            Action::StartProxy => {}
            Action::UseProvider(id) => {
                if id == "direct" {
                    self.route.on = false;
                } else {
                    self.route.on = true;
                    self.route.provider = Some(id.clone());
                    self.route.fallback.retain(|name| name != id);
                }
            }
            Action::UseAccount { provider, id } => {
                if let Some(row) = self.rows.iter_mut().find(|row| &row.id == provider) {
                    for account in &mut row.accounts {
                        account.current = &account.id == id;
                    }
                }
            }
            Action::Rotate(on) => self.auto_rotate = *on,
            Action::Fallback(ids) => self.route.fallback = ids.clone(),
            Action::SignIn { .. } => {
                self.notice = Some((
                    "Demo only. Run cc-proxy monitor to sign in to a real account.".into(),
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
            Entry::Rotate => format!(
                "Auto-rotate  {}",
                if self.auto_rotate { "on" } else { "off" }
            ),
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
                let role = if self.route.target() == Some(id.as_str()) {
                    " [primary]".into()
                } else if let Some(at) = self.route.fallback.iter().position(|name| name == id) {
                    format!(" [fallback {}]", at + 1)
                } else if row.state == AuthState::Missing {
                    "  not set up".into()
                } else {
                    String::new()
                };
                format!("{}{role}", ui::provider_name(id))
            }
            Entry::Account(provider, id) => {
                let row = self.rows.iter().find(|row| &row.id == provider).unwrap();
                let account = row
                    .accounts
                    .iter()
                    .find(|account| &account.id == id)
                    .unwrap();
                let now = crate::rotation::now_ms();
                let current = if account.current { " [current]" } else { "" };
                let first = if self.auto_rotate
                    && row.pinned.is_none()
                    && row
                        .first_reset(now)
                        .is_some_and(|(first, _)| first.id == account.id)
                {
                    " [first reset]"
                } else {
                    ""
                };
                let summary = account.usage_summary(now);
                let stale = if account.usage_error.is_some() {
                    " [stale]"
                } else {
                    ""
                };
                let status = format!("{current}{first}{stale}");
                let name_width =
                    usize::from(width).saturating_sub(4 + summary.width() + status.len());
                if name_width < 8 {
                    let name_width = usize::from(width).saturating_sub(2 + status.len());
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
                    let mut lines = vec![Line::styled(
                        format!("  {}{status}", super::ellipsize(&account.name, name_width)),
                        Style::default().fg(name_color),
                    )];
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
                    Span::styled(
                        format!("  {}", super::ellipsize(&account.name, name_width)),
                        Style::default().fg(name_color),
                    ),
                    Span::raw("  "),
                ];
                spans.extend(summary.spans);
                spans.push(Span::styled(status, Style::default().fg(DIM_WHITE)));
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
            Entry::Rotate => {
                text.push("Auto-rotate switches saved accounts after a provider confirms a limit and a reset time.".into());
                text.push("If every account is spent, it tries your fallback providers in order. It does not switch after output starts.".into());
                text.push(
                    "Off by default. Changes apply to new requests without a restart.".into(),
                );
            }
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
                                account.name,
                                if account.current { " [current]" } else { "" }
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
                        text.push(Line::styled(
                            format!("Refresh failed. Last values may be out of date. {error}"),
                            Style::default().fg(YELLOW),
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
                            "[current] is the saved choice. Auto-rotate skips it while it is waiting."
                        } else {
                            "[current] is the saved choice. Auto-rotate is off; choose another account to switch."
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
            frame.render_widget(Paragraph::new("Accounts are managed on the computer running the proxy. This --url connection is read-only.\n\nOpen cc-proxy monitor on that computer to choose providers, accounts or rotation. Press 2 for activity.")
                .style(Style::default().fg(DIM_WHITE)).wrap(Wrap { trim: false }).block(panel("Remote proxy", false)), area);
            return;
        }
        let [summary, body] =
            Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(area);
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
                    self.menu.is_none(),
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
            if self.menu.is_some() {
                "Choose an action"
            } else {
                "Details"
            },
            self.menu.is_some(),
        );
        let inner = detail.inner(detail_area);
        frame.render_widget(detail, detail_area);
        if let Some(menu) = &self.menu {
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
        Ok(AccountRow { current: current.as_ref() == Some(&account.id), waits, id: account.id, name: account.name, state, usage: None, usage_error: None })
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
    let _ = send.send(rows);
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
                    _ => MonitorUsage::NotReported("This provider does not report usage. Confirmed wait timers still appear here."),
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
                account.waits.retain(|(model, _)| model.is_some());
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
        for old in ["58%", "88%", "[stale]", "refreshing"] {
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
            "[stale]",
            "resets in 2h",
            "Refresh failed",
            "Press r to retry",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        assert!(!text.to_ascii_lowercase().contains("checking"), "{text}");
        assert!(!text.contains("refreshing"), "{text}");

        let send = pending_refresh(&mut panel);
        panel.press(KeyCode::Char('r'));
        send.send(metadata(demo_rows())).unwrap();
        panel.refresh();
        assert_eq!(panel.rows[0].accounts[1].usage, previous);
        let text = render(&mut panel, 160, 30);
        for expected in ["refreshing", "[stale]", "58%", "Press r to retry"] {
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
        for absent in ["checking...", "Checking usage...", "[stale]", "%"] {
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
        assert_eq!(panel.selected, Entry::Rotate);
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
    fn navigation_keeps_selection_visible_and_actions_are_scoped() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        for _ in 0..12 {
            panel.press(KeyCode::Down);
        }
        assert_eq!(panel.selected, Entry::Rotate);
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
            "[current]",
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
                "direct",
                "codex",
                "kimi",
                "anthropic",
                "gemini",
                "cursor",
                "grok",
                "opencode-zen"
            ]
        );
        panel.route.provider = Some("gemini".into());
        panel.route.fallback = vec!["kimi".into(), "codex".into()];
        assert_eq!(
            providers(&panel),
            [
                "direct",
                "gemini",
                "kimi",
                "codex",
                "anthropic",
                "cursor",
                "grok",
                "opencode-zen"
            ]
        );
        panel.route.on = false;
        assert_eq!(providers(&panel)[0], "direct");
    }

    #[test]
    fn claude_subscription_is_pinned_after_direct_and_keeps_its_own_accounts() {
        let mut panel = ProvidersPanel::new(Mode::Demo);
        let mut claude = panel.rows[0].clone();
        claude.id = "claude".into();
        panel.rows.push(claude);
        assert_eq!(
            &panel.entries()[..2],
            &[Entry::Direct, Entry::Provider("claude".into())]
        );
        panel.selected = Entry::Account("claude".into(), "demo-work".into());
        assert!(render(&mut panel, 160, 35).contains("5h 58% left"));
        panel.preview_action(&Action::UseProvider("claude".into()));
        assert_eq!(panel.route.target(), Some("claude"));
        panel.selected = Entry::Direct;
        assert_eq!(panel.actions()[0].1, Action::UseProvider("direct".into()));
        panel.preview_action(&Action::UseProvider("direct".into()));
        assert!(panel.route.target().is_none());
        assert_eq!(panel.route.provider.as_deref(), Some("claude"));
        let text = render(&mut panel, 160, 35);
        assert!(text.contains("Disable proxy routing [in use]"), "{text}");
        assert!(text.contains("original settings and login"), "{text}");
        panel.press(KeyCode::Home);
        assert_eq!(panel.selected, Entry::Direct);
        panel.press(KeyCode::End);
        assert_eq!(panel.selected, Entry::Rotate);
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
