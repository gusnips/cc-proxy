use anyhow::Result;
use cc_proxy::{
    config, daemon, legacy_hook, logging,
    monitor::MonitorHandle,
    paths,
    registry::Registry,
    route,
    server::{self, ServerConfig},
    start_flow::{self, Pick},
    tiers,
    tui::{self, MonitorExit, MonitorUiConfig},
    ui::{self, Mood},
};
use clap::{ArgAction, Parser, Subcommand};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Parser)]
#[command(
    name = "cc-proxy",
    version = VERSION,
    about = "Anthropic-compatible proxy for Claude Code provider backends",
    disable_version_flag = true
)]
struct Cli {
    #[arg(long = "version", short = 'v', action = ArgAction::SetTrue)]
    version_flag: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Print version information
    Version,
    /// Old shell functions still call this. It removes the hook and says what to do next
    #[command(hide = true, disable_help_flag = true)]
    Claude {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<std::ffi::OsString>,
    },
    /// Start the proxy as a background service and send Claude Code through it (default)
    #[command(alias = "serve")]
    Start {
        #[arg(long)]
        port: Option<u16>,
        /// Only serve, in the foreground. Leaves Claude Code's settings alone
        #[arg(long = "no-monitor", action = ArgAction::SetTrue)]
        no_monitor: bool,
        /// Run in the foreground with the monitor dashboard. Quitting it switches routing off
        #[arg(long, conflicts_with = "no_monitor", action = ArgAction::SetTrue)]
        monitor: bool,
    },
    /// Switch where Claude Code goes: a provider, or `claude` for your own subscription
    Use {
        /// codex, kimi or claude. Leave it out to pick from a list
        target: Option<String>,
    },
    /// Switch routing off and stop the background proxy service
    Stop,
    /// Show whether the proxy is running and where Claude Code goes
    Status,
    /// Restart the background proxy service
    Restart {
        #[arg(long)]
        port: Option<u16>,
    },
    /// Validate the config file and ask a running service to reload it
    Reload,
    /// Attach a read-only dashboard to a running proxy
    Monitor {
        #[arg(long)]
        url: Option<reqwest::Url>,
    },
    /// Open the monitor TUI with mock data and no proxy server
    #[command(hide = true)]
    Demo,
    /// List supported provider models
    Models {
        #[arg(long)]
        full: bool,
        #[command(subcommand)]
        command: Option<ModelsCommand>,
    },
    /// View or change config.json values
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Update the installed binary to the latest release
    Update {
        /// Only report whether an update is available
        #[arg(long)]
        check: bool,
        /// Install a specific release tag instead of the latest
        #[arg(long)]
        version: Option<String>,
    },
    /// Manage Codex authentication
    Codex {
        #[command(subcommand)]
        command: ProviderGroup,
    },
    /// Manage Kimi authentication
    Kimi {
        #[command(subcommand)]
        command: ProviderGroup,
    },
    /// Manage GitHub Copilot authentication
    Copilot {
        #[command(subcommand)]
        command: ProviderGroup,
    },
    /// Manage Cursor authentication
    Cursor {
        #[command(subcommand)]
        command: ProviderGroup,
    },
    /// Manage Grok authentication
    Grok {
        #[command(subcommand)]
        command: ProviderGroup,
    },
    /// Manage GLM authentication
    Glm {
        #[command(subcommand)]
        command: ProviderGroup,
    },
    /// Manage the OpenCode Go API key
    #[command(name = "opencode")]
    OpenCode {
        #[command(subcommand)]
        command: ProviderGroup,
    },
    /// Show how much of your Codex, Kimi and OpenCode Go plans you have used
    Usage {
        /// Show only this provider; leave it out to see all of them
        provider: Option<cc_proxy::usage::UsageProvider>,
        /// Print JSON instead of text
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ProviderGroup {
    Auth {
        #[command(subcommand)]
        command: cc_proxy::provider::AuthCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ModelsCommand {
    /// Show or choose the model each Claude name (fable, opus, sonnet, haiku) uses
    Map {
        /// The Claude name to choose a model for
        name: Option<String>,
        /// The model it should use
        model: Option<String>,
        /// Forget your choice and let cc-proxy pick again
        #[arg(long)]
        reset: bool,
        /// The provider to change. Defaults to the one Claude Code goes to now
        #[arg(long)]
        provider: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Show one value from config.json
    Get {
        /// Dotted key, e.g. port or opencode.apiKey
        key: String,
    },
    /// Write one value to config.json
    Set {
        /// Dotted key, e.g. port or codex.fullLane
        key: String,
        /// New value
        value: String,
    },
    /// Show all known keys and their values
    List,
    /// Open config.json in $VISUAL or $EDITOR
    Edit,
}

fn main() {
    if let Err(error) = run() {
        ui::print_error(&error);
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    if cli.version_flag {
        println!("cc-proxy {}", VERSION);
        return Ok(());
    }

    let commands = cli.command.unwrap_or(Commands::Start {
        port: None,
        no_monitor: false,
        monitor: false,
    });

    if heals_first(&commands) {
        route::heal();
    }

    match commands {
        Commands::Version => {
            println!("cc-proxy {}", VERSION);
            Ok(())
        }
        Commands::Claude { .. } => old_claude_command(),
        Commands::Start {
            port,
            no_monitor,
            monitor,
        } => {
            if daemon::is_daemon_child() {
                return daemon::serve_daemon_child(port);
            }
            match select_serve_mode(no_monitor, monitor) {
                ServeMode::Daemon => start_daemon(port),
                ServeMode::Plain => serve_plain(port),
                ServeMode::Monitor => serve_with_monitor(port),
            }
        }
        Commands::Use { target } => use_target(target.as_deref()),
        Commands::Demo => {
            let registry = Registry::new();
            tui::run_mock_monitor(config::port(), &registry)
        }
        Commands::Monitor { url } => {
            let from_flag = url.is_some();
            let url = url.unwrap_or_else(|| {
                format!("http://127.0.0.1:{}", config::port())
                    .parse()
                    .expect("local proxy URL")
            });
            if !from_flag && matches!(daemon::describe(), daemon::DaemonStatus::Stopped) {
                anyhow::bail!(
                    "proxy is not running. Start it with `cc-proxy start`, \
                     then attach with `cc-proxy monitor`."
                );
            }
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            let client = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(2))
                .build()?;
            let monitor = runtime.block_on(cc_proxy::monitor::remote::RemoteMonitor::connect(
                client,
                url.clone(),
            ))?;
            tui::run_attached_monitor(|| monitor.snapshot(), url.to_string())?;
            Ok(())
        }
        Commands::Models {
            command:
                Some(ModelsCommand::Map {
                    name,
                    model,
                    reset,
                    provider,
                }),
            ..
        } => cc_proxy::models_map::run(
            name.as_deref(),
            model.as_deref(),
            reset,
            provider.as_deref(),
        ),
        Commands::Models {
            full,
            command: None,
        } => {
            print_models(&Registry::new(), full);
            Ok(())
        }
        Commands::Config { command } => match command {
            ConfigCommand::Get { key } => cc_proxy::config_keys::run_config_get(&key),
            ConfigCommand::Set { key, value } => {
                cc_proxy::config_keys::run_config_set(&key, &value)
            }
            ConfigCommand::List => cc_proxy::config_keys::run_config_list(),
            ConfigCommand::Edit => cc_proxy::config_keys::run_config_edit(),
        },
        Commands::Update { check, version } => {
            cc_proxy::update::run_update(check, version.as_deref())
        }
        Commands::Stop => stop_daemon(),
        Commands::Status => daemon_status(),
        Commands::Restart { port } => restart_daemon(port),
        Commands::Reload => reload_daemon(),
        Commands::Codex { command } => run_provider_cli("codex", command),
        Commands::Kimi { command } => run_provider_cli("kimi", command),
        Commands::Copilot { command } => run_provider_cli("copilot", command),
        Commands::Cursor { command } => run_provider_cli("cursor", command),
        Commands::Grok { command } => run_provider_cli("grok", command),
        Commands::Glm { command } => run_provider_cli("glm", command),
        Commands::OpenCode { command } => run_provider_cli("opencode", command),
        Commands::Usage { provider, json } => match cc_proxy::usage::run(provider, json)? {
            0 => Ok(()),
            code => std::process::exit(code),
        },
    }
}

async fn run_service(config: ServerConfig) -> Result<()> {
    let mut signals = ServiceShutdownSignals::new()?;
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    let server = server::serve_with_shutdown(config, async {
        let _ = stopped.await;
    });
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result,
        signal = signals.recv() => {
            signal?;
            let _ = shutdown.send(());
            tokio::select! {
                result = &mut server => result,
                signal = signals.recv() => {
                    signal?;
                    std::process::exit(130);
                }
            }
        }
    }
}

#[cfg(unix)]
struct ServiceShutdownSignals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl ServiceShutdownSignals {
    fn new() -> std::io::Result<Self> {
        Ok(Self {
            interrupt: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
            terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
        })
    }

    async fn recv(&mut self) -> std::io::Result<()> {
        tokio::select! {
            _ = self.interrupt.recv() => Ok(()),
            _ = self.terminate.recv() => Ok(()),
        }
    }
}

#[cfg(windows)]
struct ServiceShutdownSignals {
    ctrl_c: tokio::signal::windows::CtrlC,
}

#[cfg(windows)]
impl ServiceShutdownSignals {
    fn new() -> std::io::Result<Self> {
        Ok(Self {
            ctrl_c: tokio::signal::windows::ctrl_c()?,
        })
    }

    async fn recv(&mut self) -> std::io::Result<()> {
        let _ = self.ctrl_c.recv().await;
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
struct ServiceShutdownSignals;

#[cfg(not(any(unix, windows)))]
impl ServiceShutdownSignals {
    fn new() -> std::io::Result<Self> {
        Ok(Self)
    }

    async fn recv(&mut self) -> std::io::Result<()> {
        tokio::signal::ctrl_c().await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServeMode {
    Daemon,
    Monitor,
    Plain,
}

fn select_serve_mode(no_monitor: bool, monitor: bool) -> ServeMode {
    if monitor {
        ServeMode::Monitor
    } else if no_monitor {
        ServeMode::Plain
    } else {
        ServeMode::Daemon
    }
}

/// Commands that check, before they run, that a routed Claude Code still has
/// a proxy to talk to. The ones that start or stop the proxy do their own.
fn heals_first(command: &Commands) -> bool {
    !matches!(
        command,
        Commands::Start { .. }
            | Commands::Stop
            | Commands::Use { .. }
            | Commands::Restart { .. }
            | Commands::Version
            | Commands::Demo
            | Commands::Claude { .. }
    )
}

/// What the old `claude` shell function runs. It has nothing left to start:
/// take the function out, and say what to do.
fn old_claude_command() -> Result<()> {
    let removed = legacy_hook::remove()?;
    let took_out = if removed.is_empty() {
        String::new()
    } else {
        format!(" cc-proxy took it out of {}.", removed.join(", "))
    };
    anyhow::bail!(
        "The shell hook is gone.{took_out} Run `unset -f claude` or open a new terminal, \
         then run `claude` directly."
    )
}

/// Remove the hook older versions added, returning a note when there was one.
/// A file that can't be written is worth a note, not a failed start.
fn remove_old_hook() -> Option<String> {
    match legacy_hook::remove() {
        Ok(files) if files.is_empty() => None,
        Ok(files) => Some(format!(
            "Removed the old `claude` hook from {}. Open a new terminal, or run \
             `unset -f claude`.",
            files.join(", ")
        )),
        Err(error) => Some(format!("Couldn't remove the old `claude` hook: {error:#}")),
    }
}

/// A proxy that is up, and what to tell the user about it.
struct Proxy {
    /// Where Claude Code reaches it.
    url: String,
    port: u16,
    lines: Vec<String>,
}

/// Start the background proxy, or take the one that already runs: ours, or
/// one something else started (a Homebrew service).
fn ensure_proxy(port: Option<u16>) -> Result<Proxy> {
    if let daemon::DaemonStatus::Unmanaged { port: serving } = daemon::describe()
        && port.is_none_or(|wanted| wanted == serving)
    {
        return Ok(Proxy {
            url: daemon::client_url("127.0.0.1", serving),
            port: serving,
            lines: vec![
                "cc-proxy is already running".into(),
                format!(
                    "It serves port {serving} and cc-proxy didn't start it, so `cc-proxy stop` \
                     can't stop it."
                ),
            ],
        });
    }
    let label = format!(
        "starting cc-proxy on port {}",
        port.unwrap_or_else(config::port)
    );
    Ok(
        match ui::waiting(&label, || daemon::serve_background(port))? {
            daemon::ServeOutcome::AlreadyRunning(info) => {
                let mut lines = vec!["cc-proxy is already running".into(), address(&info)];
                if let Some(wanted) = port.filter(|wanted| *wanted != info.port) {
                    lines.push(format!(
                        "It serves port {}, so `--port {wanted}` was ignored. Move it with \
                     `cc-proxy restart --port {wanted}`, or run a second one in the \
                     foreground with `cc-proxy start --no-monitor --port {wanted}`.",
                        info.port,
                    ));
                }
                Proxy {
                    url: info.listen_url(),
                    port: info.port,
                    lines,
                }
            }
            daemon::ServeOutcome::Started(info) => Proxy {
                url: info.listen_url(),
                port: info.port,
                lines: vec![
                    "cc-proxy started".into(),
                    address(&info),
                    format!("Logs go to {}", paths::log_file().display()),
                ],
            },
        },
    )
}

/// Point Claude Code at the proxy for a provider, or back at Anthropic.
fn set_route(pick: Pick, url: &str) -> Result<()> {
    match pick {
        Pick::Provider(provider) => route::activate(provider, url),
        Pick::Claude => route::deactivate(),
    }
}

/// Where Claude Code goes now, and what to run to change that.
fn route_lines(pick: Pick) -> Vec<String> {
    let mut lines = route::status_lines();
    if let Pick::Provider(provider) = pick {
        lines.insert(
            1,
            "Windows that are already open follow within a few seconds.".into(),
        );
        let mappings = tiers::mapping(&Registry::new(), &config::route(), provider);
        lines.splice(2..2, tiers::describe(&mappings));
    }
    lines
}

fn start_daemon(port: Option<u16>) -> Result<()> {
    route::preflight()?;
    let hook_note = remove_old_hook();
    let pick = start_flow::pick(None)?;
    let proxy = ensure_proxy(port)?;
    set_route(pick, &proxy.url)?;

    let mut lines = proxy.lines;
    lines.extend(route_lines(pick));
    lines.extend(route::version_note(proxy.port));
    lines.extend(hook_note);
    lines.push("`cc-proxy monitor` shows traffic. `cc-proxy stop` switches routing off.".into());
    ui::print_card(Mood::Awake, &lines);
    Ok(())
}

/// `cc-proxy use`: switch where Claude Code goes, with the proxy kept running.
fn use_target(target: Option<&str>) -> Result<()> {
    route::preflight()?;
    let pick = start_flow::pick(target)?;
    let mut lines = Vec::new();
    let mut proxy_port = None;
    match pick {
        Pick::Provider(_) => {
            let proxy = ensure_proxy(None)?;
            set_route(pick, &proxy.url)?;
            proxy_port = Some(proxy.port);
            lines.extend(proxy.lines);
        }
        Pick::Claude => route::deactivate()?,
    }
    lines.extend(route_lines(pick));
    lines.extend(proxy_port.and_then(route::version_note));
    ui::print_card(Mood::Awake, &lines);
    Ok(())
}

/// Serve in the foreground and leave Claude Code's settings alone. This is
/// what Homebrew and systemd run, so a reboot never routes anything.
fn serve_plain(port: Option<u16>) -> Result<()> {
    let bind_address = config::bind_address();
    let effective_port = port.unwrap_or_else(config::port);
    let registry = Registry::new();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    print_server_banner(&bind_address, effective_port, &registry);
    runtime
        .block_on(run_service(ServerConfig {
            bind_address,
            port: effective_port,
            monitor: Some(MonitorHandle::default()),
        }))
        .map_err(|err| anyhow::anyhow!(err))
}

/// Serve in the foreground with the dashboard. Routing lasts as long as the
/// dashboard does.
fn serve_with_monitor(port: Option<u16>) -> Result<()> {
    route::preflight()?;
    if let Some(note) = remove_old_hook() {
        ui::print_note(Mood::Awake, &[note]);
    }
    // Asked before the dashboard takes the screen.
    let pick = start_flow::pick(None)?;
    let bind_address = config::bind_address();
    let effective_port = port.unwrap_or_else(config::port);
    let registry = Registry::new();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let stderr_guard = logging::suppress_stderr();
    let monitor = MonitorHandle::default();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let (shutdown_complete_tx, shutdown_complete_rx) = std::sync::mpsc::channel();
    let listener = runtime.block_on(server::bind_proxy_listener(&bind_address, effective_port))?;
    let local_addr = listener.local_addr()?;
    let ip = local_addr.ip().to_string();
    set_route(pick, &daemon::client_url(&ip, local_addr.port()))?;
    let monitor_listen_url = listen_url(&ip, local_addr.port());
    let server_monitor = monitor.clone();
    let server_task = runtime.spawn(async move {
        let result = server::serve_listener(listener, Some(server_monitor), async move {
            let _ = shutdown_rx.await;
        })
        .await;
        let _ = shutdown_complete_tx.send(());
        result
    });
    let ui_result = tui::run_monitor(
        monitor,
        MonitorUiConfig {
            listen_url: monitor_listen_url,
            port: effective_port,
            registry: &registry,
            shutdown: Some(shutdown_tx),
            shutdown_complete: Some(shutdown_complete_rx),
        },
    );
    // The dashboard is gone, and so is the proxy: switch routing off before
    // anything below can return early.
    let routing_off = route::deactivate();
    drop(stderr_guard);
    if let Err(error) = &routing_off {
        ui::eprint_note(
            Mood::Unsure,
            &[format!(
                "Couldn't switch Claude Code back to your subscription: {error:#}"
            )],
        );
    }
    if matches!(&ui_result, Ok(MonitorExit::ForceQuit)) {
        server_task.abort();
        let _ = runtime.block_on(server_task);
        std::process::exit(130);
    }
    let server_result = runtime.block_on(server_task)?;
    ui_result?;
    server_result.map_err(|err| anyhow::anyhow!(err))
}

fn address(info: &daemon::DaemonInfo) -> String {
    format!("{} · pid {}", info.listen_url(), info.pid)
}

fn start_hint() -> &'static str {
    "Start it with `cc-proxy start`."
}

fn stop_daemon() -> Result<()> {
    // Routing goes off first, so Claude Code is back on your subscription even
    // when the proxy is slow to stop or isn't ours to stop.
    let routing_off = route::deactivate();
    let (mood, mut lines) = match ui::waiting("stopping cc-proxy", daemon::stop_service)? {
        daemon::StopOutcome::NotRunning => (Mood::Asleep, vec!["cc-proxy is not running".into()]),
        daemon::StopOutcome::Stopped { pid } => (
            Mood::Asleep,
            vec!["cc-proxy stopped".into(), format!("pid {pid} has exited.")],
        ),
        daemon::StopOutcome::Unmanaged { port } => (
            Mood::Unsure,
            vec![
                format!("Something answers on port {port}, but cc-proxy didn't start it"),
                "Stop it with `brew services stop cc-proxy`, or stop that process yourself.".into(),
            ],
        ),
    };
    match &routing_off {
        Ok(()) => lines.push("Claude Code uses your Claude subscription.".into()),
        Err(error) => lines.push(format!(
            "Couldn't switch Claude Code back to your subscription: {error:#}"
        )),
    }
    lines.push(start_hint().into());
    ui::print_card(mood, &lines);
    Ok(())
}

fn daemon_status() -> Result<()> {
    match daemon::describe() {
        daemon::DaemonStatus::Running(info) => {
            let headline = match uptime(&info.started_at) {
                Some(up) => format!("cc-proxy is running · up {up}"),
                None => "cc-proxy is running".into(),
            };
            let mut lines = vec![headline, address(&info)];
            lines.extend(route::status_lines());
            lines.extend(route::version_note(info.port));
            ui::print_card(Mood::Awake, &lines);
            Ok(())
        }
        daemon::DaemonStatus::Unmanaged { port } => {
            let mut lines = vec![
                format!("Something answers on port {port}, but cc-proxy didn't start it"),
                "It has no pidfile, so `cc-proxy stop` can't stop it. Stop that process \
                 yourself, or with `brew services stop cc-proxy`."
                    .into(),
            ];
            lines.extend(route::status_lines());
            lines.extend(route::version_note(port));
            ui::print_card(Mood::Unsure, &lines);
            Ok(())
        }
        daemon::DaemonStatus::Stopped => {
            ui::print_card(
                Mood::Asleep,
                &["cc-proxy is not running".into(), start_hint().into()],
            );
            std::process::exit(1);
        }
    }
}

/// "2h14m" since `started_at`, an RFC 3339 time from the pidfile.
fn uptime(started_at: &str) -> Option<String> {
    let started = started_at.parse::<jiff::Timestamp>().ok()?;
    let up = std::time::Duration::try_from(jiff::Timestamp::now().duration_since(started)).ok()?;
    Some(ui::duration(up))
}

fn restart_daemon(port: Option<u16>) -> Result<()> {
    let info = ui::waiting("restarting cc-proxy", || daemon::restart_service(port))?;
    let mut lines = vec!["cc-proxy restarted".into(), address(&info)];
    // A new port means Claude Code's settings point at the old one.
    if route::refresh(&info.listen_url())? {
        lines.push("Claude Code's settings now point at the new address.".into());
    }
    ui::print_card(Mood::Awake, &lines);
    Ok(())
}

fn reload_daemon() -> Result<()> {
    match daemon::reload_service()? {
        daemon::ReloadOutcome::Reloaded => ui::print_card(
            Mood::Glad,
            &[
                "config reloaded".into(),
                "Settings from the file apply on the next request.".into(),
                "Bind address, port and environment variables need `cc-proxy restart`.".into(),
            ],
        ),
        daemon::ReloadOutcome::ValidatedOnly => ui::print_card(
            Mood::Unsure,
            &[
                "config is valid".into(),
                "This platform can't reload a running proxy. Run `cc-proxy restart` to \
                 apply the changes."
                    .into(),
            ],
        ),
        daemon::ReloadOutcome::NotRunning => {
            ui::print_card(
                Mood::Asleep,
                &[
                    "cc-proxy is not running, but the config file is valid".into(),
                    start_hint().into(),
                ],
            );
            std::process::exit(1);
        }
    }
    Ok(())
}

fn run_provider_cli(name: &str, command: ProviderGroup) -> Result<()> {
    let registry = Registry::new();
    let provider = registry
        .provider(name)
        .ok_or_else(|| anyhow::anyhow!("unknown provider: {name}"))?;
    let handlers = provider.cli();
    match command {
        ProviderGroup::Auth { command } => match command {
            cc_proxy::provider::AuthCommand::Login => {
                if let Err(err) = handlers.login() {
                    eprintln!("{err}");
                    std::process::exit(2);
                }
                Ok(())
            }
            cc_proxy::provider::AuthCommand::Device => {
                if let Err(err) = handlers.device() {
                    eprintln!("{err}");
                    std::process::exit(2);
                }
                Ok(())
            }
            cc_proxy::provider::AuthCommand::Status => {
                if let Err(err) = handlers.status() {
                    println!("{err}");
                    if err.to_string() == "Not authenticated" {
                        std::process::exit(1);
                    }
                    std::process::exit(2);
                }
                Ok(())
            }
            cc_proxy::provider::AuthCommand::Logout => {
                handlers.logout()?;
                Ok(())
            }
        },
    }
}

fn print_models(registry: &Registry, full: bool) {
    let grouped = registry.grouped_models();
    let styled = ui::styled(&std::io::stdout());
    for provider in [
        "codex", "kimi", "grok", "opencode", "copilot", "cursor", "glm",
    ] {
        let Some(models) = grouped.get(provider) else {
            continue;
        };
        let name = ui::strong(provider, ui::provider_color(provider), styled);
        if full || provider != "cursor" {
            println!("{name}: {}", models.join(", "));
        } else {
            println!("{name}: {}", compact_cursor_list(models));
        }
    }
}

fn compact_cursor_list(models: &[String]) -> String {
    let (dynamic, legacy): (Vec<&String>, Vec<&String>) =
        models.iter().partition(|model| model.contains(':'));
    let mut out = legacy
        .iter()
        .map(|model| model.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    // The legacy names are all the registry lists; `cursor:<model>` takes
    // any id Cursor knows, so the line says so instead of counting zero.
    if dynamic.is_empty() {
        out.push_str("; or cursor:<model> for any model Cursor offers, e.g. cursor:gpt-5.5");
    } else {
        out.push_str(&format!(
            "; {} cursor model aliases, e.g. cursor:gpt-5.5. Run `cc-proxy models --full` for all of them",
            dynamic.len()
        ));
    }
    out
}

fn listen_url(bind_address: &str, port: u16) -> String {
    match bind_address.parse::<std::net::IpAddr>() {
        Ok(ip) => format!("http://{}", std::net::SocketAddr::new(ip, port)),
        Err(_) => format!("http://{bind_address}:{port}"),
    }
}

fn print_server_banner(bind_address: &str, port: u16, registry: &Registry) {
    println!("Proxy listening on {}", listen_url(bind_address, port));
    println!("Logs: {}", paths::log_file().display());
    let cfg = paths::config_dir();
    if cfg.exists() {
        println!("Config: {}", cfg.display());
    }
    print_models(registry, false);
    println!();
    println!("This only serves. Run `cc-proxy start` to send Claude Code through a provider.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_serve_selects_daemon() {
        assert_eq!(select_serve_mode(false, false), ServeMode::Daemon);
    }

    #[test]
    fn no_monitor_selects_plain_mode() {
        assert_eq!(select_serve_mode(true, false), ServeMode::Plain);
    }

    #[test]
    fn monitor_flag_selects_monitor_mode() {
        assert_eq!(select_serve_mode(false, true), ServeMode::Monitor);
    }

    #[test]
    fn lifecycle_commands_parse() {
        for args in [
            vec!["cc-proxy", "stop"],
            vec!["cc-proxy", "status"],
            vec!["cc-proxy", "reload"],
            vec!["cc-proxy", "restart"],
            vec!["cc-proxy", "restart", "--port", "18766"],
            vec!["cc-proxy", "start", "--monitor"],
            vec!["cc-proxy", "start", "--no-monitor"],
            vec!["cc-proxy", "serve", "--no-monitor"],
            vec!["cc-proxy", "use"],
            vec!["cc-proxy", "use", "kimi"],
            vec!["cc-proxy", "use", "claude"],
        ] {
            assert!(Cli::try_parse_from(&args).is_ok(), "{args:?}");
        }
        assert!(Cli::try_parse_from(["cc-proxy", "start", "--monitor", "--no-monitor"]).is_err());
    }

    #[test]
    fn the_removed_commands_are_gone() {
        for command in ["setup", "shell", "on", "off"] {
            assert!(
                Cli::try_parse_from(["cc-proxy", command]).is_err(),
                "{command}"
            );
        }
    }

    #[test]
    fn old_claude_function_arguments_still_parse() {
        let cli = Cli::try_parse_from([
            "cc-proxy", "claude", "--resume", "abc", "-p", "hi", "-v", "--help", "--",
        ])
        .unwrap();

        let command = cli.command.unwrap();
        assert!(matches!(command, Commands::Claude { .. }));
        assert!(!heals_first(&command));
    }

    #[test]
    fn demo_command_parses_without_server_options() {
        let cli = Cli::try_parse_from(["cc-proxy", "demo"]).unwrap();

        assert!(matches!(cli.command, Some(Commands::Demo)));
    }

    #[tokio::test]
    async fn shutdown_signal_setup_and_receive_preserve_io_results() {
        fn assert_constructor(_: fn() -> std::io::Result<ServiceShutdownSignals>) {}
        fn assert_io_future<F: std::future::Future<Output = std::io::Result<()>>>(_: &F) {}

        assert_constructor(ServiceShutdownSignals::new);
        let mut signals = ServiceShutdownSignals::new().unwrap();
        let receive = signals.recv();
        assert_io_future(&receive);
    }

    #[test]
    fn usage_command_parses_provider_and_json_flag() {
        let cli = Cli::try_parse_from(["cc-proxy", "usage", "opencode", "--json"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Usage {
                provider: Some(cc_proxy::usage::UsageProvider::OpenCode),
                json: true
            })
        ));

        let cli = Cli::try_parse_from(["cc-proxy", "usage"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Usage {
                provider: None,
                json: false
            })
        ));
        assert!(Cli::try_parse_from(["cc-proxy", "opencode", "usage"]).is_err());
    }

    #[test]
    fn listen_url_brackets_ipv6_addresses() {
        assert_eq!(listen_url("::1", 18765), "http://[::1]:18765");
    }
}
