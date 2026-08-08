---
title: "perf: batch the pane status polls off the main threads (audit T2)"
type: perf
date: 2026-08-08
status: implemented (U1–U6 2026-08-08; KTD8 resolved as the documented-exception
  fallback; R8 verified live 2026-08-08 21:28 — fly main thread avg 5.5%, peak
  30% per 100 ms, zero saturation bursts at the poll cadence, vs 80–110% ×
  200–300 ms every 1.5 s before; webview main thread avg 40% with streaming
  agents = the remaining T4 render cost)
origin: docs/notes/2026-08-08-typing-latency-diagnosis.md
---

# perf: batch the pane status polls off the main threads (audit T2)

## Summary

The 2026-08-08 typing-latency diagnosis
(`docs/notes/2026-08-08-typing-latency-diagnosis.md`) measured the app's GTK
main thread stalling at 80–110% for ~200–300 ms every ~1.5 s. The cause is
the audit's T2 poll storm executed as **sync Tauri commands** (which run on
the GTK main thread): two unsynchronized 1.5 s frontend pollers issue 3–4
invokes **per pane per tick** (`pane_cwd`, `pane_command`,
`pane_session_id`, `pane_activity`), `pane_activity` walks the full `/proc`
table per call, `pane_session_id` readdir+stats a whole Claude project dir
per call, and the nudge polls `pane_activity(focusedPane)` at 1 Hz whenever
the dashboard is closed. `pty_write` — every keystroke — is a sync command
on that same congested thread.

This plan folds the pollers into **one batched async command per tick**
backed by **one `/proc` snapshot**, TTLs the project-dir scan, and moves the
keystroke write off the main thread without giving up write ordering. It
deliberately does not touch rendering — the webview-side saturation is the
audit's T4 (WebGL), a separate change.

## Problem Frame

- The webview main thread is the scarcest resource in the app (diagnosis
  finding 4); every `invoke` costs it a serialization + postMessage round
  trip, so ~60–80 invokes/1.5 s at 20 panes is structural overhead even
  when the backend is fast.
- Tauri v2 runs **sync** commands on the main thread and **async** commands
  on the tokio runtime. All five hot commands are sync today. The fix is
  not "make the /proc walk faster" but "stop doing O(panes) blocking work
  on the main thread at all."
- Poll *semantics* are load-bearing and must not drift: the cwd poll is the
  always-on resume-argv/session capture point (resume plan U4,
  fix-resume-session-selection KTD-A), the activity poll drives the
  dashboard/feed roster and the nudge's busy→idle edge (running-state KTD5,
  nudge KTD1), and the worker ticker exists because WebKit throttles
  main-thread timers while backgrounded
  (fix-feed-stale-status-while-backgrounded). The batching must preserve
  each of these, changing only *where* and *how often* the work runs.

## Key Technical Decisions

- **KTD1 — one batched command per tick; the backend owns the fan-out.**
  New `panes_status(pane_ids: Vec<PaneId>) -> Vec<PaneStatus>` returns, per
  pane: `cwd`, `isAgent`, `argv` (agent-only, the resume capture source),
  `sessionId` (agent-only), the activity snapshot, and `liveTaskCount`.
  One invoke per tick regardless of pane count replaces ~3–4 × N. The
  existing per-pane commands stay registered (spawn-time probes and the
  on-close persist still use them) but no poller calls them.
- **KTD2 — async command + `spawn_blocking`; nothing blocking on the main
  thread.** `panes_status` is an `async fn` whose body runs the /proc and
  readdir work inside `tauri::async_runtime::spawn_blocking`, so it
  neither stalls the GTK loop (the measured 200–300 ms bursts) nor
  starves the shared tokio pool with blocking I/O.
- **KTD3 — one `/proc` table snapshot per call, shared across panes.**
  `read_proc_table()` runs once; `count_background_task_groups` is already
  pure over a table, and `descendants_in_table`'s pid→children map is built
  once per snapshot (both were audit sub-items of T2a). Per-pane residue is
  only the cheap `foreground_pid` + `proc_comm`/`proc_cmdline` reads.
- **KTD4 — session-id resolution is TTL-cached (~5 s per pane) inside the
  batch.** The poll is the version-skew *fallback* capture — the
  SessionStart hook is the precise path (attribution plan, `Poll < Hook <
  Pick`) — so up to ~5 s extra staleness on rotation is acceptable by
  design, and a hook capture at higher trust rank is unaffected. The
  audit's alternative (skip the scan when the project dir's mtime is
  unchanged) is **rejected for correctness**: appending to an existing
  transcript does not bump the directory mtime, so with two live sessions
  in one cwd the newest-file answer can change while the dir mtime doesn't
  — the cache would serve a stale id indefinitely, not boundedly.
- **KTD5 — `pty_write` goes async; ordering moves to the caller.** Making
  the write command async takes keystrokes off the congested main thread,
  but two async invokes may complete on different tokio workers — the
  current keystroke ordering is an artifact of main-thread serialization.
  The `ipc.ts` wrapper therefore serializes writes through a **per-pane
  promise chain** (order guaranteed end-to-end regardless of backend
  threading; negligible cost at typing rates). `emit_attention` and
  `AttentionManager::on_input` are already thread-safe (they run from hook
  dispatch threads today).
- **KTD6 — one clock.** The merged poller runs on the existing 1.5 s
  worker ticker (backgrounding-proof); the separate main-thread
  `refreshCwds` interval is deleted. The cwd/argv/session capture happens
  every tick unconditionally (it is always-on today); activity/task-count
  consumption stays gated by `homeViewOpen || feedEnabled` exactly as now —
  but since the marginal cost of computing it inside the one batched call
  is a single shared /proc snapshot, the *command* always returns the full
  shape and the frontend simply ignores what it isn't consuming. KTD-C
  (usage fetch on dashboard open only) is untouched.
- **KTD7 — the nudge stops walking /proc.** The nudge's 1 Hz
  `pane_activity(focused)` call — a full table walk per second on the main
  thread precisely while the user types — is replaced by reading the
  focused pane's entry from the shared poll result (`agentByLeaf`). The
  nudge's busy→idle edge then updates at the 1.5 s cadence instead of 1 Hz;
  with the default `nudgeIdleMs` = 1500 the worst-case nudge delay moves
  from ~2.5 s to ~3 s — accepted (the nudge is a courtesy, not a deadline).
  Its own 1 s interval survives only to advance the *user-idle* clock,
  which needs no IPC.
- **KTD8 — fix the `foreground_pid` ioctl-under-lock in passing** (audit
  T2 bonus): `PtyManager::foreground_pid` runs the tcgetpgrp ioctl via
  `Pane::foreground_pid` while holding the registry lock, against the
  module's own KTD13 discipline. If the master handle can't be borrowed out
  cheaply, the fallback is documenting the measured cost — the ioctl is
  non-blocking and cheap; discipline, not latency, is the driver here.

## Requirements

- **R1** — exactly **one** status invoke per poll tick, independent of pane
  count; no per-pane `pane_cwd`/`pane_command`/`pane_session_id`/
  `pane_activity` calls remain in any repeating poller (pollers = the
  merged ticker, the nudge, focus-regain refresh).
- **R2** — at most one full `/proc` table read per `panes_status` call.
- **R3** — at most one project-dir readdir per agent pane per TTL window
  (~5 s); a hook-sourced session capture is never blocked or downgraded by
  the cache.
- **R4** — no blocking syscall from the poll path runs on the GTK main
  thread; `panes_status` and `pty_write` are async commands.
- **R5** — per-pane keystroke write order is preserved (per-pane promise
  chain in the wrapper); interleaving with `submit`/paste paths unchanged.
- **R6** — observable behavior parity: resume argv/session capture
  semantics (incl. change-tracking and null-never-clears), dashboard/feed
  roster values, nudge trigger semantics (modulo the KTD7 cadence note),
  and focus-regain immediate refresh all behave as today.
- **R7** — the worker-ticker backgrounding immunity is preserved for the
  merged poller (the deleted main-thread interval must not take the cwd
  capture down with it while the window is occluded).
- **R8** — validated live by the diagnosis note's re-measurement protocol:
  after the change, the fly main thread shows no ≥100 ms saturation bursts
  at the poll cadence with ~10 live panes / 4 agents, and total poll-tick
  invoke count is 1.

## Units of Work

- **U1 — backend `panes_status`** (`pty/mod.rs`): `PaneStatus` wire struct
  (serde camelCase), shared-snapshot fan-out over the id list, async
  command + `spawn_blocking`, registered in `lib.rs` invoke_handler.
  Tests: graceful on unknown ids; one-table sharing exercised via the pure
  helpers; shape snapshot.
- **U2 — session-id TTL cache** (backend, per-pane ~5 s, keyed by pane id +
  resolved cwd so a `cd` invalidates): sits inside U1's fan-out. Test: a
  second call within the TTL performs no dir read (inject a counting
  reader or gate by mtime-of-call count); cwd change busts the entry.
- **U3 — frontend merge** (`App.svelte`, `ipc.ts`): one `refreshPanes()`
  on the worker ticker consuming `panesStatus`; `captureResumeArgv`/
  `captureResumeSession` read from the batched result (same
  change-tracking); delete the main-thread `refreshCwds` interval; keep
  the focus-regain immediate refresh; `panesStatus` wrapper in `ipc.ts`.
  Vitest for any logic that stays pure.
- **U4 — nudge rewire** (KTD7): busy→idle edge from the shared poll
  result; keep the 1 s user-idle tick IPC-free. Vitest: nudge state
  machine unchanged, only the sampling source moves.
- **U5 — async `pty_write` + per-pane write chain** (KTD5/R5):
  `pty/mod.rs` + the `ptyWrite` wrapper. Test: wrapper preserves order
  under interleaved resolution (vitest with controllable promises).
- **U6 — `foreground_pid` lock discipline** (KTD8, best-effort) + doc
  sites: audit note T2 checked off, CLAUDE.md poll description, plans
  README row, and the live re-measurement recorded in the diagnosis note.

## Risks

- **Async reorder of writes** — addressed head-on by KTD5/R5; the risk of
  *not* chaining is silent keystroke transposition under load, which is
  worse than the latency being fixed.
- **Session-capture staleness** (KTD4): bounded at TTL; the attribution
  trust ranking means a stale poll id can never override a hook or pick.
- **Tokio-pool starvation** if the blocking work ran as a plain async fn —
  prevented by `spawn_blocking` (KTD2); worth a doc comment since the next
  hot command added will copy this shape.
- **Nudge timing drift** (KTD7): ~0.5 s worse worst-case; if it reads
  wrong in use, the escape hatch is polling `panes_status([focused])` at
  1 Hz — still batched, still async, one pane.
- **Behavior parity regressions** in the capture path (R6): mitigated by
  keeping `shouldCaptureSession`/change-tracking logic untouched and only
  swapping its input source.

## Validation

1. `cargo test --offline` + `pnpm test:unit` + `pnpm check`.
2. Live: rebuild, install, reproduce the diagnosis load (~10 panes, 4
   streaming agents), rerun the note's protocol — expect the fly main
   thread flat (no poll bursts), webview invoke churn reduced, and the
   remaining saturation attributable to render cost only (the T4
   signature: ~1:1 flush↔wake correlation persisting).
3. Regression sweep of the consumers: dashboard statuses, feed roster
   (`curl /feed`), resume capture after an agent `/clear` (poll fallback
   within ~5 s + hook precise), nudge appearing on busy→idle for a
   defocused-then-idle agent.
