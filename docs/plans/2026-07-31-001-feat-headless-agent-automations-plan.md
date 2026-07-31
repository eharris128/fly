---
title: "feat: Closed-loop headless agent automations"
type: feat
date: 2026-07-31
status: implemented (U1–U5 2026-07-31; U6 live validation pending)
---

# feat: Closed-loop headless agent automations

## Summary

Regular (non-monitor) agent-mode automations gain the option — and, by
default, the disposition — to dispatch **headless**: a backend-owned
`claude -p --output-format stream-json` child via the existing
`automations/headless.rs` runner, instead of a `claude` session in a pane. A
headless agent run is **closed-loop by design**: one prompt in, one captured
result out, no pane, no tab, no interactive return path — the automation
agent never "comes back for prompting." This picks up the headless-monitor
plan's first deferred item ("Headless dispatch as an option for regular agent
automations") and, with it, the second and third (surfacing `session_id` in
`runs` output; retiring the transcript-retry capture for runs that go
headless). Failure visibility moves from the kept-open failed tab (which no
longer exists) to the alert pipeline, and the dashboard/feed learn to show a
running headless run honestly.

Origin: the deferred follow-ups of
`docs/plans/2026-07-11-003-feat-headless-monitor-checks-plan.md` plus the
2026-07-31 direction decision that automation agents should always be
closed-loop chains, not interactive panes.

---

## Problem Frame

Today a regular agent automation rides the full interactive machinery — a
background ephemeral tab, Stop-hook close, the KTD5 completion-raise
suppression, the transcript retry-read racing Claude's Stop-before-flush —
for a run that is, in practice, never interacted with. The machinery exists
because v1 predates the headless runner. The runner now exists, is proven for
monitor checks (empirical stream contract pinned on 2.1.207), and the row
model, sweep exemptions, kill discipline, and shared close tail were all
deliberately built on a row-level `headless` marker rather than the monitor
flavor — so the substrate is already general. What's missing is the flag, the
routing, the failure-visibility replacement for the kept-failed-tab, and the
dashboard read on a run that no longer appears as a pane.

The one thing the pane path provided that headless must consciously replace:
a **failed** run kept its tab open (automations plan R7 / workspace-and-model
R7) so the user would see it. A headless failure with no surface would be a
silent regression — hence R6 below.

---

## Requirements

**Flag & resolution**

- R1. `Mode::Agent` gains `headless: Option<bool>` (`#[serde(default)]`,
  camelCase, legacy rows load as `None`). `config.automation_defaults` gains
  `headless: bool`. Effective dispatch mode resolves automation-explicit →
  config default, at claim time, in one place.
- R2. The config default ships **`true`** — closed-loop is the stated
  direction, and the knob (not a migration) is the escape hatch. An existing
  automation flips to headless on upgrade unless created/re-created with the
  explicit pane override. `fly automation create` grows `--headless` /
  `--paned` (mutually exclusive; agent-only; `--headless` rejected with
  `--monitor`, which is already unconditionally headless — the `--not-before`
  rejection pattern).
- R3. Monitors are unchanged: their checks remain unconditionally headless
  regardless of flag or config (`monitor` still forces the marker), and the
  verdict/retire pipeline is untouched.

**Dispatch & lifecycle**

- R4. A headless agent run reuses the monitor runner end to end: same argv
  shape (`-p --output-format stream-json --verbose
  --dangerously-skip-permissions [--model/--effort/--fallback-model]
  <prompt last>`), same clean env (inherit-minus-strip; no
  `FLY_PANE_TOKEN`/`FLY_SOCKET_PATH` — the R22 mutation surface stays closed),
  same `RUN_DEADLINE_MS` + kill discipline, same in-flight registry feeding
  the overlap probe and delete/shutdown/backstop kills. No
  `automation://agent-run` is emitted; the frontend never hears about the run
  except through `automation://changed`/`run-closed`.
- R5. Claim-before-run (automations R2), dispatch-off-the-store-lock (KTD-B),
  the **usage gate** (usage-limit-deferral plan — headless agent runs still
  spend the plan, so they stay gated), overlap skip-if-running (KTD-D), and
  `retry_on_interrupt` semantics all hold unchanged. The claim-time
  frontend-ready deferral is carved out for headless agent claims exactly as
  it is for monitors — there is no event to drop — and the retry drain
  likewise.

**Capture & close**

- R6. A **Failed** headless agent close surfaces through the alert pipeline
  (one sanitized line: automation name + error + run id), ringing the
  Automations sink pane / pending queue exactly like a script alert — this
  replaces the kept-failed-tab visibility. A Succeeded close is silent (tab
  auto-close parity). Monitor escalation (`check_monitor_escalation`) remains
  monitor-only.
- R7. `RunRow.output` is the stream `result` text through
  `redact::clean_captured` (sanitize → scrub, tail-cap last at close) and
  `RunRow.session_id` is stamped from the `init` event — both already the
  shared-close behavior; the transcript retry capture
  (`CAPTURE_ATTEMPTS`/`sole_transcript_since`) becomes unreachable for
  headless runs and stays only for explicitly-paned agent automations. The
  Stop-before-flush race and the busy-cwd capture abstention disappear for
  every headless run.
- R8. Non-monitor runs never verdict-parse, never retire, never count toward
  "monitor broken" — guaranteed by the existing `automation.monitor` gate in
  `close_run_with_capture`, pinned by test.

**Visibility**

- R9. The dashboard automations panel shows a running headless run honestly:
  the row's last-run read distinguishes a live headless run (e.g.
  `running · 2m` with elapsed time) — there is no tab or agent-list row to
  find, so the panel row is the primary surface, as it already is for monitor
  checks.
- R10. `fly automation runs` and `show` display the run's `sessionId` (and
  the derived transcript path in `runs -v` or equivalent) — the closed-loop
  debugging handle, closing the headless plan's second deferred item.
- R11. The feed's `AutomationEntry` gains an additive `headless: bool` (the
  monitor-enrichment back-compat convention: absent-field tolerant), so an
  external consumer can tell a closed-loop automation from a pane-spawning
  one.

**Hygiene**

- R12. New store/config/wire fields are `#[serde(default)]` camelCase and
  legacy-JSON round-trip tested. `CLAUDE.md` (Automations + Agent dispatch
  paragraphs) and `docs/plans/README.md` are updated when the work lands.

---

## Key Technical Decisions

- **Reuse the monitor runner wholesale; the row marker is the routing
  truth.** No second runner, no runner changes beyond naming: the
  headless-monitor plan deliberately keyed every sweep exemption, kill gate,
  and probe on `RunRow.headless`, not on the monitor flavor — so a regular
  agent run that claims with `headless: true` inherits all of it for free.
  The only monitor-specific behavior in the whole pipeline is the verdict
  parse, and it is already gated on `automation.monitor` inside the shared
  close tail (R8).
- **Resolution at claim, stamped once.** The sweep resolves effective
  headless (automation flag → config default) *before* the store lock — the
  usage-gate precedent for pre-lock config reads — and passes it into
  `Automation::claim`, which stamps `RunRow.headless = monitor || resolved`.
  Claim gains one argument; the alternative (dispatcher-side resolution)
  would let the row marker and the routing disagree, which the sweep
  exemptions cannot tolerate. Dispatch then routes on the **claimed row's**
  marker, threaded through the manager's `dispatch()` to
  `CompositeDispatcher.dispatch_agent`, whose monitor fork widens to
  `headless`.
- **Config default `true`, dispatch-time.** Per the direction decision:
  closed-loop is the norm, panes are the exception. Applying the default at
  dispatch (not baked in at create) means flipping the config knob flips
  every non-explicit automation at once — one lever, no per-automation
  migration. The behavior change on upgrade is deliberate and documented;
  `--paned` (or the knob) restores the old shape per-automation or globally.
- **Failure visibility rides the existing alert path.** The kept-failed-tab
  was the pane path's failure surface; its headless replacement is a
  sanitized alert line through the same `AlertsLog`/sink machinery scripts
  use (R16-sanitized, queued when no sink exists). This is one new call site
  on an existing seam, not a new channel — and it upgrades failure visibility
  from "a tab you might notice" to "the attention ring."
- **No agent-list rows for headless runs (v1 of this plan).** A closed-loop
  run is an automation, not an agent you can talk to — projecting it into the
  dashboard's Agents list (whose rows are jump targets into live panes) would
  advertise an interaction that cannot happen. The automations panel row +
  alert-on-failure + feed `lastStatus` are the honest surfaces. Revisit only
  if lived usage disagrees (recorded as deferred, not designed).
- **No new concurrency cap.** N simultaneously-due headless automations
  spawn N claude children, same as N pane automations spawned N panes today;
  per-automation overlap skip and the usage gate bound the realistic blast
  radius. The KTD-D capacity pattern remains available if real usage
  disproves this (risk noted).

---

## High-Level Technical Design

```mermaid
sequenceDiagram
    participant Sweep as Sweep (store lock)
    participant Cfg as ConfigStore (pre-lock)
    participant Mgr as AutomationManager
    participant HR as HeadlessRunner
    participant CC as claude -p (child)

    Sweep->>Cfg: resolve headless (flag → default) + usage gate
    Sweep->>Sweep: claim(…, headless) + flush (row marker stamped)
    Sweep->>Mgr: dispatch (lock released, KTD-B)
    Mgr->>HR: dispatch_agent forks on row.headless
    HR->>CC: spawn (clean env, deadline, registry)
    CC-->>HR: init → … → result
    HR->>Mgr: close_headless_run (cleaned text, session id)
    Mgr->>Mgr: shared close tail: non-monitor ⇒ no verdict parse
    Mgr-->>Mgr: Failed ⇒ alert line + ring; run-closed emit
```

---

## Assumptions

- The pane path stays useful enough to keep (explicit `--paned`, plus every
  pre-existing plan test): removing it entirely — and with it the transcript
  retry capture, KTD5 suppression, and ephemeral-tab machinery — is a
  follow-up cleanup once headless has lived a while, not this plan.
- A long-prompt agent run fits the existing 30-min `RUN_DEADLINE_MS`; no
  per-automation deadline knob yet (deferred, as it was for monitors).
- The 2.1.207 stream contract pinned by the headless plan still holds for
  the installed claude at implementation time; re-verify `init`/`result` once
  during U6 live validation rather than re-running the full probe suite.

---

## Implementation Units

### U1. Model & claim: the `headless` flag and its resolution

- **Goal:** the store model carries the flag; claim stamps the row from a
  caller-resolved value; resolution is one pure function.
- **Requirements:** R1, R3, R12.
- **Files:** `src-tauri/src/automations/model.rs` (+ in-module tests).
- **Approach:** `Mode::Agent` gains `headless: Option<bool>`
  (`#[serde(default)]`); a pure
  `Mode::resolved_headless(&self, default: bool) -> bool` (agent-only;
  scripts always false). `Automation::claim` gains a `headless: bool`
  argument; the row marker becomes `self.monitor || headless` for agent
  claims (monitors force true regardless of the argument — R3). Every
  existing claim call site threads the resolved value; monitor call sites may
  pass anything (forced).
- **Test scenarios:** legacy `{kind, prompt}` and `{kind, prompt, model}`
  rows round-trip with `headless: None`; explicit `false` survives a
  round-trip (it is an override, not an absence); claim with resolved true
  stamps the row on a plain agent automation; claim with resolved false on a
  monitor still stamps true; script claims never stamp.
- **Verification:** `cargo test --offline` model suite green; a pre-change
  store file loads unchanged.

### U2. Config & CLI

- **Goal:** the default knob and the create-time override exist and display.
- **Requirements:** R1, R2, R10 (display half), R12.
- **Files:** `src-tauri/src/config/schema.rs`, `src-tauri/src/cli/automation.rs`,
  `src-tauri/src/hooks/protocol.rs` (create envelope field, `#[serde(default)]`).
- **Approach:** `AutomationDefaults.headless: bool`, default `true` (R2
  rationale in the doc comment). `create --headless`/`--paned` (mutually
  exclusive; agent-only; `--headless` with `--monitor` rejected as redundant,
  `--paned` with `--monitor` rejected as contradictory — the `--not-before`
  validation pattern, enforced CLI-side *and* on the untrusted wire).
  `show` prints the automation's dispatch disposition (`headless` /
  `paned` / `default (headless)`); `runs` prints `sessionId` when stamped and
  the derived transcript path (`~/.claude/projects/<encoded-cwd>/<id>.jsonl`)
  under a verbose flag or unconditionally on `show` of a single run —
  closing the headless plan's deferred display item.
- **Test scenarios:** flag parsing incl. both rejections; wire envelope
  back-compat (old CLI posting no field ⇒ `None`); config default
  round-trip; `runs` output includes the session id for a stamped row.
- **Verification:** CLI tests green; `fly automation create --help` shows
  the flags.

### U3. Routing, gates, and close semantics

- **Goal:** a headless-resolved agent claim dispatches through the runner
  with every invariant intact, and a failed close rings.
- **Requirements:** R4, R5, R6, R7, R8.
- **Dependencies:** U1, U2.
- **Files:** `src-tauri/src/automations/mod.rs`, `src-tauri/src/lib.rs`
  (`CompositeDispatcher`, alert closure), tests in both +
  `src-tauri/tests/headless_runner.rs` (extend).
- **Approach:** The sweep (and manual-run, and retry-drain) resolves
  effective headless pre-lock alongside the usage-gate read and threads it to
  claim; `dispatch()` passes the claimed row's marker to the dispatcher;
  `CompositeDispatcher.dispatch_agent`'s fork widens from
  `automation.monitor` to the row marker. The frontend-ready claim deferral
  and retry-drain gate carve out headless-resolved agent claims (widening the
  existing monitor carve-out — same rationale, no event to drop). The usage
  gate is *not* touched (it keys agent-mode, which headless still is —
  asserted). Failed headless non-monitor closes call the alert seam with a
  sanitized `name: error (run id)` line before `run-closed` (queue/ring
  semantics identical to script alerts); Succeeded closes stay silent.
  Delete/shutdown/backstop kill gates need no change (row-marker driven —
  asserted, not assumed).
- **Test scenarios:** a due plain agent automation with config default true
  dispatches headless (no `agent-run` emission) and the row is marked; with
  explicit `paned` it still routes to the pane arm; manual run respects the
  same resolution; frontend-ready down defers a paned claim but not a
  headless one; usage-gated window skip-defers a headless occurrence exactly
  as a paned one; Failed headless close rings the alert sink once with a
  sanitized line and a Succeeded close rings nothing; non-monitor Succeeded
  close with a ` ```verdict ``` `-shaped output does NOT retire or escalate
  (R8 pin); overlap probe blocks the next claim while the registry entry is
  alive; delete mid-run invokes the headless killer.
- **Verification:** full `cargo test --offline` green; existing monitor and
  pane-path suites green unmodified.

### U4. Dashboard & feed visibility

- **Goal:** a running or finished headless run is legible where it lives.
- **Requirements:** R9, R11, R12.
- **Dependencies:** U1 (marker on the wire).
- **Files:** `src-tauri/src/automations/mod.rs` (dashboard DTO),
  `src-tauri/src/feed/wire.rs`, `src/lib/automations.ts`,
  `src/lib/HomeView.svelte`, `src/lib/feed.ts`.
- **Approach:** The `AutomationsDashboard` row and feed `AutomationEntry`
  gain additive `headless: bool` (absent-field back-compat, the
  monitor-enrichment convention). `automationsToRows` renders a Running last
  run with elapsed time (`running · 2m`) — the panel already refetches on
  `automation://changed`, which fires at claim, so the row goes live without
  new events; a finished run keeps today's `lastStatus` read. No Agents-list
  projection (KTD above).
- **Test scenarios:** vitest — `automationsToRows` renders the running
  elapsed form and falls back to today's strings for terminal rows; feed
  wire round-trip with and without the new field; `buildFeedPayload`
  untouched (automations ride the backend leg).
- **Verification:** `pnpm check` + `pnpm test:unit` green;
  `cargo test --offline` wire tests green.

### U5. Docs

- **Goal:** the repo's self-description matches.
- **Requirements:** R12.
- **Dependencies:** U3, U4.
- **Files:** `CLAUDE.md`, `docs/plans/README.md`,
  `docs/plans/2026-07-11-003-feat-headless-monitor-checks-plan.md` (tick the
  deferred items with a pointer here).
- **Approach:** Rewrite the Automations "Agent dispatch" sentences: agent
  runs dispatch headless by default (closed-loop; config
  `automation_defaults.headless`, per-automation override), pane dispatch is
  the explicit exception, failed headless runs ring the alert path. Flip the
  README row from Planned; annotate the old plan's Deferred list.
- **Verification:** grep for stale "spawns a `claude … ` agent pane" claims.

### U6. Live validation (dev flavor)

- **Goal:** one real closed-loop automation observed end to end.
- **Requirements:** R4–R7, R9 (observed).
- **Approach:** In `pnpm flavor:dev`: create a plain agent automation (no
  flags), manual-run it — observe no tab, dashboard row `running · …`, then
  Succeeded with output + sessionId in `fly automation runs`. Repeat with a
  prompt engineered to fail (bad cwd or forced nonzero) — observe the alert
  ring + log line. Create one with `--paned` — observe the old tab behavior
  intact. Re-verify the `init`/`result` stream shape once against the
  installed claude.
- **Verification:** checklist observed; deviations recorded in this plan.

---

## Acceptance Examples

- AE1. **Covers R1/R2/R4.** Given a plain `fly automation create` agent
  automation under default config, when it fires, then no tab or pane
  appears, no `automation://agent-run` is emitted, and the run closes with
  the stream result as output and a stamped sessionId.
- AE2. **Covers R6.** Given a headless agent run whose child exits nonzero
  with no result, when it closes Failed, then the Automations sink rings once
  with a sanitized line naming the automation and the error, and the
  dashboard row reads failed.
- AE3. **Covers R2/R5.** Given `--paned` on an otherwise-identical
  automation, when it fires, then the pane path behaves byte-identically to
  today — ephemeral tab, transcript capture, kept-open on failure.
- AE4. **Covers R8.** Given a non-monitor headless run whose output contains
  a well-formed ` ```verdict ``` ` block, when it closes Succeeded, then
  nothing retires, nothing escalates, and the block is just text in
  `RunRow.output`.
- AE5. **Covers R9/R10.** Given a running headless automation, when the
  dashboard is open, then its panel row shows a live running read with
  elapsed time, and after close `fly automation runs` shows the sessionId.

---

## Scope Boundaries

- Monitors: behavior-identical (dispatch, verdict, retire, bundles, broken
  escalation).
- The pane dispatch path, ephemeral-tab machinery, transcript retry capture,
  and KTD5 suppression all remain, reachable via `--paned`/config — removal
  is a follow-up cleanup, not this plan.
- No Agent SDK / sidecar; plain `claude -p` only (the framing decision:
  `claude -p` *is* the closed-loop chain surface from a Rust backend).
- No multi-step chains, no re-prompting, no conversation continuation — one
  prompt, one result, by design.

### Deferred to Follow-Up Work

- Removing the pane agent path entirely (and with it the retry capture and
  KTD5) once headless has proven out — the old plan's third deferred item,
  half-closed here.
- Projecting running closed-loop runs into the dashboard Agents list or feed
  `agents` roster — deliberately declined above; revisit on lived usage.
- A headless concurrency cap (KTD-D pattern) if fan-out proves real.
- Per-automation deadline override (30-min `RUN_DEADLINE_MS` for all).
- Multi-step chains / programmatic between-turn control (would motivate the
  actual Agent SDK + a sidecar; out of category for now).

---

## Risks & Dependencies

- **Default-flip on upgrade (deliberate).** Existing agent automations go
  headless when this ships; their failure surface moves from kept-tab to
  alert ring in the same release. Mitigation: R6 ships in the same unit as
  the routing (U3), never separately; CLAUDE.md states the flip.
- **Failure diagnosis without a tab.** A pane let you scroll the session; a
  headless failure gives the row error + output + sessionId. The transcript
  path (R10) is the deep-dive handle — `claude --resume <sessionId>` even
  works on it manually. Accepted; this is the monitor precedent.
- **Usage-gate interplay:** a headless run at an exhausted window is
  skip-deferred pre-claim exactly as today (asserted in U3); the deferred U6
  close-honesty pin from the usage-gate plan (`Stop ⇒ Succeeded`
  misclassification) does not apply to headless runs at all — a non-success
  `result` reads Failed, which is a small honesty *improvement* at the
  limit.
- Depends on as-built: `automations/headless.rs` (runner, registry, kill
  discipline), the shared close tail (`close_run_with_capture`), the alert
  seam (`surface_alert`/`AlertsLog`), the pre-lock config-read precedent
  (`usage/gate.rs`), stream contract pinned 2.1.207.

---

## Sources

- Deferred items + substrate:
  `docs/plans/2026-07-11-003-feat-headless-monitor-checks-plan.md`
  (Deferred to Follow-Up Work; Empirical Contract; the row-marker KTD this
  plan leans on).
- Direction decision: 2026-07-31 session — automation agents are closed-loop
  chains, never interactive panes (memory:
  `automation-agents-closed-loop-direction`).
- As-built anchors: `automations/model.rs::{Mode, Automation::claim, RunRow}`,
  `automations/mod.rs::{close_run_with_capture, close_headless_run,
  in_flight_widened, frontend_ready}`, `lib.rs::CompositeDispatcher`,
  `automations/headless.rs::HeadlessRunner`, `automations/alerts.rs`,
  `usage/gate.rs`, `lib/automations.ts::automationsToRows`,
  `feed/wire.rs::AutomationEntry`.
- Prior plans whose invariants this must preserve:
  `2026-07-01-002-feat-automations` (R2/KTD-B/KTD-D, R7 failed-tab, R22),
  `2026-07-03-002-feat-automations-workspace-and-model` (launch resolution,
  capture seam, auto-close R7), `2026-07-16-001-feat-automations-usage-limit-deferral`
  (gate placement), `2026-07-05-001-feat-automations-interrupt-resilience`
  (retry semantics).
