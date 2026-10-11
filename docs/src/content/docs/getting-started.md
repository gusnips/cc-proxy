---
title: Getting started
description: Install cc-proxy, authenticate with Codex, start the server, and open one working Claude Code session.
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

Short on time? `cc-proxy setup` does steps 2 to 4 for any provider: it signs you in, picks the models and says what to run.

## 2. Sign in to Codex

Use a **ChatGPT Plus or Pro account**, not an OpenAI API account:

```sh
cc-proxy codex auth login
```

For SSH or another headless environment, use `cc-proxy codex auth device` instead.

## 3. Turn it on for Claude Code

```sh
cc-proxy on
```

This installs the shell hook and turns on the proxy connection. In an
existing zsh terminal, run `exec zsh` once. New terminals load it themselves.
Use `exec bash` or `exec fish` if that's your shell. Your zsh or bash alias
keeps its flags; no Claude settings file needs editing.

## 4. Open Claude Code

```sh
claude
```

It starts the proxy in the background if needed, then opens Claude Code on
it. Send a prompt. You can see the request with `cc-proxy monitor`.
`cc-proxy off` uses your saved Claude connection again for new sessions.

On Windows, or without the shell hook, run `cc-proxy claude` instead.
`--resume`, `--worktree` and your other Claude flags work as usual.

To choose a model, use `claude --model opus` for one session or save a default:

```sh
cc-proxy config set claude.model opus
```

For persistent settings, provider switching, and model discovery, continue to [Configure Claude Code](/using/configure-claude-code/) and [Models and routing](/using/models-and-routing/).
