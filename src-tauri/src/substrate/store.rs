//! Durable leaf ⇄ session ⇄ token mapping (U2, KTD8).
//!
//! Sessions outlive fly, so the (leaf key → tmux session name, pane token)
//! binding must survive the process: on restart, fly discovers live sessions
//! (store × `has-session`, KTD4) and re-registers each stored token with the
//! new instance's `TokenRegistry` so a surviving agent's hooks — which hold
//! the token in their long-lived process env — keep authenticating.
//!
//! Same durability idiom as `session/resume.rs`: one file, write-through per
//! mutation, unique-temp + fsync + rename, `0600` in a `0700` dir. The token
//! is a secret at rest exactly like `feed.token` in the config file — same
//! trust domain (same-uid), same protections.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// One surviving-capable session binding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionRecord {
    /// Marked tmux session name (`fly-<flavor>-<slug>`, KTD4).
    pub session_name: String,
    /// The pane's hook token, exactly as injected via `-e` (KTD8).
    pub token: String,
    /// Unix ms at creation — diagnostic only, never load-bearing.
    pub created_at_ms: u64,
}

/// leaf key → binding. BTreeMap for deterministic serialization.
pub type SessionRecords = BTreeMap<String, SessionRecord>;

/// Read the store; absent or unreadable ⇒ empty (the caller re-creates
/// sessions it cannot prove — never destroys on a read failure, KTD7).
pub fn read_records(path: &Path) -> SessionRecords {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => SessionRecords::default(),
    }
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn write_records(path: &Path, records: &SessionRecords) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    let tmp: PathBuf = path.with_extension(format!(
        "json.tmp.{}.{}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        std::fs::write(&tmp, serde_json::to_vec_pretty(records)?)?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        std::fs::File::open(&tmp)?.sync_all()?;
        std::fs::rename(&tmp, path)?;
        if let Some(parent) = path.parent() {
            if let Ok(dir) = std::fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn store_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Insert or replace a leaf's binding; flushed before return.
pub fn upsert_at(
    path: &Path,
    leaf_key: &str,
    record: SessionRecord,
) -> std::io::Result<()> {
    let _g = store_guard();
    let mut records = read_records(path);
    records.insert(leaf_key.to_string(), record);
    write_records(path, &records)
}

/// Drop a leaf's binding (pane closed / session killed); flushed.
pub fn prune_at(path: &Path, leaf_key: &str) -> std::io::Result<()> {
    let _g = store_guard();
    let mut records = read_records(path);
    if records.remove(leaf_key).is_some() {
        write_records(path, &records)?;
    }
    Ok(())
}

/// Retain only the given leaves (layout reconciliation). KTD7 caveat for
/// callers: never feed this a leaf set derived from a FAILED observation —
/// "could not list sessions" must not become "prune everything".
pub fn retain_at(path: &Path, keep: &[&str]) -> std::io::Result<()> {
    let _g = store_guard();
    let mut records = read_records(path);
    let before = records.len();
    records.retain(|k, _| keep.contains(&k.as_str()));
    if records.len() != before {
        write_records(path, &records)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fly-substrate-store-test-{}-{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("substrate-sessions.json")
    }

    fn rec(name: &str) -> SessionRecord {
        SessionRecord {
            session_name: name.into(),
            token: "ab".repeat(32),
            created_at_ms: 1,
        }
    }

    #[test]
    fn upsert_read_roundtrip_and_permissions() {
        let path = tmp_store();
        upsert_at(&path, "leaf-1", rec("fly-fly-leaf-1")).unwrap();
        upsert_at(&path, "leaf-2", rec("fly-fly-leaf-2")).unwrap();
        let got = read_records(&path);
        assert_eq!(got.len(), 2);
        assert_eq!(got["leaf-1"].session_name, "fly-fly-leaf-1");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn prune_and_retain() {
        let path = tmp_store();
        upsert_at(&path, "a", rec("fly-fly-a")).unwrap();
        upsert_at(&path, "b", rec("fly-fly-b")).unwrap();
        upsert_at(&path, "c", rec("fly-fly-c")).unwrap();
        prune_at(&path, "b").unwrap();
        retain_at(&path, &["a"]).unwrap();
        let got = read_records(&path);
        assert_eq!(got.keys().collect::<Vec<_>>(), vec!["a"]);
    }

    #[test]
    fn unreadable_store_reads_empty_never_errors() {
        let path = tmp_store();
        std::fs::write(&path, b"{not json").unwrap();
        assert!(read_records(&path).is_empty());
    }
}

/// Load-or-mint the flavor's persisted substrate event token (U8/KTD8
/// continuity): a tmux server outliving fly holds the token in its
/// environment, so a NEW fly instance must present the SAME one for
/// surviving sessions' hooks to keep authenticating. 0600 beside the
/// session store; same at-rest trust class as `feed.token`.
pub fn load_or_mint_server_token(path: &Path) -> String {
    if let Ok(bytes) = std::fs::read(path) {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            if let Some(t) = v.get("eventToken").and_then(|t| t.as_str()) {
                if t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return t.to_string();
                }
            }
        }
    }
    use rand::RngCore as _;
    let mut raw = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut raw);
    let token: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    let _g = store_guard();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, serde_json::json!({ "eventToken": token }).to_string());
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    token
}

#[cfg(test)]
mod server_token_tests {
    use super::*;

    #[test]
    fn server_token_persists_and_reloads() {
        let dir = std::env::temp_dir().join(format!(
            "fly-substrate-token-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("substrate-server.json");
        let a = load_or_mint_server_token(&path);
        let b = load_or_mint_server_token(&path);
        assert_eq!(a, b, "second instance reuses the persisted token");
        assert_eq!(a.len(), 64);
        std::fs::write(&path, b"{corrupt").unwrap();
        let c = load_or_mint_server_token(&path);
        assert_ne!(c, a, "corrupt file re-mints");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
