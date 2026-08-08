//! The dependency predicate (U2 of
//! `docs/plans/2026-08-07-002-feat-automation-dependencies-plan.md` — all
//! IDs in this file cite that plan): pure decision logic for whether a
//! dependent automation's due occurrence fires against its upstream, waits,
//! or records an honest [`RunStatus::Withheld`] decline.
//!
//! Pure like [`super::model`]: no I/O, no clocks — time arrives as
//! `occurrence_ms`/`now_ms` arguments, records as plain references — so
//! every branch is testable without a manager. The sweep (U4) evaluates
//! this inside its single mutate hold via a read-only pre-pass; the manual
//! run path shares the exact same decision through [`manual_decision`].
//!
//! KTD4 — one symmetric `within` window: a qualifying upstream success must
//! have `finished_at ≥ occurrence − within`, and the dependent defers until
//! `occurrence + within` before withholding. KTD5 — success is
//! `Succeeded` ∧ not a FAIL verdict (the honest-verdict rule: a monitor-
//! style FAIL rides a `succeeded` row). KTD3 — exactly-once: any upstream
//! run id already stamped in the dependent's history is refused. Saturating
//! arithmetic throughout (R6): the window and every timestamp are untrusted
//! numeric input and release builds have overflow checks off.

use std::collections::{BTreeMap, HashSet};

use super::model::{Automation, Dependency, RunRow, RunStatus, VerdictOutcome};

/// KTD6: maximum dependency-chain depth accepted at create time. Edges are
/// set at create only (update may only clear one — automation-update KTD2),
/// so this bounds a *linear* chain (A→B→…); it exists to keep
/// `show`/debugging tractable, not to protect the sweep (which is
/// single-hop by construction).
pub const MAX_CHAIN_DEPTH: usize = 8;

/// R7: accepted `--within` range — below a minute the 10s sweep tick
/// dominates; above a week the "freshness" framing is meaningless.
pub const WITHIN_MIN_MS: u64 = 60 * 1000;
pub const WITHIN_MAX_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// What a due dependent occurrence should do (R4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepDecision {
    /// A fresh, successful, not-yet-consumed upstream run exists — claim,
    /// stamping this id on the claimed row (KTD3).
    Satisfied { upstream_run_id: String },
    /// No qualifying run yet, but the wait window is still open — leave the
    /// occurrence completely untouched (no row, no advance; the R5
    /// frontend-gate precedent) and re-evaluate next tick.
    Wait,
    /// The window closed without a qualifying run — record the honest
    /// decline (R5 reasons) and advance the schedule.
    Withhold { reason: String },
}

/// Evaluate the dependency for one due occurrence (R4). `dependent_runs` is
/// the dependent's own bounded history (the KTD3 consumption record);
/// `upstream` is `None` when the named automation no longer exists — an
/// immediate withhold, no wait (nothing can make a deleted upstream
/// succeed).
pub fn evaluate(
    dep: &Dependency,
    dependent_runs: &[RunRow],
    upstream: Option<&Automation>,
    occurrence_ms: u64,
    now_ms: u64,
) -> DepDecision {
    let within = dep.within();
    let window_start = occurrence_ms.saturating_sub(within);
    let deadline = occurrence_ms.saturating_add(within);

    let Some(up) = upstream else {
        return DepDecision::Withhold {
            reason: format!("upstream {} no longer exists", dep.upstream_id),
        };
    };

    let consumed: HashSet<&str> = dependent_runs
        .iter()
        .filter_map(|r| r.upstream_run_id.as_deref())
        .collect();

    // Newest-first: the freshest qualifying success wins (an older
    // unconsumed one behind it is by definition staler evidence).
    for r in up.runs.iter().rev() {
        if qualifies(r, window_start) && !consumed.contains(r.id.as_str()) {
            return DepDecision::Satisfied {
                upstream_run_id: r.id.clone(),
            };
        }
    }

    if now_ms < deadline {
        return DepDecision::Wait;
    }
    DepDecision::Withhold {
        reason: withhold_reason(up, &consumed, window_start),
    }
}

/// The manual-run flavor (R12): a manual dependent run is synchronous, so
/// there is no occurrence to wait against — evaluate at `now` and convert a
/// would-be `Wait` into the honest reason the operator sees immediately.
/// Same predicate, same consumption rule, same reasons.
pub fn manual_decision(
    dep: &Dependency,
    dependent_runs: &[RunRow],
    upstream: Option<&Automation>,
    now_ms: u64,
) -> DepDecision {
    match evaluate(dep, dependent_runs, upstream, now_ms, now_ms) {
        DepDecision::Wait => {
            let consumed: HashSet<&str> = dependent_runs
                .iter()
                .filter_map(|r| r.upstream_run_id.as_deref())
                .collect();
            let window_start = now_ms.saturating_sub(dep.within());
            // `upstream` is Some here: a missing upstream never returns Wait.
            let reason = upstream
                .map(|up| withhold_reason(up, &consumed, window_start))
                .unwrap_or_else(|| format!("upstream {} no longer exists", dep.upstream_id));
            DepDecision::Withhold { reason }
        }
        other => other,
    }
}

/// KTD5: a qualifying upstream run — closed `Succeeded`, fresh enough, and
/// not carrying a FAIL verdict (an infra-clean run whose *check* honestly
/// failed must not read as pipeline success).
fn qualifies(r: &RunRow, window_start_ms: u64) -> bool {
    r.status == RunStatus::Succeeded
        && r.finished_at.is_some_and(|t| t >= window_start_ms)
        && r.verdict
            .as_ref()
            .map_or(true, |v| v.outcome != VerdictOutcome::Fail)
}

/// R5: derive the specific decline reason from what the upstream actually
/// did in the window. Checked in order: still in flight → the newest
/// in-window terminal row's own story (failed / skipped / withheld — the
/// upstream's reason is quoted, so a chain's explanation propagates — /
/// FAIL verdict / already consumed) → a success exists but predates the
/// window (stale) → it has never run at all. All strings are fly-minted
/// (control-safe by construction) and pinned by tests.
fn withhold_reason(up: &Automation, consumed: &HashSet<&str>, window_start_ms: u64) -> String {
    if up.in_flight() {
        return "upstream run still in flight at the wait deadline".to_owned();
    }
    let newest_in_window = up
        .runs
        .iter()
        .rev()
        .find(|r| r.status.is_terminal() && r.finished_at.is_some_and(|t| t >= window_start_ms));
    if let Some(r) = newest_in_window {
        let detail = r.error.as_deref().unwrap_or("no detail");
        return match r.status {
            RunStatus::Failed => format!("upstream failed ({detail})"),
            RunStatus::Skipped => format!("upstream was skipped ({detail})"),
            RunStatus::Withheld => format!("upstream was withheld: {detail}"),
            RunStatus::Succeeded => {
                if r.verdict
                    .as_ref()
                    .is_some_and(|v| v.outcome == VerdictOutcome::Fail)
                {
                    "upstream succeeded with a FAIL verdict".to_owned()
                } else if consumed.contains(r.id.as_str()) {
                    format!("no new upstream run (run {} already consumed)", r.id)
                } else {
                    // Unreachable in practice: an unconsumed in-window
                    // success would have satisfied the predicate. Honest
                    // fallback rather than a panic.
                    "upstream state unreadable".to_owned()
                }
            }
            RunStatus::Running => unreachable!("filtered terminal"),
        };
    }
    if up
        .runs
        .iter()
        .any(|r| r.status == RunStatus::Succeeded)
    {
        return "upstream stale (last success predates the freshness window)".to_owned();
    }
    "upstream has not run".to_owned()
}

/// R7/KTD6: create-time chain validation over a store snapshot. The first
/// hop must exist and must not be a monitor (a monitor retires after one
/// verdict — a dependent on it would wither forever; rejected in v1). The
/// walk then climbs `after` edges: depth beyond [`MAX_CHAIN_DEPTH`] or any
/// revisit (a cycle — only constructible by hand-editing the store file,
/// since edges are set at create only and `fly automation update` can only
/// clear one, automation-update KTD2) rejects. A *dangling* mid-chain edge (an
/// upstream deleted after its dependent was created) ends the walk without
/// error — that chain's honesty is the sweep's job (R8), not this create's
/// fault.
pub fn validate_chain(
    snapshot: &BTreeMap<String, Automation>,
    upstream_id: &str,
) -> Result<(), String> {
    let Some(first) = snapshot.get(upstream_id) else {
        return Err(format!("--after: no such automation: {upstream_id}"));
    };
    if first.monitor {
        return Err(format!(
            "--after: {upstream_id} is a monitor — a monitor retires after one verdict, so a \
             dependent on it would never fire again; depend on a recurring automation instead"
        ));
    }
    let mut visited: HashSet<&str> = HashSet::new();
    visited.insert(upstream_id);
    let mut current = first;
    let mut depth = 1usize;
    while let Some(edge) = &current.after {
        depth += 1;
        if depth > MAX_CHAIN_DEPTH {
            return Err(format!(
                "--after: dependency chain deeper than {MAX_CHAIN_DEPTH} — flatten the pipeline"
            ));
        }
        if !visited.insert(edge.upstream_id.as_str()) {
            return Err(format!(
                "--after: dependency cycle detected through {}",
                edge.upstream_id
            ));
        }
        match snapshot.get(&edge.upstream_id) {
            Some(next) => current = next,
            None => break, // dangling mid-chain edge: the sweep reports it honestly
        }
    }
    Ok(())
}

/// R7: clamp-validate a wire `within_ms`. Hard-rejects rather than silently
/// clamping — the author asked for a specific window and should know it was
/// out of range.
pub fn validate_within(within_ms: u64) -> Result<u64, String> {
    if !(WITHIN_MIN_MS..=WITHIN_MAX_MS).contains(&within_ms) {
        return Err(format!(
            "--within must be between 1 minute and 7 days (got {within_ms} ms)"
        ));
    }
    Ok(within_ms)
}

#[cfg(test)]
mod tests {
    use super::super::model::{
        Mode, Origin, RunOutcome, Trigger, Verdict, VerdictOutcome, DEFAULT_AFTER_WITHIN_MS,
    };
    use super::*;

    fn script_mode() -> Mode {
        Mode::Script {
            script_file: "script".into(),
            interpreter: "bash".into(),
            timeout_ms: 120_000,
        }
    }

    fn automation(id: &str) -> Automation {
        Automation {
            id: id.into(),
            name: format!("{id} name"),
            cron: "0 9 * * *".into(),
            timezone: "UTC".into(),
            enabled: true,
            retry_on_interrupt: false,
            monitor: false,
            not_before_ms: None,
            retired_at: None,
            pickup_pointers: None,
            after: None,
            cwd: "/tmp".into(),
            mode: script_mode(),
            origin: Origin {
                pane_id: 1,
                workspace_id: "ws".into(),
                label: "cli".into(),
            },
            created_at: 0,
            updated_at: 0,
            next_run_at: Some(60_000),
            runs: Vec::new(),
        }
    }

    fn dep(upstream: &str, within_ms: Option<u64>) -> Dependency {
        Dependency {
            upstream_id: upstream.into(),
            within_ms,
        }
    }

    /// Close a fresh claimed run on `a` with the given outcome at `t`.
    fn run_closed(a: &mut Automation, id: &str, outcome: RunOutcome, t: u64) {
        a.claim(a.next_run_at, t.saturating_sub(1), Trigger::Schedule, id, false)
            .unwrap();
        a.close(id, outcome, t);
    }

    fn succeeded(a: &mut Automation, id: &str, t: u64) {
        run_closed(
            a,
            id,
            RunOutcome::Succeeded {
                output: Some("ok".into()),
            },
            t,
        );
    }

    fn failed(a: &mut Automation, id: &str, t: u64) {
        run_closed(
            a,
            id,
            RunOutcome::Failed {
                error: "timed out".into(),
                exit_code: None,
                output: None,
            },
            t,
        );
    }

    const OCC: u64 = 1_000_000;
    const W: u64 = 100_000;

    // R4/KTD5: a fresh, successful, unconsumed upstream run satisfies; the
    // qualifying run's id is what the claim will consume.
    #[test]
    fn fresh_unconsumed_upstream_success_satisfies() {
        let mut up = automation("up");
        succeeded(&mut up, "u1", OCC - 30_000);
        let d = evaluate(&dep("up", Some(W)), &[], Some(&up), OCC, OCC + 1);
        assert_eq!(
            d,
            DepDecision::Satisfied {
                upstream_run_id: "u1".into()
            }
        );
    }

    // The plan's required stale-upstream-refused pin (KTD4): a success
    // older than `occurrence − within` never qualifies — Wait inside the
    // window, then a specific stale withhold.
    #[test]
    fn stale_upstream_refused() {
        let mut up = automation("up");
        succeeded(&mut up, "u1", OCC - W - 1); // 1 ms too old
        let inside = evaluate(&dep("up", Some(W)), &[], Some(&up), OCC, OCC + 10);
        assert_eq!(inside, DepDecision::Wait, "window still open — keep waiting");
        let past = evaluate(&dep("up", Some(W)), &[], Some(&up), OCC, OCC + W);
        assert_eq!(
            past,
            DepDecision::Withhold {
                reason: "upstream stale (last success predates the freshness window)".into()
            }
        );
    }

    // The plan's required dependent-does-not-fire-when-upstream-failed pin
    // (R5): a failed upstream in the window yields Wait (a manual re-run
    // could still land), then an honest withhold quoting the failure.
    #[test]
    fn dependent_does_not_fire_when_upstream_failed() {
        let mut up = automation("up");
        failed(&mut up, "u1", OCC - 10_000);
        let inside = evaluate(&dep("up", Some(W)), &[], Some(&up), OCC, OCC + 10);
        assert_eq!(inside, DepDecision::Wait);
        let past = evaluate(&dep("up", Some(W)), &[], Some(&up), OCC, OCC + W);
        assert_eq!(
            past,
            DepDecision::Withhold {
                reason: "upstream failed (timed out)".into()
            }
        );
    }

    // KTD3: an already-consumed upstream run id never re-satisfies — the
    // exactly-once core, and the withhold names the consumed run.
    #[test]
    fn consumed_upstream_run_is_refused_with_a_naming_reason() {
        let mut up = automation("up");
        succeeded(&mut up, "u1", OCC - 30_000);
        let mut dependent = automation("down");
        dependent
            .claim(None, OCC - 20_000, Trigger::Schedule, "d1", false)
            .unwrap();
        dependent.stamp_upstream("d1", "u1");

        let d = evaluate(
            &dep("up", Some(W)),
            &dependent.runs,
            Some(&up),
            OCC,
            OCC + W,
        );
        assert_eq!(
            d,
            DepDecision::Withhold {
                reason: "no new upstream run (run u1 already consumed)".into()
            }
        );
    }

    // KTD5: a Succeeded upstream row carrying a FAIL verdict is not
    // pipeline success (the honest-verdict rule).
    #[test]
    fn fail_verdict_on_a_succeeded_upstream_run_is_refused() {
        let mut up = automation("up");
        succeeded(&mut up, "u1", OCC - 30_000);
        up.runs.last_mut().unwrap().verdict = Some(Verdict {
            outcome: VerdictOutcome::Fail,
            note: "broken".into(),
        });
        let d = evaluate(&dep("up", Some(W)), &[], Some(&up), OCC, OCC + W);
        assert_eq!(
            d,
            DepDecision::Withhold {
                reason: "upstream succeeded with a FAIL verdict".into()
            }
        );
    }

    // R5: reason specificity — never ran, still in flight, skipped, and the
    // chain-propagating withheld-upstream case.
    #[test]
    fn withhold_reasons_are_specific_and_chain_propagating() {
        let never = automation("up");
        assert_eq!(
            evaluate(&dep("up", Some(W)), &[], Some(&never), OCC, OCC + W),
            DepDecision::Withhold {
                reason: "upstream has not run".into()
            }
        );

        let mut running = automation("up");
        running
            .claim(None, OCC - 5_000, Trigger::Schedule, "u1", false)
            .unwrap();
        assert_eq!(
            evaluate(&dep("up", Some(W)), &[], Some(&running), OCC, OCC + W),
            DepDecision::Withhold {
                reason: "upstream run still in flight at the wait deadline".into()
            }
        );

        let mut skipped = automation("up");
        skipped.skip(OCC - 5_000, Trigger::Schedule, "run in flight", "u1");
        assert_eq!(
            evaluate(&dep("up", Some(W)), &[], Some(&skipped), OCC, OCC + W),
            DepDecision::Withhold {
                reason: "upstream was skipped (run in flight)".into()
            }
        );

        // A→B→C: B itself withheld; C's reason quotes B's, so the human
        // reading C's row learns the root cause.
        let mut withheld = automation("up");
        withheld.withhold(OCC - 5_000, Trigger::Schedule, "upstream failed (timed out)", "u1");
        assert_eq!(
            evaluate(&dep("up", Some(W)), &[], Some(&withheld), OCC, OCC + W),
            DepDecision::Withhold {
                reason: "upstream was withheld: upstream failed (timed out)".into()
            }
        );

        // Deleted upstream: immediate, even inside the window (R4).
        assert_eq!(
            evaluate(&dep("gone", Some(W)), &[], None, OCC, OCC + 1),
            DepDecision::Withhold {
                reason: "upstream gone no longer exists".into()
            }
        );
    }

    // R4: the newest qualifying success wins when several are unconsumed.
    #[test]
    fn newest_unconsumed_success_wins() {
        let mut up = automation("up");
        succeeded(&mut up, "u1", OCC - 40_000);
        succeeded(&mut up, "u2", OCC - 20_000);
        let d = evaluate(&dep("up", Some(W)), &[], Some(&up), OCC, OCC + 1);
        assert_eq!(
            d,
            DepDecision::Satisfied {
                upstream_run_id: "u2".into()
            }
        );
    }

    // R13 (Q7): an interrupted upstream's retry success is a NEW run id —
    // it satisfies once even though the original attempt failed; with the
    // retry's id consumed, nothing further qualifies.
    #[test]
    fn upstream_retry_success_satisfies_exactly_once() {
        let mut up = automation("up");
        failed(&mut up, "u1", OCC - 50_000); // the interrupted original
        up.claim(None, OCC - 20_000, Trigger::Retry, "u1-retry", false)
            .unwrap();
        up.close(
            "u1-retry",
            RunOutcome::Succeeded {
                output: Some("ok".into()),
            },
            OCC - 10_000,
        );

        let first = evaluate(&dep("up", Some(W)), &[], Some(&up), OCC, OCC + 1);
        assert_eq!(
            first,
            DepDecision::Satisfied {
                upstream_run_id: "u1-retry".into()
            }
        );

        let mut dependent = automation("down");
        dependent
            .claim(None, OCC, Trigger::Schedule, "d1", false)
            .unwrap();
        dependent.stamp_upstream("d1", "u1-retry");
        let second = evaluate(
            &dep("up", Some(W)),
            &dependent.runs,
            Some(&up),
            OCC + 60_000,
            OCC + 60_000 + W,
        );
        assert!(
            matches!(second, DepDecision::Withhold { .. }),
            "the retry's run id is consumed — no double fire"
        );
    }

    // R12: the manual flavor converts a would-be Wait into the honest
    // synchronous refusal, and passes a satisfied edge through unchanged.
    #[test]
    fn manual_decision_never_waits() {
        let mut up = automation("up");
        failed(&mut up, "u1", OCC - 10_000);
        let d = manual_decision(&dep("up", Some(W)), &[], Some(&up), OCC);
        assert_eq!(
            d,
            DepDecision::Withhold {
                reason: "upstream failed (timed out)".into()
            }
        );

        let mut ok = automation("up");
        succeeded(&mut ok, "u2", OCC - 10_000);
        assert_eq!(
            manual_decision(&dep("up", Some(W)), &[], Some(&ok), OCC),
            DepDecision::Satisfied {
                upstream_run_id: "u2".into()
            }
        );
    }

    // KTD4: within defaults when unset.
    #[test]
    fn default_window_applies_when_within_is_unset() {
        let occ: u64 = 10 * DEFAULT_AFTER_WITHIN_MS;
        let mut up = automation("up");
        succeeded(&mut up, "u1", occ - DEFAULT_AFTER_WITHIN_MS + 1);
        assert!(matches!(
            evaluate(&dep("up", None), &[], Some(&up), occ, occ + 1),
            DepDecision::Satisfied { .. }
        ));
        let mut stale = automation("up");
        succeeded(&mut stale, "u1", occ - DEFAULT_AFTER_WITHIN_MS - 1);
        assert_eq!(
            evaluate(&dep("up", None), &[], Some(&stale), occ, occ + 1),
            DepDecision::Wait
        );
    }

    // The plan's required cycle-rejected-at-create pin (KTD6): a
    // hand-edited cyclic store is rejected by the create-time walk, as is a
    // chain past the depth cap; a healthy chain and a dangling mid-chain
    // edge pass; a missing or monitor first hop rejects.
    #[test]
    fn cycle_rejected_at_create() {
        let mut map: BTreeMap<String, Automation> = BTreeMap::new();
        // a → b → a (only constructible by editing the store file).
        let mut a = automation("a");
        a.after = Some(dep("b", None));
        let mut b = automation("b");
        b.after = Some(dep("a", None));
        map.insert("a".into(), a);
        map.insert("b".into(), b);
        let err = validate_chain(&map, "a").unwrap_err();
        assert!(err.contains("cycle"), "got: {err}");

        // Self-cycle in a hand-edited record.
        let mut s = automation("s");
        s.after = Some(dep("s", None));
        map.insert("s".into(), s);
        let err = validate_chain(&map, "s").unwrap_err();
        assert!(err.contains("cycle"), "got: {err}");

        // Depth: a chain of MAX_CHAIN_DEPTH + 1 upstreams rejects…
        let mut deep: BTreeMap<String, Automation> = BTreeMap::new();
        for i in 0..=MAX_CHAIN_DEPTH {
            let mut n = automation(&format!("n{i}"));
            if i > 0 {
                n.after = Some(dep(&format!("n{}", i - 1), None));
            }
            deep.insert(format!("n{i}"), n);
        }
        let err = validate_chain(&deep, &format!("n{MAX_CHAIN_DEPTH}")).unwrap_err();
        assert!(err.contains("deeper than"), "got: {err}");
        // …while one hop shorter passes.
        assert!(validate_chain(&deep, &format!("n{}", MAX_CHAIN_DEPTH - 1)).is_ok());

        // Dangling mid-chain edge: fine at create (the sweep reports it).
        let mut dangling: BTreeMap<String, Automation> = BTreeMap::new();
        let mut d = automation("d");
        d.after = Some(dep("ghost", None));
        dangling.insert("d".into(), d);
        assert!(validate_chain(&dangling, "d").is_ok());

        // Missing / monitor first hops reject with specific messages.
        assert!(validate_chain(&dangling, "ghost").unwrap_err().contains("no such"));
        let mut m = automation("m");
        m.monitor = true;
        dangling.insert("m".into(), m);
        assert!(validate_chain(&dangling, "m").unwrap_err().contains("monitor"));
    }

    // R7: the within range is hard-validated, not silently clamped.
    #[test]
    fn within_range_is_hard_validated() {
        assert!(validate_within(WITHIN_MIN_MS - 1).is_err());
        assert!(validate_within(WITHIN_MAX_MS + 1).is_err());
        assert_eq!(validate_within(WITHIN_MIN_MS), Ok(WITHIN_MIN_MS));
        assert_eq!(validate_within(WITHIN_MAX_MS), Ok(WITHIN_MAX_MS));
    }
}
