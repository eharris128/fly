# fly primitives for the CVE-outreach DAG — design note (G1–G9)

**Status:** design, 2026-08-08. G1 implemented (this plan); G2–G9 proposed, not
built. Written from the brief `../secure/docs/agent-prompts/fly-dag-primitives.md`
after reading the two sibling repos' own gap registers
(`../secure/docs/campaign-dag-gaps.md`, `../secure/docs/candidate-pipeline-ledger.md`)
and the fly source they cite.

**Scope of this repo's part.** fly *schedules* stages, *chains* them on run
outcome (`--after`), *gates* (withholds), and *surfaces* state. The DAG's
*judgement* — range locks, spend decisions, send authorization — stays in the
repos next to the evidence (`campaign-dag-gaps.md` §3; Non-goal 3 below). Every
primitive here is measured against that line: if it would make fly decide
something about a *candidate*, it is out of scope; if it makes fly represent a
*run* more honestly, it is in.

**The six live automations (do not rewire — propose, let the operator migrate):**

| id | name | mode | schedule | cwd |
|---|---|---|---|---|
| `9ukbpue112` | feed triage | script | `0 9 * * 1-5` | feed |
| `ksdnx5whgt` | feed triage analysis | agent | `5 9 * * 1-5`, `--after 9ukbpue112 --within 90m` | feed |
| `4341og0pjb` | feed stage 5 (gate-1 handoff) | script | `0 10-18 * * 1-5`, `--after ksdnx5whgt --within 90m` | feed | *(was `wk5dz9yav2`, polling; recreated 2026-08-08 when feed adopted G1)*
| `nqcesrj1qq` | secure handoff intake (feed→secure) | script | `0 10-18 * * 1-5` | secure |
| `sjbrjb2pgh` | secure range leg (gate-2 opener) | agent | `30 10-17 * * 1-5`, `--after nqcesrj1qq --within 90m` | secure |
| `hctsfexvz3` | secure lab leg (stage 3 calibration) | agent | `0 12,16 * * 1-5`, `--after sjbrjb2pgh --within 4h` | secure |

---

## Non-goals (invariants every primitive here inherits)

Carried verbatim from the brief; they bound the whole note.

1. **A human gate must never auto-resolve.** No timeout-approve, no quorum, no
   default-proceed. (`candidate-pipeline-ledger.md` §4 is the argument.)
2. **Declining is not failing.** G1's whole point is a *correct* no-op that
   neither fires the dependent nor alerts a human. It is never a failure with a
   friendlier label.
3. **fly must not acquire judgement.** Verdicts are about **runs** (did this run
   do work / break / decline), never about a candidate. A fly feature that
   records a verdict *about a candidate* is out of scope.
4. **Do not make silence look like success.** Both repos fail closed (a missing
   sentinel, an unreadable ledger read as "did not run", never "nothing to do").
   Every new primitive keeps that bias.
5. **Exactly-once consumption must remain expressible.** `upstream_run_id` is the
   pattern both repos copied by hand; nothing here regresses it.

---

## Priority order

| # | Gap | Size | Retires |
|---|---|---|---|
| **G1** | verdict-gated non-monitor runs + `DECLINED` outcome | **~S (built here)** | 2 agent Step-0 gates; unblocks retiring 2 polls (with G5) |
| G6 | agent-mode timeout | S | a fly-source edit per timeout change; a real safety hole (a hung `docker compose up` leaving a vulnerable WordPress up) |
| G4 | script-drift warning on `fly automation show` | S | silent divergence of 2 versioned repo scripts from what runs |
| G7 | `fly automation graph` + staleness | M | a morning spent reading two ledgers in two repos |
| G5 | stable `--after <name>` addressing | M | id churn across 4 docs + 2 skills on every recreate |
| G8 | `--on-miss` catch-up policy (after documenting current behaviour) | S–M | a silently dropped weekday = a day's ~40 candidates |
| G3 | path/commit trigger | M (or *decline*) | the feed→secure hourly poll — **only if** it beats a poll+marker, which it may not |
| G9 | document manual-vs-`--after` + cross-`cwd` chaining | XS | two recurring "is this supported?" questions |
| G2 | blocked-on-human run state | — | **recommend fly does NOT own this** (see G2) |

**G1 first by a wide margin** — small, nearly-existing code, and it is the one
that turns "the agent legs cannot say they had nothing to do" from a
three-places-hand-built workaround into a native fact. The rest are genuine but
each is either smaller leverage (G6, G4), visibility (G7), ergonomics (G5), a
policy decision to *make* rather than build (G8), a likely-decline (G3), or a
doc task (G9). G2 is a recommendation to build nothing.

---

## G1 — the dependency predicate reads a verdict nothing can give it *(implemented)*

**Current behaviour.** `depend.rs::qualifies` (`src-tauri/src/automations/depend.rs:128`)
already respects an upstream verdict:

```rust
r.verdict.as_ref().map_or(true, |v| v.outcome != VerdictOutcome::Fail)
```

But `mod.rs::close_run_with_capture` (`src-tauri/src/automations/mod.rs:1828`)
parses a verdict **only for monitors**:

```rust
let parsed = if automation.monitor && automation.retired_at.is_none()
    && matches!(outcome, RunOutcome::Succeeded { .. }) { … } else { None };
```

and `depend.rs::validate_chain` (`:203`) **rejects a monitor as an upstream**.
The two sets are disjoint: for every automation that can legally be an upstream,
`r.verdict` is always `None`, so the `!= Fail` clause is dead. The repos state
the same conclusion from the outside — `campaign-dag-gaps.md` §6-step-5: *"the
one channel that turns agent text into run state — the fenced `verdict` block —
is parsed for monitors only, and `depend.rs::validate_chain` rejects a monitor
upstream outright."*

**What it costs downstream.** An agent run's status is process-completion, so *a
leg that correctly declines records `Succeeded`* and the dependent cannot tell
"did the work" from "had nothing to do". Worked around three times:

- `feed/gate1_handoff.py` refuses `--after` and **polls hourly** with a
  `last_consumed` marker (its docstring calls it re-implementing fly's
  `upstream_run_id`).
- `secure/scripts/handoff_intake.py` — the same, keyed on a content digest in
  `meta.handoffs`.
- Both agent legs (`sjbrjb2pgh`, `hctsfexvz3`) carry a hand-written "Step 0
  fail-closed gate" in their skills: read a ledger, decline if there is no work,
  and *hope* the operator tells a decline from a success.

Note `DECLINED` is already a *lived* concept downstream: feed's `::sync` guard
exits **4** with sentinel `status: "declined"` (`campaign-dag-gaps.md` §4.4) —
"nothing failed; the local files are intact." G1 gives fly the same word.

**The primitive (built).** Two additions:

1. **A third verdict outcome, `DECLINED`.** PASS/FAIL is not enough for a
   pipeline:

   | verdict | means | dependent | alert? |
   |---|---|---|---|
   | `PASS` | did the work | **fires** | — |
   | `DECLINED` | nothing to do — correct | **does not fire** | **no** |
   | `FAIL` | tried and broke | does not fire | **yes** |

   `DECLINED` is the missing one and the whole point: today it must be spelled
   as a lie (`PASS` → dependent fires on nothing) or a slander (`FAIL` → alerts a
   human about a correctly-idle stage).

2. **Parse verdicts for verdict-gated non-monitor runs.** Opt-in per automation
   via `verdict_gated: bool` (agent-mode only), set by `--verdict-gated` on
   **both `create` and `update`** — the latter matters so the operator adds it to
   the existing range/lab legs *in place*, no recreate (respects G5). A gated
   non-monitor Succeeded close parses the fenced block and **stamps the verdict on
   the row without retiring** (retiring stays monitor-only).

**KTD1 — opt-in is explicit, never inferred.** The brief allowed inferring
verdict-gating from the presence of a block. Rejected: an existing live leg that
happens to print a ```` ```verdict ```` block would silently start gating its
dependent — a change to a running automation's semantics, which the brief
forbids ("Do not rewire the live automations"). An explicit flag means the six
live automations are byte-for-byte unaffected until the operator opts each in.
`verdict_gated` is `#[serde(default = false)]`, so old stores and old CLI
binaries read as ungated.

**KTD2 — `qualifies` becomes "absent or PASS", and it is a no-op on today's
data.** `map_or(true, |v| v.outcome != Fail)` → `map_or(true, |v| v.outcome ==
Pass)`. For every existing row `verdict` is `None` → `true` → still qualifies, so
nothing changes for the six live automations. Once a gate is opted in: PASS
qualifies; FAIL and DECLINED do not.

**KTD3 — a decline is a distinct decision, not a withhold.** A new
`DepDecision::Declined { reason }` sits beside `Withhold`. Both record a
born-terminal row and advance the schedule, but **`Declined` does not ring the
alert** and `Withhold` does (dependencies R9). Modelling it as a separate variant
(rather than a `silent` bool on `Withhold`) keeps the existing exact-match
`Withhold { reason }` test suite intact and reads honestly at the call site.

**KTD4 — decline propagates through a chain (A→B→C).** When B stands down because
its upstream declined, B records its own decline row so C also stands down
silently. The propagation signal reuses `RunStatus::Withheld` **plus a
`Declined`-outcome verdict stamped on that row** — no new `RunStatus` (which would
compound the dependencies plan's accepted back-compat break). A dependent reading
its upstream's newest in-window terminal row distinguishes:

- Succeeded **+ `DECLINED` verdict** → the upstream ran and declined → `Declined`
  (silent);
- Withheld **+ `DECLINED` verdict** → the upstream itself propagated a decline →
  `Declined` (silent);
- Withheld / Failed / Skipped / stale / consumed / missing (**no `DECLINED`
  marker**) → `Withhold` (alert) — exactly as today.

**KTD5 — inside the window a gated dependent still `Wait`s.** A decline is
detected only at the wait deadline; inside the window a later manual PASS re-run
of the upstream could still land, so the occurrence is left untouched and
re-evaluated each tick — identical to the existing failed-upstream path. Harmless
for a decline (the dependent simply stands down a little later).

**KTD6 — monitors never retire on `DECLINED`.** The enum gains a variant that a
monitor could in principle emit. The monitor close path filters it: only PASS/FAIL
retire a monitor; a `DECLINED` from a monitor is treated as a not-done check
(abstain), consistent with monitors being a done/not-done instrument.

**What it lets the repos delete.** The two agent legs' hand-written Step-0
fail-closed gates become a single line at the end of the prompt: *emit
`DECLINED` when there is no work.* The dependent then does the right thing
natively. Second-order: with `DECLINED` expressible, the hourly polls in the
table can become chained where their producer is a *run* (not the human seam of
G3) — but **do not remove the repos' exactly-once markers**: they are also crash
protection (Non-goal 5). They stop being the *only* mechanism.

**Tests (this plan):** `depend.rs` — a gated upstream's `DECLINED` yields
`Declined` (silent) not `Withhold`; a `FAIL` still `Withhold`s + would alert; a
`PASS` `Satisfies`; decline propagates A→B→C; the existing suite is unchanged
(the `qualifies` edit is a no-op on `None`). `verdict.rs` — `DECLINED` parses;
abstain-on-surprise/echo-guard/two-block rules extend unchanged.

---

## G2 — no `blocked-human` run state — **recommend fly does not own this**

**Current behaviour.** fly's run states are `succeeded / failed / skipped /
running / withheld` (`model.rs:229`). None is "parked, waiting on a person, not
an error, resumable out of band."

**What the repo built.** `secure/scripts/pipeline_ledger.py` + a dashboard Gates
tab (`/api/gates`, `/api/gates/approve`). Crucially, `candidate-pipeline-ledger.md`
§1 **concludes the state belongs there, not in fly**: *"blocked-on-human is not a
mechanism to invent. It is a status on a per-candidate row … fly never needs to
represent it — which is the point."* The two human gates carry evidence fly must
not hold — Gate A's request object is the claimed range + primary evidence + the
lab result + any disagreement; Gate B's is population, contactable count, language
mix, gov/edu holds, the notice-page text (§4). That is candidate judgement
(Non-goal 3).

**Recommendation: build nothing here.** A fly `blocked` run state would either
duplicate the ledger (the "two truths" failure the transport doc warns against)
or be a thin state carrying no evidence — in which case the operator still opens
the ledger to act, and fly gained a state that decides nothing. If a *thin
notification* is ever wanted (a dashboard nudge "N candidates await review, oldest
X days"), it should **read from** the ledger's `blocked-human`/`gate`/`since`
vocabulary, never invent a parallel one, and it must inherit §4's constraints (no
timeout-approve, no quorum, no default-proceed). fly's existing `Withheld` is a
*born-terminal* sibling — a parked/resumable gate state is a different animal and
its natural home is the repo that holds the evidence. **Do not build a fly gate
state without a concrete need the ledger cannot serve.**

---

## G3 — no trigger on an artifact appearing — **likely decline**

**Current behaviour.** `--after` chains on an upstream *run*. The feed→secure edge
has no upstream run: its producer is a **human** Gate-1 session that writes
`feed/docs/handoffs/<slug>-<CVE>.md` and commits it (`handoff-transport.md`, "Why
the consumer polls instead of being chained"). So `nqcesrj1qq` polls hourly.

**Proposed primitive.** A path/glob trigger (`--on-path 'docs/handoffs/*.md'`,
debounced) or a git-commit trigger, making the edge event-driven.

**Recommendation: keep polling; document why.** Polling with an exactly-once
content-digest marker is *robust*, and a filesystem watcher that misses an event
while fly is closed is strictly worse than a poll that catches up on next tick.
`handoff-transport.md` and `candidate-pipeline-ledger.md` §3 both already chose
poll-with-digest deliberately. Build a trigger only if a producer appears that is
a *machine* writing at a rate where hourly latency hurts — not the human seam,
which is the only artifact producer today. This is a "keep polling, and here is
why" answer, which the brief names as a useful outcome.

---

## G4 — a script automation is a *copy*, so the repo version silently drifts

**Current behaviour.** `create` copies the script body into the private store
(`Store::put_script` → `automation-scripts/<id>/script`) and stamps *that* path
into the record; it never re-reads the `--script-file` path
(`campaign-dag-gaps.md` §4.2). **Editing the tracked repo copy changes nothing
until delete-and-recreate.** `feed/scripts/stage5-cron.sh` and
`secure/scripts/handoff-intake-cron.sh` are reviewable, testable, and *not* what
runs.

**Proposed primitive (cheaper of two).** A **drift warning** on `fly automation
show`: when the record carries the originating `--script-file` path and that file
still exists, compare its bytes to the stored body and flag "stored body differs
from `<path>`". Requires storing the *source* path at create time (today only the
private store path is kept). The alternative — a "re-read from path each run"
mode — is more powerful but re-introduces the exact trust problem the copy avoids
(the app opening a client-controlled path at dispatch, R21) and couples a run's
behaviour to whatever is on disk that morning. The warning is cheaper and, per
§4.2 ("Diff the two if you suspect drift"), is what the operator actually does by
hand. **Recommend the warning.**

---

## G5 — recreation churns ids, and ids are embedded in prose

**Current behaviour.** Because of G4, changing a script means delete-and-recreate,
minting a **new id**; and deleting an upstream **orphans its dependents**, forcing
*their* recreation too. That cascade happened 2026-08-07: `pzi426458k → 9ukbpue112`
forced `urfyodia11 → ksdnx5whgt` (`campaign-dag-gaps.md` §4.4 tail). Six
automations carry ids across four docs and two skills.

**What already helps.** `fly automation update` landed 2026-08-08 (this repo's
`2026-08-08-001` plan) — it patches name/schedule/prompt/script *in place*, keeping
the id and dependency edge, which removes the *most common* recreate reason. G1
adds `--verdict-gated` to `update` for the same reason. So the churn surface is
already shrinking.

**Proposed primitive.** Make **`--after <name>` legal** (stable operator-chosen
names as the addressing primitive, resolved to the id at evaluation) so a
dependency survives an upstream recreate; and/or **re-point dependents on delete**
instead of orphaning them. Names are already unique-ish operator handles; the risk
is name collisions and rename semantics (does `--after` bind to the name or snap
to an id at create?). **Recommend: after G1/G6/G4**, because `update` already
retired the script-edit recreate, which was the dominant trigger. Size M — it
touches the create/validate/sweep resolution path and needs a rename story.

---

## G6 — agent mode has no timeout

**Current behaviour.** `--timeout` is script-mode only; agent runs use the
un-flagged `mod.rs::RUN_DEADLINE_MS` (raised 30 → 90 min on 2026-08-07,
`campaign-dag-gaps.md` §4.4). The lab leg (`hctsfexvz3`) drives Docker and took
9.7 min on its first real run; a hung `docker compose up` has no per-automation
ceiling and **leaves a deliberately vulnerable WordPress install running while it
hangs**. This is a safety property, not tidiness.

**Proposed primitive.** Accept `--timeout` for agent mode too, validated against
the same `TIMEOUT_MAX_MS` ceiling (refuse over-ceiling, never clamp — the §4.4
trap), applied as the run deadline for that automation's headless/paned dispatch
in place of the global default. Small: the deadline machinery exists; this makes
it per-automation and flag-settable, and lifts the agent-only rejection on
`timeout_ms`. **Recommend second after G1.**

---

## G7 — no view of the chain, no staleness signal

**Current behaviour.** `fly automation list` prints `after:<id>` per row; there is
no graph and nothing answers "which stage is stalled." The operator's real morning
question — *did a candidate get stuck, and where?* — means reading two ledgers in
two repos (`campaign-dag-gaps.md` §5's real lesson: the failure surfaced as a red
row when what was needed was "today's 40 are unworked and the queue is growing").

**Proposed primitive.** `fly automation graph` (render the `--after` DAG, mark each
edge's last-consumed freshness) and/or a per-automation "last successful run
consumed by its dependent N days ago" staleness field. fly can honestly report the
*run* topology and freshness; it cannot report *candidate* backlog (that is the
ledger's, Non-goal 3). Scope the feature to run-graph + freshness and stop there.
Size M. **Recommend after the cheap wins.**

---

## G8 — occurrences missed while fly was closed

**Current behaviour (document first).** `schedule.rs` handles a missed
*not-before* moment, and the sweep **collapses any backlog into one occurrence**
(the claim advances `next_run_at` from *now*, `mod.rs:2502`) — so a day the laptop
was shut is **not** caught up; the next run works the next window. For a
weekday-morning triage, a silently skipped day is data loss
(`campaign-dag-gaps.md` §4.3, "skip-on-overlap is data loss"). Checks/monitors
correctly do not catch up (that plan says so); a daily *triage* is the opposite
workload.

**Proposed primitive.** `--on-miss skip|run-once` (default `skip`, current
behaviour). `run-once` fires a single catch-up occurrence when the sweep finds the
last scheduled occurrence never ran. This is a *policy decision to make* before
it is code — decide the semantics (how far back does "missed" look? interaction
with `--after` and usage-gating?) then implement. Size S–M. **Recommend after
documenting the current rule in `fly automation run --help`/the plan** so the
operator can decide.

---

## G9 — two semantics to confirm and document (not change)

- **Manual runs vs `--after`.** `mod.rs:1511` builds a `dep_decision` via
  `depend::manual_decision` (R23: a manual run is user-initiated). A manual run
  **is** evaluated against the predicate and **can be withheld** — it is *not* an
  override; §7 of `campaign-dag-gaps.md` verified this live ("Manual is not an
  override"). Action: make `fly automation run --help` say so explicitly.
- **Cross-`cwd` chaining.** `--after` binds by upstream **id**, independent of
  `cwd`; nothing in `depend.rs` consults `cwd`. So chaining a secure automation
  onto a feed one is supported. Action: add a test that pins a cross-`cwd` chain
  and document it, since the next pipeline step relies on it.

---

## Migration note — for the operator to run (not run here)

G1 changes nothing until you opt in. Nothing below is required; do it when ready.

1. **Range leg (`sjbrjb2pgh`) — opt into verdict gating, no recreate:**
   `fly automation update sjbrjb2pgh --verdict-gated`
   Then edit its skill/prompt to end with a fenced verdict block: `PASS` when it
   advanced a candidate, `DECLINED` when the intake row had nothing to advance,
   `FAIL` on a real error. Its Step-0 hand-gate can then be deleted — a decline is
   now a first-class outcome the lab leg reads.
2. **Lab leg (`hctsfexvz3`) — same:** `fly automation update hctsfexvz3 --verdict-gated`,
   add the verdict block to its prompt, delete its Step-0 gate. With the range leg
   emitting `DECLINED`, the lab leg now **stands down silently** on a no-work day
   instead of either firing on nothing or being alerted about a correct idle.
3. **Verify before trusting:** run each leg manually on a known no-work day and
   confirm the dependent records a silent decline (no alert), and on a work day
   confirm it fires. Keep the repos' exactly-once markers in place regardless —
   they remain the crash-protection floor (Non-goal 5).
4. **Do not** touch the script legs' verdicts yet (G1 is agent-mode only); their
   exactly-once poll consumers are unaffected and remain correct.

Ask Evan before opting in the leg that feeds a spend stage.
