---
title: Codex
description: Configure ChatGPT Codex authentication, models, reasoning, tools, images, transports, continuation, compaction, and OpenAI-compatible APIs.
---

Codex uses the ChatGPT subscription Responses endpoint at `https://chatgpt.com/backend-api/codex/responses`.

OpenAI's Thibault Sottiaux has publicly welcomed using Codex through other coding
harnesses:

> [Share the recipe. People want to know how to use GPT-5.6 Sol in CC. We don't
> discriminate on the harness.](https://x.com/thsottiaux/status/2075830097488249060)

## Account and authentication

Sign in with a **ChatGPT Plus or Pro account**, not OpenAI API credentials.

```sh
cc-proxy codex auth login
# Headless device-code flow
cc-proxy codex auth device
cc-proxy codex auth status
```

The proxy owns its tokens and does not read native Codex CLI credentials. It refreshes expiring access tokens with a single-flight guard. See [Files and storage](/reference/files-and-storage/) for credential locations.

## Models and service tiers

Use `cc-proxy models` as the current catalog. Model access depends on your ChatGPT account. A model rejected by the subscription produces the upstream error verbatim.

Claude-style aliases map to Codex models: `haiku` and `claude-haiku-*` to `gpt-6-luna`, `sonnet` and `claude-sonnet-*` to `gpt-5.6-terra`, and `opus`, `fable`, `claude-opus-*` (including `claude-opus-5-5`), and `claude-fable-*` to `gpt-6-sol`.

Append `-fast` to a Codex model to request `service_tier: "priority"`. For example, `gpt-6-sol-fast` sends `gpt-6-sol` with the priority tier. Models discovered in the Codex CLI cache and unlisted `gpt-*` IDs keep this behavior.

Use `gpt-6-astra-ultrafast` to request `service_tier: "ultrafast"`. Astra is the only model with known ultrafast support in the proxy's tier list. The CLI cache does not report tier support. An `-ultrafast` suffix on another Codex model is removed and requests priority instead, including unlisted `gpt-*` IDs. Use one suffix; names such as `gpt-6-astra-fast-ultrafast` are rejected.

On `/v1/messages`, `CCP_CODEX_SERVICE_TIER` or `codex.serviceTier` wins over the suffix. A model override's own suffix wins over the requested model's suffix. Ultrafast falls back to priority when the final model is not Astra. The OpenAI-compatible routes ignore the global service-tier setting: they use the model suffix, and an explicit `/v1/responses` `service_tier` is forwarded unchanged.

## Reasoning

Claude Code's `/effort` value maps to Codex `reasoning.effort`: `low`, `medium`, `high`, `xhigh`, or `max`. A request that turns thinking off sends `none`. A proxy override can also force `none`.

When reasoning is enabled, the proxy requests an automatic reasoning summary and translates summary deltas into Claude Code thinking blocks. Codex may omit a summary for a simple prompt. `CCP_CODEX_REASONING_SUMMARY=off` suppresses summaries while preserving effort and encrypted continuation content.

Claude Code summary compaction requests are capped at low effort by default because they perform extraction over a large transcript. `CCP_COMPACT_EFFORT=off` disables the cap, `none` removes reasoning, and another valid effort sets a different maximum. The cap never raises effort.

### Auto-review effort

Claude Code's non-streaming, tool-free security-review requests route to `gpt-6-luna` by default when they use Codex. `autoReviewModel` can select another model. To set a separate effort for these requests, use:

```sh
cc-proxy config set autoReviewEffort low
```

Or set `CCP_AUTO_REVIEW_EFFORT` for the proxy process. Accepted values are `none`, `low`, `medium`, `high`, `xhigh`, and `max`. This setting overrides global Codex effort only after an auto-review route selects Codex. `none` disables reasoning. Unset, empty, or `off` keeps ordinary effort precedence; an environment value of `off` disables the file setting. Invalid values fail only the selected review request and name the setting to fix.

Normal messages, token counting, reviews routed to another provider, and OpenAI-compatible requests keep their existing effort rules. The compaction cap still applies last.

## Tools and multimodal input

- Claude function tools and tool results map to Responses API function calls and outputs.
- Claude Code's forced `web_search_20250305` subrequest uses Codex's standalone
  `/alpha/search` endpoint. It keeps the resolved model, omits search reasoning,
  and preserves non-empty domain filters, so Luna searches do not require a Sol
  Responses turn. Automatic hosted-search requests remain on the full Responses
  API because the standalone endpoint cannot decide whether to invoke a tool.
  Structured result DTOs map back to Anthropic `server_tool_use` and
  `web_search_tool_result` blocks, while standalone text output remains text.
  The proxy locally estimates input and output tokens and reports search usage.
  Standalone search sessions use the same ownership identity as continuation:
  Main keeps its Claude Code session ID, while each direct Agent gets a stable,
  opaque owner derived from its session and Agent IDs. The parent Agent ID is
  validation-only. Missing, malformed, or ambiguous identity headers use a fresh
  random search ID instead of sharing state. The search body ID and upstream
  `session_id`, `x-client-request-id`, and `x-codex-window-id` headers all carry
  that same search owner (with the window header's required `:0` suffix).
- Top-level base64 user images map to `input_image`.
- Supported base64 images nested in tool results also map to `input_image`.
- Remote image URLs, malformed images, and unsupported tool-result image forms remain textual placeholders.
- Strict JSON schema output maps to Responses `text.format`.

## Transport and continuation

WebSocket is the default transport. Set `CCP_CODEX_TRANSPORT=http` for HTTP SSE, or `auto` to use WebSocket with HTTP fallback only when setup fails before a request is sent.

On the HTTP transport, the proxy waits five minutes for the response headers before failing the request. Codex withholds the response head until the model produces its first output, so a large request to a high-effort model can hold it for minutes. `CCP_CODEX_HEADER_TIMEOUT_MS` or `codex.headerTimeoutMs` changes that bound.

### Connection pacing

Fresh WebSocket connections are paced adaptively. While the origin accepts upgrades the
proxy opens connections without spacing them; each rejected upgrade widens the spacing
(1s, then doubling up to 8s), and a run of successful connections narrows it back down.

Because continuation is off by default, every request opens a fresh connection, so a
fixed spacing would cap a single process at roughly one generation per second no matter
how healthy the origin is. Pacing is therefore the price of an observed rejection rather
than a standing tax.

Set `CCP_CODEX_WS_CONNECT_SPACING_MS` (or `codex.websocketConnectSpacingMs`) to impose a
floor the proxy never relaxes below. The default is `0`.

WebSocket setup honors `HTTP_PROXY` for `ws://`, `HTTPS_PROXY` for the default `wss://` endpoint, `ALL_PROXY` as a fallback, and `NO_PROXY` exclusions. A normal HTTP proxy can therefore carry the default WebSocket connection with CONNECT; TUN mode is not required. Set proxy variables before starting the process and restart after changing them. For example, setting `HTTPS_PROXY` to `http://127.0.0.1:7890` sends HTTPS/WSS destinations through the HTTP proxy at port 7890; it does not require an `https://` proxy URL.

`CCP_CODEX_PREVIOUS_RESPONSE_ID=1` enables append-only WebSocket continuation. A valid identity containing only a Claude Code session ID owns the Main continuation for that session. Each valid direct Agent ID owns an independent continuation and reusable WebSocket within the same session. Nested Agents are keyed by their direct child ID; the parent ID is validated but does not become part of the owner key. The proxy sends `previous_response_id` only when the translated request shape and transcript extension are safe, and only on the exact live WebSocket that produced that response.

An absent, malformed, or ambiguous identity does not reject the HTTP request; that request proceeds without continuation or WebSocket reuse. If the originating socket is missing, dead, or has been replaced, the proxy retries once with the full translated input and without the stale response ID. Continuation and connection state is held only in memory and is lost when the proxy restarts.

Detected auto-review classifier subrequests are intentionally stateless even when valid session and Agent headers are present. They neither consume nor publish continuation or WebSocket ownership.

## Server compaction

Claude Code normally compacts a long conversation by asking the active model to write a portable text summary. Later turns contain that summary instead of the original transcript. This works across providers, but a prose summary can flatten details from a long, tool-heavy Codex session.

Codex server compaction preserves the same boundary in a model-native form. Codex returns an opaque encrypted `compaction` item representing the earlier Responses history. On later turns, the proxy gives that item back to Codex together with selected recent messages and everything added after the boundary. Claude Code still receives its normal portable summary, so the session has a safe fallback.

This is most useful for long coding sessions where continuity after `/compact` or automatic compaction matters. It does not increase the model's context window or prevent Claude Code from compacting. The boundary also takes longer because it adds one Codex request.

### How it works

1. Claude Code reaches a manual or automatic compaction boundary.
2. The proxy sends the translated conversation to Codex with a trailing `compaction_trigger`.
3. Codex returns an encrypted `compaction` item, which the proxy keeps in memory for that Claude Code session and model.
4. Claude Code completes its normal summary request. The proxy uses the resulting summary as an exact anchor.
5. On subsequent matching turns, the proxy replaces the portable summary with the encrypted item, retained recent context, and post-compaction messages.

The encrypted item remains opaque to the proxy. It is stored only in memory and sent back to Codex as native Responses input.

### Enable server compaction

Server compaction is disabled by default. Enable it in `config.json`:

```json
{
  "codex": {
    "serverCompaction": true
  }
}
```

Or enable it for one proxy process:

```sh
CCP_CODEX_SERVER_COMPACTION=1 cc-proxy serve
```

### Fallbacks and visibility

Replay requires the same Claude Code session and Codex model with append-only history. A branch, proxy restart, provider or model change, malformed response, upstream failure, memory limit, or 30 minutes without matching activity discards the native state and uses Claude Code's portable summary instead.

While the native request is active, the monitor shows `compacting`. Structured log events named `server_compaction_triggered`, `server_compaction_completed`, and `server_compaction_failed` report each attempt and outcome.

## OpenAI-compatible APIs

`CCP_CODEX_RESPONSES_API=1` enables both `POST /v1/responses` and `POST /v1/chat/completions`. The setting is under Codex configuration, but the routes also accept Kimi, Grok, OpenCode Go, and Cursor models.

The Responses route preserves native JSON or SSE response bodies for registered Codex models. The Chat Completions route translates standard text messages, reasoning effort, JSON object or JSON Schema output, and buffered or streaming responses. Its omitted reasoning effort defaults to `medium`; the proxy-wide Codex effort override still takes precedence.

The proxy replaces incoming credentials with stored Codex auth for both routes. Response retrieval or deletion, function calling through Chat Completions, and WebSocket ingress are outside their scope. See [HTTP API](/reference/http-api/) for supported Chat Completions fields and error behavior.

## Images API

`CCP_CODEX_IMAGES_API=1` separately enables `POST /v1/images/generations` and `POST /v1/images/edits`. The routes reuse the proxy's stored ChatGPT OAuth session and target the ChatGPT Codex image backend; no OpenAI Platform API key is required.

```sh
CCP_CODEX_IMAGES_API=1 cc-proxy serve
```

The model defaults to and is restricted to `gpt-image-2`. Generation accepts JSON. Editing accepts either Codex JSON data URLs or OpenAI-style multipart uploads, which the proxy validates and converts into the Codex JSON contract. Results are returned as `data[].b64_json`. Masks, remote URLs, URL-formatted output, and image variations are not supported.

This is an internal ChatGPT Codex interface rather than the public Platform Images API. It consumes the signed-in account's image quota and can change without public API compatibility guarantees. Image prompts, uploads, generated base64, and upstream error bodies are excluded from traffic captures and persistent error diagnostics.

Because callers are not authenticated, binding to a LAN address lets every firewall-admitted host consume the signed-in account's quota. Restrict the listener to a trusted interface/subnet and never expose it through router forwarding, UPnP, a public tunnel, or permissive IPv6 rules.

See [Configuration](/reference/configuration/) for every Codex setting and [Troubleshooting](/using/troubleshooting/) for auth, model, and transport failures.
