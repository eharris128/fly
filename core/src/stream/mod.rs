//! Output streaming + the pane↔attention wiring.
//!
//! Bridges a pane's PTY read thread to the shell over a raw-byte sink (KTD3 —
//! under `fly core` the control socket's binary pane-output frames), and
//! connects panes to the authenticated hook channel: each pane gets a token
//! (injected into its env), is registered with the attention manager, and is
//! cleaned up on exit. The command bodies here are plain fns the control
//! registry dispatches to (`control::registry`, the one command surface).

pub mod coalesce;

use std::sync::Arc;

use serde::Serialize;

use coalesce::{Coalescer, CoalescerRegistry};

use crate::hooks::TokenRegistry;
use crate::pty::{PaneId, PtyManager, SpawnConfig};
use crate::state::attention::{AttentionState, Outcome, Reason, Tier};
use crate::state::lifecycle::LifecycleState;
use crate::state::AttentionManager;

pub const PANE_EXIT_EVENT: &str = "pane://exit";
pub const PANE_ATTENTION_EVENT: &str = "pane://attention";
pub const NOTIFICATION_ADDED_EVENT: &str = "notification://added";

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PaneExitEvent {
    pane_id: u64,
    state: LifecycleState,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AttentionEvent {
    pane_id: u64,
    state: AttentionState,
    reason: Option<Reason>,
    tier: Option<Tier>,
}

/// The `pane://exit` payload as a JSON value — one place, both shells (U3).
pub fn pane_exit_payload(pane_id: u64, state: LifecycleState) -> serde_json::Value {
    serde_json::to_value(PaneExitEvent { pane_id, state }).expect("exit event serializes")
}

/// The `pane://attention` payload as a JSON value — the one place the event
/// shape is built (Electron-shell migration U2/KTD1).
pub fn attention_event_payload(pane: PaneId, outcome: &Outcome) -> serde_json::Value {
    serde_json::to_value(AttentionEvent {
        pane_id: pane.0,
        state: outcome.state,
        reason: outcome.reason,
        tier: outcome.tier,
    })
    .expect("attention event serializes")
}

/// The seed event for one recorded notification (KTD16): the backend is the
/// policy authority and emits this when policy says `record`; the frontend owns
/// the read/unread/cleared lifecycle and resolves `paneId → leafKey`. `read` is
/// the backend-authored read-at-birth bit (the user was viewing the pane).
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct NotificationAddedEvent {
    id: u64,
    pane_id: u64,
    reason: Reason,
    title: Option<String>,
    body: Option<String>,
    ts: u64,
    read: bool,
}

/// The `notification://added` payload as a JSON value — one place, both
/// shells (Electron-shell migration U3.5).
#[allow(clippy::too_many_arguments)]
pub fn notification_added_payload(
    id: u64,
    pane: PaneId,
    reason: Reason,
    title: Option<String>,
    body: Option<String>,
    ts: u64,
    read: bool,
) -> serde_json::Value {
    serde_json::to_value(NotificationAddedEvent {
        id,
        pane_id: pane.0,
        reason,
        title,
        body,
        ts,
        read,
    })
    .expect("notification event serializes")
}

/// Where a spawn sends its events (`pane://…` names): `fly core` wraps
/// `ControlServer::broadcast_event` (Electron-shell migration U3), tests
/// record — canonical alias re-exported by `control::registry`.
pub type EventSink = Arc<dyn Fn(&str, serde_json::Value) + Send + Sync>;

/// Per-pane raw-output sink, called `(paneId, bytes)`: the control registry
/// feeds `broadcast_pane_output` binary frames (KTD3), which need the id on
/// every frame.
pub type PaneByteSink = Box<dyn Fn(u64, Vec<u8>) + Send + Sync + 'static>;

/// Everything [`spawn_pane_with`] needs from its host — the shared managers,
/// the optional automation subsystems (absent in a U3 core), the hook-socket
/// path injected into pane env, and the event sink.
pub struct SpawnDeps {
    pub pty: Arc<PtyManager>,
    pub tokens: Arc<TokenRegistry>,
    pub attention: Arc<AttentionManager>,
    pub coalescers: Arc<CoalescerRegistry>,
    pub automations: Option<Arc<crate::automations::AutomationManager>>,
    pub alerts: Option<Arc<crate::automations::alerts::AlertsLog>>,
    pub hook_socket_path: String,
    pub events: EventSink,
}

/// The wire arguments of a spawn, shell-independent (KTD1 shapes).
pub struct SpawnRequest {
    pub rows: u16,
    pub cols: u16,
    pub cwd: Option<String>,
    pub leaf_key: String,
    pub command: Option<Vec<String>>,
    pub automation_run_id: Option<String>,
    pub ephemeral: Option<bool>,
}

/// The `spawn_pane` command body (Electron-shell migration U3): pane
/// lifecycle — token adopt/issue, attention registration, automation linking,
/// coalesced output, ordered exit teardown — in one place.
pub fn spawn_pane_with(
    deps: &SpawnDeps,
    req: SpawnRequest,
    byte_sink: PaneByteSink,
) -> Result<PaneId, String> {
    let SpawnDeps {
        pty,
        tokens,
        attention,
        coalescers,
        automations,
        alerts,
        hook_socket_path,
        events,
    } = deps;
    let SpawnRequest {
        rows,
        cols,
        cwd,
        leaf_key,
        command,
        automation_run_id,
        ephemeral,
    } = req;
    let id = pty.reserve_id();
    // Register the token before the child starts so no callback can race it.
    // U8 reattach: a leaf whose marked tmux session survives is ADOPTED — the
    // agent's long-lived env holds the PREVIOUS instance's pane token, so
    // that stored token is re-registered for this pane instead of minting
    // one nothing would ever present (KTD8). The spawn path below detects
    // the same adoptable session and skips new-session.
    let token = match pty
        .substrate_handle()
        .and_then(|sub| sub.adoptable_session(&leaf_key))
    {
        Some(record) => match tokens.register_existing(id, &record.token) {
            Ok(()) => record.token,
            Err(_) => tokens.issue(id), // malformed stored token → fresh spawn path
        },
        None => tokens.issue(id),
    };
    attention.register(id);

    // U7 (R10): link this pane to its automation run atomically BEFORE the
    // child spawns, so the ack-timeout / alive-probe / Stop close all see the
    // pane immediately (no window where a live pane reads pane_id = None). A
    // late spawn — the run already force-failed (ack timeout) or was deleted —
    // fails here, aborting the spawn rather than orphaning a live pane that the
    // next occurrence would double up on. Clean up the token/attention
    // registration on that abort so nothing leaks for an id that never spawned.
    if let Some(run_id) = automation_run_id.as_deref() {
        let Some(mgr) = automations.as_ref() else {
            tokens.revoke(id);
            attention.remove(id);
            return Err("automations manager unavailable".into());
        };
        if let Err(e) = mgr.set_run_pane(run_id, id.0) {
            tokens.revoke(id);
            attention.remove(id);
            return Err(e);
        }
        mgr.register_automation_pane(id.0);
    }

    let socket_path = hook_socket_path.clone();

    // Raw bytes end-to-end (KTD3): the control socket's binary pane-output
    // frames carry them untranscoded. The per-pane coalescer (T1 of the
    // 2026-07-23 performance audit — see `coalesce.rs`) batches reads on a
    // visibility-aware deadline before they hit the channel, so most traffic
    // rides the ≥ 1 KiB raw path and the webview sees a few messages per pane
    // per second instead of one per PTY read. One forwarder per pane, one
    // channel — ordering is preserved.
    let coalescer = Coalescer::spawn(id.0, move |bytes: Vec<u8>| {
        byte_sink(id.0, bytes);
    });
    let sink = {
        let coalescer = Arc::clone(&coalescer);
        Box::new(move |bytes: &[u8]| coalescer.push(bytes))
    };

    let on_exit = {
        let events = Arc::clone(events);
        let attention = Arc::clone(attention);
        let tokens = Arc::clone(tokens);
        let coalescer = Arc::clone(&coalescer);
        let coalescers = Arc::clone(coalescers);
        let automations = automations.clone();
        let alerts = alerts.clone();
        Box::new(move |id: PaneId, state: LifecycleState| {
            // Drain + stop the coalescer before anything is announced, so the
            // pane's final output reaches the frontend ahead of the exit note.
            coalescer.close();
            coalescers.remove(id.0);
            // Exiting clears attention and tears down the pane's auth.
            if let Some(outcome) = attention.on_exit(id) {
                events(PANE_ATTENTION_EVENT, attention_event_payload(id, &outcome));
            }
            attention.remove(id);
            tokens.revoke(id);
            // U7 pane-exit tap: if this pane was an automation agent run, close
            // its still-running row failed (a pane that died before Stop) and
            // clear the recursion-registry entry. Spawned off the PTY read thread
            // because the U4b output capture inside the close now retries the
            // transcript read (bounded ~2s), which must not block pane teardown
            // (alert-sink clear + PANE_EXIT_EVENT below). on_pane_exit takes the
            // store lock, never the PTY registry lock, so the store→PTY order
            // (KTD-B) holds. A no-op for ordinary panes.
            if let Some(mgr) = automations.clone() {
                let pane_id = id.0;
                std::thread::spawn(move || mgr.on_pane_exit(pane_id));
            }
            // U6: if this pane was the automations alert sink, clear the
            // registration so a later alert re-opens a fresh sink pane
            // (no-op for any other pane).
            if let Some(alerts) = alerts.as_ref() {
                alerts.clear_sink_if(id.0);
            }
            events(PANE_EXIT_EVENT, pane_exit_payload(id.0, state));
        })
    };

    let cfg = SpawnConfig {
        command,
        cwd,
        rows,
        cols,
        leaf_key: Some(leaf_key),
        // U10: automation-linked panes are ephemeral by definition; the
        // frontend passes the flag for the other ephemeral tabs (sink).
        ephemeral: ephemeral.unwrap_or(false) || automation_run_id.is_some(),
        env: vec![
            ("FLY_PANE_TOKEN".into(), token.clone()),
            ("FLY_SOCKET_PATH".into(), socket_path),
        ],
        ..Default::default()
    };

    // Register before the spawn: the read thread only touches the registry on
    // its exit path, so insert-before-spawn guarantees removal follows
    // insertion even for a child that dies instantly.
    coalescers.insert(id.0, Arc::clone(&coalescer));
    match pty.spawn_with_id(id, cfg, token, sink, on_exit) {
        Ok(pane) => Ok(pane),
        Err(e) => {
            coalescers.remove(id.0);
            coalescer.close(); // stop the forwarder thread; nothing to drain
            Err(e)
        }
    }
}

/// What a renderer gets back when it re-attaches to a pane the backend still
/// owns (renderer-crash recovery): the live pane's id, its recent raw output
/// to repaint from, and the attention it may have raised while nobody was
/// listening.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdoptedPane {
    pub pane_id: u64,
    /// The pane's CURRENT grid — the renderer sizes its xterm to this
    /// before replaying `tail`, so the bytes land on the geometry they were
    /// produced for (a replay into a differently sized grid soft-wraps
    /// wrongly and then reflows into a smear on the first fit). The pane is
    /// deliberately NOT resized here: the frontend mounts restored panes
    /// hidden behind the dashboard with a default 80×24 xterm, and driving
    /// a live agent's TUI to that and back would cost two full re-layouts;
    /// the pane's ResizeObserver fits and resizes it once it is shown.
    pub rows: u16,
    pub cols: u16,
    /// The pane's raw-output tail ring (`pty::pane::TailRing`, ≤ 64 KiB —
    /// the same bytes the feed's screen fallback replays through `vte`),
    /// UTF-8 lossy: the ring starts at an arbitrary byte, so a split
    /// multibyte char at the cut is replaced rather than refused.
    pub tail: String,
    pub attention: AttentionState,
    pub reason: Option<Reason>,
}

/// Re-attach a renderer to the **live** pane already owning `leaf_key`,
/// instead of spawning a second one (renderer-crash recovery, 2026-08-22).
/// When the Chromium renderer dies the backend and its panes survive — the
/// Electron shell reloads the frontend, which restores its saved layout and
/// would otherwise `spawn_pane` every leaf again: under the pty substrate
/// that orphans each working agent behind a fresh shell (invisible until
/// shutdown reap); under tmux it adopts the session but leaves the previous
/// `Pane` registered for the same leaf. This is the one path that makes a
/// reload a re-attach on both substrates: same pane id (the token, the
/// attention registration, the automation link, the coalescer all stay),
/// the pane's grid reported so the xterm can match it, and the tail ring
/// returned for the initial paint.
///
/// Capture-then-subscribe, like the tmux U8 adopt replay: the renderer
/// discards frames buffered before its sink binds and paints the tail, so a
/// few ms of output in the gap can be lost but never duplicated — loss is
/// invisible on an idle reattach, duplication never is. `None` ⇒ no live
/// pane for that leaf: the caller spawns as usual. Live only
/// (`pane_by_leaf`'s `is_live` gate): an exited-but-unreaped pane would
/// accept no input and is about to announce `pane://exit` to nobody.
pub fn adopt_live_pane_with(
    pty: &PtyManager,
    attention: &AttentionManager,
    leaf_key: &str,
) -> Option<AdoptedPane> {
    let id = pty.pane_by_leaf(leaf_key)?;
    let tail = pty.screen_tail_by_leaf(leaf_key)?;
    let (state, reason) = attention
        .snapshot(id)
        .unwrap_or((AttentionState::Idle, None));
    Some(AdoptedPane {
        pane_id: id.0,
        rows: tail.rows,
        cols: tail.cols,
        tail: String::from_utf8_lossy(&tail.bytes).into_owned(),
        attention: state,
        reason,
    })
}

/// U7 (tmux-substrate KTD6): open the focused pane's tmux session in a real
/// terminal — the native-typing escape hatch. Refused for PTY-backed panes
/// (nothing to attach). The terminal command is `config.terminal`; the argv
/// shape is the tested `substrate::attach_command` table. Spawned detached:
/// the terminal is the user's process, not fly's. The `attach_pane` command.
pub fn attach_pane_now(
    pty: &PtyManager,
    config: &crate::config::ConfigStore,
    pane_id: PaneId,
) -> Result<(), String> {
    let Some((sub, session)) = pty.tmux_backend_of(pane_id) else {
        return Err(
            "this pane is not tmux-backed (substrate is off) — nothing to attach".into(),
        );
    };
    let argv = crate::substrate::attach_command(
        &config.get().terminal,
        sub.socket_name(),
        &session,
    );
    std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("launching {}: {e}", argv[0]))?;
    Ok(())
}

/// Replicate the set of visible panes — the active tab's leaves in the active
/// workspace (U17). Generalizes the old per-pane keyboard-focus replication:
/// any visible pane counts as "looking" for the Acknowledged transition. Also
/// retunes every pane's output-coalescing deadline (fast for visible panes,
/// slow for hidden — see `coalesce.rs`). The `set_visible_panes` command body
/// (2026-08-27-001 KTD3: the registry dispatches here, it holds no logic).
pub fn set_visible_panes(
    attention: &AttentionManager,
    coalescers: &CoalescerRegistry,
    events: &EventSink,
    pane_ids: &[PaneId],
) {
    let ids: Vec<u64> = pane_ids.iter().map(|p| p.0).collect();
    coalescers.set_visible_panes(&ids);
    for (pane, outcome) in attention.set_visible_panes(pane_ids) {
        events(PANE_ATTENTION_EVENT, attention_event_payload(pane, &outcome));
    }
}

/// Replicate the window foreground state to the backend (KTD8). The
/// `set_window_foreground` command body.
pub fn set_window_foreground(attention: &AttentionManager, events: &EventSink, foregrounded: bool) {
    for (pane, outcome) in attention.set_foreground(foregrounded) {
        events(PANE_ATTENTION_EVENT, attention_event_payload(pane, &outcome));
    }
}

/// Write input to a pane and clear its attention — the `pty_write` command
/// body (2026-08-27-001 KTD3). xterm.js `onData` yields a string whose UTF-8
/// bytes (including control bytes like Ctrl-C `0x03`) are written verbatim;
/// typing also clears the pane's attention (stage two of the two-stage
/// clear). A full kernel PTY buffer can block the write, which is why the
/// shell serializes writes per pane (`lib/write-chain.ts`, poll-batching
/// KTD5/R5) and the control socket serves each connection on its own thread.
pub fn write_input(
    pty: &PtyManager,
    attention: &AttentionManager,
    events: &EventSink,
    pane_id: PaneId,
    data: &str,
) -> Result<(), String> {
    pty.write(pane_id, data.as_bytes())?;
    if let Some(outcome) = attention.on_input(pane_id) {
        events(PANE_ATTENTION_EVENT, attention_event_payload(pane_id, &outcome));
    }
    Ok(())
}
