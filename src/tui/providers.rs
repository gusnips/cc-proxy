//! The monitor's Providers overlay: which providers have a sign-in or key,
//! and how much of each plan is used. Read-only, and read in the monitor's
//! own process, so the proxy's protocol doesn't change.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
};

use super::{BG, DIM, DIM_WHITE, GREEN, TEAL, WHITE};
use crate::provider::AuthState;
use crate::registry::Registry;
use crate::ui;

/// How long a lookup is shown before the overlay asks again.
const REFRESH_AFTER: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRow {
    pub id: String,
    pub state: AuthState,
    /// The plan and its usage, once the provider has answered.
    pub usage: Option<String>,
}

/// The rows, and the lookup that fills them in the background.
#[derive(Default)]
pub struct ProvidersPanel {
    rows: Vec<ProviderRow>,
    loaded_at: Option<Instant>,
    pending: Option<mpsc::Receiver<Vec<ProviderRow>>>,
}

impl ProvidersPanel {
    /// Takes what the lookup has sent, and starts a new one when the rows
    /// are over a minute old. Call it while the overlay is open.
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
}

/// Sends the sign-in states at once, then the same rows with usage added:
/// reading usage asks each provider, and can take seconds.
fn load(send: &mpsc::Sender<Vec<ProviderRow>>) {
    let registry = Registry::with_default_alias();
    let mut rows: Vec<ProviderRow> = registry
        .list_provider_names()
        .into_iter()
        .filter_map(|id| {
            let state = registry.provider(&id)?.cli().auth_state();
            Some(ProviderRow {
                id,
                state,
                usage: None,
            })
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
    let _ = send.send(rows);
}

fn state_text(state: &AuthState) -> String {
    match state {
        AuthState::SignedIn {
            account,
            expires_ms,
        } => {
            let who = account.as_ref().map_or("signed in".to_string(), |account| {
                format!("signed in as {account}")
            });
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_millis() as u64);
            match expires_ms {
                Some(expires) if *expires > now_ms => format!(
                    "{who}, token good for {}",
                    ui::duration(Duration::from_millis(expires - now_ms))
                ),
                Some(_) => format!("{who}, renews on next use"),
                None => who,
            }
        }
        AuthState::KeySaved => "API key set".to_string(),
        AuthState::Missing => "not set up".to_string(),
    }
}

pub fn render_overlay(frame: &mut ratatui::Frame<'_>, area: Rect, panel: &ProvidersPanel) {
    let width = 84.min(area.width.saturating_sub(4)).max(36);
    let height = (panel.rows.len().max(1) as u16 * 2 + 5)
        .min(area.height.saturating_sub(2))
        .max(8);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(Span::styled(" Providers ", Style::default().fg(TEAL)))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(TEAL))
        .style(Style::default().bg(BG));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let mut lines = Vec::new();
    if panel.rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "  Checking what cc-proxy has for each provider...",
            Style::default().fg(DIM_WHITE),
        )));
    }
    for row in &panel.rows {
        let set_up = row.state != AuthState::Missing;
        lines.push(Line::from(vec![
            Span::styled(
                if set_up { "  ● " } else { "  ○ " },
                Style::default().fg(if set_up { GREEN } else { DIM }),
            ),
            Span::styled(
                format!("{:<12}", ui::provider_name(&row.id)),
                Style::default().fg(ui::provider_color(&row.id)),
            ),
            Span::styled(
                state_text(&row.state),
                Style::default().fg(if set_up { WHITE } else { DIM }),
            ),
        ]));
        if let Some(usage) = &row.usage {
            lines.push(Line::from(Span::styled(
                format!("                {usage}"),
                Style::default().fg(DIM_WHITE),
            )));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("cc-proxy setup", Style::default().fg(WHITE)),
        Span::styled(" adds a provider  ", Style::default().fg(DIM)),
        Span::styled("Esc", Style::default().fg(TEAL)),
        Span::styled(" close", Style::default().fg(DIM)),
    ]));
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(BG))
            .wrap(Wrap { trim: false }),
        inner,
    );
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    fn render(panel: &ProvidersPanel) -> String {
        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        terminal
            .draw(|frame| render_overlay(frame, frame.area(), panel))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_overlay_shows_each_provider_with_its_state_and_usage() {
        let panel = ProvidersPanel {
            rows: vec![
                ProviderRow {
                    id: "codex".into(),
                    state: AuthState::SignedIn {
                        account: Some("acct_1".into()),
                        expires_ms: Some(0),
                    },
                    usage: Some("plus · 5h 42% · week 12%".into()),
                },
                ProviderRow {
                    id: "glm".into(),
                    state: AuthState::KeySaved,
                    usage: None,
                },
                ProviderRow {
                    id: "kimi".into(),
                    state: AuthState::Missing,
                    usage: None,
                },
            ],
            ..ProvidersPanel::default()
        };
        let text = render(&panel);
        for expected in [
            "Providers",
            "Codex",
            "signed in as acct_1, renews on next use",
            "plus · 5h 42% · week 12%",
            "GLM",
            "API key set",
            "Kimi",
            "not set up",
            "cc-proxy setup",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in\n{text}");
        }
    }

    #[test]
    fn the_overlay_says_it_is_checking_before_any_row_arrives() {
        assert!(render(&ProvidersPanel::default()).contains("Checking"));
    }
}
