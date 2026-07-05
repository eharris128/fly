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
    /// Spawn a fresh pane running `claude "<prompt>"` in the automation's cwd
    /// (R9). `model`/`effort` (automations-workspace-and-model plan, U1 —
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
    pub fn claim(
        &mut self,
        next_run_at: Option<u64>,
        now_ms: u64,
        trigger: Trigger,
        run_id: &str,
    ) -> Result<(), ClaimError> {
        if !self.enabled {
            return Err(ClaimError::Disabled);
        }
        // A retry consumes no occurrence (like a manual run) — only a scheduled
        // claim records the occurrence it is burning.
        let scheduled_for = match trigger {
            Trigger::Schedule => self.next_run_at,
            Trigger::Manual | Trigger::Retry => None,
        };
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
    fn push_row(&mut self, row: RunRow) {
        self.runs.push(row);
        while self.runs.len() > RUN_HISTORY_CAP {
            let Some(oldest_terminal) = self.runs.iter().position(|r| r.status.is_terminal())
            else {
                break; // nothing terminal to evict — never drop a Running row
            };
            self.runs.remove(oldest_terminal);
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
            a.claim(Some(360_000), 61_000, Trigger::Schedule, "r1"),
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
            a.claim(Some(360_000), 61_000, Trigger::Schedule, "r1"),
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
            effort: None,
        });
        a.next_run_at = None; // paused

        assert_eq!(a.claim(None, 61_000, Trigger::Manual, "r1"), Ok(()));
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

        assert_eq!(a.claim(keep, 61_000, Trigger::Retry, "r1"), Ok(()));
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
        a.claim(Some(360_000), 61_000, Trigger::Schedule, "r1")
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
        a.claim(Some(360_000), 61_000, Trigger::Schedule, "r1")
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
        a.claim(Some(360_000), 61_000, Trigger::Schedule, "r1")
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
        a.claim(Some(360_000), 61_000, Trigger::Schedule, "r1")
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
            effort: None,
        });
        // A claim mid-history: an agent run that stays Running while later
        // occurrences skip past it.
        a.claim(Some(60_000), 10, Trigger::Schedule, "live").unwrap();
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
        a.claim(Some(360_000), 61_000, Trigger::Schedule, "r1")
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
        a.claim(Some(360_000), 61_000, Trigger::Schedule, "r1")
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

        let back: Automation = serde_json::from_value(v).unwrap();
        assert_eq!(back, a);

        // Agent mode carries its prompt under the same tag convention.
        let agent = serde_json::to_value(Mode::Agent {
            prompt: "summarize CI".into(),
            model: None,
            effort: None,
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
            effort: Some("high".into()),
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
    // default).
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
            }
        );
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

    // R7: in-flight is exactly "a Running row exists" — skips and closed
    // rows never count.
    #[test]
    fn in_flight_is_true_only_while_a_run_row_is_running() {
        let mut a = automation(script_mode());
        assert!(!a.in_flight());

        a.skip(50_000, Trigger::Schedule, "capacity", "r0");
        assert!(!a.in_flight(), "skipped rows are born terminal");

        a.claim(Some(360_000), 61_000, Trigger::Schedule, "r1")
            .unwrap();
        assert!(a.in_flight());

        a.close("r1", RunOutcome::Succeeded { output: None }, 70_000);
        assert!(!a.in_flight());
    }
}
