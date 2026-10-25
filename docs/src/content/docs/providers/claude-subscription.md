---
title: Claude subscription
description: Save Claude subscription accounts, see their usage and rotate when a usage window runs out.
---

Send Claude Code through the proxy using your Claude subscription:

```sh
cc-proxy use claude
```

This is separate from `anthropic`, which uses an Anthropic API key and API credits.

## Sign in and add accounts

On first use, cc-proxy copies your existing Claude Code sign-in when it can read it.
On macOS it reads the Claude Code Keychain item. Other installations can use
`~/.claude/.credentials.json`. Account names use the signed-in email when available.
The original credentials are not deleted or rewritten.

If no local sign-in is available:

```sh
cc-proxy auth login claude
```

To save another subscription without changing the current account:

```sh
cc-proxy auth add claude
```

Use `cc-proxy auth use claude` to choose a saved account. Use
`cc-proxy auth status claude` to list them. `--device` opens a manual authorization
code flow for terminals that cannot receive the browser callback.

Each account has its own credentials under the proxy's `claude` directory.
Tokens renew before they expire. Signing out of the proxy does not sign you out
of Claude Code, and an empty account list prevents automatic import from undoing
that sign-out.

Claude Code and the proxy can renew the same copied sign-in. If Claude rejects a
renewal, sign in to that saved account again with
`cc-proxy auth login claude --replace`.

## Usage and rotation

```sh
cc-proxy usage claude
```

The usage command and monitor show the remaining 5-hour and weekly allowances
for each saved account. A failed lookup is unavailable, not zero.

With `auth.autoRotate` on, a Messages request can move to another saved account
when Claude confirms an exhausted usage window and its reset time. The proxy
never switches accounts after it starts returning an answer. A short request
throttle does not become a subscription wait timer.

## Disable proxy routing

```sh
cc-proxy use direct
```

This restores your original Claude Code settings and login while leaving the
proxy running. It does not select a saved proxy account. `cc-proxy stop` restores
the same settings and also stops the proxy.

This provider uses Claude's subscription OAuth endpoints. Anthropic controls
access, usage limits and the terms for those endpoints.
