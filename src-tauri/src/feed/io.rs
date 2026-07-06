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

use crate::session::resume;
use crate::session::transcript::{self, LastReply};

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
    cache: Mutex<HashMap<String, CachedReply>>,
}

/// One leaf's memoized resolution: the transcript identity it was computed
/// from, and the reply (or confirmed absence) it yielded.
struct CachedReply {
    transcript: PathBuf,
    mtime: SystemTime,
    len: u64,
    reply: Option<LastReply>,
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

    /// The leaf's latest reply, or `None` when any link is missing: no resume
    /// record, no captured session/cwd, no transcript on disk, or a text-free
    /// assistant tail. All of those mean "no reply yet" — the endpoint's empty
    /// state — never an error. Text is control-sanitized here (R16 posture for
    /// agent-authored text; newlines/tabs kept) so every consumer of the pair
    /// sees the same bytes; a reply that sanitizes to blank counts as absent.
    pub fn resolve(&self, leaf_key: &str) -> Option<LastReply> {
        let record = resume::read_records(&self.resume_path).remove(leaf_key)?;
        let session_id = record.session_id?;
        let session_cwd = record.session_cwd?;
        let root = self.projects_root.as_ref()?;
        let path = root
            .join(transcript::encode_cwd(&session_cwd))
            .join(format!("{session_id}.jsonl"));

        let meta = std::fs::metadata(&path).ok()?;
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let len = meta.len();

        let mut cache = self.cache.lock().unwrap();
        if let Some(hit) = cache.get(leaf_key) {
            if hit.transcript == path && hit.mtime == mtime && hit.len == len {
                return hit.reply.clone();
            }
        }
        let reply = transcript::last_assistant_reply(&path).and_then(|r| {
            let text = crate::notify::sanitize_multiline(&r.text);
            (!text.trim().is_empty()).then_some(LastReply {
                text,
                replied_at_ms: r.replied_at_ms,
            })
        });
        cache.insert(
            leaf_key.to_string(),
            CachedReply {
                transcript: path,
                mtime,
                len,
                reply: reply.clone(),
            },
        );
        reply
    }
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

/// Gap between the paste write and the [`SUBMIT`] write: long enough for the
/// composer to leave paste handling and settle (human-keypress scale), short
/// enough to be imperceptible on the HTTP round-trip.
pub const SUBMIT_DELAY: std::time::Duration = std::time::Duration::from_millis(150);

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
        let r = fixture(dir.path(), "leaf-1", "sid-abc", "/home/evan/projects/play", TRANSCRIPT);
        let reply = r.resolve("leaf-1").expect("a reply resolves");
        assert_eq!(reply.text, "All tests pass.");
        assert_eq!(reply.replied_at_ms, Some(REPLIED_AT));
    }

    #[test]
    fn missing_links_resolve_to_none_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let r = fixture(dir.path(), "leaf-1", "sid-abc", "/home/evan/projects/play", TRANSCRIPT);
        // Unknown leaf (no record) → None.
        assert_eq!(r.resolve("leaf-ghost"), None);
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
        assert_eq!(r.resolve("leaf-2"), None);
        // A record whose transcript is gone from disk → None.
        std::fs::remove_dir_all(dir.path().join("projects")).unwrap();
        assert_eq!(r.resolve("leaf-1"), None);
    }

    #[test]
    fn cache_serves_unchanged_files_and_refreshes_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = "/home/evan/projects/play";
        let r = fixture(dir.path(), "leaf-1", "sid-abc", cwd, TRANSCRIPT);
        assert_eq!(r.resolve("leaf-1").unwrap().text, "All tests pass.");
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
        let reply = r.resolve("leaf-1").unwrap();
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
        assert_eq!(r.resolve("leaf-1").unwrap().text, "ok[2J\ndone");
        // A reply that is nothing but stripped chars counts as no reply.
        let blank = r#"{"type":"assistant","message":{"role":"assistant","content":"\u0007\u001b"}}"#;
        let r = fixture(dir.path(), "leaf-2", "sid-def", "/q", &format!("{blank}\n"));
        assert_eq!(r.resolve("leaf-2"), None);
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
