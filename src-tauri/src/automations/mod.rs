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

pub mod alerts;
pub mod model;
pub mod redact;
pub mod schedule;
pub mod script;
pub mod store;

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use tauri::Emitter;

use crate::config::{AutomationDefaults, ConfigStore};
use model::{
    Automation, Mode, Origin, RunMode, RunOutcome, RunRow, RunStatus, Trigger, ERR_DELETED,
    ERR_INTERRUPTED, ERR_TIMED_OUT,
};
use store::{Store, StoreHealth};

/// The model / reasoning effort / fallback an agent run actually launches with
/// (automations-workspace-and-model plan, U4a — R11/R13). Resolved once per
/// dispatch by [`resolve_agent_launch`], recorded on the run row (R13), and
/// carried to the frontend on the `automation://agent-run` event so the pane
/// launches with the exact flags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedLaunch {
    /// `--model` value; `None` ⇒ omit the flag (Claude's own default).
    pub model: Option<String>,
    /// `--effort` value; `None` ⇒ omit the flag.
    pub effort: Option<String>,
    /// `--fallback-model` value; `None` when it equals the resolved primary
    /// (a fallback identical to the primary is a no-op — omit it).
    pub fallback: Option<String>,
}

/// Deterministic launch resolution (U4a — R11/R12/R15). The primary model and
/// effort resolve **automation → shared default → Claude's own default**
/// (`None` at the end means "omit the flag, let Claude decide"). The fallback
/// is always the shared default's `fallback_model`, except it is omitted when
/// it already equals the resolved primary. Pure — unit-tested without a manager.
pub fn resolve_agent_launch(
    agent_model: Option<&str>,
    agent_effort: Option<&str>,
    defaults: &AutomationDefaults,
) -> ResolvedLaunch {
    let model = agent_model
        .or(defaults.model.as_deref())
        .map(str::to_owned);
    let effort = agent_effort
        .or(defaults.effort.as_deref())
        .map(str::to_owned);
    // Fallback is meaningful only when it differs from the resolved primary.
    let fallback = match &model {
        Some(m) if *m == defaults.fallback_model => None,
        _ => Some(defaults.fallback_model.clone()),
    };
    ResolvedLaunch {
        model,
        effort,
        fallback,
    }
}

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

/// R11 run deadline: an **agent** run still `Running` this long after it
/// started is closed failed([`ERR_TIMED_OUT`]) by the sweep. Unlike the ack
/// timeout (which fires only on a *never-linked* run), the deadline close
/// **keeps** the linked `pane_id`, so R7's alive-probe treats a genuinely
/// stuck-but-alive agent as still in flight (no fan-out); a dead pane's row
/// simply stays terminal.
pub const RUN_DEADLINE_MS: u64 = 30 * 60 * 1000;

/// Error string for the run linked to a pane that exited before any Stop
/// closed it (U7 pane-exit tap). A run already closed (Stop→succeeded or the
/// deadline→timed-out) is left untouched — this only catches a pane that died
/// mid-run.
pub const ERR_PANE_EXIT: &str = "pane exited";

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

/// U4b seam: capture an agent run's final assistant turn for its
/// [`RunRow::output`] (R8). Given the automation's `cwd` and the run's dispatch
/// time (`started_at`, epoch ms), resolve the run's transcript and return the
/// **scrubbed + control-sanitized** final message — or `None` when it can't be
/// resolved unambiguously (never the wrong session's content — a confidentiality
/// guard, see `transcript::sole_transcript_since`). Invoked from the pane-keyed
/// close, off the store lock (KTD-B): it reads a transcript from disk, so it must
/// never run under the lock. Default: always `None` (no capture); `lib.rs` wires
/// the transcript reader.
pub type OutputCapturer = Arc<dyn Fn(&str, u64) -> Option<String> + Send + Sync>;

/// U5 event payload (automations-workspace-and-model): an **agent** run reached
/// a terminal status. Serde camelCase (`runId`/`automationId`/`status`), the
/// same wire convention as the rest of the automations contract.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunClosedEvent {
    pub run_id: String,
    pub automation_id: String,
    pub status: RunStatus,
}

/// U5 seam: emit `automation://run-closed` after an **agent** run closes, so the
/// frontend tab lifecycle (U8) can auto-close a `succeeded` run's tab or keep a
/// `failed` one. Called only for agent-run closes, always **after** the store
/// mutation returns (lock released, KTD-B). Default no-op; `lib.rs` wires the
/// Tauri emit, tests wire a collector.
pub type RunClosedEmitter = Arc<dyn Fn(&RunClosedEvent) + Send + Sync>;

/// How claimed runs leave the manager (KTD-C/E): the sweep claims + flushes
/// under the store lock, releases it, then calls exactly one of these.
/// Implementations must not call back into the manager *synchronously from
/// the dispatch call on paths that take the store lock and then block* — the
/// contract is simply that dispatch runs outside the store lock, so calling
/// `list()`/`close`-style APIs from other threads (U5's reaper) or even from
/// the dispatch itself is safe.
pub trait Dispatcher: Send + Sync {
    /// Start an agent run (U7: emit `automation://agent-run`). `launch` is the
    /// resolved model/effort/fallback (U4a) the pane should launch with.
    fn dispatch_agent(
        &self,
        automation: &Automation,
        run_id: &str,
        launch: &ResolvedLaunch,
    ) -> Result<(), String>;
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
    fn dispatch_agent(
        &self,
        _a: &Automation,
        _run_id: &str,
        _launch: &ResolvedLaunch,
    ) -> Result<(), String> {
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
    /// (Superseded by the durable Automations-workspace role marker in the
    /// frontend, U6/U7; retained on the wire for now.)
    origin_workspace_hint: String,
    /// Resolved launch flags (U4a — R11): the frontend appends `--model` /
    /// `--effort` / `--fallback-model` only when the matching field is set.
    model: Option<String>,
    effort: Option<String>,
    fallback_model: Option<String>,
}

impl Dispatcher for AgentDispatcher {
    fn dispatch_agent(
        &self,
        a: &Automation,
        run_id: &str,
        launch: &ResolvedLaunch,
    ) -> Result<(), String> {
        let prompt = match &a.mode {
            Mode::Agent { prompt, .. } => prompt.clone(),
            _ => return Err("BUG: dispatch_agent on non-agent automation".into()),
        };
        let event = AgentRunEvent {
            run_id: run_id.to_string(),
            name: a.name.clone(),
            prompt,
            cwd: a.cwd.clone(),
            origin_workspace_hint: a.origin.workspace_id.clone(),
            model: launch.model.clone(),
            effort: launch.effort.clone(),
            fallback_model: launch.fallback.clone(),
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
        /// Optional pinned launch model + reasoning effort (U2/U1 of the
        /// automations-workspace-and-model plan, R9/R10/R14). `None` ⇒ fall
        /// through to the shared default / Claude's own default at dispatch.
        model: Option<String>,
        effort: Option<String>,
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
    /// U4a: shared config for agent-launch resolution (model/effort/fallback,
    /// R11/R12/R15). Read off the store lock (KTD8). Defaults to an ephemeral
    /// store with `Config::default()` so tests need no config wiring; `lib.rs`
    /// injects the real file-backed `ConfigStore` via [`AutomationManager::set_config`].
    config: Mutex<Arc<ConfigStore>>,
    /// U4b: transcript-reading capture of an agent run's final message (see
    /// [`OutputCapturer`]). Default no-op; `lib.rs` injects the real reader.
    output_capturer: Mutex<OutputCapturer>,
    /// U5: emit `automation://run-closed` on agent-run close (see
    /// [`RunClosedEmitter`]). Default no-op; `lib.rs` injects the Tauri emit.
    emit_run_closed: Mutex<RunClosedEmitter>,
    /// R5 gate: while false, due **agent** automations are deferred —
    /// neither claimed nor skipped (see [`AutomationManager::sweep_once`]).
    frontend_ready: AtomicBool,
    /// R22 recursion registry: pane ids of automation-spawned agent panes.
    /// Populated at spawn ([`AutomationManager::register_automation_pane`]),
    /// cleared on pane exit ([`AutomationManager::on_pane_exit`]). U9's CLI
    /// recursion gate rejects create/run from a pane in this set. In-memory
    /// only (pane ids are per-launch); not persisted.
    automation_panes: Mutex<HashSet<u64>>,
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
            config: Mutex::new(Arc::new(ConfigStore::ephemeral(
                crate::config::Config::default(),
            ))),
            output_capturer: Mutex::new(Arc::new(|_cwd: &str, _since: u64| None)),
            emit_run_closed: Mutex::new(Arc::new(|_ev: &RunClosedEvent| {})),
            frontend_ready: AtomicBool::new(false),
            automation_panes: Mutex::new(HashSet::new()),
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
    ///
    /// Also **clears every persisted `pane_id`** (U7 launch-stability): pane
    /// ids reset each launch (they start at 1 and are never reused within a
    /// run), so a `pane_id` loaded from disk can never refer to a live pane
    /// now. Left in place, a stale terminal row's id could resolve to an
    /// unrelated new pane and wedge R7's alive-probe (the automation would
    /// look permanently in-flight and never fire). One clear on load is the
    /// whole fix — within a launch, ids are unique, so no live row ever needs
    /// its id cleared.
    fn recover_interrupted(&self) {
        let now = (self.clock)();
        let _ = self.store.mutate(|map| {
            for a in map.values_mut() {
                for r in a.runs.iter_mut() {
                    r.pane_id = None;
                }
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

    /// U4a: inject the real file-backed [`ConfigStore`] so agent-launch
    /// resolution reads the user's shared automation defaults (R12/R15).
    pub fn set_config(&self, config: Arc<ConfigStore>) {
        *self.config.lock().unwrap() = config;
    }

    /// U4b: inject the transcript-reading output capturer (see [`OutputCapturer`]).
    pub fn set_output_capturer(&self, c: OutputCapturer) {
        *self.output_capturer.lock().unwrap() = c;
    }

    /// U5: inject the `automation://run-closed` emitter (see [`RunClosedEmitter`]).
    pub fn set_run_closed_emitter(&self, e: RunClosedEmitter) {
        *self.emit_run_closed.lock().unwrap() = e;
    }

    /// U5: fire `automation://run-closed` for an agent run that reached
    /// `status`. Always called after the store mutation, outside the lock.
    fn emit_run_closed(&self, automation_id: &str, run_id: &str, status: RunStatus) {
        let ev = RunClosedEvent {
            run_id: run_id.to_string(),
            automation_id: automation_id.to_string(),
            status,
        };
        (self.emit_run_closed.lock().unwrap())(&ev);
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
        let mut minted = false;
        for _ in 0..8 {
            if self.store.get(&id).is_none() {
                minted = true;
                break;
            }
            id = mint_id();
        }
        if !minted {
            // 8 collisions in a 36^10 space is astronomically improbable, but
            // proceeding would `put_script` onto (and only later reject) a
            // colliding id — clobbering the victim's stored script. Bail
            // before any script write.
            return Err("could not mint a unique automation id; retry".into());
        }

        let mode = match spec.mode {
            CreateMode::Agent {
                prompt,
                model,
                effort,
            } => Mode::Agent {
                prompt,
                model,
                effort,
            },
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
        // store::delete is flush-tolerant (KTD-B: the in-memory removal stays
        // authoritative on a flush failure, recorded in health), so the R23
        // teardown below always runs when the automation existed — a flush
        // failure never orphans the in-flight script group it kills.
        let Some(mut automation) = self.store.delete(id) else {
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

    /// U7 Stop-event closure (KTD-F): find the automation run linked to a pane
    /// and close it as **succeeded**. Called by the hook dispatch on the first
    /// Stop. Idempotent: a no-op if the pane is not linked to any *running*
    /// automation run (a second Stop, or a run already closed by the deadline
    /// / pane exit). The agent interaction lives in the pane's scrollback, so
    /// the row carries no output/exit code.
    pub fn close_run_by_pane(&self, pane_id: u64) -> Result<(), String> {
        self.close_run_by_pane_with(pane_id, RunOutcome::Succeeded { output: None })
    }

    /// U7 pane-exit closure: close the run linked to a pane as
    /// **failed([`ERR_PANE_EXIT`])** — the pane died before any Stop closed
    /// it. Idempotent like [`AutomationManager::close_run_by_pane`]: a run
    /// already closed (Stop→succeeded, or the deadline→timed-out, which keeps
    /// its `pane_id`) is left untouched. Called from the `stream::spawn_pane`
    /// on-exit tap (outside any PTY lock — the store→PTY lock order, KTD-B).
    pub fn close_run_by_pane_failed(&self, pane_id: u64) -> Result<(), String> {
        self.close_run_by_pane_with(
            pane_id,
            RunOutcome::Failed {
                error: ERR_PANE_EXIT.into(),
                exit_code: None,
                output: None,
            },
        )
    }

    /// Shared body of the pane-keyed closes: find the *running* run linked to
    /// `pane_id` and close it with `outcome`. Idempotent — returns `Ok(())`
    /// whether it closed a row, found no running run, or hit an already-closed
    /// one (all benign for the Stop / pane-exit callers).
    fn close_run_by_pane_with(&self, pane_id: u64, mut outcome: RunOutcome) -> Result<(), String> {
        let found = {
            let map = self.store.snapshot();
            let mut found: Option<(String, String, String, Option<u64>)> = None;
            for (auto_id, a) in map.iter() {
                for run in &a.runs {
                    if run.pane_id == Some(pane_id) && run.status == RunStatus::Running {
                        // Also capture cwd + dispatch time for the U4b transcript
                        // read below.
                        found =
                            Some((auto_id.clone(), run.id.clone(), a.cwd.clone(), run.started_at));
                        break;
                    }
                }
                if found.is_some() {
                    break;
                }
            }
            found
        };
        let Some((automation_id, run_id, cwd, started_at)) = found else {
            // No running run for this pane: idempotent no-op (second Stop, or a
            // run the deadline/other path already closed).
            return Ok(());
        };
        // U4b (R8): capture the agent's final assistant turn into the run's
        // output, unless the caller already supplied output. The capturer reads
        // a transcript from disk (off the store lock, KTD-B) and abstains on an
        // ambiguous cwd — so this never blocks the close and never records the
        // wrong session's content. `close_run` tail-caps the text (R8).
        if outcome_output(&outcome).is_none() {
            if let Some(started) = started_at {
                let capturer = Arc::clone(&self.output_capturer.lock().unwrap());
                if let Some(text) = capturer(&cwd, started) {
                    outcome = with_output(outcome, text);
                }
            }
        }
        // U5: emit `automation://run-closed` when this close actually closed the
        // row (idempotent no-op on a second Stop / already-closed run).
        let status = outcome_status(&outcome);
        if self.close_run(&automation_id, &run_id, outcome) == model::CloseResult::Closed {
            self.emit_run_closed(&automation_id, &run_id, status);
        }
        Ok(())
    }

    /// U7 pane-exit tap: the pane linked to an agent run has exited. Close its
    /// still-running row failed (a pane that died before Stop), and clear the
    /// R22 recursion-registry entry (load-bearing: it deliberately outlives a
    /// delete so create→delete can't un-gate a still-live pane — only the
    /// pane's actual exit clears it). Both touch locks other than the PTY
    /// registry's, and this runs from the read thread holding no PTY lock, so
    /// the store→PTY lock order (KTD-B) is preserved.
    pub fn on_pane_exit(&self, pane_id: u64) {
        self.unregister_automation_pane(pane_id);
        let _ = self.close_run_by_pane_failed(pane_id);
    }

    /// U7 link-at-spawn (R10): atomically set the linked `pane_id` on a
    /// `Running` run row and flush, so the ack-timeout, the R7 alive-probe,
    /// and the Stop/pane-exit closes can all find the pane immediately (no
    /// window where a live pane has `pane_id = None`). Emits
    /// `automation://changed` after the lock (KTD-B) so the dashboard picks up
    /// the jump target (U10).
    ///
    /// **Fails a late spawn** (R10 fan-out guard): if the run id is unknown
    /// (evicted / deleted) or already terminal (the ack timeout or a delete
    /// beat the spawn), it returns `Err` and the caller aborts the spawn
    /// rather than leaving a live pane orphaned from any run — which would let
    /// the next occurrence start a second concurrent pane.
    pub fn set_run_pane(&self, run_id: &str, pane_id: u64) -> Result<(), String> {
        // Ok(auto_id) = linked; Err(()) = found but terminal (late); the outer
        // Option is None when no row has that id.
        let outcome: Option<Result<String, ()>> = flush_tolerant(
            self.store.mutate(|map| {
                for a in map.values_mut() {
                    if let Some(row) = a.runs.iter_mut().find(|r| r.id == run_id) {
                        if row.status != RunStatus::Running {
                            return Some(Err(()));
                        }
                        row.pane_id = Some(pane_id);
                        return Some(Ok(a.id.clone()));
                    }
                }
                None
            }),
            // Flush failed but the mutation applied (KTD-B): re-derive from the
            // authoritative map — a row now carrying this pane_id linked.
            || {
                for a in self.store.snapshot().into_values() {
                    if let Some(row) = a.runs.iter().find(|r| r.id == run_id) {
                        return Some(if row.pane_id == Some(pane_id) {
                            Ok(a.id.clone())
                        } else {
                            Err(())
                        });
                    }
                }
                None
            },
        );
        match outcome {
            Some(Ok(automation_id)) => {
                (self.emit_changed)(&automation_id); // after the lock (KTD-B)
                Ok(())
            }
            Some(Err(())) => Err(format!(
                "automation run {run_id} is already closed; refusing to link pane {pane_id}"
            )),
            None => Err(format!("no automation run {run_id} to link pane {pane_id}")),
        }
    }

    /// R22: mark a pane as automation-spawned (recursion registry). Called by
    /// `stream::spawn_pane` right after [`AutomationManager::set_run_pane`].
    pub fn register_automation_pane(&self, pane_id: u64) {
        self.automation_panes.lock().unwrap().insert(pane_id);
    }

    /// R22: clear a pane's automation-spawned mark (on pane exit).
    pub fn unregister_automation_pane(&self, pane_id: u64) {
        self.automation_panes.lock().unwrap().remove(&pane_id);
    }

    /// R22 recursion gate (consumed by U9's CLI): whether a pane was spawned
    /// by an automation — such panes may not create or run automations.
    pub fn is_automation_pane(&self, pane_id: u64) -> bool {
        self.automation_panes.lock().unwrap().contains(&pane_id)
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
        // U5: agent runs the sweep itself closed failed (ack-timeout / deadline),
        // to emit `automation://run-closed` in phase 3 (outside the lock).
        let mut closed_agent_runs: Vec<(String, String)> = Vec::new();
        let flush_result = self.store.mutate(|map| {
            for a in map.values_mut() {
                let mut touched = false;

                // R10 ack-timeout: agent rows never linked to a pane within
                // the window close failed (a dropped agent-run event).
                for run_id in ack_timed_out_agent_runs(a, now_ms) {
                    a.close(&run_id, failed(ERR_SPAWN_ACK), now_ms);
                    closed_agent_runs.push((a.id.clone(), run_id));
                    touched = true;
                }

                // R11 run deadline: an agent run linked to a pane but still
                // Running past the 30-min deadline closes failed(timed out).
                // `close` leaves `pane_id` in place, so if the pane is still
                // alive the R7 alive-probe keeps this occurrence in flight —
                // a genuinely stuck agent skips the next occurrence instead of
                // fanning out a second pane; once the pane exits, the probe
                // reads dead and the schedule resumes.
                for run_id in deadline_expired_agent_runs(a, now_ms) {
                    a.close(&run_id, failed(ERR_TIMED_OUT), now_ms);
                    closed_agent_runs.push((a.id.clone(), run_id));
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

        // R2 persist-before-dispatch: if the phase-1 claim flush FAILED (e.g.
        // ENOSPC), the Running rows live only in memory (KTD-B keeps them
        // authoritative), not on disk. Dispatching them now would break R2 —
        // a crash before the next successful flush loses the claim on disk
        // while the side effect already ran, re-firing the occurrence on
        // restart. So defer dispatch this tick: the in-memory Running rows
        // block re-claims (skip-if-running) until a later flush persists them
        // or startup recovery closes them interrupted on the next launch.
        // Health already records the flush error for the dashboard (R6).
        if flush_result.is_err() {
            if !to_dispatch.is_empty() {
                eprintln!(
                    "[fly-automations] sweep claim flush failed; deferring dispatch of {} \
                     run(s) to preserve persist-before-dispatch (R2)",
                    to_dispatch.len()
                );
            }
            to_dispatch.clear();
        }

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
                        // R3 refinement: only recompute if a concurrent pause
                        // or disable hasn't nulled next_run_at during the
                        // dispatch window — the recompute must never resurrect
                        // a schedule the user just paused (R23).
                        if a.enabled && a.next_run_at.is_some() {
                            a.rollback_recompute(recomputed);
                        }
                    }
                });
            }
        }

        // Phase 3: emit — after all store work, no lock held (KTD-B).
        for id in changed {
            (self.emit_changed)(&id);
        }
        // U5: agent runs the sweep closed failed (ack-timeout / deadline) emit
        // run-closed so the frontend tab lifecycle can react (keeps a failed
        // tab, U8) — also outside the lock.
        for (automation_id, run_id) in closed_agent_runs {
            self.emit_run_closed(&automation_id, &run_id, RunStatus::Failed);
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
        match &automation.mode {
            Mode::Agent { model, effort, .. } => {
                // U4a: resolve launch off the store lock (KTD8 — config is a
                // separate RwLock read), record what we launched with (R13),
                // then hand the resolved flags to the dispatcher so they ride
                // the `automation://agent-run` event (R11).
                let defaults = self.config.lock().unwrap().get().automation_defaults;
                let launch =
                    resolve_agent_launch(model.as_deref(), effort.as_deref(), &defaults);
                if launch.model.is_some() || launch.effort.is_some() {
                    self.set_run_launch(run_id, &launch);
                }
                dispatcher.dispatch_agent(automation, run_id, &launch)
            }
            Mode::Script { .. } => dispatcher.dispatch_script(automation, run_id),
        }
    }

    /// U4a (R13): stamp the resolved launch model/effort onto a still-`Running`
    /// agent run row, so the dashboard records what the run launched with.
    /// Quiet — emits no `automation://changed` (every caller is a dispatch path
    /// that emits afterward: sweep phase 3 / `manual_run`). Best-effort: an
    /// unknown or already-terminal row (the ack-timeout or a delete beat the
    /// dispatch) is a no-op.
    fn set_run_launch(&self, run_id: &str, launch: &ResolvedLaunch) {
        let _ = self.store.mutate(|map| {
            for a in map.values_mut() {
                if let Some(row) = a.runs.iter_mut().find(|r| r.id == run_id) {
                    if row.status == RunStatus::Running {
                        row.model = launch.model.clone();
                        row.effort = launch.effort.clone();
                    }
                    return;
                }
            }
        });
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

/// Agent rows still `Running` past the R11 [`RUN_DEADLINE_MS`] deadline. These
/// have a linked pane (an unlinked run hits the ack timeout first); the
/// deadline close keeps that `pane_id` so the alive-probe can still see a
/// stuck-but-alive agent (R7 widening).
fn deadline_expired_agent_runs(a: &Automation, now_ms: u64) -> Vec<String> {
    a.runs
        .iter()
        .filter(|r| {
            r.status == RunStatus::Running
                && r.mode == RunMode::Agent
                && r.started_at
                    .is_some_and(|t| t.saturating_add(RUN_DEADLINE_MS) <= now_ms)
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

/// The output slot of a terminal outcome, whichever variant (U4b): so the
/// pane-keyed close only captures a transcript when the caller left it empty.
fn outcome_output(o: &RunOutcome) -> Option<&String> {
    match o {
        RunOutcome::Succeeded { output } => output.as_ref(),
        RunOutcome::Failed { output, .. } => output.as_ref(),
    }
}

/// The terminal [`RunStatus`] a [`RunOutcome`] closes to (U5 run-closed event).
fn outcome_status(o: &RunOutcome) -> RunStatus {
    match o {
        RunOutcome::Succeeded { .. } => RunStatus::Succeeded,
        RunOutcome::Failed { .. } => RunStatus::Failed,
    }
}

/// Set the output on a terminal outcome, preserving its variant + fields (U4b).
fn with_output(o: RunOutcome, text: String) -> RunOutcome {
    match o {
        RunOutcome::Succeeded { .. } => RunOutcome::Succeeded {
            output: Some(text),
        },
        RunOutcome::Failed {
            error, exit_code, ..
        } => RunOutcome::Failed {
            error,
            exit_code,
            output: Some(text),
        },
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

/// The read-only automations dashboard payload (U10, R25/R6). `automations` is
/// the raw list in the model's serde-camelCase shape — the same shape that
/// already crosses the store file and the socket (see [`model`]); the frontend
/// view-model (`src/lib/automations.ts`) does the sort + humanization, matching
/// the CLI's `load_store_at` ordering. The three health fields flatten
/// [`store::StoreHealth`] for the R6 warning row: `degraded` is the at-a-glance
/// bit, `corrupt_bak` names where corrupt bytes were preserved (so the warning
/// can point the user at them), and `flush_error` carries a failing-flush
/// detail. Sticky across successful flushes for the app session (see
/// [`store::StoreHealth`]).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomationsDashboard {
    pub automations: Vec<Automation>,
    pub degraded: bool,
    pub corrupt_bak: Option<String>,
    pub flush_error: Option<String>,
}

/// List every automation plus store health for the dashboard panel (U10). A
/// pure read over the manager (no mutation, no lock held past the snapshot), so
/// it is cheap enough to call on dashboard open and refetch on every
/// `automation://changed`.
#[tauri::command]
pub fn list_automations(
    manager: tauri::State<'_, Arc<AutomationManager>>,
) -> AutomationsDashboard {
    let health = manager.store_health();
    AutomationsDashboard {
        automations: manager.list(),
        degraded: !health.is_ok(),
        corrupt_bak: health.corrupt_bak.map(|p| p.display().to_string()),
        flush_error: health.flush_error,
    }
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
        /// U4a: the resolved launch handed to each `dispatch_agent`.
        agent_launches: Mutex<Vec<ResolvedLaunch>>,
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
        fn dispatch_agent(
            &self,
            a: &Automation,
            run_id: &str,
            launch: &ResolvedLaunch,
        ) -> Result<(), String> {
            self.agent_launches.lock().unwrap().push(launch.clone());
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
        /// U5: collected `automation://run-closed` payloads (run id + status).
        run_closed: Arc<Mutex<Vec<(String, RunStatus)>>>,
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
        let run_closed: Arc<Mutex<Vec<(String, RunStatus)>>> = Arc::new(Mutex::new(Vec::new()));
        let rc = Arc::clone(&run_closed);
        mgr.set_run_closed_emitter(Arc::new(move |ev: &RunClosedEvent| {
            rc.lock().unwrap().push((ev.run_id.clone(), ev.status));
        }));
        Harness {
            mgr,
            clock,
            events,
            dispatcher,
            run_closed,
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
                model: None,
                effort: None,
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

    // U4a resolver (R11/R12/R15): resolution order (automation → shared default
    // → Claude default) and the fallback rule (omitted iff it equals the
    // resolved primary).
    #[test]
    fn resolve_agent_launch_resolution_order_and_fallback_u4a() {
        let defaults = AutomationDefaults {
            model: Some("sonnet".into()),
            effort: Some("medium".into()),
            fallback_model: "sonnet".into(),
        };
        // Automation values win over the shared default; primary opus ≠ sonnet.
        let r = resolve_agent_launch(Some("opus"), Some("high"), &defaults);
        assert_eq!(r.model.as_deref(), Some("opus"));
        assert_eq!(r.effort.as_deref(), Some("high"));
        assert_eq!(r.fallback.as_deref(), Some("sonnet"), "differs ⇒ present");

        // Absent automation values fall through to the shared default; here the
        // resolved primary equals the fallback model, so the fallback is omitted.
        let r = resolve_agent_launch(None, None, &defaults);
        assert_eq!(r.model.as_deref(), Some("sonnet"));
        assert_eq!(r.effort.as_deref(), Some("medium"));
        assert_eq!(r.fallback, None, "primary == fallback_model ⇒ omitted");

        // Both absent and no shared default ⇒ None/None (Claude default), with
        // the fallback still present (a None primary never equals the fallback).
        let r = resolve_agent_launch(None, None, &AutomationDefaults::default());
        assert_eq!(r.model, None);
        assert_eq!(r.effort, None);
        assert_eq!(r.fallback.as_deref(), Some("sonnet"));
    }

    // U4a (R13): a dispatched agent run records the resolved model/effort on its
    // RunRow and hands the resolved launch (with fallback) to the dispatcher.
    // The automation pins both, so they win over the empty shared default.
    #[test]
    fn agent_dispatch_records_resolved_model_effort_and_passes_launch_u4a() {
        let h = harness();
        let spec = CreateSpec {
            mode: CreateMode::Agent {
                prompt: "audit".into(),
                model: Some("opus".into()),
                effort: Some("high".into()),
            },
            ..script_spec("pinned")
        };
        let id = h.mgr.create(spec).unwrap().automation.id;
        h.set_now(T0 + FIVE_MIN);
        h.mgr.set_frontend_ready();
        h.sweep();

        let runs = h.runs(&id);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, RunStatus::Running);
        assert_eq!(runs[0].model.as_deref(), Some("opus"), "R13: recorded model");
        assert_eq!(runs[0].effort.as_deref(), Some("high"), "R13: recorded effort");

        let launches = h.dispatcher.agent_launches.lock().unwrap();
        assert_eq!(launches.len(), 1, "one agent dispatch");
        assert_eq!(launches[0].model.as_deref(), Some("opus"));
        assert_eq!(launches[0].effort.as_deref(), Some("high"));
        assert_eq!(
            launches[0].fallback.as_deref(),
            Some("sonnet"),
            "default fallback, differs from the pinned primary"
        );
    }

    // U4b (R8): a pane-keyed agent close captures the run's final message into
    // RunRow.output via the injected capturer. The default (no capturer) path is
    // covered by the other close tests, which leave output None.
    #[test]
    fn close_run_by_pane_captures_agent_output_via_the_seam_u4b() {
        let h = harness();
        let id = h.mgr.create(agent_spec("cap")).unwrap().automation.id;
        h.set_now(T0 + FIVE_MIN);
        h.mgr.set_frontend_ready();
        h.sweep();
        let run_id = h.runs(&id)[0].id.clone();
        h.mgr.set_run_pane(&run_id, 42).unwrap();

        // Inject a capturer and close via the pane (Stop → succeeded).
        h.mgr
            .set_output_capturer(Arc::new(|_cwd: &str, _since: u64| {
                Some("final summary".to_string())
            }));
        h.mgr.close_run_by_pane(42).unwrap();

        let row = &h.runs(&id)[0];
        assert_eq!(row.status, RunStatus::Succeeded);
        assert_eq!(
            row.output.as_deref(),
            Some("final summary"),
            "capturer output recorded on the run row"
        );
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

    // ---- U7 agent lifecycle: link-at-spawn, Stop/pane-exit/deadline close --

    /// Claim one agent run and return its (automation id, run id), ready to
    /// link a pane to. Sets frontend-ready so the agent claim proceeds (R5).
    fn claim_agent_run(h: &Harness, name: &str) -> (String, String) {
        h.mgr.set_frontend_ready();
        let id = create_due(h, agent_spec(name));
        h.sweep();
        let run_id = h.runs(&id)[0].id.clone();
        assert_eq!(h.runs(&id)[0].status, RunStatus::Running);
        assert_eq!(h.runs(&id)[0].pane_id, None, "not linked until spawn acks");
        (id, run_id)
    }

    // R10 link-at-spawn: set_run_pane stamps pane_id on the Running row,
    // flushes (a raw reload agrees), and emits automation://changed so the
    // dashboard picks up the jump target (U10).
    #[test]
    fn set_run_pane_links_pane_flushes_and_emits_r10() {
        let h = harness();
        let (id, run_id) = claim_agent_run(&h, "linker");
        h.events.lock().unwrap().clear();

        h.mgr.set_run_pane(&run_id, 42).expect("links a running run");

        assert_eq!(h.runs(&id)[0].pane_id, Some(42));
        assert_eq!(*h.events.lock().unwrap(), vec![id.clone()], "emitted once");
        assert_eq!(
            store_in(&h.dir).get(&id).unwrap().runs[0].pane_id,
            Some(42),
            "flushed to disk"
        );
    }

    // R10 fan-out guard: a late spawn fails set_run_pane so the caller aborts
    // (never leaves a live pane orphaned from a run). Late = the run already
    // closed (ack timeout / delete beat the spawn) or the id is unknown.
    #[test]
    fn set_run_pane_rejects_a_late_or_unknown_spawn_r10() {
        let h = harness();
        let (id, run_id) = claim_agent_run(&h, "late");
        // The ack timeout closes it before the spawn acks.
        h.mgr
            .close_run(&id, &run_id, failed(ERR_SPAWN_ACK));

        let err = h.mgr.set_run_pane(&run_id, 7).unwrap_err();
        assert!(err.contains("already closed"), "{err}");
        assert_eq!(h.runs(&id)[0].pane_id, None, "no link onto a terminal row");

        let err = h.mgr.set_run_pane("ghost", 7).unwrap_err();
        assert!(err.contains("no automation run"), "{err}");
    }

    // A linked agent run is NOT ack-timed-out — only a never-linked one is
    // (the ack window guards a dropped agent-run event, not a live pane). Past
    // the ack window a linked run stays Running until the deadline governs it.
    #[test]
    fn linked_agent_run_survives_the_ack_window_r10() {
        let h = harness();
        let (id, run_id) = claim_agent_run(&h, "linked");
        h.mgr.set_run_pane(&run_id, 9).unwrap();

        h.set_now(T0 + FIVE_MIN + AGENT_ACK_TIMEOUT_MS + 1);
        h.sweep();
        assert_eq!(
            h.runs(&id)[0].status,
            RunStatus::Running,
            "linked run is not ack-timed-out"
        );
    }

    // KTD-F Stop close on a linked pane: the first Stop closes the run
    // succeeded; a second Stop (or any later pane-keyed close) is a no-op.
    #[test]
    fn close_run_by_pane_closes_succeeded_then_is_idempotent_ktd_f() {
        let h = harness();
        let (id, run_id) = claim_agent_run(&h, "stopper");
        h.mgr.set_run_pane(&run_id, 11).unwrap();

        h.mgr.close_run_by_pane(11).expect("first Stop closes");
        assert_eq!(h.runs(&id)[0].status, RunStatus::Succeeded);

        // Second Stop: no running run for the pane → idempotent no-op.
        h.mgr.close_run_by_pane(11).expect("second Stop is a no-op");
        assert_eq!(h.runs(&id)[0].status, RunStatus::Succeeded, "still succeeded");
        // A pane exit after a clean Stop leaves the succeeded row untouched.
        h.mgr.on_pane_exit(11);
        assert_eq!(h.runs(&id)[0].status, RunStatus::Succeeded);
    }

    // R11 pane-exit close: a pane that dies before any Stop closes its run
    // failed(pane exited), and the recursion-registry entry clears on exit.
    #[test]
    fn on_pane_exit_closes_running_run_failed_and_clears_registry_r11() {
        let h = harness();
        let (id, run_id) = claim_agent_run(&h, "crasher");
        h.mgr.set_run_pane(&run_id, 13).unwrap();
        h.mgr.register_automation_pane(13);
        assert!(h.mgr.is_automation_pane(13));

        h.mgr.on_pane_exit(13);

        assert_eq!(h.runs(&id)[0].status, RunStatus::Failed);
        assert_eq!(h.runs(&id)[0].error.as_deref(), Some(ERR_PANE_EXIT));
        assert!(!h.mgr.is_automation_pane(13), "registry entry cleared on exit");
    }

    // R11 deadline: an agent run still Running past the 30-min deadline closes
    // failed(timed out) with pane_id RETAINED. While the pane stays alive the
    // R7 alive-probe keeps the occurrence in flight (the next one skips — no
    // fan-out); once the pane dies the schedule resumes claiming.
    #[test]
    fn agent_run_past_deadline_times_out_keeps_pane_and_blocks_fanout_r11() {
        let h = harness();
        let (id, run_id) = claim_agent_run(&h, "stuck");
        h.mgr.set_run_pane(&run_id, 42).unwrap();
        // The pane is (and stays) alive for now.
        h.mgr
            .set_agent_pane_alive(Arc::new(|row: &RunRow| row.pane_id == Some(42)));

        // 30 min later the deadline fires; the same tick finds the next
        // occurrence due but the stuck-alive pane keeps it in flight → Skipped,
        // never a second dispatch.
        h.set_now(T0 + FIVE_MIN + RUN_DEADLINE_MS);
        h.sweep();

        let runs = h.runs(&id);
        assert_eq!(runs[0].status, RunStatus::Failed);
        assert_eq!(runs[0].error.as_deref(), Some(ERR_TIMED_OUT));
        assert_eq!(runs[0].pane_id, Some(42), "pane_id retained past the deadline");
        assert_eq!(runs[1].status, RunStatus::Skipped, "no fan-out while alive");
        assert_eq!(runs[1].error.as_deref(), Some(SKIP_IN_FLIGHT));
        assert_eq!(h.dispatcher.count(), 1, "exactly one dispatch — no second pane");

        // The pane finally dies: the alive-probe reads dead, so the next
        // occurrence claims again.
        h.mgr.set_agent_pane_alive(Arc::new(|_row: &RunRow| false));
        let next = h.next_run_at(&id).expect("re-armed after the skip");
        h.set_now(next);
        h.sweep();
        assert_eq!(h.dispatcher.count(), 2, "schedule resumes once the pane is gone");
    }

    // U5 (automations-workspace-and-model): an agent-run close emits
    // `automation://run-closed` with the terminal status — succeeded on a Stop
    // (pane close), failed on a deadline timeout — while a script close never
    // does. All emissions happen in phase 3 / after the close, outside the lock.
    #[test]
    fn run_closed_event_fires_for_agent_closes_only_u5() {
        // Agent Stop → run-closed succeeded.
        let h = harness();
        let (_agent, run1) = claim_agent_run(&h, "a");
        h.mgr.set_run_pane(&run1, 10).unwrap();
        h.mgr.close_run_by_pane(10).unwrap();
        assert_eq!(
            *h.run_closed.lock().unwrap(),
            vec![(run1, RunStatus::Succeeded)]
        );

        // Deadline timeout on a linked agent run → run-closed failed (the sweep).
        let h2 = harness();
        let (_id2, run2) = claim_agent_run(&h2, "stuck");
        h2.mgr.set_run_pane(&run2, 20).unwrap();
        h2.set_now(T0 + FIVE_MIN + RUN_DEADLINE_MS);
        h2.sweep();
        assert!(
            h2.run_closed
                .lock()
                .unwrap()
                .iter()
                .any(|(r, s)| *r == run2 && *s == RunStatus::Failed),
            "deadline close emits run-closed failed"
        );

        // A script close (via `close_run`) never emits run-closed.
        let h3 = harness();
        let script = h3.mgr.create(script_spec("s")).unwrap().automation.id;
        h3.set_now(T0 + FIVE_MIN);
        h3.sweep();
        let srun = h3.runs(&script).last().unwrap().id.clone();
        h3.mgr
            .close_run(&script, &srun, RunOutcome::Succeeded { output: None });
        assert!(
            h3.run_closed.lock().unwrap().is_empty(),
            "script close emits no run-closed"
        );
    }

    // R22 recursion registry: register / query / unregister round-trips.
    #[test]
    fn recursion_registry_tracks_automation_panes_r22() {
        let h = harness();
        assert!(!h.mgr.is_automation_pane(5));
        h.mgr.register_automation_pane(5);
        assert!(h.mgr.is_automation_pane(5));
        h.mgr.unregister_automation_pane(5);
        assert!(!h.mgr.is_automation_pane(5));
    }

    // U7 launch-stability: a persisted pane_id can never refer to a live pane
    // after a restart (ids reset each launch), so startup recovery clears
    // every row's pane_id — otherwise a stale terminal row could resolve to an
    // unrelated new pane and wedge R7's alive-probe.
    #[test]
    fn startup_recovery_clears_persisted_pane_ids_for_launch_stability() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = store_in(&dir);
            store
                .mutate(|map| {
                    let mut a = raw_automation("a1");
                    a.mode = Mode::Agent {
                        prompt: "x".into(),
                        model: None,
                        effort: None,
                    };
                    // A terminal row carrying a pane_id from a prior launch.
                    a.claim(Some(T0 + 2 * FIVE_MIN), T0 + FIVE_MIN, Trigger::Schedule, "r1")
                        .unwrap();
                    a.runs[0].pane_id = Some(3);
                    a.close(
                        "r1",
                        RunOutcome::Failed {
                            error: ERR_TIMED_OUT.into(),
                            exit_code: None,
                            output: None,
                        },
                        T0 + 2 * FIVE_MIN,
                    );
                    map.insert(a.id.clone(), a);
                })
                .unwrap();
        }
        let h = harness_in(dir); // construction runs recovery

        assert_eq!(
            h.runs("a1")[0].pane_id,
            None,
            "persisted pane_id cleared on load"
        );
        // And the clear was flushed.
        assert_eq!(store_in(&h.dir).get("a1").unwrap().runs[0].pane_id, None);
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
