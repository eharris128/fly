//! Write-through automation store (U3 of
//! `docs/plans/2026-07-01-002-feat-automations-plan.md`; R6, R8, KTD-B).
//!
//! **The in-memory map is the authority; the file is a write-through mirror.**
//! Writers span the sweep thread, per-connection socket threads, the hook
//! dispatch path, and Tauri commands (KTD-B), so a `session/resume.rs`-style
//! file read-modify-write would lose updates — a claim flush racing a pause
//! drops the pause. Instead [`Store`] holds a `Mutex<BTreeMap>` and every
//! mutation runs under the lock and flushes the **full document** atomically
//! (temp + rename, `0600` set on the temp file *before* the rename, dir
//! `0700` — mirroring `session::write_session` / `session::resume::
//! write_records`) before returning. The flush rewrites the whole file per
//! mutation; the R8 caps (20 rows × 8 KiB tails) bound it, and the plan
//! accepts this as the desktop-scale ceiling.
//!
//! **Flush failure does not poison the in-memory state.** The closure's
//! mutation is kept (the map stays authoritative — rolling it back could
//! discard a claim that already dispatched), the `io::Error` is returned to
//! the caller, and [`StoreHealth::flush_error`] records it; the next
//! successful flush writes the full document, so one success makes the disk
//! consistent again and clears the flag.
//!
//! **Corrupt store file at load (R6):** renamed aside to `<file>.bad.bak`
//! (the corrupt bytes are preserved, never overwritten), the store starts
//! empty, [`StoreHealth::corrupt_bak`] records where the bytes went (sticky
//! for the app session — later successful flushes must not hide the
//! dashboard's warning row, U10/R25), and the degradation is logged to
//! stderr (the `[fly-webview]`-style bracket-prefix convention in `lib.rs`).
//!
//! **Script content lives on disk, not in the JSON** (KTD-B, bb parity):
//! `automation-scripts/<id>/script` under the same app-data root, written on
//! create ([`Store::put_script`]) and removed — file *and* its `<id>/` dir —
//! on delete ([`Store::delete`]). Script files are `0600` in `0700` dirs.
//!
//! This unit is dumb persistence: it mints no ids and validates no cron —
//! that is the manager's job (U4). Ids are only checked for filesystem
//! safety (a single path segment) before being joined into script paths.

use std::collections::BTreeMap;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;

use super::model::Automation;

/// Store name under the app data dir (`$XDG_DATA_HOME/<app>/`, honoring
/// `FLY_APP_NAME` via `crate::app_dir_name()` — the same root
/// `session::data_dir` derives).
const STORE_FILE: &str = "automations.json";
/// Script-content root under the same app data dir (KTD-B).
const SCRIPTS_DIR: &str = "automation-scripts";

/// Queryable store health (R6). Two independent degradation axes — a corrupt
/// file found at load and a failing flush — can hold simultaneously, so this
/// is a struct of optionals rather than the plan sketch's `Ok/Degraded` enum;
/// healthy = both `None`.
///
/// Serializes camelCase so the dashboard (U10) can render the warning row
/// naming the `.bad.bak` path directly.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreHealth {
    /// Where the unparseable bytes of a corrupt store file were preserved at
    /// load (R6). Normally `<file>.bad.bak`; if even the rename-aside failed
    /// (unwritable data dir — where every flush will fail too), this is the
    /// original store path, since the bytes are still there. Sticky for the
    /// app session so the U10 warning survives later successful flushes.
    pub corrupt_bak: Option<PathBuf>,
    /// The most recent flush failure, cleared by the next successful flush
    /// (a flush writes the full document, so one success resynchronizes the
    /// disk).
    pub flush_error: Option<String>,
}

impl StoreHealth {
    pub fn is_ok(&self) -> bool {
        self.corrupt_bak.is_none() && self.flush_error.is_none()
    }
}

/// Map + health under one mutex: health transitions (flush failure/recovery)
/// happen atomically with the mutation that caused them, and there is no
/// second lock to order against (KTD-B lock discipline).
struct Inner {
    map: BTreeMap<String, Automation>,
    health: StoreHealth,
}

/// The mutex authority (KTD-B): all reads and mutations go through here; the
/// store file on disk is only ever a flush *of* this map, never a source
/// merged back in after load.
pub struct Store {
    inner: Mutex<Inner>,
    /// The store file (`automations.json`).
    path: PathBuf,
    /// Root of per-automation script dirs (`automation-scripts/`).
    scripts_dir: PathBuf,
}

// ---- default paths (session::data_dir convention) ---------------------------

/// The store file under the `FLY_APP_NAME` data root.
pub fn store_path() -> PathBuf {
    crate::session::data_dir().join(STORE_FILE)
}

/// The script-content root under the same data root.
pub fn scripts_dir() -> PathBuf {
    crate::session::data_dir().join(SCRIPTS_DIR)
}

impl Store {
    /// Load the store from explicit paths (the path-taking pure core; tests
    /// point this at a tempdir). Missing file → empty store, healthy. A file
    /// that exists but does not parse → renamed aside to `<file>.bad.bak`
    /// (never overwritten, R6), empty store, degraded health, stderr log.
    ///
    /// Never fails: persistence degradation must not stop the app (the
    /// attention pipeline and PTYs do not depend on this store).
    pub fn load_at(path: PathBuf, scripts_dir: PathBuf) -> Store {
        let (map, health) = match std::fs::read(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Fresh install: no store file yet.
                (BTreeMap::new(), StoreHealth::default())
            }
            Err(e) => {
                // The file exists but is unreadable (EACCES / EIO / EISDIR):
                // do NOT treat it as a fresh install and silently start empty
                // — surface the degradation so the dashboard warns and the
                // user knows their automations are present-but-unread rather
                // than gone. (A read-blocked 0600 file owned by this UID is a
                // narrow case; the next successful flush would still overwrite
                // it, but the corrupt_bak flag makes that visible first, R6.)
                eprintln!(
                    "[fly-automations] store file {} exists but could not be read ({e}); \
                     starting empty and degraded (R6)",
                    path.display()
                );
                (
                    BTreeMap::new(),
                    StoreHealth {
                        corrupt_bak: Some(path.clone()),
                        flush_error: None,
                    },
                )
            }
            Ok(bytes) => match serde_json::from_slice::<BTreeMap<String, Automation>>(&bytes) {
                Ok(map) => (map, StoreHealth::default()),
                Err(e) => {
                    let bak = unique_bak_path(&path);
                    let preserved_at = match std::fs::rename(&path, &bak) {
                        Ok(()) => {
                            eprintln!(
                                "[fly-automations] store file {} is corrupt ({e}); \
                                 preserved at {} and starting empty (R6)",
                                path.display(),
                                bak.display()
                            );
                            bak
                        }
                        Err(rename_err) => {
                            // Rename-aside needs only dir write permission —
                            // the same permission every flush needs — so a
                            // failure here means flushes will fail too and
                            // the corrupt bytes stay protected in place.
                            eprintln!(
                                "[fly-automations] store file {} is corrupt ({e}) and could \
                                 not be renamed aside ({rename_err}); starting empty (R6)",
                                path.display()
                            );
                            path.clone()
                        }
                    };
                    (
                        BTreeMap::new(),
                        StoreHealth {
                            corrupt_bak: Some(preserved_at),
                            flush_error: None,
                        },
                    )
                }
            },
        };
        Store {
            inner: Mutex::new(Inner { map, health }),
            path,
            scripts_dir,
        }
    }

    /// Load from the default `$XDG_DATA_HOME/<app>/` paths.
    pub fn load_default() -> Store {
        Store::load_at(store_path(), scripts_dir())
    }

    /// Lock the inner state, **recovering from poison** (audit-remediation
    /// U4/KTD4, the `session/resume.rs` precedent): a mutation closure that
    /// panicked poisons the mutex, and a plain `unwrap` would then panic every
    /// subsequent sweep tick — killing the sweep thread for the rest of the
    /// session. Recovery is safe for the same reason `resume.rs` documents:
    /// the on-disk file is only ever a complete renamed snapshot (never a
    /// partial write), so the worst a mid-mutation panic leaves behind is a
    /// half-applied *in-memory* edit of one automation — which the next
    /// mutation continues from, exactly as if the closure had returned early.
    fn lock_recovered(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    // ---- mutation (KTD-B: lock → mutate → flush → return) ------------------

    /// Run `f` against the map and flush the full document atomically before
    /// returning, all under one lock hold — the KTD-B no-lost-updates
    /// contract. Always flushes (write-through; the store cannot know whether
    /// `f` changed anything, and a spurious full-document write is harmless
    /// at desktop scale).
    ///
    /// On flush failure the in-memory mutation is **kept** (the map stays
    /// authoritative; see the module doc), `flush_error` is recorded, and the
    /// `io::Error` is returned so the caller can surface it.
    ///
    /// Per KTD-B lock discipline, `f` must be pure map manipulation — never
    /// dispatch, emit events, or block on I/O inside it.
    pub fn mutate<R>(&self, f: impl FnOnce(&mut BTreeMap<String, Automation>) -> R) -> io::Result<R> {
        let mut inner = self.lock_recovered();
        let result = f(&mut inner.map);
        match write_map(&self.path, &inner.map) {
            Ok(()) => {
                inner.health.flush_error = None;
                Ok(result)
            }
            Err(e) => {
                eprintln!(
                    "[fly-automations] flush to {} failed ({e}); \
                     in-memory state remains authoritative (KTD-B)",
                    self.path.display()
                );
                inner.health.flush_error = Some(e.to_string());
                Err(e)
            }
        }
    }

    /// Remove an automation: drop its map entry (flushed under the lock),
    /// then remove its script file and `<id>/` dir (R23 teardown half owned
    /// by the store). Returns the removed record, or `None` if the id was
    /// unknown.
    ///
    /// **Flush-tolerant** like every other mutation, but a removal cannot use
    /// the `mutate` + `flush_tolerant` refetch dance (the entry is gone, so a
    /// refetch yields `None`). So the removal + flush run inline here and the
    /// removed record is returned **even when the flush fails** (KTD-B: the
    /// in-memory removal stays authoritative; `flush_error` records the
    /// degradation for the dashboard). Never resurrecting the entry on a flush
    /// failure is load-bearing: the caller's R23 teardown (kill the in-flight
    /// script group, close open rows) runs off this return value, so dropping
    /// it would orphan a live process group and misreport the delete.
    ///
    /// Script-dir removal is best-effort cleanup attempted regardless of the
    /// flush outcome (it also clears a half-created orphan — script written,
    /// entry never flushed); its failure is logged, not surfaced.
    pub fn delete(&self, id: &str) -> Option<Automation> {
        let removed = {
            let mut inner = self.lock_recovered();
            let removed = inner.map.remove(id);
            match write_map(&self.path, &inner.map) {
                Ok(()) => inner.health.flush_error = None,
                Err(e) => {
                    eprintln!(
                        "[fly-automations] flush after delete of {id:?} to {} failed ({e}); \
                         in-memory removal remains authoritative (KTD-B)",
                        self.path.display()
                    );
                    inner.health.flush_error = Some(e.to_string());
                }
            }
            removed
        };
        // Lock released. Best-effort script cleanup regardless of flush outcome.
        if let Err(e) = self.remove_script(id) {
            eprintln!(
                "[fly-automations] could not remove script dir for {id:?} ({e}); \
                 automation entry already deleted"
            );
        }
        removed
    }

    // ---- reads --------------------------------------------------------------

    /// Clone of the full map (dashboard/list reads, U10).
    pub fn snapshot(&self) -> BTreeMap<String, Automation> {
        self.lock_recovered().map.clone()
    }

    /// Clone of one automation.
    pub fn get(&self, id: &str) -> Option<Automation> {
        self.lock_recovered().map.get(id).cloned()
    }

    /// Current health (R6) — see [`StoreHealth`].
    pub fn health(&self) -> StoreHealth {
        self.lock_recovered().health.clone()
    }

    // ---- script content files (KTD-B: on disk, not in the JSON) -------------

    /// Write an automation's script content to
    /// `automation-scripts/<id>/script` (`0600` file in `0700` dirs, written
    /// atomically like the store file — script bodies can carry secrets).
    /// Returns the script path for the manager to stamp into
    /// `Mode::Script.script_file`.
    ///
    /// Create order (U4): `put_script` first, then `mutate(insert)` — a crash
    /// between them leaves at worst an orphan script dir, never a store entry
    /// pointing at a missing script.
    pub fn put_script(&self, id: &str, content: &str) -> io::Result<PathBuf> {
        let dir = self.script_dir_for(id)?;
        create_private_dir(&self.scripts_dir)?;
        create_private_dir(&dir)?;
        let path = dir.join("script");
        write_atomic_owner_only(&path, content.as_bytes())?;
        Ok(path)
    }

    /// The script path for an id (no I/O) — where [`Store::put_script`] wrote
    /// or will write.
    pub fn script_path(&self, id: &str) -> io::Result<PathBuf> {
        Ok(self.script_dir_for(id)?.join("script"))
    }

    /// Remove an automation's script file and its `<id>/` dir. Missing is
    /// fine (agent-mode automations never had one).
    fn remove_script(&self, id: &str) -> io::Result<()> {
        let dir = self.script_dir_for(id)?;
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Join an id into the scripts root, rejecting anything that is not a
    /// single safe path segment. Ids are minted by the manager (U4) as short
    /// alphanumeric strings, so this never fires in practice — it is a
    /// path-traversal guard at the filesystem boundary (the
    /// `session::safe_key` convention, but rejecting instead of filtering so
    /// two distinct ids can never collide onto one dir).
    fn script_dir_for(&self, id: &str) -> io::Result<PathBuf> {
        let safe = !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !safe {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unsafe automation id {id:?}"),
            ));
        }
        Ok(self.scripts_dir.join(id))
    }
}

// ---- atomic write helpers (mirror session::write_session, R6) ---------------

/// A non-clobbering `.bad.bak` path for a corrupt store file (R6: prior
/// forensic bytes are never overwritten). Prefers `<file>.bad.bak`, then
/// `.bad.bak.2`, `.3`, … if earlier corruption already preserved copies.
/// Bounded (release builds have overflow checks off); after 999 preserved
/// corruptions it falls back to the base name (astronomically unreachable).
fn unique_bak_path(path: &Path) -> PathBuf {
    let base = format!("{}.bad.bak", path.display());
    let first = PathBuf::from(&base);
    if !first.exists() {
        return first;
    }
    for n in 2..1000u32 {
        let cand = PathBuf::from(format!("{base}.{n}"));
        if !cand.exists() {
            return cand;
        }
    }
    first
}

/// Serialize and atomically replace the store file. Mirrors
/// `session::write_session` / `session::resume::write_records` (the repo's
/// established shape for this, kept local rather than extracted so
/// `session/mod.rs` stays untouched): parent dir created `0700`; bytes land
/// in a same-dir temp file; mode `0600` is set on the temp file **before**
/// the rename, so there is never a world-readable window (prompts and
/// captured script output are sensitive at rest).
fn write_map(path: &Path, map: &BTreeMap<String, Automation>) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    write_atomic_owner_only(path, &serde_json::to_vec_pretty(map)?)
}

/// Temp + chmod-0600 + rename in `path`'s own dir (same filesystem, so the
/// rename is atomic). `pub(crate)`: the monitor bundle write (monitor-handoff
/// U3, `mod.rs::write_bundle_file`) shares these two primitives rather than
/// keeping a third copy of the dance (alerts.rs keeps its own documented
/// local pair; this was the tie-breaker toward sharing).
pub(crate) fn write_atomic_owner_only(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    // fsync-before-rename (audit-remediation U5/KTD5): without it, a power
    // loss after the rename can leave the *new* name pointing at unwritten
    // data on some filesystems — atomicity against crashes of this process
    // was already covered by the rename, durability against power loss was
    // not. The files are small and writes per-mutation, so the cost is noise.
    std::fs::File::open(&tmp)?.sync_all()?;
    std::fs::rename(&tmp, path)?;
    sync_parent_dir(path);
    Ok(())
}

/// Best-effort fsync of `path`'s parent directory after a rename, so the
/// directory entry itself is durable (U5/KTD5). Failure is ignored: some
/// filesystems refuse directory fsync, and the data-file sync above already
/// covers the common cases.
pub(crate) fn sync_parent_dir(path: &Path) {
    if let Some(parent) = path.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
}

/// `create_dir_all` + explicit `0700` (never left to umask).
pub(crate) fn create_private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::model::{Mode, Origin, RunOutcome, Trigger};
    use super::*;

    /// A store rooted in its own tempdir subpaths (dir kept alive by the
    /// returned guard).
    fn store_in(dir: &tempfile::TempDir) -> Store {
        Store::load_at(
            dir.path().join("data").join("automations.json"),
            dir.path().join("data").join("automation-scripts"),
        )
    }

    fn automation(id: &str) -> Automation {
        Automation {
            id: id.into(),
            name: format!("watch {id}"),
            cron: "*/5 * * * *".into(),
            timezone: "America/New_York".into(),
            enabled: true,
            retry_on_interrupt: false,
            monitor: false,
            not_before_ms: None,
            retired_at: None,
            pickup_pointers: None,
            after: None,
            cwd: "/tmp".into(),
            mode: Mode::Script {
                script_file: "script".into(),
                interpreter: "bash".into(),
                timeout_ms: 120_000,
            },
            origin: Origin {
                pane_id: 7,
                workspace_id: "ws-1".into(),
                label: "cli".into(),
            },
            created_at: 1_000,
            updated_at: 1_000,
            next_run_at: Some(60_000),
            runs: Vec::new(),
        }
    }

    // R6/R8: a mutation flushed by the write-through store reloads
    // identically — including run history rows, the R8 state the store must
    // not lose across a restart.
    #[test]
    fn mutation_persists_and_reloads_identically_through_a_real_tempdir() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);

        store
            .mutate(|map| {
                let mut a = automation("a1");
                a.claim(Some(360_000), 61_000, Trigger::Schedule, "r1", false).unwrap();
                a.close(
                    "r1",
                    RunOutcome::Succeeded {
                        output: Some("ok".into()),
                    },
                    70_000,
                );
                map.insert(a.id.clone(), a);
            })
            .unwrap();
        let before = store.snapshot();

        // A fresh Store over the same paths sees exactly what was flushed.
        let reloaded = store_in(&dir);
        assert_eq!(reloaded.snapshot(), before);
        assert_eq!(reloaded.get("a1").unwrap().runs.len(), 1);
        assert!(reloaded.health().is_ok());
    }

    // KTD-B — THE regression test this unit exists for: concurrent writers
    // through one Store lose neither update. A file read-modify-write shape
    // would drop interleaved inserts; the mutex authority must not.
    #[test]
    fn concurrent_mutations_from_two_threads_lose_neither_update() {
        const PER_THREAD: usize = 25;
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(store_in(&dir));

        let threads: Vec<_> = (0..2)
            .map(|t| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    for i in 0..PER_THREAD {
                        let id = format!("t{t}-a{i}");
                        store
                            .mutate(|map| {
                                map.insert(id.clone(), automation(&id));
                            })
                            .unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }

        assert_eq!(store.snapshot().len(), 2 * PER_THREAD, "in-memory kept all");
        // And the write-through file agrees — the last flush under the lock
        // serialized the full, merged document.
        let reloaded = store_in(&dir);
        assert_eq!(reloaded.snapshot().len(), 2 * PER_THREAD, "disk kept all");
    }

    // R6: a corrupt store file is renamed aside byte-identically (never
    // overwritten, never deleted), the store starts empty, and health is
    // degraded naming the backup path.
    #[test]
    fn corrupt_json_moves_aside_to_bad_bak_starts_empty_and_degrades_health() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data").join("automations.json");
        let scripts = dir.path().join("data").join("automation-scripts");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let corrupt = b"{ definitely not json";
        std::fs::write(&path, corrupt).unwrap();

        let store = Store::load_at(path.clone(), scripts);

        assert!(store.snapshot().is_empty(), "starts empty");
        let bak = PathBuf::from(format!("{}.bad.bak", path.display()));
        assert_eq!(
            std::fs::read(&bak).unwrap(),
            corrupt,
            "original bytes preserved in the backup"
        );
        assert!(!path.exists(), "corrupt file renamed, not copied");
        let health = store.health();
        assert!(!health.is_ok());
        assert_eq!(health.corrupt_bak, Some(bak.clone()));

        // Sticky (R25): a later successful flush must not hide the warning.
        store
            .mutate(|map| {
                map.insert("a1".into(), automation("a1"));
            })
            .unwrap();
        assert_eq!(store.health().corrupt_bak, Some(bak));
        assert!(store.health().flush_error.is_none());
    }

    // R6: missing file is the fresh-install case — empty and healthy, no
    // backup invented.
    #[test]
    fn missing_file_loads_empty_with_healthy_status() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        assert!(store.snapshot().is_empty());
        assert!(store.health().is_ok());
        assert_eq!(store.health(), StoreHealth::default());
    }

    // KTD-B/R23: delete removes the map entry (flushed) AND the script file
    // and its <id>/ dir — stored script content never outlives its
    // automation.
    #[test]
    fn delete_removes_the_script_file_and_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);

        let script = store.put_script("a1", "#!/bin/sh\necho hi\n").unwrap();
        let id_dir = script.parent().unwrap().to_path_buf();
        assert!(script.is_file());
        store
            .mutate(|map| {
                map.insert("a1".into(), automation("a1"));
            })
            .unwrap();

        let removed = store.delete("a1");
        assert_eq!(removed.map(|a| a.id), Some("a1".to_string()));
        assert!(!script.exists(), "script file removed");
        assert!(!id_dir.exists(), "<id>/ dir removed too");
        assert!(store.get("a1").is_none());
        assert!(store_in(&dir).get("a1").is_none(), "removal flushed to disk");

        // Deleting an unknown id (or an agent automation with no script) is
        // a calm no-op, not an error.
        assert_eq!(store.delete("ghost"), None);
    }

    // A flush failure during delete must STILL return the removed record
    // (KTD-B: the in-memory removal is authoritative) — otherwise the
    // manager's R23 teardown (kill the in-flight script group, close rows)
    // never runs and an in-flight process is orphaned. Regression for the
    // "delete drops teardown on flush failure" bug (delete previously
    // `?`-propagated the flush error, discarding the removed record).
    #[test]
    fn delete_on_flush_failure_still_returns_the_removed_record_and_degrades_health() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let store = store_in(&dir);
        store
            .mutate(|map| {
                map.insert("a1".into(), automation("a1"));
            })
            .unwrap();

        // Fail the post-remove flush the same way the mutate flush-failure
        // test does: remove the data dir and make the tempdir root read-only,
        // so `create_private_dir` cannot recreate it (a plain chmod of `data`
        // would be undone by that recreate).
        std::fs::remove_dir_all(&data).unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let removed = store.delete("a1");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();

        assert_eq!(
            removed.map(|a| a.id),
            Some("a1".to_string()),
            "removed record survives the flush failure (the R23 teardown depends on it)"
        );
        assert!(store.get("a1").is_none(), "in-memory removal is authoritative");
        assert!(
            store.health().flush_error.is_some(),
            "flush failure surfaced in health for the dashboard"
        );
    }

    // KTD-B: flush failure surfaces the io error, keeps the in-memory
    // mutation (the map stays authoritative), notes it in health, and a
    // later successful flush resynchronizes the disk and clears the flag.
    #[test]
    fn flush_failure_surfaces_error_without_poisoning_in_memory_state() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let store = store_in(&dir);
        store
            .mutate(|map| {
                map.insert("a1".into(), automation("a1"));
            })
            .unwrap();

        // Failure injection: chmodding the store dir itself read-only cannot
        // work — every flush re-asserts 0700 on it (self-healing perms, the
        // write_session precedent) and the owner's chmod always succeeds. So
        // remove the store dir and make its PARENT read-only: the flush's
        // create_dir_all then fails with PermissionDenied.
        std::fs::remove_dir_all(&data).unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        if std::fs::create_dir(dir.path().join("probe")).is_ok() {
            // Running as root (or an ACL overrides the mode): the failure
            // injection cannot work — skip gracefully.
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            eprintln!("skipping flush-failure test: read-only dir is still writable");
            return;
        }

        let err = store
            .mutate(|map| {
                map.insert("a2".into(), automation("a2"));
            })
            .expect_err("flush into a read-only dir must fail");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            store.get("a2").is_some(),
            "in-memory mutation kept — the map is the authority"
        );
        assert_eq!(store.snapshot().len(), 2);
        let health = store.health();
        assert!(health.flush_error.is_some(), "health notes the failed flush");
        assert!(health.corrupt_bak.is_none());

        // Disk writable again: the next flush recreates the store dir and
        // writes the FULL document, so the previously unflushed mutation
        // reaches disk and health clears.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        store
            .mutate(|map| {
                map.insert("a3".into(), automation("a3"));
            })
            .unwrap();
        assert!(store.health().is_ok());
        let reloaded = store_in(&dir);
        assert_eq!(reloaded.snapshot().len(), 3, "a2 caught up on the next flush");
        assert!(reloaded.get("a2").is_some());
    }

    // Audit-remediation U4/KTD4: a panicking mutation closure poisons the
    // mutex; every later lock must recover (resume.rs precedent) — a plain
    // unwrap would panic the sweep thread on its next tick and kill it for
    // the session.
    #[test]
    fn a_panicking_mutation_closure_does_not_poison_later_operations() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        store
            .mutate(|map| {
                map.insert("a1".into(), automation("a1"));
            })
            .unwrap();

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = store.mutate(|map| {
                // A half-applied edit, then a panic mid-closure.
                map.insert("a2".into(), automation("a2"));
                panic!("mutation closure blew up");
            });
        }));
        assert!(panicked.is_err(), "the closure's panic propagates to its caller");

        // Every entry point recovers: reads…
        assert!(store.get("a1").is_some());
        assert!(store.health().flush_error.is_none());
        // …and the next mutation proceeds against the recovered map (the
        // half-applied a2 insert is simply present — continue-from semantics).
        store
            .mutate(|map| {
                map.insert("a3".into(), automation("a3"));
            })
            .expect("post-panic mutate succeeds");
        assert_eq!(store.snapshot().len(), 3);
        assert_eq!(store_in(&dir).snapshot().len(), 3, "flushed after recovery");
        assert_eq!(store.delete("a2").map(|a| a.id), Some("a2".to_string()));
    }

    // R6: on-disk modes are explicit, never umask — store file 0600 in a
    // 0700 dir; script file 0600 in 0700 dirs (root and <id>/).
    #[test]
    fn store_file_is_0600_and_its_dir_0700_after_flush() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        store
            .mutate(|map| {
                map.insert("a1".into(), automation("a1"));
            })
            .unwrap();
        let script = store.put_script("a1", "echo hi").unwrap();

        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let data = dir.path().join("data");
        assert_eq!(mode(&data.join("automations.json")), 0o600, "store file");
        assert_eq!(mode(&data), 0o700, "data dir");
        assert_eq!(mode(&script), 0o600, "script file");
        assert_eq!(mode(script.parent().unwrap()), 0o700, "<id>/ dir");
        assert_eq!(
            mode(&data.join("automation-scripts")),
            0o700,
            "scripts root dir"
        );
    }

    // U3 boundary guard: ids are joined into filesystem paths, so anything
    // that is not a single safe path segment is rejected (never filtered —
    // filtering could collide two distinct ids onto one dir).
    #[test]
    fn script_apis_reject_ids_that_are_not_a_safe_path_segment() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        for bad in ["../evil", "a/b", "", ".", "a b", "a\0b"] {
            let err = store.put_script(bad, "echo hi").expect_err(bad);
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{bad:?}");
            assert!(store.script_path(bad).is_err(), "{bad:?}");
        }
        // The guard leaves manager-minted ids untouched.
        assert!(store.script_path("auto_1-Ab").is_ok());
    }
}
