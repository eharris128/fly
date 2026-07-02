//! Automations — cron-scheduled agent and script runs
//! (docs/plans/2026-07-01-002-feat-automations-plan.md).
//!
//! U1 ships the pure domain vocabulary ([`model`]), U2 the schedule math
//! ([`schedule`]), U3 the write-through mutex-authority store ([`store`]),
//! and this module's own body is U4: the [`AutomationManager`] (create /
//! pause / resume / delete / manual run), the `fly-automation-sweep` loop,
//! startup recovery, and ordered shutdown. The script runner (U5) and the
//! agent dispatcher (U7) plug into the seams defined here.
//!
//! **KTD-B lock discipline (load-bearing).** The store mutex is the single
//! authority every writer contends on (sweep thread, socket handler threads,
//! Tauri commands), so nothing slow or re-entrant may ever run under it:
//!
//! - the sweep decides + mutates + **flushes** each tick's claims under one
//!   [`store::Store::mutate`] hold, then **releases the lock, then
//!   dispatches** ([`AutomationManager::sweep_once`] is structured in
//!   visibly separate phases);
//! - `automation://changed` emission always happens after the mutating call
//!   returns (lock released);
//! - the script-killer seam is invoked outside the store lock (delete,
//!   shutdown);
//! - the sweep thread is joined ([`SweepHandle::stop_and_join`]) from
//!   `lifecycle::shutdown`, which holds no store lock.
//!
//! The two injected *probes* ([`AutomationManager::set_agent_pane_alive`],
//! [`AutomationManager::set_script_capacity`]) are the one sanctioned
//! exception: they are consulted inside the sweep's mutate closure so the
//! pre-claim checks (KTD-D) are atomic with the claim. They must therefore
//! be cheap, non-blocking reads and must **never** call back into this
//! manager or its store (the store mutex is not re-entrant).

pub mod model;
pub mod schedule;
pub mod script;
pub mod store;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use tauri::Emitter;

use model::{
    Automation, Mode, Origin, RunMode, RunOutcome, RunRow, RunStatus, Trigger, ERR_DELETED,
    ERR_INTERRUPTED,
};
use store::{Store, StoreHealth};

/// Sweep cadence (KTD-C): the named `fly-automation-sweep` thread wakes this
/// often. Due-ness is `next_run_at <= now`, so a 10s tick bounds claim
/// latency without mattering for correctness.
pub const SWEEP_TICK_MS: u64 = 10_000;

/// R10 scaffolding: an **agent** run whose spawn was never acked (no pane
/// linked) within this window is closed failed by the sweep. U7 supplies the
/// ack (threading the run id through `spawn_pane` sets [`RunRow::pane_id`]);
/// until then this also cleans up after a dropped `automation://agent-run`
/// event.
pub const AGENT_ACK_TIMEOUT_MS: u64 = 30_000;

/// Error string for agent runs closed by the R10 ack timeout.
pub const ERR_SPAWN_ACK: &str = "spawn ack timeout";

/// Skip reason recorded for the R7 overlap skip.
pub const SKIP_IN_FLIGHT: &str = "run in flight";
/// Skip reason recorded for the U5 global script-capacity skip (KTD-D).
pub const SKIP_CAPACITY: &str = "capacity";

/// The Tauri event emitted after every mutation (payload: the automation id).
/// The dashboard (U10) refetches on it.
pub const AUTOMATION_CHANGED_EVENT: &str = "automation://changed";

/// Length of minted automation/run ids (short random alphanumerics).
const ID_LEN: usize = 10;

// ---- injected seams (everything tests with fakes) ---------------------------

/// Injected clock: epoch ms. The app wires `notify::now_unix_ms`; tests wire
/// an atomic they advance by hand (the [`crate::state::manager`] shape).
pub type Clock = Box<dyn Fn() -> u64 + Send + Sync>;

/// Injected `automation://changed` emitter, called with the automation id
/// **after** the store lock is released (KTD-B). The app wires a Tauri
/// `AppHandle::emit`; tests wire a Vec collector.
pub type ChangedEmitter = Box<dyn Fn(&str) + Send + Sync>;

/// U5 seam: kill the in-flight script process group for a **run id**. Called
/// on delete (R23) and shutdown (R5), always outside the store lock (KTD-B).
/// Default: no-op; `lib.rs` injects [`script::ScriptRunner::kill_run`] (U5).
pub type ScriptKiller = Arc<dyn Fn(&str) + Send + Sync>;

/// U7 seam widening R7's in-flight check: whether a *terminal* agent row
/// (deadline-failed) still has its linked pane alive — a stuck agent must
/// skip, not fan out. Consulted inside the sweep's mutate closure (module
/// doc): must be a cheap read and never re-enter the manager/store. Default:
/// `false` until U7 wires pane liveness.
pub type PaneAliveProbe = Arc<dyn Fn(&RunRow) -> bool + Send + Sync>;

/// U5 seam: whether global script capacity is available (in-flight cap).
/// Consulted inside the sweep's mutate closure so the KTD-D pre-claim skip is
/// atomic with the claim: same constraints as [`PaneAliveProbe`]. Default:
/// `true`; `lib.rs` wires [`script::ScriptRunner::has_capacity`] (U5).
pub type CapacityProbe = Arc<dyn Fn() -> bool + Send + Sync>;

/// How claimed runs leave the manager (KTD-C/E): the sweep claims + flushes
/// under the store lock, releases it, then calls exactly one of these.
/// Implementations must not call back into the manager *synchronously from
/// the dispatch call on paths that take the store lock and then block* — the
/// contract is simply that dispatch runs outside the store lock, so calling
/// `list()`/`close`-style APIs from other threads (U5's reaper) or even from
/// the dispatch itself is safe.
pub trait Dispatcher: Send + Sync {
    /// Start an agent run (U7: emit `automation://agent-run`).
    fn dispatch_agent(&self, automation: &Automation, run_id: &str) -> Result<(), String>;
    /// Start a script run (U5: spawn the interpreter in its own group).
    fn dispatch_script(&self, automation: &Automation, run_id: &str) -> Result<(), String>;
}

/// Placeholder dispatcher: every dispatch fails, so claimed runs close
/// failed and the schedule recomputes (R3) — visible, never silent. `lib.rs`
/// constructs the manager with it (the [`script::ScriptRunner`] needs the
/// manager's `Arc` for its row closer, so it is injected right after via
/// [`AutomationManager::set_dispatcher`]); the runner keeps the agent arm
/// unwired until U7.
pub struct UnwiredDispatcher;

impl Dispatcher for UnwiredDispatcher {
    fn dispatch_agent(&self, _a: &Automation, _run_id: &str) -> Result<(), String> {
        Err("dispatch not wired yet (U5/U7)".into())
    }
    fn dispatch_script(&self, _a: &Automation, _run_id: &str) -> Result<(), String> {
        Err("dispatch not wired yet (U5/U7)".into())
    }
}

/// U7 agent dispatcher: emits the `automation://agent-run` event to the
/// frontend (R9, KTD-E). The frontend creates a background tab with the
/// prompt-threaded command, calls `spawn_pane` with the run id, and the
/// backend links run↔pane atomically on spawn (R10).
pub struct AgentDispatcher {
    app: tauri::AppHandle,
}

impl AgentDispatcher {
    pub fn new(app: tauri::AppHandle) -> Arc<Self> {
        Arc::new(AgentDispatcher { app })
    }
}

/// U7 event payload: agent run creation. Emitted after the run is persisted
/// (claim flushed) so the frontend can safely call `spawn_pane` with the id.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentRunEvent {
    run_id: String,
    name: String,
    prompt: String,
    cwd: String,
    /// R9: workspace identity (not pane id) so origin resolves after restart.
    origin_workspace_hint: String,
}

impl Dispatcher for AgentDispatcher {
    fn dispatch_agent(&self, a: &Automation, run_id: &str) -> Result<(), String> {
        let prompt = match &a.mode {
            Mode::Agent { prompt } => prompt.clone(),
            _ => return Err("BUG: dispatch_agent on non-agent automation".into()),
        };
        let event = AgentRunEvent {
            run_id: run_id.to_string(),
            name: a.name.clone(),
            prompt,
            cwd: a.cwd.clone(),
            origin_workspace_hint: a.origin.workspace_id.clone(),
        };
        self.app
            .emit("automation://agent-run", &event)
            .map_err(|e| format!("could not emit automation://agent-run: {e}"))?;
        Ok(())
    }

    fn dispatch_script(&self, _a: &Automation, _run_id: &str) -> Result<(), String> {
        Err("AgentDispatcher: script runs dispatched elsewhere (U5)".into())
    }
}

// ---- manager API payloads ----------------------------------------------------

/// What [`AutomationManager::create`] needs (U9 builds this from CLI flags).
#[derive(Debug, Clone)]
pub struct CreateSpec {
    pub name: String,
    /// 5-field cron expression (validated via [`schedule::validate`], R1).
    pub cron: String,
    /// IANA timezone (validated via [`schedule::validate`], R1).
    pub timezone: String,
    pub cwd: String,
    pub mode: CreateMode,
    /// Resolved by the caller at create time (R22/R9: pane id + workspace
    /// identity + label); the manager only stamps it.
    pub origin: Origin,
}

/// Mode payload at create time. Script content arrives inline and is written
/// to the store's script dir ([`store::Store::put_script`]) — it never lives
/// in the JSON (KTD-B).
#[derive(Debug, Clone)]
pub enum CreateMode {
    Agent {
        prompt: String,
    },
    Script {
        content: String,
        interpreter: String,
        timeout_ms: u64,
    },
}

/// A successful create: the persisted record plus R1's *advisory* min-gap
/// warning, returned (not raised) so the CLI can print it (U9). Hard
/// validation errors reject the create instead.
#[derive(Debug, Clone)]
pub struct Created {
    pub automation: Automation,
    pub warning: Option<String>,
}

/// What a manual run did (R23/R7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManualRun {
    /// Claimed and dispatched (schedule untouched — manual never advances).
    Started { run_id: String },
    /// A run was already in flight: recorded as a skipped row (R7).
    Skipped { run_id: String },
}

// ---- the manager -------------------------------------------------------------

/// The scheduler shell over the U3 store (the `state/manager.rs` shape: a
/// thin locked shell over pure cores — [`model`] transitions + [`schedule`]
/// math). Everything time- or side-effect-shaped is injected so the whole
/// unit tests with fakes.
pub struct AutomationManager {
    store: Store,
    dispatcher: Mutex<Arc<dyn Dispatcher>>,
    clock: Clock,
    emit_changed: ChangedEmitter,
    script_killer: Mutex<ScriptKiller>,
    agent_pane_alive: Mutex<PaneAliveProbe>,
    script_capacity: Mutex<CapacityProbe>,
    /// R5 gate: while false, due **agent** automations are deferred —
    /// neither claimed nor skipped (see [`AutomationManager::sweep_once`]).
    frontend_ready: AtomicBool,
    /// Sweep stop flag + wake, so shutdown interrupts the 10s wait instantly.
    sweep_stop: Mutex<bool>,
    sweep_wake: Condvar,
}

impl AutomationManager {
    /// Construct the manager and run **startup recovery** (R5): every row
    /// still `Running` in the loaded store belongs to a previous app run
    /// whose outcome is unknowable — close them all failed("interrupted") in
    /// one flush.
    pub fn new(
        store: Store,
        dispatcher: Arc<dyn Dispatcher>,
        clock: Clock,
        emit_changed: ChangedEmitter,
    ) -> AutomationManager {
        let mgr = AutomationManager {
            store,
            dispatcher: Mutex::new(dispatcher),
            clock,
            emit_changed,
            script_killer: Mutex::new(Arc::new(|_run_id: &str| {})),
            agent_pane_alive: Mutex::new(Arc::new(|_row: &RunRow| false)),
            script_capacity: Mutex::new(Arc::new(|| true)),
            frontend_ready: AtomicBool::new(false),
            sweep_stop: Mutex::new(false),
            sweep_wake: Condvar::new(),
        };
        mgr.recover_interrupted();
        mgr
    }

    /// R5 startup recovery: close all orphaned `Running` rows
    /// failed([`ERR_INTERRUPTED`]) under one lock hold / one flush. No
    /// `automation://changed` is emitted — this runs before any listener
    /// exists, and the dashboard fetches on open anyway.
    fn recover_interrupted(&self) {
        let now = (self.clock)();
        let _ = self.store.mutate(|map| {
            for a in map.values_mut() {
                for run_id in running_run_ids(a) {
                    a.close(&run_id, failed(ERR_INTERRUPTED), now);
                }
            }
        });
    }

    // ---- seam injection (U5/U7 wire the real implementations) ---------------

    /// Replace the dispatcher (U5/U7). Held in a mutex only so the runners —
    /// which themselves need an `Arc<AutomationManager>` to close rows — can
    /// be constructed after the manager and injected here.
    pub fn set_dispatcher(&self, d: Arc<dyn Dispatcher>) {
        *self.dispatcher.lock().unwrap() = d;
    }

    /// Inject the U5 script-group killer (see [`ScriptKiller`]).
    pub fn set_script_killer(&self, k: ScriptKiller) {
        *self.script_killer.lock().unwrap() = k;
    }

    /// Inject the U7 pane-liveness probe (see [`PaneAliveProbe`]).
    pub fn set_agent_pane_alive(&self, p: PaneAliveProbe) {
        *self.agent_pane_alive.lock().unwrap() = p;
    }

    /// Inject the U5 script-capacity probe (see [`CapacityProbe`]).
    pub fn set_script_capacity(&self, c: CapacityProbe) {
        *self.script_capacity.lock().unwrap() = c;
    }

    /// R5: the frontend finished restore and is listening for agent-run
    /// events. Until this flips, the sweep defers due agent automations.
    pub fn set_frontend_ready(&self) {
        self.frontend_ready.store(true, Ordering::Release);
    }

    // ---- manager API (R1, R23) -----------------------------------------------

    /// Create an automation (R1): validate cron + tz via [`schedule`] (hard
    /// errors reject; the advisory min-gap warning is *returned* alongside
    /// success for the CLI to print, U9), mint a short unique id, write
    /// script content to the store's script dir first (a crash between the
    /// two steps leaves at worst an orphan script dir, never an entry
    /// pointing at a missing script), stamp origin + timestamps, compute the
    /// initial `next_run_at` via [`schedule::advance`], persist, and emit
    /// `automation://changed`.
    pub fn create(&self, spec: CreateSpec) -> Result<Created, String> {
        let validation = schedule::validate(&spec.cron, &spec.timezone)?;
        let now = (self.clock)();
        let next_run_at = schedule::advance(&spec.cron, &spec.timezone, now)?;

        // Mint a unique id (bounded retry; a collision in a 36^10 space at
        // desktop scale is astronomically unlikely).
        let mut id = mint_id();
        for _ in 0..8 {
            if self.store.get(&id).is_none() {
                break;
            }
            id = mint_id();
        }

        let mode = match spec.mode {
            CreateMode::Agent { prompt } => Mode::Agent { prompt },
            CreateMode::Script {
                content,
                interpreter,
                timeout_ms,
            } => {
                let path = self
                    .store
                    .put_script(&id, &content)
                    .map_err(|e| format!("could not store script content: {e}"))?;
                Mode::Script {
                    script_file: path.to_string_lossy().into_owned(),
                    interpreter,
                    timeout_ms,
                }
            }
        };

        let automation = Automation {
            id: id.clone(),
            name: spec.name,
            cron: spec.cron,
            timezone: spec.timezone,
            enabled: true,
            cwd: spec.cwd,
            mode,
            origin: spec.origin,
            created_at: now,
            updated_at: now,
            next_run_at,
            runs: Vec::new(),
        };
        let record = automation.clone();
        let inserted = self.store.mutate(|map| {
            if map.contains_key(&id) {
                return false; // minted-id collision after the pre-check: bail
            }
            map.insert(id.clone(), automation);
            true
        });
        // A flush failure keeps the in-memory record live (the store is the
        // authority, KTD-B) — surface it as a warning, not a failed create,
        // so a CLI retry doesn't mint a duplicate schedule.
        let mut warning = validation.min_gap_warning;
        match inserted {
            Ok(true) => {}
            Ok(false) => return Err("id collision while creating automation; retry".into()),
            Err(e) => {
                warning = Some(match warning {
                    Some(w) => format!("{w}; store flush failed: {e} (automation is live in memory)"),
                    None => format!("store flush failed: {e} (automation is live in memory)"),
                });
            }
        }
        (self.emit_changed)(&id); // after the lock is released (KTD-B)
        Ok(Created {
            automation: record,
            warning,
        })
    }

    /// R23: pause nulls `next_run_at` (the sweep's due check never fires on
    /// `None`). Returns the updated record.
    pub fn pause(&self, id: &str) -> Result<Automation, String> {
        let now = (self.clock)();
        let updated = self.with_automation(id, |a| {
            a.next_run_at = None;
            a.updated_at = now;
        })?;
        (self.emit_changed)(id);
        Ok(updated)
    }

    /// R23: resume recomputes `next_run_at` **from now** via
    /// [`schedule::advance`] — a stale past value must never fire instantly
    /// (AE7: an automation paused for a week resumes into the future).
    pub fn resume(&self, id: &str) -> Result<Automation, String> {
        let now = (self.clock)();
        let updated = self.with_automation(id, |a| {
            // Pure cron math inside the lock is fine (KTD-B bans dispatch/
            // emit/IO, not computation). Unparseable stored state degrades
            // to paused rather than firing at a bogus instant.
            a.next_run_at = schedule::advance(&a.cron, &a.timezone, now).ok().flatten();
            a.updated_at = now;
        })?;
        (self.emit_changed)(id);
        Ok(updated)
    }

    /// R23 teardown: remove the record + its stored script content (store
    /// owns that half), close its open rows failed([`ERR_DELETED`]), and
    /// kill any in-flight script group via the U5 seam — killer invoked
    /// **after** the store lock is released (KTD-B). The in-flight *agent*
    /// pane is unlinked, never killed; its recursion-registry entry (U7)
    /// deliberately survives until the pane exits, otherwise create → delete
    /// would un-gate a still-live automation-spawned pane (R22).
    ///
    /// Returns the removed record with its rows closed — the store no longer
    /// holds it, so this return value is the only witness of the closure.
    pub fn delete(&self, id: &str) -> Result<Automation, String> {
        let removed = self
            .store
            .delete(id) // mutate(remove) + flush under the lock; script dir cleanup after
            .map_err(|e| format!("could not persist delete: {e}"))?;
        let Some(mut automation) = removed else {
            return Err(format!("no such automation: {id}"));
        };
        // Lock released. Kill in-flight script groups (no-op seam until U5),
        // and close the open rows on the removed record.
        let now = (self.clock)();
        let killer = Arc::clone(&self.script_killer.lock().unwrap());
        for run_id in running_run_ids(&automation) {
            if automation.mode.kind() == RunMode::Script {
                killer(&run_id);
            }
            automation.close(&run_id, failed(ERR_DELETED), now);
        }
        (self.emit_changed)(id);
        Ok(automation)
    }

    /// R23 manual run: allowed on a paused automation, never advances the
    /// schedule, and respects the R7 overlap skip. Dispatches regardless of
    /// the frontend-ready gate (a manual run is user-initiated — the R5 gate
    /// exists to defer *unattended* schedule fires, not explicit requests).
    /// A manual dispatch failure closes the row failed but leaves
    /// `next_run_at` alone — no occurrence was consumed, so there is nothing
    /// to recompute (R3 applies to scheduled claims).
    pub fn manual_run(&self, id: &str) -> Result<ManualRun, String> {
        let now = (self.clock)();
        let run_id = mint_id();
        let alive = Arc::clone(&self.agent_pane_alive.lock().unwrap());
        // Phase 1 (KTD-D/R2): decide + record + FLUSH under one lock hold.
        // `None` = unknown id; the inner Result carries the claim outcome.
        let decision: Option<Result<ManualRun, String>> = flush_tolerant(
            self.store.mutate(|map| {
                let Some(a) = map.get_mut(id) else {
                    return None;
                };
                if in_flight_widened(a, &alive) {
                    a.skip(now, Trigger::Manual, SKIP_IN_FLIGHT, &run_id);
                    return Some(Ok(ManualRun::Skipped {
                        run_id: run_id.clone(),
                    }));
                }
                // Manual claims pass the *current* next_run_at through
                // unchanged (R23: no advance); claim() records
                // scheduled_for: None for manual triggers.
                let keep = a.next_run_at;
                Some(
                    a.claim(keep, now, Trigger::Manual, &run_id)
                        .map(|()| ManualRun::Started {
                            run_id: run_id.clone(),
                        })
                        .map_err(|_| "automation is disabled".to_string()),
                )
            }),
            // Flush failed but the in-memory mutation applied (KTD-B store
            // contract): re-derive the decision from the recorded row.
            || {
                self.store.get(id).map(|a| {
                    match a.runs.iter().find(|r| r.id == run_id).map(|r| r.status) {
                        Some(RunStatus::Skipped) => Ok(ManualRun::Skipped {
                            run_id: run_id.clone(),
                        }),
                        Some(_) => Ok(ManualRun::Started {
                            run_id: run_id.clone(),
                        }),
                        None => Err("automation is disabled".to_string()),
                    }
                })
            },
        );
        let outcome = decision.ok_or_else(|| format!("no such automation: {id}"))??;
        if let ManualRun::Skipped { .. } = outcome {
            (self.emit_changed)(id);
            return Ok(outcome);
        }
        // Phase 2 (KTD-B): lock released — dispatch.
        let automation = self
            .store
            .get(id)
            .ok_or_else(|| format!("no such automation: {id}"))?;
        if let Err(e) = self.dispatch(&automation, &run_id) {
            let _ = self.store.mutate(|map| {
                if let Some(a) = map.get_mut(id) {
                    a.close(&run_id, failed(&e), now);
                }
            });
            (self.emit_changed)(id);
            // Manual runs are synchronous user requests: surface the failure
            // (the row already records it) instead of reporting Started.
            return Err(format!("run could not start: {e}"));
        }
        (self.emit_changed)(id);
        Ok(outcome)
    }

    /// U5/U7 seam: close a run row with a terminal outcome — how the script
    /// reaper (and later the agent lifecycle) reports results back. KTD-B
    /// discipline: mutate + flush under one lock hold, emit
    /// `automation://changed` after release. Callers run on their own
    /// threads (the runner's reaper), never under the store lock.
    ///
    /// Terminal rows stay closed ([`model::CloseResult::AlreadyClosed`]) —
    /// a reaper reporting after delete/shutdown already closed the row is a
    /// benign no-op. An unknown automation id (deleted mid-run) reports
    /// [`model::CloseResult::NotFound`].
    pub fn close_run(
        &self,
        automation_id: &str,
        run_id: &str,
        outcome: RunOutcome,
    ) -> model::CloseResult {
        let now = (self.clock)();
        let result = flush_tolerant(
            self.store.mutate(|map| {
                map.get_mut(automation_id)
                    .map(|a| a.close(run_id, outcome, now))
            }),
            // Flush failed but the mutation applied (KTD-B store contract):
            // re-derive the result from the authoritative map. A terminal
            // row reads as Closed (indistinguishable from AlreadyClosed
            // here; callers treat both as done).
            || {
                self.store.get(automation_id).map(|a| {
                    match a.runs.iter().find(|r| r.id == run_id) {
                        Some(r) if r.status.is_terminal() => model::CloseResult::Closed,
                        _ => model::CloseResult::NotFound,
                    }
                })
            },
        );
        match result {
            Some(res) => {
                if res == model::CloseResult::Closed {
                    (self.emit_changed)(automation_id); // after the lock (KTD-B)
                }
                res
            }
            None => model::CloseResult::NotFound,
        }
    }

    /// U7 Stop-event closure (KTD-F): find any automation run linked to a pane
    /// and close it as succeeded. Called by the hook dispatch on the first Stop.
    /// Idempotent: no-op if the pane is not linked to any automation run.
    pub fn close_run_by_pane(&self, pane_id: u64) -> Result<(), String> {
        let (automation_id, run_id) = {
            let map = self.store.snapshot();
            let mut found: Option<(String, String)> = None;
            for (auto_id, a) in map.iter() {
                for run in &a.runs {
                    if run.pane_id == Some(pane_id) && run.status == RunStatus::Running {
                        found = Some((auto_id.clone(), run.id.clone()));
                        break;
                    }
                }
                if found.is_some() {
                    break;
                }
            }
            found.ok_or_else(|| String::from("no running automation for pane"))?
        };
        // Close as succeeded with no output/exit code (the agent interaction
        // is in the pane's scrollback, not here).
        let outcome = RunOutcome::Succeeded { output: None };
        let result = self.close_run(&automation_id, &run_id, outcome);
        match result {
            model::CloseResult::Closed => Ok(()),
            model::CloseResult::NotFound | model::CloseResult::AlreadyClosed => {
                // Idempotent: a second Stop is a no-op.
                Ok(())
            }
        }
    }

    /// Snapshot of every automation (dashboard/CLI list reads).
    pub fn list(&self) -> Vec<Automation> {
        self.store.snapshot().into_values().collect()
    }

    /// One automation by id.
    pub fn get(&self, id: &str) -> Option<Automation> {
        self.store.get(id)
    }

    /// R6 passthrough: the store's degradation state for the dashboard.
    pub fn store_health(&self) -> StoreHealth {
        self.store.health()
    }

    // ---- the sweep (R2/R4/R7, KTD-C/D) ----------------------------------------

    /// One sweep tick, pure of threads and wall clocks — the
    /// `fly-automation-sweep` loop calls this with the injected clock's now;
    /// tests call it directly. Phases (KTD-B, visibly upheld):
    ///
    /// 1. **Under one store lock hold** ([`store::Store::mutate`], which also
    ///    flushes before returning — R2's persist-before-run): close R10
    ///    ack-timed-out agent rows; then for each due automation run the
    ///    KTD-D pre-claim checks (R7 overlap → skipped row + advance; U5
    ///    capacity → skipped("capacity") + advance) or claim it (advance via
    ///    [`schedule::advance`] + `Running` row). No `Running` row is ever
    ///    persisted and then abandoned: every claim collected here is
    ///    dispatched in phase 2, and a dispatch failure closes it.
    /// 2. **Lock released**: dispatch each claim. Failure → recompute
    ///    `next_run_at` from now (R3 — never the pre-claim value) and close
    ///    the row failed, in a second short mutate.
    /// 3. Emit `automation://changed` per touched automation — after all
    ///    store work.
    ///
    /// R5 frontend gating: while the webview has not signalled ready, a due
    /// **agent** automation is left completely untouched — not claimed, not
    /// skipped, not failed — because its dispatch event would fire into a
    /// listener-less void and burn the occurrence. It simply stays due and
    /// claims on a later tick once ready (advance-from-now then collapses
    /// any backlog into one run, R4). **Script** automations claim and
    /// dispatch normally: a webview that never loads must still run script
    /// watchdogs (R5).
    pub fn sweep_once(&self, now_ms: u64) {
        let frontend_ready = self.frontend_ready.load(Ordering::Acquire);
        let alive = Arc::clone(&self.agent_pane_alive.lock().unwrap());
        let capacity = Arc::clone(&self.script_capacity.lock().unwrap());

        // Phase 1: decide + mutate + flush under ONE lock hold.
        let mut changed: Vec<String> = Vec::new();
        let mut to_dispatch: Vec<(Automation, String)> = Vec::new();
        let _ = self.store.mutate(|map| {
            for a in map.values_mut() {
                let mut touched = false;

                // R10 ack-timeout scaffolding: agent rows never linked to a
                // pane within the window close failed.
                for run_id in ack_timed_out_agent_runs(a, now_ms) {
                    a.close(&run_id, failed(ERR_SPAWN_ACK), now_ms);
                    touched = true;
                }

                // Due = enabled ∧ next_run_at <= now (None = paused, R23).
                let due = a.enabled && a.next_run_at.is_some_and(|t| t <= now_ms);
                if due {
                    let is_agent = a.mode.kind() == RunMode::Agent;
                    if is_agent && !frontend_ready {
                        // R5: defer — see the method doc. Deliberately no row,
                        // no advance, no event.
                    } else if in_flight_widened(a, &alive) {
                        // R7/KTD-D pre-claim skip: record + STILL advance the
                        // schedule past the skipped occurrence.
                        a.skip(now_ms, Trigger::Schedule, SKIP_IN_FLIGHT, &mint_id());
                        a.rollback_recompute(advance_or_pause(a, now_ms));
                        touched = true;
                    } else if !is_agent && !capacity() {
                        // U5 capacity pre-claim skip (KTD-D): never a stranded
                        // Running row.
                        a.skip(now_ms, Trigger::Schedule, SKIP_CAPACITY, &mint_id());
                        a.rollback_recompute(advance_or_pause(a, now_ms));
                        touched = true;
                    } else {
                        // Claim (R2/R4): advance from NOW — collapsing any
                        // backlog into this one occurrence — and append the
                        // Running row. The enclosing mutate() flushes before
                        // returning: persist-before-dispatch.
                        let advanced = advance_or_pause(a, now_ms);
                        let run_id = mint_id();
                        if a.claim(advanced, now_ms, Trigger::Schedule, &run_id).is_ok() {
                            to_dispatch.push((a.clone(), run_id));
                            touched = true;
                        }
                    }
                }
                if touched {
                    changed.push(a.id.clone());
                }
            }
        });

        // Phase 2: the store lock is RELEASED — dispatch (KTD-B: the
        // load-bearing discipline; a Dispatcher may safely call back into
        // list()/get() from here).
        for (automation, run_id) in to_dispatch {
            if let Err(e) = self.dispatch(&automation, &run_id) {
                // R3: recompute from now — never restore the pre-claim value
                // (it could clobber a concurrent edit). Pure math outside the
                // lock, applied in a second short mutate.
                let recomputed = schedule::advance(&automation.cron, &automation.timezone, now_ms)
                    .ok()
                    .flatten();
                let _ = self.store.mutate(|map| {
                    if let Some(a) = map.get_mut(&automation.id) {
                        a.close(&run_id, failed(&e), now_ms);
                        a.rollback_recompute(recomputed);
                    }
                });
            }
        }

        // Phase 3: emit — after all store work, no lock held (KTD-B).
        for id in changed {
            (self.emit_changed)(&id);
        }
    }

    /// R5 shutdown half (called from `lifecycle::shutdown` after the sweep
    /// thread is joined): kill in-flight script groups via the U5 seam
    /// (outside the store lock, KTD-B), then close every `Running` row
    /// failed([`ERR_INTERRUPTED`]) in one final flush.
    pub fn shutdown(&self) {
        let now = (self.clock)();
        let killer = Arc::clone(&self.script_killer.lock().unwrap());
        // Kill first, with no store lock held (KTD-B): collect in-flight
        // script run ids from a snapshot.
        for a in self.store.snapshot().values() {
            if a.mode.kind() == RunMode::Script {
                for run_id in running_run_ids(a) {
                    killer(&run_id);
                }
            }
        }
        // Then close all in-flight rows — the final flush.
        let _ = self.store.mutate(|map| {
            for a in map.values_mut() {
                for run_id in running_run_ids(a) {
                    a.close(&run_id, failed(ERR_INTERRUPTED), now);
                }
            }
        });
    }

    // ---- internals ------------------------------------------------------------

    /// Route a claimed run to the dispatcher (lock-free by construction: the
    /// dispatcher Arc is cloned out of its cell first, and callers only
    /// invoke this after the store mutate returned).
    fn dispatch(&self, automation: &Automation, run_id: &str) -> Result<(), String> {
        let dispatcher = Arc::clone(&self.dispatcher.lock().unwrap());
        match automation.mode.kind() {
            RunMode::Agent => dispatcher.dispatch_agent(automation, run_id),
            RunMode::Script => dispatcher.dispatch_script(automation, run_id),
        }
    }

    /// Mutate one automation under the store lock (flushes on return) and
    /// return the updated record; errors on an unknown id. Flush-tolerant
    /// (KTD-B store contract: the in-memory mutation is kept on a flush
    /// failure, so the op succeeded — refetch the authoritative record).
    fn with_automation(
        &self,
        id: &str,
        f: impl FnOnce(&mut Automation),
    ) -> Result<Automation, String> {
        flush_tolerant(
            self.store.mutate(|map| {
                map.get_mut(id).map(|a| {
                    f(a);
                    a.clone()
                })
            }),
            || self.store.get(id),
        )
        .ok_or_else(|| format!("no such automation: {id}"))
    }
}

/// Unwrap a [`store::Store::mutate`] result under the KTD-B store contract:
/// a flush failure keeps the in-memory mutation (the map is the authority),
/// so the operation itself succeeded — log the degradation (health already
/// recorded it for the dashboard) and recover the result via `refetch`.
fn flush_tolerant<R>(result: std::io::Result<R>, refetch: impl FnOnce() -> R) -> R {
    match result {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "[fly-automations] store flush failed ({e}); \
                 in-memory state remains authoritative (KTD-B)"
            );
            refetch()
        }
    }
}

/// Ids of rows still `Running` (helper shared by recovery/delete/shutdown).
fn running_run_ids(a: &Automation) -> Vec<String> {
    a.runs
        .iter()
        .filter(|r| r.status == RunStatus::Running)
        .map(|r| r.id.clone())
        .collect()
}

/// Agent rows past the R10 ack window with no pane ever linked.
fn ack_timed_out_agent_runs(a: &Automation, now_ms: u64) -> Vec<String> {
    a.runs
        .iter()
        .filter(|r| {
            r.status == RunStatus::Running
                && r.mode == RunMode::Agent
                && r.pane_id.is_none()
                && r.started_at
                    .is_some_and(|t| t.saturating_add(AGENT_ACK_TIMEOUT_MS) <= now_ms)
        })
        .map(|r| r.id.clone())
        .collect()
}

/// R7 with the U7 widening: in flight = a `Running` row exists, or the probe
/// says a terminal agent row's linked pane is still alive (deadline-failed
/// but not done — a stuck agent must skip, not fan out).
fn in_flight_widened(a: &Automation, alive: &PaneAliveProbe) -> bool {
    a.in_flight()
        || a.runs
            .iter()
            .any(|r| r.status == RunStatus::Failed && r.pane_id.is_some() && alive(r))
}

/// [`schedule::advance`] with degraded fallbacks: an unparseable stored
/// cron/tz (same-UID file edits) or an exhausted schedule pauses the
/// automation (`None`) instead of wedging the sweep.
fn advance_or_pause(a: &Automation, now_ms: u64) -> Option<u64> {
    match schedule::advance(&a.cron, &a.timezone, now_ms) {
        Ok(next) => next,
        Err(e) => {
            eprintln!(
                "[fly-automations] cannot advance schedule for {:?} ({e}); pausing it",
                a.id
            );
            None
        }
    }
}

/// A `Failed` outcome with no exit code / output — the shape every
/// manager-side close uses (interrupted / deleted / ack timeout / dispatch
/// error). Runner-side closes (U5/U7) carry codes and output themselves.
fn failed(error: &str) -> RunOutcome {
    RunOutcome::Failed {
        error: error.to_owned(),
        exit_code: None,
        output: None,
    }
}

/// Mint a short random alphanumeric id (automation and run ids). `rand` is
/// already in-tree for the hook tokens; ids are identity, not secrets, so
/// thread_rng's CSPRNG is more than enough.
fn mint_id() -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..ID_LEN)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect()
}

// ---- the sweep thread (KTD-C) -------------------------------------------------

/// Handle to the running `fly-automation-sweep` thread, managed by Tauri so
/// `lifecycle::shutdown` can stop and join it (outside any store lock,
/// KTD-B).
pub struct SweepHandle {
    manager: Arc<AutomationManager>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl SweepHandle {
    /// Stop the sweep (flag + condvar wake, so the 10s wait ends instantly)
    /// and join the thread. Idempotent. The caller holds no store lock
    /// (KTD-B); the in-progress tick, if any, completes before the join
    /// returns, so no claim can race the shutdown close that follows.
    pub fn stop_and_join(&self) {
        {
            let mut stop = self.manager.sweep_stop.lock().unwrap();
            *stop = true;
            self.manager.sweep_wake.notify_all();
        }
        if let Some(h) = self.handle.lock().unwrap().take() {
            let _ = h.join();
        }
    }
}

/// Start the named sweep thread (KTD-C: a plain `std::thread` loop like
/// `fly-hook-accept`, no daemon, no async timer). It ticks every
/// [`SWEEP_TICK_MS`], sweeping once immediately so an overdue backlog fires
/// without waiting a tick — agent runs still defer until frontend-ready (R5).
pub fn start_sweep(manager: Arc<AutomationManager>) -> std::io::Result<SweepHandle> {
    let mgr = Arc::clone(&manager);
    let handle = std::thread::Builder::new()
        .name("fly-automation-sweep".into())
        .spawn(move || loop {
            mgr.sweep_once((mgr.clock)());
            let stop = mgr.sweep_stop.lock().unwrap();
            if *stop {
                break;
            }
            let (stop, _) = mgr
                .sweep_wake
                .wait_timeout(stop, Duration::from_millis(SWEEP_TICK_MS))
                .unwrap();
            if *stop {
                break;
            }
            // A spurious wake just sweeps early — harmless (due-ness is
            // absolute, not tick-relative).
        })?;
    Ok(SweepHandle {
        manager,
        handle: Mutex::new(Some(handle)),
    })
}

// ---- Tauri command surface -----------------------------------------------------

/// R5: the frontend signals it has finished restore and is listening for
/// automation events. Until this arrives the sweep defers due **agent**
/// automations (script automations run regardless — see
/// [`AutomationManager::sweep_once`]). Registered in `lib.rs`; the frontend
/// caller arrives with U8.
#[tauri::command]
pub fn automations_frontend_ready(manager: tauri::State<'_, Arc<AutomationManager>>) {
    manager.set_frontend_ready();
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;
    use std::sync::Mutex;

    use super::*;

    /// 2026-01-06T00:00:00Z — a real instant (croner needs valid dates) on a
    /// 5-minute boundary, so `*/5 * * * *` in UTC steps in clean 5-minute
    /// increments from here.
    const T0: u64 = 1_767_657_600_000;
    const FIVE_MIN: u64 = 5 * 60 * 1000;

    /// Fake dispatcher: records calls, optionally fails, and — for the KTD-B
    /// structural assertion — optionally calls back into `manager.list()`
    /// from inside the dispatch, which would deadlock on the non-reentrant
    /// store mutex if the sweep dispatched while holding it.
    #[derive(Default)]
    struct FakeDispatcher {
        calls: Mutex<Vec<(String, String, RunMode)>>,
        fail_with: Mutex<Option<String>>,
        reenter: Mutex<Option<Arc<AutomationManager>>>,
    }

    impl FakeDispatcher {
        fn record(&self, a: &Automation, run_id: &str, mode: RunMode) -> Result<(), String> {
            if let Some(mgr) = self.reenter.lock().unwrap().as_ref() {
                // KTD-B: safe exactly because dispatch runs outside the lock.
                let _ = mgr.list();
            }
            self.calls
                .lock()
                .unwrap()
                .push((a.id.clone(), run_id.to_owned(), mode));
            match self.fail_with.lock().unwrap().clone() {
                Some(e) => Err(e),
                None => Ok(()),
            }
        }
        fn count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    impl Dispatcher for FakeDispatcher {
        fn dispatch_agent(&self, a: &Automation, run_id: &str) -> Result<(), String> {
            self.record(a, run_id, RunMode::Agent)
        }
        fn dispatch_script(&self, a: &Automation, run_id: &str) -> Result<(), String> {
            self.record(a, run_id, RunMode::Script)
        }
    }

    struct Harness {
        mgr: Arc<AutomationManager>,
        clock: Arc<AtomicU64>,
        events: Arc<Mutex<Vec<String>>>,
        dispatcher: Arc<FakeDispatcher>,
        dir: tempfile::TempDir,
    }

    impl Harness {
        fn now(&self) -> u64 {
            self.clock.load(Ordering::SeqCst)
        }
        fn set_now(&self, t: u64) {
            self.clock.store(t, Ordering::SeqCst);
        }
        fn sweep(&self) {
            self.mgr.sweep_once(self.now());
        }
        fn runs(&self, id: &str) -> Vec<RunRow> {
            self.mgr.get(id).expect("automation exists").runs
        }
        fn next_run_at(&self, id: &str) -> Option<u64> {
            self.mgr.get(id).expect("automation exists").next_run_at
        }
    }

    fn store_in(dir: &tempfile::TempDir) -> Store {
        Store::load_at(
            dir.path().join("data").join("automations.json"),
            dir.path().join("data").join("automation-scripts"),
        )
    }

    /// Manager over a tempdir store with fake clock/dispatcher/emitter (the
    /// state/manager.rs everything-injected shape). Construction runs the R5
    /// startup recovery against whatever `dir` already holds.
    fn harness_in(dir: tempfile::TempDir) -> Harness {
        let clock = Arc::new(AtomicU64::new(T0));
        let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let dispatcher = Arc::new(FakeDispatcher::default());
        let c = Arc::clone(&clock);
        let ev = Arc::clone(&events);
        let mgr = Arc::new(AutomationManager::new(
            store_in(&dir),
            Arc::clone(&dispatcher) as Arc<dyn Dispatcher>,
            Box::new(move || c.load(Ordering::SeqCst)),
            Box::new(move |id: &str| ev.lock().unwrap().push(id.to_owned())),
        ));
        Harness {
            mgr,
            clock,
            events,
            dispatcher,
            dir,
        }
    }

    fn harness() -> Harness {
        harness_in(tempfile::tempdir().unwrap())
    }

    fn origin() -> Origin {
        Origin {
            pane_id: 7,
            workspace_id: "ws-1".into(),
            label: "cli".into(),
        }
    }

    fn script_spec(name: &str) -> CreateSpec {
        CreateSpec {
            name: name.into(),
            cron: "*/5 * * * *".into(),
            timezone: "UTC".into(),
            cwd: "/tmp".into(),
            mode: CreateMode::Script {
                content: "echo hi".into(),
                interpreter: "bash".into(),
                timeout_ms: 120_000,
            },
            origin: origin(),
        }
    }

    fn agent_spec(name: &str) -> CreateSpec {
        CreateSpec {
            mode: CreateMode::Agent {
                prompt: "summarize overnight CI".into(),
            },
            ..script_spec(name)
        }
    }

    /// Create at T0 and move the clock to the first occurrence (T0 + 5min),
    /// so the automation is exactly due.
    fn create_due(h: &Harness, spec: CreateSpec) -> String {
        assert_eq!(h.now(), T0, "create_due assumes a fresh harness clock");
        let created = h.mgr.create(spec).expect("create succeeds");
        assert_eq!(
            created.automation.next_run_at,
            Some(T0 + FIVE_MIN),
            "initial next_run_at lands on the first boundary"
        );
        h.set_now(T0 + FIVE_MIN);
        created.automation.id
    }

    // R2/KTD-D: one due occurrence yields exactly one claim — a second tick
    // at the same instant finds next_run_at already advanced (the claim and
    // the advance are one atomic, flushed step).
    #[test]
    fn due_automation_claims_exactly_once_per_occurrence_r2() {
        let h = harness();
        let id = create_due(&h, script_spec("disk watch"));

        h.sweep();
        h.sweep(); // same now: the occurrence is consumed

        assert_eq!(h.dispatcher.count(), 1, "one dispatch per occurrence");
        let runs = h.runs(&id);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, RunStatus::Running);
        assert_eq!(runs[0].trigger, Trigger::Schedule);
        assert_eq!(runs[0].scheduled_for, Some(T0 + FIVE_MIN));
        assert_eq!(
            h.next_run_at(&id),
            Some(T0 + 2 * FIVE_MIN),
            "advanced from now to the next boundary"
        );
    }

    // R3: a dispatch failure closes the row failed and RECOMPUTES the
    // schedule from now (never restores the pre-claim value); once the next
    // occurrence arrives, a fresh claim retries.
    #[test]
    fn failing_dispatcher_recomputes_schedule_from_now_fails_row_and_retries_r3() {
        let h = harness();
        let id = create_due(&h, script_spec("flaky"));
        *h.dispatcher.fail_with.lock().unwrap() = Some("boom".into());

        h.sweep();

        let runs = h.runs(&id);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, RunStatus::Failed);
        assert_eq!(runs[0].error.as_deref(), Some("boom"));
        assert_eq!(
            h.next_run_at(&id),
            Some(T0 + 2 * FIVE_MIN),
            "recomputed from now (R3), not cleared, not restored"
        );

        // The failed row is terminal — not in flight — so the next
        // occurrence claims again.
        *h.dispatcher.fail_with.lock().unwrap() = None;
        h.set_now(T0 + 2 * FIVE_MIN);
        h.sweep();
        let runs = h.runs(&id);
        assert_eq!(runs.len(), 2, "retried on the next occurrence");
        assert_eq!(runs[1].status, RunStatus::Running);
        assert_eq!(h.dispatcher.count(), 2);
    }

    // R7/KTD-D: a due automation with a run in flight records a Skipped row
    // (born terminal, no Running row ever persisted) and the schedule STILL
    // advances past the skipped occurrence.
    #[test]
    fn in_flight_run_records_skipped_row_and_still_advances_schedule_r7() {
        let h = harness();
        let id = create_due(&h, script_spec("slow script"));
        h.sweep(); // claim #1: Running (nothing closes it — no runner in U4)

        h.set_now(T0 + 2 * FIVE_MIN); // next occurrence due, run still in flight
        h.sweep();

        assert_eq!(h.dispatcher.count(), 1, "no second dispatch (AE4)");
        let runs = h.runs(&id);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[1].status, RunStatus::Skipped);
        assert_eq!(runs[1].error.as_deref(), Some(SKIP_IN_FLIGHT));
        assert_eq!(runs[1].trigger, Trigger::Schedule);
        assert_eq!(
            h.next_run_at(&id),
            Some(T0 + 3 * FIVE_MIN),
            "schedule advances past the skipped occurrence (KTD-D)"
        );
    }

    // R4: a next_run_at three days stale (app closed / machine asleep)
    // collapses into exactly one claim, with the next occurrence computed
    // from NOW — never a catch-up burst (AE6).
    #[test]
    fn stale_next_run_at_three_days_past_collapses_to_one_claim_from_now_r4() {
        let h = harness();
        let id = create_due(&h, script_spec("hourly-ish"));
        let now = T0 + 3 * 24 * 60 * 60 * 1000; // 3 days later (a boundary)
        h.set_now(now);

        h.sweep();
        h.sweep();

        assert_eq!(h.dispatcher.count(), 1, "backlog collapsed to one run");
        let runs = h.runs(&id);
        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs[0].scheduled_for,
            Some(T0 + FIVE_MIN),
            "the row records the stale occurrence it consumed"
        );
        assert_eq!(
            h.next_run_at(&id),
            Some(now + FIVE_MIN),
            "next occurrence computed from now, not from the backlog"
        );
    }

    // R5: startup recovery closes rows still Running in the loaded store as
    // failed("interrupted") — a prior app run's outcome is unknowable — and
    // flushes once.
    #[test]
    fn startup_recovery_closes_orphaned_running_rows_failed_interrupted_r5() {
        let dir = tempfile::tempdir().unwrap();
        {
            // A prior app run: a claim was persisted (R2's flush-before-
            // dispatch), then the app died before the run could close.
            let store = store_in(&dir);
            store
                .mutate(|map| {
                    let mut a = raw_automation("a1");
                    a.claim(Some(T0 + 2 * FIVE_MIN), T0 + FIVE_MIN, Trigger::Schedule, "r1")
                        .unwrap();
                    map.insert(a.id.clone(), a);
                })
                .unwrap();
        }
        let h = harness_in(dir); // construction runs recovery

        let row = &h.runs("a1")[0];
        assert_eq!(row.status, RunStatus::Failed);
        assert_eq!(row.error.as_deref(), Some(ERR_INTERRUPTED));
        assert_eq!(row.finished_at, Some(T0), "closed at the recovery clock");
        // And the recovery was flushed: a raw reload from disk agrees.
        let reloaded = store_in(&h.dir);
        assert_eq!(
            reloaded.get("a1").unwrap().runs[0].status,
            RunStatus::Failed
        );
    }

    // R23/AE7: pause nulls next_run_at; resume RECOMPUTES from now — an
    // automation paused for a week must not fire instantly on a stale value.
    #[test]
    fn resume_after_pause_recomputes_from_now_never_instant_fires_r23() {
        let h = harness();
        let id = create_due(&h, script_spec("paused a week"));

        let paused = h.mgr.pause(&id).unwrap();
        assert_eq!(paused.next_run_at, None, "paused = next_run_at nulled");

        let now = T0 + 7 * 24 * 60 * 60 * 1000 + 12_345; // a week later
        h.set_now(now);
        let resumed = h.mgr.resume(&id).unwrap();
        let next = resumed.next_run_at.expect("re-armed");
        assert!(next > now, "strictly in the future — no instant fire");

        h.sweep();
        assert_eq!(h.dispatcher.count(), 0, "nothing due right after resume");
        assert!(h.runs(&id).is_empty());
    }

    // R23: a manual run is allowed on a paused automation, never consumes an
    // occurrence, and never advances the schedule.
    #[test]
    fn manual_run_on_paused_automation_runs_without_advancing_schedule_r23() {
        let h = harness();
        let id = create_due(&h, script_spec("manual on paused"));
        h.mgr.pause(&id).unwrap();

        let outcome = h.mgr.manual_run(&id).unwrap();
        assert!(matches!(outcome, ManualRun::Started { .. }));
        assert_eq!(h.dispatcher.count(), 1);
        let runs = h.runs(&id);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, RunStatus::Running);
        assert_eq!(runs[0].trigger, Trigger::Manual);
        assert_eq!(runs[0].scheduled_for, None, "no occurrence consumed");
        assert_eq!(h.next_run_at(&id), None, "still paused — never advanced");
    }

    // R7/R23: a manual run while a run is in flight is skipped, not queued
    // and not concurrent.
    #[test]
    fn manual_run_during_in_flight_run_is_skipped_r7() {
        let h = harness();
        let id = create_due(&h, script_spec("busy"));
        h.sweep(); // in flight

        let outcome = h.mgr.manual_run(&id).unwrap();
        assert!(matches!(outcome, ManualRun::Skipped { .. }));
        assert_eq!(h.dispatcher.count(), 1, "no second dispatch");
        let runs = h.runs(&id);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[1].status, RunStatus::Skipped);
        assert_eq!(runs[1].trigger, Trigger::Manual);
        assert_eq!(runs[1].error.as_deref(), Some(SKIP_IN_FLIGHT));
    }

    // R23 teardown: delete with an in-flight script run kills its group (U5
    // killer seam, invoked outside the store lock), closes the open row
    // failed("deleted"), removes the record + stored script content. (The
    // agent pane recursion-registry entry surviving until pane exit is U7's
    // registry — only a doc note here.)
    #[test]
    fn delete_with_in_flight_run_kills_script_group_and_closes_rows_deleted_r23() {
        let h = harness();
        let id = create_due(&h, script_spec("doomed"));
        h.sweep(); // in flight
        let run_id = h.runs(&id)[0].id.clone();
        let script_file = match h.mgr.get(&id).unwrap().mode {
            Mode::Script { ref script_file, .. } => std::path::PathBuf::from(script_file),
            _ => unreachable!(),
        };
        assert!(script_file.is_file(), "script content stored on create");

        let killed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let k = Arc::clone(&killed);
        h.mgr
            .set_script_killer(Arc::new(move |rid: &str| k.lock().unwrap().push(rid.into())));

        let removed = h.mgr.delete(&id).unwrap();

        assert_eq!(*killed.lock().unwrap(), vec![run_id.clone()]);
        let row = removed.runs.iter().find(|r| r.id == run_id).unwrap();
        assert_eq!(row.status, RunStatus::Failed);
        assert_eq!(row.error.as_deref(), Some(ERR_DELETED));
        assert!(h.mgr.get(&id).is_none(), "record removed");
        assert!(!script_file.exists(), "stored script content removed");
        assert!(h.events.lock().unwrap().contains(&id));
        assert!(h.mgr.delete(&id).is_err(), "second delete: no such automation");
    }

    // R1: create persists, emits automation://changed, and RETURNS the
    // advisory min-gap warning alongside success (the CLI prints it, U9) —
    // hard validation errors reject instead.
    #[test]
    fn create_emits_changed_event_and_returns_advisory_warning_r1() {
        let h = harness();
        let clean = h.mgr.create(script_spec("clean")).unwrap();
        assert_eq!(clean.warning, None);
        assert!(h.events.lock().unwrap().contains(&clean.automation.id));

        let fast = h
            .mgr
            .create(CreateSpec {
                cron: "* * * * *".into(),
                ..script_spec("too fast")
            })
            .unwrap();
        let warning = fast.warning.expect("advisory warning returned, not raised");
        assert!(warning.contains("5-minute"), "{warning}");
        assert!(h.mgr.get(&fast.automation.id).is_some(), "still persisted");
    }

    // R1/R21: hard validation errors (bad cron, unknown tz) reject the
    // create — nothing persists, nothing emits.
    #[test]
    fn create_rejects_hard_validation_errors_without_persisting_r1() {
        let h = harness();
        let err = h
            .mgr
            .create(CreateSpec {
                cron: "99 * * * *".into(),
                ..script_spec("bad cron")
            })
            .unwrap_err();
        assert!(err.contains("99 * * * *"), "{err}");

        let err = h
            .mgr
            .create(CreateSpec {
                timezone: "America/Nowhere".into(),
                ..script_spec("bad tz")
            })
            .unwrap_err();
        assert!(err.contains("America/Nowhere"), "{err}");

        assert!(h.mgr.list().is_empty());
        assert!(h.events.lock().unwrap().is_empty());
    }

    // R5: while the frontend has not signalled ready, a due AGENT automation
    // is deferred — no row, no advance, no failure (its dispatch event would
    // fire into a listener-less void and burn the occurrence) — while a due
    // SCRIPT automation claims and dispatches normally. Once ready, the
    // deferred agent claims on the next tick.
    #[test]
    fn frontend_not_ready_defers_agent_claims_while_script_claims_proceed_r5() {
        let h = harness();
        let agent = h.mgr.create(agent_spec("agent")).unwrap().automation.id;
        let script = h.mgr.create(script_spec("script")).unwrap().automation.id;
        h.set_now(T0 + FIVE_MIN); // both due

        h.sweep();

        assert!(h.runs(&agent).is_empty(), "agent deferred: no row of any kind");
        assert_eq!(
            h.next_run_at(&agent),
            Some(T0 + FIVE_MIN),
            "agent still due — the occurrence is not burned"
        );
        assert_eq!(h.runs(&script).len(), 1, "script ran regardless (R5)");
        assert_eq!(h.runs(&script)[0].status, RunStatus::Running);

        h.mgr.set_frontend_ready();
        h.sweep();
        let agent_runs = h.runs(&agent);
        assert_eq!(agent_runs.len(), 1, "deferred agent claims once ready");
        assert_eq!(agent_runs[0].status, RunStatus::Running);
        assert_eq!(h.dispatcher.count(), 2);
    }

    // KTD-D: the U5 capacity pre-claim check — a due script automation with
    // no capacity records skipped("capacity") and the schedule advances; no
    // Running row is ever persisted, nothing dispatches.
    #[test]
    fn capacity_unavailable_script_records_skipped_capacity_row_and_advances_ktd_d() {
        let h = harness();
        let id = create_due(&h, script_spec("at capacity"));
        h.mgr.set_script_capacity(Arc::new(|| false));

        h.sweep();

        assert_eq!(h.dispatcher.count(), 0);
        let runs = h.runs(&id);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, RunStatus::Skipped);
        assert_eq!(runs[0].error.as_deref(), Some(SKIP_CAPACITY));
        assert_eq!(h.next_run_at(&id), Some(T0 + 2 * FIVE_MIN), "advanced");
    }

    // KTD-B (structural, the load-bearing lock discipline): the dispatcher
    // itself calls back into manager.list() — which takes the store lock —
    // from inside dispatch. If the sweep or manual_run dispatched while
    // holding the store lock, this would deadlock; completing proves the
    // claim flush and the dispatch are separate lock scopes.
    #[test]
    fn dispatch_runs_outside_the_store_lock_ktd_b() {
        let h = harness();
        let id = create_due(&h, script_spec("reentrant"));
        *h.dispatcher.reenter.lock().unwrap() = Some(Arc::clone(&h.mgr));

        h.sweep(); // scheduled-claim dispatch path
        assert_eq!(h.dispatcher.count(), 1);

        // Drain the in-flight row so the manual path claims too.
        let run_id = h.runs(&id)[0].id.clone();
        let _ = h.mgr.store.mutate(|map| {
            map.get_mut(&id)
                .map(|a| a.close(&run_id, RunOutcome::Succeeded { output: None }, T0));
        });
        let outcome = h.mgr.manual_run(&id).unwrap(); // manual-dispatch path
        assert!(matches!(outcome, ManualRun::Started { .. }));
        assert_eq!(h.dispatcher.count(), 2);
    }

    // U5: close_run is the runner's report-back seam — it closes the Running
    // row with the outcome, flushes, and emits automation://changed after
    // the lock; terminal rows stay closed; unknown ids report NotFound.
    #[test]
    fn close_run_closes_running_row_flushes_and_emits_changed_u5() {
        let h = harness();
        let id = create_due(&h, script_spec("reporting"));
        h.sweep(); // claim → Running
        let run_id = h.runs(&id)[0].id.clone();
        h.events.lock().unwrap().clear();

        let res = h.mgr.close_run(
            &id,
            &run_id,
            RunOutcome::Succeeded {
                output: Some("[stderr]\nwarn".into()),
            },
        );
        assert_eq!(res, model::CloseResult::Closed);
        let row = &h.runs(&id)[0];
        assert_eq!(row.status, RunStatus::Succeeded);
        assert_eq!(row.output.as_deref(), Some("[stderr]\nwarn"));
        assert_eq!(row.finished_at, Some(h.now()));
        assert_eq!(*h.events.lock().unwrap(), vec![id.clone()], "emitted once");
        // Flushed: a raw reload agrees.
        assert_eq!(
            store_in(&h.dir).get(&id).unwrap().runs[0].status,
            RunStatus::Succeeded
        );

        // Terminal rows stay closed (a reaper reporting after delete/
        // shutdown already closed the row): no second emit.
        let res = h.mgr.close_run(
            &id,
            &run_id,
            RunOutcome::Failed {
                error: "late".into(),
                exit_code: Some(1),
                output: None,
            },
        );
        assert_eq!(res, model::CloseResult::AlreadyClosed);
        assert_eq!(h.runs(&id)[0].status, RunStatus::Succeeded, "first close wins");
        assert_eq!(h.events.lock().unwrap().len(), 1, "no emit for a no-op");

        // Unknown automation (deleted mid-run) reports NotFound calmly.
        assert_eq!(
            h.mgr
                .close_run("ghost", "r1", RunOutcome::Succeeded { output: None }),
            model::CloseResult::NotFound
        );
    }

    // R10 scaffolding: an agent run whose spawn is never acked (no pane
    // linked) closes failed at the 30s window on a later tick.
    #[test]
    fn sweep_closes_agent_runs_unacked_past_the_ack_window_r10() {
        let h = harness();
        h.mgr.set_frontend_ready();
        let id = create_due(&h, agent_spec("never spawns"));
        h.sweep(); // claim + dispatch ok; no pane will ever ack in U4

        h.set_now(T0 + FIVE_MIN + AGENT_ACK_TIMEOUT_MS - 1);
        h.sweep();
        assert_eq!(h.runs(&id)[0].status, RunStatus::Running, "window still open");

        h.set_now(T0 + FIVE_MIN + AGENT_ACK_TIMEOUT_MS);
        h.sweep();
        let runs = h.runs(&id);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, RunStatus::Failed);
        assert_eq!(runs[0].error.as_deref(), Some(ERR_SPAWN_ACK));
    }

    // R5 shutdown: kill in-flight script groups (killer seam, outside the
    // store lock) and close every in-flight row failed("interrupted") in one
    // final flush — agent rows close too, but only script groups are killed.
    #[test]
    fn shutdown_kills_script_groups_and_closes_in_flight_rows_interrupted_r5() {
        let h = harness();
        h.mgr.set_frontend_ready();
        let script = h.mgr.create(script_spec("script")).unwrap().automation.id;
        let agent = h.mgr.create(agent_spec("agent")).unwrap().automation.id;
        h.set_now(T0 + FIVE_MIN); // both due
        h.sweep(); // → both claim
        let script_run = h.runs(&script)[0].id.clone();

        let killed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let k = Arc::clone(&killed);
        h.mgr
            .set_script_killer(Arc::new(move |rid: &str| k.lock().unwrap().push(rid.into())));

        h.mgr.shutdown();

        assert_eq!(
            *killed.lock().unwrap(),
            vec![script_run],
            "script group killed; the agent pane is never killed (R23/R5)"
        );
        for id in [&script, &agent] {
            let row = &h.runs(id)[0];
            assert_eq!(row.status, RunStatus::Failed);
            assert_eq!(row.error.as_deref(), Some(ERR_INTERRUPTED));
        }
        // Final flush: a raw reload sees the closed rows.
        let reloaded = store_in(&h.dir);
        for a in reloaded.snapshot().values() {
            assert_eq!(a.runs[0].status, RunStatus::Failed);
        }
    }

    // KTD-C/R5: the named sweep thread starts, ticks, and stops promptly on
    // the flag + condvar wake (no 10s lag); stop_and_join is idempotent.
    #[test]
    fn sweep_thread_stops_and_joins_promptly_on_shutdown_ktd_c() {
        let h = harness();
        let id = create_due(&h, script_spec("threaded"));
        let handle = start_sweep(Arc::clone(&h.mgr)).expect("thread starts");
        // The loop sweeps immediately on entry; wait for the claim.
        for _ in 0..200 {
            if !h.runs(&id).is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(h.runs(&id).len(), 1, "the real thread swept");
        handle.stop_and_join();
        handle.stop_and_join(); // idempotent
    }

    /// A hand-built record for tests that seed the store directly (the
    /// recovery test's "prior app run").
    fn raw_automation(id: &str) -> Automation {
        Automation {
            id: id.into(),
            name: format!("watch {id}"),
            cron: "*/5 * * * *".into(),
            timezone: "UTC".into(),
            enabled: true,
            cwd: "/tmp".into(),
            mode: Mode::Script {
                script_file: "script".into(),
                interpreter: "bash".into(),
                timeout_ms: 120_000,
            },
            origin: origin(),
            created_at: T0,
            updated_at: T0,
            next_run_at: Some(T0 + FIVE_MIN),
            runs: Vec::new(),
        }
    }
}
