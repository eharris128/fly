//! Handoff-target resolution (session-handoff U1; R4/R5/R12,
//! docs/plans/2026-07-02-001-feat-session-handoff-plan.md).
//!
//! Resolves a leaf's *previous* session into a spawnable target for the handoff
//! chords (U2): read the leaf's durable [`super::resume`] record — not the
//! live-only `pane_session_id`, whose 15-minute recency window fails for an
//! agent that exited hours ago (R4) — derive the transcript path from the
//! record's `session_cwd` + `session_id` via [`super::transcript`]'s encoding,
//! and qualify with `session_last_turn_ms`: the record must exist, the
//! transcript file must exist, and it must contain at least one real
//! conversation turn — a metadata-only transcript never qualifies (R5).
//! Resolution happens at call time, never from a snapshot — session ids rotate
//! on `/clear` (KTD1). The core is path-taking (records + projects root as
//! arguments), mirroring [`super::transcript`], so it is tested without a
//! running app.

use std::path::Path;

use super::resume::ResumeRecords;
use super::transcript;

/// A qualified previous session, ready to hand to a fresh agent (U1). The
/// transcript path is backend-derived (KTD1: path derivation stays
/// backend-only). `session_cwd` is the **record's** cwd, `None` when the record
/// never captured one — the frontend spawns there when present, falling back to
/// the pane's live cwd (R12; precedence decided here, the fallback applied in
/// U2). `last_turn_ms` is the last real turn's Unix-ms stamp — present by
/// construction, since a target only qualifies with at least one real turn
/// (R5). Serialized camelCase, the repo's wire-contract convention.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffTarget {
    pub session_id: String,
    pub transcript_path: String,
    pub session_cwd: Option<String>,
    pub last_turn_ms: u64,
}

/// The basename gate on hook-reported ids: a `/`- or `..`-bearing id could
/// steer the derived transcript path outside `projects_root` — which then rides
/// the `--add-dir` grant and the stock prompt. The validator itself now lives
/// in [`super::resume`] and is applied at every **write** too (fix-session-pane-
/// attribution KTD8/R15); this read-side check stays as defense in depth for a
/// store written by an older binary. A rejected id degrades to `None` — the R6
/// notice — exactly like the other disqualifiers.
use super::resume::is_plausible_session_id as is_plausible_transcript_basename;

/// Path-taking core of [`resolve_handoff_target`]: resolve `leaf_key`'s record
/// in `records` against the transcript store rooted at `projects_root`.
///
/// R12 precedence: the record's `session_cwd` wins over the caller-supplied
/// `live_cwd` for deriving the transcript path — the transcript and the
/// worktree the new agent sees stay coherent. `live_cwd` is only the derivation
/// fallback for a record that never captured its cwd (in which case the
/// target's own `session_cwd` stays `None`, so U2's live-cwd spawn fallback
/// applies to the same directory the transcript was found under).
pub fn resolve_in_root(
    records: &ResumeRecords,
    leaf_key: &str,
    live_cwd: Option<&str>,
    projects_root: &Path,
) -> Option<HandoffTarget> {
    let rec = records.get(leaf_key)?;
    let session_id = rec.session_id.as_deref()?;
    // Gate the hook-reported id before any path math (see the helper above).
    if !is_plausible_transcript_basename(session_id) {
        return None;
    }
    // R12 precedence: the record's cwd, then the caller's live cwd, else bail.
    let cwd = rec.session_cwd.as_deref().or(live_cwd)?;
    let transcript_path = projects_root
        .join(transcript::encode_cwd(cwd))
        .join(format!("{session_id}.jsonl"));
    // R5's whole qualification in one probe: a missing/unreadable file and a
    // metadata-only body both come back `None` (bounded, checked timestamp
    // arithmetic inside — safe under release overflow-checks = false).
    let last_turn_ms = transcript::session_last_turn_ms(&transcript_path)?;
    Some(HandoffTarget {
        session_id: session_id.to_string(),
        transcript_path: transcript_path.to_string_lossy().into_owned(),
        session_cwd: rec.session_cwd.clone(),
        last_turn_ms,
    })
}

/// Command: resolve the focused leaf's previous session into a spawnable
/// handoff target, or `None` when nothing qualifies — no record, no transcript
/// file, or no real conversation turn (R4/R5; the `None` feeds the R6 notice).
/// Reads the resume store at call time, never from a snapshot. `live_cwd` is
/// the pane's current cwd, used only as the derivation fallback above.
#[tauri::command]
pub fn resolve_handoff_target(
    leaf_key: String,
    live_cwd: Option<String>,
) -> Option<HandoffTarget> {
    let records = super::resume::load_resume_records();
    let root = transcript::claude_projects_root()?;
    resolve_in_root(&records, &leaf_key, live_cwd.as_deref(), &root)
}

// ---- pick-list candidates (fix-session-pane-attribution U5) -----------------

/// One pick-list row (U5; R6/R7): a spawnable [`HandoffTarget`] plus the
/// display-only snippet of its most recent text-bearing turn. Selecting a
/// candidate proceeds exactly as if the target had been precisely captured
/// (R8) — the flattened target IS the spawn contract.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffCandidate {
    #[serde(flatten)]
    pub target: HandoffTarget,
    pub snippet: Option<String>,
}

/// Cap on candidates for one cwd (the plan's deferred count question, resolved
/// here): a long-lived project dir can hold hundreds of transcripts, each of
/// which costs a full-body read to qualify — and a pick-list past ~20 rows is
/// unusable anyway. Files are visited newest-mtime-first, so the cap sheds the
/// *oldest* history.
const CANDIDATE_CAP: usize = 20;

/// Path-taking core of [`list_handoff_candidates`]: every real-turn-qualified
/// transcript in the leaf's cwd, **not** gated by the poll's freshness window
/// (KTD4/KTD7 — reset stays non-lossy: an aged-out true target is still
/// selectable), ordered by last real turn descending. The scan cwd follows
/// [`resolve_in_root`]'s precedence — the record's `session_cwd`, then
/// `live_cwd`. Only validated basenames become candidates (KTD8); a
/// metadata-only transcript never qualifies (R11 counts on the empty vec).
pub fn list_candidates_in_root(
    records: &ResumeRecords,
    leaf_key: &str,
    live_cwd: Option<&str>,
    projects_root: &Path,
) -> Vec<HandoffCandidate> {
    let rec = records.get(leaf_key);
    let Some(cwd) = rec
        .and_then(|r| r.session_cwd.as_deref())
        .or(live_cwd)
        .map(str::to_string)
    else {
        return Vec::new();
    };
    let dir = projects_root.join(transcript::encode_cwd(&cwd));
    // Newest-mtime-first so the CANDIDATE_CAP sheds the oldest transcripts
    // without having paid a body read for them (mtime is only the visiting
    // order; the displayed/sorted stamp is the last real turn).
    let mut entries = transcript::read_project_entries(&dir);
    entries.sort_by(|a, b| b.1.cmp(&a.1));

    let mut out = Vec::new();
    for (name, _) in &entries {
        if out.len() >= CANDIDATE_CAP {
            break;
        }
        let Some(id) = transcript::jsonl_id(name) else {
            continue;
        };
        if !super::resume::is_plausible_session_id(id) {
            continue; // KTD8: never emit an implausible basename
        }
        let path = dir.join(name);
        let Some(summary) = transcript::session_turn_summary(&path) else {
            continue; // no real turn → not a candidate (R5/R11)
        };
        out.push(HandoffCandidate {
            target: HandoffTarget {
                session_id: id.to_string(),
                transcript_path: path.to_string_lossy().into_owned(),
                session_cwd: Some(cwd.clone()),
                last_turn_ms: summary.last_turn_ms,
            },
            snippet: summary.snippet,
        });
    }
    out.sort_by(|a, b| b.target.last_turn_ms.cmp(&a.target.last_turn_ms));
    out
}

/// Command: the cwd's qualifying sessions for the pick-list (U5; R6/R7/R11),
/// last-activity first, aged-out targets included. An empty vec drives the
/// existing "no previous session" notice — never an empty picker (R11).
#[tauri::command]
pub fn list_handoff_candidates(
    leaf_key: String,
    live_cwd: Option<String>,
) -> Vec<HandoffCandidate> {
    let records = super::resume::load_resume_records();
    let Some(root) = transcript::claude_projects_root() else {
        return Vec::new();
    };
    list_candidates_in_root(&records, &leaf_key, live_cwd.as_deref(), &root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::session::resume::ResumeRecord;

    /// A transcript with two real turns then a metadata tail — the qualifying
    /// shape (mirrors the real store; same stamps as `transcript.rs`'s fixture).
    const TURNFUL_FIXTURE: &str = concat!(
        r#"{"type":"mode","sessionId":"s"}"#,
        "\n",
        r#"{"type":"user","timestamp":"2026-06-19T19:17:04.506Z","message":{"role":"user"}}"#,
        "\n",
        r#"{"type":"assistant","timestamp":"2026-06-19T19:17:16.402Z","message":{"role":"assistant"}}"#,
        "\n",
        r#"{"type":"ai-title"}"#,
        "\n",
    );
    /// The fixture's last real turn (cross-checked in `transcript.rs` tests).
    const LAST_TURN_MS: u64 = 1_781_896_636_402;

    /// The AE3 shape: a transcript a metadata-only open produced — the file
    /// exists but carries no timestamped conversation turn.
    const METADATA_ONLY_FIXTURE: &str = concat!(
        r#"{"type":"mode","sessionId":"s"}"#,
        "\n",
        r#"{"type":"permission-mode"}"#,
        "\n",
        r#"{"type":"ai-title"}"#,
        "\n",
    );

    fn records_with(
        session_id: Option<&str>,
        session_cwd: Option<&str>,
    ) -> ResumeRecords {
        let mut records = ResumeRecords::new();
        records.insert(
            "leaf-1".to_string(),
            ResumeRecord {
                session_id: session_id.map(str::to_string),
                session_cwd: session_cwd.map(str::to_string),
                argv: Some(vec!["claude".into()]),
                is_agent: true,
                ..Default::default()
            },
        );
        records
    }

    fn write_transcript(
        root: &Path,
        encoded_dir: &str,
        session_id: &str,
        body: &str,
    ) -> PathBuf {
        let dir = root.join(encoded_dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{session_id}.jsonl"));
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn resolves_a_turnful_transcript_at_the_exact_encoded_path() {
        let root = tempfile::tempdir().unwrap();
        // Both `/` and `.` encode to `-` (KTD-E), so the `.obsidian` segment
        // yields a double dash in the project-dir name.
        let records = records_with(Some("sess-1"), Some("/home/evan/.obsidian/notes"));
        let expected = write_transcript(
            root.path(),
            "-home-evan--obsidian-notes",
            "sess-1",
            TURNFUL_FIXTURE,
        );

        let target = resolve_in_root(&records, "leaf-1", None, root.path())
            .expect("a turnful transcript qualifies");
        assert_eq!(target.session_id, "sess-1");
        assert_eq!(target.transcript_path, expected.to_string_lossy());
        assert_eq!(
            target.session_cwd.as_deref(),
            Some("/home/evan/.obsidian/notes")
        );
        assert_eq!(target.last_turn_ms, LAST_TURN_MS);
    }

    #[test]
    fn no_record_for_the_leaf_is_none() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_in_root(&ResumeRecords::new(), "leaf-1", None, root.path()),
            None
        );
        // A record that never captured a session id doesn't qualify either (R5).
        let records = records_with(None, Some("/proj/app"));
        assert_eq!(resolve_in_root(&records, "leaf-1", None, root.path()), None);
    }

    #[test]
    fn a_deleted_transcript_is_none() {
        let root = tempfile::tempdir().unwrap();
        // Record present, but no transcript file was ever written (or it has
        // since been deleted) → nothing qualifies (R5).
        let records = records_with(Some("sess-1"), Some("/proj/app"));
        assert_eq!(resolve_in_root(&records, "leaf-1", None, root.path()), None);
    }

    #[test]
    fn a_metadata_only_transcript_is_none() {
        // AE3: the transcript file exists but holds no real conversation turn —
        // handing a fresh agent a contentless session would be a dead pickup.
        let root = tempfile::tempdir().unwrap();
        let records = records_with(Some("sess-1"), Some("/proj/app"));
        write_transcript(root.path(), "-proj-app", "sess-1", METADATA_ONLY_FIXTURE);
        assert_eq!(resolve_in_root(&records, "leaf-1", None, root.path()), None);
    }

    #[test]
    fn a_session_id_that_is_not_a_plausible_basename_is_none() {
        // Hardening (review follow-up): the id is hook-reported, so a
        // separator-bearing one must never reach the path join — even when a
        // file actually exists at the traversed location.
        let root = tempfile::tempdir().unwrap();
        let records = records_with(Some("a/b"), Some("/proj/app"));
        write_transcript(root.path(), "-proj-app/a", "b", TURNFUL_FIXTURE);
        assert_eq!(resolve_in_root(&records, "leaf-1", None, root.path()), None);

        // The classic traversal shape is rejected before any derivation.
        let records = records_with(Some("../../../etc/passwd"), Some("/proj/app"));
        assert_eq!(resolve_in_root(&records, "leaf-1", None, root.path()), None);

        // An empty id and a dotdot-smuggling (separator-free) id don't
        // qualify either.
        let records = records_with(Some(""), Some("/proj/app"));
        assert_eq!(resolve_in_root(&records, "leaf-1", None, root.path()), None);
        let records = records_with(Some("..sess"), Some("/proj/app"));
        assert_eq!(resolve_in_root(&records, "leaf-1", None, root.path()), None);
    }

    // ---- list_candidates_in_root (fix-attribution U5) -----------------------

    /// A one-turn transcript body with a chosen stamp + user text, so tests can
    /// stage several sessions with distinct last-activity times and snippets.
    fn turnful_body(ts: &str, text: &str) -> String {
        format!(
            concat!(
                r#"{{"type":"mode","sessionId":"s"}}"#,
                "\n",
                r#"{{"type":"user","timestamp":"{ts}","message":{{"role":"user","content":"{text}"}}}}"#,
                "\n",
            ),
            ts = ts,
            text = text,
        )
    }

    #[test]
    fn lists_qualifying_candidates_ordered_by_last_turn_desc() {
        // R6/R7 + KTD7: every real-turn transcript qualifies — including one
        // whose activity long predates any freshness window (reset stays
        // non-lossy) — ordered newest-turn-first with recognizable snippets.
        // Metadata-only files, implausible basenames, and non-transcripts are
        // never emitted (R11/KTD8).
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("-proj-app");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("mid.jsonl"),
            turnful_body("2026-06-19T00:00:00Z", "refactor the store"),
        )
        .unwrap();
        std::fs::write(
            dir.join("aged-out.jsonl"),
            turnful_body("2026-01-01T00:00:00Z", "ancient work"),
        )
        .unwrap();
        std::fs::write(
            dir.join("newest.jsonl"),
            turnful_body("2026-06-25T00:00:00Z", "fix the picker"),
        )
        .unwrap();
        std::fs::write(dir.join("meta-only.jsonl"), METADATA_ONLY_FIXTURE).unwrap();
        std::fs::write(
            dir.join("a..b.jsonl"),
            turnful_body("2026-06-26T00:00:00Z", "smuggled"),
        )
        .unwrap();
        std::fs::write(dir.join("notes.txt"), "not a transcript").unwrap();

        let got = list_candidates_in_root(
            &ResumeRecords::new(),
            "leaf-1",
            Some("/proj/app"),
            root.path(),
        );
        let ids: Vec<&str> = got.iter().map(|c| c.target.session_id.as_str()).collect();
        assert_eq!(ids, vec!["newest", "mid", "aged-out"], "last-turn desc");
        assert_eq!(got[0].snippet.as_deref(), Some("fix the picker"));
        assert_eq!(got[2].snippet.as_deref(), Some("ancient work"));
        assert!(got[0].target.last_turn_ms > got[1].target.last_turn_ms);
        assert_eq!(got[0].target.session_cwd.as_deref(), Some("/proj/app"));
        assert!(got[0].target.transcript_path.ends_with("newest.jsonl"));
    }

    #[test]
    fn no_qualifying_transcripts_is_an_empty_vec() {
        // R11: the empty vec drives the "no previous session" notice — never
        // an empty picker. Covers: no cwd at all, an absent project dir, and a
        // dir holding only a metadata-only transcript.
        let root = tempfile::tempdir().unwrap();
        let none = list_candidates_in_root(&ResumeRecords::new(), "leaf-1", None, root.path());
        assert!(none.is_empty(), "no cwd from anywhere");
        let missing = list_candidates_in_root(
            &ResumeRecords::new(),
            "leaf-1",
            Some("/proj/app"),
            root.path(),
        );
        assert!(missing.is_empty(), "project dir absent");
        write_transcript(root.path(), "-proj-app", "sess-1", METADATA_ONLY_FIXTURE);
        let meta_only = list_candidates_in_root(
            &ResumeRecords::new(),
            "leaf-1",
            Some("/proj/app"),
            root.path(),
        );
        assert!(meta_only.is_empty(), "metadata-only never qualifies");
    }

    #[test]
    fn candidates_scan_the_records_cwd_before_the_live_cwd() {
        // The scan cwd mirrors resolve_in_root's R12 precedence, so the picker
        // and the single-target path always look in the same place.
        let root = tempfile::tempdir().unwrap();
        let records = records_with(None, Some("/proj/recorded"));
        write_transcript(
            root.path(),
            "-proj-recorded",
            "recorded-sess",
            &turnful_body("2026-06-19T00:00:00Z", "recorded work"),
        );
        write_transcript(
            root.path(),
            "-proj-live",
            "live-sess",
            &turnful_body("2026-06-19T00:00:00Z", "live work"),
        );

        let got =
            list_candidates_in_root(&records, "leaf-1", Some("/proj/live"), root.path());
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].target.session_id, "recorded-sess");
        // A leaf with no record scans the live cwd.
        let got = list_candidates_in_root(
            &ResumeRecords::new(),
            "leaf-1",
            Some("/proj/live"),
            root.path(),
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].target.session_id, "live-sess");
    }

    #[test]
    fn candidate_serialization_flattens_the_target() {
        // The wire shape the picker consumes: target fields at the top level
        // (camelCase) beside `snippet` — ipc.ts mirrors this as an interface
        // extension.
        let c = HandoffCandidate {
            target: HandoffTarget {
                session_id: "s".into(),
                transcript_path: "/t.jsonl".into(),
                session_cwd: Some("/p".into()),
                last_turn_ms: 5,
            },
            snippet: Some("hi".into()),
        };
        let v: serde_json::Value = serde_json::to_value(&c).unwrap();
        assert_eq!(v["sessionId"], "s");
        assert_eq!(v["lastTurnMs"], 5);
        assert_eq!(v["snippet"], "hi");
    }

    #[test]
    fn the_records_cwd_wins_over_the_callers_live_cwd() {
        // R12 precedence: the transcript exists only under the RECORD's encoded
        // cwd — resolving via the live cwd would find nothing — and the target
        // carries the record's cwd, not the caller's.
        let root = tempfile::tempdir().unwrap();
        let records = records_with(Some("sess-1"), Some("/proj/recorded"));
        let expected =
            write_transcript(root.path(), "-proj-recorded", "sess-1", TURNFUL_FIXTURE);

        let target = resolve_in_root(&records, "leaf-1", Some("/proj/live"), root.path())
            .expect("qualifies via the record's cwd");
        assert_eq!(
            target.session_cwd.as_deref(),
            Some("/proj/recorded"),
            "R12: the record's cwd rides the target"
        );
        assert_eq!(target.transcript_path, expected.to_string_lossy());
    }

    #[test]
    fn a_record_missing_its_cwd_falls_back_to_the_live_cwd_for_derivation() {
        let root = tempfile::tempdir().unwrap();
        let records = records_with(Some("sess-1"), None);
        let expected = write_transcript(root.path(), "-proj-live", "sess-1", TURNFUL_FIXTURE);

        let target = resolve_in_root(&records, "leaf-1", Some("/proj/live"), root.path())
            .expect("derives from the live cwd when the record has none");
        assert_eq!(target.transcript_path, expected.to_string_lossy());
        // The target's own cwd stays None — U2's `sessionCwd ?? live` fallback
        // then spawns in the same directory the transcript was found under.
        assert_eq!(target.session_cwd, None);
        // And with no cwd from anywhere, nothing resolves.
        assert_eq!(resolve_in_root(&records, "leaf-1", None, root.path()), None);
    }
}
