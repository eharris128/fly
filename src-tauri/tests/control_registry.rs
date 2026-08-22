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
    _server: Arc<ControlServer>,
    client: UnixStream,
    _tmp: tempfile::TempDir,
}

fn rig() -> Rig {
    rig_with(None)
}

fn rig_with(shutdown: Option<Arc<dyn Fn() + Send + Sync>>) -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let config = Arc::new(fly_lib::config::ConfigStore::load(
        tmp.path().join("config.json"),
    ));
    let path = tmp.path().join("control.sock");
    // Both sinks broadcast through the server, resolved via a slot filled
    // after start — the same shape `fly core` wires.
    let server_slot: Arc<std::sync::Mutex<Option<Arc<ControlServer>>>> =
        Arc::new(std::sync::Mutex::new(None));
    let event_slot = Arc::clone(&server_slot);
    let bytes_slot = Arc::clone(&server_slot);
    let handles = CoreHandles {
        pty: Arc::new(fly_lib::pty::PtyManager::new()),
        tokens: Arc::new(fly_lib::hooks::TokenRegistry::new()),
        attention: Arc::new(fly_lib::state::AttentionManager::new(400, false)),
        config,
        coalescers: Arc::new(fly_lib::stream::coalesce::CoalescerRegistry::default()),
        automations: None,
        alerts: None,
        feed: Some(Arc::new(fly_lib::feed::FeedState::new())),
        hook_socket_path: tmp.path().join("hook.sock"),
        launch_mode: fly_lib::LaunchMode::Normal,
        events: Arc::new(move |name: &str, payload: serde_json::Value| {
            if let Some(s) = event_slot.lock().unwrap().as_ref() {
                s.broadcast_event(name, payload);
            }
        }),
        pane_bytes: Arc::new(move |pane: u64, bytes: Vec<u8>| {
            if let Some(s) = bytes_slot.lock().unwrap().as_ref() {
                s.broadcast_pane_output(pane, &bytes);
            }
        }),
        shutdown,
    };
    let server = Arc::new(
        ControlServer::start(path.clone(), build_registry(handles), None).unwrap(),
    );
    *server_slot.lock().unwrap() = Some(Arc::clone(&server));
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
fn core_shutdown_without_a_host_hook_answers_a_clear_error() {
    let mut r = rig();
    let v = call(&mut r, 1, "core/shutdown", serde_json::Value::Null);
    assert!(
        v["err"].as_str().unwrap().contains("core/shutdown"),
        "a host with no shutdown hook must refuse loudly, got {v}"
    );
}

#[test]
fn core_shutdown_triggers_the_host_hook_and_acks_before_teardown() {
    let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = Arc::clone(&fired);
    let mut r = rig_with(Some(Arc::new(move || {
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
    })));
    // The ack arrives on the still-open connection: the handler only requests
    // teardown; the host exits after the response flushes (U6).
    let v = call(&mut r, 1, "core/shutdown", serde_json::Value::Null);
    assert_eq!(v["ok"]["shuttingDown"], true);
    assert!(fired.load(std::sync::atomic::Ordering::SeqCst));
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
fn u35_commands_name_their_unit_and_unknown_cmds_err() {
    let mut r = rig();
    let v = call(
        &mut r,
        1,
        "register_alert_sink",
        serde_json::json!({"paneId": 1}),
    );
    assert!(v["err"].as_str().unwrap().contains("U3.5"), "{v}");
    let v = call(&mut r, 2, "no/such", serde_json::Value::Null);
    assert!(v["err"].as_str().unwrap().contains("unknown command"));
}

#[test]
fn get_launch_mode_serializes_like_the_tauri_command() {
    let mut r = rig();
    let v = call(&mut r, 1, "get_launch_mode", serde_json::Value::Null);
    assert_eq!(v["ok"], "normal"); // LaunchMode's lowercase serde shape
}

/// The U3 centerpiece: a real pane spawned over the socket, output arriving
/// as 0x02 binary frames tagged with the pane id, and close driving the
/// ordered teardown with a `pane://exit` event fanned out — final output
/// strictly before the exit note (the coalescer-drain ordering guarantee).
#[test]
fn spawn_stream_close_lifecycle_over_the_socket() {
    let mut r = rig();
    let v = call(
        &mut r,
        1,
        "spawn_pane",
        serde_json::json!({
            "rows": 24, "cols": 80,
            "leafKey": "leaf-e2e",
            "command": ["bash", "--norc", "-c", "echo CONTROL_MARKER; sleep 30"],
        }),
    );
    let pane = v["ok"].as_u64().expect("spawned pane id");

    // Collect frames until the marker shows up in this pane's output.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut out = Vec::new();
    while std::time::Instant::now() < deadline {
        match read_frame(&mut r.client).unwrap().unwrap() {
            Frame::PaneOutput { pane: p, bytes } if p == pane => {
                out.extend_from_slice(&bytes);
                if String::from_utf8_lossy(&out).contains("CONTROL_MARKER") {
                    break;
                }
            }
            _ => {}
        }
    }
    assert!(
        String::from_utf8_lossy(&out).contains("CONTROL_MARKER"),
        "pane output must ride the binary frames"
    );

    // Close → pane://exit event for this pane.
    let body = serde_json::to_vec(
        &serde_json::json!({"id": 2, "cmd": "close_pane", "args": {"paneId": pane}}),
    )
    .unwrap();
    write_frame(&mut r.client, &Frame::Json(body)).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut saw_exit = false;
    while std::time::Instant::now() < deadline && !saw_exit {
        match read_frame(&mut r.client).unwrap().unwrap() {
            Frame::Json(b) => {
                let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
                if v.get("event").map(|e| e == "pane://exit").unwrap_or(false)
                    && v["payload"]["paneId"] == pane
                {
                    saw_exit = true;
                }
            }
            _ => {}
        }
    }
    assert!(saw_exit, "close must fan out pane://exit");
}

/// `call` for a rig with a streaming pane: pane-output frames (0x02) can
/// interleave with the response and are not the answer — nor is a late
/// response to an earlier raw-written request (a close whose `pane://exit`
/// was read first); those are skipped rather than asserted against.
fn call_skipping_output(
    rig: &mut Rig,
    id: u64,
    cmd: &str,
    args: serde_json::Value,
) -> serde_json::Value {
    let body =
        serde_json::to_vec(&serde_json::json!({"id": id, "cmd": cmd, "args": args})).unwrap();
    write_frame(&mut rig.client, &Frame::Json(body)).unwrap();
    loop {
        match read_frame(&mut rig.client).unwrap().unwrap() {
            Frame::Json(b) => {
                let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
                if v.get("id").map(|i| i == id).unwrap_or(false) {
                    return v;
                }
            }
            Frame::PaneOutput { .. } => {}
            other => panic!("unexpected frame {other:?}"),
        }
    }
}

/// Renderer-crash recovery: a second client (the reloaded renderer) asks to
/// adopt the live pane for a leaf and gets the SAME pane id back with the
/// pane's current grid and recent output as the tail — no second spawn, so the agent behind
/// the leaf is neither orphaned (pty substrate) nor doubled (tmux). A leaf
/// nobody owns answers null, which is the caller's cue to spawn.
#[test]
fn adopt_live_pane_reattaches_the_same_pane_with_its_tail() {
    let mut r = rig();
    let v = call_skipping_output(
        &mut r,
        1,
        "spawn_pane",
        serde_json::json!({
            "rows": 24, "cols": 80,
            "leafKey": "leaf-adopt",
            "command": ["bash", "--norc", "-c", "echo ADOPT_MARKER; sleep 30"],
        }),
    );
    let pane = v["ok"].as_u64().expect("spawned pane id");

    // Wait for the marker to have flowed (it lands in the tail ring on the
    // way to the broadcast sink).
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut seen = false;
    while std::time::Instant::now() < deadline && !seen {
        if let Frame::PaneOutput { pane: p, bytes } = read_frame(&mut r.client).unwrap().unwrap() {
            seen = p == pane && String::from_utf8_lossy(&bytes).contains("ADOPT_MARKER");
        }
    }
    assert!(seen, "marker must have been emitted before adopting");

    // Nobody owns this leaf → null → spawn.
    let v = call_skipping_output(
        &mut r,
        2,
        "adopt_live_pane",
        serde_json::json!({"leafKey": "leaf-nobody"}),
    );
    assert!(v["ok"].is_null(), "unknown leaf answers null, got {v}");

    // The live leaf → same id, tail carries the marker, attention idle.
    let v = call_skipping_output(
        &mut r,
        3,
        "adopt_live_pane",
        serde_json::json!({"leafKey": "leaf-adopt"}),
    );
    assert_eq!(v["ok"]["paneId"], pane, "re-attach returns the live pane, not a new one");
    assert!(
        v["ok"]["tail"].as_str().unwrap().contains("ADOPT_MARKER"),
        "tail ring replays recent output: {v}"
    );
    assert_eq!(v["ok"]["rows"], 24, "the pane's own grid rides along: {v}");
    assert_eq!(v["ok"]["cols"], 80);
    assert_eq!(v["ok"]["attention"], "idle");
    assert!(v["ok"]["reason"].is_null());

    // Closing it makes the leaf un-adoptable again (live-only gate). Raw
    // write: the exit event and the close response arrive in either order.
    let body = serde_json::to_vec(
        &serde_json::json!({"id": 4, "cmd": "close_pane", "args": {"paneId": pane}}),
    )
    .unwrap();
    write_frame(&mut r.client, &Frame::Json(body)).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut gone = false;
    while std::time::Instant::now() < deadline && !gone {
        if let Frame::Json(b) = read_frame(&mut r.client).unwrap().unwrap() {
            let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
            gone = v.get("event").map(|e| e == "pane://exit").unwrap_or(false)
                && v["payload"]["paneId"] == pane;
        }
    }
    assert!(gone, "close must fan out pane://exit");
    let v = call_skipping_output(
        &mut r,
        5,
        "adopt_live_pane",
        serde_json::json!({"leafKey": "leaf-adopt"}),
    );
    assert!(v["ok"].is_null(), "an exited pane is not adoptable: {v}");
}
