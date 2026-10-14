---
title: Configure Claude Code
description: What cc-proxy writes to Claude Code's settings.json, how it treats values you set yourself, and how to set the same variables by hand.
---

`cc-proxy start` and `cc-proxy use <provider>` send Claude Code to the proxy through `~/.claude/settings.json`. They set the connection in `env` and the model labels in `modelPicker`. If you set `CLAUDE_CONFIG_DIR`, cc-proxy edits `settings.json` in that folder instead.

Claude Code reads this file again whenever it changes, in every open window. That is why a switch reaches terminals, editors and windows that are already running, with no shell hook and no restart.

## What cc-proxy writes

| Key in `env`                                 | Value while routing is on | Why                                                                                                                                                                                                                                                                  |
| -------------------------------------------- | ------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `ANTHROPIC_BASE_URL`                         | `http://127.0.0.1:18765`  | Sends API requests to the proxy. The port follows your config.                                                                                                                                                                                                       |
| `CLAUDE_CODE_OAUTH_TOKEN`                    | `unused`                  | Satisfies Claude Code's credential check, and keeps your real login token away from the proxy. The proxy never uses this value. Use it instead of `ANTHROPIC_AUTH_TOKEN`: with that one, Claude Code ignores usage-limit headers and can't show when a limit resets. |
| `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC`   | `1`                       | Cuts background traffic sent to the provider.                                                                                                                                                                                                                        |
| `CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK`  | `1`                       | Stops Claude Code from retrying a half-finished stream without streaming, which can run tool calls twice.                                                                                                                                                            |
| `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY` | `1`                       | Lets Claude Code's `/model` picker list `claude-opus`, `claude-sonnet`, `claude-haiku` and `claude-fable` from the proxy.                                                                                                                                            |

While routing is on, cc-proxy also writes a `modelPicker` block with provider/model labels. It replaces the built-in picker rows, keeps your custom rows and learns the exact native IDs from running sessions. It does not change `model` or any `ANTHROPIC_DEFAULT_*_MODEL` value.

cc-proxy keeps the order of your keys and the text of every value it doesn't change. If `settings.json` is a symlink, it writes to the file the link points to. If the file isn't valid JSON, `start` and `use` stop and say where the problem is. Nothing is overwritten.

## Values you set yourself

- **`ANTHROPIC_BASE_URL` and `CLAUDE_CODE_OAUTH_TOKEN`.** cc-proxy takes these over while routing is on. It saves what you had under a top-level `cc-proxy` key in the same file, and puts it back when routing turns off.
- **The three switches below them.** cc-proxy sets one only if you haven't. A value you wrote is never changed, in either direction.
- **If you edit a key by hand while routing is on,** cc-proxy sees that your value differs from what it wrote and leaves it alone when routing turns off.
- Your original `modelPicker` is saved too. Your custom labels take priority. Editing or deleting the managed picker stops label updates for that routing run. Your edit is kept.

## Turning it off

`cc-proxy stop` and `cc-proxy use claude` restore your original picker, or remove it if you had none. They don't delete the `env` keys. Claude Code adds `env` values from this file but never removes one it stops seeing, so a deleted key would leave an open window pointed at the proxy. cc-proxy writes values that point back at Anthropic instead:

- `ANTHROPIC_BASE_URL` becomes `https://api.anthropic.com`, or your saved value.
- `CLAUDE_CODE_OAUTH_TOKEN` becomes an empty string, or your saved value.

Your Claude Code login is used again right away.

If the proxy stops without a `stop` (a crash, a reboot, `kill`), the next `cc-proxy` command notices that nothing answers and does the same. Until then, new Claude Code windows can't connect.

## Which model a Claude name gets

Claude Code asks for `opus`, `sonnet`, `haiku` and `fable`, or a longer name such as `claude-opus-4-8`. cc-proxy turns each into a model of the provider you picked. See [Models and routing](/using/models-and-routing/) and `cc-proxy models map`.

If your `settings.json` pins a model (`ANTHROPIC_MODEL` or one of the `ANTHROPIC_DEFAULT_*_MODEL` keys), Claude Code sends that exact name. A provider model goes to its provider; a Claude name still follows the tier mapping. `cc-proxy status` warns about pinned names.

## Limits

- A window that was open before `settings.json` existed never sees changes. Restart it once.
- A window that started on a plain Claude login can keep request features after a switch. For Message Threads (requests that omit history already held by Anthropic), cc-proxy asks Claude Code to resend the full conversation. This recovery does not require restarting Claude Code. Other unsupported features may still need separate handling.
- A project's `.claude/settings.json` beats the user file. If a project sets its own `ANTHROPIC_BASE_URL`, that project ignores cc-proxy.
- `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY` and `apiKeyHelper` take priority over the placeholder token. Claude Code then can't show limit reset times. `cc-proxy status` warns when any of them is set.
- Claude Code and cc-proxy can write the file at the same moment. cc-proxy checks the file again right before it writes and tries once more. There is no lock.
- Only the folder `CLAUDE_CONFIG_DIR` points to is managed.
- Some hosts, such as the desktop app or an SDK, may protect or skip user-level `env`. There, set the variables yourself.

## Set the variables yourself

Use this to launch one session without `cc-proxy start`, or on a host that skips user-level `env`:

```sh
ANTHROPIC_BASE_URL=http://127.0.0.1:18765 \
CLAUDE_CODE_OAUTH_TOKEN=unused \
ANTHROPIC_MODEL=gpt-6-sol[1m] \
ANTHROPIC_DEFAULT_HAIKU_MODEL=gpt-6-luna[1m] \
CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 \
CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1 \
  claude
```

Claude Code lets `settings.json` beat process variables. If cc-proxy has written to that file (it has, once you ran `cc-proxy start`), the file's values win, and after `cc-proxy stop` that means Anthropic. Run `cc-proxy start` first, or pass the same values in `claude --settings '{"env":{...}}'`, which beats the file.

`ANTHROPIC_MODEL` picks the main model, and therefore the provider. `ANTHROPIC_DEFAULT_HAIKU_MODEL` picks the model for titles and other small requests. It replaces the older `ANTHROPIC_SMALL_FAST_MODEL`. Use models the proxy routes.

The proxy always makes streaming upstream requests. It can still accumulate a non-streaming Anthropic response when the client requests one.

## Compaction settings

A trailing `[1m]` tells Claude Code to use its larger local context policy. The proxy strips the suffix before upstream routing. It does not enlarge the provider's context window.

OpenAI's [GPT-5.6 subscription update](https://x.com/thsottiaux/status/2076495156757577895)
sets the ChatGPT context limit to 272K tokens. Set
`CLAUDE_CODE_AUTO_COMPACT_WINDOW=272000` with `gpt-5.6-sol[1m]` so Claude Code
compacts before the upstream limit. Put it in the `env` block of `settings.json`. cc-proxy never changes a key you set there.

Grok 4.5 and Grok 4.6 are 500,000 tokens. Set `CLAUDE_CODE_MAX_CONTEXT_TOKENS=500000` and omit `[1m]`.

For a provider and model with a different real context limit, choose a safe value or omit the override. `DISABLE_AUTO_COMPACT=1` disables automatic compaction while preserving manual `/compact`, but the session can then hit the upstream limit.

## Proxy settings are separate

`CCP_*`, `PORT`, and `config.json` configure the **cc-proxy server process**. They control the listener, provider endpoints, transport, credentials, and diagnostics. They do not belong in Claude Code's client environment unless the same shell also starts the proxy.

See [Configuration](/reference/configuration/) for the canonical server setting table.
