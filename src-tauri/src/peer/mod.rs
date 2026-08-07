//! Agent peer messaging (agent-peer-messaging plan): the pure core behind the
//! `peer/list` (`fly agents`) and `peer/send` (`fly send`) socket ops.
//!
//! `dispatch_peer_op` is the AppHandle-free dispatch (U6, the
//! `lib.rs::dispatch_automation_op` shape): every dependency is injected
//! through [`PeerPorts`] so the gate *ordering* — the part whose failure
//! modes are silent and destructive — is a unit-testable fact. `lib.rs`
//! supplies the real implementations (PtyManager, FeedState, the shared
//! question resolver, `feed::drop::deliver_with_guards`).
//!
//! The send-gate order (KTD9), cheapest-first, roster-dependent gates behind
//! the explicit staleness check so consent is never read from a frozen
//! roster:
//! selfSend → tooLong → resolve target (unknownPane) → rosterStale →
//! notOptedIn → rateLimited → askPending → deliver
//! (paneChanged/notAgent/deliveryFailed/submitIncomplete from the guards).

pub mod compose;
pub mod list;
pub mod rate;

use crate::cli::peer::{PeerRequest, PeerResponse};
use crate::feed::drop::DropOutcome;
use crate::feed::wire::AgentEntry;

/// The injected dependencies of one dispatch (U6). All borrows so the caller
/// composes them from locals; every callback is invoked at most once per
/// dispatch except none.
pub struct PeerPorts<'a> {
    /// The server clock, epoch ms (staleness + rate refill).
    pub now_ms: u64,
    /// The pushed roster + its publish stamp, one snapshot (KTD3).
    pub roster: &'a dyn Fn() -> (Vec<AgentEntry>, Option<u64>),
    /// Pane id → its leaf key (the delivery address), None for a dead or
    /// unknown pane.
    pub leaf_for_pane: &'a dyn Fn(u64) -> Option<String>,
    /// The sender's rate bucket (R11): true grants one send.
    pub try_take_rate: &'a dyn Fn(u64, u64) -> bool,
    /// The wide question gate (KTD5): whether *any* pending question blocks
    /// this leaf — the drop route's own predicate, never a new one.
    pub ask_pending: &'a dyn Fn(&str) -> bool,
    /// Guarded delivery (KTD4): (expected pane id, leaf, composed text) →
    /// outcome, via `feed::drop::deliver_with_guards` with a no-op commit.
    pub deliver: &'a dyn Fn(u64, &str, &str) -> DropOutcome,
}

/// Route one authenticated `peer/*` request. `origin` is the token-resolved
/// pane (KTD2) — the wire payload never carries a sender.
pub fn dispatch_peer_op(origin: u64, req: &PeerRequest, ports: &PeerPorts) -> PeerResponse {
    match req.op.as_str() {
        "peer/list" => {
            let (agents, published_at) = (ports.roster)();
            PeerResponse::listed(list::build_list(
                &agents,
                published_at,
                ports.now_ms,
                origin,
            ))
        }
        "peer/send" => dispatch_send(origin, req, ports),
        _ => PeerResponse::err("unknownOp"),
    }
}

fn dispatch_send(origin: u64, req: &PeerRequest, ports: &PeerPorts) -> PeerResponse {
    let (Some(target), Some(raw)) = (req.pane, req.message.as_deref()) else {
        return PeerResponse::err("badRequest");
    };
    // 1. selfSend — before any port is touched (pinned by test).
    if target == origin {
        return PeerResponse::err("selfSend");
    }
    // 2. tooLong — cheap length check, no roster (compose re-checks; this
    //    keeps an oversize body from paying the roster/rate/resolver path).
    if raw.chars().count() > compose::PEER_MESSAGE_CAP {
        return PeerResponse::err("tooLong");
    }
    if raw.trim().is_empty() {
        return PeerResponse::err("badRequest");
    }
    // 3. Resolve the target to its delivery address.
    let Some(leaf) = (ports.leaf_for_pane)(target) else {
        return PeerResponse::err("unknownPane");
    };
    // 4. rosterStale — nothing roster-derived is trusted past a stale stamp
    //    (KTD9: the write path refuses where the read path merely marks).
    let (agents, published_at) = (ports.roster)();
    if list::is_stale(published_at, ports.now_ms) {
        return PeerResponse::err("rosterStale");
    }
    // 5. notOptedIn — consent (KTD6). A pane absent from the roster is not an
    //    agent and never becomes addressable (the feed's 404-authority rule).
    let opted_in = agents
        .iter()
        .find(|a| a.pane_id == Some(target))
        .is_some_and(|a| a.peer_opt_in);
    if !opted_in {
        return PeerResponse::err("notOptedIn");
    }
    // 6. rateLimited — before the expensive question resolution, so a
    //    spamming sender costs a map lookup, not a transcript walk (KTD8).
    if !(ports.try_take_rate)(origin, ports.now_ms) {
        return PeerResponse::err("rateLimited");
    }
    // 7. askPending — the wide gate (KTD5): any pending question refuses;
    //    `paste_payload`'s leading ESC would silently cancel a picker.
    if (ports.ask_pending)(&leaf) {
        return PeerResponse::err("askPending");
    }
    // Compose: sanitize → scrub → frame, sender identity from the roster row
    // of the *token-resolved* origin (KTD2/KTD7) — never the wire.
    let sender = sender_identity(origin, &agents);
    let text = match compose::compose_peer_message(&sender, raw) {
        Ok(t) => t,
        Err(compose::ComposeError::TooLong) => return PeerResponse::err("tooLong"),
        Err(compose::ComposeError::Empty) => return PeerResponse::err("badRequest"),
    };
    // 8. Guarded delivery (KTD4).
    match (ports.deliver)(target, &leaf, &text) {
        DropOutcome::Delivered => PeerResponse::delivered(),
        DropOutcome::UnknownPane => PeerResponse::err("unknownPane"),
        DropOutcome::PaneChanged => PeerResponse::err("paneChanged"),
        DropOutcome::NotAgent => PeerResponse::err("notAgent"),
        // The peer path's commit is a no-op (nothing to publish), so a commit
        // failure is unreachable — mapped anyway rather than panicking.
        DropOutcome::CommitFailed(e) | DropOutcome::PasteFailed(e) => {
            PeerResponse::err_detail("deliveryFailed", e)
        }
        DropOutcome::SubmitIncomplete(e) => PeerResponse::err_detail("submitIncomplete", e),
    }
}

/// The sender line's facts (KTD7): the origin pane's own roster row when it
/// has one (workspace/tab/cwd), else pane id alone — a bare shell running
/// `fly send` is a legal sender and degrades gracefully.
fn sender_identity(origin: u64, agents: &[AgentEntry]) -> compose::SenderIdentity {
    match agents.iter().find(|a| a.pane_id == Some(origin)) {
        Some(row) => compose::SenderIdentity {
            pane_id: origin,
            cwd: row.cwd.clone(),
            workspace: Some(row.workspace.clone()),
            tab: Some(row.tab.clone()),
        },
        None => compose::SenderIdentity {
            pane_id: origin,
            cwd: None,
            workspace: None,
            tab: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    fn agent(leaf: &str, pane: u64, opt_in: bool) -> AgentEntry {
        AgentEntry {
            leaf_key: leaf.into(),
            workspace: "home".into(),
            tab: "fly".into(),
            cwd: Some("/p".into()),
            status: "idle".into(),
            needs_attention: false,
            reason: None,
            working_for_ms: None,
            live_task_count: 0,
            num: None,
            last_reply_at: None,
            question_pending_at: None,
            pane_id: Some(pane),
            peer_opt_in: opt_in,
        }
    }

    /// A harness whose every port records/refuses per-case; unused ports
    /// panic so the gate order is pinned, not just the outcomes.
    struct Harness {
        roster: Vec<AgentEntry>,
        published_at: Option<u64>,
        now: u64,
        leaf: Option<String>,
        rate_ok: bool,
        ask: bool,
        outcome: RefCell<Option<DropOutcome>>,
        delivered_text: RefCell<Option<String>>,
        touched_roster: Cell<bool>,
        touched_rate: Cell<bool>,
        touched_ask: Cell<bool>,
    }

    impl Harness {
        fn new() -> Self {
            Harness {
                roster: vec![agent("l-sender", 7, false), agent("l-target", 12, true)],
                published_at: Some(59_000),
                now: 60_000,
                leaf: Some("l-target".into()),
                rate_ok: true,
                ask: false,
                outcome: RefCell::new(Some(DropOutcome::Delivered)),
                delivered_text: RefCell::new(None),
                touched_roster: Cell::new(false),
                touched_rate: Cell::new(false),
                touched_ask: Cell::new(false),
            }
        }

        fn send(&self, origin: u64, target: u64, message: &str) -> PeerResponse {
            let req = PeerRequest {
                token: String::new(),
                op: "peer/send".into(),
                pane: Some(target),
                message: Some(message.into()),
            };
            let roster = || {
                self.touched_roster.set(true);
                (self.roster.clone(), self.published_at)
            };
            let leaf_for_pane = |_p: u64| self.leaf.clone();
            let try_take = |_pane: u64, _now: u64| {
                self.touched_rate.set(true);
                self.rate_ok
            };
            let ask = |_leaf: &str| {
                self.touched_ask.set(true);
                self.ask
            };
            let deliver = |_expect: u64, _leaf: &str, text: &str| {
                *self.delivered_text.borrow_mut() = Some(text.to_string());
                self.outcome
                    .borrow_mut()
                    .take()
                    .expect("deliver invoked at most once")
            };
            dispatch_peer_op(
                origin,
                &req,
                &PeerPorts {
                    now_ms: self.now,
                    roster: &roster,
                    leaf_for_pane: &leaf_for_pane,
                    try_take_rate: &try_take,
                    ask_pending: &ask,
                    deliver: &deliver,
                },
            )
        }
    }

    fn code(resp: &PeerResponse) -> &str {
        resp.error.as_deref().unwrap_or("")
    }

    #[test]
    fn happy_path_delivers_the_framed_text() {
        let h = Harness::new();
        let resp = h.send(7, 12, "results are in /tmp/out.json");
        assert!(resp.ok, "{resp:?}");
        let text = h.delivered_text.borrow().clone().unwrap();
        assert!(text.contains("From pane 7"));
        assert!(text.contains("results are in /tmp/out.json"));
        assert!(text.contains("UNTRUSTED"));
    }

    #[test]
    fn self_send_refuses_before_any_port_runs() {
        let h = Harness::new();
        let resp = h.send(12, 12, "hi");
        assert_eq!(code(&resp), "selfSend");
        assert!(!h.touched_roster.get(), "roster untouched");
        assert!(!h.touched_rate.get(), "rate untouched");
        assert!(!h.touched_ask.get(), "resolver untouched");
    }

    #[test]
    fn oversize_refuses_before_the_roster_is_read() {
        let h = Harness::new();
        let big = "x".repeat(compose::PEER_MESSAGE_CAP + 1);
        assert_eq!(code(&h.send(7, 12, &big)), "tooLong");
        assert!(!h.touched_roster.get());
    }

    #[test]
    fn unknown_pane_refuses_when_no_leaf_resolves() {
        let mut h = Harness::new();
        h.leaf = None;
        assert_eq!(code(&h.send(7, 12, "hi")), "unknownPane");
    }

    #[test]
    fn stale_roster_refuses_even_when_the_target_is_opted_in() {
        let mut h = Harness::new();
        h.published_at = Some(h.now - list::STALE_AFTER_MS - 1);
        assert_eq!(code(&h.send(7, 12, "hi")), "rosterStale");
        assert!(!h.touched_rate.get(), "no rate spend on a stale roster");
        // Never-pushed is equally stale.
        let mut h2 = Harness::new();
        h2.published_at = None;
        assert_eq!(code(&h2.send(7, 12, "hi")), "rosterStale");
    }

    #[test]
    fn missing_opt_in_and_missing_roster_row_both_refuse_not_opted_in() {
        let mut h = Harness::new();
        h.roster = vec![agent("l-sender", 7, false), agent("l-target", 12, false)];
        assert_eq!(code(&h.send(7, 12, "hi")), "notOptedIn");
        // Target absent from the roster entirely (a non-agent pane): same
        // refusal — never addressable.
        let mut h2 = Harness::new();
        h2.roster = vec![agent("l-sender", 7, false)];
        assert_eq!(code(&h2.send(7, 12, "hi")), "notOptedIn");
    }

    #[test]
    fn rate_refusal_never_reaches_the_question_resolver() {
        let mut h = Harness::new();
        h.rate_ok = false;
        assert_eq!(code(&h.send(7, 12, "hi")), "rateLimited");
        assert!(h.touched_rate.get());
        assert!(!h.touched_ask.get(), "spam costs no transcript walk (KTD8)");
    }

    #[test]
    fn any_pending_question_refuses_ask_pending() {
        let mut h = Harness::new();
        h.ask = true;
        assert_eq!(code(&h.send(7, 12, "hi")), "askPending");
        assert!(
            h.delivered_text.borrow().is_none(),
            "nothing reached delivery"
        );
    }

    #[test]
    fn guard_outcomes_map_to_their_codes() {
        for (outcome, expect) in [
            (DropOutcome::UnknownPane, "unknownPane"),
            (DropOutcome::PaneChanged, "paneChanged"),
            (DropOutcome::NotAgent, "notAgent"),
            (DropOutcome::PasteFailed("w".into()), "deliveryFailed"),
            (
                DropOutcome::SubmitIncomplete("e".into()),
                "submitIncomplete",
            ),
        ] {
            let h = Harness::new();
            *h.outcome.borrow_mut() = Some(outcome);
            assert_eq!(code(&h.send(7, 12, "hi")), expect);
        }
    }

    #[test]
    fn sender_identity_comes_from_the_origin_roster_row_or_degrades() {
        let h = Harness::new();
        let _ = h.send(7, 12, "hi");
        let text = h.delivered_text.borrow().clone().unwrap();
        assert!(text.contains("workspace \"home\""), "roster-derived identity");
        // An origin with no roster row (bare shell) degrades to pane id only.
        let h2 = Harness::new();
        let _ = h2.send(99, 12, "hi");
        let text2 = h2.delivered_text.borrow().clone().unwrap();
        assert!(text2.contains("From pane 99"));
        assert!(text2.contains("another process in this fly session"));
    }

    #[test]
    fn bad_requests_are_refused() {
        let h = Harness::new();
        // Missing pane / missing message / blank message / unknown op.
        let req = PeerRequest {
            op: "peer/send".into(),
            ..Default::default()
        };
        let ports_panic_roster = || panic!("untouched");
        let ports = PeerPorts {
            now_ms: 0,
            roster: &ports_panic_roster,
            leaf_for_pane: &|_| panic!("untouched"),
            try_take_rate: &|_, _| panic!("untouched"),
            ask_pending: &|_| panic!("untouched"),
            deliver: &|_, _, _| panic!("untouched"),
        };
        assert_eq!(code(&dispatch_peer_op(7, &req, &ports)), "badRequest");
        assert_eq!(code(&h.send(7, 12, "   ")), "badRequest");
        let unknown = PeerRequest {
            op: "peer/bogus".into(),
            ..Default::default()
        };
        assert_eq!(code(&dispatch_peer_op(7, &unknown, &ports)), "unknownOp");
    }

    #[test]
    fn list_marks_self_and_serves_stale_loudly_instead_of_refusing() {
        let h = Harness::new();
        let req = PeerRequest {
            op: "peer/list".into(),
            ..Default::default()
        };
        let roster = || (h.roster.clone(), Some(h.now - list::STALE_AFTER_MS - 1));
        let ports = PeerPorts {
            now_ms: h.now,
            roster: &roster,
            leaf_for_pane: &|_| panic!("list never resolves panes"),
            try_take_rate: &|_, _| panic!("list is not rate limited"),
            ask_pending: &|_| panic!("list never resolves questions"),
            deliver: &|_, _, _| panic!("list never delivers"),
        };
        let resp = dispatch_peer_op(7, &req, &ports);
        assert!(resp.ok);
        let list = resp.list.unwrap();
        assert!(list.stale, "a stale read serves marked, not refused (KTD9)");
        assert!(list.agents.iter().any(|a| a.is_self && a.pane_id == Some(7)));
    }
}
