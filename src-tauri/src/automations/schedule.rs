//! Cron schedule math for automations (U2 of
//! `docs/plans/2026-07-01-002-feat-automations-plan.md`): validation and the
//! R1 minimum-cadence clamp, on DST-correct occurrence computation (KTD-A).
//!
//! Pure module in the [`crate::state::attention`] shape: time arrives as
//! `now_ms`/`after_ms` epoch-ms arguments, never a wall-clock read. All
//! datetime math is done in zoned [`chrono_tz::Tz`] values — never
//! naive-then-convert (KTD-A): query instants are built with
//! `Tz::timestamp_opt` and croner resolves its wall-clock search through that
//! zone, so the two DST edges behave per croner 3.0.1's contract (pinned
//! empirically in the tests below):
//!
//! - **Spring-forward gap:** a fixed-time job whose wall-clock time falls in
//!   the gap snaps to the first valid instant after it; interval jobs skip
//!   the nonexistent occurrences.
//! - **Fall-back fold:** a fixed-time job fires once, at the *earliest* of
//!   the duplicated pair; interval duplicates are only revisited when the
//!   query instant itself lies inside the repeated hour (croner's
//!   fires-each-duplicate contract holds for its long-lived iterator, not
//!   for fresh strictly-after queries — see
//!   [`next_occurrence_ms`]'s fold guard).
//!
//! **Resolved "5-minute-floor clamp drift" open question (R1 / KTD-C):** a
//! bare `max(cron_next, now + 5min)` clamp makes boundary-aligned crons
//! (`*/5 * * * *`) walk forward by the sweep latency on every claim
//! (9:00 → 9:05:07 → 9:10:14 …), because the claim always lands a few seconds
//! after the boundary. [`advance`] therefore *snaps to the cron boundary*
//! whenever it is within [`SNAP_EPSILON_MS`] of the floor
//! (`next >= now + MIN_CADENCE_MS - SNAP_EPSILON_MS`); only genuinely
//! too-fast schedules get the `now + 5min` floor. The advisory min-gap check
//! in [`validate`] is best-effort — this clamp is the enforcement (R1).

use std::str::FromStr;

use chrono::{DateTime, NaiveDateTime, TimeZone};
use chrono_tz::Tz;
use croner::Cron;

/// R1: the minimum effective cadence — `advance` never returns an instant
/// sooner than `now + MIN_CADENCE_MS` (modulo the snap epsilon below).
pub const MIN_CADENCE_MS: u64 = 5 * 60 * 1000;

/// The snap window for the resolved clamp-drift decision (module doc): a
/// cron occurrence at least this close to the R1 floor is returned as-is,
/// keeping boundary-aligned schedules on their boundaries instead of
/// drifting by sweep latency each claim.
pub const SNAP_EPSILON_MS: u64 = 30 * 1000;

/// Advisory gap sampling (bb parity, KTD-A): at most this many consecutive
/// occurrences are examined. Hard cap — never trust a parsed expression to
/// terminate a loop on its own.
const GAP_SAMPLE_MAX_OCCURRENCES: usize = 256;

/// Advisory gap sampling window: 2 days from the fixed reference instant.
const GAP_SAMPLE_WINDOW_MS: u64 = 2 * 24 * 60 * 60 * 1000;

/// Fixed reference instant for gap sampling: 2026-01-06T00:00:00Z. Fixed so
/// [`validate`] is deterministic (no wall-clock reads), and mid-January so
/// the 2-day window crosses no DST transition in practice (advisory,
/// best-effort — R1's clamp is the enforcement).
const GAP_SAMPLE_REFERENCE_MS: u64 = 1_767_657_600_000;

/// Bound on the strictly-after scan in [`next_occurrence_ms`]. Inside a
/// fall-back fold croner can surface the first-pass instant of an ambiguous
/// pair (≤ the query instant) before its pending second-pass twin; one extra
/// step resolves that, so a small hard cap is ample.
const FOLD_SCAN_CAP: usize = 8;

/// Successful validation outcome (R1). `min_gap_warning` is the *advisory*
/// half of the minimum-cadence rule — distinguishable from hard `Err`s so
/// callers (U9's create path) can persist the automation and still surface
/// the warning. The clamp in [`advance`] is the enforcement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Validation {
    pub min_gap_warning: Option<String>,
}

/// Validate a cron expression + IANA timezone (R1): exactly 5
/// whitespace-separated fields, a parseable [`chrono_tz::Tz`], a
/// croner-parseable expression, plus the advisory min-gap sample
/// (~[`GAP_SAMPLE_MAX_OCCURRENCES`] occurrences over a 2-day window from a
/// fixed reference instant, gaps measured in local wall-clock time — bb
/// parity).
pub fn validate(cron: &str, tz: &str) -> Result<Validation, String> {
    let (cron, zone) = parse(cron, tz)?;
    Ok(Validation {
        min_gap_warning: min_gap_warning(&cron, &zone),
    })
}

/// The next occurrence strictly after `after_ms`, as epoch ms (`Ok(None)`
/// when the expression has no future occurrence). All math in zoned
/// [`chrono_tz::Tz`] values (KTD-A).
pub fn next_occurrence_ms(cron: &str, tz: &str, after_ms: u64) -> Result<Option<u64>, String> {
    let (cron, zone) = parse(cron, tz)?;
    let start = zoned(&zone, after_ms)?;
    // Strictly-after fold guard: croner's iterator starts non-inclusive at
    // second granularity, but inside a fall-back fold its wall-clock search
    // can surface the *earlier* (first-pass) instant of an ambiguous pair —
    // an instant at or before the query — ahead of its pending second-pass
    // twin. Skip anything ≤ `after_ms` under a hard cap (bounded loop; never
    // trust parsed input to terminate iteration).
    let mut iter = cron.iter_after(start);
    for _ in 0..FOLD_SCAN_CAP {
        let Some(occ) = iter.next() else {
            // croner found no further occurrence (search limit reached —
            // e.g. an expression that never matches): nothing to schedule.
            return Ok(None);
        };
        let ms = u64::try_from(occ.timestamp_millis())
            .map_err(|_| "cron occurrence predates the epoch".to_string())?;
        if ms > after_ms {
            return Ok(Some(ms));
        }
    }
    Err("cron occurrence scan failed to progress past the query instant".into())
}

/// R1's clamp with the resolved drift decision (module doc): let
/// `next = next_occurrence_ms(now)`; if `next >= now + MIN_CADENCE_MS −
/// SNAP_EPSILON_MS`, return `next` (snap to the cron boundary); else return
/// `now + MIN_CADENCE_MS` (the floor). `Ok(None)` mirrors
/// [`next_occurrence_ms`]: no future occurrence, nothing to schedule.
pub fn advance(cron: &str, tz: &str, now_ms: u64) -> Result<Option<u64>, String> {
    let next = next_occurrence_ms(cron, tz, now_ms)?;
    Ok(next.map(|next| {
        // Saturating throughout: `now_ms` and the occurrence both derive
        // from parsed/stored input, and release builds have overflow checks
        // off (repo convention).
        let floor = now_ms.saturating_add(MIN_CADENCE_MS);
        if next.saturating_add(SNAP_EPSILON_MS) >= floor {
            next // snap to the cron boundary (resolved drift decision)
        } else {
            floor // R1: the 5-minute minimum-cadence floor
        }
    }))
}

/// Shared parse gate for all three entry points, in R1's order: field count
/// (exactly 5 — a stored 6-field expression must fail loudly, not misparse),
/// then the timezone (message names the bad value, R21), then croner.
fn parse(expr: &str, tz: &str) -> Result<(Cron, Tz), String> {
    let fields = expr.split_whitespace().count();
    if fields != 5 {
        if fields == 6 {
            return Err(
                "expected 5 cron fields, got 6 — fly uses 5-field cron expressions \
                 (minute hour day-of-month month day-of-week), not 6-field with seconds"
                    .to_string(),
            );
        }
        return Err(format!(
            "expected 5 cron fields (minute hour day-of-month month day-of-week), got {fields}"
        ));
    }
    let zone: Tz = tz.parse().map_err(|_| {
        format!("unknown timezone {tz:?} — expected an IANA name like \"America/New_York\"")
    })?;
    // The default croner parser treats a 5-field expression as seconds-less
    // (seconds fixed at 0); the count gate above guarantees 5 fields.
    let cron =
        Cron::from_str(expr).map_err(|e| format!("invalid cron expression {expr:?}: {e}"))?;
    Ok((cron, zone))
}

/// An epoch-ms instant as a zoned datetime (KTD-A: occurrence math starts
/// zoned and stays zoned). Floors to the containing second — croner works at
/// second granularity and 5-field occurrences land on whole minutes, so
/// [`next_occurrence_ms`]'s `> after_ms` filter preserves ms-strictness.
fn zoned(zone: &Tz, epoch_ms: u64) -> Result<DateTime<Tz>, String> {
    let secs = i64::try_from(epoch_ms / 1000)
        .map_err(|_| "timestamp out of range for schedule math".to_string())?;
    zone.timestamp_opt(secs, 0)
        .single()
        .ok_or_else(|| "timestamp out of range for schedule math".to_string())
}

/// The advisory half of R1's minimum cadence: sample up to
/// [`GAP_SAMPLE_MAX_OCCURRENCES`] consecutive occurrences over a 2-day
/// window from the fixed reference instant and measure consecutive gaps in
/// **local wall-clock** time (bb parity). Any gap under [`MIN_CADENCE_MS`]
/// yields a warning naming the observed minimum. Best-effort by design — a
/// too-fast schedule outside the sampled window is still caught at run time
/// by [`advance`]'s clamp.
fn min_gap_warning(cron: &Cron, zone: &Tz) -> Option<String> {
    let start = zoned(zone, GAP_SAMPLE_REFERENCE_MS).ok()?;
    let window_end_ms = GAP_SAMPLE_REFERENCE_MS.saturating_add(GAP_SAMPLE_WINDOW_MS);
    let mut prev: Option<NaiveDateTime> = None;
    let mut min_gap_ms: Option<i64> = None;
    // `take` bounds the sample; croner's own year/iteration limits bound
    // each `next()` internally.
    for occ in cron.iter_after(start).take(GAP_SAMPLE_MAX_OCCURRENCES) {
        let in_window = u64::try_from(occ.timestamp_millis()).is_ok_and(|ms| ms <= window_end_ms);
        if !in_window {
            break;
        }
        let local = occ.naive_local();
        if let Some(prev) = prev {
            let gap = (local - prev).num_milliseconds();
            if min_gap_ms.map_or(true, |m| gap < m) {
                min_gap_ms = Some(gap);
            }
        }
        prev = Some(local);
    }
    let min = min_gap_ms?;
    if min < MIN_CADENCE_MS as i64 {
        Some(format!(
            "occurrences can be as close as {}s apart; fly enforces a 5-minute minimum \
             cadence by clamping at run time (R1)",
            min.max(0) / 1000
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NY: &str = "America/New_York";
    const UTC: &str = "UTC";

    fn zone(name: &str) -> Tz {
        name.parse().expect("test tz parses")
    }

    /// Epoch ms of an unambiguous local wall-clock instant in `z`.
    fn local_ms(z: &Tz, y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> u64 {
        z.with_ymd_and_hms(y, mo, d, h, mi, s)
            .single()
            .expect("unambiguous local time")
            .timestamp_millis() as u64
    }

    /// Epoch ms of the EARLIEST mapping of an ambiguous (fall-back) local
    /// wall-clock instant — the first, pre-transition pass.
    fn local_ms_earliest(z: &Tz, y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> u64 {
        z.with_ymd_and_hms(y, mo, d, h, mi, s)
            .earliest()
            .expect("local time exists")
            .timestamp_millis() as u64
    }

    /// Epoch ms of the LATEST mapping of an ambiguous (fall-back) local
    /// wall-clock instant — the second, post-transition pass.
    fn local_ms_latest(z: &Tz, y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> u64 {
        z.with_ymd_and_hms(y, mo, d, h, mi, s)
            .latest()
            .expect("local time exists")
            .timestamp_millis() as u64
    }

    // R1: the canonical well-formed automation schedule — five fields, a
    // real IANA zone — validates with no hard error and no advisory warning
    // (a 5-minute cadence is exactly the floor, never below it).
    #[test]
    fn five_field_cron_with_valid_tz_validates_clean_with_no_advisory_warning_r1() {
        let v = validate("*/5 * * * *", NY).expect("validates");
        assert_eq!(v.min_gap_warning, None);
    }

    // R1/KTD-A: a `*/5` cron steps in 5-minute increments — successive
    // next-occurrence queries land on consecutive boundaries.
    #[test]
    fn next_occurrence_steps_a_five_minute_cron_by_five_minutes_r1() {
        let utc = zone(UTC);
        let after = local_ms(&utc, 2026, 1, 6, 9, 2, 17);
        let first = next_occurrence_ms("*/5 * * * *", UTC, after).unwrap();
        assert_eq!(first, Some(local_ms(&utc, 2026, 1, 6, 9, 5, 0)));

        let second = next_occurrence_ms("*/5 * * * *", UTC, first.unwrap()).unwrap();
        assert_eq!(second, Some(local_ms(&utc, 2026, 1, 6, 9, 10, 0)));
        assert_eq!(
            second.unwrap() - first.unwrap(),
            MIN_CADENCE_MS,
            "consecutive boundaries are exactly 5 minutes apart"
        );
    }

    // Strictly-after semantics: querying at exactly an occurrence instant
    // returns the NEXT one, never the instant itself.
    #[test]
    fn next_occurrence_at_exactly_an_occurrence_instant_returns_the_following_one() {
        let utc = zone(UTC);
        let boundary = local_ms(&utc, 2026, 1, 6, 9, 5, 0);
        let next = next_occurrence_ms("*/5 * * * *", UTC, boundary).unwrap();
        assert_eq!(next, Some(local_ms(&utc, 2026, 1, 6, 9, 10, 0)));
    }

    // R1: `* * * * *` is not a hard error — it validates, but the advisory
    // min-gap sample (60s consecutive gaps) trips the warning. The clamp in
    // `advance` is the enforcement; this is the best-effort heads-up.
    #[test]
    fn every_minute_cron_validates_but_trips_the_advisory_min_gap_warning_r1() {
        let v = validate("* * * * *", NY).expect("advisory, not a hard error");
        let warning = v.min_gap_warning.expect("warning trips");
        assert!(
            warning.contains("60s"),
            "warning names the observed gap: {warning}"
        );
        assert!(
            warning.contains("5-minute"),
            "warning points at the enforced cadence: {warning}"
        );
    }

    // R1: an every-minute cron claimed at :00:07 gets the floor — its next
    // occurrence (:01:00, 53s away) is far below `floor − ε` (now + 4:30),
    // so `advance` returns exactly now + 5min.
    #[test]
    fn advance_floors_an_every_minute_cron_claimed_mid_minute_to_now_plus_five_minutes_r1() {
        let utc = zone(UTC);
        let now = local_ms(&utc, 2026, 1, 6, 9, 0, 7);
        let advanced = advance("* * * * *", UTC, now).unwrap();
        assert_eq!(advanced, Some(now + MIN_CADENCE_MS));
    }

    // Resolved clamp-drift decision (R1/KTD-C open question): a `*/5` cron
    // claimed 7 seconds after its boundary snaps to the NEXT boundary
    // (9:05:00), not boundary + 7s — the run phase must not walk forward by
    // sweep latency each claim.
    #[test]
    fn advance_snaps_a_boundary_aligned_cron_claimed_seconds_late_back_to_the_boundary_r1() {
        let utc = zone(UTC);
        let now = local_ms(&utc, 2026, 1, 6, 9, 0, 7);
        let advanced = advance("*/5 * * * *", UTC, now).unwrap();
        assert_eq!(
            advanced,
            Some(local_ms(&utc, 2026, 1, 6, 9, 5, 0)),
            "snapped to the boundary, not floored to 9:05:07"
        );
    }

    // R1: a cron whose next occurrence is only 30s out is floored to
    // now + 5min — 30s is well outside the snap epsilon window
    // (floor − ε = now + 4:30).
    #[test]
    fn advance_floors_a_cron_due_in_thirty_seconds_to_now_plus_five_minutes_r1() {
        let utc = zone(UTC);
        // Daily 09:30 cron, claimed at 09:29:30 — next occurrence in 30s.
        let now = local_ms(&utc, 2026, 1, 6, 9, 29, 30);
        let advanced = advance("30 9 * * *", UTC, now).unwrap();
        assert_eq!(advanced, Some(now + MIN_CADENCE_MS));
    }

    // R1: fly is 5-field-only — a 6-field (seconds-bearing) expression is
    // rejected with a message saying so, and other wrong counts name the
    // count they got.
    #[test]
    fn six_field_expressions_are_rejected_pointing_at_the_five_field_convention_r1() {
        let err = validate("0 */5 * * * *", NY).unwrap_err();
        assert!(err.contains("5-field"), "names the convention: {err}");
        assert!(err.contains("got 6"), "names the count: {err}");

        let err = validate("* * * *", NY).unwrap_err();
        assert!(err.contains("got 4"), "names the count: {err}");
    }

    // R1/R21: a bad timezone is a hard error and the message names the bad
    // value (actionable, per R21) — from validate and from the occurrence
    // paths alike.
    #[test]
    fn unknown_timezone_is_rejected_with_the_bad_tz_named_r1() {
        let err = validate("*/5 * * * *", "America/Nowhere").unwrap_err();
        assert!(err.contains("America/Nowhere"), "names the tz: {err}");

        let err = next_occurrence_ms("*/5 * * * *", "America/Nowhere", 0).unwrap_err();
        assert!(err.contains("America/Nowhere"), "names the tz: {err}");

        let err = advance("*/5 * * * *", "America/Nowhere", 0).unwrap_err();
        assert!(err.contains("America/Nowhere"), "names the tz: {err}");
    }

    // R1: five fields that croner itself cannot parse (minute 99) are a
    // hard error naming the expression.
    #[test]
    fn unparseable_cron_field_values_are_rejected_r1() {
        let err = validate("99 * * * *", NY).unwrap_err();
        assert!(err.contains("99 * * * *"), "names the expression: {err}");
    }

    // R1: `1,2 * * * *` fires at :01 and :02 each hour — a 1-minute gap once
    // an hour. The 2-day sample sees that pair, so the advisory warning
    // TRIPS deterministically (documented outcome for this shape).
    #[test]
    fn one_and_two_past_each_hour_trips_the_advisory_min_gap_warning_r1() {
        let v = validate("1,2 * * * *", NY).expect("advisory, not a hard error");
        let warning = v.min_gap_warning.expect("1-minute gap trips the advisory");
        assert!(warning.contains("60s"), "names the observed gap: {warning}");
    }

    // KTD-A, croner 3.0.1 spring-forward contract (pinned empirically): a
    // fixed-time job (`30 2 * * *` is fixed second/minute/hour) whose wall
    // time falls in the 2026-03-08 America/New_York gap (02:00→03:00) SNAPS
    // to the first valid instant after the gap — 03:00:00 EDT — rather than
    // being skipped; the next day it is back on the literal 02:30.
    #[test]
    fn fixed_cron_in_the_spring_forward_gap_snaps_to_the_first_instant_after_the_gap_ktd_a() {
        let ny = zone(NY);
        let after = local_ms(&ny, 2026, 3, 8, 0, 0, 0); // midnight EST, gap day
        let gap_day = next_occurrence_ms("30 2 * * *", NY, after).unwrap();
        assert_eq!(
            gap_day,
            Some(local_ms(&ny, 2026, 3, 8, 3, 0, 0)),
            "02:30 does not exist — snapped to 03:00 EDT"
        );

        let next_day = next_occurrence_ms("30 2 * * *", NY, gap_day.unwrap()).unwrap();
        assert_eq!(
            next_day,
            Some(local_ms(&ny, 2026, 3, 9, 2, 30, 0)),
            "back on the literal 02:30 once the gap day is past"
        );
    }

    // KTD-A, croner 3.0.1 spring-forward contract (pinned empirically):
    // interval jobs (`*/30`) skip occurrences inside the gap — from 01:30
    // EST the next fire is 03:00 EDT (02:00 and 02:30 never exist).
    #[test]
    fn interval_cron_skips_occurrences_inside_the_spring_forward_gap_ktd_a() {
        let ny = zone(NY);
        let after = local_ms(&ny, 2026, 3, 8, 1, 30, 0); // 01:30 EST
        let got = next_occurrence_ms("*/30 * * * *", NY, after).unwrap();
        assert_eq!(
            got,
            Some(local_ms(&ny, 2026, 3, 8, 3, 0, 0)),
            "gap occurrences skipped; next real instant is 03:00 EDT"
        );
    }

    // KTD-A, croner 3.0.1 fall-back contract (pinned empirically): a
    // fixed-time job in the duplicated 2026-11-01 America/New_York hour
    // fires ONCE, at the earliest (EDT) of the ambiguous pair; the EST
    // repeat of 01:30 is not fired — the next occurrence is the following
    // day.
    #[test]
    fn fixed_cron_in_the_fall_back_fold_fires_once_at_the_earliest_occurrence_ktd_a() {
        let ny = zone(NY);
        let after = local_ms(&ny, 2026, 11, 1, 0, 0, 0); // 00:00 EDT, fold day
        let edt_130 = local_ms_earliest(&ny, 2026, 11, 1, 1, 30, 0);
        let est_130 = local_ms_latest(&ny, 2026, 11, 1, 1, 30, 0);
        assert_eq!(est_130 - edt_130, 60 * 60 * 1000, "the pair is 1h apart");

        let got = next_occurrence_ms("30 1 * * *", NY, after).unwrap();
        assert_eq!(got, Some(edt_130), "earliest of the ambiguous pair");

        let next = next_occurrence_ms("30 1 * * *", NY, edt_130).unwrap();
        assert_eq!(
            next,
            Some(local_ms(&ny, 2026, 11, 2, 1, 30, 0)),
            "the EST repeat is not fired; next is the following day"
        );
    }

    // KTD-A, croner 3.0.1 fall-back contract as it manifests through fresh
    // strictly-after queries (pinned empirically): stepping from the first
    // (EDT) pass visits each wall-clock occurrence once — 01:00 EDT → 01:30
    // EDT → 02:00 EST — without revisiting the EST repeat pass; but a query
    // from INSIDE the repeat pass sees its remainder (croner surfaces the
    // ambiguous pair's earlier instant first, which is ≤ the query — the
    // fold guard must skip it and return the true next, never an instant in
    // the past).
    #[test]
    fn fall_back_queries_stay_strictly_after_and_see_the_est_repeat_only_from_inside_ktd_a() {
        let ny = zone(NY);
        let edt_100 = local_ms_earliest(&ny, 2026, 11, 1, 1, 0, 0);
        let edt_130 = local_ms_earliest(&ny, 2026, 11, 1, 1, 30, 0);
        let est_110 = local_ms_latest(&ny, 2026, 11, 1, 1, 10, 0);
        let est_130 = local_ms_latest(&ny, 2026, 11, 1, 1, 30, 0);
        let est_200 = local_ms(&ny, 2026, 11, 1, 2, 0, 0); // unambiguous

        // First pass: wall-clock forward, one visit per wall time.
        let got = next_occurrence_ms("*/30 * * * *", NY, edt_100).unwrap();
        assert_eq!(got, Some(edt_130));
        let got = next_occurrence_ms("*/30 * * * *", NY, edt_130).unwrap();
        assert_eq!(got, Some(est_200), "the EST repeat pass is not revisited");

        // From inside the repeat pass: the remaining EST occurrence is next,
        // and the result is strictly after the query instant.
        let got = next_occurrence_ms("*/30 * * * *", NY, est_110).unwrap();
        assert_eq!(got, Some(est_130), "sees the repeat from within it");
        assert!(got.unwrap() > est_110, "never an instant in the past");
    }
}
