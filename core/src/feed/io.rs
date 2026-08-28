//! The per-agent IO halves of the feed endpoints (feed-agent-reply-io):
//! resolving a leaf's **latest reply** (U3, the read half) and building the
//! **input payload** delivered to its PTY (U5, the write half).
//!
//! Both halves are path-injected and framework-free so they are unit-tested on
//! temp dirs, mirroring `session/transcript.rs`. The resolver is the single
//! source of a reply's `(text, repliedAt)` pair — `GET /agents/{key}/output`
//! and the `/feed` frame's `lastReplyAt` both go through it, so the two values
//! cannot drift (R3: a matching stamp is what lets the consumer clear its
//! unread dot by reading the reply).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;

use super::wire::{QuestionBody, QuestionOption, QuestionSpec, TurnEntry};
use crate::automations::redact;
use crate::session::resume;
use crate::session::transcript::{self, LastReply, PendingInteraction, PendingKind, TurnRole};

/// Resolve a leaf key's latest assistant reply from durable state (U3):
/// resume record (leaf → `session_id` + `session_cwd`) → transcript path
/// (`<projects-root>/<encoded-cwd>/<session-id>.jsonl`) → last assistant turn.
///
/// The resume store — not the live pane — is the resolution root because it is
/// pane-precise (Hook/Pick ranked, fix-session-pane-attribution), survives an
/// app restart, and never abstains on a same-cwd sibling the way the live poll
/// must. Session ids are validated at store-write time
/// (`resume::is_plausible_session_id`), so joining one into the projects root
/// cannot escape it.
///
/// Reads are cached per leaf keyed by the transcript's `(path, mtime, len)`
/// (KTD5): frames re-resolve every connected consumer × every agent × every
/// bump, and a multi-MB transcript must not be re-parsed when unchanged.
pub struct ReplyResolver {
    resume_path: PathBuf,
    projects_root: Option<PathBuf>,
    cache: Mutex<HashMap<String, CachedIo>>,
}

/// One leaf's memoized resolution: the transcript identity it was computed
/// from, and the reply + pending question (or confirmed absence) it yielded.
/// Only transcript-derived facts live here (feed-pending-question KTD4) — the
/// attention-dependent permission gate is applied at emit/response time,
/// outside the cache, so a stale entry degrades to "not exposed", never to a
/// wrong exposure.
struct CachedIo {
    transcript: PathBuf,
    mtime: SystemTime,
    len: u64,
    io: ResolvedIo,
}

/// The per-leaf IO facts from one transcript resolution (feed-pending-question
/// U3; feed-conversation-tail U2): the latest completed reply, the pending
/// question, and the recent conversation tail — question and turns already
/// scrubbed / sanitized / truncated into the wire shape. Every surface reads
/// this one source, so `/feed`'s `questionPendingAt` and `/output`'s
/// `question.askedAt` cannot drift for a choice question (R4), and the tail's
/// final turn cannot drift from the reply it correlates with.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ResolvedIo {
    pub reply: Option<LastReply>,
    pub question: Option<QuestionBody>,
    /// Wire-ready conversation tail (feed-conversation-tail R1): empty means
    /// "no servable history" and serializes as an absent key (R5).
    pub turns: Vec<TurnEntry>,
    /// The tier-1 pending signal (feed-question-screen-fallback R2): the
    /// ask-time stamp of a corroborated-waiting pane whose transcript yielded
    /// no question. Set ONLY by the fallback layer (`fallback.rs`) — the
    /// transcript resolver itself always leaves it `None`. The frame's
    /// `questionPendingAt` falls back to it when no (gated) question body
    /// exists, so "pending, body unavailable" still surfaces.
    pub pending_fallback_at: Option<u64>,
}

impl ReplyResolver {
    /// The production resolver: fly's own resume store + Claude's projects root
    /// (honoring `CLAUDE_CONFIG_DIR`, resolved once at construction).
    pub fn new(resume_path: PathBuf) -> Self {
        Self::with_projects_root(resume_path, transcript::claude_projects_root())
    }

    /// Root-injected constructor (tests): resolve against an arbitrary
    /// projects root instead of `~/.claude/projects`.
    pub fn with_projects_root(resume_path: PathBuf, projects_root: Option<PathBuf>) -> Self {
        Self {
            resume_path,
            projects_root,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Both IO facts for a leaf from one transcript read (feed-pending-question
    /// U3/KTD4): the latest reply, and the pending question already in wire
    /// shape. Either half is `None` when any link is missing — no resume
    /// record, no captured session/cwd, no transcript on disk, a text-free
    /// tail, nothing pending. All of those mean "no data" — never an error.
    ///
    /// Reply text is control-sanitized (R16 posture; newlines/tabs kept),
    /// *then* secret-scrubbed (audit-remediation U1/R1 — the same
    /// sanitize-before-scrub order [`clean`] pins, so a control char inside a
    /// token can't defeat the prefix/marker scan; see [`clean`] for the
    /// anti-reassembly argument) so every consumer of the pair sees the same
    /// bytes; a reply that sanitizes to blank counts as absent. The reply is
    /// never truncated — only questions and turns carry caps.
    ///
    /// The conversation tail (feed-conversation-tail U2) is served only
    /// alongside a *stamped* wire reply — the array must end with the current
    /// reply, `at == repliedAt` (R3) — and every turn's text goes through the
    /// full [`clean`] pipeline (R7): a secret-bearing final turn reads
    /// `[redacted]` in both `text` and `turns`.
    pub fn resolve_io(&self, leaf_key: &str) -> ResolvedIo {
        let Some(path) = self.transcript_path(leaf_key) else {
            return ResolvedIo::default();
        };
        let Ok(meta) = std::fs::metadata(&path) else {
            return ResolvedIo::default();
        };
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let len = meta.len();

        let mut cache = self.cache.lock().unwrap();
        if let Some(hit) = cache.get(leaf_key) {
            if hit.transcript == path && hit.mtime == mtime && hit.len == len {
                return hit.io.clone();
            }
        }
        let io = transcript::transcript_io(&path)
            .map(|t| {
                let reply = t.reply.and_then(|r| {
                    let text = redact::clean_text(&r.text);
                    (!text.trim().is_empty()).then_some(LastReply {
                        text,
                        replied_at_ms: r.replied_at_ms,
                    })
                });
                let turns = reply
                    .as_ref()
                    .and_then(|r| r.replied_at_ms)
                    .map(|at| shape_turns(&t.turns, at))
                    .unwrap_or_default();
                ResolvedIo {
                    reply,
                    question: t.pending.as_ref().and_then(question_body),
                    turns,
                    pending_fallback_at: None,
                }
            })
            .unwrap_or_default();
        cache.insert(
            leaf_key.to_string(),
            CachedIo {
                transcript: path,
                mtime,
                len,
                io: io.clone(),
            },
        );
        io
    }

    /// The leaf's transcript path via its resume record (leaf → `session_id` +
    /// `session_cwd` → `<projects-root>/<encoded-cwd>/<session-id>.jsonl`).
    fn transcript_path(&self, leaf_key: &str) -> Option<PathBuf> {
        let record = resume::read_records(&self.resume_path).remove(leaf_key)?;
        let session_id = record.session_id?;
        let session_cwd = record.session_cwd?;
        let root = self.projects_root.as_ref()?;
        Some(
            root.join(transcript::encode_cwd(&session_cwd))
                .join(format!("{session_id}.jsonl")),
        )
    }
}

// ---- question shaping: sanitize → scrub → truncate (U3, R8/KTD7) -----------

/// Exposure ceilings, in chars, pinned per the plan (values negotiable; the
/// contract is that they are pinned and oversized content is
/// **truncated-and-served, never abstained** — an oversized injected question
/// must not suppress exposure).
pub(crate) const MAX_QUESTIONS: usize = 4;
pub(crate) const MAX_OPTIONS: usize = 8;
pub(crate) const QUESTION_CAP: usize = 512;
pub(crate) const HEADER_CAP: usize = 512;
pub(crate) const LABEL_CAP: usize = 128;
pub(crate) const DESCRIPTION_CAP: usize = 1024;
pub(crate) const CONTEXT_CAP: usize = 2048;
pub(crate) const REQUEST_CAP: usize = 512;

/// The one string pipeline for every newly exposed question string (R8):
/// control-sanitize the **full, untruncated** value first (R16 posture,
/// newlines/tabs kept), *then* secret-scrub, *then* truncate to `cap` on a
/// char boundary with an ellipsis.
///
/// Order matters twice, and both properties hold because scrub sees the full,
/// control-free string:
/// - **Straddle (R8's original goal):** scrub runs on the full-length value,
///   so a secret spanning the truncation boundary is masked before its tail is
///   cut.
/// - **Reassembly (review finding):** `redact::scrub_secrets` matches tokens by
///   `starts_with(prefix)` / key-marker `contains`, both of which a control or
///   zero-width char *inside* the token defeats. Sanitizing **before** scrub
///   makes the token contiguous when the prefix/marker scan runs, so a crafted
///   `sk-\u{200b}ant-…` can't slip past scrub and then be re-formed into
///   cleartext by a later sanitize pass. (Scrubbing first, as the code
///   originally did, left exactly that reassembly hole.)
///
/// `None` when the result is blank (a blank string is absent, never served
/// empty).
pub(crate) fn clean(raw: &str, cap: usize) -> Option<String> {
    let sane = crate::notify::sanitize_multiline(raw);
    let scrubbed = redact::scrub_secrets(&sane);
    if scrubbed.trim().is_empty() {
        return None;
    }
    let mut out: String = scrubbed.chars().take(cap).collect();
    if scrubbed.chars().count() > cap {
        out.push('…');
    }
    Some(out)
}

// ---- conversation-tail shaping (feed-conversation-tail U2) ------------------

/// Serving ceilings for the conversation tail (R4), pinned like the question
/// ceilings above — the consumer pins its own caps at or above these. Depth
/// counts **served** turns (drops don't shrink the window); text is char-capped
/// through [`clean`], so a turn is never more than 4·`TURN_CAP` bytes of UTF-8
/// plus a 3-byte ellipsis — the same order as the 8 KiB automations output
/// tail cap (`automations::model::OUTPUT_TAIL_CAP_BYTES`) it references.
pub const MAX_TURNS: usize = 12;
pub const TURN_CAP: usize = 2048;
// The raw scan must retain at least a full serving window (KTD3).
const _: () = assert!(MAX_TURNS <= transcript::RAW_TURN_BUFFER);

/// Shape the raw conversation window into the wire `turns` (U2): walk backward
/// from the newest agent turn — which the transcript scan guarantees is the
/// same entry the reply came from (KTD1) — collecting up to [`MAX_TURNS`]
/// turns whose stamp parsed and whose text survives [`clean`] (sanitize →
/// scrub → truncate to [`TURN_CAP`], R7), then reverse to oldest → newest.
/// Prompts newer than the reply are cut so the array ends with the current
/// reply, `at == repliedAt` (R3/KTD2); they surface once the next reply closes
/// them out. Empty — the caller's omit signal (R5) — when no agent turn
/// exists in the window or when the newest one doesn't carry the reply's own
/// stamp (defensive: serving it would break the correlation contract).
fn shape_turns(raw: &[transcript::RawTurn], replied_at_ms: u64) -> Vec<TurnEntry> {
    let Some(end) = raw.iter().rposition(|t| t.role == TurnRole::Agent) else {
        return Vec::new();
    };
    if raw[end].at_ms != Some(replied_at_ms) {
        return Vec::new();
    }
    let mut newest_first: Vec<TurnEntry> = Vec::new();
    for t in raw[..=end].iter().rev() {
        if newest_first.len() == MAX_TURNS {
            break;
        }
        let Some(at) = t.at_ms else { continue };
        let Some(text) = clean(&t.text, TURN_CAP) else { continue };
        let role = match t.role {
            TurnRole::User => "user",
            TurnRole::Agent => "agent",
        };
        newest_first.push(TurnEntry {
            role: role.to_string(),
            at,
            text,
        });
    }
    newest_first.reverse();
    newest_first
}

/// Shape a parsed pending interaction into the wire `QuestionBody`, applying
/// the [`clean`] pipeline to every exposed string and the pinned count
/// ceilings. `None` when nothing exposable survives (every question blank —
/// the KTD1 abstain posture, extended to the display layer).
///
/// `answerable` is recomputed *here*, after cleaning, and is stricter than the
/// parse-time flag on both ends: it ANDs `p.answerable` (which already carries
/// the transcript-layer parse-drop verdict this function can't re-derive) with
/// a fresh check that no question or option was dropped by *this* display-layer
/// clean (blank after sanitize). Either kind of drop breaks the wire↔screen
/// index mapping a digit answer relies on, so it forces `answerable: false`.
/// Truncating a >8-option tail keeps the 1..8 prefix mapping intact and stays
/// answerable.
///
/// Each wire option carries `key` — the 1-based **source** position (the digit
/// the on-screen picker binds to that option, verified live). It is taken from
/// the pre-drop enumeration index so a blank-dropped option leaves a gap rather
/// than renumbering its successors (a renumber would silently mis-map a digit);
/// for an answerable question no drops occur, so keys are contiguous `1..N`.
///
/// `pub(crate)` for the hook-ask-channel (U6): the hook leg shapes its
/// `PendingInteraction` through this same function, so choice-body rules
/// (caps, drops, answerability, `otherKey`) cannot drift between sources.
pub(crate) fn question_body(p: &PendingInteraction) -> Option<QuestionBody> {
    match p.kind {
        PendingKind::Choice => {
            let mut dropped_any = false;
            let mut questions: Vec<QuestionSpec> = Vec::new();
            for q in p.questions.iter().take(MAX_QUESTIONS) {
                let Some(question) = clean(&q.question, QUESTION_CAP) else {
                    dropped_any = true;
                    continue;
                };
                let mut options: Vec<QuestionOption> = Vec::new();
                for (i, o) in q.options.iter().take(MAX_OPTIONS).enumerate() {
                    let Some(label) = clean(&o.label, LABEL_CAP) else {
                        dropped_any = true;
                        continue;
                    };
                    options.push(QuestionOption {
                        key: (i + 1).to_string(),
                        label,
                        description: clean(&o.description, DESCRIPTION_CAP).unwrap_or_default(),
                    });
                }
                questions.push(QuestionSpec {
                    question,
                    header: clean(&q.header, HEADER_CAP).unwrap_or_default(),
                    multi_select: q.multi_select,
                    options,
                    other_key: None,
                });
            }
            if questions.is_empty() {
                return None;
            }
            let answerable = p.answerable
                && !dropped_any
                && p.questions.len() == 1
                && questions.len() == 1
                && !questions[0].multi_select
                && !questions[0].options.is_empty();
            // The free-text row's digit (feed-other-answer R2), only on the one
            // answerable shape: the picker appends "Type something." directly
            // after the authored options, so its digit is the SOURCE option
            // count + 1 (pre-truncation — a truncated >9 tail would need a
            // two-keystroke digit, which `None`s out here). Verified live on
            // 2.1.206; a transcript body cannot see the rendered row, so this
            // rests on that appended-row contract (KTD2 of the plan).
            if answerable {
                questions[0].other_key = other_digit_after(p.questions[0].options.len());
            }
            Some(QuestionBody {
                asked_at: p.asked_at_ms,
                kind: "choice".into(),
                tool: p.tool.clone(),
                answerable,
                context: p.context.as_deref().and_then(|c| clean(c, CONTEXT_CAP)),
                questions,
                request: None,
                source: None,
            })
        }
        PendingKind::Permission => Some(QuestionBody {
            asked_at: p.asked_at_ms,
            kind: "permission".into(),
            tool: p.tool.clone(),
            answerable: false,
            context: p.context.as_deref().and_then(|c| clean(c, CONTEXT_CAP)),
            questions: Vec::new(),
            request: p.input.as_ref().and_then(|i| permission_request(&p.tool, i)),
            source: None,
        }),
    }
}

/// A one-line, scrubbed summary of a permission request's input, from
/// well-known fields only: `Bash` → `command`, `Edit`/`Write` → `file_path`.
/// Other tools get no summary (their `tool` name is already on the body) —
/// guessing at arbitrary input shapes would risk exposing more than the
/// dialog shows (KTD1 posture).
fn permission_request(tool: &str, input: &serde_json::Value) -> Option<String> {
    let field = match tool {
        "Bash" => "command",
        "Edit" | "Write" => "file_path",
        _ => return None,
    };
    input
        .get(field)
        .and_then(|v| v.as_str())
        .and_then(|s| clean(s, REQUEST_CAP))
}

/// Build the paste half of one remotely-submitted message (U5): the text
/// normalized and wrapped exactly like the handoff pre-type
/// (`lib/handoff.ts::injectionPayload`, R9 posture) — CR/CRLF → LF, every
/// other control char stripped (ESC among them, so the payload can never forge
/// the paste-end marker or smuggle a raw terminal sequence), bracketed-paste
/// wrapped so embedded newlines land in the composer.
///
/// The submit ([`SUBMIT`]) is deliberately NOT part of this payload: Claude
/// Code's composer treats bytes arriving in the same input chunk as the paste
/// as pasted content, so a same-chunk `\r` becomes a composer newline, not a
/// submit (verified live against 2.1.201). The caller writes this payload,
/// waits [`SUBMIT_DELAY`], then writes [`SUBMIT`] as its own chunk — a real
/// paste followed by a real Enter keypress, which is the endpoint's whole
/// contract.
pub fn paste_payload(text: &str) -> Vec<u8> {
    let inner: String = text
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .filter(|&c| c == '\n' || !c.is_control())
        .collect();
    format!("\x1b[200~{inner}\x1b[201~").into_bytes()
}

/// The Enter keypress that submits the pasted text — written as its own PTY
/// chunk after [`SUBMIT_DELAY`] (see [`paste_payload`]).
pub const SUBMIT: &[u8] = b"\r";

/// Cap on a `mode: "keys"` answer, in chars (feed-pending-question KTD6): a
/// picker answer is a digit or two — nothing legitimate needs more, and the
/// route rejects an over-cap body outright (400) rather than truncating bytes
/// into a pane in an unknown UI state.
pub const KEYS_MAX_CHARS: usize = 16;

/// Build the `mode: "keys"` payload (R9): every `char::is_control` char is
/// dropped — a *char*-level test, so a multi-byte C1 encoding can never pass
/// the way a byte-level check could — leaving no ESC (no forged paste
/// markers, no raw terminal sequences), no `\r`/`\n` (no submit smuggled into
/// an answer). No paste wrap and no trailing SUBMIT: digit keys select picker
/// options instantly, so the payload must arrive exactly as keystrokes.
/// `None` when nothing survives the filter — the caller delivers nothing
/// (400), never an empty write.
pub fn keys_payload(text: &str) -> Option<Vec<u8>> {
    let filtered: String = text.chars().filter(|&c| !c.is_control()).collect();
    (!filtered.is_empty()).then(|| filtered.into_bytes())
}

/// Gap between the paste write and the [`SUBMIT`] write: long enough for the
/// composer to leave paste handling and settle (human-keypress scale), short
/// enough to be imperceptible on the HTTP round-trip. Also the gap between the
/// three chunks of a `mode:"other"` delivery (digit → text → Enter): the live
/// probe (2026-07-10, 2.1.206) showed the picker drops a digit that arrives
/// coalesced with the text — chunk boundaries plus a human-scale gap are what
/// make the choreography deterministic (feed-other-answer KTD1).
pub const SUBMIT_DELAY: std::time::Duration = std::time::Duration::from_millis(150);

/// Cap on a `mode:"other"` free-text answer, in chars (feed-other-answer R6):
/// sentence scale — far above a picker digit, far below the 64 KiB body cap.
/// The route rejects an over-cap body outright (400) rather than truncating
/// bytes into the pane, same posture as [`KEYS_MAX_CHARS`].
pub const OTHER_MAX_CHARS: usize = 512;

/// The digit that focuses the picker's own "Type something." free-text row
/// for a transcript-derived question: the picker appends that row directly
/// after the authored options, so its digit is source option count + 1
/// (feed-other-answer R2, verified live on 2.1.206). `None` past 9 — a
/// two-char digit is not a single keystroke, so it is undeliverable.
pub(crate) fn other_digit_after(source_options: usize) -> Option<String> {
    let digit = source_options + 1;
    (digit <= 9).then(|| digit.to_string())
}

/// Build the text chunk of a `mode:"other"` answer (feed-other-answer R5): the
/// bytes typed into the picker's focused "Type something." input. Newlines
/// collapse to single spaces first (the inline input is one line; a kept `\r`
/// would submit early, a dropped one would join words), then every remaining
/// control char is dropped exactly like [`keys_payload`] — no ESC, so nothing
/// the picker could read as a cancel (the failure `paste_payload`'s leading
/// paste marker causes at an unfocused picker, the whole reason this mode
/// exists), and no smuggled submit (the Enter is the caller's own delayed
/// chunk). `None` when nothing printable survives — the route 400s, never an
/// empty write.
pub fn other_payload(text: &str) -> Option<Vec<u8>> {
    let one_line = text.replace("\r\n", " ").replace(['\r', '\n'], " ");
    let filtered: String = one_line.chars().filter(|&c| !c.is_control()).collect();
    (!filtered.trim().is_empty()).then(|| filtered.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A transcript whose last assistant turn is a stamped, two-block reply.
    const TRANSCRIPT: &str = concat!(
        r#"{"type":"user","message":{"role":"user","content":"status?"}}"#,
        "\n",
        r#"{"type":"assistant","timestamp":"2026-06-19T19:17:16.402Z","message":{"role":"assistant","content":[{"type":"text","text":"All tests pass."}]}}"#,
        "\n",
    );
    const REPLIED_AT: u64 = 1_781_896_636_402;

    /// Write a resume store mapping `leaf` → (`sid`, `cwd`) and the matching
    /// transcript under `root`, returning the resolver over both.
    fn fixture(
        dir: &std::path::Path,
        leaf: &str,
        sid: &str,
        cwd: &str,
        body: &str,
    ) -> ReplyResolver {
        let resume_path = dir.join("resume.json");
        resume::upsert_at(
            &resume_path,
            leaf,
            resume::ResumePartial {
                session_id: Some(sid.to_string()),
                session_cwd: Some(cwd.to_string()),
                session_source: Some(resume::SessionSource::Hook),
                ..Default::default()
            },
        )
        .unwrap();
        let root = dir.join("projects");
        let project = root.join(transcript::encode_cwd(cwd));
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(format!("{sid}.jsonl")), body).unwrap();
        ReplyResolver::with_projects_root(resume_path, Some(root))
    }

    #[test]
    fn resolves_a_leaf_to_its_transcripts_last_reply() {
        let dir = tempfile::tempdir().unwrap();
        let r = fixture(dir.path(), "leaf-1", "sid-abc", "/home/alice/projects/play", TRANSCRIPT);
        let reply = r.resolve_io("leaf-1").reply.expect("a reply resolves");
        assert_eq!(reply.text, "All tests pass.");
        assert_eq!(reply.replied_at_ms, Some(REPLIED_AT));
    }

    #[test]
    fn missing_links_resolve_to_none_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let r = fixture(dir.path(), "leaf-1", "sid-abc", "/home/alice/projects/play", TRANSCRIPT);
        // Unknown leaf (no record) → None.
        assert_eq!(r.resolve_io("leaf-ghost").reply, None);
        // A record with no captured session yet → None.
        resume::upsert_at(
            &dir.path().join("resume.json"),
            "leaf-2",
            resume::ResumePartial {
                argv: Some(vec!["claude".into()]),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(r.resolve_io("leaf-2").reply, None);
        // A record whose transcript is gone from disk → None.
        std::fs::remove_dir_all(dir.path().join("projects")).unwrap();
        assert_eq!(r.resolve_io("leaf-1").reply, None);
    }

    #[test]
    fn cache_serves_unchanged_files_and_refreshes_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = "/home/alice/projects/play";
        let r = fixture(dir.path(), "leaf-1", "sid-abc", cwd, TRANSCRIPT);
        assert_eq!(r.resolve_io("leaf-1").reply.unwrap().text, "All tests pass.");
        // Append a newer reply (mtime/len change) → the resolver re-reads.
        let path = dir
            .path()
            .join("projects")
            .join(transcript::encode_cwd(cwd))
            .join("sid-abc.jsonl");
        let appended = format!(
            "{TRANSCRIPT}{}\n",
            r#"{"type":"assistant","timestamp":"2026-06-19T19:20:00.000Z","message":{"role":"assistant","content":[{"type":"text","text":"One flaked on retry."}]}}"#
        );
        std::fs::write(&path, appended).unwrap();
        let reply = r.resolve_io("leaf-1").reply.unwrap();
        assert_eq!(reply.text, "One flaked on retry.");
        assert!(reply.replied_at_ms.unwrap() > REPLIED_AT);
    }

    #[test]
    fn reply_text_is_control_sanitized_and_blank_counts_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        // A reply carrying a JSON-escaped ESC sequence (the form a real
        // transcript stores): the control char is stripped after parse, the
        // printable residue and newline kept (sanitize_multiline, R16 posture).
        let body = r#"{"type":"assistant","message":{"role":"assistant","content":"ok\u001b[2J\ndone"}}"#;
        let r = fixture(dir.path(), "leaf-1", "sid-abc", "/p", &format!("{body}\n"));
        assert_eq!(r.resolve_io("leaf-1").reply.unwrap().text, "ok[2J\ndone");
        // A reply that is nothing but stripped chars counts as no reply.
        let blank = r#"{"type":"assistant","message":{"role":"assistant","content":"\u0007\u001b"}}"#;
        let r = fixture(dir.path(), "leaf-2", "sid-def", "/q", &format!("{blank}\n"));
        assert_eq!(r.resolve_io("leaf-2").reply, None);
    }

    #[test]
    fn reply_text_is_secret_scrubbed_in_both_text_and_turns() {
        // Audit-remediation U1/R1: the reply passes sanitize → scrub like every
        // other feed-exposed string, so a secret echoed in the final assistant
        // turn reads [redacted] in `text` AND in the tail's last turn.
        let secret = format!("sk-ant-api03-{}", "a".repeat(30));
        let body = [
            user_line(1, "what key did you use?"),
            agent_line(5, &format!("I used {secret} for that call.")),
        ]
        .join("\n");
        let dir = tempfile::tempdir().unwrap();
        let r = fixture(dir.path(), "leaf-1", "sid-abc", "/p", &format!("{body}\n"));
        let io = r.resolve_io("leaf-1");
        let text = &io.reply.as_ref().unwrap().text;
        assert!(!text.contains("sk-ant"), "reply leaked: {text}");
        assert!(text.contains("[redacted]"));
        assert_eq!(&io.turns.last().unwrap().text, text, "parity with turns");
        // A cache hit (unchanged mtime/len) serves the scrubbed form too.
        let cached = r.resolve_io("leaf-1");
        assert_eq!(&cached.reply.unwrap().text, text);
    }

    #[test]
    fn a_reply_that_scrubs_to_redacted_only_still_serves() {
        // Scrubbing never blanks a reply into absence — the marker itself is
        // non-blank, so a secret-only reply serves as "[redacted]".
        let body = agent_line(5, "sk-ant-api03-abcdefghijklmnopqrstuvwxyz1234");
        let dir = tempfile::tempdir().unwrap();
        let r = fixture(dir.path(), "leaf-1", "sid-abc", "/p", &format!("{body}\n"));
        assert_eq!(r.resolve_io("leaf-1").reply.unwrap().text, "[redacted]");
    }

    // ---- resolve_io: reply + pending from one read (feed-pending-question U3)

    /// A transcript with a completed, stamped reply followed by a pending
    /// AskUserQuestion whose context is that same reply text.
    const PENDING_ASK: &str = concat!(
        r#"{"type":"user","message":{"role":"user","content":"pick for me"}}"#,
        "\n",
        r#"{"type":"assistant","timestamp":"2026-06-19T19:17:16.402Z","message":{"role":"assistant","content":[{"type":"text","text":"Two options stand out."}]}}"#,
        "\n",
        r#"{"type":"assistant","timestamp":"2026-06-19T19:17:20.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"ask1","name":"AskUserQuestion","input":{"questions":[{"question":"Which one?","header":"Pick","multiSelect":false,"options":[{"label":"Alpha","description":"first"},{"label":"Beta","description":"second"}]}]}}]}}"#,
        "\n",
    );
    const PENDING_ASKED_AT: u64 = 1_781_896_640_000;

    #[test]
    fn resolve_io_yields_reply_and_pending_question_from_one_fixture() {
        let dir = tempfile::tempdir().unwrap();
        let r = fixture(dir.path(), "leaf-1", "sid-abc", "/p", PENDING_ASK);
        let io = r.resolve_io("leaf-1");
        // The reply half: the completed text turn (duplication with context is
        // blessed by design — the last text entry IS the context).
        assert_eq!(io.reply.as_ref().unwrap().text, "Two options stand out.");
        assert_eq!(io.reply.as_ref().unwrap().replied_at_ms, Some(REPLIED_AT));
        // The pending half, in wire shape.
        let q = io.question.expect("pending question");
        assert_eq!(q.asked_at, PENDING_ASKED_AT);
        assert_eq!(q.kind, "choice");
        assert!(q.answerable);
        assert_eq!(q.context.as_deref(), Some("Two options stand out."));
        assert_eq!(q.questions.len(), 1);
        assert_eq!(q.questions[0].options.len(), 2);
        // And the thin resolve() front still serves the reply alone.
        assert_eq!(r.resolve_io("leaf-1").reply.unwrap().text, "Two options stand out.");
    }

    #[test]
    fn appending_the_tool_result_clears_pending_on_reread() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = "/p";
        let r = fixture(dir.path(), "leaf-1", "sid-abc", cwd, PENDING_ASK);
        assert!(r.resolve_io("leaf-1").question.is_some());
        // The answer lands (mtime/len change) → the cache re-reads → cleared.
        let path = dir
            .path()
            .join("projects")
            .join(transcript::encode_cwd(cwd))
            .join("sid-abc.jsonl");
        let answered = format!(
            "{PENDING_ASK}{}\n",
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"ask1","content":"chose Alpha"}]}}"#
        );
        std::fs::write(&path, answered).unwrap();
        let io = r.resolve_io("leaf-1");
        assert_eq!(io.question, None);
        assert!(io.reply.is_some(), "the reply half is unaffected");
    }

    #[test]
    fn a_secret_straddling_the_truncation_boundary_is_still_masked() {
        // R8 ordering: scrub runs on the FULL string, then truncate. Build a
        // description whose sk-ant token starts just before the cap — a
        // truncate-first pipeline would slice the token and leak its head.
        let secret = format!("sk-ant-api03-{}", "a".repeat(30));
        let desc = format!("{} {} tail", "x".repeat(1010), secret);
        let entry = format!(
            r#"{{"type":"assistant","timestamp":"2026-06-19T19:17:20.000Z","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"t1","name":"AskUserQuestion","input":{{"questions":[{{"question":"Q?","options":[{{"label":"A","description":"{desc}"}}]}}]}}}}]}}}}"#
        );
        let dir = tempfile::tempdir().unwrap();
        let r = fixture(dir.path(), "leaf-1", "sid-abc", "/p", &format!("{entry}\n"));
        let q = r.resolve_io("leaf-1").question.expect("pending");
        let served = &q.questions[0].options[0].description;
        assert!(!served.contains("sk-ant"), "leaked head: {served}");
        assert!(served.contains("[redacted]"));
    }

    #[test]
    fn secrets_are_scrubbed_across_the_whole_surface() {
        // A secret in a Bash permission input AND one in an option description
        // are both masked (KTD7 spans every newly exposed string).
        let bash = r#"{"type":"assistant","timestamp":"2026-06-19T19:17:20.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"curl -H 'Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6' api"}}]}}"#;
        let dir = tempfile::tempdir().unwrap();
        let r = fixture(dir.path(), "leaf-1", "sid-abc", "/p", &format!("{bash}\n"));
        let q = r.resolve_io("leaf-1").question.expect("pending");
        assert_eq!(q.kind, "permission");
        assert_eq!(q.tool, "Bash");
        let req = q.request.expect("summary");
        assert!(req.contains("[redacted]"), "was: {req}");
        assert!(!req.contains("eyJ"), "was: {req}");
        assert!(q.context.is_none());
    }

    #[test]
    fn oversized_batches_truncate_to_ceilings_and_still_serve() {
        // 6 questions × 12 options with an oversized description: served at
        // 4 × 8 with capped strings — never abstained (the contract).
        let options: Vec<String> = (0..12)
            .map(|i| format!(r#"{{"label":"opt{i}","description":"{}"}}"#, "d".repeat(1500)))
            .collect();
        let questions: Vec<String> = (0..6)
            .map(|i| {
                format!(
                    r#"{{"question":"Q{i}?","header":"H","multiSelect":false,"options":[{}]}}"#,
                    options.join(",")
                )
            })
            .collect();
        let entry = format!(
            r#"{{"type":"assistant","timestamp":"2026-06-19T19:17:20.000Z","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"t1","name":"AskUserQuestion","input":{{"questions":[{}]}}}}]}}}}"#,
            questions.join(",")
        );
        let dir = tempfile::tempdir().unwrap();
        let r = fixture(dir.path(), "leaf-1", "sid-abc", "/p", &format!("{entry}\n"));
        let q = r.resolve_io("leaf-1").question.expect("served, not abstained");
        assert_eq!(q.questions.len(), 4);
        assert_eq!(q.questions[0].options.len(), 8);
        let d = &q.questions[0].options[0].description;
        assert!(d.chars().count() <= 1025, "capped (+ellipsis), was {}", d.len());
        assert!(d.ends_with('…'));
        assert!(!q.answerable, "a multi-question batch is read-only");
    }

    #[test]
    fn control_chars_strip_and_a_blank_label_drops_its_option_and_answerability() {
        // One option's label is nothing but control chars → dropped; the drop
        // breaks the wire↔screen digit mapping, so answerable goes false even
        // though one clean option remains.
        let entry = r#"{"type":"assistant","timestamp":"2026-06-19T19:17:20.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"AskUserQuestion","input":{"questions":[{"question":"Q\u001b[2J?","options":[{"label":"\u0007\u001b","description":"gone"},{"label":"Keep","description":"stays"}]}]}}]}}"#;
        let dir = tempfile::tempdir().unwrap();
        let r = fixture(dir.path(), "leaf-1", "sid-abc", "/p", &format!("{entry}\n"));
        let q = r.resolve_io("leaf-1").question.expect("pending");
        assert_eq!(q.questions[0].question, "Q[2J?", "control byte stripped");
        assert_eq!(q.questions[0].options.len(), 1);
        assert_eq!(q.questions[0].options[0].label, "Keep");
        assert!(!q.answerable, "a dropped option breaks digit mapping");
    }

    #[test]
    fn a_control_char_inside_a_secret_token_cannot_reassemble_past_the_scrubber() {
        // Review finding: with scrub-before-sanitize a crafted secret carrying
        // a zero-width/control char that breaks the token prefix (or a
        // sensitive-key marker) slips past the scrubber, then sanitize strips
        // the char and re-forms the secret in cleartext on the wire. The
        // sanitize-then-scrub order closes it: scrub sees the contiguous token.
        // A leading ZWSP before the prefix, and one splitting a KEY= marker.
        let entry = r#"{"type":"assistant","timestamp":"2026-06-19T19:17:20.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"AskUserQuestion","input":{"questions":[{"question":"ok?","options":[{"label":"A","description":"token \u200bsk-ant-api03-abcdefghijklmnop here"},{"label":"B","description":"AWS_SEC\u200bRET_ACCESS_KEY=wJalrXUtnFEMIabcdef"}]}]}}]}}"#;
        let dir = tempfile::tempdir().unwrap();
        let r = fixture(dir.path(), "leaf-1", "sid-abc", "/p", &format!("{entry}\n"));
        let q = r.resolve_io("leaf-1").question.expect("pending");
        let d0 = &q.questions[0].options[0].description;
        let d1 = &q.questions[0].options[1].description;
        assert!(!d0.contains("sk-ant-api03"), "zwsp-prefixed token leaked: {d0}");
        assert!(d0.contains("[redacted]"), "was: {d0}");
        assert!(!d1.contains("wJalrXUtnFEMI"), "zwsp-split key value leaked: {d1}");
        assert!(d1.contains("[redacted]"), "was: {d1}");
    }

    #[test]
    fn an_answerable_choice_carries_contiguous_source_position_keys() {
        // R7/agent-native: an answerable question hands the consumer the exact
        // digit each option binds to (`key`), 1-based and contiguous, so it
        // never has to reverse-engineer the picker keybinding.
        let dir = tempfile::tempdir().unwrap();
        let r = fixture(dir.path(), "leaf-1", "sid-abc", "/p", PENDING_ASK);
        let q = r.resolve_io("leaf-1").question.expect("pending");
        assert!(q.answerable);
        assert_eq!(q.questions[0].options[0].key, "1");
        assert_eq!(q.questions[0].options[1].key, "2");
    }

    #[test]
    fn a_single_select_with_over_eight_options_truncates_but_stays_answerable() {
        // The truncate-and-serve path for ONE single-select question: the >8
        // tail is cut, the 1..8 prefix mapping is intact (no drops), so it
        // stays answerable — distinct from the multi-question read-only path.
        let options: Vec<String> = (0..12).map(|i| format!(r#"{{"label":"opt{i}"}}"#)).collect();
        let entry = format!(
            r#"{{"type":"assistant","timestamp":"2026-06-19T19:17:20.000Z","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"t1","name":"AskUserQuestion","input":{{"questions":[{{"question":"Pick?","multiSelect":false,"options":[{}]}}]}}}}]}}}}"#,
            options.join(",")
        );
        let dir = tempfile::tempdir().unwrap();
        let r = fixture(dir.path(), "leaf-1", "sid-abc", "/p", &format!("{entry}\n"));
        let q = r.resolve_io("leaf-1").question.expect("served");
        assert_eq!(q.questions[0].options.len(), 8);
        assert_eq!(q.questions[0].options[7].key, "8");
        assert!(q.answerable, "an 8-of-12 prefix keeps digit mapping and stays answerable");
    }

    #[test]
    fn permission_request_maps_edit_write_file_path_and_abstains_for_unknown_tools() {
        let dir = tempfile::tempdir().unwrap();
        // Edit → file_path summary.
        let edit = r#"{"type":"assistant","timestamp":"2026-06-19T19:17:20.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"/home/e/app.rs","old_string":"a","new_string":"b"}}]}}"#;
        let r = fixture(dir.path(), "leaf-edit", "sid-e", "/pe", &format!("{edit}\n"));
        let q = r.resolve_io("leaf-edit").question.expect("pending");
        assert_eq!(q.tool, "Edit");
        assert_eq!(q.request.as_deref(), Some("/home/e/app.rs"));
        // Write → file_path too.
        let write = r#"{"type":"assistant","timestamp":"2026-06-19T19:17:20.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Write","input":{"file_path":"/tmp/out.txt","content":"x"}}]}}"#;
        let r = fixture(dir.path(), "leaf-write", "sid-w", "/pw", &format!("{write}\n"));
        assert_eq!(
            r.resolve_io("leaf-write").question.unwrap().request.as_deref(),
            Some("/tmp/out.txt")
        );
        // An unknown tool exposes its name but no synthesized summary (KTD1).
        let webfetch = r#"{"type":"assistant","timestamp":"2026-06-19T19:17:20.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"WebFetch","input":{"url":"https://x"}}]}}"#;
        let r = fixture(dir.path(), "leaf-wf", "sid-f", "/pf", &format!("{webfetch}\n"));
        let q = r.resolve_io("leaf-wf").question.expect("pending");
        assert_eq!(q.tool, "WebFetch");
        assert_eq!(q.request, None);
    }

    #[test]
    fn missing_links_yield_empty_io_never_error() {
        let dir = tempfile::tempdir().unwrap();
        let r = fixture(dir.path(), "leaf-1", "sid-abc", "/p", PENDING_ASK);
        // Unknown leaf → both halves None.
        assert_eq!(r.resolve_io("leaf-ghost"), ResolvedIo::default());
        // Transcript gone from disk → both halves None.
        std::fs::remove_dir_all(dir.path().join("projects")).unwrap();
        assert_eq!(r.resolve_io("leaf-1"), ResolvedIo::default());
    }

    // ---- conversation tail (feed-conversation-tail U2) ----------------------

    /// A stamped user prompt entry. Seconds offset keeps stamps ordered.
    fn user_line(sec: u8, text: &str) -> String {
        format!(
            r#"{{"type":"user","timestamp":"2026-06-19T19:17:{sec:02}.000Z","message":{{"role":"user","content":"{text}"}}}}"#
        )
    }

    /// A stamped assistant text entry.
    fn agent_line(sec: u8, text: &str) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"2026-06-19T19:17:{sec:02}.000Z","message":{{"role":"assistant","content":[{{"type":"text","text":"{text}"}}]}}}}"#
        )
    }

    #[test]
    fn turns_end_with_the_reply_and_cut_trailing_prompts() {
        // R3/KTD2: a prompt delivered AFTER the last reply (the agent is still
        // working on it) is cut, so the array's final turn is the current
        // reply and its `at` equals `repliedAt`. Tool chatter in between
        // (tool_use / tool_result entries) never becomes a turn, and the
        // mid-run narration collapses into the run's final reply (R6).
        let body = [
            user_line(1, "run the tests"),
            agent_line(5, "Running."),
            r#"{"type":"assistant","timestamp":"2026-06-19T19:17:06.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test"}}]}}"#.to_string(),
            r#"{"type":"user","timestamp":"2026-06-19T19:17:08.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#.to_string(),
            agent_line(16, "All tests pass."),
            user_line(30, "now the lints"),
        ]
        .join("\n");
        let dir = tempfile::tempdir().unwrap();
        let r = fixture(dir.path(), "leaf-1", "sid-abc", "/p", &format!("{body}\n"));
        let io = r.resolve_io("leaf-1");
        let reply = io.reply.expect("reply");
        assert_eq!(reply.text, "All tests pass.");
        let turns = &io.turns;
        assert_eq!(
            turns.iter().map(|t| t.role.as_str()).collect::<Vec<_>>(),
            vec!["user", "agent"],
            "trailing prompt cut, tool chatter absent, narration collapsed"
        );
        assert_eq!(turns[0].text, "run the tests");
        assert_eq!(turns.last().unwrap().text, "All tests pass.");
        assert_eq!(turns.last().unwrap().at, reply.replied_at_ms.unwrap());
        // Oldest → newest.
        assert!(turns.windows(2).all(|w| w[0].at <= w[1].at));
    }

    #[test]
    fn turns_depth_is_capped_serving_the_newest_window_ending_at_the_reply() {
        // R4: >MAX_TURNS of history serves exactly MAX_TURNS, the newest
        // window, still ending at the reply.
        let mut lines: Vec<String> = Vec::new();
        for i in 0..20u8 {
            lines.push(user_line(2 * i, &format!("prompt {i}")));
            lines.push(agent_line(2 * i + 1, &format!("reply {i}")));
        }
        let dir = tempfile::tempdir().unwrap();
        let r = fixture(dir.path(), "leaf-1", "sid-abc", "/p", &format!("{}\n", lines.join("\n")));
        let io = r.resolve_io("leaf-1");
        assert_eq!(io.turns.len(), MAX_TURNS);
        assert_eq!(io.turns.last().unwrap().text, "reply 19");
        assert_eq!(
            io.turns.last().unwrap().at,
            io.reply.unwrap().replied_at_ms.unwrap()
        );
        assert_eq!(io.turns[0].text, "prompt 14", "the newest 12-turn window");
    }

    #[test]
    fn turn_text_is_scrubbed_and_capped() {
        // R7: every turn's text passes the full clean pipeline — a secret in a
        // PROMPT is masked (stricter than the legacy unscrubbed reply `text`),
        // and an oversized turn truncates to TURN_CAP chars + ellipsis.
        let long = "x".repeat(TURN_CAP + 100);
        let body = [
            user_line(1, &format!("my key is sk-ant-api03-{} ok", "a".repeat(30))),
            user_line(2, &long),
            agent_line(5, "Noted."),
        ]
        .join("\n");
        let dir = tempfile::tempdir().unwrap();
        let r = fixture(dir.path(), "leaf-1", "sid-abc", "/p", &format!("{body}\n"));
        let turns = r.resolve_io("leaf-1").turns;
        assert_eq!(turns.len(), 3);
        assert!(!turns[0].text.contains("sk-ant"), "was: {}", turns[0].text);
        assert!(turns[0].text.contains("[redacted]"));
        assert_eq!(turns[1].text.chars().count(), TURN_CAP + 1, "cap + ellipsis");
        assert!(turns[1].text.ends_with('…'));
    }

    #[test]
    fn turns_are_omitted_without_a_stamped_reply() {
        // R5: no reply at all → no turns (a prompt-only tail is deferred until
        // the next reply closes it out)…
        let dir = tempfile::tempdir().unwrap();
        let r = fixture(dir.path(), "leaf-1", "sid-abc", "/p", &format!("{}\n", user_line(1, "hi")));
        let io = r.resolve_io("leaf-1");
        assert_eq!(io.reply, None);
        assert!(io.turns.is_empty());
        // …and a reply whose entry carried no parseable stamp serves text but
        // no turns — a turn without a numeric `at` is unservable (R2), so the
        // array could not end with the reply.
        let stampless = r#"{"type":"assistant","message":{"role":"assistant","content":"done"}}"#;
        let r = fixture(dir.path(), "leaf-2", "sid-def", "/q", &format!("{stampless}\n"));
        let io = r.resolve_io("leaf-2");
        assert_eq!(io.reply.as_ref().unwrap().replied_at_ms, None);
        assert!(io.turns.is_empty());
    }

    // ---- paste_payload / SUBMIT (U5) ----------------------------------------

    #[test]
    fn paste_is_bracket_wrapped_with_no_same_chunk_submit() {
        // The \r must NOT ride the paste chunk — Claude Code would treat it as
        // pasted content (a composer newline) instead of a submit; the caller
        // sends SUBMIT as its own later write.
        assert_eq!(paste_payload("hello"), b"\x1b[200~hello\x1b[201~".to_vec());
        assert_eq!(SUBMIT, b"\r");
        // Empty text degrades to an empty paste + the separate Enter (typing
        // nothing and pressing Enter) — faithful to the "as if typed" contract.
        assert_eq!(paste_payload(""), b"\x1b[200~\x1b[201~".to_vec());
    }

    // ---- keys_payload (feed-pending-question U6, R9/KTD6) -------------------

    #[test]
    fn keys_payload_strips_every_control_char_and_never_wraps() {
        // ESC (paste-marker forgery / raw sequences), \r and \n (a smuggled
        // submit), and other controls are dropped; printable residue is
        // delivered raw — no bracketed-paste wrap, no trailing SUBMIT.
        assert_eq!(keys_payload("2").as_deref(), Some(b"2".as_slice()));
        assert_eq!(
            keys_payload("1\x1b[201~\r\n2\x07").as_deref(),
            Some(b"1[201~2".as_slice())
        );
        // Empty after the filter → no write at all (the route 400s).
        assert_eq!(keys_payload("\x1b\r\n\x07"), None);
        assert_eq!(keys_payload(""), None);
    }

    // ---- other_payload / other_digit_after (feed-other-answer R2/R5/R6) -----

    #[test]
    fn other_payload_collapses_newlines_strips_controls_and_never_wraps() {
        // A sentence passes through raw — no paste markers, no trailing Enter.
        assert_eq!(
            other_payload("Use the staging bucket instead").as_deref(),
            Some(b"Use the staging bucket instead".as_slice())
        );
        // Newlines (any flavor) become single spaces: the inline input is one
        // line, and a raw \r would submit mid-answer.
        assert_eq!(
            other_payload("line one\r\nline two\nline three\rend").as_deref(),
            Some(b"line one line two line three end".as_slice())
        );
        // ESC and other controls are dropped — the payload can never carry a
        // paste marker or a cancel.
        assert_eq!(
            other_payload("a\x1b[201~b\x07c").as_deref(),
            Some(b"a[201~bc".as_slice())
        );
        // Nothing printable left → no write at all (the route 400s). A
        // newline-only text collapses to blank spaces and counts as empty too.
        assert_eq!(other_payload("\x1b\x07"), None);
        assert_eq!(other_payload("\n\r\n"), None);
        assert_eq!(other_payload(""), None);
    }

    #[test]
    fn other_digit_is_source_count_plus_one_single_keystroke_only() {
        assert_eq!(other_digit_after(2).as_deref(), Some("3"));
        assert_eq!(other_digit_after(8).as_deref(), Some("9"));
        // 9+ authored options put the row past a one-keystroke digit — and a
        // >MAX_OPTIONS batch was truncated on the wire besides — undeliverable.
        assert_eq!(other_digit_after(9), None);
        assert_eq!(other_digit_after(12), None);
    }

    #[test]
    fn an_answerable_choice_carries_the_other_row_digit() {
        // feed-other-answer R2: the wire hands the consumer the digit that
        // focuses "Type something." — authored count + 1 (here 2 + 1).
        let dir = tempfile::tempdir().unwrap();
        let r = fixture(dir.path(), "leaf-1", "sid-abc", "/p", PENDING_ASK);
        let q = r.resolve_io("leaf-1").question.expect("pending");
        assert!(q.answerable);
        assert_eq!(q.questions[0].other_key.as_deref(), Some("3"));
    }

    #[test]
    fn unanswerable_shapes_carry_no_other_key() {
        // A multi-question batch is read-only — no otherKey anywhere.
        let questions: Vec<String> = (0..2)
            .map(|i| {
                format!(
                    r#"{{"question":"Q{i}?","multiSelect":false,"options":[{{"label":"A"}},{{"label":"B"}}]}}"#
                )
            })
            .collect();
        let entry = format!(
            r#"{{"type":"assistant","timestamp":"2026-06-19T19:17:20.000Z","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"t1","name":"AskUserQuestion","input":{{"questions":[{}]}}}}]}}}}"#,
            questions.join(",")
        );
        let dir = tempfile::tempdir().unwrap();
        let r = fixture(dir.path(), "leaf-1", "sid-abc", "/p", &format!("{entry}\n"));
        let q = r.resolve_io("leaf-1").question.expect("served");
        assert!(!q.answerable);
        assert!(q.questions.iter().all(|s| s.other_key.is_none()));
    }

    #[test]
    fn keys_payload_passes_multibyte_text_but_its_filter_is_char_level() {
        // The R9 filter is a char-level is_control test, so multi-byte C1
        // encodings (U+009B CSI, a control) are stripped even though a
        // byte-level 0x20..=0x7E check on UTF-8 bytes could pass them.
        assert_eq!(keys_payload("a\u{009b}b").as_deref(), Some("ab".as_bytes()));
        // Plain multi-byte printables survive.
        assert_eq!(keys_payload("é").as_deref(), Some("é".as_bytes()));
    }

    #[test]
    fn paste_normalizes_newlines_and_strips_controls() {
        // CRLF/CR → LF (kept: the paste wrap turns them into composer
        // newlines, not submits); ESC and other controls stripped so the text
        // can never forge the paste-end marker or a raw terminal sequence.
        assert_eq!(
            paste_payload("a\r\nb\rc"),
            b"\x1b[200~a\nb\nc\x1b[201~".to_vec()
        );
        assert_eq!(
            paste_payload("x\x1b[201~y\x07z"),
            b"\x1b[200~x[201~yz\x1b[201~".to_vec()
        );
    }
}
