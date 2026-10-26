use anyhow::Result;
use cc_proxy::{
    config, daemon, legacy_hook, logging,
    monitor::MonitorHandle,
    paths,
    registry::Registry,
    route,
    server::{self, ServerConfig},
    start_flow::{self, Pick},
    tiers, tips,
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
    /// Start the proxy as a background service and send Claude Code through it (default)
    #[command(alias = "serve")]
    Start {
        /// Provider to send Claude Code to (or `direct` to bypass the proxy). Leave it out to pick from a list
        target: Option<String>,
        #[arg(long)]
        port: Option<u16>,
        /// Only serve, in the foreground. Leaves Claude Code's settings alone
        #[arg(long = "no-monitor", action = ArgAction::SetTrue)]
        no_monitor: bool,
        /// Run in the foreground with the monitor dashboard. Quitting it switches routing off
        #[arg(long, conflicts_with = "no_monitor", action = ArgAction::SetTrue)]
        monitor: bool,
    },
    /// Put a provider first, set the order of several, or use `direct` to bypass the proxy
    Use {
        /// A provider, or `direct` to bypass the proxy. One provider moves to the top of the pool and the rest keep their order. Several set the order requests try them, and turn the rest off. Leave it out to pick from a list
        #[arg(value_name = "PROVIDER")]
        targets: Vec<String>,
        /// Turn every other provider off, so only this provider is used
        #[arg(long)]
        only: bool,
    },
    /// See the order requests try providers and accounts, turn them on or off, and move them
    Pool {
        #[command(subcommand)]
        command: Option<PoolCommand>,
    },
    /// Switch routing off and stop the background proxy service
    Stop,
    /// Show whether the proxy is running and where Claude Code goes
    Status,
    /// List every provider and where Claude Code goes, or add and remove your own
    Provider {
        #[command(subcommand)]
        command: Option<ProviderCommand>,
    },
    /// Sign in to providers, or see who is signed in (login, device, status, logout)
    Auth {
        #[command(subcommand)]
        command: Option<cc_proxy::provider::AuthCommand>,
    },
    /// Sign in to a provider (same as `auth login`)
    Login(cc_proxy::provider::LoginArgs),
    /// Sign out of a provider (same as `auth logout`)
    Logout(cc_proxy::provider::LogoutArgs),
    /// See or choose the model each Claude name uses, and list provider models
    Models {
        #[command(subcommand)]
        command: Option<ModelsCommand>,
    },
    /// Show how much of your Codex, Kimi and OpenCode Go plans you have used
    Usage {
        /// Show only this provider; leave it out to see all of them
        provider: Option<cc_proxy::usage::UsageProvider>,
        /// Print JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Open accounts, settings and live activity. If the proxy is off, press s to start it
    #[command(visible_aliases = ["monitor", "show"])]
    Open {
        #[arg(long)]
        url: Option<reqwest::Url>,
    },
    /// Restart the background proxy service
    Restart {
        #[arg(long)]
        port: Option<u16>,
    },
    /// Validate the config file and ask a running service to reload it
    Reload,
    /// Manage the background proxy service (start, stop, status, restart, reload)
    Service {
        #[command(subcommand)]
        command: Option<ServiceCommand>,
    },
    /// View or change config.json values. With no subcommand, opens a settings screen
    Config {
        #[command(subcommand)]
        command: Option<ConfigCommand>,
    },
    /// Update the installed binary to the latest release
    Update {
        /// Only report whether an update is available
        #[arg(long)]
        check: bool,
        /// Install a specific release tag instead of the latest
        #[arg(long)]
        version: Option<String>,
        /// Reinstall even when the installed version already matches
        #[arg(long)]
        force: bool,
    },
    /// Print version information
    Version,
    /// Old shell functions still call this. It removes the hook and says what to do next
    #[command(hide = true, disable_help_flag = true)]
    Claude {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<std::ffi::OsString>,
    },
    /// Open the monitor TUI with mock data and no proxy server
    #[command(hide = true)]
    Demo,
}

#[derive(Debug, Clone, Subcommand)]
enum PoolCommand {
    /// Show the order requests try, with each row on or off
    List,
    /// Turn on a provider, or one of its accounts
    On {
        provider: String,
        /// One account of the provider. Leave it out for all of them
        account: Option<String>,
    },
    /// Turn off a provider, or one of its accounts
    Off {
        provider: String,
        /// One account of the provider. Leave it out for all of them
        account: Option<String>,
    },
    /// Move a provider or an account up or down one place
    Move {
        provider: String,
        /// `up` or `down`, after the account when the provider has several
        #[arg(num_args = 1..=2, value_name = "ACCOUNT> <up|down")]
        rest: Vec<String>,
    },
}

#[derive(Debug, Clone, Subcommand)]
enum ServiceCommand {
    /// Start the proxy as a background service and send Claude Code through it
    #[command(alias = "serve")]
    Start {
        /// Provider to send Claude Code to (or `direct` to bypass the proxy). Leave it out to pick from a list
        target: Option<String>,
        #[arg(long)]
        port: Option<u16>,
        /// Only serve, in the foreground. Leaves Claude Code's settings alone
        #[arg(long = "no-monitor", action = ArgAction::SetTrue)]
        no_monitor: bool,
        /// Run in the foreground with the monitor dashboard. Quitting it switches routing off
        #[arg(long, conflicts_with = "no_monitor", action = ArgAction::SetTrue)]
        monitor: bool,
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
}

#[derive(Debug, Clone, Subcommand)]
enum ProviderCommand {
    /// Add or update a custom provider endpoint in config.json
    Add {
        /// Short name for the provider (e.g. openrouter, ollama)
        name: String,
        /// Base URL for the endpoint (e.g. https://openrouter.ai/api/v1)
        base_url: String,
        /// Wire protocol: openai (chat/completions), responses, or anthropic (messages)
        #[arg(long, value_enum, default_value_t = cc_proxy::config::CustomFormat::OpenAi)]
        format: cc_proxy::config::CustomFormat,
        /// Model IDs this provider serves (repeat --model for multiple). Leave it out to ask the
        /// endpoint, and `cc-proxy models refresh <name>` asks again later
        #[arg(long = "model", short = 'm')]
        models: Vec<String>,
        /// Environment variable that holds the API key
        #[arg(long)]
        api_key_env: Option<String>,
    },
    /// Remove a custom provider from config.json
    Remove {
        /// Name of the custom provider to remove. Leave it out to pick from a list
        name: Option<String>,
    },
    /// List every provider, its sign-in state and where Claude Code goes
    List,
}

#[derive(Debug, Subcommand)]
enum ModelsCommand {
    /// Show the model each Claude name (fable, opus, sonnet, haiku) uses
    Show {
        /// Provider to show. Defaults to the one Claude Code goes to now
        provider: Option<String>,
    },
    /// List every model a provider serves, or every provider's models
    List {
        /// Provider to list models for
        provider: Option<String>,
        /// Print every Cursor alias
        #[arg(long)]
        full: bool,
    },
    /// Choose the model each Claude name uses, with search
    Choose {
        /// Provider to change. Defaults to the one Claude Code goes to now
        provider: Option<String>,
    },
    /// Set the model one Claude name uses
    Set {
        /// The Claude name: fable, opus, sonnet or haiku
        name: String,
        /// The model it should use
        model: String,
        /// Provider to change. Defaults to the one Claude Code goes to now
        #[arg(long)]
        provider: Option<String>,
    },
    /// Forget your choice for one Claude name, or for all four
    Reset {
        /// The Claude name to forget. All four when left out
        name: Option<String>,
        /// Provider to change. Defaults to the one Claude Code goes to now
        #[arg(long)]
        provider: Option<String>,
    },
    /// Fetch a provider's live model list (or `all` to refresh every provider)
    Refresh {
        /// Provider to refresh, or `all`. Defaults to picking from a list
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
        target: None,
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
            target,
            port,
            no_monitor,
            monitor,
        } => run_service_command(ServiceCommand::Start {
            target,
            port,
            no_monitor,
            monitor,
        }),
        Commands::Use { targets, only } => use_target(&targets, only),
        Commands::Pool { command } => match command.unwrap_or(PoolCommand::List) {
            PoolCommand::List => cc_proxy::pool_cli::list(),
            PoolCommand::On { provider, account } => {
                cc_proxy::pool_cli::turn(true, &provider, account.as_deref())
            }
            PoolCommand::Off { provider, account } => {
                cc_proxy::pool_cli::turn(false, &provider, account.as_deref())
            }
            PoolCommand::Move { provider, rest } => cc_proxy::pool_cli::shift(&provider, &rest),
        },
        Commands::Demo => {
            let registry = Registry::new();
            tui::run_mock_monitor(config::port(), &registry)
        }
        Commands::Open { url } => {
            let from_flag = url.is_some();
            let url = url.unwrap_or_else(|| {
                format!("http://127.0.0.1:{}", config::port())
                    .parse()
                    .expect("local proxy URL")
            });
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            let client = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(2))
                .build()?;
            // A local proxy may be off: the monitor opens anyway and offers to start it.
            // An explicit URL that doesn't answer is most likely a typo, so it fails.
            let monitor = runtime.block_on(async {
                if from_flag {
                    cc_proxy::monitor::remote::RemoteMonitor::connect(client, url.clone()).await
                } else {
                    cc_proxy::monitor::remote::RemoteMonitor::wait_for(client, url.clone()).await
                }
            })?;
            tui::run_attached_monitor(|| monitor.snapshot(), url.to_string(), !from_flag)?;
            Ok(())
        }
        Commands::Models { command } => {
            use cc_proxy::models_map;
            match command.unwrap_or(ModelsCommand::Show { provider: None }) {
                ModelsCommand::Show { provider } => models_map::run_show(provider.as_deref()),
                ModelsCommand::List { provider, full } => {
                    let registry = Registry::new();
                    let picked = if provider.is_none() && cc_proxy::prompt::is_interactive() {
                        let providers = route::routable_providers(&registry);
                        let mut options = vec!["All providers".to_string()];
                        options.extend(
                            providers
                                .iter()
                                .map(|id| format!("{} ({id})", ui::provider_name(id))),
                        );
                        let idx = cc_proxy::prompt::choose(
                            "Which provider's models do you want to list?",
                            &options,
                            0,
                        )?;
                        idx.checked_sub(1).map(|i| providers[i].to_string())
                    } else {
                        provider
                    };
                    print_models(&registry, full, picked.as_deref());
                    Ok(())
                }
                ModelsCommand::Choose { provider } => models_map::run_choose(provider.as_deref()),
                ModelsCommand::Set {
                    name,
                    model,
                    provider,
                } => models_map::run_set(&name, &model, provider.as_deref()),
                ModelsCommand::Reset { name, provider } => {
                    models_map::run_reset(name.as_deref(), provider.as_deref())
                }
                ModelsCommand::Refresh { provider } => models_map::run_refresh(provider.as_deref()),
            }
        }
        Commands::Config { command } => match command {
            None => {
                cc_proxy::config_keys::run_config_screen()?;
                route::refresh_model_labels();
                Ok(())
            }
            Some(ConfigCommand::Get { key }) => cc_proxy::config_keys::run_config_get(&key),
            Some(ConfigCommand::Set { key, value }) => {
                cc_proxy::config_keys::run_config_set(&key, &value)?;
                route::refresh_model_labels();
                Ok(())
            }
            Some(ConfigCommand::List) => cc_proxy::config_keys::run_config_list(),
            Some(ConfigCommand::Edit) => {
                cc_proxy::config_keys::run_config_edit()?;
                route::refresh_model_labels();
                Ok(())
            }
        },
        Commands::Update {
            check,
            version,
            force,
        } => cc_proxy::update::run_update(check, version.as_deref(), force),
        Commands::Stop => run_service_command(ServiceCommand::Stop),
        Commands::Status => run_service_command(ServiceCommand::Status),
        Commands::Restart { port } => run_service_command(ServiceCommand::Restart { port }),
        Commands::Reload => run_service_command(ServiceCommand::Reload),
        Commands::Service { command } => {
            run_service_command(command.unwrap_or(ServiceCommand::Status))
        }
        Commands::Auth { command } => run_auth_cli(command),
        Commands::Login(args) => run_auth_cli(Some(cc_proxy::provider::AuthCommand::Login(args))),
        Commands::Logout(args) => run_auth_cli(Some(cc_proxy::provider::AuthCommand::Logout(args))),
        Commands::Provider { command } => match command.unwrap_or(ProviderCommand::List) {
            ProviderCommand::Add {
                name,
                base_url,
                format,
                models,
                api_key_env,
            } => cc_proxy::providers::custom::run_provider_add(
                &name,
                &base_url,
                format,
                models,
                api_key_env,
            ),
            ProviderCommand::Remove { name } => {
                cc_proxy::providers::custom::run_provider_remove(name.as_deref())
            }
            ProviderCommand::List => {
                cc_proxy::providers::custom::run_provider_list(&Registry::new())
            }
        },
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

fn run_service_command(command: ServiceCommand) -> Result<()> {
    match command {
        ServiceCommand::Start {
            target,
            port,
            no_monitor,
            monitor,
        } => {
            if daemon::is_daemon_child() {
                return daemon::serve_daemon_child(port);
            }
            match select_serve_mode(no_monitor, monitor) {
                ServeMode::Daemon => start_daemon(target.as_deref(), port),
                ServeMode::Plain => serve_plain(port),
                ServeMode::Monitor => serve_with_monitor(target.as_deref(), port),
            }
        }
        ServiceCommand::Stop => stop_daemon(),
        ServiceCommand::Status => daemon_status(),
        ServiceCommand::Restart { port } => restart_daemon(port),
        ServiceCommand::Reload => reload_daemon(),
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
            | Commands::Service {
                command: Some(
                    ServiceCommand::Start { .. }
                        | ServiceCommand::Stop
                        | ServiceCommand::Restart { .. }
                )
            }
            | Commands::Version
            | Commands::Demo
            | Commands::Open { url: Some(_) }
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

/// Point Claude Code at the proxy for a provider, or restore its original settings.
fn set_route(pick: Pick, url: &str, chain: route::Chain) -> Result<()> {
    match pick {
        Pick::Provider(provider) => route::activate(provider, url, chain),
        Pick::Direct => route::deactivate(),
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
        lines.splice(2..2, tiers::describe(provider, &mappings));
    }
    lines
}

fn start_daemon(target: Option<&str>, port: Option<u16>) -> Result<()> {
    route::preflight()?;
    let hook_note = remove_old_hook();
    let pick = start_flow::pick(target)?;
    let proxy = ensure_proxy(port)?;
    set_route(pick, &proxy.url, route::Chain::Keep)?;

    let mut lines = proxy.lines;
    lines.extend(route_lines(pick));
    lines.extend(route::version_note(proxy.port));
    lines.extend(hook_note);
    lines.push("`cc-proxy open` shows traffic. `cc-proxy stop` switches routing off.".into());
    ui::print_card(Mood::Awake, &lines);
    tips::print("start");
    Ok(())
}

/// `cc-proxy use`: switch where Claude Code goes, with the proxy kept running.
fn use_target(targets: &[String], only: bool) -> Result<()> {
    if targets.len() > 1
        && targets
            .iter()
            .any(|target| target.trim().eq_ignore_ascii_case(route::DIRECT))
    {
        anyhow::bail!(
            "`direct` bypasses the proxy and can't be listed with providers. \
             Run `cc-proxy use direct` on its own, or list only providers."
        );
    }
    if only && targets.len() > 1 {
        anyhow::bail!(
            "`--only` leaves one provider on, so it can't come with a list of providers. \
             Name one provider, or drop the flag."
        );
    }
    route::preflight()?;
    let pick = start_flow::pick(targets.first().map(String::as_str))?;
    let mut lines = Vec::new();
    let mut proxy_port = None;
    match pick {
        Pick::Provider(_) => {
            // Checked before the proxy starts, so a typo changes nothing. One
            // provider goes to the top and the rest keep their order. A list or
            // the flag turns the others off.
            let chain = match targets.get(1..) {
                Some(rest) if !rest.is_empty() => route::Chain::Set(route::clean_chain(rest)?),
                _ if only => route::Chain::Set(Vec::new()),
                _ => route::Chain::Keep,
            };
            let proxy = ensure_proxy(None)?;
            set_route(pick, &proxy.url, chain)?;
            proxy_port = Some(proxy.port);
            lines.extend(proxy.lines);
        }
        Pick::Direct => {
            route::deactivate()?;
            lines.push("Routing is off. This command does not stop the proxy.".into());
        }
    }
    lines.extend(route_lines(pick));
    lines.extend(proxy_port.and_then(route::version_note));
    ui::print_card(Mood::Awake, &lines);
    tips::print("use");
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
fn serve_with_monitor(target: Option<&str>, port: Option<u16>) -> Result<()> {
    route::preflight()?;
    if let Some(note) = remove_old_hook() {
        ui::print_note(Mood::Awake, &[note]);
    }
    // Asked before the dashboard takes the screen.
    let pick = start_flow::pick(target)?;
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
    set_route(
        pick,
        &daemon::client_url(&ip, local_addr.port()),
        route::Chain::Keep,
    )?;
    let monitor_listen_url = daemon::client_url(&ip, local_addr.port());
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
                "Couldn't restore Claude Code's original settings: {error:#}"
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
    // Restore Claude Code's settings first, even when the proxy is slow to stop
    // or isn't ours to stop.
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
        Ok(()) => lines
            .push("Claude Code uses its original settings and login, without the proxy.".into()),
        Err(error) => lines.push(format!(
            "Couldn't restore Claude Code's original settings: {error:#}"
        )),
    }
    lines.push(start_hint().into());
    ui::print_card(mood, &lines);
    tips::print("stop");
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
            tips::print("status");
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
    let refreshed = route::refresh(&info.listen_url())?;
    lines.extend(route::status_lines());
    if refreshed {
        lines.push("Claude Code's settings were refreshed.".into());
    }
    ui::print_card(Mood::Awake, &lines);
    tips::print("restart");
    Ok(())
}

fn reload_daemon() -> Result<()> {
    match daemon::reload_service()? {
        daemon::ReloadOutcome::Reloaded => {
            let mut lines = vec!["config reloaded".into()];
            lines.extend(route::status_lines());
            lines.push("Settings from the file apply on the next request.".into());
            lines.push(
                "Bind address, port and environment variables need `cc-proxy restart`.".into(),
            );
            ui::print_card(Mood::Glad, &lines);
        }
        daemon::ReloadOutcome::ValidatedOnly => {
            let mut lines = vec!["config is valid".into()];
            lines.extend(route::status_lines());
            lines.push(
                "This platform can't reload a running proxy. Run `cc-proxy restart` to \
                 apply the changes."
                    .into(),
            );
            ui::print_card(Mood::Unsure, &lines);
        }
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
    route::refresh_model_labels();
    Ok(())
}

fn pick_auth_provider(registry: &Registry, asked: Option<&str>, question: &str) -> Result<String> {
    if let Some(name) = asked {
        return Ok(name.trim().to_ascii_lowercase());
    }
    let providers = registry.list_provider_names();
    if providers.is_empty() {
        anyhow::bail!("no providers are registered");
    }
    let labels: Vec<String> = providers
        .iter()
        .map(|id| format!("{} ({id})", ui::provider_name(id)))
        .collect();
    let remembered = config::route().provider;
    let default = providers
        .iter()
        .position(|id| Some(id.as_str()) == remembered.as_deref())
        .unwrap_or(0);
    let idx = cc_proxy::prompt::choose(question, &labels, default)?;
    Ok(providers[idx].clone())
}

fn print_all_auth_status(registry: &Registry) -> Result<()> {
    let styled = ui::styled(&std::io::stdout());
    let mut any_signed_in = false;
    for id in registry.list_provider_names() {
        let Some(provider) = registry.provider(&id) else {
            continue;
        };
        let state = provider.cli().auth_state();
        if state != cc_proxy::provider::AuthState::Missing {
            any_signed_in = true;
        }
        let label = ui::strong(&id, ui::provider_color(&id), styled);
        let summary = match provider.cli().accounts() {
            Some(support) => cc_proxy::auth_cli::AccountCli::open(&id, support)?.summary(),
            None => None,
        }
        .unwrap_or_else(|| state.describe());
        println!("{label}: {summary}");
    }
    if !any_signed_in {
        std::process::exit(1);
    }
    Ok(())
}

fn run_auth_cli(command: Option<cc_proxy::provider::AuthCommand>) -> Result<()> {
    use cc_proxy::provider::AuthCommand;
    let registry = Registry::new();
    let command = command.unwrap_or(AuthCommand::Status { provider: None });
    match command {
        AuthCommand::Status { provider: None } => print_all_auth_status(&registry),
        AuthCommand::Status {
            provider: Some(name),
        } => {
            let name = name.trim().to_ascii_lowercase();
            let provider = registry
                .provider(&name)
                .ok_or_else(|| anyhow::anyhow!("unknown provider: {name}"))?;
            if let Err(err) = provider.cli().status() {
                println!("{err}");
                if err.to_string() == "Not authenticated" {
                    println!("Run `cc-proxy login {name}` to sign in.");
                    std::process::exit(1);
                }
                std::process::exit(2);
            }
            Ok(())
        }
        AuthCommand::Login(cc_proxy::provider::LoginArgs {
            provider,
            device,
            replace,
            account,
        }) => {
            let name = pick_auth_provider(
                &registry,
                provider.as_deref(),
                "Which provider do you want to sign in to?",
            )?;
            let provider = registry
                .provider(&name)
                .ok_or_else(|| anyhow::anyhow!("unknown provider: {name}"))?;
            let result = if let Some(support) = provider.cli().accounts() {
                cc_proxy::auth_cli::AccountCli::open(&name, support)?.login(
                    &cc_proxy::auth_cli::LoginOptions {
                        device,
                        replace,
                        account,
                    },
                )
            } else if account.is_some() {
                anyhow::bail!(
                    "{} keeps one login. Leave out `--account` to sign in again.",
                    ui::provider_name(&name)
                );
            } else if device {
                provider.cli().device()
            } else {
                provider.cli().login()
            };
            if let Err(err) = result {
                eprintln!("{err}");
                std::process::exit(2);
            }
            Ok(())
        }
        AuthCommand::Add {
            provider,
            name: account,
            device,
        } => {
            let name = pick_auth_provider(
                &registry,
                provider.as_deref(),
                "Which provider do you want to add an account to?",
            )?;
            let provider = registry
                .provider(&name)
                .ok_or_else(|| anyhow::anyhow!("unknown provider: {name}"))?;
            let support = provider.cli().accounts().ok_or_else(|| {
                anyhow::anyhow!(
                    "{} keeps one login. Run `cc-proxy auth login {name}` to sign in again.",
                    ui::provider_name(&name)
                )
            })?;
            cc_proxy::auth_cli::AccountCli::open(&name, support)?.add(account.as_deref(), device)
        }
        AuthCommand::Use { provider, account } => {
            let name = pick_auth_provider(
                &registry,
                provider.as_deref(),
                "Which provider do you want to switch accounts for?",
            )?;
            let provider = registry
                .provider(&name)
                .ok_or_else(|| anyhow::anyhow!("unknown provider: {name}"))?;
            let support = provider.cli().accounts().ok_or_else(|| {
                anyhow::anyhow!(
                    "{} keeps one login. There is no other saved account to switch to.",
                    ui::provider_name(&name)
                )
            })?;
            cc_proxy::auth_cli::AccountCli::open(&name, support)?.use_account(account.as_deref())
        }
        AuthCommand::Device { provider } => {
            let name = pick_auth_provider(
                &registry,
                provider.as_deref(),
                "Which provider do you want to sign in to?",
            )?;
            let provider = registry
                .provider(&name)
                .ok_or_else(|| anyhow::anyhow!("unknown provider: {name}"))?;
            if let Err(err) = provider.cli().device() {
                eprintln!("{err}");
                std::process::exit(2);
            }
            Ok(())
        }
        AuthCommand::Logout(cc_proxy::provider::LogoutArgs {
            provider,
            account,
            all,
        }) => {
            let name = pick_auth_provider(
                &registry,
                provider.as_deref(),
                "Which provider do you want to sign out of?",
            )?;
            let provider = registry
                .provider(&name)
                .ok_or_else(|| anyhow::anyhow!("unknown provider: {name}"))?;
            if let Some(support) = provider.cli().accounts() {
                cc_proxy::auth_cli::AccountCli::open(&name, support)?
                    .logout(account.as_deref(), all)?;
            } else if account.is_some() {
                anyhow::bail!(
                    "{} keeps one login. Leave out the account name to sign out.",
                    ui::provider_name(&name)
                );
            } else {
                provider.cli().logout()?;
            }
            Ok(())
        }
    }
}

fn print_models(registry: &Registry, full: bool, filter_provider: Option<&str>) {
    let grouped = registry.grouped_models();
    let styled = ui::styled(&std::io::stdout());
    let builtins = [
        "codex",
        "kimi",
        "grok",
        "opencode",
        "opencode-zen",
        "copilot",
        "cursor",
        "glm",
        "claude",
        "anthropic",
        "gemini",
        "antigravity",
    ];
    for provider in builtins.into_iter().map(str::to_string).chain(
        grouped
            .keys()
            .filter(|k| !builtins.contains(&k.as_str()))
            .cloned(),
    ) {
        if let Some(wanted) = filter_provider
            && !provider.eq_ignore_ascii_case(wanted.trim())
        {
            continue;
        }
        let Some(models) = grouped.get(&provider) else {
            continue;
        };
        let name = ui::strong(&provider, ui::provider_color(&provider), styled);
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
            "; {} cursor model aliases, e.g. cursor:gpt-5.5. Run `cc-proxy models list --full` for all of them",
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
    print_models(registry, false, None);
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
            vec!["cc-proxy", "start", "codex"],
            vec!["cc-proxy", "start", "codex", "--monitor"],
            vec!["cc-proxy", "start", "--no-monitor"],
            vec!["cc-proxy", "serve", "--no-monitor"],
            vec!["cc-proxy", "service"],
            vec!["cc-proxy", "service", "status"],
            vec!["cc-proxy", "service", "stop"],
            vec!["cc-proxy", "service", "reload"],
            vec!["cc-proxy", "service", "restart", "--port", "18766"],
            vec!["cc-proxy", "service", "start", "codex"],
            vec!["cc-proxy", "service", "start", "--monitor"],
            vec!["cc-proxy", "service", "serve", "--no-monitor"],
            vec!["cc-proxy", "auth"],
            vec!["cc-proxy", "auth", "status"],
            vec!["cc-proxy", "auth", "status", "opencode"],
            vec!["cc-proxy", "auth", "login"],
            vec!["cc-proxy", "auth", "login", "opencode"],
            vec!["cc-proxy", "auth", "login", "codex", "--device"],
            vec!["cc-proxy", "auth", "device", "codex"],
            vec!["cc-proxy", "login", "codex"],
            vec!["cc-proxy", "login", "codex", "--device"],
            vec![
                "cc-proxy",
                "login",
                "codex",
                "--replace",
                "--account",
                "work",
            ],
            vec!["cc-proxy", "logout", "codex"],
            vec!["cc-proxy", "logout", "codex", "work"],
            vec!["cc-proxy", "logout", "codex", "--all"],
            vec!["cc-proxy", "auth", "add", "codex", "--name", "work"],
            vec!["cc-proxy", "auth", "use", "codex", "work"],
            vec!["cc-proxy", "auth", "logout", "kimi"],
            vec!["cc-proxy", "use"],
            vec!["cc-proxy", "use", "kimi"],
            vec!["cc-proxy", "use", "kimi", "codex", "antigravity"],
            vec!["cc-proxy", "start", "claude"],
            vec!["cc-proxy", "start", "direct"],
            vec!["cc-proxy", "use", "claude"],
            vec!["cc-proxy", "use", "claude", "codex"],
            vec!["cc-proxy", "use", "direct"],
        ] {
            assert!(Cli::try_parse_from(&args).is_ok(), "{args:?}");
        }
        assert!(Cli::try_parse_from(["cc-proxy", "start", "--monitor", "--no-monitor"]).is_err());
        assert!(
            Cli::try_parse_from(["cc-proxy", "service", "start", "--monitor", "--no-monitor"])
                .is_err()
        );
    }

    #[test]
    fn every_command_a_tip_names_exists() {
        for (_, text) in tips::TIPS {
            let commands = text.split('`').skip(1).step_by(2);
            for command in commands.filter(|span| span.starts_with("cc-proxy ")) {
                let args: Vec<_> = command.split_whitespace().collect();
                assert!(Cli::try_parse_from(&args).is_ok(), "{text}");
            }
        }
    }

    #[test]
    fn the_removed_commands_are_gone() {
        for command in [
            "setup", "shell", "on", "off", "codex", "kimi", "copilot", "cursor", "grok", "glm",
            "opencode",
        ] {
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
    fn explicit_open_connections_never_heal_local_routing() {
        let cli =
            Cli::try_parse_from(["cc-proxy", "open", "--url", "http://127.0.0.1:19999"]).unwrap();
        assert!(!heals_first(&cli.command.unwrap()));
    }

    #[test]
    fn open_is_the_command_and_monitor_and_show_work_as_its_names() {
        for name in ["open", "monitor", "show"] {
            let cli = Cli::try_parse_from(["cc-proxy", name]).unwrap();
            assert!(
                matches!(cli.command, Some(Commands::Open { url: None })),
                "{name}"
            );
        }
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
