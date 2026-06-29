# PR Target Safety Instructions - 2026-06-29

## Summary

Updated `AGENTS.md` to make repository targeting rules explicit after an upstream PR was opened by mistake.

---

## Details

- Added a dedicated `GitHub / Pull Request Safety` section.
- Future AI agents must default to Wes's fork (`wessport/opensessions`) for routine pushed branches and PRs.
- Agents must not open PRs against upstream/original (`Ataraxy-Labs/opensessions`) unless the user explicitly asks.
- If remotes are ambiguous, agents must stop and ask rather than infer from terms like “MR”, “PR”, “push”, or “review”.

---

## Files Changed

- `AGENTS.md` - Added explicit fork/upstream PR safety rules.
- `ai_logs/21-pr-target-safety-instructions.md` - Session log for this instruction update.
