---
title: Models and routing
description: Understand provider model patterns, aliases, Codex fast mode, context hints, small models, model listing, and Claude Code gateway discovery.
---

The model ID in each request selects its provider. One proxy listener can serve every provider through Anthropic Messages and the enabled OpenAI-compatible routes.

## Routing patterns

| Pattern | Provider |
| --- | --- |
| Any `gpt-*` ID and its local tier forms (`-fast` or `-ultrafast`; unlisted base IDs forward to Codex) | Codex |
| `k3`, `kimi-k3`, `kimi-k2.6`, `k2.6` (legacy `kimi-for-coding`); unlisted `kimi-*`/`k2-*` forward raw | Kimi |
| `grok-composer-2.5-fast`, `grok-4.5`, `grok-4.6`, `grok-4.7`; unlisted `grok-*` forward raw | Grok |
| Non-conflicting OpenCode Go IDs and **every** `opencode-go/<model-id>`, registered or not | OpenCode Go |
| `glm-5.3`, `glm-5.3-flash`, `glm-5.3-highspeed`, `glm-5.2`; unlisted `glm-*` forward raw | GLM |
| Every `copilot/<id>` | GitHub Copilot |
| `cursor`, Cursor legacy aliases, `cursor:<id>`, `cursor-plan:<id>`, `cursor-ask:<id>` | Cursor Agent |
| Anthropic-style aliases such as `haiku`, `sonnet`, `opus`, `fable`, and registered `claude-*` aliases | The `aliasProvider`, Codex by default |

Other unknown IDs return HTTP 400 with the supported provider catalog. Unlisted provider-prefixed IDs go to their named provider, which reports unknown models. The `opencode-go/` prefix also forwards any ID to OpenCode Go with an inferred wire protocol. Arbitrary model names have no implicit fallback.

## Prefer the live catalog

The model list changes faster than documentation. Ask the installed CLI:

```sh
cc-proxy models
cc-proxy models --full
```

The compact command abbreviates Cursor's dynamic aliases. `--full` prints every alias discovered through the installed Cursor Agent catalog.

The HTTP equivalent is:

```sh
curl http://127.0.0.1:18765/v1/models
```

## Codex service tiers

Codex models have a local `-fast` form. The proxy removes the suffix and requests `service_tier: "priority"`. This also works for models discovered in the Codex CLI cache and unlisted `gpt-*` IDs.

`gpt-6-astra-ultrafast` requests `service_tier: "ultrafast"`. Astra is the only model with known ultrafast support in the proxy's tier list; catalog discovery does not add tier support. On other Codex models, including unlisted `gpt-*` IDs, `-ultrafast` is removed and requests priority instead. Use one suffix; repeated or mixed suffixes are rejected.

On `/v1/messages`, `codex.serviceTier` or `CCP_CODEX_SERVICE_TIER` wins over either suffix. Ultrafast falls back to priority when the final model is not Astra. The OpenAI-compatible routes use the suffix without the global service-tier override, and `/v1/responses` forwards an explicit `service_tier` unchanged.

## The `[1m]` hint

A trailing `[1m]` affects Claude Code's local compaction policy. The proxy strips it before matching and forwarding the model. Use it only when the upstream model and account can accept the resulting context, and set a safe `CLAUDE_CODE_AUTO_COMPACT_WINDOW` when the real limit is below one million tokens.

## Main and small models

Set both model variables to routable IDs:

```sh
ANTHROPIC_MODEL=gpt-6-sol[1m]
ANTHROPIC_DEFAULT_HAIKU_MODEL=gpt-6-luna[1m]
```

Claude Code sends background work to its small/fast model. Its built-in Haiku ID can route through `aliasProvider`, but a concrete provider ID keeps the behavior explicit.

## Claude Code `/model`

When `ANTHROPIC_BASE_URL` already points to the proxy, these can change the request model:

- `claude --model <id>` at launch
- Claude Code's `/model` command in a session
- `ANTHROPIC_MODEL` for a new process

A model change can switch upstream providers because routing is per request. It does not change `ANTHROPIC_BASE_URL` or move the process back to direct Anthropic.

## Gateway model discovery

Enable Claude Code discovery at process start:

```sh
CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1 \
ANTHROPIC_BASE_URL=http://127.0.0.1:18765 \
ANTHROPIC_AUTH_TOKEN=unused \
ANTHROPIC_MODEL=gpt-6-sol[1m] \
ANTHROPIC_DEFAULT_HAIKU_MODEL=gpt-6-luna[1m] \
  claude
```

`/v1/models` includes raw provider IDs and Anthropic-style aliases. Claude Code's gateway picker filters for IDs beginning with `claude` or `anthropic`, so configured aliases are the entries most likely to appear. Raw IDs remain usable through `--model`, `/model`, and environment variables.

## Alias routing

`CCP_ALIAS_PROVIDER=kimi` or `"aliasProvider": "kimi"` routes recognized Anthropic-style aliases to Kimi. Accepted values are `codex` and `kimi`. Explicit provider IDs always use their provider.
