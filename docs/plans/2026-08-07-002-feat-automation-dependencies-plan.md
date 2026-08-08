---
title: "feat: Automation dependencies — `fly automation create --after <id>`"
type: feat
date: 2026-08-07
status: implemented
---

# feat: Automation dependencies — `fly automation create --after <id>`

## Summary

Automations gain one edge type: a **dependent** automation declares an
**upstream** automation (`--after <id>`) and its scheduled occurrences fire
only against a *fresh, successful, not-yet-consumed* upstream run. When the
upstream hasn't finished yet, the dependent **waits** (bounded); when the
upstream failed, was skipped, is stale, or never ran, the dependent records an
honest, inspectable **withheld** run row saying exactly why — never a lying
green row over stale data, never a `failed` that reads as "the dependent
broke".

Motivation: fly automations are leaf nodes by construction — both dispatch
paths strip `FLY_PANE_TOKEN`/`FLY_SOCKET_PATH` (script runner R14,
`script.rs`; headless runner R13, `headless.rs`), so no automation can trigger
another, and the only scheduling input is cron. The production feed pipeline
therefore encodes a guess: the analysis leg fires at 09:22 because the modal
leg *usually* finishes by then, and it already misfires (run `j1xkxkfbhn`:
`skipped — run in flight`; failed modal runs let the analysis chew yesterday's
digest). The credential strip is deliberate security posture and is **not
touched** — the fix is a scheduler feature. This edge is needed ~8 more times
for the planned campaign DAG (`../secure/docs/campaign-dag-gaps.md` §1, §4.1,
§6 — step 2 of its suggested order is exactly this plan).

Origin: `../secure/docs/campaign-dag-gaps.md` (2026-08-07 design capture).

---

## Problem Frame

The valuable property is **not** "B runs after A" — cron already approximates
that. It is that a dependent which *declines* to run produces an honest record
of why, so a human reading the dashboard learns "today's upstream was
truncated, so the analysis did not run" instead of seeing a green row over
stale work. Three consequences drive the design:

1. **The decline needs its own terminal state.** `failed` means "the dependent
   broke"; `skipped` is already overloaded (run-in-flight, capacity, usage
   limit). A new born-terminal status carries the explanation.
2. **Exactly one dependent run per upstream run.** This is precisely the shape
   that produced the automations plan's U7.5 fan-out landmine. Consumption
   must be recorded atomically with the claim and pinned by test.
3. **Freshness is part of correctness.** A dependent must not fire at 15:00
   off a 09:00 upstream run, and must not fire twice off one run just because
   its own cron ticks faster.

## Key Technical Decisions

- **KTD1 — the dependency is a precondition on the dependent's own cron,
  evaluated by the sweep; not an event edge fired from the upstream's close.**
  (Q5.) The dependent keeps a required `--cron`; `sweep_once` evaluates the
  predicate at the due occurrence. Chosen over a close-time trigger because:
  the sweep is the single scheduler heartbeat (KTD-C of the automations plan)
  and already owns claim/persist/dispatch discipline; a close-time trigger
  would add a second dispatch producer threading through every close path
  (pane Stop, headless close, backstop, shutdown) with its own re-entrancy
  hazards on the store lock; and `next run` keeps an honest meaning. The
  event-latency cost is bounded by the sweep tick (10 s), not by cron: an
  unsatisfied due occurrence **defers** — no row, no advance, the R5
  frontend-gate precedent — re-evaluating every tick until the window closes,
  so the dependent fires within ~10 s of the upstream's success. `next run`
  in `list` means "next evaluation instant"; a currently-deferring dependent
  renders `waiting on upstream`.
- **KTD2 — the honest decline is a first-class terminal state:**
  `RunStatus::Withheld`, born terminal like `Skipped` (no `Running →
  Withheld` edge), reason in `RunRow.error`, specific and chain-propagating
  ("upstream failed (timed out)", "upstream was withheld: upstream has not
  run", "upstream stale (last success 6h before the window)"). Every withhold
  rings one sanitized line through the existing Automations alert path (the
  headless-failure precedent — visibility without a new attention producer).
- **KTD3 — exactly-once by consumption stamping.** (Q4.) A dependent's
  claimed row stamps `RunRow.upstream_run_id` (the consumed upstream run's
  id) **in the same store mutation as the claim**; the predicate refuses any
  upstream run id already stamped in the dependent's bounded history. Both
  writers of a claim (sweep, manual run) decide-and-stamp under one
  `store.mutate` hold, so no interleaving can double-consume (the U7.5
  lesson: link atomically or fan out).
- **KTD4 — one symmetric `within` window answers both staleness and wait.**
  (Q3.) For a scheduled occurrence at `T` with window `W` (default 60 min,
  `--within`): a qualifying upstream success must have finished at or after
  `T − W`, and the dependent defers until `T + W` before recording the
  withhold. One knob, both directions; measured against the *occurrence*, so
  deferral doesn't slide the staleness bound. A manual upstream re-run inside
  the window satisfies a deferring dependent naturally (it is simply a new,
  unconsumed success).
- **KTD5 — upstream success = `Succeeded` ∧ not a FAIL verdict; the
  author-supplied gate predicate is a reserved, additive slot.** (Q1.) v1
  reads run status (script exit 0 / agent clean close), refusing a
  `Succeeded` row carrying a FAIL verdict (the honest-verdict rule from the
  feed-monitor-enrichment note). Exit-0-is-too-weak (the modal leg's
  `truncated: true` sentinel) is real but its sentinel is itself still to be
  built (campaign-dag-gaps §6 step 1) — so the dependency is a **struct**
  (`Dependency { upstreamId, withinMs }`, all-`#[serde(default)]` fields),
  and a future `gate` field (a predicate command run at claim time, exit 0 =
  pass) lands additively with no schema break.
- **KTD6 — cycles are unconstructible through the API; create validates
  anyway; the sweep is single-hop.** (Q6.) Edges are create-only (there is no
  automation-update op), so no create can close a cycle — the new record has
  no dependents yet. The create-time walk still climbs the upstream chain
  (depth cap 8, visited-set cycle rejection) because the store file is
  hand-editable JSON. Defense in depth: the sweep predicate reads exactly one
  hop (the named upstream's run history), so even a hand-edited cycle can
  never recurse or hang the scheduler — the members merely defer and then
  withhold, each row honestly naming its upstream.
- **KTD7 — the credential strip is untouched.** No change to script R14 /
  headless R13 env stripping, no change to `feed/` auth, no new socket
  capability: `--after` rides the existing `automation/create` envelope as
  two optional fields, with the same untrusted-wire re-validation posture as
  `monitor`/`headless`.
- **KTD8 — back-compat posture.** All new store/wire fields are additive
  `#[serde(default)]` camelCase (legacy rows load; `upstreamRunId` is
  skip-if-none so non-dependent rows serialize byte-identically). The one
  deliberate extension is the new `RunStatus` **value** `"withheld"`: an
  *older fly binary* loading a store that already contains a withheld row
  fails the whole-map parse and degrades via the R6 `.bad.bak` path. Accepted:
  fly has no supported downgrade path, the value appears only after a
  dependent actually withholds, and the forensic bytes are preserved. Feed
  consumers see `"withheld"` as a new `lastStatus` string — the feed contract
  already requires tolerating unknown values.

## Requirements

**Vocabulary & store (U1)**

- R1. `Automation` gains `after: Option<Dependency>` (`#[serde(default)]`,
  camelCase); `Dependency { upstream_id: String, within_ms: Option<u64> }`
  with `within_ms = None` meaning `DEFAULT_AFTER_WITHIN_MS` (60 min). Legacy
  rows load unchanged; a record without `after` behaves exactly as today.
- R2. `RunStatus` gains `Withheld` (born terminal; `is_terminal() == true`;
  no edge out of `Running` into it). `Automation::withhold` mirrors
  `Automation::skip` (KTD-D: the schedule advance stays the caller's separate
  `rollback_recompute` step); `RunOutcome` deliberately gains **no** withheld
  variant.
- R3. `RunRow` gains `upstream_run_id: Option<String>` (`#[serde(default,
  skip_serializing_if = "Option::is_none")]`), stamped only on a dependent's
  claimed rows (`Automation::stamp_upstream`, called in the same mutate as
  the claim — KTD3).

**The predicate (U2, pure — `automations/depend.rs`)**

- R4. `evaluate(dependent, upstream: Option<&Automation>, occurrence_ms,
  now_ms) → Satisfied { upstream_run_id } | Wait | Withhold { reason }`.
  Qualifying run: newest upstream row with `status == Succeeded`, no FAIL
  verdict, `finished_at ≥ occurrence − within`, and id not already consumed
  by any row in the dependent's history. No qualifying run → `Wait` while
  `now < occurrence + within`, else `Withhold`.
- R5. Withhold reasons are specific and derived from what the upstream
  actually did in the window, checked in this order: upstream missing
  (deleted) — immediate, no wait; upstream still running at the deadline;
  newest in-window terminal row failed / was skipped / was itself withheld
  (the reason quotes the upstream's own reason — chain propagation) / already
  consumed / carried a FAIL verdict; success exists but is stale; upstream
  has never run. Reasons are fly-minted strings (control-safe by
  construction) and unit-tested.
- R6. Saturating arithmetic throughout (`within` and timestamps are untrusted
  numeric input; release builds have overflow checks off).

**Create & delete (U3)**

- R7. `CreateSpec`/`AutomationRequest` gain `after`/`within_ms`
  (create-only, additive on the wire). The manager's create validates on the
  untrusted payload: upstream must exist; upstream must not be a monitor
  (a monitor retires after one verdict — a dependent would wither forever;
  rejected in v1, revisit if a real use appears); `--after` + `--monitor` on
  the dependent rejected (a monitor is a parked experiment, not a pipeline
  stage); `within_ms` clamped to [1 min, 7 days]; the upstream **chain** walk
  (KTD6) rejects depth > 8 and any revisit (cycle). Fan-out (many dependents
  on one upstream) is allowed and safe by construction — each dependent
  tracks its own consumption. Fan-in (multiple upstreams per dependent) is a
  **non-goal** of this plan.
- R8. Deleting an upstream is allowed (no cascade, no refusal); its
  dependents thereafter withhold with "upstream no longer exists". The delete
  response carries an advisory warning naming the dependents left dangling.
  (The upstream-exists check at create is TOCTOU-racy against a concurrent
  delete; the sweep's missing-upstream withhold is the honest backstop.)

**The sweep & manual runs (U4)**

- R9. Gate order for a due dependent, pinned by doc comment and test: R5
  frontend-ready deferral → R7 overlap skip → **dependency predicate** →
  usage gate → claim. `Wait` leaves the automation completely untouched (no
  row, no advance — the R5 precedent); `Withhold` appends the terminal row,
  advances the schedule past the occurrence (KTD-D), emits
  `automation://changed`, and rings one alert line (`withheld: <reason>
  (run <id>)`) through the existing sink after the lock (KTD-B).
- R10. The predicate is evaluated **inside the sweep's single mutate hold**
  via a read-only pre-pass over the map (upstream lookups need the map while
  the claim loop holds `values_mut` — two passes, one lock, atomicity
  preserved). Dispatch stays off the lock; a `Satisfied` claim stamps
  `upstream_run_id` in the same mutation (KTD3).
- R11. The usage-gate pre-lock peek counts a due dependent as "agent claim
  possible" only when its predicate currently reads `Satisfied` — a
  deferring dependent must not put the OAuth usage fetch on a timer for up
  to `within` (KTD-C: no dispatch-less tick fetches). Benign snapshot TOCTOU,
  same as the existing peek.
- R12. A **manual run** of a dependent evaluates the same predicate
  (`Satisfied` → runs, consuming the upstream run like a scheduled fire;
  otherwise → appends a Withheld row with the same honest reason and reports
  it synchronously — `ManualRun::Withheld`, CLI prints `withheld: <reason>`).
  Manual is *not* an override in v1: the correct recovery is to re-run the
  upstream (which naturally satisfies the edge within ~10 s via deferral or a
  fresh manual dependent run). No `--force`; revisit if operations demand it.
- R13. (Q7.) An interrupted **upstream** re-run (`Trigger::Retry`) that
  succeeds is a new run id ⇒ at most one dependent fire against it (KTD3);
  the dependent can never double-fire for one upstream occurrence across
  {original, retry}. An interrupted **dependent**'s own retry **bypasses**
  the predicate (its occurrence already legitimately consumed the upstream
  run; the retry re-attempts the same work) and inherits the interrupted
  row's `upstream_run_id`, so consumption history stays truthful and a later
  scheduled occurrence still refuses that upstream run.

**CLI (U5)**

- R14. `fly automation create --after <id> [--within <dur>]` — `<dur>` =
  bare minutes or `Nm`/`Nh`/`Nd`; validated client-side and clamped
  server-side (R7). Old-server skew: like `--monitor`/`--headless` before
  it, an old app silently ignores the unknown fields and creates an ungated
  automation — same accepted risk, same single-install blast radius.
- R15. `list` marks dependents (`after:<id>`) and renders `waiting on
  upstream` in the next-run column while deferring (due `after`-automation
  whose occurrence is in the past); `show` prints the edge (`after: <id>
  (<name>) — requires upstream success within 60m`) and flags a dangling
  upstream; `runs` renders `withheld` rows with their reason (the existing
  error column). `status_label` gains `"withheld"`.

**Feed & dashboard (U6)**

- R16. `feed::wire::AutomationEntry` gains additive `after: Option<String>`
  (the upstream id) and `last_withheld_reason: Option<String>` (set only
  when the last run is withheld; fly-minted, control-safe). `lastStatus` may
  now read `"withheld"`. Absent-field back-compat preserved; `feed.ts`
  mirror updated.
- R17. The dashboard panel renders a withheld last run with its reason, an
  `after:<id>` tag on dependent rows, and `waiting on upstream` in the
  next-run cell while deferring (`lib/automations.ts` derives it —
  `nextRunAt` in the past + `after` set + enabled). No new interaction.

**Validation (U7)**

- R18. Live check in the dev flavor (`pnpm flavor:dev`, isolated
  `FLY_APP_NAME=fly-dev` store — never the installed app): a script upstream
  + script dependent pair exercising fire-after-success, withhold-on-failure,
  and the waiting read on the dashboard/CLI. The production
  `tn75oqx8wx → jnlnulcs4v` pair is **not** rewired by this plan.

## Units of Work

- **U1 — model vocabulary** (`automations/model.rs`): `Dependency`,
  `Automation.after`, `RunRow.upstream_run_id`, `RunStatus::Withheld`,
  `Automation::{withhold, stamp_upstream}`; serde round-trip + legacy-load
  tests (R1–R3, KTD8).
- **U2 — the pure predicate** (`automations/depend.rs`, new): `evaluate` +
  reason derivation + `DEFAULT_AFTER_WITHIN_MS`; exhaustive unit tests
  (R4–R6, KTD4/KTD5).
- **U3 — create/delete integration** (`automations/mod.rs`, `lib.rs`
  dispatch arm): `CreateSpec.after`, wire re-validation, chain walk, delete
  advisory (R7–R8, KTD6/KTD7).
- **U4 — sweep + manual run** (`automations/mod.rs`): pre-pass decisions,
  gate order, defer/withhold/claim+stamp, alert line, usage-peek refinement,
  retry semantics, `ManualRun::Withheld` (R9–R13, KTD1–KTD3).
- **U5 — CLI** (`cli/automation.rs`): flags, `--within` parse, request
  fields, list/show/runs rendering (R14–R15).
- **U6 — feed + dashboard** (`feed/wire.rs`, `feed/mod.rs`, `lib/feed.ts`,
  `lib/automations.ts`, `HomeView.svelte`): additive projection + rendering
  (R16–R17).
- **U7 — live validation** in the dev flavor (R18).

## Risks

- **Old-binary store downgrade** (KTD8): a withheld row makes the store
  unreadable to a pre-plan binary (`.bad.bak` degrade). Accepted — no
  downgrade path is supported; documented here and in the model doc comment.
- **Silent CLI/app skew on create** (R14): an old app ignores `after`. Same
  accepted posture as `monitor`; the operator sees the missing edge in
  `show`.
- **A dependent whose upstream never succeeds rings daily.** Deliberate —
  that is the visibility the framing note asks for; pausing the dependent is
  the operator's mute.

## Validation

```
cargo test --offline --manifest-path src-tauri/Cargo.toml
pnpm check && pnpm test:unit
```

Required tests (names encode the behavior, automations-suite convention):
dependent-does-not-fire-when-upstream-failed, exactly-one-dependent-run-
per-upstream-occurrence, stale-upstream-refused, cycle-rejected-at-create,
legacy-rows-load-without-the-new-field — plus the R9 gate-order pin, the R13
retry pair, the manual-withhold path, and the feed/CLI projections.

---

## Addendum — 2026-08-08: an update op exists; KTD6 survives with one word changed

`docs/plans/2026-08-08-001-feat-automation-update-plan.md` added
`fly automation update` (the `automation/update` socket op), so KTD6's
parenthetical "there is no automation-update op" is no longer literally true.
The argument it supports is unchanged, because update **cannot set or
re-point an edge** — `after`/`within_ms` on an update payload are refused
outright (`ERR_UPDATE_SET_AFTER`), and the only edge mutation it offers is
`--no-after`, which *clears* one.

Read KTD6 as: **edges are set at create only; update may only clear them.**
Clearing can never close a cycle, so "no create can close a cycle — the new
record has no dependents yet" still covers every reachable state, and the
create-time chain walk (depth cap 8, visited-set rejection) remains the
defense-in-depth against a hand-edited store file. The single-hop sweep
predicate is untouched.

One further interaction worth stating: an update **preserves the id and the
run history**, which is exactly what the delete + recreate workaround
destroyed — a recreate minted a new id (silently orphaning any dependent's
`after` edge) and reset the R8 run history that this plan's freshness
predicate and `consecutive_infra_failures` both read. Re-tuning a pipeline
stage no longer breaks its dependents.
