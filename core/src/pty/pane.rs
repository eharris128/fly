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

/// PTY read buffer. Sized to never be the limiting factor: a `read` returns
/// whatever is there, immediately when idle (low latency) and a full chunk
/// under load (natural coalescing) — KTD4.
///
/// Measured 2026-07-28 (Linux 6.8, this box): the *kernel* caps delivery well
/// below this — a single 1 MiB `write()` into the slave came back as reads of
/// median 2048 and max 8193 bytes, so 64 KiB never fills and an 8 KiB buffer
/// would behave identically. The size is harmless headroom, not the source of
/// the coalescing — the per-pane coalescer in `stream/coalesce.rs` batches
/// these reads before they hit the wire.
const READ_BUF: usize = 64 * 1024;
/// How long to wait after SIGHUP before escalating to SIGKILL on close.
const GRACE: Duration = Duration::from_millis(200);
/// Capacity of the per-pane raw output tail ring
/// (feed-question-screen-fallback U1/R7): enough to hold several full Ink
/// dialog repaints (a few KiB each) with margin, small enough to be a
/// negligible fixed per-pane cost. The ring is a memcpy tee on the read
/// thread; nothing parses it until a pending-question fallback actually
/// needs the screen.
const TAIL_RING_CAP: usize = 64 * 1024;

/// Claude Code session-identity markers stripped from every child fly spawns
/// — pane children below, and the headless monitor-check runner
/// (`crate::automations::headless`, R13 of the headless-monitor-checks plan),
/// which shares this one list rather than duplicating it. When fly itself was
/// launched from inside a Claude session (`pnpm flavor:dev` in a dev pane),
/// these leak through and make every `claude` run in a fly child consider
/// itself a *child session* of that long-gone parent — verified live (2.1.207)
/// to suppress its `~/.claude/sessions/<pid>.json` livestate file and its
/// transcript flush, which blinds the feed's pending-question fallback, resume
/// attribution, automation output capture, and handoff qualification. A fly
/// pane (or a backend-owned check) is a top-level context; a session started
/// in it is nobody's child. (fix-feed-question-detection-gaps)
pub(crate) const CLAUDE_SESSION_MARKERS: [&str; 4] = [
    "CLAUDECODE",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_ENTRYPOINT",
];

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

/// The bounded raw-output tail of a pane (feed-question-screen-fallback U1):
/// the last [`TAIL_RING_CAP`] bytes the PTY produced, plus a monotonic `seq`
/// (total bytes ever written) that identifies content versions so the screen
/// parser can cache per snapshot — a pane waiting on a dialog produces no new
/// output, so its `seq` is stable and the parse runs once.
struct TailRing {
    buf: Vec<u8>,
    /// Next write position (wraps at capacity).
    pos: usize,
    /// Bytes currently held (== capacity once the ring has wrapped).
    len: usize,
    /// Total bytes ever written — the snapshot version.
    seq: u64,
    /// Wall-clock stamp (epoch ms, caller-injected) of the most recent write.
    /// A dialog parked on screen produces no output, so this freezes at the
    /// dialog's draw time — the screen fallback's ask-time anchor when neither
    /// a hook raise nor Claude's sessions file provides one. 0 = never written.
    last_write_ms: u64,
}

impl TailRing {
    fn new(cap: usize) -> Self {
        Self {
            buf: vec![0; cap],
            pos: 0,
            len: 0,
            seq: 0,
            last_write_ms: 0,
        }
    }

    /// Append a chunk, keeping only the trailing `capacity` bytes. `now_ms` is
    /// the wall clock (time-injected, like the state machines, so tests need no
    /// real clock).
    fn write(&mut self, bytes: &[u8], now_ms: u64) {
        let cap = self.buf.len();
        self.seq += bytes.len() as u64;
        self.last_write_ms = now_ms;
        // A chunk at/over capacity replaces the whole ring with its own tail.
        let src = if bytes.len() >= cap {
            self.pos = 0;
            self.len = cap;
            self.buf.copy_from_slice(&bytes[bytes.len() - cap..]);
            return;
        } else {
            bytes
        };
        let first = (cap - self.pos).min(src.len());
        self.buf[self.pos..self.pos + first].copy_from_slice(&src[..first]);
        if first < src.len() {
            self.buf[..src.len() - first].copy_from_slice(&src[first..]);
        }
        self.pos = (self.pos + src.len()) % cap;
        self.len = (self.len + src.len()).min(cap);
    }

    /// The held bytes, oldest → newest, plus the current `seq`.
    fn snapshot(&self) -> (Vec<u8>, u64) {
        let cap = self.buf.len();
        let mut out = Vec::with_capacity(self.len);
        if self.len < cap {
            out.extend_from_slice(&self.buf[..self.len]);
        } else {
            out.extend_from_slice(&self.buf[self.pos..]);
            out.extend_from_slice(&self.buf[..self.pos]);
        }
        (out, self.seq)
    }
}

/// One pane's screen-state snapshot for the pending-question fallback
/// (feed-question-screen-fallback U1): the raw output tail, its content
/// version, and the pane's last-known grid size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenTail {
    pub bytes: Vec<u8>,
    pub seq: u64,
    pub rows: u16,
    pub cols: u16,
    /// Epoch ms of the ring's most recent write (0 = never written). Stable
    /// while a dialog is parked (a waiting pane produces no output), so it
    /// doubles as the dialog's draw-time stamp for the screen fallback's
    /// `askedAt` when no better ask-time source exists.
    pub last_write_at_ms: u64,
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
    /// Tmux arm only (U4): an externally-observed pane death, set by the
    /// `panes_status` backstop (or the KTD12 hook path). The FIFO gives no
    /// EOF for a dead-but-remaining pane — live-pinned: `pipe-pane` refuses
    /// a dead pane and its `cat` survives — so the read thread polls this
    /// between reads and exits with the recorded status.
    forced_exit: Mutex<Option<i32>>,
    /// Raw output tail (feed-question-screen-fallback U1). Its own lock, taken
    /// only by the read thread (append) and the on-demand snapshot — never by
    /// the byte path's other consumers.
    tail: Mutex<TailRing>,
    /// Last-known PTY grid size, set at spawn and updated on resize, so a
    /// screen snapshot carries the width the bytes were rendered against.
    dims: Mutex<(u16, u16)>,
}

/// What actually backs a pane's byte streams and control ops.
///
/// `Pty` is the original portable-pty path; `Tmux` is the session-substrate
/// arm (tmux plan U3): the pane is a marked session on the flavor server,
/// output arrives through a `pipe-pane` FIFO, input leaves as binary-safe
/// `send-keys -H`, and the "child" is tmux's pane process. Every consumer
/// above (`writer()`, resize, pause, activity, tail ring, teardown ordering)
/// is backend-agnostic.
enum Backend {
    Pty {
        /// PTY master. `resize`/`take_writer` take `&self`; only ever touched
        /// under the registry lock, so `Send` is sufficient.
        master: Box<dyn MasterPty + Send>,
    },
    Tmux {
        substrate: Arc<crate::substrate::Substrate>,
        session: String,
        fifo: std::path::PathBuf,
    },
}

/// A live pane (PTY- or tmux-backed; see [`Backend`]).
pub struct Pane {
    pub id: PaneId,
    backend: Backend,
    /// Writer behind its own lock so writes don't hold the registry lock.
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// Pid used to signal on close: the direct child (PTY arm) or tmux's
    /// `#{pane_pid}` (tmux arm). Valid only while live.
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
    /// U10: killed at quit, never detached (automation tabs, alerts sink).
    ephemeral: bool,
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
        let cfg_ephemeral = cfg.ephemeral;
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
        // Inherit the parent environment explicitly (CommandBuilder::new also
        // seeds it from the process env, so this is belt-and-braces — which is
        // why removals below must use env_remove, not a skip in this loop).
        for (key, value) in std::env::vars_os() {
            cmd.env(key, value);
        }
        // Claude Code session-identity markers are NOT inherited — see
        // [`CLAUDE_SESSION_MARKERS`] for the full (live-verified) rationale.
        for marker in CLAUDE_SESSION_MARKERS {
            cmd.env_remove(marker);
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
            forced_exit: Mutex::new(None),
            tail: Mutex::new(TailRing::new(TAIL_RING_CAP)),
            dims: Mutex::new((size.rows, size.cols)),
        });

        let thread_shared = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name(format!("fly-pty-{}", id.0))
            .spawn(move || read_loop(id, reader, child, sink, thread_shared, on_exit))
            .map_err(|e| format!("spawn read thread failed: {e}"))?;

        Ok(Pane {
            id,
            backend: Backend::Pty { master: pair.master },
            writer: Arc::new(Mutex::new(writer)),
            pid,
            shared,
            reaped_rx,
            reader_handle: Some(handle),
            token,
            leaf_key,
            ephemeral: cfg_ephemeral,
        })
    }

    /// Spawn a pane as a marked tmux session on the flavor server (tmux plan
    /// U3, KTD2/KTD3/KTD4). The pane command is `cfg.command` or the user's
    /// shell; fly's per-pane env extras ride `-e`; output arrives via a
    /// `pipe-pane`-fed FIFO into the same sink/activity/ring machinery as the
    /// PTY arm; input leaves through [`TmuxWriter`]. The session outlives
    /// this process by design — `teardown` (an explicit close) kills it, but
    /// a detach-style shutdown (U8) will simply drop the pane without
    /// teardown-with-kill.
    pub fn spawn_tmux(
        id: PaneId,
        cfg: SpawnConfig,
        token: String,
        mut sink: OutputSink,
        on_exit: ExitCallback,
        substrate: Arc<crate::substrate::Substrate>,
    ) -> Result<Pane, String> {
        let leaf_key = cfg
            .leaf_key
            .clone()
            .ok_or_else(|| "tmux-backed panes require a leaf key".to_string())?;
        let cfg_ephemeral = cfg.ephemeral;
        substrate.ensure_server().map_err(|e| e.to_string())?;
        let session = substrate.session_name(&leaf_key);

        // U8 reattach: a surviving marked session for this leaf is ADOPTED,
        // never respawned — the whole point of the substrate. The caller
        // (stream::spawn_pane) has already re-registered the stored token.
        let adopt = substrate.adoptable_session(&leaf_key);

        // Pane command: argv, or the user's shell — same resolution as the
        // PTY arm.
        let command: Vec<String> = match &cfg.command {
            Some(command) if !command.is_empty() => command.clone(),
            _ => {
                let shell = cfg
                    .shell
                    .clone()
                    .or_else(|| std::env::var("SHELL").ok())
                    .unwrap_or_else(|| "/bin/bash".to_string());
                let mut v = vec![shell];
                v.extend(cfg.args.clone());
                v
            }
        };

        // Per-pane env extras via `-e` (KTD8). TERM/locale identity come from
        // the tmux server (whose env was scrubbed at start — the marker strip
        // lives THERE for this arm); `FLY` matches the PTY arm.
        let mut env: std::collections::BTreeMap<String, String> = cfg
            .env
            .iter()
            .cloned()
            .collect();
        env.insert("FLY".into(), "1".into());

        let rows = cfg.rows.max(1);
        let cols = cfg.cols.max(1);
        if adopt.is_none() {
            substrate
                .tmux()
                .new_session(
                    &session,
                    cfg.cwd.as_deref().unwrap_or(""),
                    &env,
                    &command,
                    cols,
                    rows,
                )
                .map_err(|e| e.to_string())?;
            // KTD4: a dead pane keeps its final screen; death is observed via
            // `#{pane_dead}` (read-loop EOF check + the panes_status
            // backstop), not via session teardown.
            let _ = substrate.tmux().set_remain_on_exit(&session, true);
        } else {
            // Adoption housekeeping: end the ORPHANED pipe (its `cat` — a
            // tmux-server child — survived the previous fly), re-drive the
            // detached geometry to this pane's grid, and let the hooks be
            // re-armed below with the CURRENT fly binary path.
            let _ = substrate.tmux().pipe_pane_close(&session);
            let _ = substrate.tmux().set_window_size_manual(&session);
            let _ = substrate.tmux().resize_window(&session, cols, rows);
        }

        // tmux's pane process pid — the signal target and cwd anchor.
        let pid = substrate
            .tmux()
            .capture_pane(&session, false, 0)
            .ok() // not the pid source; just ensures the pane is up
            .and_then(|_| pane_root_pid(&substrate, &session));

        // FIFO for the output stream. Created 0600 before pipe-pane can open
        // its write end; unlinked at teardown.
        let fifo = substrate.fifo_path(id.0);
        let _ = std::fs::remove_file(&fifo);
        mkfifo_0600(&fifo)?;

        let (reaped_tx, reaped_rx) = mpsc::channel();
        let shared = Arc::new(PaneShared {
            lifecycle: Mutex::new(LifecycleState::Live),
            stopping: AtomicBool::new(false),
            reaped: AtomicBool::new(false),
            reaped_tx,
            paused: Mutex::new(false),
            pause_cv: Condvar::new(),
            activity: ActivityCell::new(),
            forced_exit: Mutex::new(None),
            tail: Mutex::new(TailRing::new(TAIL_RING_CAP)),
            dims: Mutex::new((rows, cols)),
        });

        // Adoption keeps the STORED token (the agent's long-lived env holds
        // it — U8/KTD8); a fresh session uses the newly-minted one.
        let token = match &adopt {
            Some(record) => record.token.clone(),
            None => token,
        };
        // Persist the leaf ⇄ session ⇄ token binding BEFORE the read thread
        // starts (U2/KTD8): a crash after this point leaves a reattachable
        // record; a crash before it leaves an unmarked-but-marked-name
        // session the startup discovery will surface as unknown-dead.
        let _ = crate::substrate::store::upsert_at(
            substrate.store_path(),
            &leaf_key,
            crate::substrate::store::SessionRecord {
                session_name: session.clone(),
                token: token.clone(),
                created_at_ms: crate::notify::now_unix_ms(),
            },
        );

        // U8 adopt: replay the surviving session's recent screen+history into
        // the fresh xterm before live bytes flow (bounded — reveal speed over
        // completeness; the full history stays in tmux for `leader t`). The
        // capture-then-arm gap can lose a few ms of output; the alternative
        // (arm-then-capture) duplicates instead. Loss is invisible on an
        // idle reattach, duplication never is — capture first.
        if adopt.is_some() {
            if let Ok(history) = substrate.tmux().capture_pane(&session, true, 2000) {
                let trimmed = history.trim_start_matches('\n');
                if !trimmed.is_empty() {
                    sink(trimmed.replace('\n', "\r\n").as_bytes());
                }
            }
        }

        let thread_shared = Arc::clone(&shared);
        let thread_substrate = Arc::clone(&substrate);
        let thread_session = session.clone();
        let thread_fifo = fifo.clone();
        let handle = std::thread::Builder::new()
            .name(format!("fly-tmux-{}", id.0))
            .spawn(move || {
                tmux_read_loop(
                    id,
                    thread_substrate,
                    thread_session,
                    thread_fifo,
                    sink,
                    thread_shared,
                    on_exit,
                )
            })
            .map_err(|e| format!("spawn read thread failed: {e}"))?;

        // Arm the pipe AFTER the reader thread exists: the reader's O_RDWR
        // open of the FIFO pairs with the consumer's write-end open, so order
        // alone can't deadlock; arming late just delays first bytes. The
        // consumer is `fly substrate-pipe` (spike 2026-08-28-001 KTD1) — the
        // same binary the hooks below invoke, never the host's `cat`.
        let fly_bin = substrate.fly_bin().to_string_lossy().into_owned();
        substrate
            .tmux()
            .pipe_pane_open(&session, &fifo.to_string_lossy(), &fly_bin)
            .map_err(|e| e.to_string())?;

        // KTD12 event hooks: pane-died reports arrive over the socket for
        // ~instant exit surfacing. Best-effort — the panes_status backstop is
        // the floor when arming fails or an event is lost.
        let _ = substrate.tmux().arm_pane_died_hook(&session, &fly_bin);
        // U7: attach-state reports feed the R9 focused-elsewhere suppression.
        let _ = substrate.tmux().arm_attach_hooks(&session, &fly_bin);

        let writer: Box<dyn Write + Send> = Box::new(TmuxWriter {
            substrate: Arc::clone(&substrate),
            session: session.clone(),
        });

        Ok(Pane {
            id,
            backend: Backend::Tmux {
                substrate,
                session,
                fifo,
            },
            writer: Arc::new(Mutex::new(writer)),
            pid,
            shared,
            reaped_rx,
            reader_handle: Some(handle),
            token,
            leaf_key: Some(leaf_key),
            ephemeral: cfg_ephemeral,
        })
    }

    /// Write input bytes to the PTY. The caller clones this handle out of the
    /// registry lock first, so the registry isn't held during the write.
    pub fn writer(&self) -> Arc<Mutex<Box<dyn Write + Send>>> {
        Arc::clone(&self.writer)
    }

    /// Resize the pane; the kernel delivers SIGWINCH so TUIs reflow (R2).
    /// Tmux arm: `resize-window` drives the detached grid (KTD2 — manual
    /// window-size mode, set at create).
    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), String> {
        match &self.backend {
            Backend::Pty { master } => master
                .resize(PtySize {
                    rows: rows.max(1),
                    cols: cols.max(1),
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|e| format!("resize failed: {e}"))?,
            Backend::Tmux { substrate, session, .. } => substrate
                .tmux()
                .resize_window(session, cols.max(1), rows.max(1))
                .map_err(|e| e.to_string())?,
        }
        *self.shared.dims.lock().unwrap() = (rows.max(1), cols.max(1));
        Ok(())
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

    /// U10: whether this pane is ephemeral (killed at quit, never detached).
    pub fn is_ephemeral(&self) -> bool {
        self.ephemeral
    }

    /// The tmux session backing this pane, when tmux-backed (U4).
    pub fn session_name(&self) -> Option<&str> {
        match &self.backend {
            Backend::Tmux { session, .. } => Some(session),
            Backend::Pty { .. } => None,
        }
    }

    /// The tmux backend handle + session, for callers that must run tmux
    /// subprocesses OUTSIDE the registry lock (KTD13 — clone out, then call).
    pub fn tmux_backend(&self) -> Option<(Arc<crate::substrate::Substrate>, String)> {
        match &self.backend {
            Backend::Tmux { substrate, session, .. } => {
                Some((Arc::clone(substrate), session.clone()))
            }
            Backend::Pty { .. } => None,
        }
    }

    /// Total output bytes ever produced (the tail ring's `seq`) — a cheap
    /// freshness probe (U6 verified submit; audit T9 shape).
    pub fn output_seq(&self) -> u64 {
        self.shared.tail.lock().unwrap().seq
    }

    /// Record an externally-observed pane death (U4 backstop / KTD12 hook):
    /// the FIFO gives no EOF for a dead-but-remaining pane, so the read
    /// thread polls this between reads and surfaces the exit. Idempotent;
    /// no-op on the PTY arm.
    pub fn force_dead(&self, status: i32) {
        if matches!(self.backend, Backend::Pty { .. }) {
            return;
        }
        let mut fe = self.shared.forced_exit.lock().unwrap();
        if fe.is_none() {
            *fe = Some(status);
        }
        drop(fe);
        // Wake a paused reader so the exit isn't gated on resume.
        self.shared.pause_cv.notify_all();
    }

    /// The foreground process group leader pid, used by U10 for `/proc`-based
    /// cwd tracking. Falls back to the child pid. Tmux arm (U4): the pane
    /// root's controlling-tty foreground group from `/proc/<pane_pid>/stat`
    /// `tpgid` — same precision as the PTY arm's `process_group_leader`, no
    /// tmux round trip — falling back to the pane root pid.
    pub fn foreground_pid(&self) -> Option<u32> {
        match &self.backend {
            Backend::Pty { master } => master
                .process_group_leader()
                .map(|p| p as u32)
                .or(self.pid),
            Backend::Tmux { .. } => self
                .pid
                .and_then(crate::cwd::foreground_tpgid)
                .or(self.pid),
        }
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

    /// The pane's current screen tail (feed-question-screen-fallback U1): the
    /// raw output ring, its content version, and the grid size the bytes were
    /// rendered against. Cheap enough for the registry lock (one bounded copy).
    pub fn screen_tail(&self) -> ScreenTail {
        let (bytes, seq, last_write_at_ms) = {
            let ring = self.shared.tail.lock().unwrap();
            let (bytes, seq) = ring.snapshot();
            (bytes, seq, ring.last_write_ms)
        };
        let (rows, cols) = *self.shared.dims.lock().unwrap();
        ScreenTail {
            bytes,
            seq,
            rows,
            cols,
            last_write_at_ms,
        }
    }

    /// Quit-path teardown for a tmux-backed pane (U8/KTD7): DETACH — stop
    /// the reader, end the pipe, unlink the FIFO, and leave the session
    /// running and the store record in place for the next instance to adopt.
    /// No-op for PTY panes (they reap via [`Self::teardown`]).
    pub fn teardown_detach(&mut self) {
        let Backend::Tmux {
            substrate,
            session,
            fifo,
        } = &self.backend
        else {
            return;
        };
        if self.reader_handle.is_none() {
            return; // already torn down
        }
        log::debug!("detaching pane {} (session survives)", self.id.0);
        self.shared.stopping.store(true, Ordering::Release);
        self.shared.pause_cv.notify_all();
        let _ = substrate.tmux().pipe_pane_close(session);
        let _ = std::fs::remove_file(fifo);
        if let Some(handle) = self.reader_handle.take() {
            let _ = handle.join();
        }
        // Store record deliberately KEPT (adoption key); session KEPT.
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

        if let Backend::Tmux {
            substrate,
            session,
            fifo,
        } = &self.backend
        {
            // Explicit close (KTD7's kill arm): kill the session — idempotent
            // if it already died — which ends the consumer, EOFs the FIFO, and lets
            // the read thread wind down; then reclaim the FIFO node and the
            // store record (an explicitly closed pane must not be reattached).
            let _ = substrate.tmux().kill_session(session);
            let _ = std::fs::remove_file(fifo);
            if let Some(leaf) = self.leaf_key.as_deref() {
                let _ = crate::substrate::store::prune_at(substrate.store_path(), leaf);
            }
            if let Some(handle) = self.reader_handle.take() {
                let _ = handle.join();
            }
            return;
        }

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
                // Tee into the tail ring (feed-question-screen-fallback U1) —
                // a bounded memcpy after the bytes are already out, under a
                // lock only the on-demand screen snapshot contends on. The
                // wall stamp anchors a parked dialog's draw time.
                shared
                    .tail
                    .lock()
                    .unwrap()
                    .write(&buf[..n], crate::notify::now_unix_ms());
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

/// The tmux arm's read loop (tmux plan U3): drain the `pipe-pane` FIFO into
/// the sink with the same activity/ring bookkeeping as the PTY loop. There is
/// no owned child to reap — session death is observed as FIFO EOF (the
/// consumer, `fly substrate-pipe`, dies with the session) and confirmed against `has-session`;
/// a transient EOF with the session still alive (a future focus-tier re-arm,
/// or pipe churn) re-opens and re-arms rather than declaring an exit.
fn tmux_read_loop(
    id: PaneId,
    _substrate: Arc<crate::substrate::Substrate>,
    _session: String,
    fifo: std::path::PathBuf,
    mut sink: OutputSink,
    shared: Arc<PaneShared>,
    on_exit: ExitCallback,
) {
    let mut buf = vec![0u8; READ_BUF];
    let mut dead_status: Option<i32> = None;
    'outer: loop {
        // O_RDWR so the open never blocks on a writer AND the FIFO never
        // reaches all-writers-closed EOF spuriously between `cat` restarts;
        // end-of-pane is signalled by `forced_exit`/`stopping`, not EOF —
        // live-pinned: a dead-but-remaining pane keeps its consumer alive and
        // `pipe-pane` refuses dead panes, so EOF simply never comes.
        let Ok(reader) = std::fs::OpenOptions::new().read(true).write(true).open(&fifo)
        else {
            break;
        };
        set_nonblocking(&reader);
        loop {
            {
                let mut paused = shared.paused.lock().unwrap();
                while *paused && !shared.stopping.load(Ordering::Acquire) {
                    if shared.forced_exit.lock().unwrap().is_some() {
                        break;
                    }
                    paused = shared.pause_cv.wait(paused).unwrap();
                }
            }
            if shared.stopping.load(Ordering::Acquire) {
                break 'outer;
            }
            if let Some(code) = *shared.forced_exit.lock().unwrap() {
                dead_status = Some(code);
                break 'outer;
            }
            // Wait for readability, ≤500 ms, then drain what's there. The
            // timeout bounds how stale a forced-exit/stop check can be.
            if !poll_readable(&reader, Duration::from_millis(500)) {
                continue;
            }
            match (&reader).read(&mut buf) {
                // O_RDWR: we hold a write end ourselves, so 0 can only mean
                // a genuinely torn-down FIFO — treat as end.
                Ok(0) => break 'outer,
                Ok(n) => {
                    sink(&buf[..n]);
                    shared.activity.record(n);
                    shared
                        .tail
                        .lock()
                        .unwrap()
                        .write(&buf[..n], crate::notify::now_unix_ms());
                }
                Err(ref e)
                    if e.kind() == std::io::ErrorKind::Interrupted
                        || e.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    continue
                }
                Err(_) => break 'outer,
            }
        }
    }

    let final_state = {
        let mut lc = shared.lifecycle.lock().unwrap();
        if !lc.is_terminal() {
            *lc = if shared.stopping.load(Ordering::Acquire) {
                LifecycleState::Killed
            } else {
                // `#{pane_dead_status}` when the dead pane was observable
                // (U4); 0 when the whole session vanished under us.
                LifecycleState::Exited {
                    code: dead_status.unwrap_or(0),
                    signal: None,
                }
            };
        }
        lc.clone()
    };
    shared.reaped.store(true, Ordering::Release);
    let _ = shared.reaped_tx.send(());
    on_exit(id, final_state);
}

/// Input transport for tmux-backed panes (U3/KTD5): each write ships as
/// binary-safe `send-keys -H` hex bytes, preserving arbitrary control
/// sequences (arrows, ESC, bracketed-paste frames) byte-exactly. One
/// subprocess per write-chain flush — flagged in the plan for a latency
/// measurement in U5, with the persistent hidden-attach client as fallback.
struct TmuxWriter {
    substrate: Arc<crate::substrate::Substrate>,
    session: String,
}

/// Max bytes per send-keys invocation (arg-count hygiene; multiple calls
/// preserve order — same thread, same server connection sequencing).
const SEND_HEX_CHUNK: usize = 512;

impl Write for TmuxWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        for chunk in buf.chunks(SEND_HEX_CHUNK) {
            self.substrate
                .tmux()
                .send_hex(&self.session, chunk)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `fcntl(F_SETFL, O_NONBLOCK)` on an open file (the FIFO read handle).
fn set_nonblocking(f: &std::fs::File) {
    use std::os::fd::AsRawFd;
    // SAFETY: plain fcntl on a fd we own.
    unsafe {
        let fd = f.as_raw_fd();
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
}

/// `poll(2)` for readability with a timeout; false on timeout/error.
fn poll_readable(f: &std::fs::File, timeout: Duration) -> bool {
    use std::os::fd::AsRawFd;
    let mut fds = libc::pollfd {
        fd: f.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: valid pollfd array of length 1.
    let rc = unsafe { libc::poll(&mut fds, 1, timeout.as_millis() as i32) };
    rc > 0 && (fds.revents & libc::POLLIN) != 0
}

/// `#{pane_pid}` of a session's (only) pane — the signal target and the cwd
/// anchor for the tmux arm.
fn pane_root_pid(substrate: &Arc<crate::substrate::Substrate>, session: &str) -> Option<u32> {
    substrate.tmux().display_message(session, "#{pane_pid}").ok()?.trim().parse().ok()
}

/// `mkfifo(path, 0600)` via libc (no new crates).
fn mkfifo_0600(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| "fifo path contains NUL".to_string())?;
    // SAFETY: plain libc call with a valid NUL-terminated path.
    let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
    if rc != 0 {
        return Err(format!(
            "mkfifo {} failed: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
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

    // ---- TailRing (feed-question-screen-fallback U1) ------------------------

    #[test]
    fn tail_ring_holds_everything_before_wrap() {
        let mut r = TailRing::new(8);
        r.write(b"abc", 100);
        r.write(b"de", 200);
        let (bytes, seq) = r.snapshot();
        assert_eq!(bytes, b"abcde");
        assert_eq!(seq, 5);
        assert_eq!(r.last_write_ms, 200, "stamp tracks the newest write");
    }

    #[test]
    fn tail_ring_wraps_keeping_the_newest_bytes_in_order() {
        let mut r = TailRing::new(8);
        r.write(b"abcdef", 100);
        r.write(b"ghij", 200); // 10 bytes total → keeps "cdefghij"
        let (bytes, seq) = r.snapshot();
        assert_eq!(bytes, b"cdefghij");
        assert_eq!(seq, 10, "seq counts every byte ever written");
    }

    #[test]
    fn tail_ring_oversized_chunk_keeps_its_own_tail() {
        let mut r = TailRing::new(4);
        r.write(b"xy", 100);
        r.write(b"abcdefgh", 200); // ≥ capacity → the chunk's own last 4 bytes
        let (bytes, seq) = r.snapshot();
        assert_eq!(bytes, b"efgh");
        assert_eq!(seq, 10);
        // And a later small write continues in order.
        r.write(b"Z", 300);
        assert_eq!(r.snapshot().0, b"fghZ");
        assert_eq!(r.last_write_ms, 300);
    }

    #[test]
    fn tail_ring_empty_snapshot_is_empty() {
        let r = TailRing::new(4);
        let (bytes, seq) = r.snapshot();
        assert!(bytes.is_empty());
        assert_eq!(seq, 0);
        assert_eq!(r.last_write_ms, 0, "never written → 0 stamp");
    }
}
