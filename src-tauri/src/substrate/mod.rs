//! The session substrate: fly-managed tmux server + sessions.
//!
//! Plan: `docs/plans/2026-08-11-001-feat-tmux-session-substrate-plan.md`.
//! This module is U1 — the tmux wrapper with a pure, executor-seamed core so
//! argument construction and error classification are unit-tested without a
//! tmux binary (the Gas City provider shape; see the 2026-08-11 reference-
//! mining note). Nothing here is reachable from the app until U3 switches the
//! spawn path behind KTD10's `substrate` config flag.
//!
//! Boundaries (KTD3/KTD4): fly owns the per-flavor server (`-L` socket) and
//! only ever operates on sessions it created and marked by name. The wrapper
//! refuses invalid names outright — tmux target metacharacters (`.`,`:`)
//! silently misroute rather than erroring.

pub mod naming;
pub mod store;
pub mod tmux;

pub use naming::{leaf_session_name, session_leaf_slug, validate_session_name};
pub use tmux::{Executor, RealExecutor, Tmux, TmuxConfig, TmuxError};

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// The app-wide substrate handle (U3): the flavor's tmux driver plus the
/// paths the session lifecycle needs. One per app, constructed at setup when
/// the `substrate` config flag selects tmux (KTD10), shared into
/// `PtyManager` and the spawn path.
pub struct Substrate {
    tmux: Tmux,
    flavor: String,
    /// Durable leaf ⇄ session ⇄ token store (U2).
    store_path: PathBuf,
    /// Where per-pane FIFOs live (runtime dir — same lifetime class as the
    /// hook socket).
    fifo_dir: PathBuf,
    /// `ensure_server` latch: probe/start once per process, under a lock so
    /// two racing spawns can't both start servers.
    server_ready: Mutex<bool>,
}

impl Substrate {
    pub fn new(flavor: String, store_path: PathBuf, fifo_dir: PathBuf) -> Self {
        let tmux = Tmux::new(TmuxConfig {
            socket_name: flavor.clone(),
            history_limit: 10_000,
        });
        Self {
            tmux,
            flavor,
            store_path,
            fifo_dir,
            server_ready: Mutex::new(false),
        }
    }

    pub fn tmux(&self) -> &Tmux {
        &self.tmux
    }

    pub fn store_path(&self) -> &std::path::Path {
        &self.store_path
    }

    /// The marked session name for a leaf (KTD4).
    pub fn session_name(&self, leaf_key: &str) -> String {
        leaf_session_name(&self.flavor, leaf_key)
    }

    /// This pane's FIFO path, keyed by pane id (ids are process-unique).
    pub fn fifo_path(&self, pane_id: u64) -> PathBuf {
        self.fifo_dir.join(format!("pane-{pane_id}.pipe"))
    }

    /// Probe-or-start the flavor server (KTD3). The server env is fly's own
    /// env **minus the Claude session markers** — the server's global env is
    /// the inherited baseline of every future pane (the overlay trap), so the
    /// strip that `Pane::spawn` applies per child moves here for the tmux
    /// arm. Degraded servers refuse (ga-h9z), they are never replaced.
    pub fn ensure_server(&self) -> Result<(), TmuxError> {
        let mut ready = self.server_ready.lock().unwrap();
        if *ready {
            return Ok(());
        }
        if !self.tmux.probe_server_alive()? {
            let env: BTreeMap<String, String> = std::env::vars()
                .filter(|(k, _)| {
                    !crate::pty::CLAUDE_SESSION_MARKERS.contains(&k.as_str())
                })
                .collect();
            self.tmux.start_server(&env)?;
        }
        *ready = true;
        Ok(())
    }
}
