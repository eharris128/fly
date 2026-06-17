//! Output streaming + the pane↔attention wiring.
//!
//! Bridges a pane's PTY read thread to the frontend over a raw-byte Channel
//! (KTD3), and connects panes to the authenticated hook channel: each pane gets
//! a token (injected into its env), is registered with the attention manager,
//! and is cleaned up on exit.

use std::sync::Arc;

use serde::Serialize;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Emitter, State};

use crate::hooks::{HookServer, TokenRegistry};
use crate::pty::{PaneId, PtyManager, SpawnConfig};
use crate::state::attention::{AttentionState, Outcome, Reason, Tier};
use crate::state::lifecycle::LifecycleState;
use crate::state::AttentionManager;

pub const PANE_EXIT_EVENT: &str = "pane://exit";
pub const PANE_ATTENTION_EVENT: &str = "pane://attention";

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

/// Spawn a pane: reserve its id, issue + inject its auth token, register it for
/// attention, stream raw output over `channel`, and clean everything up on exit.
#[tauri::command]
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
) -> Result<PaneId, String> {
    let id = pty.reserve_id();
    // Register the token before the child starts so no callback can race it.
    let token = tokens.issue(id);
    attention.register(id);

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
        cwd,
        rows,
        cols,
        env: vec![
            ("FLY_PANE_TOKEN".into(), token.clone()),
            ("FLY_SOCKET_PATH".into(), socket_path),
        ],
        ..Default::default()
    };

    pty.spawn_with_id(id, cfg, token, sink, on_exit)
}

/// Replicate a pane's keyboard focus to the backend (KTD8).
#[tauri::command]
pub fn set_pane_focus(
    app: AppHandle,
    attention: State<'_, Arc<AttentionManager>>,
    pane_id: PaneId,
    focused: bool,
) {
    for (pane, outcome) in attention.set_focus(pane_id, focused) {
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
