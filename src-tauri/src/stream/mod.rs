//! Output streaming + the pane↔attention wiring.
//!
//! Bridges a pane's PTY read thread to the frontend over a raw-byte Channel
//! (KTD3), and connects panes to the authenticated hook channel: each pane gets
//! a token (injected into its env), is registered with the attention manager,
//! and is cleaned up on exit.

pub mod coalesce;

use std::sync::Arc;

use serde::Serialize;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Emitter, Manager, State};

use coalesce::{Coalescer, CoalescerRegistry};

use crate::hooks::{HookServer, TokenRegistry};
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
/// shape is built, shared by the Tauri emit below and the control-socket
/// registry (Electron-shell migration U2/KTD1), so the two shells cannot
/// drift.
pub fn attention_event_payload(pane: PaneId, outcome: &Outcome) -> serde_json::Value {
    serde_json::to_value(AttentionEvent {
        pane_id: pane.0,
        state: outcome.state,
        reason: outcome.reason,
        tier: outcome.tier,
    })
    .expect("attention event serializes")
}

/// Emit an attention-state change to the frontend.
pub fn emit_attention(app: &AppHandle, pane: PaneId, outcome: &Outcome) {
    let _ = app.emit(PANE_ATTENTION_EVENT, attention_event_payload(pane, outcome));
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

/// Emit a recorded notification to the frontend. Title/body are already
/// sanitized (R16/R24) by the caller.
#[allow(clippy::too_many_arguments)]
pub fn emit_notification_added(
    app: &AppHandle,
    id: u64,
    pane: PaneId,
    reason: Reason,
    title: Option<String>,
    body: Option<String>,
    ts: u64,
    read: bool,
) {
    let _ = app.emit(
        NOTIFICATION_ADDED_EVENT,
        notification_added_payload(id, pane, reason, title, body, ts, read),
    );
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

/// Spawn a pane: reserve its id, issue + inject its auth token, register it for
/// attention, stream raw output over `channel`, and clean everything up on exit.
///
/// U7: `automation_run_id` threads the run id for atomically linking run↔pane
/// (R10) — if supplied, the backend links the pane to the run before the child
/// spawns, marking the pane in the recursion registry (R22).
#[tauri::command]
#[allow(clippy::too_many_arguments)] // a Tauri command surface — each arg is a wire field
pub fn spawn_pane(
    app: AppHandle,
    pty: State<'_, Arc<PtyManager>>,
    tokens: State<'_, Arc<TokenRegistry>>,
    attention: State<'_, Arc<AttentionManager>>,
    server: State<'_, HookServer>,
    coalescers: State<'_, Arc<CoalescerRegistry>>,
    channel: Channel<InvokeResponseBody>,
    rows: u16,
    cols: u16,
    cwd: Option<String>,
    leaf_key: String,
    command: Option<Vec<String>>,
    automation_run_id: Option<String>,
    ephemeral: Option<bool>,
) -> Result<PaneId, String> {
    let events: EventSink = {
        let app = app.clone();
        Arc::new(move |name: &str, payload: serde_json::Value| {
            let _ = app.emit(name, payload);
        })
    };
    let deps = SpawnDeps {
        pty: Arc::clone(pty.inner()),
        tokens: Arc::clone(tokens.inner()),
        attention: Arc::clone(attention.inner()),
        coalescers: Arc::clone(coalescers.inner()),
        automations: app
            .try_state::<Arc<crate::automations::AutomationManager>>()
            .map(|s| s.inner().clone()),
        alerts: app
            .try_state::<Arc<crate::automations::alerts::AlertsLog>>()
            .map(|s| s.inner().clone()),
        hook_socket_path: server.socket_path().to_string_lossy().into_owned(),
        events,
    };
    let byte_sink: PaneByteSink = Box::new(move |_id, bytes: Vec<u8>| {
        let _ = channel.send(InvokeResponseBody::Raw(bytes));
    });
    spawn_pane_with(
        &deps,
        SpawnRequest {
            rows,
            cols,
            cwd,
            leaf_key,
            command,
            automation_run_id,
            ephemeral,
        },
        byte_sink,
    )
}

/// Where a shell-agnostic spawn sends its events (`pane://…` names). The Tauri
/// wrapper wraps `app.emit`; the control registry wraps server broadcast
/// (Electron-shell migration U3) — canonical alias re-exported by
/// `control::registry`.
pub type EventSink = Arc<dyn Fn(&str, serde_json::Value) + Send + Sync>;

/// Per-pane raw-output sink, called `(paneId, bytes)`: the Tauri wrapper
/// feeds its per-pane `Channel` (id ignored — the channel IS the pane), the
/// control registry feeds `broadcast_pane_output` binary frames (KTD3),
/// which need the id on every frame.
pub type PaneByteSink = Box<dyn Fn(u64, Vec<u8>) + Send + Sync + 'static>;

/// Everything [`spawn_pane_with`] needs from its shell — the managers both
/// shells share, the optional automation subsystems (absent in a U3 core),
/// the hook-socket path injected into pane env, and the event sink.
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

/// The body behind [`spawn_pane`], shared with the control-socket registry
/// (Electron-shell migration U3) so pane lifecycle — token adopt/issue,
/// attention registration, automation linking, coalesced output, ordered
/// exit teardown — is identical under either shell.
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

    // Raw bytes end-to-end (KTD3) — lossless, but only literally untranscoded
    // above 1 KiB: Tauri 2.11.3 (`ipc/channel.rs:163`) re-encodes a `Raw`
    // chunk **< 1024 bytes** as a JSON number array inside an `eval()` (~3.4×
    // wire cost, exact bytes), and interactive repaints are exactly many
    // sub-1 KiB reads. The per-pane coalescer (T1 of the 2026-07-23
    // performance audit — see `coalesce.rs`) batches reads on a
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

/// Registered for command-surface parity (a command lives in THREE places)
/// but answers `None` under Tauri: this shell threads a per-pane `Channel`
/// through `spawn_pane` and bakes it into the pane's coalescer, so a
/// pane's output can't be re-bound to a new webview. The Tauri webview is
/// never reloaded in practice; a respawn there behaves exactly as before.
/// The Electron registry arm is the real implementation
/// ([`adopt_live_pane_with`]).
#[tauri::command]
pub fn adopt_live_pane(_leaf_key: String) -> Result<Option<AdoptedPane>, String> {
    Ok(None)
}

/// U7 (tmux-substrate KTD6): open the focused pane's tmux session in a real
/// terminal — the native-typing escape hatch. Refused for PTY-backed panes
/// (nothing to attach). The terminal command is `config.terminal`; the argv
/// shape is the tested `substrate::attach_command` table. Spawned detached:
/// the terminal is the user's process, not fly's.
#[tauri::command]
pub fn attach_pane(
    pty: State<'_, Arc<PtyManager>>,
    config: State<'_, Arc<crate::config::ConfigStore>>,
    pane_id: PaneId,
) -> Result<(), String> {
    attach_pane_now(&pty, &config, pane_id)
}

/// The body behind [`attach_pane`], shared with the control-socket registry
/// (Electron-shell migration U2).
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
/// slow for hidden — see `coalesce.rs`).
#[tauri::command]
pub fn set_visible_panes(
    app: AppHandle,
    attention: State<'_, Arc<AttentionManager>>,
    coalescers: State<'_, Arc<CoalescerRegistry>>,
    pane_ids: Vec<PaneId>,
) {
    let ids: Vec<u64> = pane_ids.iter().map(|p| p.0).collect();
    coalescers.set_visible_panes(&ids);
    for (pane, outcome) in attention.set_visible_panes(&pane_ids) {
        emit_attention(&app, pane, &outcome);
    }
}

/// Replicate the window foreground state to the backend (KTD8).
#[tauri::command]
pub fn set_window_foreground(
    app: AppHandle,
    attention: State<'_, Arc<AttentionManager>>,
    foregrounded: bool,
) {
    for (pane, outcome) in attention.set_foreground(foregrounded) {
        emit_attention(&app, pane, &outcome);
    }
}

/// Replicate whether the notification panel is open (a desktop/sound suppressor
/// while foregrounded — KTD15). Affects only the policy, not attention state.
#[tauri::command]
pub fn set_panel_open(attention: State<'_, Arc<AttentionManager>>, open: bool) {
    attention.set_panel_open(open);
}

/// Toggle global do-not-disturb (R17).
#[tauri::command]
pub fn set_muted(attention: State<'_, Arc<AttentionManager>>, muted: bool) {
    attention.set_muted(muted);
}

/// Mute or unmute a single workspace (R18), scoped via the pane→workspace map.
#[tauri::command]
pub fn set_workspace_muted(
    attention: State<'_, Arc<AttentionManager>>,
    workspace: String,
    muted: bool,
) {
    attention.set_workspace_muted(workspace, muted);
}

/// Record which workspace a pane belongs to, for per-workspace mute scoping.
/// Pushed by the frontend once the pane's id is known (U17).
#[tauri::command]
pub fn set_pane_workspace(
    attention: State<'_, Arc<AttentionManager>>,
    pane_id: PaneId,
    workspace: String,
) {
    attention.set_pane_workspace(pane_id, workspace);
}
