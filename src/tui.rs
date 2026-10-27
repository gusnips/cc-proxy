mod controls;
mod layout;
mod providers;

use layout::{
    CODE_WIDTH, COUNT_WIDTH, ColumnSpec, DURATION_WIDTH, EFFORT_WIDTH, ENDPOINT_WIDTH, ERROR_WIDTH,
    ID_WIDTH, LayoutTier, MODEL_MEDIUM_WIDTH, MODEL_NARROW_WIDTH, MODEL_WIDE_WIDTH,
    PROJECT_MEDIUM_WIDTH, PROJECT_WIDE_WIDTH, PROVIDER_WIDTH, RATE_WIDTH, STATUS_WIDTH, TIME_WIDTH,
    TOKEN_WIDTH,
};
use providers::ProvidersPanel;

use std::{
    collections::HashMap,
    io::{self, Stdout},
    sync::mpsc,
    time::{Duration, SystemTime},
};

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use jiff::{Timestamp, Zoned, tz::TimeZone};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
};
use tokio::sync::oneshot;

use crate::{
    config, config_keys, daemon,
    monitor::{
        MockMonitor, MonitorHandle, SESSION_TOKEN_BUCKET_SECS, Throughput,
        snapshot::{
            ActiveSnapshot, CompletedSnapshot, MonitorSnapshot, SessionSnapshot, SnapshotUpdate,
        },
    },
    paths,
    registry::Registry,
    ui::{self, DIM, DIM_WHITE, GREEN, Mood, RED, TEAL, WHITE, YELLOW},
};

const SEPARATOR: Color = Color::Rgb(72, 74, 82);
const BG: Color = Color::Rgb(18, 18, 22);
const PANEL_BG: Color = Color::Rgb(22, 22, 27);
const SELECTED_BG: Color = Color::Rgb(42, 45, 54);
const BLUE: Color = Color::Rgb(120, 170, 230);
const PURPLE: Color = Color::Rgb(190, 140, 240);
const SESSION_SPARKLINE_MIN_WIDTH: u16 = 170;
/// Four minutes of 10-second buckets.
const HEADER_SPARKLINE_BUCKETS: usize = 24;
/// How long the header face looks unsure after a failed request.
const RECENT_ERROR: Duration = Duration::from_secs(10);
const SESSION_SPARKLINE_MAX_TOKENS: u64 = 4_000;

pub struct MonitorUiConfig<'a> {
    pub listen_url: String,
    pub port: u16,
    pub registry: &'a Registry,
    pub shutdown: Option<oneshot::Sender<()>>,
    pub shutdown_complete: Option<mpsc::Receiver<()>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MonitorExit {
    ShutdownComplete,
    ForceQuit,
    Detached,
}

pub fn run_monitor(
    handle: MonitorHandle,
    config: MonitorUiConfig<'_>,
) -> Result<MonitorExit, anyhow::Error> {
    run_monitor_loop(
        || SnapshotUpdate::Live(handle.snapshot().into()),
        UiSession::from(config),
    )
}

pub fn run_attached_monitor(
    snapshot: impl FnMut() -> SnapshotUpdate,
    listen_url: String,
    local: bool,
) -> Result<MonitorExit, anyhow::Error> {
    let setup_text = format!(
        "Attached to {listen_url}\nClosing the monitor leaves the proxy running.\nUse the Accounts tab to manage a local proxy. Explicit --url connections are read-only."
    );
    run_monitor_loop(
        snapshot,
        UiSession {
            listen_url,
            setup_text,
            accounts_mode: if local {
                providers::Mode::Local
            } else {
                providers::Mode::Remote
            },
            shutdown: None,
            shutdown_complete: None,
        },
    )
}

pub fn run_mock_monitor(port: u16, registry: &Registry) -> Result<(), anyhow::Error> {
    let mut monitor = MockMonitor::new();
    run_monitor_loop(
        move || SnapshotUpdate::Live(monitor.snapshot().into()),
        UiSession {
            listen_url: "mock://tui-demo".to_string(),
            setup_text: mock_setup_text(port, registry),
            accounts_mode: providers::Mode::Demo,
            shutdown: None,
            shutdown_complete: None,
        },
    )
    .map(|_| ())
}

struct UiSession {
    listen_url: String,
    setup_text: String,
    accounts_mode: providers::Mode,
    shutdown: Option<oneshot::Sender<()>>,
    shutdown_complete: Option<mpsc::Receiver<()>>,
}

impl From<MonitorUiConfig<'_>> for UiSession {
    fn from(config: MonitorUiConfig<'_>) -> Self {
        Self {
            setup_text: setup_text(config.port, config.registry),
            listen_url: config.listen_url,
            accounts_mode: providers::Mode::Local,
            shutdown: config.shutdown,
            shutdown_complete: config.shutdown_complete,
        }
    }
}

fn run_monitor_loop(
    mut snapshot: impl FnMut() -> SnapshotUpdate,
    config: UiSession,
) -> Result<MonitorExit, anyhow::Error> {
    let mut terminal = setup_terminal()?;
    let _guard = TerminalGuard;
    let mut app = MonitorApp {
        listen_url: config.listen_url,
        setup_text: config.setup_text,
        show_setup: false,
        show_help: false,
        page: if config.accounts_mode == providers::Mode::Remote {
            Page::Activity
        } else {
            Page::Accounts
        },
        accounts_mode: config.accounts_mode,
        providers: ProvidersPanel::new(config.accounts_mode),
        settings: (config.accounts_mode != providers::Mode::Remote).then(|| {
            config_keys::SettingsPanel::new(config.accounts_mode == providers::Mode::Demo)
        }),
        detail: None,
        focus: FocusPane::Sessions,
        selected: 0,
        recent_selected: 0,
        connected: true,
        proxy_off: false,
        saving: None,
        tick: 0,
        phase: MonitorPhase::Running,
        shutdown: config.shutdown,
        shutdown_complete: config.shutdown_complete,
    };

    let run_result = run_monitor_events(&mut terminal, &mut snapshot, &mut app);
    if run_result.is_err() {
        app.begin_shutdown();
        let state = snapshot();
        let _ = terminal.draw(|frame| render(frame, &mut app, state.snapshot()));
        app.wait_for_shutdown_completion();
    }
    let cursor_result = terminal.show_cursor();
    let exit = run_result?;
    cursor_result?;
    Ok(exit)
}

fn run_monitor_events(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut snapshot: impl FnMut() -> SnapshotUpdate,
    app: &mut MonitorApp,
) -> Result<MonitorExit, anyhow::Error> {
    loop {
        let update = snapshot();
        let state = update.snapshot();
        app.connected = update.connection_error().is_none();
        // A refused local connection is cheap to check, so this runs each tick.
        app.proxy_off = !app.connected
            && app.accounts_mode == providers::Mode::Local
            && matches!(daemon::describe(), daemon::DaemonStatus::Stopped);
        app.clamp_selection(state.sessions.len(), state.recent.len());
        app.tick = app.tick.wrapping_add(1);
        finish_saving(app);
        if app.page == Page::Accounts {
            app.providers.observe(state);
            app.providers.refresh();
        }
        terminal.draw(|frame| {
            render(frame, app, state);
            // The footer carries the start key while the proxy is off, so no banner covers it.
            if let Some(error) = update.connection_error().filter(|_| !app.proxy_off) {
                let area = frame.area();
                let banner = Rect::new(
                    area.x,
                    area.y + area.height.saturating_sub(1),
                    area.width,
                    1,
                );
                frame.render_widget(
                    Paragraph::new(format!("Reconnecting; showing last snapshot: {error}"))
                        .style(Style::default().fg(YELLOW).bg(BG)),
                    banner,
                );
            }
        })?;
        if app.shutdown_is_complete() {
            return Ok(MonitorExit::ShutdownComplete);
        }
        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    _ if app.phase == MonitorPhase::Running
                        && !app.show_help
                        && !app.show_setup
                        && app.page == Page::Settings
                        && !key.modifiers.contains(KeyModifiers::CONTROL)
                        && app
                            .settings
                            .as_ref()
                            .is_some_and(|settings| settings.captures_input()) =>
                    {
                        run_settings_key(terminal, app, key)?;
                    }
                    // A name being typed owns every key, `q` and `?` included. Only
                    // Ctrl-C still leaves, like in a settings field.
                    _ if app.phase == MonitorPhase::Running
                        && !app.show_help
                        && !app.show_setup
                        && app.page == Page::Accounts
                        && app.providers.renaming()
                        && !(key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL)) =>
                    {
                        if let Some(action) = app.providers.press_rename(key) {
                            run_account_action(terminal, app, state, action)?;
                        }
                    }
                    KeyCode::Char('q') if app.is_attached() => return Ok(MonitorExit::Detached),
                    KeyCode::Char('c')
                        if app.is_attached() && key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        return Ok(MonitorExit::Detached);
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if app.handle_ctrl_c() {
                            return Ok(MonitorExit::ForceQuit);
                        }
                    }
                    _ if app.phase == MonitorPhase::ShuttingDown => {}
                    KeyCode::Char('y') if app.phase == MonitorPhase::ConfirmingShutdown => {
                        app.begin_shutdown()
                    }
                    KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q')
                        if app.phase == MonitorPhase::ConfirmingShutdown =>
                    {
                        app.cancel_shutdown_confirmation()
                    }
                    _ if app.phase == MonitorPhase::ConfirmingShutdown => {}
                    KeyCode::Esc => {
                        if let Some(exit) = app.handle_escape() {
                            return Ok(exit);
                        }
                    }
                    KeyCode::Char('q') => app.request_shutdown_confirmation(),
                    KeyCode::Char('?') => app.show_help = !app.show_help,
                    _ if app.show_help => {}
                    KeyCode::Char('b') => app.show_setup = !app.show_setup,
                    _ if app.show_setup => {}
                    _ if app.page == Page::Accounts && app.providers.menu_open() => {
                        if let Some(action) = app.providers.press(key.code) {
                            run_account_action(terminal, app, state, action)?;
                        }
                    }
                    KeyCode::Char('s') if app.proxy_off => {
                        // The result shows on Accounts, so a failure is never missed.
                        app.page = Page::Accounts;
                        run_account_action(terminal, app, state, controls::Action::StartProxy)?;
                    }
                    KeyCode::Char('1') | KeyCode::Char('p') => app.open_page(Page::Accounts),
                    KeyCode::Char('2') => app.open_page(Page::Activity),
                    KeyCode::Char('3') => app.open_page(Page::Settings),
                    KeyCode::Tab => app.open_page(app.page.next()),
                    KeyCode::BackTab => app.open_page(app.page.previous()),
                    _ if app.page == Page::Settings => {
                        run_settings_key(terminal, app, key)?;
                    }
                    _ if app.page == Page::Accounts => {
                        if let Some(action) = app.providers.press(key.code) {
                            run_account_action(terminal, app, state, action)?;
                        }
                    }
                    KeyCode::Down => app.move_down(state.sessions.len(), state.recent.len(), true),
                    KeyCode::Char('j') => {
                        app.move_down(state.sessions.len(), state.recent.len(), false)
                    }
                    KeyCode::Up => app.move_up(state.sessions.len(), state.recent.len(), true),
                    KeyCode::Char('k') => {
                        app.move_up(state.sessions.len(), state.recent.len(), false)
                    }
                    KeyCode::Left => app.detail = None,
                    KeyCode::Enter | KeyCode::Right => {
                        app.detail = match app.focus {
                            FocusPane::Sessions if !state.sessions.is_empty() => {
                                Some(DetailView::Session)
                            }
                            FocusPane::Recent if !state.recent.is_empty() => {
                                Some(DetailView::Request)
                            }
                            _ => None,
                        }
                    }
                    _ => {}
                },
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FocusPane {
    Sessions,
    Recent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    Accounts,
    Activity,
    Settings,
}

impl Page {
    fn next(self) -> Self {
        match self {
            Self::Accounts => Self::Activity,
            Self::Activity => Self::Settings,
            Self::Settings => Self::Accounts,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Accounts => Self::Settings,
            Self::Activity => Self::Accounts,
            Self::Settings => Self::Activity,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DetailView {
    Session,
    Request,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MonitorPhase {
    Running,
    ConfirmingShutdown,
    ShuttingDown,
}

struct MonitorApp {
    listen_url: String,
    setup_text: String,
    show_setup: bool,
    show_help: bool,
    page: Page,
    accounts_mode: providers::Mode,
    providers: ProvidersPanel,
    settings: Option<config_keys::SettingsPanel>,
    detail: Option<DetailView>,
    focus: FocusPane,
    selected: usize,
    recent_selected: usize,
    connected: bool,
    /// A local monitor whose proxy is not running: it offers to start it.
    proxy_off: bool,
    /// A change saving behind the monitor, and its answer when it ends.
    saving: Option<mpsc::Receiver<(controls::Action, anyhow::Result<String>)>>,
    tick: usize,
    phase: MonitorPhase,
    shutdown: Option<oneshot::Sender<()>>,
    shutdown_complete: Option<mpsc::Receiver<()>>,
}

impl MonitorApp {
    fn open_page(&mut self, page: Page) {
        if page == Page::Settings
            && self.page != page
            && let Some(settings) = &mut self.settings
        {
            settings.reload();
        }
        self.page = page;
    }

    fn is_attached(&self) -> bool {
        self.shutdown.is_none() && self.shutdown_complete.is_none()
    }
    fn handle_escape(&mut self) -> Option<MonitorExit> {
        if self.show_help {
            self.show_help = false;
        } else if self.show_setup {
            self.show_setup = false;
        } else if self.page == Page::Accounts && self.providers.close_menu() {
            // Back from an action goes to the same account.
        } else if self.page == Page::Settings {
            self.page = Page::Accounts;
        } else if self.page != Page::Activity || self.detail.take().is_none() {
            if self.is_attached() {
                return Some(MonitorExit::Detached);
            }
            self.request_shutdown_confirmation();
        }
        None
    }

    fn handle_ctrl_c(&mut self) -> bool {
        if self.phase == MonitorPhase::ShuttingDown {
            true
        } else {
            self.begin_shutdown();
            false
        }
    }

    fn request_shutdown_confirmation(&mut self) {
        if self.phase == MonitorPhase::Running {
            self.phase = MonitorPhase::ConfirmingShutdown;
        }
    }

    fn cancel_shutdown_confirmation(&mut self) {
        if self.phase == MonitorPhase::ConfirmingShutdown {
            self.phase = MonitorPhase::Running;
        }
    }

    fn begin_shutdown(&mut self) {
        if self.phase == MonitorPhase::ShuttingDown {
            return;
        }
        self.phase = MonitorPhase::ShuttingDown;
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }

    fn shutdown_is_complete(&self) -> bool {
        let Some(shutdown_complete) = &self.shutdown_complete else {
            return self.phase == MonitorPhase::ShuttingDown;
        };
        match shutdown_complete.try_recv() {
            Ok(()) | Err(mpsc::TryRecvError::Disconnected) => true,
            Err(mpsc::TryRecvError::Empty) => false,
        }
    }

    fn wait_for_shutdown_completion(&self) {
        if !self.shutdown_is_complete()
            && let Some(shutdown_complete) = &self.shutdown_complete
        {
            let _ = shutdown_complete.recv();
        }
    }

    fn clamp_selection(&mut self, sessions: usize, recent: usize) {
        self.selected = self.selected.min(sessions.saturating_sub(1));
        self.recent_selected = self.recent_selected.min(recent.saturating_sub(1));
    }

    fn move_down(&mut self, sessions: usize, recent: usize, switch_panes: bool) {
        match self.focus {
            FocusPane::Sessions => {
                if switch_panes && self.selected >= sessions.saturating_sub(1) && recent > 0 {
                    self.focus = FocusPane::Recent;
                    self.recent_selected = 0;
                } else {
                    self.selected = self
                        .selected
                        .saturating_add(1)
                        .min(sessions.saturating_sub(1));
                }
            }
            FocusPane::Recent => {
                self.recent_selected = self
                    .recent_selected
                    .saturating_add(1)
                    .min(recent.saturating_sub(1));
            }
        }
    }

    fn move_up(&mut self, sessions: usize, recent: usize, switch_panes: bool) {
        match self.focus {
            FocusPane::Sessions => self.selected = self.selected.saturating_sub(1),
            FocusPane::Recent => {
                if switch_panes && self.recent_selected == 0 && sessions > 0 {
                    self.focus = FocusPane::Sessions;
                    self.selected = sessions.saturating_sub(1);
                } else {
                    self.recent_selected = self
                        .recent_selected
                        .saturating_sub(1)
                        .min(recent.saturating_sub(1));
                }
            }
        }
    }
}

impl Drop for MonitorApp {
    fn drop(&mut self) {
        self.begin_shutdown();
    }
}

pub(crate) struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

pub(crate) fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>, anyhow::Error> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    Ok(terminal)
}

pub(crate) fn suspend_terminal() -> anyhow::Result<()> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    Ok(())
}

pub(crate) fn resume_terminal(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) -> anyhow::Result<()> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    terminal.clear()?;
    Ok(())
}

/// Hands the screen to a question (a sign-in, a model choice) without leaving
/// the alternate screen. Leaving it would show the shell's scrollback for a
/// moment between the monitor and the question, and again on the way back.
fn pause_for_questions() -> anyhow::Result<()> {
    disable_raw_mode()?;
    execute!(
        io::stdout(),
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::cursor::MoveTo(0, 0),
        crossterm::cursor::Show
    )?;
    Ok(())
}

fn resume_after_questions(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> anyhow::Result<()> {
    enable_raw_mode()?;
    terminal.clear()?;
    Ok(())
}

fn run_settings_key(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut MonitorApp,
    key: event::KeyEvent,
) -> anyhow::Result<()> {
    let Some(settings) = &mut app.settings else {
        return Ok(());
    };
    match settings.press(key) {
        config_keys::SettingsOutcome::Stay => {}
        config_keys::SettingsOutcome::Quit => app.open_page(Page::Accounts),
        config_keys::SettingsOutcome::OpenFile(at) => {
            suspend_terminal()?;
            let result = settings.open_file(at);
            resume_terminal(terminal)?;
            settings.edited(result);
        }
    }
    Ok(())
}

fn run_account_action(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut MonitorApp,
    state: &MonitorSnapshot,
    action: controls::Action,
) -> anyhow::Result<()> {
    match app.accounts_mode {
        providers::Mode::Remote => return Ok(()),
        providers::Mode::Demo => {
            app.providers.preview_action(&action);
            return Ok(());
        }
        providers::Mode::Local => {}
    }
    let starting = action == controls::Action::StartProxy;
    if !app.connected && !starting {
        let reason = if app.proxy_off {
            "The proxy is off. Press s to start it, then change accounts or routing"
        } else {
            "The proxy connection is lost. Wait for it to reconnect before changing accounts or routing"
        };
        app.providers.finish_action(Err(anyhow::anyhow!(reason)));
        return Ok(());
    }
    if !action.needs_terminal() {
        if matches!(
            action,
            controls::Action::EnableRouting | controls::Action::DisableRouting
        ) {
            start_saving(app, action);
        } else {
            let result = action.run();
            finish_account_action(app, &action, result);
        }
        return Ok(());
    }
    terminal.draw(|frame| {
        render(frame, app, state);
        let area = frame.area();
        frame.render_widget(
            Paragraph::new(if starting {
                " Starting the proxy..."
            } else {
                " Saving..."
            })
            .style(Style::default().fg(TEAL).bg(BG)),
            Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
        );
    })?;
    pause_for_questions()?;
    let result = action.run();
    // Always restore the monitor, including a cancelled sign-in.
    resume_after_questions(terminal)?;
    finish_account_action(app, &action, result);
    Ok(())
}

/// Runs a change that never asks a question behind the monitor, so keys, tabs
/// and live requests keep working while it saves. One change runs at a time:
/// while one is running, the notice already says "Saving...".
fn start_saving(app: &mut MonitorApp, action: controls::Action) {
    if app.saving.is_some() {
        return;
    }
    app.providers.show_saving();
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        let result = action.run();
        let _ = send.send((action, result));
    });
    app.saving = Some(receive);
}

/// Picks up the answer of a change that saved behind the monitor.
fn finish_saving(app: &mut MonitorApp) {
    let Some(receive) = &app.saving else {
        return;
    };
    match receive.try_recv() {
        Ok((action, result)) => {
            app.saving = None;
            finish_account_action(app, &action, result);
        }
        Err(mpsc::TryRecvError::Empty) => {}
        Err(mpsc::TryRecvError::Disconnected) => {
            app.saving = None;
            app.providers.finish_action(Err(anyhow::anyhow!(
                "The change stopped before it finished. Press r to refresh and see what was saved"
            )));
        }
    }
}

fn finish_account_action(
    app: &mut MonitorApp,
    action: &controls::Action,
    result: anyhow::Result<String>,
) {
    if result.is_ok() {
        match action {
            controls::Action::RemoveAccount { provider, id } => {
                app.providers.forget_account(provider, id)
            }
            // The row shows its new name at once. The next read confirms it.
            controls::Action::RenameAccount { provider, id, name } => {
                let _ = app.providers.rename_account(provider, id, name);
            }
            _ => {}
        }
        // The list shows the new order at once. The next read confirms it.
        app.providers.apply_pool_action(action);
        if matches!(
            action,
            controls::Action::StartProxy
                | controls::Action::EnableRouting
                | controls::Action::DisableRouting
                | controls::Action::SignIn { .. }
        ) {
            app.providers.reload_saved();
        }
    }
    if *action == controls::Action::StartProxy {
        app.providers.finish_start(result);
    } else {
        app.providers.finish_action(result);
    }
}

fn render(frame: &mut ratatui::Frame<'_>, app: &mut MonitorApp, state: &MonitorSnapshot) {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(BG)), area);

    let [header, tabs, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(area);
    render_header(frame, header, app, state);
    render_tabs(frame, tabs, app.page);
    match app.page {
        Page::Accounts => app.providers.render(frame, body),
        Page::Activity => render_activity(frame, body, app, state),
        Page::Settings => {
            if let Some(settings) = &mut app.settings {
                settings.render(frame, body);
            } else {
                frame.render_widget(
                    Paragraph::new("Settings belong to the computer running the proxy. This --url connection is read-only.\n\nRun cc-proxy open on that computer to change settings. Press 2 for activity.")
                        .style(Style::default().fg(DIM_WHITE))
                        .wrap(Wrap { trim: false })
                        .block(panel("Remote proxy", false)),
                    body,
                );
            }
        }
    }
    render_footer(frame, footer, app);

    if app.show_setup {
        render_setup_overlay(frame, area, &app.setup_text);
    }
    if app.show_help {
        render_help_overlay(frame, area, app.is_attached());
    }
    match app.phase {
        MonitorPhase::Running => {}
        MonitorPhase::ConfirmingShutdown => render_shutdown_confirmation(frame, area),
        MonitorPhase::ShuttingDown => render_shutdown_overlay(frame, area, app.tick),
    }
}

fn render_tabs(frame: &mut ratatui::Frame<'_>, area: Rect, page: Page) {
    let tabs = [
        (Page::Accounts, " 1 Accounts "),
        (Page::Activity, " 2 Activity "),
        (Page::Settings, " 3 Settings "),
    ];
    frame.render_widget(
        Paragraph::new(Line::from(
            tabs.into_iter()
                .map(|(tab, label)| {
                    Span::styled(
                        label,
                        if page == tab {
                            Style::default()
                                .fg(TEAL)
                                .bg(SELECTED_BG)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(DIM_WHITE)
                        },
                    )
                })
                .collect::<Vec<_>>(),
        )),
        area,
    );
}

fn render_activity(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &mut MonitorApp,
    state: &MonitorSnapshot,
) {
    let root = Layout::vertical([
        Constraint::Percentage(30),
        Constraint::Percentage(20),
        Constraint::Percentage(25),
        Constraint::Percentage(25),
    ])
    .split(area);
    match app.detail {
        Some(DetailView::Session) => render_session_detail(frame, root[0], state, app.selected),
        Some(DetailView::Request) => {
            render_request_detail(frame, root[0], state, app.recent_selected)
        }
        None => render_sessions(
            frame,
            root[0],
            &state.sessions,
            app.selected,
            app.focus == FocusPane::Sessions,
            state.snapshot_at,
        ),
    }
    render_active(frame, root[1], &state.active, app.tick);
    render_recent(
        frame,
        root[2],
        &state.recent,
        app.recent_selected,
        app.focus == FocusPane::Recent,
        state.snapshot_at,
    );
    render_events(frame, root[3], &state.recent);
}

fn render_header(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &MonitorApp,
    state: &MonitorSnapshot,
) {
    let (face, mood) = header_face(app, state);
    if app.page != Page::Activity {
        let status = if app.phase == MonitorPhase::ShuttingDown {
            "Shutting down"
        } else if app.proxy_off {
            "Proxy off. Press s to start it"
        } else if !app.connected {
            "Disconnected: retrying"
        } else if app.accounts_mode == providers::Mode::Demo {
            "Demo: no proxy connection"
        } else if mood == Mood::Unsure {
            "Recent request failed (2 Activity)"
        } else if state.active.is_empty() {
            "Ready for requests"
        } else {
            "Serving requests"
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!(" {face} cc-proxy "),
                    Style::default()
                        .fg(BG)
                        .bg(mood.color())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    if app.proxy_off {
                        format!("  {status}")
                    } else {
                        format!(
                            "  {status}  {} active  up {}",
                            state.active.len(),
                            ui::duration(state.uptime)
                        )
                    },
                    Style::default().fg(WHITE),
                ),
            ])),
            area,
        );
        return;
    }
    let label = Style::default().fg(DIM);
    let value = Style::default().fg(WHITE).add_modifier(Modifier::BOLD);
    let left = Line::from(vec![
        Span::styled(
            format!(" {face} cc-proxy "),
            Style::default()
                .fg(BG)
                .bg(mood.color())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}", app.listen_url),
            Style::default().fg(DIM_WHITE),
        ),
        Span::styled("  up ", label),
        Span::styled(ui::duration(state.uptime), value),
        Span::styled("  sessions ", label),
        Span::styled(state.sessions.len().to_string(), value),
        Span::styled("  active ", label),
        Span::styled(state.active.len().to_string(), value),
    ]);

    let samples = state
        .sessions
        .iter()
        .flat_map(|session| session.output_token_samples.iter().copied())
        .collect::<Vec<_>>();
    let streaming: f64 = state
        .active
        .iter()
        .map(|request| match request.rate() {
            Throughput::TokensPerSecond(rate) => rate,
            _ => 0.0,
        })
        .sum();
    let rate = if state.active.is_empty() {
        Span::styled("idle ", label)
    } else {
        Span::styled(format!("{streaming:.0} tok/s "), value)
    };
    let right = Line::from(vec![
        Span::styled("tokens ", label),
        Span::styled(
            token_sparkline(&samples, HEADER_SPARKLINE_BUCKETS, state.snapshot_at),
            Style::default().fg(TEAL),
        ),
        Span::raw("  "),
        rate,
    ]);

    let left_width = left.width() as u16;
    let right_width = right.width() as u16;
    frame.render_widget(Block::default().style(Style::default().bg(BG)), area);
    frame.render_widget(Paragraph::new(left), area);
    if area.width >= left_width + right_width + 2 {
        frame.render_widget(
            Paragraph::new(right).alignment(Alignment::Right),
            Rect {
                x: area.x + area.width - right_width,
                width: right_width,
                ..area
            },
        );
    }
}

/// The face in the header shows how the proxy is doing at a glance: it
/// talks while requests stream, looks unsure for a few seconds after an
/// error, sleeps while shutting down, and blinks now and then so you can
/// tell the dashboard is live.
fn header_face(app: &MonitorApp, state: &MonitorSnapshot) -> (&'static str, Mood) {
    let recent_error = state.recent.iter().any(|request| {
        !error_indicator(request).is_empty()
            && state
                .snapshot_at
                .duration_since(request.finished_at)
                .is_ok_and(|age| age < RECENT_ERROR)
    });
    if app.phase == MonitorPhase::ShuttingDown || app.proxy_off {
        (Mood::Asleep.face(), Mood::Asleep)
    } else if !app.connected || recent_error {
        (Mood::Unsure.face(), Mood::Unsure)
    } else if !state.active.is_empty() {
        (["•ω•", "•o•"][app.tick % 2], Mood::Awake)
    } else if app.tick.is_multiple_of(24) {
        (Mood::Asleep.face(), Mood::Awake)
    } else {
        (Mood::Awake.face(), Mood::Awake)
    }
}

fn panel(title: &'static str, focused: bool) -> Block<'static> {
    let color = if focused { TEAL } else { SEPARATOR };
    Block::default()
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(if focused { TEAL } else { DIM_WHITE })
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color))
        .style(Style::default().bg(PANEL_BG))
}

fn table_header_aligned(
    cells: impl IntoIterator<Item = (&'static str, Alignment)>,
) -> Row<'static> {
    Row::new(
        cells
            .into_iter()
            .map(|(cell, alignment)| {
                Cell::from(
                    Line::from(Span::styled(cell, Style::default().fg(TEAL))).alignment(alignment),
                )
            })
            .collect::<Vec<_>>(),
    )
    .style(Style::default().add_modifier(Modifier::BOLD))
}

fn render_empty_table_state(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    title: &'static str,
    focused: bool,
    message: &str,
    hint: &str,
) {
    frame.render_widget(panel(title, focused), area);
    let content = Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    if content.width == 0 || content.height == 0 {
        return;
    }

    let width = content.width.into();
    let mut lines = vec![Line::from(Span::styled(
        ellipsize(message, width),
        Style::default().fg(DIM_WHITE),
    ))];
    // A panel too short for both keeps the message.
    if content.height >= 2 {
        let plain = hint.replace('`', "");
        lines.push(if plain.chars().count() <= width {
            Line::from(
                hint.split('`')
                    .enumerate()
                    .map(|(index, part)| {
                        let color = if index % 2 == 1 { TEAL } else { DIM };
                        Span::styled(part.to_string(), Style::default().fg(color))
                    })
                    .collect::<Vec<_>>(),
            )
        } else {
            Line::from(Span::styled(
                ellipsize(&plain, width),
                Style::default().fg(DIM),
            ))
        });
    }
    let height = lines.len() as u16;
    let block = Rect {
        y: content.y + content.height.saturating_sub(height) / 2,
        height,
        ..content
    };
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .style(Style::default().bg(PANEL_BG)),
        block,
    );
}

fn muted_cell(value: impl Into<String>) -> Cell<'static> {
    Cell::from(Span::styled(value.into(), Style::default().fg(DIM)))
}

fn text_cell(value: impl Into<String>) -> Cell<'static> {
    Cell::from(Span::styled(value.into(), Style::default().fg(DIM_WHITE)))
}

fn model_cell(value: Option<&str>, account: Option<&str>, width: usize) -> Cell<'static> {
    let model = value.unwrap_or("-");
    let text = account.map_or_else(
        || model.to_string(),
        |account| {
            format!(
                "{account} / {}",
                model.rsplit(" → ").next().unwrap_or(model)
            )
        },
    );
    text_cell(ellipsize(&text, width))
}

fn table_column_width(area: Rect, widths: &[Constraint], column: usize) -> usize {
    let table_width = area.width.saturating_sub(2);
    Layout::horizontal(widths.to_vec())
        .spacing(1)
        .split(Rect::new(0, 0, table_width, 1))
        .get(column)
        .map_or(0, |rect| usize::from(rect.width))
}

fn ellipsize(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_string();
    }
    if width == 0 {
        return String::new();
    }

    value
        .chars()
        .take(width.saturating_sub(1))
        .chain(std::iter::once('…'))
        .collect()
}

fn display_session_id(session_id: Option<&str>) -> &str {
    let Some(session_id) = session_id.filter(|value| !value.is_empty()) else {
        return "no-session";
    };
    if uuid::Uuid::parse_str(session_id).is_ok() {
        return session_id
            .split_once('-')
            .map_or(session_id, |(first, _)| first);
    }
    session_id
}

fn number_cell(value: impl Into<String>) -> Cell<'static> {
    Cell::from(
        Line::from(Span::styled(value.into(), Style::default().fg(DIM_WHITE)))
            .alignment(Alignment::Right),
    )
}

fn status_cell(value: &str) -> Cell<'static> {
    Cell::from(Span::styled(value.to_string(), status_style(value)))
}

fn status_style(value: &str) -> Style {
    Style::default().fg(status_color(value))
}

fn status_color(value: &str) -> Color {
    match value {
        "completed" => GREEN,
        "streaming" => TEAL,
        "compacting" => PURPLE,
        "failed" => RED,
        "upstream" => BLUE,
        "selected" | "started" => YELLOW,
        _ => DIM_WHITE,
    }
}

fn http_status_style(status: Option<u16>) -> Style {
    Style::default().fg(http_status_color(status))
}

fn http_status_color(status: Option<u16>) -> Color {
    match status {
        Some(200..=299) => GREEN,
        Some(400..=499) => YELLOW,
        Some(500..=599) => RED,
        Some(_) => DIM_WHITE,
        None => DIM,
    }
}

fn rate_cell(value: String) -> Cell<'static> {
    let color = if value.contains("tok/s") {
        TEAL
    } else if value == "-" {
        DIM
    } else {
        DIM_WHITE
    };
    Cell::from(
        Line::from(Span::styled(value, Style::default().fg(color))).alignment(Alignment::Right),
    )
}

fn provider_cell(value: Option<&str>) -> Cell<'static> {
    let value = value.unwrap_or("-");
    let color = if value == "-" {
        DIM
    } else {
        ui::provider_color(value)
    };
    Cell::from(Span::styled(value.to_string(), Style::default().fg(color)))
}

fn detail_cell(value: &str) -> Cell<'static> {
    if value.is_empty() || value == "-" {
        Cell::from(Span::styled("", Style::default().fg(DIM)))
    } else {
        Cell::from(Span::styled(value.to_string(), Style::default().fg(YELLOW)))
    }
}

fn error_indicator(request: &CompletedSnapshot) -> &'static str {
    if request.status == crate::monitor::RequestStatus::Failed
        || request.http_status.is_some_and(|status| status >= 400)
        || request
            .error
            .as_deref()
            .is_some_and(|error| !error.is_empty())
    {
        "!"
    } else {
        ""
    }
}

fn compact_tokens(tokens: u64) -> String {
    crate::monitor::accounting::compact_tokens(tokens)
}

/// A priced total reads as a number, an unpriced one as a quiet note.
fn cost_color(usage: &crate::monitor::UsageTotals) -> Color {
    if usage.cost_label().is_some() {
        WHITE
    } else {
        DIM_WHITE
    }
}

fn token_value(value: Option<u64>) -> String {
    value.map(compact_tokens).unwrap_or_else(|| "-".to_string())
}

fn spinner(tick: usize) -> &'static str {
    const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    FRAMES[tick % FRAMES.len()]
}

fn sparkline_bucket(timestamp: SystemTime) -> u64 {
    timestamp
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
        / SESSION_TOKEN_BUCKET_SECS
}

fn token_sparkline(
    samples: &[(SystemTime, u64)],
    width: usize,
    reference_at: SystemTime,
) -> String {
    const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

    if width == 0 {
        return String::new();
    }

    let mut buckets = HashMap::<u64, u64>::new();
    for (timestamp, tokens) in samples {
        let bucket = sparkline_bucket(*timestamp);
        let total = buckets.entry(bucket).or_default();
        *total = total.saturating_add(*tokens);
    }

    let current_bucket = sparkline_bucket(reference_at);
    let first_bucket = current_bucket.saturating_sub(width.saturating_sub(1) as u64);
    (first_bucket..=current_bucket)
        .map(|bucket| {
            let value = buckets.get(&bucket).copied().unwrap_or(0);
            if value == 0 {
                return ' ';
            }
            let scaled = value.min(SESSION_SPARKLINE_MAX_TOKENS);
            let level = (u128::from(scaled) * LEVELS.len() as u128)
                .div_ceil(u128::from(SESSION_SPARKLINE_MAX_TOKENS))
                .saturating_sub(1) as usize;
            LEVELS[level]
        })
        .collect()
}

fn token_sparkline_line(
    samples: &[(SystemTime, u64)],
    width: usize,
    reference_at: SystemTime,
) -> Line<'static> {
    let mut sparkline = token_sparkline(samples, width, reference_at);
    let current = sparkline
        .pop()
        .map_or_else(String::new, |value| value.to_string());
    Line::from(vec![
        Span::styled(sparkline, Style::default().fg(BLUE)),
        Span::styled(current, Style::default().fg(DIM)),
    ])
}

fn column_constraints<K>(columns: &[ColumnSpec<K>]) -> Vec<Constraint> {
    columns.iter().map(ColumnSpec::constraint).collect()
}

fn column_header<K>(columns: &[ColumnSpec<K>]) -> Row<'static> {
    table_header_aligned(
        columns
            .iter()
            .map(|column| (column.header, column.alignment)),
    )
}

fn target_cell(
    provider: Option<&str>,
    model: Option<&str>,
    account: Option<&str>,
    width: usize,
) -> Cell<'static> {
    let provider = provider.unwrap_or("-");
    let model = model.unwrap_or("-");
    let target = account.map_or_else(
        || format!("{provider}/{model}"),
        |account| format!("{provider}/{account} / {model}"),
    );
    text_cell(ellipsize(&target, width))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionColumn {
    Marker,
    Id,
    Project,
    Active,
    Requests,
    Failures,
    Counts,
    Provider,
    Model,
    Target,
    Effort,
    Input,
    Output,
    Rate,
    Activity,
    Status,
}

fn session_columns(tier: LayoutTier, show_full_sparkline: bool) -> Vec<ColumnSpec<SessionColumn>> {
    use SessionColumn as C;
    match (tier, show_full_sparkline) {
        (LayoutTier::Wide, true) => vec![
            ColumnSpec::fixed(C::Marker, "", Alignment::Left, 1),
            ColumnSpec::fixed(C::Id, "ID", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_WIDE_WIDTH),
            ColumnSpec::fixed(C::Active, "A", Alignment::Right, COUNT_WIDTH),
            ColumnSpec::fixed(C::Requests, "R", Alignment::Right, COUNT_WIDTH),
            ColumnSpec::fixed(C::Failures, "F", Alignment::Right, COUNT_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_WIDE_WIDTH),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::flex(C::Activity, "Tokens/10s · 4k", Alignment::Left, 1),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
        ],
        (LayoutTier::Expanded | LayoutTier::Wide, _) => vec![
            ColumnSpec::fixed(C::Marker, "", Alignment::Left, 1),
            ColumnSpec::fixed(C::Id, "ID", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_MEDIUM_WIDTH),
            ColumnSpec::fixed(C::Counts, "A/R/F", Alignment::Right, 7),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_NARROW_WIDTH),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::flex(C::Activity, "Tokens/10s", Alignment::Left, 1),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
        ],
        (LayoutTier::Medium, _) => vec![
            ColumnSpec::fixed(C::Marker, "", Alignment::Left, 1),
            ColumnSpec::fixed(C::Id, "ID", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_MEDIUM_WIDTH),
            ColumnSpec::fixed(C::Counts, "A/R/F", Alignment::Right, 7),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::flex(C::Model, "Model", Alignment::Left, 1),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Activity, "Tok/10s", Alignment::Left, 8),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
        ],
        (LayoutTier::Narrow, _) => vec![
            ColumnSpec::fixed(C::Marker, "", Alignment::Left, 1),
            ColumnSpec::fixed(C::Id, "ID", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, 10),
            ColumnSpec::fixed(C::Counts, "A/R/F", Alignment::Right, 7),
            ColumnSpec::flex(C::Target, "Target", Alignment::Left, 1),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Activity, "Trend", Alignment::Left, 6),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
        ],
        (LayoutTier::Emergency, _) => vec![
            ColumnSpec::fixed(C::Marker, "", Alignment::Left, 1),
            ColumnSpec::fixed(C::Id, "ID", Alignment::Left, ID_WIDTH),
            ColumnSpec::flex(C::Target, "Target", Alignment::Left, 1),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
        ],
    }
}

fn render_sessions(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    sessions: &[SessionSnapshot],
    selected: usize,
    focused: bool,
    snapshot_at: SystemTime,
) {
    if sessions.is_empty() {
        render_empty_table_state(
            frame,
            area,
            "Sessions",
            focused,
            "No sessions yet",
            "Open Claude Code and its requests group here.",
        );
        return;
    }

    let tier = LayoutTier::for_outer_width(area.width);
    let show_full_sparkline = tier == LayoutTier::Wide && area.width >= SESSION_SPARKLINE_MIN_WIDTH;
    let columns = session_columns(tier, show_full_sparkline);
    let widths = column_constraints(&columns);
    let rows = sessions.iter().enumerate().map(|(index, session)| {
        let cells = columns
            .iter()
            .enumerate()
            .map(|(column_index, column)| {
                let width = table_column_width(area, &widths, column_index);
                match column.key {
                    SessionColumn::Marker => {
                        let marker = if focused && index == selected {
                            ">"
                        } else {
                            " "
                        };
                        Cell::from(Span::styled(marker, Style::default().fg(TEAL)))
                    }
                    SessionColumn::Id => {
                        text_cell(display_session_id(session.session_id.as_deref()))
                    }
                    SessionColumn::Project => {
                        text_cell(ellipsize(session.project.as_deref().unwrap_or("-"), width))
                    }
                    SessionColumn::Active => number_cell(session.active_count.to_string()),
                    SessionColumn::Requests => number_cell(session.request_count.to_string()),
                    SessionColumn::Failures => number_cell(session.failure_count.to_string()),
                    SessionColumn::Counts => number_cell(format!(
                        "{}/{}/{}",
                        session.active_count, session.request_count, session.failure_count
                    )),
                    SessionColumn::Provider => provider_cell(session.provider.as_deref()),
                    SessionColumn::Model => {
                        model_cell(session.model.as_deref(), session.account.as_deref(), width)
                    }
                    SessionColumn::Target => target_cell(
                        session.provider.as_deref(),
                        session.model.as_deref(),
                        session.account.as_deref(),
                        width,
                    ),
                    SessionColumn::Effort => text_cell(session.effort.as_deref().unwrap_or("-")),
                    SessionColumn::Input => number_cell(compact_tokens(session.input_tokens)),
                    SessionColumn::Output => number_cell(compact_tokens(session.output_tokens)),
                    SessionColumn::Rate => rate_cell(session.rate().label()),
                    SessionColumn::Activity => Cell::from(token_sparkline_line(
                        &session.output_token_samples,
                        width,
                        snapshot_at,
                    )),
                    SessionColumn::Status => status_cell(&session.last_status),
                }
            })
            .collect::<Vec<_>>();
        Row::new(cells).style(if index == selected {
            Style::default().bg(SELECTED_BG)
        } else {
            Style::default().bg(PANEL_BG)
        })
    });
    let table = Table::new(rows, widths.clone())
        .header(column_header(&columns))
        .block(panel("Sessions", focused));
    let mut table_state = TableState::default().with_selected(Some(selected));
    frame.render_stateful_widget(table, area, &mut table_state);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActiveColumn {
    Started,
    Status,
    Project,
    Session,
    Provider,
    Model,
    Target,
    Effort,
    Endpoint,
    Input,
    Output,
    Rate,
    Elapsed,
}

fn active_columns(tier: LayoutTier) -> Vec<ColumnSpec<ActiveColumn>> {
    use ActiveColumn as C;
    match tier {
        LayoutTier::Wide => vec![
            ColumnSpec::fixed(C::Started, "Started", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_WIDE_WIDTH),
            ColumnSpec::fixed(C::Session, "Session", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_WIDE_WIDTH),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::flex(C::Endpoint, "Endpoint", Alignment::Left, 1),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Elapsed, "Elapsed", Alignment::Right, DURATION_WIDTH),
        ],
        LayoutTier::Expanded => vec![
            ColumnSpec::fixed(C::Started, "Started", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_MEDIUM_WIDTH),
            ColumnSpec::fixed(C::Session, "Session", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::flex(C::Model, "Model", Alignment::Left, 1),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Endpoint, "Endpoint", Alignment::Left, ENDPOINT_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Elapsed, "Elapsed", Alignment::Right, DURATION_WIDTH),
        ],
        LayoutTier::Medium => vec![
            ColumnSpec::fixed(C::Started, "Started", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::flex(C::Model, "Model", Alignment::Left, 1),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Endpoint, "Endpoint", Alignment::Left, ENDPOINT_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Elapsed, "Elapsed", Alignment::Right, DURATION_WIDTH),
        ],
        LayoutTier::Narrow => vec![
            ColumnSpec::fixed(C::Started, "Started", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::flex(C::Model, "Model", Alignment::Left, 1),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Elapsed, "Elapsed", Alignment::Right, DURATION_WIDTH),
        ],
        LayoutTier::Emergency => vec![
            ColumnSpec::fixed(C::Started, "Started", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
            ColumnSpec::flex(C::Target, "Target", Alignment::Left, 1),
            ColumnSpec::fixed(C::Elapsed, "Elapsed", Alignment::Right, DURATION_WIDTH),
        ],
    }
}

fn render_active(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    active: &[ActiveSnapshot],
    tick: usize,
) {
    if active.is_empty() {
        render_empty_table_state(
            frame,
            area,
            "Active requests",
            false,
            "Nothing in flight",
            "Requests show here while they stream.",
        );
        return;
    }

    let columns = active_columns(LayoutTier::for_outer_width(area.width));
    let widths = column_constraints(&columns);
    let rows = active.iter().map(|request| {
        let status = if request.status == crate::monitor::RequestStatus::Compacting {
            format!("{}{}", spinner(tick), request.status.label())
        } else {
            format!("{} {}", spinner(tick), request.status.label())
        };
        let cells = columns
            .iter()
            .enumerate()
            .map(|(column_index, column)| {
                let width = table_column_width(area, &widths, column_index);
                match column.key {
                    ActiveColumn::Started => muted_cell(format_system_time(request.started_at)),
                    ActiveColumn::Status => Cell::from(Span::styled(
                        status.clone(),
                        status_style(request.status.label()),
                    )),
                    ActiveColumn::Project => {
                        text_cell(ellipsize(request.project.as_deref().unwrap_or("-"), width))
                    }
                    ActiveColumn::Session => {
                        text_cell(display_session_id(request.session_id.as_deref()))
                    }
                    ActiveColumn::Provider => provider_cell(request.provider.as_deref()),
                    ActiveColumn::Model => {
                        model_cell(request.model.as_deref(), request.account.as_deref(), width)
                    }
                    ActiveColumn::Target => target_cell(
                        request.provider.as_deref(),
                        request.model.as_deref(),
                        request.account.as_deref(),
                        width,
                    ),
                    ActiveColumn::Effort => text_cell(request.effort.as_deref().unwrap_or("-")),
                    ActiveColumn::Endpoint => muted_cell(request.endpoint.label()),
                    ActiveColumn::Input => number_cell(token_value(request.input_tokens)),
                    ActiveColumn::Output => number_cell(token_value(request.output_tokens)),
                    ActiveColumn::Rate => rate_cell(request.rate().label()),
                    ActiveColumn::Elapsed => number_cell(ui::duration(request.elapsed())),
                }
            })
            .collect::<Vec<_>>();
        Row::new(cells).style(Style::default().bg(PANEL_BG))
    });
    let table = Table::new(rows, widths.clone())
        .header(column_header(&columns))
        .block(panel("Active requests", false));
    frame.render_widget(table, area);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RecentColumn {
    Finished,
    Code,
    Project,
    Session,
    Provider,
    Model,
    Target,
    Effort,
    Endpoint,
    Latency,
    Rate,
    Input,
    Output,
    Details,
    Error,
}

fn recent_columns(tier: LayoutTier) -> Vec<ColumnSpec<RecentColumn>> {
    use RecentColumn as C;
    match tier {
        LayoutTier::Wide => vec![
            ColumnSpec::fixed(C::Finished, "Finished", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_WIDE_WIDTH),
            ColumnSpec::fixed(C::Session, "Session", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_WIDE_WIDTH),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Endpoint, "Endpoint", Alignment::Left, ENDPOINT_WIDTH),
            ColumnSpec::fixed(C::Latency, "Latency", Alignment::Right, DURATION_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::flex(C::Details, "Details", Alignment::Left, 1),
        ],
        LayoutTier::Expanded => vec![
            ColumnSpec::fixed(C::Finished, "Finished", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_MEDIUM_WIDTH),
            ColumnSpec::fixed(C::Session, "Session", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::flex(C::Model, "Model", Alignment::Left, 1),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Latency, "Latency", Alignment::Right, DURATION_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Error, "!", Alignment::Right, ERROR_WIDTH),
        ],
        LayoutTier::Medium => vec![
            ColumnSpec::fixed(C::Finished, "Finished", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::flex(C::Model, "Model", Alignment::Left, 1),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Latency, "Latency", Alignment::Right, DURATION_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Error, "!", Alignment::Right, ERROR_WIDTH),
        ],
        LayoutTier::Narrow => vec![
            ColumnSpec::fixed(C::Finished, "Finished", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::flex(C::Model, "Model", Alignment::Left, 1),
            ColumnSpec::fixed(C::Latency, "Latency", Alignment::Right, DURATION_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Error, "!", Alignment::Right, ERROR_WIDTH),
        ],
        LayoutTier::Emergency => vec![
            ColumnSpec::fixed(C::Finished, "Finished", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::flex(C::Target, "Target", Alignment::Left, 1),
            ColumnSpec::fixed(C::Latency, "Latency", Alignment::Right, DURATION_WIDTH),
            ColumnSpec::fixed(C::Error, "!", Alignment::Right, ERROR_WIDTH),
        ],
    }
}

fn http_code_cell(status: Option<u16>) -> Cell<'static> {
    Cell::from(
        Line::from(Span::styled(
            status
                .map(|status| status.to_string())
                .unwrap_or_else(|| "-".to_string()),
            http_status_style(status),
        ))
        .alignment(Alignment::Right),
    )
}

fn render_recent(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    recent: &[CompletedSnapshot],
    selected: usize,
    focused: bool,
    snapshot_at: SystemTime,
) {
    if recent.is_empty() {
        render_empty_table_state(
            frame,
            area,
            "Recent requests",
            focused,
            "No requests yet",
            "Finished requests show here, newest first.",
        );
        return;
    }

    let columns = recent_columns(LayoutTier::for_outer_width(area.width));
    let widths = column_constraints(&columns);
    let rows = recent.iter().enumerate().map(|(index, request)| {
        let cells = columns
            .iter()
            .enumerate()
            .map(|(column_index, column)| {
                let width = table_column_width(area, &widths, column_index);
                match column.key {
                    RecentColumn::Finished => muted_cell(format_system_time(request.finished_at)),
                    RecentColumn::Code => http_code_cell(request.http_status),
                    RecentColumn::Project => {
                        text_cell(ellipsize(request.project.as_deref().unwrap_or("-"), width))
                    }
                    RecentColumn::Session => {
                        text_cell(display_session_id(request.session_id.as_deref()))
                    }
                    RecentColumn::Provider => provider_cell(request.provider.as_deref()),
                    RecentColumn::Model => {
                        model_cell(request.model.as_deref(), request.account.as_deref(), width)
                    }
                    RecentColumn::Target => target_cell(
                        request.provider.as_deref(),
                        request.model.as_deref(),
                        request.account.as_deref(),
                        width,
                    ),
                    RecentColumn::Effort => text_cell(request.effort.as_deref().unwrap_or("-")),
                    RecentColumn::Endpoint => muted_cell(request.endpoint.label()),
                    RecentColumn::Latency => number_cell(ui::duration(request.latency)),
                    RecentColumn::Rate => rate_cell(request.rate().label()),
                    RecentColumn::Input => number_cell(token_value(request.input_tokens)),
                    RecentColumn::Output => number_cell(token_value(request.output_tokens)),
                    RecentColumn::Details => detail_cell(request.error.as_deref().unwrap_or("")),
                    RecentColumn::Error => detail_cell(error_indicator(request)),
                }
            })
            .collect::<Vec<_>>();
        Row::new(cells).style(if focused && index == selected {
            Style::default().bg(SELECTED_BG)
        } else {
            Style::default().bg(fresh_bg(request.finished_at, snapshot_at))
        })
    });
    let table = Table::new(rows, widths.clone())
        .header(column_header(&columns))
        .block(panel("Recent requests", focused));
    let mut table_state = TableState::default().with_selected(Some(selected));
    frame.render_stateful_widget(table, area, &mut table_state);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EventColumn {
    Time,
    Code,
    Project,
    Session,
    Provider,
    Model,
    Message,
}

fn event_columns(tier: LayoutTier) -> Vec<ColumnSpec<EventColumn>> {
    use EventColumn as C;
    match tier {
        LayoutTier::Wide => vec![
            ColumnSpec::fixed(C::Time, "Time", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_WIDE_WIDTH),
            ColumnSpec::fixed(C::Session, "Session", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_WIDE_WIDTH),
            ColumnSpec::flex(C::Message, "Message", Alignment::Left, 1),
        ],
        LayoutTier::Expanded => vec![
            ColumnSpec::fixed(C::Time, "Time", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_MEDIUM_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_MEDIUM_WIDTH),
            ColumnSpec::flex(C::Message, "Message", Alignment::Left, 1),
        ],
        LayoutTier::Medium => vec![
            ColumnSpec::fixed(C::Time, "Time", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_MEDIUM_WIDTH),
            ColumnSpec::flex(C::Message, "Message", Alignment::Left, 1),
        ],
        LayoutTier::Narrow => vec![
            ColumnSpec::fixed(C::Time, "Time", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_NARROW_WIDTH),
            ColumnSpec::flex(C::Message, "Message", Alignment::Left, 1),
        ],
        LayoutTier::Emergency => vec![
            ColumnSpec::fixed(C::Time, "Time", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::flex(C::Message, "Message", Alignment::Left, 1),
        ],
    }
}

/// A request that just finished glows teal, fading out over three seconds,
/// so new traffic catches the eye.
fn fresh_bg(finished_at: SystemTime, snapshot_at: SystemTime) -> Color {
    match snapshot_at
        .duration_since(finished_at)
        .map(|age| age.as_millis())
    {
        Ok(0..1000) => Color::Rgb(33, 58, 57),
        Ok(1000..2000) => Color::Rgb(29, 43, 45),
        Ok(2000..3000) => Color::Rgb(25, 33, 36),
        _ => PANEL_BG,
    }
}

fn render_events(frame: &mut ratatui::Frame<'_>, area: Rect, recent: &[CompletedSnapshot]) {
    let events = recent
        .iter()
        .filter(|request| {
            request.status == crate::monitor::RequestStatus::Failed
                || request.http_status.is_some_and(|status| status >= 400)
                || request.error.is_some()
        })
        .take(12)
        .collect::<Vec<_>>();
    if events.is_empty() {
        render_empty_table_state(
            frame,
            area,
            "Events",
            false,
            "No errors",
            "Failed requests and HTTP errors show here.",
        );
        return;
    }

    let columns = event_columns(LayoutTier::for_outer_width(area.width));
    let widths = column_constraints(&columns);
    let rows = events.iter().map(|request| {
        let message = request
            .error
            .as_deref()
            .filter(|error| !error.is_empty())
            .unwrap_or("-");
        let cells = columns
            .iter()
            .enumerate()
            .map(|(column_index, column)| {
                let width = table_column_width(area, &widths, column_index);
                match column.key {
                    EventColumn::Time => muted_cell(format_system_time(request.finished_at)),
                    EventColumn::Code => http_code_cell(request.http_status),
                    EventColumn::Project => {
                        text_cell(ellipsize(request.project.as_deref().unwrap_or("-"), width))
                    }
                    EventColumn::Session => {
                        text_cell(display_session_id(request.session_id.as_deref()))
                    }
                    EventColumn::Provider => provider_cell(request.provider.as_deref()),
                    EventColumn::Model => {
                        model_cell(request.model.as_deref(), request.account.as_deref(), width)
                    }
                    EventColumn::Message => detail_cell(message),
                }
            })
            .collect::<Vec<_>>();
        Row::new(cells).style(Style::default().bg(PANEL_BG))
    });
    let table = Table::new(rows, widths.clone())
        .header(column_header(&columns))
        .block(panel("Events", false));
    frame.render_widget(table, area);
}

fn render_session_detail(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &MonitorSnapshot,
    selected: usize,
) {
    let lines = if let Some(session) = state.sessions.get(selected) {
        vec![
            detail_line("session", session.label(), WHITE),
            detail_line("project", session.project.as_deref().unwrap_or("-"), TEAL),
            detail_line("active requests", session.active_count.to_string(), YELLOW),
            detail_line(
                "total requests",
                session.request_count.to_string(),
                DIM_WHITE,
            ),
            detail_line("failures", session.failure_count.to_string(), RED),
            detail_line("provider", session.provider.as_deref().unwrap_or("-"), TEAL),
            detail_line(
                "account",
                session.account.as_deref().unwrap_or("not reported"),
                WHITE,
            ),
            detail_line("model", session.model.as_deref().unwrap_or("-"), DIM_WHITE),
            detail_line("effort", session.effort.as_deref().unwrap_or("-"), YELLOW),
            detail_line("tokens", session.usage.tokens_label(), DIM_WHITE),
            detail_line(
                "cost",
                session.usage.cost_detail(),
                cost_color(&session.usage),
            ),
            detail_line("rate", session.rate().label(), TEAL),
            detail_line(
                "last status",
                session.last_status.as_str(),
                status_color(&session.last_status),
            ),
        ]
    } else {
        vec![Line::from(Span::styled(
            "No session selected",
            Style::default().fg(DIM),
        ))]
    };
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(PANEL_BG))
            .block(panel("Session detail", true)),
        area,
    );
}

fn render_request_detail(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &MonitorSnapshot,
    selected: usize,
) {
    let lines = if let Some(request) = state.recent.get(selected) {
        let mut lines = vec![
            detail_line("request", request.request_id.clone(), WHITE),
            detail_line(
                "session",
                display_session_id(request.session_id.as_deref()),
                TEAL,
            ),
            detail_line(
                "session seq",
                request
                    .session_seq
                    .map(|seq| seq.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                DIM_WHITE,
            ),
            detail_line("endpoint", request.endpoint.label(), DIM_WHITE),
            detail_line("started", format_system_time(request.started_at), DIM_WHITE),
            detail_line(
                "finished",
                format_system_time(request.finished_at),
                DIM_WHITE,
            ),
            detail_line(
                "status",
                request.status.label(),
                status_color(request.status.label()),
            ),
            detail_line(
                "http status",
                request
                    .http_status
                    .map(|status| status.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                http_status_color(request.http_status),
            ),
            detail_line("provider", request.provider.as_deref().unwrap_or("-"), TEAL),
            detail_line(
                "account",
                request.account.as_deref().unwrap_or("not reported"),
                WHITE,
            ),
            detail_line("model", request.model.as_deref().unwrap_or("-"), DIM_WHITE),
            detail_line("effort", request.effort.as_deref().unwrap_or("-"), YELLOW),
            detail_line("latency", ui::duration(request.latency), DIM_WHITE),
            detail_line("rate", request.rate().label(), TEAL),
            detail_line("input tokens", token_value(request.input_tokens), DIM_WHITE),
            detail_line(
                "output tokens",
                token_value(request.output_tokens),
                DIM_WHITE,
            ),
            detail_line(
                "cost",
                request.usage.cost_detail(),
                cost_color(&request.usage),
            ),
            detail_line(
                "stream bytes",
                request.streamed_bytes.to_string(),
                DIM_WHITE,
            ),
            detail_line(
                "stream chunks",
                request.stream_chunks.to_string(),
                DIM_WHITE,
            ),
        ];
        if let Some(error) = request.error.as_deref().filter(|error| !error.is_empty()) {
            lines.insert(8, detail_line("detail", error, YELLOW));
        }
        if let Some(path) = &request.traffic_capture_path {
            lines.insert(
                9,
                detail_line("capture", path.to_string_lossy().into_owned(), DIM_WHITE),
            );
        }
        lines
    } else {
        vec![Line::from(Span::styled(
            "No request selected",
            Style::default().fg(DIM),
        ))]
    };
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(PANEL_BG))
            .block(panel("Request detail", true)),
        area,
    );
}

fn detail_line<'a>(label: &'static str, value: impl Into<String>, value_color: Color) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("  {label:<16}"), Style::default().fg(DIM)),
        Span::styled(value.into(), Style::default().fg(value_color)),
    ])
}

fn render_footer(frame: &mut ratatui::Frame<'_>, area: Rect, app: &MonitorApp) {
    let settings_hint = app
        .settings
        .as_ref()
        .filter(|_| app.page == Page::Settings)
        .map(|settings| {
            let keys = settings.hints(area.width);
            // While a field or a search is open, `q` and `?` are typed text.
            if settings.captures_input() || area.width < 90 {
                keys.to_string()
            } else {
                format!("{keys} · Tab or 1–3 tabs · ? help · q close")
            }
        });
    let hint = if let Some(hint) = settings_hint {
        hint
    } else if app.page == Page::Accounts && app.providers.renaming() {
        " Enter save · Esc keep the old name · Ctrl-U clear · Ctrl-C close".to_string()
    } else if app.page == Page::Accounts && app.providers.moving() {
        " ↑/↓ move · ← top · → bottom · Enter save · Esc put back · q close".to_string()
    } else if app.page == Page::Accounts && app.providers.menu_open() {
        " Esc back · ↑/↓ choose · Enter run · q close".to_string()
    } else if area.width < 80 {
        " q close · ↑↓ move · Enter · u/d · ?".to_string()
    } else if app.page == Page::Accounts {
        " Tab or 1–3 tabs · ↑/↓ move · →/Enter actions · u/d details · r refresh · ? help · q close"
            .to_string()
    } else {
        " Tab or 1–3 tabs · ↑/↓ move between panes · j/k move in pane · →/Enter details · ←/Esc back · ? help · q close"
            .to_string()
    };
    let hint = if app.is_attached() {
        hint
    } else {
        hint.replace("q close", "q stop proxy")
    };
    let hint = if app.proxy_off {
        format!(" s start proxy ·{hint}")
    } else {
        hint
    };
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(TEAL).bg(BG)),
        area,
    );
}

fn render_shutdown_confirmation(frame: &mut ratatui::Frame<'_>, area: Rect) {
    let width = 44.min(area.width);
    let height = 5.min(area.height);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "Shut down proxy?",
                Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
            )),
            Line::from(vec![
                Span::styled("y", Style::default().fg(TEAL)),
                Span::styled(" confirm   ", Style::default().fg(DIM_WHITE)),
                Span::styled("n/Esc/q", Style::default().fg(TEAL)),
                Span::styled(" cancel", Style::default().fg(DIM_WHITE)),
            ]),
        ])
        .alignment(Alignment::Center)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(YELLOW))
                .style(Style::default().bg(BG)),
        )
        .style(Style::default().bg(BG)),
        popup,
    );
}

fn render_shutdown_overlay(frame: &mut ratatui::Frame<'_>, area: Rect, tick: usize) {
    let width = 40.min(area.width);
    let height = 5.min(area.height);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(format!("{} ", spinner(tick)), Style::default().fg(TEAL)),
                Span::styled(
                    "Shutting down...",
                    Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(Span::styled(
                "Press Ctrl-C to force quit",
                Style::default().fg(DIM_WHITE),
            )),
        ])
        .alignment(Alignment::Center)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(TEAL))
                .style(Style::default().bg(BG)),
        )
        .style(Style::default().bg(BG)),
        popup,
    );
}

fn render_help_overlay(frame: &mut ratatui::Frame<'_>, area: Rect, attached: bool) {
    let width = 64.min(area.width);
    let height = 20.min(area.height);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(Span::styled(" Shortcuts ", Style::default().fg(TEAL)))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(TEAL))
        .style(Style::default().bg(BG));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let lines = [
        (
            "q",
            if attached {
                "close monitor"
            } else {
                "confirm proxy shutdown"
            },
        ),
        (
            "Ctrl-C",
            if attached {
                "close monitor"
            } else {
                "stop proxy; again to force"
            },
        ),
        ("?", "toggle help"),
        ("b", "toggle setup"),
        ("1 / p", "request order and saved accounts"),
        ("2", "sessions, requests and errors"),
        ("3", "settings, including auto-rotate"),
        ("Tab / Shift-Tab", "next / previous tab"),
        ("↑ / ↓", "move between rows and activity panes"),
        ("j / k", "move down / up within a pane"),
        ("Enter / →", "choose an action or open details"),
        ("←", "back one step; on Settings it changes a value"),
        ("u / d", "scroll account details"),
        ("r", "refresh accounts and usage"),
        ("s", "start the proxy when it is off"),
        ("● / ○", "active accounts / last used when idle"),
        ("*", "last refresh failed; values may be old"),
        (
            "Esc",
            if attached {
                "back, then close monitor"
            } else {
                "back, then confirm shutdown"
            },
        ),
    ];
    let content = lines
        .into_iter()
        .map(|(key, label)| {
            Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{key:<10}"), Style::default().fg(TEAL)),
                Span::styled(label, Style::default().fg(DIM_WHITE)),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(content).style(Style::default().bg(BG)),
        inner,
    );
}

fn render_setup_overlay(frame: &mut ratatui::Frame<'_>, area: Rect, setup_text: &str) {
    let mut lines = setup_text
        .lines()
        .map(|line| {
            let style = if line.starts_with("cc-proxy ") {
                Style::default().fg(WHITE)
            } else {
                Style::default().fg(DIM_WHITE)
            };
            Line::from(vec![Span::raw("  "), Span::styled(line.to_string(), style)])
        })
        .collect::<Vec<_>>();
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("Esc", Style::default().fg(TEAL)),
        Span::styled(" close  ", Style::default().fg(DIM)),
        Span::styled("b", Style::default().fg(TEAL)),
        Span::styled(" toggle setup", Style::default().fg(DIM)),
    ]));
    let paragraph = Paragraph::new(lines)
        .style(Style::default().bg(BG))
        .wrap(Wrap { trim: false });
    let width = 84.min(area.width.saturating_sub(4)).max(36);
    let height = u16::try_from(paragraph.line_count(width.saturating_sub(2)) + 2)
        .unwrap_or(u16::MAX)
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
        .title(Span::styled(" Setup ", Style::default().fg(TEAL)))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(TEAL))
        .style(Style::default().bg(BG));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    frame.render_widget(paragraph, inner);
}

fn mock_setup_text(port: u16, registry: &Registry) -> String {
    format!(
        "Mock mode uses deterministic simulated monitor traffic.\nNo proxy server is listening.\nRun `cc-proxy start` to start the proxy.\n\n{}",
        setup_text(port, registry)
    )
}

pub fn setup_text(port: u16, registry: &Registry) -> String {
    let grouped = registry.grouped_models();
    let model_summary = ["codex", "kimi", "cursor"]
        .into_iter()
        .filter_map(|provider| {
            grouped
                .get(provider)
                .map(|models| format!("{provider}: {} models", models.len()))
        })
        .collect::<Vec<_>>()
        .join("  ");
    [
        "Send Claude Code through the proxy:".to_string(),
        "cc-proxy start".to_string(),
        "Switch provider: cc-proxy use <provider>".to_string(),
        String::new(),
        format!(
            "Proxy: {}",
            daemon::client_url(&config::bind_address(), port)
        ),
        format!("Logs: {}", paths::log_file().display()),
        format!("Config: {}", paths::config_dir().display()),
        format!("Providers: {model_summary}"),
    ]
    .join("\n")
}

fn format_system_time(time: SystemTime) -> String {
    format_system_time_in_zone(time, TimeZone::system())
}

fn format_system_time_in_zone(time: SystemTime, time_zone: TimeZone) -> String {
    let Ok(timestamp) = Timestamp::try_from(time) else {
        return "-".to_string();
    };
    Zoned::new(timestamp, time_zone)
        .strftime("%H:%M:%S")
        .to_string()
}

#[cfg(test)]
mod tests {
    use ratatui::{backend::TestBackend, buffer::Buffer};

    use super::*;
    use crate::monitor::{EndpointKind, mock_state};

    fn draw(width: u16, height: u16, render: impl FnOnce(&mut ratatui::Frame<'_>)) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(render).unwrap();
        terminal.backend().buffer().clone()
    }

    fn buffer_text(buffer: &Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn placeholder_position(buffer: &Buffer, placeholder: &str) -> Option<(u16, u16)> {
        let symbols = placeholder
            .chars()
            .map(|character| character.to_string())
            .collect::<Vec<_>>();
        (0..buffer.area.height).find_map(|y| {
            (0..buffer.area.width).find_map(|x| {
                symbols
                    .iter()
                    .enumerate()
                    .all(|(offset, symbol)| {
                        x + (offset as u16) < buffer.area.width
                            && buffer[(x + offset as u16, y)].symbol() == symbol
                    })
                    .then_some((x, y))
            })
        })
    }

    fn assert_centered(buffer: &Buffer, placeholder: &str, expected_y: u16) {
        let (x, y) = placeholder_position(buffer, placeholder).unwrap();
        let left_space = x.saturating_sub(1);
        let right_space = buffer
            .area
            .width
            .saturating_sub(x + placeholder.chars().count() as u16 + 1);
        assert_eq!(y, expected_y);
        assert!(left_space.abs_diff(right_space) <= 1);
    }

    fn headers<K>(columns: &[ColumnSpec<K>]) -> Vec<&'static str> {
        columns.iter().map(|column| column.header).collect()
    }

    fn fixed_budget<K>(columns: &[ColumnSpec<K>]) -> u16 {
        let widths = columns
            .iter()
            .map(|column| match column.width {
                layout::ColumnWidth::Fixed(width) => width,
                layout::ColumnWidth::Flex(_) => 0,
            })
            .sum::<u16>();
        widths.saturating_add(columns.len().saturating_sub(1) as u16)
    }

    fn fixed_width<K: Copy + PartialEq>(columns: &[ColumnSpec<K>], key: K) -> Option<u16> {
        columns
            .iter()
            .find(|column| column.key == key)
            .and_then(|column| match column.width {
                layout::ColumnWidth::Fixed(width) => Some(width),
                layout::ColumnWidth::Flex(_) => None,
            })
    }

    fn flex_count<K>(columns: &[ColumnSpec<K>]) -> usize {
        columns
            .iter()
            .filter(|column| matches!(column.width, layout::ColumnWidth::Flex(_)))
            .count()
    }

    fn alignment<K: Copy + PartialEq>(columns: &[ColumnSpec<K>], key: K) -> Option<Alignment> {
        columns
            .iter()
            .find(|column| column.key == key)
            .map(|column| column.alignment)
    }

    #[test]
    fn format_system_time_applies_non_utc_time_zone() {
        let timestamp = SystemTime::UNIX_EPOCH + Duration::from_secs(20 * 60 * 60);
        let time_zone = TimeZone::fixed(jiff::tz::offset(5));

        assert_eq!(format_system_time_in_zone(timestamp, time_zone), "01:00:00");
    }

    #[test]
    fn request_tables_share_time_status_provider_model_rhythm() {
        assert_eq!(
            headers(&active_columns(LayoutTier::Medium))[..4],
            ["Started", "Status", "Provider", "Model"]
        );
        assert_eq!(
            headers(&recent_columns(LayoutTier::Medium))[..4],
            ["Finished", "Code", "Provider", "Model"]
        );
        assert_eq!(
            headers(&event_columns(LayoutTier::Medium))[..4],
            ["Time", "Code", "Provider", "Model"]
        );
    }

    #[test]
    fn wide_tables_use_shared_model_and_provider_widths() {
        let sessions = session_columns(LayoutTier::Wide, true);
        let active = active_columns(LayoutTier::Wide);
        let recent = recent_columns(LayoutTier::Wide);
        let events = event_columns(LayoutTier::Wide);

        assert_eq!(
            fixed_width(&sessions, SessionColumn::Model),
            Some(MODEL_WIDE_WIDTH)
        );
        assert_eq!(
            fixed_width(&active, ActiveColumn::Model),
            Some(MODEL_WIDE_WIDTH)
        );
        assert_eq!(
            fixed_width(&recent, RecentColumn::Model),
            Some(MODEL_WIDE_WIDTH)
        );
        assert_eq!(
            fixed_width(&events, EventColumn::Model),
            Some(MODEL_WIDE_WIDTH)
        );
        assert_eq!(
            fixed_width(&active, ActiveColumn::Provider),
            Some(PROVIDER_WIDTH)
        );
        assert_eq!(
            fixed_width(&recent, RecentColumn::Provider),
            Some(PROVIDER_WIDTH)
        );
    }

    #[test]
    fn responsive_schemas_fit_their_minimum_terminal_widths() {
        assert!(fixed_budget(&session_columns(LayoutTier::Emergency, false)) <= 75);
        assert!(fixed_budget(&session_columns(LayoutTier::Narrow, false)) <= 76);
        assert!(fixed_budget(&session_columns(LayoutTier::Medium, false)) <= 88);
        assert!(fixed_budget(&session_columns(LayoutTier::Expanded, false)) <= 118);
        assert!(fixed_budget(&session_columns(LayoutTier::Wide, true)) <= 168);

        assert!(fixed_budget(&active_columns(LayoutTier::Emergency)) <= 75);
        assert!(fixed_budget(&active_columns(LayoutTier::Narrow)) <= 76);
        assert!(fixed_budget(&active_columns(LayoutTier::Medium)) <= 88);
        assert!(fixed_budget(&active_columns(LayoutTier::Expanded)) <= 118);
        assert!(fixed_budget(&active_columns(LayoutTier::Wide)) <= 152);

        assert!(fixed_budget(&recent_columns(LayoutTier::Emergency)) <= 75);
        assert!(fixed_budget(&recent_columns(LayoutTier::Narrow)) <= 76);
        assert!(fixed_budget(&recent_columns(LayoutTier::Medium)) <= 88);
        assert!(fixed_budget(&recent_columns(LayoutTier::Expanded)) <= 118);
        assert!(fixed_budget(&recent_columns(LayoutTier::Wide)) <= 152);

        assert!(fixed_budget(&event_columns(LayoutTier::Emergency)) <= 75);
        assert!(fixed_budget(&event_columns(LayoutTier::Narrow)) <= 76);
        assert!(fixed_budget(&event_columns(LayoutTier::Medium)) <= 88);
        assert!(fixed_budget(&event_columns(LayoutTier::Expanded)) <= 118);
        assert!(fixed_budget(&event_columns(LayoutTier::Wide)) <= 152);
    }

    #[test]
    fn active_table_renders_expected_headers_at_tier_boundaries() {
        let state: MonitorSnapshot = mock_state().into();
        let render_at = |width| {
            let buffer = draw(width, 8, |frame| {
                render_active(frame, frame.area(), &state.active, 0)
            });
            buffer_text(&buffer)
        };

        let emergency = render_at(77);
        assert!(emergency.contains("Started"), "{emergency}");
        assert!(emergency.contains("Target"), "{emergency}");
        assert!(!emergency.contains("Rate"), "{emergency}");

        let narrow = render_at(78);
        assert!(narrow.contains("Provider"), "{narrow}");
        assert!(narrow.contains("Model"), "{narrow}");
        assert!(narrow.contains("Effort"), "{narrow}");
        assert!(!narrow.contains("Project"), "{narrow}");

        let medium = render_at(90);
        assert!(medium.contains("Provider"), "{medium}");
        assert!(medium.contains("Model"), "{medium}");
        assert!(medium.contains("Endpoint"), "{medium}");
        assert!(!medium.contains("Project"), "{medium}");

        let expanded = render_at(120);
        assert!(expanded.contains("Project"), "{expanded}");
        assert!(expanded.contains("Session"), "{expanded}");
        assert!(expanded.contains("Endpoint"), "{expanded}");
        assert!(!expanded.contains("In"), "{expanded}");

        let wide = render_at(154);
        assert!(wide.contains("Project"), "{wide}");
        assert!(wide.contains("Session"), "{wide}");
        assert!(wide.contains("In"), "{wide}");
        assert!(wide.contains("Out"), "{wide}");
    }

    #[test]
    fn each_schema_has_one_meaningful_flexible_column() {
        for tier in [
            LayoutTier::Emergency,
            LayoutTier::Narrow,
            LayoutTier::Medium,
            LayoutTier::Expanded,
            LayoutTier::Wide,
        ] {
            let sessions = session_columns(tier, tier == LayoutTier::Wide);
            let active = active_columns(tier);
            let recent = recent_columns(tier);
            let events = event_columns(tier);

            assert_eq!(flex_count(&sessions), 1);
            assert_eq!(flex_count(&active), 1);
            assert_eq!(flex_count(&recent), 1);
            assert_eq!(flex_count(&events), 1);
            assert_eq!(
                sessions
                    .iter()
                    .filter(|column| column.header.is_empty())
                    .count(),
                1
            );
            assert!(active.iter().all(|column| !column.header.is_empty()));
            assert!(recent.iter().all(|column| !column.header.is_empty()));
            assert!(events.iter().all(|column| !column.header.is_empty()));
        }
    }

    #[test]
    fn metric_columns_are_right_aligned() {
        let sessions = session_columns(LayoutTier::Wide, true);
        for key in [
            SessionColumn::Active,
            SessionColumn::Requests,
            SessionColumn::Failures,
            SessionColumn::Input,
            SessionColumn::Output,
            SessionColumn::Rate,
        ] {
            assert_eq!(alignment(&sessions, key), Some(Alignment::Right));
        }

        let active = active_columns(LayoutTier::Wide);
        for key in [
            ActiveColumn::Input,
            ActiveColumn::Output,
            ActiveColumn::Rate,
            ActiveColumn::Elapsed,
        ] {
            assert_eq!(alignment(&active, key), Some(Alignment::Right));
        }

        let recent = recent_columns(LayoutTier::Wide);
        for key in [
            RecentColumn::Code,
            RecentColumn::Latency,
            RecentColumn::Rate,
            RecentColumn::Input,
            RecentColumn::Output,
        ] {
            assert_eq!(alignment(&recent, key), Some(Alignment::Right));
        }
    }

    #[test]
    fn narrow_schemas_use_available_space_for_context() {
        let sessions = session_columns(LayoutTier::Narrow, false);
        assert!(
            sessions
                .iter()
                .any(|column| column.key == SessionColumn::Project)
        );
        assert!(
            sessions
                .iter()
                .any(|column| column.key == SessionColumn::Target)
        );

        let active = active_columns(LayoutTier::Narrow);
        assert!(
            active
                .iter()
                .any(|column| column.key == ActiveColumn::Provider)
        );
        assert!(
            active
                .iter()
                .any(|column| column.key == ActiveColumn::Model)
        );
        assert!(
            active
                .iter()
                .any(|column| column.key == ActiveColumn::Effort)
        );

        let recent = recent_columns(LayoutTier::Narrow);
        assert!(
            recent
                .iter()
                .any(|column| column.key == RecentColumn::Provider)
        );
        assert!(
            recent
                .iter()
                .any(|column| column.key == RecentColumn::Input)
        );
        assert!(
            recent
                .iter()
                .any(|column| column.key == RecentColumn::Output)
        );

        let events = event_columns(LayoutTier::Emergency);
        assert_eq!(headers(&events), ["Time", "Code", "Message"]);
    }

    #[test]
    fn display_session_id_shortens_uuids() {
        assert_eq!(
            display_session_id(Some("57c7c914-ada4-4f40-9672-985f950fbb66")),
            "57c7c914"
        );
    }

    #[test]
    fn display_session_id_handles_atypical_ids() {
        assert_eq!(display_session_id(Some("custom-session")), "custom-session");
        assert_eq!(display_session_id(Some("")), "no-session");
        assert_eq!(display_session_id(None), "no-session");
    }

    #[test]
    fn ellipsize_marks_truncated_values() {
        assert_eq!(ellipsize("claude-sonnet-4-6", 16), "claude-sonnet-4…");
        assert_eq!(ellipsize("gpt-5.6-sol", 16), "gpt-5.6-sol");
        assert_eq!(ellipsize("anything", 0), "");
    }

    #[test]
    fn token_sparkline_uses_fixed_wall_clock_buckets() {
        let samples = [
            (SystemTime::UNIX_EPOCH + Duration::from_secs(78), 2_000),
            (SystemTime::UNIX_EPOCH + Duration::from_secs(85), 3_000),
            (SystemTime::UNIX_EPOCH + Duration::from_secs(100), 4_000),
        ];
        let bucket_start = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let bucket_end = SystemTime::UNIX_EPOCH + Duration::from_secs(109);
        let next_bucket = SystemTime::UNIX_EPOCH + Duration::from_secs(110);

        assert_eq!(token_sparkline(&[], 4, bucket_start), "    ");
        assert_eq!(token_sparkline(&samples, 4, bucket_start), "▄▆ █");
        assert_eq!(token_sparkline(&samples, 4, bucket_end), "▄▆ █");
        assert_eq!(token_sparkline(&samples, 4, next_bucket), "▆ █ ");
        assert_eq!(token_sparkline(&samples, 0, bucket_start), "");
    }

    #[test]
    fn token_sparkline_uses_fixed_shared_scale() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let current = (now, 2_000);
        let offscreen_peak = (SystemTime::UNIX_EPOCH, 4_000);

        assert_eq!(token_sparkline(&[current], 2, now), " ▄");
        assert_eq!(token_sparkline(&[offscreen_peak, current], 2, now), " ▄");
        assert_eq!(token_sparkline(&[(now, 10_000)], 1, now), "█");
    }

    #[test]
    fn activity_graph_ignores_positive_viewer_clock_skew() {
        let snapshot_at = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
        let viewer_now = snapshot_at + Duration::from_secs(3_600);
        let samples = [(snapshot_at, 4_000)];
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "request-1",
            Some("session-1".into()),
            Some(1),
            EndpointKind::Messages,
        );
        monitor.request_completed("request-1", 200, Some(10), Some(4_000));
        let mut snapshot: MonitorSnapshot = monitor.snapshot().into();
        snapshot.snapshot_at = snapshot_at;
        snapshot.sessions[0].output_token_samples = samples.into();

        assert_eq!(token_sparkline(&samples, 4, viewer_now), "    ");
        let rendered = draw(90, 8, |frame| {
            render_sessions(
                frame,
                frame.area(),
                &snapshot.sessions,
                0,
                true,
                snapshot.snapshot_at,
            )
        });
        assert!(buffer_text(&rendered).contains('█'));
    }

    #[test]
    fn activity_graph_ignores_negative_viewer_clock_skew() {
        let snapshot_at = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
        let viewer_now = snapshot_at - Duration::from_secs(3_600);
        let samples = [(snapshot_at, 4_000)];
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "request-1",
            Some("session-1".into()),
            Some(1),
            EndpointKind::Messages,
        );
        monitor.request_completed("request-1", 200, Some(10), Some(4_000));
        let mut snapshot: MonitorSnapshot = monitor.snapshot().into();
        snapshot.snapshot_at = snapshot_at;
        snapshot.sessions[0].output_token_samples = samples.into();

        assert_eq!(token_sparkline(&samples, 4, viewer_now), "    ");
        let rendered = draw(90, 8, |frame| {
            render_sessions(
                frame,
                frame.area(),
                &snapshot.sessions,
                0,
                true,
                snapshot.snapshot_at,
            )
        });
        assert!(buffer_text(&rendered).contains('█'));
    }

    #[test]
    fn token_sparkline_dims_the_current_bucket() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let samples = [
            (SystemTime::UNIX_EPOCH + Duration::from_secs(95), 2_000),
            (now, 4_000),
        ];

        let line = token_sparkline_line(&samples, 2, now);

        assert_eq!(line.spans[0].content, "▄");
        assert_eq!(line.spans[0].style.fg, Some(BLUE));
        assert_eq!(line.spans[1].content, "█");
        assert_eq!(line.spans[1].style.fg, Some(DIM));
    }

    #[test]
    fn session_sparkline_appears_at_medium_width_and_expands() {
        let monitor = MonitorHandle::new(10);
        for (index, tokens) in [1_000, 2_000, 4_000].into_iter().enumerate() {
            let request_id = format!("request-{index}");
            monitor.request_started(
                &request_id,
                Some("sess-1".to_string()),
                Some(index as u64 + 1),
                EndpointKind::Messages,
            );
            monitor.provider_selected(&request_id, "codex", "gpt-5.6-sol", None);
            monitor.request_completed(&request_id, 200, Some(100), Some(tokens));
        }
        let state: MonitorSnapshot = monitor.snapshot().into();
        let render_at = |width| {
            let buffer = draw(width, 8, |frame| {
                render_sessions(
                    frame,
                    frame.area(),
                    &state.sessions,
                    0,
                    true,
                    state.snapshot_at,
                )
            });
            buffer_text(&buffer)
        };
        let spark_chars = |text: &str| {
            text.chars()
                .filter(|ch| matches!(ch, '▁' | '▂' | '▃' | '▄' | '▅' | '▆' | '▇' | '█'))
                .count()
        };

        let emergency = render_at(77);
        assert!(!emergency.contains("Trend"), "{emergency}");
        assert_eq!(spark_chars(&emergency), 0, "{emergency}");

        let narrow = render_at(78);
        assert!(narrow.contains("Trend"), "{narrow}");
        assert!(spark_chars(&narrow) > 0, "{narrow}");

        let medium = render_at(90);
        assert!(medium.contains("Tok/10s"), "{medium}");
        assert!(spark_chars(&medium) > 0, "{medium}");

        let expanded = render_at(120);
        assert!(expanded.contains("Tokens/10s"), "{expanded}");
        assert!(spark_chars(&expanded) > 0, "{expanded}");

        let wide = render_at(SESSION_SPARKLINE_MIN_WIDTH);
        assert!(wide.contains("Tokens/10s · 4k"), "{wide}");
        assert!(spark_chars(&wide) > 0, "{wide}");
    }

    #[test]
    fn empty_tables_hide_columns_and_center_placeholders() {
        let sessions = draw(60, 9, |frame| {
            render_sessions(frame, frame.area(), &[], 0, true, SystemTime::now())
        });
        let sessions_text = buffer_text(&sessions);
        assert_centered(&sessions, "No sessions yet", 3);
        assert!(!sessions_text.contains('`'));
        assert!(!sessions_text.contains("provider"));
        assert!(sessions_text.contains("its requests group here"));

        // Too short for the hint, the message alone stays centered.
        let active = draw(27, 3, |frame| render_active(frame, frame.area(), &[], 0));
        assert_centered(&active, "Nothing in flight", 1);
        assert!(!buffer_text(&active).contains("stream"));

        let recent = draw(50, 9, |frame| {
            render_recent(frame, frame.area(), &[], 0, false, SystemTime::now())
        });
        let recent_text = buffer_text(&recent);
        assert_centered(&recent, "No requests yet", 3);
        assert!(!recent_text.contains("finished"));
        assert!(recent_text.contains("newest first"));

        let events = draw(50, 9, |frame| render_events(frame, frame.area(), &[]));
        assert_centered(&events, "No errors", 3);
        assert!(buffer_text(&events).contains("Failed requests and HTTP errors"));
    }

    #[test]
    fn active_status_keeps_full_label_at_narrow_width() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("request-1", None, None, EndpointKind::Messages);
        monitor.upstream_started("request-1");
        let state: MonitorSnapshot = monitor.snapshot().into();

        let active = draw(88, 6, |frame| {
            render_active(frame, frame.area(), &state.active, 0)
        });

        let active_text = buffer_text(&active);
        assert!(active_text.contains("⠋ upstream"), "{active_text}");
    }

    #[test]
    fn active_compacting_status_is_distinct_at_narrow_width() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("request-1", None, None, EndpointKind::Messages);
        monitor.compaction_started("request-1");
        let state: MonitorSnapshot = monitor.snapshot().into();

        let active = draw(88, 6, |frame| {
            render_active(frame, frame.area(), &state.active, 0)
        });

        let active_text = buffer_text(&active);
        assert!(active_text.contains("⠋compacting"), "{active_text}");
        assert_eq!(status_color("compacting"), PURPLE);
    }

    #[test]
    fn populated_tables_render_rows_without_placeholders() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "request-1",
            Some("sess-1".to_string()),
            Some(1),
            EndpointKind::Messages,
        );
        monitor.project_resolved("request-1", "example-project");
        monitor.provider_selected(
            "request-1",
            "codex",
            "gpt-5.6-sol",
            Some("high".to_string()),
        );
        let active_state: MonitorSnapshot = monitor.snapshot().into();

        let sessions = draw(170, 8, |frame| {
            render_sessions(
                frame,
                frame.area(),
                &active_state.sessions,
                0,
                true,
                active_state.snapshot_at,
            )
        });
        let sessions_text = buffer_text(&sessions);
        assert!(sessions_text.contains("Provider"));
        assert!(sessions_text.contains("Project"));
        assert!(sessions_text.contains("example-project"));
        assert!(sessions_text.contains("sess-1"));
        assert!(!sessions_text.contains("No sessions yet"));

        let active = draw(120, 8, |frame| {
            render_active(frame, frame.area(), &active_state.active, 0)
        });
        let active_text = buffer_text(&active);
        assert!(active_text.contains("Started"));
        assert!(active_text.contains("gpt-5.6-sol"));
        assert!(!active_text.contains("Nothing in flight"));

        monitor.request_completed("request-1", 200, Some(100), Some(25));
        let completed_state: MonitorSnapshot = monitor.snapshot().into();
        let recent = draw(140, 8, |frame| {
            render_recent(
                frame,
                frame.area(),
                &completed_state.recent,
                0,
                false,
                SystemTime::now(),
            )
        });
        let recent_text = buffer_text(&recent);
        assert!(recent_text.contains("Finished"));
        assert!(recent_text.contains("200"));
        assert!(!recent_text.contains("No requests yet"));

        let events = draw(100, 8, |frame| {
            render_events(frame, frame.area(), &completed_state.recent)
        });
        assert!(buffer_text(&events).contains("No errors"));
    }

    #[test]
    fn selected_rows_scroll_into_table_viewports() {
        let state: MonitorSnapshot = mock_state().into();
        let sessions = (0..12)
            .map(|index| {
                let mut session = state.sessions[0].clone();
                session.session_id = Some(format!("row-{index:04}"));
                session
            })
            .collect::<Vec<_>>();
        let session_buffer = draw(120, 6, |frame| {
            render_sessions(frame, frame.area(), &sessions, 11, true, state.snapshot_at)
        });
        let session_text = buffer_text(&session_buffer);
        assert!(session_text.contains("row-0011"), "{session_text}");
        assert!(!session_text.contains("row-0000"), "{session_text}");

        let recent = (0..12)
            .map(|index| {
                let mut request = state.recent[0].clone();
                request.project = Some(format!("row-{index:04}"));
                request
            })
            .collect::<Vec<_>>();
        let recent_buffer = draw(120, 6, |frame| {
            render_recent(frame, frame.area(), &recent, 11, true, SystemTime::now())
        });
        let recent_text = buffer_text(&recent_buffer);
        assert!(recent_text.contains("row-0011"), "{recent_text}");
        assert!(!recent_text.contains("row-0000"), "{recent_text}");
    }

    #[test]
    fn mock_state_renders_representative_panes_at_wide_width() {
        let state: MonitorSnapshot = mock_state().into();
        let mut app = MonitorApp {
            listen_url: "mock://tui-demo".to_string(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            page: Page::Activity,
            accounts_mode: providers::Mode::Demo,
            providers: ProvidersPanel::new(providers::Mode::Demo),
            settings: Some(config_keys::SettingsPanel::new(true)),
            detail: None,
            focus: FocusPane::Sessions,
            selected: 0,
            recent_selected: 0,
            connected: true,
            proxy_off: false,
            saving: None,
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: None,
            shutdown_complete: None,
        };

        let buffer = draw(180, 48, |frame| render(frame, &mut app, &state));
        let text = buffer_text(&buffer);

        assert!(text.contains("mock://tui-demo"), "{text}");
        assert!(text.contains("cc-proxy"), "{text}");
        assert!(text.contains("streaming"), "{text}");
        assert!(text.contains("gpt-5.6-terra"), "{text}");
        assert!(text.contains("upstream connection closed"), "{text}");
    }

    #[test]
    fn settings_is_its_own_tab_and_remote_monitors_cannot_change_settings() {
        let state: MonitorSnapshot = mock_state().into();
        let mut app = MonitorApp {
            listen_url: "mock://tui-demo".to_string(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            page: Page::Accounts,
            accounts_mode: providers::Mode::Demo,
            providers: ProvidersPanel::new(providers::Mode::Demo),
            settings: Some(config_keys::SettingsPanel::new(true)),
            detail: None,
            focus: FocusPane::Sessions,
            selected: 0,
            recent_selected: 0,
            connected: true,
            proxy_off: false,
            saving: None,
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: None,
            shutdown_complete: None,
        };
        assert_eq!(app.page.next().next().next(), Page::Accounts);
        assert_eq!(Page::Accounts.previous(), Page::Settings);

        let accounts = buffer_text(&draw(120, 30, |frame| render(frame, &mut app, &state)));
        assert!(accounts.contains("3 Settings"), "{accounts}");
        assert!(!accounts.contains("Auto-rotate"), "{accounts}");

        app.open_page(Page::Settings);
        let settings = buffer_text(&draw(120, 30, |frame| render(frame, &mut app, &state)));
        assert!(settings.contains("auth.autoRotate"), "{settings}");
        // One footer line: the panel's own keys plus the monitor's.
        assert_eq!(settings.matches("Esc back").count(), 1, "{settings}");
        assert!(settings.contains("/ search"), "{settings}");
        assert!(!settings.contains("Esc close"), "{settings}");

        // Escape goes back to Accounts instead of asking to stop the proxy.
        assert_eq!(app.handle_escape(), None);
        assert_eq!(app.page, Page::Accounts);
        assert_eq!(app.phase, MonitorPhase::Running);

        app.settings = None;
        app.open_page(Page::Settings);
        let remote = buffer_text(&draw(80, 20, |frame| render(frame, &mut app, &state)));
        assert!(remote.contains("read-only"), "{remote}");
    }

    #[test]
    fn mock_request_detail_exposes_error_and_capture_fields() {
        let state: MonitorSnapshot = mock_state().into();
        let failed = state
            .recent
            .iter()
            .position(|request| request.request_id == "req-failed-kimi")
            .unwrap();

        let detail = draw(140, 22, |frame| {
            render_request_detail(frame, frame.area(), &state, failed)
        });
        let text = buffer_text(&detail);

        assert!(text.contains("req-failed-kimi"), "{text}");
        assert!(text.contains("upstream connection closed"), "{text}");
        assert!(text.contains("req-failed-kimi.json"), "{text}");
    }

    #[test]
    fn recent_table_uses_error_indicator_at_medium_width() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("request-1", None, None, EndpointKind::Messages);
        monitor.provider_selected("request-1", "codex", "gpt-5.6-sol", None);
        monitor.request_failed("request-1", Some(502), "upstream unavailable");
        let state: MonitorSnapshot = monitor.snapshot().into();

        let recent = draw(110, 8, |frame| {
            render_recent(
                frame,
                frame.area(),
                &state.recent,
                0,
                true,
                SystemTime::now(),
            )
        });
        let recent_text = buffer_text(&recent);

        assert!(recent_text.contains("!"), "{recent_text}");
        assert!(!recent_text.contains("Details"), "{recent_text}");
        assert!(
            !recent_text.contains("upstream unavailable"),
            "{recent_text}"
        );
    }

    #[test]
    fn recent_table_keeps_detail_text_at_wide_width() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("request-1", None, None, EndpointKind::Messages);
        monitor.provider_selected("request-1", "codex", "gpt-5.6-sol", None);
        monitor.request_failed("request-1", Some(502), "upstream unavailable");
        let state: MonitorSnapshot = monitor.snapshot().into();

        let recent = draw(180, 8, |frame| {
            render_recent(
                frame,
                frame.area(),
                &state.recent,
                0,
                false,
                SystemTime::now(),
            )
        });
        let recent_text = buffer_text(&recent);

        assert!(recent_text.contains("Details"), "{recent_text}");
        assert!(
            recent_text.contains("upstream unavailable"),
            "{recent_text}"
        );
    }

    #[test]
    fn request_detail_renders_full_error_text() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "request-1",
            Some("sess-1".to_string()),
            Some(7),
            EndpointKind::Messages,
        );
        monitor.provider_selected(
            "request-1",
            "codex",
            "gpt-5.6-sol",
            Some("high".to_string()),
        );
        monitor.request_failed("request-1", Some(502), "upstream unavailable");
        let state: MonitorSnapshot = monitor.snapshot().into();

        let detail = draw(120, 20, |frame| {
            render_request_detail(frame, frame.area(), &state, 0)
        });
        let detail_text = buffer_text(&detail);

        assert!(detail_text.contains("Request detail"), "{detail_text}");
        assert!(detail_text.contains("request-1"), "{detail_text}");
        assert!(detail_text.contains("sess-1"), "{detail_text}");
        assert!(
            detail_text.contains("upstream unavailable"),
            "{detail_text}"
        );
    }

    #[test]
    fn session_and_request_details_show_an_estimated_cost_or_why_there_is_none() {
        let monitor = MonitorHandle::new(10);
        for (id, session, model) in [
            ("priced", "s-priced", "gpt-5.4"),
            ("unpriced", "s-unpriced", "gpt-6.1-sol"),
        ] {
            monitor.request_started(id, Some(session.into()), Some(1), EndpointKind::Messages);
            monitor.provider_selected(id, "codex", model, None);
            monitor.model_resolved(id, model);
            monitor.usage_updated(id, Some(10_000), Some(500));
            monitor.request_completed(id, 200, None, None);
        }
        let state: MonitorSnapshot = monitor.snapshot().into();
        let text = |selected: usize| {
            buffer_text(&draw(120, 30, |frame| {
                render_session_detail(frame, frame.area(), &state, selected)
            }))
        };
        let sessions: Vec<_> = state.sessions.iter().map(SessionSnapshot::label).collect();
        let priced = text(sessions.iter().position(|s| s == "s-priced").unwrap());
        assert!(priced.contains("10.0k in / 500 out"), "{priced}");
        assert!(priced.contains("$0.03 est., at API prices"), "{priced}");
        let unpriced = text(sessions.iter().position(|s| s == "s-unpriced").unwrap());
        assert!(unpriced.contains("no price for this model"), "{unpriced}");
        assert!(!unpriced.contains("$0.00"), "{unpriced}");

        let request = buffer_text(&draw(120, 30, |frame| {
            render_request_detail(frame, frame.area(), &state, 1)
        }));
        assert!(request.contains("cost"), "{request}");
    }

    #[test]
    fn request_account_is_visible_in_rows_and_full_detail() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "account-request",
            Some("s".into()),
            Some(1),
            EndpointKind::Messages,
        );
        monitor.provider_selected("account-request", "codex", "gpt-5.6-sol", None);
        monitor.account_selected("account-request", "work-id", "work");
        let active: MonitorSnapshot = monitor.snapshot().into();
        let screen = draw(140, 10, |frame| {
            render_active(frame, frame.area(), &active.active, 0)
        });
        assert!(buffer_text(&screen).contains("work /"));
        monitor.request_completed("account-request", 200, None, None);
        let completed: MonitorSnapshot = monitor.snapshot().into();
        let screen = draw(120, 30, |frame| {
            render_request_detail(frame, frame.area(), &completed, 0)
        });
        let text = buffer_text(&screen);
        assert!(text.contains("account") && text.contains("work"), "{text}");
    }

    #[test]
    fn events_render_matching_request_rows_without_a_placeholder() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("request-1", None, None, EndpointKind::Messages);
        monitor.request_failed("request-1", Some(502), "upstream unavailable");
        let state: MonitorSnapshot = monitor.snapshot().into();

        let events = draw(100, 8, |frame| {
            render_events(frame, frame.area(), &state.recent)
        });
        let events_text = buffer_text(&events);
        assert!(events_text.contains("Time"));
        assert!(events_text.contains("502"));
        assert!(events_text.contains("upstream unavailable"));
        assert!(!events_text.contains("No errors"));
    }

    #[test]
    fn setup_overlay_counts_wrapped_paths_before_placing_the_commands() {
        for path in ["folder/".repeat(20), "配置/".repeat(30)] {
            let setup = format!(
                "Logs: /{path}proxy.log\nConfig: /{path}config.json\n\nUse the proxy:\ncc-proxy start"
            );
            for (width, height) in [(120, 40), (80, 24)] {
                let screen = draw(width, height, |frame| {
                    render_setup_overlay(frame, frame.area(), &setup)
                });
                let text = buffer_text(&screen);
                assert!(text.contains("cc-proxy start"), "{text}");
                assert!(text.contains("toggle setup"), "{text}");
            }
        }
    }

    #[test]
    fn escape_closes_layers_before_detaching_from_the_main_screen() {
        let mut app = MonitorApp {
            listen_url: String::new(),
            setup_text: String::new(),
            show_setup: true,
            show_help: true,
            page: Page::Activity,
            accounts_mode: providers::Mode::Demo,
            providers: ProvidersPanel::new(providers::Mode::Demo),
            settings: Some(config_keys::SettingsPanel::new(true)),
            detail: Some(DetailView::Request),
            focus: FocusPane::Sessions,
            selected: 0,
            recent_selected: 0,
            connected: true,
            proxy_off: false,
            saving: None,
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: None,
            shutdown_complete: None,
        };
        assert_eq!(app.handle_escape(), None);
        assert!(!app.show_help);
        assert!(app.show_setup && app.detail.is_some());
        assert_eq!(app.handle_escape(), None);
        assert!(!app.show_setup);
        assert!(app.detail.is_some());
        assert_eq!(app.handle_escape(), None);
        assert!(app.detail.is_none());
        assert_eq!(app.handle_escape(), Some(MonitorExit::Detached));
        assert_eq!(app.phase, MonitorPhase::Running);
    }

    #[test]
    fn shutdown_confirmation_can_be_cancelled_before_signalling_server() {
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let mut app = MonitorApp {
            listen_url: "http://127.0.0.1:3000".to_string(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            page: Page::Activity,
            accounts_mode: providers::Mode::Demo,
            providers: ProvidersPanel::new(providers::Mode::Demo),
            settings: Some(config_keys::SettingsPanel::new(true)),
            detail: None,
            focus: FocusPane::Sessions,
            selected: 0,
            recent_selected: 0,
            connected: true,
            proxy_off: false,
            saving: None,
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: Some(shutdown_tx),
            shutdown_complete: Some(mpsc::channel().1),
        };

        assert_eq!(app.handle_escape(), None);
        assert_eq!(app.phase, MonitorPhase::ConfirmingShutdown);
        assert!(matches!(
            shutdown_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));

        let state: MonitorSnapshot = MonitorHandle::default().snapshot().into();
        let screen = draw(80, 24, |frame| render(frame, &mut app, &state));
        let text = buffer_text(&screen);
        assert!(text.contains("Shut down proxy?"), "{text}");
        assert!(text.contains("y confirm"), "{text}");
        assert!(text.contains("n/Esc/q cancel"), "{text}");

        app.cancel_shutdown_confirmation();
        assert_eq!(app.phase, MonitorPhase::Running);
        assert!(matches!(
            shutdown_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));

        app.request_shutdown_confirmation();
        app.begin_shutdown();
        assert_eq!(app.phase, MonitorPhase::ShuttingDown);
        assert_eq!(shutdown_rx.try_recv(), Ok(()));
    }

    #[test]
    fn an_off_proxy_says_so_and_offers_the_start_key_on_every_screen() {
        let mut app = MonitorApp {
            listen_url: "http://127.0.0.1:3000".to_string(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            page: Page::Accounts,
            accounts_mode: providers::Mode::Demo,
            providers: ProvidersPanel::new(providers::Mode::Demo),
            settings: Some(config_keys::SettingsPanel::new(true)),
            detail: None,
            focus: FocusPane::Sessions,
            selected: 0,
            recent_selected: 0,
            connected: false,
            proxy_off: true,
            saving: None,
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: None,
            shutdown_complete: None,
        };
        let state = MonitorSnapshot::empty();
        let text = buffer_text(&draw(100, 24, |frame| render(frame, &mut app, &state)));
        assert!(text.contains("Proxy off. Press s to start it"), "{text}");
        assert!(text.contains("s start proxy"), "{text}");
        assert!(!text.contains("active  up"), "{text}");

        app.page = Page::Activity;
        let text = buffer_text(&draw(100, 24, |frame| render(frame, &mut app, &state)));
        assert!(text.contains("s start proxy"), "{text}");

        // A proxy that is on but unreachable offers no start key.
        app.proxy_off = false;
        app.page = Page::Accounts;
        let text = buffer_text(&draw(100, 24, |frame| render(frame, &mut app, &state)));
        assert!(!text.contains("s start proxy"), "{text}");
        assert!(text.contains("Disconnected: retrying"), "{text}");
    }

    #[test]
    fn ctrl_c_starts_shutdown_then_requests_force_quit() {
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let (_shutdown_complete_tx, shutdown_complete_rx) = mpsc::channel();
        let mut app = MonitorApp {
            listen_url: "http://127.0.0.1:3000".to_string(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            page: Page::Activity,
            accounts_mode: providers::Mode::Demo,
            providers: ProvidersPanel::new(providers::Mode::Demo),
            settings: Some(config_keys::SettingsPanel::new(true)),
            detail: None,
            focus: FocusPane::Sessions,
            selected: 0,
            recent_selected: 0,
            connected: true,
            proxy_off: false,
            saving: None,
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: Some(shutdown_tx),
            shutdown_complete: Some(shutdown_complete_rx),
        };

        assert!(!app.handle_ctrl_c());
        assert!(app.handle_ctrl_c());

        assert_eq!(app.phase, MonitorPhase::ShuttingDown);
        assert_eq!(shutdown_rx.try_recv(), Ok(()));
        let state: MonitorSnapshot = MonitorHandle::default().snapshot().into();
        let screen = draw(80, 24, |frame| render(frame, &mut app, &state));
        let text = buffer_text(&screen);
        assert!(text.contains("Shutting down..."));
        assert!(text.contains("Press Ctrl-C to force quit"));
    }

    #[test]
    fn shutdown_completion_accepts_notification_and_sender_drop() {
        let (complete_tx, complete_rx) = mpsc::channel();
        let app = MonitorApp {
            listen_url: String::new(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            page: Page::Activity,
            accounts_mode: providers::Mode::Demo,
            providers: ProvidersPanel::new(providers::Mode::Demo),
            settings: Some(config_keys::SettingsPanel::new(true)),
            detail: None,
            focus: FocusPane::Sessions,
            selected: 0,
            recent_selected: 0,
            connected: true,
            proxy_off: false,
            saving: None,
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: None,
            shutdown_complete: Some(complete_rx),
        };

        assert!(!app.shutdown_is_complete());
        complete_tx.send(()).unwrap();
        assert!(app.shutdown_is_complete());

        let (complete_tx, complete_rx) = mpsc::channel();
        let mut app = app;
        app.shutdown_complete = Some(complete_rx);
        drop(complete_tx);
        assert!(app.shutdown_is_complete());
    }

    #[test]
    fn header_renders_configured_listen_url() {
        let app = MonitorApp {
            listen_url: "http://[::]:18765".to_string(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            page: Page::Activity,
            accounts_mode: providers::Mode::Demo,
            providers: ProvidersPanel::new(providers::Mode::Demo),
            settings: Some(config_keys::SettingsPanel::new(true)),
            detail: None,
            focus: FocusPane::Sessions,
            selected: 0,
            recent_selected: 0,
            connected: true,
            proxy_off: false,
            saving: None,
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: None,
            shutdown_complete: Some(mpsc::channel().1),
        };
        let state: MonitorSnapshot = MonitorHandle::default().snapshot().into();

        let header = draw(100, 1, |frame| {
            render_header(frame, frame.area(), &app, &state)
        });

        assert!(buffer_text(&header).contains("http://[::]:18765"));
    }

    #[test]
    fn clamp_selection_caps_to_available_sessions() {
        let mut app = MonitorApp {
            listen_url: "http://127.0.0.1:3000".to_string(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            page: Page::Activity,
            accounts_mode: providers::Mode::Demo,
            providers: ProvidersPanel::new(providers::Mode::Demo),
            settings: Some(config_keys::SettingsPanel::new(true)),
            detail: None,
            focus: FocusPane::Sessions,
            selected: 10,
            recent_selected: 10,
            connected: true,
            proxy_off: false,
            saving: None,
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: None,
            shutdown_complete: Some(mpsc::channel().1),
        };

        app.clamp_selection(3, 4);
        assert_eq!(app.selected, 2);
        assert_eq!(app.recent_selected, 3);

        app.clamp_selection(0, 0);
        assert_eq!(app.selected, 0);
        assert_eq!(app.recent_selected, 0);
    }

    #[test]
    fn arrow_navigation_moves_between_focus_panes_at_edges() {
        let mut app = MonitorApp {
            listen_url: "http://127.0.0.1:3000".to_string(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            page: Page::Activity,
            accounts_mode: providers::Mode::Demo,
            providers: ProvidersPanel::new(providers::Mode::Demo),
            settings: Some(config_keys::SettingsPanel::new(true)),
            detail: None,
            focus: FocusPane::Sessions,
            selected: 1,
            recent_selected: 0,
            connected: true,
            proxy_off: false,
            saving: None,
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: None,
            shutdown_complete: Some(mpsc::channel().1),
        };

        app.move_down(2, 3, true);
        assert_eq!(app.focus, FocusPane::Recent);
        assert_eq!(app.recent_selected, 0);

        app.move_up(2, 3, true);
        assert_eq!(app.focus, FocusPane::Sessions);
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn vim_navigation_stays_within_focused_pane() {
        let mut app = MonitorApp {
            listen_url: "http://127.0.0.1:3000".to_string(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            page: Page::Activity,
            accounts_mode: providers::Mode::Demo,
            providers: ProvidersPanel::new(providers::Mode::Demo),
            settings: Some(config_keys::SettingsPanel::new(true)),
            detail: None,
            focus: FocusPane::Sessions,
            selected: 1,
            recent_selected: 0,
            connected: true,
            proxy_off: false,
            saving: None,
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: None,
            shutdown_complete: Some(mpsc::channel().1),
        };

        app.move_down(2, 3, false);
        assert_eq!(app.focus, FocusPane::Sessions);
        assert_eq!(app.selected, 1);

        app.focus = FocusPane::Recent;
        app.move_up(2, 3, false);
        assert_eq!(app.focus, FocusPane::Recent);
        assert_eq!(app.recent_selected, 0);
    }
}
