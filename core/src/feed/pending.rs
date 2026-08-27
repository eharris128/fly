//! Ask-time raise stamps (feed-question-screen-fallback U3, KTD5).
//!
//! When Claude Code raises attention for a question/permission dialog, the
//! hook dispatch stamps the wall-clock time here, keyed by the pane's stable
//! leaf key. That stamp is the screen-derived question's `askedAt` and the
//! frame's tier-1 `questionPendingAt` — the ask-time value the transcript can
//! no longer provide on v2.1.206 (it flushes the `tool_use` only at
//! resolution).
//!
//! Stamps are overwritten by the next raise and never explicitly cleared:
//! exposure is gated downstream on live corroboration (the roster's attention
//! reason + Claude's sessions file, KTD4), so a stale stamp is inert. A
//! re-notify for the same dialog may move the stamp; the consequence is a 409
//! on an in-flight `ifAskedAt` answer — the safe direction.

use std::collections::HashMap;
use std::sync::Mutex;

/// Leaf-keyed ask-time stamps. Managed unconditionally (like `FeedState`):
/// the dispatch stamps even when the feed listener is disabled.
#[derive(Default)]
pub struct PendingSignals {
    stamps: Mutex<HashMap<String, u64>>,
}

impl PendingSignals {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the raise time for a leaf (epoch ms). Last write wins.
    pub fn stamp(&self, leaf_key: &str, at_ms: u64) {
        self.stamps
            .lock()
            .unwrap()
            .insert(leaf_key.to_string(), at_ms);
    }

    /// The leaf's most recent raise stamp, if any.
    pub fn get(&self, leaf_key: &str) -> Option<u64> {
        self.stamps.lock().unwrap().get(leaf_key).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_are_per_leaf_and_last_write_wins() {
        let s = PendingSignals::new();
        assert_eq!(s.get("leaf-1"), None);
        s.stamp("leaf-1", 100);
        s.stamp("leaf-2", 200);
        assert_eq!(s.get("leaf-1"), Some(100));
        assert_eq!(s.get("leaf-2"), Some(200));
        s.stamp("leaf-1", 300);
        assert_eq!(s.get("leaf-1"), Some(300), "a re-raise moves the stamp");
    }
}
