---
title: Changelog
description: Release notes for cc-proxy.
---

## v0.2.14 (2026-10-23)

- Codex, Kimi, Grok and Antigravity can keep more than one saved login.
  `cc-proxy auth add codex --name work` saves another account without
  switching, `cc-proxy auth use codex work` switches when you want, and
  `cc-proxy login codex --replace` signs the current account in again. Each
  account has its own credential file, so a refresh for one never rewrites
  another.
- `auth.autoRotate` (off by default) lets a request try another saved account
  when the one in use answers 429 with a provider-stated reset time. Each
  account is tried once per request, confirmed waits survive a proxy restart,
  and a stream that already started is never replayed. Antigravity model
  quotas rotate; short capacity throttles do not.
- Codex continuation, compaction and reasoning signatures are tied to the
  saved login that created them. A signature from another account, or history
  written by an older proxy version, needs a new conversation. Native
  `/v1/responses` requests that name a conversation or carry encrypted history
  are rejected because their account cannot be verified.
- `cc-proxy config` with no subcommand opens a keyboard settings list: Up and
  Down move, Left and Right change a value, Enter edits one, and a complex
  setting opens `config.json` at its line. Every change saves at once.
- `cc-proxy usage codex` and `cc-proxy usage kimi` report every saved account,
  mark the current one, and keep a failed lookup visible without switching
  accounts. Grok and Antigravity usage percentages stay unavailable.
- The monitor shows the saved account used by each request. The local
  Providers panel lists saved accounts with the current marker, provider
  usage, reset times and confirmed waits, with scroll and `r` to refresh.

## v0.2.13 (2026-10-23)

- OpenCode Go and Zen no longer fail the turn when the requested reasoning
  effort is outside what the model takes. MiMo `xhigh` and `max` run at
  `high`, and GLM `low` and `medium` run at `high`, the least each model
  offers.
- The OpenCode Zen free-limit message ("Free usage limit reached ...") now
  arrives as a limit error without retry, so the client stops instead of
  looping, with the subscribe link kept in the message.

## v0.2.12 (2026-10-22)

- Antigravity now discovers the models your account can use from Cloud Code
  Assist (`:fetchAvailableModels`), saves them to `antigravity/models.json`, and
  collapses `-low`, `-medium` and `-high` families into one picker row per
  model. Its Claude Opus and Claude Sonnet models now appear in
  `cc-proxy models choose antigravity` and Claude Code's `/model` picker.
- Antigravity's automatic Claude-name defaults now map `fable` to Claude Opus,
  `opus` to Claude Sonnet, `sonnet` to Gemini Argon (or Pro) and `haiku` to
  Gemini Flash. Request `output_config.effort` selects the matching Antigravity
  variant (`none` and `low` use `low`, `medium` uses `medium`, and `high`,
  `xhigh` and `max` use `high`).
- `cc-proxy models refresh [provider|all]` now refreshes Antigravity, GitHub
  Copilot, Gemini, OpenCode Go, OpenCode Zen and custom providers. A running
  proxy reads refreshed catalogs without a restart, and failed refreshes keep
  the previous saved list.

## v0.2.11 (2026-10-22)

- Fixed Sonnet requests routed to Gemini failing with a model-not-found error.
  Gemini 3.1 Pro is now listed as `gemini-3.1-pro-preview`, the ID Google
  serves. The existing preview filter keeps it out of automatic defaults:
  Fable, Opus and Sonnet use Gemini 4 Argon; Haiku uses Gemini 3.8 Flash.
- Gemini 3.1 Pro Preview, Gemini 4 Argon and Gemini 3.8 Flash now appear in
  Claude Code's `/model` picker. You can still choose a preview model for any
  Claude name with `cc-proxy models choose gemini`.
- Corrected the Gemini 3 Flash model ID to `gemini-3-flash-preview`.

## v0.2.10 (2026-10-20)

- Fixed `Invalid schema for function 'advisor': schema must be a JSON Schema of
'type: "object"', got 'type: null'` on `opencode-zen` (such as
  `deepseek-v4.1-flash-free`) and other providers. Anthropic server-side
  `advisor_*` tools without `input_schema` are stripped at ingress and skipped
  in tool translation, and function tool schemas without a root `type` default
  to `"type": "object"`.

## v0.2.9 (2026-10-20)

- Reading images with Claude Code's `Read` tool on `gemini` (and OpenCode's
  Gemini models or `antigravity`) no longer overflows the 1,048,576-token window
  or traps `/compact`. Image blocks inside `tool_result` now go out as Gemini
  `inlineData` parts instead of having their base64 JSON serialized into
  `functionResponse.response.result` as hundreds of thousands of text tokens.
- Gemini's "The input token count exceeds the maximum number of tokens allowed"
  400 is now classified as a context-length error and prefixed with
  `prompt is too long:` so Claude Code triggers auto-compaction.
- `cc-proxy models` is now split into single-purpose subcommands:
  - `cc-proxy models [show] [provider]` shows which model each Claude name
    (`fable`, `opus`, `sonnet`, `haiku`) uses.
  - `cc-proxy models list [provider] [--full]` lists every model a provider
    serves.
  - `cc-proxy models choose [provider]` asks for each Claude name with search.
  - `cc-proxy models set <name> <model> [--provider <provider>]` sets one name
    directly.
  - `cc-proxy models reset [name] [--provider <provider>]` clears one choice or
    all four.
  - `cc-proxy models refresh [provider]` fetches and saves the live `/models`
    list for custom providers (such as OpenRouter) and GitHub Copilot.
- `cc-proxy provider list` now marks the active provider and shows the commands
  to switch, sign in, and pick models.
- Commands that take a trailing `[provider]` (`start`, `use`, `auth login`,
  `auth device`, `auth logout`, `provider remove`, `models show`, `models list`,
  `models choose`, `models refresh`) and `models choose` itself now use an
  interactive `↑`/`↓` arrow-key selector with live type-to-filter when run in a
  terminal.

## v0.2.8 (2026-10-20)

- `gemini` no longer fails with "...parameters.properties[batch].items:
  missing field". Tool schemas now go to Gemini as standard JSON Schema
  (`parametersJsonSchema`), so a tool with an array argument that doesn't name
  its item type works. Only the few shapes Gemini rejects are rewritten.
  OpenCode's Gemini models get the same fix, and `antigravity`, which keeps
  Google's older `parameters` field, now gives such an array a string item.
- OpenCode Zen's free models work from Claude Code. Zen answers free calls only
  when they look like OpenCode's own, so cc-proxy sends Claude Code's `Read`
  tool under OpenCode's name `read`, and sends no key: Zen refuses a free call
  that carries a stale one.
- Zen's Claude models now get Claude Code's `anthropic-beta` header, as the
  `anthropic` provider does.
- New `cc-proxy models choose [provider]` picks the model each Claude name uses,
  one name at a time. Type part of a name to search, which helps on catalogs
  with hundreds of models, type a number to pick, or press Enter to keep the
  name as it is. It replaces `cc-proxy models set`.
- Resellers (OpenCode Go, OpenCode Zen, and providers added with
  `cc-proxy provider add`) no longer get guessed models. Their Claude names
  start empty, and `cc-proxy start` and `cc-proxy use` ask for each one. Enter
  skips a name.
- A provider with more than 100 models no longer fills Claude Code's `/model`
  picker. It shows the four Claude names instead.

## v0.2.7 (2026-10-20)

- Claude Code routed to `anthropic` no longer fails with "context_management:
  Extra inputs are not permitted". cc-proxy now sends Claude Code's
  `anthropic-beta` header on to Anthropic, so context editing, the advisor tool
  and newer `thinking.display` values work. OAuth betas stay behind, since
  cc-proxy signs in to Anthropic with your API key.
- An API key saved while the proxy runs now works on the next request, with no
  restart. This covers `anthropic`, `opencode`, `opencode-zen` and custom
  providers, including the key `cc-proxy start <provider>` asks for.
- A missing key now names the right provider and the command that adds one,
  for example "Anthropic has no API key. Run `cc-proxy auth login anthropic` to
  add one." Before, every provider said "OpenCode Go API key is not
  configured".
- `cc-proxy restart` and `cc-proxy reload` now say where Claude Code goes, the
  same way `cc-proxy status` does.

## v0.2.6 (2026-10-20)

- New `antigravity` provider for Google Antigravity (Cloud Code Assist) OAuth
  (`cc-proxy auth login antigravity`), supporting `gemini-4-argon`,
  `gemini-3.8-flash`, `gemini-3.7-flash`, `gemini-3.1-pro`, `claude-opus-5-5`,
  and `claude-sonnet-5-5` with automatic project discovery and per-model client
  fingerprints.
- `gemini` API-key requests now send only `x-goog-api-key` (omitting
  `Authorization: Bearer`), sanitize tool JSON schemas to Google's supported
  keyword allowlist, and strip terminal SGR mouse escape sequences from pasted
  API keys so keys with terminal noise no longer fail with HTTP 502.
- `opencode-zen` now defaults Claude's four model tiers (`fable`, `opus`,
  `sonnet`, `haiku`) to its Claude models instead of GPT, and populates all
  catalog models into Claude Code's `/model` picker for aggregator providers.
- `cc-proxy models` now supports `list [provider]`, interactive `set [provider]`,
  `refresh [provider]`, and interactive single-tier picking via
  `cc-proxy models map <tier>`.
- `cc-proxy auth login` for `opencode`, `opencode-zen`, `gemini`, and
  `anthropic` now opens the provider's API key page in the browser.

## v0.2.5 (2026-10-20)

- CLI commands now follow a consistent `cc-proxy <domain> [action] [choice]`
  order:
  - Provider authentication moves under
    `cc-proxy auth [login|device|status|logout] [provider] [--device]`.
    Running `cc-proxy auth` or `cc-proxy auth status` shows the sign-in status
    for all providers at once.
  - Background service controls live under
    `cc-proxy service [start|stop|status|restart|reload]`, while top-level
    `start`, `stop`, `status`, `restart`, and `reload` remain as shortcuts.
  - `cc-proxy start [provider]` (and `cc-proxy service start [provider]`)
    accepts an optional provider target directly.
- Custom provider endpoints can now be added with
  `cc-proxy provider add <name> <base-url> [--format openai|responses|anthropic] [--model <id>]`
  and managed with `cc-proxy provider list` and `cc-proxy provider remove`.
- New providers:
  - `anthropic`: direct Anthropic API key routing (`CCP_ANTHROPIC_API_KEY`,
    `ANTHROPIC_API_KEY`, or `cc-proxy auth login anthropic`).
  - `opencode-zen`: OpenCode Zen pay-as-you-go and free models alongside
    `opencode` (`opencode-go`), including Google `v1beta` model support,
    OpenCode CLI headers and `ses_` session affinity IDs, and Console device
    OAuth (`cc-proxy auth login opencode-zen --device`).
  - `gemini`: native Google Gemini provider supporting `GEMINI_API_KEY` /
    `GOOGLE_API_KEY`, stored keys, and `~/.gemini/oauth_creds.json` import.

## v0.2.4 (2026-10-18)

- Codex requests stopped being throttled to one every eight seconds for the rest
  of the proxy's life. The websocket connect gate counted its successes with
  `try_lock` on the same mutex it holds while a request sleeps in the gate, so
  under load every success was dropped and the spacing never came back down. One
  refused upgrade was enough to pin it at the eight-second cap: measured on a
  machine with 70 requests waiting, the median time to the first answer was 235
  seconds and Claude Code gave up at six minutes with
  `Request future ended before completion`. The gate now keeps its state in a
  mutex it never holds across the wait, so successes count again and the spacing
  relaxes back to zero once the origin accepts connections.
- `proxy.log` now says when that spacing changes, with the previous and new value
  in milliseconds, under `codex_websocket_connect_spacing_widened` and
  `codex_websocket_connect_spacing_relaxed`. A refused upgrade used to widen the
  spacing without a trace.

## v0.2.3 (2026-10-18)

- Codex now retries failed HTTP sends that previously stopped Claude Code with
  a generic network error. It uses up to three retries with randomized
  exponential backoff, within a 60-second recovery budget.
- Buffered and streaming requests use the same recovery limit. A replacement
  connection that stays silent cannot keep extending that limit. Slow headers
  on the initial request still do not trigger a replay.
- Retry logs include the request ID, next delay and attempt limit. Connection
  logs retain the underlying cause without the request URL. Temporary HTTP
  408, 409 and 425 responses now use the shared retry policy too.
- Committed output, tool calls and spent usage limits keep their existing
  no-replay safeguards. Message Threads 400s still tell Claude Code to resend
  the full conversation; they are separate from network failures.

## v0.2.2 (2026-10-14)

- Claude Code's header, `/model` picker and status-line model name now show
  the provider and model answering your requests, such as `codex/gpt-6.1-sol`.
  Labels follow provider switches and model-map changes without a restart.
  The native model ID, context policy and effort controls stay unchanged.
  `/status` still shows the native ID.
- Routing saves and restores your original `modelPicker` settings. Your
  custom labels win, and edits you make while routing is on are kept.
  A label update that fails does not fail the model request.

## v0.2.1 (2026-10-13)

- Switching an existing Claude Code session to the proxy no longer silently
  accepts a request with only its newest messages. Claude Code's Message
  Threads expect Anthropic to retain earlier history, which cc-proxy does
  not have. The proxy now rejects both thread creation and continuation
  with `thread_unsupported_request`, so Claude Code resends the full
  conversation, system prompt and tools without restarting the session.
  This also prevents history loss on the tool-result turn after a switch.

## v0.2.0 (2026-10-13)

This release replaces the shell hook with a setting that Claude Code reads
everywhere. Some commands and config keys are gone. Read "Upgrading" below.

- `cc-proxy start` now sends Claude Code through the proxy. It writes a few
  lines to the `env` block of `~/.claude/settings.json` (or the folder
  `CLAUDE_CONFIG_DIR` names). Claude Code reads that file again whenever it
  changes, so windows that are already open follow within a few seconds,
  and so do editors and new terminals. There is nothing to source and no
  `exec zsh`.
- `start` asks which provider when you are signed in to more than one, and
  which one to sign in to when you aren't signed in to any. "Claude Code
  subscription, no routing" is always on the list.
- `cc-proxy use <provider>` switches provider with no restart. `use claude`
  goes back to your Claude subscription and leaves the proxy running. All
  seven providers work: Codex, Kimi, Cursor, Grok, GitHub Copilot, GLM and
  OpenCode Go. `use glm` offers to sign in and reads the API key.
- `cc-proxy stop` switches Claude Code back first, even when the proxy is
  already dead, then stops it. Routing never comes back by itself. Before,
  the next `claude` started the proxy again.
- Switching off writes values that point back at Anthropic. It doesn't delete
  the keys, because Claude Code never unsets a key it stops seeing.
- Your own values are kept. `ANTHROPIC_BASE_URL` and
  `CLAUDE_CODE_OAUTH_TOKEN` are saved under a `cc-proxy` key in the same file
  and put back when routing turns off. A tuning switch you already set is
  never touched. Key order, formatting and symlinks are kept. A file that
  isn't valid JSON is never overwritten: `start` stops and names the problem.
- If the proxy dies while routing is on (a crash, a reboot), the next
  cc-proxy command notices, switches routing off and says so.
- Claude names now follow the provider you picked. `opus`, `sonnet`, `haiku`,
  `fable` and `claude-<name>` with any version map to a model that provider
  offers today. cc-proxy reads the family word in each model ID (`sol`,
  `terra`, `luna`, `flash`, ...) and takes the newest version, comparing
  versions as numbers. It keeps no list of versions, so a new model counts
  the day it is listed. Codex uses the same rules instead of its own copy.
- When it can't tell which model a name should get, for example on OpenCode
  Go, `start` shows its guesses and asks once. Your answer is saved.
- `cc-proxy models map` shows which model each name uses and who decided it.
  `cc-proxy models map opus gpt-6.1-sol` chooses one, and `--reset` undoes
  it. It applies on the next request.
- `/v1/models` starts with `claude-fable`, `claude-opus`, `claude-sonnet` and
  `claude-haiku` while routing is on, so Claude Code's `/model` picker lists
  them.
- A missing or unreadable `config.json` now means routing is off. Before,
  plain `claude` used the proxy in that case.
- `/healthz` reports the proxy's version. `start`, `use` and `status` warn
  when the running proxy is older than the installed binary, as it is after
  `brew upgrade` until it restarts.
- `start --no-monitor`, which Homebrew and systemd run, only serves. It never
  touches `settings.json`, so a reboot never routes anything.
- `status` warns when your settings pin a model, or hold an API key that
  hides the limit reset time.

### Upgrading

- Run `cc-proxy start`. It removes the old shell hook from your startup file.
  Then run `unset -f claude` or open a new terminal, and use `claude` as
  usual.
- Removed commands: `cc-proxy claude`, `on`, `off`, `shell install`,
  `shell uninstall` and `setup`. `cc-proxy claude` still runs, but only to
  remove the hook and say what to do.
- Removed config keys: `claude.enabled`, `claude.model`, `claude.fastModel`,
  `aliasProvider` and the variable `CCP_ALIAS_PROVIDER`. Use `cc-proxy use`
  to pick the provider and `cc-proxy models map` to pick a model for a name.
- Claude Code windows that are already open keep the old connection until you
  restart them once. After that they follow every switch.
- After `brew upgrade`, run `brew services restart cc-proxy` so the proxy
  runs the new version.

## v0.1.56 (2026-10-12)

- When a Codex usage window runs out, Claude Code now says so and shows when
  it resets, for example `You've hit your session limit · resets 4:52pm`.
  It can then wait for the reset and continue the task. Before, it printed
  `Request rejected (429)` and no time. `cc-proxy claude` and the shell hook
  used `ANTHROPIC_AUTH_TOKEN`, and in that mode Claude Code ignores usage
  limit headers. They now use `CLAUDE_CODE_OAUTH_TOKEN=unused`. If you set
  the variables yourself, switch the same way. The proxy still doesn't use
  the value.
- Codex doesn't always say which window ran out. The proxy used to send the
  reset time alone, and Claude Code read that as a server throttle
  (`not your usage limit`). It now marks the 429 as a spent limit, so you
  see `You've hit your limit · resets 4:57pm`.
- Claude Code no longer retries a 429 by itself in these sessions. For
  Codex, the proxy still retries a short 429 up to three times before it
  answers.

## v0.1.55 (2026-10-11)

- `cc-proxy on` now installs or refreshes the shell hook itself. Existing
  zsh and bash aliases keep their flags, including
  `--dangerously-skip-permissions`, without breaking the hook. Run
  `exec zsh` (or your shell's name) once in an existing terminal after
  setup. New terminals load it themselves. No Claude settings file changes.
- Your own `--settings` JSON or file no longer replaces the proxy
  connection. cc-proxy combines them into one flag, keeping your other
  settings and arguments. `off` uses your saved connection again; it
  doesn't remove proxy values you previously saved by hand.
- Codex tool results without a preceding, unanswered tool call are kept as
  labeled conversation context, including their text, supported images and
  error markers. They no longer become invalid function outputs that cause
  `400 No tool call found for function call output`.
- The v0.1.54 note blamed Codex for those 400s. The new logs show the proxy
  sent results without their calls. The ineffective identical-request retry
  is removed; the content-free pairing diagnostics stay.
- Codex now defaults to `auto`: WebSocket first, with HTTP fallback if
  connection setup fails before a request is sent. A rejected upgrade,
  including a 403, no longer ends the request when HTTP works. Explicit
  `websocket` and `http` choices still apply.
- `Esc` closes the monitor's top panel or detail view. On the main screen,
  it closes an attached monitor without stopping the proxy. In
  `start --monitor`, it asks before stopping the server instead. The setup
  panel now measures wrapped file paths so they don't hide the commands.

## v0.1.54 (2026-10-11)

- Codex sometimes answers a tool result with `400 No tool call found for
function call output`, even though the request holds the matching tool
  call. cc-proxy now sends that request again once. If Codex refuses it a
  second time, you get the same 400 as before. This is a workaround: the
  cause is still unknown, and it looks like it is on the Codex side.
- Each time it happens, `proxy.log` gets a `codex_orphan_tool_output` line
  with the call id and how the request was laid out (item counts, where the
  call and its output sit, the kind of the last items). It holds no message
  text and no tool output. Attach it to a report if the 400 keeps showing up.

## v0.1.53 (2026-10-11)

- Claude Code's newer models, such as `claude-sonnet-5-5`, no longer fail
  with `400 Unknown model`. cc-proxy reads the family in the name (`haiku`,
  `sonnet`, `opus` or `fable`) and ignores the version, so the next Claude
  release works the day it ships. Before, each version had to be added to a
  list first.
- Each Claude family now goes to the OpenAI family with the same role:
  `fable` to astra, `opus` to sol, `sonnet` to terra, `haiku` to luna. Codex
  picks the newest version of that family it knows about, from its built-in
  list and the Codex CLI's model cache. With the built-in list alone, `opus`
  now uses `gpt-6.1-sol` (it used `gpt-6-sol`) and `fable` uses
  `gpt-6-astra` (it used `gpt-6-sol`). A `codex.model` setting still wins.
- A web search on a luna model now runs on the sol of the same version, so
  a newer luna no longer fails with a 404.
- Claude Code's Bash security-review requests go to the same luna model as
  `haiku`, the newest one, instead of the fixed `gpt-6-luna`. Set
  `autoReviewModel` to pin another model.

## v0.1.52 (2026-10-02)

- `cc-proxy start` is now the name of the command that starts the proxy.
  `cc-proxy serve` still works and does the same thing, so existing service
  units and scripts keep running. The docs, the install script and every hint
  the program prints now say `start`.
- `cc-proxy update` and `cc-proxy config set` now answer with the same face
  card as `start` and `stop`. `update` shows the animation while it
  downloads, says what version you moved from, and says so if the new
  binary installed but the background proxy did not restart.

## v0.1.51 (2026-10-01)

- On Windows, cc-proxy now keeps its config under `%APPDATA%\cc-proxy` and
  its state under `%LOCALAPPDATA%\cc-proxy`, as the docs say. It had been
  using `~/.config` there. A config folder already at `~/.config/cc-proxy`
  stays in use until `%APPDATA%\cc-proxy` exists, so existing logins keep
  working. On macOS the config folder is always `~/.config/cc-proxy`, even
  when `XDG_CONFIG_HOME` is set.
- `cc-proxy serve` waits up to 30 seconds for the proxy to answer, up from
  15, so a busy machine doesn't kill a start that would have worked. A
  timeout now says what happened and what to try.
- `cc-proxy models` no longer says "0 cursor model aliases". It shows that
  `cursor:<model>` takes any model Cursor offers.
- New provider: GitHub Copilot. Run `cc-proxy copilot auth login`, approve the
  code on GitHub, then start Claude Code with `cc-proxy claude --model
copilot/gpt-5.5`. `cc-proxy models` lists the models your account can use,
  and every one starts with `copilot/`. cc-proxy marks a turn the agent
  continues after a tool result as `agent`, so Copilot bills a premium
  request only when you start a turn.
- A Copilot context overflow ("prompt token count of … exceeds the limit")
  now reaches Claude Code as "prompt is too long", so it compacts.
- `cc-proxy setup` walks you through a first run. You pick a provider, sign
  in (or paste an API key), pick the main model and the fast model, and
  choose whether to add the shell hook. At the end it tells you what to run.
  For GLM and OpenCode Go it checks the key first: a key the provider
  refuses isn't saved, and a provider that can't be reached doesn't stop
  you.
- The monitor has a Providers overlay on `p`. For each provider it shows
  whether cc-proxy has a sign-in or an API key, and for Codex, Kimi and
  OpenCode Go how much of the plan is used. It looks again once a minute.
- `cc-proxy codex auth login` now opens the sign-in page in your browser.
  `cc-proxy glm auth login` no longer shows the key as you paste it.

## v0.1.50 (2026-10-01)

- Commands look better in a terminal. `serve`, `stop`, `restart`, `reload`
  and `status` print a small card with the cc-proxy face, which shows whether
  the proxy runs, sleeps or needs a look. While a command waits, the face
  looks around until it's done. Errors print in red with their cause.
  Piped output, `NO_COLOR=1` and `TERM=dumb` get the same words as plain
  text.
- `cc-proxy status` also shows how long the proxy has run and whether plain
  `claude` goes through it.
- The plain text of the lifecycle commands changed: the first line names
  cc-proxy and the details follow on their own lines (`cc-proxy started`,
  then the address and pid), instead of `proxy started (pid …)`.
- `cc-proxy usage` draws a bar for each window in a terminal. It turns
  yellow at 70% used and red at 90%.
- The monitor's header shows the face too. It talks while requests stream,
  looks unsure for 10 seconds after a failed request, and blinks now and
  then so you can tell the dashboard is live. Beside it are four minutes of
  output tokens and the live tokens per second. A request that just
  finished glows for three seconds, every provider has its own color, and
  each empty panel says what shows up there.

## v0.1.49 (2026-10-01)

- Kimi now signs in at `auth.kimi.ai` and calls `api.kimi.ai`, where Kimi
  moved. Both hosts share one sign-in, so an existing login keeps working
  with no new `cc-proxy kimi auth login`. `kimi.oauthHost` and
  `kimi.baseUrl` still override them.
- `cc-proxy shell install` makes plain `claude` go through cc-proxy. It adds
  one line to `~/.zshrc`, `~/.bashrc` or `~/.bash_profile`, or a function
  file for fish. After that, `cc-proxy off` and `cc-proxy on` switch every
  terminal at once, and `claude --resume <id>` keeps working. The line sets
  no environment variables, so other programs never see the proxy.
  `cc-proxy shell uninstall` removes it.
- `cc-proxy claude` starts Claude Code on the proxy. It starts the proxy
  first if it isn't running, then passes every argument to `claude`, so
  `--resume`, `--worktree`, `-p` and a pasted resume command work as usual.
  The proxy's address goes in a `--settings` flag for that session, so no
  settings file changes. Choose the model it starts on with
  `cc-proxy config set claude.model <id>`, and the background model with
  `claude.fastModel`.
- The serve banner and the monitor's setup panel now point to
  `cc-proxy claude`. Their variable lists use `ANTHROPIC_DEFAULT_HAIKU_MODEL`,
  which replaced `ANTHROPIC_SMALL_FAST_MODEL` in Claude Code, and add
  `CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1`.
- `cc-proxy usage` shows how much of your Codex, Kimi and OpenCode Go plans
  you have used, and when each limit resets. Name one provider to see only
  that one: `cc-proxy usage kimi`. Add `--json` for scripts.
- `cc-proxy opencode usage` moved to `cc-proxy usage opencode`. The old
  command is gone. With `--json`, OpenCode Go's reply now sits under an
  `opencode` key.
- `cc-proxy models` no longer lists five Codex models that ChatGPT
  accounts can't use: gpt-5.2, gpt-5.3-codex, gpt-5.3-codex-spark, gpt-5.4
  and gpt-5.4-mini. The backend answers each with a 400. On the OpenAI
  routes they now fail at once with the list of models that work.
- GLM and OpenCode Go's Anthropic models (MiniMax, Qwen) now send Claude
  Code only whole stream events. When an upstream read ended halfway through
  an event and an error came next, the error was stuck onto the half event,
  so Claude Code could read neither, and text from the same read was lost.
- OpenCode Go's GPT, Grok and Muse Spark models now pass on what failed when
  the upstream fails mid-answer. A rate limit used to reach Claude Code as a
  generic "stream is invalid" error, without the upstream's message, and the
  text that came just before it was dropped.
- Cursor now tells a spent quota from a short rate limit. Both arrive as a
  429, so Claude Code kept retrying a spent quota, which a few seconds of
  waiting can't fix. A spent quota now comes back with `x-should-retry: false`,
  which stops the retries, whether Cursor reports it as an HTTP error or at
  the end of its answer.
- A Cursor 429 now passes on Cursor's reason and its own `retry-after`.
  Before, Claude Code got "Cursor upstream error" and a 5-second wait that
  Cursor never sent.
- A finished answer no longer fails when an upstream read ends halfway
  through what comes after the end of the answer, such as `data: [DONE]` or
  OpenCode Go's closing metadata. This affected Kimi, GLM and OpenCode Go.
  Claude Code got an error instead of the answer it had already been sent.

## v0.1.48 (2026-10-01)

- Upstream errors keep their meaning on Kimi, GLM, OpenCode Go and Grok. A
  prompt that is too long now returns a 400 that says "prompt is too long",
  so Claude Code compacts. A spent balance or plan quota returns a 429 that
  Claude Code does not retry. A short rate limit returns a 429 with the
  provider's own `Retry-After`. An overloaded provider returns a 529.
- The same applies to an error that arrives in the middle of a stream. It
  used to come through as a generic `api_error`, or a 502 on the OpenAI
  routes.
- Codex answers cut short by length or by a content filter now end with
  `stop_reason` `max_tokens` or `refusal`, with their usage. They used to
  fail with a 503 and run again from the start.
- Turning thinking off now works. Codex and Grok get reasoning effort
  `none`. Kimi and OpenCode Go's GLM, DeepSeek and MiMo models get their
  lowest effort, because the proxy has no way to turn their thinking off.
  The OpenAI routes accept `reasoning_effort: "none"` for every provider.
- A Codex quota wall now reports when the spent window resets. With the
  weekly window spent, it used to report the 5-hour reset.
- Retries wait a random time up to the backoff step, read `Retry-After` as
  a date as well as seconds, and retry a 529. Kimi no longer retries a
  spent balance, or a 429 that asks for longer than 30 seconds.
- A Kimi stream that is cut off, has a frame that is not JSON, or has a
  tool call without an id or name now fails with a 502. It used to come
  back as a finished answer.
- GLM answers now stream as GLM writes them. They used to arrive all at
  once, after the whole answer was done.
- A GLM answer that broke off partway used to come back empty, as if it had
  finished. Every answer that took more than 5 minutes did this. It now ends
  with an error, and so does a GLM stream that closes before its last event.
  The 5-minute limit now counts only time in which GLM sends nothing.
- Grok no longer fails a turn on a stream event type it does not know.
- On the OpenAI routes, `prompt_tokens` now counts cache writes, Kimi and
  OpenCode Go's chat models report their reasoning tokens, and a refusal
  reads `content_filter`.
- Cursor now reads the result of each tool call. Before, the reply after a
  tool call was rebuilt from text Cursor wrote before it had the result, so
  the answer ignored the result, and a second tool call in the same turn was
  lost. Now the result goes back to Cursor with the rest of the conversation,
  and Cursor answers from it.
- Cursor answers no longer stop early. A pause of 5 seconds used to end the
  answer and mark it finished, so a long think came back cut off. Now an
  answer ends only when Cursor says it is done. If Cursor sends nothing for 60
  seconds, or closes the connection before it is done, the request fails
  with an error that Claude Code retries.
- Kimi replies now stream as they are written instead of arriving all at
  once. A reply that takes longer than 120 seconds no longer fails; Kimi
  now fails only after 120 seconds with no data at all.
- When Kimi rejects the login token, the proxy now refreshes it and tries
  once more. It used to return the 401.
- Kimi tool call ids are now unique to each reply, as OpenCode Go's already
  are.
- A turn that calls a tool now ends with `tool_use` on OpenCode Go's chat
  models even when the model reports `stop`, as Kimi already did.
- When a content filter stops a reply on Kimi or on OpenCode Go's chat
  models, the reply now ends with `stop_reason` `refusal`, as on Codex and
  on OpenCode Go's GPT models. On OpenCode Go it used to fail with an error,
  and a retry hit the same filter. On Kimi it used to end as a normal answer.

## v0.1.47 (2026-09-30)

- Codex streams that drop mid-response now recover more safely. HTTP streams
  retry a partial tool call only if no tool call has reached Claude Code yet.
  A WebSocket stream reconnects only while it has sent thinking and nothing
  else. Recovery stops after 3 retries or 60 seconds from the first failure,
  whichever comes first. A request that Claude Code cancels is never retried.
- A Codex quota snapshot now returns a 429 only when the stream closes before
  the response starts. Snapshots covered by credits, and snapshots on streams
  that already started, no longer end the request.
- `gpt-6-astra-ultrafast` requests Codex's `ultrafast` service tier. On other
  models, `-ultrafast` falls back to `priority`. Stacked suffixes such as
  `-fast-ultrafast` are rejected.
- `autoReviewEffort` / `CCP_AUTO_REVIEW_EFFORT` sets the reasoning effort for
  Codex auto-review requests only. Normal requests keep their own effort.
- Grok hosted web search sends `allowed_domains` or `blocked_domains` (up to
  five) and an approximate `user_location` as Grok's native search filters.

## v0.1.43 to v0.1.46 (2026-09-30)

- Fork renamed to `cc-proxy` (`gusnips/cc-proxy`): binary, config and state
  directories, install script, release archives, and docs. An existing
  `claude-code-proxy` config is read as a fallback; macOS Keychain entries
  are unchanged so logins survive the rename.
- `opencode-go/<model>` IDs always route to OpenCode Go, even unknown ones;
  new model families resolve to the right wire protocol until
  `scripts/refresh-opencode-models.py` picks them up.
- New GLM (z.ai) provider via `cc-proxy glm auth login`.
- Codex quota handling: exhausted quota surfaces as a clean 429 with
  `Retry-After` instead of a 502 retry storm, quota walls fast-fail without
  backoff, and reasoning-token usage is reported.
- Codex translation: `fullLane` escape for sol/terra, optional reasoning
  signatures, compaction over HTTP, and a larger model allowlist from the
  CLI cache.
- Auth robustness: Kimi peer-rotated credentials, persistent browser login,
  and `opencode usage` account limits.
- `serve` now starts a background service tracked in a pidfile, with
  `status`, `stop`, `restart`, and `reload`; `monitor` attaches to it.
  `serve --monitor` / `--no-monitor` keep the foreground modes.
- `cc-proxy update` replaces the installed binary with the latest release
  (checksum-verified, service restarted when running).
- `cc-proxy config get/set/list/edit` reads and writes config.json through
  dotted keys, with secrets reported as set/unset only.
- `cc-proxy opencode auth login` stores the API key with hidden input;
  a key in Claude Code settings is used as a last resort.

## v0.1.42 (2026-09-23)

- Codex users can select GPT-6 Sol and GPT-6 Luna, including their `-fast`
  priority-tier variants.
  ([#165](https://github.com/raine/claude-code-proxy/pull/165))
- Codex now defaults to GPT-6 Sol. Opus and Fable aliases use GPT-6 Sol,
  Haiku aliases use GPT-6 Luna, and the Claude Opus 5.5 alias is supported.
  Sonnet remains on GPT-5.6 Terra.
  ([#165](https://github.com/raine/claude-code-proxy/pull/165))
- Grok users can select Grok 4.7 with `grok-4.7`.
  ([#162](https://github.com/raine/claude-code-proxy/pull/162))

## v0.1.41 (2026-09-19)

- Long Codex requests on the HTTP transport no longer hang for minutes and then
  fail: the proxy waits five minutes for the response headers instead of one,
  configurable with `CCP_CODEX_HEADER_TIMEOUT_MS` or `codex.headerTimeoutMs`,
  and a timeout there fails the request once rather than re-sending it.
  ([#160](https://github.com/raine/claude-code-proxy/pull/160))
- The installer correctly reports the existing version during upgrades instead
  of showing it as unknown.
  ([#157](https://github.com/raine/claude-code-proxy/issues/157))
- Codex requests use the current session header for cache affinity, improving
  compatibility with intermediaries that reject the legacy header.
  ([#156](https://github.com/raine/claude-code-proxy/pull/156))

## v0.1.40 (2026-09-14)

- Attach one or more monitor dashboards to a background proxy with
  `claude-code-proxy monitor`; dashboards reconnect automatically and detach
  without stopping the service.
  ([#134](https://github.com/raine/claude-code-proxy/pull/134))
- Monitor activity graphs remain accurate across machines with different clocks,
  and long-running proxies no longer accumulate unbounded monitor history.
  ([#134](https://github.com/raine/claude-code-proxy/pull/134))
- Exhausted Codex subscription limits now fail immediately instead of retrying
  for minutes, while preserving reset details for clients.
  ([#139](https://github.com/raine/claude-code-proxy/pull/139))
- HTTP connections honor the platform trust store, `SSL_CERT_FILE`, and
  `SSL_CERT_DIR`, enabling private certificate authorities and TLS-inspecting
  proxies. ([#143](https://github.com/raine/claude-code-proxy/pull/143))
- Codex compaction requests that omit a reasoning effort now use the configured
  compaction effort cap, reducing unnecessary latency and token usage.
  ([#151](https://github.com/raine/claude-code-proxy/pull/151))
- Image-heavy Claude Code sessions can send Anthropic-compatible requests up to
  64 MiB instead of becoming unusable after crossing the previous 16 MiB limit.
  ([#152](https://github.com/raine/claude-code-proxy/pull/152))
- Structured traffic captures redact replayable Codex compaction and reasoning
  data, reducing sensitive capture contents and file size.
  ([#153](https://github.com/raine/claude-code-proxy/pull/153))
- Grok streams show estimated input usage from the start and use the provider's
  exact total when available, so Claude Code's status bar no longer stays at
  zero input tokens. Grok 4.5 and 4.6 users can also configure Claude Code for
  their 500K context window.
  ([#154](https://github.com/raine/claude-code-proxy/pull/154))

## v0.1.39 (2026-09-10)

- OpenCode Go users can select 14 additional models, including Grok 4.6, GLM 5.3,
  GLM 5.3 Flash, LongCat 2.0, and Qwen 3.8 Flash. Use `opencode-go/grok-4.6`
  to select Grok through OpenCode Go.
  ([#145](https://github.com/raine/claude-code-proxy/pull/145))
- Fix OpenCode Go responses failing when the provider sends a harmless keepalive
  after completion. ([#145](https://github.com/raine/claude-code-proxy/pull/145))
- OpenCode Go now reports malformed response endings and late connection failures
  instead of marking affected streamed responses as successful.

## v0.1.38 (2026-09-09)

- Fix requests failing with an invalid Artifact tool schema in Claude Code 2.1.265+
  when using Codex. ([#141](https://github.com/raine/claude-code-proxy/issues/141),
  [#142](https://github.com/raine/claude-code-proxy/issues/142))

## v0.1.37 (2026-09-08)

- OpenCode Go requests work again instead of failing with a missing session header
  error. ([#137](https://github.com/raine/claude-code-proxy/issues/137),
  [#138](https://github.com/raine/claude-code-proxy/pull/138))

## v0.1.36 (2026-09-06)

- Codex users can select GPT-6 Astra with `gpt-6-astra` or its priority-tier
  `gpt-6-astra-fast` alias.
  ([#129](https://github.com/raine/claude-code-proxy/pull/129))
- Codex conversation continuation stays active after tool calls, avoiding
  unnecessary full-history uploads and reconnects when continuation is enabled.
  ([#118](https://github.com/raine/claude-code-proxy/issues/118),
  [#119](https://github.com/raine/claude-code-proxy/pull/119))
- Claude Code responses include request IDs, including on errors, so transcript
  tools can avoid double-counting usage and failed requests are easier to trace.
  ([#104](https://github.com/raine/claude-code-proxy/issues/104),
  [#105](https://github.com/raine/claude-code-proxy/pull/105))
- Nix builds avoid dependency download failures caused by crates.io API rate limits.

## v0.1.52 (2026-10-02)

- Codex encrypted reasoning continuation signatures can now be disabled with
  `codex.reasoningSignatures` / `CCP_CODEX_REASONING_SIGNATURES`, reducing
  transcript growth in tool-heavy Claude Code sessions.
- Codex WebSocket connection pacing is adaptive: connections start without
  spacing while the origin accepts upgrades, widen after a rejected upgrade,
  and relax again after sustained success. A fixed 1s spacing previously
  capped each process near one generation per second. Set a floor with
  `CCP_CODEX_WS_CONNECT_SPACING_MS` to restore fixed pacing.
- Codex standalone searches keep stable per-Agent sessions without colliding
  with sibling Agents that share a Claude Code session, and align all upstream
  search identity headers with the request body owner.

## v0.1.35 (2026-08-19)

- Grok web search works reliably with Claude Code, preserves other tools, and
  renders results across clients.
  ([#112](https://github.com/raine/claude-code-proxy/pull/112))
- Grok honors the requested reasoning effort on OpenAI-compatible routes.
- Codex and OpenCode Go streams handle connection failures, rate-limit updates,
  and output limits more reliably.
  ([#103](https://github.com/raine/claude-code-proxy/pull/103))

## v0.1.34 (2026-08-12)

- Grok users can select Grok 4.6 with the `grok-4.6` model name.

## v0.1.33 (2026-08-11)

- OpenCode Go users can select GLM 5, Kimi K2.5, Qwen 3.8 Max, and Qwen 3.5
  Plus. ([#102](https://github.com/raine/claude-code-proxy/pull/102))
- Codex WebSocket streams stay connected during long quiet responses and retry
  automatically when keepalive traffic fails.
- Codex connection failures preserve their specific error messages, making
  transport problems easier to diagnose.
  ([#100](https://github.com/raine/claude-code-proxy/pull/100))

## v0.1.32 (2026-08-03)

- Kimi subagents and multimodal messages with mixed text and images work instead
  of failing with an invalid content-part error.
  ([#98](https://github.com/raine/claude-code-proxy/issues/98),
  [#99](https://github.com/raine/claude-code-proxy/pull/99))

## v0.1.31 (2026-08-02)

- OpenCode Go subscriptions can power Claude Code with supported OpenAI, Google,
  and Anthropic models through the new OpenCode Go provider.
- Codex HTTP responses stream as they arrive, remain active during quiet periods,
  and recover safely from temporary failures before model output begins.
  ([#51](https://github.com/raine/claude-code-proxy/pull/51))
- OpenAI-compatible requests preserve the caller's parallel tool-call setting
  across Codex, Kimi, and Grok routes.
- Claude Code agents sharing a session keep independent Codex continuation state,
  preventing one agent from consuming or replacing another agent's context.
  ([#96](https://github.com/raine/claude-code-proxy/pull/96))
- Overlapping Codex compaction requests preserve the correct conversation history
  and summary. ([#94](https://github.com/raine/claude-code-proxy/pull/94))
- Monitor session token totals remain accurate as older requests leave the recent
  request list or receive stale usage updates.

## v0.1.30 (2026-07-31)

- OpenAI-compatible clients can use Kimi, Grok, Cursor, or Codex through the
  optional `POST /v1/chat/completions` and `POST /v1/responses` endpoints, with
  support for streaming, reasoning, function tools, usage, and provider routing.
- Codex users can transcribe audio through the optional OpenAI-compatible
  `POST /v1/audio/transcriptions` endpoint using the existing Codex sign-in.
  Enable it with `codex.transcriptionsApi` or
  `CCP_CODEX_TRANSCRIPTIONS_API=1`.
- Codex token estimates accurately count CJK text, long identifiers, minified
  code, and base64-like content, improving context and compaction decisions.
  ([#90](https://github.com/raine/claude-code-proxy/pull/90))
- Codex WebSocket sessions remain reliable under high concurrency instead of
  failing with 403 upgrade rejections.
  ([#87](https://github.com/raine/claude-code-proxy/issues/87),
  [#88](https://github.com/raine/claude-code-proxy/pull/88))

## v0.1.29 (2026-07-30)

- Codex honors required, disabled, and single-tool choices from Claude Code, and
  disables parallel tool calls when requested.
  ([#89](https://github.com/raine/claude-code-proxy/pull/89))

## v0.1.28 (2026-07-29)

- OpenAI-compatible clients can generate and edit images with `gpt-image-2`
  through optional Codex Images API routes using the existing ChatGPT sign-in.
  ([#85](https://github.com/raine/claude-code-proxy/pull/85))
- Codex streams show estimated input usage from the start and exact usage at
  completion, keeping Claude Code's live token counters useful.
  ([#86](https://github.com/raine/claude-code-proxy/pull/86))

## v0.1.27 (2026-07-29)

- Grok streams remain reliable during long responses, keepalive events, and
  output-token truncation instead of failing after partial output.
- Forced Codex web searches keep the selected model, so Luna searches no longer
  switch to Sol, while preserving domain filters and search usage reporting.
  ([#53](https://github.com/raine/claude-code-proxy/pull/53))
- Codex WebSocket connections honor `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`,
  and `NO_PROXY`, restoring standard non-TUN HTTP proxy support.
  ([#83](https://github.com/raine/claude-code-proxy/pull/83))

## v0.1.26 (2026-07-28)

- Standard OpenAI clients can use Codex through the optional
  `POST /v1/chat/completions` endpoint, with streaming, reasoning effort, and
  structured output support.
- Cursor Agent works with current client versions and restores text, thinking,
  usage, model mode, and fast-mode handling.
- Claude Code's `claude-opus-5` model name routes correctly through Codex and
  Kimi.
- Claude Code's automatic security-review classifier can use a dedicated model
  configured with `CCP_AUTO_REVIEW_MODEL` or `autoReviewModel`.
  ([#72](https://github.com/raine/claude-code-proxy/pull/72))
- Codex retries empty successful completions and returns a clear error if no
  usable response arrives. ([#70](https://github.com/raine/claude-code-proxy/pull/70),
  [#71](https://github.com/raine/claude-code-proxy/pull/71))
- Grok handles images in user messages and tool results without failing requests.
  Images are omitted by default, with opt-in vision through
  `CCP_GROK_TOOL_IMAGE`. Traffic captures redact image payloads.
  ([#69](https://github.com/raine/claude-code-proxy/pull/69))
- Nix builds use vendored dependencies for reproducible sandboxed builds.
  ([#80](https://github.com/raine/claude-code-proxy/pull/80))

## v0.1.25 (2026-07-24)

- Kimi users can select Kimi K3 with the `kimi-k3` or `k3` model name, including
  its one-million-token context window and `max` reasoning effort.
  ([#79](https://github.com/raine/claude-code-proxy/pull/79))
- The monitor shows active native Codex compaction requests with a dedicated
  `compacting` status.
- The new [documentation site](https://claude-code-proxy.raine.dev) provides
  setup guides, provider details, configuration references, troubleshooting,
  and an `llms.txt` version for coding agents.

## v0.1.24 (2026-07-23)

- Codex optionally preserves conversation continuity across Claude Code
  compaction boundaries with native encrypted compaction artifacts. Enable it
  with `codex.serverCompaction` or `CCP_CODEX_SERVER_COMPACTION`.
- Native OpenAI Responses clients can use `POST /v1/responses` with existing
  Codex authentication, including JSON responses, SSE streaming, and automatic
  token refresh. Enable the endpoint with `codex.responsesApi` or
  `CCP_CODEX_RESPONSES_API=1`; it is disabled by default.

## v0.1.23 (2026-07-22)

- Codex WebSocket streaming handles pooled connections and HTTP fallback more
  reliably, preventing concurrent requests from blocking each other or sending
  the same request twice.
- Codex errors preserve upstream status codes and optional retry timing, with
  clearer permission failures and safer WebSocket handshake diagnostics.
- Codex streaming limits oversized events and error responses, preventing
  malformed or stalled upstream responses from consuming unbounded memory.

## v0.1.22 (2026-07-20)

- Grok accepts request metadata and tool-result references sent by current Claude
  Code versions while continuing to reject malformed and unknown fields.
  ([#56](https://github.com/raine/claude-code-proxy/pull/56))
- Gateway model discovery lists every configured provider model and Claude-style
  alias through `GET /v1/models`, making supported aliases available in Claude
  Code's model picker. ([#60](https://github.com/raine/claude-code-proxy/issues/60),
  [#61](https://github.com/raine/claude-code-proxy/pull/61))
- Codex preserves supported base64 images in tool results while keeping mixed
  text, image, error, and fallback content in its original order.
  ([#59](https://github.com/raine/claude-code-proxy/pull/59))
- Codex requests continue after the included usage limit when account credits
  remain available. ([#68](https://github.com/raine/claude-code-proxy/pull/68))
- Codex compaction requests cap reasoning effort at `low` to reduce latency and
  reasoning-token usage. `CCP_COMPACT_EFFORT` can choose a different cap or
  disable the behavior. ([#67](https://github.com/raine/claude-code-proxy/pull/67))

## v0.1.21 (2026-07-15)

- The monitor shows session token activity trends at common terminal widths,
  making throughput history visible without an extra-wide window.

## v0.1.20 (2026-07-15)

- The monitor reliably shows project names for Claude Code sessions and keeps
  them visible as requests are sequenced.
- Keyboard navigation scrolls session and recent-request tables to keep the
  selected row visible.
- Pressing `q` asks for confirmation before gracefully shutting down the proxy.
- Compact monitor layouts show more project, provider, model, effort, and token
  details without requiring a wider terminal.

## v0.1.19 (2026-07-15)

- The monitor shows project and session context at more terminal widths while
  preserving key request details in narrower layouts.

## v0.1.18 (2026-07-15)

- Codex preserves encrypted reasoning across turns, improving continuity when
  conversation history is replayed. ([#52](https://github.com/raine/claude-code-proxy/pull/52))
- The new `demo` command opens the interactive monitor with simulated traffic,
  without starting a proxy server or requiring provider credentials.
- Session rows show project names and output-token activity over time, making
  concurrent sessions and usage bursts easier to identify.
- Monitor tables adapt more consistently across terminal sizes and keep important
  request details readable in compact layouts.
- The monitor stays visible during graceful shutdown and shows progress until the
  proxy finishes draining connections.

## v0.1.17 (2026-07-14)

- The proxy can listen on a configurable IP address through `CCP_BIND_ADDRESS`
  or `bindAddress`, enabling protected access from containers and remote hosts.
  ([#48](https://github.com/raine/claude-code-proxy/pull/48))
- Model names with context-window hints such as `[1m]` route correctly across
  providers. ([#50](https://github.com/raine/claude-code-proxy/pull/50))
- The monitor reports more accurate output rates by measuring generation time
  and excluding requests without complete usage and timing data.

## v0.1.16 (2026-07-13)

- GPT-5.6 Luna requests work without a custom User-Agent instead of failing with
  a model unavailable error.
  ([#45](https://github.com/raine/claude-code-proxy/issues/45))
- Canceled or replaced Codex prompts cannot interrupt later turns with stale
  continuation state.
- GPT-5.6 setup examples use a 272K compaction window to stay within the current
  ChatGPT context limit.
- Homebrew installations can run the proxy at login as a background service with
  `brew services start claude-code-proxy`.
  ([#44](https://github.com/raine/claude-code-proxy/pull/44))

## v0.1.15 (2026-07-12)

- Codex function tools preserve optional parameters, preventing unintended tool
  arguments and incorrect agent isolation choices.
  ([#43](https://github.com/raine/claude-code-proxy/issues/43))
- Forced Codex web searches return live results while preserving allowed and
  blocked domain filters.
  ([#26](https://github.com/raine/claude-code-proxy/issues/26))
- Codex credentials are stored and refreshed independently from the native Codex
  CLI, preventing either application from invalidating the other's login. Users
  who relied on the native Codex login must sign in to the proxy once after
  upgrading.
- [Expanded guidance](https://github.com/raine/claude-code-proxy/#switching-models-and-backends)
  explains how to switch models within the proxy and how to switch between the
  proxy and direct Anthropic.

## v0.1.14 (2026-07-12)

- Codex hosted web searches work when Claude Code routes them through the Luna
  small model. ([#26](https://github.com/raine/claude-code-proxy/issues/26),
  [#35](https://github.com/raine/claude-code-proxy/pull/35))
- Codex context-window errors trigger Claude Code's compaction flow instead of
  ending the request. ([#29](https://github.com/raine/claude-code-proxy/pull/29))
- Codex requests fall back to HTTP after WebSocket handshake failures while
  preserving live streaming for established connections.
  ([#39](https://github.com/raine/claude-code-proxy/pull/39))
- Codex HTTP and WebSocket failures retain upstream status codes and error
  details, making failures clearer and more actionable.
  ([#40](https://github.com/raine/claude-code-proxy/pull/40))

## v0.1.13 (2026-07-12)

- Grok users can sign in on headless hosts with `grok auth device`.
  ([#38](https://github.com/raine/claude-code-proxy/pull/38))
- Grok tool calls accept Claude Code's prompt-cache markers, preventing errors
  when switching to Grok during a tool-using session.
  ([#37](https://github.com/raine/claude-code-proxy/pull/37))
- Codex hosted web searches return their result links and citations to Claude
  Code instead of appearing to produce zero results.
  ([#10](https://github.com/raine/claude-code-proxy/issues/10))
- Codex authentication refresh is coordinated across concurrent requests and
  automatically recovers live WebSocket requests after credentials expire.
- Codex requests recover more reliably from temporary upstream failures,
  connection resets, overloads, and long-running responses.

## v0.1.12 (2026-07-12)

- Codex hosted web searches work with GPT-5.6 models instead of failing with an
  unsupported tool error. ([#26](https://github.com/raine/claude-code-proxy/issues/26),
  [#35](https://github.com/raine/claude-code-proxy/pull/35))
- Codex WebSocket connection timeouts are retried automatically, reducing
  interrupted requests.

## v0.1.11 (2026-07-11)

- Grok subscriptions can power Claude Code through browser login, with support for
  Grok 4.5 and Composer 2.5 Fast, streaming, thinking, tools, and token counts.
- Codex WebSocket requests recover from handshake failures and stay marked active
  until the full response body finishes streaming.
- The monitor shows local timestamps, clearer request status and detail indicators,
  more compact columns, arrow-key pane navigation, and an uncluttered display.
- Forward Claude Code's `max` effort as Codex `reasoning.effort: "max"` so
  GPT-5.6 can use its highest supported reasoning level instead of silently
  receiving `xhigh`. ([#28](https://github.com/raine/claude-code-proxy/pull/28))

## v0.1.10 (2026-07-10)

- Claude Code requests using Opus 4.8, Sonnet 5, and Fable 5 model names can
  route through Codex

## v0.1.9 (2026-07-10)

- Claude model aliases use the matching GPT-5.6 tier through Codex: Haiku uses
  Luna, Sonnet uses Terra, and Opus uses Sol.
- GPT-5.6 Codex requests preserve reasoning context and support system guidance
  and tools through the Responses Lite API.
- The dashboard shows requested effort and resolved upstream models, making
  routing decisions easier to inspect.

## v0.1.8 (2026-07-09)

- Codex requests can use `gpt-5.6-sol`, `gpt-5.6-terra`, and `gpt-5.6-luna`,
  including `-fast` variants.
- The default Codex setup uses `gpt-5.6-sol` with `gpt-5.6-luna` as the small
  fast model and a 372K compaction window.

## v0.1.7 (2026-07-06)

- Codex `Read` tool calls get clearer offset guidance and recover from clearly
  invalid large offsets, reducing stalled sessions caused by mistaken
  line-number reads.
- The monitor keeps request lists accurate when a client disconnects or abandons
  a request.

## v0.1.5 (2026-07-03)

- Claude Code's `xhigh` and `max` effort settings now work with Codex and Kimi
  requests instead of being rejected or downgraded unexpectedly.
  ([#20](https://github.com/raine/claude-code-proxy/pull/20))
- Codex receives clearer `Read` tool guidance for line offsets, reducing
  incorrect follow-up reads on large files.
  ([#22](https://github.com/raine/claude-code-proxy/pull/22))

## v0.1.4 (2026-07-01)

- Codex WebSocket streams recover when a pooled continuation connection closes
  before the final response, retrying the turn with full context instead of
  failing the session.

## v0.1.3 (2026-07-01)

- Codex WebSocket streams deliver live text and reasoning progress while reusing
  pooled session continuations to reduce repeated upstream input.
- Codex stream recovery handles retryable startup failures, context-window
  errors, stale continuations, completed tool-call disconnects, stalled `Read`
  arguments, quiet upstream turns, and completed-turn stop reasons.
- Codex gateway requests and tool result translation use accepted payload shapes
  and preserve omitted-block markers for malformed text and image result
  content.

## v0.1.2 (2026-06-30)

- Codex WebSocket continuations recover from streams that only deliver rate
  limit or control events, preventing Claude Code sessions from waiting
  indefinitely on a stalled upstream response.

## v0.1.1 (2026-06-30)

- Codex reasoning summaries are now surfaced as thinking blocks in the response
  stream, so you can see the model's reasoning in your Claude Code session
  when reasoning effort is enabled. Set `codex.reasoningSummary` or
  `CCP_CODEX_REASONING_SUMMARY` to `off` or `none` to suppress summary display
  while keeping reasoning effort active. (Thanks @samot-gc!)
- Codex transport errors (WebSocket connection failures, etc.) now show the
  actual error message instead of a generic "Upstream error", making
  connection issues easier to diagnose.

## v0.1.0 (2026-06-30)

- Ships the native Rust implementation as the release binary.
- Adds the default monitor TUI for `serve`.
- Improves diagnostics with failed-response captures and clearer monitor
  request details.

## v0.0.22 (2026-06-24)

- Codex requests now retry more transient stream and overload failures, making temporary upstream errors less likely to interrupt Claude Code sessions. ([#15](https://github.com/raine/claude-code-proxy/issues/15))
- Codex can now recover stalled `Read` tool calls that previously left Claude Code waiting on incomplete streamed arguments.
- Cursor tool calls are recovered more reliably when Cursor returns XML-style tool use, improving compatibility with Claude Code tools.
- Cursor auth can now be isolated with `CCP_CONFIG_DIR`, so separate proxy configs can keep separate Cursor logins.
- Cursor `composer-2.5` requests now stay in non-fast mode unless fast mode is explicitly requested. ([#17](https://github.com/raine/claude-code-proxy/issues/17), [#18](https://github.com/raine/claude-code-proxy/pull/18))

## v0.0.21 (2026-06-15)

- Forced Codex web search requests now use hosted web search correctly, fixing repeated upstream `Tool choice 'function' not found in 'tools' parameter.` errors. ([#10](https://github.com/raine/claude-code-proxy/issues/10))

## v0.0.20 (2026-06-15)

- Cursor's generic `cursor`, `cursor-agent`, `cursor-plan`, and `cursor-ask` aliases now use Cursor default model selection instead of forcing Composer 2.5 fast mode.

## v0.0.19 (2026-06-14)

- Codex now supports Claude Code hosted web search through Codex's native web search, including domain filters and search usage accounting. ([#10](https://github.com/raine/claude-code-proxy/issues/10))

## v0.0.18 (2026-06-09)

- Cursor sessions now stop heartbeat traffic after streams close, reducing stray connection errors.
- Codex now treats runtime system messages as developer guidance instead of assistant output, preventing Claude Code reminders from being repeated.

## v0.0.17 (2026-06-08)

- Added Cursor Agent as a provider, including login, model selection, ask mode, plan mode, and session continuation.
- Cursor users can select models from the Cursor catalog with `cursor:<model-id>`, `cursor-plan:<model-id>`, and `cursor-ask:<model-id>` aliases.

## v0.0.16 (2026-06-02)

- Codex now uses WebSocket transport by default
- Codex sessions can opt in to append-only continuation with `previous_response_id`, reducing repeated upload size on compatible turns.
- `CCP_TRAFFIC_LOG=1` writes redacted per-request traffic captures to help debug sessions.
- Codex request logging now includes size summaries and image warnings to make compaction and large requests easier to diagnose.
- README guidance for Codex context limits and `[1m]` model suffixes is clearer.

## v0.0.15 (2026-05-30)

- Anthropic requests that omit `stream` now receive JSON responses, fixing Claude Code `/model` validation through the proxy.

## v0.0.14 (2026-05-30)

- Codex streaming now stays responsive during long `Read` tool calls by sending keepalive pings while tool arguments are buffered.
- Truncated Codex streams now return a clear error instead of appearing to finish successfully with incomplete tool calls.
- Stalled Codex requests now time out and retry when response headers never arrive, with clearer diagnostics for slow upstream responses.

## v0.0.13 (2026-05-14)

- Windows users can now download prebuilt `windows-amd64` and `windows-arm64` release archives.

## v0.0.12 (2026-05-12)

- Codex requests can now use `gpt-5.3-codex-spark` as a supported model. ([#14](https://github.com/raine/claude-code-proxy/pull/14))

## v0.0.11 (2026-05-12)

- Claude-style aliases such as `haiku`, `sonnet`, and `opus` now default to Codex while still following the provider already active in the current Claude Code session.
- Mixed Codex and Kimi sessions now keep background alias and token-count requests on the right provider instead of unexpectedly switching providers.
- Tool results with images, errors, or unsupported blocks are handled more safely, reducing malformed upstream requests.

## v0.0.10 (2026-05-06)

- Codex requests can now use `codex.serviceTier` or `CCP_CODEX_SERVICE_TIER` to request a service tier; `fast` is sent upstream as `priority`.
- Codex model names can now include `-fast`, such as `gpt-5.4-fast[1m]`, to request fast mode per request without restarting the proxy.
- Codex's upstream endpoint can now be overridden with `codex.baseUrl` or `CCP_CODEX_BASE_URL`.

## v0.0.9 (2026-05-03)

- Kimi debugging overrides now use `CCP_KIMI_OAUTH_HOST` and `CCP_KIMI_BASE_URL`, matching the proxy's `CCP_` environment variable naming.

## v0.0.8 (2026-04-30)

- Added exponential backoff retry on upstream 429 errors, respecting
  `Retry-After` headers when present
- Added `config.json` as an alternative to environment variables (read from
  `~/.config/claude-code-proxy/config.json` on macOS, XDG-compliant on Linux)
- Made the `originator` and `User-Agent` headers configurable via new env vars
  (`CCP_CODEX_ORIGINATOR`, `CCP_CODEX_USER_AGENT`, `CCP_KIMI_USER_AGENT`,
  `CCP_ORIGINATOR`, `CCP_USER_AGENT`) and the config file
- Codex now sends a default `User-Agent: claude-code-proxy/<version>` header

## v0.0.7 (2026-04-25)

- Some security hardening inspired by [#5](https://github.com/raine/claude-code-proxy/pull/5)

## v0.0.6 (2026-04-25)

- Added support for `gpt-5.5`, and `opus`/`claude-opus-4-7` aliases now map to
  `gpt-5.5` instead of `gpt-5.4`
- Model names with a `[1m]` context suffix (e.g. `gpt-5.4[1m]`) are now
  accepted and stripped before routing, so Claude Code's larger-context model
  variants work without errors
- Documented how to switch between the proxy and direct Anthropic in the README

## v0.0.5 (2026-04-22)

- Added `CCP_CODEX_MODEL` and `CCP_CODEX_EFFORT` environment variables to
  override the model and reasoning effort for Codex requests
  ([#2](https://github.com/raine/claude-code-proxy/pull/2))
- Added `claude-sonnet-4-6` and additional model aliases so more Claude-style
  model names resolve correctly
- Improved request logging with usage summaries, time-to-first-byte metrics, and
  stream completion details for easier debugging
- Client disconnections during streaming are now handled gracefully

## v0.0.4 (2026-04-20)

- Kimi: reasoning content is now preserved across turns as Anthropic thinking
  blocks, so Claude Code sees the model's thinking and multi-turn reasoning
  stays coherent
- Kimi: thinking is always enabled

## v0.0.3 (2026-04-20)

- Renamed to `claude-code-proxy` to reflect multi-provider support
- Added Kimi (kimi.com) as a provider, with device-code login via the install
  script and support for Kimi's chat models
- Requests are now routed to providers based on the requested model, so a single
  proxy can serve both Codex and Kimi models simultaneously
- Improved token counting accuracy and fixed cached token usage reporting
- Added MIT license

## v0.0.2 (2026-04-19)

- Accept Claude-style model aliases (`haiku`, `sonnet`, `opus`, and `claude-*`
  names), resolving them to the appropriate upstream model so portable configs
  and subagents work without edits
- Fix malformed streamed Read tool arguments that Claude Code would reject when
  upstream emitted an empty `pages` field

## v0.0.1 (2026-04-19)

Initial release.
