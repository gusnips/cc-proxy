---
title: GitHub Copilot
description: Use your GitHub Copilot plan in Claude Code. Sign in, pick a copilot/ model, and see how requests are billed.
---

cc-proxy sends Copilot models to `chat/completions` on the Copilot host your plan uses, for example `https://api.individual.githubcopilot.com`.

## Sign in

You need a GitHub account with a Copilot plan.

```sh
cc-proxy copilot auth login
cc-proxy copilot auth status
```

The login prints a web address and a code. Open the address, enter the code, and approve. cc-proxy waits until you do, then saves the sign-in and reads which models your account can use.

GitHub's token does not work on the Copilot API. cc-proxy trades it for a Copilot token that lasts about 25 minutes, and trades again when it runs low. You do not need to sign in again for that. If GitHub stops accepting the saved sign-in, requests fail with a message that tells you to run `cc-proxy copilot auth login`.

The consent page says "GitHub Copilot Chat". cc-proxy uses the same client id as VS Code's Copilot Chat, because GitHub does not issue one to other tools.

## Pick a model

Every Copilot model starts with `copilot/`, so a Copilot `gpt-5.5` never collides with the Codex one.

```sh
cc-proxy claude --model copilot/gpt-5.5
```

`cc-proxy models` lists the models your account can run. The list is saved at sign-in. Until then, cc-proxy shows a short built-in list. Any other `copilot/<id>` is sent as written, and Copilot says if it does not know the id.

Models that only answer on Copilot's Responses API do not work here, because cc-proxy calls the chat endpoint.

## How requests are billed

Copilot bills one premium request each time you start a turn, and lets the agent's follow-up calls after tool results ride free. cc-proxy tells Copilot which kind each request is, using the `X-Initiator` header:

- `agent` when the last message holds only tool results.
- `user` for everything else.

## Settings

| Environment | Purpose |
| --- | --- |
| `CCP_COPILOT_BASE_URL` | Sends chat to this host instead of the one your Copilot token names. |
| `CCP_COPILOT_GITHUB_URL` | The GitHub site for sign-in. Default `https://github.com`. |
| `CCP_COPILOT_GITHUB_API_URL` | The GitHub API that issues Copilot tokens. Default `https://api.github.com`. |

These are for testing against a mock server. GitHub Enterprise Server is not supported.

## Limits

- Reasoning effort is not sent. Copilot's models use their own default.
- Count-tokens is estimated by cc-proxy, not asked from Copilot.
- Rate limits and your premium request allowance come from GitHub. A 429 reaches Claude Code with its `retry-after`.
- Copilot's terms apply to your account. Check them before you use a plan through a proxy.

The sign-in is stored in `<configuration-root>/copilot/auth.json`, next to the saved model list in `models.json`. `cc-proxy copilot auth logout` removes both.
