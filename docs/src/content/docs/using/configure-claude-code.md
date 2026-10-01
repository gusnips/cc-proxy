---
title: Configure Claude Code
description: Set Claude Code client variables for cc-proxy without mixing them with CCP proxy configuration.
---

Claude Code reads its API connection when the process starts. These variables belong to **Claude Code**, not to the proxy server.

## The short way

```sh
cc-proxy claude
```

`cc-proxy claude` starts the proxy if it isn't running, then runs `claude` with every argument you gave it. It passes the variables below in one `--settings` flag. Claude Code ranks that flag above your settings files, for that session only, and no file is written.

Two config keys choose the models:

| Config key | Sets |
| --- | --- |
| `claude.model` | `ANTHROPIC_MODEL`, the model each session starts on. Unset, Claude Code picks its own and the proxy routes it. |
| `claude.fastModel` | `ANTHROPIC_DEFAULT_HAIKU_MODEL`, the model for small background requests. |

```sh
cc-proxy config set claude.model k3[1m]
```

`--model` on the command line still wins for that session.

## Minimal client contract

```sh
ANTHROPIC_BASE_URL=http://127.0.0.1:18765 \
ANTHROPIC_AUTH_TOKEN=unused \
ANTHROPIC_MODEL=gpt-6-sol[1m] \
ANTHROPIC_DEFAULT_HAIKU_MODEL=gpt-6-luna[1m] \
CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 \
CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1 \
  claude
```

| Claude Code variable | Purpose |
| --- | --- |
| `ANTHROPIC_BASE_URL` | Sends Anthropic API requests to the local proxy. |
| `ANTHROPIC_AUTH_TOKEN` | Satisfies Claude Code's client credential requirement. The proxy does not use it for upstream auth. |
| `ANTHROPIC_MODEL` | Selects the main request model and therefore the provider. |
| `ANTHROPIC_DEFAULT_HAIKU_MODEL` | Selects the model for title generation, token-related background work, and other small requests. Use a model the proxy routes. It replaces the older `ANTHROPIC_SMALL_FAST_MODEL`. |
| `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` | Reduces background traffic sent to the subscription provider. |
| `CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1` | Prevents Claude Code from retrying a partially completed stream as non-streaming, which can duplicate tool calls. |

The proxy always makes streaming upstream requests. It can still accumulate a non-streaming Anthropic response when the client requests one.

## Compaction settings

A trailing `[1m]` tells Claude Code to use its larger local context policy. The proxy strips the suffix before upstream routing. It does not enlarge the provider's context window.

OpenAI's [GPT-5.6 subscription update](https://x.com/thsottiaux/status/2076495156757577895)
sets the ChatGPT context limit to 272K tokens. Set
`CLAUDE_CODE_AUTO_COMPACT_WINDOW=272000` with `gpt-5.6-sol[1m]` so Claude Code
compacts before the upstream limit.

Grok 4.5 and Grok 4.6 are 500,000 tokens. Set `CLAUDE_CODE_MAX_CONTEXT_TOKENS=500000` and omit `[1m]`.

For a provider and model with a different real context limit, choose a safe value or omit the override. `DISABLE_AUTO_COMPACT=1` disables automatic compaction while preserving manual `/compact`, but the session can then hit the upstream limit.

## Persistent Claude Code settings

If every Claude Code session should use the proxy, put client variables in `~/.claude/settings.json`:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:18765",
    "ANTHROPIC_AUTH_TOKEN": "unused",
    "ANTHROPIC_MODEL": "gpt-6-sol[1m]",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL": "gpt-6-luna[1m]",
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW": 272000,
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": 1,
    "CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK": 1
  }
}
```

Use process-level variables or a wrapper when you also launch Claude Code directly against Anthropic. See [Switching models and backends](/using/switching-models-and-backends/).

## Proxy settings are separate

`CCP_*`, `PORT`, and `config.json` configure the **cc-proxy server process**. They control the listener, provider endpoints, transport, credentials, and diagnostics. They do not belong in Claude Code's client environment unless the same shell also starts the proxy.

See [Configuration](/reference/configuration/) for the canonical server setting table.
