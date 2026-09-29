//! Background-service lifecycle for the proxy.
//!
//! `serve` starts a detached daemon and records it in a pidfile under the
//! state directory. `status`, `stop`, `restart`, and `reload` all resolve
//! through that pidfile, so every command knows whether the service is up.
//! A stale pidfile (dead or foreign pid) is treated as not running.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::{config, paths};

/// Env marker: when present, `serve` runs the daemon child instead of
/// spawning one.
pub const DAEMON_CHILD_ENV: &str = "CC_PROXY_DAEMON_CHILD";

/// How long `serve` waits for a fresh daemon to answer health checks.
const START_TIMEOUT: Duration = Duration::from_secs(15);
/// How long `stop` waits for graceful exit before forcing it.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DaemonInfo {
    pub pid: u32,
    pub port: u16,
    pub bind: String,
    pub started_at: String,
}

impl DaemonInfo {
    pub fn listen_url(&self) -> String {
        match self.bind.parse::<std::net::IpAddr>() {
            Ok(ip) if ip.is_unspecified() => format!("http://127.0.0.1:{}", self.port),
            _ => format!("http://{}:{}", self.bind, self.port),
        }
    }
}

#[derive(Debug)]
pub enum DaemonStatus {
    Running(DaemonInfo),
    /// No pidfile, or the pidfile is stale. Some other process may still be
    /// answering on the port; see [`probe_port_health`].
    Unmanaged { port: u16 },
    Stopped,
}

impl DaemonStatus {
    pub fn is_running(&self) -> bool {
        matches!(self, DaemonStatus::Running(_))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum StopOutcome {
    NotRunning,
    Stopped { pid: u32 },
}

#[derive(Debug, PartialEq, Eq)]
pub enum ServeOutcome {
    AlreadyRunning(DaemonInfo),
    Started(DaemonInfo),
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReloadOutcome {
    NotRunning,
    Reloaded,
    /// Non-unix platforms have no SIGHUP; the config was validated only.
    ValidatedOnly,
}

pub fn pidfile_path() -> PathBuf {
    paths::state_dir().join("proxy.pid")
}

fn read_info_file(path: &Path) -> Option<DaemonInfo> {
    let raw = fs::read_to_string(path).ok()?;
    let info: DaemonInfo = serde_json::from_str(&raw).ok()?;
    if info.pid == 0 || info.port == 0 || info.bind.is_empty() {
        return None;
    }
    Some(info)
}

fn write_info_file(path: &Path, info: &DaemonInfo) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, serde_json::to_string_pretty(info).unwrap_or_default())
}

fn remove_info_file(path: &Path) {
    let _ = fs::remove_file(path);
}

/// True when a process with this pid exists (and we may signal it).
pub fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        // SAFETY: kill with sig 0 performs no action; it only reports whether
        // the pid exists and is signalable. EPERM still means "alive".
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        windows_process_alive(pid)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

#[cfg(windows)]
fn windows_process_alive(pid: u32) -> bool {
    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output();
    match output {
        Ok(output) => {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
        }
        Err(_) => false,
    }
}

/// Whether the pid belongs to this proxy binary: `Some(false)` means the
/// pidfile is stale (pid reused by a foreign process). `None` means the
/// platform cannot tell; callers treat that as "assume ours".
fn process_is_proxy(pid: u32) -> Option<bool> {
    #[cfg(target_os = "linux")]
    {
        let cmdline = fs::read_to_string(format!("/proc/{pid}/cmdline")).ok()?;
        let exe = cmdline.split('\0').next().unwrap_or_default();
        let name = exe.rsplit('/').next().unwrap_or_default();
        Some(name == "cc-proxy")
    }
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .output()
            .ok()?;
        if !output.status.success() {
            return Some(false);
        }
        let comm = String::from_utf8_lossy(&output.stdout);
        let name = comm.trim().rsplit('/').next().unwrap_or_default();
        Some(name == "cc-proxy")
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

fn terminate_gracefully(pid: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        // SAFETY: signalling a pid we resolved from our own pidfile.
        if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let status = Command::new("taskkill")
            .args(["/PID", &pid.to_string()])
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other("taskkill refused to stop the process"))
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        Err(io::Error::other(
            "stopping a background proxy is not supported on this platform",
        ))
    }
}

fn force_kill(pid: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        // SAFETY: signalling a pid we resolved from our own pidfile.
        if unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let status = Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other("taskkill /F refused to stop the process"))
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        Err(io::Error::other(
            "stopping a background proxy is not supported on this platform",
        ))
    }
}

#[cfg(unix)]
fn send_reload(pid: u32) -> io::Result<()> {
    // SAFETY: signalling a pid we resolved from our own pidfile.
    if unsafe { libc::kill(pid as libc::pid_t, libc::SIGHUP) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Resolve the service state, removing a stale pidfile (missing, corrupt,
/// dead pid, or a pid that now belongs to a foreign process).
pub fn status_for(pid_path: &Path) -> DaemonStatus {
    match read_info_file(pid_path) {
        Some(info) if process_alive(info.pid) => match process_is_proxy(info.pid) {
            Some(false) => {
                remove_info_file(pid_path);
                DaemonStatus::Stopped
            }
            _ => DaemonStatus::Running(info),
        },
        _ => {
            remove_info_file(pid_path);
            DaemonStatus::Stopped
        }
    }
}

pub fn status() -> DaemonStatus {
    status_for(&pidfile_path())
}

/// Pidfile state plus a port probe: reports [`DaemonStatus::Unmanaged`] when
/// something answers on the configured port without a pidfile (for example a
/// foreground or TUI-attached proxy started separately).
pub fn describe() -> DaemonStatus {
    if let running @ DaemonStatus::Running(_) = status() {
        return running;
    }
    let port = config::port();
    if probe_port_health(port) {
        DaemonStatus::Unmanaged { port }
    } else {
        DaemonStatus::Stopped
    }
}

/// True when something answers `/healthz` with HTTP 200 on this port,
/// whatever process it belongs to.
pub fn probe_port_health(port: u16) -> bool {
    use std::io::{Read, Write};
    let socket: std::net::SocketAddr = match format!("127.0.0.1:{port}").parse() {
        Ok(address) => address,
        Err(_) => return false,
    };
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&socket, Duration::from_millis(500))
    else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    if stream
        .write_all(b"GET /healthz HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut head = [0u8; 15];
    match stream.read_exact(&mut head) {
        Ok(()) => {
            head.starts_with(b"HTTP/1.0 200") || head.starts_with(b"HTTP/1.1 200")
        }
        Err(_) => false,
    }
}

/// Wait until the daemon answers health checks, watching for early exit of
/// the child we spawned.
fn wait_for_child(
    child: &mut std::process::Child,
    bind: &str,
    port: u16,
    log_path: &Path,
) -> anyhow::Result<DaemonInfo> {
    let host = match bind.parse::<std::net::IpAddr>() {
        Ok(ip) if ip.is_unspecified() => "127.0.0.1".to_string(),
        _ => bind.to_string(),
    };
    let address = format!("{host}:{port}");
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let DaemonStatus::Running(info) = status()
            && probe_port_health(info.port)
        {
            return Ok(info);
        }
        if let Some(exit) = child.try_wait().map_err(anyhow::Error::from)? {
            anyhow::bail!(
                "proxy exited during startup (status {exit}); see {}",
                log_path.display()
            );
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            anyhow::bail!(
                "timed out waiting for the proxy at {address}; see {}",
                log_path.display()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Start the proxy as a detached background service.
pub fn serve_background(port: Option<u16>) -> anyhow::Result<ServeOutcome> {
    if let DaemonStatus::Running(info) = status() {
        return Ok(ServeOutcome::AlreadyRunning(info));
    }
    let bind_address = config::bind_address();
    let port = port.unwrap_or_else(config::port);
    if probe_port_health(port) {
        anyhow::bail!(
            "port {port} already answers health checks but has no pidfile — \
             another proxy instance (or something else) owns it. Stop it first, \
             or pick another port with `cc-proxy serve --port <PORT>`."
        );
    }
    let exe = std::env::current_exe()?;
    let log_path = crate::logging::log_file();
    if let Some(dir) = log_path.parent() {
        fs::create_dir_all(dir)?;
    }
    // Server logs already go to this file as JSON lines; warn/error mirrors
    // are single-line JSON too, so appending both streams keeps it parseable.
    let log_out = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let log_err = log_out.try_clone()?;
    let mut child = Command::new(exe)
        .arg("serve")
        .arg("--port")
        .arg(port.to_string())
        .env(DAEMON_CHILD_ENV, "1")
        .stdin(std::process::Stdio::null())
        .stdout(log_out)
        .stderr(log_err)
        .spawn()?;
    let info = wait_for_child(&mut child, &bind_address, port, &log_path)?;
    // The child is detached from here on; dropping the handle does not stop it.
    Ok(ServeOutcome::Started(info))
}

/// Stop the background service, waiting briefly for graceful exit.
pub fn stop_service() -> anyhow::Result<StopOutcome> {
    let info = match describe() {
        DaemonStatus::Running(info) => info,
        DaemonStatus::Unmanaged { port } => anyhow::bail!(
            "a proxy answers on port {port} but has no pidfile, so it was not \
             started by `cc-proxy serve`. Stop that process directly."
        ),
        DaemonStatus::Stopped => return Ok(StopOutcome::NotRunning),
    };
    terminate_gracefully(info.pid)
        .map_err(|error| anyhow::anyhow!("failed to stop pid {}: {error}", info.pid))?;
    if wait_for_exit(info.pid, STOP_TIMEOUT) {
        remove_info_file(&pidfile_path());
        return Ok(StopOutcome::Stopped { pid: info.pid });
    }
    force_kill(info.pid)
        .map_err(|error| anyhow::anyhow!("failed to force-stop pid {}: {error}", info.pid))?;
    if wait_for_exit(info.pid, Duration::from_secs(2)) {
        remove_info_file(&pidfile_path());
        return Ok(StopOutcome::Stopped { pid: info.pid });
    }
    anyhow::bail!("pid {} did not exit after being force-stopped", info.pid)
}

fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while process_alive(pid) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

/// Restart the background service, keeping the previous port unless `--port`
/// overrides it. Refuses when an unmanaged proxy owns the effective port.
pub fn restart_service(port: Option<u16>) -> anyhow::Result<DaemonInfo> {
    let previous_port = match status() {
        DaemonStatus::Running(info) => Some(info.port),
        // status() never reports Unmanaged; the probe below covers it.
        DaemonStatus::Stopped | DaemonStatus::Unmanaged { .. } => None,
    };
    let effective = port.or(previous_port).unwrap_or_else(config::port);
    if !status().is_running() && probe_port_health(effective) {
        anyhow::bail!(
            "a proxy answers on port {effective} but has no pidfile, so it was \
             not started by `cc-proxy serve`. Stop that process directly, or \
             restart onto another port with `cc-proxy restart --port <PORT>`."
        );
    }
    let _ = stop_service()?;
    match serve_background(Some(effective))? {
        ServeOutcome::Started(info) => Ok(info),
        ServeOutcome::AlreadyRunning(info) => Ok(info),
    }
}

/// Validate the config file, then ask a running daemon to reload it.
pub fn reload_service() -> anyhow::Result<ReloadOutcome> {
    validate_config_file()?;
    let info = match status() {
        DaemonStatus::Running(info) => info,
        DaemonStatus::Unmanaged { .. } | DaemonStatus::Stopped => {
            return Ok(ReloadOutcome::NotRunning);
        }
    };
    #[cfg(unix)]
    {
        send_reload(info.pid)
            .map_err(|error| anyhow::anyhow!("failed to reload pid {}: {error}", info.pid))?;
        Ok(ReloadOutcome::Reloaded)
    }
    #[cfg(not(unix))]
    {
        let _ = info;
        Ok(ReloadOutcome::ValidatedOnly)
    }
}

fn validate_config_file() -> anyhow::Result<()> {
    let path = config::config_path();
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => anyhow::bail!("cannot read {}: {error}", path.display()),
    };
    serde_json::from_str::<serde_json::Value>(&raw)
        .map(|_| ())
        .map_err(|error| anyhow::anyhow!("invalid config at {}: {error}", path.display()))
}

/// Entry point for the detached daemon child: detach from the terminal, bind,
/// record the pidfile, and serve until SIGINT/SIGTERM (SIGHUP reloads config).
pub fn serve_daemon_child(port: Option<u16>) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        // Detach from the controlling terminal so closing it does not deliver
        // terminal signals to the daemon. Best effort; failure is non-fatal.
        // SAFETY: setsid takes no arguments and only affects this process.
        unsafe {
            libc::setsid();
        }
    }
    let bind_address = config::bind_address();
    let port = port.unwrap_or_else(config::port);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run_daemon(bind_address, port))
}

async fn run_daemon(bind_address: String, port: u16) -> anyhow::Result<()> {
    let listener = crate::server::bind_proxy_listener(&bind_address, port).await?;
    let actual_port = listener.local_addr()?.port();
    write_info_file(
        &pidfile_path(),
        &DaemonInfo {
            pid: std::process::id(),
            port: actual_port,
            bind: bind_address.clone(),
            started_at: now_rfc3339(),
        },
    )?;
    let outcome = run_daemon_loop(listener, bind_address, actual_port).await;
    remove_info_file(&pidfile_path());
    outcome
}

async fn run_daemon_loop(
    listener: tokio::net::TcpListener,
    bind_address: String,
    port: u16,
) -> anyhow::Result<()> {
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server = crate::server::serve_listener(
        listener,
        Some(crate::monitor::MonitorHandle::default()),
        async move {
            let _ = shutdown_rx.await;
        },
    );
    tokio::pin!(server);
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut hangup = signal(SignalKind::hangup())?;
        loop {
            tokio::select! {
                result = &mut server => return result,
                _ = terminate.recv() => {
                    let _ = shutdown_tx.send(());
                    return server.await;
                }
                _ = interrupt.recv() => {
                    let _ = shutdown_tx.send(());
                    return server.await;
                }
                _ = hangup.recv() => {
                    reload_notice(&bind_address, port);
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (bind_address, port);
        tokio::signal::ctrl_c().await?;
        let _ = shutdown_tx.send(());
        server.await
    }
}

/// Re-read the config file on SIGHUP and log what changed. File-backed
/// settings already apply on the next request; bind address, port,
/// alias provider, and process environment need a restart.
#[cfg(unix)]
fn reload_notice(bind_address: &str, port: u16) {
    let fresh = config::load_config();
    let log = crate::logging::create_logger("server");
    let mut fields = serde_json::Map::new();
    fields.insert(
        "configFile".to_string(),
        serde_json::Value::String(config::config_path().display().to_string()),
    );
    fields.insert(
        "overrides".to_string(),
        serde_json::Value::Array(
            config::config_override_summary_lines(&fresh)
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        ),
    );
    if fresh.bind_address != bind_address || fresh.port != port {
        fields.insert(
            "restartRequired".to_string(),
            serde_json::Value::String(
                "bindAddress/port changed; run `cc-proxy restart` to apply".to_string(),
            ),
        );
    }
    fields.insert(
        "note".to_string(),
        serde_json::Value::String(
            "aliasProvider and environment changes apply on restart".to_string(),
        ),
    );
    log.info("config reloaded", Some(fields));
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs().to_string())
                .unwrap_or_default()
        })
}

pub fn is_daemon_child() -> bool {
    std::env::var_os(DAEMON_CHILD_ENV).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pidfile_round_trips() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("proxy.pid");
        assert!(!status_for(&path).is_running());
        let info = DaemonInfo {
            pid: 1234,
            port: 18765,
            bind: "127.0.0.1".to_string(),
            started_at: "2026-09-29T00:00:00Z".to_string(),
        };
        write_info_file(&path, &info).unwrap();
        // Pid 1234 is not ours to claim: liveness decides, and a dead or
        // foreign pid resolves to Stopped (the Running branch is covered by
        // the service lifecycle integration tests).
        assert_eq!(read_info_file(&path), Some(info));
    }

    #[test]
    fn corrupt_pidfile_is_stopped() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("proxy.pid");
        fs::write(&path, "not json").unwrap();
        assert!(!status_for(&path).is_running());
        assert!(!path.exists(), "stale pidfile must be removed");
    }

    #[test]
    fn dead_pid_is_stopped_and_cleaned() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("proxy.pid");
        // i32::MAX is never a live pid; the probe must report it dead.
        let info = DaemonInfo {
            pid: i32::MAX as u32,
            port: 18765,
            bind: "127.0.0.1".to_string(),
            started_at: "2026-09-29T00:00:00Z".to_string(),
        };
        assert!(!process_alive(info.pid));
        write_info_file(&path, &info).unwrap();
        assert!(!status_for(&path).is_running());
        assert!(!path.exists(), "stale pidfile must be removed");
    }

    #[test]
    fn current_process_is_alive_but_not_the_proxy() {
        let me = std::process::id();
        assert!(process_alive(me));
        // The test harness binary is not named cc-proxy.
        assert_eq!(process_is_proxy(me), Some(false));
    }

    #[test]
    fn listen_url_prefers_loopback_for_wildcards() {
        let wild = DaemonInfo {
            pid: 1,
            port: 18765,
            bind: "0.0.0.0".to_string(),
            started_at: String::new(),
        };
        assert_eq!(wild.listen_url(), "http://127.0.0.1:18765");
        let local = DaemonInfo {
            bind: "127.0.0.1".to_string(),
            ..wild
        };
        assert_eq!(local.listen_url(), "http://127.0.0.1:18765");
    }
}
