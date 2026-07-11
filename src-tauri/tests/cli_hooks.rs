//! U9: `fly hooks setup`/`teardown` settings merge safety, and `fly notify`
//! delivery (R11).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fly_lib::cli::hooks::{apply, teardown};
use fly_lib::cli::notify;
use fly_lib::hooks::{Dispatch, HookServer, TokenRegistry};
use fly_lib::pty::PaneId;
use fly_lib::state::attention::Reason;
use serde_json::Value;

fn read(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

const BIN: &str = "/opt/fly/fly";

#[test]
fn setup_writes_hooks_idempotently_with_absolute_path() {
    let dir = tempfile::tempdir().unwrap();
    let settings = dir.path().join("settings.json");
    apply(&settings, Path::new(BIN)).unwrap();
    apply(&settings, Path::new(BIN)).unwrap(); // re-run must not duplicate

    let v = read(&settings);
    let notif = v["hooks"]["Notification"].as_array().unwrap();
    assert_eq!(notif.len(), 1, "re-run duplicated a hook");
    let cmd = notif[0]["hooks"][0]["command"].as_str().unwrap();
    assert!(cmd.contains(BIN), "command must use the absolute binary path");
    assert!(cmd.contains("notify permission"));

    let stop = v["hooks"]["Stop"].as_array().unwrap();
    assert_eq!(stop.len(), 1);
    assert!(stop[0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .contains("notify finished"));
}

#[test]
fn setup_installs_the_permission_request_hook_with_catch_all_matcher() {
    // hook-ask-channel U5/R1: PermissionRequest matches on tool names, so the
    // fly group needs the explicit "*" (verified live: fires for Bash and
    // AskUserQuestion alike); teardown removes it like every fly group.
    let dir = tempfile::tempdir().unwrap();
    let settings = dir.path().join("settings.json");
    apply(&settings, Path::new(BIN)).unwrap();
    apply(&settings, Path::new(BIN)).unwrap(); // idempotent

    let v = read(&settings);
    let perm = v["hooks"]["PermissionRequest"].as_array().unwrap();
    assert_eq!(perm.len(), 1, "re-run duplicated the group");
    assert_eq!(perm[0]["matcher"], "*");
    let cmd = perm[0]["hooks"][0]["command"].as_str().unwrap();
    assert!(cmd.contains(BIN));
    assert!(cmd.contains("notify --claude --permission-request"));
    // The other fly events stay matcher-less (fires for every value).
    assert!(v["hooks"]["Stop"][0].get("matcher").is_none());

    teardown(&settings).unwrap();
    let v = read(&settings);
    assert!(
        v.get("hooks")
            .and_then(|h| h.get("PermissionRequest"))
            .is_none(),
        "teardown must remove the fly PermissionRequest group"
    );
}

#[test]
fn setup_preserves_other_keys_and_user_hooks_and_backs_up() {
    let dir = tempfile::tempdir().unwrap();
    let settings = dir.path().join("settings.json");
    std::fs::write(
        &settings,
        r#"{
            "env": {"FOO": "bar"},
            "hooks": {
                "PostToolUse": [{"matcher":"Write","hooks":[{"type":"command","command":"echo user"}]}],
                "Stop": [{"hooks":[{"type":"command","command":"echo user-stop"}]}]
            }
        }"#,
    )
    .unwrap();

    apply(&settings, Path::new(BIN)).unwrap();
    let v = read(&settings);

    assert_eq!(v["env"]["FOO"], "bar", "unrelated keys preserved");
    assert_eq!(
        v["hooks"]["PostToolUse"].as_array().unwrap().len(),
        1,
        "user's PostToolUse hook preserved"
    );
    let stop = v["hooks"]["Stop"].as_array().unwrap();
    assert_eq!(stop.len(), 2, "fly's Stop hook added alongside the user's");
    assert!(stop
        .iter()
        .any(|g| g["hooks"][0]["command"] == "echo user-stop"));
    assert!(stop
        .iter()
        .any(|g| g["hooks"][0]["command"].as_str().unwrap().contains("notify finished")));

    let backup = PathBuf::from(format!("{}.fly.bak", settings.display()));
    assert!(backup.exists(), "original backed up before modification");
}

#[test]
fn teardown_removes_only_fly_hooks() {
    let dir = tempfile::tempdir().unwrap();
    let settings = dir.path().join("settings.json");
    std::fs::write(
        &settings,
        r#"{"env":{"A":"b"},"hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo user-stop"}]}]}}"#,
    )
    .unwrap();

    apply(&settings, Path::new(BIN)).unwrap();
    teardown(&settings).unwrap();
    let v = read(&settings);

    let stop = v["hooks"]["Stop"].as_array().unwrap();
    assert_eq!(stop.len(), 1, "user's Stop hook remains");
    assert_eq!(stop[0]["hooks"][0]["command"], "echo user-stop");
    assert!(
        v["hooks"].get("Notification").is_none(),
        "fly-only Notification event removed entirely"
    );
    assert_eq!(v["env"]["A"], "b");
}

// ---- SessionStart capture hook (fix-session-pane-attribution U7) -----------

#[test]
fn setup_installs_a_session_start_capture_group_idempotently() {
    // R1: setup adds the capture hook beside Notification/Stop — a --capture
    // invocation with NO matcher key, so it fires for every source
    // (startup/resume/clear/compact, KTD5) — and re-running never duplicates.
    let dir = tempfile::tempdir().unwrap();
    let settings = dir.path().join("settings.json");
    apply(&settings, Path::new(BIN)).unwrap();
    apply(&settings, Path::new(BIN)).unwrap();

    let v = read(&settings);
    let start = v["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(start.len(), 1, "re-run duplicated the capture group");
    let cmd = start[0]["hooks"][0]["command"].as_str().unwrap();
    assert!(cmd.contains(BIN));
    assert!(cmd.contains("notify --claude --capture"), "cmd: {cmd}");
    assert!(
        start[0].get("matcher").is_none(),
        "an empty/omitted matcher captures all sources"
    );
    // The attention hooks are untouched beside it.
    assert_eq!(v["hooks"]["Notification"].as_array().unwrap().len(), 1);
    assert_eq!(v["hooks"]["Stop"].as_array().unwrap().len(), 1);
}

#[test]
fn teardown_removes_the_session_start_group_preserving_the_users() {
    // Symmetric teardown (KTD5): fly's capture group goes; a user's own
    // SessionStart hook stays; the emptied fly-only arrays drop.
    let dir = tempfile::tempdir().unwrap();
    let settings = dir.path().join("settings.json");
    std::fs::write(
        &settings,
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"echo user-start"}]}]}}"#,
    )
    .unwrap();

    apply(&settings, Path::new(BIN)).unwrap();
    let v = read(&settings);
    assert_eq!(
        v["hooks"]["SessionStart"].as_array().unwrap().len(),
        2,
        "fly's capture group installed alongside the user's"
    );

    teardown(&settings).unwrap();
    let v = read(&settings);
    let start = v["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(start.len(), 1, "only fly's group removed");
    assert_eq!(start[0]["hooks"][0]["command"], "echo user-start");
    assert!(v["hooks"].get("Notification").is_none());
    assert!(v["hooks"].get("Stop").is_none());
}

#[test]
fn notify_send_reaches_the_socket() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hook.sock");
    let tokens = Arc::new(TokenRegistry::new());
    let rec: Arc<Mutex<Vec<(PaneId, Reason)>>> = Arc::new(Mutex::new(Vec::new()));
    let rec2 = Arc::clone(&rec);
    let dispatch: Dispatch = Arc::new(move |pane, hook| {
        rec2.lock().unwrap().push((pane, hook.reason));
    });
    let server = HookServer::start(path, Arc::clone(&tokens), dispatch).unwrap();
    let tok = tokens.issue(PaneId(4));

    notify::send(
        server.socket_path(),
        &tok,
        Reason::Permission,
        Some("title"),
        Some("body"),
        None,
        None,
        None,  // hook_event (U7): not exercised here
        false, // capture_only (fix-attribution U2): a normal raising message
    )
    .unwrap();

    let deadline = Instant::now() + Duration::from_secs(2);
    while rec.lock().unwrap().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(rec.lock().unwrap().as_slice(), &[(PaneId(4), Reason::Permission)]);
}

#[test]
fn claude_payload_maps_stop_to_finished() {
    let p = notify::parse_claude_payload(r#"{"hook_event_name":"Stop","message":"all done"}"#);
    assert_eq!(p.reason, Some(Reason::Finished));
    assert_eq!(p.message.as_deref(), Some("all done"));
}

#[test]
fn claude_payload_maps_permission_notification() {
    let p = notify::parse_claude_payload(
        r#"{"hook_event_name":"Notification","notification_type":"permission_request"}"#,
    );
    assert_eq!(p.reason, Some(Reason::Permission));
}
