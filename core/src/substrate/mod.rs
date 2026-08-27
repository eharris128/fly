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
    /// The stable hook-socket path (KTD8), injected into the server env so
    /// tmux `run-shell` hooks can reach fly (KTD12).
    socket_path: PathBuf,
    /// The fly binary tmux hooks invoke. Production: `current_exe`; tests:
    /// `CARGO_BIN_EXE_fly` (a hook must never re-enter a test harness).
    fly_bin: PathBuf,
    /// KTD12 server-scope event token: minted per Substrate, injected into
    /// the tmux server env, presented back by `fly substrate-event`.
    /// Authorizes exactly the event-report ops — nothing else.
    event_token: String,
    /// `ensure_server` latch: probe/start once per process, under a lock so
    /// two racing spawns can't both start servers.
    server_ready: Mutex<bool>,
}

impl Substrate {
    pub fn new(
        flavor: String,
        store_path: PathBuf,
        fifo_dir: PathBuf,
        socket_path: PathBuf,
        fly_bin: PathBuf,
    ) -> Self {
        let tmux = Tmux::new(TmuxConfig {
            socket_name: flavor.clone(),
            history_limit: 10_000,
        });
        // KTD12 token, PERSISTED for continuity (U8): a tmux server that
        // outlives fly holds the token in its env, so a new instance must
        // present the same one for surviving sessions' hooks — persist-and-
        // reuse sidesteps the (unverified) question of whether run-shell
        // children observe post-hoc `set-environment -g` refreshes at all.
        let event_token = store::load_or_mint_server_token(
            &store_path.with_file_name("substrate-server.json"),
        );
        Self {
            tmux,
            flavor,
            store_path,
            fifo_dir,
            socket_path,
            fly_bin,
            event_token,
            server_ready: Mutex::new(false),
        }
    }

    /// The fly binary path for tmux hook commands (validated quote-free at
    /// arm time by the wrapper).
    pub fn fly_bin(&self) -> &std::path::Path {
        &self.fly_bin
    }

    /// Constant-time validation of a presented substrate event token
    /// (KTD12). Never compare with `==` — boundary rule.
    pub fn validate_event_token(&self, presented: &str) -> bool {
        use subtle::ConstantTimeEq as _;
        let a = self.event_token.as_bytes();
        let b = presented.as_bytes();
        if a.len() != b.len() {
            // Length is public (a fixed 64-hex format), not secret-bearing.
            return false;
        }
        a.ct_eq(b).into()
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

    /// The flavor's socket name (for building attach argv).
    pub fn socket_name(&self) -> &str {
        &self.flavor
    }

    /// A surviving, adoptable session for a leaf (U8 reattach): the store
    /// holds a binding AND tmux confirms the session lives. `None` means
    /// spawn fresh. Stale records for dead sessions are left in place —
    /// harmless (the next spawn overwrites) and KTD7-safe (an unreachable
    /// server must never look like "no session").
    pub fn adoptable_session(&self, leaf_key: &str) -> Option<store::SessionRecord> {
        let record = store::read_records(&self.store_path)
            .get(leaf_key)
            .cloned()?;
        match self.tmux.has_session(&record.session_name) {
            Ok(true) => Some(record),
            _ => None,
        }
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
            let mut env: BTreeMap<String, String> = std::env::vars()
                .filter(|(k, _)| {
                    !crate::pty::CLAUDE_SESSION_MARKERS.contains(&k.as_str())
                })
                .collect();
            // KTD12: hook commands run with the SERVER env — this is how
            // `fly substrate-event` finds the socket and authenticates.
            env.insert(
                "FLY_SUBSTRATE_TOKEN".into(),
                self.event_token.clone(),
            );
            env.insert(
                "FLY_SOCKET_PATH".into(),
                self.socket_path.to_string_lossy().into_owned(),
            );
            self.tmux.start_server(&env)?;
        }
        *ready = true;
        Ok(())
    }
}

/// Build the argv that opens `terminal` attached to a session (U7/KTD6).
/// The command-separator convention varies by family: gnome-terminal wants
/// `--`, kitty takes the command positionally, most others accept `-e`
/// followed by argv words (xterm/konsole/alacritty/x-terminal-emulator).
/// Pure so the table is unit-tested; unknown terminals get the `-e` shape.
pub fn attach_command(terminal: &str, socket_name: &str, session: &str) -> Vec<String> {
    let base = terminal.rsplit('/').next().unwrap_or(terminal);
    let mut argv: Vec<String> = vec![terminal.to_string()];
    if base.starts_with("gnome-terminal") {
        argv.push("--".into());
    } else if !base.starts_with("kitty") {
        argv.push("-e".into());
    }
    argv.extend(
        ["tmux", "-L", socket_name, "attach-session", "-t", session]
            .iter()
            .map(|s| s.to_string()),
    );
    argv
}

#[cfg(test)]
mod attach_tests {
    use super::attach_command;

    #[test]
    fn attach_command_adapts_separator_per_family() {
        let g = attach_command("/usr/bin/gnome-terminal", "fly", "fly-fly-a");
        assert_eq!(g[1], "--");
        let k = attach_command("kitty", "fly", "fly-fly-a");
        assert_eq!(k[1], "tmux");
        let x = attach_command("x-terminal-emulator", "fly-dev", "fly-fly-dev-b");
        assert_eq!(x[1], "-e");
        assert!(x.ends_with(&[
            "tmux".into(),
            "-L".into(),
            "fly-dev".into(),
            "attach-session".into(),
            "-t".into(),
            "fly-fly-dev-b".into()
        ]));
    }
}
