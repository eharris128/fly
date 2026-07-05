---
title: "feat: Automation interrupt resilience — surface + opt-in retry across app crashes"
type: feat
date: 2026-07-05
status: implemented
depth: standard
origin: investigation of a crashed scheduled agent run (rvc-exp001-curve03-read)
---

# feat: Automation interrupt resilience

## Summary

When the fly desktop app crashes or is restarted while an automation run is
in flight, the run's PTY child dies with the app and, on the next launch,
**startup recovery** (`recover_interrupted`, R5 of the automations plan) closes
the orphaned `Running` row `failed("interrupted")`. Before this plan that close
was **silent** — no alert, no `automation://changed`, no retry — so a scheduled
run killed by a crash became a `failed` row you'd only find by scrolling the
dashboard. There was also **no retry concept anywhere** in the subsystem.

This plan makes an interrupted run **impossible to miss** and adds an **opt-in,
default-off** one-shot retry:

1. **Surface, always.** Every interrupted run is pushed through the existing
   alert pipeline (`AlertsLog` append + attention ring, or the R17 pending-queue
   when the sink pane isn't up yet) and emits `automation://changed` so an open
   dashboard refreshes. Same machinery a script alert uses (R16/R17/R18).
2. **Retry, opt-in.** A new per-automation `retry_on_interrupt` flag (default
   **off**) makes an interrupted run re-dispatch **once** on the next launch as a
   `Trigger::Retry` run, honoring the same readiness/overlap gates as a scheduled
   fire. Retry-**once** is the crash-loop guard: a run *born* from a retry that is
   itself interrupted only alerts, never re-runs.

Default-off is deliberate: the motivating case (`rvc-exp001-curve03-read`)
provisions paid cloud GPUs, and the R5 design explicitly refuses to assume a
crashed run is safe to re-run ("outcome unknowable"). So money/cloud agents stay
opt-out and just get the loud alert; idempotent automations (scripts, read-only
agents) can opt in.

---

## Problem Frame

From the investigation that motivated this plan (a scheduled curve-read agent):

- `recover_interrupted` (`automations/mod.rs`) runs inside
  `AutomationManager::new`, **before** the dispatcher, alert sink, config, and
  frontend are wired. That ordering is *why* it was silent — it cannot alert or
  re-dispatch in place.
- `"interrupted"` is written in exactly two places — `recover_interrupted`
  (startup) and `shutdown()` — so it uniquely means "an app stop orphaned this
  in-flight run," never a task-logic failure.
- The alert pipeline the fix wants already exists: `AlertsLog` (R16, 64 KiB tail
  cap, control-sanitized), the R17 pending-queue for alerts that arrive before
  the sink pane exists, and `raise_alert` → attention ring (R18).

**Goal:** a crash mid-run can no longer silently drop a scheduled run; opted-in
automations recover automatically, once, without risking a crash loop or a
double-spend on non-idempotent agents.

---

## Key Technical Decisions

- **KTD1 — Defer surfacing past `new()`.** `recover_interrupted` now *collects*
  the interrupted runs (into an outer-captured `Vec`, so the list survives a
  flush failure) and `new()` stashes them in `pending_interrupts`. The alert +
  retry happen in `process_pending_interrupts`, called from the **top of
  `sweep_once`** (idempotent via `mem::take`). The sweep ticks immediately on
  spawn and unconditionally every 10s (R5), and `lib.rs` wires the interrupt sink
  **before** `start_sweep`, so the backlog surfaces on the first tick.
- **KTD2 — Reuse the alert pipeline, one shared path.** `lib.rs` factors the
  script sink's append + ring-or-queue into a single `surface_alert(name, line)`
  helper; both the script alert path and the interrupt path go through it, so
  they can't drift on R16/R17/R18. Interrupt lines read
  `[name] interrupted by restart` or `… — retrying`.
- **KTD3 — Retry rides the sweep, not a bespoke dispatch path.** Retry-eligible
  ids enqueue to `retry_queue`; `sweep_once` drains them through the existing
  claim→dispatch machinery with `Trigger::Retry`. An **agent** retry honors the
  R5 frontend-ready gate (an unattended re-run) and re-queues until ready; a
  **script** retry fires immediately. A retry is skipped if a run is already in
  flight (R7 overlap). A retry-dispatch failure closes the row failed but — unlike
  a scheduled claim — **never recomputes the schedule** (a retry consumes no
  occurrence), and is not re-enqueued (retry-once).
- **KTD4 — Retry-once crash-loop guard.** `Trigger::Retry` is a first-class
  trigger. Startup recovery reads the interrupted row's trigger; a run born from a
  `Retry` is surfaced but never retried again.

---

## Requirements

- **R1** — `Automation.retry_on_interrupt: bool`, `#[serde(default)]` (legacy
  store rows load as `false`); set at create via the CLI `--retry-on-interrupt`
  flag / socket `AutomationRequest.retry_on_interrupt` / `CreateSpec`.
- **R2** — Every interrupted run is surfaced exactly once through the
  `InterruptSink` seam and emits `automation://changed`. Surfacing survives a
  recovery flush failure (KTD-B: the in-memory close still applied).
- **R3** — A retry consumes no occurrence: `Trigger::Retry` claims pass
  `next_run_at` through unchanged and record `scheduled_for: None` (like a manual
  run, R23).
- **R4** — Retry-once: a `Trigger::Retry` run that is itself interrupted alerts
  but is not retried again.
- **R5** — Agent retries honor the frontend-ready gate; script retries do not.
- **R6** — Retry-eligibility = `retry_on_interrupt && enabled && !was_retry`.

---

## Units

- **U1 — model** (`automations/model.rs`): `retry_on_interrupt` field;
  `Trigger::Retry` variant; `claim`/`skip` treat `Retry` like `Manual` for
  `scheduled_for`. Pure test: `retry_claim_consumes_no_occurrence`.
- **U2 — manager** (`automations/mod.rs`): `InterruptedRun` + `InterruptSink`
  seam; `pending_interrupts` + `retry_queue` fields; `recover_interrupted`
  collects; `process_pending_interrupts` alerts + enqueues; `sweep_once` drains
  the backlog (top) and the retry queue (phase 1) with a no-recompute retry
  dispatch (phase 2b). Tests: opt-in retry, opt-out, retry-once (R4), agent
  defer-until-ready (R5).
- **U3 — lib.rs**: shared `surface_alert` helper; wire `set_interrupt_sink` to
  the `AlertsLog` + attention pipeline; wiring precedes `start_sweep`.
- **U4 — CLI** (`cli/automation.rs`): `--retry-on-interrupt` on `create` (+ help);
  `retry` line in `fly automation show`; `AutomationRequest.retry_on_interrupt`.
- **U5 — dashboard** (`src/ipc.ts`, `src/lib/automations.ts`,
  `src/lib/HomeView.svelte`): `retryOnInterrupt` on the wire type + view-model + a
  small `retry` row tag; `RunTrigger` gains `"retry"`. The interrupted run also
  surfaces as a `Reason::Alert` notification (existing pipeline) and as a `failed`
  last-run row in the read-only panel.

---

## Out of scope

- **Why fly crashed** — treated as a given (crash, OS kill, update, reboot).
  Resilience is independent of the crash cause.
- **Auto-retry of money/cloud agents** — deliberately opt-out by default.
- **Configurable max-attempts** — retry-once is the v1 bound; extensible later.
