//! Per-pane output-activity tracker — the "current work stretch" signal that
//! powers the agent dashboard (KTD-A, U1 of the agent-dashboard plan).
//!
//! A pane is "working" when its agent is producing output. We model that as a
//! **current work stretch**: the contiguous span since output resumed after the
//! last idle gap. Like [`super::attention`] and [`super::lifecycle`] this is
//! pure and time-injected — `now` is an argument — so the idle/working decision
//! is tested without a running PTY. The read thread (U3) feeds [`record`] one
//! timestamp per above-threshold output chunk; the dashboard poll (U4) queries
//! [`current_stretch`].
//!
//! State is the two timestamps the caller holds (atomics on `PaneShared`, U3):
//! `last_output_ms` (when output last arrived) and `work_start_ms` (when the
//! current stretch began). A gap longer than [`IDLE_GAP_MS`] ends a stretch; the
//! next output starts a fresh one. The displayed number is the *current*
//! output-active stretch, not total task time (see the plan's Problem Frame) —
//! a deliberately honest, if coarser, metric than session age.

/// Idle threshold: output silent for longer than this ends the current work
/// stretch. Generous enough to ride over typical tool-call / thinking pauses
/// without resetting (see the plan's Open Questions — tuned live against a real
/// session). A backend constant for v1; config exposure is deferred.
pub const IDLE_GAP_MS: u64 = 75_000;

/// Fold a new output timestamp into the `(last_output_ms, work_start_ms)` pair,
/// returning the updated pair (both concrete — after any output there is always
/// an active stretch).
///
/// A new stretch starts (`work_start = now`) when there was no active stretch
/// (`work_start_ms` is `None`) or the gap since the previous output exceeded
/// `gap`; otherwise the existing `work_start` is preserved and only the
/// last-output timestamp advances. `now` is monotonic per pane (ms since the
/// pane epoch); saturating math means an out-of-order `now` never panics.
pub fn record(
    last_output_ms: Option<u64>,
    work_start_ms: Option<u64>,
    now: u64,
    gap: u64,
) -> (u64, u64) {
    let new_start = match (last_output_ms, work_start_ms) {
        // An active stretch whose previous output is within the gap continues.
        (Some(last), Some(start)) if now.saturating_sub(last) <= gap => start,
        // No prior output, no active stretch, or the gap was exceeded → fresh.
        _ => now,
    };
    (now, new_start)
}

/// The current work stretch in ms (`now - work_start`), or `None` when the pane
/// is idle — never had output, or the last output is older than `gap`. The
/// boundary is inclusive: a pane whose last output landed exactly `gap` ms ago
/// is still "working" (matches [`record`]'s `> gap` reset rule).
pub fn current_stretch(
    last_output_ms: Option<u64>,
    work_start_ms: Option<u64>,
    now: u64,
    gap: u64,
) -> Option<u64> {
    match (last_output_ms, work_start_ms) {
        (Some(last), Some(start)) if now.saturating_sub(last) <= gap => {
            Some(now.saturating_sub(start))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GAP: u64 = 100;

    #[test]
    fn first_output_starts_a_stretch() {
        // No prior state → a fresh stretch anchored at `now`.
        let (last, start) = record(None, None, 500, GAP);
        assert_eq!(last, 500);
        assert_eq!(start, 500);
    }

    #[test]
    fn output_within_gap_keeps_the_original_start() {
        // Stretch began at 500; a chunk 80ms later (≤ gap) extends, not resets.
        let (last, start) = record(Some(500), Some(500), 580, GAP);
        assert_eq!(last, 580);
        assert_eq!(start, 500, "work_start is preserved across a sub-gap chunk");
    }

    #[test]
    fn output_after_the_gap_starts_a_new_stretch() {
        // 101ms of silence (> gap) ends the old stretch; the next output resets.
        let (last, start) = record(Some(500), Some(500), 601, GAP);
        assert_eq!(last, 601);
        assert_eq!(start, 601, "a gap > IDLE_GAP_MS advances work_start to now");
    }

    #[test]
    fn current_stretch_reports_elapsed_while_active() {
        // last=580 within gap of now=600 → working for now-start = 100.
        assert_eq!(current_stretch(Some(580), Some(500), 600, GAP), Some(100));
    }

    #[test]
    fn current_stretch_is_none_once_idle() {
        // last output 200ms ago (> gap) → idle, no number.
        assert_eq!(current_stretch(Some(500), Some(500), 700, GAP), None);
    }

    #[test]
    fn current_stretch_is_none_when_never_ran() {
        assert_eq!(current_stretch(None, None, 1000, GAP), None);
        assert_eq!(current_stretch(Some(10), None, 1000, GAP), None);
    }

    #[test]
    fn gap_boundary_is_inclusive_still_working() {
        // Last output exactly `gap` ms ago: still working (record agrees: continues).
        assert_eq!(current_stretch(Some(500), Some(500), 600, GAP), Some(100));
        let (_, start) = record(Some(500), Some(500), 600, GAP);
        assert_eq!(start, 500, "exactly-gap continues the stretch");
    }

    #[test]
    fn out_of_order_now_saturates_without_panic() {
        // A `now` earlier than stored timestamps yields 0, never an underflow panic.
        let (last, start) = record(Some(50), Some(50), 10, GAP);
        assert_eq!((last, start), (10, 50));
        assert_eq!(current_stretch(Some(50), Some(50), 10, GAP), Some(0));
    }
}
