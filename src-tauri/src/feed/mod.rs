//! The local agent/automation feed (feat-agent-state-local-feed) plus its
//! per-agent reply/input endpoints (feed-agent-reply-io).
//!
//! A narrowly-scoped, loopback-only realization of the browser-reachable HTTP
//! endpoint the hook socket deliberately deferred (`hooks/CLAUDE.md`, KTD7). It
//! lets an external local consumer (the `game` portfolio) *see* what agents are
//! running and what automations exist, read an agent's latest reply
//! (`GET /agents/{key}/output`), and submit a prompt back to it
//! (`POST /agents/{key}/input`) — the one deliberate mutation route, equivalent
//! to typing into the pane (feed-agent-reply-io KTD3; see `server`).
//!
//! Data flow (see the plan's High-Level Technical Design): the webview PUSHES
//! its assembled agent roster (`publish`, U2), the backend merges it with
//! automations from the authoritative store and streams the result over SSE
//! (`server`, U3). The `wire` module is the single source of truth for the
//! boundary shape, mirrored by `src/lib/feed.ts`. The `io` module resolves a
//! leaf's latest reply and builds the injected input payload.

pub mod ask;
pub mod drop;
pub mod fallback;
pub mod io;
pub mod pending;
pub mod screen;
pub mod server;
pub mod wire;

pub use server::FeedServer;

use std::sync::{Condvar, Mutex};
use std::time::Duration;

use wire::{AgentEntry, AutomationEntry, FeedSnapshot};

/// The backend cache the webview pushes into and the SSE server reads from
/// (KTD1/KTD5). Holds the latest pushed agent roster plus a monotonic `version`
/// that bumps on any change — a roster push that actually differs, or an
/// automation-change [`bump`](FeedState::bump). SSE reader threads block on the
/// `Condvar` until the version moves (or shutdown), so an idle feed costs
/// nothing and a change wakes every connected consumer exactly once.
pub struct FeedState {
    inner: Mutex<Inner>,
    changed: Condvar,
}

struct Inner {
    agents: Vec<AgentEntry>,
    version: u64,
    /// Epoch ms of the last [`FeedState::publish`] call, regardless of whether
    /// it changed anything (phone-screenshot-drop U4). See
    /// [`FeedSnapshot::published_at`].
    published_at: Option<u64>,
    shutting_down: bool,
}

/// The outcome of a blocking [`FeedState::wait_for_change`]: the version now in
/// effect and whether the state is tearing down (so the reader thread exits).
pub struct WaitResult {
    pub version: u64,
    pub shutting_down: bool,
}

/// One published agent's gate-relevant roster fields, read in a single
/// snapshot by [`FeedState::agent_gate`]: the live attention `reason` (the
/// permission-exposure gate) and the dashboard `status` (the fallback's
/// working-pane suppressor).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentGate {
    pub reason: Option<String>,
    pub status: String,
}

impl Default for FeedState {
    fn default() -> Self {
        Self::new()
    }
}

impl FeedState {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                agents: Vec::new(),
                version: 0,
                published_at: None,
                shutting_down: false,
            }),
            changed: Condvar::new(),
        }
    }

    /// Replace the cached roster. Bumps `version` and wakes readers **only when
    /// the roster actually differs** (KTD5) — an idle agent re-published every
    /// poll tick must not churn the stream. Returns whether it changed.
    ///
    /// `published_at` (epoch ms) is recorded on **every** call, including the
    /// unchanged-roster early return that skips the version bump. That
    /// asymmetry is deliberate and is the whole liveness signal
    /// (phone-screenshot-drop U4, KTD6): an idle-but-live webview publishes an
    /// identical roster every poll tick, and only a stamp that advances without
    /// a content change distinguishes it from a webview that has stopped
    /// publishing altogether. Time is passed in rather than read here, matching
    /// the injected-clock discipline the rest of this module keeps.
    pub fn publish(&self, agents: Vec<AgentEntry>, published_at: u64) -> bool {
        let mut inner = self.inner.lock().unwrap();
        inner.published_at = Some(published_at);
        if inner.agents == agents {
            return false;
        }
        inner.agents = agents;
        inner.version += 1;
        drop(inner);
        self.changed.notify_all();
        true
    }

    /// Force a version bump + wake without changing the roster — used when the
    /// *automations* half of the snapshot changed (the `automation://changed`
    /// subscription, KTD4), since those are assembled at emit time.
    pub fn bump(&self) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.version += 1;
        }
        self.changed.notify_all();
    }

    /// The version currently in effect (an SSE reader records this after each
    /// emit, then waits for it to move).
    pub fn current_version(&self) -> u64 {
        self.inner.lock().unwrap().version
    }

    /// Existence **and** the gate-relevant roster fields in one lock
    /// acquisition — the key authority for the per-agent endpoints
    /// (feed-agent-reply-io KTD2; feed-pending-question U4/KTD4): `None` =
    /// unknown agent (a key the feed has never served is a 404, and a
    /// non-agent pane never becomes remotely addressable); `Some(gate)` =
    /// published, carrying the entry's pushed attention `reason`
    /// (`Some("permission")` while a permission dialog holds the pane raised,
    /// `None` otherwise — an *acknowledged* raise clears it) and its `status`
    /// (the screen fallback declines to engage on a `working` pane). The
    /// permission gate must read existence, reason, and status from one
    /// roster snapshot — separate reads could straddle a roster swap and gate
    /// on another moment's state.
    pub fn agent_gate(&self, leaf_key: &str) -> Option<AgentGate> {
        self.inner
            .lock()
            .unwrap()
            .agents
            .iter()
            .find(|a| a.leaf_key == leaf_key)
            .map(|a| AgentGate {
                reason: a.reason.clone(),
                status: a.status.clone(),
            })
    }

    /// The pushed roster and its publish stamp in one lock acquisition
    /// (agent-peer-messaging U2/KTD3): what the `peer/list` op serves and the
    /// `peer/send` consent/staleness gates read. One snapshot, so the opt-in
    /// bit and the stamp that vouches for it can't straddle a roster swap.
    pub fn roster(&self) -> (Vec<AgentEntry>, Option<u64>) {
        let inner = self.inner.lock().unwrap();
        (inner.agents.clone(), inner.published_at)
    }

    /// Assemble the frame to emit: the cached roster + the caller-supplied
    /// automations (read from the store at emit time, KTD4) + stamp. Kept free
    /// of any store/manager dependency so it is unit-tested in isolation.
    pub fn snapshot(&self, automations: Vec<AutomationEntry>, emitted_at: u64) -> FeedSnapshot {
        let inner = self.inner.lock().unwrap();
        FeedSnapshot {
            version: inner.version,
            emitted_at,
            published_at: inner.published_at,
            agents: inner.agents.clone(),
            automations,
        }
    }

    /// Block until the version moves past `last_seen` (a change) or the state
    /// tears down, whichever comes first; `timeout` bounds the wait so the
    /// reader can send an SSE keepalive and re-check its socket. Returns the
    /// version now in effect and the shutdown flag.
    pub fn wait_for_change(&self, last_seen: u64, timeout: Duration) -> WaitResult {
        let inner = self.inner.lock().unwrap();
        let (inner, _timed_out) = self
            .changed
            .wait_timeout_while(inner, timeout, |i| {
                i.version <= last_seen && !i.shutting_down
            })
            .unwrap();
        WaitResult {
            version: inner.version,
            shutting_down: inner.shutting_down,
        }
    }

    /// Signal teardown: wake every blocked reader so it observes
    /// `shutting_down` and exits (called from the ordered shutdown, U6).
    pub fn shutdown(&self) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.shutting_down = true;
        }
        self.changed.notify_all();
    }

    pub fn is_shutting_down(&self) -> bool {
        self.inner.lock().unwrap().shutting_down
    }
}

/// Epoch ms, saturating at 0 if the clock is before the epoch.
///
/// The publish stamp is taken **here, at receipt**, rather than being carried in
/// the payload from the webview. Both detect a frozen webview identically (a
/// webview that has stopped publishing stops advancing the stamp either way),
/// and stamping backend-side avoids trusting a client-supplied clock for a value
/// the drop route's staleness gate depends on.
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn agent(leaf: &str, status: &str) -> AgentEntry {
        AgentEntry {
            leaf_key: leaf.into(),
            workspace: "home".into(),
            tab: "fly".into(),
            cwd: None,
            status: status.into(),
            needs_attention: false,
            reason: None,
            working_for_ms: None,
            live_task_count: 0,
            num: None,
            last_reply_at: None,
            question_pending_at: None,
            pane_id: None,
            peer_opt_in: false,
        }
    }

    #[test]
    fn agent_gate_tracks_existence_reason_and_status_across_the_roster() {
        // feed-pending-question U4 (the sole 404-authority + gate read): None =
        // unknown (404); Some with no reason = published, unraised; Some with
        // a reason = published + raised. Status rides the same snapshot (the
        // fallback's working-pane suppressor). A gone agent is unknown again.
        let s = FeedState::new();
        assert_eq!(s.agent_gate("l1"), None);
        s.publish(vec![agent("l1", "working")], 0);
        assert_eq!(
            s.agent_gate("l1"),
            Some(AgentGate {
                reason: None,
                status: "working".into()
            })
        );
        assert_eq!(s.agent_gate("l2"), None);
        let mut raised = agent("l2", "waiting");
        raised.reason = Some("permission".into());
        s.publish(vec![raised], 0);
        assert_eq!(
            s.agent_gate("l2"),
            Some(AgentGate {
                reason: Some("permission".into()),
                status: "waiting".into()
            })
        );
        assert_eq!(s.agent_gate("l1"), None, "gone from the roster again");
        s.publish(vec![], 0);
        assert_eq!(s.agent_gate("l2"), None, "empty roster → unknown");
    }

    #[test]
    fn publish_bumps_version_only_on_change() {
        let s = FeedState::new();
        assert_eq!(s.current_version(), 0);

        assert!(s.publish(vec![agent("l1", "working")], 0));
        assert_eq!(s.current_version(), 1);

        // Identical roster → no bump (KTD5).
        assert!(!s.publish(vec![agent("l1", "working")], 0));
        assert_eq!(s.current_version(), 1);

        // A real change → bump.
        assert!(s.publish(vec![agent("l1", "idle")], 0));
        assert_eq!(s.current_version(), 2);
    }

    #[test]
    fn snapshot_merges_agents_and_passed_automations() {
        let s = FeedState::new();
        s.publish(vec![agent("l1", "working")], 0);
        let autos = vec![AutomationEntry {
            id: "a1".into(),
            name: "n".into(),
            cron: "* * * * *".into(),
            timezone: "UTC".into(),
            enabled: true,
            monitor: false,
            headless: false,
            next_run_at: None,
            last_status: None,
            last_run_at: None,
            retired_at: None,
            last_verdict: None,
            after: None,
            last_withheld_reason: None,
        }];
        let snap = s.snapshot(autos, 123);
        assert_eq!(snap.version, 1);
        assert_eq!(snap.emitted_at, 123);
        assert_eq!(snap.agents.len(), 1);
        assert_eq!(snap.automations.len(), 1);
    }

    /// The liveness signal behind KTD6. An idle-but-live webview republishes an
    /// identical roster forever, which deliberately does *not* bump `version` —
    /// so if `publishedAt` only advanced on change, a healthy idle app would be
    /// indistinguishable from a frozen one and every drop would be refused
    /// `paneChanged` with no way to diagnose it.
    #[test]
    fn published_at_advances_on_every_publish_even_without_a_roster_change() {
        let s = FeedState::new();
        assert_eq!(s.snapshot(vec![], 0).published_at, None, "none before any push");

        assert!(s.publish(vec![agent("l1", "working")], 1_000));
        assert_eq!(s.snapshot(vec![], 0).published_at, Some(1_000));

        // Identical roster: no version bump...
        assert!(!s.publish(vec![agent("l1", "working")], 2_000));
        assert_eq!(s.current_version(), 1, "version stayed put");
        // ...but the liveness stamp still moved.
        assert_eq!(s.snapshot(vec![], 0).published_at, Some(2_000));
    }

    /// `publishedAt` describes the *push*; `emittedAt` describes the *frame*.
    /// Two frames emitted from one push share the former and differ in the
    /// latter — which is exactly what lets a consumer tell "the backend is
    /// alive" apart from "the roster is current".
    #[test]
    fn published_at_is_per_push_while_emitted_at_is_per_frame() {
        let s = FeedState::new();
        s.publish(vec![agent("l1", "working")], 500);
        let first = s.snapshot(vec![], 10_000);
        let second = s.snapshot(vec![], 20_000);
        assert_eq!(first.published_at, second.published_at);
        assert_eq!(first.published_at, Some(500));
        assert_ne!(first.emitted_at, second.emitted_at);
    }

    #[test]
    fn bump_forces_a_version_move_without_roster_change() {
        let s = FeedState::new();
        s.publish(vec![agent("l1", "working")], 0);
        assert_eq!(s.current_version(), 1);
        s.bump();
        assert_eq!(s.current_version(), 2);
    }

    #[test]
    fn wait_for_change_wakes_on_publish() {
        let s = Arc::new(FeedState::new());
        let s2 = Arc::clone(&s);
        let handle = std::thread::spawn(move || {
            // Blocks until version passes 0.
            s2.wait_for_change(0, Duration::from_secs(5))
        });
        // Give the reader a moment to park, then publish.
        std::thread::sleep(Duration::from_millis(50));
        s.publish(vec![agent("l1", "working")], 0);
        let res = handle.join().unwrap();
        assert_eq!(res.version, 1);
        assert!(!res.shutting_down);
    }

    #[test]
    fn wait_for_change_wakes_on_shutdown() {
        let s = Arc::new(FeedState::new());
        let s2 = Arc::clone(&s);
        let handle = std::thread::spawn(move || s2.wait_for_change(0, Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(50));
        s.shutdown();
        let res = handle.join().unwrap();
        assert!(res.shutting_down);
    }
}
