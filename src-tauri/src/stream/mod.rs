//! Output streaming + the pane↔attention wiring.
//!
//! Bridges a pane's PTY read thread to the frontend over a raw-byte Channel
//! (KTD3), and connects panes to the authenticated hook channel: each pane gets
//! a token (injected into its env), is registered with the attention manager,
//! and is cleaned up on exit.

use std::sync::Arc;

use serde::Serialize;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Emitter, Manager, State};

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

/// Emit an attention-state change to the frontend.
pub fn emit_attention(app: &AppHandle, pane: PaneId, outcome: &Outcome) {
    let _ = app.emit(
        PANE_ATTENTION_EVENT,
        AttentionEvent {
            pane_id: pane.0,
            state: outcome.state,
            reason: outcome.reason,
            tier: outcome.tier,
        },
    );
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
        NotificationAddedEvent {
            id,
            pane_id: pane.0,
            reason,
            title,
            body,
            ts,
            read,
        },
    );
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
    channel: Channel<InvokeResponseBody>,
    rows: u16,
    cols: u16,
    cwd: Option<String>,
    leaf_key: String,
    command: Option<Vec<String>>,
    automation_run_id: Option<String>,
) -> Result<PaneId, String> {
    let id = pty.reserve_id();
    // Register the token before the child starts so no callback can race it.
    let token = tokens.issue(id);
    attention.register(id);

    // U7 (R10): link this pane to its automation run atomically BEFORE the
    // child spawns, so the ack-timeout / alive-probe / Stop close all see the
    // pane immediately (no window where a live pane reads pane_id = None). A
    // late spawn — the run already force-failed (ack timeout) or was deleted —
    // fails here, aborting the spawn rather than orphaning a live pane that the
    // next occurrence would double up on. Clean up the token/attention
    // registration on that abort so nothing leaks for an id that never spawned.
    if let Some(run_id) = automation_run_id.as_deref() {
        let Some(mgr) = app.try_state::<Arc<crate::automations::AutomationManager>>() else {
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

    let socket_path = server.socket_path().to_string_lossy().into_owned();

    // Raw bytes end-to-end, no transcoding (KTD3).
    let sink = Box::new(move |bytes: &[u8]| {
        let _ = channel.send(InvokeResponseBody::Raw(bytes.to_vec()));
    });

    let on_exit = {
        let app = app.clone();
        let attention = Arc::clone(attention.inner());
        let tokens = Arc::clone(tokens.inner());
        Box::new(move |id: PaneId, state: LifecycleState| {
            // Exiting clears attention and tears down the pane's auth.
            if let Some(outcome) = attention.on_exit(id) {
                emit_attention(&app, id, &outcome);
            }
            attention.remove(id);
            tokens.revoke(id);
            // U7 pane-exit tap: if this pane was an automation agent run, close
            // its still-running row failed (a pane that died before Stop) and
            // clear the recursion-registry entry. This runs on the PTY read
            // thread holding no PTY registry lock, so the store lock it takes
            // respects the store→PTY order (KTD-B). A no-op for ordinary panes.
            if let Some(mgr) = app.try_state::<Arc<crate::automations::AutomationManager>>() {
                mgr.on_pane_exit(id.0);
            }
            // U6: if this pane was the automations alert sink, clear the
            // registration so a later alert re-opens a fresh sink pane
            // (no-op for any other pane).
            if let Some(alerts) = app.try_state::<Arc<crate::automations::alerts::AlertsLog>>() {
                alerts.clear_sink_if(id.0);
            }
            let _ = app.emit(
                PANE_EXIT_EVENT,
                PaneExitEvent {
                    pane_id: id.0,
                    state,
                },
            );
        })
    };

    let cfg = SpawnConfig {
        command,
        cwd,
        rows,
        cols,
        leaf_key: Some(leaf_key),
        env: vec![
            ("FLY_PANE_TOKEN".into(), token.clone()),
            ("FLY_SOCKET_PATH".into(), socket_path),
        ],
        ..Default::default()
    };

    pty.spawn_with_id(id, cfg, token, sink, on_exit)
}

/// Replicate the set of visible panes — the active tab's leaves in the active
/// workspace (U17). Generalizes the old per-pane keyboard-focus replication:
/// any visible pane counts as "looking" for the Acknowledged transition.
#[tauri::command]
pub fn set_visible_panes(
    app: AppHandle,
    attention: State<'_, Arc<AttentionManager>>,
    pane_ids: Vec<PaneId>,
) {
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
