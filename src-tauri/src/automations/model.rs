//! Pure domain model for automations: the [`Automation`] record, its bounded
//! run history ([`RunRow`], R8), and the run-row state machine (U1 of
//! `docs/plans/2026-07-01-002-feat-automations-plan.md`).
//!
//! Everything here is pure vocabulary — no I/O and no wall-clock reads; time
//! arrives as `now_ms` arguments (the [`crate::state::attention`] shape), so
//! every transition is testable without a running app. Ids are short unique
//! strings minted by the manager (U4), never here.
//!
//! Transition producers and edges (the run-row state machine):
//!
//! - [`Automation::claim`] — sweep or manual claim → appends a `Running` row
//!   and overwrites `next_run_at` with the caller-supplied advanced value
//!   (R1's clamp and R2's persist-before-run live in the schedule/store/
//!   manager units, not here).
//! - [`Automation::skip`] — the pre-claim overlap/capacity skip (R7, KTD-D)
//!   → appends a **terminal** `Skipped` row. There is deliberately no
//!   `Running → Skipped` edge: a claimed row can only close
//!   `Succeeded`/`Failed` ([`RunOutcome`] cannot express a skip).
//! - [`Automation::close`] — terminal close of a `Running` row. Terminal rows
//!   never transition again: closing one is a no-op returning
//!   [`CloseResult::AlreadyClosed`].
//! - [`Automation::rollback_recompute`] — R3: dispatch failure overwrites
//!   `next_run_at` with a freshly *recomputed* occurrence, never the
//!   pre-claim value (which could clobber a concurrent edit).
//!
//! Interrupted / timed-out / deleted runs are `Failed` with the distinct
//! error strings below — no separate statuses in v1 (R5/R23).
//!
//! Monitor vocabulary (U1 of
//! `docs/plans/2026-07-10-002-feat-monitor-handoff-plan.md` — IDs below that
//! cite the monitor-handoff plan say so explicitly): the
//! `monitor`/`not_before_ms`/`retired_at`/[`MonitorPointers`] fields on
//! [`Automation`], the per-run [`Verdict`] + `bundle_path`, the
//! [`Automation::retire`] transition (monitor-handoff R3), and the derived
//! consecutive-infra-failure count (monitor-handoff R6/R7). Same purity
//! rules: no I/O, no clocks — retirement and counting are testable
//! transitions over plain data.
//!
//! Headless-check vocabulary (U2 of
//! `docs/plans/2026-07-11-003-feat-headless-monitor-checks-plan.md`): the
//! per-run `headless` marker on [`RunRow`], derived inside
//! [`Automation::claim`] (R7), and the check's `session_id` (R12). Plain
//! data again — the sweep exemptions the marker drives live in `mod.rs`.

use serde::{Deserialize, Serialize};

/// R8: an automation keeps its last 20 run rows; older rows evict
/// oldest-first.
pub const RUN_HISTORY_CAP: usize = 20;

/// R8: stored run output is capped to an 8 KiB **tail** — the end survives,
/// because a script's verdict is its trailing lines (R15's sentinel).
pub const OUTPUT_TAIL_CAP_BYTES: usize = 8 * 1024;

/// R5: error string for runs closed by startup recovery or app shutdown.
pub const ERR_INTERRUPTED: &str = "interrupted";
/// R11: error string for agent runs closed at the run deadline (pane alive).
pub const ERR_TIMED_OUT: &str = "timed out";
/// R23: error string for open rows closed by automation delete.
pub const ERR_DELETED: &str = "deleted";

/// What a due automation executes (R1). Serde-tagged like
/// [`crate::state::lifecycle::LifecycleState`] (`"kind"` discriminator).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Mode {
    /// Run `claude "<prompt>"` in the automation's cwd (R9) — by default as
    /// a closed-loop headless `claude -p` child, or in a fresh pane when the
    /// resolved disposition is paned (see `headless` below).
    /// `model`/`effort` (automations-workspace-and-model plan, U1 —
    /// R9/R10) optionally pin the launch model + reasoning effort; they are
    /// resolved deterministically at dispatch (automation → shared default →
    /// Claude's own default, U4a) and always passed explicitly so a run never
    /// inherits the last interactive pick. `#[serde(default)]` is per-field —
    /// the `Mode` enum carries no container default, so a legacy `{kind,
    /// prompt}` row still loads with `model: None, effort: None`.
    Agent {
        prompt: String,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        effort: Option<String>,
        /// Dispatch disposition (headless-agent-automations plan, U1 — R1):
        /// `Some(true)` = closed-loop headless (`claude -p`, no pane),
        /// `Some(false)` = the explicit pane override (`--paned`), `None` =
        /// follow `config.automation_defaults.headless` at claim time (R2:
        /// the config default ships **true**, so legacy rows flip to
        /// headless on upgrade unless explicitly paned). Resolution is
        /// [`Mode::resolved_headless`] — one pure function, applied by the
        /// manager *before* the store lock (the usage-gate precedent) and
        /// stamped onto the claimed row so marker and routing can't
        /// disagree. `#[serde(default)]` keeps legacy `{kind, prompt}` rows
        /// loading as `None`.
        #[serde(default)]
        headless: Option<bool>,
    },
    /// Run a stored script with no model spend (R13–R15). `script_file` is a
    /// path under the store's script dir (U3); `interpreter` is a closed-enum
    /// name resolved by the CLI/server (U9), opaque here.
    #[serde(rename_all = "camelCase")]
    Script {
        script_file: String,
        interpreter: String,
        timeout_ms: u64,
    },
}

impl Mode {
    /// The mode discriminant a [`RunRow`] records.
    pub fn kind(&self) -> RunMode {
        match self {
            Mode::Agent { .. } => RunMode::Agent,
            Mode::Script { .. } => RunMode::Script,
        }
    }

    /// Effective dispatch disposition (headless-agent-automations U1 — R1):
    /// automation-explicit → the caller-supplied config default. Agent-only —
    /// scripts are never headless (they have their own runner). Monitors are
    /// NOT special-cased here: [`Automation::claim`] forces their marker
    /// regardless (R3), so this function stays a pure two-input resolution.
    pub fn resolved_headless(&self, default: bool) -> bool {
        match self {
            Mode::Agent { headless, .. } => headless.unwrap_or(default),
            Mode::Script { .. } => false,
        }
    }
}

/// [`Mode`] discriminant stamped on each run row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RunMode {
    Agent,
    Script,
}

/// What started a run. Manual runs never advance the schedule (R23).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Trigger {
    Schedule,
    Manual,
    /// An automatic re-run of a run that a prior app instance's crash/restart
    /// left interrupted (automations-interrupt-resilience U1). Like `Manual` it
    /// consumes no scheduled occurrence (never advances `next_run_at`); unlike
    /// it, a retry is *unattended*, so the sweep honors the R5 frontend-ready
    /// gate before dispatching an agent retry. Retry-once crash-loop guard: a
    /// run *born* from a `Retry` is never retried again — startup recovery reads
    /// the interrupted row's trigger and only alerts for a `Retry` row.
    Retry,
}

/// Run-row status. `Running` is the only non-terminal state; interrupted,
/// timed-out, and deleted runs are `Failed` with distinct error strings
/// (R5/R23), and `Skipped` is only ever *born* terminal — there is no
/// `Running → Skipped` edge (KTD-D).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RunStatus {
    Running,
    Succeeded,
    Failed,
    Skipped,
}

impl RunStatus {
    /// Terminal rows never transition further (the
    /// [`crate::state::lifecycle`] convention).
    pub fn is_terminal(self) -> bool {
        !matches!(self, RunStatus::Running)
    }
}

/// Where an automation came from (R22): the creating pane id, the workspace
/// identity resolved at create time (R9 — pane ids reset each launch, so a
/// stored pane id must never be resolved across restarts), and an origin
/// label (e.g. `"cli"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Origin {
    pub pane_id: u64,
    pub workspace_id: String,
    pub label: String,
}

/// The machine-readable healthcheck verdict (monitor-handoff U1, R2): the
/// PASS/FAIL outcome plus the free-text note parsed from a check's final
/// assistant turn at run close (monitor-handoff U3 owns the parsing; the
/// model only carries the shape). Serde camelCase like the rest of the wire
/// contract; `note` defaults so a bare `{"outcome": "pass"}` still loads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Verdict {
    pub outcome: VerdictOutcome,
    #[serde(default)]
    pub note: String,
}

/// [`Verdict`] discriminant (monitor-handoff R2). On the wire as
/// `"pass"`/`"fail"` (the file-wide camelCase convention); the uppercase
/// `PASS`/`FAIL` spelling is the *prompt-block* contract, translated by the
/// U3 parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VerdictOutcome {
    Pass,
    Fail,
}

impl VerdictOutcome {
    /// The prompt-block spelling (`PASS`/`FAIL`) — the display form shared by
    /// the alert line (`mod.rs`) and the CLI's verdict rendering, so the
    /// human-facing spelling can't drift between surfaces.
    pub fn as_str(&self) -> &'static str {
        match self {
            VerdictOutcome::Pass => "PASS",
            VerdictOutcome::Fail => "FAIL",
        }
    }
}

/// Pickup pointers captured at monitor registration (monitor-handoff U1,
/// R11): the parent session's id, transcript path, and cwd — stored on the
/// [`Automation`] record itself, not a run row, so they survive run-history
/// eviction and app restarts (monitor-handoff R4). Each field carries
/// `#[serde(default)]` so a partially-written record degrades to empty
/// strings instead of poisoning the whole store load.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorPointers {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub transcript_path: String,
    #[serde(default)]
    pub session_cwd: String,
}

/// Monitor-handoff R9: creation-time default for
/// [`Automation::retry_on_interrupt`]. Monitors default **on** — an
/// app-restart-interrupted check re-runs once instead of silently losing its
/// tick — while ordinary automations keep the interrupt-resilience default
/// of **off** (a money/cloud agent must never silently re-run). Create paths
/// (monitor-handoff U4/U5) apply this when the flag isn't explicitly set.
pub fn default_retry_on_interrupt(monitor: bool) -> bool {
    monitor
}

/// One row of an automation's bounded run history (R8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRow {
    pub id: String,
    pub mode: RunMode,
    pub trigger: Trigger,
    pub status: RunStatus,
    /// Linked pane for agent runs, threaded through `spawn_pane` (R10, U7).
    pub pane_id: Option<u64>,
    /// The model / reasoning effort this agent run launched with
    /// (automations-workspace-and-model plan, U1/U4a — R13). Stamped at
    /// dispatch after deterministic resolution; `None` for script runs and for
    /// agent runs that fell through to Claude's own default.
    /// `#[serde(default)]` keeps legacy rows loading unchanged.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    /// Parsed machine-readable verdict (monitor-handoff U1, R2): stamped at
    /// run close by the verdict path (monitor-handoff U3); `None` for every
    /// non-monitor run and for checks that parsed no verdict (a not-done
    /// check, monitor-handoff R5). `#[serde(default)]` keeps legacy rows
    /// loading unchanged.
    #[serde(default)]
    pub verdict: Option<Verdict>,
    /// Path of the durable failure bundle written for a FAIL verdict
    /// (monitor-handoff U1, R15) — the bundle file outlives the R8 output
    /// tail cap; the short verdict note rides `output` as usual.
    #[serde(default)]
    pub bundle_path: Option<String>,
    /// Headless-check marker (U2 of
    /// `docs/plans/2026-07-11-003-feat-headless-monitor-checks-plan.md` —
    /// R7): `true` when this run is a monitor check dispatched as a
    /// backend-managed `claude -p` child — no pane, no tab. Derived inside
    /// [`Automation::claim`] from `monitor` + agent mode (the one funnel for
    /// scheduled, manual, and retry claims — no call-site signature churn).
    /// The sweep's pane-oriented ack-timeout and deadline closes exclude
    /// marked rows (`mod.rs`, R7); their deadline is the runner's own, with
    /// the sweep only as a slack-delayed backstop. `#[serde(default)]` keeps
    /// legacy rows loading as `false`; an old binary rewriting a new store
    /// merely drops the marker on already-terminal rows (harmless — the
    /// plan's KTD).
    #[serde(default)]
    pub headless: bool,
    /// The headless check's Claude session id (headless-monitor-checks
    /// plan, U2 — R12): stamped from the stream's `init` event when the run
    /// closes (U4/U5) and riding the FAIL bundle alongside the derived
    /// transcript path. `None` for pane runs and for checks whose stream
    /// never delivered an `init`; omitted from the wire entirely when
    /// `None`, so pane rows serialize byte-identically to before (R14).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Captured output, capped to an [`OUTPUT_TAIL_CAP_BYTES`] tail (R8).
    pub output: Option<String>,
    pub exit_code: Option<i32>,
    /// Failure detail for `Failed` rows; the skip reason for `Skipped` rows.
    pub error: Option<String>,
    /// The occurrence this row was for (epoch ms) — the pre-claim
    /// `next_run_at`. `None` for manual runs (R23: no occurrence consumed).
    pub scheduled_for: Option<u64>,
    /// When the run started (epoch ms). `None` for skipped rows — they never
    /// ran (KTD-D).
    pub started_at: Option<u64>,
    /// When the row reached a terminal status (epoch ms). Skipped rows are
    /// born terminal, so it is set at creation.
    pub finished_at: Option<u64>,
}

/// A named, per-cwd scheduled task (R1) with its embedded bounded run
/// history (R8). Serde camelCase — this shape crosses the store file, the
/// socket protocol (U9), and the dashboard (U10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Automation {
    pub id: String,
    pub name: String,
    /// 5-field cron expression (R1) — validated by the schedule unit (U2),
    /// opaque here.
    pub cron: String,
    /// IANA timezone name (R1) — validated by U2, opaque here.
    pub timezone: String,
    pub enabled: bool,
    /// Opt-in resilience (automations-interrupt-resilience U1/R1): when true, a
    /// run this automation leaves `Running` at an app crash/restart is
    /// re-dispatched **once** on the next launch as a [`Trigger::Retry`] run.
    /// Default **off** — a money/cloud agent (the curve-read case) must never
    /// silently re-run, so only automations explicitly opted in retry.
    /// `#[serde(default)]` keeps every legacy store row loading as `false`.
    #[serde(default)]
    pub retry_on_interrupt: bool,
    /// Monitor flavor (monitor-handoff U1, R1): an agent-mode automation
    /// with a not-before time and a sparse re-check schedule that delivers
    /// one machine-readable verdict and retires. `#[serde(default)]` keeps
    /// every legacy store row loading as `false`.
    #[serde(default)]
    pub monitor: bool,
    /// Monitor-handoff R1: epoch-ms floor below which the monitor never
    /// runs — the schedule unit (monitor-handoff U2) clamps every
    /// `next_run_at` recompute with it (create / resume / post-skip).
    /// Untrusted numeric input: schedule math must stay checked/saturating
    /// (release builds have overflow checks off).
    #[serde(default)]
    pub not_before_ms: Option<u64>,
    /// Monitor-handoff R3/R4: set (epoch ms) when a parsed verdict retired
    /// this monitor — scheduling stopped permanently, sweep claims and
    /// manual runs refused ([`ClaimError::Retired`]), record and run history
    /// kept (never a delete; delete behavior is unchanged). Stamped by
    /// [`Automation::retire`] in the same store mutation that closes the
    /// verdict run (monitor-handoff U3, KTD-B).
    #[serde(default)]
    pub retired_at: Option<u64>,
    /// Monitor-handoff R11/R4: pickup pointers captured at registration
    /// (monitor-handoff U4); `None` for every non-monitor automation.
    #[serde(default)]
    pub pickup_pointers: Option<MonitorPointers>,
    pub cwd: String,
    pub mode: Mode,
    pub origin: Origin,
    /// Created stamp, epoch ms.
    pub created_at: u64,
    /// Last-mutation stamp, epoch ms (claims/skips/closes update it).
    pub updated_at: u64,
    /// Next occurrence, epoch ms. `None` = paused (R23).
    pub next_run_at: Option<u64>,
    /// Bounded run history, oldest first (R8). Last-run state is *derived*
    /// from this (see [`Automation::last_run`]) — no separate mirrors to
    /// drift.
    pub runs: Vec<RunRow>,
}

/// Why a claim was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimError {
    /// The automation is disabled: nothing was appended, nothing moved.
    Disabled,
    /// The monitor is retired (monitor-handoff R3): a parsed verdict
    /// permanently stopped it — sweep claims and manual runs alike are
    /// refused. Nothing appended, nothing moved. Outranks [`Disabled`]:
    /// retirement is the permanent state.
    ///
    /// [`Disabled`]: ClaimError::Disabled
    Retired,
}

/// Terminal outcome handed to [`Automation::close`]. Deliberately has no
/// `Skipped` variant — the run-state machine has no `Running → Skipped` edge
/// (KTD-D).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    Succeeded {
        output: Option<String>,
    },
    Failed {
        error: String,
        exit_code: Option<i32>,
        output: Option<String>,
    },
}

/// What [`Automation::close`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseResult {
    Closed,
    /// The row was already terminal — no-op (no transitions out of terminal
    /// states).
    AlreadyClosed,
    /// No row with that id (e.g. evicted by the R8 history cap).
    NotFound,
}

/// Cap a captured output string to its last [`OUTPUT_TAIL_CAP_BYTES`] bytes
/// (R8), advancing to a char boundary so multi-byte UTF-8 never splits. The
/// **tail** survives — a script's verdict is its trailing lines (R15).
/// Saturating arithmetic throughout: release builds have overflow checks off
/// (repo convention for untrusted/parsed input).
pub fn output_tail(s: &str) -> String {
    if s.len() <= OUTPUT_TAIL_CAP_BYTES {
        return s.to_owned();
    }
    let mut start = s.len().saturating_sub(OUTPUT_TAIL_CAP_BYTES);
    while start < s.len() && !s.is_char_boundary(start) {
        start = start.saturating_add(1);
    }
    s[start..].to_owned()
}

impl Automation {
    /// Claim a due (or manually triggered) automation: overwrite
    /// `next_run_at` with the caller-supplied advanced value and append a
    /// `Running` row (R2's persist-before-run flush is the store/manager's
    /// job — the model only records the transition).
    ///
    /// Rejects a disabled automation. A *paused* automation
    /// (`next_run_at == None`) is still claimable when enabled — manual runs
    /// are allowed on paused automations (R23); such callers pass the
    /// current (unchanged) `next_run_at`, since manual runs never advance
    /// the schedule.
    ///
    /// The appended row's `scheduled_for` captures the pre-claim
    /// `next_run_at` for scheduled claims (the occurrence being consumed);
    /// manual claims record `None`.
    ///
    /// Monitor-handoff R3: a retired monitor refuses every claim — sweep
    /// and manual alike — with [`ClaimError::Retired`], checked before the
    /// softer `enabled` gate (retirement is the permanent state).
    ///
    /// Headless-monitor-checks U2 (R7), widened by the
    /// headless-agent-automations plan (its U1 — R1/R3): the appended row's
    /// `headless` marker is derived HERE, in the one funnel every claim path
    /// (scheduled sweep, manual run, retry drain) goes through. `headless`
    /// is the caller-resolved effective disposition
    /// ([`Mode::resolved_headless`], resolved off the store lock); the
    /// marker becomes `monitor || headless` for agent claims — a monitor
    /// forces true regardless of the argument (R3) — and script claims
    /// never stamp. Stamping at claim (not dispatch) keeps the row marker
    /// and the routing from ever disagreeing: the sweep exemptions and kill
    /// gates all key on the row.
    pub fn claim(
        &mut self,
        next_run_at: Option<u64>,
        now_ms: u64,
        trigger: Trigger,
        run_id: &str,
        headless: bool,
    ) -> Result<(), ClaimError> {
        if self.retired_at.is_some() {
            return Err(ClaimError::Retired);
        }
        if !self.enabled {
            return Err(ClaimError::Disabled);
        }
        // A retry consumes no occurrence (like a manual run) — only a scheduled
        // claim records the occurrence it is burning.
        let scheduled_for = match trigger {
            Trigger::Schedule => self.next_run_at,
            Trigger::Manual | Trigger::Retry => None,
        };
        // The row is marked at claim — letting the sweep's pane-oriented
        // probes leave it alone regardless of which path claimed it. A
        // monitor's agent check is unconditionally headless (headless-
        // monitor-checks R7); a regular agent claim takes the caller's
        // resolved disposition (headless-agent-automations R1); scripts
        // never mark.
        let headless = self.mode.kind() == RunMode::Agent && (self.monitor || headless);
        self.next_run_at = next_run_at;
        self.updated_at = now_ms;
        self.push_row(RunRow {
            id: run_id.to_owned(),
            mode: self.mode.kind(),
            trigger,
            status: RunStatus::Running,
            pane_id: None,
            model: None,
            effort: None,
            verdict: None,
            bundle_path: None,
            headless,
            session_id: None,
            output: None,
            exit_code: None,
            error: None,
            scheduled_for,
            started_at: Some(now_ms),
            finished_at: None,
        });
        Ok(())
    }

    /// Record a pre-claim skip (R7 overlap, U5 capacity): append a terminal
    /// `Skipped` row with the reason in `error`.
    ///
    /// Honesty note (KTD-D): this does **not** touch `next_run_at`. The
    /// schedule still advances past a skipped occurrence, but that advance is
    /// the caller's separate step — recompute the next occurrence and apply
    /// it via [`Automation::rollback_recompute`] (the same
    /// overwrite-with-recomputed primitive R3 uses).
    pub fn skip(&mut self, now_ms: u64, trigger: Trigger, reason: &str, run_id: &str) {
        let scheduled_for = match trigger {
            Trigger::Schedule => self.next_run_at,
            Trigger::Manual | Trigger::Retry => None,
        };
        self.updated_at = now_ms;
        self.push_row(RunRow {
            id: run_id.to_owned(),
            mode: self.mode.kind(),
            trigger,
            status: RunStatus::Skipped,
            pane_id: None,
            model: None,
            effort: None,
            verdict: None,
            bundle_path: None,
            // A skip never dispatches, so the dispatch-shape marker stays
            // unset even on a monitor (headless-monitor-checks U2).
            headless: false,
            session_id: None,
            output: None,
            exit_code: None,
            error: Some(reason.to_owned()),
            scheduled_for,
            started_at: None,
            finished_at: Some(now_ms),
        });
    }

    /// Close a `Running` row with a terminal outcome: sets `finished_at`,
    /// the status, and the outcome's error / exit code / output (output
    /// capped to the R8 tail via [`output_tail`]).
    ///
    /// Terminal rows never transition again: closing an already-closed row
    /// is a no-op returning [`CloseResult::AlreadyClosed`]; an unknown id
    /// returns [`CloseResult::NotFound`].
    pub fn close(&mut self, run_id: &str, outcome: RunOutcome, now_ms: u64) -> CloseResult {
        let Some(row) = self.runs.iter_mut().find(|r| r.id == run_id) else {
            return CloseResult::NotFound;
        };
        if row.status.is_terminal() {
            return CloseResult::AlreadyClosed;
        }
        match outcome {
            RunOutcome::Succeeded { output } => {
                row.status = RunStatus::Succeeded;
                row.output = output.map(|o| output_tail(&o));
            }
            RunOutcome::Failed {
                error,
                exit_code,
                output,
            } => {
                row.status = RunStatus::Failed;
                row.error = Some(error);
                row.exit_code = exit_code;
                row.output = output.map(|o| output_tail(&o));
            }
        }
        row.finished_at = Some(now_ms);
        self.updated_at = now_ms;
        CloseResult::Closed
    }

    /// R3: overwrite `next_run_at` with a freshly **recomputed** occurrence
    /// (or `None` when paused/exhausted) — never the pre-claim value, which
    /// could clobber a concurrent edit. Also the advance step after a
    /// [`Automation::skip`] (KTD-D). Cron math lives in the schedule unit
    /// (U2); this module has no cron dependency.
    pub fn rollback_recompute(&mut self, recomputed_next: Option<u64>) {
        self.next_run_at = recomputed_next;
    }

    /// Retire (monitor-handoff U1, R3): a parsed verdict permanently stops
    /// scheduling — `retired_at` stamps the transition and `next_run_at`
    /// clears, but the record and its run history stay intact (monitor-
    /// handoff R4: never a delete; delete behavior is unchanged). From here
    /// on every claim is refused ([`ClaimError::Retired`]). `enabled` is not
    /// touched — retirement is its own axis, not a disable.
    ///
    /// Idempotent: a second retire keeps the original stamp, mutates
    /// nothing, and returns `false`. In-flight rows are untouched — closing
    /// a `Running` row on a just-retired monitor still lands, since
    /// [`Automation::close`] has no retirement gate (no strandable edge; the
    /// U3 close path sets the verdict, closes the row, and retires in one
    /// store mutation, KTD-B).
    pub fn retire(&mut self, now_ms: u64) -> bool {
        if self.retired_at.is_some() {
            return false;
        }
        self.retired_at = Some(now_ms);
        self.next_run_at = None;
        self.updated_at = now_ms;
        true
    }

    /// Monitor-handoff R6/R7: the consecutive-infra-failure count, DERIVED
    /// from run history — never stored, so there is no counter to strand.
    /// Walks the history newest-first; **any verdict-bearing row** is a
    /// concluded, readable check and stops the walk (the R7 reset).
    /// Otherwise:
    ///
    /// - a `Failed` row is an infrastructure failure (timeout / crash /
    ///   interruption — never a healthcheck verdict, R6) and counts;
    /// - a `Succeeded` row **with no captured output** also counts — the
    ///   monitor-handoff **U3 refinement** of this walk's original
    ///   "Succeeded always resets" rule: the check concluded but its output
    ///   could not be attributed (capture abstention — a busy cwd), so its
    ///   verdict is unreadable; without counting these, a monitor whose
    ///   output can never be read would run silent forever (the plan's
    ///   Risks note: escalation bounds it);
    /// - a verdict-less `Succeeded` row whose output **contains a
    ///   verdict-fence opener** also counts (fix(review) #5): the check
    ///   OPENED a block but it never parsed — a near-miss (decorated or
    ///   lowercase outcome, unclosed fence, two blocks), i.e. persistent
    ///   non-compliance, which must escalate to a visible "monitor broken"
    ///   rather than resetting forever. Detection is
    ///   [`super::verdict::contains_verdict_opener`] — the parser's own
    ///   opener rule, so the two cannot drift. Accepted miss: the stored
    ///   output is the R8 tail ([`output_tail`]) — a cap that cut the opener
    ///   off leaves the row reading as a plain not-done check;
    /// - any other `Succeeded` row **with output** is a readable, concluded
    ///   check — e.g. a healthy not-done check that reported "still
    ///   running" — and resets the walk (a long experiment produces many of
    ///   these; they must never escalate);
    /// - `Skipped` rows are neutral (never ran — neither count nor reset),
    ///   and so is a still-`Running` row (it has concluded nothing yet).
    ///
    /// The escalation threshold and its alert-once behavior live in the
    /// close path (monitor-handoff U3), not here.
    pub fn consecutive_infra_failures(&self) -> usize {
        let mut n = 0;
        for row in self.runs.iter().rev() {
            if row.verdict.is_some() {
                break; // a concluded, readable check: reset
            }
            match row.status {
                RunStatus::Failed => n += 1,
                // U3 refinement: concluded but unreadable (abstained capture).
                RunStatus::Succeeded if row.output.is_none() => n += 1,
                // fix(review) #5: an opened-but-unparseable block is a
                // near-miss, not a healthy not-done check.
                RunStatus::Succeeded
                    if row
                        .output
                        .as_deref()
                        .is_some_and(super::verdict::contains_verdict_opener) =>
                {
                    n += 1
                }
                RunStatus::Succeeded => break, // readable not-done check: reset
                RunStatus::Skipped | RunStatus::Running => {}
            }
        }
        n
    }

    /// R7: whether a run is in flight (a `Running` row exists). The manager
    /// widens this with R7's second clause — a deadline-failed agent run
    /// whose linked pane is still alive — which needs pane liveness the pure
    /// model doesn't have (U4/U7).
    pub fn in_flight(&self) -> bool {
        self.runs.iter().any(|r| r.status == RunStatus::Running)
    }

    /// The most recent run row — the derived last-run mirror (R25 reads
    /// status/finished_at/error from it). Derived, not stored, so it cannot
    /// drift from the history.
    pub fn last_run(&self) -> Option<&RunRow> {
        self.runs.last()
    }

    /// Append a row, evicting beyond [`RUN_HISTORY_CAP`] (R8) — the oldest
    /// **terminal** row first, never a `Running` one. A long-lived agent run
    /// (up to the 30-min deadline, U7) can outlive 20 later occurrences'
    /// rows; evicting it blind (oldest-first) would drop the only `Running`
    /// row, silently breaking [`Automation::in_flight`] and the pane-linked
    /// close path (U7's `close_run_by_pane`). So preserve every `Running` row
    /// and evict the oldest terminal one instead. If every row is `Running`
    /// (impossible under the R7 overlap gate — at most one runs at a time —
    /// but handled defensively), the history simply exceeds the cap rather
    /// than dropping a live run.
    ///
    /// Verdict-bearing rows are equally protected (monitor-handoff U1, R4):
    /// a monitor's verdict row is its durable verdict record and must
    /// survive restarts, so eviction takes the oldest *verdict-less*
    /// terminal row. In practice a monitor retires on its first verdict —
    /// claims stop, so at most one such row ever exists and post-retire the
    /// history is frozen; the guard covers the window where later rows
    /// (e.g. a still-`Running` one) coexist with it.
    fn push_row(&mut self, row: RunRow) {
        self.runs.push(row);
        while self.runs.len() > RUN_HISTORY_CAP {
            let Some(oldest_evictable) = self
                .runs
                .iter()
                .position(|r| r.status.is_terminal() && r.verdict.is_none())
            else {
                break; // never drop a Running or verdict-bearing row
            };
            self.runs.remove(oldest_evictable);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script_mode() -> Mode {
        Mode::Script {
            script_file: "script".into(),
            interpreter: "bash".into(),
            timeout_ms: 120_000,
        }
    }

    /// An enabled automation whose next occurrence is at t=60_000.
    fn automation(mode: Mode) -> Automation {
        Automation {
            id: "a1".into(),
            name: "disk watch".into(),
            cron: "*/5 * * * *".into(),
            timezone: "America/New_York".into(),
            enabled: true,
            retry_on_interrupt: false,
            monitor: false,
            not_before_ms: None,
            retired_at: None,
            pickup_pointers: None,
            cwd: "/tmp".into(),
            mode,
            origin: Origin {
                pane_id: 7,
                workspace_id: "ws-1".into(),
                label: "cli".into(),
            },
            created_at: 1_000,
            updated_at: 1_000,
            next_run_at: Some(60_000),
            runs: Vec::new(),
        }
    }

    // R1/R2: the sweep's claim consumes the due occurrence — the schedule
    // moves to the supplied advanced value and a Running row appears,
    // stamped with the occurrence it was for.
    #[test]
    fn claim_on_enabled_due_automation_advances_next_run_at_and_appends_running_row() {
        let mut a = automation(script_mode());
        assert_eq!(
            a.claim(Some(360_000), 61_000, Trigger::Schedule, "r1", false),
            Ok(())
        );

        assert_eq!(a.next_run_at, Some(360_000));
        assert_eq!(a.updated_at, 61_000);
        assert_eq!(a.runs.len(), 1);
        let row = &a.runs[0];
        assert_eq!(row.id, "r1");
        assert_eq!(row.status, RunStatus::Running);
        assert_eq!(row.mode, RunMode::Script);
        assert_eq!(row.trigger, Trigger::Schedule);
        assert_eq!(row.scheduled_for, Some(60_000), "pre-claim occurrence");
        assert_eq!(row.started_at, Some(61_000));
        assert_eq!(row.finished_at, None);
        assert_eq!(row.pane_id, None);
        assert_eq!(row.output, None);
        assert_eq!(row.exit_code, None);
        assert_eq!(row.error, None);
    }

    // R1: a disabled automation is never claimable — nothing appended,
    // nothing moved.
    #[test]
    fn claim_rejects_a_disabled_automation() {
        let mut a = automation(script_mode());
        a.enabled = false;

        assert_eq!(
            a.claim(Some(360_000), 61_000, Trigger::Schedule, "r1", false),
            Err(ClaimError::Disabled)
        );
        assert!(a.runs.is_empty());
        assert_eq!(a.next_run_at, Some(60_000));
        assert_eq!(a.updated_at, 1_000);
    }

    // R23: a manual run is allowed on a paused automation and never
    // consumes an occurrence — the caller passes the current (unchanged)
    // schedule value and the row records no scheduled_for.
    #[test]
    fn manual_claim_on_paused_automation_runs_without_consuming_an_occurrence() {
        let mut a = automation(Mode::Agent {
            prompt: "summarize CI".into(),
            model: None,
            effort: None, headless: None,
        });
        a.next_run_at = None; // paused

        assert_eq!(a.claim(None, 61_000, Trigger::Manual, "r1", false), Ok(()));
        assert_eq!(a.next_run_at, None, "still paused");
        let row = &a.runs[0];
        assert_eq!(row.trigger, Trigger::Manual);
        assert_eq!(row.mode, RunMode::Agent);
        assert_eq!(row.scheduled_for, None);
        assert_eq!(row.status, RunStatus::Running);
    }

    // Interrupt-resilience U1: a Trigger::Retry claim behaves like a manual one
    // for scheduling — the caller passes the current next_run_at through
    // unchanged and the row records no scheduled_for (a retry consumes no
    // occurrence).
    #[test]
    fn retry_claim_consumes_no_occurrence() {
        let mut a = automation(script_mode());
        let keep = a.next_run_at;

        assert_eq!(a.claim(keep, 61_000, Trigger::Retry, "r1", false), Ok(()));
        assert_eq!(a.next_run_at, keep, "retry never advances the schedule");
        let row = &a.runs[0];
        assert_eq!(row.trigger, Trigger::Retry);
        assert_eq!(row.scheduled_for, None, "no occurrence consumed");
        assert_eq!(row.status, RunStatus::Running);
    }

    // R7/KTD-D: the pre-claim skip appends a born-terminal Skipped row and
    // does NOT touch next_run_at — the schedule advance is the caller's
    // separate step (rollback_recompute), so a skipped occurrence still
    // moves on.
    #[test]
    fn skip_appends_a_skipped_row_and_leaves_schedule_advance_to_the_caller() {
        let mut a = automation(script_mode());
        a.skip(61_000, Trigger::Schedule, "run in flight", "r2");

        assert_eq!(a.next_run_at, Some(60_000), "skip itself never advances");
        assert_eq!(a.runs.len(), 1);
        let row = &a.runs[0];
        assert_eq!(row.status, RunStatus::Skipped);
        assert_eq!(row.error.as_deref(), Some("run in flight"));
        assert_eq!(row.scheduled_for, Some(60_000), "the occurrence skipped");
        assert_eq!(row.started_at, None, "a skipped run never ran");
        assert_eq!(row.finished_at, Some(61_000), "born terminal");

        // The caller then advances the schedule separately (KTD-D).
        a.rollback_recompute(Some(360_000));
        assert_eq!(a.next_run_at, Some(360_000));
    }

    // R5/R25: closing succeeded sets finished_at and the derived last-run
    // mirror reflects it.
    #[test]
    fn close_succeeded_sets_finished_at_and_derived_last_run_status() {
        let mut a = automation(script_mode());
        a.claim(Some(360_000), 61_000, Trigger::Schedule, "r1", false)
            .unwrap();

        let res = a.close(
            "r1",
            RunOutcome::Succeeded {
                output: Some("all good".into()),
            },
            70_000,
        );
        assert_eq!(res, CloseResult::Closed);
        let row = a.last_run().expect("derived last-run mirror");
        assert_eq!(row.status, RunStatus::Succeeded);
        assert_eq!(row.finished_at, Some(70_000));
        assert_eq!(row.output.as_deref(), Some("all good"));
        assert_eq!(row.error, None);
        assert!(!a.in_flight());
    }

    // R3/R5: closing failed records the error string, exit code, output,
    // and finished_at (interrupted/timed-out/deleted are Failed with
    // distinct error strings — no separate statuses).
    #[test]
    fn close_failed_records_error_exit_code_output_and_finished_at() {
        let mut a = automation(script_mode());
        a.claim(Some(360_000), 61_000, Trigger::Schedule, "r1", false)
            .unwrap();

        let res = a.close(
            "r1",
            RunOutcome::Failed {
                error: ERR_INTERRUPTED.into(),
                exit_code: Some(3),
                output: Some("boom".into()),
            },
            70_000,
        );
        assert_eq!(res, CloseResult::Closed);
        let row = &a.runs[0];
        assert_eq!(row.status, RunStatus::Failed);
        assert_eq!(row.error.as_deref(), Some(ERR_INTERRUPTED));
        assert_eq!(row.exit_code, Some(3));
        assert_eq!(row.output.as_deref(), Some("boom"));
        assert_eq!(row.finished_at, Some(70_000));
    }

    // KTD-D: no transitions out of terminal states — a second close is a
    // no-op returning an indicator and mutating nothing.
    #[test]
    fn double_close_is_a_no_op_returning_already_closed() {
        let mut a = automation(script_mode());
        a.claim(Some(360_000), 61_000, Trigger::Schedule, "r1", false)
            .unwrap();
        a.close(
            "r1",
            RunOutcome::Succeeded {
                output: Some("first".into()),
            },
            70_000,
        );

        let res = a.close(
            "r1",
            RunOutcome::Failed {
                error: "late failure".into(),
                exit_code: Some(1),
                output: None,
            },
            80_000,
        );
        assert_eq!(res, CloseResult::AlreadyClosed);
        let row = &a.runs[0];
        assert_eq!(row.status, RunStatus::Succeeded, "first close wins");
        assert_eq!(row.finished_at, Some(70_000));
        assert_eq!(row.output.as_deref(), Some("first"));
        assert_eq!(row.error, None);
    }

    // R8 edge: a run id that no longer exists (e.g. evicted) is reported,
    // not invented.
    #[test]
    fn close_on_unknown_run_id_returns_not_found() {
        let mut a = automation(script_mode());
        assert_eq!(
            a.close("ghost", RunOutcome::Succeeded { output: None }, 70_000),
            CloseResult::NotFound
        );
    }

    // R3: rollback overwrites next_run_at with the supplied recomputed
    // value — never restores the pre-claim value.
    #[test]
    fn rollback_recompute_overwrites_next_run_at_with_the_supplied_value() {
        let mut a = automation(script_mode());
        a.claim(Some(360_000), 61_000, Trigger::Schedule, "r1", false)
            .unwrap();

        a.rollback_recompute(Some(420_000));
        assert_eq!(a.next_run_at, Some(420_000), "recomputed, not pre-claim");

        a.rollback_recompute(None);
        assert_eq!(a.next_run_at, None, "None (paused/exhausted) is honored");
    }

    // R8: history is bounded at 20 rows, evicting oldest-first.
    #[test]
    fn history_evicts_beyond_twenty_rows_oldest_first() {
        let mut a = automation(script_mode());
        for i in 0..25u64 {
            a.skip(i, Trigger::Schedule, "overlap", &format!("r{i}"));
        }
        assert_eq!(a.runs.len(), RUN_HISTORY_CAP);
        assert_eq!(a.runs[0].id, "r5", "r0..r4 evicted oldest-first");
        assert_eq!(a.runs.last().unwrap().id, "r24");
    }

    // R8/U7: a long-lived Running row (an agent run pending up to the 30-min
    // deadline) is NEVER evicted by the history cap — only terminal rows are,
    // oldest-first. Evicting the Running row would silently break in_flight()
    // and the pane-linked close path.
    #[test]
    fn history_eviction_preserves_a_running_row_and_drops_oldest_terminal() {
        let mut a = automation(Mode::Agent {
            prompt: "long agent".into(),
            model: None,
            effort: None, headless: None,
        });
        // A claim mid-history: an agent run that stays Running while later
        // occurrences skip past it.
        a.claim(Some(60_000), 10, Trigger::Schedule, "live", false).unwrap();
        for i in 0..RUN_HISTORY_CAP as u64 + 5 {
            a.skip(100 + i, Trigger::Schedule, "overlap", &format!("r{i}"));
        }

        assert_eq!(a.runs.len(), RUN_HISTORY_CAP);
        assert!(
            a.runs.iter().any(|r| r.id == "live" && r.status == RunStatus::Running),
            "the Running row survives even though older than evicted terminal rows"
        );
        assert!(a.in_flight(), "in_flight stays true — the live run wasn't dropped");
        assert_eq!(a.runs[0].id, "live", "it is now the oldest surviving row");
    }

    // R8: stored output keeps the last 8 KiB — the TAIL, because a script's
    // verdict is its trailing lines (R15) — not the front.
    #[test]
    fn stored_output_truncates_to_an_8_kib_tail_keeping_the_end() {
        let mut a = automation(script_mode());
        a.claim(Some(360_000), 61_000, Trigger::Schedule, "r1", false)
            .unwrap();

        let big = format!("{}TAIL", "a".repeat(9_000));
        a.close("r1", RunOutcome::Succeeded { output: Some(big) }, 70_000);
        let out = a.runs[0].output.as_deref().unwrap();
        assert_eq!(out.len(), OUTPUT_TAIL_CAP_BYTES);
        assert!(out.ends_with("TAIL"), "the tail survives");

        // Under the cap: passes through untouched.
        assert_eq!(output_tail("short"), "short");
    }

    // R8: the tail cut lands on a char boundary — multi-byte UTF-8 output
    // must never panic or split a character.
    #[test]
    fn output_tail_lands_on_a_char_boundary_for_multibyte_utf8() {
        // 4_000 × '€' (3 bytes) + "END" = 12_003 bytes; the naive cut at
        // len − 8_192 = 3_811 falls mid-'€' and must advance.
        let big = format!("{}END", "€".repeat(4_000));
        let out = output_tail(&big);
        assert!(out.len() <= OUTPUT_TAIL_CAP_BYTES);
        assert!(out.ends_with("END"));
        assert!(out.starts_with('€'), "cut advanced to a full char");
        assert_eq!(out.len(), 8_190, "two bytes yielded to the boundary");
    }

    // U1: the wire/store shape is camelCase (the shape crosses the store
    // file, the socket protocol, and the dashboard) and round-trips
    // losslessly.
    #[test]
    fn serde_round_trip_preserves_camel_case_field_names() {
        let mut a = automation(script_mode());
        a.claim(Some(360_000), 61_000, Trigger::Schedule, "r1", false)
            .unwrap();
        a.close(
            "r1",
            RunOutcome::Failed {
                error: ERR_TIMED_OUT.into(),
                exit_code: Some(124),
                output: Some("late".into()),
            },
            70_000,
        );

        let v = serde_json::to_value(&a).unwrap();
        for key in ["nextRunAt", "createdAt", "updatedAt", "cwd", "runs"] {
            assert!(v.get(key).is_some(), "automation key {key}");
        }
        assert_eq!(v["mode"]["kind"], "script");
        for key in ["scriptFile", "interpreter", "timeoutMs"] {
            assert!(v["mode"].get(key).is_some(), "mode key {key}");
        }
        for key in ["paneId", "workspaceId", "label"] {
            assert!(v["origin"].get(key).is_some(), "origin key {key}");
        }
        let row = &v["runs"][0];
        for key in [
            "paneId",
            "exitCode",
            "scheduledFor",
            "startedAt",
            "finishedAt",
            "output",
            "error",
        ] {
            assert!(row.get(key).is_some(), "run row key {key}");
        }
        assert_eq!(row["status"], "failed");
        assert_eq!(row["trigger"], "schedule");
        assert_eq!(row["mode"], "script");
        // headless-monitor-checks U2 (R14): the marker rides the wire as a
        // plain bool; a None session id is omitted entirely.
        assert_eq!(row["headless"], false, "script rows are never headless");
        assert!(row.get("sessionId").is_none(), "None sessionId omitted");

        let back: Automation = serde_json::from_value(v).unwrap();
        assert_eq!(back, a);

        // Agent mode carries its prompt under the same tag convention.
        let agent = serde_json::to_value(Mode::Agent {
            prompt: "summarize CI".into(),
            model: None,
            effort: None, headless: None,
        })
        .unwrap();
        assert_eq!(agent["kind"], "agent");
        assert_eq!(agent["prompt"], "summarize CI");
    }

    // U1 (automations-workspace-and-model, R9/R10): Agent mode carries an
    // optional pinned model + effort, round-tripping under camelCase.
    #[test]
    fn agent_mode_model_and_effort_round_trip() {
        let mode = Mode::Agent {
            prompt: "audit disk".into(),
            model: Some("opus".into()),
            effort: Some("high".into()), headless: None,
        };
        let v = serde_json::to_value(&mode).unwrap();
        assert_eq!(v["kind"], "agent");
        assert_eq!(v["prompt"], "audit disk");
        assert_eq!(v["model"], "opus");
        assert_eq!(v["effort"], "high");
        let back: Mode = serde_json::from_value(v).unwrap();
        assert_eq!(back, mode);
    }

    // U1 back-compat: a legacy `Mode::Agent` JSON with only `{kind, prompt}`
    // (written before this plan) deserializes with `model: None, effort: None`
    // — each new field defaults independently (the enum has no container
    // default). Headless-agent-automations U1 (R1/R12): `headless` defaults
    // the same way, and a `{kind, prompt, model}` shape does too.
    #[test]
    fn legacy_agent_mode_without_model_effort_defaults_to_none() {
        let json = serde_json::json!({ "kind": "agent", "prompt": "hi" });
        let mode: Mode = serde_json::from_value(json).unwrap();
        assert_eq!(
            mode,
            Mode::Agent {
                prompt: "hi".into(),
                model: None,
                effort: None,
                headless: None,
            }
        );

        let json = serde_json::json!({ "kind": "agent", "prompt": "hi", "model": "opus" });
        let mode: Mode = serde_json::from_value(json).unwrap();
        assert_eq!(
            mode,
            Mode::Agent {
                prompt: "hi".into(),
                model: Some("opus".into()),
                effort: None,
                headless: None,
            }
        );
    }

    // Headless-agent-automations U1 (R1/R12): an explicit `headless: false`
    // is an override, not an absence — it must survive a store round-trip
    // (with the config default `true`, losing it would silently flip a
    // `--paned` automation headless).
    #[test]
    fn explicit_headless_false_survives_a_round_trip() {
        let mode = Mode::Agent {
            prompt: "audit".into(),
            model: None,
            effort: None,
            headless: Some(false),
        };
        let v = serde_json::to_value(&mode).unwrap();
        assert_eq!(v["headless"], false, "explicit false is written, not elided");
        let back: Mode = serde_json::from_value(v).unwrap();
        assert_eq!(back, mode);
    }

    // Headless-agent-automations U1 (R1): the one pure resolution —
    // automation-explicit wins over the config default; scripts never
    // resolve headless regardless of the default.
    #[test]
    fn resolved_headless_prefers_the_explicit_pin_over_the_default() {
        let agent = |headless: Option<bool>| Mode::Agent {
            prompt: "p".into(),
            model: None,
            effort: None,
            headless,
        };
        assert!(agent(None).resolved_headless(true));
        assert!(!agent(None).resolved_headless(false));
        assert!(agent(Some(true)).resolved_headless(false));
        assert!(!agent(Some(false)).resolved_headless(true));
        assert!(!script_mode().resolved_headless(true), "scripts never headless");
    }

    // U1 back-compat: a legacy `RunRow` JSON missing `model`/`effort`
    // deserializes with both `None` (no error), and a row carrying them
    // round-trips under camelCase.
    #[test]
    fn run_row_model_effort_are_back_compat_and_round_trip() {
        // Legacy row (no model/effort keys) still loads.
        let legacy = serde_json::json!({
            "id": "r1",
            "mode": "agent",
            "trigger": "schedule",
            "status": "succeeded",
            "paneId": 3,
            "output": "done",
            "exitCode": null,
            "error": null,
            "scheduledFor": 60_000,
            "startedAt": 61_000,
            "finishedAt": 62_000,
        });
        let row: RunRow = serde_json::from_value(legacy).unwrap();
        assert_eq!(row.model, None);
        assert_eq!(row.effort, None);

        // A row carrying model/effort round-trips under camelCase.
        let mut with = row.clone();
        with.model = Some("sonnet".into());
        with.effort = Some("medium".into());
        let v = serde_json::to_value(&with).unwrap();
        assert_eq!(v["model"], "sonnet");
        assert_eq!(v["effort"], "medium");
        let back: RunRow = serde_json::from_value(v).unwrap();
        assert_eq!(back, with);
    }

    // headless-monitor-checks U2 (R14): a legacy RunRow JSON without
    // `headless`/`sessionId` loads with defaults (false/None) — and in the
    // other direction a fresh row omits `sessionId` when None, so pane rows
    // serialize with no new key churn; a populated row round-trips under
    // camelCase.
    #[test]
    fn run_row_headless_and_session_id_are_back_compat_and_round_trip() {
        // Legacy row (no headless/sessionId keys) still loads.
        let legacy = serde_json::json!({
            "id": "r1",
            "mode": "agent",
            "trigger": "schedule",
            "status": "succeeded",
            "paneId": 3,
            "output": "done",
            "exitCode": null,
            "error": null,
            "scheduledFor": 60_000,
            "startedAt": 61_000,
            "finishedAt": 62_000,
        });
        let row: RunRow = serde_json::from_value(legacy).unwrap();
        assert!(!row.headless, "legacy rows default to pane dispatch");
        assert_eq!(row.session_id, None);

        // Serializing it back omits sessionId (skip-if-none) and writes the
        // marker as a plain defaulted bool.
        let v = serde_json::to_value(&row).unwrap();
        assert!(v.get("sessionId").is_none(), "None sessionId omitted");
        assert_eq!(v["headless"], false);

        // A populated headless row round-trips losslessly under camelCase.
        let mut with = row.clone();
        with.headless = true;
        with.session_id = Some("sess-42".into());
        let v = serde_json::to_value(&with).unwrap();
        assert_eq!(v["headless"], true);
        assert_eq!(v["sessionId"], "sess-42");
        let back: RunRow = serde_json::from_value(v).unwrap();
        assert_eq!(back, with);
    }

    // headless-monitor-checks U2 (R7): `headless` derives inside claim() —
    // the one funnel for scheduled, manual, AND retry claims — as `monitor`
    // + agent mode. Regular agent automations and scripts (even a
    // defensively mis-flagged script monitor) stay unmarked, and a skip row
    // never dispatched at all, so it stays unmarked too.
    #[test]
    fn claim_derives_headless_for_monitor_agent_claims_on_every_trigger() {
        let agent_mode = || Mode::Agent {
            prompt: "check the run".into(),
            model: None,
            effort: None, headless: None,
        };

        // Monitor + agent mode: every claim path derives true.
        let mut m = automation(agent_mode());
        m.monitor = true;
        m.claim(Some(360_000), 61_000, Trigger::Schedule, "sched", false)
            .unwrap();
        m.claim(None, 62_000, Trigger::Manual, "manual", false).unwrap();
        m.claim(None, 63_000, Trigger::Retry, "retry", false).unwrap();
        assert_eq!(m.runs.len(), 3);
        for row in &m.runs {
            assert!(row.headless, "claim path {:?} derives headless", row.trigger);
            assert_eq!(row.session_id, None, "stamped later by the runner (R12)");
        }

        // Monitors force the marker even when the caller resolved FALSE
        // (headless-agent-automations R3: flag and config never un-headless
        // a monitor) — the schedule-trigger claim above already passed
        // false; re-check explicitly on a fresh record.
        let mut mf = automation(agent_mode());
        mf.monitor = true;
        mf.claim(Some(360_000), 61_000, Trigger::Schedule, "forced", false)
            .unwrap();
        assert!(mf.runs[0].headless, "monitor forces true regardless of the argument");

        // Regular (non-monitor) agent automation resolved paned: unmarked.
        let mut a = automation(agent_mode());
        a.claim(Some(360_000), 61_000, Trigger::Schedule, "r1", false)
            .unwrap();
        assert!(!a.runs[0].headless);

        // Regular agent automation resolved headless
        // (headless-agent-automations R1): the claim stamps the row.
        let mut ah = automation(agent_mode());
        ah.claim(Some(360_000), 61_000, Trigger::Schedule, "r1", true)
            .unwrap();
        assert!(ah.runs[0].headless, "resolved-headless agent claim stamps the row");

        // Script mode is never agent dispatch — even under a monitor flag,
        // and even when the caller passes resolved true.
        let mut s = automation(script_mode());
        s.monitor = true;
        s.claim(Some(360_000), 61_000, Trigger::Schedule, "r1", true)
            .unwrap();
        assert!(!s.runs[0].headless);

        // A monitor's skip row never dispatched: unmarked.
        let mut k = automation(agent_mode());
        k.monitor = true;
        k.skip(61_000, Trigger::Schedule, "run in flight", "r2");
        assert!(!k.runs[0].headless);
    }

    // headless-monitor-checks U2 (R7/R14) + R8/U7: the eviction guarantees
    // hold with the new fields present — a Running headless check row (with
    // a stamped session id) survives the cap and keeps its fields intact
    // through the churn.
    #[test]
    fn history_eviction_preserves_a_headless_running_row_with_its_fields() {
        let mut a = automation(Mode::Agent {
            prompt: "check the run".into(),
            model: None,
            effort: None, headless: None,
        });
        a.monitor = true;
        a.claim(Some(60_000), 10, Trigger::Schedule, "check", false).unwrap();
        // U4/U5 stamp the session id from the stream's init event; the model
        // test sets the pub field directly (the verdict-test convention).
        a.runs[0].session_id = Some("sess-42".into());
        for i in 0..RUN_HISTORY_CAP as u64 + 5 {
            a.skip(100 + i, Trigger::Schedule, "overlap", &format!("r{i}"));
        }

        assert_eq!(a.runs.len(), RUN_HISTORY_CAP);
        let live = a
            .runs
            .iter()
            .find(|r| r.id == "check")
            .expect("the Running headless row survives eviction");
        assert_eq!(live.status, RunStatus::Running);
        assert!(live.headless, "marker intact through the churn");
        assert_eq!(live.session_id.as_deref(), Some("sess-42"));
        assert!(a.in_flight(), "in_flight still reads the surviving row");
    }

    // monitor-handoff U1 back-compat: a legacy store Automation JSON written
    // before the monitor plan (no monitor / notBeforeMs / retiredAt /
    // pickupPointers keys; rows without verdict / bundlePath) loads with
    // defaults — the store file must round-trip unchanged.
    #[test]
    fn legacy_automation_without_monitor_fields_defaults_and_loads() {
        let legacy = serde_json::json!({
            "id": "a1",
            "name": "old watch",
            "cron": "*/5 * * * *",
            "timezone": "UTC",
            "enabled": true,
            "cwd": "/tmp",
            "mode": {
                "kind": "script",
                "scriptFile": "s",
                "interpreter": "bash",
                "timeoutMs": 1000,
            },
            "origin": { "paneId": 1, "workspaceId": "ws", "label": "cli" },
            "createdAt": 0,
            "updatedAt": 0,
            "nextRunAt": 60_000,
            "runs": [{
                "id": "r1",
                "mode": "script",
                "trigger": "schedule",
                "status": "failed",
                "paneId": null,
                "output": null,
                "exitCode": 1,
                "error": "boom",
                "scheduledFor": 60_000,
                "startedAt": 61_000,
                "finishedAt": 62_000,
            }],
        });
        let a: Automation = serde_json::from_value(legacy).unwrap();
        assert!(!a.monitor);
        assert_eq!(a.not_before_ms, None);
        assert_eq!(a.retired_at, None);
        assert_eq!(a.pickup_pointers, None);
        assert!(!a.retry_on_interrupt, "pre-existing default still holds");
        assert_eq!(a.runs[0].verdict, None);
        assert_eq!(a.runs[0].bundle_path, None);
        // headless-monitor-checks U2 (R14): pre-plan rows also lack
        // headless/sessionId — they default too.
        assert!(!a.runs[0].headless);
        assert_eq!(a.runs[0].session_id, None);
        // The derived count reads legacy rows too: one trailing verdict-less
        // Failed row is one infra failure (monitor-handoff R6).
        assert_eq!(a.consecutive_infra_failures(), 1);
    }

    // monitor-handoff U1: the populated monitor shape rides the same
    // camelCase wire contract (store file / socket / dashboard) and
    // round-trips losslessly.
    #[test]
    fn monitor_fields_serialize_camel_case_and_round_trip() {
        let mut m = automation(script_mode());
        m.monitor = true;
        m.not_before_ms = Some(1_720_000_000_000);
        m.pickup_pointers = Some(MonitorPointers {
            session_id: "sess-1".into(),
            transcript_path: "/home/u/.claude/projects/x/sess-1.jsonl".into(),
            session_cwd: "/home/u/exp".into(),
        });
        m.claim(Some(360_000), 61_000, Trigger::Schedule, "r1", false)
            .unwrap();
        m.close(
            "r1",
            RunOutcome::Failed {
                error: "experiment died".into(),
                exit_code: None,
                output: Some("FAIL: experiment died".into()),
            },
            70_000,
        );
        // U3 stamps these in the same store mutation as the close; the model
        // test sets the pub fields directly.
        m.runs[0].verdict = Some(Verdict {
            outcome: VerdictOutcome::Fail,
            note: "experiment died".into(),
        });
        m.runs[0].bundle_path = Some("/data/bundles/a1-r1.md".into());
        m.retire(70_000);

        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["monitor"], true);
        assert_eq!(v["notBeforeMs"], 1_720_000_000_000u64);
        assert_eq!(v["retiredAt"], 70_000);
        assert_eq!(v["pickupPointers"]["sessionId"], "sess-1");
        assert_eq!(
            v["pickupPointers"]["transcriptPath"],
            "/home/u/.claude/projects/x/sess-1.jsonl"
        );
        assert_eq!(v["pickupPointers"]["sessionCwd"], "/home/u/exp");
        let row = &v["runs"][0];
        assert_eq!(row["verdict"]["outcome"], "fail");
        assert_eq!(row["verdict"]["note"], "experiment died");
        assert_eq!(row["bundlePath"], "/data/bundles/a1-r1.md");

        let back: Automation = serde_json::from_value(v).unwrap();
        assert_eq!(back, m);

        // A bare verdict object without a note still loads (nested default).
        let bare: Verdict = serde_json::from_value(serde_json::json!({ "outcome": "pass" })).unwrap();
        assert_eq!(bare.outcome, VerdictOutcome::Pass);
        assert_eq!(bare.note, "");
    }

    // monitor-handoff R3: a retired monitor refuses claims — sweep and
    // manual alike — appending nothing and moving nothing; retirement
    // outranks disabled.
    #[test]
    fn claim_rejects_a_retired_monitor_for_schedule_and_manual_triggers() {
        let mut a = automation(script_mode());
        a.monitor = true;
        assert!(a.retire(90_000));

        assert_eq!(
            a.claim(Some(360_000), 91_000, Trigger::Schedule, "r1", false),
            Err(ClaimError::Retired)
        );
        assert_eq!(
            a.claim(None, 91_000, Trigger::Manual, "r2", false),
            Err(ClaimError::Retired)
        );
        assert!(a.runs.is_empty(), "nothing appended");
        assert_eq!(a.next_run_at, None, "retire cleared the schedule");
        assert_eq!(a.updated_at, 90_000, "refused claims mutate nothing");

        // Retired wins over disabled — the permanent state reports first.
        a.enabled = false;
        assert_eq!(
            a.claim(None, 92_000, Trigger::Manual, "r3", false),
            Err(ClaimError::Retired)
        );
    }

    // monitor-handoff R3/R4: retire stops scheduling permanently WITHOUT
    // deleting; it is idempotent (the first stamp wins); and a Running row
    // on a just-retired monitor still closes — no stranded in-flight row.
    #[test]
    fn retire_is_idempotent_and_a_just_retired_monitors_running_row_still_closes() {
        let mut a = automation(script_mode());
        a.monitor = true;
        a.claim(Some(360_000), 61_000, Trigger::Schedule, "r1", false)
            .unwrap();

        assert!(a.retire(70_000), "first retire transitions");
        assert_eq!(a.retired_at, Some(70_000));
        assert_eq!(a.next_run_at, None, "scheduling stops permanently");
        assert_eq!(a.updated_at, 70_000);
        assert_eq!(a.runs.len(), 1, "history intact — retire never deletes");
        assert!(a.enabled, "retire is its own axis, not a disable");

        assert!(!a.retire(80_000), "second retire is a no-op");
        assert_eq!(a.retired_at, Some(70_000), "original stamp kept");
        assert_eq!(a.updated_at, 70_000, "the no-op mutates nothing");

        // The in-flight run still lands terminal (close has no retire gate).
        assert_eq!(
            a.close("r1", RunOutcome::Succeeded { output: None }, 81_000),
            CloseResult::Closed
        );
        assert!(!a.in_flight(), "no stranded Running row");
    }

    // monitor-handoff R6/R7 (+ the U3 refinement): the infra-failure count
    // is DERIVED from trailing verdict-less rows that are Failed OR
    // Succeeded-without-output (a concluded check whose capture abstained —
    // its verdict is unreadable). Skipped (and in-flight Running) rows are
    // neutral; a Succeeded row WITH output (a readable not-done check) or
    // any verdict-bearing row resets.
    #[test]
    fn consecutive_infra_failures_derives_from_trailing_verdictless_failed_rows() {
        let mut a = automation(script_mode());
        a.monitor = true;
        assert_eq!(a.consecutive_infra_failures(), 0, "empty history");

        let fail = |a: &mut Automation, id: &str, t: u64| {
            a.claim(Some(t + 1000), t, Trigger::Schedule, id, false).unwrap();
            a.close(
                id,
                RunOutcome::Failed {
                    error: ERR_TIMED_OUT.into(),
                    exit_code: None,
                    output: None,
                },
                t + 1,
            );
        };

        fail(&mut a, "f1", 1_000);
        assert_eq!(a.consecutive_infra_failures(), 1);

        a.skip(2_000, Trigger::Schedule, "overlap", "s1");
        assert_eq!(a.consecutive_infra_failures(), 1, "Skipped is neutral");

        fail(&mut a, "f2", 3_000);
        assert_eq!(a.consecutive_infra_failures(), 2);

        a.claim(Some(5_000), 4_000, Trigger::Schedule, "live", false).unwrap();
        assert_eq!(
            a.consecutive_infra_failures(),
            2,
            "an in-flight Running row concluded nothing — neutral"
        );

        a.close(
            "live",
            RunOutcome::Succeeded {
                output: Some("still training — no verdict yet".into()),
            },
            4_500,
        );
        assert_eq!(
            a.consecutive_infra_failures(),
            0,
            "a readable not-done check (Succeeded WITH output) resets"
        );

        // U3 refinement: a Succeeded check whose output was never captured
        // (abstention — output None) is unreadable and COUNTS, so a monitor
        // whose output can never be attributed eventually escalates.
        a.claim(Some(6_000), 4_600, Trigger::Schedule, "abst", false).unwrap();
        a.close("abst", RunOutcome::Succeeded { output: None }, 4_700);
        assert_eq!(
            a.consecutive_infra_failures(),
            1,
            "an abstained (output-less) Succeeded check counts"
        );

        fail(&mut a, "f3", 5_000);
        assert_eq!(a.consecutive_infra_failures(), 2);

        // A verdict-bearing row — even a Failed one — is a conclusion, not
        // an infra failure: it resets the trailing count (monitor-handoff R6).
        fail(&mut a, "v1", 6_000);
        let v1 = a.runs.iter_mut().find(|r| r.id == "v1").unwrap();
        v1.verdict = Some(Verdict {
            outcome: VerdictOutcome::Fail,
            note: "experiment died".into(),
        });
        assert_eq!(a.consecutive_infra_failures(), 0, "verdict row resets");

        fail(&mut a, "f4", 7_000);
        assert_eq!(a.consecutive_infra_failures(), 1, "count restarts after");

        // fix(review) #5: a near-miss block — an opened ```verdict fence that
        // never parsed — counts like an unreadable check, extending the
        // trailing streak instead of resetting it.
        a.claim(Some(9_000), 8_000, Trigger::Schedule, "m1", false).unwrap();
        a.close(
            "m1",
            RunOutcome::Succeeded {
                output: Some("```verdict\nPASS: all good\n```".into()),
            },
            8_100,
        );
        assert_eq!(
            a.consecutive_infra_failures(),
            2,
            "an opened-but-unparseable block counts (near-miss)"
        );
    }

    // fix(review) #5 (monitor-handoff R7 — the plan's Risks promise:
    // escalation converts persistent non-compliance into a visible broken
    // signal): a Succeeded verdict-less row whose output contains a
    // verdict-fence OPENER that never parsed (decorated outcome, lowercase,
    // unclosed fence) counts as unreadable in the walk; plain not-done text
    // (no opener) still resets. Opener detection is the parser's own rule
    // (`verdict::contains_verdict_opener`), so the two cannot drift. Accepted
    // miss (documented on the walk): the stored output is the R8 tail — a
    // cap that cut the opener off reads as a plain not-done check.
    #[test]
    fn near_miss_verdict_blocks_count_as_unreadable_in_the_walk() {
        let mut a = automation(script_mode());
        a.monitor = true;

        let succeed_with = |a: &mut Automation, id: &str, t: u64, out: &str| {
            a.claim(Some(t + 1000), t, Trigger::Schedule, id, false).unwrap();
            a.close(
                id,
                RunOutcome::Succeeded {
                    output: Some(out.into()),
                },
                t + 1,
            );
        };

        // Decorated outcome line: the opener is there, the block never parses.
        succeed_with(&mut a, "m1", 1_000, "checked.\n```verdict\nPASS: all good\n```");
        assert_eq!(a.consecutive_infra_failures(), 1, "near-miss counts");

        // Lowercase outcome behind an unclosed fence: still a near-miss.
        succeed_with(&mut a, "m2", 2_000, "```verdict\npass\nprobably fine");
        assert_eq!(a.consecutive_infra_failures(), 2, "the streak grows");

        // Plain not-done text without an opener resets — the healthy
        // long-experiment case must never escalate.
        succeed_with(&mut a, "ok", 3_000, "still training — nothing to report");
        assert_eq!(
            a.consecutive_infra_failures(),
            0,
            "a readable not-done check resets"
        );
    }

    // monitor-handoff R4 + R8/U7: the history cap never evicts a
    // verdict-bearing row (the monitor's durable verdict record) — nor, as
    // before, a Running one; the oldest verdict-less terminal row goes
    // instead.
    #[test]
    fn history_eviction_preserves_verdict_bearing_and_running_rows() {
        let mut a = automation(script_mode());
        a.monitor = true;
        // The verdict run closes first...
        a.claim(Some(60_000), 10, Trigger::Schedule, "verdict-run", false)
            .unwrap();
        a.close(
            "verdict-run",
            RunOutcome::Failed {
                error: "experiment died".into(),
                exit_code: None,
                output: None,
            },
            20,
        );
        a.runs[0].verdict = Some(Verdict {
            outcome: VerdictOutcome::Fail,
            note: "experiment died".into(),
        });
        // ...a later run is still in flight...
        a.claim(Some(120_000), 30, Trigger::Schedule, "live", false).unwrap();
        // ...and skip pressure pushes the history well past the cap.
        for i in 0..RUN_HISTORY_CAP as u64 + 5 {
            a.skip(100 + i, Trigger::Schedule, "overlap", &format!("r{i}"));
        }

        assert_eq!(a.runs.len(), RUN_HISTORY_CAP);
        assert!(
            a.runs.iter().any(|r| r.id == "verdict-run"),
            "the verdict-bearing row survives eviction"
        );
        assert!(
            a.runs
                .iter()
                .any(|r| r.id == "live" && r.status == RunStatus::Running),
            "the Running row survives as before"
        );
        assert_eq!(a.runs[0].id, "verdict-run", "protected rows are the oldest survivors");
    }

    // monitor-handoff R9: monitors default retry_on_interrupt ON at
    // creation (an app-restart-interrupted check re-runs once); ordinary
    // automations keep the interrupt-resilience default of off.
    #[test]
    fn monitors_default_retry_on_interrupt_on() {
        assert!(default_retry_on_interrupt(true));
        assert!(!default_retry_on_interrupt(false));
    }

    // R7: in-flight is exactly "a Running row exists" — skips and closed
    // rows never count.
    #[test]
    fn in_flight_is_true_only_while_a_run_row_is_running() {
        let mut a = automation(script_mode());
        assert!(!a.in_flight());

        a.skip(50_000, Trigger::Schedule, "capacity", "r0");
        assert!(!a.in_flight(), "skipped rows are born terminal");

        a.claim(Some(360_000), 61_000, Trigger::Schedule, "r1", false)
            .unwrap();
        assert!(a.in_flight());

        a.close("r1", RunOutcome::Succeeded { output: None }, 70_000);
        assert!(!a.in_flight());
    }
}
