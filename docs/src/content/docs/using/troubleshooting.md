---
title: Troubleshooting
description: Diagnose listener, authentication, model routing, streaming, context, Cursor bundle, rate-limit, logging, and traffic-capture problems.
---

## Proxy is unreachable

Check the process and liveness route:

```sh
curl http://127.0.0.1:18765/healthz
```

A healthy listener returns `{"ok":true,"version":"..."}`. Confirm `ANTHROPIC_BASE_URL` uses the same address and port. `cc-proxy start --port 11435` and `PORT=11435` change the listener port. `--port` wins for that command.

`cc-proxy status` reports the background service state. If it claims a proxy
answers without a pidfile, that process was not started by `cc-proxy start`.
Stop it directly before starting the service on the same port.

If Claude Code says it can't connect right after a crash or a reboot, routing is still on but nothing answers. Run any cc-proxy command, for example `cc-proxy status`. It sees that the proxy is gone, sends Claude Code back to your Claude subscription and says so. Then run `cc-proxy start` to route again.

## Claude Code ignores a switch

`cc-proxy start`, `use` and `stop` change `settings.json`, and open windows follow within a few seconds. A window can miss the change when:

- it was open before `~/.claude/settings.json` existed. Restart it once.
- a project's `.claude/settings.json` sets its own `ANTHROPIC_BASE_URL`. Project settings beat your user file.
- the host skips user-level `env`, as some desktop and SDK launches do. Set the variables yourself (see [Configure Claude Code](/using/configure-claude-code/#set-the-variables-yourself)).

`cc-proxy status` shows where Claude Code goes now and warns about a pinned model or an API key in your settings.

## Claude Code forgets the task after switching

Some Claude Code sessions use Message Threads: they send only the newest messages and expect Anthropic to remember the rest. Version 0.2.0 accepted those partial requests without storing their earlier history. A tool result could reach Codex without the original task, even if the first request after switching worked.

The fix returns `thread_unsupported_request` before contacting a provider. Claude Code then retries with the full conversation, system prompt and tools, in the same running session. Removing the `thread` field alone would not restore the missing history.

Update the proxy to a version containing this fix. Updating the binary does not replace an already running proxy; arrange a proxy-only restart when no requests are active. Do not restart Claude Code or cancel workflows to apply this fix. A workflow already paused until a usage limit resets may still wait for its client-side timer.

## Authentication error

Check the provider selected by the model, then inspect its credential:

```sh
cc-proxy codex auth status
cc-proxy kimi auth status
cc-proxy grok auth status
cc-proxy cursor auth status
```

Use that provider's login command when credentials are missing or expired. Codex requires ChatGPT subscription auth, not an OpenAI API key. Each provider uses proxy-owned credentials.
OpenCode Go has no `auth status` command; configure its API key through
`CCP_OPENCODE_API_KEY`, `OPENCODE_API_KEY`, or `opencode.apiKey` in
`config.json`.

## Model returns HTTP 400

If the message says routing is off, that Claude Code window still points at the proxy but you ran `cc-proxy stop` or `cc-proxy use claude`. Restart the window, or run `cc-proxy start` to route it again. If it says cc-proxy has no model for a name, run `cc-proxy models map <name> <model>`.

Otherwise run:

```sh
cc-proxy models
cc-proxy models --full
```

An unknown local ID returns a catalog in the error. A known ID can still be rejected upstream when the account, subscription, or region lacks access. Cursor's prefixed form, such as `cursor:gpt-5.5`, forces Cursor routing and avoids cross-provider collisions.

## Background requests fail

Claude Code sends titles and other small background tasks to its `haiku` model, independently of the main model. Run `cc-proxy models map` to see which model `haiku` gets. If it is wrong, run `cc-proxy models map haiku <model>`.

## A tool runs twice

Set `CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1` for Claude Code. Retrying a partially completed stream as non-streaming can duplicate tool calls.

## Session reaches context limits

`[1m]` changes Claude Code's local compaction threshold and does not enlarge upstream context. Use a safe `CLAUDE_CODE_AUTO_COMPACT_WINDOW`, remove `[1m]`, or compact earlier. For ChatGPT GPT-5.6 subscription models, `272000` is the documented setup value.

## Codex WebSocket fails

For a non-TUN local HTTP proxy, set both destination-scheme variables before starting the proxy:

| Variable      | Value                   |
| ------------- | ----------------------- |
| `HTTP_PROXY`  | `http://127.0.0.1:7890` |
| `HTTPS_PROXY` | `http://127.0.0.1:7890` |

Set them through the operating system, service manager, or shell, then start `cc-proxy start` in the same environment.

The default `wss://chatgpt.com` connection uses `HTTPS_PROXY`; a working proxy should show `CONNECT chatgpt.com:443`. The `http://` value is normal: it describes how to reach the proxy, while `HTTPS_PROXY` describes which destinations use it. Restart cc-proxy after changing these variables because the client and pooled WebSocket route are created at startup.

Check `NO_PROXY` when the proxy sees no request. Proxy connection, authentication, or CONNECT failure is returned as an error and never retried directly. Environment variables are supported; OS proxy settings and PAC/WPAD discovery are not automatic.

Use HTTP SSE to isolate transport behavior:

```sh
CCP_CODEX_TRANSPORT=http cc-proxy start
```

The default `auto` transport falls back to HTTP when WebSocket setup fails before sending the request, including a rejected upgrade. It does not replay an in-flight request. A WebSocket 403 alone doesn't prove your login expired; check `cc-proxy codex auth status` before signing in again. If you chose `websocket` explicitly, set `codex.transport` to `auto` to allow fallback.

## Cursor bundle cannot be found

Cursor needs the installed Cursor Agent JavaScript bundle for protobuf schemas. Confirm Cursor Agent is installed, then set:

```sh
CCP_CURSOR_AGENT_BUNDLE=/path/to/cursor-agent/index.js \
  cc-proxy start
```

## Rate limited

Upstream limits are shared with other clients on the same account. Codex limit responses and Kimi HTTP 429 responses surface as HTTP 429 with `retry-after`. Wait for the indicated interval or reduce concurrent traffic.

When a Codex usage window runs out, Claude Code shows when it resets and can continue the task by itself after that. Sessions routed by `cc-proxy start` do this. If you set the variables yourself, use `CLAUDE_CODE_OAUTH_TOKEN=unused` instead of `ANTHROPIC_AUTH_TOKEN`. See [Configure Claude Code](/using/configure-claude-code/).

## Find the complete error

The monitor shows request detail. `proxy.log` contains structured JSONL events, and `errors/` stores complete redacted failed-response payloads. A `request_failed` event includes the error file path.

For a source checkout, run the isolated helper:

```sh
./scripts/debug-proxy
```

It prints a random local URL, a sourceable `client.env`, and artifact paths. Keep the terminal open, reproduce once, then press Ctrl-C. Captures contain sensitive prompt and tool content.

## Focused traffic capture

```sh
CCP_LOG_VERBOSE=1 \
CCP_TRAFFIC_LOG=1 \
  cc-proxy start --no-monitor
```

Traffic capture writes the inbound request, translated upstream request, upstream headers and events, and downstream events in emission order. Known credentials are redacted, but prompts and tool content are preserved. Disable capture and delete artifacts after diagnosis.
