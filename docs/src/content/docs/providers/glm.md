---
title: GLM
description: Configure z.ai GLM authentication and models.
---

GLM uses z.ai's Anthropic-native endpoint at `https://api.z.ai/api/anthropic`.
The proxy forwards requests verbatim and pipes the Anthropic reply straight
back; no format translation is involved.

## Account and authentication

Get an API key from your [z.ai](https://z.ai) console, then either paste it
interactively or export it:

```sh
cc-proxy glm auth login
```

```sh
export CCP_GLM_API_KEY=<your z.ai API key>
cc-proxy start
```

`CCP_GLM_API_KEY` takes precedence over `GLM_API_KEY`. A key saved with
`glm auth login` is stored through the proxy's credential store and used when
no environment key is set. The proxy does not implement a GLM login flow.

## Models

Run `cc-proxy models` for the current catalog. GLM serves `glm-5.3` and
`glm-5.3-flash` under those bare IDs:

```sh
ANTHROPIC_MODEL=glm-5.3 \
ANTHROPIC_DEFAULT_HAIKU_MODEL=glm-5.3-flash \
  claude --model glm-5.3
```

The bare ID `glm-5.3` is owned by the GLM provider. OpenCode Go serves its own
`glm-5.3`; prefix the ID with `opencode-go/` to select that version.
Legacy IDs such as `glm-5.2` and `glm-4.7` still route to the GLM provider;
z.ai itself maps them onto the current models.

## Configuration

- `CCP_GLM_API_KEY`, `GLM_API_KEY`, or a stored key supplies the credential.
- `CCP_GLM_BASE_URL` or `glm.baseUrl` changes the API base URL.
