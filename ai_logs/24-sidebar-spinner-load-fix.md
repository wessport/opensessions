# Sidebar Spinner Load Fix - 2026-07-22

## Summary

Investigated tmux sluggishness while opensessions was running and found many hidden sidebar panes animating unnecessarily. The TUI now only runs the spinner animation loop for the sidebar associated with the current mux session.

---

## Details

- Observed a live opensessions environment with many hidden `opensessions-sidebar` panes after switching/creating sessions.
- Identified that every hidden sidebar received global agent state and ran the 120ms spinner interval whenever any session had a running agent.
- Added a current-sidebar guard so inactive/background sidebars do not maintain the animation timer.
- Left active sidebar behavior unchanged: the current sidebar still animates while initializing or while any session has a running agent.

---

## Files Changed

- `apps/tui/src/index.tsx` - Added `isCurrentSidebar` and gated the spinner interval on it.
- `packages/runtime/test/close-sidebar.test.ts` - Added a source-structure regression test for background sidebar spinner gating.

---

## Commands Reference

```bash
cd packages/runtime && bun test test/close-sidebar.test.ts
```
