//! The local, read-only agent/automation feed (feat-agent-state-local-feed).
//!
//! A narrowly-scoped, loopback-only realization of the browser-reachable HTTP
//! endpoint the hook socket deliberately deferred (`hooks/CLAUDE.md`, KTD7). It
//! lets an external local consumer (the `game` portfolio) *see* what agents are
//! running and what automations exist — never act on them.
//!
//! Data flow (see the plan's High-Level Technical Design): the webview PUSHES
//! its assembled agent roster (`publish`, U2), the backend merges it with
//! automations from the authoritative store and streams the result over SSE
//! (`server`, U3). The `wire` module is the single source of truth for the
//! boundary shape, mirrored by `src/lib/feed.ts`.

pub mod wire;

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use wire::{AgentEntry, AutomationEntry, FeedPublishPayload, FeedSnapshot};

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
    shutting_down: bool,
}

/// The outcome of a blocking [`FeedState::wait_for_change`]: the version now in
/// effect and whether the state is tearing down (so the reader thread exits).
pub struct WaitResult {
    pub version: u64,
    pub shutting_down: bool,
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
                shutting_down: false,
            }),
            changed: Condvar::new(),
        }
    }

    /// Replace the cached roster. Bumps `version` and wakes readers **only when
    /// the roster actually differs** (KTD5) — an idle agent re-published every
    /// poll tick must not churn the stream. Returns whether it changed.
    pub fn publish(&self, agents: Vec<AgentEntry>) -> bool {
        let mut inner = self.inner.lock().unwrap();
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

    /// Assemble the frame to emit: the cached roster + the caller-supplied
    /// automations (read from the store at emit time, KTD4) + stamp. Kept free
    /// of any store/manager dependency so it is unit-tested in isolation.
    pub fn snapshot(&self, automations: Vec<AutomationEntry>, emitted_at: u64) -> FeedSnapshot {
        let inner = self.inner.lock().unwrap();
        FeedSnapshot {
            version: inner.version,
            emitted_at,
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

/// Command: the webview pushes its assembled agent roster here each poll (U5).
/// Always available (managed even when the feed listener is disabled), so the
/// publisher never errors on a disabled feed — the roster is simply cached and
/// never served.
#[tauri::command]
pub fn publish_agent_feed(
    payload: FeedPublishPayload,
    state: tauri::State<'_, Arc<FeedState>>,
) -> bool {
    state.publish(payload.agents)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        }
    }

    #[test]
    fn publish_bumps_version_only_on_change() {
        let s = FeedState::new();
        assert_eq!(s.current_version(), 0);

        assert!(s.publish(vec![agent("l1", "working")]));
        assert_eq!(s.current_version(), 1);

        // Identical roster → no bump (KTD5).
        assert!(!s.publish(vec![agent("l1", "working")]));
        assert_eq!(s.current_version(), 1);

        // A real change → bump.
        assert!(s.publish(vec![agent("l1", "idle")]));
        assert_eq!(s.current_version(), 2);
    }

    #[test]
    fn snapshot_merges_agents_and_passed_automations() {
        let s = FeedState::new();
        s.publish(vec![agent("l1", "working")]);
        let autos = vec![AutomationEntry {
            id: "a1".into(),
            name: "n".into(),
            cron: "* * * * *".into(),
            timezone: "UTC".into(),
            enabled: true,
            next_run_at: None,
            last_status: None,
            last_run_at: None,
        }];
        let snap = s.snapshot(autos, 123);
        assert_eq!(snap.version, 1);
        assert_eq!(snap.emitted_at, 123);
        assert_eq!(snap.agents.len(), 1);
        assert_eq!(snap.automations.len(), 1);
    }

    #[test]
    fn bump_forces_a_version_move_without_roster_change() {
        let s = FeedState::new();
        s.publish(vec![agent("l1", "working")]);
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
        s.publish(vec![agent("l1", "working")]);
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
