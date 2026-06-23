//! Session persistence and restore (U12, R13/R14).
//!
//! Restore is explicitly lossy: the frontend rebuilds the layout and spawns
//! fresh shells in the saved cwd, replays capped scrollback as inert text, and
//! never auto-runs a prior command (the Zellij ENTER-to-run lesson, KTD10).
//! The backend persists an opaque layout blob (the frontend owns the tree) plus
//! per-pane scrollback files. State lives under the XDG data dir — a *separate*
//! tree from the config store (U13), so a corrupt session never wipes settings.
//!
//! Resume state (the crash-durable agent mapping) lives in a sibling
//! write-through store, [`resume`], decoupled from this debounced layout blob.

pub mod resume;
pub mod transcript;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Bump when the session schema changes; an old version restores as a default
/// workspace rather than misparsing.
const SESSION_VERSION: u64 = 1;

pub fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
        // `<app>` = fly, or a FLY_APP_NAME override so a dev flavor keeps its
        // own session/scrollback separate from an installed release.
        .join(crate::app_dir_name())
}

pub fn session_path() -> PathBuf {
    data_dir().join("session.json")
}

pub fn scrollback_dir() -> PathBuf {
    data_dir().join("scrollback")
}

/// Persist the session: a `{version, layout}` document written atomically
/// (temp + rename) so a crash mid-write can't truncate the live file.
///
/// The session can now carry notification titles/bodies (agent output) when
/// `saveScrollback` is on, so it is written `0600` like the scrollback files —
/// and the mode is set on the **temp file before the rename**, so there is never
/// a world-readable window between rename and a chmod-after (U20/KTD16).
pub fn write_session(path: &Path, layout: &Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    let doc = serde_json::json!({ "version": SESSION_VERSION, "layout": layout });
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&doc)?)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Read the saved layout, or `None` if missing / corrupt / version-mismatched.
/// A bad file is renamed aside (not overwritten) so the user can recover it and
/// the caller falls back to a default workspace.
pub fn read_session(path: &Path) -> Option<Value> {
    let bytes = std::fs::read(path).ok()?;
    let parsed: Result<Value, _> = serde_json::from_slice(&bytes);
    match parsed {
        Ok(doc) if doc.get("version").and_then(Value::as_u64) == Some(SESSION_VERSION) => {
            doc.get("layout").cloned()
        }
        _ => {
            let backup = PathBuf::from(format!("{}.bad.bak", path.display()));
            let _ = std::fs::rename(path, &backup);
            None
        }
    }
}

/// Write a pane's scrollback to a `0600` file in a `0700` dir (opt-in; KTD10).
/// Perms are set explicitly, not left to umask.
pub fn write_scrollback(dir: &Path, pane_key: &str, data: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let path = dir.join(safe_key(pane_key));
    std::fs::write(&path, data)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

pub fn read_scrollback(dir: &Path, pane_key: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(safe_key(pane_key))).ok()
}

/// Keep pane keys (e.g. "leaf-3") filesystem-safe.
fn safe_key(key: &str) -> String {
    let cleaned: String = key
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if cleaned.is_empty() {
        "pane".into()
    } else {
        cleaned
    }
}

// ---- Tauri command surface -------------------------------------------------

#[tauri::command]
pub fn save_session(layout: Value) -> Result<(), String> {
    write_session(&session_path(), &layout).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn load_session() -> Option<Value> {
    read_session(&session_path())
}

#[tauri::command]
pub fn save_scrollback(pane_key: String, data: String) -> Result<(), String> {
    write_scrollback(&scrollback_dir(), &pane_key, &data).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn load_scrollback(pane_key: String) -> Option<String> {
    read_scrollback(&scrollback_dir(), &pane_key)
}
