---
title: Configuration
description: Canonical cc-proxy configuration keys, environment variables, defaults, precedence, boolean parsing, and example config.json.
---

Proxy settings come from environment variables or `config.json`. Precedence is **environment variable, config file, built-in default**. The `start --port` option takes precedence over all port settings.

These settings configure the proxy process. Claude Code client settings such as `ANTHROPIC_BASE_URL`, `ANTHROPIC_MODEL`, and `CLAUDE_CODE_*` are documented separately in [Configure Claude Code](/using/configure-claude-code/).

## Example `config.json`

```json
{
  "bindAddress": "127.0.0.1",
  "port": 18765,
  "auth": { "autoRotate": false },
  "autoReviewModel": "gpt-5.6-terra",
  "route": {
    "on": true,
    "provider": "codex",
    "fallback": ["antigravity"],
    "tiers": { "codex": { "haiku": "gpt-6-luna" } }
  },
  "codex": {
    "originator": "cc-proxy",
    "userAgent": "cc-proxy/0.1.24",
    "model": "gpt-6-sol",
    "effort": "high",
    "reasoningSummary": "auto",
    "serviceTier": "fast",
    "baseUrl": "https://chatgpt.com/backend-api/codex/responses",
    "transport": "auto",
    "headerTimeoutMs": 300000,
    "previousResponseId": false,
    "serverCompaction": false,
    "responsesApi": false,
    "imagesApi": false,
    "imagesBaseUrl": "https://chatgpt.com/backend-api/codex"
  },
  "kimi": {
    "userAgent": "KimiCLI/1.37.0",
    "oauthHost": "https://auth.kimi.ai",
    "baseUrl": "https://api.kimi.ai/coding/v1"
  },
  "grok": {
    "baseUrl": "https://cli-chat-proxy.grok.com/v1",
    "clientVersion": "0.2.93"
  },
  "opencode": {
    "apiKey": "YOUR_OPENCODE_GO_API_KEY",
    "baseUrl": "https://opencode.ai/zen/go/v1"
  },
  "cursor": {
    "baseUrl": "https://api2.cursor.sh",
    "clientVersion": "0.48.5",
    "agentBundle": "/path/to/cursor-agent/index.js"
  },
  "log": {
    "stderr": false,
    "verbose": false
  }
}
```

All keys are optional. An unreadable file, malformed JSON, or incompatible field type causes the file configuration to be ignored and built-in or environment values to apply.

## Core and diagnostics

| Environment              | Config key         | Default                   | Purpose                                                                                                                                |
| ------------------------ | ------------------ | ------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| `CCP_BIND_ADDRESS`       | `bindAddress`      | `127.0.0.1`               | Listener IP address.                                                                                                                   |
| `PORT`                   | `port`             | `18765`                   | Listener port.                                                                                                                         |
| `CCP_CONFIG_DIR`         | none               | Platform config directory | Replaces the configuration and file-backed auth root.                                                                                  |
| `CCP_AUTO_REVIEW_MODEL`  | `autoReviewModel`  | newest luna for Codex     | Routes Claude Code's non-streaming, tool-free Bash security-review classifier through a routable model.                                |
| `CCP_AUTO_REVIEW_EFFORT` | `autoReviewEffort` | unset                     | Sets `none`, `low`, `medium`, `high`, `xhigh`, or `max` only for auto-review requests routed to Codex. `off` inherits ordinary effort. |
| `CCP_LOG_STDERR`         | `log.stderr`       | `false`                   | Mirrors logs to stderr when present in the environment, regardless of its value.                                                       |
| `CCP_LOG_VERBOSE`        | `log.verbose`      | `false`                   | Preserves full string fields in structured logs when present, regardless of its value.                                                 |
| `CCP_TRAFFIC_LOG`        | none               | `false`                   | Enables full request captures for `1`, `true`, or `yes`.                                                                               |
| `XDG_STATE_HOME`         | none               | `~/.local/state`          | State base on macOS and Linux.                                                                                                         |

`CCP_CONFIG_DIR` affects `config.json` and file-backed provider auth. It does not relocate the state directory.

## Saved accounts and auto-rotate

Every OAuth and API-key provider (Codex, Kimi, Grok, Antigravity, GLM, Gemini,
OpenCode Go, OpenCode Zen, Anthropic, and custom providers) can keep more than
one saved account or API key. Add one with `cc-proxy auth add <provider>`, then
choose it with `cc-proxy auth use <provider> <account>`. These commands do not
restart the proxy. After signing in to Antigravity, run
`cc-proxy models refresh antigravity` to fetch the current account's model
list. Adding another account does not replace that shared list.

`auth.autoRotate` defaults to `false`. To let a running proxy switch accounts:

```sh
cc-proxy config set auth.autoRotate true
```

Or run `cc-proxy config`, find `auth.autoRotate` and press Right. The change
applies on the next request.

When a Messages request returns HTTP 429 with a valid `Retry-After` or a
provider-confirmed future reset time, the account waits until that time.
cc-proxy tries another saved account from the same provider. Each account is
tried at most once per request. Quota percentages alone never trigger a switch.

An API key has no reset time. When a provider answers that the credit is
spent (a payment error such as Anthropic's 402 `billing_error` or "credit
balance is too low", or a spent quota with no clock), cc-proxy moves the request
to the next saved account and sits the spent one out. It tries that account
again 30 minutes later, because a top-up is not visible to cc-proxy. That try is
a recheck, never a claimed reset. After you add credit, run
`cc-proxy auth use <provider> <account>` (or choose Use account for new requests
in the monitor) to try it at once. A failed recheck costs one quick error that
the next account absorbs in the same request. A timeout, a bad key or a short
rate limit without a reset time never sits an account out.

If every saved account is out of credit, the answer says so, names the commands
above and carries no countdown, so Claude Code stops instead of waiting for a
reset that does not exist. It is an HTTP 402 `billing_error`, Anthropic's own
shape for a payment problem. A 429 would make Claude Code say the server is
"temporarily limiting requests", which sends you to wait for something that
will not come back. A throttle, or a spent quota that names its own reset, stays
a 429.

Codex's usage-limit reset clock works even when it omits `Retry-After`.
Antigravity's `QUOTA_EXHAUSTED` errors use Google's retry delay or reset
clock. When a 429 arrives without one, cc-proxy asks Google's quota endpoint
whether the model's group (Gemini, or Claude and GPT-OSS) has a spent 5-hour or
weekly window. If one is spent, the model gets a real deadline: the later reset
of the spent windows. If both windows still have quota, the 429 is a capacity
throttle and the request stays on the same account. A limit blocks only the
model that hit it on that account.
Antigravity's short `RATE_LIMIT_EXCEEDED` capacity throttles never switch
accounts.

The current account is used again once its wait ends. An account whose wait
ends becomes eligible again. `cc-proxy auth status codex` shows each account's
wait.

To start rolling 5-hour quota windows early while `cc-proxy` is running, enable
`auth.hourlyPing`:

```sh
cc-proxy config set auth.hourlyPing true
```

At the start of every hour (`XX:00`), cc-proxy sends a one-token request using
the smallest tier model to every saved account, not only the current one, of
the provider Claude Code routes to and its fallbacks. Only plans with a rolling
window get it: Claude, Codex, Kimi, Antigravity, OpenCode Go and GLM. A key that
pays per token (Anthropic, Gemini, OpenCode Zen, a custom provider) would be
charged for every ping, and a monthly plan (Cursor, Copilot) has no window to
start, so those are left alone. An account that is waiting on a limit or is out
of credit is skipped. With routing off there is nothing to ping.

## Provider fallback

`route.fallback` lists providers to try, in order, when every saved account of
the routed provider is waiting:

```sh
cc-proxy use codex antigravity gemini
```

writes `"provider": "codex"` and `"fallback": ["antigravity", "gemini"]`.
Each request starts on the routed provider again, so the chain returns to it as
soon as it frees up. A fallback provider without an account for the requested
tier is skipped. Leave the array out, or run `cc-proxy use codex --no-fallback`,
to turn the chain off. `cc-proxy use kimi` on its own keeps the chain: Kimi
moves to the front and Codex becomes its first fallback.

When every provider in the chain is spent, the proxy answers one 429 that names
the provider freeing up first and when. The response carries the
`anthropic-ratelimit-unified-*` headers, so Claude Code shows "You've hit your
limit · resets at ..." and resumes the turn when that time arrives instead of
treating it as a retryable error. When the first one to come back is an account
out of credit, there is no reset to show: the answer has no headers and Claude
Code stops. A provider whose accounts are all out of credit is skipped like any
other spent provider.

Rotation never replays output after a stream starts. Codex reasoning signatures
name the saved login that created them, so they remain usable after a proxy
restart but cannot move between accounts. When a conversation carries reasoning
from another saved login (after a switch, a replacement, or history from an
older proxy version), cc-proxy drops only that reasoning and continues with the
text and tool history. The answer may know less about the earlier reasoning;
nothing else is lost.

Codex's native Responses route rejects encrypted history, item references,
`conversation` and `previous_response_id` because their account cannot be
verified. Send full text and tool history instead.

OpenAI-compatible routes, images and audio use the current account without
rotation. API-key providers, Copilot and Cursor do not support saved-account
rotation in this version.

Cooldowns are saved under the state directory, so restarting the proxy does
not clear them. See [Files and storage](/reference/files-and-storage/).

## Routing state

`cc-proxy start`, `use` and `stop` keep the routing state in a `route` object in `config.json`. The proxy reads it on every request, so a switch needs no restart, even when a Homebrew service runs the proxy. `config set` does not take these keys. Use the commands.

| Config key                      | Default | Meaning                                                                                                                                                                |
| ------------------------------- | ------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `route.on`                      | `false` | Whether Claude names are served. Only `start` and `use <provider>` turn it on. `stop` and `use direct` turn it off.                                                    |
| `route.provider`                | unset   | The provider Claude Code goes to. It stays set while routing is off, so `start` can offer it again.                                                                    |
| `route.tiers.<provider>.<name>` | unset   | The model you chose for a Claude name (`fable`, `opus`, `sonnet`, `haiku`). Written by `cc-proxy models choose`, `cc-proxy models set` and the questions `start` asks. |

A missing, unreadable or malformed `config.json` means routing is off. Nothing is routed by accident.

Codex auto-review classifier requests use the newest luna model by default. Requests routed through other providers retain their requested model. `CCP_AUTO_REVIEW_MODEL` or `autoReviewModel` selects an explicit routable model for detected classifier requests without changing the session's provider affinity. Normal messages, streaming requests, tool-using requests, and token counting retain their requested model.

`autoReviewEffort` is disabled by default and applies only when an auto-review route selects Codex. It overrides `codex.effort` or `CCP_CODEX_EFFORT` for that request; `none` explicitly disables reasoning. Unset, empty, or `off` inherits ordinary effort. A non-empty `CCP_AUTO_REVIEW_EFFORT` wins over the file; `off` disables the file value, while an empty environment value falls through to it. Invalid values fail the selected review request with the accepted values. The compaction cap still applies last. Other providers and OpenAI-compatible requests are unchanged.

## Outbound proxies

Outbound HTTP requests and Codex WebSocket setup inherit standard proxy environment variables when each provider client is created:

| Environment                   | Applies to                                      | Purpose                                                                                                   |
| ----------------------------- | ----------------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| `HTTP_PROXY` / `http_proxy`   | `http://` and `ws://` destinations              | Routes plain HTTP and WebSocket Upgrade requests through the configured proxy.                            |
| `HTTPS_PROXY` / `https_proxy` | `https://` and `wss://` destinations            | Routes TLS destinations through the configured proxy, normally with HTTP CONNECT.                         |
| `ALL_PROXY` / `all_proxy`     | Any destination without a scheme-specific proxy | Provides the fallback proxy.                                                                              |
| `NO_PROXY` / `no_proxy`       | Matching destination hosts and IPs              | Bypasses proxy routing. Supports comma-separated domains, subdomains, IP addresses, CIDR ranges, and `*`. |

On case-sensitive platforms, uppercase names are checked before lowercase names when both spellings exist. Windows treats environment names as case-insensitive, so each uppercase/lowercase pair identifies one variable. Set these variables before starting cc-proxy; changing them requires a restart because clients and pooled WebSocket connections retain their startup route. For CGI safety, proxy environment variables are ignored when `REQUEST_METHOD` is present. The variable name describes the **destination** scheme, so this default-WSS configuration is valid even though the local proxy URL uses `http://`:

| Variable      | Value                   |
| ------------- | ----------------------- |
| `HTTP_PROXY`  | `http://127.0.0.1:7890` |
| `HTTPS_PROXY` | `http://127.0.0.1:7890` |

After setting both variables through the operating system, service manager, or shell, start `cc-proxy start` in the same environment.

Proxy URLs may use `http`, `https`, `socks4`, `socks4a`, `socks5`, or `socks5h`. HTTP proxy URLs can contain percent-encoded Basic credentials, for example `http://user:password@127.0.0.1:7890`; SOCKS5 and SOCKS5H URLs can contain username/password credentials for the SOCKS handshake. SOCKS4 and SOCKS4A are supported without URL credentials. Prefer a secret-management mechanism when available because environment variables may be visible to other local processes. WSS certificate verification uses both bundled public roots and the platform native root store, including locally installed enterprise proxy CAs. Malformed or unsupported proxy URLs fail provider startup rather than being ignored. Proxy failures do not silently retry with a direct connection; only `NO_PROXY` selects direct routing. OS proxy settings, PAC files, and WPAD are not read automatically.

## Codex

| Environment                      | Config key                  | Default                                 | Purpose                                                                                                                                                                      |
| -------------------------------- | --------------------------- | --------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `CCP_CODEX_MODEL`                | `codex.model`               | unset                                   | Forces every Codex request to one upstream model.                                                                                                                            |
| `CCP_CODEX_EFFORT`               | `codex.effort`              | unset                                   | Forces `none`, `low`, `medium`, `high`, `xhigh`, or `max`.                                                                                                                   |
| `CCP_COMPACT_EFFORT`             | none                        | `low`                                   | Caps Codex reasoning effort for Claude Code summary compaction requests. `off` disables the cap and `none` removes reasoning.                                                |
| `CCP_CODEX_REASONING_SUMMARY`    | `codex.reasoningSummary`    | unset                                   | Overrides summary mode. `off` and `none` suppress summaries.                                                                                                                 |
| `CCP_CODEX_REASONING_SIGNATURES` | `codex.reasoningSignatures` | `on`                                    | Preserves Codex encrypted reasoning continuation in Anthropic thinking signatures; `off`, `none`, `false`, and `0` suppress signature storage.                               |
| `CCP_CODEX_SERVICE_TIER`         | `codex.serviceTier`         | unset                                   | Requests `fast`, `priority`, `ultrafast`, or `flex` on Codex Messages requests. `fast` sends `priority`. Ultrafast sends `priority` unless the final model is `gpt-6-astra`. |
| `CCP_CODEX_BASE_URL`             | `codex.baseUrl`             | ChatGPT Codex Responses URL             | Changes the Codex endpoint.                                                                                                                                                  |
| `CCP_CODEX_TRANSPORT`            | `codex.transport`           | `auto`                                  | Uses WebSocket with HTTP fallback if setup fails before sending a request. Set `websocket` or `http` to use only that transport.                                             |
| `CCP_CODEX_HEADER_TIMEOUT_MS`    | `codex.headerTimeoutMs`     | `300000`                                | Sets how long an HTTP-transport request waits for the Codex response headers. Values below `1000` are ignored.                                                               |
| `CCP_CODEX_PREVIOUS_RESPONSE_ID` | `codex.previousResponseId`  | `false`                                 | Enables append-only WebSocket continuation for `1`, `true`, or `yes`.                                                                                                        |
| `CCP_CODEX_FULL_LANE`            | `codex.fullLane`            | `false`                                 | Routes `gpt-5.6-sol` / `gpt-5.6-terra` through the full Responses lane instead of Responses Lite, enabling parallel tool calls (`gpt-5.6-luna` stays on Lite).               |
| `CCP_CODEX_SERVER_COMPACTION`    | `codex.serverCompaction`    | `false`                                 | Enables or disables native compaction for standard boolean words.                                                                                                            |
| `CCP_CODEX_RESPONSES_API`        | `codex.responsesApi`        | `false`                                 | Enables `/v1/responses` and `/v1/chat/completions` for every registered provider. Accepts `1`, `true`, or `yes`.                                                             |
| `CCP_CODEX_IMAGES_API`           | `codex.imagesApi`           | `false`                                 | Enables `/v1/images/generations` and `/v1/images/edits` for `1`, `true`, or `yes`.                                                                                           |
| `CCP_CODEX_IMAGES_BASE_URL`      | `codex.imagesBaseUrl`       | `https://chatgpt.com/backend-api/codex` | Sets the trusted Codex Images API root; production use is restricted to HTTPS `chatgpt.com/backend-api/codex`.                                                               |
| `CCP_CODEX_ORIGINATOR`           | `codex.originator`          | `cc-proxy`                              | Changes the Codex `originator` header.                                                                                                                                       |
| `CCP_CODEX_USER_AGENT`           | `codex.userAgent`           | `cc-proxy/<version>`                    | Changes the Codex user-agent.                                                                                                                                                |

`CLAUDE_CODE_PROXY_CODEX_BASE_URL` remains an accepted fallback for the Codex base URL. `CCP_CODEX_BASE_URL` takes precedence.

## Kimi

| Environment           | Config key       | Default                         | Purpose                      |
| --------------------- | ---------------- | ------------------------------- | ---------------------------- |
| `CCP_KIMI_OAUTH_HOST` | `kimi.oauthHost` | `https://auth.kimi.ai`          | Changes the OAuth host.      |
| `CCP_KIMI_BASE_URL`   | `kimi.baseUrl`   | `https://api.kimi.ai/coding/v1` | Changes the API base URL.    |
| `CCP_KIMI_USER_AGENT` | `kimi.userAgent` | `KimiCLI/1.37.0`                | Changes the Kimi user-agent. |

## GitHub Copilot

| Environment                  | Config key | Default                          | Purpose                                    |
| ---------------------------- | ---------- | -------------------------------- | ------------------------------------------ |
| `CCP_COPILOT_BASE_URL`       | none       | the host the Copilot token names | Sends chat to this host.                   |
| `CCP_COPILOT_GITHUB_URL`     | none       | `https://github.com`             | The GitHub site for sign-in.               |
| `CCP_COPILOT_GITHUB_API_URL` | none       | `https://api.github.com`         | The GitHub API that issues Copilot tokens. |

## Grok

| Environment               | Config key           | Default                              | Purpose                                                                                                                                 |
| ------------------------- | -------------------- | ------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------- |
| `CCP_GROK_BASE_URL`       | `grok.baseUrl`       | `https://cli-chat-proxy.grok.com/v1` | Changes the Responses API base URL.                                                                                                     |
| `CCP_GROK_CLIENT_VERSION` | `grok.clientVersion` | `0.2.93`                             | Changes the Grok client version header.                                                                                                 |
| `CCP_GROK_TOOL_IMAGE`     | none                 | `omit`                               | Selects `omit`, `reattach`, `inline`, or `reject` image handling.                                                                       |
| `CCP_GROK_HOSTED_SEARCH`  | none                 | off                                  | Set to `1`, `on`, or `true` to let hosted search tools replace the caller's own search tools and force them on an explicit search turn. |
| `CCP_GROK_SEARCH_BLOCKS`  | none                 | `text`                               | Selects how a hosted search is reported: `text` for a text block, `native` for `server_tool_use` plus a `*_tool_result` block.          |

## OpenCode Go

| Environment             | Config key         | Default                         | Purpose                                                                                  |
| ----------------------- | ------------------ | ------------------------------- | ---------------------------------------------------------------------------------------- |
| `CCP_OPENCODE_API_KEY`  | `opencode.apiKey`  | unset                           | OpenCode Go API key; takes precedence over `OPENCODE_API_KEY` and config.                |
| `OPENCODE_API_KEY`      | `opencode.apiKey`  | unset                           | Fallback API-key variable accepted by the proxy when the CCP-specific variable is unset. |
| `CCP_OPENCODE_BASE_URL` | `opencode.baseUrl` | `https://opencode.ai/zen/go/v1` | Changes the OpenCode Go API base URL.                                                    |

## Cursor Agent

| Environment                 | Config key             | Default                  | Purpose                                                         |
| --------------------------- | ---------------------- | ------------------------ | --------------------------------------------------------------- |
| `CCP_CURSOR_BASE_URL`       | `cursor.baseUrl`       | `https://api2.cursor.sh` | Changes the Cursor API base URL.                                |
| `CCP_CURSOR_CLIENT_VERSION` | `cursor.clientVersion` | `0.48.5`                 | Changes Cursor client version headers.                          |
| `CCP_CURSOR_AGENT_BUNDLE`   | `cursor.agentBundle`   | Auto-detected            | Points to Cursor Agent's bundled `index.js` protobuf schemas.   |
| `CCP_CURSOR_AUTH_TOKEN`     | none                   | unset                    | Uses a bearer token instead of proxy-owned Cursor auth storage. |

## Shared compatibility fallbacks

`CCP_USER_AGENT` is the fallback when a Codex or Kimi provider-specific user-agent is absent. Prefer `CCP_CODEX_USER_AGENT` or `CCP_KIMI_USER_AGENT` in durable configuration.

Endpoint and client identity overrides can break provider compatibility. Use defaults unless a controlled integration or focused diagnostic requires an override.
