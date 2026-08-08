//! Monitor verdict parsing, escalation math, and failure-bundle rendering
//! (U3 of `docs/plans/2026-07-10-002-feat-monitor-handoff-plan.md` — all
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
//! The two now differ by **exactly one outcome, on purpose**: the parser also
//! accepts `DECLINED` (fly-dag-primitives G1, for verdict-gated non-monitor
//! legs), while the spec deliberately still lists only PASS/FAIL, because a
//! monitor is a done/not-done instrument and never retires on a decline (G1
//! KTD6 — filtered at the monitor close path in [`super`]). Keep it to that
//! one difference: spec and skill stay byte-identical to each other.
//!
//! Parsing follows the repo's abstain-on-surprise convention (R2/R5):
//! anything that is not exactly one well-formed block is a not-done check —
//! `None`, never a guess. Two blocks abstain; an unclosed fence abstains; a
//! lowercase or decorated outcome line abstains; a block embedded in
//! surrounding prose parses (the fences are matched per-line).

use super::model::{MonitorPointers, Verdict, VerdictOutcome};

/// The prompt-side verdict-block contract (R2). Quoted **verbatim** by the U8
/// skill file — edit here and there together, nowhere else. The example block
/// inside it deliberately does **not** parse: its note is
/// [`SPEC_NOTE_PLACEHOLDER`], which [`parse_verdict`] treats as an echo of the
/// spec, not a verdict (fix(review) #3) — a check that merely quotes these
/// instructions back must never read as a spurious PASS and retire the
/// monitor. The abstention is tested below, so spec and parser cannot drift.
pub const VERDICT_BLOCK_SPEC: &str = r#"End your final message with exactly one fenced verdict block:

```verdict
PASS
<free-text note — one or more lines>
```

The first line inside the fence must be exactly PASS or FAIL — uppercase,
alone on its line. Every line after it up to the closing fence is the
free-text result note. Emit exactly one such block. If the experiment is not
finished yet, emit NO verdict block at all — say it is still running and stop."#;

/// The example note line inside [`VERDICT_BLOCK_SPEC`], verbatim — defined
/// once here so the spec text and the echo guard in [`parse_verdict`] cannot
/// drift (a test asserts the spec contains this exact line). A parsed block
/// whose note equals it is the spec quoted back, not a real verdict —
/// abstain (fix(review) #3).
const SPEC_NOTE_PLACEHOLDER: &str = "<free-text note — one or more lines>";

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
///   next multiple — **until the run-history cap saturates the derived
///   count** (fix(review) #13). With the 20-row cap (`model::RUN_HISTORY_CAP`,
///   R8) the count can never exceed 20, so an unbroken streak rings six
///   times (3, 6, 9, 12, 15, 18) and then goes silent: the cap pins the
///   count at 20, which is not a multiple of three. Deliberate honesty, not
///   a to-fix: the user has been rung six times, and a monitor stuck that
///   long is loudly broken already.
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
/// - outcome: the first non-blank line inside, exactly `PASS`, `FAIL`, or
///   `DECLINED` (uppercase, nothing else on the line — the wire spelling is
///   [`VerdictOutcome`]'s camelCase, translated here; `DECLINED` is the
///   fly-dag-primitives G1 non-monitor outcome);
/// - note: every line after the outcome up to the closing fence (a line whose
///   trimmed content is exactly ```` ``` ````), inner newlines preserved,
///   outer whitespace trimmed; may be empty;
/// - zero openers, two-plus openers, or a missing closing fence ⇒ `None`;
/// - a note equal to [`SPEC_NOTE_PLACEHOLDER`] ⇒ `None` — the block is
///   [`VERDICT_BLOCK_SPEC`]'s own example quoted back (a check echoing its
///   instructions), never a real verdict (fix(review) #3).
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
        .filter(|(_, l)| is_verdict_opener(l))
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
        // fly-dag-primitives G1: the third outcome, emitted by verdict-gated
        // NON-monitor legs to mean "ran, nothing to do". Monitors never retire
        // on it (filtered at the monitor close path, G1 KTD6); the monitor
        // prompt contract [`VERDICT_BLOCK_SPEC`] deliberately still lists only
        // PASS/FAIL, so a monitor is never told to emit it.
        "DECLINED" => VerdictOutcome::Declined,
        _ => return None, // decorated/lowercase outcome line ⇒ abstain
    };
    let note = inner[first.saturating_add(1)..].join("\n").trim().to_owned();
    if note == SPEC_NOTE_PLACEHOLDER {
        // Echo guard (fix(review) #3): this is the spec's own example block —
        // a check quoting its instructions back, not a delivered verdict.
        return None;
    }
    Some(Verdict { outcome, note })
}

/// The one verdict-fence opener detection (a line whose trimmed content is
/// exactly ```` ```verdict ````), shared by [`parse_verdict`] and
/// [`contains_verdict_opener`] so the two cannot drift.
fn is_verdict_opener(line: &str) -> bool {
    line.trim() == "```verdict"
}

/// Whether `text` contains a verdict-fence **opener** line — the same
/// per-line detection [`parse_verdict`] uses (fix(review) #5). The escalation
/// walk ([`super::model::Automation::consecutive_infra_failures`]) treats a
/// concluded check that OPENED a verdict block yet yielded no parseable
/// verdict (decorated/lowercase outcome, unclosed fence, two blocks) as an
/// unreadable check rather than a healthy not-done one: persistent
/// near-misses must escalate to a visible "monitor broken", never run silent
/// forever (the plan's Risks promise).
pub fn contains_verdict_opener(text: &str) -> bool {
    text.lines().any(is_verdict_opener)
}

/// The run-identifying half of a failure bundle (R15) — grouped so the three
/// same-typed `&str` identifiers can't be transposed at the call site (the
/// compiler catches a swap; a positional list would not).
pub struct BundleContext<'a> {
    pub automation_name: &'a str,
    pub automation_id: &'a str,
    pub run_id: &'a str,
    pub closed_at_ms: u64,
}

/// Headless-monitor-checks U5 (its R12): the failing **check's own**
/// diagnostic session — the stream-derived session id stamped on the closed
/// run row plus the transcript path derived from it
/// (`~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`, the plan's
/// Empirical Contract). The caller derives the path via
/// `session::transcript::claude_project_dir` (the one home of the cwd
/// encoding — never reimplemented here; this module stays pure) and
/// sanitizes the id at build time (the R16 rendered-document posture — the
/// id is untrusted stream JSON). `transcript_path` is `None` when no
/// home/config root resolves; the block then renders the id alone.
///
/// Rendered as its own labeled block, deliberately **distinct from the
/// registration-time "Pickup pointers"**: the pointers are the parked
/// experiment's parent session (the pickup target); this is the check run
/// that produced the verdict (the diagnostic). An operator triaging a FAIL
/// must never conflate the two.
pub struct CheckSession {
    pub session_id: String,
    pub transcript_path: Option<String>,
}

/// R15: render the durable failure bundle written for a FAIL verdict — the
/// verdict note, the pickup pointers captured at registration (R11/R4), the
/// failing check's own session block when one is known
/// ([`CheckSession`], headless-monitor-checks U5 — omitted entirely when the
/// close carried no session id, e.g. a legacy row closed without an `init`
/// event), and the captured final turn as evidence (the bundle lives outside
/// the R8 run-output tail cap; the caller applies its own generous evidence
/// cap). Pure string assembly; the manager writes it to disk after the close
/// mutation releases the store lock (KTD-B), fail-tolerant.
pub fn render_bundle(
    ctx: &BundleContext,
    verdict: &Verdict,
    evidence: &str,
    pointers: Option<&MonitorPointers>,
    check_session: Option<&CheckSession>,
) -> String {
    let pointers_block = match pointers {
        Some(p) => format!(
            "- sessionId: {}\n- transcriptPath: {}\n- sessionCwd: {}",
            p.session_id, p.transcript_path, p.session_cwd
        ),
        None => "(none captured — pointers are stamped at monitor registration)".to_owned(),
    };
    let check_session_block = match check_session {
        Some(cs) => {
            let path_line = match &cs.transcript_path {
                Some(p) => format!("\n- transcriptPath: {p}"),
                None => String::new(),
            };
            format!(
                "\n## Check session\n\
                 \n\
                 The failing check's own session (diagnostic) — NOT a pickup target;\n\
                 pick up from the pointers above.\n\
                 \n\
                 - sessionId: {}{path_line}\n",
                cs.session_id
            )
        }
        None => String::new(),
    };
    format!(
        "# Monitor failure bundle — {name}\n\
         \n\
         - automation: {id}\n\
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
         {check_session_block}\
         \n\
         ## Evidence — the check's full final message\n\
         \n\
         {evidence}\n",
        name = ctx.automation_name,
        id = ctx.automation_id,
        run_id = ctx.run_id,
        closed_at_ms = ctx.closed_at_ms,
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

    // fly-dag-primitives G1: DECLINED is the third outcome — it parses like
    // PASS/FAIL (uppercase, alone on its line), keeping its free-text note.
    #[test]
    fn parses_a_declined_block() {
        assert_eq!(
            parse_verdict("```verdict\nDECLINED\nno candidates in the queue today\n```"),
            Some(Verdict {
                outcome: VerdictOutcome::Declined,
                note: "no candidates in the queue today".into(),
            })
        );
    }

    // G1: DECLINED obeys the same strict outcome-line rule — lowercase or
    // decorated forms abstain (the abstain-on-surprise convention extends
    // unchanged).
    #[test]
    fn declined_is_strict_uppercase_alone_on_its_line() {
        assert_eq!(parse_verdict("```verdict\ndeclined\nx\n```"), None);
        assert_eq!(parse_verdict("```verdict\nDECLINED: nothing to do\n```"), None);
        assert_eq!(parse_verdict("```verdict\nnothing to DECLINE\n```"), None);
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

    // The echo guard (fix(review) #3): the one-place spec (R2, quoted
    // verbatim by the U8 skill) contains an example block whose note is the
    // placeholder — a check that merely quotes the spec back must abstain,
    // never retire the monitor on a spurious PASS. The containment assertion
    // pins the placeholder const to the spec text so they cannot drift.
    #[test]
    fn the_spec_texts_own_example_abstains_as_an_echo_guard() {
        assert!(
            VERDICT_BLOCK_SPEC.contains(SPEC_NOTE_PLACEHOLDER),
            "the guard's placeholder is the spec's example note line, verbatim"
        );
        assert_eq!(
            parse_verdict(VERDICT_BLOCK_SPEC),
            None,
            "the spec's own example is an echo, not a verdict"
        );
    }

    // The echo guard is exact-match only: a PASS block with a REAL note —
    // even one that merely resembles or mentions the placeholder — parses.
    #[test]
    fn a_pass_block_with_a_real_note_still_parses_past_the_echo_guard() {
        assert_eq!(
            parse_verdict("```verdict\nPASS\nfree-text note: run converged\n```"),
            Some(Verdict {
                outcome: VerdictOutcome::Pass,
                note: "free-text note: run converged".into(),
            })
        );
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
        let ctx = BundleContext {
            automation_name: "train watch",
            automation_id: "a1",
            run_id: "r1",
            closed_at_ms: 70_000,
        };
        let b = render_bundle(&ctx, &v, "full final turn here", Some(&p), None);
        assert!(b.contains("train watch"));
        assert!(b.contains("- automation: a1"));
        assert!(b.contains("- run: r1"));
        assert!(b.contains("- closedAtMs: 70000"));
        assert!(b.contains("loss diverged"));
        assert!(b.contains("sessionId: sess-9"));
        assert!(b.contains("transcriptPath: /home/u/.claude/projects/x/sess-9.jsonl"));
        assert!(b.contains("sessionCwd: /home/u/exp"));
        assert!(b.contains("full final turn here"));
        // No session id on the close (pane-era/legacy edge) ⇒ the Check
        // session block is omitted entirely, never rendered empty.
        assert!(!b.contains("Check session"));
    }

    // Headless-monitor-checks U5 (its R12): the failing check's own session
    // rides the bundle as a clearly-labeled block — id + derived transcript
    // path — distinct from the registration-time pickup pointers, so an
    // operator never confuses the check's diagnostic session with the parent
    // pickup target.
    #[test]
    fn bundle_renders_the_check_session_block_distinct_from_pickup_pointers() {
        let v = Verdict {
            outcome: VerdictOutcome::Fail,
            note: "loss diverged".into(),
        };
        let p = MonitorPointers {
            session_id: "sess-parent".into(),
            transcript_path: "/home/u/.claude/projects/x/sess-parent.jsonl".into(),
            session_cwd: "/home/u/exp".into(),
        };
        let cs = CheckSession {
            session_id: "sess-check".into(),
            transcript_path: Some("/home/u/.claude/projects/x/sess-check.jsonl".into()),
        };
        let ctx = BundleContext {
            automation_name: "train watch",
            automation_id: "a1",
            run_id: "r1",
            closed_at_ms: 70_000,
        };
        let b = render_bundle(&ctx, &v, "evidence", Some(&p), Some(&cs));
        assert!(b.contains("## Check session"), "its own labeled block");
        assert!(b.contains("- sessionId: sess-check"));
        assert!(b.contains(
            "- transcriptPath: /home/u/.claude/projects/x/sess-check.jsonl"
        ));
        assert!(
            b.contains("NOT a pickup target"),
            "the block itself disambiguates against the pointers"
        );
        // Both blocks coexist; the pickup pointers are untouched.
        assert!(b.contains("## Pickup pointers"));
        assert!(b.contains("- sessionId: sess-parent"));
        // Ordering: pointers first (the pickup target leads), then the
        // check's diagnostic session, then the evidence.
        let pointers_at = b.find("## Pickup pointers").unwrap();
        let check_at = b.find("## Check session").unwrap();
        let evidence_at = b.find("## Evidence").unwrap();
        assert!(pointers_at < check_at && check_at < evidence_at);
    }

    // U5 degraded shape: a check session whose transcript path could not be
    // derived (no home/config root) still renders its id — the path line is
    // simply absent, never a blank value.
    #[test]
    fn check_session_without_a_derivable_path_renders_the_id_alone() {
        let v = Verdict {
            outcome: VerdictOutcome::Fail,
            note: "died".into(),
        };
        let cs = CheckSession {
            session_id: "sess-check".into(),
            transcript_path: None,
        };
        let ctx = BundleContext {
            automation_name: "w",
            automation_id: "a1",
            run_id: "r1",
            closed_at_ms: 1,
        };
        let b = render_bundle(&ctx, &v, "evidence", None, Some(&cs));
        assert!(b.contains("- sessionId: sess-check"));
        let check_block = &b[b.find("## Check session").unwrap()..];
        assert!(
            !check_block.contains("transcriptPath"),
            "no path line inside the check block when none derived"
        );
    }

    // R15 degraded shape: a monitor without pointers (defensive — U4 refuses
    // such creates) still renders a usable bundle.
    #[test]
    fn bundle_notes_missing_pointers() {
        let v = Verdict {
            outcome: VerdictOutcome::Fail,
            note: "died".into(),
        };
        let ctx = BundleContext {
            automation_name: "w",
            automation_id: "a1",
            run_id: "r1",
            closed_at_ms: 1,
        };
        let b = render_bundle(&ctx, &v, "evidence", None, None);
        assert!(b.contains("(none captured"));
        assert!(b.contains("evidence"));
    }
}
