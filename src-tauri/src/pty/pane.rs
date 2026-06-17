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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

use super::{ExitCallback, OutputSink, PaneId, SpawnConfig};
use crate::state::lifecycle::LifecycleState;

/// PTY read buffer. A large buffer gives natural coalescing under load (a
/// single `read` returns a big chunk during a flood) while staying low-latency
/// when idle (it returns immediately with whatever's there) — KTD4.
const READ_BUF: usize = 64 * 1024;
/// How long to wait after SIGHUP before escalating to SIGKILL on close.
const GRACE: Duration = Duration::from_millis(200);

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

        let shell = cfg
            .shell
            .clone()
            .or_else(|| std::env::var("SHELL").ok())
            .unwrap_or_else(|| "/bin/bash".to_string());

        let mut cmd = CommandBuilder::new(&shell);
        for arg in &cfg.args {
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
            .map_err(|e| format!("failed to spawn shell {shell:?}: {e}"))?;
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

    /// The foreground process group leader pid, used by U10 for `/proc`-based
    /// cwd tracking. Falls back to the child pid.
    pub fn foreground_pid(&self) -> Option<u32> {
        self.master
            .process_group_leader()
            .map(|p| p as u32)
            .or(self.pid)
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
            Ok(n) => sink(&buf[..n]),
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
