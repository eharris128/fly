# Automations Work Queue (U6-U10)

**Status:** U1-U5 committed, U11-U12 committed, U7 complete (backend + Stop-event closure done)  
**Last Updated:** 2026-07-01  
**Remaining:** U6, U8, U9, U10 (U7's spawn_pane atomicity blocked by U8)

---

## U6: Alert Surfacing — Alerts Module and Sink Pane

**Goal:** Non-silent script output reaches the user through the attention pipeline.

**Requirements:** R16 (sanitized log append), R17 (sink pane creation/registration), R18 (Reason::Alert flows end-to-end)

**Status:** U5 (script runner) and U12 (Reason::Alert) ready; U7 stop-event ready

### Implementation Checklist

#### 1. Create `src-tauri/src/automations/alerts.rs`

- **AlertsLog struct:**
  - Path: `$XDG_DATA_HOME/<app>/automation-alerts.log` (mode 0600 in 0700 dir)
  - Append method: takes `[automation-name] first-line` + body, sanitizes via `notify::sanitize_title/body` **at write time**
  - Startup truncate to 64 KiB tail on app start
  - Control-char stripping removes newlines (prevents forged log entries, R16)

- **Pending queue:**
  - Holds alerts arriving before sink registers (R17)
  - `fn queue_alert(&mut self, line: String)`
  - `fn drain_pending(&mut self) -> Vec<String>`

- **Sink registry:**
  - Track pane_id of current alerts-pane (created on demand)
  - `fn register_sink(&mut self, pane_id: u64)`
  - `fn clear_sink(&mut self)` on pane exit
  - `fn has_sink(&self) -> bool`

- **Tests:** log write sanitizes ANSI/OSC escapes, pending drains once on registration, sink clears on pane exit

#### 2. Wire AlertsLog into `src-tauri/src/lib.rs` setup

- Create on app startup (after automations_mgr but before sweep start)
- Store as managed state: `app.manage(alerts_log)`
- Export a `raise_alert` closure or wrapper for U5's dispatch to call after claim

#### 3. Update U5 ScriptRunner dispatch to emit alerts (in `src-tauri/src/automations/script.rs`)

- When `dispatch_script` completes a run classified as "alert" (U5 already does this classification)
- Instead of returning an error, emit to the alerts sink (U6 integration point)
- Call a `raise_alert(automation_name, line)` that:
  - Appends to log
  - Raises `Signal { reason: Alert, tier: Cli }` on the sink pane (if registered)
  - If no sink, queues for later (R17)

**Note:** The plan says U6 depends on U5, but the script classification is done in U5. U6 just wires the alerting surface. Verify U5's output classification logic is complete before starting U6.

#### 4. Frontend event listener in `src/App.svelte`

- Listen for `automation://alert-pending` event
- Single-flight create a background ephemeral tab:
  - Title: "Automations" (fixed)
  - Command: `["tail", "-n", "50", "-f", "<log-path>"]`
  - Cwd: app data dir
  - Ephemeral flag (U11)
- Call `emit("automationSinkRegistered", { paneId })`

#### 5. Implement `register_sink` Tauri command

- Frontend calls after creating sink tab and getting pane id
- Backend registers the pane in AlertsLog
- Drains pending queue and emits raises for each

---

## U7.5: spawn_pane Atomicity (BLOCKING U8)

**Note:** U7 backend is complete, but this spawn_pane change is needed for U8 to work.

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

### Implementation Checklist

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

## U8: Agent-run Pane Creation and Retention (Frontend)

**Goal:** Agent runs land as background tabs that don't steal focus.

**Requirements:** R9 (backend half + frontend placement), R12 (ephemeral flag, tab retention)

**Status:** U7 emits `automation://agent-run`, U11 ephemeral flag ready, U7.5 spawn_pane atomicity ready

**Blocked by:** U7.5 (spawn_pane linking)

### Implementation Checklist

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

## U9: CLI Subcommand and Socket Protocol Extension

**Goal:** `fly automation …` end-to-end, with auth, origin stamping, recursion gate.

**Requirements:** R19-R24 (all CLI operations)

**Status:** U4 (manager ready), U2 (validation ready), hook socket ready

**Dependencies:** U7.5 (pane-id recursion registry, for R22 gate)

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

## U10: Dashboard Panel and Docs

**Goal:** Read-only visibility of automations.

**Requirements:** R25 (panel), R6 (store health warning)

**Status:** U4 (manager ready), all prior units ready

### Implementation Checklist

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

After all units land:

- [ ] Script automation runs silently with empty stdout
- [ ] Script with `{"wakeAgent": false}` in final line runs silently
- [ ] Script with output shows alert pane, banner, and log line (no ANSI/OSC escapes)
- [ ] Agent automation emits event, frontend creates background tab
- [ ] Agent run closes on first Stop hook (second Stop is no-op)
- [ ] 5-minute automation fires exactly once over a 10-minute window
- [ ] Paused automation resumes into future (no instant fire)
- [ ] Automation-spawned pane blocks create/run (recursion gate)
- [ ] CLI list/show work outside a pane (read store file directly)
- [ ] CLI create fires banner (even if silent), stores to disk
- [ ] Dashboard lists automations, shows next-run / last-status, jump to tabs
- [ ] Store corruption rendered as `.bad.bak` rename + warning in dashboard

---

## Known Open Questions (Deferred)

- **Agent-run output preservation (U8):** Decide whether to keep succeeded tabs open indefinitely, close after grace window, or make it opt-in per automation
- **Creation-rate visibility (U9):** Whether create banners bypass rate-limiting (currently no special handling)
- **5-minute clamp drift (R1):** Whether to accept drifting next-run times or snap to cron boundary when safe
- **Alerts pane interactivity (U6):** Whether to signal the tail-f pane as read-only and auto-close dead husks
- **CLI discoverability (U10):** Whether to add automation commands to HotkeyMenu or CommandPalette
