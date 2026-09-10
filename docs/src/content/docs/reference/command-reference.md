---
title: Command reference
description: Canonical cc-proxy command syntax for serving, monitoring, listing models, version output, provider authentication, and OpenCode Go usage.
---

Running `cc-proxy` without a subcommand is equivalent to `cc-proxy serve`.

## Global version commands

```sh
cc-proxy --version
cc-proxy -v
cc-proxy version
```

Each prints `cc-proxy <version>`.

## `serve`

```sh
cc-proxy serve [--port <PORT>] [--no-monitor]
```

Starts the local HTTP proxy and blocks until shutdown.

| Option | Behavior |
| --- | --- |
| `--port <PORT>` | Overrides `PORT`, `config.json`, and the default for this invocation. |
| `--no-monitor` | Uses plain output even when stdout is a terminal. |

The bind address comes from `CCP_BIND_ADDRESS` or `bindAddress`. Interactive stdout opens the monitor unless `--no-monitor` is present. Non-terminal stdout uses plain mode.

Plain mode continues collecting monitor history and supports separate dashboards. SIGTERM (Unix) and Ctrl-C request graceful service shutdown.

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

Examples:

```sh
cc-proxy codex auth login
cc-proxy grok auth device
cc-proxy kimi auth status
cc-proxy cursor auth logout
```

A missing credential makes `auth status` exit with status 1. Other provider command failures exit with status 2. Successful commands exit with status 0.

Logout removes the local proxy-owned credential. It does not call the provider to revoke a refresh token.

## OpenCode Go usage

```sh
cc-proxy opencode usage [--json]
```

Fetches the account's rolling five-hour, weekly, and monthly usage directly
from OpenCode Go. The default output is human-readable; `--json` prints the
upstream response for scripts. The command uses the same API key and base URL
as OpenCode model requests.

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
