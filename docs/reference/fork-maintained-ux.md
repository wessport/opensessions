# Fork-Maintained UX Contracts

This fork carries user workflows that must survive upstream merges, runtime
rewrites, and UI migrations. Treat the behavior below as product contracts,
not incidental implementation details.

## Protected Workflows

- `n` and `c` open the tmux sessionizer popup. They must not silently create a
  sequentially numbered session.
- The sessionizer supports fuzzy selection, explicit valid paths (including
  `~/...`), colon-separated `SESSIONIZER_DIR` roots, configurable
  `SESSIONIZER_MAXDEPTH`, hidden-directory filtering, and switching to an
  existing same-named session.
- Session rows retain inline agent state signals, including running and
  completed/waiting-for-feedback states.
- Electric Fusion remains available as a theme. Palette choice and transparent
  background are independent settings controlled with the arrow keys.
- Clicking a displayed session directory opens it in the platform file browser.
- Clicking git insertion/deletion counts opens a persistent diff view; launch
  failures must remain visible instead of flashing and disappearing.
- Existing sidebars remain present while switching sessions. Session switching
  must not destroy and recreate the sidebar pane.

## Upstream Sync Checklist

Before merging canonical upstream changes or replacing a runtime/UI layer:

1. Review this file and `docs/reference/features-and-keybindings.md`.
2. Run `cargo test --workspace --lib --bins` and
   `bun test scripts/sessionizer.test.ts`.
3. Run the tmux E2E suite for changes to hooks, panes, session switching, or
   input routing.
4. Update the tests and this inventory deliberately when changing a protected
   workflow. Do not delete an implementation merely because another runtime no
   longer calls it; first prove the replacement preserves the contract.

Git merges normally preserve committed fork changes. The main risk is semantic
loss during conflict resolution or rewrites, so each protected workflow should
also have an executable regression test at its owning boundary.
