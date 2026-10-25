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

Colors live in `src/ui.rs`: `TEAL`, `WHITE`, `DIM_WHITE`, `BRIGHT_GREEN`, `GREEN`, `RED`, `YELLOW`, `DIM`, and `provider_color` for providers. Never a second palette. A status color always comes with a word.

Remaining allowance uses the same colors for percentages and bars: bright green from 80%, green above 30%, yellow above 10% and red at 10% or less. Keep window labels and reset times neutral. Plain text and JSON keep the same numbers without color codes.

## Components

- `ui::print_card`, `ui::print_note`, `ui::print_error`, `ui::waiting`, `ui::meter`, `ui::duration`, `ui::provider_name`.
- `prompt::choose`, `prompt::search`, `prompt::confirm`, `prompt::read_hidden_line`, `RawModeGuard`.
- Monitor: `TerminalGuard`, `setup_terminal`, `suspend_terminal` and `resume_terminal` in `src/tui.rs`. Sign-in and model selection reuse the CLI prompts, then return to the same monitor row. Settings reuse the same terminal handoff.
- Accounts is the monitor's first tab: one provider/account list and a detail area. Disable proxy routing stays first and Claude subscription stays second, with its saved accounts below it. Other providers follow by active route, fallbacks in request order, other connected providers alphabetically, then providers not set up. Auto-rotate is a separate setting after the provider list. Enter opens actions in the detail area, not a modal. Wide terminals place them side by side; narrow terminals stack them. Account rows show each reported quota window and a countdown when depleted, using the same numbers and colors as Details. Refresh keeps the last values visible and marks pending or failed refreshes separately. Long names shorten in the list so usage stays visible; Details keeps the full name. Failed usage is unavailable, never zero. The background worker loads usage, never a request.
- Activity is the second tab and keeps the session, active request, recent request and error views. Request rows put the saved account name before the resolved model. Full details keep the original model text.
- Left/Right, Tab and 1/2 switch tabs. Up/Down or j/k move through rows or actions. Escape closes the active layer first. Actions only run after Enter on an explicit choice. Saved account choices change that provider, not Claude Code's route.
- A saved current account is not an active request. Mark it `[current]` beside its quota. A confirmed global wait shows a countdown; it shows `0%` only when reported quota confirms depletion. When every account has a confirmed global wait, mark the earliest release `[first reset]` and explain that the next request rechecks availability. Model-only waits must not claim the whole provider is waiting. Rendering never changes the saved account.
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

- The selected row is `❯` in bold teal. Hints are dim, on one footer line that changes with the mode.
- Every error says what failed, why, and what to run.
- A value that an environment variable overrides says so next to the value.
- A secret is masked while typed and never shown again.
- Changes that save at once say "Saved". A change that needs a restart says so in the same line.
