//! The automations usage gate — "should a scheduled agent run dispatch right
//! now, and if not, until when?" (usage-limit-deferral plan, U2).
//!
//! [`defer_floor_ms`] is the pure predicate (R2/KTD6/KTD7): given a
//! [`UsageSnapshot`] it answers `Some(floor_ms)` only when an overall plan
//! window (`session` / `weekly_all`) reads ≥100% with a valid future
//! `resets_at` — and never while an overage window is actively billing
//! (usage continues on the meter, not this gate's call to make). Every other
//! shape — sub-limit windows, per-model (`weekly_scoped`) windows, unknown
//! kinds, a missing/stale/absurd `resets_at` — is `None` = dispatch normally.
//! **Fail-open is the module's contract (KTD3)**: the gate may only ever
//! delay a run; a wrongly-open gate costs one wasted dispatch (yesterday's
//! behavior), a wrongly-closed gate would silently starve a schedule.
//!
//! [`OauthUsageGate`] is the impure shell the sweep injects (as an
//! [`crate::automations::UsageGate`] closure, wired in `lib.rs`): a blocking
//! short-timeout fetch through the module's shared request core
//! ([`super::fetch_snapshot`], KTD5 — one code path with the dashboard, so
//! auth/headers/parse can't drift) plus a small TTL cache. It fetches only
//! when consulted, and the sweep consults it only on a tick that could
//! actually claim an agent-mode occurrence — so KTD-C of the dashboard plan
//! ("never on a timer") still holds subsystem-wide.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::{fetch_snapshot, UsageSnapshot};
use crate::session::transcript::iso8601_to_ms;

/// Round-trip budget for a gate fetch. Deliberately far tighter than the
/// dashboard's 30s: the fetch runs on the sweep thread (off the store lock,
/// KTD2/KTD-B), so a stalled endpoint may delay one tick's dispatch phase but
/// never a lock hold — and a timeout simply fails open.
const GATE_TIMEOUT: Duration = Duration::from_secs(5);

/// Snapshot reuse window — belt-and-suspenders against repeated due ticks
/// (the sweep ticks every 10s). Deferral itself makes consultations rare:
/// once deferred, nothing is due until the reset. Fetch *errors* are cached
/// for the same TTL so a dead endpoint isn't hammered (fail-open means the
/// caller dispatches either way).
const GATE_CACHE_TTL: Duration = Duration::from_secs(60);

/// Ceiling on how far a `resets_at` may defer (KTD7): the widest plan window
/// is 7 days, so anything past ~8 days is a shape we don't trust — fail open.
/// `resets_at` is untrusted remote input; release builds compile with
/// `overflow-checks = false`, hence the saturating comparison below and the
/// checked parse in [`iso8601_to_ms`].
const MAX_DEFER_MS: u64 = 8 * 24 * 60 * 60 * 1000;

/// Window kinds that gate dispatch (KTD6): the overall session (5h) and
/// weekly (7d) windows. `weekly_scoped` (per-model) deliberately does not
/// gate — matching an automation's resolved model against a scope label
/// doesn't earn its complexity yet (plan: Out of scope).
const GATING_KINDS: [&str; 2] = ["session", "weekly_all"];

/// The pure at-limit predicate (R2): `Some(floor_ms)` when dispatch should
/// defer until `floor_ms`, `None` when it should proceed. The floor is the
/// **latest** qualifying `resets_at` — every exhausted window must reset
/// before a run can do useful work.
pub fn defer_floor_ms(snap: &UsageSnapshot, now_ms: u64) -> Option<u64> {
    // An actively-billing overage window means hitting 100% doesn't block
    // usage — it starts costing extra. Whether to spend is the user's call
    // (they enabled overage), not this gate's: open (KTD6).
    if snap
        .limits
        .iter()
        .any(|l| l.kind.as_deref() == Some("overage") && l.is_active)
    {
        return None;
    }
    snap.limits
        .iter()
        .filter(|l| l.kind.as_deref().is_some_and(|k| GATING_KINDS.contains(&k)))
        .filter(|l| l.percent >= 100.0)
        .filter_map(|l| iso8601_to_ms(l.resets_at.as_deref()?))
        // KTD7: a stale (past) or absurdly-far reset is a shape we don't
        // trust, not a deferral.
        .filter(|&r| r > now_ms && r.saturating_sub(now_ms) <= MAX_DEFER_MS)
        .max()
}

/// The impure shell: shared request core + TTL cache. Blocking by design —
/// the sweep is a plain thread in a deliberately non-async crate, so the
/// async core runs to completion on the crate's lazily-built runtime
/// (`usage::block_on`, 2026-08-27-001 KTD5).
pub struct OauthUsageGate {
    /// Last fetch outcome (`None` = it failed) and when it landed.
    cache: Mutex<Option<(Instant, Option<UsageSnapshot>)>>,
}

impl OauthUsageGate {
    pub fn new() -> Self {
        Self {
            cache: Mutex::new(None),
        }
    }

    /// The sweep-facing verdict (the [`crate::automations::UsageGate`]
    /// contract): fail-open on every fetch failure, else the pure predicate.
    pub fn defer_floor(&self, now_ms: u64) -> Option<u64> {
        self.snapshot()
            .and_then(|snap| defer_floor_ms(&snap, now_ms))
    }

    /// The cached-or-fresh snapshot; `None` when the freshest fetch failed.
    /// The cache lock is held across the fetch — only the sweep thread ever
    /// consults the gate, so there is no contention to create, and a second
    /// caller *should* wait for (and then reuse) the in-flight result rather
    /// than double-fetch.
    fn snapshot(&self) -> Option<UsageSnapshot> {
        let mut cache = self.cache.lock().unwrap();
        if let Some((at, snap)) = cache.as_ref() {
            if at.elapsed() < GATE_CACHE_TTL {
                return snap.clone();
            }
        }
        let fetched = super::block_on(fetch_snapshot(GATE_TIMEOUT)).ok();
        *cache = Some((Instant::now(), fetched.clone()));
        fetched
    }
}

impl Default for OauthUsageGate {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::UsageLimit;

    /// `2026-06-30T12:49:59+00:00` — the exact offset form the endpoint
    /// serves (see the module fixture in [`crate::usage`]).
    const RESET: &str = "2026-06-30T12:49:59+00:00";
    /// The same instant in epoch ms (cross-checked in the transcript tests).
    const RESET_MS: u64 = 1_782_823_799_000;
    /// A "now" one hour before the reset.
    const NOW: u64 = RESET_MS - 60 * 60 * 1000;

    fn limit(kind: &str, percent: f64, resets_at: Option<&str>) -> UsageLimit {
        UsageLimit {
            kind: Some(kind.into()),
            group: None,
            percent,
            severity: None,
            resets_at: resets_at.map(str::to_owned),
            scope_label: None,
            is_active: false,
        }
    }

    fn snap(limits: Vec<UsageLimit>) -> UsageSnapshot {
        UsageSnapshot {
            limits,
            plan: Some("max".into()),
        }
    }

    #[test]
    fn under_limit_windows_do_not_gate() {
        let s = snap(vec![
            limit("session", 6.0, Some(RESET)),
            limit("weekly_all", 99.9, Some(RESET)),
        ]);
        assert_eq!(defer_floor_ms(&s, NOW), None);
    }

    #[test]
    fn exhausted_session_defers_to_its_reset() {
        let s = snap(vec![limit("session", 100.0, Some(RESET))]);
        assert_eq!(defer_floor_ms(&s, NOW), Some(RESET_MS));
    }

    #[test]
    fn latest_reset_wins_when_both_windows_are_exhausted() {
        // The weekly window resets later — every exhausted window must reset
        // before a run can proceed (R2).
        let later = "2026-07-03T12:59:59+00:00";
        let later_ms = iso8601_to_ms(later).unwrap();
        let s = snap(vec![
            limit("session", 100.0, Some(RESET)),
            limit("weekly_all", 100.0, Some(later)),
        ]);
        assert_eq!(defer_floor_ms(&s, NOW), Some(later_ms));
    }

    #[test]
    fn per_model_window_never_gates() {
        // KTD6: weekly_scoped is out of scope — an exhausted per-model window
        // must not block an automation that may resolve to a different model.
        let s = snap(vec![limit("weekly_scoped", 100.0, Some(RESET))]);
        assert_eq!(defer_floor_ms(&s, NOW), None);
    }

    #[test]
    fn unknown_kinds_are_ignored() {
        let s = snap(vec![
            limit("new_window_kind", 100.0, Some(RESET)),
            UsageLimit {
                kind: None,
                ..limit("session", 100.0, Some(RESET))
            },
        ]);
        assert_eq!(defer_floor_ms(&s, NOW), None);
    }

    #[test]
    fn active_overage_opens_the_gate() {
        // KTD6: overage billing means usage continues — the exhausted window
        // must not defer anything.
        let mut overage = limit("overage", 0.0, None);
        overage.is_active = true;
        let s = snap(vec![limit("session", 100.0, Some(RESET)), overage]);
        assert_eq!(defer_floor_ms(&s, NOW), None);
    }

    #[test]
    fn inactive_overage_row_does_not_open_the_gate() {
        let s = snap(vec![
            limit("session", 100.0, Some(RESET)),
            limit("overage", 0.0, None), // is_active: false
        ]);
        assert_eq!(defer_floor_ms(&s, NOW), Some(RESET_MS));
    }

    #[test]
    fn missing_or_unparsable_resets_at_fails_open() {
        // KTD3/KTD7: an exhausted window we can't put a reset time on is a
        // surprise, not a deferral.
        let s = snap(vec![limit("session", 100.0, None)]);
        assert_eq!(defer_floor_ms(&s, NOW), None);
        let s = snap(vec![limit("session", 100.0, Some("garbage"))]);
        assert_eq!(defer_floor_ms(&s, NOW), None);
    }

    #[test]
    fn stale_or_absurd_resets_at_fails_open() {
        // A reset already in the past: the window's data is stale — open.
        let s = snap(vec![limit("session", 100.0, Some(RESET))]);
        assert_eq!(defer_floor_ms(&s, RESET_MS), None, "reset == now is not future");
        assert_eq!(defer_floor_ms(&s, RESET_MS + 1), None, "reset in the past");
        // A reset further out than any real window (KTD7): don't trust it.
        let s = snap(vec![limit("session", 100.0, Some("2027-01-01T00:00:00+00:00"))]);
        assert_eq!(defer_floor_ms(&s, NOW), None);
    }

    #[test]
    fn empty_snapshot_fails_open() {
        assert_eq!(defer_floor_ms(&snap(vec![]), NOW), None);
    }

    #[test]
    fn block_on_initializes_the_crate_runtime_lazily() {
        // [`OauthUsageGate::snapshot`] blocks the sweep thread on
        // `usage::block_on` — which must lazily build the crate runtime on
        // first use from any plain thread, and serve a second thread
        // concurrently. Probe exactly that mechanism, so the gate can't panic
        // on a missing runtime. (The fetch itself stays untested here: it
        // would hit the live endpoint with this box's real credentials.)
        assert_eq!(crate::usage::block_on(async { 41 + 1 }), 42);
        let t = std::thread::spawn(|| crate::usage::block_on(async { 2 + 2 }));
        assert_eq!(crate::usage::block_on(async { 1 + 1 }), 2);
        assert_eq!(t.join().unwrap(), 4);
    }
}
