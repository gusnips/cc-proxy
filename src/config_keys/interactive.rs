//! `cc-proxy config` with no subcommand: every setting in one list, driven by
//! the keyboard.
//!
//! Up and down move. Left and right change a yes/no or a fixed choice. Enter
//! types a value. The rows that hold maps open config.json in your editor, at
//! that key. A change is saved the moment you make it, the same way
//! `cc-proxy config set` saves it, so there is nothing to confirm and nothing
//! to undo when you close the screen.

use std::cell::RefCell;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph, Wrap},
};

use super::{Applied, CONFIG_KEYS, ConfigKey};
use crate::ui::{DIM, DIM_WHITE, GREEN, RED, TEAL, WHITE, YELLOW};

/// The real file for the command and monitor, or an isolated map for demos and tests.
trait Store {
    fn read(&self) -> Result<serde_json::Value>;
    fn apply(&self, key: &ConfigKey, raw: &str) -> Result<Applied>;

    fn is_demo(&self) -> bool {
        false
    }
}

enum SettingsStore {
    Disk,
    Demo(RefCell<serde_json::Value>),
}

impl Store for SettingsStore {
    fn read(&self) -> Result<serde_json::Value> {
        match self {
            Self::Disk => super::read_file(),
            Self::Demo(root) => Ok(root.borrow().clone()),
        }
    }

    fn apply(&self, key: &ConfigKey, raw: &str) -> Result<Applied> {
        match self {
            Self::Disk => super::apply(key, raw),
            Self::Demo(root) => {
                let value = super::parse_value(key, raw)?;
                super::set_dotted(&mut root.borrow_mut(), key.path, value)?;
                Ok(Applied {
                    path: "config.json".into(),
                    overridden_by: None,
                    restart: false,
                })
            }
        }
    }

    fn is_demo(&self) -> bool {
        matches!(self, Self::Demo(_))
    }
}

/// The existing settings screen, rendered inside a monitor tab. Embedded, a
/// search starts with `/`, because the monitor owns the bare letters.
pub struct SettingsPanel {
    screen: Screen<SettingsStore>,
}

impl SettingsPanel {
    pub fn new(demo: bool) -> Self {
        let store = if demo {
            SettingsStore::Demo(RefCell::new(serde_json::json!({})))
        } else {
            SettingsStore::Disk
        };
        let mut screen = Screen::new(store);
        screen.embedded = true;
        Self { screen }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.screen.view(frame, area);
    }

    pub fn press(&mut self, key: KeyEvent) -> SettingsOutcome {
        if key.kind != KeyEventKind::Press {
            return SettingsOutcome::Stay;
        }
        self.screen.press(key)
    }

    /// A field or a search owns navigation and text until Escape clears it.
    pub fn captures_input(&self) -> bool {
        self.screen.draft.is_some() || self.screen.is_searching()
    }

    /// The caller suspends its terminal before opening the editor, then resumes it.
    pub fn open_file(&mut self, at: Option<&'static str>) -> Result<()> {
        if self.screen.store.is_demo() {
            self.screen.notice = Some(Notice::Saved(
                "Demo: the editor stays closed. No files were changed.".into(),
            ));
            Ok(())
        } else {
            super::editor::open(at)
        }
    }

    pub fn edited(&mut self, result: Result<()>) {
        self.screen.edited(result);
    }

    /// The keys that work now. Embedded, the monitor shows this on its own
    /// footer line, so the panel draws none.
    pub fn hints(&self, width: u16) -> &'static str {
        self.screen.hints(width)
    }

    pub fn reload(&mut self) {
        self.screen.reload();
    }

    /// Preview a boolean setting in the demo store without writing any file.
    pub fn set_demo_bool(&mut self, path: &str, value: bool) {
        if let SettingsStore::Demo(root) = &self.screen.store {
            let _ = super::set_dotted(&mut root.borrow_mut(), path, serde_json::Value::Bool(value));
            self.screen.reload();
        }
    }

    pub fn bool_value(&self, path: &str) -> Option<bool> {
        let root = self.screen.file.as_ref().ok()?;
        super::get_dotted(root, path)?.as_bool()
    }
}

#[derive(Clone, Copy)]
enum Row {
    Setting(&'static ConfigKey),
    /// A map or a whole file: opens config.json in your editor, at `key`.
    File {
        label: &'static str,
        name: &'static str,
        key: Option<&'static str>,
        blurb: &'static str,
    },
}

const FILE_ROWS: [Row; 2] = [
    Row::File {
        label: "Custom providers",
        name: "providers",
        key: Some("providers"),
        blurb: "providers you added with `cc-proxy provider add`",
    },
    Row::File {
        label: "All settings file",
        name: "config.json",
        key: None,
        blurb: "the whole file, for anything not listed here",
    },
];

impl Row {
    fn label(&self) -> &'static str {
        match self {
            Self::Setting(key) => key.label,
            Self::File { label, .. } => label,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Setting(key) => key.path,
            Self::File { name, .. } => name,
        }
    }

    fn blurb(&self) -> &'static str {
        match self {
            Self::Setting(key) => key.blurb,
            Self::File { blurb, .. } => blurb,
        }
    }
}

/// A value being typed.
struct Draft {
    key: &'static ConfigKey,
    chars: Vec<char>,
    cursor: usize,
    /// What the field held when it opened. Enter on an unchanged field saves nothing.
    initial: String,
    error: Option<String>,
}

enum Notice {
    Saved(String),
    Problem(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum SettingsOutcome {
    Stay,
    Quit,
    /// Open config.json in the editor, at this key when there is one.
    OpenFile(Option<&'static str>),
}

struct Screen<S: Store> {
    store: S,
    rows: Vec<Row>,
    /// What each row is searched by: its name and its description.
    search: Vec<String>,
    /// config.json as it is now, or why it can't be read.
    file: Result<serde_json::Value, String>,
    query: String,
    /// Embedded in the monitor: bare letters belong to it, so a search starts
    /// with `/` and stays open, even with nothing typed yet, until Escape.
    embedded: bool,
    search_open: bool,
    /// Index into `rows`, so the choice survives searching and saving.
    selected: usize,
    /// How far the list has scrolled, kept between frames so it doesn't jump.
    list: ListState,
    draft: Option<Draft>,
    notice: Option<Notice>,
}

impl<S: Store> Screen<S> {
    fn new(store: S) -> Self {
        let rows: Vec<Row> = CONFIG_KEYS
            .iter()
            .map(Row::Setting)
            .chain(FILE_ROWS)
            .collect();
        let search = rows
            .iter()
            .map(|row| format!("{} {} {}", row.label(), row.name(), row.blurb()))
            .collect();
        let mut screen = Self {
            store,
            rows,
            search,
            file: Ok(serde_json::json!({})),
            query: String::new(),
            embedded: false,
            search_open: false,
            selected: 0,
            list: ListState::default(),
            draft: None,
            notice: None,
        };
        screen.reload();
        screen
    }

    fn reload(&mut self) {
        self.file = self.store.read().map_err(|error| format!("{error:#}"));
    }

    /// Rows that match the search, in list order.
    fn visible(&self) -> Vec<usize> {
        crate::prompt::matching(&self.search, &self.query)
    }

    fn is_searching(&self) -> bool {
        self.search_open || !self.query.is_empty()
    }

    /// The text a setting holds in the file, if it holds a plain value.
    fn file_text(&self, key: &ConfigKey) -> Option<String> {
        let root = self.file.as_ref().ok()?;
        super::get_dotted(root, key.path).and_then(super::scalar_text)
    }

    /// What a setting holds now: the file's value, else its default.
    fn current_text(&self, key: &ConfigKey) -> Option<String> {
        self.file_text(key)
            .or_else(|| key.default.map(str::to_string))
    }

    fn press(&mut self, key: KeyEvent) -> SettingsOutcome {
        self.notice = None;
        match self.draft.take() {
            Some(draft) => self.type_value(draft, key),
            None => self.browse(key),
        }
    }

    fn browse(&mut self, key: KeyEvent) -> SettingsOutcome {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let has_selection = self.visible().contains(&self.selected);
        match key.code {
            KeyCode::Char('c') if control => return SettingsOutcome::Quit,
            KeyCode::Esc if !self.is_searching() => return SettingsOutcome::Quit,
            KeyCode::Esc => {
                self.query.clear();
                self.search_open = false;
            }
            KeyCode::Up => self.step(-1),
            KeyCode::Char('p') if control => self.step(-1),
            KeyCode::Down => self.step(1),
            KeyCode::Char('n') if control => self.step(1),
            KeyCode::Left if has_selection => self.cycle(-1),
            KeyCode::Right if has_selection => self.cycle(1),
            KeyCode::Enter if has_selection => return self.activate(),
            KeyCode::Backspace => {
                // Backspace on an empty search closes it.
                if self.query.pop().is_none() {
                    self.search_open = false;
                }
            }
            KeyCode::Char('/') if self.embedded && !self.is_searching() => {
                self.search_open = true;
            }
            KeyCode::Char(ch)
                if (!self.embedded || self.is_searching())
                    && !control
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.query.push(ch);
            }
            _ => {}
        }
        // Keep the selection while it still matches; else the first match.
        let visible = self.visible();
        if !visible.contains(&self.selected)
            && let Some(&first) = visible.first()
        {
            self.selected = first;
        }
        SettingsOutcome::Stay
    }

    fn step(&mut self, by: isize) {
        let visible = self.visible();
        if visible.is_empty() {
            return;
        }
        let at = visible
            .iter()
            .position(|row| *row == self.selected)
            .unwrap_or(0);
        self.selected = visible[(at as isize + by).rem_euclid(visible.len() as isize) as usize];
    }

    /// Left and right on a yes/no or a fixed choice: save the one before or after.
    fn cycle(&mut self, by: isize) {
        let Some(Row::Setting(key)) = self.rows.get(self.selected).copied() else {
            return;
        };
        let Some(choices) = key.choices() else {
            return;
        };
        let at = self
            .current_text(key)
            .and_then(|now| choices.iter().position(|choice| *choice == now));
        let next = match at {
            Some(at) => (at as isize + by).rem_euclid(choices.len() as isize) as usize,
            // Unset: the first choice going forward, the last going back.
            None if by > 0 => 0,
            None => choices.len() - 1,
        };
        self.save(key, choices[next]);
    }

    fn activate(&mut self) -> SettingsOutcome {
        match self.rows.get(self.selected).copied() {
            Some(Row::File { key, .. }) => SettingsOutcome::OpenFile(key),
            Some(Row::Setting(key)) if key.choices().is_some() => {
                self.cycle(1);
                SettingsOutcome::Stay
            }
            Some(Row::Setting(key)) => {
                let initial = self.current_text(key).unwrap_or_default();
                let chars: Vec<char> = initial.chars().collect();
                self.draft = Some(Draft {
                    key,
                    cursor: chars.len(),
                    chars,
                    initial,
                    error: None,
                });
                SettingsOutcome::Stay
            }
            None => SettingsOutcome::Stay,
        }
    }

    fn type_value(&mut self, mut draft: Draft, key: KeyEvent) -> SettingsOutcome {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if control => return SettingsOutcome::Quit,
            KeyCode::Esc => return SettingsOutcome::Stay,
            KeyCode::Enter => {
                let text: String = draft.chars.iter().collect();
                if text == draft.initial {
                    return SettingsOutcome::Stay;
                }
                match self.try_save(draft.key, &text) {
                    Ok(()) => return SettingsOutcome::Stay,
                    Err(error) => draft.error = Some(format!("{error:#}")),
                }
            }
            KeyCode::Left => draft.cursor = draft.cursor.saturating_sub(1),
            KeyCode::Right => draft.cursor = (draft.cursor + 1).min(draft.chars.len()),
            KeyCode::Home => draft.cursor = 0,
            KeyCode::End => draft.cursor = draft.chars.len(),
            KeyCode::Backspace if draft.cursor > 0 => {
                draft.cursor -= 1;
                draft.chars.remove(draft.cursor);
            }
            KeyCode::Delete if draft.cursor < draft.chars.len() => {
                draft.chars.remove(draft.cursor);
            }
            KeyCode::Char('u') if control => {
                draft.chars.clear();
                draft.cursor = 0;
            }
            // ponytail: a paste arrives as key presses, so a pasted newline saves the
            // draft. Bracketed paste would fix it; nothing here needs it yet.
            KeyCode::Char(ch) if !control && !key.modifiers.contains(KeyModifiers::ALT) => {
                draft.chars.insert(draft.cursor, ch);
                draft.cursor += 1;
            }
            _ => {}
        }
        self.draft = Some(draft);
        SettingsOutcome::Stay
    }

    fn try_save(&mut self, key: &ConfigKey, raw: &str) -> Result<()> {
        let applied = self.store.apply(key, raw)?;
        self.notice = Some(Notice::Saved(if self.store.is_demo() {
            format!("Demo: changed {}. No files were written.", key.path)
        } else {
            saved_line(key, &applied)
        }));
        self.reload();
        Ok(())
    }

    /// Save a value picked from a list. There is no field to show a failure in,
    /// so it goes to the line under the list.
    fn save(&mut self, key: &ConfigKey, raw: &str) {
        if let Err(error) = self.try_save(key, raw) {
            self.notice = Some(Notice::Problem(format!(
                "Couldn't save {}: {error:#}. Try ←/→ again.",
                key.path
            )));
        }
    }

    /// The editor closed: read the file again, whatever the editor did.
    fn edited(&mut self, result: Result<()>) {
        self.reload();
        match result {
            Ok(()) if self.file.is_err() => self.notice = None,
            Ok(()) if !self.store.is_demo() => {
                self.notice = Some(Notice::Saved("Editor closed. Settings reloaded.".into()));
            }
            Err(error) => {
                self.notice = Some(Notice::Problem(format!(
                    "Couldn't edit config.json: {error:#}. Press Enter to try again."
                )));
            }
            Ok(()) => {}
        }
    }

    /// What a row shows as its value.
    fn value_text(&self, row: &Row) -> String {
        match row {
            Row::File { .. } if self.store.is_demo() => "editor is off in demo".to_string(),
            Row::File { .. } => "opens in your editor".to_string(),
            // A file that can't be read has no values to show. Showing defaults
            // would say the opposite of what is in it.
            Row::Setting(_) if self.file.is_err() => "-".to_string(),
            Row::Setting(key) if self.store.is_demo() => {
                super::file_display_value(key, self.file_text(key))
            }
            Row::Setting(key) => super::display_value(key, self.file_text(key)),
        }
    }

    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let area = area.intersection(frame.area());
        if area.width == 0 || area.height == 0 {
            return;
        }
        let [header, list, detail, status, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(u16::from(!self.embedded)),
        ])
        .areas(area);

        let search = if !self.is_searching() {
            let hint = if self.embedded {
                "/ to search"
            } else {
                "type to search"
            };
            Span::styled(hint, Style::default().fg(DIM))
        } else {
            Span::styled(
                format!("search: {}", self.query),
                Style::default().fg(WHITE),
            )
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    " Settings  ",
                    Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
                ),
                search,
            ])),
            header,
        );

        let visible = self.visible();
        if visible.is_empty() {
            frame.render_widget(
                Paragraph::new(format!(
                    "  No setting matches \"{}\". Press Backspace to change the search.",
                    self.query
                ))
                .style(Style::default().fg(DIM_WHITE))
                .wrap(Wrap { trim: false }),
                list,
            );
        } else {
            let label_width = self
                .rows
                .iter()
                .map(|row| row.label().chars().count())
                .max()
                .unwrap_or(0);
            let show_key = list.width >= 76;
            let name_width = if show_key {
                self.rows
                    .iter()
                    .map(|row| row.name().chars().count())
                    .max()
                    .unwrap_or(0)
            } else {
                0
            };
            let items: Vec<ListItem> = visible
                .iter()
                .map(|&index| self.item(index, label_width, name_width))
                .collect();
            self.list
                .select(visible.iter().position(|row| *row == self.selected));
            frame.render_stateful_widget(List::new(items), list, &mut self.list);
            if let Some(draft) = &self.draft
                && let Some(at) = self.list.selected()
                && list.width > 0
                && list.height > 0
            {
                // The field sits where the value was: marker, label, optional key, two spaces.
                let row = at.saturating_sub(self.list.offset());
                let key_cols = if name_width > 0 { 2 + name_width } else { 0 };
                let offset =
                    (4 + label_width + key_cols + draft.cursor).min(usize::from(u16::MAX)) as u16;
                frame.set_cursor_position((
                    list.x.saturating_add(offset).min(list.right() - 1),
                    list.y.saturating_add(row as u16).min(list.bottom() - 1),
                ));
            }
        }

        if visible.contains(&self.selected)
            && let Some(row) = self.rows.get(self.selected)
        {
            let mut lines = vec![Line::from(vec![
                Span::styled(row.name(), Style::default().fg(TEAL)),
                Span::styled(" · ", Style::default().fg(DIM)),
                Span::styled(row.blurb(), Style::default().fg(DIM_WHITE)),
            ])];
            if let Row::Setting(_) = row {
                lines.push(Line::styled(
                    format!("Now: {}", self.value_text(row)),
                    Style::default().fg(WHITE),
                ));
            }
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), detail);
        }

        frame.render_widget(Paragraph::new(self.status_line()), status);
        frame.render_widget(
            Paragraph::new(self.hints(area.width)).style(Style::default().fg(DIM)),
            footer,
        );
    }

    /// One row of the list. While a value is typed, the field takes its place.
    fn item(&self, index: usize, label_width: usize, name_width: usize) -> ListItem<'static> {
        let row = &self.rows[index];
        let selected = index == self.selected;
        let label = format!("{:<label_width$}", row.label());
        let marker = if selected { "❯ " } else { "  " };
        let marker_style = Style::default().fg(TEAL).add_modifier(Modifier::BOLD);
        let label_style = if selected {
            marker_style
        } else {
            Style::default().fg(WHITE)
        };

        let value = match &self.draft {
            Some(draft) if selected => {
                let shown: String = draft.chars.iter().collect();
                Span::styled(
                    shown,
                    Style::default()
                        .fg(WHITE)
                        .add_modifier(Modifier::UNDERLINED),
                )
            }
            _ => {
                let mut text = self.value_text(row);
                if selected
                    && let Row::Setting(key) = row
                    && key.choices().is_some()
                {
                    text = format!("‹ {text} ›");
                }
                Span::styled(text, Style::default().fg(DIM_WHITE))
            }
        };
        let mut spans = vec![
            Span::styled(marker, marker_style),
            Span::styled(label, label_style),
        ];
        if name_width > 0 {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                format!("{:<name_width$}", row.name()),
                Style::default().fg(DIM),
            ));
        }
        spans.push(Span::raw("  "));
        spans.push(value);
        ListItem::new(Line::from(spans))
    }

    fn status_line(&self) -> Line<'static> {
        if let Some(error) = self.draft.as_ref().and_then(|draft| draft.error.as_ref()) {
            return Line::styled(format!(" {error}"), Style::default().fg(RED));
        }
        match (&self.notice, &self.file) {
            (Some(Notice::Saved(text)), _) => {
                Line::styled(format!(" {text}"), Style::default().fg(GREEN))
            }
            (Some(Notice::Problem(text)), _) => {
                Line::styled(format!(" {text}"), Style::default().fg(RED))
            }
            (None, Err(error)) => Line::styled(
                format!(
                    " config.json can't be read: {error}. Fix it in the `config.json` row, then come back."
                ),
                Style::default().fg(YELLOW),
            ),
            (None, Ok(_)) if self.store.is_demo() => Line::styled(
                " Demo: changes stay here. No files are changed.",
                Style::default().fg(DIM_WHITE),
            ),
            (None, Ok(_)) => Line::raw(""),
        }
    }

    fn hints(&self, width: u16) -> &'static str {
        // On a narrow terminal the line is cut off at the right, so the short
        // forms keep the way out (Esc) inside 40 columns.
        match (
            self.draft.is_some(),
            self.is_searching(),
            self.embedded,
            width >= 64,
        ) {
            (true, _, _, true) => " Enter save · Esc cancel · ←/→ move in the field",
            (true, _, _, false) => " Enter save · Esc cancel",
            (false, false, false, true) => {
                " ↑/↓ move · ←/→ change · Enter edit · type to search · Esc close"
            }
            (false, false, false, false) => " ↑/↓ move · Enter edit · Esc close",
            (false, false, true, true) => {
                " ↑/↓ move · ←/→ change · Enter edit · / search · Esc back"
            }
            // 39 columns: the monitor's narrow tab keeps change, search and the way out.
            (false, false, true, false) => " ↑/↓ · ←/→ change · / search · Esc back",
            (false, true, _, true) => " ↑/↓ move · ←/→ change · Enter edit · Esc clear search",
            (false, true, _, false) => " Enter edit · Esc clear search",
        }
    }
}

fn saved_line(key: &ConfigKey, applied: &Applied) -> String {
    let mut line = format!("Saved {}.", key.path);
    if let Some(var) = applied.overridden_by {
        line.push_str(&format!(" ${var} is set and still wins at runtime."));
    }
    if applied.restart {
        line.push_str(" Run `cc-proxy restart` to apply it.");
    }
    line
}

pub(super) fn run() -> Result<()> {
    // Not embedded: typing goes straight into the search.
    let mut screen = SettingsPanel {
        screen: Screen::new(SettingsStore::Disk),
    };
    let mut terminal = crate::tui::setup_terminal()?;
    let _guard = crate::tui::TerminalGuard;
    loop {
        terminal.draw(|frame| screen.render(frame, frame.area()))?;
        let Event::Key(key) = event::read()? else {
            // A resize or a focus change: draw again.
            continue;
        };
        match screen.press(key) {
            SettingsOutcome::Stay => {}
            SettingsOutcome::Quit => return Ok(()),
            SettingsOutcome::OpenFile(at) => {
                crate::tui::suspend_terminal()?;
                let result = screen.open_file(at);
                // Back on the screen even when the editor never started.
                crate::tui::resume_terminal(&mut terminal)?;
                screen.edited(result);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    /// config.json held in memory, validated and written the way the disk store does.
    struct Memory(RefCell<serde_json::Value>);

    impl Memory {
        fn with(root: serde_json::Value) -> Self {
            Self(RefCell::new(root))
        }
    }

    impl Store for &Memory {
        fn read(&self) -> Result<serde_json::Value> {
            Ok(self.0.borrow().clone())
        }

        fn apply(&self, key: &ConfigKey, raw: &str) -> Result<Applied> {
            let value = super::super::parse_value(key, raw)?;
            super::super::set_dotted(&mut self.0.borrow_mut(), key.path, value)?;
            Ok(Applied {
                path: PathBuf::from("config.json"),
                overridden_by: None,
                restart: key.needs_restart(),
            })
        }
    }

    struct Unreadable;

    impl Store for Unreadable {
        fn read(&self) -> Result<serde_json::Value> {
            anyhow::bail!("invalid config at /c/config.json: expected value at line 1")
        }

        fn apply(&self, _: &ConfigKey, _: &str) -> Result<Applied> {
            anyhow::bail!("not reached")
        }
    }

    fn press<S: Store>(screen: &mut Screen<S>, code: KeyCode) -> SettingsOutcome {
        screen.press(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn type_text<S: Store>(screen: &mut Screen<S>, text: &str) {
        for ch in text.chars() {
            press(screen, KeyCode::Char(ch));
        }
    }

    fn go_to<S: Store>(screen: &mut Screen<S>, name: &str) {
        screen.selected = screen
            .rows
            .iter()
            .position(|row| row.name() == name)
            .unwrap();
    }

    fn drawn<S: Store>(screen: &mut Screen<S>, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| screen.view(frame, frame.area()))
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

    fn saved_text<S: Store>(screen: &Screen<S>) -> Option<&str> {
        match &screen.notice {
            Some(Notice::Saved(text)) => Some(text),
            _ => None,
        }
    }

    #[test]
    fn left_and_right_step_through_choices_and_save_at_once() {
        let file = Memory::with(serde_json::json!({}));
        let mut screen = Screen::new(&file);

        go_to(&mut screen, "codex.fullLane");
        press(&mut screen, KeyCode::Right);
        assert_eq!(file.0.borrow()["codex"]["fullLane"], true);
        assert_eq!(saved_text(&screen), Some("Saved codex.fullLane."));
        press(&mut screen, KeyCode::Left);
        assert_eq!(file.0.borrow()["codex"]["fullLane"], false);
        // Enter steps forward too.
        press(&mut screen, KeyCode::Enter);
        assert_eq!(file.0.borrow()["codex"]["fullLane"], true);

        // Unset, there is no current choice: right starts at the first, left at the last.
        go_to(&mut screen, "autoReviewEffort");
        press(&mut screen, KeyCode::Right);
        assert_eq!(file.0.borrow()["autoReviewEffort"], "off");
        *file.0.borrow_mut() = serde_json::json!({});
        screen.reload();
        press(&mut screen, KeyCode::Left);
        assert_eq!(file.0.borrow()["autoReviewEffort"], "max");
        // And it wraps.
        press(&mut screen, KeyCode::Right);
        assert_eq!(file.0.borrow()["autoReviewEffort"], "off");

        // A key that takes typed values ignores left and right.
        go_to(&mut screen, "port");
        press(&mut screen, KeyCode::Right);
        assert!(file.0.borrow().get("port").is_none());
    }

    #[test]
    fn enter_opens_a_field_and_saves_what_is_typed() {
        let file = Memory::with(serde_json::json!({"port": 18000}));
        let mut screen = Screen::new(&file);
        go_to(&mut screen, "port");

        press(&mut screen, KeyCode::Enter);
        assert_eq!(screen.draft.as_ref().unwrap().initial, "18000");
        for _ in 0..5 {
            press(&mut screen, KeyCode::Backspace);
        }
        type_text(&mut screen, "8080");
        press(&mut screen, KeyCode::Enter);

        assert!(screen.draft.is_none());
        assert_eq!(file.0.borrow()["port"], 8080);
        assert_eq!(
            saved_text(&screen),
            Some("Saved port. Run `cc-proxy restart` to apply it.")
        );

        // An unchanged field saves nothing.
        file.0.borrow_mut()["port"] = serde_json::json!(9000);
        screen.reload();
        press(&mut screen, KeyCode::Enter);
        press(&mut screen, KeyCode::Enter);
        assert!(screen.draft.is_none());
        assert!(saved_text(&screen).is_none());
    }

    #[test]
    fn a_value_that_fails_stays_in_the_field_with_the_reason() {
        let file = Memory::with(serde_json::json!({"port": 18000}));
        let mut screen = Screen::new(&file);
        go_to(&mut screen, "port");

        press(&mut screen, KeyCode::Enter);
        type_text(&mut screen, "x");
        press(&mut screen, KeyCode::Enter);
        let draft = screen.draft.as_ref().expect("the field stays open");
        assert!(draft.error.as_ref().unwrap().contains("port"));
        assert_eq!(draft.chars.iter().collect::<String>(), "18000x");
        assert_eq!(file.0.borrow()["port"], 18000);
        assert!(drawn(&mut screen, 80, 24).contains("must be a port 1-65535"));

        // Escape drops only the draft.
        press(&mut screen, KeyCode::Esc);
        assert!(screen.draft.is_none());
        assert_eq!(file.0.borrow()["port"], 18000);
    }

    #[test]
    fn rows_show_human_labels_dotted_keys_and_descriptions() {
        let file = Memory::with(serde_json::json!({}));
        let mut screen = Screen::new(&file);

        let list = drawn(&mut screen, 100, 24);
        assert!(list.contains("❯ Auto-rotate accounts"));
        assert!(list.contains("auth.autoRotate"));
        assert!(list.contains("Pause new requests"));
        assert!(list.contains("auth.pauseRequests"));
        assert!(list.contains("Wait for account reset"));
        assert!(list.contains("auth.waitForReset"));
        assert!(list.contains("switch saved accounts when one reaches a limit"));

        // Searching by the human label finds the setting too.
        type_text(&mut screen, "Pause new");
        assert_eq!(screen.visible().len(), 1);
        assert_eq!(screen.rows[screen.selected].name(), "auth.pauseRequests");
    }

    #[test]
    fn typing_in_the_list_searches_and_keeps_the_selection_while_it_matches() {
        let file = Memory::with(serde_json::json!({}));
        let mut screen = Screen::new(&file);
        go_to(&mut screen, "codex.imagesApi");

        type_text(&mut screen, "codex");
        assert_eq!(screen.rows[screen.selected].name(), "codex.imagesApi");
        type_text(&mut screen, " transcr");
        assert_eq!(
            screen.rows[screen.selected].name(),
            "codex.transcriptionsApi"
        );
        assert_eq!(screen.visible().len(), 1);

        type_text(&mut screen, "zzz");
        assert!(screen.visible().is_empty());
        let empty = drawn(&mut screen, 80, 24);
        assert!(empty.contains("No setting matches \"codex transcrzzz\""));
        assert!(!empty.contains("Now:"));
        // Nothing can change a hidden row when the search has no matches.
        press(&mut screen, KeyCode::Down);
        press(&mut screen, KeyCode::Left);
        press(&mut screen, KeyCode::Right);
        assert_eq!(press(&mut screen, KeyCode::Enter), SettingsOutcome::Stay);
        assert_eq!(*file.0.borrow(), serde_json::json!({}));

        for _ in 0..3 {
            press(&mut screen, KeyCode::Backspace);
        }
        assert_eq!(screen.visible().len(), 1);
        // Escape clears the search first and closes second.
        assert_eq!(press(&mut screen, KeyCode::Esc), SettingsOutcome::Stay);
        assert!(screen.query.is_empty());
        assert_eq!(press(&mut screen, KeyCode::Esc), SettingsOutcome::Quit);
    }

    #[test]
    fn up_and_down_wrap_around() {
        let file = Memory::with(serde_json::json!({}));
        let mut screen = Screen::new(&file);
        press(&mut screen, KeyCode::Up);
        assert_eq!(screen.rows[screen.selected].name(), "config.json");
        press(&mut screen, KeyCode::Down);
        assert_eq!(screen.selected, 0);
    }

    #[test]
    fn rows_that_hold_maps_open_the_file_at_their_key() {
        let file = Memory::with(serde_json::json!({}));
        let mut screen = Screen::new(&file);
        go_to(&mut screen, "providers");
        assert_eq!(
            press(&mut screen, KeyCode::Enter),
            SettingsOutcome::OpenFile(Some("providers"))
        );
        go_to(&mut screen, "config.json");
        assert_eq!(
            press(&mut screen, KeyCode::Enter),
            SettingsOutcome::OpenFile(None)
        );
        // Left and right change nothing on these rows.
        press(&mut screen, KeyCode::Right);
        assert!(screen.notice.is_none());
    }

    #[test]
    fn a_file_that_cannot_be_read_shows_no_values_and_says_why() {
        let mut screen = Screen::new(Unreadable);
        let text = drawn(&mut screen, 100, 24);
        assert!(text.contains("config.json can't be read: invalid config at /c/config.json"));
        assert!(!text.contains("(default"));

        // The editor row is the way out, and when it closes the file is read again.
        screen.edited(Ok(()));
        screen.edited(Err(anyhow::anyhow!("editor exited with exit status: 1")));
        assert!(matches!(screen.notice, Some(Notice::Problem(_))));
    }

    #[test]
    fn the_screen_fits_a_small_terminal_and_keeps_the_selection_in_view() {
        let file = Memory::with(serde_json::json!({}));
        let mut screen = Screen::new(&file);

        let wide = drawn(&mut screen, 80, 24);
        assert!(wide.contains("Settings"));
        assert!(wide.contains("❯ Auto-rotate accounts"));
        assert!(wide.contains("auth.autoRotate"));
        assert!(wide.contains("type to search"));
        assert!(wide.contains("Esc close"));

        go_to(&mut screen, "port");
        let small = drawn(&mut screen, 40, 8);
        assert!(small.contains("❯ Listener port"));
        assert!(small.contains("port"));
        assert!(small.contains("Esc close"));
        // The way out stays visible in every mode.
        type_text(&mut screen, "po");
        assert!(drawn(&mut screen, 40, 8).contains("Esc clear search"));
        press(&mut screen, KeyCode::Esc);
        press(&mut screen, KeyCode::Enter);
        assert!(drawn(&mut screen, 40, 8).contains("Esc cancel"));
        press(&mut screen, KeyCode::Esc);

        // Far down a short list, the selected row is still on screen.
        go_to(&mut screen, "config.json");
        let at_file = drawn(&mut screen, 60, 10);
        assert!(at_file.contains("❯ All settings file"));
        assert!(at_file.contains("config.json"));
        go_to(&mut screen, "port");
        let at_port = drawn(&mut screen, 60, 10);
        assert!(at_port.contains("❯ Listener port"));
        assert!(at_port.contains("port"));
    }

    #[test]
    fn an_embedded_panel_renders_only_inside_its_area() {
        let mut panel = SettingsPanel::new(true);
        go_to(&mut panel.screen, "port");
        panel.press(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        type_text(&mut panel.screen, &"1".repeat(100));

        for area in [
            Rect::new(8, 4, 64, 18),
            Rect::new(9, 6, 40, 8),
            Rect::new(98, 28, 20, 8),
            Rect::new(3, 2, 1, 1),
            Rect::new(4, 3, 0, 0),
        ] {
            let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
            terminal
                .draw(|frame| {
                    frame.render_widget(
                        Paragraph::new(vec![Line::from(".".repeat(100)); 30]),
                        frame.area(),
                    );
                    panel.render(frame, area);
                })
                .unwrap();
            let clipped = area.intersection(Rect::new(0, 0, 100, 30));
            let buffer = terminal.backend().buffer();
            for y in 0..30 {
                for x in 0..100 {
                    let inside = x >= clipped.x
                        && x < clipped.right()
                        && y >= clipped.y
                        && y < clipped.bottom();
                    if !inside {
                        assert_eq!(buffer[(x, y)].symbol(), ".", "outside {area:?} at {x},{y}");
                    }
                }
            }
            if area.width >= 40 {
                let cursor = terminal.get_cursor_position().unwrap();
                assert!(cursor.x >= clipped.x && cursor.x < clipped.right());
                assert!(cursor.y >= clipped.y && cursor.y < clipped.bottom());
            }
        }
    }

    #[test]
    fn demo_settings_and_editor_stay_in_memory() {
        let mut panel = SettingsPanel::new(true);
        assert!(matches!(panel.screen.store, SettingsStore::Demo(_)));
        assert_eq!(panel.screen.file.as_ref().unwrap(), &serde_json::json!({}));
        go_to(&mut panel.screen, "auth.autoRotate");
        panel.press(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(
            panel.screen.file.as_ref().unwrap()["auth"]["autoRotate"],
            true
        );
        assert_eq!(
            saved_text(&panel.screen),
            Some("Demo: changed auth.autoRotate. No files were written.")
        );
        panel.reload();
        assert_eq!(
            panel.screen.file.as_ref().unwrap()["auth"]["autoRotate"],
            true
        );

        // A fresh demo has its own store, never the real saved settings.
        let fresh = SettingsPanel::new(true);
        assert_eq!(fresh.screen.file.as_ref().unwrap(), &serde_json::json!({}));
        let unset_key = super::super::find_key("codex.baseUrl").unwrap();
        assert_eq!(panel.screen.value_text(&Row::Setting(unset_key)), "unset");

        go_to(&mut panel.screen, "providers");
        assert_eq!(
            panel.press(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            SettingsOutcome::OpenFile(Some("providers"))
        );
        let result = panel.open_file(Some("providers"));
        panel.edited(result);
        assert_eq!(
            saved_text(&panel.screen),
            Some("Demo: the editor stays closed. No files were changed.")
        );
    }

    #[test]
    fn embedded_hints_say_back_and_slash_search_at_every_width() {
        let mut panel = SettingsPanel::new(true);
        for (width, hint) in [
            (
                80,
                "↑/↓ move · ←/→ change · Enter edit · / search · Esc back",
            ),
            (40, "↑/↓ · ←/→ change · / search · Esc back"),
        ] {
            // The monitor owns the footer line, so the panel draws no hints.
            assert!(panel.hints(width).contains(hint), "{width} columns");
            let text = drawn(&mut panel.screen, width, 10);
            assert!(text.contains("/ to search"));
            assert!(!text.contains(hint) && !text.contains("type to search"));
        }

        // Standalone keeps typing straight into the search and closing with Esc.
        let file = Memory::with(serde_json::json!({}));
        let mut screen = Screen::new(&file);
        let text = drawn(&mut screen, 40, 10);
        assert!(text.contains("type to search") && text.contains("Esc close"));
        assert!(!text.contains("/ search") && !text.contains("Esc back"));
    }

    #[test]
    fn an_embedded_panel_searches_only_after_a_slash_and_captures_until_escape() {
        let mut panel = SettingsPanel::new(true);
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        assert!(!panel.captures_input());

        // The monitor owns bare letters and digits until a search is open.
        for ch in ['p', 'q', 'b', 's', '1', '2', '3', '?'] {
            panel.press(key(KeyCode::Char(ch)));
            assert!(!panel.captures_input());
            assert!(panel.screen.query.is_empty());
        }
        let idle = drawn(&mut panel.screen, 80, 24);
        assert!(idle.contains("/ to search") && panel.hints(80).contains("/ search · Esc back"));
        assert!(!idle.contains("type to search") && !panel.hints(80).contains("Esc close"));

        // `/` opens the search, even before anything is typed.
        panel.press(key(KeyCode::Char('/')));
        assert!(panel.captures_input());
        assert!(panel.screen.query.is_empty());
        panel.press(key(KeyCode::Char('p')));
        panel.press(key(KeyCode::Char('2')));
        assert_eq!(panel.screen.query, "p2");
        assert!(panel.hints(80).contains("Esc clear search"));
        assert_eq!(panel.press(key(KeyCode::Esc)), SettingsOutcome::Stay);
        assert!(!panel.captures_input());

        // Backspace on an empty search closes it.
        panel.press(key(KeyCode::Char('/')));
        panel.press(key(KeyCode::Backspace));
        assert!(!panel.captures_input());

        go_to(&mut panel.screen, "port");
        panel.press(key(KeyCode::Enter));
        assert!(panel.captures_input());
        panel.press(key(KeyCode::Char('3')));
        assert_eq!(
            panel
                .screen
                .draft
                .as_ref()
                .unwrap()
                .chars
                .iter()
                .collect::<String>(),
            "187653"
        );
        panel.press(key(KeyCode::Esc));
        assert!(!panel.captures_input());
        assert_eq!(panel.press(key(KeyCode::Esc)), SettingsOutcome::Quit);
    }

    #[test]
    fn editor_results_reload_settings_and_failures_allow_another_try() {
        let file = Memory::with(serde_json::json!({"port": 18000}));
        let mut screen = Screen::new(&file);
        go_to(&mut screen, "config.json");
        file.0.borrow_mut()["port"] = serde_json::json!(9000);
        screen.edited(Ok(()));
        assert_eq!(screen.file.as_ref().unwrap()["port"], 9000);
        assert_eq!(
            saved_text(&screen),
            Some("Editor closed. Settings reloaded.")
        );

        screen.edited(Err(anyhow::anyhow!("editor exited with exit status: 1")));
        let Some(Notice::Problem(message)) = &screen.notice else {
            panic!("the editor failure must stay visible");
        };
        assert!(message.contains("editor exited with exit status: 1"));
        assert!(message.contains("Press Enter to try again"));
        assert_eq!(
            press(&mut screen, KeyCode::Enter),
            SettingsOutcome::OpenFile(None)
        );
    }

    #[test]
    fn the_saved_line_says_what_still_wins_and_what_needs_a_restart() {
        let key = CONFIG_KEYS.iter().find(|key| key.path == "port").unwrap();
        let applied = Applied {
            path: PathBuf::from("config.json"),
            overridden_by: Some("PORT"),
            restart: true,
        };
        assert_eq!(
            saved_line(key, &applied),
            "Saved port. $PORT is set and still wins at runtime. Run `cc-proxy restart` to apply it."
        );
    }
}
