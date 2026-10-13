---
title: Switching models and backends
description: Switch Claude Code between providers and your Claude subscription with one command, and change models inside a session.
---

cc-proxy edits the `env` block of Claude Code's `settings.json`, and Claude Code reads that file again whenever it changes. So one command switches every open window and terminal. You don't restart anything.

| Goal                                                        | Command                                                                          |
| ----------------------------------------------------------- | -------------------------------------------------------------------------------- |
| Send Claude Code through the proxy                          | `cc-proxy start`                                                                 |
| Change provider                                             | `cc-proxy use kimi` (or `codex`, `cursor`, `grok`, `copilot`, `glm`, `opencode`) |
| Go back to your Claude subscription, keep the proxy running | `cc-proxy use claude`                                                            |
| Go back to your Claude subscription and stop the proxy      | `cc-proxy stop`                                                                  |
| Try one model once                                          | `claude --model <id>`                                                            |
| Change model inside a session                               | `/model`                                                                         |

`use` takes a provider only if you are signed in to it. If you aren't, it asks `Sign in now? [y/N]` and signs you in. The default is no, so a script never opens a browser.

```sh
cc-proxy use kimi
cc-proxy use claude
cc-proxy use codex
```

The proxy keeps running through all of it. Routing never comes back on its own: after `stop` or `use claude`, only `start` or `use <provider>` turns it on again.

## One-shot aliases

To launch one session on a model without changing `settings.json`:

```sh
alias csol='ANTHROPIC_BASE_URL=http://127.0.0.1:18765 CLAUDE_CODE_OAUTH_TOKEN=unused ANTHROPIC_MODEL=gpt-6-sol[1m] ANTHROPIC_DEFAULT_HAIKU_MODEL=gpt-6-luna[1m] CLAUDE_CODE_AUTO_COMPACT_WINDOW=272000 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1 claude'
alias cgrok='ANTHROPIC_BASE_URL=http://127.0.0.1:18765 CLAUDE_CODE_OAUTH_TOKEN=unused ANTHROPIC_MODEL=grok-4.5 ANTHROPIC_DEFAULT_HAIKU_MODEL=grok-4.5 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1 claude'
```

These affect only the launched process. But Claude Code lets `settings.json` beat process variables. After `cc-proxy stop` the file holds `https://api.anthropic.com`, and that wins over the alias. Run `cc-proxy start` first, or pass the connection in a `--settings` flag, which beats the file:

```sh
claude --settings '{"env":{"ANTHROPIC_BASE_URL":"http://127.0.0.1:18765","CLAUDE_CODE_OAUTH_TOKEN":"unused"}}'
```

## In-session model changes

With Claude Code already pointed at the proxy:

```text
/model gpt-6-sol-fast[1m]
/model k3[1m]
/model grok-4.5
/model cursor:gpt-5.5
```

The provider changes with the model. Provider auth must already exist. Claude Code preserves one conversation, while provider-specific in-memory continuation can reset when the provider or model changes.

## Scope

cc-proxy does not provide a profile GUI or change the base URL of a process that has no `settings.json` to read. It writes only the `env` block described in [Configure Claude Code](/using/configure-claude-code/). Hosts that skip user-level `env`, such as some desktop and SDK launches, need the variables set by hand.
