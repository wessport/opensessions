# Restart Sidebar Recovery - 2026-07-06

## Summary

Added a first-class `opensessions start` recovery command and hardened the tmux focus path so restored shell panes titled `opensessions-sidebar` no longer block sidebar startup after a reboot/session restore.

---

## Details

### `opensessions start`

- Added a small shell CLI at `bin/opensessions`.
- The `start` subcommand must run inside an attached tmux session.
- It re-sources `opensessions.tmux` to repair plugin environment/keybindings and then delegates to the canonical `focus.sh` restore path.

### Stale sidebar self-healing

- Updated `integrations/tmux-plugin/scripts/focus.sh` to inspect `pane_current_command`, not just pane title.
- Panes titled `opensessions-sidebar` but running plain shells such as `zsh`, `bash`, or `fish` are treated as stale restore artifacts.
- Stale sidebar shell panes are killed before starting the server and requesting `/ensure-sidebar?reveal=1`, allowing the runtime to spawn a real TUI sidebar.

### Documentation and tests

- README now documents `opensessions start` as the preferred restart/session-restore recovery command.
- README includes the concrete recovery sequence:
  `tmux new -A -s opensessions` followed by `opensessions start`.
- Added tests covering stale shell-pane detection and the CLI recovery entrypoint.

---

## Files Changed

- `bin/opensessions` - New user-facing CLI helper with `start` subcommand.
- `integrations/tmux-plugin/scripts/focus.sh` - Ignores/kills stale restored shell panes titled as sidebars.
- `package.json` - Registers the `opensessions` bin entry.
- `packages/runtime/test/close-sidebar.test.ts` - Adds focused coverage for restart recovery behavior.
- `README.md` - Documents `opensessions start` and the two-command recovery sequence.

---

## Commands Reference

```bash
bun test packages/runtime/test/close-sidebar.test.ts
sh -n bin/opensessions
sh -n integrations/tmux-plugin/scripts/focus.sh

# Preferred recovery after reboot/session restore
tmux new -A -s opensessions
opensessions start
```
