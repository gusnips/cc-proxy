//! `cc-proxy config` with no subcommand: every setting in one list, driven by
//! the keyboard.
//!
//! Up and down move. Left and right change a yes/no or a fixed choice. Enter
//! types a value. The rows that hold maps open config.json in your editor, at
//! that key. A change is saved the moment you make it, the same way
//! `cc-proxy config set` saves it, so there is nothing to confirm and nothing
//! to undo when you close the screen.

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph, Wrap},
};

use super::{Applied, CONFIG_KEYS, ConfigKey, ConfigKind};
use crate::ui::{DIM, DIM_WHITE, GREEN, RED, TEAL, WHITE, YELLOW};

/// Where the screen reads settings from and saves them to. The disk in the
/// command; a plain map in the tests.
trait Store {
    fn read(&self) -> Result<serde_json::Value>;
    fn apply(&self, key: &ConfigKey, raw: &str) -> Result<Applied>;
}

struct Disk;

impl Store for Disk {
    fn read(&self) -> Result<serde_json::Value> {
        super::read_file()
    }

    fn apply(&self, key: &ConfigKey, raw: &str) -> Result<Applied> {
        super::apply(key, raw)
    }
}

#[derive(Clone, Copy)]
enum Row {
    Setting(&'static ConfigKey),
    /// A map or a whole file: opens config.json in your editor, at `key`.
    File {
        name: &'static str,
        key: Option<&'static str>,
        blurb: &'static str,
    },
}

const FILE_ROWS: [Row; 2] = [
    Row::File {
        name: "providers",
        key: Some("providers"),
        blurb: "providers you added with `cc-proxy provider add`",
    },
    Row::File {
        name: "config.json",
        key: None,
        blurb: "the whole file, for anything not listed here",
    },
];

impl Row {
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
enum Outcome {
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
            .map(|row| format!("{} {}", row.name(), row.blurb()))
            .collect();
        let mut screen = Self {
            store,
            rows,
            search,
            file: Ok(serde_json::json!({})),
            query: String::new(),
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

    fn press(&mut self, key: KeyEvent) -> Outcome {
        self.notice = None;
        match self.draft.take() {
            Some(draft) => self.type_value(draft, key),
            None => self.browse(key),
        }
    }

    fn browse(&mut self, key: KeyEvent) -> Outcome {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if control => return Outcome::Quit,
            KeyCode::Esc if self.query.is_empty() => return Outcome::Quit,
            KeyCode::Esc => self.query.clear(),
            KeyCode::Up => self.step(-1),
            KeyCode::Char('p') if control => self.step(-1),
            KeyCode::Down => self.step(1),
            KeyCode::Char('n') if control => self.step(1),
            KeyCode::Left => self.cycle(-1),
            KeyCode::Right => self.cycle(1),
            KeyCode::Enter => return self.activate(),
            KeyCode::Backspace => {
                self.query.pop();
            }
            KeyCode::Char(ch) if !control && !key.modifiers.contains(KeyModifiers::ALT) => {
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
        Outcome::Stay
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

    fn activate(&mut self) -> Outcome {
        match self.rows.get(self.selected).copied() {
            Some(Row::File { key, .. }) => Outcome::OpenFile(key),
            Some(Row::Setting(key)) if key.choices().is_some() => {
                self.cycle(1);
                Outcome::Stay
            }
            Some(Row::Setting(key)) => {
                // A secret starts empty: what is saved is never shown again.
                let initial = match key.kind {
                    ConfigKind::Secret => String::new(),
                    _ => self.current_text(key).unwrap_or_default(),
                };
                let chars: Vec<char> = initial.chars().collect();
                self.draft = Some(Draft {
                    key,
                    cursor: chars.len(),
                    chars,
                    initial,
                    error: None,
                });
                Outcome::Stay
            }
            None => Outcome::Stay,
        }
    }

    fn type_value(&mut self, mut draft: Draft, key: KeyEvent) -> Outcome {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if control => return Outcome::Quit,
            KeyCode::Esc => return Outcome::Stay,
            KeyCode::Enter => {
                let text: String = draft.chars.iter().collect();
                if text == draft.initial {
                    return Outcome::Stay;
                }
                match self.try_save(draft.key, &text) {
                    Ok(()) => return Outcome::Stay,
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
        Outcome::Stay
    }

    fn try_save(&mut self, key: &ConfigKey, raw: &str) -> Result<()> {
        let applied = self.store.apply(key, raw)?;
        self.notice = Some(Notice::Saved(saved_line(key, &applied)));
        self.reload();
        Ok(())
    }

    /// Save a value picked from a list. There is no field to show a failure in,
    /// so it goes to the line under the list.
    fn save(&mut self, key: &ConfigKey, raw: &str) {
        if let Err(error) = self.try_save(key, raw) {
            self.notice = Some(Notice::Problem(format!("{error:#}")));
        }
    }

    /// The editor closed: read the file again, whatever the editor did.
    fn edited(&mut self, result: Result<()>) {
        self.reload();
        if let Err(error) = result {
            self.notice = Some(Notice::Problem(format!("{error:#}")));
        }
    }

    /// What a row shows as its value.
    fn value_text(&self, row: &Row) -> String {
        match row {
            Row::File { .. } => "opens in your editor".to_string(),
            // A file that can't be read has no values to show. Showing defaults
            // would say the opposite of what is in it.
            Row::Setting(_) if self.file.is_err() => "-".to_string(),
            Row::Setting(key) => super::display_value(key, self.file_text(key)),
        }
    }

    fn view(&mut self, frame: &mut Frame) {
        let [header, list, detail, status, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        let search = if self.query.is_empty() {
            Span::styled("type to search", Style::default().fg(DIM))
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
            let name_width = self
                .rows
                .iter()
                .map(|row| row.name().chars().count())
                .max()
                .unwrap_or(0);
            let items: Vec<ListItem> = visible
                .iter()
                .map(|&index| self.item(index, name_width))
                .collect();
            self.list
                .select(visible.iter().position(|row| *row == self.selected));
            frame.render_stateful_widget(List::new(items), list, &mut self.list);
            if let Some(draft) = &self.draft
                && let Some(at) = self.list.selected()
            {
                // The field sits where the value was: marker, name, two spaces.
                let row = at.saturating_sub(self.list.offset());
                let x = list.x + 2 + name_width as u16 + 2 + draft.cursor as u16;
                frame.set_cursor_position((
                    x.min(list.right().saturating_sub(1)),
                    list.y + row as u16,
                ));
            }
        }

        if let Some(row) = self.rows.get(self.selected) {
            let mut lines = vec![Line::styled(row.blurb(), Style::default().fg(DIM_WHITE))];
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
            Paragraph::new(self.hints(frame.area().width)).style(Style::default().fg(DIM)),
            footer,
        );
    }

    /// One row of the list. While a value is typed, the field takes its place.
    fn item(&self, index: usize, name_width: usize) -> ListItem<'static> {
        let row = &self.rows[index];
        let selected = index == self.selected;
        let name = format!("{:<name_width$}", row.name());
        let marker = if selected { "❯ " } else { "  " };
        let marker_style = Style::default().fg(TEAL).add_modifier(Modifier::BOLD);
        let name_style = if selected {
            marker_style
        } else {
            Style::default().fg(WHITE)
        };

        let value = match &self.draft {
            Some(draft) if selected => {
                let shown: String = if draft.key.kind == ConfigKind::Secret {
                    "•".repeat(draft.chars.len())
                } else {
                    draft.chars.iter().collect()
                };
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
        ListItem::new(Line::from(vec![
            Span::styled(marker, marker_style),
            Span::styled(name, name_style),
            Span::raw("  "),
            value,
        ]))
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
            (None, Ok(_)) => Line::raw(""),
        }
    }

    fn hints(&self, width: u16) -> &'static str {
        // On a narrow terminal the line is cut off at the right, so the short
        // forms keep the way out (Esc) inside 40 columns.
        match (self.draft.is_some(), self.query.is_empty(), width >= 64) {
            (true, _, true) => " Enter save · Esc cancel · ←/→ move in the field",
            (true, _, false) => " Enter save · Esc cancel",
            (false, true, true) => {
                " ↑/↓ move · ←/→ change · Enter edit · type to search · Esc close"
            }
            (false, true, false) => " ↑/↓ move · Enter edit · Esc close",
            (false, false, true) => " ↑/↓ move · ←/→ change · Enter edit · Esc clear search",
            (false, false, false) => " Enter edit · Esc clear search",
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

fn suspend() -> Result<()> {
    disable_raw_mode()?;
    execute!(std::io::stdout(), LeaveAlternateScreen)?;
    Ok(())
}

fn resume(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>) -> Result<()> {
    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen)?;
    terminal.clear()?;
    Ok(())
}

pub(super) fn run() -> Result<()> {
    let mut screen = Screen::new(Disk);
    let mut terminal = crate::tui::setup_terminal()?;
    let _guard = crate::tui::TerminalGuard;
    loop {
        terminal.draw(|frame| screen.view(frame))?;
        let Event::Key(key) = event::read()? else {
            // A resize or a focus change: draw again.
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match screen.press(key) {
            Outcome::Stay => {}
            Outcome::Quit => return Ok(()),
            Outcome::OpenFile(at) => {
                suspend()?;
                let result = super::editor::open(at);
                // Back on the screen even when the editor never started.
                resume(&mut terminal)?;
                screen.edited(result);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::PathBuf;

    use ratatui::backend::TestBackend;

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

    fn press<S: Store>(screen: &mut Screen<S>, code: KeyCode) -> Outcome {
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
        terminal.draw(|frame| screen.view(frame)).unwrap();
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
    fn a_secret_starts_empty_and_is_never_drawn() {
        let file = Memory::with(serde_json::json!({"opencode": {"apiKey": "old-secret"}}));
        let mut screen = Screen::new(&file);
        go_to(&mut screen, "opencode.apiKey");

        let list = drawn(&mut screen, 80, 24);
        assert!(list.contains("set (config.json)"));
        assert!(!list.contains("old-secret"));

        press(&mut screen, KeyCode::Enter);
        assert!(screen.draft.as_ref().unwrap().chars.is_empty());
        type_text(&mut screen, "sk-123");
        let field = drawn(&mut screen, 80, 24);
        assert!(field.contains("••••••"));
        assert!(!field.contains("sk-123"));

        press(&mut screen, KeyCode::Enter);
        assert_eq!(file.0.borrow()["opencode"]["apiKey"], "sk-123");
        let after = drawn(&mut screen, 80, 24);
        assert!(after.contains("Saved opencode.apiKey."));
        assert!(!after.contains("sk-123"));
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
        assert!(drawn(&mut screen, 80, 24).contains("No setting matches \"codex transcrzzz\""));
        // Down and Enter do nothing with nothing to pick.
        press(&mut screen, KeyCode::Down);
        assert_eq!(press(&mut screen, KeyCode::Enter), Outcome::Stay);

        for _ in 0..3 {
            press(&mut screen, KeyCode::Backspace);
        }
        assert_eq!(screen.visible().len(), 1);
        // Escape clears the search first and closes second.
        assert_eq!(press(&mut screen, KeyCode::Esc), Outcome::Stay);
        assert!(screen.query.is_empty());
        assert_eq!(press(&mut screen, KeyCode::Esc), Outcome::Quit);
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
            Outcome::OpenFile(Some("providers"))
        );
        go_to(&mut screen, "config.json");
        assert_eq!(press(&mut screen, KeyCode::Enter), Outcome::OpenFile(None));
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
        assert!(wide.contains("❯ port"));
        assert!(wide.contains("type to search"));
        assert!(wide.contains("Esc close"));

        let small = drawn(&mut screen, 40, 8);
        assert!(small.contains("❯ port"));
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
        assert!(drawn(&mut screen, 60, 10).contains("❯ config.json"));
        go_to(&mut screen, "port");
        assert!(drawn(&mut screen, 60, 10).contains("❯ port"));
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
