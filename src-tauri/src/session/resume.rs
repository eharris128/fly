//! Write-through resume store + clean-exit marker (U2; R2/R3/R9/R10) — now
//! trust-ranked, locked, and validated at ingestion
//! (fix-session-pane-attribution U3; R3/R5/R10/R15/R16, KTD2/KTD8/KTD9).
//!
//! A small, crash-durable, backend-owned store of per-leaf resume records,
//! separate from the debounced layout blob (`session/mod.rs`, KTD-D). Each upsert
//! flushes immediately (atomic temp + rename), so the last-known agent mapping is
//! on disk even when an unclean shutdown (OOM kill, `SIGKILL`, power loss, WebKit
//! renderer crash) skips fly's ordered teardown.
//!
//! Three writers feed the `session_id`, ranked by **trust** (KTD2): the
//! always-on poll (`Poll`), the token-authenticated hook path (`Hook`), and an
//! explicit human pick (`Pick`). The rank is assigned at each call site — a
//! socket message can never self-declare one (a `Hook` id is pane-precise but
//! *forgeable* by in-pane code, so it outranks the poll's cwd-level guess but
//! never a human decision). Upserts stay field-merging — the poll's
//! `argv`/`is_agent` and the session writers never clobber each other — and all
//! writes serialize under one lock so the rank compare-and-write is atomic
//! (R16, KTD9).
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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// KTD9: every store mutation (upsert / pick / prune / retain / reset) holds
/// this lock across its read-compare-write, so interleaved writers can never
/// lose-update a higher-ranked id, and two flushes never race on the file.
/// A poisoned lock is recovered (`into_inner`) — the store on disk is always a
/// complete rename'd snapshot, so continuing after a panicked writer is safe.
static STORE_LOCK: Mutex<()> = Mutex::new(());

/// Per-write temp-file discriminator (KTD9): `SessionStart` capture writes land
/// exactly when the poll notices the same new session, and a shared fixed temp
/// name would let those flushes truncate each other into the corruption
/// (`.bad.bak`) path. Pid + counter keeps each flush's temp file unique.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Where a stored `session_id` came from, ranked by trust: `Poll` (a cwd-level
/// newest-mtime guess) < `Hook` (pane-precise via the authenticated socket, but
/// forgeable by in-pane code) < `Pick` (an explicit human decision). The derive
/// order **is** the rank (`PartialOrd` by variant); serde snake_case is the wire
/// form (KTD2). Default `Poll`, so a store written by an older binary loads at
/// the lowest trust.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SessionSource {
    #[default]
    Poll,
    Hook,
    Pick,
}

/// Is `session_id` a plausible transcript **basename**? Ids originate in
/// hook-reported data (forgeable in-pane) and are joined into transcript paths,
/// so a `/`- or `..`-bearing one could steer path derivation outside the
/// projects root. Accepting only a nonempty `[A-Za-z0-9._-]` id with no `..`
/// keeps every derived path provably under its root. Applied at **every write**
/// (`upsert_at`) and every producer, not just the resolve site (KTD8, R15).
pub(crate) fn is_plausible_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        && !session_id.contains("..")
}

/// Is `session_cwd` a plausible spawn directory? It steers where a handoff
/// agent launches (and which project config it auto-loads), so a relative or
/// control-char-bearing value is rejected at write time (KTD8, R15). Char-level
/// `is_control` covers C0, DEL, and C1 without false-rejecting multi-byte UTF-8
/// paths.
pub(crate) fn is_plausible_session_cwd(session_cwd: &str) -> bool {
    session_cwd.starts_with('/') && !session_cwd.chars().any(|c| c.is_control())
}

/// One agent leaf's resume mapping. `argv` is the captured launch command (the
/// flag source); `session_cwd` is the hook-reported project dir resume runs in
/// (KTD-H). `session_source` ranks how the id was captured (KTD2);
/// `divergence_pending` flags a `Pick` whose pane's live session was hook-reported
/// as different — kept, never auto-cleared to the hook's id (R10/R14). Serialized
/// camelCase so the frontend reads it directly (U5/U8).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeRecord {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub session_cwd: Option<String>,
    #[serde(default)]
    pub session_source: SessionSource,
    #[serde(default)]
    pub divergence_pending: bool,
    #[serde(default)]
    pub argv: Option<Vec<String>>,
    #[serde(default)]
    pub is_agent: bool,
    #[serde(default)]
    pub updated_at: u64,
}

/// A field-merging upsert: each `Some` field overwrites, each `None` is left
/// untouched, so the session writers (`session_id`/`session_cwd`) and the poll
/// writer (`argv`/`is_agent`) never clobber each other. `session_source` is the
/// call-site-assigned trust rank of the id (KTD2) — `None` degrades to `Poll`,
/// never elevated — and `session_cwd` rides the id's trust gate: a partial
/// carrying a cwd without an id writes neither.
#[derive(Debug, Clone, Default)]
pub struct ResumePartial {
    pub session_id: Option<String>,
    pub session_cwd: Option<String>,
    pub session_source: Option<SessionSource>,
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
/// `write_session`, but with a per-write unique temp name (KTD9): concurrent
/// flushes sharing one fixed temp path can truncate each other mid-write and
/// rename a torn file into the corruption (`.bad.bak`) path.
pub fn write_records(path: &Path, records: &ResumeRecords) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    let tmp = path.with_extension(format!(
        "json.tmp.{}.{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&tmp, serde_json::to_vec_pretty(records)?)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Acquire the store lock, recovering from poisoning (see [`STORE_LOCK`]).
fn store_guard() -> std::sync::MutexGuard<'static, ()> {
    STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Field-merge `partial` into the record for `leaf_key` and flush write-through,
/// returning the **effective** stored record so a caller can see whether its
/// write took (the pick flow reads back `session_source`/`divergence_pending`).
///
/// The session fields go through the trust gate (KTD2, R5/R10/R15):
/// - an implausible `session_id` (or one arriving with no rank when the store
///   holds a higher-ranked id) writes nothing; an implausible `session_cwd` is
///   dropped while a plausible id still lands;
/// - an incoming rank ≥ the stored rank overwrites id + cwd + source and clears
///   `divergence_pending` (same-source rotation: poll→poll, hook→hook on
///   `/clear`; a fresh `Pick` supersedes cleanly);
/// - a `Hook` id against a stored `Pick` never clears or rebinds the pick — it
///   sets `divergence_pending` when the ids differ (the re-pick prompt's
///   signal) and clears it when they match (the live session corroborates the
///   pick; a forger gains nothing by toggling a flag that only ever prompts).
///
/// The whole read-compare-write holds the store lock (KTD9, R16).
pub fn upsert_at(
    path: &Path,
    leaf_key: &str,
    partial: ResumePartial,
) -> std::io::Result<ResumeRecord> {
    let _guard = store_guard();
    let mut records = read_records(path);
    let rec = records.entry(leaf_key.to_string()).or_default();
    if let Some(id) = partial.session_id {
        if is_plausible_session_id(&id) {
            let incoming = partial.session_source.unwrap_or_default();
            let cwd = partial
                .session_cwd
                .filter(|c| is_plausible_session_cwd(c));
            if rec.session_id.is_none() || incoming >= rec.session_source {
                rec.session_id = Some(id);
                if cwd.is_some() {
                    rec.session_cwd = cwd;
                }
                rec.session_source = incoming;
                rec.divergence_pending = false;
            } else if incoming == SessionSource::Hook
                && rec.session_source == SessionSource::Pick
            {
                rec.divergence_pending = rec.session_id.as_deref() != Some(id.as_str());
            }
        }
    }
    if let Some(v) = partial.argv {
        rec.argv = Some(v);
    }
    if let Some(v) = partial.is_agent {
        rec.is_agent = v;
    }
    rec.updated_at = crate::notify::now_unix_ms();
    let effective = rec.clone();
    write_records(path, &records)?;
    Ok(effective)
}

/// Clear one leaf's session attribution — id, source, and divergence flag —
/// leaving the poll's `argv`/`is_agent` intact (fix-session-pane-attribution
/// U8, KTD7/R14). The user-initiated escape valve for a stranded, stale, or
/// diverged precise id that no automatic writer may correct: resolution then
/// returns empty and the next launch re-captures via the pick-list. A no-op
/// (no write) when the leaf has no record or no session id.
pub fn reset_attribution_at(path: &Path, leaf_key: &str) -> std::io::Result<()> {
    let _guard = store_guard();
    let mut records = read_records(path);
    let Some(rec) = records.get_mut(leaf_key) else {
        return Ok(());
    };
    if rec.session_id.is_none() && !rec.divergence_pending {
        return Ok(());
    }
    rec.session_id = None;
    rec.session_source = SessionSource::default();
    rec.divergence_pending = false;
    rec.updated_at = crate::notify::now_unix_ms();
    write_records(path, &records)
}

/// Remove one leaf's record (used to prune an orphan whose layout leaf is gone),
/// leaving the others. A no-op write is skipped when nothing was removed.
pub fn prune_at(path: &Path, leaf_key: &str) -> std::io::Result<()> {
    let _guard = store_guard();
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
    let _guard = store_guard();
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
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// Command: the always-on poll captures an agent leaf's active session id
/// write-through (fix-resume-session-selection U2). The id comes from Claude's
/// transcript store (KTD-A), so capture is independent of the installed `fly`
/// binary's version — the skew that silently disabled the hook path — and fires
/// before the first `Notification`/`Stop`. Stamped `Poll`, the lowest trust
/// rank (fix-session-pane-attribution KTD2): the poll's cwd-level guess never
/// overwrites a hook-precise or human-picked id, and this command — reachable
/// only from the frontend, never the socket — is the sole `Poll` writer.
/// Field-merging, so it never clobbers the poll's `argv`/`is_agent`. Returns
/// the effective stored record so the caller sees whether the write took.
/// Writes through `resume_path()` under the `FLY_APP_NAME` root, so
/// `fly`/`fly-dev` stay isolated (R7).
#[tauri::command]
pub fn save_resume_session(
    leaf_key: String,
    session_id: String,
    session_cwd: Option<String>,
) -> Result<ResumeRecord, String> {
    upsert_at(
        &resume_path(),
        &leaf_key,
        ResumePartial {
            session_id: Some(session_id),
            session_cwd,
            session_source: Some(SessionSource::Poll),
            ..Default::default()
        },
    )
    .map_err(|e| e.to_string())
}

/// Command: bind a user-picked session to a leaf (fix-session-pane-attribution
/// U6, R10). Stamped `Pick`, the highest trust rank (KTD2): an explicit human
/// decision supersedes anything stored and is itself superseded only by another
/// explicit action (a later pick, or the U8 reset). Frontend-only like
/// `save_resume_session` — no socket path can reach this rank. Returns the
/// effective stored record (`session_source`/`divergence_pending`) so the pick
/// flow can confirm the bind and drive the re-pick prompt.
#[tauri::command]
pub fn save_session_pick(
    leaf_key: String,
    session_id: String,
    session_cwd: Option<String>,
) -> Result<ResumeRecord, String> {
    upsert_at(
        &resume_path(),
        &leaf_key,
        ResumePartial {
            session_id: Some(session_id),
            session_cwd,
            session_source: Some(SessionSource::Pick),
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

/// Command: the user-initiated attribution reset (fix-session-pane-attribution
/// U8, KTD7/R14) — see [`reset_attribution_at`]. Frontend-only, like the other
/// explicit-action writers: no socket path can clear an id.
#[tauri::command]
pub fn reset_pane_attribution(leaf_key: String) -> Result<(), String> {
    reset_attribution_at(&resume_path(), &leaf_key).map_err(|e| e.to_string())
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

    // ---- trust-ranked session writes (fix-session-pane-attribution U3) -----

    /// A session-id partial stamped with an explicit source (KTD2).
    fn ranked(session_id: &str, source: SessionSource) -> ResumePartial {
        ResumePartial {
            session_id: Some(session_id.to_string()),
            session_source: Some(source),
            ..Default::default()
        }
    }

    fn stored(path: &Path, leaf: &str) -> ResumeRecord {
        read_records(path)[leaf].clone()
    }

    #[test]
    fn poll_over_poll_rotation_still_succeeds() {
        // Same-source rotation (KTD2): the poll may replace its own guess —
        // `/clear` rotates the active session and the poll tracks it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resume.json");
        upsert_at(&path, "leaf", ranked("sess-A", SessionSource::Poll)).unwrap();
        upsert_at(&path, "leaf", ranked("sess-B", SessionSource::Poll)).unwrap();
        let rec = stored(&path, "leaf");
        assert_eq!(rec.session_id.as_deref(), Some("sess-B"));
        assert_eq!(rec.session_source, SessionSource::Poll);
    }

    #[test]
    fn higher_rank_overwrites_lower() {
        // Hook > Poll, Pick > Poll, Pick > Hook — each precise capture
        // supersedes a less-trusted one (R3/R5).
        for (lower, higher) in [
            (SessionSource::Poll, SessionSource::Hook),
            (SessionSource::Poll, SessionSource::Pick),
            (SessionSource::Hook, SessionSource::Pick),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("resume.json");
            upsert_at(&path, "leaf", ranked("sess-low", lower)).unwrap();
            upsert_at(&path, "leaf", ranked("sess-high", higher)).unwrap();
            let rec = stored(&path, "leaf");
            assert_eq!(rec.session_id.as_deref(), Some("sess-high"), "{higher:?} over {lower:?}");
            assert_eq!(rec.session_source, higher);
        }
    }

    #[test]
    fn lower_rank_never_overwrites_higher() {
        // The poll's cwd-level guess must not clobber a precise id (R5), and a
        // forgeable hook id must not clobber a human pick (R10).
        for (higher, lower) in [
            (SessionSource::Hook, SessionSource::Poll),
            (SessionSource::Pick, SessionSource::Poll),
            (SessionSource::Pick, SessionSource::Hook),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("resume.json");
            upsert_at(&path, "leaf", ranked("sess-precise", higher)).unwrap();
            let effective =
                upsert_at(&path, "leaf", ranked("sess-guess", lower)).unwrap();
            assert_eq!(
                effective.session_id.as_deref(),
                Some("sess-precise"),
                "{lower:?} must not overwrite {higher:?}"
            );
            assert_eq!(effective.session_source, higher);
        }
    }

    #[test]
    fn unranked_write_defaults_to_poll_rank() {
        // A partial with no source (an older call site) is a Poll-rank write:
        // it lands on an empty record but never over a precise id.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resume.json");
        upsert_at(&path, "leaf", record(Some("sess-first"))).unwrap();
        let rec = stored(&path, "leaf");
        assert_eq!(rec.session_id.as_deref(), Some("sess-first"));
        assert_eq!(rec.session_source, SessionSource::Poll);

        upsert_at(&path, "leaf", ranked("sess-hook", SessionSource::Hook)).unwrap();
        upsert_at(&path, "leaf", record(Some("sess-unranked"))).unwrap();
        assert_eq!(
            stored(&path, "leaf").session_id.as_deref(),
            Some("sess-hook"),
            "an unranked write cannot displace a hook capture"
        );
    }

    #[test]
    fn divergent_hook_flags_but_keeps_the_pick() {
        // KTD2/R10/R14 (AE6): a hook reporting a different live id than a
        // stored pick sets divergence_pending and changes nothing else — a
        // forged capture can neither rebind nor clear a human decision.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resume.json");
        upsert_at(&path, "leaf", ranked("sess-picked", SessionSource::Pick)).unwrap();
        let effective =
            upsert_at(&path, "leaf", ranked("sess-live", SessionSource::Hook)).unwrap();
        assert_eq!(effective.session_id.as_deref(), Some("sess-picked"));
        assert_eq!(effective.session_source, SessionSource::Pick);
        assert!(effective.divergence_pending, "divergence flagged for re-pick");
    }

    #[test]
    fn matching_hook_corroborates_the_pick() {
        // A hook reporting the SAME id as the pick clears a pending divergence:
        // the live session corroborates the human decision. (Toggling the flag
        // is harmless to forge — it only ever drives a re-pick prompt, never a
        // rebind.)
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resume.json");
        upsert_at(&path, "leaf", ranked("sess-picked", SessionSource::Pick)).unwrap();
        upsert_at(&path, "leaf", ranked("sess-other", SessionSource::Hook)).unwrap();
        assert!(stored(&path, "leaf").divergence_pending);
        let effective =
            upsert_at(&path, "leaf", ranked("sess-picked", SessionSource::Hook)).unwrap();
        assert!(!effective.divergence_pending, "corroboration clears the flag");
        assert_eq!(effective.session_source, SessionSource::Pick);
    }

    #[test]
    fn a_new_pick_supersedes_and_clears_divergence() {
        // R10: a pick is superseded only by an explicit user action — another
        // pick — which rebinds cleanly and drops the stale divergence flag.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resume.json");
        upsert_at(&path, "leaf", ranked("sess-old-pick", SessionSource::Pick)).unwrap();
        upsert_at(&path, "leaf", ranked("sess-live", SessionSource::Hook)).unwrap();
        let effective =
            upsert_at(&path, "leaf", ranked("sess-new-pick", SessionSource::Pick)).unwrap();
        assert_eq!(effective.session_id.as_deref(), Some("sess-new-pick"));
        assert_eq!(effective.session_source, SessionSource::Pick);
        assert!(!effective.divergence_pending);
    }

    #[test]
    fn implausible_session_id_is_not_stored() {
        // R15/KTD8: separator-bearing, traversal, empty, and control-char ids
        // are rejected at ingestion — the stored value is untouched.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resume.json");
        upsert_at(&path, "leaf", ranked("sess-good", SessionSource::Pick)).unwrap();
        for bad in ["a/b", "../../etc/passwd", "", "..sess", "sess\u{7}id", "sess id"] {
            let effective =
                upsert_at(&path, "leaf", ranked(bad, SessionSource::Pick)).unwrap();
            assert_eq!(
                effective.session_id.as_deref(),
                Some("sess-good"),
                "rejected {bad:?}"
            );
        }
    }

    #[test]
    fn implausible_session_cwd_is_dropped_while_the_id_lands() {
        // R15: a relative or control-char cwd steers the spawn dir, so it is
        // dropped at write time; the (plausible) id still captures.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resume.json");
        for bad_cwd in ["relative/path", "/proj\napp", "/proj\u{1b}[2Japp", ""] {
            let effective = upsert_at(
                &path,
                "leaf",
                ResumePartial {
                    session_id: Some("sess-1".into()),
                    session_cwd: Some(bad_cwd.to_string()),
                    session_source: Some(SessionSource::Hook),
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(effective.session_id.as_deref(), Some("sess-1"));
            assert_eq!(effective.session_cwd, None, "rejected cwd {bad_cwd:?}");
        }
        // A plausible absolute cwd (non-ASCII included) is stored.
        let effective = upsert_at(
            &path,
            "leaf",
            ResumePartial {
                session_id: Some("sess-1".into()),
                session_cwd: Some("/proj/café".into()),
                session_source: Some(SessionSource::Hook),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(effective.session_cwd.as_deref(), Some("/proj/café"));
    }

    #[test]
    fn interleaved_hook_and_poll_writes_settle_on_the_hook_id() {
        // KTD9/R16: concurrent writers — the hook's per-birth capture racing
        // the poll — must settle deterministically on the higher rank with the
        // store intact (no torn temp files, no `.bad.bak`).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resume.json");
        upsert_at(&path, "leaf", ranked("sess-hook", SessionSource::Hook)).unwrap();
        let mut handles = Vec::new();
        for i in 0..8 {
            let path = path.clone();
            handles.push(std::thread::spawn(move || {
                for j in 0..25 {
                    let source = if i % 2 == 0 {
                        SessionSource::Poll
                    } else {
                        SessionSource::Hook
                    };
                    let id = if source == SessionSource::Hook {
                        "sess-hook".to_string()
                    } else {
                        format!("sess-poll-{i}-{j}")
                    };
                    upsert_at(&path, "leaf", ranked(&id, source)).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let rec = stored(&path, "leaf");
        assert_eq!(rec.session_id.as_deref(), Some("sess-hook"));
        assert_eq!(rec.session_source, SessionSource::Hook);
        assert!(
            !PathBuf::from(format!("{}.bad.bak", path.display())).exists(),
            "no corruption backup — the store never tore"
        );
    }

    #[test]
    fn a_store_without_source_fields_loads_as_poll() {
        // Back-compat: a store written by an older binary carries no
        // sessionSource/divergencePending — it loads at the lowest trust, so a
        // later precise capture supersedes it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resume.json");
        std::fs::write(
            &path,
            r#"{"leaf-1":{"sessionId":"sess-old","sessionCwd":"/proj","argv":["claude"],"isAgent":true,"updatedAt":7}}"#,
        )
        .unwrap();
        let rec = stored(&path, "leaf-1");
        assert_eq!(rec.session_source, SessionSource::Poll);
        assert!(!rec.divergence_pending);
        assert_eq!(rec.session_id.as_deref(), Some("sess-old"));
    }

    #[test]
    fn reset_clears_attribution_keeping_the_poll_fields() {
        // U8/KTD7: reset is the user escape valve — it clears id + source +
        // divergence so resolution returns empty (→ the pick-list), while the
        // poll's argv/is_agent survive for the resume-command builder.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resume.json");
        upsert_at(
            &path,
            "leaf",
            ResumePartial {
                argv: Some(vec!["claude".into()]),
                is_agent: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        upsert_at(&path, "leaf", ranked("sess-stale", SessionSource::Pick)).unwrap();
        upsert_at(&path, "leaf", ranked("sess-live", SessionSource::Hook)).unwrap();
        assert!(stored(&path, "leaf").divergence_pending);

        reset_attribution_at(&path, "leaf").unwrap();
        let rec = stored(&path, "leaf");
        assert_eq!(rec.session_id, None);
        assert_eq!(rec.session_source, SessionSource::Poll);
        assert!(!rec.divergence_pending);
        assert_eq!(rec.argv.as_deref(), Some(["claude".to_string()].as_slice()));
        assert!(rec.is_agent);

        // Reset on an unset leaf (or an absent record) is a no-op, not an error.
        reset_attribution_at(&path, "leaf").unwrap();
        reset_attribution_at(&path, "no-such-leaf").unwrap();
    }

    #[test]
    fn session_source_serializes_snake_case_under_camel_case_keys() {
        // The wire contract (serde camelCase struct keys, snake_case enum
        // values) crosses the store file and the frontend mirror in ipc.ts.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resume.json");
        upsert_at(&path, "leaf", ranked("sess-1", SessionSource::Hook)).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains(r#""sessionSource": "hook""#), "body: {body}");
        assert!(body.contains(r#""divergencePending": false"#), "body: {body}");
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
