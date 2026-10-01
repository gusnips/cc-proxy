---
title: Switching models and backends
description: Choose launch-time patterns for switching between cc-proxy and direct Anthropic, then switch routed models within a proxy session.
---

Claude Code binds its base URL and client auth when the process starts. A **backend switch** needs a new Claude Code process. A **model switch** can stay in the same proxy-backed session because the proxy routes each request by model ID.

| Goal | Pattern |
| --- | --- |
| Start one session on the proxy | `cc-proxy claude`, with any `claude` flags |
| Always use the proxy | Put client variables in `~/.claude/settings.json` |
| Try one model once | Prefix `claude` with environment variables or use an alias |
| Toggle between proxy and direct Anthropic | `cc-proxy shell install` once, then `cc-proxy on` / `cc-proxy off` |
| Stay on the proxy and change provider or model | Use `/model`, `--model`, or a new `ANTHROPIC_MODEL` |

## One-shot aliases

```sh
alias csol='ANTHROPIC_BASE_URL=http://127.0.0.1:18765 ANTHROPIC_AUTH_TOKEN=unused ANTHROPIC_MODEL=gpt-6-sol[1m] ANTHROPIC_DEFAULT_HAIKU_MODEL=gpt-6-luna[1m] CLAUDE_CODE_AUTO_COMPACT_WINDOW=272000 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1 claude'
alias cgrok='ANTHROPIC_BASE_URL=http://127.0.0.1:18765 ANTHROPIC_AUTH_TOKEN=unused ANTHROPIC_MODEL=grok-4.5 ANTHROPIC_DEFAULT_HAIKU_MODEL=grok-4.5 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1 claude'
```

These affect only the launched process.

## Switch plain `claude` on and off

```sh
cc-proxy shell install
```

It adds one line to the file your shell reads when a terminal opens:
`~/.zshrc` for zsh, `~/.bashrc` for bash on Linux, `~/.bash_profile` for bash
on macOS. fish gets `~/.config/fish/functions/claude.fish` instead. That line
defines a `claude` command that runs `cc-proxy claude` while cc-proxy is on,
and plain Claude Code while it's off. It sets no environment variables, so
nothing else on your machine sees the proxy.

```sh
cc-proxy off   # plain claude, in every terminal
cc-proxy on    # claude through the proxy again
```

`on` and `off` reach every terminal at once, because the `claude` command
asks cc-proxy each time it runs. A terminal that was already open when you
ran `shell install` needs `exec zsh` (or your shell's name) once. A Claude
Code session that's already open keeps its connection until you quit it.
`cc-proxy shell uninstall` removes the line and turns cc-proxy off.

## In-session model changes

With the base URL already pointing to the proxy:

```text
/model gpt-6-sol-fast[1m]
/model k3[1m]
/model grok-4.5
/model cursor:gpt-5.5
```

The provider changes with the model. Provider auth must already exist. Claude Code preserves one conversation, while provider-specific in-memory continuation can reset when the provider or model changes.

## Scope

cc-proxy does not provide a profile GUI, rewrite Claude Code settings, change base URLs in a running process, or configure Desktop and IDE launch environments. Use process environment, a wrapper, or a dedicated profile manager for those concerns.
