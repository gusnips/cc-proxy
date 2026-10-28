# cc-proxy: design

## The world

A calm terminal tool with a small face. A command prints a headline, a few plain lines and the next command to run. It is not a dashboard of boxes. Teal means awake and ready.

## Surfaces

- Command output (read): a face card, or a one-line note. Plain text when piped.
- Prompts (operate): `src/prompt.rs`, an arrow-key list with type-to-filter. Piped input answers numbered rows.
- Monitor (operate): `src/tui.rs`, ratatui, dark only, dense rows.
- Settings screen (operate): ratatui like the monitor, one list on the alternate screen, with the palette from `src/ui.rs`. Opened by `cc-proxy config`.
- Docs (read): Starlight, `docs/src/content/docs`.

## Tokens

Colors live in `src/ui.rs`: `TEAL`, `WHITE`, `DIM_WHITE`, `BRIGHT_GREEN`, `GREEN`, `RED`, `YELLOW`, `DIM`, and `provider_color` for providers. Never a second palette. A status color always comes with a word or a symbol that Details and the `?` legend explain.

Remaining allowance uses the same colors for percentages and bars: bright green from 80%, green above 30%, yellow above 10% and red at 10% or less. When a weekly or monthly allowance in the same family is at 0%, a 5-hour window is dimmed (`DIM_WHITE`) instead of green because that allowance cannot be used until the weekly window resets. Keep window labels and reset times neutral. Plain text and JSON keep the same numbers without color codes.

## Components

- `ui::print_card`, `ui::print_note`, `ui::print_error`, `ui::print_tip`, `ui::waiting`, `ui::meter`, `ui::duration`, `ui::provider_name`.
- `prompt::choose`, `prompt::search`, `prompt::confirm`, `prompt::read_hidden_line`, `RawModeGuard`.
- Monitor: `TerminalGuard`, `setup_terminal`, `suspend_terminal` and `resume_terminal` in `src/tui.rs`. Sign-in and model selection reuse the CLI prompts, then return to the same monitor row. Settings reuse the same terminal handoff.
- Accounts is the monitor's first tab: one flat list in the order requests try it, and a detail area. A row is a saved account, drawn as `Provider / name`, or a provider that holds one credential (an API key) and is drawn by its name. Rows of different providers mix, and the list is never grouped by provider. The order is the request pool (`route.pool`). A row that is off keeps its place, shows its name dim and ends with `off`. New chats share the top 5 ready accounts across providers (`auth.spreadChats` changes the number), the one with the fewest chats in the last hour first. A chat stays on its account until that account hits a limit, is turned off or is removed. Reordering never moves an existing chat; the header explains how chats pick an account. Providers with no login are not in the pool and wait below it, tagged `not set up`. Claude subscription is a normal provider. Disable proxy routing is the last row. While routing is off it reads `Proxy routing is off [in use]` and its one action, Send Claude Code through the proxy, turns routing back on without changing the list. Settings such as auto-rotate are not rows here; they live on the Settings tab. Enter or Right opens actions in the detail area, not a modal. Wide terminals place them side by side; narrow terminals stack them. Account rows show each reported quota window and a countdown when depleted, using the same numbers and colors as Details. Refresh keeps the last values visible and marks a pending refresh with `refreshing` in the title. Long names shorten only at the end of the name line; Details keeps the full name. Failed usage is unavailable, never zero. A row whose refresh failed keeps its last values and ends with a yellow `*`; Details starts with `* Refresh failed`, says the values may be out of date and gives the reason. The row never shows the long error. After a 429 the automatic refresh leaves that account alone until the provider's wait ends, and the row says how long; `r` asks at once. Only a login that is gone or turned down says to sign in again. A wait timer sits beside the quota windows and never replaces them. An account whose balance is spent says `out of credit` and when cc-proxy tries it again; that time is a recheck, so it never shows as a reset or as `[first reset]`. A spent family (Gemini or Claude on Antigravity) never marks the whole account as waiting. Removing a saved account asks first, names the account and the provider, and says the login is deleted. The background worker loads usage, never a request.
- Actions stay on the monitor's screen. A question (a sign-in, a model choice) takes the screen without leaving the alternate screen, so the shell's scrollback never shows between the monitor and the question. Only an action that may ask leaves the list: sign in or start. Deleting asks inside the monitor: a second line, Enter deletes and Escape keeps. `Move...` enters move mode on the selected row: Up and Down move it one place, Left jumps to the top, Right jumps to the bottom, Enter saves the new order in one write, and Escape cancels and puts the old order back. Rename opens a name field in the details area, with the same editing keys as the CLI prompt (`prompt::InputLine`): it starts with the old name, or with the sign-in's email when the name is only `main` or `account-N`. Enter saves, Escape keeps the old name, and the field takes every key, `q` included. A refused name stays in the field with the reason beside it.
- Settings is the third tab: the same list as `cc-proxy config`, with `auth.autoRotate`, `auth.pauseRequests` and `auth.waitForReset` first. Each row shows a plain human name alongside the dotted key on wide terminals, and the detail area shows the dotted key, description and current value. API keys belong to saved accounts (`cc-proxy auth login`), not `config.json` settings. Letters type into the search while a value or search is open, so tab keys are `Tab`, `Shift-Tab` and `1` to `3` outside a field. It is not shown for `--url` connections.
- Activity is the second tab and keeps the session, active request, recent request and error views. Request rows put the saved account name before the resolved model. Full details keep the original model text. Held requests show `paused` or `waiting` in yellow, with the reset countdown when known.
- Tab, Shift-Tab and 1/2/3 switch tabs. Outside text fields, menus and overlays, `p` pauses or resumes new requests (`auth.pauseRequests`) and `w` toggles automatic waiting for the earliest confirmed reset (`auth.waitForReset`). Both settings persist in `config.json` across monitor close and proxy restart, and the header and tab bar show pause state, held request counts (distinguishing `0 requests queued` while paused), reset countdowns and `auto-wait` status on every tab. Paused and waiting requests never mark an account row as generating (`●`). Left and Right never switch tabs: Right goes one layer in (opens actions or details), Left goes one layer back like Escape, and on Settings they change a choice. Up/Down or j/k move through rows or actions. Escape closes the active layer first. Actions only run after Enter on an explicit choice. Moving or turning off a row changes the order requests try, not where Claude Code goes. The account menu has only move, enable/disable, sign-in, rename and remove. It has no Use now or Use provider actions; the list and on/off state decide new-chat routing.
- Every account row has two lines: `Provider / name` first, then quota, waits, active-request count and state. Names use the row's full width instead of sharing space with usage. Claude plans are labeled `Claude`; API keys are labeled `Anthropic`. The header counts requests running across all windows. Each account currently serving a generation has a teal `●` and bold teal name, and Details lists all active models and their request counts. Multiple accounts can be marked at once. When all requests are idle, a dim `○` marks the last account used. Before the first request there is no mark. Traffic and refresh never move the cursor. A token count is not a generation, and an unknown account must never highlight a different saved account. A confirmed global wait shows a countdown; it shows `0%` only when reported quota confirms depletion. When every row that is on has a confirmed global wait, mark the earliest release `[first reset]` in bold yellow, color its name yellow, show `Next reset: <account> in <time>` on the status line beside `Last used`, and explain in Details that the next request rechecks availability. Model-only waits must not claim the whole row is waiting. Rendering never changes the pool.
- Explicit `monitor --url` connections open Activity and are read-only. They never load this computer's accounts. Demo mode uses simulated accounts; its actions never write real settings or credentials. A disconnected monitor pauses changes and says it is reconnecting.

Build new screens from these. Don't add a terminal UI dependency.

## States

- Loading: the face looks around (`ui::waiting`), shown only after 150 ms.
- Success: `Mood::Glad`. Stopped or off: `Mood::Asleep`. Needs a look: `Mood::Unsure`. Failed: `Mood::Hurt`, with each cause on its own line.
- Empty: say what belongs there and the command that adds it.
- Not a terminal: the same words, no escape codes.

## Motion

Only the waiting face moves. Lists and screens don't animate.

## Rules

- A command's own next step stays in its card. A tip is something else: one random line from `src/tips.rs`, dim, under the card, in line with its text. It is never about the command that just ran, never in piped output, and every command it names must exist (a test parses them).
- The selected row is `❯` in bold teal. Hints are dim, on one footer line that changes with the mode.
- Every error says what failed, why, and what to run.
- A value that an environment variable overrides says so next to the value.
- A secret shows as one `*` for each character typed, so a paste is seen to land, and is never shown again. `prompt::read_hidden_line` draws it; Escape or Ctrl-C cancels and saves nothing.
- Changes that save at once say "Saved". A change that needs a restart says so in the same line.
