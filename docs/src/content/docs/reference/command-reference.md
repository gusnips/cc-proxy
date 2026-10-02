---
title: Command reference
description: Canonical cc-proxy command syntax for starting Claude Code, serving, monitoring, listing models, version output, provider authentication, and OpenCode Go usage.
---

Running `cc-proxy` without a subcommand is equivalent to `cc-proxy serve`.

In a terminal, commands print in color next to a small face that shows how
the proxy is: awake when it runs, asleep when it's stopped, unsure when
something needs a look. While a command waits (start, stop, restart), the
face looks around until it's done. Piped or redirected output, `NO_COLOR=1`
and `TERM=dumb` get the same words as plain text, with no escape codes.

## Global version commands

```sh
cc-proxy --version
cc-proxy -v
cc-proxy version
```

Each prints `cc-proxy <version>`.

## `claude`

```sh
cc-proxy claude [claude arguments...]
```

Starts Claude Code on the proxy. If nothing answers on the configured port,
it starts the background service first. Then it runs
`claude --settings '<json>'` followed by every argument you gave, unchanged.
`cc-proxy claude --help` shows Claude Code's own help.

The JSON holds the variables in
[Configure Claude Code](/using/configure-claude-code/), plus `claude.model` and
`claude.fastModel` from `config.json` when they are set. Settings passed this
way rank above `~/.claude/settings.json` and the project's settings, for that
session only.

On Unix, cc-proxy replaces itself with `claude`, so the exit code and signals
are Claude Code's own. On Windows it waits for `claude` and exits with its
code. If there's no `claude` on `PATH`, it exits with code 127.

## `shell install`, `shell uninstall`, `on`, `off`

```sh
cc-proxy shell install
cc-proxy shell uninstall
cc-proxy on
cc-proxy off
```

`shell install` makes plain `claude` run through `cc-proxy claude` while
cc-proxy is on. It picks the shell from `$SHELL`:

| Shell | What it writes |
| --- | --- |
| zsh | One line in `$ZDOTDIR/.zshrc`, or `~/.zshrc` |
| bash on Linux | One line in `~/.bashrc` |
| bash on macOS | One line in `~/.bash_profile`. With no `.bash_profile`, it goes in the `~/.bash_login` or `~/.profile` you have, because bash reads only the first of the three that exists |
| fish | `$XDG_CONFIG_HOME/fish/functions/claude.fish`, or `~/.config/fish/functions/claude.fish` |

For zsh and bash, the line loads a `claude` function from `shell/claude.sh`
in the config directory. Running `shell install` again changes nothing. Any
other shell exits with code 1 and says to run `cc-proxy claude` instead.
`shell install` also turns cc-proxy on.

`shell uninstall` removes the line from every one of those files, deletes
the function files it wrote, and turns cc-proxy off, so terminals that are
still open go back to plain Claude Code.

`on` and `off` set `claude.enabled` in `config.json`. The `claude` function
asks cc-proxy on every call, so the change applies in every terminal at once.
`cc-proxy claude`, typed out in full, always uses the proxy.

## `serve`

```sh
cc-proxy serve [--port <PORT>] [--no-monitor | --monitor]
```

Starts the proxy as a background service and exits once it answers health
checks, printing its pid and listening address. A second `serve` reports the
running pid instead of starting another copy. The service is tracked in a
pidfile under the state directory; `status`, `stop`, `restart`, and `reload`
all resolve through it.

| Option | Behavior |
| --- | --- |
| `--port <PORT>` | Overrides `PORT`, `config.json`, and the default for this invocation. |
| `--no-monitor` | Runs in the foreground with plain output instead of starting a service. |
| `--monitor` | Runs in the foreground with the monitor dashboard attached. |

The bind address comes from `CCP_BIND_ADDRESS` or `bindAddress`.

Foreground mode keeps running until SIGTERM (Unix) or Ctrl-C requests graceful
shutdown, and continues collecting monitor history for separate dashboards.

## `status`, `stop`, `restart`, `reload`

```sh
cc-proxy status
cc-proxy stop
cc-proxy restart [--port <PORT>]
cc-proxy reload
```

`status` reports how long the service has run, its pid and listening
address, and whether plain `claude` goes through it, or exits 1 when the
service is down. `stop` shuts a running service down gracefully (exit 0 when
there is nothing to stop). `restart` stops and starts again, keeping the
previous port unless `--port` overrides it. `reload` validates `config.json`
and asks a running service to re-read it; file-backed settings apply on the
next request, while bind address, port, alias provider, and environment need
`restart`. `reload` exits 1 when no service is running (after validating the
config) and validates only on platforms without SIGHUP.

When something answers on the configured port without a pidfile — a
foreground or dashboard-attached proxy started separately, for example —
`status` says so explicitly, and `stop`/`restart` refuse to touch a process
they did not start.

## `monitor`

```sh
cc-proxy monitor [--url <URL>]
```

Attach a read-only dashboard to a running proxy. The default URL is `http://127.0.0.1:<configured-port>`. No provider login is needed in the dashboard process. `q` and `Ctrl-C` detach without stopping the proxy; multiple dashboards are supported. After a connection failure, the dashboard retains its last snapshot and retries. The proxy accepts monitor reads only from loopback peers; use an SSH port forward for another machine.

## `demo`

```sh
cc-proxy demo
```

Opens the monitor with deterministic simulated traffic. It does not bind a network port or contact providers.

## `models`

```sh
cc-proxy models [--full]
```

Prints supported IDs grouped by provider. The default output compacts Cursor's runtime catalog. `--full` prints every Cursor alias.

## Provider authentication

The command shape is:

```text
cc-proxy <provider> auth <action>
```

| Provider | `login` | `device` | `status` | `logout` |
| --- | --- | --- | --- | --- |
| `codex` | Browser PKCE | Device code | Account, expiry, storage | Delete proxy credential |
| `kimi` | Device code | Unsupported | User, expiry, scope, storage | Delete proxy credential |
| `grok` | Browser PKCE | Device code | Expiry and storage | Delete proxy credential |
| `cursor` | Browser polling flow | Unsupported | Source, claims, expiry | Delete proxy credential |
| `glm` | API key entry | Unsupported | Env and stored key presence | Delete proxy credential |
| `opencode` | API key entry (hidden) | Unsupported | Key source and base URL | Delete stored key |

Examples:

```sh
cc-proxy codex auth login
cc-proxy grok auth device
cc-proxy kimi auth status
cc-proxy cursor auth logout
```

A missing credential makes `auth status` exit with status 1. Other provider command failures exit with status 2. Successful commands exit with status 0.

Logout removes the local proxy-owned credential. It does not call the provider to revoke a refresh token.

## `usage`

```sh
cc-proxy usage [codex|kimi|opencode] [--json]
```

Shows how much of each plan you have used and when each limit resets, asked
straight from the provider with the sign-in the proxy already holds:

```text
Codex (plus)
  5-hour window: 45% used, resets in 2h 10m (17:00)
  Weekly: 7% used, resets in 4d 9h (Mon 00:00)
```

In a terminal, each window also gets a bar that turns yellow at 70% used and
red at 90%.

| Provider | Asks | Shows |
| --- | --- | --- |
| `codex` | `GET https://chatgpt.com/backend-api/wham/usage` | 5-hour and weekly windows, plan |
| `kimi` | `GET https://api.kimi.ai/coding/v1/usages` | 5-hour and weekly quotas, plan |
| `opencode` | `GET https://opencode.ai/zen/go/v1/usage` | Rolling 5-hour, weekly and monthly windows |

Each address follows the provider's base URL (`codex.baseUrl`, `kimi.baseUrl`,
`opencode.baseUrl`). No provider documents these endpoints, so a Codex or Kimi
window that cc-proxy can't read shows as `not reported`.

With no provider named, every provider is shown. Each one you haven't signed
in to gets one line with its login command. Name a provider to see only that
one. If it isn't signed in, that line goes to stderr and the command exits
with status 1. A failed lookup exits with status 2.

`--json` prints one object with a key per provider. Codex and Kimi share one
shape:

```json
{
  "codex": {
    "plan": "plus",
    "five_hour": { "used_percent": 45.0, "resets_at_ms": 1786294800000 },
    "weekly": { "used_percent": 7.0, "resets_at_ms": 1786752000000 }
  }
}
```

`resets_at_ms` is a Unix time in milliseconds. A field the provider didn't
send is left out. `opencode` holds OpenCode Go's own reply, unchanged.

## OpenCode Go API key

Store the key once without echoing it:

```sh
cc-proxy opencode auth login
```

The key lands in `opencode.apiKey` in config.json. `opencode auth status`
reports which source provides the key; `opencode auth logout` removes the
stored key. When nothing of ours is set, the proxy falls back to
`OPENCODE_API_KEY` in Claude Code's `~/.claude/settings.json`.

## Configuration values

```sh
cc-proxy config get <key>
cc-proxy config set <key> <value>
cc-proxy config list
cc-proxy config edit
```

Reads and writes config.json through dotted keys (`port`,
`opencode.apiKey`, `codex.fullLane`). `get` prints one value, `set`
validates and writes one, `list` shows every known key, and `edit` opens
the file in `$VISUAL` or `$EDITOR`. Reads show the file value; when an
environment variable overrides a key at runtime, the commands say so.
Secret keys only ever report set or unset, never their value. Restart the
service after changing bind settings.

## Updating

```sh
cc-proxy update [--check] [--version <tag>]
```

Replaces the installed binary with a newer GitHub release: version check,
HTTPS download, SHA-256 verification, atomic swap, and a service restart
when one is running. `--check` only reports. Re-running the install
script (`scripts/install.sh`) updates the same way.

## Development commands

From a source checkout:

```sh
cargo run -- serve
cargo test --all
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
just check
just docs
```

`just docs` installs the locked documentation dependencies and starts the Astro development server on an available local port.
