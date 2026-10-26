---
title: Files and storage
description: Canonical cc-proxy configuration, credential, device ID, log, error, traffic-capture, and service-log paths on macOS, Linux, and Windows.
---

cc-proxy separates configuration and credentials from runtime state.

## Directory roots

| Platform | Configuration root                       | State root                                   |
| -------- | ---------------------------------------- | -------------------------------------------- |
| macOS    | `~/.config/cc-proxy`                     | `${XDG_STATE_HOME:-~/.local/state}/cc-proxy` |
| Linux    | `${XDG_CONFIG_HOME:-~/.config}/cc-proxy` | `${XDG_STATE_HOME:-~/.local/state}/cc-proxy` |
| Windows  | `%APPDATA%/cc-proxy`                     | `%LOCALAPPDATA%/cc-proxy`                    |

A `config.json` left in the previous `claude-code-proxy` configuration root is
still read when the new location has none, and file-backed provider
credentials fall back to the old directory the same way.

Windows falls back to `%USERPROFILE%/AppData/Roaming` and `%USERPROFILE%/AppData/Local` when the corresponding environment variable is absent.

`CCP_CONFIG_DIR` replaces the configuration root for the current process. It does not change the state root.

`proxy.pid` under the state root tracks the background service (`start`
writes it after binding; `status`, `stop`, `restart`, and `reload` resolve
through it). A pidfile left behind by a dead process is treated as not
running and removed.

## Configuration

`config.json` lives directly under the configuration root. See [Configuration](/reference/configuration/) for its schema and precedence.

## Claude Code's settings file

`cc-proxy start`, `use` and `stop` edit one file that isn't in either root: `settings.json` in Claude Code's config folder. That is `~/.claude/settings.json`, or the folder `CLAUDE_CONFIG_DIR` names. cc-proxy changes only the `env` block and one top-level `cc-proxy` key that holds your earlier values. Everything else in the file, including key order and formatting, stays as it was. A symlink is followed. A file that isn't valid JSON is never overwritten. See [Configure Claude Code](/using/configure-claude-code/).

## Provider credentials

Codex, Kimi, Grok and Antigravity keep saved accounts under their provider folder:

- `accounts.json`: account names, internal IDs and which account is current.
- `accounts/<id>.json`: one account's credentials. A new sign-in gets a new ID.
- `accounts.lock`: a short file lock for account changes and refresh writes.

A refresh only writes its own account file. A refresh that finishes after
sign-out or replacement cannot restore the old login. Credential files are
private to your user where the operating system supports file permissions.

On first use, a single login in `<provider>/auth.json`, the old configuration
folder or Codex's `claude-code-proxy.codex` Keychain entry moves into an account
called `main`. The old copy is cleared only after the account is saved.

Cursor still reads its `claude-code-proxy.cursor` Keychain entry on macOS,
with a file fallback. Copilot keeps `copilot/auth.json`.
`CCP_CURSOR_AUTH_TOKEN` supplies Cursor's login directly for that process.
`CCP_CONFIG_DIR` changes the file root and disables Keychain use.

`rotation.json` under the state root keeps cooldown deadlines by provider and
internal account ID, with a model name when the limit applies to one model.
It holds no tokens. `auth status` reads it without contacting a provider.

`usage-last.json` in the same folder keeps the numbers from each saved account's
last good usage lookup: the remaining percent and reset time of each window. The
monitor shows them, with a warning, when a later lookup fails. It holds no tokens.

OpenCode Go is the exception: it reads its API key from
`CCP_OPENCODE_API_KEY`, `OPENCODE_API_KEY`, or `opencode.apiKey` in
`config.json` and does not create a provider auth store.

The proxy owns these credentials independently of native Codex, Grok, and Cursor Agent stores.

## Kimi device ID

Each Kimi account keeps its device ID in its own credential file. A migrated
login keeps the ID from `<configuration-root>/kimi/device_id`. New logins get
new IDs. Copy the whole provider folder, not a token by itself.

## Structured log

`proxy.log` lives under the state root. It uses JSON Lines and rotates at 20 MiB. Known credential keys, including authorization, access tokens, refresh tokens, ID tokens, and account headers, are redacted before writing.

A Homebrew service also writes `service.log` under the state root.

## Failed responses

`errors/` under the state root contains JSON files for failed proxy responses. A `request_failed` log event includes `errorFile`, which points to the complete redacted payload.

Error payloads are safer to share than raw traffic captures, but inspect their prompt-derived content and paths before publishing them.

## Traffic captures

Set `CCP_TRAFFIC_LOG=1` to create captures under `traffic/` in the state root. Requests are grouped by Claude Code session and request sequence. A request directory can include:

- inbound Anthropic request and metadata
- translated upstream request
- upstream URL and redacted headers
- raw or decoded upstream events
- translated downstream events and response
- transport or reducer error details

Event filenames use monotonic sequence numbers so lexical order preserves emission order.

<div class="security-callout">
<strong>Traffic captures preserve content.</strong> Header and token redaction does not remove prompts, source code, tool definitions, tool inputs, tool results, images, or provider output. Treat the entire directory as sensitive user data.
</div>

Enable capture only for a focused reproduction, keep the directory local, and delete it after the investigation.
