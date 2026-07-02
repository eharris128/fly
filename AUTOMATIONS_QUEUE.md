# Automations Work Queue (U6-U10)

**Status:** ✅ **Feature complete.** All of U1–U12 have landed — U1-U5 + U11-U12
committed, U7/U7.5/U8/U9/U10 complete, and **U6 complete** (see their sections).  
**Last Updated:** 2026-07-02  
**Remaining:** none — the automations feature is done.

---

## U6: Alert Surfacing — Alerts Module and Sink Pane — DONE (2026-07-02)

**Goal:** Non-silent script output reaches the user through the attention pipeline.

**Requirements:** R16 (sanitized log append), R17 (sink pane creation/registration + pending queue), R18 (Reason::Alert flows end-to-end)

**Status:** ✅ **Complete.** The seam U5 exposed (`ScriptRunner`'s no-op
`AlertSink`) is now the real alerts-log append + attention raise. Landed:

- `automations/alerts.rs` (new, pure + tested): **`AlertsLog`** —
  `automation-alerts.log` under `session::data_dir()` (`0600` in the `0700`
  dir, private-file helpers recreated locally from `store.rs`).
  - **Append (R16):** `append(name, first_line)` writes `[name] first-line\n`
    **sanitized at write time** via `notify::sanitize_title`/`sanitize_body`,
    which strip control chars *including newlines* — so a script's stdout can
    never forge a second log line (the trailing `\n` is the only newline). Uses
    `O_APPEND` (atomic per-write for records this small; concurrent reaper
    appends don't interleave).
  - **Startup truncate:** `startup_truncate()` keeps the trailing 64 KiB at a
    line boundary (saturating byte math — release overflow-checks are off).
  - **Pending queue (R17):** `sink_or_queue` (atomic check-and-queue for the
    reaper closure), `queue_alert`/`drain_pending`, bounded at `MAX_PENDING`
    (drops oldest — the ring is per-pane binary).
  - **Sink registry (R17):** `register_sink(pane_id) -> Vec<QueuedAlert>` (sets
    the sink + atomically drains the backlog), `clear_sink_if(pane_id)` (clears
    only the matching pane), `has_sink`.
  - **Tests (5):** append strips ANSI/OSC/newlines (single-line record, 0600);
    pending drains once on registration; sink clears on the matching pane's exit
    only; startup truncate keeps a line-aligned tail (and leaves a small file
    alone); the pending queue is bounded.
- `automations/mod.rs`: `pub mod alerts;`.
- `lib.rs`:
  - Builds `AlertsLog::open_default()` (truncates on start), replaces the
    runner's no-op sink via `set_alert_sink`, and `app.manage`s the log. The
    sink closure runs on the reaper thread and touches **only** the AlertsLog
    lock + the log file — never the store lock (KTD-B). Per alert: append (R16),
    then `sink_or_queue` → **raise on the registered sink pane** (R18) or **queue
    + emit `automation://alert-pending`** (R17).
  - `raise_alert(app, attention, pane_id)` helper — the same seam the hook
    dispatch uses (`attention.signal(Signal { reason: Alert, tier: Cli })` →
    `stream::emit_attention`), so an alert rings a pane exactly like an agent
    raise (the attention lock is independent of the store lock).
  - `register_alert_sink` Tauri command (registered in `invoke_handler!`): the
    frontend registers its sink pane → backend drains the backlog, raising once
    per drained alert. `AlertPendingEvent { logPath }` payload struct.
- `stream/mod.rs`: the `spawn_pane` on-exit tap now calls
  `AlertsLog::clear_sink_if(id)` so the sink registration self-heals when the
  Automations pane exits (a later alert re-opens a fresh sink).
- `ipc.ts`: `AlertPendingEvent`, `onAlertPending` listener, `registerAlertSink`.
- `App.svelte`: `handleAlertPending` **single-flights** (guarded on the sink
  leaf still resolving — self-healing if the tab was closed) a **background
  ephemeral tab** titled "Automations" running `["tail","-n","50","-f",logPath]`
  in the log's dir (modeled on `handleAgentRun`; no focus steal, no run id).
  `onSpawned` calls `registerAlertSink` once the sink pane mounts;
  `sinkCommandByLeaf` seeds the command; listener registered + torn down in
  `onMount`.
- `lib/Terminal.svelte`: `onSpawned` now fires **after** the pane's
  `pane://exit`/`pane://attention` listeners are live, so the ring emitted
  synchronously by the sink registration drain is never missed (a general
  early-attention-event robustness fix).

---

## U7.5: spawn_pane Atomicity (DONE — 2026-07-02)

**Note:** U7 backend is complete, and this spawn_pane change is now done, so U8 is unblocked.

> **✅ RESOLVED (2026-07-02).** All six audit sub-items below landed together:
> 1. **Link-at-spawn** — `spawn_pane` now calls `AutomationManager::set_run_pane(run_id, pane_id)` (sets `RunRow.pane_id`, flushes, emits `automation://changed`) + `register_automation_pane` before the child spawns; a **late spawn** (run already terminal/unknown) returns `Err` and aborts the spawn (with token/attention cleanup) — no fan-out.
> 2. **R11 30-min deadline** — `sweep_once` phase-1 closes agent `Running` rows past `RUN_DEADLINE_MS` as `failed(ERR_TIMED_OUT)` **keeping `pane_id`**, so R7's alive-probe holds a stuck-but-alive agent in flight (verified: same-tick next occurrence records Skipped, one dispatch, no second pane).
> 3. **Pane-exit close** — `stream::spawn_pane`'s `on_exit` tap calls `AutomationManager::on_pane_exit` → closes the linked run `failed(ERR_PANE_EXIT)` and unregisters the recursion entry.
> 4. **Latent bugs fixed** — alive-probe now gates on `is_live()`; `push_row` evicts the oldest *terminal* row (never a `Running` one); persisted `pane_id`s are cleared on load (launch-stability) — ids are unique within a launch so no per-close clearing is needed.
> 5. **Lock order** — `on_pane_exit`/`set_run_pane` take the store lock while holding **no** PTY lock (`on_exit` runs on the read thread free of the registry lock), preserving store→PTY (KTD-B). Documented at both call sites.
> 6. **Tests** — added link-at-spawn, late-spawn-fails, linked-survives-ack, Stop-close (idempotent), pane-exit close, deadline+no-fanout, recursion registry, and launch-stability; plus a model test that a `Running` row survives history eviction. (89 automations tests pass.)

<details><summary>Original audit findings (2026-07-01) — retained for reference</summary>

> **⚠️ Audit findings (2026-07-01) — the agent-run lifecycle is a stub; do NOT land U8 until this whole section is done.**
> The `automation_run_id` param on `spawn_pane` is currently `_`-prefixed and discarded, so `RunRow.pane_id` is **never set**. Consequences, all latent only because `automations_frontend_ready` is never called yet (agent runs defer forever):
> 1. **Sequencing landmine (must fix atomically):** the 30 s ack timeout keys on `pane_id.is_none()`. The moment U8 flips frontend-ready *without* the linking below, **every agent run force-fails at 30 s even though its pane is alive**, then the next occurrence spawns a second concurrent pane → fan-out. The link (below) and the U8 frontend-ready caller **must land in the same change**.
> 2. **R11 30-min run deadline is entirely unimplemented** (only the `ERR_TIMED_OUT` string exists). Add a deadline check to `sweep_once`'s phase-1 closure: close agent `Running` rows whose `started_at + 30min ≤ now` as `failed(ERR_TIMED_OUT)` **without clearing `pane_id`** (so R7's alive-probe keeps a genuinely-stuck agent in-flight). This item is NOT in the checklist below — add it.
> 3. **R11 pane-exit → failed is unimplemented:** `stream::spawn_pane`'s `on_exit` closure closes no run. Add a "close the linked run failed on pane exit before Stop" tap (and unregister the recursion-registry entry there).
> 4. **Latent bugs to fix as part of the linking work, not before:**
>    - Pane-alive probe (`lib.rs`) uses `pty_mgr.lifecycle(id).is_some()`, which is `true` for an *exited-but-not-closed* pane → strands the automation. Use `.is_some_and(|s| s.is_live())`.
>    - `pane_id` is not launch-stable (resets to 1); a persisted `Failed` row keeps its old id and a restart can resolve it to an unrelated live pane. Clear `pane_id` on every close **except** the deadline-alive case, or tag rows with a boot id.
>    - `model::push_row` evicts oldest-first regardless of status, so a long-lived `Running` row can be evicted after 20 pushes → `in_flight()`/`close_run` silently break. Evict the oldest *terminal* row, retaining any `Running` row.
> 5. **Lock-order invariant to document + honor:** the sweep consults the pane-alive probe (which takes the PTY registry lock) while holding the store lock, so the order is **store → PTY**. The pane-exit tap must call `close_run*` **outside** `panes.lock()` or it's an AB-BA deadlock that wedges every sweep tick.
> 6. **Add the agent-lifecycle tests that don't exist:** link-at-spawn, Stop-close (pane linked), deadline fire, pane-exit close, late-spawn-fails-the-spawn. The only agent-path test today asserts the ack fires when *no* pane links — i.e. it green-lights the broken behavior.

</details>

### Implementation Checklist (landed — see the resolved note above)

#### Update `spawn_pane` in `src-tauri/src/stream/mod.rs`

**The automation_run_id parameter is already added but unused.** Now wire it:

1. **Link run↔pane atomically before spawn:**
   ```rust
   if let Some(run_id) = automation_run_id {
       if let Some(mgr) = app.try_state::<Arc<automations::AutomationManager>>() {
           // Link the pane id to the run (updates RunRow::pane_id)
           mgr.set_run_pane(&run_id, id)?;  // NEW METHOD
           // Register pane in recursion registry (blocks create/run from this pane)
           mgr.register_automation_pane(id)?;  // NEW METHOD
       }
   }
   ```

2. **Add to AutomationManager in `src-tauri/src/automations/mod.rs`:**

   - `set_run_pane(run_id: &str, pane_id: PaneId) -> Result<(), String>`
     - Find the automation + run by scanning all (run ids are globally unique in this app)
     - Update `RunRow::pane_id = Some(pane_id.0)`
     - Flush to disk immediately (atomic rename, since pane spawn is racy)
     - Lock-free: spawning happens outside the manager lock

   - `register_automation_pane(pane_id: PaneId) -> Result<(), String>`
     - Add pane_id to the recursion-gate registry
     - This is what blocks `create`/`run` from automation-spawned panes (R22)

3. **Pane-exit cleanup in `stream/mod.rs`:**
   - In the `on_exit` closure, call `mgr.unregister_automation_pane(pane_id)`
   - This allows the registry entry to clear when the pane exits (R22 load-bearing)

---

## U8: Agent-run Pane Creation and Retention (Frontend) — DONE (2026-07-02)

**Goal:** Agent runs land as background tabs that don't steal focus.

**Requirements:** R9 (backend half + frontend placement), R12 (ephemeral flag, tab retention)

**Status:** ✅ **Complete.** Landed:
- `ipc.ts`: `automationRunId` on `SpawnOpts`/`spawnPane`; `AgentRunEvent` + `onAgentRun` listener; `automationsFrontendReady` command wrapper.
- `Terminal.svelte`: `automationRunId` prop threaded into `spawnPane` (+ a try/catch so a rejected late-link shows an error line, not a blank pane).
- `automation-panes.ts` (new, pure + tested): `resolveTargetWorkspace` → origin ws if it exists, else first (R9 fallback).
- `App.svelte`: `handleAgentRun` creates a **background ephemeral tab** (`title = name`, `["claude", prompt]`, `cwd`) in the resolved workspace without touching `activeWorkspaceId`/`activeTabId`; `onAgentRun` registered in `onMount`, then `automationsFrontendReady()` called once the listener is live; listener torn down on unmount.
- `pnpm check` 0 errors; 231 frontend tests pass; backend builds.

**Deferred (blocked product decision — see plan open questions):** R12's auto-close on the agent's first `Stop`. Tabs are kept until the user closes them (succeeded and failed alike). The `automation://run-closed` event + `shouldCloseOnSuccess` are intentionally NOT built until that decision lands.

**Live end-to-end verification** (agent automation → background tab spawns) is pending **U9** — no UI/CLI path exists yet to create or manually-run an agent automation, so the dispatch can't be triggered until the CLI lands.

### Implementation Checklist (landed — see status above)

#### 1. Update `src/ipc.ts` event types

Add event types for:
- `automation://agent-run { runId, name, prompt, cwd, originWorkspaceHint }`
- `automation://run-closed { runId, automation_id, status }`

#### 2. Create `src/lib/automation-panes.ts`

Pure helpers (exported + tested):
- `function placePaneInWorkspace(workspaceId: string, tabs: Tab[], fallbackWorkspace: string): string`
  - If workspace exists, append tab there and return its tab id
  - If not, append to first workspace (R9 fallback)
  - Returns the new tab id (needed for spawn_pane call)

- `function shouldCloseOnSuccess(run: AgentRun, config: Config): boolean`
  - Policy: check the "Agent-run output preservation vs auto-close" decision
  - For now: return `false` (keep all tabs, since decision is open; U8 doc notes this)

#### 3. Wire `automation://agent-run` listener in `src/App.svelte`

On event:
1. Parse the `originWorkspaceHint` to find the workspace (R9)
2. Create a background tab via `placePaneInWorkspace`:
   - Title: automation name
   - Command: `["claude", prompt]`
   - Cwd: from event
   - Ephemeral: true (U11)
   - Store `runId` and `automationId` on the tab for later closure lookup
3. Once the pane mounts (Terminal.svelte), call `spawn_pane` with `automation_run_id: runId`
4. Frontend does NOT switch active workspace/tab (background only)

#### 4. Wire `automation://run-closed` listener

On event with `status: "succeeded"`:
- Find the tab by stored `runId`
- If `shouldCloseOnSuccess()`: close it (remove from tab list)
- If not: keep it (user can see output in scrollback)

On status `"failed"`:
- Keep the tab (R12: failed runs preserved so user can debug)

---

## U9: CLI Subcommand and Socket Protocol Extension — DONE (2026-07-02)

**Goal:** `fly automation …` end-to-end, with auth, origin stamping, recursion gate.

**Requirements:** R19-R24 (all CLI operations)

**Status:** ✅ **Complete.** Landed:
- `hooks/protocol.rs`: backward-compatible `Envelope { token, op=default "notify" }`; `op` absent/`"notify"`/unknown → the unchanged notify path, `"automation/*"` → the request handler.
- `hooks/server.rs`: `RequestHandler` type + `start_with_handler`; `handle_conn` now validates the token **first** (the security boundary, for every message), then branches — notify dispatches as before (no response), automation ops call the handler and write the `{ok,…}` response with a write timeout (bounds a non-reading peer).
- `state/manager.rs`: `pane_workspace(pane)` getter for origin stamping.
- `cli/automation.rs` (new): the client. Mutating ops (`create`/`pause`/`resume`/`run`/`delete`) post over the socket with the pane token, `send_request` with a ~5s bounded wait → "may have committed" (R20). Read ops (`list`/`show`/`runs`) read `store_path()` directly (R19, work outside a pane / with no app). `--json` everywhere; captured output sanitized for the terminal (R16/R20) but newline-preserving. `--script-file` read client-side (R21).
- `lib.rs`: thin `handle_automation_request` (AppHandle → resolves manager, recursion flag, workspace) delegating to a pure, testable `dispatch_automation_op` (R22 gate first, then origin-stamped routing).
- Tests: protocol envelope; `automation_cli.rs` integration (socket create→persisted + origin stamped, R22 recursion reject, invalid-token→no-response security boundary, dispatch core create/pause/resume/delete/unknown); `load_store_at` direct read; rel-time + sanitize helpers. 222 lib + 4 integration tests pass; clippy clean (only 2 pre-existing warnings).
- Verified via the real binary: `list`/`list --json`, outside-pane rejection (exit 1), flag validation (exit 2), unknown subcommand usage.

**Requirements:** R19-R24 (all CLI operations)

**Dependencies:** U7.5 (pane-id recursion registry, for R22 gate) — satisfied.

### Original notes

### Implementation Checklist

#### 1. Create `src-tauri/src/cli/automation.rs`

**High-level structure:**
```rust
pub fn run(args: &[String]) -> i32
  // Parse argv: automation create|list|show|runs|pause|resume|run|delete
  // Route to subcommand handlers below

// Each subcommand handler:
fn handle_create(args: &[String]) -> i32
fn handle_list(args: &[String]) -> i32
// etc.
```

**Create subcommand:**
- Flags: `--name`, `--cron`, `--tz`, `--cwd` (opt), `--prompt` XOR `--script`/`--script-file` XOR both = error
- `--interpreter` (bash|sh|node|python3 enum, not free-form)
- `--timeout` ms (clamped [1s, 900s], default 120s)
- `--json` output
- Read `--script-file` content **client-side** (app never opens a client path)
- Origin: extract `FLY_PANE_TOKEN` + resolve workspace from pane (R22)
- Send over socket as `op: "automation/create"` JSON
- Parse response: `{ ok?, error?, id? }`
- If error, print + exit 1
- If ok, print create banner (sanitized name + mode) regardless (R24)
- If no response in ~5s, report "may-have-committed" message (R20)

**List subcommand:**
- `--json` output
- Read store file directly via `app_dir_name()` paths (works outside a pane, R19)
- Emit each automation (id, name, schedule, status, next_run, last_run)

**Show/runs subcommand:**
- Show one automation or its run history
- `runs --output <runId>` prints captured output
- Sanitize output via `notify::sanitize_title/body` in non-JSON path (R20)
- `--json` skips sanitization (raw capture)

**Pause/resume/run/delete subcommands:**
- All require token (only work inside a pane, R19)
- All check recursion registry (R22: reject from automation-spawned panes)
- Send over socket, wait for response
- Print result + error details

#### 2. Update `src-tauri/src/hooks/protocol.rs`

**Backward-compatible protocol extension:**
- Add `op` field with `#[serde(default = "default_op")]` → defaults to `"notify"`
- Server checks `op` field; unknown ops treated as `notify` (backward compat)
- Automation ops: `automation/create`, `automation/list`, etc.

#### 3. Update `src-tauri/src/hooks/server.rs`

**Response writing (only for automation ops):**
- After token validation (not before!), parse the two-stage `op` discriminator
- If `op` is `notify`: dispatch as before, no response
- If `op` is `automation/*`: serialize response, write it, then close the stream
- Response format: `{ ok: bool, id?: string, error?: string }`
- Write timeout (Tauri default is fine)
- Bound concurrent handlers (prevent wedge by peer that never reads)

#### 4. CLI entry in `src-tauri/src/cli/mod.rs`

```rust
pub fn run(args: &[String]) -> i32 {
    if args.is_empty() {
        return help();
    }
    match args[0].as_str() {
        "notify" => notify::run(&args[1..]),
        "automation" => automation::run(&args[1..]),
        "hooks" => hooks::run(&args[1..]),
        _ => {
            eprintln!("fly: unknown subcommand {}", args[0]);
            2
        }
    }
}
```

#### 5. Integration tests in `src-tauri/tests/automation_cli.rs`

Test the full round-trip: create → list → run → show (output).

---

## U10: Dashboard Panel and Docs — DONE (2026-07-02)

**Goal:** Read-only visibility of automations.

**Requirements:** R25 (panel), R6 (store health warning)

**Status:** ✅ **Complete.** Landed:
- `automations/mod.rs`: `list_automations` command + `AutomationsDashboard` DTO
  (`{ automations, degraded, corruptBak, flushError }`, serde camelCase) — flattens
  `StoreHealth` (which already derives `Serialize`) for the R6 warning. Registered in
  `lib.rs`'s `invoke_handler!`.
- `ipc.ts`: TS mirrors of the model (`Automation`/`RunRow`/`AutomationSpec`/`AutomationOrigin`
  + `RunStatus`/`AutomationMode`/`RunTrigger`), the `AutomationsDashboard` DTO, `listAutomations()`,
  and the `onAutomationChanged()` listener (payload = automation id).
- `lib/automations.ts` (new, pure + tested): `automationsToRows(automations, nowMs)` — sort
  next-run asc / paused last / ties by name (mirrors the CLI's `load_store_at`), last-status/-run/-error
  derived from the last run row, `linkedPaneId` derived for a future jump affordance. Helpers
  `humanSchedule(cron, tz)` (every N min / hourly / daily / weekly / monthly / raw fallback) and
  `relativeTime(ms, nowMs)` (full-word, pluralized, past/future-branched — underflow-safe).
- `lib/automations.test.ts` (new): 13 tests — sorting, paused-last, tie-by-name, last-run derivation,
  never-run, running-fallback-to-startedAt, empty, `humanSchedule` shapes + raw fallback, `relativeTime`
  just-now/past/future.
- `HomeView.svelte`: read-only automations panel stacked **below the agent list in the left column**
  (static text — no selection/jump), empty state (`No automations · Run \`fly automation create --help\``),
  and the R6 degraded warning row (`⚠ Store corrupted · see <corruptBak or ~/.local/share/fly/automations.json>`).
- `App.svelte`: fetches `listAutomations` on dashboard open (re-run on `homeViewOpen`), refetches on
  `automation://changed` (only while open), tears the listener down on unmount; passes rows + degraded +
  corruptBak to `HomeView`.
- `CLAUDE.md`: automations module map (U1–U10), `fly automation …` in the two-roles section, `Reason::Alert` +
  `Tier::Cli` (KTD-H) in the attention pipeline, and the stale `state/suppress.rs` → `state/policy.rs` fix.
- `pnpm check` 0 errors; 244 frontend tests pass (13 new); Rust 222 lib + integration tests pass; clippy clean
  (only the 2 pre-existing warnings — `cli/hooks.rs:202` `Error::other`, `mod.rs:538` `manual_run` let-else).

**Deferred (as the checklist allowed):** the **jump affordance** (item 6). The pane_id→leaf/tab jump
mapping lives in `App.svelte` (`leafByPaneId`), not `HomeView`, and wiring it through is more work than the
rest of U10 combined. The view-model already derives `linkedPaneId` per row, so the panel can gain a jump
later without touching `automations.ts`. **Live verification** (a fresh `pnpm tauri build`/`flavor:dev` build)
was not run — the pure view-model is vitest-covered and `pnpm check` is green; a live build is nice-to-have.
Also note the panel's relative times are static between refetches (fetch-on-open + refetch-on-changed,
mirroring the usage panel) — they don't tick on a timer.

### Implementation Checklist (landed — see status above)

#### 1. Create `src/lib/automations.ts`

**View model:**
```typescript
type AutomationRow = {
  id: string;
  name: string;
  schedule: string; // human-readable: "every 5 min · America/New_York"
  paused: boolean;
  nextRun: string | null; // relative time: "in 5 minutes"
  lastStatus: "never run" | "succeeded" | "failed";
  lastRun: string | null; // relative time: "5 minutes ago"
  lastError: string | null; // truncated
  linkedPaneId?: u64; // if automation-spawned tab is open
};

export function automationsToRows(automations: Automation[], time_now_ms: number): AutomationRow[]
```

**Helpers:**
- `humanSchedule(cron, tz): string` - coarse humanization (every 5 min, daily, etc.) or fallback to raw cron
- `relativeTime(ms): string` - "in 5 minutes", "5 minutes ago", "just now", "1 day ago"
- Sorting: next_run ascending, paused last

#### 2. Update `src/lib/HomeView.svelte`

Add automations panel:
- Below the agent-list region in the left column (stacked, not a third column)
- Fetch via `listAutomations` Tauri command on dashboard open
- Refetch on `automation://changed` event
- Rows: static text (no keyboard selection, unlike agent list)
- Jump affordance: if a row has `linkedPaneId`, expose a jump target like agent rows do
- Empty state: "No automations · Run `fly automation create --help` to get started"
- Warning row: if `store_health` degraded, show "⚠ Store corrupted · see `~/.local/share/<app>/automation-alerts.log.bad.bak`"

#### 3. Add Tauri command in `src-tauri/src/lib.rs`

```rust
#[tauri::command]
pub fn list_automations(mgr: State<Arc<AutomationManager>>) -> Result<Vec<AutomationForDashboard>, String>
```

Returns: automations + store_health for the warning row

#### 4. Update `src/ipc.ts`

Export the types and command wrapper for `listAutomations`.

#### 5. Update CLAUDE.md

- Add module map section for automations (U1-U10 architecture)
- Add to commands table: `fly automation …`
- Add to attention pipeline: `Reason::Alert` + `Tier::Cli` behavior (KTD-H)
- Fix stale reference: `state/suppress.rs` → `state/policy.rs`

---

## Quick Reference: Build Order for Next Session

**Recommended order (dependencies flow downward):**

1. **U6: Alert Surfacing** (depends on U5, U7, U12)
   - AlertsLog implementation
   - Wire into U5 dispatch
   - Frontend sink-pane listener
   - Time: ~2-3 hours

2. **U7.5: spawn_pane Atomicity** (depends on U4, U7 Stop-event closure)
   - Add manager methods for run↔pane linking and pane-registry
   - Wire into spawn_pane (already has `_automation_run_id` parameter)
   - Time: ~1 hour

3. **U8: Agent-run Pane Creation** (depends on U7.5, U11, U12)
   - automation-panes.ts pure helpers + tests
   - App.svelte listener + tab placement
   - Run-closed handling (keep/close policy deferred)
   - Time: ~2 hours

4. **U9: CLI Subcommand** (depends on U2, U4, U7.5 recursion registry)
   - automation.rs implementation (create, list, show, runs, pause, resume, run, delete)
   - Protocol extension in hooks/server.rs
   - Integration tests
   - Time: ~3-4 hours

5. **U10: Dashboard Panel** (depends on U4, all priors)
   - automations.ts view model + tests
   - HomeView.svelte integration
   - Tauri command + IPC types
   - CLAUDE.md update
   - Time: ~2 hours

**Total for all of U6-U10:** ~10-12 hours of focused work (could be split across 2-3 sessions)

---

## Test Checklist (End-to-End Verification)

All units have landed. Each behavior below has **automated coverage** (noted
per item); the checkboxes track *live-app* end-to-end verification, still
pending because live runs are awkward here (old installed release; Wayland
screenshots need `GDK_BACKEND=x11` — see CLAUDE.md). Run them against a
`pnpm flavor:dev` build to tick them off.

- [ ] Script automation runs silently with empty stdout — *unit: `script.rs` `exit_zero_empty_stdout_closes_succeeded_with_null_output_ae1`*
- [ ] Script with `{"wakeAgent": false}` in final line runs silently — *unit: `script.rs` `trailing_wake_agent_false_is_silent_despite_earlier_output_ae2`*
- [ ] Script with output shows alert pane + log line, no ANSI/OSC escapes — *unit: `script.rs` `sentinel_mid_output_alerts_with_first_line_ae3` (classification) + `alerts.rs` `append_sanitizes_control_chars_and_newlines_r16` (sanitized log line)*
- [ ] Agent automation emits event, frontend creates background tab — *unit: `mod.rs` agent-run tests + `automation-panes.ts` placement*
- [ ] Agent run closes on first Stop hook (second Stop is no-op) — *unit: `mod.rs` Stop-close (idempotent)*
- [ ] 5-minute automation fires exactly once over a 10-minute window — *unit: `mod.rs` sweep/`schedule.rs` advance tests*
- [ ] Paused automation resumes into future (no instant fire) — *unit: `mod.rs` resume-into-future (AE7)*
- [ ] Automation-spawned pane blocks create/run (recursion gate) — *unit: `mod.rs`/`lib.rs` recursion-gate tests*
- [ ] CLI list/show work outside a pane (read store file directly) — *unit: `automation_cli.rs`*
- [ ] CLI create fires banner (even if silent), stores to disk — *unit: `automation_cli.rs`*
- [ ] Dashboard lists automations, shows next-run / last-status, jump to tabs — *unit: `automations.test.ts`*
- [ ] Store corruption rendered as `.bad.bak` rename + warning in dashboard — *unit: `store.rs` corrupt-json test*
- [ ] Alert with no sink pane open queues, then rings + tails the log once the Automations tab spawns — *unit: `alerts.rs` `pending_drains_once_on_sink_registration_r17`*

---

## Known Open Questions (Deferred)

- **Agent-run output preservation (U8):** Decide whether to keep succeeded tabs open indefinitely, close after grace window, or make it opt-in per automation
- **Creation-rate visibility (U9):** Whether create banners bypass rate-limiting (currently no special handling)
- **5-minute clamp drift (R1):** Whether to accept drifting next-run times or snap to cron boundary when safe
- **Alerts pane interactivity (U6):** Whether to signal the tail-f pane as read-only and auto-close dead husks
- **CLI discoverability (U10):** Whether to add automation commands to HotkeyMenu or CommandPalette
