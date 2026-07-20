# Sidebar Token Race Fix - 2026-07-06

## Summary

Fixed a regression where `prefix + o + s` could appear to open the opensessions sidebar and then immediately close or fail to restore it. The root cause was a recent per-instance auth-token startup ordering bug: a duplicate server startup that failed because the port was already in use could still overwrite the active server's token file.

Follow-up: fixed a related live failure where creating a new session in `../loc/loc-6686` left the new session without a sidebar because the live server was still running but its `/tmp/opensessions.<server-key>.token` file had disappeared.

---

## Details

### Root Cause

- The live tmux install was running from `~/.tmux/plugins/opensessions`.
- The key binding was registered correctly as `prefix` → `o` → `s`, pointing at `integrations/tmux-plugin/scripts/focus.sh`.
- The server was alive on the per-tmux port, but authenticated sidebar requests returned `401 Unauthorized`.
- A failed duplicate server startup had written a new `/tmp/opensessions.<server-key>.token` before `Bun.serve()` failed with `EADDRINUSE`, leaving the active server with an in-memory token that no longer matched the token file used by tmux scripts.

### Fix

Moved PID/token publication in `packages/runtime/src/server/index.ts` until after `Bun.serve()` successfully binds the port. A duplicate startup that loses the port race now throws before touching the active server's PID or token files.

The same source change was applied to the active TPM install at `~/.tmux/plugins/opensessions` so live tmux behavior uses the fix immediately.

Added a second hardening pass:

- the live server republishes its PID/token files on startup, periodically, and at the start of request handling
- cleanup only removes PID/token files if they still contain this server's own PID/token
- the sessionizer probes the liveness endpoint once when the token file is missing, then retries reading the token before giving up

### Live Recovery

- Restarted only the opensessions server process for the active tmux socket.
- Re-ran the focus path via tmux.
- Confirmed the sidebar pane was restored and remained present in the current window.
- Confirmed authenticated `POST /refresh` returned `200 OK`.

---

## Files Changed

- `packages/runtime/src/server/index.ts` - Publish PID/token only after `Bun.serve()` succeeds.
- `apps/tui/scripts/sessionizer.sh` - Republish/retry token lookup before notifying opensessions after session creation.
- `packages/runtime/test/close-sidebar.test.ts` - Added coverage for the sessionizer missing-token retry behavior.
- `~/.tmux/plugins/opensessions/packages/runtime/src/server/index.ts` - Applied the same fix to the live TPM install.
- `~/.tmux/plugins/opensessions/apps/tui/scripts/sessionizer.sh` - Applied the same sessionizer fix to the live TPM install.

---

## Verification

```bash
cd packages/runtime && bun test test/server-auth.test.ts test/close-sidebar.test.ts

# Live duplicate-start regression check:
# Start a second server with the same OPENSESSIONS_SERVER_KEY while the real one is listening.
# Result: EADDRINUSE as expected, and the existing token file remained unchanged.

# Live missing-token check:
# Remove /tmp/opensessions.<server-key>.token, hit GET /, verify the same token is recreated,
# then POST /refresh with that token and confirm 200 OK.
```
