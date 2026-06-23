//! App lifecycle: ordered shutdown so no save is lost and no process leaks (U14).
//!
//! Composes the teardown fragments owned by other units: U2's per-pane reap,
//! U8's socket/token release (via `HookServer::Drop`), and U12's single-instance
//! lock (via the plugin). On launch, a stale socket from a prior crash is
//! reclaimed when the hook server re-binds (U8).

use std::sync::Arc;

use tauri::{AppHandle, Manager};

use crate::pty::PtyManager;

/// Tear down on quit (R4): close and reap every pane so no child is orphaned or
/// left a zombie. The frontend has already flushed a final session save (via
/// its close-requested handler) before this runs, while the shells are still
/// alive so their cwds are captured (R13/R14).
pub fn shutdown(app: &AppHandle) {
    // Write the clean-exit marker *before* reaping (KTD-G): reaching this ordered
    // path at all means the quit was clean, so the next launch sees the marker and
    // stays in normal mode. An unclean exit never runs this, leaving the marker
    // absent → the next launch offers resume (U7/U2). Best-effort; a failure to
    // write it just makes the next launch over-offer, never under-offer.
    let _ = crate::session::resume::set_clean_exit_at(
        &crate::session::resume::clean_exit_path(),
        true,
    );
    if let Some(pty) = app.try_state::<Arc<PtyManager>>() {
        pty.close_all();
    }
    // The hook socket and single-instance lock are released by their Drop impls
    // as the managed state is dropped on exit.
}
