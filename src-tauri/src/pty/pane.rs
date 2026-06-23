//! A single PTY-backed pane: spawn, a dedicated blocking read thread, resize,
//! input, and ordered teardown (KTD2, KTD13).
//!
//! Concurrency discipline (KTD13):
//! - The reader/writer are cloned out of the PTY master so the read thread
//!   never holds the pane registry lock during blocking I/O.
//! - The child is owned solely by the read thread, which reaps it with
//!   `wait()` when the read side ends — so no zombie remains.
//! - Teardown sets a `stopping` flag, signals the child (SIGHUP → grace →
//!   SIGKILL), and joins the read thread, so the child is reaped before the
//!   pane is dropped and no accessor ever resolves a half-dead pane.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

use super::{ExitCallback, OutputSink, PaneId, SpawnConfig};
use crate::state::activity;
use crate::state::lifecycle::LifecycleState;

/// PTY read buffer. A large buffer gives natural coalescing under load (a
/// single `read` returns a big chunk during a flood) while staying low-latency
/// when idle (it returns immediately with whatever's there) — KTD4.
const READ_BUF: usize = 64 * 1024;
/// How long to wait after SIGHUP before escalating to SIGKILL on close.
const GRACE: Duration = Duration::from_millis(200);
/// Output chunks smaller than this don't anchor or extend a work stretch (U3):
/// a lone keystroke echo or a tiny cursor/spinner redraw is not "the agent
/// working". Deliberately small — real agent output arrives in larger bursts —
/// so a slowly-generating agent is never mistaken for idle. The exact value is
/// tuned live (see the plan's Open Questions); the attention gate (KTD-E) is the
/// primary idle defense, this is the secondary one.
const MIN_OUTPUT_BYTES: usize = 2;

/// Per-pane output-activity state shared with the read thread (U3, KTD-A/KTD-B).
/// The read thread records above-threshold output chunks; the dashboard poll
/// queries the current work stretch. Backed by atomics so the record path holds
/// no lock and never perturbs the byte stream (KTD-B). `has_stretch` is an
/// explicit presence flag rather than overloading `0` — a first chunk arriving
/// in the same millisecond as the epoch legitimately records `now == 0`.
struct ActivityCell {
    /// Per-pane monotonic clock; only durations are reported, so it needs no
    /// cross-pane comparability (resolves the "no global clock" gap).
    epoch: Instant,
    last_output_ms: AtomicU64,
    work_start_ms: AtomicU64,
    has_stretch: AtomicBool,
}

impl ActivityCell {
    fn new() -> Self {
        Self {
            epoch: Instant::now(),
            last_output_ms: AtomicU64::new(0),
            work_start_ms: AtomicU64::new(0),
            has_stretch: AtomicBool::new(false),
        }
    }

    /// ms since this cell's epoch.
    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// Record an output chunk of `n` bytes at the current time.
    fn record(&self, n: usize) {
        self.record_at(self.now_ms(), n);
    }

    /// Record an output chunk at an explicit `now` (time-injected for tests).
    /// Sub-[`MIN_OUTPUT_BYTES`] chunks are ignored. Relaxed ordering: this is a
    /// display signal, and the read thread is the only writer.
    fn record_at(&self, now: u64, n: usize) {
        if n < MIN_OUTPUT_BYTES {
            return;
        }
        let (last, start) = if self.has_stretch.load(Ordering::Relaxed) {
            (
                Some(self.last_output_ms.load(Ordering::Relaxed)),
                Some(self.work_start_ms.load(Ordering::Relaxed)),
            )
        } else {
            (None, None)
        };
        let (new_last, new_start) = activity::record(last, start, now, activity::IDLE_GAP_MS);
        self.last_output_ms.store(new_last, Ordering::Relaxed);
        self.work_start_ms.store(new_start, Ordering::Relaxed);
        self.has_stretch.store(true, Ordering::Relaxed);
    }

    /// `(working_for_ms, last_output_ago_ms)` at the current time.
    fn snapshot(&self) -> (Option<u64>, Option<u64>) {
        self.snapshot_at(self.now_ms())
    }

    /// `(working_for_ms, last_output_ago_ms)` at an explicit `now`. The pair is
    /// read with Relaxed loads and may, in principle, tear across a concurrent
    /// record — at a ~1.5s poll vs a sub-ms write that is unobservable and
    /// self-corrects on the next poll (KTD-B).
    fn snapshot_at(&self, now: u64) -> (Option<u64>, Option<u64>) {
        if !self.has_stretch.load(Ordering::Relaxed) {
            return (None, None);
        }
        let last = self.last_output_ms.load(Ordering::Relaxed);
        let start = self.work_start_ms.load(Ordering::Relaxed);
        let working_for =
            activity::current_stretch(Some(last), Some(start), now, activity::IDLE_GAP_MS);
        (working_for, Some(now.saturating_sub(last)))
    }
}

/// State shared between a pane and its read thread.
struct PaneShared {
    /// Authoritative lifecycle. The read thread sets the terminal state after
    /// reaping; the registry and UI read it.
    lifecycle: Mutex<LifecycleState>,
    /// Set by teardown to mark close-intent: the reaped state becomes `Killed`
    /// rather than `Exited`.
    stopping: AtomicBool,
    /// Set true once the child has been reaped, so teardown never signals a
    /// pid that may have been recycled.
    reaped: AtomicBool,
    /// Fires once when the child is reaped (lets teardown time its SIGKILL
    /// escalation without a busy-wait).
    reaped_tx: Sender<()>,
    /// Backpressure (KTD4): when set, the read thread parks instead of issuing
    /// reads, so the kernel PTY buffer backpressures the producer losslessly.
    /// The thread is never torn down — pause/resume just gate reads.
    paused: Mutex<bool>,
    pause_cv: Condvar,
    /// Per-pane output-activity state (U3): the "current work stretch" the agent
    /// dashboard reads. Recorded by the read thread, queried by the poll.
    activity: ActivityCell,
}

/// A live PTY pane.
pub struct Pane {
    pub id: PaneId,
    /// PTY master. `resize`/`take_writer` take `&self`; only ever touched under
    /// the registry lock, so `Send` is sufficient.
    master: Box<dyn MasterPty + Send>,
    /// Writer behind its own lock so writes don't hold the registry lock.
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// Child pid, used to signal the process on close. Valid only while live.
    pid: Option<u32>,
    shared: Arc<PaneShared>,
    reaped_rx: Receiver<()>,
    reader_handle: Option<JoinHandle<()>>,
    /// Per-pane auth token for the hook channel (U8); registered before the
    /// child starts so no callback can race registration.
    token: String,
    /// The frontend's stable leaf key (U3), so the hook dispatch can key this
    /// pane's resume record. `None` for headless/test spawns.
    leaf_key: Option<String>,
}

impl Pane {
    /// Spawn a shell into a fresh PTY and start its read thread.
    ///
    /// `token` is the per-pane secret already registered with the hook server
    /// (U8); it is injected into the child env by the caller via `cfg.env`.
    pub fn spawn(
        id: PaneId,
        cfg: SpawnConfig,
        token: String,
        sink: OutputSink,
        on_exit: ExitCallback,
    ) -> Result<Pane, String> {
        let leaf_key = cfg.leaf_key.clone();
        let pty_system = native_pty_system();
        let size = PtySize {
            rows: cfg.rows.max(1),
            cols: cfg.cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = pty_system
            .openpty(size)
            .map_err(|e| format!("openpty failed: {e}"))?;

        // A resume spawn (U6/KTD-E) runs `command[0] command[1..]`; otherwise the
        // interactive shell, exactly as before. `command: None` reproduces the
        // old `$SHELL`/`/bin/bash` path byte-for-byte.
        let (program, args): (String, Vec<String>) = match &cfg.command {
            Some(command) if !command.is_empty() => {
                (command[0].clone(), command[1..].to_vec())
            }
            _ => {
                let shell = cfg
                    .shell
                    .clone()
                    .or_else(|| std::env::var("SHELL").ok())
                    .unwrap_or_else(|| "/bin/bash".to_string());
                (shell, cfg.args.clone())
            }
        };

        let mut cmd = CommandBuilder::new(&program);
        for arg in &args {
            cmd.arg(arg);
        }
        if let Some(cwd) = &cfg.cwd {
            cmd.cwd(cwd);
        }
        // portable-pty clears the child env (env_clear in as_command), so we
        // inherit the parent environment explicitly — otherwise the shell has
        // no PATH/HOME and is unusable.
        for (key, value) in std::env::vars_os() {
            cmd.env(key, value);
        }
        // Terminal identity + a UTF-8 locale if the user has none set.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("FLY", "1");
        if std::env::var_os("LANG").is_none()
            && std::env::var_os("LC_ALL").is_none()
            && std::env::var_os("LC_CTYPE").is_none()
        {
            cmd.env("LANG", "C.UTF-8");
        }
        // Per-pane extras (FLY_PANE_TOKEN, FLY_SOCKET_PATH, …) win over the rest.
        for (key, value) in &cfg.env {
            cmd.env(key, value);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("failed to spawn {program:?}: {e}"))?;
        let pid = child.process_id();

        // Drop the slave so the master observes EOF once the child exits
        // (KTD2). Without this the read never ends and the child can't be
        // detected as gone.
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("clone reader failed: {e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("take writer failed: {e}"))?;

        let (reaped_tx, reaped_rx) = mpsc::channel();
        let shared = Arc::new(PaneShared {
            lifecycle: Mutex::new(LifecycleState::Live),
            stopping: AtomicBool::new(false),
            reaped: AtomicBool::new(false),
            reaped_tx,
            paused: Mutex::new(false),
            pause_cv: Condvar::new(),
            activity: ActivityCell::new(),
        });

        let thread_shared = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name(format!("fly-pty-{}", id.0))
            .spawn(move || read_loop(id, reader, child, sink, thread_shared, on_exit))
            .map_err(|e| format!("spawn read thread failed: {e}"))?;

        Ok(Pane {
            id,
            master: pair.master,
            writer: Arc::new(Mutex::new(writer)),
            pid,
            shared,
            reaped_rx,
            reader_handle: Some(handle),
            token,
            leaf_key,
        })
    }

    /// Write input bytes to the PTY. The caller clones this handle out of the
    /// registry lock first, so the registry isn't held during the write.
    pub fn writer(&self) -> Arc<Mutex<Box<dyn Write + Send>>> {
        Arc::clone(&self.writer)
    }

    /// Resize the PTY; the kernel delivers SIGWINCH so TUIs reflow (R2).
    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), String> {
        self.master
            .resize(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("resize failed: {e}"))
    }

    /// Pause reads (backpressure). The in-flight read drains, then the thread
    /// parks until [`Pane::resume`] (KTD4).
    pub fn pause(&self) {
        *self.shared.paused.lock().unwrap() = true;
    }

    /// Resume reads after a pause.
    pub fn resume(&self) {
        *self.shared.paused.lock().unwrap() = false;
        self.shared.pause_cv.notify_all();
    }

    /// Current lifecycle state.
    pub fn lifecycle(&self) -> LifecycleState {
        self.shared.lifecycle.lock().unwrap().clone()
    }

    /// The pane's auth token (U8).
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The pane's stable frontend leaf key (U3), if it carried one.
    pub fn leaf_key(&self) -> Option<&str> {
        self.leaf_key.as_deref()
    }

    /// The foreground process group leader pid, used by U10 for `/proc`-based
    /// cwd tracking. Falls back to the child pid.
    pub fn foreground_pid(&self) -> Option<u32> {
        self.master
            .process_group_leader()
            .map(|p| p as u32)
            .or(self.pid)
    }

    /// The pane's current output-activity snapshot (U3): the work stretch and
    /// how long since its last above-threshold output. Reads atomics only, so
    /// it is safe to call under the registry lock.
    pub fn activity(&self) -> super::PaneActivitySnapshot {
        let (working_for_ms, last_output_ago_ms) = self.shared.activity.snapshot();
        super::PaneActivitySnapshot {
            working_for_ms,
            last_output_ago_ms,
        }
    }

    /// Tear down the pane: stop the child and reap it before returning, so no
    /// orphan or zombie survives (KTD13). Idempotent.
    pub fn teardown(&mut self) {
        if self.reader_handle.is_none() {
            return; // already torn down
        }
        log::debug!("tearing down pane {}", self.id.0);
        self.shared.stopping.store(true, Ordering::Release);
        // Wake the read thread if it's parked on the pause condvar.
        self.shared.pause_cv.notify_all();

        if let Some(pid) = self.pid {
            // Graceful hangup unless the child is already gone. The shell is a
            // session leader (portable-pty setsid), so its death makes the
            // kernel SIGHUP the foreground job too.
            if !self.shared.reaped.load(Ordering::Acquire) {
                signal_pid(pid, libc::SIGHUP);
            }
            // Wait briefly for the read thread to reap, then hard-kill. We only
            // signal while `reaped` is false, so the pid can't have been reused.
            if self.reaped_rx.recv_timeout(GRACE).is_err()
                && !self.shared.reaped.load(Ordering::Acquire)
            {
                signal_pid(pid, libc::SIGKILL);
            }
        }

        // Joining guarantees the child has been reaped before we drop the pane.
        if let Some(handle) = self.reader_handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        // Safety net: a pane dropped without an explicit close still tears down
        // its child rather than leaking it.
        self.teardown();
    }
}

/// The blocking read loop. Owns the child and reaps it when the read side ends.
fn read_loop(
    id: PaneId,
    mut reader: Box<dyn Read + Send>,
    mut child: Box<dyn Child + Send + Sync>,
    mut sink: OutputSink,
    shared: Arc<PaneShared>,
    on_exit: ExitCallback,
) {
    let mut buf = vec![0u8; READ_BUF];
    loop {
        // Park while paused for backpressure (KTD4), but wake to tear down.
        {
            let mut paused = shared.paused.lock().unwrap();
            while *paused && !shared.stopping.load(Ordering::Acquire) {
                paused = shared.pause_cv.wait(paused).unwrap();
            }
        }
        if shared.stopping.load(Ordering::Acquire) {
            break;
        }
        match reader.read(&mut buf) {
            Ok(0) => break, // EOF
            Ok(n) => {
                sink(&buf[..n]);
                // Record output activity for the work-stretch signal (U3). Off
                // the byte path's critical work: a single Relaxed atomic update
                // for above-threshold chunks, after the bytes are already out.
                shared.activity.record(n);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            // On Linux the master read returns EIO (not EOF) once the child
            // exits; treat any other error as end-of-stream.
            Err(_) => break,
        }
    }

    // The read side ended ⟹ the child closed the PTY ⟹ it has exited. Reap it.
    let status = child.wait();
    let final_state = {
        let mut lc = shared.lifecycle.lock().unwrap();
        if !lc.is_terminal() {
            *lc = if shared.stopping.load(Ordering::Acquire) {
                LifecycleState::Killed
            } else {
                match &status {
                    Ok(st) => LifecycleState::Exited {
                        code: st.exit_code() as i32,
                        signal: st.signal().map(|s| s.to_string()),
                    },
                    Err(e) => LifecycleState::Failed {
                        error: format!("wait failed: {e}"),
                    },
                }
            };
        }
        lc.clone()
    };
    shared.reaped.store(true, Ordering::Release);
    let _ = shared.reaped_tx.send(());
    // Notify outside the lock; surfaces the exit to the frontend (U3).
    on_exit(id, final_state);
}

/// Send a signal to a process. ESRCH (already gone) is ignored.
fn signal_pid(pid: u32, sig: i32) {
    // SAFETY: kill(2) is always safe to call; an invalid pid yields ESRCH,
    // which we ignore. We only reach here while the child is unreaped, so the
    // pid is still valid for our child.
    unsafe {
        libc::kill(pid as libc::pid_t, sig);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GAP: u64 = activity::IDLE_GAP_MS;

    #[test]
    fn records_and_queries_a_stretch() {
        let cell = ActivityCell::new();
        cell.record_at(500, 50);
        let (working_for, ago) = cell.snapshot_at(560);
        assert_eq!(working_for, Some(60));
        assert_eq!(ago, Some(60));
    }

    #[test]
    fn idle_after_gap_returns_none() {
        let cell = ActivityCell::new();
        cell.record_at(500, 50);
        let (working_for, _) = cell.snapshot_at(500 + GAP + 1);
        assert_eq!(working_for, None, "no output within the gap → idle");
    }

    #[test]
    fn a_gap_resets_the_stretch() {
        let cell = ActivityCell::new();
        cell.record_at(500, 50);
        cell.record_at(500 + GAP + 1, 50); // silence > gap → fresh stretch
        let (working_for, _) = cell.snapshot_at(500 + GAP + 11);
        assert_eq!(working_for, Some(10), "second output anchors a new stretch");
    }

    #[test]
    fn first_chunk_at_zero_is_a_real_stretch() {
        let cell = ActivityCell::new();
        cell.record_at(0, 50);
        let (working_for, _) = cell.snapshot_at(0);
        assert_eq!(
            working_for,
            Some(0),
            "now == 0 records via has_stretch, not the None sentinel"
        );
    }

    #[test]
    fn sub_threshold_chunk_is_ignored() {
        let cell = ActivityCell::new();
        cell.record_at(500, 1); // a lone byte (echo / redraw) is below threshold
        assert_eq!(
            cell.snapshot_at(500),
            (None, None),
            "a 1-byte chunk neither starts nor extends a stretch"
        );
    }

    #[test]
    fn never_ran_is_idle() {
        let cell = ActivityCell::new();
        assert_eq!(cell.snapshot_at(1000), (None, None));
    }
}
