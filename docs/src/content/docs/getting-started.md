---
title: Getting started
description: Install cc-proxy, sign in to Codex, turn routing on, and open one working Claude Code session.
---

This path gets one Codex-backed Claude Code session working. See [Choosing a provider](/providers/choosing-a-provider/) for Kimi, Grok, OpenCode Go, and Cursor Agent.

## 1. Install

From a GitHub release:

```sh
curl -fsSL https://raw.githubusercontent.com/gusnips/cc-proxy/main/scripts/install.sh | bash
```

Or from a local checkout:

```sh
./scripts/install.sh --local
```

Windows archives and binaries for every supported platform are on the [GitHub releases page](https://github.com/gusnips/cc-proxy/releases).

Update later with `cc-proxy update` (add `--check` to only report), or by
re-running the install command and restarting the service.

## 2. Sign in to Codex

Use a **ChatGPT Plus or Pro account**, not an OpenAI API account:

```sh
cc-proxy login codex
```

For SSH or another headless environment, use `cc-proxy login codex --device` instead.

## 3. Turn routing on

```sh
cc-proxy start
```

This starts the proxy in the background and sends Claude Code to it. If you
are signed in to more than one provider, it asks which one. If you aren't
signed in to any, it asks which one to sign in to.

cc-proxy writes a few lines to the `env` block of `~/.claude/settings.json`.
That is all. There is no shell hook to install and nothing to source.

## 4. Open Claude Code

```sh
claude
```

Send a prompt. You can watch the request with `cc-proxy monitor`.

Claude Code windows that are already open follow the change within a few
seconds. Editors and new terminals work the same way.

## Switch or turn it off

```sh
cc-proxy use kimi     # another provider
cc-proxy use claude   # your Claude subscription; the proxy keeps running
cc-proxy stop         # your Claude subscription, and the proxy stops
```

Routing never comes back by itself. Run `cc-proxy start` or
`cc-proxy use <provider>` when you want it again.

## Choose a model

Claude Code asks for `opus`, `sonnet`, `haiku` and `fable`. cc-proxy gives each
name a model from the provider you picked. To see them, or to change one:

```sh
cc-proxy models
cc-proxy models set haiku gpt-6-luna
cc-proxy models choose
```

`cc-proxy models choose` asks about each name in turn. Type part of a model
name to search, or press Enter to keep a name as it is.

`claude --model <id>` with a full model id, such as `gpt-6.1-sol`, works too.

For the exact settings lines, provider switching and model discovery, continue to [Configure Claude Code](/using/configure-claude-code/) and [Models and routing](/using/models-and-routing/).
