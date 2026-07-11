//! Headless monitor checks — the stream core and the runner (U1 + U3 of
//! `docs/plans/2026-07-11-003-feat-headless-monitor-checks-plan.md`; all
//! R-IDs below cite that plan).
//!
//! A monitor check runs as a backend-owned `claude -p --output-format
//! stream-json --verbose` child. This module has two halves:
//!
//! - the **process-free core** (U1): the tolerant NDJSON event view
//!   ([`parse_line`] → [`StreamEvent`], R11) and the pure infra-vs-readable
//!   outcome classification ([`StreamFold`] → [`CheckOutcome`], R10). No
//!   process or IO type appears in it — exit/timeout/kill facts arrive as
//!   plain values ([`ExitFacts`]) — so the whole contract tests without a
//!   child process (the [`super::model`] purity rule);
//! - the **process-owning runner** (U3): [`HeadlessRunner`] — spawn (R1
//!   process shape, R13 env hygiene), pipe reading, the monotonic deadline
//!   (R4), and the SIGTERM-first kill-and-sweep discipline (R5) — which
//!   drives the core and hands the classified [`CheckOutcome`] to the
//!   injected [`CheckCloser`] (U4 wires the manager's close-by-run-id
//!   entry, the `ScriptRunner` wiring pattern).
//!
//! **The stream contract is empirical, not documented** (upstream issue
//! #24612), pinned live against claude 2.1.207 in the plan's Empirical
//! Contract. Only `type:"system"`/`subtype:"init"` (which carries the
//! check's `session_id`, R12) and `type:"result"` are depended on; every
//! other shape — including event types and subtypes unheard of today, and
//! events arriving *after* the `result` (a `task_notification` was observed
//! there) — is [`StreamEvent::Other`] and ignored. Classification follows
//! the repo's abstain-on-surprise convention, degraded to infra (the
//! "Tolerant NDJSON parsing, abstain-to-infra" KTD): anything surprising —
//! an oversized or unparsable line that swallowed the `result`, a second
//! `result` event ([`super::verdict`]'s two-blocks-abstain rule applied to
//! the stream), a non-success result, EOF without a result — classifies
//! [`CheckOutcome::Infra`], never a fabricated verdict. A future claude that
//! breaks the contract therefore reads as "monitor broken" within three
//! checks (the existing escalation), never as a silent or wrong verdict.
//!
//! Lines are handled as **bytes** end to end: the runner splits at line
//! boundaries and each complete line's bytes are parsed as JSON directly
//! (`serde_json::from_slice`), so a multibyte UTF-8 character split across
//! read chunks *inside* a line is never lossy-converted — invalid UTF-8 is
//! simply an unparsable line, skipped like any other.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{ResolvedLaunch, RUN_DEADLINE_MS};

/// R11: the per-line cap. A line longer than this is skipped **without
/// parsing** ([`parse_line`] returns `None`); if the skipped line was the
/// `result`, the stream ends result-less and classifies
/// [`CheckOutcome::Infra`]. The runner (U3) also enforces this cap at read
/// time — a bounded reader may hand the fold a truncated over-cap line,
/// which then fails JSON parsing and is skipped all the same.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

/// The minimal event view over one NDJSON stream line (R11): only the two
/// shapes the check depends on are distinguished; everything else is
/// [`StreamEvent::Other`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    /// `type:"system", subtype:"init"` — the stream's opening event. Its
    /// `session_id` (R12) is stamped on the run row and rides the FAIL
    /// bundle alongside the derived transcript path; `None` when the field
    /// is missing or not a string (tolerated, not a surprise).
    Init { session_id: Option<String> },
    /// `type:"result"` — the check's terminal event. `success` is
    /// `subtype == "success"` AND `is_error` is not `true`; any other
    /// combination (unknown subtype, missing subtype, `is_error: true`)
    /// is a non-success result, which classification treats as infra
    /// (abstain-on-surprise — only `"success"` was observed live, and
    /// non-success subtypes are refused by convention, not observation).
    /// `text` is the `result` field's string, or empty when missing.
    Result { success: bool, text: String },
    /// Anything else — `assistant`, `rate_limit_event`, the observed system
    /// subtypes (`hook_started`, `thinking_tokens`, `task_notification`, …),
    /// and every shape not yet invented. Ignored by the fold.
    Other,
}

/// R11: parse one complete stream line (bytes, without or with its trailing
/// newline) into the minimal event view. Returns `None` for a **skipped**
/// line — over the [`MAX_LINE_BYTES`] cap (never parsed at all) or not valid
/// JSON — and `Some(StreamEvent::Other)` for valid JSON of any unknown
/// shape. Bytes go to `serde_json` directly, never through a lossy string
/// conversion (see the module doc on UTF-8).
pub fn parse_line(line: &[u8]) -> Option<StreamEvent> {
    if line.len() > MAX_LINE_BYTES {
        return None;
    }
    let v: Value = serde_json::from_slice(line).ok()?;
    Some(match v.get("type").and_then(Value::as_str) {
        Some("system") if v.get("subtype").and_then(Value::as_str) == Some("init") => {
            StreamEvent::Init {
                session_id: v.get("session_id").and_then(Value::as_str).map(str::to_owned),
            }
        }
        Some("result") => StreamEvent::Result {
            success: v.get("subtype").and_then(Value::as_str) == Some("success")
                && v.get("is_error").and_then(Value::as_bool) != Some(true),
            text: v
                .get("result")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        },
        _ => StreamEvent::Other,
    })
}

/// How the child's process side ended, as observed by the runner (U3) and
/// handed to [`StreamFold::finish`] as plain values — the exit status /
/// timeout / runner-initiated-kill flags of R10. Spawn failure is not a
/// variant: a child that never spawned has no stream to fold, so the runner
/// constructs that infra row directly via [`CheckOutcome::spawn_failed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitFacts {
    /// The child exited on its own — **spontaneously**, not by any runner
    /// kill. `code` follows `std::process::ExitStatus::code()`: `Some` on a
    /// normal exit, `None` when a signal (that the runner did not send)
    /// killed it.
    Exited { code: Option<i32> },
    /// The runner's deadline (R4, `RUN_DEADLINE_MS`) lapsed and the runner
    /// killed the child. Always infra (R10's timeout row) — even after a
    /// success result, because the runner's stream-end policy kills a
    /// lingering child long before the deadline, so reaching it with a
    /// result in hand means something surprising happened.
    TimedOut,
    /// The runner's own lingering-exit kill (R10): a success `result` was
    /// already streamed, the child lingered (the observed backgrounding
    /// quirk — result early, claude stays on a task), and the runner killed
    /// it under its stream-end policy. This flag is what keeps such a check
    /// Clean; without it every healthy backgrounding check would falsely
    /// infra-fail and ring "monitor broken" in three.
    LingerKilled,
}

/// R10: the classified outcome of one headless check.
///
/// **Sanitize/scrub contract (R8) — read before storing either field.**
/// Nothing in this module is cleaned: [`CheckOutcome::Clean`]'s `text` is
/// the result event's raw text, and [`CheckOutcome::Infra`]'s `reason` may
/// embed the caller-supplied stderr tail **verbatim** — raw child output,
/// control characters and secrets included. The caller MUST route both
/// through the shared sanitize → scrub helper (U5) before they land on a
/// run row, a bundle, the dashboard, or an alert line; the stderr tail
/// itself is passed *into* [`StreamFold::finish`] already bounded by the
/// runner (a small tail, never the full pipe), but bounded is not clean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// A readable check: exactly one success `result` was streamed and the
    /// process end corroborates it (exit 0 or the runner's own
    /// [`ExitFacts::LingerKilled`]). `text` is the result's exact text —
    /// possibly empty: the empty→`None` mapping happens downstream in the
    /// shared cleaning helper (U4 asserts the end-to-end parity with an
    /// empty pane capture), never here. Whether `text` carries a verdict
    /// block is downstream's concern too ([`super::verdict`]); no-verdict
    /// Clean is a healthy not-done check. `session_id` comes from the
    /// stream's `init` event (R12), `None` if none was seen.
    Clean {
        text: String,
        session_id: Option<String>,
    },
    /// An infra-unreadable check (R10): the run closes Failed and feeds the
    /// existing broken-monitor escalation. `reason` is descriptive and may
    /// embed raw stderr — see the sanitize/scrub contract above.
    Infra { reason: String },
}

impl CheckOutcome {
    /// R10's spawn-failure row: the child never spawned, so there is no
    /// stream to fold — the runner (U3) builds this infra outcome directly
    /// from the spawn error's text.
    pub fn spawn_failed(error: &str) -> CheckOutcome {
        CheckOutcome::Infra {
            reason: format!("spawn failed: {error}"),
        }
    }
}

/// The first `result` event's payload, retained by the fold until
/// [`StreamFold::finish`] classifies it.
#[derive(Debug, Clone)]
struct FirstResult {
    success: bool,
    text: String,
}

/// R10: the pure fold over the parsed event sequence. Feed each event (or
/// raw line) as it arrives, then settle with the end-of-stream facts:
///
/// ```text
/// let mut fold = StreamFold::new();
/// for line in lines { fold.feed_line(line); }
/// let outcome = fold.finish(exit_facts, stderr_tail);
/// ```
///
/// Memory is O(1) in the stream length: only the first `init`'s session id
/// and the first `result`'s text are retained — a second `result` merely
/// sets a surprise flag (its text is dropped), and [`StreamEvent::Other`]
/// carries nothing — so an unbounded or misbehaving stream cannot balloon
/// the fold (R11's tolerance without the memory bill).
#[derive(Debug, Default, Clone)]
pub struct StreamFold {
    /// From the first `init` seen; later `init`s (never observed live) are
    /// ignored — first-wins keeps the id stable, and R12 needs one id.
    session_id: Option<String>,
    result: Option<FirstResult>,
    /// A second `result` event was seen — a surprise ([`super::verdict`]'s
    /// two-blocks-abstain convention applied to the stream) → Infra.
    extra_result: bool,
}

impl StreamFold {
    pub fn new() -> StreamFold {
        StreamFold::default()
    }

    /// Parse one raw line ([`parse_line`]) and feed it; a skipped line
    /// (over-cap or unparsable, R11) changes nothing.
    pub fn feed_line(&mut self, line: &[u8]) {
        if let Some(event) = parse_line(line) {
            self.feed(event);
        }
    }

    /// Whether a `result` event has been folded yet. The runner (U3) polls
    /// this to start its post-result linger clock (the stream-end policy) —
    /// classification itself still goes through [`StreamFold::finish`].
    pub fn has_result(&self) -> bool {
        self.result.is_some()
    }

    /// Fold one parsed event.
    pub fn feed(&mut self, event: StreamEvent) {
        match event {
            StreamEvent::Init { session_id } => {
                if self.session_id.is_none() {
                    self.session_id = session_id;
                }
            }
            StreamEvent::Result { success, text } => {
                if self.result.is_some() {
                    self.extra_result = true;
                } else {
                    self.result = Some(FirstResult { success, text });
                }
            }
            StreamEvent::Other => {}
        }
    }

    /// R10's classification table, one row per arm. `stderr_tail` is the
    /// runner-bounded raw stderr tail, embedded verbatim in the
    /// nonzero-exit/signal reasons (empty tail ⇒ omitted) — see
    /// [`CheckOutcome`]'s sanitize/scrub contract for why raw is safe *here*
    /// and nowhere downstream.
    pub fn finish(self, end: ExitFacts, stderr_tail: &str) -> CheckOutcome {
        let infra = |reason: String| CheckOutcome::Infra { reason };
        let with_tail = |msg: String| {
            if stderr_tail.is_empty() {
                msg
            } else {
                format!("{msg}; stderr: {stderr_tail}")
            }
        };
        // Surprise first: two results could disagree — refuse to pick,
        // whatever the exit looked like.
        if self.extra_result {
            return infra("malformed stream: more than one result event".to_owned());
        }
        match (self.result, end) {
            // Malformed stream (R10/R11): EOF with no result — including the
            // over-cap/unparsable-line case that swallowed it.
            (None, ExitFacts::Exited { code: Some(0) }) => {
                infra("malformed stream: no result event before exit 0".to_owned())
            }
            (None, ExitFacts::Exited { code: Some(code) }) => {
                infra(with_tail(format!("exited {code} with no result event")))
            }
            (None, ExitFacts::Exited { code: None }) => {
                infra(with_tail("killed by a signal with no result event".to_owned()))
            }
            (None, ExitFacts::TimedOut) => {
                infra("timed out: killed at the run deadline with no result event".to_owned())
            }
            // Defensive: the runner only linger-kills after a result, so this
            // combination is itself a surprise → infra.
            (None, ExitFacts::LingerKilled) => {
                infra("malformed stream: runner kill with no result event".to_owned())
            }
            // A non-success result is infra regardless of how the process
            // ended (abstain-on-surprise) — never Clean, even on exit 0.
            (Some(r), _) if !r.success => {
                infra("stream reported a non-success result event".to_owned())
            }
            // The Clean rows (R10): a sole success result corroborated by a
            // clean exit or by the runner's own lingering-exit kill.
            (Some(r), ExitFacts::Exited { code: Some(0) } | ExitFacts::LingerKilled) => {
                CheckOutcome::Clean {
                    text: r.text,
                    session_id: self.session_id,
                }
            }
            // R10's timeout row is flat: a deadline kill is not the linger
            // kill (see [`ExitFacts::TimedOut`]).
            (Some(_), ExitFacts::TimedOut) => {
                infra("timed out: killed at the run deadline after its result event".to_owned())
            }
            // R10, verbatim: "a *spontaneous* nonzero exit remains infra" —
            // only the runner's own kill exempts a post-result death.
            (Some(_), ExitFacts::Exited { code: Some(code) }) => {
                infra(with_tail(format!("spontaneous exit {code} after the result event")))
            }
            (Some(_), ExitFacts::Exited { code: None }) => {
                infra(with_tail("spontaneous signal death after the result event".to_owned()))
            }
        }
    }
}

// ===========================================================================
// U3 — the process-owning half: spawn, read, deadline, kill discipline
// ===========================================================================

/// Bounded raw stderr tail handed to [`StreamFold::finish`] (R8's input): a
/// chatty child must never block on a full stderr pipe and misread as a
/// timeout, so stderr is drained concurrently on its own thread into this
/// small tail. Raw here BY CONTRACT — the caller of the closer (U4/U5)
/// cleans it before it lands anywhere (see [`CheckOutcome`]'s doc).
pub const STDERR_TAIL_BYTES: usize = 4 * 1024;

/// stdout read-chunk size. Line assembly is byte-exact across chunk
/// boundaries (see [`read_stdout`]), so this is throughput tuning only — a
/// multibyte UTF-8 character split across two reads inside a line survives.
const READ_BUF: usize = 8 * 1024;

/// Runner-leg SIGTERM→SIGKILL grace (R5): empirically (2.1.207) SIGTERM
/// makes claude reap its own child tree and exit within ~6 s even mid-tool,
/// so the runner's own kill legs (deadline, stream-end) afford 10 s before
/// escalating. The seam legs use [`SEAM_KILL_GRACE`] instead.
pub const TERM_KILL_GRACE: Duration = Duration::from_secs(10);

/// Seam-leg grace (delete / shutdown / backstop, R5): deliberately short —
/// these run on paths that must stay fast (`script.rs::SEAM_KILL_GRACE`
/// precedent). The descendant-snapshot sweep, not the grace, is the
/// no-orphan guarantee here.
pub const SEAM_KILL_GRACE: Duration = Duration::from_millis(200);

/// Post-result linger (the stream-end policy): once a success `result` has
/// been streamed the child is expected to wrap up and exit on its own; a
/// child still alive this long after the event is the observed backgrounding
/// quirk (result early, claude stays on a task) and is killed →
/// [`ExitFacts::LingerKilled`] → still Clean (R10). The clock runs from the
/// **result event**, not stdout EOF — a lingering claude holds stdout open,
/// so an EOF-anchored clock would never fire and the check would wrongly
/// ride to the deadline (which classifies Infra).
pub const LINGER_EXIT_GRACE: Duration = Duration::from_secs(5);

/// `try_wait` / liveness poll interval (`script.rs::REAP_POLL` precedent).
const CHECK_POLL: Duration = Duration::from_millis(25);

/// How long the runner waits for a reader thread after the child is gone.
/// Readers EOF when every pipe-holder exits; a leaked descendant holding the
/// pipe is killed by the snapshot sweep on the kill legs, and on a natural
/// exit this bound keeps the close from wedging on a leaked FD — the fold is
/// read under its lock, so a still-blocked reader degrades, never corrupts.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// How the runner closes a run row: `(automation_id, run_id, outcome)`.
/// U4 wires the manager's run-id-keyed close-with-text entry (the
/// [`super::script::RunCloser`] wiring pattern: the closer takes the store
/// lock itself, never called under it); tests wire a collector. The check's
/// `session_id` rides inside [`CheckOutcome::Clean`] (R12).
pub type CheckCloser = Arc<dyn Fn(&str, &str, CheckOutcome) + Send + Sync>;

/// Every timing knob the runner uses, injectable so the integration tests
/// run in milliseconds. [`Default`] is the real production set.
#[derive(Debug, Clone)]
pub struct HeadlessTiming {
    /// R4: the run deadline, enforced by the runner thread on a MONOTONIC
    /// clock (the sweep's epoch-time backstop fires only at deadline +
    /// slack — see [`super::HEADLESS_DEADLINE_SLACK_MS`]).
    pub deadline: Duration,
    /// See [`LINGER_EXIT_GRACE`].
    pub linger_exit_grace: Duration,
    /// See [`TERM_KILL_GRACE`] — the runner's own kill legs.
    pub term_grace: Duration,
    /// See [`SEAM_KILL_GRACE`] — [`HeadlessRunner::kill_run`]'s legs.
    pub seam_grace: Duration,
    /// See [`CHECK_POLL`].
    pub poll: Duration,
}

impl Default for HeadlessTiming {
    fn default() -> HeadlessTiming {
        HeadlessTiming {
            deadline: Duration::from_millis(RUN_DEADLINE_MS),
            linger_exit_grace: LINGER_EXIT_GRACE,
            term_grace: TERM_KILL_GRACE,
            seam_grace: SEAM_KILL_GRACE,
            poll: CHECK_POLL,
        }
    }
}

/// One in-flight check in the registry: everything the seam legs and U4's
/// probes need without a `Child` handle. **Liveness is never mere entry
/// presence** — the probe is pid + start-time ([`child_alive`]), so a
/// terminal-but-alive child stays visible to the overlap check and a reused
/// pid never reads as ours.
struct InflightCheck {
    automation_id: String,
    pid: u32,
    /// `/proc/<pid>/stat` field 22 (`starttime`, clock ticks since boot) at
    /// spawn — the pid-reuse pin. `None` when the read failed (child died
    /// within microseconds); liveness then degrades to a bare existence
    /// check.
    start_time: Option<u64>,
    /// Monotonic spawn instant — the backstop's suspend-proof gate (U2 KTD:
    /// epoch age alone can lapse across a laptop suspend while the check is
    /// healthy).
    spawned: Instant,
    /// Set (via [`DoneGuard`]) when the owning runner thread has finished —
    /// including by panic. While false, reaping belongs to that thread;
    /// once true, [`HeadlessRunner::kill_run`] also reaps and evicts.
    runner_done: Arc<AtomicBool>,
}

/// Sets the `runner_done` flag on drop — including a panicking unwind, so a
/// dead runner thread's entry stays in the registry (pid still killable by
/// the backstop) but is marked orphaned for the seam's reap-and-evict.
struct DoneGuard(Arc<AtomicBool>);

impl Drop for DoneGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Stream state shared between the stdout reader thread and the runner
/// thread. `eof` is stored with Release AFTER the final feed, and the runner
/// loads it with Acquire BEFORE reading `result_at` — so "EOF observed, no
/// result" can never be a torn read of a stream whose result was actually
/// fed (the EOF-no-result kill leg depends on this order).
#[derive(Default)]
struct StreamShared {
    fold: Mutex<StreamFold>,
    /// When the first `result` event was folded — starts the linger clock.
    result_at: Mutex<Option<Instant>>,
    eof: AtomicBool,
}

/// The headless monitor-check runner (U3): owns the `claude -p` child for
/// the check's whole lifetime — spawn, stream, deadline, kill — and closes
/// through the injected [`CheckCloser`]. Constructed once at startup
/// (`lib.rs`, U5) with the manager closer, exactly like
/// [`super::script::ScriptRunner`]; the binary path and timings are
/// injectable so integration tests drive fixture scripts in milliseconds.
///
/// **Registry lock discipline:** the `inflight` mutex is held only for map
/// access — lookup/insert/remove — never across a kill grace or a `/proc`
/// walk (lookup, release, then signal). It never nests with the store lock
/// (the closer takes that itself, after this runner is done).
pub struct HeadlessRunner {
    closer: CheckCloser,
    /// In-flight registry, keyed by run id. Inserted before [`Self::run`]
    /// returns; removed on every exit path of the runner thread, by the
    /// thread-spawn-failure arm, and by [`Self::kill_run`] once the owning
    /// thread is gone.
    inflight: Arc<Mutex<HashMap<String, InflightCheck>>>,
    /// The claude binary (default `"claude"`, resolved via PATH); tests
    /// point it at fixture scripts.
    claude_bin: String,
    timing: HeadlessTiming,
}

impl HeadlessRunner {
    /// Production construction: `claude` on PATH, real timings.
    pub fn new(closer: CheckCloser) -> HeadlessRunner {
        HeadlessRunner::with_config(closer, "claude", HeadlessTiming::default())
    }

    /// Test/injection construction: explicit binary and timings.
    pub fn with_config(
        closer: CheckCloser,
        claude_bin: impl Into<String>,
        timing: HeadlessTiming,
    ) -> HeadlessRunner {
        HeadlessRunner {
            closer,
            inflight: Arc::new(Mutex::new(HashMap::new())),
            claude_bin: claude_bin.into(),
            timing,
        }
    }

    /// Number of registry entries (tests; not a liveness statement).
    pub fn inflight_count(&self) -> usize {
        self.inflight.lock().unwrap().len()
    }

    /// U4's overlap probe (R7): whether this automation has a check whose
    /// **child is still alive** — pid + start-time pinned, never mere entry
    /// presence — so a terminal-but-alive child blocks the next claim and
    /// manual runs, and a dead child's stale entry never does. Cheap (`/proc`
    /// stat reads off the registry lock); safe inside the sweep's mutate
    /// closure per the manager's probe contract.
    pub fn automation_check_alive(&self, automation_id: &str) -> bool {
        let pins: Vec<(u32, Option<u64>)> = self
            .inflight
            .lock()
            .unwrap()
            .values()
            .filter(|e| e.automation_id == automation_id)
            .map(|e| (e.pid, e.start_time))
            .collect();
        pins.into_iter().any(|(pid, st)| child_alive(pid, st))
    }

    /// U4's backstop monotonic gate (U2 KTD): true when the run's registry
    /// entry has lapsed its monotonic deadline — or the entry is gone
    /// entirely (runner finished, or died and was evicted). The sweep's
    /// epoch-age leg must ALSO hold before the backstop kills, so a laptop
    /// suspend longer than the slack never kills a healthy check.
    pub fn monotonic_deadline_lapsed(&self, run_id: &str) -> bool {
        match self.inflight.lock().unwrap().get(run_id) {
            None => true,
            Some(e) => e.spawned.elapsed() >= self.timing.deadline,
        }
    }

    /// The seam kill (delete R5 / shutdown R5 / backstop R7): the one kill
    /// sequence with the SHORT per-trigger grace — snapshot descendants
    /// before the first signal, SIGTERM the (start-time-pinned) child,
    /// bounded grace, re-snapshot + SIGKILL, sweep the surviving snapshot
    /// pids' groups. When the owning runner thread is gone (died without
    /// closing), this also **reaps** the child via a bounded `WNOHANG`
    /// `waitpid` loop and evicts the entry — the backstop path must leave no
    /// zombie behind. While the runner thread lives, reaping and closure stay
    /// its job (it observes the death via `try_wait` and closes — usually an
    /// AlreadyClosed no-op after a delete/shutdown close). No-op for unknown
    /// run ids.
    ///
    /// Residual pid-reuse window, documented and accepted (the
    /// `script.rs::drain_captures` precedent): between this leg's liveness
    /// probe and its signal the live runner thread could reap and the pid be
    /// recycled — a sub-millisecond window requiring a full pid-space
    /// wraparound; every snapshot pid is start-time-verified besides.
    pub fn kill_run(&self, run_id: &str) {
        let Some((pid, start_time, runner_done)) = self
            .inflight
            .lock()
            .unwrap()
            .get(run_id)
            .map(|e| (e.pid, e.start_time, Arc::clone(&e.runner_done)))
        else {
            return;
        };
        // Registry lock released: nothing below holds it across the grace.
        if child_alive(pid, start_time) {
            let mut snapshot = descendants_of(pid);
            signal_pid(pid, SIGTERM);
            let end = Instant::now() + self.timing.seam_grace;
            while child_alive(pid, start_time) && Instant::now() < end {
                std::thread::sleep(self.timing.poll);
            }
            if child_alive(pid, start_time) {
                snapshot.extend(descendants_of(pid));
                signal_pid(pid, SIGKILL);
            }
            sweep_survivors(pid, &snapshot);
        }
        if runner_done.load(Ordering::Acquire) {
            // Owning thread gone: reap (bounded — SIGKILL death is prompt)
            // and evict, so the zombie clears and the entry can't shadow a
            // later run. `waitpid` is safe from any thread: the dead runner
            // dropped its `Child` handle un-reaped.
            reap_wnohang(pid, self.timing.seam_grace.max(Duration::from_millis(500)), self.timing.poll);
            self.inflight.lock().unwrap().remove(run_id);
        }
    }

    /// Kill every in-flight check (shutdown R5) — the registry-complete
    /// belt-and-braces mirroring [`super::script::ScriptRunner::kill_all_inflight`].
    pub fn kill_all_inflight(&self) {
        let ids: Vec<String> = self.inflight.lock().unwrap().keys().cloned().collect();
        for id in ids {
            self.kill_run(&id);
        }
    }

    /// Dispatch one headless check and return fast (the sweep must never
    /// block on a child): spawn the child, register it, start the reader
    /// threads and the named `fly-monitor-check` runner thread, and hand
    /// everything after — stream, deadline, kill, classification, closure —
    /// to that thread. Every failure path closes through the [`CheckCloser`]
    /// with [`CheckOutcome::spawn_failed`]; nothing is left half-registered.
    ///
    /// **Argv (R1, pane parity with `App.buildAgentArgv`):** `-p
    /// --output-format stream-json --verbose --dangerously-skip-permissions
    /// [--model M] [--effort E] [--fallback-model F] <prompt LAST>` — the
    /// prompt must be the final positional (a variadic flag before it would
    /// swallow it; the pane path orders identically). Skip-permissions is
    /// non-optional: without it every check stalls on its first tool ask
    /// into a deadline infra-fail. `launch` comes straight from
    /// [`super::resolve_agent_launch`] (R3 — U4 hands it over unchanged).
    ///
    /// **Process shape (R1/R13):** `process_group(0)` isolates claude from
    /// fly's signal context (a Ctrl-C to fly's group must not reach checks).
    /// It does NOT reach claude's tool children, which fork their own
    /// sessions per the plan's Empirical Contract — the descendant-snapshot
    /// sweep, not the group, is the real no-orphan guarantee. cwd = the
    /// automation's cwd; stdin null; stdout/stderr piped.
    ///
    /// **Env (R13, the "Clean env via the existing strip list" KTD):**
    /// inherit-minus-strip, NOT `env_clear` — claude legitimately consumes
    /// ambient env (HOME for `~/.claude` credentials, PATH, proxy/CA
    /// overrides), and the pane spawn precedent for claude is exactly this
    /// posture. Stripped: `FLY_PANE_TOKEN` + `FLY_SOCKET_PATH` (the one
    /// property that keeps installed hooks a measured no-op AND closes the
    /// automation-mutation socket surface — the headless R22 equivalent,
    /// guarded by a refactor-proof integration test) plus the shared
    /// child-session marker list ([`crate::pty::CLAUDE_SESSION_MARKERS`]).
    pub fn run(
        &self,
        automation_id: &str,
        run_id: &str,
        cwd: &str,
        prompt: &str,
        launch: &ResolvedLaunch,
    ) {
        let mut cmd = Command::new(&self.claude_bin);
        cmd.arg("-p")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose")
            .arg("--dangerously-skip-permissions");
        if let Some(model) = &launch.model {
            cmd.arg("--model").arg(model);
        }
        if let Some(effort) = &launch.effort {
            cmd.arg("--effort").arg(effort);
        }
        if let Some(fallback) = &launch.fallback {
            cmd.arg("--fallback-model").arg(fallback);
        }
        cmd.arg(prompt); // the final positional — nothing after it
        cmd.current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        cmd.env_remove("FLY_PANE_TOKEN")
            .env_remove("FLY_SOCKET_PATH");
        for marker in crate::pty::CLAUDE_SESSION_MARKERS {
            cmd.env_remove(marker);
        }

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                (self.closer)(
                    automation_id,
                    run_id,
                    CheckOutcome::spawn_failed(&format!(
                        "{e} (binary {:?}, cwd {cwd:?})",
                        self.claude_bin
                    )),
                );
                return;
            }
        };
        let pid = child.id();
        let start_time = read_stat(pid).map(|s| s.start_time);
        let spawned = Instant::now();
        let runner_done = Arc::new(AtomicBool::new(false));
        self.inflight.lock().unwrap().insert(
            run_id.to_owned(),
            InflightCheck {
                automation_id: automation_id.to_owned(),
                pid,
                start_time,
                spawned,
                runner_done: Arc::clone(&runner_done),
            },
        );

        // Reader threads. stdout/stderr are Some by construction (piped
        // above). If a reader thread cannot start (rare), its channel sender
        // drops and the runner's bounded recv degrades that stream to empty
        // — never wedged (the `script.rs::spawn_reader` tolerance).
        let stream = Arc::new(StreamShared::default());
        let (out_done_tx, rx_out_done) = mpsc::channel();
        let out_stream = Arc::clone(&stream);
        let stdout = child.stdout.take().expect("stdout piped");
        let _ = std::thread::Builder::new()
            .name("fly-monitor-check-out".into())
            .spawn(move || {
                read_stdout(stdout, &out_stream);
                let _ = out_done_tx.send(());
            });
        let rx_err = spawn_stderr_tail(child.stderr.take().expect("stderr piped"));

        let job = CheckJob {
            automation_id: automation_id.to_owned(),
            run_id: run_id.to_owned(),
            timing: self.timing.clone(),
            spawned,
            stream,
            rx_out_done,
            rx_err,
            closer: Arc::clone(&self.closer),
            inflight: Arc::clone(&self.inflight),
            runner_done: Arc::clone(&runner_done),
        };
        // The child rides to the runner thread in a cell so the
        // spawn-failure arm can take it back for an inline kill + reap
        // (`script.rs` pattern — a moved-into-closure child would be
        // unrecoverable after a failed thread spawn).
        let child_cell = Arc::new(Mutex::new(Some(child)));
        let thread_cell = Arc::clone(&child_cell);
        if let Err(e) = std::thread::Builder::new()
            .name("fly-monitor-check".into())
            .spawn(move || {
                let child = thread_cell
                    .lock()
                    .unwrap()
                    .take()
                    .expect("child handed to exactly one runner thread");
                drive(child, job);
            })
        {
            // No runner thread means no deadline and no closure: kill + reap
            // inline (zero grace — nothing useful can stream) and close.
            if let Some(mut child) = child_cell.lock().unwrap().take() {
                kill_and_sweep_child(&mut child, Duration::ZERO, self.timing.poll);
            }
            runner_done.store(true, Ordering::Release);
            self.inflight.lock().unwrap().remove(run_id);
            (self.closer)(
                automation_id,
                run_id,
                CheckOutcome::spawn_failed(&format!("could not start runner thread: {e}")),
            );
        }
    }
}

/// Everything the runner thread needs, moved off the dispatch path.
struct CheckJob {
    automation_id: String,
    run_id: String,
    timing: HeadlessTiming,
    spawned: Instant,
    stream: Arc<StreamShared>,
    rx_out_done: Receiver<()>,
    rx_err: Receiver<String>,
    closer: CheckCloser,
    inflight: Arc<Mutex<HashMap<String, InflightCheck>>>,
    runner_done: Arc<AtomicBool>,
}

/// The runner thread (named `fly-monitor-check`): monotonic `try_wait` poll
/// encoding the stream-end policy, then drain → classify → deregister →
/// close. Kill-confirm-then-close: every kill leg finishes its whole
/// SIGTERM→grace→SIGKILL+sweep sequence before the closer fires, so the
/// runner's close always beats the sweep backstop.
///
/// The policy, one leg per arm (R4/R10):
/// - child exited on its own → [`ExitFacts::Exited`] with its real code
///   (`None` = a signal the runner did not send — spontaneous, R10);
/// - result seen and the child outlives [`HeadlessTiming::linger_exit_grace`]
///   (clock from the result event — see [`LINGER_EXIT_GRACE`] for why not
///   EOF) → kill → [`ExitFacts::LingerKilled`] → Clean per R10. A child that
///   exits on its own *within* the grace lands in the first arm as
///   `Exited { code }` — exit 0 stays Clean, a spontaneous nonzero is Infra;
/// - stdout EOF with NO result → kill immediately, never wait out the
///   deadline (R11). **Documented choice:** the outcome is classified with
///   the child's ACTUAL exit facts from the kill — `Exited { code: None }`
///   when the runner's signal took it ("killed by a signal with no result
///   event" — the truthful infra reason), `Exited { code: Some(c) }` when it
///   exited by itself between EOF and the kill. Not `LingerKilled` (that
///   exempts a *post-result* kill only — no result means nothing to keep
///   Clean) and not `TimedOut` (the deadline never lapsed);
/// - the monotonic deadline lapses with no result in hand → kill →
///   [`ExitFacts::TimedOut`]. With a result in hand the linger clock owns
///   termination instead (it fires within `linger_exit_grace` of the result,
///   bounding the run at deadline + linger + term grace worst-case), so a
///   healthy verdict arriving just before the deadline is never discarded
///   as a timeout.
fn drive(mut child: Child, job: CheckJob) {
    // Marks the thread finished on EVERY exit — including a panic — so the
    // seam knows when reaping falls to it (see [`DoneGuard`]).
    let _done = DoneGuard(Arc::clone(&job.runner_done));
    let run_deadline = job.spawned + job.timing.deadline;
    let facts = loop {
        match child.try_wait() {
            Ok(Some(status)) => break ExitFacts::Exited { code: status.code() },
            Ok(None) => {}
            Err(_) => {
                // try_wait failed (should not happen for our own child):
                // fall back to a blocking reap so nothing zombies.
                break match child.wait() {
                    Ok(status) => ExitFacts::Exited { code: status.code() },
                    Err(_) => ExitFacts::Exited { code: None },
                };
            }
        }
        let now = Instant::now();
        // Load order matters: `eof` (Acquire) BEFORE `result_at` — the
        // reader stores `eof` (Release) after its final feed, so an observed
        // EOF is never paired with a stale `result_at: None` for a stream
        // whose result was actually fed (see [`StreamShared`]).
        let eof = job.stream.eof.load(Ordering::Acquire);
        let result_at = *job.stream.result_at.lock().unwrap();
        if let Some(at) = result_at {
            if now >= at + job.timing.linger_exit_grace {
                kill_and_sweep_child(&mut child, job.timing.term_grace, job.timing.poll);
                break ExitFacts::LingerKilled;
            }
        } else if eof {
            let status = kill_and_sweep_child(&mut child, job.timing.term_grace, job.timing.poll);
            break ExitFacts::Exited {
                code: status.and_then(|s| s.code()),
            };
        } else if now >= run_deadline {
            kill_and_sweep_child(&mut child, job.timing.term_grace, job.timing.poll);
            break ExitFacts::TimedOut;
        }
        std::thread::sleep(job.timing.poll);
    };
    // The child is reaped in every arm above (try_wait success, blocking
    // wait, or the kill sequence's own wait) — no zombie survives the loop.
    // Bounded drains: the pipes EOF once every holder is dead (the sweep
    // guarantees that on kill legs); the fold is complete once the stdout
    // reader is done, and a wedged reader degrades to whatever was fed.
    let _ = job.rx_out_done.recv_timeout(DRAIN_GRACE);
    let stderr_tail = job.rx_err.recv_timeout(DRAIN_GRACE).unwrap_or_default();
    let fold = job.stream.fold.lock().unwrap().clone();
    let outcome = fold.finish(facts, &stderr_tail);
    // Deregister BEFORE closing: the closer may consult the overlap probe
    // via the manager, and this run must no longer read as in flight.
    job.inflight.lock().unwrap().remove(&job.run_id);
    (job.closer)(&job.automation_id, &job.run_id, outcome);
}

// ---- stdout / stderr readers ------------------------------------------------------

/// Feed one complete line into the shared fold and stamp `result_at` on the
/// first result (under the fold lock's release, before the reader can set
/// `eof`).
fn feed_line_shared(shared: &StreamShared, line: &[u8]) {
    let has_result = {
        let mut fold = shared.fold.lock().unwrap();
        fold.feed_line(line);
        fold.has_result()
    };
    if has_result {
        let mut at = shared.result_at.lock().unwrap();
        if at.is_none() {
            *at = Some(Instant::now());
        }
    }
}

/// Append to the line accumulator, capped at `MAX_LINE_BYTES + 1` bytes: an
/// over-cap line must not buffer unboundedly (R11 at READ time), so the
/// remainder is read and discarded — the retained `cap + 1` length makes
/// [`parse_line`] skip it deterministically, even if the discarded tail
/// would have completed valid JSON (a truncated prefix must never
/// half-parse).
fn append_capped(line: &mut Vec<u8>, chunk: &[u8]) {
    let room = (MAX_LINE_BYTES + 1).saturating_sub(line.len());
    line.extend_from_slice(&chunk[..chunk.len().min(room)]);
}

/// The stdout reader (thread `fly-monitor-check-out`): byte-exact line
/// assembly across read chunks — a multibyte UTF-8 character split across
/// two reads inside a line is reassembled before the fold ever sees it (the
/// module-doc bytes-end-to-end rule) — with the per-line cap enforced at
/// read time ([`append_capped`]). A final unterminated line is fed too
/// (claude ends lines with `\n`, but a dying child may not). Stores `eof`
/// last (Release — see [`StreamShared`]).
fn read_stdout(mut r: impl Read, shared: &StreamShared) {
    let mut buf = vec![0u8; READ_BUF];
    let mut line: Vec<u8> = Vec::new();
    loop {
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let mut rest = &buf[..n];
                while let Some(pos) = rest.iter().position(|&b| b == b'\n') {
                    append_capped(&mut line, &rest[..pos]);
                    feed_line_shared(shared, &line);
                    line.clear();
                    rest = &rest[pos + 1..];
                }
                append_capped(&mut line, rest);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    if !line.is_empty() {
        feed_line_shared(shared, &line);
    }
    shared.eof.store(true, Ordering::Release);
}

/// Drain a reader to EOF keeping only the trailing `cap` bytes (the
/// [`STDERR_TAIL_BYTES`] tail). Byte-granular cut; the lossy conversion
/// renders a split leading char as U+FFFD — head damage only.
fn drain_tail(mut r: impl Read, cap: usize) -> String {
    let mut tail: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                tail.extend_from_slice(&buf[..n]);
                if tail.len() > cap {
                    let cut = tail.len() - cap;
                    tail.drain(..cut);
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&tail).into_owned()
}

/// Start the stderr tail thread; the final tail arrives on the channel.
fn spawn_stderr_tail(r: impl Read + Send + 'static) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("fly-monitor-check-err".into())
        .spawn(move || {
            let _ = tx.send(drain_tail(r, STDERR_TAIL_BYTES));
        });
    rx
}

// ---- kill discipline (R5) ---------------------------------------------------------

const SIGTERM: i32 = libc::SIGTERM;
const SIGKILL: i32 = libc::SIGKILL;

/// Send a signal to one pid. ESRCH (already gone) is ignored.
fn signal_pid(pid: u32, sig: i32) {
    // SAFETY: kill(2) is always safe to call; a stale pid yields ESRCH.
    unsafe {
        libc::kill(pid as libc::pid_t, sig);
    }
}

/// The one kill sequence (R5, the "SIGTERM-first kill" KTD), runner-leg
/// shape — the caller owns the `Child`, so the direct child is signaled
/// while **unreaped** (the kernel pins an un-`wait()`ed pid; `pty/pane.rs`
/// / `script.rs` precedent) and needs no start-time pin of its own:
///
/// 1. snapshot descendants via `/proc` BEFORE the first signal — post-mortem
///    discovery is impossible, orphans reparent to init and a PPID walk
///    misses them;
/// 2. SIGTERM the child (empirically claude then reaps its own child tree,
///    ~6 s worst observed);
/// 3. bounded per-trigger grace on `try_wait`;
/// 4. still alive → re-snapshot (catches anything forked during the grace),
///    SIGKILL the child, blocking reap;
/// 5. sweep the union of snapshot pids ([`sweep_survivors`]): each survivor
///    is start-time-verified, then SIGKILLed directly AND via its process
///    group.
///
/// Returns the child's exit status (`None` only if even the blocking reap
/// failed). Deaths between snapshot and kill are benign — ESRCH ignored.
fn kill_and_sweep_child(child: &mut Child, grace: Duration, poll: Duration) -> Option<ExitStatus> {
    let pid = child.id();
    let mut snapshot = descendants_of(pid);
    signal_pid(pid, SIGTERM);
    let end = Instant::now() + grace;
    let mut status: Option<ExitStatus> = None;
    loop {
        if let Ok(Some(st)) = child.try_wait() {
            status = Some(st);
            break;
        }
        if Instant::now() >= end {
            break;
        }
        std::thread::sleep(poll);
    }
    if status.is_none() {
        // Re-snapshot while the child is still alive (its descendants are
        // still PPID-reachable), then hard-kill and reap.
        snapshot.extend(descendants_of(pid));
        signal_pid(pid, SIGKILL);
        status = child.wait().ok();
    }
    sweep_survivors(pid, &snapshot);
    status
}

/// SIGKILL every surviving snapshot pid and its process group (R5 step 5).
/// Each pid is re-`stat`ed first: gone or start-time-mismatched (pid reused)
/// or already a zombie → skipped. The group kill uses the survivor's
/// *current* pgid from that same fresh stat; groups ≤ 1 and fly's own group
/// are never signaled (a hostile descendant could `setpgid` into fly's group
/// — same session — and a blind `kill(-pgid)` would take fly down with it).
fn sweep_survivors(child_pid: u32, snapshot: &[ProcStat]) {
    // SAFETY: getpgrp(2) has no failure mode.
    let own_pgid = unsafe { libc::getpgrp() } as u32;
    let mut seen: HashSet<u32> = HashSet::new();
    for p in snapshot {
        if p.pid == child_pid || !seen.insert(p.pid) {
            continue;
        }
        let Some(cur) = read_stat(p.pid) else { continue };
        if cur.start_time != p.start_time || matches!(cur.state, 'Z' | 'X') {
            continue;
        }
        signal_pid(p.pid, SIGKILL);
        if cur.pgid > 1 && cur.pgid != own_pgid {
            // SAFETY: kill(2); a stale pgid yields ESRCH, ignored.
            unsafe {
                libc::kill(-(cur.pgid as libc::pid_t), SIGKILL);
            }
        }
    }
}

/// Bounded non-blocking reap for the seam's orphaned-runner path: `waitpid`
/// with `WNOHANG` until the child is reaped (`> 0`), turns out not to be
/// ours / already reaped (`-1`, ECHILD), or the bound lapses.
fn reap_wnohang(pid: u32, bound: Duration, poll: Duration) {
    let end = Instant::now() + bound;
    loop {
        let mut status: libc::c_int = 0;
        // SAFETY: waitpid(2) with WNOHANG never blocks; an invalid pid
        // yields ECHILD (-1), which ends the loop.
        let r = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
        if r != 0 {
            return;
        }
        if Instant::now() >= end {
            return;
        }
        std::thread::sleep(poll);
    }
}

// ---- /proc reading ---------------------------------------------------------------

/// The slice of `/proc/<pid>/stat` the kill discipline needs. Field numbers
/// per `proc(5)`: `ppid` (4), `pgrp` (5), `starttime` (22) — the latter is
/// clock ticks since boot at process start, the pid-reuse pin.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcStat {
    pid: u32,
    ppid: u32,
    pgid: u32,
    start_time: u64,
    /// Single-char run state (`R`/`S`/`Z`/`X`/…) — zombies are "dead" to
    /// liveness probes (unsignalable, kept only for their parent's reap).
    state: char,
}

/// Parse one `stat` line. The parsing trap (the `cwd::parse_stat_line`
/// precedent): field 2 (`comm`) is parenthesized and may itself contain
/// spaces and parens, so split on the LAST `')'`; the space-separated
/// remainder is fields 3.. — index 0 = `state`, 1 = `ppid`, 2 = `pgrp`,
/// 19 = `starttime`. Any missing/non-numeric field yields `None` (skipped,
/// never a panic — same tolerance as the rest of fly's `/proc` readers).
fn parse_stat_line(line: &str) -> Option<ProcStat> {
    let lparen = line.find('(')?;
    let rparen = line.rfind(')')?;
    if rparen < lparen {
        return None;
    }
    let pid: u32 = line[..lparen].trim().parse().ok()?;
    let rest: Vec<&str> = line[rparen + 1..].split_whitespace().collect();
    let state = rest.first()?.chars().next()?;
    let ppid: u32 = rest.get(1)?.parse().ok()?;
    let pgid: u32 = rest.get(2)?.parse().ok()?;
    let start_time: u64 = rest.get(19)?.parse().ok()?;
    Some(ProcStat {
        pid,
        ppid,
        pgid,
        start_time,
        state,
    })
}

/// Read + parse one pid's stat. `None` when the process is gone/unreadable.
fn read_stat(pid: u32) -> Option<ProcStat> {
    let line = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat_line(&line)
}

/// Whether the pinned child is still alive: `/proc` present, start-time
/// matching the recorded pin (a reused pid has a later starttime), and not
/// a zombie/dead. `recorded: None` (the spawn-time stat read failed)
/// degrades to a bare existence check.
fn child_alive(pid: u32, recorded: Option<u64>) -> bool {
    match read_stat(pid) {
        None => false,
        Some(s) => {
            if matches!(s.state, 'Z' | 'X') {
                return false;
            }
            match recorded {
                Some(r) => s.start_time == r,
                None => true,
            }
        }
    }
}

/// Snapshot the transitive descendants of `root` (root excluded): one
/// `/proc` scan (the `cwd::read_proc_table` shape — vanished/unreadable
/// entries silently skipped), PPID-edge BFS with a visited set so a
/// malformed table (self-parent, cycle) cannot loop. Each descendant is
/// recorded with its start-time so the later sweep can pin against pid
/// reuse. Cross-session residue accepted like `script.rs::drain_captures`.
fn descendants_of(root: u32) -> Vec<ProcStat> {
    let mut table: Vec<ProcStat> = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return table;
    };
    for entry in dir.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{name}/stat")) {
            if let Some(s) = parse_stat_line(&stat) {
                table.push(s);
            }
        }
    }
    let mut children: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, e) in table.iter().enumerate() {
        children.entry(e.ppid).or_default().push(i);
    }
    let mut visited: HashSet<u32> = HashSet::from([root]);
    let mut out: Vec<ProcStat> = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        let Some(kids) = children.get(&parent) else { continue };
        for &i in kids {
            let e = &table[i];
            if visited.insert(e.pid) {
                out.push(e.clone());
                frontier.push(e.pid);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fold a whole stream of byte lines and settle it — the runner's shape.
    fn classify(lines: &[&[u8]], end: ExitFacts, stderr_tail: &str) -> CheckOutcome {
        let mut fold = StreamFold::new();
        for line in lines {
            fold.feed_line(line);
        }
        fold.finish(end, stderr_tail)
    }

    fn reason(outcome: CheckOutcome) -> String {
        match outcome {
            CheckOutcome::Infra { reason } => reason,
            other => panic!("expected Infra, got {other:?}"),
        }
    }

    // The captured real stream shape (Empirical Contract, claude 2.1.207):
    // init + assistant + rate_limit_event + result → Clean with the exact
    // result text and the init's session id (R10, R12).
    #[test]
    fn captured_real_stream_is_clean_with_exact_text_and_session_id() {
        let lines: &[&[u8]] = &[
            br#"{"type":"system","subtype":"init","cwd":"/home/u/exp","session_id":"5b1e2c7a-9d1f-4b6e-8a2e-0c3d4e5f6a7b","tools":["Bash","Read"],"model":"claude-sonnet-4-5-20250929","permissionMode":"bypassPermissions","apiKeySource":"none"}"#,
            br#"{"type":"assistant","message":{"id":"msg_014abc","type":"message","role":"assistant","model":"claude-sonnet-4-5-20250929","content":[{"type":"text","text":"Checking the experiment now."}],"stop_reason":"end_turn"},"session_id":"5b1e2c7a-9d1f-4b6e-8a2e-0c3d4e5f6a7b"}"#,
            br#"{"type":"rate_limit_event","rate_limit":{"status":"allowed","unifiedRateLimitFallbackAvailable":false}}"#,
            br#"{"type":"result","subtype":"success","is_error":false,"duration_ms":6321,"duration_api_ms":5100,"num_turns":3,"result":"The run is healthy; loss still falling.","session_id":"5b1e2c7a-9d1f-4b6e-8a2e-0c3d4e5f6a7b","total_cost_usd":0.0421,"usage":{"input_tokens":4,"output_tokens":128}}"#,
        ];
        assert_eq!(
            classify(lines, ExitFacts::Exited { code: Some(0) }, ""),
            CheckOutcome::Clean {
                text: "The run is healthy; loss still falling.".to_owned(),
                session_id: Some("5b1e2c7a-9d1f-4b6e-8a2e-0c3d4e5f6a7b".to_owned()),
            }
        );
    }

    // R11: unknown event types and system subtypes are ignored — including
    // the observed wild ones (`thinking_tokens`, `hook_started`) and events
    // AFTER the result (a `task_notification` was observed there live).
    // Result-then-more-events is still Clean.
    #[test]
    fn unknown_types_and_subtypes_are_ignored_even_after_the_result() {
        let lines: &[&[u8]] = &[
            br#"{"type":"system","subtype":"init","session_id":"s-1","model":"m","cwd":"/x"}"#,
            br#"{"type":"system","subtype":"hook_started","hook":"SessionStart"}"#,
            br#"{"type":"system","subtype":"thinking_tokens","tokens":512}"#,
            br#"{"type":"telemetry","subtype":"v99","payload":{"future":"shape"}}"#,
            br#"{"type":"result","subtype":"success","is_error":false,"result":"done","session_id":"s-1","num_turns":1}"#,
            br#"{"type":"system","subtype":"task_notification","task_id":"t1","status":"completed"}"#,
        ];
        assert_eq!(
            classify(lines, ExitFacts::Exited { code: Some(0) }, ""),
            CheckOutcome::Clean {
                text: "done".to_owned(),
                session_id: Some("s-1".to_owned()),
            }
        );
    }

    // Abstain-on-surprise (the verdict two-blocks rule, applied to the
    // stream): a second result event → Infra, even with a clean exit and
    // even though both results claimed success.
    #[test]
    fn a_second_result_event_is_a_surprise_infra() {
        let lines: &[&[u8]] = &[
            br#"{"type":"result","subtype":"success","is_error":false,"result":"first"}"#,
            br#"{"type":"result","subtype":"success","is_error":false,"result":"second"}"#,
        ];
        let r = reason(classify(lines, ExitFacts::Exited { code: Some(0) }, ""));
        assert!(r.contains("more than one result"), "reason: {r}");
    }

    // R10 malformed-stream row: EOF with no result event, clean exit → Infra.
    #[test]
    fn eof_with_no_result_and_exit_zero_is_infra() {
        let lines: &[&[u8]] = &[
            br#"{"type":"system","subtype":"init","session_id":"s-1"}"#,
            br#"{"type":"assistant","message":{"content":[{"type":"text","text":"hm"}]}}"#,
        ];
        let r = reason(classify(lines, ExitFacts::Exited { code: Some(0) }, ""));
        assert!(r.contains("no result event"), "reason: {r}");
        // Defensive corner of the same row: a runner linger-kill with no
        // result is equally malformed (the runner only linger-kills after a
        // result, so the combination is itself a surprise).
        let r = reason(classify(lines, ExitFacts::LingerKilled, ""));
        assert!(r.contains("no result event"), "reason: {r}");
    }

    // R10: nonzero exit without a result → Infra carrying the exit code and
    // the raw stderr tail. The tail lands verbatim BY CONTRACT — the caller
    // (U5's shared sanitize → scrub helper) cleans it before storage; this
    // module never does (see CheckOutcome's doc).
    #[test]
    fn nonzero_exit_with_no_result_carries_code_and_stderr_tail() {
        let r = reason(classify(
            &[br#"{"type":"system","subtype":"init","session_id":"s-1"}"#],
            ExitFacts::Exited { code: Some(3) },
            "Error: ENOENT no such file",
        ));
        assert!(r.contains('3'), "reason carries the exit code: {r}");
        assert!(
            r.contains("Error: ENOENT no such file"),
            "reason carries the raw tail: {r}"
        );
        // A spontaneous signal death with no result is the same row.
        let r = reason(classify(
            &[],
            ExitFacts::Exited { code: None },
            "killed from outside",
        ));
        assert!(r.contains("signal"), "reason: {r}");
        assert!(r.contains("killed from outside"), "reason: {r}");
    }

    // R10's timeout row: the deadline kill is Infra — with no result, and
    // (flat rule) even after a success result, because the runner's
    // stream-end policy means a post-result deadline is itself surprising.
    #[test]
    fn timeout_is_infra_timed_out() {
        let r = reason(classify(&[], ExitFacts::TimedOut, ""));
        assert!(r.contains("timed out"), "reason: {r}");

        let lines: &[&[u8]] =
            &[br#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#];
        let r = reason(classify(lines, ExitFacts::TimedOut, ""));
        assert!(r.contains("timed out"), "reason: {r}");
    }

    // R10: a non-success result is Infra, never Clean — `is_error: true`
    // (even with subtype "success"), an unobserved error subtype, or a
    // missing subtype all abstain, exit 0 notwithstanding.
    #[test]
    fn is_error_true_or_non_success_subtype_is_infra_never_clean() {
        let cases: &[&[u8]] = &[
            br#"{"type":"result","subtype":"success","is_error":true,"result":"lying"}"#,
            br#"{"type":"result","subtype":"error_during_execution","is_error":false,"result":"x"}"#,
            br#"{"type":"result","is_error":false,"result":"no subtype at all"}"#,
        ];
        for line in cases {
            let r = reason(classify(&[line], ExitFacts::Exited { code: Some(0) }, ""));
            assert!(r.contains("non-success"), "reason: {r}");
        }
    }

    // R11: a malformed JSON line mid-stream is skipped; the surrounding
    // stream still classifies normally.
    #[test]
    fn malformed_json_line_mid_stream_is_skipped() {
        let lines: &[&[u8]] = &[
            br#"{"type":"system","subtype":"init","session_id":"s-1"}"#,
            b"}{ this is not JSON at all",
            b"",
            br#"{"type":"result","subtype":"success","is_error":false,"result":"fine"}"#,
        ];
        assert_eq!(
            classify(lines, ExitFacts::Exited { code: Some(0) }, ""),
            CheckOutcome::Clean {
                text: "fine".to_owned(),
                session_id: Some("s-1".to_owned()),
            }
        );
    }

    // R11: a stream that is only garbage ends result-less → Infra.
    #[test]
    fn a_stream_of_only_garbage_is_infra() {
        let lines: &[&[u8]] = &[b"not json", b"also not json", b"\x1b[31mstill not\x1b[0m"];
        let r = reason(classify(lines, ExitFacts::Exited { code: Some(0) }, ""));
        assert!(r.contains("no result event"), "reason: {r}");
    }

    // R11: a line over MAX_LINE_BYTES is skipped without parsing — even when
    // it would have been a valid success result — and the run then ends
    // Infra because the skipped line swallowed the result.
    #[test]
    fn an_over_cap_line_is_skipped_without_parsing_and_a_swallowed_result_is_infra() {
        let huge = format!(
            r#"{{"type":"result","subtype":"success","is_error":false,"result":"{}"}}"#,
            "x".repeat(MAX_LINE_BYTES)
        );
        assert!(huge.len() > MAX_LINE_BYTES);
        assert_eq!(parse_line(huge.as_bytes()), None, "skipped, not parsed");

        let init: &[u8] = br#"{"type":"system","subtype":"init","session_id":"s-1"}"#;
        let r = reason(classify(
            &[init, huge.as_bytes()],
            ExitFacts::Exited { code: Some(0) },
            "",
        ));
        assert!(r.contains("no result event"), "reason: {r}");
    }

    // R10: a success result already seen stays Clean when the subsequent
    // exit is the runner's own lingering-exit kill (the backgrounding
    // quirk); a *spontaneous* nonzero or signal exit after the result
    // remains infra — only the runner's kill is exempt.
    #[test]
    fn success_result_then_runner_linger_kill_stays_clean() {
        let lines: &[&[u8]] = &[
            br#"{"type":"system","subtype":"init","session_id":"s-9"}"#,
            br#"{"type":"result","subtype":"success","is_error":false,"result":"parked; still training"}"#,
        ];
        assert_eq!(
            classify(lines, ExitFacts::LingerKilled, ""),
            CheckOutcome::Clean {
                text: "parked; still training".to_owned(),
                session_id: Some("s-9".to_owned()),
            }
        );
    }

    #[test]
    fn spontaneous_nonzero_or_signal_exit_after_a_result_is_infra() {
        let lines: &[&[u8]] =
            &[br#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#];
        let r = reason(classify(lines, ExitFacts::Exited { code: Some(1) }, ""));
        assert!(r.contains("spontaneous"), "reason: {r}");
        assert!(r.contains('1'), "reason carries the code: {r}");
        // Empty stderr tail ⇒ no dangling "; stderr:" fragment.
        assert!(!r.contains("stderr"), "reason: {r}");

        let r = reason(classify(lines, ExitFacts::Exited { code: None }, ""));
        assert!(r.contains("signal"), "reason: {r}");
    }

    // R10: an empty result text is Clean with empty text — the empty→None
    // mapping (which makes the row read unreadable in the derived counter)
    // happens downstream in the shared cleaning helper (U4), never here.
    #[test]
    fn empty_result_text_is_clean_with_empty_text() {
        let lines: &[&[u8]] =
            &[br#"{"type":"result","subtype":"success","is_error":false,"result":""}"#];
        assert_eq!(
            classify(lines, ExitFacts::Exited { code: Some(0) }, ""),
            CheckOutcome::Clean {
                text: String::new(),
                session_id: None,
            }
        );
    }

    // Bytes end to end: a line containing multibyte UTF-8 parses intact when
    // fed as bytes (the runner splits at line boundaries only, so no lossy
    // conversion can land mid-character).
    #[test]
    fn multibyte_utf8_lines_parse_intact_as_bytes() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"naïve — ✓ 完了 🚀"}"#;
        assert_eq!(
            classify(&[line.as_bytes()], ExitFacts::Exited { code: Some(0) }, ""),
            CheckOutcome::Clean {
                text: "naïve — ✓ 完了 🚀".to_owned(),
                session_id: None,
            }
        );
        // Invalid UTF-8 bytes are just an unparsable (skipped) line — never
        // lossy-converted into something that half-parses.
        assert_eq!(parse_line(b"{\"type\":\"result\xff\xfe\"}"), None);
    }

    // R10's spawn-failure row: no child, no stream — the runner constructs
    // the infra outcome directly from the spawn error.
    #[test]
    fn spawn_failure_is_infra_with_the_error() {
        let r = reason(CheckOutcome::spawn_failed("No such file or directory (os error 2)"));
        assert!(r.contains("spawn failed"), "reason: {r}");
        assert!(r.contains("os error 2"), "reason: {r}");
    }

    // R12: no init ⇒ session id None (Clean still stands on the result
    // alone); and the first init wins over a later one, so the stamped id
    // is stable.
    #[test]
    fn session_id_is_none_without_init_and_first_init_wins() {
        let lines: &[&[u8]] =
            &[br#"{"type":"result","subtype":"success","is_error":false,"result":"ok"}"#];
        assert_eq!(
            classify(lines, ExitFacts::Exited { code: Some(0) }, ""),
            CheckOutcome::Clean {
                text: "ok".to_owned(),
                session_id: None,
            }
        );

        let lines: &[&[u8]] = &[
            br#"{"type":"system","subtype":"init","session_id":"first"}"#,
            br#"{"type":"system","subtype":"init","session_id":"second"}"#,
            br#"{"type":"result","subtype":"success","is_error":false,"result":"ok"}"#,
        ];
        assert_eq!(
            classify(lines, ExitFacts::Exited { code: Some(0) }, ""),
            CheckOutcome::Clean {
                text: "ok".to_owned(),
                session_id: Some("first".to_owned()),
            }
        );
    }

    // ================= U3 pure helpers (no processes) =================

    // R5: the stat parse survives the proc(5) comm trap (spaces + parens)
    // and pulls starttime from field 22 — the pid-reuse pin the whole kill
    // discipline leans on.
    #[test]
    fn parse_stat_line_extracts_pgid_and_starttime_past_a_hostile_comm() {
        // 22 fields' worth after the comm: state ppid pgrp session tty tpgid
        // flags minflt cminflt majflt cmajflt utime stime cutime cstime
        // priority nice threads itrealvalue STARTTIME ...
        let line = "4242 (weird (proc) name) S 4200 4241 4200 0 -1 4194560 \
                    10 0 0 0 1 2 0 0 20 0 1 0 987654321 12345 67";
        let s = parse_stat_line(line).expect("parses despite parens in comm");
        assert_eq!(s.pid, 4242);
        assert_eq!(s.state, 'S');
        assert_eq!(s.ppid, 4200);
        assert_eq!(s.pgid, 4241);
        assert_eq!(s.start_time, 987_654_321);
    }

    #[test]
    fn parse_stat_line_rejects_truncated_or_malformed_lines() {
        assert!(parse_stat_line("").is_none());
        assert!(parse_stat_line("no parens").is_none());
        // Enough fields for ppid/pgrp but truncated before starttime (19).
        assert!(parse_stat_line("1 (init) S 0 1 1 0 -1 4194560 10").is_none());
        assert!(parse_stat_line("abc (c) S 1 1 1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 5").is_none());
    }

    #[test]
    fn read_stat_sees_our_own_process() {
        let me = std::process::id();
        let s = read_stat(me).expect("own /proc stat is readable");
        assert_eq!(s.pid, me);
        assert!(s.start_time > 0, "starttime is a real tick count");
        assert!(!matches!(s.state, 'Z' | 'X'));
        assert!(child_alive(me, Some(s.start_time)));
        assert!(
            !child_alive(me, Some(s.start_time + 1)),
            "a start-time mismatch reads as a reused pid — dead"
        );
    }

    // R11 at read time: the accumulator caps at MAX_LINE_BYTES + 1, so an
    // over-cap line is retained just long enough to be deterministically
    // skipped by parse_line — even when the truncated prefix would have been
    // complete valid JSON on its own.
    #[test]
    fn append_capped_bounds_an_over_cap_line_to_a_deterministic_skip() {
        let mut line = Vec::new();
        append_capped(&mut line, &vec![b'x'; MAX_LINE_BYTES]);
        assert_eq!(line.len(), MAX_LINE_BYTES);
        append_capped(&mut line, &vec![b'y'; 4096]);
        assert_eq!(line.len(), MAX_LINE_BYTES + 1, "one byte over — never more");
        append_capped(&mut line, b"more");
        assert_eq!(line.len(), MAX_LINE_BYTES + 1);
        assert_eq!(parse_line(&line), None, "over-cap → skipped, unparsed");
    }

    // The reader's line assembly is byte-exact across chunk boundaries: a
    // stream delivered in awkward slices — including a split INSIDE a
    // multibyte character — folds identically to the whole-line feed.
    #[test]
    fn read_stdout_reassembles_lines_and_split_utf8_across_chunks() {
        struct Chunked(Vec<Vec<u8>>);
        impl std::io::Read for Chunked {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.0.is_empty() {
                    return Ok(0);
                }
                let chunk = self.0.remove(0);
                buf[..chunk.len()].copy_from_slice(&chunk);
                Ok(chunk.len())
            }
        }
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"完了 🚀"}"#;
        let bytes = format!("{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s-1\"}}\n{line}\n");
        let bytes = bytes.as_bytes();
        // Split mid-🚀 (a 4-byte char): find its first byte and cut inside it.
        let rocket = bytes.windows(4).position(|w| w == "🚀".as_bytes()).unwrap();
        let chunks = vec![
            bytes[..rocket + 2].to_vec(),
            bytes[rocket + 2..rocket + 3].to_vec(),
            bytes[rocket + 3..].to_vec(),
        ];
        let shared = StreamShared::default();
        read_stdout(Chunked(chunks), &shared);
        assert!(shared.eof.load(Ordering::Acquire));
        assert!(shared.result_at.lock().unwrap().is_some());
        let fold = shared.fold.lock().unwrap().clone();
        assert_eq!(
            fold.finish(ExitFacts::Exited { code: Some(0) }, ""),
            CheckOutcome::Clean {
                text: "完了 🚀".to_owned(),
                session_id: Some("s-1".to_owned()),
            }
        );
    }

    // A final unterminated line (a dying child may never send the trailing
    // newline) is still fed.
    #[test]
    fn read_stdout_feeds_a_final_unterminated_line() {
        let shared = StreamShared::default();
        read_stdout(
            &br#"{"type":"result","subtype":"success","is_error":false,"result":"no newline"}"#[..],
            &shared,
        );
        let fold = shared.fold.lock().unwrap().clone();
        assert!(fold.has_result());
    }

    #[test]
    fn drain_tail_keeps_the_trailing_bytes() {
        let data = [vec![b'a'; 5000], b"THE END".to_vec()].concat();
        let tail = drain_tail(&data[..], 16);
        assert_eq!(tail.len(), 16);
        assert!(tail.ends_with("THE END"));
    }
}
