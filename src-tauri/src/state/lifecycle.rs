//! Pane lifecycle state.
//!
//! One half of the orthogonal per-pane model (KTD8). The variants mirror the
//! HTD lifecycle diagram. U2 drives `Spawning`/`Live`/`Exited`/`Killed`/
//! `Failed`; `RestoredInert` is entered on session restore (U12). U7 adds the
//! formal transition table and the attention machine.

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LifecycleState {
    /// Child is being forked/exec'd.
    Spawning,
    /// Child is running.
    Live,
    /// Child exited on its own. `code` is the wait status exit code; `signal`
    /// names the terminating signal when the child was signalled, so the UI
    /// can distinguish a clean `0` from a non-zero or signal exit.
    Exited { code: i32, signal: Option<String> },
    /// Child was terminated by an explicit pane close or app quit.
    Killed,
    /// Spawn failed (fork / cwd / exec error).
    Failed { error: String },
    /// Restored from a saved session as inert text; becomes `Live` on the
    /// first command (U12).
    RestoredInert,
}

impl LifecycleState {
    /// Terminal states never transition further on their own.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            LifecycleState::Exited { .. } | LifecycleState::Killed | LifecycleState::Failed { .. }
        )
    }

    /// True only while the child process is running.
    pub fn is_live(&self) -> bool {
        matches!(self, LifecycleState::Live)
    }
}
