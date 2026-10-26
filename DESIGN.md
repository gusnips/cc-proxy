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

Remaining allowance uses the same colors for percentages and bars: bright green from 80%, green above 30%, yellow above 10% and red at 10% or less. Keep window labels and reset times neutral. Plain text and JSON keep the same numbers without color codes.

## Components

- `ui::print_card`, `ui::print_note`, `ui::print_error`, `ui::print_tip`, `ui::waiting`, `ui::meter`, `ui::duration`, `ui::provider_name`.
- `prompt::choose`, `prompt::search`, `prompt::confirm`, `prompt::read_hidden_line`, `RawModeGuard`.
- Monitor: `TerminalGuard`, `setup_terminal`, `suspend_terminal` and `resume_terminal` in `src/tui.rs`. Sign-in and model selection reuse the CLI prompts, then return to the same monitor row. Settings reuse the same terminal handoff.
- Accounts is the monitor's first tab: one provider/account list and a detail area. Providers follow the route Claude Code uses: the primary first, then fallbacks in request order, then other connected providers alphabetically, then providers not set up. Claude subscription is a normal provider and sorts the same way. Disable proxy routing is the last row, after the providers. Settings such as auto-rotate are not rows here; they live on the Settings tab. Enter or Right opens actions in the detail area, not a modal. Wide terminals place them side by side; narrow terminals stack them. Account rows show each reported quota window and a countdown when depleted, using the same numbers and colors as Details. Refresh keeps the last values visible and marks a pending refresh with `refreshing` in the title. Long names shorten in the list so usage stays visible; Details keeps the full name. Failed usage is unavailable, never zero. A row whose refresh failed keeps its last values and ends with a yellow `*`; Details starts with `* Refresh failed`, says the values may be out of date and gives the reason. The row never shows the long error. A wait timer sits beside the quota windows and never replaces them. An account whose balance is spent says `out of credit` and when cc-proxy tries it again; that time is a recheck, so it never shows as a reset or as `[first reset]`. A spent family (Gemini or Claude on Antigravity) never marks the whole account as waiting. Removing a saved account asks first, names the account and the provider, and says the login is deleted. The background worker loads usage, never a request.
- Actions stay on the monitor's screen. A question (a sign-in, a model choice) takes the screen without leaving the alternate screen, so the shell's scrollback never shows between the monitor and the question. Only an action that may ask leaves the list: sign in, start, and use provider when a sign-in or a model choice is missing. Deleting asks inside the monitor: a second line, Enter deletes and Escape keeps.
- Settings is the third tab: the same list as `cc-proxy config`, with auto-rotate first. Letters type into the search while a value or search is open, so tab keys are `Tab`, `Shift-Tab` and `1` to `3` outside a field. It is not shown for `--url` connections.
- Activity is the second tab and keeps the session, active request, recent request and error views. Request rows put the saved account name before the resolved model. Full details keep the original model text.
- Tab, Shift-Tab and 1/2/3 switch tabs. Left and Right never switch tabs: Right goes one layer in (opens actions or details), Left goes one layer back like Escape, and on Settings they change a choice. Up/Down or j/k move through rows or actions. Escape closes the active layer first. Actions only run after Enter on an explicit choice. Saved account choices change that provider, not Claude Code's route.
- A saved current account is not an active request. Mark it with one `●` before its name and a bold name, never a second `[current]` tag. Teal belongs to who answers: the provider and the account that served the newest request (the line under the header says it in words: `Serving` while one runs, `Last used` after), or the route's first choice before any request. A fallback that answers is teal, not the primary. The saved choice of every other provider keeps a white `●`. `[primary]` and `[fallback N]` are quiet route labels, never the highlight. The cursor follows the account that answers, even across providers, unless the person moved it. A token count is not a request that was answered. A confirmed global wait shows a countdown; it shows `0%` only when reported quota confirms depletion. When every account has a confirmed global wait, mark the earliest release `[first reset]` and explain that the next request rechecks availability. Model-only waits must not claim the whole provider is waiting. Rendering never changes the saved account.
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
