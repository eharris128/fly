//! Claude Code's live per-session state files
//! (feed-question-screen-fallback U4, KTD4).
//!
//! Claude Code ≥ 2.1.206 maintains `~/.claude/sessions/<pid>.json` for each
//! running session: `{sessionId, cwd, status, waitingFor?, statusUpdatedAt}`
//! (verified live 2026-07-10). `status: "waiting"` while a permission dialog
//! *or* an AskUserQuestion picker is open (the file labels both
//! `waitingFor: "permission prompt"`), flipping to `busy`/`idle`/`shell` when
//! the dialog resolves — a durable, poll-able pending/cleared corroborator.
//!
//! Two hard limits, both verified live, shape how the caller may use this:
//! - No question body — corroboration only, never a content source.
//! - An entry can say `waiting` for a pane fly no longer serves (the process
//!   outlives the pane's roster publication), so this file must never be the
//!   existence authority — the published roster is (feed KTD2).
//!
//! fly only ever reads under `~/.claude`; it writes nothing there (the same
//! posture as `transcript.rs`).

use std::path::{Path, PathBuf};

/// One session's waiting state, as the fallback consumes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitingState {
    /// `status == "waiting"` — the dialog-open corroborator.
    pub waiting: bool,
    /// Epoch ms the status last changed. Stable while a dialog is open, so it
    /// doubles as the ask-time stamp when no attention raise was recorded
    /// (KTD5 fallback stamp).
    pub status_updated_at_ms: u64,
}

/// `~/.claude/sessions` (honoring `CLAUDE_CONFIG_DIR`, mirroring
/// `transcript::claude_projects_root`). `None` when no root resolves.
pub fn claude_sessions_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Some(PathBuf::from(dir).join("sessions"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude/sessions"))
}

/// The waiting state of `session_id` under `root`, or `None` when the dir is
/// unreadable or no entry matches — "not corroborated", never an error. The
/// files are pid-keyed, so this scans the dir (a handful of ~350-byte files)
/// and matches on the embedded `sessionId`; it runs only on the fallback
/// path, never per frame for settled agents.
pub fn waiting_state(root: &Path, session_id: &str) -> Option<WaitingState> {
    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Some(state) = waiting_state_from_str(&body, session_id) {
            return Some(state);
        }
    }
    None
}

/// Pure core of [`waiting_state`]: parse one session file defensively and
/// return its state iff it belongs to `session_id`. Unknown fields ignored;
/// a missing `status`/`statusUpdatedAt` abstains (the contract is
/// undocumented — KTD1 posture).
fn waiting_state_from_str(body: &str, session_id: &str) -> Option<WaitingState> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    if v.get("sessionId").and_then(|s| s.as_str()) != Some(session_id) {
        return None;
    }
    let status = v.get("status").and_then(|s| s.as_str())?;
    let status_updated_at_ms = v.get("statusUpdatedAt").and_then(|t| t.as_u64())?;
    Some(WaitingState {
        waiting: status == "waiting",
        status_updated_at_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The live shape captured 2026-07-10 (Claude Code 2.1.206), trimmed.
    const WAITING: &str = r#"{"pid":91405,"sessionId":"8fbf500d-70c2-4868-b752-76c7c6a11b36","cwd":"/home/alice/projects/p2","startedAt":1783681928870,"version":"2.1.206","kind":"interactive","status":"waiting","updatedAt":1783681938899,"statusUpdatedAt":1783681938899,"waitingFor":"permission prompt"}"#;
    const BUSY: &str = r#"{"pid":11569,"sessionId":"c3892a3a-0000-0000-0000-000000000000","cwd":"/home/alice/projects/fly","status":"busy","updatedAt":1783685291836,"statusUpdatedAt":1783685291836}"#;

    #[test]
    fn matches_by_session_id_and_reads_waiting() {
        let s = waiting_state_from_str(WAITING, "8fbf500d-70c2-4868-b752-76c7c6a11b36")
            .expect("matches");
        assert!(s.waiting);
        assert_eq!(s.status_updated_at_ms, 1_783_681_938_899);
        // A different session id does not match.
        assert_eq!(waiting_state_from_str(WAITING, "other-id"), None);
    }

    #[test]
    fn non_waiting_status_reads_false() {
        let s = waiting_state_from_str(BUSY, "c3892a3a-0000-0000-0000-000000000000")
            .expect("matches");
        assert!(!s.waiting);
    }

    #[test]
    fn malformed_or_incomplete_entries_abstain() {
        assert_eq!(waiting_state_from_str("not json", "x"), None);
        assert_eq!(waiting_state_from_str("{}", "x"), None);
        // Missing statusUpdatedAt → abstain (unguardable stamp).
        let no_stamp = r#"{"sessionId":"x","status":"waiting"}"#;
        assert_eq!(waiting_state_from_str(no_stamp, "x"), None);
    }

    #[test]
    fn dir_scan_finds_the_matching_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("91405.json"), WAITING).unwrap();
        std::fs::write(dir.path().join("11569.json"), BUSY).unwrap();
        std::fs::write(dir.path().join("junk.txt"), "ignored").unwrap();
        let s = waiting_state(dir.path(), "8fbf500d-70c2-4868-b752-76c7c6a11b36")
            .expect("found");
        assert!(s.waiting);
        // Unknown id / missing dir → None, never an error.
        assert_eq!(waiting_state(dir.path(), "ghost"), None);
        assert_eq!(waiting_state(Path::new("/nonexistent-fly-test"), "x"), None);
    }
}
