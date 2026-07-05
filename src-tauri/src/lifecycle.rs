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
    // Automations (R5), BEFORE the PTY reap: (1) stop and join the sweep
    // thread — joined here with no store lock held (KTD-B), and once joined no
    // new claim can race the closes below; (2) kill in-flight script groups
    // (the U5 killer seam; killer runs outside the store lock) and close every
    // in-flight run row failed("interrupted") in one final flush. Ordered
    // before `pty.close_all()` so an in-flight *agent* run's row closes while
    // its pane teardown is still pending — the row must never record a pane
    // exit as the outcome of a shutdown.
    if let Some(sweep) = app.try_state::<crate::automations::SweepHandle>() {
        sweep.stop_and_join();
    }
    if let Some(automations) = app.try_state::<Arc<crate::automations::AutomationManager>>() {
        automations.shutdown();
    }
    // Local feed (feat-agent-state-local-feed): signal teardown so every blocked
    // SSE reader thread wakes and exits promptly. The listener thread itself is
    // joined by `FeedServer::Drop` when the managed state drops on exit (same
    // Drop-owned pattern as the hook socket below). Ordered here so readers stop
    // before the panes they describe are reaped.
    if let Some(feed) = app.try_state::<Arc<crate::feed::FeedState>>() {
        feed.shutdown();
    }
    if let Some(pty) = app.try_state::<Arc<PtyManager>>() {
        pty.close_all();
    }
    // The hook socket, feed server, and single-instance lock are released by
    // their Drop impls as the managed state is dropped on exit.
}
