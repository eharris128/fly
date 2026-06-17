//! Live working-directory tracking (U10, R13).
//!
//! Reads `/proc/<pid>/cwd` of a pane's foreground process — robust on Linux and
//! needs no shell cooperation, since default Ubuntu shells don't emit OSC 7
//! outside VTE. Sampled on a low cadence (focus change / before a save), never
//! on the hot output path. The optional OSC 7 fast path and the OSC/BEL
//! attention scanner are deferred with the multi-agent matrix (KTD9).

use std::path::PathBuf;

/// The current working directory of process `pid`, via `/proc/<pid>/cwd`.
/// Returns `None` if the process is gone or unreadable.
pub fn proc_cwd(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}
