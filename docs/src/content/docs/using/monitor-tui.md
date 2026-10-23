---
title: Monitor TUI
description: Use the cc-proxy monitor to inspect sessions, active and recent requests, providers, errors, token usage, throughput, and setup.
---

`cc-proxy start` starts the proxy as a background service. The monitor is a
separate view over that service. Attaching or detaching never starts or
stops the proxy.

```sh
# Start the service (prints its pid).
cc-proxy start

# Attach from any terminal; repeat for additional dashboards.
cc-proxy monitor
```

`cc-proxy start --monitor` starts a foreground proxy with the dashboard
attached in one process instead. `cc-proxy start --no-monitor` runs a
foreground proxy with plain output, which suits service managers.

Use `cc-proxy monitor --url http://127.0.0.1:19999` for a different port. Without `--url`, the port follows the usual proxy configuration. The attached dashboard reads the running service's existing history; it does not start a proxy or need provider credentials.

In an attached dashboard, `q` and `Ctrl-C` close the monitor immediately and leave the service running. `Esc` closes the top panel or detail view first; on the main screen, it closes the monitor. Multiple dashboards can attach independently. If the service becomes unavailable, the dashboard marks its last snapshot as stale and reconnects automatically. Network polling runs outside the terminal event loop.

## What the monitor shows

- A header with the cc-proxy face, which shows the proxy's state at a
  glance: it talks while requests stream, looks unsure for 10 seconds after
  a failed request, and sleeps while the proxy shuts down. Beside it are the
  uptime, the session and active request counts, the output tokens of the
  last four minutes, and the live tokens per second.
- Sessions grouped by Claude Code session ID and project
- Active request lifecycle, selected provider, saved account and model
- Recent requests, HTTP status, elapsed time, and errors. A request that
  just finished glows for three seconds.
- Each provider in its own color
- Input and output token totals
- Output throughput based on matched upstream timing and cumulative usage samples
- Paths to traffic captures when capture is enabled
- Configuration overrides and a ready-to-copy Claude Code setup

## Keyboard controls

| Key                | Action                                                                                                                                  |
| ------------------ | --------------------------------------------------------------------------------------------------------------------------------------- |
| `Tab`, `←`, `→`    | Change focused pane                                                                                                                     |
| `j`, `k`, `↓`, `↑` | Move selection                                                                                                                          |
| `Enter`            | Open session or request details                                                                                                         |
| `Esc`              | Close the top panel or detail view. On the main screen, close an attached monitor; in `start --monitor`, ask before stopping the proxy. |
| `?`                | Toggle shortcut help                                                                                                                    |
| `b`                | Toggle the setup overlay                                                                                                                |
| `p`                | Toggle local providers, saved accounts, usage and reset timers                                                                          |
| `q`                | Detach an attached dashboard; in the built-in dashboard, confirm proxy shutdown                                                         |
| `Ctrl-C`           | Detach an attached dashboard; in the built-in dashboard, start shutdown (press again to force exit)                                     |

The request table changes columns as the terminal width changes. Saved account
names appear before models. Request and session details show the full name.
The name belongs to that request, not whichever account is current now.

Press `p` to inspect local saved accounts. `[current]` marks the account chosen
for new requests. Codex and Kimi rows show provider-reported usage and reset
times. Confirmed cooldowns show how long each account must wait, including
model-specific limits. Grok and Antigravity usage percentages are not available;
their confirmed cooldowns still appear.

Up and Down scroll the panel. `r` refreshes it. Usage loads in the background
and refreshes while the panel is open. A failed lookup says it is unavailable
and names the CLI command to retry. This panel reads this computer's accounts,
not a remote proxy's credentials when you attach through SSH.

## Plain logs

Use plain output when the process runs under a service manager, in CI, or through a pipe:

```sh
cc-proxy start --no-monitor
```

`CCP_LOG_STDERR=1` mirrors JSONL log events to stderr in plain mode.

Plain mode retains monitor accounting even with no dashboard attached. On Unix, SIGTERM starts graceful proxy shutdown; Ctrl-C does the same. The service manager owns the process lifetime.

## Demo mode

Explore the full interface without binding a port or using provider credentials:

```sh
cc-proxy demo
```

The deterministic simulation covers active, successful, and failed requests across providers, projects, throughput states, and responsive layouts.

## Background service

A Homebrew installation can run at login:

```sh
brew services start cc-proxy
```

Service output lives in `~/.local/state/cc-proxy/service.log` on macOS and Linux. The structured `proxy.log` shares the state directory. Provider login remains an interactive one-time command.

Run `cc-proxy monitor` to inspect that service. The monitor endpoint only accepts loopback connections, even if the inference listener binds a LAN address. For a service on another machine, forward its port with SSH and point `monitor --url` at the local end of the tunnel.

History remains in the proxy's memory and resets when the proxy restarts. The dashboard polls snapshots every 250 ms and displays server-computed durations and throughput. The attached setup overlay describes its connection; provider setup remains with the service and its built-in dashboard.
