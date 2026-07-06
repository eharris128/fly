//! PTY backend: a registry of panes keyed by an opaque, never-reused
//! [`PaneId`]. All pane operations route through [`PtyManager`] so the deferred
//! daemonized-mux / remote-pane work becomes a transport swap behind a stable
//! interface (KTD5).

mod pane;

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::state::lifecycle::LifecycleState;
use pane::Pane;

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
}

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
    pub fn spawn_with_id(
        &self,
        id: PaneId,
        cfg: SpawnConfig,
        token: String,
        sink: OutputSink,
        on_exit: ExitCallback,
    ) -> Result<PaneId, String> {
        let pane = Pane::spawn(id, cfg, token, sink, on_exit)?;
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

    /// The pane's foreground pid, for `/proc`-based cwd tracking (U10).
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
#[tauri::command]
pub fn pty_write(
    app: tauri::AppHandle,
    manager: tauri::State<'_, Arc<PtyManager>>,
    attention: tauri::State<'_, Arc<crate::state::AttentionManager>>,
    pane_id: PaneId,
    data: String,
) -> Result<(), String> {
    manager.write(pane_id, data.as_bytes())?;
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
}
