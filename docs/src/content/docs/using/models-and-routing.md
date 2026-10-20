---
title: Models and routing
description: Understand provider model patterns, how Claude names map to models, Codex fast mode, context hints, model listing, and Claude Code gateway discovery.
---

The model ID in each request selects its provider. One proxy listener can serve every provider through Anthropic Messages and the enabled OpenAI-compatible routes.

## Routing patterns

| Pattern                                                                                               | Provider                                                           |
| ----------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------ |
| Any `gpt-*` ID and its local tier forms (`-fast` or `-ultrafast`; unlisted base IDs forward to Codex) | Codex                                                              |
| `k3`, `kimi-k3`, `kimi-k2.6`, `k2.6` (legacy `kimi-for-coding`); unlisted `kimi-*`/`k2-*` forward raw | Kimi                                                               |
| `grok-composer-2.5-fast`, `grok-4.5`, `grok-4.6`, `grok-4.7`; unlisted `grok-*` forward raw           | Grok                                                               |
| Non-conflicting OpenCode Go IDs and **every** `opencode-go/<model-id>`, registered or not             | OpenCode Go                                                        |
| `glm-5.3`, `glm-5.3-flash`, `glm-5.3-highspeed`, `glm-5.2`; unlisted `glm-*` forward raw              | GLM                                                                |
| Every `copilot/<id>`                                                                                  | GitHub Copilot                                                     |
| `cursor`, Cursor legacy aliases, `cursor:<id>`, `cursor-plan:<id>`, `cursor-ask:<id>`                 | Cursor Agent                                                       |
| Claude names: `fable`, `opus`, `sonnet`, `haiku`, and `claude-<name>` with any version after it       | The model chosen for that name (see [Claude names](#claude-names)) |

Other unknown IDs return HTTP 400 with the supported provider catalog. Unlisted provider-prefixed IDs go to their named provider, which reports unknown models. The `opencode-go/` prefix also forwards any ID to OpenCode Go with an inferred wire protocol. Arbitrary model names have no implicit fallback.

## Claude names

Claude Code asks for four model names: `fable`, `opus`, `sonnet` and `haiku`. It also sends longer ones, such as `claude-opus-4-8` or `claude-haiku-4-5-20251001`. Each one means a tier, whatever version follows it.

While routing is on, cc-proxy turns a tier into one model of the provider you picked with `cc-proxy start` or `cc-proxy use`. It looks in this order:

1. Your choice, saved by `cc-proxy models choose` or `cc-proxy models map`.
2. A match it works out from the models the provider offers. A provider that resells other vendors' models (OpenCode Go, OpenCode Zen, and providers you add with `cc-proxy provider add`, such as OpenRouter) gets no match: you choose.
3. Neither: an HTTP 400 that names `cc-proxy models choose`.

To work out a match, cc-proxy reads one word in each model ID and keeps the newest version:

| Tier     | Words                                                           |
| -------- | --------------------------------------------------------------- |
| `fable`  | `astra`, `fable`                                                |
| `opus`   | `opus`, `sol`, `max`, `ultra`                                   |
| `sonnet` | `sonnet`, `terra`, `pro`, `plus`                                |
| `haiku`  | `haiku`, `luna`, `flash`, `mini`, `lite`, `fast`, `air`, `nano` |

Versions compare as numbers, so 6.10 is newer than 6.9. cc-proxy keeps no list of versions: a new model counts the day the provider lists it. Chat models only. Embedding, audio, image, preview and experimental IDs are skipped.

- A tier with no match borrows the nearest one: `fable` from `opus`, `opus` from `sonnet`, `sonnet` from `opus`, `haiku` from `sonnet`.
- A provider with one line of models (Kimi, GLM, Grok) uses it for every name. Its small models go to `haiku`.
- On GitHub Copilot, the Claude models answer the Claude names.

When two lines of models compete for a name, cc-proxy isn't sure. `start` and `use` then show the guesses and ask once: `Use these? [Y/n]`. Your answer is saved. If there is nobody to answer (a script), cc-proxy uses its guesses for now and saves nothing.

On a reseller, `start` and `use` ask for each name that has no model, the same way `cc-proxy models choose` does. Press Enter to skip a name. A skipped name stays empty, and the next `start` or `use` asks about it again.

An explicit provider model ID skips this tier lookup. Codex's `codex.model` override still applies. A name can point at a model of another provider.

### See or change a name

```sh
cc-proxy models choose                   # ask for each name, with search
cc-proxy models choose opencode-zen      # another provider
cc-proxy models map                      # the four names on the current provider
cc-proxy models map opus gpt-6.1-sol     # choose one name directly
cc-proxy models map opus --reset         # let cc-proxy pick again
cc-proxy models map --provider kimi      # another provider
```

`models choose` asks about each name in turn and lists the best fits first. Type part of a name to search the whole catalog: every word must match, so `claude opus` finds `openrouter/anthropic/claude-opus-4.1`. Type a number to pick from the list shown, or press Enter to keep the name as it is.

Each line says who decided: `your choice`, `picked by cc-proxy`, or `a guess, not confirmed`. A choice is checked against the models cc-proxy serves. It takes effect on the next request, with no restart. It is stored in `config.json` as `route.tiers.<provider>.<name>`.

While routing is off, a Claude name gets an HTTP 400 that says so. That window still points at the proxy. Restart it, or run `cc-proxy start`.

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

## Claude Code `/model`

When `ANTHROPIC_BASE_URL` already points to the proxy, these can change the request model:

- `claude --model <id>` at launch
- Claude Code's `/model` command in a session
- `ANTHROPIC_MODEL` for a new process

A model change can switch upstream providers because routing is per request. It does not change `ANTHROPIC_BASE_URL` or move the process back to direct Anthropic.

If you pin a model in `settings.json` (`ANTHROPIC_MODEL` or an `ANTHROPIC_DEFAULT_*_MODEL` key), Claude Code sends that exact ID. A pinned Claude name still follows the tier mapping; a provider ID selects its provider. Leave these keys out to keep Claude Code's own defaults.

## Gateway model discovery

`cc-proxy start` turns on `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY` in `settings.json` (unless you set it yourself). While routing is on, `/v1/models` starts with `claude-fable`, `claude-opus`, `claude-sonnet` and `claude-haiku`. Their display names show the provider and model, for example `codex/gpt-6.1-sol`.

For Claude Code itself, cc-proxy writes native `modelPicker` labels. Its header, `/model` picker and status-line `model.display_name` then show the model answering your requests. Status lines pick up the label on their next refresh. A picker row's description says which Claude tier it uses. Labels follow provider switches, model choices and resets. An existing session gets its label when its next regular request reaches the proxy. No restart is needed.

These are display names, not new routing IDs. `codex/gpt-6.1-sol` is a label; use `gpt-6.1-sol` in a request. The selected native ID stays unchanged, as do Claude Code's context policy and effort controls. Codex's `codex.model` override is reflected in the label. Cursor's automatic selection shows `cursor/default`, not a guessed model.

Your custom picker rows stay. A custom label for the same ID wins. If you edit the picker while routing is on, cc-proxy stops updating it and keeps your edit when routing turns off. Otherwise, `cc-proxy use claude` and `cc-proxy stop` restore your original picker.

Claude Code's `/status` view still shows the selected native ID. Managed settings or a `--settings` picker can take priority over the user file. Older clients that do not support `modelPicker` may keep their built-in names.

Raw provider IDs follow in `/v1/models` for other clients. They stay usable through `--model`, `/model` and environment variables. While routing is off, the four `claude-*` entries are left out.
