//! Local saved accounts and usage. Network lookups run only on the panel's worker.

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
};

use super::{BG, DIM, DIM_WHITE, GREEN, TEAL, WHITE, YELLOW};
use crate::accounts::Accounts;
use crate::provider::AuthState;
use crate::registry::Registry;
use crate::{rotation::Cooldowns, ui};

const REFRESH_AFTER: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
struct AccountRow {
    id: String,
    name: String,
    current: bool,
    state: String,
    usage: Option<Vec<String>>,
    waits: Vec<(Option<String>, u64)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRow {
    pub id: String,
    pub state: AuthState,
    pub usage: Option<String>,
    accounts: Vec<AccountRow>,
    error: Option<String>,
}

#[derive(Default)]
pub struct ProvidersPanel {
    rows: Vec<ProviderRow>,
    loaded_at: Option<Instant>,
    pending: Option<mpsc::Receiver<Vec<ProviderRow>>>,
    scroll: u16,
}

impl ProvidersPanel {
    pub fn refresh(&mut self) {
        if let Some(pending) = &self.pending {
            loop {
                match pending.try_recv() {
                    Ok(rows) => self.rows = rows,
                    Err(mpsc::TryRecvError::Empty) => return,
                    Err(mpsc::TryRecvError::Disconnected) => break,
                }
            }
            self.pending = None;
            self.loaded_at = Some(Instant::now());
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

    pub fn scroll(&mut self, down: bool) {
        self.scroll = if down {
            self.scroll.saturating_add(1)
        } else {
            self.scroll.saturating_sub(1)
        };
    }
}

fn account_rows(
    id: &str,
    support: &dyn crate::accounts::AccountSupport,
) -> anyhow::Result<Vec<AccountRow>> {
    let accounts = Accounts::open(id, support)?;
    let current = accounts.current()?.map(|account| account.id);
    accounts
        .list()?
        .into_iter()
        .map(|account| {
            let mut state = match accounts.login(&account.name) {
                Ok(Some(login)) => support.state(&login).describe(),
                Ok(None) => format!(
                    "No saved login. Run cc-proxy login {id} --replace --account {}",
                    account.name
                ),
                Err(error) => format!("Can't read the login: {error:#}. Run cc-proxy login {id} --replace --account {}", account.name),
            };
            let waits = match Cooldowns::new().waits(id, &account.id) {
                Ok(waits) => waits,
                Err(error) => {
                    state.push_str(&format!(". Can't read the wait timer: {error:#}"));
                    Vec::new()
                }
            };
            Ok(AccountRow {
                current: current.as_ref() == Some(&account.id),
                waits,
                id: account.id,
                name: account.name,
                state,
                usage: None,
            })
        })
        .collect()
}

fn load(send: &mpsc::Sender<Vec<ProviderRow>>) {
    let registry = Registry::new();
    let mut rows: Vec<ProviderRow> = registry
        .list_provider_names()
        .into_iter()
        .filter_map(|id| {
            let provider = registry.provider(&id)?;
            let cli = provider.cli();
            let mut row = ProviderRow {
                state: cli.auth_state(),
                id,
                usage: None,
                accounts: Vec::new(),
                error: None,
            };
            if let Some(support) = cli.accounts() {
                match account_rows(&row.id, support) {
                    Ok(accounts) => row.accounts = accounts,
                    Err(error) => {
                        row.error = Some(format!(
                            "Can't read accounts: {error:#}. Run cc-proxy auth status {}",
                            row.id
                        ))
                    }
                }
            }
            Some(row)
        })
        .collect();
    let route = crate::config::route();
    rows.sort_by_key(|row| {
        (
            route.target() != Some(row.id.as_str()),
            row.accounts.is_empty(),
        )
    });
    if send.send(rows.clone()).is_err() {
        return;
    }
    apply_account_usage(&mut rows, &crate::usage::account_usage_lines());
    for (id, headline) in crate::usage::headlines() {
        if let Some(row) = rows.iter_mut().find(|row| row.id == id) {
            row.usage = Some(headline);
        }
    }
    let _ = send.send(rows);
}

fn apply_account_usage(rows: &mut [ProviderRow], usage: &HashMap<(String, String), Vec<String>>) {
    for row in rows {
        for account in &mut row.accounts {
            account.usage = Some(
                usage
                    .get(&(row.id.clone(), account.id.clone()))
                    .cloned()
                    .unwrap_or_else(|| {
                        vec![format!("usage unavailable; run cc-proxy usage {}", row.id)]
                    }),
            );
        }
    }
}

fn lines(panel: &ProvidersPanel, now: u64) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if panel.rows.is_empty() {
        lines.push(Line::from("Checking local saved accounts..."));
    }
    lines.push(Line::from(Span::styled(
        "cc-proxy use <provider> switches provider; auth add saves another login.",
        Style::default().fg(DIM_WHITE),
    )));
    let route = crate::config::route();
    if let Some(provider) = route.target() {
        lines.push(Line::from(format!(
            "Claude names use {}. Auto-rotate is {}.",
            ui::provider_name(provider),
            if crate::config::auth_auto_rotate() {
                "on"
            } else {
                "off"
            }
        )));
    }
    for row in &panel.rows {
        let set_up = row.state != AuthState::Missing;
        lines.push(Line::from(vec![
            Span::styled(
                if set_up { "● " } else { "○ " },
                Style::default().fg(if set_up { GREEN } else { DIM }),
            ),
            Span::styled(
                ui::provider_name(&row.id).to_string(),
                Style::default().fg(WHITE),
            ),
            Span::styled(
                if row.accounts.is_empty() {
                    format!("  {}", row.state.describe())
                } else {
                    String::new()
                },
                Style::default().fg(DIM_WHITE),
            ),
        ]));
        if let Some(error) = &row.error {
            lines.push(Line::from(Span::styled(
                error.clone(),
                Style::default().fg(YELLOW),
            )));
        }
        for account in &row.accounts {
            lines.push(Line::from(vec![
                Span::styled(
                    format!(
                        "  {}{}",
                        account.name,
                        if account.current { " [current]" } else { "" }
                    ),
                    Style::default().fg(if account.current { TEAL } else { WHITE }),
                ),
                Span::styled(
                    format!("  {}", account.state),
                    Style::default().fg(DIM_WHITE),
                ),
            ]));
            match &account.usage {
                Some(usage) => {
                    for line in usage {
                        lines.push(Line::from(Span::styled(
                            format!("    {line}"),
                            Style::default().fg(DIM_WHITE),
                        )));
                    }
                }
                None => lines.push(Line::from(Span::styled(
                    "    Checking usage...",
                    Style::default().fg(DIM_WHITE),
                ))),
            }
            for (model, until) in &account.waits {
                if *until <= now {
                    continue;
                }
                let scope = model
                    .as_ref()
                    .map_or(String::new(), |model| format!(" for {model}"));
                let wait = ui::duration(Duration::from_millis(until - now));
                lines.push(Line::from(Span::styled(
                    format!("    At its limit{scope}. Retry in {wait}."),
                    Style::default().fg(YELLOW),
                )));
            }
        }
        if let Some(usage) = &row.usage {
            lines.push(Line::from(format!("  {usage}")));
        }
        if !row.accounts.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("  Switch: cc-proxy auth use {} <account>", row.id),
                Style::default().fg(DIM_WHITE),
            )));
            lines.push(Line::from(Span::styled(
                format!("  Add: cc-proxy auth add {}", row.id),
                Style::default().fg(DIM_WHITE),
            )));
        }
    }
    lines
}

pub fn render_overlay(frame: &mut ratatui::Frame<'_>, area: Rect, panel: &ProvidersPanel) {
    let width = area.width.saturating_sub(2).min(110);
    let height = area.height.saturating_sub(2);
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + 1,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(" Local providers and accounts ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(TEAL))
        .style(Style::default().bg(BG));
    let mut inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height == 0 {
        return;
    }
    let footer = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
    inner.height = inner.height.saturating_sub(1);
    let text = Paragraph::new(lines(panel, crate::rotation::now_ms())).wrap(Wrap { trim: false });
    let max_scroll = text
        .line_count(inner.width)
        .saturating_sub(usize::from(inner.height));
    let scroll = usize::from(panel.scroll)
        .min(max_scroll)
        .min(usize::from(u16::MAX)) as u16;
    frame.render_widget(
        text.scroll((scroll, 0)).style(Style::default().bg(BG)),
        inner,
    );
    frame.render_widget(
        Paragraph::new(if panel.pending.is_some() {
            "Refreshing usage...  Up/Down scroll | Esc close"
        } else {
            "Up/Down scroll | r refresh | Esc close"
        })
        .style(Style::default().fg(TEAL)),
        footer,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn render(panel: &ProvidersPanel, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render_overlay(frame, frame.area(), panel))
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

    fn fixture() -> ProvidersPanel {
        ProvidersPanel {
            rows: vec![ProviderRow {
                id: "codex".into(),
                state: AuthState::SignedIn {
                    account: None,
                    expires_ms: None,
                },
                usage: None,
                error: None,
                accounts: vec![
                    AccountRow {
                        id: "old".into(),
                        name: "home".into(),
                        current: false,
                        state: "signed in".into(),
                        usage: Some(vec!["5h 0% left, resets in 1h".into()]),
                        waits: vec![(None, crate::rotation::now_ms() + 60_000)],
                    },
                    AccountRow {
                        id: "new".into(),
                        name: "work".into(),
                        current: true,
                        state: "signed in".into(),
                        usage: Some(vec![
                            "5h 58% left, resets in 2h".into(),
                            "week 88% left, resets in 4d".into(),
                        ]),
                        waits: Vec::new(),
                    },
                ],
            }],
            ..ProvidersPanel::default()
        }
    }

    #[test]
    fn accounts_show_current_usage_and_waits_without_color_alone() {
        let text = render(&fixture(), 110, 25);
        for expected in [
            "Local providers and accounts",
            "home",
            "work [current]",
            "5h 58% left",
            "week 88% left",
            "At its limit",
            "Retry in",
            "auth use codex",
            "r refresh",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
    }

    #[test]
    fn usage_is_joined_by_immutable_id_not_account_name_or_position() {
        let mut panel = fixture();
        apply_account_usage(
            &mut panel.rows,
            &HashMap::from([(
                ("codex".into(), "new".into()),
                vec!["5h 93% left".to_string()],
            )]),
        );
        assert_eq!(
            panel.rows[0].accounts[0].usage.as_deref(),
            Some(&["usage unavailable; run cc-proxy usage codex".to_string()][..])
        );
        assert_eq!(
            panel.rows[0].accounts[1].usage.as_deref(),
            Some(&["5h 93% left".to_string()][..])
        );
    }

    #[test]
    fn narrow_and_small_panels_scroll_without_overflow() {
        let mut panel = fixture();
        for (width, height) in [(110, 25), (40, 16), (20, 8), (3, 2)] {
            render(&panel, width, height);
            panel.scroll(true);
            render(&panel, width, height);
        }
        assert!(
            render(&ProvidersPanel::default(), 80, 12).contains("Checking local saved accounts")
        );
    }
}
