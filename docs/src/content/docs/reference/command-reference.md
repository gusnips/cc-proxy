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
cc-proxy models [show] [provider]
cc-proxy models list [provider] [--full]
cc-proxy models choose [provider]
cc-proxy models set <name> <model> [--provider <provider>]
cc-proxy models reset [name] [--provider <provider>]
cc-proxy models refresh [provider|all]
```

`models` (or `models show`) shows which model each Claude name (`fable`, `opus`, `sonnet`, `haiku`) uses on a provider, and who decided it.

`models list` prints supported IDs grouped by provider, or for one provider. The default output compacts Cursor's runtime catalog. `--full` prints every Cursor alias.

`models choose` asks which model each Claude name uses, one name at a time. Type part of a name to search the provider's catalog (every word must match), a number to pick from the list shown, or press Enter to keep the name as it is. The provider defaults to the one Claude Code goes to now.

`models set` sets one Claude name directly, and `models reset` forgets your choice for one name, or for all four when you give no name.

`models refresh` fetches live lists for Antigravity, GitHub Copilot, Gemini, OpenCode Go, OpenCode Zen and providers you added. Use `models refresh all` to refresh them together. Failures keep the saved list and do not stop the other refreshes. Providers without live discovery are reported as skipped. See [Refresh model lists](/using/models-and-routing/#refresh-model-lists).

## Provider authentication

The command shape is:

```text
cc-proxy auth <action> [provider]
```

For sign-in and sign-out, the shorter forms take the same arguments and options:

```sh
cc-proxy login codex                # same as: cc-proxy auth login codex
cc-proxy login codex --device
cc-proxy login codex --replace
cc-proxy logout codex               # same as: cc-proxy auth logout codex
cc-proxy logout codex work
cc-proxy logout codex --all
```

| Provider      | `login`                | `device`        | `status`                             | `logout`                               |
| ------------- | ---------------------- | --------------- | ------------------------------------ | -------------------------------------- |
| `codex`       | Browser PKCE           | Device code     | Account, expiry, storage             | Delete proxy credential                |
| `kimi`        | Device code            | Unsupported     | User, expiry, scope, storage         | Delete proxy credential                |
| `grok`        | Browser PKCE           | Device code     | Expiry and storage                   | Delete proxy credential                |
| `antigravity` | Google browser sign-in | Same as `login` | Account and expiry                   | Delete a saved account                 |
| `copilot`     | Device code            | Same as `login` | Account, host, token expiry, storage | Delete proxy credential and model list |
| `cursor`      | Browser polling flow   | Unsupported     | Source, claims, expiry               | Delete proxy credential                |
| `glm`         | API key entry          | Unsupported     | Env and stored key presence          | Delete proxy credential                |
| `opencode`    | API key entry (hidden) | Unsupported     | Key source and base URL              | Delete stored key                      |

Examples:

```sh
cc-proxy login codex
cc-proxy auth device grok
cc-proxy auth status kimi
cc-proxy logout cursor
```

### Saved accounts

Codex, Kimi, Grok and Antigravity support these commands:

```sh
cc-proxy auth add codex --name work
cc-proxy auth use codex work
cc-proxy auth status codex
cc-proxy login codex --replace --account work
cc-proxy logout codex work
cc-proxy logout codex --all
```

`add` saves another login without switching away from the current account.
`--name` is optional; the first default name is `main`, then `account-2` and so
on. Names use up to 32 letters, numbers, `-` or `_` and start with a letter or
number. `--device` chooses device-code login when supported.

`login` with an existing account asks Add or Replace. Outside a terminal it
refuses to guess: use `auth add` or `login --replace`. `--account` names which
account to replace; without it, the current account is replaced. A failed or
cancelled sign-in keeps the saved login. Signing in as an identity already
saved offers to replace that account instead of adding a duplicate.

`auth use` without an account opens a picker in a terminal. The switch applies
to the next request, without restarting the proxy. `auth status` lists all
accounts, the current one and any limit wait. `logout` without a name signs
out when there is one account; with several, it asks which one. Scripts must
name the account or use `--all`.

Other providers still keep one login. See [auto-rotate](/reference/configuration/#saved-accounts-and-auto-rotate)
for automatic switching and its limits.

A missing credential makes `auth status` exit with status 1. Other provider command failures exit with status 2. Successful commands exit with status 0.

Logout removes the local proxy-owned credential. It does not call the provider to revoke a refresh token.

## `usage`

```sh
cc-proxy usage [codex|kimi|opencode] [--json]
```

Shows how much of each plan you have used and when each limit resets, asked
straight from the provider with the sign-in the proxy already holds:

```text
Codex `home` [current] (plus)
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

Codex and Kimi show every saved account and mark the current one. Usage
lookups never change which account requests use. A failed lookup names the
account and leaves other accounts visible. `auth status <provider>` shows
saved accounts and confirmed cooldowns without contacting the provider.

`--json` prints one object with a key per provider. Codex and Kimi share one
shape:

```json
{
  "codex": {
    "accounts": [
      {
        "name": "home",
        "current": true,
        "usage": {
          "plan": "plus",
          "five_hour": { "used_percent": 45.0, "resets_at_ms": 1786294800000 },
          "weekly": { "used_percent": 7.0, "resets_at_ms": 1786752000000 }
        }
      }
    ]
  }
}
```

`resets_at_ms` is a Unix time in milliseconds. A field the provider didn't
send is left out. `opencode` holds OpenCode Go's own reply, unchanged.

## OpenCode Go API key

Store the key once without echoing it:

```sh
cc-proxy login opencode
```

The key lands in `opencode.apiKey` in config.json. `auth status opencode`
reports which source provides the key; `logout opencode` removes the
stored key. When nothing of ours is set, the proxy falls back to
`OPENCODE_API_KEY` in Claude Code's `~/.claude/settings.json`.

## Configuration values

```sh
cc-proxy config
cc-proxy config get <key>
cc-proxy config set <key> <value>
cc-proxy config list
cc-proxy config edit
```

Reads and writes config.json through dotted keys (`port`,
`opencode.apiKey`, `codex.fullLane`). `get` prints one value, `set`
validates and writes one, `list` shows every known key, and `edit` opens
the file in your editor. Reads show the file value; when an
environment variable overrides a key at runtime, the commands say so.
Secret keys only ever report set or unset, never their value. Restart the
service after changing `port` or `bindAddress`.

### The settings screen

In a terminal, `cc-proxy config` with no subcommand lists every setting:

| Key                  | What it does                                                         |
| -------------------- | -------------------------------------------------------------------- |
| Up, Down             | Move through the list. It wraps at the ends.                         |
| Left, Right          | Step a yes/no or a fixed choice back or forward, and save it.        |
| Enter                | On a yes/no or a choice, step forward. On text or a number, type it. |
| Any letter or number | Search by name or description. Backspace edits the search.           |
| Esc                  | Clear the search, then close. In a field, drop what you typed.       |
| Ctrl-C               | Close.                                                               |

Every change saves as soon as you make it, exactly as `config set` would, so
there is nothing to confirm and nothing to undo when you close the screen. The
line under the list says what was saved, whether an environment variable still
wins, and whether the running proxy needs `cc-proxy restart`. A key is hidden
while you type it and never shown again.

The `providers` and `config.json` rows open the file in your editor, on the
line of the `providers` key. When the editor closes, the screen reads the file
again.

Run in a pipe or a script, `cc-proxy config` prints the same list as
`cc-proxy config list` and never waits for input.

### Your editor

`edit` and the editor rows use `$VISUAL`, or `$EDITOR` when that is empty. The
variable can hold arguments and quotes, such as `code --wait` or
`"/Applications/Sublime Text.app/subl" -w`. It never goes through a shell, so
`$HOME` and `|` reach the editor as plain text. vim, nvim, nano, emacs, VS Code,
Cursor, Windsurf and VSCodium start on the right line. Any other editor opens
at the top. An editor that returns before you close the file (most graphical
ones) needs its wait flag, such as `-w`. cc-proxy adds `--wait` for the VS Code
family itself.

### A file cc-proxy can't read

If config.json isn't valid JSON, can't be read, or a key you change sits inside
a value that isn't an object (`"log": "x"` when you set `log.verbose`), cc-proxy
stops and names the file or the key. It never replaces a file it can't read
with a new one. Fix it with `cc-proxy config edit`, then try again.

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
