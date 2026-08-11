//! PTY backend: a registry of panes keyed by an opaque, never-reused
//! [`PaneId`]. All pane operations route through [`PtyManager`] so the deferred
//! daemonized-mux / remote-pane work becomes a transport swap behind a stable
//! interface (KTD5).

mod pane;

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::state::lifecycle::LifecycleState;
use pane::Pane;
pub(crate) use pane::CLAUDE_SESSION_MARKERS;
pub use pane::ScreenTail;

/// Opaque pane handle. Ids are allocated monotonically and never reused, so a
/// stale id from a closed pane resolves to "gone" rather than aliasing a reused
/// slot — the generational guarantee from KTD13, achieved by non-reuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PaneId(pub u64);

/// A pane's raw output-activity snapshot (U3): the current work stretch and how
/// long since its last above-threshold output, both in ms. `working_for_ms` is
/// `None` when the pane is idle. Combined with agent detection into the
/// `PaneActivity` command payload in U4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneActivitySnapshot {
    pub working_for_ms: Option<u64>,
    pub last_output_ago_ms: Option<u64>,
}

/// Raw PTY output sink. The read thread calls this for each chunk of bytes.
/// U3 wires a `tauri::ipc::Channel`; tests wire an mpsc sender.
pub type OutputSink = Box<dyn FnMut(&[u8]) + Send>;

/// Called once by the read thread after the child is reaped, with the pane's
/// final lifecycle state. U3 wires this to a Tauri event so the frontend can
/// surface the exit; tests pass a no-op.
pub type ExitCallback = Box<dyn FnOnce(PaneId, LifecycleState) + Send>;

/// How to spawn a pane's shell.
#[derive(Debug, Clone)]
pub struct SpawnConfig {
    /// Program to run instead of the `$SHELL` default (U6): `command[0]` is the
    /// program, `command[1..]` its args. `None` (the default) runs an
    /// interactive shell exactly as before; `Some` is used only to auto-run a
    /// resumed Claude agent — the scoped KTD10 exception (KTD-E). Takes
    /// precedence over `shell`/`args`.
    pub command: Option<Vec<String>>,
    /// Shell to run; defaults to `$SHELL`, then `/bin/bash`.
    pub shell: Option<String>,
    /// Extra arguments to the shell (empty = a plain interactive shell).
    pub args: Vec<String>,
    /// Initial working directory; defaults to the shell's choice (`$HOME`).
    pub cwd: Option<String>,
    /// Extra environment entries (e.g. `FLY_PANE_TOKEN`, `FLY_SOCKET_PATH`).
    pub env: Vec<(String, String)>,
    /// The frontend's stable leaf key for this pane (U3) — the restart-stable
    /// per-pane identity resume records are keyed by. `None` in tests/headless
    /// spawns that don't carry one.
    pub leaf_key: Option<String>,
    pub rows: u16,
    pub cols: u16,
}

impl Default for SpawnConfig {
    fn default() -> Self {
        Self {
            command: None,
            shell: None,
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            leaf_key: None,
            rows: 24,
            cols: 80,
        }
    }
}

/// In-process registry of live panes (KTD5). The `Mutex` guards registry
/// mutation only; reader/writer handles are cloned out so blocking I/O never
/// holds this lock (KTD13).
pub struct PtyManager {
    panes: Mutex<HashMap<PaneId, Pane>>,
    next_id: AtomicU64,
    /// Per-pane TTL cache for the project-dir session-id scan (poll-batching
    /// plan KTD4/R3): `active_session_for_cwd` readdir+stats a whole Claude
    /// project dir, so the batched poll re-resolves it at most once per
    /// [`SESSION_ID_TTL`] per pane. Keyed by pane id and busted on a cwd
    /// change (a `cd` must not serve the old dir's answer). Independent of
    /// `panes` — never held together with it.
    session_cache: Mutex<HashMap<PaneId, SessionCacheEntry>>,
    /// The tmux substrate handle when the KTD10 flag selects it (tmux plan
    /// U3); `None` ⇒ every spawn is PTY-backed. Set once at app setup.
    substrate: Mutex<Option<std::sync::Arc<crate::substrate::Substrate>>>,
}

/// One pane's cached session-id resolution (poll-batching KTD4). `resolved`
/// deliberately caches `None` too: an abstention costs the same dir scan as a
/// hit, and the TTL bounds how long a new session stays unseen.
struct SessionCacheEntry {
    cwd: PathBuf,
    resolved: Option<String>,
    at: Instant,
}

/// How long a cached session-id resolution is served before the project dir is
/// re-scanned (poll-batching KTD4). The poll is the version-skew *fallback*
/// capture — the SessionStart hook is the precise path and bypasses this cache
/// entirely — so up to ~5 s of staleness on rotation is within design.
const SESSION_ID_TTL: Duration = Duration::from_secs(5);

impl Default for PtyManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PtyManager {
    pub fn new() -> Self {
        Self {
            panes: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            session_cache: Mutex::new(HashMap::new()),
            substrate: Mutex::new(None),
        }
    }

    /// Spawn a new pane and register it. `token` is the per-pane auth secret
    /// (U8), already registered with the hook server and injected into `cfg.env`
    /// by the caller before this call so no callback can race registration.
    /// Reserve the next pane id without spawning. Lets the caller issue the
    /// pane's auth token and inject it into the env before the child starts,
    /// so no callback can race registration (KTD7).
    pub fn reserve_id(&self) -> PaneId {
        PaneId(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Spawn a pane under a previously reserved id.
    /// Select the tmux substrate for every subsequent spawn (tmux plan
    /// U3/KTD10). Set once at app setup when the `substrate` config flag says
    /// tmux; absent (tests, the default config) every spawn is PTY-backed
    /// exactly as before.
    pub fn set_substrate(&self, substrate: std::sync::Arc<crate::substrate::Substrate>) {
        *self.substrate.lock().unwrap() = Some(substrate);
    }

    /// The pane's output-ring sequence (total bytes ever produced) — the
    /// cheap freshness probe the U6 verified-submit loop polls (also the
    /// perf-audit T9 accessor shape: seq without the 64 KiB ring copy).
    pub fn pane_output_seq(&self, id: PaneId) -> Option<u64> {
        self.panes.lock().unwrap().get(&id).map(|p| p.output_seq())
    }

    /// U6 delivery insurance: SIGWINCH-wake a detached tmux-backed pane's
    /// TUI before programmatic input. No-op for PTY panes and attached
    /// sessions. The tmux subprocesses run OUTSIDE the registry lock
    /// (KTD13) — the backend handle is cloned out first.
    pub fn wake_if_detached(&self, id: PaneId) {
        let target = {
            let panes = self.panes.lock().unwrap();
            panes.get(&id).and_then(|p| p.tmux_backend())
        };
        if let Some((sub, session)) = target {
            if let Ok(false) = sub.tmux().is_session_attached(&session) {
                let _ = sub.tmux().wake_pane(&session);
            }
        }
    }

    /// KTD12 pane-died event ingress: force-mark the pane backed by
    /// `session` dead with `status`. Touches only pane-shared state under
    /// the registry lock; the pane's poll-loop reader surfaces the exit.
    /// Unknown sessions are ignored (the event is a hint, panes close
    /// concurrently).
    pub fn force_dead_by_session(&self, session: &str, status: i32) {
        let panes = self.panes.lock().unwrap();
        for pane in panes.values() {
            if pane.session_name() == Some(session) {
                pane.force_dead(status);
            }
        }
    }

    pub fn spawn_with_id(
        &self,
        id: PaneId,
        cfg: SpawnConfig,
        token: String,
        sink: OutputSink,
        on_exit: ExitCallback,
    ) -> Result<PaneId, String> {
        let substrate = self.substrate.lock().unwrap().clone();
        let pane = match substrate {
            Some(sub) if cfg.leaf_key.is_some() => {
                Pane::spawn_tmux(id, cfg, token, sink, on_exit, sub)?
            }
            // Leafless spawns (tests/headless) stay PTY-backed even under the
            // flag: they have no stable identity to mark a session with.
            _ => Pane::spawn(id, cfg, token, sink, on_exit)?,
        };
        self.panes.lock().unwrap().insert(id, pane);
        Ok(id)
    }

    /// Reserve an id and spawn in one step (used where the token isn't needed
    /// up front, e.g. tests).
    pub fn spawn(
        &self,
        cfg: SpawnConfig,
        token: String,
        sink: OutputSink,
        on_exit: ExitCallback,
    ) -> Result<PaneId, String> {
        let id = self.reserve_id();
        self.spawn_with_id(id, cfg, token, sink, on_exit)
    }

    /// Write input bytes to a pane's PTY. Clones the writer out of the registry
    /// lock so the write itself doesn't hold it.
    pub fn write(&self, id: PaneId, data: &[u8]) -> Result<(), String> {
        let writer = {
            let panes = self.panes.lock().unwrap();
            let pane = panes.get(&id).ok_or_else(|| no_such(id))?;
            pane.writer()
        };
        let mut w = writer.lock().unwrap();
        w.write_all(data).map_err(|e| format!("write failed: {e}"))?;
        w.flush().map_err(|e| format!("flush failed: {e}"))
    }

    /// Resize a pane's PTY (R2).
    pub fn resize(&self, id: PaneId, rows: u16, cols: u16) -> Result<(), String> {
        let panes = self.panes.lock().unwrap();
        let pane = panes.get(&id).ok_or_else(|| no_such(id))?;
        pane.resize(rows, cols)
    }

    /// Pause reads on a pane (backpressure, KTD4).
    pub fn pause(&self, id: PaneId) -> Result<(), String> {
        let panes = self.panes.lock().unwrap();
        panes.get(&id).ok_or_else(|| no_such(id))?.pause();
        Ok(())
    }

    /// Resume reads on a pane.
    pub fn resume(&self, id: PaneId) -> Result<(), String> {
        let panes = self.panes.lock().unwrap();
        panes.get(&id).ok_or_else(|| no_such(id))?.resume();
        Ok(())
    }

    /// Close a pane: remove it from the registry first (so no new accessor
    /// resolves it), then tear down and reap its child (KTD13).
    pub fn close(&self, id: PaneId) -> Result<(), String> {
        let pane = self.panes.lock().unwrap().remove(&id);
        match pane {
            Some(mut p) => {
                p.teardown();
                Ok(())
            }
            None => Err(no_such(id)),
        }
    }

    /// Current lifecycle state of a pane, if it exists.
    pub fn lifecycle(&self, id: PaneId) -> Option<LifecycleState> {
        self.panes.lock().unwrap().get(&id).map(|p| p.lifecycle())
    }

    /// The pane's hook auth token (U8).
    pub fn token(&self, id: PaneId) -> Option<String> {
        self.panes
            .lock()
            .unwrap()
            .get(&id)
            .map(|p| p.token().to_string())
    }

    /// The pane's stable frontend leaf key (U3), used by the hook dispatch to key
    /// the pane's resume record. `None` if the pane is gone or carried no key.
    pub fn leaf_key(&self, id: PaneId) -> Option<String> {
        self.panes
            .lock()
            .unwrap()
            .get(&id)
            .and_then(|p| p.leaf_key().map(str::to_string))
    }

    /// The **live** pane currently owning `leaf_key` — the reverse of
    /// [`leaf_key`](Self::leaf_key), for the feed's input route
    /// (feed-agent-reply-io U5). Ids are never reused, so if a leaf ever had
    /// two registered panes (a respawn racing a close), the highest id is the
    /// current one; an exited-but-unreaped pane doesn't count (writing to it
    /// would land nowhere — same `is_live` gate as the automations R7 probe).
    pub fn pane_by_leaf(&self, leaf_key: &str) -> Option<PaneId> {
        self.panes
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, p)| p.leaf_key() == Some(leaf_key) && p.lifecycle().is_live())
            .map(|(id, _)| *id)
            .max_by_key(|id| id.0)
    }

    /// The screen tail of the live pane owning `leaf_key`
    /// (feed-question-screen-fallback U1): the raw output ring + grid size the
    /// pending-question fallback parses. Same live-pane resolution as
    /// [`pane_by_leaf`](Self::pane_by_leaf).
    pub fn screen_tail_by_leaf(&self, leaf_key: &str) -> Option<ScreenTail> {
        let panes = self.panes.lock().unwrap();
        panes
            .iter()
            .filter(|(_, p)| p.leaf_key() == Some(leaf_key) && p.lifecycle().is_live())
            .max_by_key(|(id, _)| id.0)
            .map(|(_, p)| p.screen_tail())
    }

    /// The pane's foreground pid, for `/proc`-based cwd tracking (U10).
    ///
    /// Runs the `tcgetpgrp` ioctl under the registry lock — a documented,
    /// deliberate exception to the KTD13 no-syscall-under-lock discipline
    /// (poll-batching plan KTD8): the ioctl is non-blocking and ~µs-cheap, and
    /// freeing it would mean re-plumbing the `Box<dyn MasterPty>` (raw-fd
    /// storage or an inner mutex) for no measurable latency win. KTD13's real
    /// target is *blocking* I/O; keep that out of here.
    pub fn foreground_pid(&self, id: PaneId) -> Option<u32> {
        self.panes
            .lock()
            .unwrap()
            .get(&id)
            .and_then(|p| p.foreground_pid())
    }

    /// The pane's live working directory (U10, R13).
    pub fn cwd(&self, id: PaneId) -> Option<std::path::PathBuf> {
        self.foreground_pid(id).and_then(crate::cwd::proc_cwd)
    }

    /// The pane's output-activity snapshot (U3): work stretch + last-output age.
    /// Reads only atomics, so it is safe under the registry lock. `None` if the
    /// pane is gone.
    pub fn pane_activity(&self, id: PaneId) -> Option<PaneActivitySnapshot> {
        self.panes.lock().unwrap().get(&id).map(|p| p.activity())
    }

    /// Whether the pane's foreground process is a Claude Code agent (KTD-D, U2):
    /// true exactly when [`PtyManager::pane_command`] resolves an agent argv.
    pub fn is_agent(&self, id: PaneId) -> bool {
        self.pane_command(id).is_some()
    }

    /// The pane's foreground pid **and** argv when its foreground process is a
    /// Claude agent, else `None` — the shared `is_claude`-gated prologue of
    /// [`pane_command`], [`pane_session_id`], and [`agent_task_count`]. Resolves
    /// the foreground pid (which takes and drops the registry lock), then reads
    /// `/proc` and runs the pure matcher with **no** lock held — the two-step shape
    /// of [`cwd`], so a blocking syscall never holds the `panes` mutex every command
    /// and the read-thread teardown contend on (KTD13). Returns the pid alongside
    /// the argv so a caller that needs the pid (cwd / session / task-count
    /// resolution) doesn't re-resolve it.
    fn claude_foreground(&self, id: PaneId) -> Option<(u32, Vec<String>)> {
        let pid = self.foreground_pid(id)?;
        let comm = crate::cwd::proc_comm(pid);
        let argv = crate::cwd::proc_cmdline(pid);
        crate::cwd::is_claude(comm.as_deref(), &argv).then_some((pid, argv))
    }

    /// The pane's foreground argv **only when it is a Claude agent** (U4, the flag
    /// source captured into the resume store, R2/R5). `None` for a bare shell or a
    /// gone pane — never a non-agent's argv. See [`claude_foreground`] for the
    /// shared pid-resolve + `is_claude`-gate + lock discipline.
    pub fn pane_command(&self, id: PaneId) -> Option<Vec<String>> {
        self.claude_foreground(id).map(|(_, argv)| argv)
    }

    /// The pane's active Claude `session_id`, resolved from Claude's transcript
    /// store (fix-resume-session-selection U1, KTD-A) — `is_claude`-gated, so a
    /// bare shell never resolves an id. Reads the agent pid directly (not [`cwd`],
    /// which would re-resolve it) so agent-ness and cwd come from one pid; the dir
    /// read of `~/.claude/projects` runs with no registry lock held (see
    /// [`claude_foreground`]). `None` for a non-agent, a gone pane, or when no
    /// active transcript is found — never a non-agent's id.
    pub fn pane_session_id(&self, id: PaneId) -> Option<String> {
        let (pid, _) = self.claude_foreground(id)?;
        let cwd = crate::cwd::proc_cwd(pid)?;
        crate::session::transcript::active_session_for_cwd(&cwd)
    }

    /// Whether the pane runs a Claude agent and, if so, how many live background
    /// task groups run beneath it (running-state plan U3, KTD2/KTD4) — the
    /// dashboard's "N tasks" number. Reuses the agent pid from [`claude_foreground`]
    /// to root the descendant scan; the whole `/proc` table read + walk runs with
    /// no registry lock held (KTD13/KTD4). `Some(n)` for an agent (`n` may be 0);
    /// `None` for a bare shell or a gone pane — never a panic. The table is one
    /// snapshot, so the count is internally consistent against pid reuse within the
    /// call (KTD4).
    pub fn agent_task_count(&self, id: PaneId) -> Option<u32> {
        let (pid, _) = self.claude_foreground(id)?;
        let table = crate::cwd::read_proc_table();
        Some(crate::cwd::count_background_task_groups(&table, pid))
    }

    /// Batched per-tick status for every pane the frontend polls (poll-batching
    /// plan U1, KTD1/KTD3): one call answers what previously took 3–4 invokes
    /// *per pane* (`pane_cwd` + `pane_command` + `pane_session_id` +
    /// `pane_activity`). The `/proc` table is snapshotted **at most once per
    /// call** and shared across every agent pane's task count (KTD3) — read
    /// lazily, so a tick with no agents never walks the table at all. Field
    /// semantics are exactly the per-pane commands': `cwd` resolves for any
    /// pane, `argv`/`session_id` only for a Claude agent (a bare shell's argv
    /// is never reported), a non-agent reports null timings and a zero task
    /// count, and a gone pane degrades to the empty status — never an error.
    pub fn panes_status(&self, ids: &[PaneId]) -> Vec<PaneStatus> {
        // U4 exit-detection backstop for tmux-backed panes: one
        // `list-panes -a` snapshot per tick; a marked session whose pane
        // process died gets its pane force-marked dead, which the pane's
        // poll-loop reader surfaces as an exit within ~500 ms. (EOF cannot
        // carry this: live-pinned, a dead-but-remaining pane keeps its
        // pipe `cat` alive and `pipe-pane` refuses dead panes.) The tmux
        // subprocess runs BEFORE the registry lock (KTD13); `force_dead`
        // itself touches only pane-shared state. Event-driven precision
        // arrives with the KTD12 pane-died hook; this is the lost-hook /
        // no-hook floor.
        let substrate = self.substrate.lock().unwrap().clone();
        if let Some(sub) = substrate {
            if let Ok(dead) = sub.tmux().list_dead_marked() {
                if !dead.is_empty() {
                    let panes = self.panes.lock().unwrap();
                    for pane in panes.values() {
                        if let Some(session) = pane.session_name() {
                            if let Some((_, status)) =
                                dead.iter().find(|(name, _)| name == session)
                            {
                                pane.force_dead(*status);
                            }
                        }
                    }
                }
            }
        }
        let now = Instant::now();
        let mut table: Option<Vec<crate::cwd::ProcEntry>> = None;
        let out = ids
            .iter()
            .map(|&id| self.pane_status_one(id, &mut table, now))
            .collect();
        self.prune_session_cache();
        out
    }

    /// One pane's slice of [`panes_status`](Self::panes_status). `table` is the
    /// call-shared lazy `/proc` snapshot (KTD3).
    fn pane_status_one(
        &self,
        id: PaneId,
        table: &mut Option<Vec<crate::cwd::ProcEntry>>,
        now: Instant,
    ) -> PaneStatus {
        let Some(pid) = self.foreground_pid(id) else {
            return PaneStatus::gone(id);
        };
        let cwd = crate::cwd::proc_cwd(pid);
        let comm = crate::cwd::proc_comm(pid);
        let argv = crate::cwd::proc_cmdline(pid);
        let is_agent = crate::cwd::is_claude(comm.as_deref(), &argv);
        if !is_agent {
            // Parity with `pane_activity`: a non-agent reports null timings and
            // a zero count (and never its argv), but its cwd still resolves —
            // the always-on persistence capture needs shell cwds too.
            return PaneStatus {
                cwd: cwd.map(|p| p.to_string_lossy().into_owned()),
                ..PaneStatus::gone(id)
            };
        }
        let table = table.get_or_insert_with(crate::cwd::read_proc_table);
        let live_task_count = crate::cwd::count_background_task_groups(table, pid);
        let session_id = cwd.as_deref().and_then(|c| {
            self.session_id_cached(id, c, now, crate::session::transcript::active_session_for_cwd)
        });
        let snap = self.pane_activity(id);
        PaneStatus {
            pane_id: id,
            cwd: cwd.map(|p| p.to_string_lossy().into_owned()),
            is_agent: true,
            argv: Some(argv),
            session_id,
            working_for_ms: snap.and_then(|s| s.working_for_ms),
            last_output_ago_ms: snap.and_then(|s| s.last_output_ago_ms),
            live_task_count,
        }
    }

    /// TTL-cached session-id resolution (poll-batching U2, KTD4). Serves the
    /// cached answer while it is younger than [`SESSION_ID_TTL`] **and** the
    /// pane's cwd is unchanged; otherwise runs `resolve` (the project-dir scan)
    /// and re-stamps the entry. `now` and `resolve` are injected so tests drive
    /// the clock and count the scans without touching the filesystem.
    fn session_id_cached(
        &self,
        id: PaneId,
        cwd: &Path,
        now: Instant,
        resolve: impl FnOnce(&Path) -> Option<String>,
    ) -> Option<String> {
        {
            let cache = self.session_cache.lock().unwrap();
            if let Some(e) = cache.get(&id) {
                if e.cwd == cwd && now.saturating_duration_since(e.at) < SESSION_ID_TTL {
                    return e.resolved.clone();
                }
            }
        }
        let resolved = resolve(cwd);
        self.session_cache.lock().unwrap().insert(
            id,
            SessionCacheEntry { cwd: cwd.to_path_buf(), resolved: resolved.clone(), at: now },
        );
        resolved
    }

    /// Drop session-cache entries for panes no longer registered, so the map
    /// tracks the live pane set instead of growing per closed pane. Takes the
    /// two locks strictly in sequence, never nested.
    fn prune_session_cache(&self) {
        let live: std::collections::HashSet<PaneId> =
            self.panes.lock().unwrap().keys().copied().collect();
        self.session_cache.lock().unwrap().retain(|id, _| live.contains(id));
    }

    /// Number of registered panes.
    pub fn count(&self) -> usize {
        self.panes.lock().unwrap().len()
    }

    /// Close every pane (used at app shutdown, U14).
    pub fn close_all(&self) {
        let mut drained: Vec<Pane> = {
            let mut panes = self.panes.lock().unwrap();
            panes.drain().map(|(_, p)| p).collect()
        };
        for p in &mut drained {
            p.teardown();
        }
    }
}

fn no_such(id: PaneId) -> String {
    format!("no such pane: {}", id.0)
}

// ---- Tauri command surface -------------------------------------------------
// Thin wrappers over `PtyManager`. `spawn_pane` (which needs an `ipc::Channel`
// for output) lands in U3; these control-plane commands are independent of it.

/// Write input to a pane. xterm.js `onData` yields a string whose UTF-8 bytes
/// (including control bytes like Ctrl-C `0x03`) are written verbatim. Typing
/// also clears the pane's attention (stage two of the two-stage clear).
///
/// Async (poll-batching plan KTD5/R4): as a sync command this ran on the GTK
/// main thread, so every keystroke queued behind whatever main-thread work was
/// in flight — measured at 200–300 ms poll-storm bursts before the batching
/// plan. The PTY write runs under `spawn_blocking` because a full kernel PTY
/// buffer can block the write. Two async invokes may complete out of order
/// across tokio workers, so **write ordering is the caller's job**: the
/// `ipc.ts::ptyWrite` wrapper serializes writes per pane (KTD5/R5) — do not
/// call this command concurrently for one pane from new code paths.
#[tauri::command]
pub async fn pty_write(
    app: tauri::AppHandle,
    manager: tauri::State<'_, Arc<PtyManager>>,
    attention: tauri::State<'_, Arc<crate::state::AttentionManager>>,
    pane_id: PaneId,
    data: String,
) -> Result<(), String> {
    let m = Arc::clone(manager.inner());
    tauri::async_runtime::spawn_blocking(move || m.write(pane_id, data.as_bytes()))
        .await
        .map_err(|e| format!("pty_write: {e}"))??;
    // Attention clear + emit are thread-safe (they run from hook dispatch
    // threads today), so staying off the main thread here is fine.
    if let Some(outcome) = attention.on_input(pane_id) {
        crate::stream::emit_attention(&app, pane_id, &outcome);
    }
    Ok(())
}

/// Resize a pane's PTY to the given grid.
#[tauri::command]
pub fn pty_resize(
    manager: tauri::State<'_, Arc<PtyManager>>,
    pane_id: PaneId,
    rows: u16,
    cols: u16,
) -> Result<(), String> {
    manager.resize(pane_id, rows, cols)
}

/// Close a pane and reap its child.
#[tauri::command]
pub fn close_pane(
    manager: tauri::State<'_, Arc<PtyManager>>,
    pane_id: PaneId,
) -> Result<(), String> {
    manager.close(pane_id)
}

/// Pause a pane's output (frontend flow control, KTD4): called when unacked
/// bytes exceed the high watermark.
#[tauri::command]
pub fn pty_pause(
    manager: tauri::State<'_, Arc<PtyManager>>,
    pane_id: PaneId,
) -> Result<(), String> {
    manager.pause(pane_id)
}

/// Resume a paused pane when unacked bytes drain below the low watermark.
#[tauri::command]
pub fn pty_resume(
    manager: tauri::State<'_, Arc<PtyManager>>,
    pane_id: PaneId,
) -> Result<(), String> {
    manager.resume(pane_id)
}

/// The pane's live working directory, for session restore (U10, R13).
#[tauri::command]
pub fn pane_cwd(
    manager: tauri::State<'_, Arc<PtyManager>>,
    pane_id: PaneId,
) -> Option<String> {
    manager.cwd(pane_id).map(|p| p.to_string_lossy().into_owned())
}

/// The pane's foreground argv when it is a Claude agent, else `None` (U4). The
/// always-on cwd poll reads this to capture each agent's launch flags
/// write-through into the resume store; a bare shell or gone pane yields `None`,
/// so a non-agent's argv is never persisted.
#[tauri::command]
pub fn pane_command(
    manager: tauri::State<'_, Arc<PtyManager>>,
    pane_id: PaneId,
) -> Option<Vec<String>> {
    manager.pane_command(pane_id)
}

/// The pane's active Claude `session_id` from the transcript store, else `None`
/// (fix-resume-session-selection U1). The always-on poll reads this to capture
/// each agent's precise session id write-through into the resume store —
/// hook-independent, so it works under installed-binary version skew (KTD-A). A
/// bare shell or gone pane yields `None`, so a non-agent never gets an id.
#[tauri::command]
pub fn pane_session_id(
    manager: tauri::State<'_, Arc<PtyManager>>,
    pane_id: PaneId,
) -> Option<String> {
    manager.pane_session_id(pane_id)
}

/// The agent dashboard payload for one pane (U4; running-state U3): whether it is
/// a Claude Code agent, its current work stretch and last-output age, and the
/// count of live background task groups beneath it. `working_for_ms` is `None`
/// when the pane is idle or not an agent; `live_task_count` is `0` for a non-agent
/// or gone pane. Polled per pane while the dashboard is open (KTD-C).
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PaneActivity {
    pub is_agent: bool,
    pub working_for_ms: Option<u64>,
    pub last_output_ago_ms: Option<u64>,
    pub live_task_count: u32,
}

/// One pane's full poll-tick status (poll-batching plan U1) — the batched
/// union of `pane_cwd` + `pane_command` + `pane_session_id` + `pane_activity`,
/// with each field keeping its per-pane command's exact semantics (agent-only
/// `argv`/`session_id`, null timings for a non-agent, empty status for a gone
/// pane). This shape crosses the IPC boundary to `ipc.ts::panesStatus`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PaneStatus {
    pub pane_id: PaneId,
    pub cwd: Option<String>,
    pub is_agent: bool,
    pub argv: Option<Vec<String>>,
    pub session_id: Option<String>,
    pub working_for_ms: Option<u64>,
    pub last_output_ago_ms: Option<u64>,
    pub live_task_count: u32,
}

impl PaneStatus {
    /// The degraded status for a gone (or non-agent, cwd aside) pane: all
    /// absent, never an error — a stale id must not fail the whole batch.
    fn gone(id: PaneId) -> Self {
        Self {
            pane_id: id,
            cwd: None,
            is_agent: false,
            argv: None,
            session_id: None,
            working_for_ms: None,
            last_output_ago_ms: None,
            live_task_count: 0,
        }
    }
}

/// Batched per-tick pane status (poll-batching plan U1, KTD1/KTD2): **one**
/// invoke per poll tick replaces the per-pane
/// `pane_cwd`/`pane_command`/`pane_session_id`/`pane_activity` fan-out (~3–4 ×
/// pane count). Async deliberately: sync commands run on the GTK main thread,
/// where this poll's `/proc` walks and project-dir scans were measured stalling
/// input 200–300 ms every tick (`docs/notes/2026-08-08-typing-latency-diagnosis.md`);
/// the blocking work runs under `spawn_blocking` so it also never ties up the
/// shared tokio pool (KTD2). The per-pane commands stay registered for one-off
/// probes (spawn, on-close persist), but no repeating poller calls them (R1).
#[tauri::command]
pub async fn panes_status(
    manager: tauri::State<'_, Arc<PtyManager>>,
    pane_ids: Vec<PaneId>,
) -> Result<Vec<PaneStatus>, String> {
    let manager = Arc::clone(manager.inner());
    tauri::async_runtime::spawn_blocking(move || manager.panes_status(&pane_ids))
        .await
        .map_err(|e| format!("panes_status: {e}"))
}

/// Per-pane agent state for the dashboard poll. Composes `/proc` agent detection
/// (U2) and the background-task-group count (running-state U3) — both from a
/// single `foreground_pid` resolution via [`agent_task_count`](PtyManager::agent_task_count) —
/// with the output-activity snapshot (U3 of the dashboard plan). A non-agent or
/// gone pane reports `is_agent: false`, null timings, and `live_task_count: 0` —
/// never a panic.
#[tauri::command]
pub fn pane_activity(
    manager: tauri::State<'_, Arc<PtyManager>>,
    pane_id: PaneId,
) -> PaneActivity {
    // One foreground-pid resolution gates agent-ness and roots the task count.
    let Some(live_task_count) = manager.agent_task_count(pane_id) else {
        return PaneActivity {
            is_agent: false,
            working_for_ms: None,
            last_output_ago_ms: None,
            live_task_count: 0,
        };
    };
    let snap = manager.pane_activity(pane_id);
    PaneActivity {
        is_agent: true,
        working_for_ms: snap.and_then(|s| s.working_for_ms),
        last_output_ago_ms: snap.and_then(|s| s.last_output_ago_ms),
        live_task_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accessors_on_unknown_pane_are_graceful() {
        // A stale id from a closed pane resolves to "gone", never a panic (U3/U4).
        let m = PtyManager::new();
        let ghost = PaneId(999);
        assert_eq!(m.pane_activity(ghost), None);
        assert!(!m.is_agent(ghost));
        assert_eq!(m.pane_command(ghost), None);
        assert_eq!(m.pane_session_id(ghost), None); // transcript path: graceful
        assert_eq!(m.agent_task_count(ghost), None); // count path: graceful, no panic
        assert_eq!(m.leaf_key(ghost), None);
        assert_eq!(m.lifecycle(ghost), None);
        assert_eq!(m.pane_by_leaf("leaf-404"), None); // reverse lookup: graceful
    }

    // ---- panes_status (poll-batching plan U1/U2) ---------------------------

    #[test]
    fn pane_status_wire_shape_is_camel_case() {
        // Pins the IPC contract `ipc.ts::PaneStatus` mirrors (U1): camelCase
        // keys, paneId as a bare number.
        let json = serde_json::to_value(PaneStatus::gone(PaneId(3))).unwrap();
        let obj = json.as_object().unwrap();
        for key in [
            "paneId",
            "cwd",
            "isAgent",
            "argv",
            "sessionId",
            "workingForMs",
            "lastOutputAgoMs",
            "liveTaskCount",
        ] {
            assert!(obj.contains_key(key), "missing wire key {key}");
        }
        assert_eq!(obj["paneId"], serde_json::json!(3));
        assert_eq!(obj["liveTaskCount"], serde_json::json!(0));
    }

    #[test]
    fn panes_status_on_unknown_panes_is_graceful_and_empty() {
        // A batch of stale ids degrades per pane to the empty status — the
        // whole batch never errors, and order matches the request (U1).
        let m = PtyManager::new();
        let out = m.panes_status(&[PaneId(7), PaneId(9)]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].pane_id, PaneId(7));
        assert_eq!(out[1].pane_id, PaneId(9));
        for s in &out {
            assert!(!s.is_agent);
            assert_eq!(s.cwd, None);
            assert_eq!(s.argv, None);
            assert_eq!(s.session_id, None);
            assert_eq!(s.working_for_ms, None);
            assert_eq!(s.last_output_ago_ms, None);
            assert_eq!(s.live_task_count, 0);
        }
    }

    #[test]
    fn session_cache_serves_within_ttl_without_rescanning() {
        // Two resolutions inside the TTL for the same pane+cwd run the dir
        // scan once (U2/R3); the second call serves the cached answer.
        let m = PtyManager::new();
        let id = PaneId(1);
        let cwd = Path::new("/some/project");
        let t0 = Instant::now();
        let mut scans = 0;
        let r1 = m.session_id_cached(id, cwd, t0, |_| {
            scans += 1;
            Some("sess-a".into())
        });
        assert_eq!(r1.as_deref(), Some("sess-a"));
        let r2 = m.session_id_cached(id, cwd, t0 + Duration::from_secs(2), |_| {
            scans += 1;
            Some("sess-b".into()) // must NOT be reached
        });
        assert_eq!(r2.as_deref(), Some("sess-a"), "cached answer served");
        assert_eq!(scans, 1, "one dir scan inside the TTL");
    }

    #[test]
    fn session_cache_rescans_after_ttl_expiry() {
        let m = PtyManager::new();
        let id = PaneId(1);
        let cwd = Path::new("/some/project");
        let t0 = Instant::now();
        m.session_id_cached(id, cwd, t0, |_| Some("sess-a".into()));
        let r = m.session_id_cached(id, cwd, t0 + SESSION_ID_TTL, |_| Some("sess-b".into()));
        assert_eq!(r.as_deref(), Some("sess-b"), "TTL elapsed → fresh scan");
    }

    #[test]
    fn session_cache_busts_on_cwd_change() {
        // A `cd` mid-TTL must not serve the old project dir's answer (KTD4).
        let m = PtyManager::new();
        let id = PaneId(1);
        let t0 = Instant::now();
        m.session_id_cached(id, Path::new("/proj/a"), t0, |_| Some("sess-a".into()));
        let r = m.session_id_cached(id, Path::new("/proj/b"), t0 + Duration::from_secs(1), |_| {
            Some("sess-b".into())
        });
        assert_eq!(r.as_deref(), Some("sess-b"), "cwd change → fresh scan");
    }

    #[test]
    fn session_cache_caches_abstention_too() {
        // `None` (ambiguous/missing dir) is cached like a hit: the scan cost is
        // identical, and the TTL bounds how long a new session stays unseen.
        let m = PtyManager::new();
        let id = PaneId(1);
        let cwd = Path::new("/some/project");
        let t0 = Instant::now();
        let mut scans = 0;
        assert_eq!(
            m.session_id_cached(id, cwd, t0, |_| {
                scans += 1;
                None
            }),
            None
        );
        assert_eq!(
            m.session_id_cached(id, cwd, t0 + Duration::from_secs(1), |_| {
                scans += 1;
                Some("late".into())
            }),
            None,
            "abstention served from cache inside the TTL"
        );
        assert_eq!(scans, 1);
    }

    #[test]
    fn panes_status_prunes_cache_entries_for_gone_panes() {
        // A closed pane's cache entry is dropped by the next batch call, so the
        // map tracks the live pane set (U2 tidiness).
        let m = PtyManager::new();
        let ghost = PaneId(42);
        m.session_id_cached(ghost, Path::new("/proj"), Instant::now(), |_| Some("s".into()));
        assert_eq!(m.session_cache.lock().unwrap().len(), 1);
        let _ = m.panes_status(&[ghost]);
        assert_eq!(
            m.session_cache.lock().unwrap().len(),
            0,
            "unregistered pane's entry pruned"
        );
    }
}
