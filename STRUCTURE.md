# Monitor structure

One monitor for choosing how to use the proxy and checking its work. Accounts comes first because people need to know which login is in use and what happens when it reaches a limit. Request internals stay one tab away.

## Navigation

```text
cc-proxy monitor
  1 Accounts
    Route and fallback order
    Provider and saved account list
    Selected item's usage, wait timers and actions
  2 Activity
    Sessions
    Active requests
    Recent requests
    Errors
  ? Shortcuts
  b Connection and setup
```

Left/Right, Tab and 1/2 change tabs. Up/Down moves through rows. Enter opens an item's actions, then runs the chosen action. Escape returns to the item. The existing monitor command stays the only entry point; no second dashboard command.

## Accounts

The list keeps providers and their saved accounts together. The current account is not the same as the primary provider: choosing an account does not switch Claude Code's route. A separate Use provider action does that.

Actions live beside the selected item, or below it on a narrow terminal. Keep usage and wait timers with the account they belong to. Rotation and fallback are explicit choices, never enabled as a side effect of adding a login.

## Activity

Keep the existing request views and details. Up/Down crosses between the sessions and recent requests at their edges; j/k stays within the selected pane. Do not repeat these tables on Accounts. Its header needs only connection state and active work.

## Shared parts and boundaries

Reuse the CLI's account store, provider selection, model choices and sign-in prompts. Do not create another account or rotation system for the monitor. The terminal handoff is shared with settings.

Explicit --url connections open Activity and never change this computer's accounts or route. Demo actions affect simulated rows only. Closing an attached monitor leaves the proxy running; stopping a foreground proxy still asks first.
