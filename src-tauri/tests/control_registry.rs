//! Registry-over-socket coverage (Electron-shell migration U2): the ported
//! command table serving real managers end-to-end through the control
//! socket — KTD1 name/shape fidelity (camelCase args exactly as `ipc.ts`
//! sends them), config persistence through the store, graceful unknown-pane
//! answers, and the deliberate U3 refusals. Durable-store commands
//! (session/resume/scrollback) are exercised by their own unit tests against
//! injected paths; here we avoid touching the real data root.

use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use fly_lib::control::frame::{read_frame, write_frame};
use fly_lib::control::registry::{build_registry, CoreHandles};
use fly_lib::control::{ControlServer, Frame};

struct Rig {
    _server: ControlServer,
    client: UnixStream,
    _tmp: tempfile::TempDir,
}

fn rig() -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let config = Arc::new(fly_lib::config::ConfigStore::load(
        tmp.path().join("config.json"),
    ));
    let handles = CoreHandles {
        pty: Arc::new(fly_lib::pty::PtyManager::new()),
        attention: Arc::new(fly_lib::state::AttentionManager::new(400, false)),
        config,
        coalescers: Arc::new(fly_lib::stream::coalesce::CoalescerRegistry::default()),
        automations: None,
        feed: Some(Arc::new(fly_lib::feed::FeedState::new())),
        events: Arc::new(|_, _| {}),
    };
    let path = tmp.path().join("control.sock");
    let server = ControlServer::start(path.clone(), build_registry(handles), None).unwrap();
    let client = UnixStream::connect(&path).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    Rig {
        _server: server,
        client,
        _tmp: tmp,
    }
}

fn call(rig: &mut Rig, id: u64, cmd: &str, args: serde_json::Value) -> serde_json::Value {
    let body =
        serde_json::to_vec(&serde_json::json!({"id": id, "cmd": cmd, "args": args})).unwrap();
    write_frame(&mut rig.client, &Frame::Json(body)).unwrap();
    loop {
        match read_frame(&mut rig.client).unwrap().unwrap() {
            Frame::Json(b) => {
                let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
                // Skip broadcast events; we want the response to `id`.
                if v.get("id").is_some() {
                    assert_eq!(v["id"], id);
                    return v;
                }
            }
            other => panic!("unexpected frame {other:?}"),
        }
    }
}

#[test]
fn config_roundtrips_through_the_store() {
    let mut r = rig();
    let v = call(&mut r, 1, "get_config", serde_json::Value::Null);
    let mut config = v["ok"].clone();
    assert_eq!(config["fontSize"], 15); // default
    config["fontSize"] = serde_json::json!(17);
    let v = call(&mut r, 2, "set_config", serde_json::json!({ "config": config }));
    assert_eq!(v["ok"]["fontSize"], 17);
    let v = call(&mut r, 3, "get_config", serde_json::Value::Null);
    assert_eq!(v["ok"]["fontSize"], 17, "set must update the live store");
}

#[test]
fn unknown_panes_answer_gracefully_with_camel_case_args() {
    let mut r = rig();
    // Accessors: null-ish, never an error (the Tauri behavior).
    let v = call(&mut r, 1, "pane_cwd", serde_json::json!({"paneId": 999}));
    assert!(v["ok"].is_null());
    let v = call(&mut r, 2, "pane_activity", serde_json::json!({"paneId": 999}));
    assert_eq!(v["ok"]["isAgent"], false);
    assert_eq!(v["ok"]["liveTaskCount"], 0);
    let v = call(&mut r, 3, "panes_status", serde_json::json!({"paneIds": []}));
    assert_eq!(v["ok"], serde_json::json!([]));
    // Mutations on a gone pane: an error string, not a crash.
    let v = call(
        &mut r,
        4,
        "pty_resize",
        serde_json::json!({"paneId": 999, "rows": 24, "cols": 80}),
    );
    assert!(v["err"].is_string());
    // snake_case args must be refused — the wire is camelCase (KTD1).
    let v = call(&mut r, 5, "pane_cwd", serde_json::json!({"pane_id": 1}));
    assert!(v["err"].as_str().unwrap().contains("bad arguments"));
}

#[test]
fn attention_replication_commands_accept_their_shapes() {
    let mut r = rig();
    for (id, cmd, args) in [
        (1, "set_muted", serde_json::json!({"muted": true})),
        (2, "set_panel_open", serde_json::json!({"open": true})),
        (3, "set_window_foreground", serde_json::json!({"foregrounded": false})),
        (
            4,
            "set_workspace_muted",
            serde_json::json!({"workspace": "ws-1", "muted": true}),
        ),
        (
            5,
            "set_pane_workspace",
            serde_json::json!({"paneId": 1, "workspace": "ws-1"}),
        ),
        (6, "set_visible_panes", serde_json::json!({"paneIds": [1, 2]})),
    ] {
        let v = call(&mut r, id, cmd, args);
        assert!(v.get("ok").is_some(), "{cmd} should succeed: {v}");
    }
}

#[test]
fn stateless_helpers_answer() {
    let mut r = rig();
    let v = call(
        &mut r,
        1,
        "monitor_pickup_check",
        serde_json::json!({"transcriptPath": "/definitely/not/here", "cwd": "/also/not"}),
    );
    assert_eq!(v["ok"]["transcriptExists"], false);
    assert_eq!(v["ok"]["cwdExists"], false);
    let v = call(&mut r, 2, "frontend_log", serde_json::json!({"msg": "probe"}));
    assert!(v["ok"].is_null());
    let v = call(
        &mut r,
        3,
        "qualifying_session_count",
        serde_json::json!({"cwd": "/nonexistent-cwd-for-test"}),
    );
    assert_eq!(v["ok"], 0);
}

#[test]
fn feed_publish_works_and_automations_refuse_without_manager() {
    let mut r = rig();
    // An empty roster into a fresh state is content-unchanged → false (the
    // stamp still advances — that asymmetry is the liveness signal); a real
    // row is a content change → true. Same wire shape `feed.ts` pushes.
    let v = call(
        &mut r,
        1,
        "publish_agent_feed",
        serde_json::json!({"payload": {"agents": []}}),
    );
    assert_eq!(v["ok"], false);
    let agent = serde_json::json!({
        "leafKey": "leaf-1", "workspace": "ws", "tab": "t", "cwd": null,
        "status": "idle", "needsAttention": false, "reason": null,
        "workingForMs": null, "liveTaskCount": 0, "num": 1
    });
    let v = call(
        &mut r,
        7,
        "publish_agent_feed",
        serde_json::json!({"payload": {"agents": [agent]}}),
    );
    assert_eq!(v["ok"], true);
    for (id, cmd) in [
        (2, "list_automations"),
        (3, "automations_frontend_ready"),
    ] {
        let v = call(&mut r, id, cmd, serde_json::Value::Null);
        assert!(
            v["err"].as_str().unwrap().contains("automations unavailable"),
            "{cmd}: {v}"
        );
    }
}

#[test]
fn shell_coupled_commands_name_their_unit() {
    let mut r = rig();
    for (id, cmd) in [(1, "spawn_pane"), (2, "register_alert_sink"), (3, "get_launch_mode")] {
        let v = call(&mut r, id, cmd, serde_json::Value::Null);
        assert!(v["err"].as_str().unwrap().contains("U3"), "{cmd}: {v}");
    }
    let v = call(&mut r, 4, "no/such", serde_json::Value::Null);
    assert!(v["err"].as_str().unwrap().contains("unknown command"));
}
