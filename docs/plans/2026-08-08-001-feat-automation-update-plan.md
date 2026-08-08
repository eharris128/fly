---
title: "feat: `fly automation update` — patch a stored automation in place"
type: feat
date: 2026-08-08
status: planned
---

# feat: `fly automation update` — patch a stored automation in place

## Summary

Automations gain one mutating op: `fly automation update <id> [flags]`
patches a stored record in place — name, schedule, cwd-independent launch
parameters, prompt or script content — without the delete + recreate dance
that today loses the run history, the id (breaking any dependent's `after`
edge), and the origin stamp. Update is **patch-semantics** (absent =
unchanged, explicit `--no-*` flags to clear), socket-only like every other
mutating op, and deliberately refuses the mutations whose immutability other
parts of the design lean on: mode-kind switches, monitors, retired records,
and setting/changing a dependency edge.

Motivation: the production feed pipeline's automations get re-tuned weekly
(prompt wording, model pin, cron drift), and each tune today is
delete + recreate. That resets `runs` (the R8 bounded history that the
dependencies predicate and `consecutive_infra_failures` both read), mints a
new id (silently orphaning any dependent's `after` edge — the delete
advisory fires, but the recreate can't reclaim the old id), and re-stamps
`origin` to whatever pane did the surgery.

---

## Problem Frame

An update op is trivially easy to make *too* powerful. Three existing design
arguments rest on fields being immutable after create, and this plan's core
job is drawing the line so those arguments survive:

1. **Dependency-plan KTD6** ("cycles are unconstructible through the API")
   rests literally on "there is no automation-update op". An update that can
   set `after` re-opens cycle construction against the live graph.
2. **Monitor registration is refuse-or-store** (monitor-handoff R11/R12):
   pickup pointers are captured from the registering pane at create time and
   the parent tab closes on the strength of that capture. There is no pane
   to re-capture from at update time.
3. **Claimed rows are self-contained** (workspace-and-model R13,
   headless-agent-automations R2): model/effort/headless are resolved at
   claim and stamped on the row, so a mid-flight update must never need to
   touch a running run — and must be documented as not touching it.

## Key Technical Decisions

- **KTD1 — patch semantics with a closed-set clear list.** Absent wire field
  = unchanged. That collides with the existing wire convention where `None`
  means "use the default" (create), so clearing a pinned value gets its own
  explicit channel: a new additive `clear: Vec<String>` field on
  `AutomationRequest` whose members come from a closed set (`model`,
  `effort`, `disposition`, `retryOnInterrupt`, `after`); an unknown member is
  **refused**, never ignored. The CLI maps `--no-model`, `--no-effort`,
  `--default-disposition`, `--no-retry-on-interrupt`, `--no-after` onto it.
  Notably `retry_on_interrupt: bool` on the wire cannot express "turn off"
  (skip-if-false serialization) — both directions of that toggle ride the
  update tri-state (`true` via the existing field, `false` via `clear`),
  and the existing field's type is not changed.
- **KTD2 — the exclusions are the design.** Refused with distinct errors:
  mode-kind switch (agent ↔ script — delete + recreate is the honest
  spelling of "this is a different automation"); any update to a **monitor**
  (frame 2 above; its cron/floor tuning is a non-goal — revisit if a real
  case appears); any update to a **retired** record (mirrors the resume
  retirement gate — same inside-the-mutation check, fix(review) #2
  precedent); **setting or changing `after`** — only `--no-after` (clearing)
  is allowed, because removing an edge can never close a cycle, so the
  dependency plan's KTD6 argument survives verbatim with one word changed
  ("edges are **set at create only**; update may only clear them"). `cwd` is
  also excluded in v1: changing it silently changes which transcripts the
  output-capture confidentiality guard (`sole_transcript_since`, resolved by
  cwd + dispatch time) can match — cheap to add later, not worth the
  reasoning burden now.
- **KTD3 — in-flight runs are untouched, by construction and by doc.** An
  update never kills, re-dispatches, or re-parameterizes a `Running` row:
  the claim already stamped resolved model/effort/headless onto the row
  (frame 3), a running script's interpreter holds the *old* script file
  (KTD5), and the new values simply apply from the next claim. Pinned by
  test: update mid-run → the running row closes under its original
  parameters, the next claim uses the new ones.
- **KTD4 — schedule recompute only when enabled.** A `cron`/`timezone`
  change validates via `schedule::validate`, surfaces the R1 min-gap
  advisory as a create-style warning (never fatal), and recomputes
  `next_run_at` from **now** via `schedule::advance_from` — with the
  `not_before_ms` floor riding the recompute as everywhere else — but only
  if the automation is currently enabled. A paused automation stays paused
  (`next_run_at` stays `None`) and picks up the new schedule on resume,
  which already recomputes from now (R23/AE7); update never flips the
  enabled bit in either direction — pause/resume remain that bit's sole
  owners.
- **KTD5 — script content replaces by write-new-then-swap, never overwrite
  in place.** A running script's interpreter has the old `script_file` path
  open; overwriting it mid-run would splice the new program under a running
  process. New content is written to a **fresh** file via the store's script
  dir *before* the record mutation (a failed write leaves the record
  untouched); the mutation swaps the `script_file` pointer; the orphaned old
  file is deleted best-effort after the lock (KTD-B: no IO inside it). A
  crash between write and swap leaves an unreferenced script file — inert
  residue, cleaned opportunistically by delete's existing script-dir
  teardown discipline.
- **KTD6 — untrusted-wire re-validation mirrors create's arm exactly.** The
  socket payload is untrusted regardless of what the CLI already checked:
  cron/timezone through `schedule::validate`; `timeout_ms` over
  `TIMEOUT_MAX_MS` **refused, not clamped** (the 2026-08-07 lesson: three
  surfaces agreeing on a number that is not the one enforced cost a real
  debugging session); `effort` in the closed set; `headless`/`paned`
  agent-mode only; agent-only fields on a script automation (and vice versa)
  refused as a disguised mode switch (KTD2).
- **KTD7 — skew posture: fail-closed, and that's an upgrade.** An old app
  receiving `automation/update` hits the dispatch default arm and returns an
  explicit unknown-op error — unlike create's silently-ignored new *fields*
  (the accepted `monitor`/`headless`/`after` posture), a whole new *op*
  fails loudly, which is strictly better. The `clear` field is additive
  `#[serde(default)]`/skip-if-empty, so old CLIs and new servers stay
  mutually intelligible on every existing op.

## Requirements

**Manager (U1 — `automations/mod.rs`)**

- R1. `UpdateSpec`: `Option` per updatable field — `name`, `cron`,
  `timezone`, `retry_on_interrupt: Option<bool>`, agent `prompt` /
  `model` / `effort` / `headless: Option<Option<bool>>` (outer = change?,
  inner = the tri-state), script `content` / `interpreter` / `timeout_ms` —
  plus the parsed clear set. An empty spec (nothing to change) is refused
  before any store access.
- R2. `AutomationManager::update(id, UpdateSpec) → Result<Updated, String>`
  runs one flush-tolerant store mutation in the `resume` style; the gates
  run **inside** the mutation before any write: no such id; retired;
  monitor; mode mismatch (agent fields against a script record or vice
  versa).
- R3. Schedule semantics per KTD4: recompute only on cron/timezone change
  and only when enabled; `not_before_ms` floor preserved; unparseable new
  state cannot be stored (validated before the mutation), so the resume
  degrade path is never needed here.
- R4. The R1 min-gap advisory rides the return value as a create-style
  `warning: Option<String>`, alongside a flush-degraded warning when the
  KTD-B flush fails (record live in memory, not durable).
- R5. `updated_at` stamped; `emit_changed(id)` after the lock so the
  dashboard refetches (`automation://changed`); the feed projection needs
  **no** schema change — it re-reads the mutated fields on the next frame.
- R6. In-flight runs untouched per KTD3, pinned by the mid-run test.

**Wire & dispatch (U2 — `cli/automation.rs` types, `lib.rs` arm)**

- R7. `AutomationRequest` gains `clear: Vec<String>` (`#[serde(default)]`,
  skip-if-empty). Every other update field reuses the existing optional
  request fields (`id`, `name`, `cron`, `timezone`, `prompt`, `script`,
  `interpreter`, `timeout_ms`, `model`, `effort`, `headless`) — no new
  parallel shapes. `retry_on_interrupt: true` on an update means "turn on";
  `"retryOnInterrupt"` in `clear` means "turn off"; both together refused.
- R8. The `automation/update` arm in `dispatch_automation_op` re-validates
  the untrusted payload per KTD6, resolves the clear set against the closed
  name list (unknown → refused), builds `UpdateSpec`, and calls the manager.
  The R22 recursion gate applies unchanged (it runs before the op match).
- R9. Response: `ok` + the automation id + the advisory warning — the
  existing `AutomationResponse` shape, no new fields.

**Script swap (U3 — `automations/store.rs`)**

- R10. A store-level replace-script operation implementing KTD5:
  write-new-file (reusing `put_script`'s naming/permissions), return the new
  relative path for the record swap, delete-old best-effort after the
  caller's mutation completes. Unit-tested for the failed-write leaves-
  everything-untouched arm.

**CLI (U4 — `cli/automation.rs`)**

- R11. `fly automation update <id> [--name] [--cron] [--timezone]
  [--prompt] [--script <text> | --script-file <path>] [--interpreter]
  [--timeout] [--model | --no-model] [--effort | --no-effort]
  [--headless | --paned | --default-disposition]
  [--retry-on-interrupt | --no-retry-on-interrupt] [--no-after] [--json]`.
  Script content is read client-side (R21 discipline: the app never opens a
  client path). Contradictory pairs (`--model` + `--no-model`, etc.) and
  zero change flags are CLI errors before any socket traffic.
- R12. On success the CLI prints the updated record `show`-style plus the
  advisory warning; `--json` emits the response object. The top-level usage
  line and `--help` gain the subcommand.

**Docs-truth repair (U5)**

- R13. Every comment that says or implies "create-only because no update op
  exists" is revised in the same change that makes it false:
  `model.rs::Automation.after` ("no update op exists — the KTD6 cycle
  argument rests on this" → "update may only clear, never set — KTD6
  preserved"), `depend.rs`'s KTD6 restatement, the `AutomationRequest`
  field docs (`retry_on_interrupt`, `monitor`, `not_before_ms`, `headless`,
  `after` — each now "create-only" only where still true), and the
  dependencies plan gains a dated addendum note. `CLAUDE.md`'s automations
  section gains the subcommand.

**Integration & validation (U6)**

- R14. A `hook_auth`-style integration test: update posted from a validated
  pane succeeds end-to-end; an automation-spawned pane is refused by R22;
  an old-server-shaped dispatch (unknown op) yields the explicit error.
- R15. Live check in the dev flavor (`pnpm flavor:dev`, isolated
  `FLY_APP_NAME=fly-dev` store): update a script automation's cron + content
  while a run is in flight; observe the running row close under old
  parameters, the next fire use the new ones, and the dashboard row change
  on the `automation://changed` refetch without a restart.

## Units of Work

- **U1 — manager** (`automations/mod.rs`): `UpdateSpec`,
  `AutomationManager::update`, gates, recompute, warning plumbing; test-
  first (R1–R6, KTD2–KTD4).
- **U2 — wire + dispatch arm** (`cli/automation.rs` types, `lib.rs`):
  `clear` field, `automation/update` arm with create-mirrored validation
  (R7–R9, KTD1/KTD6/KTD7).
- **U3 — script swap** (`automations/store.rs`): write-new-then-swap +
  orphan cleanup (R10, KTD5).
- **U4 — CLI** (`cli/automation.rs`): flag parsing, clear mapping,
  show-style echo, usage text (R11–R12).
- **U5 — docs-truth repair**: comment revisions + dependencies-plan
  addendum + `CLAUDE.md` (R13).
- **U6 — integration + live validation** (R14–R15).

## Risks

- **Doc-comment drift is the real hazard here, not code.** Three plans'
  arguments cite "no update op exists"; U5 is a first-class unit, not
  cleanup, and lands in the same commit as U2.
- **Clear-list typos.** A misspelled `clear` member is refused (KTD1), never
  silently ignored — the cost is a round-trip error, the alternative is a
  pin the user believes cleared and didn't.
- **Orphaned script files** on a crash between write and swap (KTD5): inert,
  bounded by crash frequency, and removable by hand in the store's script
  dir. Accepted over the alternative (overwrite in place under a running
  interpreter).
- **Scope pressure.** `cwd`, monitor tuning, and `after` re-pointing are the
  three predictable "can update just also…" asks; each is excluded for a
  stated reason (KTD2) and each has a clean additive path later. Refusing
  them in v1 is a feature.

## Validation

```
cargo test --offline --manifest-path src-tauri/Cargo.toml
pnpm check && pnpm test:unit
```

Required tests (names encode the behavior, automations-suite convention):
update-refuses-mode-switch, update-refuses-monitor-and-retired,
update-refuses-setting-after-but-clears-it, cron-change-recomputes-only-when-
enabled (paused-stays-paused), not-before-floor-rides-the-recompute,
timeout-over-ceiling-refused-not-clamped, mid-run-update-leaves-the-running-
row-alone, script-swap-write-new-then-swap (+ failed-write untouched arm),
empty-update-refused, unknown-clear-member-refused, retry-toggle-both-
directions, R22-gated-pane-refused — plus the CLI contradictory-pair parses
and the min-gap advisory passthrough.
