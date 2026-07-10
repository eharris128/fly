//! Monitor verdict parsing, escalation math, and failure-bundle rendering
//! (U3 of `docs/plans/2026-07-10-001-feat-monitor-handoff-plan.md` — all
//! R-IDs below cite that plan).
//!
//! Everything here is pure text/number work — no I/O, no clocks, no locks —
//! so the whole contract tests without a manager (the [`super::model`]
//! purity rule). The close-path integration (one store mutation that closes
//! the row, stamps the verdict, retires, and records the bundle path; bundle
//! write + alert after lock release, KTD-B) lives in [`super`]
//! (`AutomationManager`), not here.
//!
//! **The verdict-block contract lives in exactly one place**:
//! [`VERDICT_BLOCK_SPEC`]. The U8 skill quotes it verbatim so the parser and
//! the prompt contract cannot drift; [`parse_verdict`] is its implementation.
//!
//! Parsing follows the repo's abstain-on-surprise convention (R2/R5):
//! anything that is not exactly one well-formed block is a not-done check —
//! `None`, never a guess. Two blocks abstain; an unclosed fence abstains; a
//! lowercase or decorated outcome line abstains; a block embedded in
//! surrounding prose parses (the fences are matched per-line).

use super::model::{MonitorPointers, Verdict, VerdictOutcome};

/// The prompt-side verdict-block contract (R2). Quoted **verbatim** by the U8
/// skill file — edit here and there together, nowhere else. The example block
/// inside it is itself parseable by [`parse_verdict`] (tested below), so the
/// spec cannot drift from the parser.
pub const VERDICT_BLOCK_SPEC: &str = r#"End your final message with exactly one fenced verdict block:

```verdict
PASS
<free-text note — one or more lines>
```

The first line inside the fence must be exactly PASS or FAIL — uppercase,
alone on its line. Every line after it up to the closing fence is the
free-text result note. Emit exactly one such block. If the experiment is not
finished yet, emit NO verdict block at all — say it is still running and stop."#;

/// R7: the consecutive verdict-less-failed-check threshold that flags a
/// monitor as broken.
pub const MONITOR_BROKEN_THRESHOLD: usize = 3;

/// R7: whether the derived consecutive-infra-failure count
/// ([`super::model::Automation::consecutive_infra_failures`]) is due a
/// "monitor broken" alert **right now** — i.e. evaluated at the close that
/// produced the newest trailing failure.
///
/// "The counter resets after alerting" with a *derived, never stored* count
/// (the U1 decision — no counter to strand) is represented as modular
/// arithmetic: the alert fires exactly when the count is a positive multiple
/// of [`MONITOR_BROKEN_THRESHOLD`] (3, 6, 9, …). That yields:
///
/// - one alert on the third consecutive failure — counts 4 and 5 stay silent
///   (never a per-failure alert storm);
/// - the implicit post-alert reset: three *further* failures (count 6) ring
///   again, matching R7's "resets after alerting";
/// - self-healing when a close goes unevaluated (e.g. a shutdown-interrupted
///   check closes while the app is exiting): the streak still rings at the
///   next multiple instead of never.
///
/// A clean check or a verdict-bearing row zeroes the derived count itself
/// (the R7 reset — see the U1 walk), so this predicate needs no memory.
pub fn broken_alert_due(consecutive_infra_failures: usize) -> bool {
    consecutive_infra_failures > 0 && consecutive_infra_failures % MONITOR_BROKEN_THRESHOLD == 0
}

/// R2: parse the one machine-readable verdict block out of a check's captured
/// final assistant turn. Returns `None` — a not-done check (R5) — for
/// anything but exactly one well-formed block (abstain-on-surprise):
///
/// - opening fence: a line whose trimmed content is exactly ```` ```verdict ````;
/// - outcome: the first non-blank line inside, exactly `PASS` or `FAIL`
///   (uppercase, nothing else on the line — the wire spelling is
///   [`VerdictOutcome`]'s camelCase, translated here);
/// - note: every line after the outcome up to the closing fence (a line whose
///   trimmed content is exactly ```` ``` ````), inner newlines preserved,
///   outer whitespace trimmed; may be empty;
/// - zero openers, two-plus openers, or a missing closing fence ⇒ `None`.
///
/// Surrounding prose is fine — fences are matched per-line, so a block
/// embedded in a longer message parses. Callers hand this the **full**
/// captured turn, pre-truncation (the R8 tail cap runs after — parse first,
/// cap later).
pub fn parse_verdict(text: &str) -> Option<Verdict> {
    let lines: Vec<&str> = text.lines().collect();
    let openers: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.trim() == "```verdict")
        .map(|(i, _)| i)
        .collect();
    // Exactly one block or nothing (abstain-on-surprise: two blocks could
    // disagree — refuse to pick).
    let [open] = openers[..] else {
        return None;
    };
    // Saturating index math: `text` is untrusted parsed input and release
    // builds have overflow checks off (repo convention).
    let close = (open.saturating_add(1)..lines.len()).find(|&i| lines[i].trim() == "```")?;
    let inner = &lines[open.saturating_add(1)..close];
    let first = inner.iter().position(|l| !l.trim().is_empty())?; // empty block ⇒ abstain
    let outcome = match inner[first].trim() {
        "PASS" => VerdictOutcome::Pass,
        "FAIL" => VerdictOutcome::Fail,
        _ => return None, // decorated/lowercase outcome line ⇒ abstain
    };
    let note = inner[first.saturating_add(1)..].join("\n").trim().to_owned();
    Some(Verdict { outcome, note })
}

/// R15: render the durable failure bundle written for a FAIL verdict — the
/// verdict note, the pickup pointers captured at registration (R11/R4), and
/// the **full** captured final turn as evidence (the bundle lives outside the
/// R8 run-output tail cap; that is its whole point). Pure string assembly;
/// the manager writes it to disk after the close mutation releases the store
/// lock (KTD-B), fail-tolerant.
pub fn render_bundle(
    automation_name: &str,
    automation_id: &str,
    run_id: &str,
    verdict: &Verdict,
    evidence: &str,
    pointers: Option<&MonitorPointers>,
    closed_at_ms: u64,
) -> String {
    let pointers_block = match pointers {
        Some(p) => format!(
            "- sessionId: {}\n- transcriptPath: {}\n- sessionCwd: {}",
            p.session_id, p.transcript_path, p.session_cwd
        ),
        None => "(none captured — pointers are stamped at monitor registration)".to_owned(),
    };
    format!(
        "# Monitor failure bundle — {automation_name}\n\
         \n\
         - automation: {automation_id}\n\
         - run: {run_id}\n\
         - verdict: FAIL\n\
         - closedAtMs: {closed_at_ms}\n\
         \n\
         ## Verdict note\n\
         \n\
         {note}\n\
         \n\
         ## Pickup pointers\n\
         \n\
         {pointers_block}\n\
         \n\
         ## Evidence — the check's full final message\n\
         \n\
         {evidence}\n",
        note = verdict.note,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // R2: a clean PASS block parses to Pass + its note.
    #[test]
    fn parses_a_clean_pass_block() {
        let text = "```verdict\nPASS\nexperiment converged\n```";
        assert_eq!(
            parse_verdict(text),
            Some(Verdict {
                outcome: VerdictOutcome::Pass,
                note: "experiment converged".into(),
            })
        );
    }

    // R2: a clean FAIL block with a multi-line note keeps the note's inner
    // newlines (outer whitespace trimmed).
    #[test]
    fn parses_a_clean_fail_block_with_multiline_note() {
        let text = "```verdict\nFAIL\nloss diverged at step 40\nsee train.log line 88\n```";
        assert_eq!(
            parse_verdict(text),
            Some(Verdict {
                outcome: VerdictOutcome::Fail,
                note: "loss diverged at step 40\nsee train.log line 88".into(),
            })
        );
    }

    // R2/R5: no block at all is a not-done check — abstain.
    #[test]
    fn abstains_when_no_block_present() {
        assert_eq!(parse_verdict("still training; 3 epochs to go"), None);
        assert_eq!(parse_verdict(""), None);
        // A plain code fence is not a verdict fence.
        assert_eq!(parse_verdict("```\nPASS\n```"), None);
    }

    // Abstain-on-surprise: two blocks could disagree — refuse to pick.
    #[test]
    fn abstains_on_two_blocks() {
        let text = "```verdict\nPASS\nok\n```\nand yet\n```verdict\nFAIL\nnot ok\n```";
        assert_eq!(parse_verdict(text), None);
    }

    // R2: a block embedded in surrounding prose parses — fences are matched
    // per-line, so the agent may narrate around its verdict.
    #[test]
    fn parses_a_block_embedded_in_surrounding_prose() {
        let text = "I checked the training run as instructed.\n\n\
                    ```verdict\nFAIL\nprocess died overnight\n```\n\n\
                    Happy to dig further.";
        assert_eq!(
            parse_verdict(text),
            Some(Verdict {
                outcome: VerdictOutcome::Fail,
                note: "process died overnight".into(),
            })
        );
    }

    // Abstain-on-surprise: an unclosed fence never parses.
    #[test]
    fn abstains_on_an_unclosed_fence() {
        assert_eq!(parse_verdict("```verdict\nPASS\nnote without a closing fence"), None);
    }

    // Strict outcome line (R2): lowercase, decorated, or missing outcome
    // lines abstain — the contract is uppercase PASS/FAIL alone on its line.
    #[test]
    fn abstains_on_a_nonconforming_outcome_line() {
        assert_eq!(parse_verdict("```verdict\nPass\nok\n```"), None);
        assert_eq!(parse_verdict("```verdict\nPASS: all good\n```"), None);
        assert_eq!(parse_verdict("```verdict\nthe run PASSED\n```"), None);
        assert_eq!(parse_verdict("```verdict\n```"), None, "empty block");
        assert_eq!(parse_verdict("```verdict\n\n   \n```"), None, "blank block");
    }

    // The note is optional: a bare outcome still parses (blank lines between
    // the fence and the outcome are tolerated).
    #[test]
    fn pass_without_note_parses_with_empty_note() {
        assert_eq!(
            parse_verdict("```verdict\nPASS\n```"),
            Some(Verdict {
                outcome: VerdictOutcome::Pass,
                note: String::new(),
            })
        );
        assert_eq!(
            parse_verdict("```verdict\n\nFAIL\n```"),
            Some(Verdict {
                outcome: VerdictOutcome::Fail,
                note: String::new(),
            })
        );
    }

    // The one-place spec (R2, quoted verbatim by the U8 skill) contains an
    // example block that this parser accepts — the contract cannot drift
    // from its own documentation.
    #[test]
    fn the_spec_texts_own_example_parses() {
        let v = parse_verdict(VERDICT_BLOCK_SPEC).expect("the spec's example block parses");
        assert_eq!(v.outcome, VerdictOutcome::Pass);
        assert_eq!(v.note, "<free-text note — one or more lines>");
    }

    // R7: the derived "reset after alerting" — the alert fires at each
    // positive multiple of the threshold and nowhere else, so the third
    // consecutive failure rings exactly once and three further failures ring
    // again.
    #[test]
    fn broken_alert_fires_at_each_positive_multiple_of_three() {
        assert!(!broken_alert_due(0));
        assert!(!broken_alert_due(1));
        assert!(!broken_alert_due(2));
        assert!(broken_alert_due(3), "third consecutive failure rings");
        assert!(!broken_alert_due(4), "no per-failure alert storm");
        assert!(!broken_alert_due(5));
        assert!(broken_alert_due(6), "the post-alert reset: three more ring again");
    }

    // R15: the bundle carries the verdict note, the pickup pointers, and the
    // full evidence text.
    #[test]
    fn bundle_renders_note_pointers_and_evidence() {
        let v = Verdict {
            outcome: VerdictOutcome::Fail,
            note: "loss diverged".into(),
        };
        let p = MonitorPointers {
            session_id: "sess-9".into(),
            transcript_path: "/home/u/.claude/projects/x/sess-9.jsonl".into(),
            session_cwd: "/home/u/exp".into(),
        };
        let b = render_bundle("train watch", "a1", "r1", &v, "full final turn here", Some(&p), 70_000);
        assert!(b.contains("train watch"));
        assert!(b.contains("- automation: a1"));
        assert!(b.contains("- run: r1"));
        assert!(b.contains("- closedAtMs: 70000"));
        assert!(b.contains("loss diverged"));
        assert!(b.contains("sessionId: sess-9"));
        assert!(b.contains("transcriptPath: /home/u/.claude/projects/x/sess-9.jsonl"));
        assert!(b.contains("sessionCwd: /home/u/exp"));
        assert!(b.contains("full final turn here"));
    }

    // R15 degraded shape: a monitor without pointers (defensive — U4 refuses
    // such creates) still renders a usable bundle.
    #[test]
    fn bundle_notes_missing_pointers() {
        let v = Verdict {
            outcome: VerdictOutcome::Fail,
            note: "died".into(),
        };
        let b = render_bundle("w", "a1", "r1", &v, "evidence", None, 1);
        assert!(b.contains("(none captured"));
        assert!(b.contains("evidence"));
    }
}
