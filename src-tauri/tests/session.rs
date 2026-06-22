//! U12 session persistence (R13/R14): round-trip, corrupt/version fallback,
//! and scrollback file permissions.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use fly_lib::session::{read_scrollback, read_session, write_scrollback, write_session};
use serde_json::json;

#[test]
fn session_layout_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");
    let layout = json!({
        "tabs": [{ "tree": { "kind": "leaf", "key": "leaf-1" },
                   "panes": { "leaf-1": { "cwd": "/tmp", "title": "agent 1" } } }]
    });
    write_session(&path, &layout).unwrap();
    assert_eq!(read_session(&path), Some(layout));
}

#[test]
fn missing_session_is_none() {
    let dir = tempfile::tempdir().unwrap();
    assert!(read_session(&dir.path().join("nope.json")).is_none());
}

#[test]
fn corrupt_session_backs_up_and_returns_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");
    std::fs::write(&path, "{ this is not valid json").unwrap();
    assert!(read_session(&path).is_none());
    let backup = PathBuf::from(format!("{}.bad.bak", path.display()));
    assert!(backup.exists(), "corrupt session should be preserved, not lost");
}

#[test]
fn version_mismatch_falls_back_to_default() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");
    std::fs::write(&path, r#"{"version":999,"layout":{"tabs":[]}}"#).unwrap();
    assert!(read_session(&path).is_none());
}

#[test]
fn session_file_is_owner_only() {
    // The session may carry notification bodies (agent output), so it is 0600
    // like the scrollback files — set before the rename, no world-readable
    // window (U20/KTD16).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");
    write_session(&path, &json!({ "tabs": [] })).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "session file must be 0600");
}

#[test]
fn scrollback_round_trips_with_owner_only_perms() {
    let dir = tempfile::tempdir().unwrap();
    let sb = dir.path().join("scrollback");
    write_scrollback(&sb, "leaf-1", "previous output\n").unwrap();
    assert_eq!(read_scrollback(&sb, "leaf-1").as_deref(), Some("previous output\n"));

    let dir_mode = std::fs::metadata(&sb).unwrap().permissions().mode() & 0o777;
    let file_mode = std::fs::metadata(sb.join("leaf-1")).unwrap().permissions().mode() & 0o777;
    assert_eq!(dir_mode, 0o700, "scrollback dir must be 0700");
    assert_eq!(file_mode, 0o600, "scrollback file must be 0600");
}
