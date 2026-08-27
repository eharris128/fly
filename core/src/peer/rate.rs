//! The peer-send rate limiter (agent-peer-messaging U5, R11/KTD8): a
//! clock-injected token bucket per sender pane plus a global backstop.
//!
//! This is the *enforceable* fan-out brake — hop/depth counters were rejected
//! in the plan because causation between an inbound message and the
//! recipient's next send crosses a model and is invisible to the server. What
//! spend-per-origin bounds: a runaway loop between two mutually-opted-in
//! panes converges to the refill rate, visible in both panes the whole time.
//!
//! In-memory only, reset on restart — a brake, not an audit log. Pure (the
//! caller passes `now_ms`), so refill math is tested without sleeping.

use std::collections::HashMap;

/// Per-pane burst: how many sends a quiet pane can fire back-to-back.
pub const BURST: u32 = 5;
/// Per-pane refill: one send earned back every 12s (~5/min sustained).
pub const REFILL_MS: u64 = 12_000;
/// Global backstop across all senders.
pub const GLOBAL_BURST: u32 = 15;
/// Global refill: one send every 4s across the whole app.
pub const GLOBAL_REFILL_MS: u64 = 4_000;

/// Prune scan trigger: with more tracked panes than this, full buckets are
/// dropped on the next take (pane ids are never reused, so a full bucket for
/// a dead pane is only memory).
const PRUNE_ABOVE: usize = 64;

struct Bucket {
    available: u32,
    burst: u32,
    refill_ms: u64,
    last_refill_ms: u64,
}

impl Bucket {
    fn new(burst: u32, refill_ms: u64, now_ms: u64) -> Self {
        Bucket {
            available: burst,
            burst,
            refill_ms,
            last_refill_ms: now_ms,
        }
    }

    /// Advance the refill clock, then take one token if available.
    fn try_take(&mut self, now_ms: u64) -> bool {
        let elapsed = now_ms.saturating_sub(self.last_refill_ms);
        let earned = (elapsed / self.refill_ms) as u32;
        if earned > 0 {
            self.available = (self.available.saturating_add(earned)).min(self.burst);
            // Advance by whole refill intervals only, so a fractional
            // remainder keeps accruing instead of being dropped.
            self.last_refill_ms += u64::from(earned) * self.refill_ms;
        }
        if self.available == 0 {
            return false;
        }
        self.available -= 1;
        true
    }
}

/// The bucket set. Callers wrap it in a `Mutex` (the handler runs on socket
/// connection threads); the type itself stays lock-free and clock-injected.
pub struct Buckets {
    per_pane: HashMap<u64, Bucket>,
    global: Bucket,
}

impl Default for Buckets {
    fn default() -> Self {
        Self::new()
    }
}

impl Buckets {
    pub fn new() -> Self {
        Buckets {
            per_pane: HashMap::new(),
            global: Bucket::new(GLOBAL_BURST, GLOBAL_REFILL_MS, 0),
        }
    }

    /// One send attempt by `pane` at `now_ms`. Consumes a per-pane token AND a
    /// global token; refuses (consuming neither) when either is dry — checked
    /// per-pane first so one spammer drains its own bucket before the shared
    /// one.
    pub fn try_take(&mut self, pane: u64, now_ms: u64) -> bool {
        if self.per_pane.len() > PRUNE_ABOVE {
            self.per_pane.retain(|_, b| {
                // Refill in place so a long-idle bucket reads as full.
                let elapsed = now_ms.saturating_sub(b.last_refill_ms);
                let earned = (elapsed / b.refill_ms) as u32;
                b.available.saturating_add(earned) < b.burst
            });
        }
        let bucket = self
            .per_pane
            .entry(pane)
            .or_insert_with(|| Bucket::new(BURST, REFILL_MS, now_ms));
        // Peek-then-commit across the two buckets: a per-pane token must not
        // be burned when the global bucket refuses.
        if bucket.available == 0 && !bucket_would_refill(bucket, now_ms) {
            return false;
        }
        if !self.global.try_take(now_ms) {
            return false;
        }
        // Cannot refuse: the peek above established a token exists or refills.
        bucket.try_take(now_ms)
    }
}

/// Whether advancing `b`'s refill clock to `now_ms` would yield ≥1 token.
fn bucket_would_refill(b: &Bucket, now_ms: u64) -> bool {
    now_ms.saturating_sub(b.last_refill_ms) / b.refill_ms > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_allows_n_then_refuses() {
        let mut b = Buckets::new();
        for i in 0..BURST {
            assert!(b.try_take(1, 0), "take {i} within burst");
        }
        assert!(!b.try_take(1, 0), "over burst refused");
    }

    #[test]
    fn refill_restores_at_the_configured_rate() {
        let mut b = Buckets::new();
        for _ in 0..BURST {
            assert!(b.try_take(1, 0));
        }
        assert!(!b.try_take(1, REFILL_MS - 1), "one ms early: still dry");
        assert!(b.try_take(1, REFILL_MS), "one interval earns one token");
        assert!(!b.try_take(1, REFILL_MS), "and only one");
        // Fractional remainders accrue: two half-intervals sum to a token.
        assert!(b.try_take(1, 3 * REFILL_MS), "later interval earns again");
    }

    #[test]
    fn two_senders_do_not_share_a_bucket() {
        let mut b = Buckets::new();
        for _ in 0..BURST {
            assert!(b.try_take(1, 0));
        }
        assert!(!b.try_take(1, 0));
        assert!(b.try_take(2, 0), "a different pane has its own burst");
    }

    #[test]
    fn global_backstop_refuses_when_many_senders_sum_past_it() {
        let mut b = Buckets::new();
        let mut granted = 0;
        // 10 panes × BURST(5) = 50 attempts, all at t=0: the global burst (15)
        // is the binding cap.
        for pane in 0..10u64 {
            for _ in 0..BURST {
                if b.try_take(pane, 0) {
                    granted += 1;
                }
            }
        }
        assert_eq!(granted, GLOBAL_BURST);
    }

    #[test]
    fn a_global_refusal_does_not_burn_the_per_pane_token() {
        let mut b = Buckets::new();
        // Drain the global bucket with other panes.
        let mut drained = 0;
        for pane in 100..200u64 {
            if b.try_take(pane, 0) {
                drained += 1;
            }
            if drained == GLOBAL_BURST {
                break;
            }
        }
        // Pane 1 refused on the global bucket…
        assert!(!b.try_take(1, 0));
        // …still has its full burst once the global refills.
        let later = GLOBAL_REFILL_MS * u64::from(BURST);
        let mut ok = 0;
        for i in 0..BURST {
            if b.try_take(1, later + i as u64) {
                ok += 1;
            }
        }
        // Global refilled BURST tokens over `later`, so all BURST per-pane
        // tokens must have been grantable.
        assert_eq!(ok, BURST, "per-pane burst survived the global refusal");
    }

    #[test]
    fn pruning_drops_dead_full_buckets_without_touching_live_ones() {
        let mut b = Buckets::new();
        // Track many panes (full buckets after a long idle).
        for pane in 0..80u64 {
            let _ = b.try_take(pane, 0);
        }
        // Pane 0 is mid-drain (not full) at prune time; the rest refill to
        // full by t = BURST*REFILL_MS and get pruned.
        for _ in 0..BURST {
            let _ = b.try_take(0, u64::from(BURST) * REFILL_MS);
        }
        let t = 10 * u64::from(BURST) * REFILL_MS;
        let _ = b.try_take(999, t); // triggers the prune scan
        assert!(b.per_pane.len() < 80, "full buckets were pruned");
        assert!(b.per_pane.contains_key(&999));
    }
}
