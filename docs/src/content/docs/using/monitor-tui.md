---
title: Monitor
description: Choose providers and accounts, check usage and manage rotation. Inspect requests in the Activity tab.
---

Open the monitor for a running proxy:

```sh
cc-proxy monitor
```

If the proxy is stopped, run `cc-proxy start` first. Closing this monitor leaves
the proxy running. You can open it in more than one terminal.

## Accounts

Accounts opens first. It shows where Claude Code goes, your fallback order and
saved logins grouped by provider. `[primary]` marks the provider Claude Code uses.
`[current]` marks the saved account chosen for that provider's new requests.

Use Up and Down to choose a row, then Enter to see its actions. Choose an action
with the arrows and press Enter to run it. Escape goes back without making a
change. Wide terminals show the list and details side by side. Narrow terminals
put details below the list.

- Use provider sends Claude Code to that provider. If needed, the monitor
  opens the existing sign-in and model-choice prompts, then returns to your row.
  Windows already open follow within a few seconds.
- Use account changes new requests for that provider. It does not change
  where Claude Code goes. Requests already running keep their account.
- Sign in saves a login. You can add another account or sign in to a saved
  account again. Adding a login does not change the provider or current account.
- Fallback adds a signed-in provider to the end of the fallback order, removes
  it or moves it earlier. When you choose a fallback provider as primary, it is
  removed from its own fallback list.
- Auto-rotate turns automatic switching on or off. It is off by default.
  A provider must confirm a limit and a reset time before the proxy switches.
  If every saved account is spent, it tries the fallback providers in order.
  Rotation applies to Messages requests before output starts, not every API.
- Claude subscription sends Claude Code straight to your own Claude plan.
  The proxy keeps running, so you can switch back here.

Changes say what was saved and apply without a restart. A failed action stays
open with its cause and a retry. If the proxy connection drops, changes pause
until it reconnects.

### Usage and wait timers

Select a saved account to see its usage. Codex and Kimi show how much is left in
each window and when it resets. Confirmed limits show how long the account must
wait, including model-specific limits. Providers that do not report usage still
show confirmed wait timers.

Usage loads in the background and refreshes while Accounts is open. Press `r` to
refresh now. A failed lookup says usage is unavailable, not zero. Page Up and Page
Down scroll long details.

If an environment variable supplies the login, the details name it and explain
why saved account choices do not apply. Unset it in the proxy service to use a
saved account.

## Activity

Press `2` for the detailed monitoring view:

- Sessions grouped by Claude Code session and project
- Active requests with their provider, saved account, model and progress
- Recent requests with HTTP status, duration, errors and token counts
- Errors and paths to traffic captures, when enabled

The header shows uptime, active work, token history and live output speed. The
face looks unsure for 10 seconds after a failed request. Request rows keep the
name of the account they actually used, even after you switch accounts.

Up and Down cross between sessions and recent requests at the ends of their
lists. `j` and `k` stay within the current list. Enter opens details; Escape
returns. Columns adapt to the terminal width.

## Keys

| Key                          | Action                                                       |
| ---------------------------- | ------------------------------------------------------------ |
| `1`, `p`                     | Open Accounts                                                |
| `2`                          | Open Activity                                                |
| `Tab`, `Shift-Tab`, `←`, `→` | Change tab                                                   |
| `↑`, `↓`, `j`, `k`           | Move through rows or actions                                 |
| `Home`, `End`                | First or last Accounts row                                   |
| `Enter`                      | Open actions, run the chosen action or open activity details |
| `Esc`                        | Go back, then close an attached monitor                      |
| `Page Up`, `Page Down`       | Scroll account details                                       |
| `r`                          | Refresh accounts and usage                                   |
| `?`                          | Show shortcuts                                               |
| `b`                          | Show connection and setup                                    |
| `q`, `Ctrl-C`                | Close an attached monitor without stopping the proxy         |

## A different port or computer

```sh
cc-proxy monitor --url http://127.0.0.1:19999
```

An explicit `--url` connection is read-only and opens Activity. It does not need
provider credentials, load this computer's accounts or change its settings.
Manage accounts by running `cc-proxy monitor` on the computer running the proxy.

The monitor endpoint accepts only loopback connections. For another machine,
forward the port through SSH, then point `--url` at the local end of the tunnel.
If the connection drops, the monitor keeps its last snapshot, marks it as stale
and retries in the background.

## Foreground and plain output

`cc-proxy start --monitor` starts a foreground proxy with the same interface.
Unlike an attached monitor, quitting it stops that proxy and switches routing
off. `q` or Escape asks first. Ctrl-C starts shutdown; press it again to force
exit.

Use plain output under a service manager or through a pipe:

```sh
cc-proxy start --no-monitor
```

On Unix, SIGTERM or Ctrl-C starts graceful shutdown. `CCP_LOG_STDERR=1` mirrors
structured logs to stderr. History stays in the proxy's memory, survives closing
a monitor and resets when the proxy restarts.

## Try it without changing anything

```sh
cc-proxy demo
```

Demo mode has simulated accounts, usage, wait timers and traffic. You can try
account, provider and rotation actions without changing real logins or settings.
It does not open a port or contact providers.

## Run at login

A Homebrew installation can run at login:

```sh
brew services start cc-proxy
```

Then open `cc-proxy monitor`. Service output lives in
`~/.local/state/cc-proxy/service.log` on macOS and Linux. Structured request logs
are in `proxy.log` in the same folder.
