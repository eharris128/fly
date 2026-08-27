//! The core control socket (Electron-shell migration plan
//! `2026-08-12-002`, U1): the transport a display shell (Electron main /
//! renderer bridge) uses to drive the headless fly backend. Wire contract in
//! `docs/core-protocol.md` — that document and this module are edited only
//! together.
//!
//! Shape mirrors `hooks/` (the socket discipline is deliberately the same —
//! same-uid peer-cred gate, never-steal bind, bounded frames, `ConnCap`):
//! - [`frame`] — the length-prefixed binary framing (JSON / pane-output /
//!   pane-input frames; KTD3's no-JSON byte path).
//! - [`envelope`] — the JSON request/response/event envelopes (KTD1: command
//!   and event names are exactly the names `src/ipc.ts` sends).
//! - [`server`] — bind/accept/dispatch/broadcast; U2 registers the real
//!   command table, U1 ships only `core/ping`.
//!
//! There is deliberately **no token** here: the hook socket authenticates
//! *which pane* a caller is; the control client is the user's own shell, so
//! same-uid is the whole boundary (KTD2 keeps everything security-relevant on
//! the Rust side of this seam).

pub mod envelope;
pub mod frame;
pub mod registry;
pub mod server;

pub use envelope::{Event, Request};
pub use frame::{Frame, MAX_FRAME};
pub use server::{CommandHandler, ControlServer, PaneInputHandler};

use std::path::PathBuf;

/// Where the control socket lives — beside `hook.sock` under the per-flavor
/// runtime dir, for the same reason (`lib.rs::hook_socket_path`): the path
/// must be stable per flavor so a shell can find its backend across restarts,
/// while dev flavors stay isolated.
pub fn control_socket_path() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join(crate::app_dir_name()).join("control.sock")
}
