---
title: Monitor
description: Choose providers and accounts, check usage and change settings. Inspect requests in the Activity tab.
---

Open the monitor:

```sh
cc-proxy open
```

`cc-proxy monitor` and `cc-proxy show` also work. They are other names for the same command.

If the proxy is stopped, the monitor still opens. It says "Proxy off" and shows
your saved accounts. Press `s` to start the proxy. It uses the provider you
chose last, or asks if you never chose one. The monitor connects by itself once
the proxy answers. Until then, changes to accounts and routing wait.

Closing this monitor leaves the proxy running. You can open it in more than one
terminal.

## Accounts

Accounts opens first. It shows where Claude Code goes, your fallback order and
saved logins grouped by provider, in that order: the primary provider, then each
fallback, then your other connected providers, then providers you have not set up.
Claude subscription follows the same order. `[primary]` marks the provider Claude Code uses.
A bullet (`●`) and a bold name mark the saved account choice, not an active
request, so you can find it at a glance. The one that serves new requests now,
the choice of the primary provider, is in teal; the choice of any other provider
is in white. The primary provider's name is bold teal too. When auto-rotate
moves the saved account, the cursor moves with it, unless you had moved it
somewhere else. With auto-rotate on, a new
request skips accounts still waiting. When every account has a confirmed wait,
`[first reset]` marks the earliest reset. The saved choice changes when a request
actually switches accounts, not while the proxy is idle.

Use Up and Down to choose a row, then Enter to see its actions. Choose an action
with the arrows and press Enter to run it. Escape goes back without making a
change. Wide terminals show the list and details side by side. Narrow terminals
put details below the list.

- Use provider sends Claude Code to that provider. If the provider isn't signed
  in, or a Claude name has no model yet, the monitor asks right there with the
  same prompts the commands use, then returns to your row. Otherwise it just
  saves and says so. Windows already open follow within a few seconds.
- Use account changes new requests for that provider. It does not change
  where Claude Code goes. Requests already running keep their account.
- Sign in saves a login. You can add another account or sign in to a saved
  account again. It asks for a browser, a code or a key on the monitor's own
  screen, and the monitor comes back when you are done. Adding a login does not
  change the provider or current account.
- Fallback adds a signed-in provider to the end of the fallback order, removes
  it or moves it earlier. When you choose another provider as primary, it moves
  to the front and the one you had before becomes the first fallback. The rest
  of the order stays.
- Remove saved account deletes one saved login from this computer. Choosing it
  opens one more line that names the account and says what is deleted. Enter on
  that line deletes it, and Escape keeps it. If it was the current account, the
  first one left takes over. To use it again, sign in again.
- Disable proxy routing is the last row. It sends Claude Code back to its
  original settings and login. The proxy keeps running, so you can switch back
  here. It does not pick a saved account.
- Claude subscription is a normal provider with its own saved accounts, usage and
  rotation, and requests go through the proxy.

Changes say what was saved and apply without a restart. A failed action stays
open with its cause and a retry. If the proxy connection drops, changes pause
until it reconnects.

## Settings

Press `3` for settings. It is the same list as `cc-proxy config`, with
auto-rotate first. Use Up and Down to move, Left and Right to change a yes or
no, and Enter to type a value. Left and Right never change tab, so use Tab,
Shift-Tab or `1` to `3` to leave. Changes save at once and apply to new requests,
except the port and address, which need `cc-proxy restart`. Type to search the
list. Escape clears the search first, then goes back to Accounts.

Auto-rotate is off by default. A provider must confirm a limit and a reset time,
or say the credit is spent, before the proxy switches. An account out of credit
shows `out of credit` and when cc-proxy tries it again (a recheck every 30
minutes, not a reset). Choose Use account for new requests after you add credit
to try it at once. If every saved account is spent, it tries the fallback
providers in order. Rotation applies to Messages requests before output
starts, not every API.

### Usage and wait timers

Select a saved account to see its usage. Claude, Codex and Kimi show how much is left in
each window and when it resets. Confirmed limits show how long the account must
wait, including model-specific limits. The wait shows beside the quota windows
and never replaces them. Antigravity lists its Gemini and Claude windows
separately, so one spent family does not mark the whole account as waiting.
Providers that do not report usage still show confirmed wait timers. Remaining percentages are bright green from 80%,
green above 30%, yellow above 10% and red at 10% or less. These colors also apply
to the bars and numbers in `cc-proxy usage`.

An Antigravity account shows its plan beside its address, such as `(free plan)`.
A free plan has a weekly allowance and no 5-hour window. That allowance is not
a list of the models the plan can use: Google can refuse a model with "Resource
has been exhausted" while the account still shows 100%. Details says so for a
free account. cc-proxy logs the reasons Google gives for each Antigravity 429
(`upstream_429` in `proxy.log`: the reasons, domains, model and quota ids, never
the message), so a refusal can be told apart from a spent quota.

Usage loads in the background and refreshes while Accounts is open. Press `r` to
refresh now. A failed lookup says usage is unavailable, not zero. If a refresh
fails after a good one, the row keeps the last numbers and ends with a
yellow `*`. Details starts with `* Refresh failed`, says the numbers may be out
of date and gives the reason in one line. The last good numbers are saved, so
they are still there if you close the monitor and open it again. Page Up and Page
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

| Key                    | Action                                                       |
| ---------------------- | ------------------------------------------------------------ |
| `1`, `p`               | Open Accounts                                                |
| `2`                    | Open Activity                                                |
| `3`                    | Open Settings                                                |
| `Tab`, `Shift-Tab`     | Change tab                                                   |
| `→`                    | Open actions or details, or change a choice in Settings      |
| `←`                    | Go back one step, or change a choice in Settings             |
| `↑`, `↓`, `j`, `k`     | Move through rows or actions                                 |
| `Home`, `End`          | First or last Accounts row                                   |
| `Enter`                | Open actions, run the chosen action or open activity details |
| `Esc`                  | Go back, then close an attached monitor                      |
| `Page Up`, `Page Down` | Scroll account details                                       |
| `r`                    | Refresh accounts and usage                                   |
| `?`                    | Show shortcuts                                               |
| `b`                    | Show connection and setup                                    |
| `q`, `Ctrl-C`          | Close an attached monitor without stopping the proxy         |

## A different port or computer

```sh
cc-proxy open --url http://127.0.0.1:19999
```

An explicit `--url` connection is read-only and opens Activity. It does not need
provider credentials, load this computer's accounts or change its settings.
Manage accounts by running `cc-proxy open` on the computer running the proxy.

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

Then run `cc-proxy open`. Service output lives in
`~/.local/state/cc-proxy/service.log` on macOS and Linux. Structured request logs
are in `proxy.log` in the same folder.
