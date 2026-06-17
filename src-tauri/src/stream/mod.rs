//! Output streaming: bridges a pane's PTY read thread to the frontend over a
//! Tauri Channel carrying raw bytes (KTD3). The control-plane commands
//! (write/resize/close) live in `pty`; this module owns `spawn_pane` and the
//! raw-byte output path, plus the pane-exit event.

use std::sync::Arc;

use serde::Serialize;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Emitter, State};

use crate::pty::{PaneId, PtyManager, SpawnConfig};
use crate::state::lifecycle::LifecycleState;

/// Event name carrying a pane's terminal lifecycle state to the frontend.
pub const PANE_EXIT_EVENT: &str = "pane://exit";

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PaneExitEvent {
    pane_id: u64,
    state: LifecycleState,
}

/// Spawn a pane and stream its raw PTY output over `channel`.
///
/// `channel` is a `Channel<ArrayBuffer>` on the JS side; we send
/// `InvokeResponseBody::Raw` so bytes cross the IPC boundary without
/// base64/JSON transcoding that would corrupt UTF-8 or escape sequences (KTD3).
/// The per-pane auth token is wired in U8; here it is empty.
#[tauri::command]
pub fn spawn_pane(
    app: AppHandle,
    manager: State<'_, Arc<PtyManager>>,
    channel: Channel<InvokeResponseBody>,
    rows: u16,
    cols: u16,
    cwd: Option<String>,
) -> Result<PaneId, String> {
    let sink = Box::new(move |bytes: &[u8]| {
        let _ = channel.send(InvokeResponseBody::Raw(bytes.to_vec()));
    });

    let on_exit = {
        let app = app.clone();
        Box::new(move |id: PaneId, state: LifecycleState| {
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
        ..Default::default()
    };

    manager.spawn(cfg, String::new(), sink, on_exit)
}
