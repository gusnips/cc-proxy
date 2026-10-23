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

Colors live in `src/ui.rs`: `TEAL`, `WHITE`, `DIM_WHITE`, `GREEN`, `RED`, `YELLOW`, `DIM`, and `provider_color` for providers. Never a second palette. A status color always comes with a word.

## Components

- `ui::print_card`, `ui::print_note`, `ui::print_error`, `ui::waiting`, `ui::meter`, `ui::duration`, `ui::provider_name`.
- `prompt::choose`, `prompt::search`, `prompt::confirm`, `prompt::read_hidden_line`, `RawModeGuard`.
- Monitor: `TerminalGuard` and `setup_terminal` in `src/tui.rs`.

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
