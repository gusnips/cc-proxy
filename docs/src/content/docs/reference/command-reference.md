---
title: Command reference
description: Canonical cc-proxy command syntax for starting Claude Code, serving, monitoring, listing models, version output, provider authentication, and OpenCode Go usage.
---

Running `cc-proxy` without a subcommand is equivalent to `cc-proxy start`.

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

## `start`

```sh
cc-proxy start [--port <PORT>] [--no-monitor | --monitor]
```

Turns routing on and starts the proxy as a background service. It exits once the proxy answers health checks, printing its pid and listening address.

1. It removes the shell hook that older versions added, if it finds one.
2. It picks the provider. With one provider signed in, it uses that one. With several, it asks, and the last choice is the default. "Claude Code subscription, no routing" is always on the list. With none signed in, it asks which one to sign in to.
3. It asks which model a Claude name should use, only for names it can't match with confidence. Answers are saved. See [Claude names](/using/models-and-routing/#claude-names).
4. It starts the service, or reuses one that already answers on the port, such as a Homebrew service.
5. It writes the `env` block in Claude Code's `settings.json` and sets `route.on` in `config.json`. Open Claude Code windows follow within a few seconds.

A second `start` reports the running pid instead of starting another copy. The service is tracked in a pidfile under the state directory; `status`, `stop`, `restart`, and `reload` all resolve through it.

| Option          | Behavior                                                                                                                                                      |
| --------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--port <PORT>` | Overrides `PORT`, `config.json`, and the default for this invocation.                                                                                         |
| `--no-monitor`  | Runs in the foreground with plain output. It only serves: it never touches `settings.json`. Homebrew and systemd run this, so a reboot never routes anything. |
| `--monitor`     | Runs in the foreground with the monitor dashboard attached. Routing lasts as long as the dashboard does and switches off when you quit it.                    |

The bind address comes from `CCP_BIND_ADDRESS` or `bindAddress`.

Foreground mode keeps running until SIGTERM (Unix) or Ctrl-C requests graceful shutdown, and continues collecting monitor history for separate dashboards.

If `settings.json` isn't valid JSON, `start` stops before it does anything and names the file and the error. Fix the file, then run it again.

## `use`

```sh
cc-proxy use [provider|claude]
```

Changes where Claude Code goes, with the proxy kept running. The provider is `codex`, `kimi`, `cursor`, `grok`, `copilot`, `glm` or `opencode`. `claude` sends Claude Code back to your Claude subscription.

With no argument, `use` asks the same question as `start`. If the provider isn't signed in, it asks `Sign in now? [y/N]` and runs that provider's sign-in. The default is no, so a script never opens a browser. For GLM and OpenCode Go, sign-in reads an API key.

The switch applies on the next request in every open window. Nothing restarts.

## `status`, `stop`, `restart`, `reload`

```sh
cc-proxy status
cc-proxy stop
cc-proxy restart [--port <PORT>]
cc-proxy reload
```

`status` reports how long the service has run, its pid and listening address, where Claude Code goes, and warnings about `settings.json` (a pinned model, or an API key that hides limit reset times). It also warns when the proxy runs an older version than the installed binary, which happens after `brew upgrade` until you restart. It exits 1 when the service is down.

`stop` sends Claude Code back to your Claude subscription first, even if the proxy is already dead, then shuts the service down gracefully (exit 0 when there is nothing to stop). Routing does not come back until you run `start` or `use`. If something answers on the port that cc-proxy didn't start, such as a Homebrew service, `stop` leaves it alone and says to use `brew services stop cc-proxy`.

`restart` stops and starts again, keeping the previous port unless `--port` overrides it. If the port changes, it points Claude Code at the new one. `reload` validates `config.json` and asks a running service to re-read it; file-backed settings apply on the next request, while bind address, port, and environment need `restart`. `reload` exits 1 when no service is running (after validating the config) and validates only on platforms without SIGHUP.

When something answers on the configured port without a pidfile (a foreground or dashboard-attached proxy started separately, for example), `status` says so explicitly, and `restart` refuses to touch a process it did not start.

If routing is on but nothing answers on the port (the proxy crashed, or the machine rebooted), the next cc-proxy command notices, switches routing off and says so on stderr. Until then, new Claude Code windows can't connect.

## Removed in 0.2.0

`cc-proxy claude`, `on`, `off`, `shell install`, `shell uninstall` and `setup` are gone. `start` and `use` do their jobs without a shell hook. `cc-proxy claude` still runs, only to remove the old hook and print what to do next. Run `unset -f claude` or open a new terminal, then run `claude` directly. The config keys `claude.enabled`, `claude.model`, `claude.fastModel` and `aliasProvider` (and `CCP_ALIAS_PROVIDER`) are gone too. See [Models and routing](/using/models-and-routing/) for the replacement.

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
cc-proxy models choose [provider]
cc-proxy models map [name] [model] [--reset] [--provider <provider>]
```

`models` prints supported IDs grouped by provider. The default output compacts Cursor's runtime catalog. `--full` prints every Cursor alias.

`models choose` asks which model each Claude name uses, one name at a time. Type part of a name to search the provider's catalog (every word must match), a number to pick from the list shown, or press Enter to keep the name as it is. The provider defaults to the one Claude Code goes to now.

`models map` shows which model each Claude name (`fable`, `opus`, `sonnet`, `haiku`) uses on a provider, and who decided it. With a name and a model, it saves your choice. With `--reset`, it forgets your choice for that name, or for all four names when you give no name. The provider defaults to the one Claude Code goes to now; `--provider` picks another. A model must be one cc-proxy serves. The change applies on the next request.

## Provider authentication

The command shape is:

```text
cc-proxy <provider> auth <action>
```

| Provider   | `login`                | `device`        | `status`                             | `logout`                               |
| ---------- | ---------------------- | --------------- | ------------------------------------ | -------------------------------------- |
| `codex`    | Browser PKCE           | Device code     | Account, expiry, storage             | Delete proxy credential                |
| `kimi`     | Device code            | Unsupported     | User, expiry, scope, storage         | Delete proxy credential                |
| `grok`     | Browser PKCE           | Device code     | Expiry and storage                   | Delete proxy credential                |
| `copilot`  | Device code            | Same as `login` | Account, host, token expiry, storage | Delete proxy credential and model list |
| `cursor`   | Browser polling flow   | Unsupported     | Source, claims, expiry               | Delete proxy credential                |
| `glm`      | API key entry          | Unsupported     | Env and stored key presence          | Delete proxy credential                |
| `opencode` | API key entry (hidden) | Unsupported     | Key source and base URL              | Delete stored key                      |

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

| Provider   | Asks                                             | Shows                                      |
| ---------- | ------------------------------------------------ | ------------------------------------------ |
| `codex`    | `GET https://chatgpt.com/backend-api/wham/usage` | 5-hour and weekly windows, plan            |
| `kimi`     | `GET https://api.kimi.ai/coding/v1/usages`       | 5-hour and weekly quotas, plan             |
| `opencode` | `GET https://opencode.ai/zen/go/v1/usage`        | Rolling 5-hour, weekly and monthly windows |

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
cargo run -- start
cargo test --all
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
just check
just docs
```

`just docs` installs the locked documentation dependencies and starts the Astro development server on an available local port.
