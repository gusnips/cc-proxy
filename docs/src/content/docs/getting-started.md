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

## 2. Sign in to Codex

Use a **ChatGPT Plus or Pro account**, not an OpenAI API account:

```sh
cc-proxy codex auth login
```

For SSH or another headless environment, use `cc-proxy codex auth device` instead.

## 3. Start the proxy

```sh
cc-proxy serve
```

It starts in the background and prints its pid. Confirm it with
`cc-proxy status`. It listens on `127.0.0.1:18765`. Attach the dashboard any
time with `cc-proxy monitor`, or start foreground-with-dashboard mode with
`cc-proxy serve --monitor`.

## 4. Start Claude Code

Open another terminal and run:

```sh
cc-proxy claude --model gpt-6-sol
```

It starts the proxy first if it isn't running. Every argument goes to
`claude` unchanged, so `cc-proxy claude --resume <id>` and
`cc-proxy claude -p "..."` work as usual. The proxy's address is passed in a
`--settings` flag for this session only, so `~/.claude/settings.json` stays as
it is, and plain `claude` still talks to Anthropic.

Send a prompt. The request appears in the monitor and the response streams into Claude Code.

To start every session on the same model, save it once:

```sh
cc-proxy config set claude.model gpt-6-sol
```

For persistent settings, provider switching, and model discovery, continue to [Configure Claude Code](/using/configure-claude-code/) and [Models and routing](/using/models-and-routing/).
