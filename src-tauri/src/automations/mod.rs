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
//! - the script- and headless-killer seams are invoked outside the store
//!   lock (delete, shutdown, the headless sweep backstop — a kill sequence
//!   sleeps a grace, which must never happen under the lock);
//! - the sweep thread is joined ([`SweepHandle::stop_and_join`]) from
//!   `lifecycle::shutdown`, which holds no store lock.
//!
//! The injected *probes* ([`AutomationManager::set_agent_pane_alive`],
//! [`AutomationManager::set_script_capacity`],
//! [`AutomationManager::set_headless_check_alive`]) are the one sanctioned
//! exception: they are consulted inside the sweep's mutate closure so the
//! pre-claim checks (KTD-D) are atomic with the claim. They must therefore
//! be cheap, non-blocking reads and must **never** call back into this
//! manager or its store (the store mutex is not re-entrant).

pub mod alerts;
pub mod headless;
pub mod model;
pub mod redact;
pub mod schedule;
pub mod script;
pub mod store;
pub mod verdict;

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use tauri::Emitter;

use crate::config::{AutomationDefaults, ConfigStore};
use model::{
    Automation, ClaimError, Mode, Origin, RunMode, RunOutcome, RunRow, RunStatus, Trigger,
    Verdict, VerdictOutcome, ERR_DELETED, ERR_INTERRUPTED, ERR_TIMED_OUT,
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

/// Headless backstop slack (U2 of
/// `docs/plans/2026-07-11-003-feat-headless-monitor-checks-plan.md` — R7).
/// A headless monitor check is exempt from both pane-oriented sweep closes
/// (ack timeout and deadline — see the probes below): its runner thread
/// enforces [`RUN_DEADLINE_MS`] itself on a MONOTONIC clock and closes the
/// row (U3). The sweep keeps only a kill-then-close BACKSTOP for a dead
/// runner thread, firing at deadline + this slack so the runner's own close
/// wins every ordinary race. The consumer is [`AutomationManager::sweep_once`]'s
/// phase 2c (U4) — the [`HeadlessKiller`] seam plus the in-flight registry's
/// monotonic gate ([`HeadlessDeadlineGate`]: the entry's spawn `Instant`
/// must also have lapsed, or the entry be gone, before the kill — epoch age
/// alone can lapse across a laptop suspend while the check is healthy);
/// [`headless_deadline_expired_runs`] is the epoch leg this slack bounds.
pub const HEADLESS_DEADLINE_SLACK_MS: u64 = 60_000;

/// Error string for the run linked to a pane that exited before any Stop
/// closed it (U7 pane-exit tap). A run already closed (Stop→succeeded or the
/// deadline→timed-out) is left untouched — this only catches a pane that died
/// mid-run.
pub const ERR_PANE_EXIT: &str = "pane exited";

/// Error string for a headless check row closed by the sweep's backstop
/// (headless-monitor-checks U4 — R7): the runner thread died without
/// closing its row, so the sweep killed the (possibly still-live) child
/// through the [`HeadlessKiller`] seam and closed the row itself. Distinct
/// from the runner's own timeout reasons (`"timed out: killed at the run
/// deadline …"`, [`headless`]) so a backstop close is tellable from a
/// runner close at a glance.
pub const ERR_HEADLESS_BACKSTOP: &str =
    "timed out: check runner dead, killed by the sweep backstop";

/// Skip reason recorded for the R7 overlap skip.
pub const SKIP_IN_FLIGHT: &str = "run in flight";
/// Skip reason recorded for the U5 global script-capacity skip (KTD-D).
pub const SKIP_CAPACITY: &str = "capacity";

/// The Tauri event emitted after every mutation (payload: the automation id).
/// The dashboard (U10) refetches on it.
pub const AUTOMATION_CHANGED_EVENT: &str = "automation://changed";

/// The Tauri event emitted after a successful **monitor** create's store
/// flush (monitor-handoff U4 — the backend half of R13). Payload:
/// [`MonitorRegisteredEvent`] `{ paneId, automationId }`. The frontend (U6)
/// maps the registering pane → leaf → tab and closes it — the parent session
/// handed its watch off, so its tab is residue.
pub const MONITOR_REGISTERED_EVENT: &str = "automation://monitor-registered";

/// Monitor-handoff R12: the distinct refusal returned over the socket when a
/// monitor create cannot capture qualified pickup pointers from the
/// registering pane — no resume record, an implausible session id, a missing
/// transcript, or a metadata-only transcript (no real turn) all abstain into
/// this one string, and NOTHING is stored. Shared with the integration tests
/// so the contract can't drift silently.
pub const ERR_MONITOR_POINTERS: &str = "monitor create refused: could not capture pickup \
     pointers — this pane has no qualified session (a transcript with at least one real \
     turn); nothing was created";

/// Monitor-handoff R3 (fix(review) #2): the refusal [`AutomationManager::resume`]
/// returns for a retired monitor. Retirement is permanent — re-arming
/// `next_run_at` would set the sweep re-claiming (and being refused) every
/// tick forever.
const ERR_RESUME_RETIRED: &str =
    "monitor is retired — it delivered its verdict and cannot be resumed";

/// Monitor-handoff R3 (fix(review) #8): the refusal
/// [`AutomationManager::manual_run`] returns for a retired monitor — distinct
/// from the (resumable) disabled refusal.
const ERR_RUN_RETIRED: &str =
    "monitor is retired — it delivered its verdict and will never run again";

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

/// Headless-monitor-checks U4 seam (R5): kill an in-flight headless monitor
/// check by **run id** — the headless twin of [`ScriptKiller`], for checks
/// whose child fly owns directly (no pane, no script group of fly's making).
/// Called on delete (R5), shutdown (R5), and by the sweep's R7 backstop,
/// always **outside** the store lock (KTD-B — the kill sequence sleeps a
/// bounded grace). Default: no-op; `lib.rs` injects
/// [`headless::HeadlessRunner::kill_run`] (U5).
pub type HeadlessKiller = Arc<dyn Fn(&str) + Send + Sync>;

/// Headless-monitor-checks U4 seam widening R7's in-flight check — the
/// headless mirror of [`PaneAliveProbe`]: whether the **automation id** has
/// an in-flight registry entry whose child process is still alive (pid +
/// start-time pinned by the runner, never mere entry presence). A
/// terminal-but-alive check — a backstop/deadline kill that failed to stick —
/// must skip the next claim, not fan a second child out. Consulted inside
/// the sweep's mutate closure (module doc): cheap, non-blocking, never
/// re-enters the manager/store. Default: `false`; `lib.rs` wires
/// [`headless::HeadlessRunner::automation_check_alive`] (U5).
pub type HeadlessAliveProbe = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Headless-monitor-checks U4 seam (R7): the backstop's **monotonic gate**
/// for a run id — `true` when the in-flight registry entry's monotonic
/// deadline has also lapsed, or the entry is gone entirely (runner finished
/// or died and was evicted). The epoch leg
/// ([`headless_deadline_expired_runs`]) alone can lapse across a laptop
/// suspend while the check is healthy, so the backstop kills only when both
/// legs agree. Consulted off the store lock, right before the kill. Default:
/// `true` — an unwired manager has no registry at all, which IS the
/// entry-gone case (matching
/// [`headless::HeadlessRunner::monotonic_deadline_lapsed`], wired in U5).
pub type HeadlessDeadlineGate = Arc<dyn Fn(&str) -> bool + Send + Sync>;

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

/// Monitor-handoff U4 event payload (the backend half of R13): a monitor was
/// registered from `pane_id` and persisted as `automation_id`. Serde
/// camelCase (`paneId`/`automationId`), the file-wide wire convention.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorRegisteredEvent {
    pub pane_id: u64,
    pub automation_id: String,
}

/// Monitor-handoff U4 seam (R13): emit [`MONITOR_REGISTERED_EVENT`] after a
/// successful monitor create — invoked by the socket create arm
/// (`lib.rs::dispatch_automation_op`) strictly **after** [`AutomationManager::create`]
/// returned, i.e. after the store flush and off the store lock (KTD-B).
/// Default no-op; `lib.rs` wires the Tauri emit, tests wire a collector.
pub type MonitorRegisteredEmitter = Arc<dyn Fn(&MonitorRegisteredEvent) + Send + Sync>;

/// One run that a prior app instance's crash/restart left `Running`, closed
/// `failed("interrupted")` by startup recovery (interrupt-resilience U2/R2).
/// Carries just enough for the post-recovery step to (a) surface it through the
/// alert pipeline and (b) decide whether it is eligible for a one-shot retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterruptedRun {
    pub automation_id: String,
    pub run_id: String,
    /// The automation's name — the alert line's `[name]`.
    pub name: String,
    /// The automation's opt-in flag at recovery time (R1).
    pub retry_on_interrupt: bool,
    /// The automation was enabled at recovery — a paused/disabled one never
    /// retries (it only alerts).
    pub enabled: bool,
    /// The interrupted row itself was a [`Trigger::Retry`] run — the retry-once
    /// crash-loop guard: such a run alerts but is never retried again (R4).
    pub was_retry: bool,
}

impl InterruptedRun {
    /// Whether this interrupted run should be re-dispatched once (R1/R4): the
    /// automation opted in, is still enabled, and the interrupted run was not
    /// itself a retry.
    pub fn is_retry_eligible(&self) -> bool {
        self.retry_on_interrupt && self.enabled && !self.was_retry
    }
}

/// Seam: surface an interrupted run (interrupt-resilience U2/R2). Default no-op;
/// `lib.rs` wires it to the alert pipeline (`AlertsLog` append + attention ring
/// or R17 pending-queue), the same machinery a script alert uses. Called from
/// the post-recovery step, never under the store lock (KTD-B).
pub type InterruptSink = Arc<dyn Fn(&InterruptedRun) + Send + Sync>;

/// Monitor-handoff U3 seam (R14/R15/R7): surface a monitor verdict or a
/// "monitor broken" escalation as `(automation name, alert line)` — the exact
/// shape of `lib.rs`'s shared `surface_alert` path (`AlertsLog` append +
/// `Signal { Alert, Cli }` ring or R17 pending-queue), so verdict attention
/// rides the existing alert machinery (the plan's KTD: no new attention
/// producer). Always invoked **after** the store lock is released (KTD-B).
/// Default no-op; `lib.rs` injects `surface_alert` itself.
pub type MonitorAlertSink = Arc<dyn Fn(&str, &str) + Send + Sync>;

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
    /// Opt-in interrupt resilience (interrupt-resilience U1/R1). Default `false`
    /// at the CLI/socket boundary; stamped onto the [`Automation`] as-is.
    pub retry_on_interrupt: bool,
    /// Monitor not-before floor, epoch ms (monitor-handoff U2, R1): clamps
    /// the initial `next_run_at` via [`schedule::advance_from`] and is
    /// stamped onto the record so every later recompute keeps clamping.
    /// `None` for plain recurring automations; U4/U5 thread the real value
    /// through from `create --monitor --not-before`.
    pub not_before_ms: Option<u64>,
    /// Monitor flavor (monitor-handoff U4, R1): stamped as-is. The create
    /// arm in `lib.rs` is the enforcement point pairing this with
    /// `pickup_pointers` (R11/R12) and agent mode — the manager only stamps.
    pub monitor: bool,
    /// Pickup pointers captured from the registering pane at create time
    /// (monitor-handoff U4, R11) — resolved by the caller against the pane's
    /// own attribution (never self-declared over the wire); `None` for every
    /// non-monitor create. A monitor create with no qualified pointers never
    /// reaches this spec: it is refused upstream with
    /// [`ERR_MONITOR_POINTERS`] (R12).
    pub pickup_pointers: Option<crate::automations::model::MonitorPointers>,
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
    /// Whether the create's store flush reached disk (fix(review) #14).
    /// `false` only in the KTD-B degraded arm: the record is live in memory
    /// (and `warning` says so) but would not survive a restart. `warning` is
    /// **overloaded** (the R1 min-gap advisory rides it too), so callers that
    /// must know the flush outcome read this flag, never the string — the
    /// monitor-registered emit (monitor-handoff R13) is gated on it in
    /// `lib.rs`: R12's refuse-rather-than-lose posture means never closing
    /// the parent tab on a registration that may die with the app.
    pub flush_ok: bool,
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
    /// Headless-monitor-checks U4 (R5): kill an in-flight headless check by
    /// run id (see [`HeadlessKiller`]). Default no-op; `lib.rs` injects the
    /// runner's `kill_run` (U5).
    headless_killer: Mutex<HeadlessKiller>,
    /// Headless-monitor-checks U4 (R7): the overlap probe's headless leg
    /// (see [`HeadlessAliveProbe`]). Default `false`.
    headless_check_alive: Mutex<HeadlessAliveProbe>,
    /// Headless-monitor-checks U4 (R7): the backstop's monotonic gate (see
    /// [`HeadlessDeadlineGate`]). Default `true` (= entry gone).
    headless_deadline_gate: Mutex<HeadlessDeadlineGate>,
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
    /// Monitor-handoff U4 (R13): emit `automation://monitor-registered` after
    /// a successful monitor create (see [`MonitorRegisteredEmitter`]).
    /// Default no-op; `lib.rs` injects the Tauri emit.
    emit_monitor_registered: Mutex<MonitorRegisteredEmitter>,
    /// Interrupt-resilience U2: surface an interrupted run through the alert
    /// pipeline (see [`InterruptSink`]). Default no-op; `lib.rs` injects the
    /// real one.
    interrupt_sink: Mutex<InterruptSink>,
    /// Monitor-handoff U3: verdict / broken-monitor alerts (see
    /// [`MonitorAlertSink`]). Default no-op; `lib.rs` injects `surface_alert`.
    monitor_alert_sink: Mutex<MonitorAlertSink>,
    /// Monitor-handoff U3 (R15): directory the FAIL-verdict bundle files are
    /// written under — the `FLY_APP_NAME` data root's `monitor-bundles/` in
    /// the app (`lib.rs` injects it), a tempdir in tests. `None` (the
    /// default) degrades to the fail-tolerant no-bundle path: the close and
    /// retire still land and the alert notes the missing bundle.
    bundle_dir: Mutex<Option<std::path::PathBuf>>,
    /// Interrupt-resilience U2/R2: runs that startup recovery closed
    /// `interrupted`, stashed by [`AutomationManager::new`] and drained once by
    /// [`AutomationManager::process_pending_interrupts`] on the first sweep tick
    /// (once the alert sink is wired) — alert each, enqueue the retry-eligible.
    pending_interrupts: Mutex<Vec<InterruptedRun>>,
    /// Interrupt-resilience U2/R1: automation ids awaiting a one-shot
    /// [`Trigger::Retry`] re-dispatch. Drained inside the sweep so a retry
    /// honors the same readiness/overlap gates as a scheduled fire (an agent
    /// retry waits for `frontend_ready`; a re-run is skipped if a run is already
    /// in flight).
    retry_queue: Mutex<VecDeque<String>>,
    /// R5 gate: while false, due **agent** automations are deferred —
    /// neither claimed nor skipped (see [`AutomationManager::sweep_once`]).
    /// Monitor automations are carved out (headless-monitor-checks R6):
    /// their checks dispatch headless — no `automation://agent-run` event
    /// for a listener-less webview to drop — so scheduled fires and retries
    /// proceed with the gate down.
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
            headless_killer: Mutex::new(Arc::new(|_run_id: &str| {})),
            headless_check_alive: Mutex::new(Arc::new(|_automation_id: &str| false)),
            headless_deadline_gate: Mutex::new(Arc::new(|_run_id: &str| true)),
            config: Mutex::new(Arc::new(ConfigStore::ephemeral(
                crate::config::Config::default(),
            ))),
            output_capturer: Mutex::new(Arc::new(|_cwd: &str, _since: u64| None)),
            emit_run_closed: Mutex::new(Arc::new(|_ev: &RunClosedEvent| {})),
            emit_monitor_registered: Mutex::new(Arc::new(|_ev: &MonitorRegisteredEvent| {})),
            interrupt_sink: Mutex::new(Arc::new(|_ir: &InterruptedRun| {})),
            monitor_alert_sink: Mutex::new(Arc::new(|_name: &str, _line: &str| {})),
            bundle_dir: Mutex::new(None),
            pending_interrupts: Mutex::new(Vec::new()),
            retry_queue: Mutex::new(VecDeque::new()),
            frontend_ready: AtomicBool::new(false),
            automation_panes: Mutex::new(HashSet::new()),
            sweep_stop: Mutex::new(false),
            sweep_wake: Condvar::new(),
        };
        // Close orphaned Running rows and stash them; the alert + retry happen
        // later, once the alert sink is wired (the first sweep tick drains).
        let interrupted = mgr.recover_interrupted();
        *mgr.pending_interrupts.lock().unwrap() = interrupted;
        mgr
    }

    /// R5 startup recovery: close all orphaned `Running` rows
    /// failed([`ERR_INTERRUPTED`]) under one lock hold / one flush, and **return
    /// them** (interrupt-resilience U2/R2) so the caller can stash them for the
    /// post-recovery alert + retry step. No `automation://changed` and no alert
    /// fire here — this runs inside [`AutomationManager::new`], before the alert
    /// sink or any listener exists; surfacing is deferred to
    /// [`AutomationManager::process_pending_interrupts`], which the first sweep
    /// tick calls once the sink is wired.
    ///
    /// Also **clears every persisted `pane_id`** (U7 launch-stability): pane
    /// ids reset each launch (they start at 1 and are never reused within a
    /// run), so a `pane_id` loaded from disk can never refer to a live pane
    /// now. Left in place, a stale terminal row's id could resolve to an
    /// unrelated new pane and wedge R7's alive-probe (the automation would
    /// look permanently in-flight and never fire). One clear on load is the
    /// whole fix — within a launch, ids are unique, so no live row ever needs
    /// its id cleared.
    fn recover_interrupted(&self) -> Vec<InterruptedRun> {
        let now = (self.clock)();
        // Collected into an outer Vec (not the closure's return) so it survives
        // a flush failure: `store.mutate` drops the closure's value on flush
        // error, but the in-memory close still applied (KTD-B), so the alert +
        // retry must still fire.
        let mut interrupted: Vec<InterruptedRun> = Vec::new();
        let _ = self.store.mutate(|map| {
            for a in map.values_mut() {
                for r in a.runs.iter_mut() {
                    r.pane_id = None;
                }
                for run_id in running_run_ids(a) {
                    // Read the row's trigger before the close (the close does not
                    // change it) to honor the retry-once guard (R4).
                    let was_retry = a
                        .runs
                        .iter()
                        .find(|r| r.id == run_id)
                        .is_some_and(|r| r.trigger == Trigger::Retry);
                    a.close(&run_id, failed(ERR_INTERRUPTED), now);
                    interrupted.push(InterruptedRun {
                        automation_id: a.id.clone(),
                        run_id,
                        name: a.name.clone(),
                        retry_on_interrupt: a.retry_on_interrupt,
                        enabled: a.enabled,
                        was_retry,
                    });
                }
            }
        });
        interrupted
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

    /// Headless-monitor-checks U4: inject the headless-check killer (see
    /// [`HeadlessKiller`]).
    pub fn set_headless_killer(&self, k: HeadlessKiller) {
        *self.headless_killer.lock().unwrap() = k;
    }

    /// Headless-monitor-checks U4: inject the headless in-flight probe (see
    /// [`HeadlessAliveProbe`]).
    pub fn set_headless_check_alive(&self, p: HeadlessAliveProbe) {
        *self.headless_check_alive.lock().unwrap() = p;
    }

    /// Headless-monitor-checks U4: inject the backstop's monotonic gate (see
    /// [`HeadlessDeadlineGate`]).
    pub fn set_headless_deadline_gate(&self, g: HeadlessDeadlineGate) {
        *self.headless_deadline_gate.lock().unwrap() = g;
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

    /// Monitor-handoff U4: inject the `automation://monitor-registered`
    /// emitter (see [`MonitorRegisteredEmitter`]).
    pub fn set_monitor_registered_emitter(&self, e: MonitorRegisteredEmitter) {
        *self.emit_monitor_registered.lock().unwrap() = e;
    }

    /// Monitor-handoff U4 (R13's backend half): fire
    /// [`MONITOR_REGISTERED_EVENT`] for a monitor `automation_id` just
    /// registered from `pane_id`. Called by the socket create arm strictly
    /// after [`AutomationManager::create`] returned — the store is flushed
    /// and its lock released (KTD-B) — so the frontend's tab close (U6) can
    /// never observe a registration the store might still lose.
    pub fn emit_monitor_registered(&self, pane_id: u64, automation_id: &str) {
        let ev = MonitorRegisteredEvent {
            pane_id,
            automation_id: automation_id.to_string(),
        };
        let emitter = Arc::clone(&self.emit_monitor_registered.lock().unwrap());
        emitter(&ev);
    }

    /// Interrupt-resilience U2: inject the interrupted-run alert sink (see
    /// [`InterruptSink`]). Wire this **before** the sweep starts so the first
    /// tick's [`AutomationManager::process_pending_interrupts`] can surface the
    /// startup-recovery backlog.
    pub fn set_interrupt_sink(&self, sink: InterruptSink) {
        *self.interrupt_sink.lock().unwrap() = sink;
    }

    /// Monitor-handoff U3: inject the verdict / broken-monitor alert sink
    /// (see [`MonitorAlertSink`]).
    pub fn set_monitor_alert_sink(&self, sink: MonitorAlertSink) {
        *self.monitor_alert_sink.lock().unwrap() = sink;
    }

    /// Monitor-handoff U3 (R15): inject the failure-bundle directory (the
    /// `FLY_APP_NAME` data root's `monitor-bundles/`).
    pub fn set_bundle_dir(&self, dir: std::path::PathBuf) {
        *self.bundle_dir.lock().unwrap() = Some(dir);
    }

    /// Interrupt-resilience U2/R2/R3: drain the startup-recovery backlog exactly
    /// once — alert **every** interrupted run through the [`InterruptSink`], and
    /// enqueue the retry-eligible ones ([`InterruptedRun::is_retry_eligible`])
    /// for a one-shot [`Trigger::Retry`] re-dispatch by the sweep. Idempotent:
    /// `mem::take` empties the backlog, so a second call is a no-op. Takes no
    /// store lock — only the pending/sink/retry mutexes — so it is safe to call
    /// from the top of `sweep_once` (KTD-B). Also fires `automation://changed`
    /// per affected automation so an already-open dashboard refreshes the now
    /// `interrupted` run row.
    pub fn process_pending_interrupts(&self) {
        let pending: Vec<InterruptedRun> =
            std::mem::take(&mut *self.pending_interrupts.lock().unwrap());
        if pending.is_empty() {
            return;
        }
        let sink = Arc::clone(&self.interrupt_sink.lock().unwrap());
        let mut to_retry: Vec<String> = Vec::new();
        for ir in &pending {
            sink(ir);
            if ir.is_retry_eligible() {
                to_retry.push(ir.automation_id.clone());
            }
        }
        if !to_retry.is_empty() {
            let mut q = self.retry_queue.lock().unwrap();
            q.extend(to_retry);
        }
        // Emitted after the sink/queue work, holding no store lock (KTD-B). A
        // listener may not exist yet at startup; the dashboard refetches on open
        // regardless, so this is a best-effort live refresh.
        for ir in &pending {
            (self.emit_changed)(&ir.automation_id);
        }
        // Monitor-handoff R7: an interrupted check is an infra failure —
        // evaluate the broken-monitor escalation now that the alert sink is
        // wired (this runs on the first sweep tick), deduped per automation.
        let mut ids: Vec<String> = pending.iter().map(|ir| ir.automation_id.clone()).collect();
        ids.sort();
        ids.dedup();
        for id in ids {
            self.check_monitor_escalation(&id);
        }
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
    /// initial `next_run_at` via [`schedule::advance_from`] (the not-before
    /// floor clamps it, monitor-handoff U2/R1), persist, and emit
    /// `automation://changed`.
    pub fn create(&self, spec: CreateSpec) -> Result<Created, String> {
        let validation = schedule::validate(&spec.cron, &spec.timezone)?;
        let now = (self.clock)();
        let next_run_at =
            schedule::advance_from(&spec.cron, &spec.timezone, now, spec.not_before_ms)?;

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
            retry_on_interrupt: spec.retry_on_interrupt,
            // Monitor fields (monitor-handoff plan U1/U2/U4): the not-before
            // floor is stamped from the spec — already clamped into
            // `next_run_at` above (R1) — so every later recompute keeps
            // clamping; the flag and the registration-time pickup pointers
            // (R11) arrive pre-qualified from the create arm in `lib.rs`
            // (a pointer-less monitor create was refused there, R12).
            monitor: spec.monitor,
            not_before_ms: spec.not_before_ms,
            retired_at: None,
            pickup_pointers: spec.pickup_pointers,
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
        // so a CLI retry doesn't mint a duplicate schedule. `flush_ok`
        // carries the outcome as a typed flag (fix(review) #14): the warning
        // string is overloaded with the R1 min-gap advisory, so callers must
        // never string-match it.
        let mut warning = validation.min_gap_warning;
        let mut flush_ok = true;
        match inserted {
            Ok(true) => {}
            Ok(false) => return Err("id collision while creating automation; retry".into()),
            Err(e) => {
                flush_ok = false;
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
            flush_ok,
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
    /// [`schedule::advance_from`] — a stale past value must never fire
    /// instantly (AE7: an automation paused for a week resumes into the
    /// future), and the not-before floor rides the same recompute
    /// (monitor-handoff U2, R1): resuming a parked monitor before its floor
    /// never schedules early.
    ///
    /// A **retired** monitor refuses (monitor-handoff R3: scheduling stopped
    /// permanently — re-arming `next_run_at` would set the sweep re-claiming,
    /// and being refused, every tick forever). The gate runs inside the
    /// mutation, before any write, so no re-arm can slip between check and
    /// write (fix(review) #2).
    pub fn resume(&self, id: &str) -> Result<Automation, String> {
        let now = (self.clock)();
        // Not `with_automation`: that helper cannot express a refusal, so the
        // retirement gate runs the same flush-tolerant mutate inline.
        let updated: Option<Result<Automation, String>> = flush_tolerant(
            self.store.mutate(|map| {
                map.get_mut(id).map(|a| {
                    if a.retired_at.is_some() {
                        return Err(ERR_RESUME_RETIRED.to_string());
                    }
                    // Pure cron math inside the lock is fine (KTD-B bans
                    // dispatch/emit/IO, not computation). Unparseable stored
                    // state degrades to paused rather than firing at a bogus
                    // instant.
                    a.next_run_at =
                        schedule::advance_from(&a.cron, &a.timezone, now, a.not_before_ms)
                            .ok()
                            .flatten();
                    a.updated_at = now;
                    Ok(a.clone())
                })
            }),
            // Flush failed but the mutation applied (KTD-B store contract):
            // re-derive from the authoritative record — the retirement gate
            // re-checks identically, so the refetch reports the same outcome.
            || {
                self.store.get(id).map(|a| {
                    if a.retired_at.is_some() {
                        Err(ERR_RESUME_RETIRED.to_string())
                    } else {
                        Ok(a)
                    }
                })
            },
        );
        let updated = updated.ok_or_else(|| format!("no such automation: {id}"))??;
        (self.emit_changed)(id);
        Ok(updated)
    }

    /// R23 teardown: remove the record + its stored script content (store
    /// owns that half), close its open rows failed([`ERR_DELETED`]), and
    /// kill any in-flight script group via the U5 seam **or in-flight
    /// headless check via the headless-killer seam (headless-monitor-checks
    /// U4 — R5: a delete mid-check kills the backend-owned child; there is
    /// no pane to leave alone)** — killers invoked **after** the store lock
    /// is released (KTD-B). The in-flight *pane* agent is unlinked, never
    /// killed; its recursion-registry entry (U7) deliberately survives until
    /// the pane exits, otherwise create → delete would un-gate a still-live
    /// automation-spawned pane (R22).
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
        // Lock released. Kill in-flight script groups / headless children
        // (no-op seams until wired), and close the open rows on the removed
        // record. The row-level `headless` marker picks the killer — mode
        // alone can't (a monitor is agent-mode).
        let now = (self.clock)();
        let script_killer = Arc::clone(&self.script_killer.lock().unwrap());
        let headless_killer = Arc::clone(&self.headless_killer.lock().unwrap());
        let running: Vec<(String, bool)> = automation
            .runs
            .iter()
            .filter(|r| r.status == RunStatus::Running)
            .map(|r| (r.id.clone(), r.headless))
            .collect();
        for (run_id, headless) in running {
            if headless {
                headless_killer(&run_id);
            } else if automation.mode.kind() == RunMode::Script {
                script_killer(&run_id);
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
        let headless_alive = Arc::clone(&self.headless_check_alive.lock().unwrap());
        // Phase 1 (KTD-D/R2): decide + record + FLUSH under one lock hold.
        // `None` = unknown id; the inner Result carries the claim outcome.
        let decision: Option<Result<ManualRun, String>> = flush_tolerant(
            self.store.mutate(|map| {
                let Some(a) = map.get_mut(id) else {
                    return None;
                };
                if in_flight_widened(a, &alive, &headless_alive) {
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
                        // fix(review) #8: report the variant — a retired
                        // monitor (R3, permanent) must not read as merely
                        // disabled (resumable).
                        .map_err(|e| match e {
                            ClaimError::Retired => ERR_RUN_RETIRED.to_string(),
                            ClaimError::Disabled => "automation is disabled".to_string(),
                        }),
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
                        // No row recorded ⇒ the claim was refused. The
                        // [`ClaimError`] variant is lost here (this branch
                        // re-derives from the map), so re-check retirement on
                        // the fetched record for the right message
                        // (fix(review) #8).
                        None if a.retired_at.is_some() => Err(ERR_RUN_RETIRED.to_string()),
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
            // Monitor-handoff R7: a manual-run dispatch failure on a monitor
            // is an infra failure like any other (after the lock, KTD-B). The
            // fetched record is in hand — only monitors take the check.
            if automation.monitor {
                self.check_monitor_escalation(id);
            }
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
        self.close_run_stamping(automation_id, run_id, outcome, None)
    }

    /// [`AutomationManager::close_run`] plus the headless session-id stamp
    /// (headless-monitor-checks U4 — R12): when `session_id` is `Some`, it
    /// is recorded on the row **in the same store mutation** as the close,
    /// so a crash can never strand a closed check without its session
    /// pointer. `None` (every non-headless caller) leaves the field
    /// untouched — pane rows keep serializing byte-identically (R14).
    fn close_run_stamping(
        &self,
        automation_id: &str,
        run_id: &str,
        outcome: RunOutcome,
        session_id: Option<&str>,
    ) -> model::CloseResult {
        let now = (self.clock)();
        // Monitor-handoff R6/R7: a failed close is an infrastructure failure
        // (never a verdict) — evaluate the broken-monitor escalation after
        // the mutation lands (outside the lock, below).
        let status = outcome_status(&outcome);
        let result = flush_tolerant(
            self.store.mutate(|map| {
                map.get_mut(automation_id).map(|a| {
                    let res = a.close(run_id, outcome, now);
                    if res == model::CloseResult::Closed {
                        if let (Some(sid), Some(row)) =
                            (session_id, a.runs.iter_mut().find(|r| r.id == run_id))
                        {
                            row.session_id = Some(sid.to_owned());
                        }
                    }
                    res
                })
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
                    if status == RunStatus::Failed {
                        // Monitor-handoff R7 (after the lock, KTD-B).
                        self.check_monitor_escalation(automation_id);
                    }
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
    /// `pane_id`, capture its transcript output (U4b), and delegate to the
    /// one shared close tail
    /// ([`AutomationManager::close_run_with_capture`] — where the monitor
    /// verdict/retire/escalation semantics live). Idempotent — returns
    /// `Ok(())` whether it closed a row, found no running run, or hit an
    /// already-closed one (all benign for the Stop / pane-exit callers).
    ///
    /// U4b (R8): the capturer reads a transcript from disk (off the store
    /// lock, KTD-B) and abstains on an ambiguous cwd — so this never blocks
    /// the close and never records the wrong session's content. The captured
    /// text fills the outcome's output slot **in full** — the R8 tail cap
    /// runs inside the close mutation, after the tail's verdict parse.
    fn close_run_by_pane_with(&self, pane_id: u64, mut outcome: RunOutcome) -> Result<(), String> {
        let found: Option<(Automation, String, Option<u64>)> = {
            let map = self.store.snapshot();
            let mut found = None;
            'outer: for a in map.into_values() {
                for run in &a.runs {
                    if run.pane_id == Some(pane_id) && run.status == RunStatus::Running {
                        // Keep the whole record: cwd + dispatch time feed the
                        // U4b transcript read below; monitor flag / pointers /
                        // name feed the U3 verdict path.
                        let run_id = run.id.clone();
                        let started_at = run.started_at;
                        found = Some((a, run_id, started_at));
                        break 'outer;
                    }
                }
            }
            found
        };
        let Some((automation, run_id, started_at)) = found else {
            // No running run for this pane: idempotent no-op (second Stop, or a
            // run the deadline/other path already closed).
            return Ok(());
        };
        if outcome_output(&outcome).is_none() {
            if let Some(started) = started_at {
                let capturer = Arc::clone(&self.output_capturer.lock().unwrap());
                if let Some(text) = capturer(&automation.cwd, started) {
                    outcome = with_output(outcome, text);
                }
            }
        }
        self.close_run_with_capture(&automation, &run_id, outcome, None)
    }

    /// **The one shared verdict-close tail** (headless-monitor-checks U4 —
    /// the "One shared verdict close path" KTD, carrying monitor-handoff
    /// U3's semantics): close `run_id` on the pre-fetched `automation` with
    /// `outcome`, whose output slot holds the FULL already-captured,
    /// already-cleaned text — sanitize → scrub happened upstream (the
    /// capturer / [`AutomationManager::close_headless_run`]) and the R8 tail
    /// cap runs downstream inside the close mutation, so the verdict parse
    /// here always sees the full cleaned text **before** capping (R8). Both
    /// close routes end here — the pane path
    /// ([`AutomationManager::close_run_by_pane_with`], after transcript
    /// capture) and the headless path
    /// ([`AutomationManager::close_headless_run`], with the stream result) —
    /// so retire semantics exist in exactly one place (R9):
    ///
    /// - a live (not yet retired) monitor's `Succeeded` close parses the R2
    ///   verdict from the output slot — Failed closes are infrastructure
    ///   failures, never a verdict (R6) — and a parsed verdict routes to the
    ///   atomic close+verdict+retire
    ///   ([`AutomationManager::close_monitor_run_retiring`]);
    /// - otherwise the ordinary close lands the row (the Failed-close R7
    ///   escalation lives inside [`AutomationManager::close_run`]'s body),
    ///   `automation://run-closed` fires when the row actually closed (plain
    ///   `close_run` never emits it), and a verdict-less `Succeeded`
    ///   **monitor** close evaluates the R7 escalation and lets the derived
    ///   walk decide: an absent output (abstained pane capture / empty
    ///   headless result, the U3 refinement) and a near-miss block (an
    ///   opened ```` ```verdict ```` fence that never parsed, fix(review)
    ///   #5) count toward "monitor broken", while a readable not-done check
    ///   derives zero and stays silent. Without this leg a monitor emitting
    ///   near-miss/empty results would never ring.
    ///
    /// `session_id` is the headless check's stream-derived id (R12), stamped
    /// in the same store mutation as the close on both branches; the pane
    /// path passes `None`.
    fn close_run_with_capture(
        &self,
        automation: &Automation,
        run_id: &str,
        outcome: RunOutcome,
        session_id: Option<&str>,
    ) -> Result<(), String> {
        let parsed = if automation.monitor
            && automation.retired_at.is_none()
            && matches!(outcome, RunOutcome::Succeeded { .. })
        {
            outcome_output(&outcome).and_then(|t| verdict::parse_verdict(t))
        } else {
            None
        };
        if let Some(v) = parsed {
            let evidence = outcome_output(&outcome).cloned().unwrap_or_default();
            return self.close_monitor_run_retiring(
                automation, run_id, outcome, v, &evidence, session_id,
            );
        }
        let status = outcome_status(&outcome);
        if self.close_run_stamping(&automation.id, run_id, outcome, session_id)
            == model::CloseResult::Closed
        {
            self.emit_run_closed(&automation.id, run_id, status);
            if automation.monitor && status == RunStatus::Succeeded {
                // R7, after the lock (KTD-B). Every Succeeded close on this
                // branch is verdict-less by construction (a parsed verdict
                // routed to the retiring close above) — see the method doc.
                self.check_monitor_escalation(&automation.id);
            }
        }
        Ok(())
    }

    /// Headless-monitor-checks U4 (R8/R9/R12): the run-id-keyed close entry
    /// the [`headless::CheckCloser`] calls (U5 constructs the closure) — the
    /// headless twin of the pane-keyed Stop/exit closes, delegating to the
    /// same shared tail ([`AutomationManager::close_run_with_capture`]) so a
    /// headless verdict retires through exactly the mutation a pane verdict
    /// does. Also the sweep backstop's close (R7), with
    /// [`ERR_HEADLESS_BACKSTOP`] as the infra reason.
    ///
    /// **Cleaning happens here**, per [`headless::CheckOutcome`]'s
    /// sanitize/scrub contract (nothing in `headless.rs` is cleaned): both
    /// the Clean result text and the Infra reason go sanitize → scrub before
    /// touching the row, through the ONE shared helper pair
    /// ([`redact::clean_captured`] / [`redact::clean_text`], U5) that
    /// `lib.rs`'s `OutputCapturer` closure also calls — so the order
    /// invariant (control chars first, or one inside a token splits it past
    /// the scrub; the R8 tail cap last, inside the close mutation, AFTER the
    /// tail's verdict parse) cannot drift between paths.
    /// Empty-after-cleaning result text maps to `None` — exact parity with a
    /// pane capture that came back empty, so the row reads *unreadable* in
    /// the derived R7 counter. The Infra reason (which may embed the
    /// runner-bounded raw stderr tail) goes through [`redact::clean_text`] —
    /// the non-`None`ing sibling, since a stored error string must survive —
    /// and is additionally head-capped (the fly-authored classification
    /// message leads it) since `error` has no cap of its own in the close.
    ///
    /// An unknown automation id (deleted mid-check) is a benign no-op, like
    /// every reaper-side close; an already-terminal row lands as the usual
    /// `AlreadyClosed` no-op inside the tail.
    pub fn close_headless_run(
        &self,
        automation_id: &str,
        run_id: &str,
        outcome: headless::CheckOutcome,
    ) {
        let Some(automation) = self.store.get(automation_id) else {
            return;
        };
        let (outcome, session_id): (RunOutcome, Option<String>) = match outcome {
            headless::CheckOutcome::Clean { text, session_id } => {
                let output = redact::clean_captured(&text);
                (RunOutcome::Succeeded { output }, session_id)
            }
            headless::CheckOutcome::Infra { reason } => {
                let cleaned = redact::clean_text(&reason);
                let error = head_capped(&cleaned, model::OUTPUT_TAIL_CAP_BYTES).to_owned();
                (
                    RunOutcome::Failed {
                        error,
                        exit_code: None,
                        output: None,
                    },
                    None,
                )
            }
        };
        let _ = self.close_run_with_capture(&automation, run_id, outcome, session_id.as_deref());
    }

    /// Monitor-handoff U3 (R2/R3/R4/R14/R15): the verdict close. **One store
    /// mutation** — under a single [`store::Store::mutate`] hold — closes the
    /// `Running` row (tail-capping its output, R8), stamps the parsed
    /// [`Verdict`], records the bundle path, and retires the monitor
    /// ([`Automation::retire`]: `retired_at` set, `next_run_at` cleared), so
    /// no crash between steps can strand a verdict without its retirement or
    /// vice versa (the plan's atomicity KTD). The bundle-file write (FAIL
    /// only) and the alert raise happen strictly **after** the lock is
    /// released (KTD-B), on the caller's thread (the Stop dispatch already
    /// runs the capturer off the hook/PTY threads).
    ///
    /// Fail-tolerant like everything else on this path: a bundle write
    /// failure (or an unwired bundle dir) never blocks the close/retire —
    /// the alert line notes the missing bundle instead (mirrors
    /// [`flush_tolerant`]'s in-memory-wins posture).
    ///
    /// Headless-monitor-checks U4 (R12): `session_id` — the headless check's
    /// stream-derived id — rides the same mutation, stamped on the row with
    /// the verdict; `None` on the pane path. On a FAIL it also rides the
    /// bundle as the "Check session" block (U5): the id plus the transcript
    /// path derived from the automation's cwd
    /// ([`crate::session::transcript::claude_project_dir`] — the one home of
    /// the cwd encoding), distinct from the registration-time pickup
    /// pointers; omitted entirely when the close carried no id.
    fn close_monitor_run_retiring(
        &self,
        automation: &Automation,
        run_id: &str,
        outcome: RunOutcome,
        verdict: Verdict,
        evidence: &str,
        session_id: Option<&str>,
    ) -> Result<(), String> {
        let now = (self.clock)();
        let status = outcome_status(&outcome);
        // The bundle path is decided *before* the mutation so the run row can
        // reference it atomically (R15: the row is the bundle's index); the
        // file itself is written after the lock below. `None` when there is
        // no FAIL bundle to write (PASS) or no bundle dir is wired.
        let bundle_path: Option<String> = if verdict.outcome == VerdictOutcome::Fail {
            self.bundle_dir.lock().unwrap().as_ref().map(|d| {
                d.join(format!("{}-{}.md", automation.id, run_id))
                    .to_string_lossy()
                    .into_owned()
            })
        } else {
            None
        };
        // R8 cap discipline (fix(review) #11): the note is untrusted agent
        // output riding every flush and DTO fetch — stamp a HEAD-capped copy
        // on the row (the head carries the verdict summary; contrast the R8
        // output tail, where the verdict trails). The FULL note still rides
        // the FAIL bundle below, and the alert line takes only its first line.
        let v = Verdict {
            outcome: verdict.outcome,
            note: head_capped(&verdict.note, model::OUTPUT_TAIL_CAP_BYTES).to_owned(),
        };
        let bp = bundle_path.clone();
        let sid: Option<String> = session_id.map(str::to_owned);
        let closed: Option<model::CloseResult> = flush_tolerant(
            self.store.mutate(|map| {
                map.get_mut(&automation.id).map(|a| {
                    let res = a.close(run_id, outcome, now);
                    if res == model::CloseResult::Closed {
                        if let Some(row) = a.runs.iter_mut().find(|r| r.id == run_id) {
                            row.verdict = Some(v.clone());
                            row.bundle_path = bp.clone();
                            // Headless-monitor-checks R12: the check's
                            // session id lands atomically with its verdict.
                            if sid.is_some() {
                                row.session_id = sid.clone();
                            }
                        }
                        // R3: retire in the SAME mutation as the close.
                        a.retire(now);
                    }
                    res
                })
            }),
            // Flush failed but the mutation applied (KTD-B store contract):
            // re-derive from the authoritative map.
            || {
                self.store.get(&automation.id).map(|a| {
                    match a.runs.iter().find(|r| r.id == run_id) {
                        Some(r) if r.status.is_terminal() => model::CloseResult::Closed,
                        _ => model::CloseResult::NotFound,
                    }
                })
            },
        );
        if closed != Some(model::CloseResult::Closed) {
            // Deleted mid-close, or another path (deadline/pane-exit) closed
            // the row first — that close carried no verdict, so nothing
            // retires and nothing rings here (idempotent, like the plain
            // pane-keyed close).
            return Ok(());
        }
        // ---- lock released (KTD-B): bundle write, alert, events -------------
        // R15: write the failure bundle — verdict note + pickup pointers +
        // the captured turn (outside the R8 8-KiB run-output tail cap; its own
        // generous cap below keeps a pathologically verbose turn from writing
        // an unbounded file — the tail is kept, where the verdict block and
        // failure narrative live, matching the R8 tail convention).
        let mut bundle_note = String::new();
        if verdict.outcome == VerdictOutcome::Fail {
            // The automation name is untrusted creator input embedded into a
            // rendered document — flatten control chars at write time, the
            // alerts-log posture (R16; fix(review) #11).
            let safe_name = crate::notify::sanitize_title(&automation.name);
            let ctx = verdict::BundleContext {
                automation_name: &safe_name,
                automation_id: &automation.id,
                run_id,
                closed_at_ms: now,
            };
            // Headless-monitor-checks U5 (R12): the check's own diagnostic
            // session block. The id is untrusted stream JSON embedded into a
            // rendered document — flatten control chars at write time, the
            // alerts-log posture (R16), like the name above (the row keeps
            // the raw id; real ids are UUIDs, so this is a no-op there). The
            // transcript path derives from the sanitized id so the two lines
            // cohere; `None` (no home/config root) drops just the path line.
            let check_session = session_id.map(|sid| {
                let safe_sid = crate::notify::sanitize_title(sid);
                let transcript_path = crate::session::transcript::claude_project_dir(
                    std::path::Path::new(&automation.cwd),
                )
                .map(|d| {
                    d.join(format!("{safe_sid}.jsonl"))
                        .to_string_lossy()
                        .into_owned()
                });
                verdict::CheckSession {
                    session_id: safe_sid,
                    transcript_path,
                }
            });
            let rendered = verdict::render_bundle(
                &ctx,
                &verdict,
                tail_capped(evidence, BUNDLE_EVIDENCE_CAP_BYTES),
                automation.pickup_pointers.as_ref(),
                check_session.as_ref(),
            );
            bundle_note = match &bundle_path {
                Some(p) => match write_bundle_file(std::path::Path::new(p), &rendered) {
                    Ok(()) => format!(" — bundle: {p}"),
                    Err(e) => {
                        eprintln!(
                            "[fly-automations] could not write monitor bundle {p} ({e}); \
                             the run close and retirement still hold"
                        );
                        " — bundle could not be written".to_owned()
                    }
                },
                None => " — bundle could not be written (no bundle dir)".to_owned(),
            };
        }
        // R14/R15: the verdict rings through the existing alert path.
        let line = verdict_alert_line(&verdict, &bundle_note);
        let sink = Arc::clone(&self.monitor_alert_sink.lock().unwrap());
        sink(&automation.name, &line);
        (self.emit_changed)(&automation.id);
        self.emit_run_closed(&automation.id, run_id, status);
        Ok(())
    }

    /// Monitor-handoff R7: evaluate the broken-monitor escalation for one
    /// automation, called after an infra-failure close **outside** the store
    /// lock (KTD-B — it snapshots the record, then rings). The count is
    /// derived from run history ([`Automation::consecutive_infra_failures`],
    /// U1); [`verdict::broken_alert_due`] fires at each positive multiple of
    /// three, which *is* the "reset after alerting" representation (see its
    /// doc). Retired and non-monitor automations never ring.
    fn check_monitor_escalation(&self, automation_id: &str) {
        let Some(a) = self.store.get(automation_id) else {
            return;
        };
        if !a.monitor || a.retired_at.is_some() {
            return;
        }
        let n = a.consecutive_infra_failures();
        if verdict::broken_alert_due(n) {
            let line =
                format!("monitor broken: {n} consecutive checks failed without a verdict");
            let sink = Arc::clone(&self.monitor_alert_sink.lock().unwrap());
            sink(&a.name, &line);
        }
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
    /// watchdogs (R5). **Monitor** automations are carved out of the
    /// deferral too (headless-monitor-checks R6, for scheduled fires and
    /// retries alike): their checks dispatch headless — no
    /// `automation://agent-run` event to drop — so the gate has nothing to
    /// protect.
    ///
    /// Headless backstop (headless-monitor-checks U4 — R7): phase 1 also
    /// *collects* (never closes) Running headless rows past deadline +
    /// slack ([`headless_deadline_expired_runs`]); phase 2c consults the
    /// monotonic gate per run, kills through the [`HeadlessKiller`] seam,
    /// and only then closes Failed through the shared tail — kill strictly
    /// before close, both off the store lock.
    pub fn sweep_once(&self, now_ms: u64) {
        // Interrupt-resilience U2: surface the startup-recovery backlog (alert +
        // enqueue retries) before anything else this tick. Idempotent — a no-op
        // after the first tick drains it. Takes no store lock (KTD-B).
        self.process_pending_interrupts();

        let frontend_ready = self.frontend_ready.load(Ordering::Acquire);
        let alive = Arc::clone(&self.agent_pane_alive.lock().unwrap());
        let headless_alive = Arc::clone(&self.headless_check_alive.lock().unwrap());
        let capacity = Arc::clone(&self.script_capacity.lock().unwrap());
        // Interrupt-resilience U2/R1: this tick's retry candidates, taken out of
        // the queue up front (no nested store↔queue lock). Agent retries that
        // can't run yet (frontend not ready) are collected into `requeue` and
        // put back after the mutate.
        let retry_ids: Vec<String> = self.retry_queue.lock().unwrap().drain(..).collect();
        let mut requeue: Vec<String> = Vec::new();

        // Phase 1: decide + mutate + flush under ONE lock hold.
        let mut changed: Vec<String> = Vec::new();
        let mut to_dispatch: Vec<(Automation, String)> = Vec::new();
        // Interrupt-resilience U2/R1: retry claims dispatch on their own path
        // (phase 2b) — unlike a scheduled claim, a retry-dispatch failure must
        // NOT recompute the schedule (a retry consumes no occurrence).
        let mut to_dispatch_retry: Vec<(Automation, String)> = Vec::new();
        // U5: agent runs the sweep itself closed failed (ack-timeout / deadline),
        // to emit `automation://run-closed` in phase 3 (outside the lock).
        let mut closed_agent_runs: Vec<(String, String)> = Vec::new();
        // Monitor-handoff R7: monitors among those closes, collected while the
        // `.monitor` bit is in hand so the phase-3 escalation check never
        // re-fetches (and clones) an ordinary automation just to bail.
        let mut monitor_infra_failed: Vec<String> = Vec::new();
        // Headless-monitor-checks R7 backstop candidates (epoch leg): dead
        // runners' rows, kill-then-closed in phase 2c — never inside the
        // mutate (the kill sleeps a grace, KTD-B).
        let mut headless_backstop: Vec<(String, String)> = Vec::new();
        let flush_result = self.store.mutate(|map| {
            for a in map.values_mut() {
                let mut touched = false;

                // R10 ack-timeout: agent rows never linked to a pane within
                // the window close failed (a dropped agent-run event).
                // Headless check rows are exempt — see the probe
                // (headless-monitor-checks R7).
                for run_id in ack_timed_out_agent_runs(a, now_ms) {
                    a.close(&run_id, failed(ERR_SPAWN_ACK), now_ms);
                    closed_agent_runs.push((a.id.clone(), run_id));
                    if a.monitor {
                        monitor_infra_failed.push(a.id.clone());
                    }
                    touched = true;
                }

                // R11 run deadline: an agent run linked to a pane but still
                // Running past the 30-min deadline closes failed(timed out).
                // `close` leaves `pane_id` in place, so if the pane is still
                // alive the R7 alive-probe keeps this occurrence in flight —
                // a genuinely stuck agent skips the next occurrence instead of
                // fanning out a second pane; once the pane exits, the probe
                // reads dead and the schedule resumes. Headless check rows
                // are exempt — their runner owns the deadline on a monotonic
                // clock; see the probe's suspend-race note
                // (headless-monitor-checks R7).
                for run_id in deadline_expired_agent_runs(a, now_ms) {
                    a.close(&run_id, failed(ERR_TIMED_OUT), now_ms);
                    closed_agent_runs.push((a.id.clone(), run_id));
                    if a.monitor {
                        monitor_infra_failed.push(a.id.clone());
                    }
                    touched = true;
                }

                // Headless-monitor-checks R7 backstop (epoch leg): COLLECT
                // Running headless rows past deadline + slack — no close
                // here. The kill must run off the store lock (KTD-B) and
                // strictly before the close (kill-then-close), with the
                // monotonic gate consulted at kill time (phase 2c). The row
                // stays Running this tick, so it still blocks the overlap
                // check below.
                for run_id in headless_deadline_expired_runs(a, now_ms) {
                    headless_backstop.push((a.id.clone(), run_id));
                }

                // Due = enabled ∧ next_run_at <= now (None = paused, R23).
                let due = a.enabled && a.next_run_at.is_some_and(|t| t <= now_ms);
                if due {
                    let is_agent = a.mode.kind() == RunMode::Agent;
                    if is_agent && !a.monitor && !frontend_ready {
                        // R5: defer — see the method doc. Deliberately no row,
                        // no advance, no event. Monitors pass (headless-
                        // monitor-checks R6): their checks dispatch headless,
                        // so there is no event for a listener-less webview
                        // to drop.
                    } else if in_flight_widened(a, &alive, &headless_alive) {
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
                        match a.claim(advanced, now_ms, Trigger::Schedule, &run_id) {
                            Ok(()) => {
                                to_dispatch.push((a.clone(), run_id));
                                touched = true;
                            }
                            // Monitor-handoff R3 defense in depth (fix(review)
                            // #2): a due-but-RETIRED record is degraded state
                            // (retire() nulls the schedule in the same
                            // mutation that stamps `retired_at`; only an
                            // on-disk edit or a historic re-arm bug pairs
                            // them). Null the schedule so the record
                            // self-heals instead of being re-claimed and
                            // refused every tick forever.
                            Err(ClaimError::Retired) => {
                                a.rollback_recompute(None);
                                touched = true;
                            }
                            // `due` requires `enabled`, so a Disabled refusal
                            // is unreachable here; leave the record alone.
                            Err(ClaimError::Disabled) => {}
                        }
                    }
                }
                if touched {
                    changed.push(a.id.clone());
                }
            }

            // Interrupt-resilience U2/R1/R4: drain this tick's retry candidates
            // under the same lock hold, so a retry claim is persisted before
            // dispatch exactly like a scheduled one (R2). A retry honors the R5
            // frontend-ready gate (an unattended re-run) and the R7 overlap
            // check, and — like a manual run (R23) — passes `next_run_at`
            // through unchanged so it never advances the schedule.
            for rid in &retry_ids {
                let Some(a) = map.get_mut(rid) else {
                    continue; // deleted between recovery and now
                };
                if !a.enabled {
                    continue; // paused/disabled since recovery — drop the retry
                }
                // Monitors are carved out of the readiness deferral
                // (headless-monitor-checks R6): a monitor retry dispatches
                // headless, so it neither waits on nor requeues behind the
                // frontend-ready gate.
                if a.mode.kind() == RunMode::Agent && !a.monitor && !frontend_ready {
                    requeue.push(rid.clone()); // defer until the frontend is up
                    continue;
                }
                if in_flight_widened(a, &alive, &headless_alive) {
                    continue; // a run is already in flight — the retry is moot
                }
                let run_id = mint_id();
                let keep = a.next_run_at;
                if a.claim(keep, now_ms, Trigger::Retry, &run_id).is_ok() {
                    to_dispatch_retry.push((a.clone(), run_id));
                    changed.push(a.id.clone());
                }
            }
        });

        // Re-queue retries deferred this tick (agent, frontend not yet ready).
        // Outside the store lock; the next tick retries them.
        if !requeue.is_empty() {
            let mut q = self.retry_queue.lock().unwrap();
            for id in requeue {
                q.push_back(id);
            }
        }

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
            // Same R2 discipline for retry claims: their Running rows live only
            // in memory on a flush failure, so defer their dispatch too (startup
            // recovery / ack-timeout closes them on the next launch/tick).
            to_dispatch_retry.clear();
        }

        // Monitor-handoff R7: monitors whose runs the sweep failed this tick
        // (ack timeout / deadline above; dispatch failures pushed below while
        // the claimed `Automation` — and its `.monitor` bit — is in hand, so
        // ordinary automations never reach the escalation's store re-fetch).
        // Evaluated in phase 3, deduped and outside the lock.
        let mut infra_failed = monitor_infra_failed;

        // Phase 2: the store lock is RELEASED — dispatch (KTD-B: the
        // load-bearing discipline; a Dispatcher may safely call back into
        // list()/get() from here).
        for (automation, run_id) in to_dispatch {
            if let Err(e) = self.dispatch(&automation, &run_id) {
                if automation.monitor {
                    infra_failed.push(automation.id.clone());
                }
                // R3: recompute from now — never restore the pre-claim value
                // (it could clobber a concurrent edit). Pure math outside the
                // lock, applied in a second short mutate. The not-before
                // floor clamps here too (monitor-handoff U2, R1).
                let recomputed = schedule::advance_from(
                    &automation.cron,
                    &automation.timezone,
                    now_ms,
                    automation.not_before_ms,
                )
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

        // Phase 2b (interrupt-resilience U2/R1): dispatch retry claims. A retry
        // consumes no occurrence, so a dispatch failure only closes the row
        // failed — it never recomputes `next_run_at` (unlike the scheduled path
        // above). Retry-once: a failed retry is not re-enqueued.
        for (automation, run_id) in to_dispatch_retry {
            if let Err(e) = self.dispatch(&automation, &run_id) {
                let _ = self.store.mutate(|map| {
                    if let Some(a) = map.get_mut(&automation.id) {
                        a.close(&run_id, failed(&e), now_ms);
                    }
                });
                if automation.monitor {
                    infra_failed.push(automation.id.clone());
                }
                // The automation is already in `changed` (the phase-1 retry
                // claim pushed it); only the run-closed emit is still owed so
                // the frontend tab lifecycle reacts to a failed agent retry.
                if automation.mode.kind() == RunMode::Agent {
                    closed_agent_runs.push((automation.id.clone(), run_id));
                }
            }
        }

        // Phase 2c (headless-monitor-checks U4 — R7): the backstop
        // kill-then-close for headless rows a dead runner thread abandoned.
        // Off the store lock (the kill sleeps a bounded grace, KTD-B). The
        // MONOTONIC gate is consulted per run right before the kill: the
        // epoch leg alone can lapse across a laptop suspend while the check
        // is healthy, so a closed gate (registry entry present, its
        // monotonic deadline not yet lapsed) skips both the kill and the
        // close this tick — the runner still owns the row. The kill strictly
        // precedes the close, so a later claim can never overlap a
        // still-alive child; the close routes through the shared tail
        // (run-closed emit + the Failed-close escalation), and a runner
        // close that raced us in lands as the benign AlreadyClosed no-op.
        if !headless_backstop.is_empty() {
            let gate = Arc::clone(&self.headless_deadline_gate.lock().unwrap());
            let killer = Arc::clone(&self.headless_killer.lock().unwrap());
            for (automation_id, run_id) in headless_backstop {
                if !gate(&run_id) {
                    continue; // suspend case: monotonic deadline not lapsed
                }
                killer(&run_id);
                self.close_headless_run(
                    &automation_id,
                    &run_id,
                    headless::CheckOutcome::Infra {
                        reason: ERR_HEADLESS_BACKSTOP.to_owned(),
                    },
                );
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
        // Monitor-handoff R7: broken-monitor escalation for every monitor
        // the sweep failed a run on this tick — deduped so one tick's
        // multiple closes on the same monitor ring at most once, and outside
        // the lock like every alert (KTD-B).
        infra_failed.sort();
        infra_failed.dedup();
        for id in infra_failed {
            self.check_monitor_escalation(&id);
        }
    }

    /// R5 shutdown half (called from `lifecycle::shutdown` after the sweep
    /// thread is joined): kill in-flight script groups via the U5 seam and
    /// in-flight headless checks via the headless-killer seam
    /// (headless-monitor-checks U4 — R5: the check's child is backend-owned,
    /// with nothing else left to reap it) — both outside the store lock
    /// (KTD-B) — then close every `Running` row failed([`ERR_INTERRUPTED`])
    /// in one final flush. Pane agents are never killed (their panes are the
    /// PTY reaper's job, later in the shutdown order).
    pub fn shutdown(&self) {
        let now = (self.clock)();
        let script_killer = Arc::clone(&self.script_killer.lock().unwrap());
        let headless_killer = Arc::clone(&self.headless_killer.lock().unwrap());
        // Kill first, with no store lock held (KTD-B): collect in-flight
        // run ids from a snapshot. The row-level `headless` marker picks the
        // killer — mode alone can't (a monitor is agent-mode).
        for a in self.store.snapshot().values() {
            for run in a.runs.iter().filter(|r| r.status == RunStatus::Running) {
                if run.headless {
                    headless_killer(&run.id);
                } else if a.mode.kind() == RunMode::Script {
                    script_killer(&run.id);
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

/// Agent rows past the R10 ack window with no pane ever linked. Headless
/// monitor-check rows are exempt (headless-monitor-checks plan, U2 — R7):
/// they never link a pane by design, so without the exclusion every check
/// running longer than the 30 s window would be force-failed.
fn ack_timed_out_agent_runs(a: &Automation, now_ms: u64) -> Vec<String> {
    a.runs
        .iter()
        .filter(|r| {
            r.status == RunStatus::Running
                && r.mode == RunMode::Agent
                && !r.headless
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
///
/// Headless monitor-check rows are exempt (headless-monitor-checks plan,
/// U2 — R7): the runner thread enforces the same deadline itself on a
/// MONOTONIC clock, while this sweep counts epoch time. Across a laptop
/// suspend the epoch age can lapse while the monotonic deadline has not, so
/// the sweep would close a healthy check first and the runner's later
/// verdict would land [`model::CloseResult::AlreadyClosed`] — silently
/// discarded. A dead runner thread is caught instead by the
/// [`headless_deadline_expired_runs`] backstop at deadline + slack.
fn deadline_expired_agent_runs(a: &Automation, now_ms: u64) -> Vec<String> {
    a.runs
        .iter()
        .filter(|r| {
            r.status == RunStatus::Running
                && r.mode == RunMode::Agent
                && !r.headless
                && r.started_at
                    .is_some_and(|t| t.saturating_add(RUN_DEADLINE_MS) <= now_ms)
        })
        .map(|r| r.id.clone())
        .collect()
}

/// Headless rows still `Running` past [`RUN_DEADLINE_MS`] +
/// [`HEADLESS_DEADLINE_SLACK_MS`] — the epoch leg of the R7 backstop
/// (headless-monitor-checks plan, U2). The runner's own monotonic deadline
/// close normally wins by the whole slack; a row this probe returns means
/// the runner thread died with its child possibly still alive, and the
/// sweep must kill-then-close so the orphan can't block the overlap probe
/// (or burn spend) forever. Consumed by [`AutomationManager::sweep_once`]'s
/// phase 2c (U4): the [`HeadlessKiller`] seam plus the in-flight registry's
/// monotonic gate ([`HeadlessDeadlineGate`]) — the entry's spawn `Instant`
/// must ALSO have lapsed (or the entry be gone) before the kill, so a
/// suspend longer than the slack never kills a healthy check. Saturating
/// arithmetic like its siblings: release builds have overflow checks off.
fn headless_deadline_expired_runs(a: &Automation, now_ms: u64) -> Vec<String> {
    a.runs
        .iter()
        .filter(|r| {
            r.status == RunStatus::Running
                && r.headless
                && r.started_at.is_some_and(|t| {
                    t.saturating_add(RUN_DEADLINE_MS)
                        .saturating_add(HEADLESS_DEADLINE_SLACK_MS)
                        <= now_ms
                })
        })
        .map(|r| r.id.clone())
        .collect()
}

/// R7 with the U7 widening: in flight = a `Running` row exists, or the probe
/// says a terminal agent row's linked pane is still alive (deadline-failed
/// but not done — a stuck agent must skip, not fan out).
///
/// Headless-monitor-checks U4 widens it again (its R7): a **terminal
/// headless row** whose in-flight registry entry still holds a live child —
/// a backstop/deadline kill that failed to stick — keeps the automation in
/// flight, the headless mirror of the stuck-pane clause. The row-existence
/// pre-check keeps the probe unconsulted for automations with no headless
/// history (every non-monitor); the probe itself is child-liveness, so a
/// dead child's stale entry never blocks (see [`HeadlessAliveProbe`]).
fn in_flight_widened(
    a: &Automation,
    alive: &PaneAliveProbe,
    headless_alive: &HeadlessAliveProbe,
) -> bool {
    a.in_flight()
        || a.runs
            .iter()
            .any(|r| r.status == RunStatus::Failed && r.pane_id.is_some() && alive(r))
        || (a.runs.iter().any(|r| r.headless && r.status.is_terminal())
            && headless_alive(&a.id))
}

/// [`schedule::advance_from`] with degraded fallbacks: an unparseable stored
/// cron/tz (same-UID file edits) or an exhausted schedule pauses the
/// automation (`None`) instead of wedging the sweep. The record's not-before
/// floor (monitor-handoff U2, R1) clamps every sweep-side recompute — claim,
/// pre-claim overlap skip, and capacity skip alike — so no path schedules
/// early.
fn advance_or_pause(a: &Automation, now_ms: u64) -> Option<u64> {
    match schedule::advance_from(&a.cron, &a.timezone, now_ms, a.not_before_ms) {
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

/// Monitor-handoff R14/R15: the one-line alert a verdict raises —
/// `monitor PASS: <note first line>` / `monitor FAIL: <note first line>` plus
/// the bundle suffix (path, or the could-not-be-written note). The sink's log
/// half sanitizes + caps at write time (alerts.rs R16), so only the note's
/// first line rides here; the full note lives on the run row / bundle.
fn verdict_alert_line(v: &Verdict, bundle_note: &str) -> String {
    let word = v.outcome.as_str();
    let first = v.note.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        format!("monitor {word}{bundle_note}")
    } else {
        format!("monitor {word}: {first}{bundle_note}")
    }
}

/// Monitor-handoff R15: generous byte cap on the bundle's evidence section —
/// far above the R8 8-KiB run-output tail (a bundle's whole point is escaping
/// it), aligned with the read path's display cap ([`BUNDLE_READ_CAP_BYTES`])
/// so a bundle on disk stays in the size class the fallback surface assumes.
/// Enforced at write time; without it a pathologically verbose final turn
/// would write an unbounded file.
const BUNDLE_EVIDENCE_CAP_BYTES: usize = 256 * 1024;

/// Cap `s` to its first `cap` bytes on a char boundary — the **head**
/// survives (fix(review) #11: a verdict note leads with its summary line,
/// unlike captured run output, whose verdict trails — see [`tail_capped`]).
/// Bounded arithmetic on in-range indices: the text is untrusted agent
/// output and release builds have overflow checks off.
fn head_capped(s: &str, cap: usize) -> &str {
    if s.len() <= cap {
        return s;
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Cap `s` to its last `cap` bytes on a char boundary (the tail survives —
/// the verdict block and failure narrative end the message, the same
/// convention as `model::output_tail`). Saturating arithmetic: the text is
/// captured agent output and release builds have overflow checks off.
fn tail_capped(s: &str, cap: usize) -> &str {
    if s.len() <= cap {
        return s;
    }
    let mut start = s.len().saturating_sub(cap);
    while start < s.len() && !s.is_char_boundary(start) {
        start = start.saturating_add(1);
    }
    &s[start..]
}

/// Monitor-handoff R15: write a failure bundle — `0600` file in a `0700` dir
/// via temp + rename (reusing the store's private-file primitives), under the
/// injected bundle dir. Called only after the close mutation releases the
/// store lock (KTD-B); errors surface to the caller, which degrades to the
/// "bundle could not be written" alert note (fail-tolerant).
fn write_bundle_file(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        store::create_private_dir(parent)?;
    }
    store::write_atomic_owner_only(path, content.as_bytes())
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
///
/// The two monitor fields (monitor-handoff U7, R18) carry the *derived*
/// broken-monitor inputs the raw list can't: `infra_failures` is the
/// per-monitor consecutive-infra-failure count
/// ([`Automation::consecutive_infra_failures`] — a method, so it never rides
/// the model's serialization) precomputed backend-side so the frontend
/// needn't re-derive the walk from run history, and
/// `monitor_broken_threshold` is [`verdict::MONITOR_BROKEN_THRESHOLD`]
/// carried on the wire so the frontend comparison can't drift from the one
/// Rust constant. Still a read-only projection.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomationsDashboard {
    pub automations: Vec<Automation>,
    /// Monitor id → derived consecutive-infra-failure count (monitors only).
    pub infra_failures: std::collections::HashMap<String, usize>,
    /// The R7 broken threshold, mirrored from [`verdict::MONITOR_BROKEN_THRESHOLD`].
    pub monitor_broken_threshold: usize,
    pub degraded: bool,
    pub corrupt_bak: Option<String>,
    pub flush_error: Option<String>,
}

/// Precompute each monitor's derived consecutive-infra-failure count
/// (monitor-handoff U7): the frontend gets the number, not the walk. Only
/// monitors appear — an ordinary automation has no broken state to derive.
fn monitor_infra_failures(
    automations: &[Automation],
) -> std::collections::HashMap<String, usize> {
    automations
        .iter()
        .filter(|a| a.monitor)
        .map(|a| (a.id.clone(), a.consecutive_infra_failures()))
        .collect()
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
    let automations = manager.list();
    let infra_failures = monitor_infra_failures(&automations);
    AutomationsDashboard {
        automations,
        infra_failures,
        monitor_broken_threshold: verdict::MONITOR_BROKEN_THRESHOLD,
        degraded: !health.is_ok(),
        corrupt_bak: health.corrupt_bak.map(|p| p.display().to_string()),
        flush_error: health.flush_error,
    }
}

/// R17 pickup validation (monitor-handoff U7): whether a failed monitor's
/// stored pickup pointers still resolve on disk — the transcript file and
/// the session cwd. A pure, read-only metadata check (two `stat`s, no file
/// contents), so the pickup button can decide spawn-vs-fallback without a
/// broken `claude` launch. Serde camelCase like every dashboard shape.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PickupCheck {
    pub transcript_exists: bool,
    pub cwd_exists: bool,
}

/// Check a pickup target's transcript + cwd existence (monitor-handoff U7,
/// R17). Read-only by construction: `Path::is_file` / `Path::is_dir` only.
#[tauri::command]
pub fn monitor_pickup_check(transcript_path: String, cwd: String) -> PickupCheck {
    PickupCheck {
        transcript_exists: std::path::Path::new(&transcript_path).is_file(),
        cwd_exists: std::path::Path::new(&cwd).is_dir(),
    }
}

/// Display cap for the R17 fallback surface: a bundle is the verdict note +
/// pickup pointers + one full captured turn, so 256 KiB covers any real one;
/// the head (verdict + pointers) is the useful part, so oversize truncates
/// the tail.
const BUNDLE_READ_CAP_BYTES: usize = 256 * 1024;

/// Read a monitor failure bundle's text for the dashboard's R17 fallback
/// (monitor-handoff U7): when pickup can't spawn (transcript/cwd gone), the
/// panel shows the raw bundle instead. Read-only, and **scoped**: the path
/// must canonicalize to a file inside the app's monitor-bundles dir — the
/// webview only ever echoes back a `bundlePath` the backend stamped on a run
/// row, so anything outside the bundle dir is a forged or corrupted path and
/// is refused, never read. Errors are one-line strings for the panel.
#[tauri::command]
pub fn read_monitor_bundle(
    manager: tauri::State<'_, Arc<AutomationManager>>,
    path: String,
) -> Result<String, String> {
    let dir = manager.bundle_dir.lock().unwrap().clone();
    read_bundle_scoped(dir.as_deref(), &path)
}

/// The scoped bundle read behind [`read_monitor_bundle`], split out so the
/// scope check is testable without Tauri state. Canonicalizes both sides so
/// `..` traversal and symlinks out of the bundle dir are refused.
fn read_bundle_scoped(
    bundle_dir: Option<&std::path::Path>,
    path: &str,
) -> Result<String, String> {
    let Some(dir) = bundle_dir else {
        return Err("no bundle directory is configured".to_string());
    };
    let dir = std::fs::canonicalize(dir).map_err(|e| format!("bundle dir unavailable: {e}"))?;
    let file = std::fs::canonicalize(path).map_err(|e| format!("bundle unreadable: {e}"))?;
    if !file.starts_with(&dir) {
        return Err("not a monitor bundle path".to_string());
    }
    let text =
        std::fs::read_to_string(&file).map_err(|e| format!("bundle unreadable: {e}"))?;
    if text.len() <= BUNDLE_READ_CAP_BYTES {
        return Ok(text);
    }
    // Truncate on a char boundary (the repo's release-overflow posture: all
    // arithmetic here is on in-range usizes).
    let mut end = BUNDLE_READ_CAP_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Ok(format!("{}\n… (truncated)", &text[..end]))
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
        /// Headless-monitor-checks U4 routing contract: the `monitor` flag
        /// visible on the `Automation` each `dispatch_agent` received — what
        /// lib.rs's CompositeDispatcher forks on (U5).
        agent_monitor: Mutex<Vec<bool>>,
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
            self.agent_monitor.lock().unwrap().push(a.monitor);
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
        /// Monitor-handoff U3: collected monitor alerts as
        /// `(automation name, alert line)` — the sink re-enters `list()` on
        /// every call (the FakeDispatcher::reenter pattern), structurally
        /// asserting alerts fire outside the store lock (KTD-B).
        monitor_alerts: Arc<Mutex<Vec<(String, String)>>>,
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
        // Monitor-handoff U3: collect verdict/broken alerts, re-entering
        // list() from inside the sink — safe exactly because alerts fire
        // after the store lock is released (KTD-B); a regression to
        // under-the-lock alerting deadlocks the non-reentrant store mutex.
        let monitor_alerts: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let ma = Arc::clone(&monitor_alerts);
        let sink_mgr = Arc::downgrade(&mgr);
        mgr.set_monitor_alert_sink(Arc::new(move |name: &str, line: &str| {
            if let Some(m) = sink_mgr.upgrade() {
                let _ = m.list();
            }
            ma.lock().unwrap().push((name.to_owned(), line.to_owned()));
        }));
        Harness {
            mgr,
            clock,
            events,
            dispatcher,
            run_closed,
            monitor_alerts,
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
            retry_on_interrupt: false,
            not_before_ms: None,
            monitor: false,
            pickup_pointers: None,
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

    // monitor-handoff U2, R1: a create carrying a future not-before floor
    // never schedules before it — the initial next_run_at is the first cron
    // occurrence at/after the floor (not the first from now), the floor is
    // persisted on the record, and the sweep stays quiet until it passes.
    #[test]
    fn create_with_future_not_before_clamps_initial_next_run_at_monitor_r1() {
        let h = harness();
        let nb = T0 + 3 * 24 * 60 * 60 * 1000; // three days out, boundary-aligned
        let created = h
            .mgr
            .create(CreateSpec {
                not_before_ms: Some(nb),
                ..script_spec("parked")
            })
            .unwrap();

        assert_eq!(
            created.automation.next_run_at,
            Some(nb + FIVE_MIN),
            "first */5 occurrence strictly after the floor, days past 'now'"
        );
        assert_eq!(
            created.automation.not_before_ms,
            Some(nb),
            "floor persisted on the record for every later recompute"
        );

        h.set_now(T0 + FIVE_MIN); // would be due without the floor
        h.sweep();
        assert_eq!(h.dispatcher.count(), 0, "parked — nothing fires before the floor");
        assert!(h.runs(&created.automation.id).is_empty());
    }

    // monitor-handoff U2, R1 (plan scenario 3): resuming a paused monitor
    // BEFORE its not-before must not schedule early — the resume recompute
    // clamps to the floor, not to now.
    #[test]
    fn resume_before_not_before_does_not_schedule_early_monitor_r1() {
        let h = harness();
        let nb = T0 + 24 * 60 * 60 * 1000; // tomorrow, boundary-aligned
        let id = h
            .mgr
            .create(CreateSpec {
                not_before_ms: Some(nb),
                ..script_spec("parked monitor")
            })
            .unwrap()
            .automation
            .id;
        h.mgr.pause(&id).unwrap();
        assert_eq!(h.next_run_at(&id), None, "paused = next_run_at nulled");

        h.set_now(T0 + FIVE_MIN); // still well before the floor
        let resumed = h.mgr.resume(&id).unwrap();
        assert_eq!(
            resumed.next_run_at,
            Some(nb + FIVE_MIN),
            "recomputed from the floor, not from now"
        );
    }

    // monitor-handoff U2, R1: the sweep-side claim recompute clamps too —
    // even a next_run_at forced BELOW the floor (degraded same-UID store
    // edit) re-arms at the first occurrence after the floor once it fires,
    // so no recompute path schedules early.
    #[test]
    fn sweep_claim_recompute_clamps_to_not_before_monitor_r1() {
        let h = harness();
        let nb = T0 + 24 * 60 * 60 * 1000;
        let id = h
            .mgr
            .create(CreateSpec {
                not_before_ms: Some(nb),
                ..script_spec("edited early")
            })
            .unwrap()
            .automation
            .id;
        // Force the degraded shape: due now despite the future floor.
        let _ = h.mgr.store.mutate(|map| {
            map.get_mut(&id).map(|a| a.next_run_at = Some(T0));
        });

        h.sweep(); // due → claim → advance_from clamps the re-arm
        assert_eq!(
            h.next_run_at(&id),
            Some(nb + FIVE_MIN),
            "re-armed at the first occurrence after the floor, never earlier"
        );
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
    // Extended for headless-monitor-checks U4 (its R5): an in-flight monitor
    // check is killed through the HEADLESS killer seam (its child is
    // backend-owned) and its row closes interrupted like the rest; the pane
    // agent alone stays unkilled.
    #[test]
    fn shutdown_kills_script_groups_and_closes_in_flight_rows_interrupted_r5() {
        let h = harness();
        h.mgr.set_frontend_ready();
        let script = h.mgr.create(script_spec("script")).unwrap().automation.id;
        let agent = h.mgr.create(agent_spec("agent")).unwrap().automation.id;
        let monitor = h.mgr.create(agent_spec("watch")).unwrap().automation.id;
        make_monitor(&h, &monitor);
        h.set_now(T0 + FIVE_MIN); // all due
        h.sweep(); // → all claim
        let script_run = h.runs(&script)[0].id.clone();
        let check_run = h.runs(&monitor)[0].id.clone();
        assert!(h.runs(&monitor)[0].headless, "the check row is headless");

        let killed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let k = Arc::clone(&killed);
        h.mgr
            .set_script_killer(Arc::new(move |rid: &str| k.lock().unwrap().push(rid.into())));
        let headless_killed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let hk = Arc::clone(&headless_killed);
        h.mgr
            .set_headless_killer(Arc::new(move |rid: &str| hk.lock().unwrap().push(rid.into())));

        h.mgr.shutdown();

        assert_eq!(
            *killed.lock().unwrap(),
            vec![script_run],
            "script group killed; the agent pane is never killed (R23/R5)"
        );
        assert_eq!(
            *headless_killed.lock().unwrap(),
            vec![check_run],
            "the headless check's child is killed through its own seam"
        );
        for id in [&script, &agent, &monitor] {
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

    // headless-monitor-checks U2 (R7): a monitor's claimed check row is
    // marked headless (through the real manager claim path) and both
    // pane-oriented sweep closes leave it alone — it never links a pane, so
    // the ack window would force-fail every check over 30 s, and the epoch
    // deadline would race the runner's monotonic one across a suspend.
    // Pane-less/pane-linked REGULAR agent rows stay governed as before.
    #[test]
    fn headless_check_rows_are_exempt_from_ack_and_deadline_sweep_closes() {
        let h = harness();
        h.mgr.set_frontend_ready();
        let id = h.mgr.create(agent_spec("train watch")).unwrap().automation.id;
        make_monitor(&h, &id);
        h.set_now(T0 + FIVE_MIN);
        h.sweep(); // claim (dispatch still rides the pane seam until U4)

        let t = T0 + FIVE_MIN;
        let claimed = h.runs(&id);
        let run_id = claimed[0].id.clone();
        assert!(claimed[0].headless, "monitor agent claim derives headless");
        assert_eq!(claimed[0].pane_id, None, "a check never links a pane");

        // Probe level: neither pane-oriented probe ever returns it.
        let a = h.mgr.get(&id).unwrap();
        assert!(
            ack_timed_out_agent_runs(&a, t + AGENT_ACK_TIMEOUT_MS + 1).is_empty(),
            "pane-less headless row is not ack-timed-out"
        );
        assert!(
            deadline_expired_agent_runs(&a, t + RUN_DEADLINE_MS + 1).is_empty(),
            "headless row is not deadline-closed by the sweep"
        );

        // Sweep level: past the ack window AND past the deadline the row is
        // still Running — no ERR_SPAWN_ACK, no ERR_TIMED_OUT close.
        h.set_now(t + AGENT_ACK_TIMEOUT_MS + 1);
        h.sweep();
        h.set_now(t + RUN_DEADLINE_MS + 1);
        h.sweep();
        let row = h
            .runs(&id)
            .into_iter()
            .find(|r| r.id == run_id)
            .expect("the check row survives");
        assert_eq!(row.status, RunStatus::Running, "exempt from both closes");
        assert!(h.run_closed.lock().unwrap().is_empty(), "no run-closed fired");

        // Regular agent rows keep the old rules: a pane-less one still hits
        // the ack window, and a pane-linked one is still deadline-returned.
        let h2 = harness();
        h2.mgr.set_frontend_ready();
        let plain = create_due(&h2, agent_spec("plain"));
        h2.sweep();
        let a2 = h2.mgr.get(&plain).unwrap();
        let run2 = a2.runs[0].id.clone();
        assert_eq!(
            ack_timed_out_agent_runs(&a2, T0 + FIVE_MIN + AGENT_ACK_TIMEOUT_MS),
            vec![run2.clone()],
            "pane-less regular agent row still ack-times-out"
        );
        h2.mgr.set_run_pane(&run2, 9).unwrap();
        let a2 = h2.mgr.get(&plain).unwrap();
        assert_eq!(
            deadline_expired_agent_runs(&a2, T0 + FIVE_MIN + RUN_DEADLINE_MS),
            vec![run2],
            "pane-linked regular agent row still deadline-expires"
        );
    }

    // headless-monitor-checks U2 (R7): the backstop probe returns Running
    // headless rows only past deadline + slack (inside the slack the
    // runner's own close wins), never terminal rows, and never regular
    // agent rows. This is the epoch leg only — U4 wires the kill-then-close
    // consumer plus the registry's monotonic suspend gate.
    #[test]
    fn headless_backstop_probe_fires_only_past_deadline_plus_slack() {
        let h = harness();
        h.mgr.set_frontend_ready();
        let id = h.mgr.create(agent_spec("train watch")).unwrap().automation.id;
        make_monitor(&h, &id);
        h.set_now(T0 + FIVE_MIN);
        h.sweep();
        let mut a = h.mgr.get(&id).unwrap();
        let run_id = a.runs[0].id.clone();
        assert!(a.runs[0].headless);

        let fire_at = T0 + FIVE_MIN + RUN_DEADLINE_MS + HEADLESS_DEADLINE_SLACK_MS;
        assert!(
            headless_deadline_expired_runs(&a, fire_at - 1).is_empty(),
            "inside the slack the runner still owns the close"
        );
        assert_eq!(
            headless_deadline_expired_runs(&a, fire_at),
            vec![run_id.clone()],
            "a dead runner's row is returned past deadline + slack"
        );

        // Terminal rows never fire — a closed check is out of the backstop's
        // jurisdiction no matter how old.
        a.close(&run_id, failed(ERR_TIMED_OUT), fire_at);
        assert!(headless_deadline_expired_runs(&a, fire_at + 1).is_empty());

        // Regular agent rows never fire — the existing deadline close (and
        // its pane alive-probe) governs them.
        let h2 = harness();
        h2.mgr.set_frontend_ready();
        let plain = create_due(&h2, agent_spec("plain"));
        h2.sweep();
        let a2 = h2.mgr.get(&plain).unwrap();
        assert!(headless_deadline_expired_runs(
            &a2,
            T0 + FIVE_MIN + RUN_DEADLINE_MS + HEADLESS_DEADLINE_SLACK_MS + 1,
        )
        .is_empty());
    }

    // ---- headless-monitor-checks U4: routing, shared close, gates, backstop --

    /// Create a monitor due at the first occurrence and sweep-claim its
    /// headless check — deliberately WITHOUT `set_frontend_ready` (the R6
    /// carve-out is part of the contract). Returns (automation id, run id).
    fn claim_headless_check(h: &Harness) -> (String, String) {
        let id = h.mgr.create(agent_spec("train watch")).unwrap().automation.id;
        make_monitor(h, &id);
        h.set_now(T0 + FIVE_MIN);
        h.sweep();
        let run = h.runs(&id).last().expect("claimed check").clone();
        assert_eq!(run.status, RunStatus::Running);
        assert!(run.headless, "a monitor's agent claim is headless (U2)");
        (id, run.id)
    }

    // headless-monitor-checks R1/R3 routing contract: a due monitor
    // dispatches through Dispatcher::dispatch_agent with the `monitor` flag
    // visible on the Automation it receives (lib.rs's CompositeDispatcher
    // forks on exactly that, U5) and the launch resolved + stamped on the
    // headless row (`set_run_launch` parity with the pane path) — all with
    // the frontend-ready gate down (R6). A regular agent automation
    // dispatches with monitor=false and a pane-shaped (non-headless) row.
    #[test]
    fn monitor_dispatch_rides_dispatch_agent_with_monitor_visible_and_launch_stamped() {
        let h = harness();
        let id = h
            .mgr
            .create(CreateSpec {
                mode: CreateMode::Agent {
                    prompt: "check the run".into(),
                    model: Some("opus".into()),
                    effort: Some("high".into()),
                },
                ..script_spec("train watch")
            })
            .unwrap()
            .automation
            .id;
        make_monitor(&h, &id);
        h.set_now(T0 + FIVE_MIN);
        h.sweep(); // gate deliberately never set (R6)

        assert_eq!(h.dispatcher.count(), 1, "dispatched through dispatch_agent");
        assert_eq!(
            *h.dispatcher.agent_monitor.lock().unwrap(),
            vec![true],
            "the monitor flag is visible to the dispatcher (the U5 fork point)"
        );
        let launches = h.dispatcher.agent_launches.lock().unwrap();
        assert_eq!(launches[0].model.as_deref(), Some("opus"));
        assert_eq!(launches[0].effort.as_deref(), Some("high"));
        let row = &h.runs(&id)[0];
        assert!(row.headless);
        assert_eq!(row.model.as_deref(), Some("opus"), "R3: stamped on the row");
        assert_eq!(row.effort.as_deref(), Some("high"));

        // A regular agent automation routes identically but monitor=false.
        let h2 = harness();
        h2.mgr.set_frontend_ready();
        let plain = create_due(&h2, agent_spec("plain"));
        h2.sweep();
        assert_eq!(*h2.dispatcher.agent_monitor.lock().unwrap(), vec![false]);
        assert!(!h2.runs(&plain)[0].headless);
    }

    // headless-monitor-checks R1: a manual run on a monitor routes through
    // dispatch_agent too (monitor visible), and the manual claim derives a
    // headless row like any other claim path.
    #[test]
    fn manual_run_on_a_monitor_dispatches_through_dispatch_agent() {
        let h = harness();
        let id = h.mgr.create(agent_spec("train watch")).unwrap().automation.id;
        make_monitor(&h, &id);

        let outcome = h.mgr.manual_run(&id).unwrap();

        assert!(matches!(outcome, ManualRun::Started { .. }));
        assert_eq!(h.dispatcher.count(), 1);
        assert_eq!(*h.dispatcher.agent_monitor.lock().unwrap(), vec![true]);
        let row = &h.runs(&id)[0];
        assert_eq!(row.trigger, Trigger::Manual);
        assert!(row.headless, "a manual monitor claim is headless too");
    }

    // headless-monitor-checks R9 (the "One shared verdict close path" KTD):
    // a headless close whose result text carries a FAIL verdict retires in
    // ONE mutation — verdict + retiredAt + bundle path — writes the bundle,
    // rings the alert sink, emits run-closed, and stamps the check's session
    // id (R12) in that same mutation: behavior-parity with the pane path's
    // verdict close, through the same tail.
    #[test]
    fn headless_fail_verdict_retires_bundles_alerts_and_stamps_session_id() {
        let h = harness();
        let bundles = h.dir.path().join("bundles");
        h.mgr.set_bundle_dir(bundles.clone());
        let (id, run_id) = claim_headless_check(&h);

        h.mgr.close_headless_run(
            &id,
            &run_id,
            headless::CheckOutcome::Clean {
                text: "Traceback: boom at train.py:88\n\n```verdict\nFAIL\nloss diverged\n```"
                    .into(),
                session_id: Some("sess-check".into()),
            },
        );

        let a = h.mgr.get(&id).unwrap();
        let row = a.runs.iter().find(|r| r.id == run_id).unwrap();
        assert_eq!(row.status, RunStatus::Succeeded, "the CHECK concluded fine");
        assert_eq!(row.verdict.as_ref().unwrap().outcome, VerdictOutcome::Fail);
        assert_eq!(
            row.session_id.as_deref(),
            Some("sess-check"),
            "R12: stamped in the same mutation as the verdict"
        );
        assert_eq!(a.retired_at, Some(T0 + FIVE_MIN), "retired with the close");
        assert_eq!(a.next_run_at, None, "scheduling stopped permanently");
        let bundle_path = row.bundle_path.clone().expect("row references the bundle");
        let content = std::fs::read_to_string(&bundle_path).expect("bundle written");
        assert!(content.contains("loss diverged"));
        assert!(
            content.contains("Traceback: boom at train.py:88"),
            "full evidence, pre-cap (R8)"
        );
        // U5 (R12): the check's own session rides the bundle as its labeled
        // block — id plus the transcript path derived from the automation's
        // cwd through the ONE encoding home (session::transcript, never
        // reimplemented). The spec cwd is "/tmp" → "-tmp".
        assert!(content.contains("## Check session"));
        assert!(content.contains("- sessionId: sess-check"));
        let enc = crate::session::transcript::encode_cwd("/tmp");
        assert!(
            content.contains(&format!("{enc}/sess-check.jsonl")),
            "the derived transcript path rides the bundle: {content}"
        );
        {
            let alerts = h.monitor_alerts.lock().unwrap();
            assert_eq!(alerts.len(), 1, "rings once through the alert sink");
            assert!(alerts[0].1.starts_with("monitor FAIL: loss diverged"));
        }
        assert_eq!(
            *h.run_closed.lock().unwrap(),
            vec![(run_id.clone(), RunStatus::Succeeded)],
            "run-closed emitted (a no-op for the tab-less frontend, R16)"
        );
        // Durable: verdict + retirement + session id survive a reload.
        let reloaded = store_in(&h.dir).get(&id).unwrap();
        assert!(reloaded.retired_at.is_some());
        let rrow = reloaded.runs.iter().find(|r| r.id == run_id).unwrap();
        assert_eq!(rrow.session_id.as_deref(), Some("sess-check"));
    }

    // headless-monitor-checks R8: the verdict parse sees the FULL cleaned
    // text BEFORE the tail cap. A PASS block that LEADS a text three times
    // the R8 cap still retires — a post-cap parse would have lost the block
    // to the tail — and the stored output is the capped tail. Cleaning ran
    // first: a control char in the text never reaches the row.
    #[test]
    fn headless_verdict_parses_full_cleaned_text_before_the_tail_cap_r8() {
        let h = harness();
        let (id, run_id) = claim_headless_check(&h);
        let text = format!(
            "```verdict\nPASS\ndone\n```\n\u{1b}[2J{}",
            "x".repeat(3 * model::OUTPUT_TAIL_CAP_BYTES)
        );

        h.mgr.close_headless_run(
            &id,
            &run_id,
            headless::CheckOutcome::Clean {
                text,
                session_id: None,
            },
        );

        let a = h.mgr.get(&id).unwrap();
        let row = a.runs.iter().find(|r| r.id == run_id).unwrap();
        assert_eq!(
            row.verdict.as_ref().unwrap().outcome,
            VerdictOutcome::Pass,
            "parsed from the full text, before capping"
        );
        assert!(a.retired_at.is_some());
        let output = row.output.as_ref().expect("output recorded");
        assert_eq!(
            output.len(),
            model::OUTPUT_TAIL_CAP_BYTES,
            "output tail-capped after the parse"
        );
        assert!(!output.contains('\u{1b}'), "sanitized before storage");
    }

    // headless-monitor-checks R8: cleaning order on the headless entry —
    // sanitize → scrub — for both the Clean result text (a secret token is
    // masked before the row) and the Infra reason (which may embed the raw
    // stderr tail: control chars stripped, then head-capped, the fly-authored
    // classification message surviving at the head).
    #[test]
    fn headless_close_sanitizes_and_scrubs_result_text_and_infra_reason_r8() {
        // Clean leg: a secret in the result text is masked on the row.
        let h = harness();
        let (id, run_id) = claim_headless_check(&h);
        h.mgr.close_headless_run(
            &id,
            &run_id,
            headless::CheckOutcome::Clean {
                text: "still training; found sk-abcdefghij0123456789 in env".into(),
                session_id: None,
            },
        );
        let output = h.runs(&id)[0].output.clone().expect("output recorded");
        assert!(!output.contains("sk-abcdefghij0123456789"), "{output}");
        assert!(output.contains("[redacted]"), "{output}");

        // Infra leg: control chars stripped, reason head-capped.
        let h2 = harness();
        let (id2, run2) = claim_headless_check(&h2);
        h2.mgr.close_headless_run(
            &id2,
            &run2,
            headless::CheckOutcome::Infra {
                reason: format!(
                    "exited 1 with no result event; stderr: \u{1b}[2J{}",
                    "y".repeat(3 * model::OUTPUT_TAIL_CAP_BYTES)
                ),
            },
        );
        let row = h2.runs(&id2)[0].clone();
        assert_eq!(row.status, RunStatus::Failed);
        let error = row.error.expect("reason recorded");
        assert!(!error.contains('\u{1b}'), "control chars never land");
        assert!(error.len() <= model::OUTPUT_TAIL_CAP_BYTES, "reason capped");
        assert!(
            error.starts_with("exited 1 with no result event"),
            "the classification message survives at the head: {error}"
        );
    }

    // headless-monitor-checks R9 escalation parity, infra leg: three
    // consecutive headless infra closes ring "monitor broken" exactly once,
    // and a readable not-done check resets the derived count.
    #[test]
    fn three_headless_infra_closes_ring_monitor_broken_once_then_reset() {
        let h = harness();
        let id = h.mgr.create(agent_spec("train watch")).unwrap().automation.id;
        make_monitor(&h, &id);

        for i in 1..=3u64 {
            h.set_now(T0 + i * FIVE_MIN);
            h.sweep();
            let run_id = h.runs(&id).last().unwrap().id.clone();
            h.mgr.close_headless_run(
                &id,
                &run_id,
                headless::CheckOutcome::Infra {
                    reason: "exited 1 with no result event".into(),
                },
            );
            if i < 3 {
                assert!(
                    h.monitor_alerts.lock().unwrap().is_empty(),
                    "below the threshold at {i}"
                );
            }
        }
        {
            let alerts = h.monitor_alerts.lock().unwrap();
            assert_eq!(alerts.len(), 1, "the third infra close rings once");
            assert!(alerts[0].1.starts_with("monitor broken:"), "{}", alerts[0].1);
        }

        // A readable not-done check resets the derived count, silently.
        h.set_now(T0 + 4 * FIVE_MIN);
        h.sweep();
        let run_id = h.runs(&id).last().unwrap().id.clone();
        h.mgr.close_headless_run(
            &id,
            &run_id,
            headless::CheckOutcome::Clean {
                text: "still training; nothing to report".into(),
                session_id: None,
            },
        );
        assert_eq!(h.mgr.get(&id).unwrap().consecutive_infra_failures(), 0);
        assert_eq!(h.monitor_alerts.lock().unwrap().len(), 1, "no new ring");
    }

    // headless-monitor-checks R9 escalation parity, Succeeded legs — these
    // live on the SHARED tail (plain close_run checks escalation only on
    // Failed): three verdict-less Succeeded headless closes — near-miss
    // fences and an empty-to-None result between them — ring "monitor
    // broken", never retire, and never stamp a verdict.
    #[test]
    fn three_headless_succeeded_unreadable_closes_ring_monitor_broken() {
        let h = harness();
        let id = h.mgr.create(agent_spec("train watch")).unwrap().automation.id;
        make_monitor(&h, &id);

        let texts = [
            "checked.\n```verdict\nPASS: decorated\n```", // near-miss opener
            "",                                           // empty → output None
            "```verdict\nnope\n```",                      // near-miss again
        ];
        for (i, text) in texts.iter().enumerate() {
            h.set_now(T0 + (i as u64 + 1) * FIVE_MIN);
            h.sweep();
            let run_id = h.runs(&id).last().unwrap().id.clone();
            h.mgr.close_headless_run(
                &id,
                &run_id,
                headless::CheckOutcome::Clean {
                    text: text.to_string(),
                    session_id: None,
                },
            );
        }

        let a = h.mgr.get(&id).unwrap();
        assert_eq!(a.retired_at, None, "near-misses never retire (abstain)");
        assert!(a.runs.iter().all(|r| r.verdict.is_none()));
        assert_eq!(a.consecutive_infra_failures(), 3);
        let alerts = h.monitor_alerts.lock().unwrap();
        assert_eq!(alerts.len(), 1, "the third unreadable Succeeded close rings");
        assert!(alerts[0].1.starts_with("monitor broken:"), "{}", alerts[0].1);
    }

    // headless-monitor-checks R9 escalation parity, dispatch-failure leg
    // (also proves R6 end-to-end: the gate is never set, yet the monitor
    // claims and its dispatch failures land): a failing dispatch seam closes
    // each claim failed and counts toward "monitor broken" — three ring once.
    #[test]
    fn monitor_dispatch_failures_close_failed_and_ring_broken_at_three() {
        let h = harness();
        let id = h.mgr.create(agent_spec("train watch")).unwrap().automation.id;
        make_monitor(&h, &id);
        *h.dispatcher.fail_with.lock().unwrap() = Some("boom".into());

        for i in 1..=3u64 {
            h.set_now(T0 + i * FIVE_MIN);
            h.sweep(); // gate deliberately never set (R6)
        }

        let a = h.mgr.get(&id).unwrap();
        assert_eq!(a.runs.len(), 3, "each occurrence claimed despite the gate");
        assert!(a
            .runs
            .iter()
            .all(|r| r.status == RunStatus::Failed && r.error.as_deref() == Some("boom")));
        assert_eq!(a.consecutive_infra_failures(), 3);
        let alerts = h.monitor_alerts.lock().unwrap();
        assert_eq!(alerts.len(), 1, "rings once at three");
        assert!(alerts[0].1.starts_with("monitor broken:"), "{}", alerts[0].1);
    }

    // headless-monitor-checks (U1's empty-text mapping, close half): a
    // Clean("") outcome closes Succeeded with output None — exact parity
    // with an abstained pane capture — and reads UNREADABLE in the derived
    // counter; the session id still lands on the ordinary (non-verdict) leg.
    #[test]
    fn empty_headless_result_closes_succeeded_output_none_and_reads_unreadable() {
        let h = harness();
        let (id, run_id) = claim_headless_check(&h);

        h.mgr.close_headless_run(
            &id,
            &run_id,
            headless::CheckOutcome::Clean {
                text: "".into(),
                session_id: Some("sess-empty".into()),
            },
        );

        let a = h.mgr.get(&id).unwrap();
        let row = a.runs.iter().find(|r| r.id == run_id).unwrap();
        assert_eq!(row.status, RunStatus::Succeeded);
        assert_eq!(row.output, None, "empty maps to None (pane-capture parity)");
        assert_eq!(
            row.session_id.as_deref(),
            Some("sess-empty"),
            "R12: stamped on the ordinary close leg too"
        );
        assert_eq!(a.retired_at, None);
        assert_eq!(
            a.consecutive_infra_failures(),
            1,
            "an output-less Succeeded row reads unreadable"
        );
        assert_eq!(
            *h.run_closed.lock().unwrap(),
            vec![(run_id, RunStatus::Succeeded)]
        );
    }

    // headless-monitor-checks R7 overlap widening: a TERMINAL headless row
    // whose registry entry still holds a live child (the probe says alive)
    // blocks the next scheduled claim (Skipped, in-flight reason) and makes
    // manual_run refuse — mirroring the stuck-pane alive-probe. Once the
    // child reads dead, claims resume.
    #[test]
    fn terminal_headless_row_with_live_child_blocks_claims_and_manual_runs_r7() {
        let h = harness();
        let (id, run_id) = claim_headless_check(&h);
        // The check's row goes terminal, but the child survived the kill.
        h.mgr.close_headless_run(
            &id,
            &run_id,
            headless::CheckOutcome::Infra {
                reason: ERR_HEADLESS_BACKSTOP.into(),
            },
        );
        h.mgr
            .set_headless_check_alive(Arc::new(|_automation_id: &str| true));

        let next = h.next_run_at(&id).expect("still scheduled");
        h.set_now(next);
        h.sweep();
        let runs = h.runs(&id);
        assert_eq!(runs.last().unwrap().status, RunStatus::Skipped);
        assert_eq!(runs.last().unwrap().error.as_deref(), Some(SKIP_IN_FLIGHT));
        assert_eq!(h.dispatcher.count(), 1, "no fan-out beside a live child");

        let outcome = h.mgr.manual_run(&id).unwrap();
        assert!(
            matches!(outcome, ManualRun::Skipped { .. }),
            "manual_run refuses too"
        );

        // The child finally dies: the probe reads dead, the schedule resumes.
        h.mgr
            .set_headless_check_alive(Arc::new(|_automation_id: &str| false));
        let next = h.next_run_at(&id).expect("re-armed past the skip");
        h.set_now(next);
        h.sweep();
        assert_eq!(h.dispatcher.count(), 2, "claims resume once the child is gone");
    }

    // headless-monitor-checks R6: with the frontend-ready gate down, a due
    // MONITOR is claimed and dispatched while a due regular agent automation
    // stays deferred un-claimed (its occurrence not burned) — the carve-out
    // exists because a headless check emits no agent-run event to drop.
    #[test]
    fn frontend_gate_down_monitor_claims_while_regular_agent_defers_r6() {
        let h = harness();
        let monitor = h.mgr.create(agent_spec("train watch")).unwrap().automation.id;
        make_monitor(&h, &monitor);
        let plain = h.mgr.create(agent_spec("plain")).unwrap().automation.id;
        h.set_now(T0 + FIVE_MIN); // both due; gate never set

        h.sweep();

        let mruns = h.runs(&monitor);
        assert_eq!(mruns.len(), 1, "monitor claimed with the gate down");
        assert_eq!(mruns[0].status, RunStatus::Running);
        assert!(mruns[0].headless);
        assert_eq!(h.dispatcher.count(), 1, "and dispatched");
        assert!(h.runs(&plain).is_empty(), "regular agent still deferred");
        assert_eq!(
            h.next_run_at(&plain),
            Some(T0 + FIVE_MIN),
            "the deferred occurrence is not burned"
        );
    }

    // headless-monitor-checks R6, retry leg: an interrupted monitor's
    // one-shot retry claims and dispatches on the FIRST sweep with the gate
    // down — never requeued behind frontend-ready (contrast
    // agent_retry_defers_until_frontend_ready_r5 for regular agents).
    #[test]
    fn monitor_retry_dispatches_with_the_gate_down_and_never_requeues_r6() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = store_in(&dir);
            store
                .mutate(|map| {
                    let mut a = raw_automation("m1");
                    a.monitor = true;
                    a.retry_on_interrupt = true;
                    a.mode = agent_mode();
                    a.claim(Some(T0 + 2 * FIVE_MIN), T0 + FIVE_MIN, Trigger::Schedule, "r1")
                        .unwrap();
                    map.insert(a.id.clone(), a);
                })
                .unwrap();
        }
        let h = harness_in(dir);

        h.sweep(); // gate never set

        assert_eq!(h.dispatcher.count(), 1, "retry dispatched immediately");
        let runs = h.runs("m1");
        assert_eq!(runs.len(), 2, "failed original + running retry");
        assert_eq!(runs[1].trigger, Trigger::Retry);
        assert_eq!(runs[1].status, RunStatus::Running);
        assert!(runs[1].headless, "the retry claim derives headless too");
        assert!(
            h.mgr.retry_queue.lock().unwrap().is_empty(),
            "nothing requeued on the gate"
        );
    }

    // headless-monitor-checks R7 backstop: a Running headless row past
    // deadline + slack is kill-then-closed ONLY when the monotonic gate
    // agrees. Gate closed (the suspend case: epoch lapsed, monotonic not) →
    // neither kill nor close; gate open → the killer fires strictly BEFORE
    // the Failed close (it observes the row still Running — and reads the
    // store, structurally asserting the kill runs off the store lock,
    // KTD-B), then the row closes with the backstop reason through the
    // shared tail: run-closed emitted, escalation counted.
    #[test]
    fn backstop_kills_then_closes_only_when_the_monotonic_gate_agrees_r7() {
        let h = harness();
        let (id, run_id) = claim_headless_check(&h);
        let killed: Arc<Mutex<Vec<(String, RunStatus)>>> = Arc::new(Mutex::new(Vec::new()));
        let k = Arc::clone(&killed);
        let probe_mgr = Arc::downgrade(&h.mgr);
        let (aid, rid) = (id.clone(), run_id.clone());
        h.mgr.set_headless_killer(Arc::new(move |run_id: &str| {
            let status = probe_mgr
                .upgrade()
                .and_then(|m| m.get(&aid))
                .and_then(|a| a.runs.iter().find(|r| r.id == rid).map(|r| r.status))
                .expect("the row exists at kill time");
            k.lock().unwrap().push((run_id.to_owned(), status));
        }));

        // Suspend case: epoch age lapsed, monotonic deadline not.
        h.mgr.set_headless_deadline_gate(Arc::new(|_run_id: &str| false));
        h.set_now(T0 + FIVE_MIN + RUN_DEADLINE_MS + HEADLESS_DEADLINE_SLACK_MS);
        h.sweep();
        assert!(killed.lock().unwrap().is_empty(), "closed gate: no kill");
        assert_eq!(
            h.runs(&id)[0].status,
            RunStatus::Running,
            "and no close — the runner still owns the row"
        );

        // Gate opens (monotonic lapsed / entry gone): kill, then close.
        h.mgr.set_headless_deadline_gate(Arc::new(|_run_id: &str| true));
        h.sweep();
        assert_eq!(
            *killed.lock().unwrap(),
            vec![(run_id.clone(), RunStatus::Running)],
            "killer fired exactly once, BEFORE the close"
        );
        let a = h.mgr.get(&id).unwrap();
        let row = a.runs.iter().find(|r| r.id == run_id).unwrap();
        assert_eq!(row.status, RunStatus::Failed);
        assert_eq!(row.error.as_deref(), Some(ERR_HEADLESS_BACKSTOP));
        assert_eq!(
            a.consecutive_infra_failures(),
            1,
            "a backstop close counts toward broken"
        );
        assert!(
            h.run_closed
                .lock()
                .unwrap()
                .iter()
                .any(|(r, s)| *r == run_id && *s == RunStatus::Failed),
            "run-closed emitted through the shared tail"
        );
    }

    // headless-monitor-checks R5: deleting an automation mid-check invokes
    // the HEADLESS killer for the check's run (the script killer stays
    // silent — a monitor is agent-mode) and closes the row deleted on the
    // removed record.
    #[test]
    fn delete_mid_check_invokes_headless_killer_and_closes_deleted_r5() {
        let h = harness();
        let (id, run_id) = claim_headless_check(&h);
        let killed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let k = Arc::clone(&killed);
        h.mgr
            .set_headless_killer(Arc::new(move |rid: &str| k.lock().unwrap().push(rid.into())));
        let script_killed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sk = Arc::clone(&script_killed);
        h.mgr
            .set_script_killer(Arc::new(move |rid: &str| sk.lock().unwrap().push(rid.into())));

        let removed = h.mgr.delete(&id).unwrap();

        assert_eq!(*killed.lock().unwrap(), vec![run_id.clone()]);
        assert!(script_killed.lock().unwrap().is_empty(), "not a script kill");
        let row = removed.runs.iter().find(|r| r.id == run_id).unwrap();
        assert_eq!(row.status, RunStatus::Failed);
        assert_eq!(row.error.as_deref(), Some(ERR_DELETED));
        assert!(h.mgr.get(&id).is_none(), "record removed");
    }

    // headless-monitor-checks retirement race: a verdict close landing on a
    // monitor a concurrent path already retired takes the existing
    // idempotent branch — the fetched record's `retired_at` gates the parse,
    // so the row closes as an ordinary Succeeded check: no verdict stamped,
    // no re-ring, the original retirement stamp untouched.
    #[test]
    fn headless_verdict_close_on_an_already_retired_monitor_is_the_idempotent_no_op() {
        let h = harness();
        let (id, run_id) = claim_headless_check(&h);
        // Concurrent retirement while the check runs (stamped directly, the
        // make_monitor pattern — a retired monitor refuses new claims, so
        // the Running-row + retired pair only arises from this race).
        let _ = h.mgr.store.mutate(|map| {
            map.get_mut(&id).unwrap().retired_at = Some(T0);
        });

        h.mgr.close_headless_run(
            &id,
            &run_id,
            headless::CheckOutcome::Clean {
                text: "```verdict\nPASS\ndone\n```".into(),
                session_id: None,
            },
        );

        let a = h.mgr.get(&id).unwrap();
        assert_eq!(a.retired_at, Some(T0), "original stamp untouched");
        let row = a.runs.iter().find(|r| r.id == run_id).unwrap();
        assert_eq!(row.status, RunStatus::Succeeded, "the row still closes");
        assert_eq!(row.verdict, None, "no verdict stamped past retirement");
        assert!(row.bundle_path.is_none());
        assert!(h.monitor_alerts.lock().unwrap().is_empty(), "nothing rings");
    }

    // headless-monitor-checks: an unknown automation id (deleted mid-check)
    // makes close_headless_run a calm no-op — the reaper-side convention.
    #[test]
    fn close_headless_run_on_a_deleted_automation_is_a_no_op() {
        let h = harness();
        h.mgr.close_headless_run(
            "ghost",
            "r1",
            headless::CheckOutcome::Clean {
                text: "```verdict\nPASS\ndone\n```".into(),
                session_id: None,
            },
        );
        assert!(h.monitor_alerts.lock().unwrap().is_empty());
        assert!(h.run_closed.lock().unwrap().is_empty());
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
            retry_on_interrupt: false,
            monitor: false,
            not_before_ms: None,
            retired_at: None,
            pickup_pointers: None,
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

    // ---- interrupt-resilience (U1/U2) ----------------------------------------

    fn script_mode() -> Mode {
        Mode::Script {
            script_file: "script".into(),
            interpreter: "bash".into(),
            timeout_ms: 120_000,
        }
    }

    fn agent_mode() -> Mode {
        Mode::Agent {
            prompt: "audit".into(),
            model: None,
            effort: None,
        }
    }

    /// Seed a store dir with one automation a prior app run left mid-flight: a
    /// `Running` row claimed with `trigger` (persisted per R2), plus the retry
    /// opt-in and mode under test.
    fn seed_interrupted(
        dir: &tempfile::TempDir,
        retry_on_interrupt: bool,
        mode: Mode,
        trigger: Trigger,
    ) {
        let store = store_in(dir);
        store
            .mutate(|map| {
                let mut a = raw_automation("a1");
                a.retry_on_interrupt = retry_on_interrupt;
                a.mode = mode;
                a.claim(Some(T0 + 2 * FIVE_MIN), T0 + FIVE_MIN, trigger, "r1")
                    .unwrap();
                map.insert(a.id.clone(), a);
            })
            .unwrap();
    }

    /// A collecting interrupt sink: records `(automation_id, retry_eligible)`
    /// per surfaced interrupt.
    #[allow(clippy::type_complexity)]
    fn collecting_sink() -> (InterruptSink, Arc<Mutex<Vec<(String, bool)>>>) {
        let collected: Arc<Mutex<Vec<(String, bool)>>> = Arc::new(Mutex::new(Vec::new()));
        let c = Arc::clone(&collected);
        let sink: InterruptSink = Arc::new(move |ir: &InterruptedRun| {
            c.lock()
                .unwrap()
                .push((ir.automation_id.clone(), ir.is_retry_eligible()));
        });
        (sink, collected)
    }

    // U2/R1/R2: an opted-in automation's interrupted run is surfaced through the
    // sink AND re-dispatched once as a Trigger::Retry run (a script needs no
    // frontend-ready gate). The retry consumes no occurrence (scheduled_for
    // None). Idempotent: a second sweep neither re-alerts nor re-dispatches.
    #[test]
    fn interrupted_run_alerts_and_opt_in_retries_as_trigger_retry_u2() {
        let dir = tempfile::tempdir().unwrap();
        seed_interrupted(&dir, true, script_mode(), Trigger::Schedule);
        let h = harness_in(dir);
        let (sink, alerts) = collecting_sink();
        h.mgr.set_interrupt_sink(sink);

        // Recovery closed the original row failed(interrupted).
        assert_eq!(h.runs("a1")[0].status, RunStatus::Failed);
        assert_eq!(h.runs("a1")[0].error.as_deref(), Some(ERR_INTERRUPTED));

        h.sweep();
        assert_eq!(
            alerts.lock().unwrap().as_slice(),
            &[("a1".to_string(), true)],
            "surfaced once, retry-eligible"
        );
        assert_eq!(h.dispatcher.count(), 1, "retry dispatched");
        let runs = h.runs("a1");
        assert_eq!(runs.len(), 2, "failed original + running retry");
        assert_eq!(runs[1].trigger, Trigger::Retry);
        assert_eq!(runs[1].status, RunStatus::Running);
        assert_eq!(runs[1].scheduled_for, None, "a retry consumes no occurrence");

        h.sweep();
        assert_eq!(alerts.lock().unwrap().len(), 1, "backlog drained — no re-alert");
        assert_eq!(h.dispatcher.count(), 1, "retry already in flight — no re-dispatch");
    }

    // U2/R1: without the opt-in, an interrupted run is still surfaced but never
    // re-dispatched (the curve-read case — money agents must not silently rerun).
    #[test]
    fn interrupt_without_opt_in_alerts_but_never_retries_u2() {
        let dir = tempfile::tempdir().unwrap();
        seed_interrupted(&dir, false, script_mode(), Trigger::Schedule);
        let h = harness_in(dir);
        let (sink, alerts) = collecting_sink();
        h.mgr.set_interrupt_sink(sink);

        h.sweep();
        assert_eq!(
            alerts.lock().unwrap().as_slice(),
            &[("a1".to_string(), false)],
            "surfaced, not eligible"
        );
        assert_eq!(h.dispatcher.count(), 0, "opt-out never retries");
        assert_eq!(h.runs("a1").len(), 1, "no retry row appended");
    }

    // U2/R4 retry-once crash-loop guard: a run BORN from a retry that is itself
    // interrupted is surfaced but never retried again.
    #[test]
    fn a_retry_run_interrupted_again_alerts_but_never_retries_again_r4() {
        let dir = tempfile::tempdir().unwrap();
        seed_interrupted(&dir, true, script_mode(), Trigger::Retry);
        let h = harness_in(dir);
        let (sink, alerts) = collecting_sink();
        h.mgr.set_interrupt_sink(sink);

        h.sweep();
        assert_eq!(
            alerts.lock().unwrap().as_slice(),
            &[("a1".to_string(), false)],
            "retry-once: a re-run's own interrupt is not eligible"
        );
        assert_eq!(h.dispatcher.count(), 0, "no second retry");
        assert_eq!(h.runs("a1").len(), 1);
    }

    // U2/R5: an agent retry honors the frontend-ready gate — it alerts
    // immediately but defers the re-dispatch (re-queued) until the frontend is
    // up, then fires on the next sweep.
    #[test]
    fn agent_retry_defers_until_frontend_ready_r5() {
        let dir = tempfile::tempdir().unwrap();
        seed_interrupted(&dir, true, agent_mode(), Trigger::Schedule);
        let h = harness_in(dir);
        let (sink, alerts) = collecting_sink();
        h.mgr.set_interrupt_sink(sink);

        h.sweep();
        assert_eq!(alerts.lock().unwrap().len(), 1, "alert fires regardless of readiness");
        assert_eq!(h.dispatcher.count(), 0, "agent retry waits for the frontend (R5)");
        assert_eq!(h.runs("a1").len(), 1, "no retry row yet");

        h.mgr.set_frontend_ready();
        h.sweep();
        assert_eq!(alerts.lock().unwrap().len(), 1, "not re-alerted on the second tick");
        assert_eq!(h.dispatcher.count(), 1, "agent retry now dispatched");
        assert_eq!(h.runs("a1")[1].trigger, Trigger::Retry);
    }

    // ---- monitor-handoff U3: verdict close, retire, bundle, escalation ------

    /// Flip a created automation into a monitor with pickup pointers. U4/U5
    /// thread these through the create path; U3 tests stamp them directly
    /// (this child module sees the manager's private store).
    fn make_monitor(h: &Harness, id: &str) {
        let _ = h.mgr.store.mutate(|map| {
            let a = map.get_mut(id).expect("automation exists");
            a.monitor = true;
            a.pickup_pointers = Some(model::MonitorPointers {
                session_id: "sess-9".into(),
                transcript_path: "/home/u/.claude/projects/x/sess-9.jsonl".into(),
                session_cwd: "/home/u/exp".into(),
            });
        });
    }

    /// Sweep-claim the due run and link it to `pane_id`; returns the run id.
    fn claim_and_link(h: &Harness, id: &str, pane_id: u64) -> String {
        h.sweep();
        let run_id = h.runs(id).last().expect("a claimed row").id.clone();
        h.mgr.set_run_pane(&run_id, pane_id).unwrap();
        run_id
    }

    /// One full infra-failure check cycle at the current clock: claim, link,
    /// then the pane dies before Stop (R6: an infrastructure failure).
    fn infra_fail_cycle(h: &Harness, id: &str, pane_id: u64) {
        let _ = claim_and_link(h, id, pane_id);
        h.mgr.close_run_by_pane_failed(pane_id).unwrap();
    }

    /// Retire a monitor through the real verdict path: one due check whose
    /// captured turn carries a PASS block. Assumes a fresh harness clock
    /// (moves it to the first occurrence) and `make_monitor` already ran.
    fn retire_via_pass_verdict(h: &Harness, id: &str, pane_id: u64) {
        h.mgr.set_frontend_ready();
        h.set_now(T0 + FIVE_MIN);
        let _ = claim_and_link(h, id, pane_id);
        h.mgr.set_output_capturer(Arc::new(|_cwd: &str, _since: u64| {
            Some("```verdict\nPASS\ndone\n```".to_string())
        }));
        h.mgr.close_run_by_pane(pane_id).unwrap();
        assert!(h.mgr.get(id).unwrap().retired_at.is_some(), "retired");
    }

    // Monitor-handoff R3 (fix(review) #2): resume() refuses a retired
    // monitor — retirement is permanent, so the schedule is never re-armed
    // (a re-arm would set the sweep re-claiming, and being refused, every
    // tick forever).
    #[test]
    fn resume_refuses_a_retired_monitor_and_never_rearms_r3() {
        let h = harness();
        let id = h.mgr.create(agent_spec("train watch")).unwrap().automation.id;
        make_monitor(&h, &id);
        retire_via_pass_verdict(&h, &id, 42);

        let err = h
            .mgr
            .resume(&id)
            .expect_err("resume on a retired monitor refuses");
        assert!(err.contains("retired"), "the refusal names retirement: {err}");
        assert_eq!(h.next_run_at(&id), None, "schedule NOT re-armed (R3)");

        // And the sweep keeps finding nothing due — no grinding claims.
        h.set_now(T0 + 10 * FIVE_MIN);
        h.sweep();
        assert_eq!(h.runs(&id).len(), 1, "no new rows after the refused resume");
    }

    // Monitor-handoff R3 defense in depth (fix(review) #2): a degraded
    // due-but-retired record (retire() forbids the pair — an on-disk edit or
    // a historic re-arm produced it) self-heals: the sweep's refused claim
    // nulls the schedule instead of re-claiming and being refused every tick
    // forever.
    #[test]
    fn sweep_self_heals_a_due_but_retired_record_r3() {
        let h = harness();
        let id = h.mgr.create(agent_spec("train watch")).unwrap().automation.id;
        make_monitor(&h, &id);
        h.mgr.set_frontend_ready();
        // Degrade: retired yet still scheduled, stamped directly the way a
        // hand-edited store file would present it.
        let _ = h.mgr.store.mutate(|map| {
            map.get_mut(&id).unwrap().retired_at = Some(T0);
        });
        h.set_now(T0 + FIVE_MIN); // the stale next_run_at is now due
        h.sweep();

        assert_eq!(h.dispatcher.count(), 0, "a retired monitor never dispatches");
        assert_eq!(h.next_run_at(&id), None, "the sweep nulled the degraded schedule");
        assert!(h.runs(&id).is_empty(), "no row appended — the claim was refused");

        // Healed: the next tick has nothing due and touches nothing.
        h.set_now(T0 + 2 * FIVE_MIN);
        h.sweep();
        assert_eq!(h.dispatcher.count(), 0);
    }

    // Monitor-handoff R3 (fix(review) #8): manual_run on a retired monitor
    // reports the retirement — permanent, never-runs-again — not the generic
    // (resumable) "disabled" refusal. The manager-layer mirror of the model's
    // claim_rejects_a_retired_monitor_for_schedule_and_manual_triggers.
    #[test]
    fn manual_run_on_a_retired_monitor_reports_retirement_r3() {
        let h = harness();
        let id = h.mgr.create(agent_spec("train watch")).unwrap().automation.id;
        make_monitor(&h, &id);
        retire_via_pass_verdict(&h, &id, 42);

        let err = h.mgr.manual_run(&id).expect_err("refused");
        assert_eq!(err, ERR_RUN_RETIRED, "the retirement-specific message");
        assert_eq!(
            h.runs(&id).len(),
            1,
            "nothing appended by the refused manual claim"
        );
    }

    // Monitor-handoff R2/R3/R14 (+ R4 verification): a Stop whose captured
    // final turn carries a PASS block closes the row Succeeded, stamps the
    // verdict, retires the monitor, and clears next_run_at — all observable
    // after ONE close call — then rings the alert (after the lock: the sink
    // re-enters list()) with the note; no bundle for a PASS. The retirement
    // and verdict survive a store reload, and the sweep never claims again.
    #[test]
    fn monitor_pass_verdict_retires_in_one_close_and_alerts_r2_r3_r14() {
        let h = harness();
        let id = h.mgr.create(agent_spec("train watch")).unwrap().automation.id;
        make_monitor(&h, &id);
        h.mgr.set_frontend_ready();
        h.set_now(T0 + FIVE_MIN);
        let run_id = claim_and_link(&h, &id, 42);
        h.mgr.set_output_capturer(Arc::new(|_cwd: &str, _since: u64| {
            Some(
                "Checked the run as instructed.\n\n\
                 ```verdict\nPASS\nconverged at step 800\n```\n"
                    .to_string(),
            )
        }));

        h.mgr.close_run_by_pane(42).unwrap();

        let a = h.mgr.get(&id).unwrap();
        let row = a.runs.iter().find(|r| r.id == run_id).unwrap();
        assert_eq!(row.status, RunStatus::Succeeded);
        assert_eq!(
            row.verdict,
            Some(Verdict {
                outcome: VerdictOutcome::Pass,
                note: "converged at step 800".into(),
            })
        );
        assert_eq!(row.bundle_path, None, "a PASS writes no bundle");
        assert!(row.output.is_some(), "captured output still recorded (R8)");
        assert_eq!(a.retired_at, Some(T0 + FIVE_MIN), "retired with the close");
        assert_eq!(a.next_run_at, None, "scheduling stopped permanently (R3)");

        // R14: exactly one alert, carrying the note.
        assert_eq!(
            *h.monitor_alerts.lock().unwrap(),
            vec![(
                "train watch".to_string(),
                "monitor PASS: converged at step 800".to_string()
            )]
        );
        // The frontend tab lifecycle still hears the close.
        assert_eq!(
            *h.run_closed.lock().unwrap(),
            vec![(run_id.clone(), RunStatus::Succeeded)]
        );

        // R3: the retired monitor never claims again.
        h.set_now(T0 + 10 * FIVE_MIN);
        h.sweep();
        assert_eq!(h.runs(&id).len(), 1, "no new claims after retirement");

        // R4: verdict + retirement are durable across a store reload.
        let reloaded = store_in(&h.dir).get(&id).expect("persisted");
        assert_eq!(reloaded.retired_at, Some(T0 + FIVE_MIN));
        assert_eq!(
            reloaded.runs.iter().find(|r| r.id == run_id).unwrap().verdict,
            row.verdict
        );
    }

    // Monitor-handoff R15: a FAIL verdict writes the durable bundle — verdict
    // note + pickup pointers + the FULL captured turn (outside the R8 tail
    // cap) — references it from the run row, retires, and rings with the
    // bundle path in the alert line.
    #[test]
    fn monitor_fail_verdict_writes_bundle_with_pointers_and_evidence_r15() {
        let h = harness();
        let bundles = h.dir.path().join("bundles");
        h.mgr.set_bundle_dir(bundles.clone());
        let id = h.mgr.create(agent_spec("train watch")).unwrap().automation.id;
        make_monitor(&h, &id);
        h.mgr.set_frontend_ready();
        h.set_now(T0 + FIVE_MIN);
        let run_id = claim_and_link(&h, &id, 42);
        h.mgr.set_output_capturer(Arc::new(|_cwd: &str, _since: u64| {
            Some(
                "Traceback (most recent call last): boom at train.py:88\n\n\
                 ```verdict\nFAIL\nloss diverged\n```"
                    .to_string(),
            )
        }));

        h.mgr.close_run_by_pane(42).unwrap();

        let a = h.mgr.get(&id).unwrap();
        let row = a.runs.iter().find(|r| r.id == run_id).unwrap();
        assert_eq!(
            row.status,
            RunStatus::Succeeded,
            "the CHECK ran fine — the FAIL is the experiment's verdict"
        );
        assert_eq!(row.verdict.as_ref().unwrap().outcome, VerdictOutcome::Fail);
        assert!(a.retired_at.is_some(), "a FAIL verdict retires too (R3)");

        let bundle_path = row.bundle_path.clone().expect("row references the bundle");
        assert_eq!(
            std::path::PathBuf::from(&bundle_path),
            bundles.join(format!("{id}-{run_id}.md"))
        );
        let content = std::fs::read_to_string(&bundle_path).expect("bundle file exists");
        assert!(content.contains("loss diverged"), "verdict note: {content}");
        assert!(content.contains("sess-9"), "pickup pointers ride the bundle");
        assert!(content.contains("/home/u/.claude/projects/x/sess-9.jsonl"));
        assert!(content.contains("/home/u/exp"));
        assert!(
            content.contains("Traceback (most recent call last): boom at train.py:88"),
            "the full evidence text, not the tail cap"
        );
        // U5 (R12): a pane-path close carries no check session id — the
        // Check-session block is omitted entirely, never rendered empty.
        assert!(!content.contains("Check session"));

        let alerts = h.monitor_alerts.lock().unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].0, "train watch");
        assert!(alerts[0].1.starts_with("monitor FAIL: loss diverged"));
        assert!(
            alerts[0].1.contains(&bundle_path),
            "the alert carries the bundle path: {}",
            alerts[0].1
        );
    }

    // fix(review) #11 (R8 cap discipline × R15): the verdict note is
    // untrusted agent output — a multi-megabyte note is HEAD-capped at stamp
    // time on the run row (the head carries the summary line), while the
    // FAIL bundle still records the full note (the bundle's whole point is
    // escaping the row caps). The name embedded in the bundle is sanitized
    // like the alerts log (R16).
    #[test]
    fn huge_verdict_note_is_head_capped_on_the_row_but_full_in_the_bundle_r8_r15() {
        let h = harness();
        let bundles = h.dir.path().join("bundles");
        h.mgr.set_bundle_dir(bundles.clone());
        // A control char in the name must not reach the rendered bundle.
        let id = h
            .mgr
            .create(agent_spec("train \u{1b}[2Jwatch"))
            .unwrap()
            .automation
            .id;
        make_monitor(&h, &id);
        h.mgr.set_frontend_ready();
        h.set_now(T0 + FIVE_MIN);
        let run_id = claim_and_link(&h, &id, 42);
        // ~3 MiB note behind a one-line summary.
        let big_note = format!("loss diverged\n{}", "x".repeat(3 * 1024 * 1024));
        let note_in = big_note.clone();
        h.mgr.set_output_capturer(Arc::new(move |_cwd: &str, _since: u64| {
            Some(format!("```verdict\nFAIL\n{note_in}\n```"))
        }));

        h.mgr.close_run_by_pane(42).unwrap();

        let a = h.mgr.get(&id).unwrap();
        let row = a.runs.iter().find(|r| r.id == run_id).unwrap();
        let stamped = &row.verdict.as_ref().expect("verdict stamped").note;
        assert_eq!(
            stamped.len(),
            model::OUTPUT_TAIL_CAP_BYTES,
            "row note capped to the R8 size class"
        );
        assert!(
            big_note.starts_with(stamped.as_str()),
            "the HEAD survives — the summary line leads the note"
        );

        let content =
            std::fs::read_to_string(row.bundle_path.as_ref().expect("bundle written")).unwrap();
        assert!(
            content.contains(&big_note),
            "the bundle records the FULL note (R15)"
        );
        assert!(
            !content.contains('\u{1b}'),
            "control chars in the name never reach the bundle (R16)"
        );
    }

    // Monitor-handoff R5 (AE6): a check with no recognizable verdict block —
    // or one whose capture abstains — ends silently: row Succeeded with no
    // verdict, monitor stays parked and scheduled, nothing rings.
    #[test]
    fn monitor_check_without_verdict_stays_scheduled_and_silent_r5() {
        let h = harness();
        let id = h.mgr.create(agent_spec("train watch")).unwrap().automation.id;
        make_monitor(&h, &id);
        h.mgr.set_frontend_ready();
        h.set_now(T0 + FIVE_MIN);
        let run_id = claim_and_link(&h, &id, 42);
        h.mgr.set_output_capturer(Arc::new(|_cwd: &str, _since: u64| {
            Some("still training — 3 epochs to go; nothing to report".to_string())
        }));

        h.mgr.close_run_by_pane(42).unwrap();

        let a = h.mgr.get(&id).unwrap();
        let row = a.runs.iter().find(|r| r.id == run_id).unwrap();
        assert_eq!(row.status, RunStatus::Succeeded);
        assert_eq!(row.verdict, None, "a not-done check carries no verdict");
        assert_eq!(row.bundle_path, None);
        assert_eq!(a.retired_at, None, "monitor stays parked");
        assert_eq!(
            a.next_run_at,
            Some(T0 + 2 * FIVE_MIN),
            "the schedule continues"
        );
        assert!(
            h.monitor_alerts.lock().unwrap().is_empty(),
            "nothing rings (R5)"
        );
        assert_eq!(
            a.consecutive_infra_failures(),
            0,
            "a readable not-done check never escalates (it resets)"
        );

        // Capture abstention (busy cwd — the capturer returns None) is
        // equally verdict-less and equally silent here, but it COUNTS toward
        // the R7 escalation (the U3 refinement) — one abstention is below
        // the threshold, so still no ring.
        h.mgr
            .set_output_capturer(Arc::new(|_cwd: &str, _since: u64| None));
        h.set_now(T0 + 2 * FIVE_MIN);
        let run2 = claim_and_link(&h, &id, 43);
        h.mgr.close_run_by_pane(43).unwrap();
        let a = h.mgr.get(&id).unwrap();
        assert_eq!(a.runs.iter().find(|r| r.id == run2).unwrap().verdict, None);
        assert_eq!(a.retired_at, None);
        assert!(h.monitor_alerts.lock().unwrap().is_empty());
        assert_eq!(
            a.consecutive_infra_failures(),
            1,
            "the abstained check counts, below the threshold"
        );
    }

    // Monitor-handoff R7, the U3 refinement (plan Risks: "Transcript capture
    // abstains in busy cwds … Escalation bounds it"): a monitor whose output
    // can never be attributed — every check concludes but its capture
    // abstains, so no verdict can ever be read — escalates like an infra
    // failure instead of running silent forever. Three consecutive abstained
    // checks ring "monitor broken" once; a later readable not-done check
    // resets the count and stays silent.
    #[test]
    fn three_consecutive_capture_abstentions_ring_monitor_broken_r7() {
        let h = harness();
        let id = h.mgr.create(agent_spec("train watch")).unwrap().automation.id;
        make_monitor(&h, &id);
        h.mgr.set_frontend_ready();
        // Busy cwd: the capturer abstains on every check.
        h.mgr
            .set_output_capturer(Arc::new(|_cwd: &str, _since: u64| None));

        for i in 1..=2u64 {
            h.set_now(T0 + i * FIVE_MIN);
            let _ = claim_and_link(&h, &id, 400 + i);
            h.mgr.close_run_by_pane(400 + i).unwrap();
        }
        assert!(
            h.monitor_alerts.lock().unwrap().is_empty(),
            "two abstentions: below the threshold, still silent"
        );

        h.set_now(T0 + 3 * FIVE_MIN);
        let _ = claim_and_link(&h, &id, 403);
        h.mgr.close_run_by_pane(403).unwrap();
        {
            let alerts = h.monitor_alerts.lock().unwrap();
            assert_eq!(alerts.len(), 1, "the third unreadable check rings");
            assert!(
                alerts[0].1.starts_with("monitor broken:"),
                "{}",
                alerts[0].1
            );
        }
        let a = h.mgr.get(&id).unwrap();
        assert_eq!(a.retired_at, None, "broken is not retired");
        assert!(a.next_run_at.is_some(), "the schedule continues");

        // A readable not-done check (capture returns text, no verdict block)
        // resets the count and is silent — the healthy long-experiment case.
        h.mgr.set_output_capturer(Arc::new(|_cwd: &str, _since: u64| {
            Some("still training; nothing to report".to_string())
        }));
        h.set_now(T0 + 4 * FIVE_MIN);
        let _ = claim_and_link(&h, &id, 404);
        h.mgr.close_run_by_pane(404).unwrap();
        assert_eq!(
            h.monitor_alerts.lock().unwrap().len(),
            1,
            "a readable not-done check is silent"
        );
        assert_eq!(
            h.mgr.get(&id).unwrap().consecutive_infra_failures(),
            0,
            "and it resets the derived count"
        );
    }

    // Monitor-handoff R6/R7 (AE5): three consecutive verdict-less failed
    // checks ring "monitor broken" exactly once — the fourth stays silent, a
    // clean check resets the derived count, and a fresh streak of three
    // rings again (the post-alert reset). The monitor is never retired and
    // the schedule continues throughout; no bundle exists.
    #[test]
    fn three_consecutive_infra_failures_ring_monitor_broken_once_then_reset_r6_r7() {
        let h = harness();
        let id = h.mgr.create(agent_spec("train watch")).unwrap().automation.id;
        make_monitor(&h, &id);
        h.mgr.set_frontend_ready();

        for i in 1..=2u64 {
            h.set_now(T0 + i * FIVE_MIN);
            infra_fail_cycle(&h, &id, 100 + i);
        }
        assert!(
            h.monitor_alerts.lock().unwrap().is_empty(),
            "two failures: below the threshold"
        );

        h.set_now(T0 + 3 * FIVE_MIN);
        infra_fail_cycle(&h, &id, 103);
        assert_eq!(
            *h.monitor_alerts.lock().unwrap(),
            vec![(
                "train watch".to_string(),
                "monitor broken: 3 consecutive checks failed without a verdict".to_string()
            )],
            "the third consecutive failure rings exactly once"
        );
        let a = h.mgr.get(&id).unwrap();
        assert_eq!(a.retired_at, None, "broken is not retired (AE5)");
        assert!(a.next_run_at.is_some(), "the schedule continues (AE5)");
        assert!(
            a.runs.iter().all(|r| r.verdict.is_none()),
            "infra failures never carry a verdict (R6)"
        );
        assert!(
            a.runs.iter().all(|r| r.bundle_path.is_none()),
            "no failure bundle exists (AE5)"
        );

        // A fourth failure does not re-ring — no per-failure alert storm.
        h.set_now(T0 + 4 * FIVE_MIN);
        infra_fail_cycle(&h, &id, 104);
        assert_eq!(h.monitor_alerts.lock().unwrap().len(), 1);

        // A clean check — Succeeded with CAPTURED output but no verdict (a
        // readable not-done check) — resets the derived count…
        h.mgr.set_output_capturer(Arc::new(|_cwd: &str, _since: u64| {
            Some("still training — nothing to report".to_string())
        }));
        h.set_now(T0 + 5 * FIVE_MIN);
        let _ = claim_and_link(&h, &id, 105);
        h.mgr.close_run_by_pane(105).unwrap();
        assert_eq!(
            h.monitor_alerts.lock().unwrap().len(),
            1,
            "a clean check is silent (R5) and resets (R7)"
        );

        // …so a fresh streak of three rings again.
        for i in 6..=8u64 {
            h.set_now(T0 + i * FIVE_MIN);
            infra_fail_cycle(&h, &id, 100 + i);
        }
        assert_eq!(
            h.monitor_alerts.lock().unwrap().len(),
            2,
            "three NEW failures after the reset ring again"
        );
    }

    // fix(review) #14 (KTD-B × monitor-handoff R12): a create whose store
    // flush fails still succeeds — the record is live in memory — but
    // reports the outcome via the TYPED `flush_ok` flag: the warning string
    // is overloaded with the R1 min-gap advisory, so callers (the socket
    // create arm's monitor-registered gate) must never string-match it.
    #[test]
    fn create_under_a_failing_flush_returns_ok_with_flush_ok_false_r12() {
        use std::os::unix::fs::PermissionsExt;
        let h = harness();
        let ok = h.mgr.create(agent_spec("healthy")).unwrap();
        assert!(ok.flush_ok, "a flushed create reports flush_ok");

        // Failure injection (the store.rs pattern): remove the store dir and
        // make its parent read-only, so the flush's create_dir_all fails.
        let data = h.dir.path().join("data");
        std::fs::remove_dir_all(&data).unwrap();
        std::fs::set_permissions(h.dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        if std::fs::create_dir(h.dir.path().join("probe")).is_ok() {
            // Running as root (or an ACL overrides the mode): the injection
            // cannot work — skip gracefully, like the store.rs tests.
            std::fs::set_permissions(h.dir.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
            eprintln!("skipping flush-failure test: read-only dir is still writable");
            return;
        }
        let created = h.mgr.create(agent_spec("degraded")).expect("create still succeeds");
        std::fs::set_permissions(h.dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(!created.flush_ok, "the failed flush reads as a typed flag");
        assert!(
            created
                .warning
                .as_deref()
                .unwrap_or("")
                .contains("store flush failed"),
            "the CLI-facing warning still rides: {:?}",
            created.warning
        );
        assert!(
            h.mgr.get(&created.automation.id).is_some(),
            "the record is live in memory (KTD-B)"
        );
    }

    // Review testing gap (delete racing the verdict close): a monitor check
    // is claimed + linked, the automation is deleted mid-check, then the
    // Stop lands with a verdict-bearing capturer. Both the pane-keyed close
    // (the snapshot finds nothing) and the retiring close's own mutation
    // (simulating the snapshot winning the race — the record fetched before
    // the delete) take the documented benign branch: Ok(()), no panic, no
    // alert, no bundle, nothing resurrected.
    #[test]
    fn delete_racing_a_verdict_close_is_a_benign_no_op() {
        let h = harness();
        let bundles = h.dir.path().join("bundles");
        h.mgr.set_bundle_dir(bundles.clone());
        let id = h.mgr.create(agent_spec("train watch")).unwrap().automation.id;
        make_monitor(&h, &id);
        h.mgr.set_frontend_ready();
        h.set_now(T0 + FIVE_MIN);
        let run_id = claim_and_link(&h, &id, 42);
        h.mgr.set_output_capturer(Arc::new(|_cwd: &str, _since: u64| {
            Some("```verdict\nFAIL\ndied\n```".to_string())
        }));
        let pre_delete = h.mgr.get(&id).expect("fetched before the delete");

        let removed = h.mgr.delete(&id).expect("delete succeeds");
        assert_eq!(
            removed.runs.last().unwrap().status,
            RunStatus::Failed,
            "delete closed the in-flight row on the removed record (R23)"
        );

        // The Stop lands after the delete: the snapshot finds no running run.
        assert_eq!(h.mgr.close_run_by_pane(42), Ok(()));
        // And the tighter interleaving — snapshot before the delete, close
        // mutation after it — hits close_monitor_run_retiring's NotFound arm.
        // (Reworked for headless-monitor-checks U4: the retiring close gained
        // a session-id parameter — `None` here, the pane path's value.)
        assert_eq!(
            h.mgr.close_monitor_run_retiring(
                &pre_delete,
                &run_id,
                RunOutcome::Succeeded {
                    output: Some("```verdict\nFAIL\ndied\n```".into()),
                },
                Verdict {
                    outcome: VerdictOutcome::Fail,
                    note: "died".into(),
                },
                "evidence",
                None,
            ),
            Ok(())
        );

        assert!(h.mgr.get(&id).is_none(), "nothing resurrected");
        assert!(h.monitor_alerts.lock().unwrap().is_empty(), "no alert rings");
        assert!(!bundles.exists(), "no bundle file was written");
    }

    // Monitor-handoff R7 (fix(review) #5 — the plan's Risks promise:
    // escalation converts persistent non-compliance into a visible broken
    // signal): a check that keeps emitting a NEAR-MISS verdict block — an
    // opened ```verdict fence that never parses (decorated outcome here) —
    // neither retires (abstain-on-surprise, R2) nor resets the derived
    // count: three in a row ring "monitor broken" exactly like infra
    // failures, evaluated at the close that produced the third.
    #[test]
    fn three_consecutive_near_miss_verdict_blocks_ring_monitor_broken_r7() {
        let h = harness();
        let id = h.mgr.create(agent_spec("train watch")).unwrap().automation.id;
        make_monitor(&h, &id);
        h.mgr.set_frontend_ready();
        h.mgr.set_output_capturer(Arc::new(|_cwd: &str, _since: u64| {
            Some("checked it.\n```verdict\nPASS: all good\n```".to_string())
        }));

        for i in 1..=2u64 {
            h.set_now(T0 + i * FIVE_MIN);
            let _ = claim_and_link(&h, &id, 500 + i);
            h.mgr.close_run_by_pane(500 + i).unwrap();
        }
        assert!(
            h.monitor_alerts.lock().unwrap().is_empty(),
            "two near-misses: below the threshold, still silent"
        );
        let a = h.mgr.get(&id).unwrap();
        assert_eq!(a.retired_at, None, "a near-miss never retires (R2 abstention)");
        assert!(a.runs.iter().all(|r| r.verdict.is_none()));

        h.set_now(T0 + 3 * FIVE_MIN);
        let _ = claim_and_link(&h, &id, 503);
        h.mgr.close_run_by_pane(503).unwrap();
        let alerts = h.monitor_alerts.lock().unwrap();
        assert_eq!(alerts.len(), 1, "the third consecutive near-miss rings");
        assert!(alerts[0].1.starts_with("monitor broken:"), "{}", alerts[0].1);
    }

    // Monitor-handoff R6: a check that hangs to the R11 deadline closes
    // failed(timed out) — never a verdict — and counts toward the broken
    // escalation. Since headless-monitor-checks U2 the SWEEP no longer
    // closes it (the claimed row is headless — deadline-exempt; the runner
    // enforces the deadline itself, U3, with the U4 backstop behind it), so
    // the timeout close arrives through the manager's close entry exactly
    // as the runner's will, and `close_run` evaluates the escalation on the
    // Failed close.
    #[test]
    fn deadline_timed_out_check_counts_toward_broken_escalation_r6() {
        let h = harness();
        let id = h.mgr.create(agent_spec("train watch")).unwrap().automation.id;
        make_monitor(&h, &id);
        h.mgr.set_frontend_ready();
        // Two pane-death failures…
        for i in 1..=2u64 {
            h.set_now(T0 + i * FIVE_MIN);
            infra_fail_cycle(&h, &id, 200 + i);
        }
        // …then a check that hangs: the deadline sweep leaves the headless
        // row alone (the U2 exemption)…
        h.set_now(T0 + 3 * FIVE_MIN);
        h.sweep();
        let run_id = h.runs(&id).last().expect("claimed row").id.clone();
        h.set_now(T0 + 3 * FIVE_MIN + RUN_DEADLINE_MS);
        h.sweep();
        {
            let a = h.mgr.get(&id).unwrap();
            let row = a.runs.iter().find(|r| r.id == run_id).unwrap();
            assert!(row.headless, "monitor claim is headless (U2)");
            assert_eq!(row.status, RunStatus::Running, "sweep-exempt at the deadline");
        }
        // …and the runner's own timeout close reports back (U3 owns this).
        assert_eq!(
            h.mgr.close_run(&id, &run_id, failed(ERR_TIMED_OUT)),
            model::CloseResult::Closed
        );

        let a = h.mgr.get(&id).unwrap();
        let row = a.runs.iter().find(|r| r.id == run_id).unwrap();
        assert_eq!(row.status, RunStatus::Failed);
        assert_eq!(row.error.as_deref(), Some(ERR_TIMED_OUT));
        assert_eq!(row.verdict, None, "a timeout is never a verdict (R6)");
        let alerts = h.monitor_alerts.lock().unwrap();
        assert_eq!(alerts.len(), 1, "third consecutive infra failure rings");
        assert!(alerts[0].1.starts_with("monitor broken:"), "{}", alerts[0].1);
    }

    // Monitor-handoff R15 fail-tolerance (mirrors flush_tolerant): a bundle
    // write failure never blocks the verdict — the row still closes with the
    // verdict stamped and the monitor retires; the alert notes the missing
    // bundle instead of carrying a path.
    #[test]
    fn bundle_write_failure_still_closes_and_retires_and_alert_notes_it_r15() {
        let h = harness();
        // A regular FILE occupies the bundle dir's parent path, so the
        // bundle write's create_dir_all fails deterministically.
        let blocker = h.dir.path().join("blocker");
        std::fs::write(&blocker, b"not a dir").unwrap();
        h.mgr.set_bundle_dir(blocker.join("bundles"));
        let id = h.mgr.create(agent_spec("train watch")).unwrap().automation.id;
        make_monitor(&h, &id);
        h.mgr.set_frontend_ready();
        h.set_now(T0 + FIVE_MIN);
        let run_id = claim_and_link(&h, &id, 42);
        h.mgr.set_output_capturer(Arc::new(|_cwd: &str, _since: u64| {
            Some("```verdict\nFAIL\ndied\n```".to_string())
        }));

        h.mgr.close_run_by_pane(42).unwrap();

        let a = h.mgr.get(&id).unwrap();
        let row = a.runs.iter().find(|r| r.id == run_id).unwrap();
        assert_eq!(row.verdict.as_ref().unwrap().outcome, VerdictOutcome::Fail);
        assert!(a.retired_at.is_some(), "close and retire still land");
        let intended = row.bundle_path.as_ref().expect("intended path recorded");
        assert!(
            !std::path::Path::new(intended).exists(),
            "no file could be written"
        );
        let alerts = h.monitor_alerts.lock().unwrap();
        assert_eq!(alerts.len(), 1);
        assert!(
            alerts[0].1.contains("bundle could not be written"),
            "{}",
            alerts[0].1
        );
    }

    // Monitor-handoff R7 × interrupt-resilience: an app restart that
    // interrupts the third consecutive failing check still escalates — the
    // first post-restart sweep tick (which drains startup recovery's
    // backlog) evaluates the derived count and rings broken.
    #[test]
    fn restart_interrupted_third_failure_escalates_on_first_sweep_r7() {
        let h = harness();
        let id = h.mgr.create(agent_spec("train watch")).unwrap().automation.id;
        make_monitor(&h, &id);
        h.mgr.set_frontend_ready();
        for i in 1..=2u64 {
            h.set_now(T0 + i * FIVE_MIN);
            infra_fail_cycle(&h, &id, 300 + i);
        }
        // The third check claims and is left Running — app dies mid-check.
        h.set_now(T0 + 3 * FIVE_MIN);
        let _ = claim_and_link(&h, &id, 303);
        assert!(h.mgr.get(&id).unwrap().in_flight());
        assert!(h.monitor_alerts.lock().unwrap().is_empty());

        // "Restart": a fresh manager over the same store dir. Recovery closes
        // the orphan Running row failed(interrupted); the first sweep tick
        // surfaces the backlog and runs the R7 check with the count at three.
        let Harness { dir, .. } = h;
        let h2 = harness_in(dir);
        h2.mgr.set_frontend_ready();
        h2.set_now(T0 + 4 * FIVE_MIN);
        h2.sweep();

        let alerts = h2.monitor_alerts.lock().unwrap();
        assert_eq!(
            alerts
                .iter()
                .filter(|(_, l)| l.starts_with("monitor broken:"))
                .count(),
            1,
            "the interrupted third check rings broken post-restart: {alerts:?}"
        );
    }

    // Monitor-handoff U7 (R18): the dashboard DTO's precomputed infra-failure
    // map covers monitors only and mirrors the model's derived walk, so the
    // frontend never re-derives it from run history.
    #[test]
    fn monitor_infra_failures_maps_monitors_only_u7() {
        fn bare(id: &str, monitor: bool) -> Automation {
            Automation {
                id: id.into(),
                name: id.into(),
                cron: "*/5 * * * *".into(),
                timezone: "UTC".into(),
                enabled: true,
                retry_on_interrupt: false,
                monitor,
                not_before_ms: None,
                retired_at: None,
                pickup_pointers: None,
                cwd: "/tmp".into(),
                mode: Mode::Agent { prompt: "p".into(), model: None, effort: None },
                origin: Origin {
                    pane_id: 1,
                    workspace_id: "ws-1".into(),
                    label: "cli".into(),
                },
                created_at: T0,
                updated_at: T0,
                next_run_at: Some(T0),
                runs: Vec::new(),
            }
        }
        // Both carry one verdict-less Failed row — an infra failure for the
        // monitor's derived walk, meaningless for the plain automation.
        let fail = || RunOutcome::Failed {
            error: "x".into(),
            exit_code: None,
            output: None,
        };
        let mut plain = bare("a1", false);
        plain.claim(Some(T0), T0, Trigger::Schedule, "r1").unwrap();
        plain.close("r1", fail(), T0);
        let mut mon = bare("a2", true);
        mon.claim(Some(T0), T0, Trigger::Schedule, "r1").unwrap();
        mon.close("r1", fail(), T0);

        let map = monitor_infra_failures(&[plain, mon.clone()]);
        assert_eq!(map.len(), 1, "non-monitors never appear");
        assert_eq!(map.get("a2"), Some(&mon.consecutive_infra_failures()));
        assert_eq!(map.get("a2"), Some(&1));
    }

    // Monitor-handoff U7 (R17): the fallback bundle read is scoped to the
    // bundle dir — inside reads, everything else (outside paths, `..`
    // traversal, missing files, unwired dir) refuses without touching disk
    // contents.
    #[test]
    fn read_bundle_scoped_reads_inside_and_refuses_outside_u7() {
        let dir = tempfile::tempdir().unwrap();
        let bundles = dir.path().join("monitor-bundles");
        std::fs::create_dir_all(&bundles).unwrap();
        let inside = bundles.join("a1-r1.md");
        std::fs::write(&inside, "verdict: FAIL\nevidence").unwrap();
        let outside = dir.path().join("secret.txt");
        std::fs::write(&outside, "nope").unwrap();

        // Inside the dir: reads.
        assert_eq!(
            read_bundle_scoped(Some(&bundles), inside.to_str().unwrap()).unwrap(),
            "verdict: FAIL\nevidence"
        );
        // Outside the dir: refused.
        assert!(read_bundle_scoped(Some(&bundles), outside.to_str().unwrap())
            .unwrap_err()
            .contains("not a monitor bundle path"));
        // `..` traversal out of the dir: canonicalization catches it.
        let sneaky = bundles.join("../secret.txt");
        assert!(read_bundle_scoped(Some(&bundles), sneaky.to_str().unwrap())
            .unwrap_err()
            .contains("not a monitor bundle path"));
        // Missing file / unwired dir: one-line errors, never a panic.
        let gone = bundles.join("missing.md");
        assert!(read_bundle_scoped(Some(&bundles), gone.to_str().unwrap()).is_err());
        assert!(read_bundle_scoped(None, inside.to_str().unwrap()).is_err());
    }

    // Oversize bundles truncate at the display cap on a char boundary, keeping
    // the head (verdict + pointers — the useful part).
    #[test]
    fn read_bundle_scoped_truncates_oversize_at_cap_u7() {
        let dir = tempfile::tempdir().unwrap();
        let bundles = dir.path().join("monitor-bundles");
        std::fs::create_dir_all(&bundles).unwrap();
        let big = bundles.join("big.md");
        // Multibyte content so the char-boundary walk is exercised.
        let content = "é".repeat(BUNDLE_READ_CAP_BYTES); // 2 bytes each → 2× the cap
        std::fs::write(&big, &content).unwrap();

        let text = read_bundle_scoped(Some(&bundles), big.to_str().unwrap()).unwrap();
        assert!(text.ends_with("… (truncated)"));
        assert!(text.len() <= BUNDLE_READ_CAP_BYTES + 32);
        assert!(text.starts_with('é'), "head kept");
    }
}
