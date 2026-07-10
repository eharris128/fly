# Residual review findings — feat/monitor-handoff

Deferred findings from the multi-agent code review of the `monitor-handoff`
branch. All eleven actionable (`downstream-resolver`) findings were applied in
commit `3ad724c` (`fix(review): apply monitor-handoff review findings …`); the
items below are the human-owned remainder (design or structure calls) plus the
suppressed lower-confidence risks worth keeping visible.

**Review run:** `/tmp/compound-engineering/ce-code-review/20260710-193838-345abbca/`
(base `2b44fb6`). 9 structured reviewers (correctness, security, adversarial,
testing, maintainability, project-standards, reliability, api-contract,
julik-frontend-races) + 2 CE agents (agent-native, learnings). 8/8 independent
validators confirmed their findings. Verdict: **Ready with fixes** — cleared by
the applied commit; the below are the deliberately-deferred remainder.

## Deferred findings (human-owned)

### #4 — A wedged check pane defeats the broken-monitor escalation (P2, validated)
- **Where:** `src-tauri/src/automations/model.rs::consecutive_infra_failures`
  interacting with the sweep's deadline close + alive-probe skip loop.
- **What:** A monitor check pane that lives past the 30-min deadline but never
  exits produces one `Failed` row, then `SKIP_IN_FLIGHT` skips forever (skips
  are neutral in the derived count); history eviction eventually drops the one
  `Failed` row, so the count pins at 1 then 0 and "monitor broken" never rings
  while the monitor silently never checks.
- **Why deferred:** the fix is a design decision — counting in-flight skips
  after a trailing verdict-less failure changes skip semantics for ordinary
  automations too, and a wall-clock "no verdict in N days" fallback is a new
  mechanism. The wedged pane itself is visible in the Automations workspace,
  which bounds the practical blindness.

### #9 — `cli/automation.rs` crossed 1,000 lines (P2)
- **What:** U5's monitor rendering/validation pushed the file to ~1.5k lines
  (roughly half tests). Proposed: extract `monitor_state_label`, `verdict_run`,
  `verdict_line`, `type_label`, `next_label`, `sort_bucket`,
  `validate_monitor_flags`, `monitor_launch_defaults`, `parse_not_before` into
  `cli/automation_monitor.rs`, mirroring the backend's `verdict.rs` split.
- **Why deferred:** pure structure; churn outweighed benefit inside the same
  review cycle. Good first move next time this file is touched.

### #10 — R7 escalation wiring is repeated at ~8 close sites (P2, validated)
- **Where:** `src-tauri/src/automations/mod.rs` (sweep ack-timeout, deadline,
  dispatch/retry failures, `manual_run`, `close_run`, abstention close,
  `process_pending_interrupts`).
- **What:** each terminal-failure close carries its own `if monitor { … }`
  escalation guard; a future close path can silently forget the R7 wiring.
- **Why deferred:** the validator confirmed a single funnel is not implementable
  as proposed — the store's non-reentrant mutex forces in-lock batched closes
  and out-of-lock immediate closes into two different call shapes — so the real
  fix is two narrower helpers, a refactor worth its own pass.

## Suppressed lower-confidence risks (anchor 50, recorded not fixed)

- **Version-skew downgrade:** an older `fly` app binary rewriting the store
  drops every monitor/verdict field (serde ignores unknowns on its flush) and
  can effectively un-retire a monitor; likewise an older app accepts a newer
  CLI's `--monitor` create as a plain indefinitely-recurring automation with no
  warning. No schema/version marker exists in the store or socket protocol to
  detect a downgrade. Relevant mainly to the stable+dev side-by-side flavor
  setup in CLAUDE.md.
- **Flush-failure verdict-loss window:** if the retire mutation's flush fails
  and the app then crashes, the alert may have fired for a verdict that no
  longer exists after restart (the check re-runs; a duplicate alert is the
  benign outcome).
- **Sweep-thread alert I/O:** verdict/broken alerts run synchronously on the
  10s sweep thread (the sink is a log append + queue today; watch if the sink
  ever grows blocking work).
- **Pre-existing:** `refreshAutomations` has no request sequencing, so
  overlapping `automation://changed` bursts could let a stale response win
  (predates this branch).

## Live-verification follow-ups (plan U6/U7/U8)

The plan's manual `pnpm flavor:dev` checks — register a real monitor and watch
the tab close, fail a toy monitor and click pickup, dry-run the skill
end-to-end (handoff → check → verdict → pickup) — were not run in the
implementing session; all pure logic beneath them is unit/integration tested.
