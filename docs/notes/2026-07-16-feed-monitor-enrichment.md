# feed-monitor-enrichment — monitor, retiredAt, lastVerdict on the automation projection

**Landed:** `608bb46` (branch `feat/feed-monitor-enrichment`, merged as
`d1bac0b`, 2026-07-11). **Code:** `src-tauri/src/feed/wire.rs`
(`AutomationEntry`, `VerdictEntry`, `from_automation`), mirrored in
`src/lib/feed.ts`.

This feature shipped without a fly-side plan: its design lives in the
consumer's own repo, in its Ambient Wall plan (the `U6`/`U7` IDs cited in the
wire doc-comments are that plan's units, not a fly plan's; fly's own
`2026-07-11-001-*` is the unrelated feed-other-answer plan). This note is the
fly-side record so the wire-contract change has a doc footprint here.

## What and why

The Ambient Wall (the feed's local consumer) needs honest pass/fail truth the
wire did not carry. A monitor check's *run* can succeed (the headless `claude
-p` process ran cleanly) while its *parsed verdict* is FAIL — a verdict is
parsed only from an infra-clean run — so `lastStatus` alone shows green on a
failed experiment. Three additive fields on the feed's `AutomationEntry`
projection fix that:

- `monitor` (bool) — is this automation a monitor.
- `retiredAt` (epoch ms, null when live) — stamped in the same store mutation
  that closes the verdict run, so it aligns with `lastRunAt` and the verdict.
- `lastVerdict` (`{ outcome: "pass"|"fail", note }`) — projected from the last
  run's `Verdict`, which stays `last_run()` permanently because a monitor
  retires on its first verdict and refuses every later claim. Omitted when the
  run carried no verdict (every non-monitor, and a monitor's not-done checks).

## Compatibility

Additive only: `monitor` defaults false and `retiredAt` null on absent-field
load, `lastVerdict` is skip-if-none — an older consumer reads the
pre-enrichment shape unchanged.

**Consumer rule:** read `lastVerdict`, not `lastStatus`, for pass/fail — a
FAIL verdict rides a `lastStatus:"succeeded"` row by design.
