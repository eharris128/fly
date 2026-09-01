//! The held permission-ask registry (hook-ask-channel U3, KTD1/KTD2).
//!
//! One entry per leaf: the typed ask a `PermissionRequest` hook registered
//! over the socket, plus the sender half of the held connection's decision
//! mailbox. The entry's lifetime IS the ask's lifetime: it is created when the
//! hook connects, and removed when the ask resolves — a remote answer
//! ([`AskRegistry::answer`] sends the decision line), a local answer (Claude
//! kills the hook, the connection drops, the conn thread's `on_drop` calls
//! [`AskRegistry::clear_if`]), a replacement (a newer ask for the same leaf —
//! Claude shows one dialog per session at a time, so the older one is stale by
//! definition), or shutdown. No timers, no polling.
//!
//! Bounded (KTD2): at [`MAX_HELD_ASKS`] a *new* leaf's registration is
//! declined — the hook is released immediately and detection degrades to the
//! transcript/screen chain. A replacement never counts against the cap.
//!
//! Generations make the drop guard race-safe: a connection that dies *after*
//! its entry was replaced must not clear the replacement, so `clear_if` only
//! removes the generation it was armed with.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Mutex;

use crate::hooks::protocol::{ask_decision_line, AskPayload};

/// Cap on concurrently held asks (KTD2) — one per leaf, so this is really a
/// cap on simultaneously-blocked panes. Far above any real workspace; a bound,
/// not a budget.
pub const MAX_HELD_ASKS: usize = 64;

/// A read-only view of one held ask, served to the feed resolver (U6).
#[derive(Debug, Clone, PartialEq)]
pub struct HeldAsk {
    /// Fly's own receipt stamp (epoch ms) — the ask's `askedAt` on every wire
    /// surface (KTD3: never a transcript stamp).
    pub asked_at_ms: u64,
    pub payload: AskPayload,
}

/// What became of an [`AskRegistry::answer`] attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerOutcome {
    /// The decision line was handed to the held connection; the entry is gone.
    Delivered,
    /// No held ask for the leaf, the stamp mismatched (a newer ask replaced
    /// it), or the connection died in the race — the caller 409s; a local
    /// answer already won or will win.
    Gone,
}

struct Entry {
    generation: u64,
    asked_at_ms: u64,
    payload: AskPayload,
    tx: Sender<String>,
}

#[derive(Default)]
pub struct AskRegistry {
    inner: Mutex<HashMap<String, Entry>>,
    next_gen: AtomicU64,
}

impl AskRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a held ask for `leaf_key` (last-write-wins per leaf, KTD2).
    /// Returns the entry's generation (for [`clear_if`](Self::clear_if)) and
    /// the receiver the held connection parks on; `None` declines (cap hit by
    /// a new leaf). Replacing an entry drops the old sender, which releases
    /// the old connection (its hook exits quietly).
    pub fn register(
        &self,
        leaf_key: &str,
        payload: AskPayload,
        now_ms: u64,
    ) -> Option<(u64, Receiver<String>)> {
        let mut inner = self.inner.lock().unwrap();
        if inner.len() >= MAX_HELD_ASKS && !inner.contains_key(leaf_key) {
            return None;
        }
        let generation = self.next_gen.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::channel();
        inner.insert(
            leaf_key.to_string(),
            Entry {
                generation,
                asked_at_ms: now_ms,
                payload,
                tx,
            },
        );
        Some((generation, rx))
    }

    /// Remove the leaf's entry only if it still holds `generation` (the drop guard —
    /// a stale connection's death never clears a newer ask). Returns whether
    /// an entry was actually removed (the caller bumps the feed only then).
    pub fn clear_if(&self, leaf_key: &str, generation: u64) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if inner.get(leaf_key).is_some_and(|e| e.generation == generation) {
            inner.remove(leaf_key);
            return true;
        }
        false
    }

    /// The leaf's held ask, if any (the resolver's U6 read).
    pub fn get(&self, leaf_key: &str) -> Option<HeldAsk> {
        self.inner.lock().unwrap().get(leaf_key).map(|e| HeldAsk {
            asked_at_ms: e.asked_at_ms,
            payload: e.payload.clone(),
        })
    }

    /// Answer the leaf's held ask (hook-ask-channel R6/R7): the stamp must
    /// match (`ifAskedAt` semantics — a replaced ask's stamp differs), the
    /// entry is removed and the decision line sent under one lock hold, and a
    /// send onto a dead connection (the local-answer race) reports [`Gone`]
    /// rather than delivering into the void.
    pub fn answer(&self, leaf_key: &str, if_asked_at: u64, allow: bool) -> AnswerOutcome {
        let mut inner = self.inner.lock().unwrap();
        match inner.get(leaf_key) {
            Some(e) if e.asked_at_ms == if_asked_at => {}
            _ => return AnswerOutcome::Gone,
        }
        let entry = inner.remove(leaf_key).expect("checked above");
        match entry.tx.send(ask_decision_line(allow)) {
            Ok(()) => AnswerOutcome::Delivered,
            Err(_) => AnswerOutcome::Gone,
        }
    }

    /// Release every held ask (R9, ordered shutdown): dropping the senders
    /// wakes each held connection, which closes without a decision — the hook
    /// exits quietly, the dialog proceeds normally.
    pub fn shutdown(&self) {
        self.inner.lock().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Duration;

    fn ask(tool: &str) -> AskPayload {
        AskPayload {
            tool: Some(tool.into()),
            ..Default::default()
        }
    }

    #[test]
    fn register_get_and_drop_guard_lifecycle() {
        let r = AskRegistry::new();
        let (generation, _rx) = r.register("leaf-1", ask("Bash"), 1_000).unwrap();
        let held = r.get("leaf-1").expect("held");
        assert_eq!(held.asked_at_ms, 1_000);
        assert_eq!(held.payload.tool.as_deref(), Some("Bash"));
        // The armed drop guard clears its own generation…
        assert!(r.clear_if("leaf-1", generation));
        assert_eq!(r.get("leaf-1"), None);
        // …and is idempotent once gone.
        assert!(!r.clear_if("leaf-1", generation));
    }

    #[test]
    fn replacement_wins_and_the_stale_guard_cannot_clear_it() {
        let r = AskRegistry::new();
        let (old_gen, old_rx) = r.register("leaf-1", ask("Bash"), 1_000).unwrap();
        let (_new_gen, _new_rx) = r.register("leaf-1", ask("Write"), 2_000).unwrap();
        // The old connection was released: its sender is gone.
        assert_eq!(
            old_rx.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Disconnected)
        );
        // The old connection's late death must not clear the new ask (KTD2).
        assert!(!r.clear_if("leaf-1", old_gen));
        assert_eq!(r.get("leaf-1").unwrap().asked_at_ms, 2_000);
    }

    #[test]
    fn answer_delivers_once_with_stamp_discipline() {
        let r = AskRegistry::new();
        let (_gen, rx) = r.register("leaf-1", ask("Bash"), 1_000).unwrap();
        // Wrong stamp (a newer ask's answer guard) → Gone, entry intact.
        assert_eq!(r.answer("leaf-1", 999, true), AnswerOutcome::Gone);
        assert!(r.get("leaf-1").is_some());
        // Matching stamp → the decision line arrives and the entry clears.
        assert_eq!(r.answer("leaf-1", 1_000, false), AnswerOutcome::Delivered);
        let line = rx.recv_timeout(Duration::from_millis(50)).unwrap();
        assert!(line.contains("\"behavior\":\"deny\""));
        assert_eq!(r.get("leaf-1"), None);
        // Second answer → Gone (the latch upstream also guards this).
        assert_eq!(r.answer("leaf-1", 1_000, true), AnswerOutcome::Gone);
    }

    #[test]
    fn answer_onto_a_dead_connection_reports_gone() {
        // The local-answer race: the conn thread died (receiver dropped)
        // before the registry heard about it.
        let r = AskRegistry::new();
        let (_gen, rx) = r.register("leaf-1", ask("Bash"), 1_000).unwrap();
        drop(rx);
        assert_eq!(r.answer("leaf-1", 1_000, true), AnswerOutcome::Gone);
        // The entry is consumed either way — the ask was over.
        assert_eq!(r.get("leaf-1"), None);
    }

    #[test]
    fn cap_declines_new_leaves_but_never_replacements() {
        let r = AskRegistry::new();
        let mut keep = Vec::new();
        for i in 0..MAX_HELD_ASKS {
            keep.push(r.register(&format!("leaf-{i}"), ask("Bash"), i as u64).unwrap());
        }
        assert!(r.register("leaf-overflow", ask("Bash"), 9_999).is_none());
        // A replacement for an existing leaf still lands at the cap.
        assert!(r.register("leaf-0", ask("Write"), 9_999).is_some());
        assert_eq!(r.get("leaf-0").unwrap().asked_at_ms, 9_999);
    }

    #[test]
    fn shutdown_releases_every_held_connection() {
        let r = AskRegistry::new();
        let (_g1, rx1) = r.register("leaf-1", ask("Bash"), 1).unwrap();
        let (_g2, rx2) = r.register("leaf-2", ask("Write"), 2).unwrap();
        r.shutdown();
        for rx in [rx1, rx2] {
            assert_eq!(
                rx.recv_timeout(Duration::from_millis(50)),
                Err(RecvTimeoutError::Disconnected)
            );
        }
        assert_eq!(r.get("leaf-1"), None);
    }
}
