---
title: OpenCode Go
description: Configure an OpenCode Go API key, account usage, model routing, streaming, tools, and provider overrides.
---

OpenCode Go uses the API at `https://opencode.ai/zen/go/v1`. Its catalog spans
OpenAI-compatible chat completions, Anthropic-compatible messages, and OpenAI
Responses; the proxy selects the wire protocol for each registered model. Each
mapping comes from the [official OpenCode Go endpoint table](https://opencode.ai/docs/go/#endpoints)
or direct protocol verification against the API.

## Account and authentication

Subscribe to OpenCode Go, copy your API key, and store it once (input is
hidden, nothing lands in shell history):

```sh
cc-proxy opencode auth login
```

The key lands in `opencode.apiKey` in config.json. Alternatives, in
precedence order: `CCP_OPENCODE_API_KEY`, `OPENCODE_API_KEY`, then the
config key. When none of ours is set, the proxy falls back to
`OPENCODE_API_KEY` in Claude Code's `~/.claude/settings.json`, so a key
already pasted there keeps working. The proxy does not implement an
OpenCode login flow.

To see the current percentage used and reset time for each account limit, run:

```sh
cc-proxy opencode usage
cc-proxy opencode usage --json
```

This fetches OpenCode Go's rolling five-hour, weekly, and monthly windows. The
upstream `/usage` endpoint is implemented by OpenCode but is not yet listed in
its public API table, so its response format may evolve. The JSON form preserves
additional upstream fields for scripting.

The proxy also exposes the same limits in the standard Claude Code Router
account format. In Claude Code Router, enable **Fetch usage** for the proxy
provider and select **Standard usage endpoint**. The dashboard will discover
`/.well-known/ccr/account`; `/v1/account/limits` is available as a compatible
alias. These routes use the proxy's configured OpenCode key and ignore the
incoming placeholder key. Successful upstream results are cached for 60 seconds;
an expired refresh failure is returned rather than silently serving stale data.

## Models

Run `cc-proxy models` for the statically registered catalog. Every
registered model has a provider-qualified form. Bare IDs are also accepted when
they do not belong to another provider:

```sh
ANTHROPIC_MODEL=opencode-go/glm-5.2 \
ANTHROPIC_SMALL_FAST_MODEL=opencode-go/glm-5.2 \
  claude --model opencode-go/glm-5.2
```

The bare IDs `gpt-5.6-luna`, `gpt-6-luna`, `grok-4.5`, `grok-4.6`, `grok-4.7`,
`kimi-k3`, and `kimi-k2.6` remain owned by the existing Codex, Grok, or Kimi
providers. Prefix those IDs with `opencode-go/` to select the OpenCode Go
version.

Any other `opencode-go/<model-id>` is forwarded to OpenCode Go even when the
local catalog has never seen it, using the wire protocol inferred from the
model family (minimax and qwen use messages, grok/gpt/muse-spark use
responses, everything else uses chat completions). Refresh the catalog with
`scripts/refresh-opencode-models.py` when new models appear so they resolve to
their documented protocol instead of the inference. IDs OpenCode Go does not
serve fail with its own upstream error.

## Tools and streaming

Claude function definitions, tool choices, tool calls, and tool results are
translated for chat-completions models. Tool-call argument fragments are
streamed incrementally and reassembled into Anthropic `tool_use` blocks.
Upstream tool behavior remains model-dependent.

Models served through the Anthropic-compatible endpoint retain their native
messages stream. Responses-backed models are translated to the same Anthropic
event stream as other providers. The proxy handles `/v1/messages/count_tokens`
locally and does not send that request to OpenCode Go.

## Configuration

- `CCP_OPENCODE_API_KEY`, `OPENCODE_API_KEY`, or `opencode.apiKey` supplies the key.
- `CCP_OPENCODE_BASE_URL` or `opencode.baseUrl` changes the API base URL.

OpenCode Go adds and removes model IDs over time. Unknown `opencode-go/`
IDs are forwarded upstream instead of being rejected locally, so a catalog
change on their side never breaks routing here. Access, usage-limit, and
unknown-model errors are surfaced from OpenCode Go.

Run `scripts/refresh-opencode-models.py --help` to refresh the registered
catalog from the live `/v1/models` endpoint.
