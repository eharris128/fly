//! Headless monitor checks — the pure stream/outcome core (U1 of
//! `docs/plans/2026-07-11-003-feat-headless-monitor-checks-plan.md`; all
//! R-IDs below cite that plan).
//!
//! A monitor check runs as a backend-owned `claude -p --output-format
//! stream-json --verbose` child; this module is the **process-free half** of
//! that runner: the tolerant NDJSON event view ([`parse_line`] →
//! [`StreamEvent`], R11) and the pure infra-vs-readable outcome
//! classification ([`StreamFold`] → [`CheckOutcome`], R10). The
//! process-owning half (spawn, pipe reading, deadline, kill discipline — U3)
//! joins this module later and drives these types; no process or IO type
//! appears here — exit/timeout/kill facts arrive as plain values
//! ([`ExitFacts`]) — so the whole contract tests without a child process
//! (the [`super::model`] purity rule).
//!
//! **The stream contract is empirical, not documented** (upstream issue
//! #24612), pinned live against claude 2.1.207 in the plan's Empirical
//! Contract. Only `type:"system"`/`subtype:"init"` (which carries the
//! check's `session_id`, R12) and `type:"result"` are depended on; every
//! other shape — including event types and subtypes unheard of today, and
//! events arriving *after* the `result` (a `task_notification` was observed
//! there) — is [`StreamEvent::Other`] and ignored. Classification follows
//! the repo's abstain-on-surprise convention, degraded to infra (the
//! "Tolerant NDJSON parsing, abstain-to-infra" KTD): anything surprising —
//! an oversized or unparsable line that swallowed the `result`, a second
//! `result` event ([`super::verdict`]'s two-blocks-abstain rule applied to
//! the stream), a non-success result, EOF without a result — classifies
//! [`CheckOutcome::Infra`], never a fabricated verdict. A future claude that
//! breaks the contract therefore reads as "monitor broken" within three
//! checks (the existing escalation), never as a silent or wrong verdict.
//!
//! Lines are handled as **bytes** end to end: the runner splits at line
//! boundaries and each complete line's bytes are parsed as JSON directly
//! (`serde_json::from_slice`), so a multibyte UTF-8 character split across
//! read chunks *inside* a line is never lossy-converted — invalid UTF-8 is
//! simply an unparsable line, skipped like any other.

use serde_json::Value;

/// R11: the per-line cap. A line longer than this is skipped **without
/// parsing** ([`parse_line`] returns `None`); if the skipped line was the
/// `result`, the stream ends result-less and classifies
/// [`CheckOutcome::Infra`]. The runner (U3) also enforces this cap at read
/// time — a bounded reader may hand the fold a truncated over-cap line,
/// which then fails JSON parsing and is skipped all the same.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

/// The minimal event view over one NDJSON stream line (R11): only the two
/// shapes the check depends on are distinguished; everything else is
/// [`StreamEvent::Other`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    /// `type:"system", subtype:"init"` — the stream's opening event. Its
    /// `session_id` (R12) is stamped on the run row and rides the FAIL
    /// bundle alongside the derived transcript path; `None` when the field
    /// is missing or not a string (tolerated, not a surprise).
    Init { session_id: Option<String> },
    /// `type:"result"` — the check's terminal event. `success` is
    /// `subtype == "success"` AND `is_error` is not `true`; any other
    /// combination (unknown subtype, missing subtype, `is_error: true`)
    /// is a non-success result, which classification treats as infra
    /// (abstain-on-surprise — only `"success"` was observed live, and
    /// non-success subtypes are refused by convention, not observation).
    /// `text` is the `result` field's string, or empty when missing.
    Result { success: bool, text: String },
    /// Anything else — `assistant`, `rate_limit_event`, the observed system
    /// subtypes (`hook_started`, `thinking_tokens`, `task_notification`, …),
    /// and every shape not yet invented. Ignored by the fold.
    Other,
}

/// R11: parse one complete stream line (bytes, without or with its trailing
/// newline) into the minimal event view. Returns `None` for a **skipped**
/// line — over the [`MAX_LINE_BYTES`] cap (never parsed at all) or not valid
/// JSON — and `Some(StreamEvent::Other)` for valid JSON of any unknown
/// shape. Bytes go to `serde_json` directly, never through a lossy string
/// conversion (see the module doc on UTF-8).
pub fn parse_line(line: &[u8]) -> Option<StreamEvent> {
    if line.len() > MAX_LINE_BYTES {
        return None;
    }
    let v: Value = serde_json::from_slice(line).ok()?;
    Some(match v.get("type").and_then(Value::as_str) {
        Some("system") if v.get("subtype").and_then(Value::as_str) == Some("init") => {
            StreamEvent::Init {
                session_id: v.get("session_id").and_then(Value::as_str).map(str::to_owned),
            }
        }
        Some("result") => StreamEvent::Result {
            success: v.get("subtype").and_then(Value::as_str) == Some("success")
                && v.get("is_error").and_then(Value::as_bool) != Some(true),
            text: v
                .get("result")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        },
        _ => StreamEvent::Other,
    })
}

/// How the child's process side ended, as observed by the runner (U3) and
/// handed to [`StreamFold::finish`] as plain values — the exit status /
/// timeout / runner-initiated-kill flags of R10. Spawn failure is not a
/// variant: a child that never spawned has no stream to fold, so the runner
/// constructs that infra row directly via [`CheckOutcome::spawn_failed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitFacts {
    /// The child exited on its own — **spontaneously**, not by any runner
    /// kill. `code` follows `std::process::ExitStatus::code()`: `Some` on a
    /// normal exit, `None` when a signal (that the runner did not send)
    /// killed it.
    Exited { code: Option<i32> },
    /// The runner's deadline (R4, `RUN_DEADLINE_MS`) lapsed and the runner
    /// killed the child. Always infra (R10's timeout row) — even after a
    /// success result, because the runner's stream-end policy kills a
    /// lingering child long before the deadline, so reaching it with a
    /// result in hand means something surprising happened.
    TimedOut,
    /// The runner's own lingering-exit kill (R10): a success `result` was
    /// already streamed, the child lingered (the observed backgrounding
    /// quirk — result early, claude stays on a task), and the runner killed
    /// it under its stream-end policy. This flag is what keeps such a check
    /// Clean; without it every healthy backgrounding check would falsely
    /// infra-fail and ring "monitor broken" in three.
    LingerKilled,
}

/// R10: the classified outcome of one headless check.
///
/// **Sanitize/scrub contract (R8) — read before storing either field.**
/// Nothing in this module is cleaned: [`CheckOutcome::Clean`]'s `text` is
/// the result event's raw text, and [`CheckOutcome::Infra`]'s `reason` may
/// embed the caller-supplied stderr tail **verbatim** — raw child output,
/// control characters and secrets included. The caller MUST route both
/// through the shared sanitize → scrub helper (U5) before they land on a
/// run row, a bundle, the dashboard, or an alert line; the stderr tail
/// itself is passed *into* [`StreamFold::finish`] already bounded by the
/// runner (a small tail, never the full pipe), but bounded is not clean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// A readable check: exactly one success `result` was streamed and the
    /// process end corroborates it (exit 0 or the runner's own
    /// [`ExitFacts::LingerKilled`]). `text` is the result's exact text —
    /// possibly empty: the empty→`None` mapping happens downstream in the
    /// shared cleaning helper (U4 asserts the end-to-end parity with an
    /// empty pane capture), never here. Whether `text` carries a verdict
    /// block is downstream's concern too ([`super::verdict`]); no-verdict
    /// Clean is a healthy not-done check. `session_id` comes from the
    /// stream's `init` event (R12), `None` if none was seen.
    Clean {
        text: String,
        session_id: Option<String>,
    },
    /// An infra-unreadable check (R10): the run closes Failed and feeds the
    /// existing broken-monitor escalation. `reason` is descriptive and may
    /// embed raw stderr — see the sanitize/scrub contract above.
    Infra { reason: String },
}

impl CheckOutcome {
    /// R10's spawn-failure row: the child never spawned, so there is no
    /// stream to fold — the runner (U3) builds this infra outcome directly
    /// from the spawn error's text.
    pub fn spawn_failed(error: &str) -> CheckOutcome {
        CheckOutcome::Infra {
            reason: format!("spawn failed: {error}"),
        }
    }
}

/// The first `result` event's payload, retained by the fold until
/// [`StreamFold::finish`] classifies it.
#[derive(Debug, Clone)]
struct FirstResult {
    success: bool,
    text: String,
}

/// R10: the pure fold over the parsed event sequence. Feed each event (or
/// raw line) as it arrives, then settle with the end-of-stream facts:
///
/// ```text
/// let mut fold = StreamFold::new();
/// for line in lines { fold.feed_line(line); }
/// let outcome = fold.finish(exit_facts, stderr_tail);
/// ```
///
/// Memory is O(1) in the stream length: only the first `init`'s session id
/// and the first `result`'s text are retained — a second `result` merely
/// sets a surprise flag (its text is dropped), and [`StreamEvent::Other`]
/// carries nothing — so an unbounded or misbehaving stream cannot balloon
/// the fold (R11's tolerance without the memory bill).
#[derive(Debug, Default, Clone)]
pub struct StreamFold {
    /// From the first `init` seen; later `init`s (never observed live) are
    /// ignored — first-wins keeps the id stable, and R12 needs one id.
    session_id: Option<String>,
    result: Option<FirstResult>,
    /// A second `result` event was seen — a surprise ([`super::verdict`]'s
    /// two-blocks-abstain convention applied to the stream) → Infra.
    extra_result: bool,
}

impl StreamFold {
    pub fn new() -> StreamFold {
        StreamFold::default()
    }

    /// Parse one raw line ([`parse_line`]) and feed it; a skipped line
    /// (over-cap or unparsable, R11) changes nothing.
    pub fn feed_line(&mut self, line: &[u8]) {
        if let Some(event) = parse_line(line) {
            self.feed(event);
        }
    }

    /// Fold one parsed event.
    pub fn feed(&mut self, event: StreamEvent) {
        match event {
            StreamEvent::Init { session_id } => {
                if self.session_id.is_none() {
                    self.session_id = session_id;
                }
            }
            StreamEvent::Result { success, text } => {
                if self.result.is_some() {
                    self.extra_result = true;
                } else {
                    self.result = Some(FirstResult { success, text });
                }
            }
            StreamEvent::Other => {}
        }
    }

    /// R10's classification table, one row per arm. `stderr_tail` is the
    /// runner-bounded raw stderr tail, embedded verbatim in the
    /// nonzero-exit/signal reasons (empty tail ⇒ omitted) — see
    /// [`CheckOutcome`]'s sanitize/scrub contract for why raw is safe *here*
    /// and nowhere downstream.
    pub fn finish(self, end: ExitFacts, stderr_tail: &str) -> CheckOutcome {
        let infra = |reason: String| CheckOutcome::Infra { reason };
        let with_tail = |msg: String| {
            if stderr_tail.is_empty() {
                msg
            } else {
                format!("{msg}; stderr: {stderr_tail}")
            }
        };
        // Surprise first: two results could disagree — refuse to pick,
        // whatever the exit looked like.
        if self.extra_result {
            return infra("malformed stream: more than one result event".to_owned());
        }
        match (self.result, end) {
            // Malformed stream (R10/R11): EOF with no result — including the
            // over-cap/unparsable-line case that swallowed it.
            (None, ExitFacts::Exited { code: Some(0) }) => {
                infra("malformed stream: no result event before exit 0".to_owned())
            }
            (None, ExitFacts::Exited { code: Some(code) }) => {
                infra(with_tail(format!("exited {code} with no result event")))
            }
            (None, ExitFacts::Exited { code: None }) => {
                infra(with_tail("killed by a signal with no result event".to_owned()))
            }
            (None, ExitFacts::TimedOut) => {
                infra("timed out: killed at the run deadline with no result event".to_owned())
            }
            // Defensive: the runner only linger-kills after a result, so this
            // combination is itself a surprise → infra.
            (None, ExitFacts::LingerKilled) => {
                infra("malformed stream: runner kill with no result event".to_owned())
            }
            // A non-success result is infra regardless of how the process
            // ended (abstain-on-surprise) — never Clean, even on exit 0.
            (Some(r), _) if !r.success => {
                infra("stream reported a non-success result event".to_owned())
            }
            // The Clean rows (R10): a sole success result corroborated by a
            // clean exit or by the runner's own lingering-exit kill.
            (Some(r), ExitFacts::Exited { code: Some(0) } | ExitFacts::LingerKilled) => {
                CheckOutcome::Clean {
                    text: r.text,
                    session_id: self.session_id,
                }
            }
            // R10's timeout row is flat: a deadline kill is not the linger
            // kill (see [`ExitFacts::TimedOut`]).
            (Some(_), ExitFacts::TimedOut) => {
                infra("timed out: killed at the run deadline after its result event".to_owned())
            }
            // R10, verbatim: "a *spontaneous* nonzero exit remains infra" —
            // only the runner's own kill exempts a post-result death.
            (Some(_), ExitFacts::Exited { code: Some(code) }) => {
                infra(with_tail(format!("spontaneous exit {code} after the result event")))
            }
            (Some(_), ExitFacts::Exited { code: None }) => {
                infra(with_tail("spontaneous signal death after the result event".to_owned()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fold a whole stream of byte lines and settle it — the runner's shape.
    fn classify(lines: &[&[u8]], end: ExitFacts, stderr_tail: &str) -> CheckOutcome {
        let mut fold = StreamFold::new();
        for line in lines {
            fold.feed_line(line);
        }
        fold.finish(end, stderr_tail)
    }

    fn reason(outcome: CheckOutcome) -> String {
        match outcome {
            CheckOutcome::Infra { reason } => reason,
            other => panic!("expected Infra, got {other:?}"),
        }
    }

    // The captured real stream shape (Empirical Contract, claude 2.1.207):
    // init + assistant + rate_limit_event + result → Clean with the exact
    // result text and the init's session id (R10, R12).
    #[test]
    fn captured_real_stream_is_clean_with_exact_text_and_session_id() {
        let lines: &[&[u8]] = &[
            br#"{"type":"system","subtype":"init","cwd":"/home/u/exp","session_id":"5b1e2c7a-9d1f-4b6e-8a2e-0c3d4e5f6a7b","tools":["Bash","Read"],"model":"claude-sonnet-4-5-20250929","permissionMode":"bypassPermissions","apiKeySource":"none"}"#,
            br#"{"type":"assistant","message":{"id":"msg_014abc","type":"message","role":"assistant","model":"claude-sonnet-4-5-20250929","content":[{"type":"text","text":"Checking the experiment now."}],"stop_reason":"end_turn"},"session_id":"5b1e2c7a-9d1f-4b6e-8a2e-0c3d4e5f6a7b"}"#,
            br#"{"type":"rate_limit_event","rate_limit":{"status":"allowed","unifiedRateLimitFallbackAvailable":false}}"#,
            br#"{"type":"result","subtype":"success","is_error":false,"duration_ms":6321,"duration_api_ms":5100,"num_turns":3,"result":"The run is healthy; loss still falling.","session_id":"5b1e2c7a-9d1f-4b6e-8a2e-0c3d4e5f6a7b","total_cost_usd":0.0421,"usage":{"input_tokens":4,"output_tokens":128}}"#,
        ];
        assert_eq!(
            classify(lines, ExitFacts::Exited { code: Some(0) }, ""),
            CheckOutcome::Clean {
                text: "The run is healthy; loss still falling.".to_owned(),
                session_id: Some("5b1e2c7a-9d1f-4b6e-8a2e-0c3d4e5f6a7b".to_owned()),
            }
        );
    }

    // R11: unknown event types and system subtypes are ignored — including
    // the observed wild ones (`thinking_tokens`, `hook_started`) and events
    // AFTER the result (a `task_notification` was observed there live).
    // Result-then-more-events is still Clean.
    #[test]
    fn unknown_types_and_subtypes_are_ignored_even_after_the_result() {
        let lines: &[&[u8]] = &[
            br#"{"type":"system","subtype":"init","session_id":"s-1","model":"m","cwd":"/x"}"#,
            br#"{"type":"system","subtype":"hook_started","hook":"SessionStart"}"#,
            br#"{"type":"system","subtype":"thinking_tokens","tokens":512}"#,
            br#"{"type":"telemetry","subtype":"v99","payload":{"future":"shape"}}"#,
            br#"{"type":"result","subtype":"success","is_error":false,"result":"done","session_id":"s-1","num_turns":1}"#,
            br#"{"type":"system","subtype":"task_notification","task_id":"t1","status":"completed"}"#,
        ];
        assert_eq!(
            classify(lines, ExitFacts::Exited { code: Some(0) }, ""),
            CheckOutcome::Clean {
                text: "done".to_owned(),
                session_id: Some("s-1".to_owned()),
            }
        );
    }

    // Abstain-on-surprise (the verdict two-blocks rule, applied to the
    // stream): a second result event → Infra, even with a clean exit and
    // even though both results claimed success.
    #[test]
    fn a_second_result_event_is_a_surprise_infra() {
        let lines: &[&[u8]] = &[
            br#"{"type":"result","subtype":"success","is_error":false,"result":"first"}"#,
            br#"{"type":"result","subtype":"success","is_error":false,"result":"second"}"#,
        ];
        let r = reason(classify(lines, ExitFacts::Exited { code: Some(0) }, ""));
        assert!(r.contains("more than one result"), "reason: {r}");
    }

    // R10 malformed-stream row: EOF with no result event, clean exit → Infra.
    #[test]
    fn eof_with_no_result_and_exit_zero_is_infra() {
        let lines: &[&[u8]] = &[
            br#"{"type":"system","subtype":"init","session_id":"s-1"}"#,
            br#"{"type":"assistant","message":{"content":[{"type":"text","text":"hm"}]}}"#,
        ];
        let r = reason(classify(lines, ExitFacts::Exited { code: Some(0) }, ""));
        assert!(r.contains("no result event"), "reason: {r}");
        // Defensive corner of the same row: a runner linger-kill with no
        // result is equally malformed (the runner only linger-kills after a
        // result, so the combination is itself a surprise).
        let r = reason(classify(lines, ExitFacts::LingerKilled, ""));
        assert!(r.contains("no result event"), "reason: {r}");
    }

    // R10: nonzero exit without a result → Infra carrying the exit code and
    // the raw stderr tail. The tail lands verbatim BY CONTRACT — the caller
    // (U5's shared sanitize → scrub helper) cleans it before storage; this
    // module never does (see CheckOutcome's doc).
    #[test]
    fn nonzero_exit_with_no_result_carries_code_and_stderr_tail() {
        let r = reason(classify(
            &[br#"{"type":"system","subtype":"init","session_id":"s-1"}"#],
            ExitFacts::Exited { code: Some(3) },
            "Error: ENOENT no such file",
        ));
        assert!(r.contains('3'), "reason carries the exit code: {r}");
        assert!(
            r.contains("Error: ENOENT no such file"),
            "reason carries the raw tail: {r}"
        );
        // A spontaneous signal death with no result is the same row.
        let r = reason(classify(
            &[],
            ExitFacts::Exited { code: None },
            "killed from outside",
        ));
        assert!(r.contains("signal"), "reason: {r}");
        assert!(r.contains("killed from outside"), "reason: {r}");
    }

    // R10's timeout row: the deadline kill is Infra — with no result, and
    // (flat rule) even after a success result, because the runner's
    // stream-end policy means a post-result deadline is itself surprising.
    #[test]
    fn timeout_is_infra_timed_out() {
        let r = reason(classify(&[], ExitFacts::TimedOut, ""));
        assert!(r.contains("timed out"), "reason: {r}");

        let lines: &[&[u8]] =
            &[br#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#];
        let r = reason(classify(lines, ExitFacts::TimedOut, ""));
        assert!(r.contains("timed out"), "reason: {r}");
    }

    // R10: a non-success result is Infra, never Clean — `is_error: true`
    // (even with subtype "success"), an unobserved error subtype, or a
    // missing subtype all abstain, exit 0 notwithstanding.
    #[test]
    fn is_error_true_or_non_success_subtype_is_infra_never_clean() {
        let cases: &[&[u8]] = &[
            br#"{"type":"result","subtype":"success","is_error":true,"result":"lying"}"#,
            br#"{"type":"result","subtype":"error_during_execution","is_error":false,"result":"x"}"#,
            br#"{"type":"result","is_error":false,"result":"no subtype at all"}"#,
        ];
        for line in cases {
            let r = reason(classify(&[line], ExitFacts::Exited { code: Some(0) }, ""));
            assert!(r.contains("non-success"), "reason: {r}");
        }
    }

    // R11: a malformed JSON line mid-stream is skipped; the surrounding
    // stream still classifies normally.
    #[test]
    fn malformed_json_line_mid_stream_is_skipped() {
        let lines: &[&[u8]] = &[
            br#"{"type":"system","subtype":"init","session_id":"s-1"}"#,
            b"}{ this is not JSON at all",
            b"",
            br#"{"type":"result","subtype":"success","is_error":false,"result":"fine"}"#,
        ];
        assert_eq!(
            classify(lines, ExitFacts::Exited { code: Some(0) }, ""),
            CheckOutcome::Clean {
                text: "fine".to_owned(),
                session_id: Some("s-1".to_owned()),
            }
        );
    }

    // R11: a stream that is only garbage ends result-less → Infra.
    #[test]
    fn a_stream_of_only_garbage_is_infra() {
        let lines: &[&[u8]] = &[b"not json", b"also not json", b"\x1b[31mstill not\x1b[0m"];
        let r = reason(classify(lines, ExitFacts::Exited { code: Some(0) }, ""));
        assert!(r.contains("no result event"), "reason: {r}");
    }

    // R11: a line over MAX_LINE_BYTES is skipped without parsing — even when
    // it would have been a valid success result — and the run then ends
    // Infra because the skipped line swallowed the result.
    #[test]
    fn an_over_cap_line_is_skipped_without_parsing_and_a_swallowed_result_is_infra() {
        let huge = format!(
            r#"{{"type":"result","subtype":"success","is_error":false,"result":"{}"}}"#,
            "x".repeat(MAX_LINE_BYTES)
        );
        assert!(huge.len() > MAX_LINE_BYTES);
        assert_eq!(parse_line(huge.as_bytes()), None, "skipped, not parsed");

        let init: &[u8] = br#"{"type":"system","subtype":"init","session_id":"s-1"}"#;
        let r = reason(classify(
            &[init, huge.as_bytes()],
            ExitFacts::Exited { code: Some(0) },
            "",
        ));
        assert!(r.contains("no result event"), "reason: {r}");
    }

    // R10: a success result already seen stays Clean when the subsequent
    // exit is the runner's own lingering-exit kill (the backgrounding
    // quirk); a *spontaneous* nonzero or signal exit after the result
    // remains infra — only the runner's kill is exempt.
    #[test]
    fn success_result_then_runner_linger_kill_stays_clean() {
        let lines: &[&[u8]] = &[
            br#"{"type":"system","subtype":"init","session_id":"s-9"}"#,
            br#"{"type":"result","subtype":"success","is_error":false,"result":"parked; still training"}"#,
        ];
        assert_eq!(
            classify(lines, ExitFacts::LingerKilled, ""),
            CheckOutcome::Clean {
                text: "parked; still training".to_owned(),
                session_id: Some("s-9".to_owned()),
            }
        );
    }

    #[test]
    fn spontaneous_nonzero_or_signal_exit_after_a_result_is_infra() {
        let lines: &[&[u8]] =
            &[br#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#];
        let r = reason(classify(lines, ExitFacts::Exited { code: Some(1) }, ""));
        assert!(r.contains("spontaneous"), "reason: {r}");
        assert!(r.contains('1'), "reason carries the code: {r}");
        // Empty stderr tail ⇒ no dangling "; stderr:" fragment.
        assert!(!r.contains("stderr"), "reason: {r}");

        let r = reason(classify(lines, ExitFacts::Exited { code: None }, ""));
        assert!(r.contains("signal"), "reason: {r}");
    }

    // R10: an empty result text is Clean with empty text — the empty→None
    // mapping (which makes the row read unreadable in the derived counter)
    // happens downstream in the shared cleaning helper (U4), never here.
    #[test]
    fn empty_result_text_is_clean_with_empty_text() {
        let lines: &[&[u8]] =
            &[br#"{"type":"result","subtype":"success","is_error":false,"result":""}"#];
        assert_eq!(
            classify(lines, ExitFacts::Exited { code: Some(0) }, ""),
            CheckOutcome::Clean {
                text: String::new(),
                session_id: None,
            }
        );
    }

    // Bytes end to end: a line containing multibyte UTF-8 parses intact when
    // fed as bytes (the runner splits at line boundaries only, so no lossy
    // conversion can land mid-character).
    #[test]
    fn multibyte_utf8_lines_parse_intact_as_bytes() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"naïve — ✓ 完了 🚀"}"#;
        assert_eq!(
            classify(&[line.as_bytes()], ExitFacts::Exited { code: Some(0) }, ""),
            CheckOutcome::Clean {
                text: "naïve — ✓ 完了 🚀".to_owned(),
                session_id: None,
            }
        );
        // Invalid UTF-8 bytes are just an unparsable (skipped) line — never
        // lossy-converted into something that half-parses.
        assert_eq!(parse_line(b"{\"type\":\"result\xff\xfe\"}"), None);
    }

    // R10's spawn-failure row: no child, no stream — the runner constructs
    // the infra outcome directly from the spawn error.
    #[test]
    fn spawn_failure_is_infra_with_the_error() {
        let r = reason(CheckOutcome::spawn_failed("No such file or directory (os error 2)"));
        assert!(r.contains("spawn failed"), "reason: {r}");
        assert!(r.contains("os error 2"), "reason: {r}");
    }

    // R12: no init ⇒ session id None (Clean still stands on the result
    // alone); and the first init wins over a later one, so the stamped id
    // is stable.
    #[test]
    fn session_id_is_none_without_init_and_first_init_wins() {
        let lines: &[&[u8]] =
            &[br#"{"type":"result","subtype":"success","is_error":false,"result":"ok"}"#];
        assert_eq!(
            classify(lines, ExitFacts::Exited { code: Some(0) }, ""),
            CheckOutcome::Clean {
                text: "ok".to_owned(),
                session_id: None,
            }
        );

        let lines: &[&[u8]] = &[
            br#"{"type":"system","subtype":"init","session_id":"first"}"#,
            br#"{"type":"system","subtype":"init","session_id":"second"}"#,
            br#"{"type":"result","subtype":"success","is_error":false,"result":"ok"}"#,
        ];
        assert_eq!(
            classify(lines, ExitFacts::Exited { code: Some(0) }, ""),
            CheckOutcome::Clean {
                text: "ok".to_owned(),
                session_id: Some("first".to_owned()),
            }
        );
    }
}
