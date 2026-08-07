//! Script runner and wake gate (U5 of
//! `docs/plans/2026-07-01-002-feat-automations-plan.md`; R13, R14, R15,
//! KTD-I).
//!
//! [`ScriptRunner`] is the real [`Dispatcher`] for script-mode runs (the
//! agent arm stays unwired until U7): it spawns the stored script's
//! interpreter in **its own process group** with a scrubbed allowlist env,
//! drains stdout/stderr into 64 KiB tail-capped buffers on reader threads,
//! enforces a run deadline with SIGHUP → grace → SIGKILL escalation to the
//! whole group, reaps on a named thread, classifies the result through the
//! pure R15 wake gate ([`classify`]), and closes the run row via the injected
//! [`RunCloser`] (the manager's `close_run`, KTD-B: never under the store
//! lock — the closer takes it itself, on this runner's reaper thread).
//!
//! **Process-group discipline (KTD-I).** Claude Code and shell scripts spawn
//! nested children (the dashboard pgid finding); signaling only the direct
//! child leaks grandchildren. The child is spawned with `process_group(0)`
//! (it becomes leader of a fresh group whose pgid == its pid) and every
//! signal goes to `-pgid`. Reused-pid guard, lifted from `pty/pane.rs` to
//! the group: we only signal `-pgid` while the direct child is **unreaped**
//! — an un-`wait()`ed child is at worst a zombie, the kernel keeps its pid
//! reserved, and no new process (hence no new process *group*) can take that
//! id, so `kill(-pgid, …)` can never hit an unrelated group. The one
//! post-reap signal (in `drain_captures`, for a script that exited but left
//! pipe-holding children behind) is guarded by a liveness probe and its
//! residual reuse window (every holder exits or `setsid`s between probe and
//! signal) is documented and accepted.
//!
//! **Env (R14).** `env_clear()` + exactly the [`ENV_ALLOWLIST`] (forwarded
//! from the app env when present) + `FLY_AUTOMATION_ID` /
//! `FLY_AUTOMATION_RUN_ID`. `FLY_PANE_TOKEN` / `FLY_SOCKET_PATH` are never
//! present (asserted in tests, the `notify/command.rs` refactor guard).
//! PATH note (the classic cron surprise): the allowlisted `PATH` is the app
//! process's **launch** PATH — a GNOME-launcher env, not the interactive
//! shell's — so a script depending on `nvm`/`pyenv`/`cargo` shims must set
//! them up itself.
//!
//! **Capture (R13/R15).** Reader threads keep the **tail** on overflow (the
//! sentinel is a trailing line — same rationale as `model::output_tail`).
//! This 64 KiB capture cap is distinct from the 8 KiB *storage* tail the
//! model applies at close: the wake-gate sentinel is evaluated on the
//! untruncated capture tail, before the storage cap. Tail cuts happen at
//! byte granularity and `from_utf8_lossy` renders a split leading char as
//! U+FFFD — head damage only, never the trailing sentinel line.
//!
//! **Alert hand-off (U6 seam).** An alert-classified run closes SUCCEEDED
//! with its output captured and emits an [`AlertEvent`] through the
//! [`AlertSink`]. The default sink is a no-op; the production sink (U6,
//! wired by `lib.rs::set_alert_sink` via [`ScriptRunner::set_alert_sink`])
//! appends to the sanitized alerts log and raises `Reason::Alert` on the
//! sink pane — a runner without one shows alerts only as the run row's
//! captured output (the tests' shape).

use std::collections::HashMap;
use std::io::Read;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::model::{Automation, Mode, RunOutcome};
use super::Dispatcher;

/// Global in-flight script cap (KTD-D; the `notify/command.rs` MAX_INFLIGHT
/// precedent). The sweep's capacity probe skips pre-claim at this bound;
/// dispatch re-checks it so a manual run cannot overshoot either.
pub const MAX_INFLIGHT_SCRIPTS: usize = 4;

/// Capture cap per stream (R13): 64 KiB, keeping the TAIL on overflow.
/// Distinct from the 8 KiB storage tail (`model::OUTPUT_TAIL_CAP_BYTES`).
pub const CAPTURE_CAP_BYTES: usize = 64 * 1024;

/// R13 timeout bounds, applied at RUN time regardless of the stored value
/// (the store is same-UID-writable — untrusted numeric input).
///
/// `TIMEOUT_MAX_MS` raised 15 min -> 2 h on 2026-08-07. The old ceiling was
/// not a safety property — the clamp exists to bound untrusted numeric input,
/// not to assert that 15 minutes is a safe maximum — and it silently capped a
/// real scheduled job whose runtime is set by how much work the outside world
/// produced that day (2 min on a quiet day, 35-45 after a backlog). A cap
/// below the workload's natural spread fails *unpredictably*: every by-hand
/// test passes and the scheduled run dies.
///
/// The DEFAULT is deliberately unchanged: unset still means 2 minutes, so a
/// script that forgets `--timeout` is still bounded tightly. Only the ceiling
/// an author may opt into moved.
pub const TIMEOUT_MIN_MS: u64 = 1_000;
pub const TIMEOUT_MAX_MS: u64 = 2 * 60 * 60 * 1_000;
pub const TIMEOUT_DEFAULT_MS: u64 = 120_000;

/// R14: the exact allowlist rebuilt after `env_clear()`. Deliberately
/// narrower than a login shell — notably no `SSH_AUTH_SOCK` (an unattended
/// scheduled script must not inherit the user's live SSH identity).
pub const ENV_ALLOWLIST: [&str; 8] = [
    "PATH", "HOME", "USER", "SHELL", "LANG", "LC_ALL", "LC_CTYPE", "TERM",
];

/// Deadline escalation grace (R13): SIGHUP → this → SIGKILL.
const TIMEOUT_KILL_GRACE: Duration = Duration::from_secs(2);
/// Killer-seam grace (delete R23 / shutdown R5). Deliberately short — the
/// seam runs on the shutdown path, which must stay fast; a script that
/// ignores SIGHUP for 200ms just meets SIGKILL sooner (`pty/pane.rs` GRACE).
const SEAM_KILL_GRACE: Duration = Duration::from_millis(200);
/// try_wait / group-liveness poll interval.
const REAP_POLL: Duration = Duration::from_millis(25);
/// How long the reaper waits for a reader thread's EOF after the child is
/// reaped, per attempt, before concluding lingering group members hold the
/// pipes (and killing them). Bounds closure latency for a script that leaks
/// background children.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

// ---- injected seams -----------------------------------------------------------

/// How the reaper closes a run row: `(automation_id, run_id, outcome)`.
/// `lib.rs` wires `AutomationManager::close_run` (which takes the store lock
/// itself and emits `automation://changed` after releasing it — KTD-B);
/// tests wire a collector.
pub type RunCloser = Arc<dyn Fn(&str, &str, RunOutcome) + Send + Sync>;

/// An alert-classified run (R15: exit 0 with non-silent stdout), handed off
/// for surfacing.
///
/// The default sink is a no-op; the production sink (U6, installed via
/// [`ScriptRunner::set_alert_sink`]) does the sanitized alerts-log append +
/// the `Signal { reason: Alert, tier: Cli }` raise on the sink pane (R16).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertEvent {
    pub automation_id: String,
    pub automation_name: String,
    pub run_id: String,
    /// First non-empty stdout line (banner/log-line text, pre-sanitization —
    /// U6 sanitizes at the surface).
    pub first_line: String,
    /// The full combined capture (64 KiB capture tail, **before** the 8 KiB
    /// storage cap).
    pub capture: String,
}

/// U6 seam: where alert-classified runs go. Defaults to a no-op (see
/// [`AlertEvent`]).
pub type AlertSink = Arc<dyn Fn(AlertEvent) + Send + Sync>;

// ---- the wake gate (R15) — pure, exhaustively tested ---------------------------

/// How the script run ended, as seen by the reaper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitOutcome {
    /// The child exited on its own. Signal deaths map to the shell
    /// convention `128 + signo` (a delete/shutdown SIGKILL shows as 137);
    /// those rows usually close first via the manager's own failed close, so
    /// the reaper's close is an AlreadyClosed no-op.
    Code(i32),
    /// The R13 deadline expired and the group was killed. `hard_killed` is
    /// true when the SIGHUP grace lapsed and SIGKILL was needed.
    TimedOut { hard_killed: bool },
}

/// The R15 verdict. Output *storage* is decided separately ([`conclude`]);
/// this is only the classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptVerdict {
    /// Exit 0 and stdout says nothing actionable: empty/whitespace-only, a
    /// trailing `{"wakeAgent": false}` sentinel line, or stderr-only output.
    SilentSuccess,
    /// Exit 0 with any other stdout: surface it (AE3).
    Alert {
        /// First non-empty stdout line, trimmed.
        first_line: String,
    },
    /// Non-zero exit or timeout. `exit_code` is `None` for timeouts.
    Failed { exit_code: Option<i32> },
}

/// The R15 wake gate. `stderr` participates in *storage*, never in the
/// verdict (stderr-only output on exit 0 is a silent success) — the
/// parameter is kept so the rule is stated where it is decided.
///
/// The sentinel is evaluated on the **last non-empty stdout line** of the
/// untruncated 64 KiB capture tail, before the 8 KiB storage cap. A sentinel
/// anywhere else (mid-output) does not suppress — the script "said more"
/// after opting out, so it wakes.
pub fn classify(exit: &ExitOutcome, stdout: &str, _stderr: &str) -> ScriptVerdict {
    let code = match exit {
        ExitOutcome::Code(c) => *c,
        ExitOutcome::TimedOut { .. } => return ScriptVerdict::Failed { exit_code: None },
    };
    if code != 0 {
        return ScriptVerdict::Failed {
            exit_code: Some(code),
        };
    }
    match stdout.lines().rev().find(|l| !l.trim().is_empty()) {
        None => ScriptVerdict::SilentSuccess,
        Some(line) if wake_suppressed(line) => ScriptVerdict::SilentSuccess,
        Some(_) => ScriptVerdict::Alert {
            first_line: stdout
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim()
                .to_owned(),
        },
    }
}

/// Whether a line is the `{"wakeAgent": false}` sentinel (R15).
///
/// **Tolerance decision (documented):** the line must parse as a JSON
/// *object* whose `wakeAgent` key is exactly the JSON boolean `false`.
/// Extra keys are accepted — bb parity says the line *is* the object, and
/// tolerating extra keys lets a script attach metadata without breaking
/// suppression. Key name is case-sensitive; `"false"` (a string), `0`, or
/// `null` do not suppress; a bare `false`, an array, or any non-object never
/// suppresses.
fn wake_suppressed(line: &str) -> bool {
    matches!(
        serde_json::from_str::<serde_json::Value>(line.trim()),
        Ok(serde_json::Value::Object(o)) if o.get("wakeAgent") == Some(&serde_json::Value::Bool(false))
    )
}

/// Verdict → row closure (R15/AE1): the pure half the reaper applies.
/// Returns the outcome plus, for alerts, the first stdout line for the
/// [`AlertEvent`].
///
/// **Output-storage decision (documented, AE1):** the row stores the
/// *combined* capture (stdout, then a `[stderr]`-labeled stderr section)
/// only when it is non-empty after trimming — a truly silent tick stores
/// `output: null`, and whitespace-only noise is treated as nothing. This
/// applies to all three verdicts; alerts and failures therefore always
/// carry whatever the script actually said (the storage tail cap is applied
/// later by `model::close`).
pub fn conclude(
    exit: &ExitOutcome,
    timeout_ms: u64,
    stdout: &str,
    stderr: &str,
) -> (RunOutcome, Option<String>) {
    let output = combined_capture(stdout, stderr);
    match classify(exit, stdout, stderr) {
        ScriptVerdict::SilentSuccess => (RunOutcome::Succeeded { output }, None),
        ScriptVerdict::Alert { first_line } => {
            (RunOutcome::Succeeded { output }, Some(first_line))
        }
        ScriptVerdict::Failed { exit_code } => (
            RunOutcome::Failed {
                error: failure_error(exit, timeout_ms),
                exit_code,
                output,
            },
            None,
        ),
    }
}

/// The failed row's error string: `exit status N`, or the timeout detail.
fn failure_error(exit: &ExitOutcome, timeout_ms: u64) -> String {
    match exit {
        ExitOutcome::Code(c) => format!("exit status {c}"),
        ExitOutcome::TimedOut { hard_killed } => format!(
            "timed out after {timeout_ms}ms{}",
            if *hard_killed { " (SIGKILL)" } else { "" }
        ),
    }
}

/// Combine the two capture streams for storage (see [`conclude`] for the
/// documented choice). Whitespace-only streams count as empty.
fn combined_capture(stdout: &str, stderr: &str) -> Option<String> {
    match (stdout.trim().is_empty(), stderr.trim().is_empty()) {
        (true, true) => None,
        (false, true) => Some(stdout.to_owned()),
        (true, false) => Some(format!("[stderr]\n{stderr}")),
        (false, false) => Some(format!(
            "{}\n[stderr]\n{stderr}",
            stdout.trim_end_matches('\n')
        )),
    }
}

/// Clamp a stored `timeout_ms` at run time (R13): `0` means "unset" and
/// takes the default; anything else clamps into `[1s, 900s]`. Saturating by
/// construction — the result bounds every deadline computation.
pub fn clamp_timeout_ms(stored: u64) -> u64 {
    if stored == 0 {
        return TIMEOUT_DEFAULT_MS;
    }
    stored.clamp(TIMEOUT_MIN_MS, TIMEOUT_MAX_MS)
}

/// Resolve a stored interpreter name to a fixed binary name — a closed set,
/// never a free-form string (the U9 argv-injection guard, enforced here too
/// because the store file is same-UID-writable). Resolution to an absolute
/// path is left to `PATH` (the launch PATH — see the module doc).
pub fn resolve_interpreter(name: &str) -> Result<&'static str, String> {
    match name {
        "bash" => Ok("bash"),
        "sh" => Ok("sh"),
        "node" => Ok("node"),
        "python3" => Ok("python3"),
        other => Err(format!(
            "unknown interpreter {other:?} (allowed: bash, sh, node, python3)"
        )),
    }
}

/// Reject an option-like script path before it reaches `Command::arg` — the
/// argv-injection guard's second half (R14/U9). The interpreter enum blocks a
/// free-form interpreter, but the script path is passed as the interpreter's
/// first argv, so a path beginning with `-` (e.g. `-cPAYLOAD`, `--eval=…`)
/// would be interpreted as a *flag* by bash/sh/python3 rather than a file —
/// arbitrary execution. The manager only ever writes `script_file` as a store
/// path under `automation-scripts/`, so this fires only on a tampered store
/// (same-UID trust domain, KTD-K) — fail the run loudly rather than exec an
/// attacker-chosen flag.
pub fn checked_script_path(script_file: &str) -> Result<(), String> {
    if script_file.starts_with('-') {
        return Err(format!(
            "refusing to run script with option-like path {script_file:?} \
             (argv-injection guard)"
        ));
    }
    Ok(())
}

// ---- capture buffers ------------------------------------------------------------

/// A byte buffer that keeps at most `cap` bytes, discarding from the FRONT on
/// overflow — the tail survives (R15: the sentinel is a trailing line; the
/// same reason `model::output_tail` keeps the end). Cuts are byte-granular;
/// [`TailBuf::into_string`]'s lossy conversion renders a split leading char
/// as U+FFFD (head damage only).
struct TailBuf {
    buf: Vec<u8>,
    cap: usize,
}

impl TailBuf {
    fn new(cap: usize) -> TailBuf {
        TailBuf {
            buf: Vec::new(),
            cap,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        if chunk.len() >= self.cap {
            self.buf.clear();
            self.buf
                .extend_from_slice(&chunk[chunk.len().saturating_sub(self.cap)..]);
            return;
        }
        self.buf.extend_from_slice(chunk);
        if self.buf.len() > self.cap {
            let cut = self.buf.len().saturating_sub(self.cap);
            self.buf.drain(..cut);
        }
    }

    fn into_string(self) -> String {
        String::from_utf8_lossy(&self.buf).into_owned()
    }
}

/// Drain a pipe to EOF into a string keeping only the trailing `cap` bytes
/// (runs on a reader thread). Shared with the headless check runner
/// (headless-monitor-checks U3 — its bounded stderr tail), which passes its
/// own, much smaller cap.
pub(crate) fn drain_reader(mut r: impl Read, cap: usize) -> String {
    let mut tail = TailBuf::new(cap);
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => tail.push(&buf[..n]),
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    tail.into_string()
}

/// Start a reader thread; its final capture arrives on the returned channel.
/// If the thread cannot start (rare), the sender drops and the reaper's
/// bounded recv treats the stream as empty — degraded, never wedged.
/// Shared with the headless check runner (see [`drain_reader`]).
pub(crate) fn spawn_reader(name: &str, r: impl Read + Send + 'static, cap: usize) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    let _ = std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let _ = tx.send(drain_reader(r, cap));
        });
    rx
}

// ---- group signaling -------------------------------------------------------------

/// Signal the whole process group. ESRCH (already gone) is ignored. See the
/// module doc for when this is safe to call.
fn signal_group(pgid: i32, sig: i32) {
    // SAFETY: kill(2) is always safe to call; a stale pgid yields ESRCH.
    unsafe {
        libc::kill(-pgid, sig);
    }
}

/// Whether the group still has signalable members.
fn group_alive(pgid: i32) -> bool {
    // SAFETY: kill(2) with signal 0 only error-checks.
    unsafe { libc::kill(-pgid, 0) == 0 }
}

// ---- the runner -------------------------------------------------------------------

/// One in-flight script run: the group to signal and whether its direct
/// child has been reaped (the guard that makes `-pgid` signaling safe — see
/// the module doc).
struct InflightScript {
    pgid: i32,
    reaped: Arc<AtomicBool>,
}

/// The script-mode [`Dispatcher`] (R13/R14/R15). Construct with the row
/// closer, then wire into the manager's seams (`set_dispatcher`,
/// `set_script_killer`, `set_script_capacity`) — `lib.rs` does this at
/// startup.
pub struct ScriptRunner {
    closer: RunCloser,
    /// U6 seam (see [`AlertSink`]); swapped in after construction, read at
    /// alert time so late registration still catches later runs.
    alert_sink: Arc<Mutex<AlertSink>>,
    /// In-flight registry, keyed by run id; entries are inserted before
    /// dispatch returns and removed by the reaper. Shared with reaper
    /// threads via Arc. Lock order: never held across a store-lock
    /// acquisition (the capacity probe takes only this lock; the closer
    /// takes only the store lock; they never nest).
    inflight: Arc<Mutex<HashMap<String, InflightScript>>>,
}

impl ScriptRunner {
    pub fn new(closer: RunCloser) -> ScriptRunner {
        ScriptRunner {
            closer,
            alert_sink: Arc::new(Mutex::new(Arc::new(|_ev: AlertEvent| {}))),
            inflight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Replace the alert sink (U6). Applies to runs concluding after the
    /// call — an alert concluding mid-swap uses whichever sink it read.
    pub fn set_alert_sink(&self, sink: AlertSink) {
        *self.alert_sink.lock().unwrap() = sink;
    }

    /// Current in-flight count (the sweep's capacity probe input).
    pub fn inflight_count(&self) -> usize {
        self.inflight.lock().unwrap().len()
    }

    /// The KTD-D capacity probe: `inflight < 4`. Cheap and re-entrancy-free
    /// (takes only the registry lock), so it is safe inside the sweep's
    /// mutate closure per the manager's probe contract.
    pub fn has_capacity(&self) -> bool {
        self.inflight_count() < MAX_INFLIGHT_SCRIPTS
    }

    /// The manager's [`super::ScriptKiller`] seam (delete R23, shutdown R5):
    /// SIGHUP the run's group, wait a short grace, then SIGKILL. Safe while
    /// the entry exists (`reaped` false ⇒ the leader is at worst a zombie ⇒
    /// the pgid cannot be recycled — module doc). No-op for unknown or
    /// already-reaped runs. The reaper thread observes the death and closes
    /// the row (usually an AlreadyClosed no-op — the manager closes
    /// deleted/interrupted rows itself).
    pub fn kill_run(&self, run_id: &str) {
        let Some((pgid, reaped)) = self
            .inflight
            .lock()
            .unwrap()
            .get(run_id)
            .map(|e| (e.pgid, Arc::clone(&e.reaped)))
        else {
            return;
        };
        if reaped.load(Ordering::Acquire) {
            return;
        }
        signal_group(pgid, SIGHUP);
        let end = Instant::now() + SEAM_KILL_GRACE;
        loop {
            if reaped.load(Ordering::Acquire) || !group_alive(pgid) {
                return;
            }
            if Instant::now() >= end {
                break;
            }
            std::thread::sleep(REAP_POLL);
        }
        if !reaped.load(Ordering::Acquire) {
            signal_group(pgid, SIGKILL);
        }
    }

    /// Kill every in-flight group (R13 shutdown half). The manager's
    /// shutdown reaches the same result through the killer seam per running
    /// row; this is the registry-complete belt-and-braces the plan asks for
    /// (covers a group whose row already closed, e.g. after a delete).
    pub fn kill_all_inflight(&self) {
        let ids: Vec<String> = self.inflight.lock().unwrap().keys().cloned().collect();
        for id in ids {
            self.kill_run(&id);
        }
    }

    /// Spawn a script run and return fast (the [`Dispatcher`] contract): the
    /// child, its readers, and the reaper thread are started; everything
    /// after — deadline, classification, row closure — happens on the named
    /// `fly-automation-script` reaper thread. Never SIGCHLD (GTK owns it):
    /// the reaper `wait()`s its own child.
    ///
    /// Errors (bad interpreter, capacity, nonexistent cwd → spawn failure)
    /// return `Err` so the manager's existing dispatch-failure path closes
    /// the row failed and recomputes the schedule (R3).
    fn dispatch(&self, automation: &Automation, run_id: &str) -> Result<(), String> {
        let Mode::Script {
            script_file,
            interpreter,
            timeout_ms,
        } = &automation.mode
        else {
            return Err(format!(
                "automation {} is not script-mode",
                automation.id
            ));
        };
        let interpreter = resolve_interpreter(interpreter)?;
        checked_script_path(script_file)?;
        let timeout_ms = clamp_timeout_ms(*timeout_ms);

        let mut cmd = Command::new(interpreter);
        cmd.arg(script_file)
            .current_dir(&automation.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // KTD-I: fresh process group, pgid == child pid.
            .process_group(0);
        // R14: allowlist, never inheritance.
        cmd.env_clear();
        for key in ENV_ALLOWLIST {
            if let Some(val) = std::env::var_os(key) {
                cmd.env(key, val);
            }
        }
        cmd.env("FLY_AUTOMATION_ID", &automation.id);
        cmd.env("FLY_AUTOMATION_RUN_ID", run_id);

        // Capacity check + spawn + registration under ONE registry hold, so
        // concurrent dispatches (sweep + manual) cannot overshoot the cap.
        // The hold spans a fork/exec — milliseconds; the only other takers
        // (probe, killer, reaper removal) tolerate that.
        let mut child = {
            let mut inflight = self.inflight.lock().unwrap();
            if inflight.len() >= MAX_INFLIGHT_SCRIPTS {
                return Err(format!(
                    "script capacity exhausted ({MAX_INFLIGHT_SCRIPTS} runs in flight)"
                ));
            }
            let child = cmd.spawn().map_err(|e| {
                format!(
                    "could not start script (interpreter {interpreter}, cwd {:?}): {e}",
                    automation.cwd
                )
            })?;
            inflight.insert(
                run_id.to_owned(),
                InflightScript {
                    pgid: child.id() as i32,
                    reaped: Arc::new(AtomicBool::new(false)),
                },
            );
            child
        };
        let pgid = child.id() as i32;
        let reaped = Arc::clone(&self.inflight.lock().unwrap()[run_id].reaped);

        // Reader threads drain the pipes to EOF (R13: 64 KiB tail caps).
        // stdout/stderr are Some by construction (Stdio::piped above).
        let rx_out = spawn_reader(
            "fly-automation-script-out",
            child.stdout.take().expect("stdout piped"),
            CAPTURE_CAP_BYTES,
        );
        let rx_err = spawn_reader(
            "fly-automation-script-err",
            child.stderr.take().expect("stderr piped"),
            CAPTURE_CAP_BYTES,
        );

        let job = ReapJob {
            automation_id: automation.id.clone(),
            automation_name: automation.name.clone(),
            run_id: run_id.to_owned(),
            pgid,
            reaped: Arc::clone(&reaped),
            timeout_ms,
            rx_out,
            rx_err,
            closer: Arc::clone(&self.closer),
            alert_sink: Arc::clone(&self.alert_sink),
            inflight: Arc::clone(&self.inflight),
        };
        // The child rides to the reaper thread in a cell so the spawn-failure
        // arm can take it back for an inline kill + reap (a moved-into-closure
        // child would be unrecoverable after a failed thread spawn).
        let child_cell = Arc::new(Mutex::new(Some(child)));
        let thread_cell = Arc::clone(&child_cell);
        if let Err(e) = std::thread::Builder::new()
            .name("fly-automation-script".into())
            .spawn(move || {
                let child = thread_cell
                    .lock()
                    .unwrap()
                    .take()
                    .expect("child handed to exactly one reaper");
                reap(child, job);
            })
        {
            // No reaper means no deadline and no closure: kill + reap inline
            // and fail the dispatch (the manager closes the row).
            if let Some(mut child) = child_cell.lock().unwrap().take() {
                signal_group(pgid, SIGKILL);
                let _ = child.wait();
            }
            reaped.store(true, Ordering::Release);
            self.inflight.lock().unwrap().remove(run_id);
            return Err(format!("could not start script reaper thread: {e}"));
        }
        Ok(())
    }
}

/// The runner is the real [`Dispatcher`] for scripts; the agent arm stays
/// the unwired error until U7 lands its dispatcher (which will compose with
/// this one in `lib.rs`).
impl Dispatcher for ScriptRunner {
    fn dispatch_agent(
        &self,
        _a: &Automation,
        _run_id: &str,
        _launch: &super::ResolvedLaunch,
        _headless: bool,
    ) -> Result<(), String> {
        Err("agent dispatch not wired yet (U7)".into())
    }
    fn dispatch_script(&self, a: &Automation, run_id: &str) -> Result<(), String> {
        self.dispatch(a, run_id)
    }
}

const SIGHUP: i32 = libc::SIGHUP;
const SIGKILL: i32 = libc::SIGKILL;

/// Everything the reaper thread needs, moved off the dispatch path.
struct ReapJob {
    automation_id: String,
    automation_name: String,
    run_id: String,
    pgid: i32,
    reaped: Arc<AtomicBool>,
    timeout_ms: u64,
    rx_out: Receiver<String>,
    rx_err: Receiver<String>,
    closer: RunCloser,
    alert_sink: Arc<Mutex<AlertSink>>,
    inflight: Arc<Mutex<HashMap<String, InflightScript>>>,
}

/// The reaper (named thread `fly-automation-script`): wait with deadline,
/// escalate on expiry, reap, drain captures, classify, close, alert.
fn reap(mut child: Child, job: ReapJob) {
    let deadline = Instant::now() + Duration::from_millis(job.timeout_ms);
    let exit = loop {
        match child.try_wait() {
            Ok(Some(status)) => break ExitOutcome::Code(exit_code_of(&status)),
            Ok(None) => {
                if Instant::now() >= deadline {
                    break kill_group_and_reap(&mut child, job.pgid);
                }
                std::thread::sleep(REAP_POLL);
            }
            Err(_) => {
                // try_wait failed (should not happen for our own child):
                // fall back to a blocking reap so nothing zombies.
                break match child.wait() {
                    Ok(status) => ExitOutcome::Code(exit_code_of(&status)),
                    Err(_) => ExitOutcome::Code(-1),
                };
            }
        }
    };
    // The direct child is reaped in every arm above. Mark it before the
    // (possibly slow) drain so the killer seam stops signaling a pgid that
    // is no longer pinned by an unreaped leader.
    job.reaped.store(true, Ordering::Release);

    let (stdout, stderr) = drain_captures(job.pgid, &job.rx_out, &job.rx_err);

    // Registry entry removed on reap — frees a capacity slot before the row
    // closes (KTD-D ordering is unaffected: the row was persisted at claim).
    job.inflight.lock().unwrap().remove(&job.run_id);

    let (outcome, alert_first_line) = conclude(&exit, job.timeout_ms, &stdout, &stderr);
    (job.closer)(&job.automation_id, &job.run_id, outcome);
    if let Some(first_line) = alert_first_line {
        let sink = Arc::clone(&*job.alert_sink.lock().unwrap());
        sink(AlertEvent {
            automation_id: job.automation_id,
            automation_name: job.automation_name,
            run_id: job.run_id,
            first_line,
            capture: combined_capture(&stdout, &stderr).unwrap_or_default(),
        });
    }
}

/// R13 deadline escalation: SIGHUP the group, grace, SIGKILL the group, then
/// reap the direct child (safe order: the child is unreaped throughout, so
/// the pgid is pinned — module doc).
fn kill_group_and_reap(child: &mut Child, pgid: i32) -> ExitOutcome {
    signal_group(pgid, SIGHUP);
    let grace_end = Instant::now() + TIMEOUT_KILL_GRACE;
    let mut hard_killed = false;
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        if Instant::now() >= grace_end {
            hard_killed = true;
            signal_group(pgid, SIGKILL);
            let _ = child.wait();
            break;
        }
        std::thread::sleep(REAP_POLL);
    }
    ExitOutcome::TimedOut { hard_killed }
}

/// Map an exit status to the R15 code: `code()`, or the shell convention
/// `128 + signo` for signal deaths.
fn exit_code_of(status: &std::process::ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128i32.saturating_add(status.signal().unwrap_or(0)))
}

/// Collect both captures after the direct child is reaped. Readers EOF when
/// every pipe-holder exits; a script that leaked background children keeps
/// the pipes open, so after [`DRAIN_GRACE`] the lingering group is killed
/// (SIGHUP → short grace → SIGKILL) and the drain retried.
///
/// This is the one post-reap group signal: the pgid is no longer pinned by
/// a zombie leader, but a blocked reader implies a live pipe-holder, which
/// (absent a deliberate `setsid`) is a live group member keeping the pgid
/// unrecyclable; the probe-then-signal residue for a daemonizing script is
/// documented and accepted (module doc).
fn drain_captures(pgid: i32, rx_out: &Receiver<String>, rx_err: &Receiver<String>) -> (String, String) {
    let mut out = rx_out.recv_timeout(DRAIN_GRACE).ok();
    let mut err = rx_err.recv_timeout(if out.is_some() {
        DRAIN_GRACE
    } else {
        Duration::from_millis(50)
    })
    .ok();
    if out.is_none() || err.is_none() {
        if group_alive(pgid) {
            signal_group(pgid, SIGHUP);
            let end = Instant::now() + SEAM_KILL_GRACE;
            while group_alive(pgid) && Instant::now() < end {
                std::thread::sleep(REAP_POLL);
            }
            if group_alive(pgid) {
                signal_group(pgid, SIGKILL);
            }
        }
        if out.is_none() {
            out = rx_out.recv_timeout(DRAIN_GRACE).ok();
        }
        if err.is_none() {
            err = rx_err.recv_timeout(DRAIN_GRACE).ok();
        }
    }
    (out.unwrap_or_default(), err.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::super::model::{Origin, RunOutcome};
    use super::*;

    // ================= the wake gate (pure, no processes) =================

    fn code(c: i32) -> ExitOutcome {
        ExitOutcome::Code(c)
    }

    // R15/AE1: exit 0 with empty or whitespace-only stdout is a silent
    // success; stderr never participates in the verdict.
    #[test]
    fn classify_exit_zero_with_empty_or_whitespace_stdout_is_silent_r15() {
        assert_eq!(classify(&code(0), "", ""), ScriptVerdict::SilentSuccess);
        assert_eq!(
            classify(&code(0), "  \n\n\t \n", ""),
            ScriptVerdict::SilentSuccess
        );
        // stderr-only output: still silent (R15) — storage handles capture.
        assert_eq!(
            classify(&code(0), "", "warning: disk slow\n"),
            ScriptVerdict::SilentSuccess
        );
    }

    // R15/AE2: a trailing {"wakeAgent": false} line suppresses even with
    // earlier diagnostic output; trailing blank lines don't defeat it.
    #[test]
    fn classify_trailing_wake_agent_false_sentinel_is_silent_r15() {
        let out = "checked 12 disks\nall nominal\n{\"wakeAgent\": false}\n";
        assert_eq!(classify(&code(0), out, ""), ScriptVerdict::SilentSuccess);
        assert_eq!(
            classify(&code(0), "{\"wakeAgent\": false}\n\n  \n", ""),
            ScriptVerdict::SilentSuccess,
            "trailing whitespace lines are skipped when finding the last line"
        );
        // Documented tolerance: extra keys are accepted (the line IS the
        // object, bb parity).
        assert_eq!(
            classify(&code(0), "{\"wakeAgent\": false, \"checked\": 12}", ""),
            ScriptVerdict::SilentSuccess
        );
    }

    // R15: only the exact boolean-false object key suppresses — true,
    // strings, wrong case, and non-objects all wake.
    #[test]
    fn classify_non_sentinel_json_variants_still_alert_r15() {
        for line in [
            "{\"wakeAgent\": true}",
            "{\"wakeAgent\": \"false\"}",
            "{\"wakeagent\": false}",
            "{\"wakeAgent\": null}",
            "false",
            "[{\"wakeAgent\": false}]",
            "not json at all",
        ] {
            assert!(
                matches!(classify(&code(0), line, ""), ScriptVerdict::Alert { .. }),
                "{line:?} must not suppress"
            );
        }
    }

    // R15: a sentinel that is not the LAST non-empty line does not suppress
    // — the script said more after opting out.
    #[test]
    fn classify_sentinel_mid_output_is_an_alert_r15() {
        let out = "Disk at 93%\n{\"wakeAgent\": false}\nmore detail\n";
        assert_eq!(
            classify(&code(0), out, ""),
            ScriptVerdict::Alert {
                first_line: "Disk at 93%".into()
            }
        );
    }

    // R15: non-zero exit or timeout is failed regardless of stdout (even a
    // sentinel does not rescue a failure).
    #[test]
    fn classify_nonzero_exit_and_timeout_are_failed_r15() {
        assert_eq!(
            classify(&code(3), "{\"wakeAgent\": false}", ""),
            ScriptVerdict::Failed { exit_code: Some(3) }
        );
        assert_eq!(
            classify(&ExitOutcome::TimedOut { hard_killed: true }, "", ""),
            ScriptVerdict::Failed { exit_code: None }
        );
    }

    // AE1 + the documented storage choice: silent + empty capture stores
    // output null; non-empty captures store combined (stderr labeled);
    // failures carry the error string and code.
    #[test]
    fn conclude_stores_combined_capture_only_when_nonempty_ae1() {
        let (outcome, alert) = conclude(&code(0), 120_000, "", "");
        assert_eq!(outcome, RunOutcome::Succeeded { output: None });
        assert_eq!(alert, None);

        let (outcome, alert) = conclude(&code(0), 120_000, "  \n", "");
        assert_eq!(
            outcome,
            RunOutcome::Succeeded { output: None },
            "whitespace-only capture stores as null too"
        );
        assert_eq!(alert, None);

        let (outcome, _) = conclude(&code(0), 120_000, "", "warn\n");
        assert_eq!(
            outcome,
            RunOutcome::Succeeded {
                output: Some("[stderr]\nwarn\n".into())
            }
        );

        let (outcome, alert) = conclude(&code(0), 120_000, "Disk at 93%\n", "hint\n");
        assert_eq!(
            outcome,
            RunOutcome::Succeeded {
                output: Some("Disk at 93%\n[stderr]\nhint\n".into())
            }
        );
        assert_eq!(alert.as_deref(), Some("Disk at 93%"));

        let (outcome, _) = conclude(&code(3), 120_000, "broken\n", "");
        assert_eq!(
            outcome,
            RunOutcome::Failed {
                error: "exit status 3".into(),
                exit_code: Some(3),
                output: Some("broken\n".into())
            }
        );

        let (outcome, _) = conclude(
            &ExitOutcome::TimedOut { hard_killed: false },
            1_000,
            "",
            "",
        );
        assert_eq!(
            outcome,
            RunOutcome::Failed {
                error: "timed out after 1000ms".into(),
                exit_code: None,
                output: None
            }
        );
    }

    // R13: the run-time clamp treats the stored value as untrusted — 0 is
    // "unset" (default), everything else lands in [1s, 900s].
    #[test]
    fn clamp_timeout_ms_bounds_untrusted_stored_values_r13() {
        assert_eq!(clamp_timeout_ms(0), TIMEOUT_DEFAULT_MS);
        assert_eq!(clamp_timeout_ms(1), TIMEOUT_MIN_MS);
        assert_eq!(clamp_timeout_ms(1_000), 1_000);
        assert_eq!(clamp_timeout_ms(120_000), 120_000);
        assert_eq!(clamp_timeout_ms(900_000), 900_000);
        assert_eq!(clamp_timeout_ms(u64::MAX), TIMEOUT_MAX_MS);
    }

    // U9 argv-injection guard, enforced at run time too: interpreters are a
    // closed set of bare binary names.
    #[test]
    fn resolve_interpreter_rejects_free_form_strings() {
        for ok in ["bash", "sh", "node", "python3"] {
            assert_eq!(resolve_interpreter(ok), Ok(ok));
        }
        for bad in ["sh -c evil", "/usr/bin/perl", "", "Bash"] {
            let err = resolve_interpreter(bad).expect_err(bad);
            assert!(err.contains("unknown interpreter"), "{err}");
        }
    }

    // R14/U9 argv-injection guard: an option-like script path (from a tampered
    // store) is rejected before it reaches Command::arg, where bash/sh/python3
    // would treat it as a flag (`-cPAYLOAD` → arbitrary exec).
    #[test]
    fn checked_script_path_rejects_option_like_paths() {
        for bad in ["-cPAYLOAD", "--eval=x", "-"] {
            assert!(checked_script_path(bad).is_err(), "{bad:?} must be rejected");
        }
        for ok in [
            "/home/u/.local/share/fly/automation-scripts/abc123/script",
            "script",
            "./script",
        ] {
            assert!(checked_script_path(ok).is_ok(), "{ok:?} must be allowed");
        }
    }

    // R13: the capture buffer keeps the TAIL on overflow, whatever the chunk
    // sizes.
    #[test]
    fn tail_buf_keeps_the_tail_on_overflow_r13() {
        let mut t = TailBuf::new(8);
        t.push(b"0123456789"); // single over-cap chunk
        assert_eq!(t.buf, b"23456789");
        let mut t = TailBuf::new(8);
        t.push(b"01234");
        t.push(b"56789"); // accumulated overflow
        assert_eq!(t.into_string(), "23456789");
    }

    // ================= the runner (real `sh` children) =================

    type ClosedRun = (String, String, RunOutcome);

    struct Harness {
        runner: Arc<ScriptRunner>,
        closed: Arc<Mutex<Vec<ClosedRun>>>,
        alerts: Arc<Mutex<Vec<AlertEvent>>>,
        dir: tempfile::TempDir,
    }

    fn harness() -> Harness {
        let closed: Arc<Mutex<Vec<ClosedRun>>> = Arc::new(Mutex::new(Vec::new()));
        let c = Arc::clone(&closed);
        let runner = Arc::new(ScriptRunner::new(Arc::new(
            move |aid: &str, rid: &str, outcome: RunOutcome| {
                c.lock().unwrap().push((aid.into(), rid.into(), outcome));
            },
        )));
        let alerts: Arc<Mutex<Vec<AlertEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let a = Arc::clone(&alerts);
        runner.set_alert_sink(Arc::new(move |ev: AlertEvent| {
            a.lock().unwrap().push(ev);
        }));
        Harness {
            runner,
            closed,
            alerts,
            dir: tempfile::tempdir().unwrap(),
        }
    }

    impl Harness {
        /// Write `body` as a script file and return a script-mode automation
        /// running it via `sh` in the tempdir.
        fn automation(&self, id: &str, body: &str, timeout_ms: u64) -> Automation {
            let path = self.dir.path().join(format!("{id}.sh"));
            std::fs::write(&path, body).unwrap();
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
                after: None,
                cwd: self.dir.path().to_string_lossy().into_owned(),
                mode: Mode::Script {
                    script_file: path.to_string_lossy().into_owned(),
                    interpreter: "sh".into(),
                    timeout_ms,
                },
                origin: Origin {
                    pane_id: 7,
                    workspace_id: "ws-1".into(),
                    label: "cli".into(),
                },
                created_at: 0,
                updated_at: 0,
                next_run_at: None,
                runs: Vec::new(),
            }
        }

        /// Wait until `n` closures arrived (bounded), then return them.
        fn wait_closed(&self, n: usize, timeout: Duration) -> Vec<ClosedRun> {
            let start = Instant::now();
            loop {
                {
                    let closed = self.closed.lock().unwrap();
                    if closed.len() >= n {
                        return closed.clone();
                    }
                }
                assert!(
                    start.elapsed() < timeout,
                    "timed out waiting for {n} closure(s); got {:?}",
                    self.closed.lock().unwrap()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        fn one_closed(&self) -> ClosedRun {
            self.wait_closed(1, Duration::from_secs(10)).remove(0)
        }
    }

    // AE1: a silent tick — exit 0, no output — closes succeeded with null
    // output and no alert.
    #[test]
    fn exit_zero_empty_stdout_closes_succeeded_with_null_output_ae1() {
        let h = harness();
        let a = h.automation("silent", "exit 0\n", 5_000);
        h.runner.dispatch_script(&a, "r1").unwrap();

        let (aid, rid, outcome) = h.one_closed();
        assert_eq!((aid.as_str(), rid.as_str()), ("silent", "r1"));
        assert_eq!(outcome, RunOutcome::Succeeded { output: None });
        assert!(h.alerts.lock().unwrap().is_empty(), "no alert for silence");
        assert_eq!(h.runner.inflight_count(), 0, "registry entry removed on reap");
    }

    // R15: whitespace-only stdout is silent too (AE1 outcome).
    #[test]
    fn whitespace_only_stdout_is_silent_r15() {
        let h = harness();
        let a = h.automation("blank", "printf '  \\n\\n'\n", 5_000);
        h.runner.dispatch_script(&a, "r1").unwrap();

        let (_, _, outcome) = h.one_closed();
        assert_eq!(outcome, RunOutcome::Succeeded { output: None });
        assert!(h.alerts.lock().unwrap().is_empty());
    }

    // AE2: diagnostics + a final {"wakeAgent": false} line — silent success,
    // no alert, but the capture is stored on the row.
    #[test]
    fn trailing_wake_agent_false_is_silent_despite_earlier_output_ae2() {
        let h = harness();
        let a = h.automation(
            "gated",
            "echo diagnostics\necho '{\"wakeAgent\": false}'\n",
            5_000,
        );
        h.runner.dispatch_script(&a, "r1").unwrap();

        let (_, _, outcome) = h.one_closed();
        match outcome {
            RunOutcome::Succeeded { output: Some(out) } => {
                assert!(out.starts_with("diagnostics"), "{out}");
            }
            other => panic!("expected succeeded with capture, got {other:?}"),
        }
        assert!(h.alerts.lock().unwrap().is_empty(), "sentinel suppressed the alert");
    }

    // R15/AE3: a mid-output sentinel does not suppress — the run closes
    // succeeded AND the alert sink fires with the first stdout line.
    #[test]
    fn sentinel_mid_output_alerts_with_first_line_ae3() {
        let h = harness();
        let a = h.automation(
            "alerting",
            "echo 'Disk at 93%'\necho '{\"wakeAgent\": false}'\necho tail\n",
            5_000,
        );
        h.runner.dispatch_script(&a, "r1").unwrap();

        let (_, _, outcome) = h.one_closed();
        assert!(
            matches!(outcome, RunOutcome::Succeeded { output: Some(_) }),
            "alerts close SUCCEEDED with output captured"
        );
        let alerts = h.alerts.lock().unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].first_line, "Disk at 93%");
        assert_eq!(alerts[0].automation_id, "alerting");
        assert_eq!(alerts[0].automation_name, "watch alerting");
        assert_eq!(alerts[0].run_id, "r1");
        assert!(alerts[0].capture.contains("tail"));
    }

    // R15: the sentinel is read from the 64 KiB CAPTURE tail (before the
    // 8 KiB storage cap) — output overflowing the capture cap with a
    // trailing sentinel is still silent.
    #[test]
    fn capture_overflowing_64kib_with_trailing_sentinel_is_still_silent_r15() {
        let h = harness();
        let a = h.automation(
            "chatty",
            "head -c 100000 /dev/zero | tr '\\0' x\necho\necho '{\"wakeAgent\": false}'\n",
            10_000,
        );
        h.runner.dispatch_script(&a, "r1").unwrap();

        let (_, _, outcome) = h.one_closed();
        match outcome {
            RunOutcome::Succeeded { output: Some(out) } => {
                // The row stores the 8 KiB STORAGE tail of the capture; the
                // sentinel line survives at its end.
                assert!(out.trim_end().ends_with("{\"wakeAgent\": false}"), "…{:?}", &out[out.len().saturating_sub(60)..]);
            }
            other => panic!("expected succeeded with capture, got {other:?}"),
        }
        assert!(
            h.alerts.lock().unwrap().is_empty(),
            "sentinel read from the capture tail suppressed the alert"
        );
    }

    // R15: non-zero exit closes failed with the code recorded and output
    // captured; no alert.
    #[test]
    fn exit_3_closes_failed_with_code_and_output_r15() {
        let h = harness();
        let a = h.automation("broken", "echo broken >&1\nexit 3\n", 5_000);
        h.runner.dispatch_script(&a, "r1").unwrap();

        let (_, _, outcome) = h.one_closed();
        assert_eq!(
            outcome,
            RunOutcome::Failed {
                error: "exit status 3".into(),
                exit_code: Some(3),
                output: Some("broken\n".into())
            }
        );
        assert!(h.alerts.lock().unwrap().is_empty(), "failures never alert");
    }

    // R15: stderr-only output on exit 0 is a silent success with the stderr
    // captured onto the row.
    #[test]
    fn stderr_only_exit_zero_is_silent_with_stderr_captured_r15() {
        let h = harness();
        let a = h.automation("warner", "echo 'warn: slow' >&2\n", 5_000);
        h.runner.dispatch_script(&a, "r1").unwrap();

        let (_, _, outcome) = h.one_closed();
        assert_eq!(
            outcome,
            RunOutcome::Succeeded {
                output: Some("[stderr]\nwarn: slow\n".into())
            }
        );
        assert!(h.alerts.lock().unwrap().is_empty(), "stderr never wakes");
    }

    // R13/KTD-I: on deadline the WHOLE group dies — SIGHUP within the grace
    // (sleep exits on SIGHUP), and no grandchild survives. The script prints
    // its own pid (== pgid, process_group(0)) so the test can poll the group.
    #[test]
    fn timeout_kills_whole_group_within_grace_no_grandchildren_survive_ktd_i() {
        let h = harness();
        // `sleep 100 &` is the grandchild-shaped leak: signaling only the
        // direct sh would orphan it for ~100s.
        let a = h.automation("stuck", "echo $$\nsleep 100 &\nsleep 100\n", 1_000);
        let start = Instant::now();
        h.runner.dispatch_script(&a, "r1").unwrap();

        let (_, _, outcome) = h.one_closed();
        assert!(
            start.elapsed() < Duration::from_secs(6),
            "closure must arrive within deadline + grace, took {:?}",
            start.elapsed()
        );
        let (error, output) = match outcome {
            RunOutcome::Failed {
                error,
                exit_code: None,
                output,
            } => (error, output),
            other => panic!("expected timeout failure, got {other:?}"),
        };
        assert!(error.starts_with("timed out after 1000ms"), "{error}");

        // No surviving group members: poll kill(-pgid, 0) until ESRCH.
        let pgid: i32 = output
            .expect("the echoed pid was captured")
            .lines()
            .next()
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let gone_by = Instant::now() + Duration::from_secs(5);
        while group_alive(pgid) {
            assert!(
                Instant::now() < gone_by,
                "process group {pgid} still alive — grandchild leaked"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(h.runner.inflight_count(), 0);
    }

    // R14: env_clear + allowlist — FLY_PANE_TOKEN / FLY_SOCKET_PATH are never
    // present even when the app env carries them (refactor guard, the
    // notify/command.rs precedent), while FLY_AUTOMATION_ID / _RUN_ID are set.
    #[test]
    fn env_is_allowlisted_tokens_absent_automation_ids_present_r14() {
        std::env::set_var("FLY_PANE_TOKEN", "super-secret");
        std::env::set_var("FLY_SOCKET_PATH", "/run/fly/hook.sock");
        let h = harness();
        let a = h.automation(
            "envprobe",
            "printf 'T=%s S=%s I=%s R=%s' \
             \"${FLY_PANE_TOKEN:-none}\" \"${FLY_SOCKET_PATH:-none}\" \
             \"$FLY_AUTOMATION_ID\" \"$FLY_AUTOMATION_RUN_ID\"\n",
            5_000,
        );
        h.runner.dispatch_script(&a, "run-42").unwrap();
        let (_, _, outcome) = h.one_closed();
        std::env::remove_var("FLY_PANE_TOKEN");
        std::env::remove_var("FLY_SOCKET_PATH");

        match outcome {
            RunOutcome::Succeeded { output: Some(out) } => {
                assert_eq!(out, "T=none S=none I=envprobe R=run-42");
            }
            other => panic!("expected succeeded with capture, got {other:?}"),
        }
    }

    // R13: a nonexistent cwd is a spawn error — the DISPATCH returns Err
    // with a clear message (U4 then rolls back + closes failed), and nothing
    // registers in flight.
    #[test]
    fn nonexistent_cwd_fails_dispatch_with_clear_error_r13() {
        let h = harness();
        let mut a = h.automation("homeless", "exit 0\n", 5_000);
        a.cwd = "/nonexistent/fly-u5-test".into();

        let err = h.runner.dispatch_script(&a, "r1").expect_err("must fail");
        assert!(err.contains("could not start script"), "{err}");
        assert!(err.contains("/nonexistent/fly-u5-test"), "{err}");
        assert_eq!(h.runner.inflight_count(), 0, "no stranded registry entry");
        assert!(h.closed.lock().unwrap().is_empty(), "closure is U4's job");
    }

    // An unknown stored interpreter fails the dispatch the same way (closed
    // set, argv-injection guard).
    #[test]
    fn unknown_interpreter_fails_dispatch_r13() {
        let h = harness();
        let mut a = h.automation("weird", "exit 0\n", 5_000);
        a.mode = match a.mode {
            Mode::Script {
                script_file,
                timeout_ms,
                ..
            } => Mode::Script {
                script_file,
                interpreter: "perl -e".into(),
                timeout_ms,
            },
            m => m,
        };
        let err = h.runner.dispatch_script(&a, "r1").expect_err("must fail");
        assert!(err.contains("unknown interpreter"), "{err}");
    }

    // KTD-D: with 4 runs in flight the capacity probe reads false and a 5th
    // dispatch is refused; slots free as runs reap.
    #[test]
    fn capacity_probe_false_at_four_in_flight_ktd_d() {
        let h = harness();
        assert!(h.runner.has_capacity());
        for i in 0..MAX_INFLIGHT_SCRIPTS {
            let a = h.automation(&format!("busy{i}"), "sleep 0.4\n", 5_000);
            h.runner.dispatch_script(&a, &format!("r{i}")).unwrap();
        }
        assert_eq!(h.runner.inflight_count(), MAX_INFLIGHT_SCRIPTS);
        assert!(!h.runner.has_capacity(), "probe false at the cap");

        let a = h.automation("fifth", "exit 0\n", 5_000);
        let err = h.runner.dispatch_script(&a, "r-over").expect_err("at cap");
        assert!(err.contains("capacity"), "{err}");

        h.wait_closed(MAX_INFLIGHT_SCRIPTS, Duration::from_secs(10));
        assert_eq!(h.runner.inflight_count(), 0, "all slots released");
        assert!(h.runner.has_capacity());
    }

    // R23/R5: the killer seam (delete/shutdown) escalates SIGHUP → grace →
    // SIGKILL on the group; the reaper observes the death, closes the row,
    // and clears the registry. kill_all_inflight covers the same path.
    #[test]
    fn kill_run_terminates_the_group_and_the_reaper_closes_r23() {
        let h = harness();
        let a = h.automation("doomed", "sleep 100\n", 900_000);
        h.runner.dispatch_script(&a, "r1").unwrap();
        // Give the child a beat to be running, then kill via the seam.
        std::thread::sleep(Duration::from_millis(50));
        let start = Instant::now();
        h.runner.kill_all_inflight(); // exercises kill_run for every entry

        let (_, _, outcome) = h.one_closed();
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "seam kill must be fast, took {:?}",
            start.elapsed()
        );
        match outcome {
            // sh dies to SIGHUP → 128 + 1.
            RunOutcome::Failed {
                exit_code: Some(129),
                ..
            } => {}
            other => panic!("expected signal-death failure, got {other:?}"),
        }
        assert_eq!(h.runner.inflight_count(), 0);
        h.runner.kill_run("r1"); // idempotent: unknown/reaped run is a no-op
    }
}
