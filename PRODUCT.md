# cc-proxy

## Who it's for

Developers who use Claude Code every day and also pay for another AI coding plan: ChatGPT Plus or Pro (Codex), Kimi, Grok, Cursor, GitHub Copilot, OpenCode Go and others. They work in a terminal, mostly on macOS, and also on Linux and Windows (guess: mostly solo developers).

## What it does

A local proxy that lets Claude Code run on those plans. It starts in the background, and `cc-proxy use <provider>` points every open Claude Code window at the provider. Success: the person keeps working in Claude Code, on the model they chose, and never edits a settings file by hand.

## Why this one

It uses plans people already pay for, not API keys. One command switches provider, and open windows follow within seconds. It writes only the `env` block of Claude Code's `settings.json`, and `cc-proxy use claude` puts everything back.

## How people use it

Daily: run `claude` through the proxy. Rare: sign in, switch provider, choose models, change a setting. Source: README. How many people hold more than one account on a provider is unknown. The request for several accounts came from the author hitting a 5-hour limit (guess: common among heavy users).

## What it can and can't do

- Real: sign in with a browser or a device code, route by model name, translate streaming and tool calls, show usage and a live monitor, update itself.
- Limits: one login per provider until saved accounts ship. No fallback between different providers. Each provider controls its own plans, model access and terms; unofficial clients can carry account risk (the README says so).
- Open: which providers can safely keep more than one login, and how a limit is reported by each.

## Voice and brand

Name: `cc-proxy`, lowercase. A small face (see `src/ui.rs`) shows the state: awake, glad, asleep, unsure, hurt. Plain sentences, a command in backticks wherever there is a next step. No em dashes, no hype words.

## Proof on hand

The monitor (`cc-proxy demo` shows it with simulated traffic) and the docs in `docs/src/content/docs`. No user counts or testimonials exist, so none are written.

## Principles

- Nothing changes where Claude Code goes unless a command says so.
- A secret never prints, not in output, logs or screens.
- Every message says what to run next.
- Output is plain text when it isn't a terminal, so scripts and logs stay clean.
- A file the user wrote is never overwritten because it was hard to read.

## Accessibility

Color never carries meaning alone: a word or symbol says the same thing. `NO_COLOR` and `TERM=dumb` are honored. Every screen works with the keyboard only.
