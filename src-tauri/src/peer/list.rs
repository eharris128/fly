//! The `peer/list` projection (agent-peer-messaging U2, R1/R2/KTD3): the
//! pushed roster → the CLI's rows, with staleness derived as data.
//!
//! Serves the roster **as published** — this module never reaches for the
//! `PtyManager` or judges freshness on the caller's behalf. A *read* may
//! serve stale data loudly (the stamp + `stale` flag ride the payload and the
//! CLI prints the banner); only the *write* path refuses to act on it
//! (KTD9's read/write asymmetry).

use crate::cli::peer::{PeerAgentRow, PeerListPayload};
use crate::feed::wire::AgentEntry;

/// Roster publish age beyond which it is marked/treated stale. The publisher
/// cadence is ~1.5s (webview poll, worker-timer-backed even hidden), so ~7
/// missed ticks is decisively wedged, not jittery.
pub const STALE_AFTER_MS: u64 = 10_000;

/// The one staleness predicate, shared by the list projection (marking) and
/// the send gate (refusing — KTD9 step 4). Never-pushed is stale.
pub fn is_stale(published_at: Option<u64>, now_ms: u64) -> bool {
    match published_at {
        Some(p) => now_ms.saturating_sub(p) > STALE_AFTER_MS,
        None => true,
    }
}

/// Project the pushed roster into the list payload. `origin_pane` marks the
/// caller's own row (`isSelf`) so an agent can find itself in the listing.
pub fn build_list(
    agents: &[AgentEntry],
    published_at: Option<u64>,
    now_ms: u64,
    origin_pane: u64,
) -> PeerListPayload {
    PeerListPayload {
        agents: agents
            .iter()
            .map(|a| PeerAgentRow {
                pane_id: a.pane_id,
                workspace: a.workspace.clone(),
                tab: a.tab.clone(),
                cwd: a.cwd.clone(),
                status: a.status.clone(),
                working_for_ms: a.working_for_ms,
                peer_opt_in: a.peer_opt_in,
                is_self: a.pane_id == Some(origin_pane),
            })
            .collect(),
        published_at,
        now: now_ms,
        stale: is_stale(published_at, now_ms),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(leaf: &str, pane: Option<u64>, opt_in: bool) -> AgentEntry {
        AgentEntry {
            leaf_key: leaf.into(),
            workspace: "home".into(),
            tab: "fly".into(),
            cwd: Some("/p".into()),
            status: "idle".into(),
            needs_attention: false,
            reason: None,
            working_for_ms: Some(1234.0),
            live_task_count: 0,
            num: None,
            last_reply_at: None,
            question_pending_at: None,
            pane_id: pane,
            peer_opt_in: opt_in,
        }
    }

    #[test]
    fn projection_carries_every_column_and_marks_self() {
        let roster = vec![agent("l1", Some(7), true), agent("l2", None, false)];
        let list = build_list(&roster, Some(1_000), 2_000, 7);
        assert_eq!(list.agents.len(), 2);
        let me = &list.agents[0];
        assert!(me.is_self && me.peer_opt_in);
        assert_eq!(me.pane_id, Some(7));
        assert_eq!(me.working_for_ms, Some(1234.0));
        let other = &list.agents[1];
        assert!(!other.is_self && !other.peer_opt_in);
        assert_eq!(other.pane_id, None, "unassigned pane rides as null");
        assert!(!list.stale);
        assert_eq!(list.published_at, Some(1_000));
        assert_eq!(list.now, 2_000);
    }

    // R2: staleness is a threshold on the publish stamp, and never-pushed is
    // stale rather than fresh.
    #[test]
    fn staleness_is_threshold_and_never_pushed() {
        assert!(!is_stale(Some(0), STALE_AFTER_MS), "at threshold: fresh");
        assert!(is_stale(Some(0), STALE_AFTER_MS + 1), "past it: stale");
        assert!(is_stale(None, 0), "never pushed is stale, not fresh");
        let list = build_list(&[], None, 5, 1);
        assert!(list.stale);
    }
}
