//! Write-through resume store + clean-exit marker (U2; R2/R3/R9/R10).
//!
//! A small, crash-durable, backend-owned store of per-leaf resume records,
//! separate from the debounced layout blob (`session/mod.rs`, KTD-D). Each upsert
//! flushes immediately (atomic temp + rename), so the last-known agent mapping is
//! on disk even when an unclean shutdown (OOM kill, `SIGKILL`, power loss, WebKit
//! renderer crash) skips fly's ordered teardown.
//!
//! Two writers serialize through the backend so they never race on the file:
//! the **hook path** upserts `session_id` + `session_cwd` (U3), and the **poll
//! path** upserts `argv` + `is_agent` (U4). Upserts are field-merging — a
//! partial sets only the fields it knows, leaving the other writer's fields
//! intact.
//!
//! The **clean-exit marker** is a tiny sentinel cleared at startup and written on
//! the ordered shutdown (KTD-G): a marker that is *absent* at the next startup
//! means the previous run died uncleanly, which drives the crash auto-offer (U7).
//!
//! All paths resolve under the `FLY_APP_NAME` root (via `super::data_dir`), so a
//! dev flavor's resume state stays isolated from an installed release (R10).

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One agent leaf's resume mapping. `argv` is the captured launch command (the
/// flag source); `session_cwd` is the hook-reported project dir resume runs in
/// (KTD-H). Serialized camelCase so the frontend reads it directly (U5/U8).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeRecord {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub session_cwd: Option<String>,
    #[serde(default)]
    pub argv: Option<Vec<String>>,
    #[serde(default)]
    pub is_agent: bool,
    #[serde(default)]
    pub updated_at: u64,
}

/// A field-merging upsert: each `Some` field overwrites, each `None` is left
/// untouched, so the hook writer (`session_id`/`session_cwd`) and the poll writer
/// (`argv`/`is_agent`) never clobber each other.
#[derive(Debug, Clone, Default)]
pub struct ResumePartial {
    pub session_id: Option<String>,
    pub session_cwd: Option<String>,
    pub argv: Option<Vec<String>>,
    pub is_agent: Option<bool>,
}

pub type ResumeRecords = BTreeMap<String, ResumeRecord>;

// ---- path-taking core (filesystem-pure, tested like session.rs) ------------

/// Read all records, or an empty map if missing/corrupt. A corrupt file is
/// renamed aside (not overwritten) so a malformed store never loses the data and
/// the caller degrades to "no records" — the same fallback as `read_session`.
pub fn read_records(path: &Path) -> ResumeRecords {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return ResumeRecords::new(),
    };
    match serde_json::from_slice::<ResumeRecords>(&bytes) {
        Ok(map) => map,
        Err(_) => {
            let backup = PathBuf::from(format!("{}.bad.bak", path.display()));
            let _ = std::fs::rename(path, &backup);
            ResumeRecords::new()
        }
    }
}

/// Write all records atomically (temp + rename) as a `0600` file in a `0700`
/// dir — the mode is set on the temp file *before* the rename, so there is never
/// a world-readable window (the session ids and cwds are sensitive). Mirrors
/// `write_session`.
pub fn write_records(path: &Path, records: &ResumeRecords) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(records)?)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Field-merge `partial` into the record for `leaf_key` and flush write-through.
pub fn upsert_at(path: &Path, leaf_key: &str, partial: ResumePartial) -> std::io::Result<()> {
    let mut records = read_records(path);
    let rec = records.entry(leaf_key.to_string()).or_default();
    if let Some(v) = partial.session_id {
        rec.session_id = Some(v);
    }
    if let Some(v) = partial.session_cwd {
        rec.session_cwd = Some(v);
    }
    if let Some(v) = partial.argv {
        rec.argv = Some(v);
    }
    if let Some(v) = partial.is_agent {
        rec.is_agent = v;
    }
    rec.updated_at = crate::notify::now_unix_ms();
    write_records(path, &records)
}

/// Remove one leaf's record (used to prune an orphan whose layout leaf is gone),
/// leaving the others. A no-op write is skipped when nothing was removed.
pub fn prune_at(path: &Path, leaf_key: &str) -> std::io::Result<()> {
    let mut records = read_records(path);
    if records.remove(leaf_key).is_some() {
        write_records(path, &records)?;
    }
    Ok(())
}

/// Keep only the records whose leaf key is still live, dropping orphans whose
/// layout leaf is gone (U8) so the store stays bounded across sessions. Writes
/// only when something was actually removed.
pub fn retain_at(
    path: &Path,
    live: &std::collections::HashSet<String>,
) -> std::io::Result<()> {
    let mut records = read_records(path);
    let before = records.len();
    records.retain(|k, _| live.contains(k));
    if records.len() != before {
        write_records(path, &records)?;
    }
    Ok(())
}

/// Set (`true`) or clear (`false`) the clean-exit marker. Set on the ordered
/// shutdown, cleared at startup (KTD-G).
pub fn set_clean_exit_at(path: &Path, clean: bool) -> std::io::Result<()> {
    if clean {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
        std::fs::write(path, b"1")?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    } else {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

/// Whether the previous run exited cleanly — i.e. the marker is present. Absent
/// ⇒ the prior run crashed (the marker was never written, or this run cleared it
/// at startup and then crashed before writing it again).
pub fn took_clean_exit_at(path: &Path) -> bool {
    path.exists()
}

// ---- default-path wrappers + command surface -------------------------------

/// The resume store path under the `FLY_APP_NAME` data root (R10).
pub fn resume_path() -> PathBuf {
    super::data_dir().join("resume.json")
}

/// The clean-exit marker path under the same root.
pub fn clean_exit_path() -> PathBuf {
    super::data_dir().join("clean-exit")
}

/// Command: the frontend loads the resume map at restore (U8). Returns an empty
/// map when the store is missing/corrupt — never an error.
#[tauri::command]
pub fn load_resume_records() -> ResumeRecords {
    read_records(&resume_path())
}

/// Command: the always-on poll captures an agent leaf's launch argv (U4). The
/// poll only calls this for a detected Claude pane, so `is_agent` is implied
/// `true`. Field-merging, so it never clobbers the hook writer's session id/cwd.
#[tauri::command]
pub fn save_resume_record(leaf_key: String, argv: Vec<String>) -> Result<(), String> {
    upsert_at(
        &resume_path(),
        &leaf_key,
        ResumePartial {
            argv: Some(argv),
            is_agent: Some(true),
            ..Default::default()
        },
    )
    .map_err(|e| e.to_string())
}

/// Command: the always-on poll captures an agent leaf's active session id
/// write-through (fix-resume-session-selection U2). The id comes from Claude's
/// transcript store (KTD-A), so capture is independent of the installed `fly`
/// binary's version — the skew that silently disabled the hook path — and fires
/// before the first `Notification`/`Stop`. Writes the **same** `session_id` +
/// `session_cwd` partial the hook path does (`lib.rs`), so the two precise sources
/// are interchangeable; field-merging, so it never clobbers the poll's
/// `argv`/`is_agent`. All writers target the pane's one active session, so
/// last-writer-wins is harmless (KTD-A/B).
#[tauri::command]
pub fn save_resume_session(
    leaf_key: String,
    session_id: String,
    session_cwd: Option<String>,
) -> Result<(), String> {
    upsert_at(
        &resume_path(),
        &leaf_key,
        ResumePartial {
            session_id: Some(session_id),
            session_cwd,
            ..Default::default()
        },
    )
    .map_err(|e| e.to_string())
}

/// Command: at restore the frontend prunes the store to the live layout leaves
/// (U8), dropping records orphaned by a closed pane or a pre-crash layout.
#[tauri::command]
pub fn prune_resume_records(live_leaf_keys: Vec<String>) -> Result<(), String> {
    let live: std::collections::HashSet<String> = live_leaf_keys.into_iter().collect();
    retain_at(&resume_path(), &live).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(session_id: Option<&str>) -> ResumePartial {
        ResumePartial {
            session_id: session_id.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn upsert_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resume.json");
        upsert_at(&path, "leaf-1", record(Some("sess-1"))).unwrap();
        let loaded = read_records(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded["leaf-1"].session_id.as_deref(), Some("sess-1"));
    }

    #[test]
    fn field_merging_upsert_does_not_clobber() {
        // The hook writes sessionId/cwd; the poll writes argv/isAgent. Two
        // partials for one leaf must merge into a single complete record.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resume.json");
        upsert_at(
            &path,
            "leaf-7",
            ResumePartial {
                session_id: Some("s".into()),
                session_cwd: Some("/proj".into()),
                ..Default::default()
            },
        )
        .unwrap();
        upsert_at(
            &path,
            "leaf-7",
            ResumePartial {
                argv: Some(vec!["claude".into(), "--model".into(), "opus".into()]),
                is_agent: Some(true),
                ..Default::default()
            },
        )
        .unwrap();

        let rec = &read_records(&path)["leaf-7"];
        assert_eq!(rec.session_id.as_deref(), Some("s"), "hook field survived");
        assert_eq!(rec.session_cwd.as_deref(), Some("/proj"));
        assert_eq!(
            rec.argv.as_deref(),
            Some(["claude".to_string(), "--model".into(), "opus".into()].as_slice()),
            "poll field merged in"
        );
        assert!(rec.is_agent);
    }

    #[test]
    fn session_capture_merges_over_argv_and_rotates_in_place() {
        // U2 (fix-003): the argv poll captures argv+isAgent once; the session poll
        // then upserts the transcript-derived id (the partial save_resume_session
        // builds). The id must merge in without clobbering argv/isAgent — and a
        // later /clear that rotates the active session must overwrite only the id.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resume.json");

        // argv path (save_resume_record): captured once, early.
        upsert_at(
            &path,
            "leaf-5",
            ResumePartial {
                argv: Some(vec!["claude".into(), "--continue".into()]),
                is_agent: Some(true),
                ..Default::default()
            },
        )
        .unwrap();

        // session path (save_resume_session): id + cwd, no argv/is_agent.
        upsert_at(
            &path,
            "leaf-5",
            ResumePartial {
                session_id: Some("sess-A".into()),
                session_cwd: Some("/home/evan/projects/play".into()),
                ..Default::default()
            },
        )
        .unwrap();

        let rec = &read_records(&path)["leaf-5"];
        assert_eq!(rec.session_id.as_deref(), Some("sess-A"));
        assert_eq!(rec.session_cwd.as_deref(), Some("/home/evan/projects/play"));
        assert_eq!(
            rec.argv.as_deref(),
            Some(["claude".to_string(), "--continue".into()].as_slice()),
            "argv survived the session upsert"
        );
        assert!(rec.is_agent, "is_agent survived the session upsert");

        // /clear rotates the active session → a new id overwrites only session_id.
        upsert_at(
            &path,
            "leaf-5",
            ResumePartial {
                session_id: Some("sess-B".into()),
                session_cwd: Some("/home/evan/projects/play".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let rec = &read_records(&path)["leaf-5"];
        assert_eq!(rec.session_id.as_deref(), Some("sess-B"), "id rotated in place");
        assert!(rec.is_agent, "is_agent still intact after rotation");
        assert_eq!(
            rec.argv.as_deref(),
            Some(["claude".to_string(), "--continue".into()].as_slice()),
        );
    }

    #[test]
    fn argv_with_spaces_round_trips_as_distinct_elements() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resume.json");
        upsert_at(
            &path,
            "leaf-1",
            ResumePartial {
                argv: Some(vec!["claude".into(), "write a poem".into()]),
                is_agent: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        let rec = &read_records(&path)["leaf-1"];
        assert_eq!(
            rec.argv.as_deref(),
            Some(["claude".to_string(), "write a poem".into()].as_slice())
        );
    }

    #[test]
    fn missing_store_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_records(&dir.path().join("nope.json")).is_empty());
    }

    #[test]
    fn corrupt_store_returns_empty_and_backs_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resume.json");
        std::fs::write(&path, "{ not valid json").unwrap();
        assert!(read_records(&path).is_empty());
        let backup = PathBuf::from(format!("{}.bad.bak", path.display()));
        assert!(backup.exists(), "corrupt store preserved, not lost");
    }

    #[test]
    fn store_file_is_owner_only_in_owner_only_dir() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("state");
        let path = sub.join("resume.json");
        upsert_at(&path, "leaf-1", record(Some("s"))).unwrap();
        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let dir_mode = std::fs::metadata(&sub).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "resume store must be 0600");
        assert_eq!(dir_mode, 0o700, "data dir must be 0700");
    }

    #[test]
    fn prune_removes_one_record_leaving_others() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resume.json");
        upsert_at(&path, "leaf-1", record(Some("a"))).unwrap();
        upsert_at(&path, "leaf-2", record(Some("b"))).unwrap();
        prune_at(&path, "leaf-1").unwrap();
        let loaded = read_records(&path);
        assert!(!loaded.contains_key("leaf-1"));
        assert_eq!(loaded["leaf-2"].session_id.as_deref(), Some("b"));
    }

    #[test]
    fn retain_drops_orphans_keeping_live_leaves() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resume.json");
        upsert_at(&path, "leaf-1", record(Some("a"))).unwrap();
        upsert_at(&path, "leaf-2", record(Some("b"))).unwrap();
        upsert_at(&path, "orphan", record(Some("z"))).unwrap();
        let live: std::collections::HashSet<String> =
            ["leaf-1".to_string(), "leaf-2".to_string()].into_iter().collect();
        retain_at(&path, &live).unwrap();
        let loaded = read_records(&path);
        assert_eq!(loaded.len(), 2);
        assert!(loaded.contains_key("leaf-1") && loaded.contains_key("leaf-2"));
        assert!(!loaded.contains_key("orphan"));
    }

    #[test]
    fn clean_exit_marker_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("clean-exit");
        // Fresh: no marker ⇒ a prior crash would be detected.
        assert!(!took_clean_exit_at(&marker));
        // Ordered shutdown sets it.
        set_clean_exit_at(&marker, true).unwrap();
        assert!(took_clean_exit_at(&marker));
        // Startup clears it; a subsequent crash then leaves it absent.
        set_clean_exit_at(&marker, false).unwrap();
        assert!(!took_clean_exit_at(&marker), "cleared marker ⇒ crash on next load");
    }
}
