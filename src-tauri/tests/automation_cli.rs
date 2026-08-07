//! U9: the `fly automation` socket path end-to-end — the request handler's
//! routing, the R22 recursion gate, origin stamping, and the security boundary
//! (invalid token → no response), plus the AppHandle-free dispatch core.
//! Monitor-handoff U4 rides the same harness: monitor creates capture pickup
//! pointers through an injected resolver (R11), refuse without them (R12), and
//! emit the registered event after the store flush (R13's backend half).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use fly_lib::automations::model::MonitorPointers;
use fly_lib::automations::store::Store;
use fly_lib::automations::{AutomationManager, Dispatcher, UnwiredDispatcher, ERR_MONITOR_POINTERS};
use fly_lib::cli::automation::{send_request, AutomationRequest, AutomationResponse};
use fly_lib::dispatch_automation_op;
use fly_lib::hooks::{Dispatch, HookServer, RequestHandler, TokenRegistry};
use fly_lib::pty::PaneId;

/// 2026-01-06T00:00:00Z — a valid instant for croner on a 5-minute boundary.
const T0: u64 = 1_767_657_600_000;

fn manager() -> (Arc<AutomationManager>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::load_at(
        dir.path().join("automations.json"),
        dir.path().join("automation-scripts"),
    );
    let mgr = Arc::new(AutomationManager::new(
        store,
        Arc::new(UnwiredDispatcher) as Arc<dyn Dispatcher>,
        Box::new(|| T0),
        Box::new(|_id: &str| {}),
    ));
    (mgr, dir)
}

/// The fixed qualified pointer set the socket-level harness resolves for
/// monitor creates (the real pane-precise resolver is wired in `lib.rs`; the
/// dispatch core only sees the seam).
fn sock_pointers() -> MonitorPointers {
    MonitorPointers {
        session_id: "sess-sock".into(),
        transcript_path: "/root/-proj-app/sess-sock.jsonl".into(),
        session_cwd: "/proj/app".into(),
    }
}

/// A hook server whose automation handler routes to `mgr` via the real
/// (AppHandle-free) dispatch core, stamping a fixed workspace and resolving
/// monitor pickup pointers to [`sock_pointers`].
fn server_over(
    mgr: &Arc<AutomationManager>,
    tokens: &Arc<TokenRegistry>,
) -> HookServer {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("hook.sock");
    // Leak the tempdir so the socket path outlives this fn (the server owns it
    // until dropped by the caller); tests are short-lived processes.
    std::mem::forget(dir);
    let noop: Dispatch = Arc::new(|_pane, _hook| {});
    let mgr_h = Arc::clone(mgr);
    let handler: RequestHandler = Arc::new(move |pane, buf: &[u8]| {
        let req: AutomationRequest = match serde_json::from_slice(buf) {
            Ok(r) => r,
            Err(e) => return AutomationResponse::err(format!("bad: {e}")).to_bytes(),
        };
        let is_recursion = mgr_h.is_automation_pane(pane.0);
        dispatch_automation_op(&mgr_h, pane.0, "ws-test", is_recursion, req, &|| {
            Some(sock_pointers())
        })
        .to_bytes()
    });
    HookServer::start_with_handler(sock, Arc::clone(tokens), noop, Some(handler)).unwrap()
}

/// The stub resolver for direct dispatch-core calls that never reach (or must
/// never attempt) pointer resolution.
fn no_pointers() -> Option<MonitorPointers> {
    None
}

fn create_req(token: &str) -> AutomationRequest {
    AutomationRequest {
        token: token.to_string(),
        op: "automation/create".to_string(),
        name: Some("nightly".to_string()),
        cron: Some("*/5 * * * *".to_string()),
        timezone: Some("UTC".to_string()),
        cwd: Some("/tmp".to_string()),
        prompt: Some("summarize CI".to_string()),
        ..Default::default()
    }
}

#[test]
fn create_over_socket_persists_and_stamps_origin_r9_r24() {
    let (mgr, _d) = manager();
    let tokens = Arc::new(TokenRegistry::new());
    let server = server_over(&mgr, &tokens);
    let tok = tokens.issue(PaneId(5));

    let resp = send_request(server.socket_path(), &create_req(&tok)).unwrap();
    assert!(resp.ok, "create succeeds: {resp:?}");
    let id = resp.id.expect("create returns the new id");

    let a = mgr.get(&id).expect("automation persisted to the store");
    assert_eq!(a.name, "nightly");
    // Origin stamped from the validated pane + its workspace (R9/R22).
    assert_eq!(a.origin.pane_id, 5, "origin pane is the validated caller");
    assert_eq!(a.origin.workspace_id, "ws-test");
    assert_eq!(a.origin.label, "cli");
}

#[test]
fn recursion_gate_rejects_create_from_an_automation_pane_r22() {
    let (mgr, _d) = manager();
    let tokens = Arc::new(TokenRegistry::new());
    let server = server_over(&mgr, &tokens);
    // Mark pane 6 as automation-spawned (as spawn_pane would, U7.5).
    mgr.register_automation_pane(6);
    let tok = tokens.issue(PaneId(6));

    let resp = send_request(server.socket_path(), &create_req(&tok)).unwrap();
    assert!(!resp.ok, "an automation-spawned pane may not create");
    assert!(
        resp.error.unwrap().contains("automation-spawned"),
        "the recursion gate is the stated reason"
    );
    assert!(mgr.list().is_empty(), "nothing persisted");

    // monitor-handoff U4: the monitor flavor changes nothing about the gate —
    // a monitor create from the same automation-spawned pane is refused with
    // the same reason (the harness resolver would happily supply pointers,
    // so the refusal proves the gate fires first).
    let mut monitor_req = create_req(&tok);
    monitor_req.monitor = true;
    let resp = send_request(server.socket_path(), &monitor_req).unwrap();
    assert!(!resp.ok, "an automation-spawned pane may not create a monitor");
    assert!(
        resp.error.unwrap().contains("automation-spawned"),
        "the recursion gate is the stated reason for monitors too"
    );
    assert!(mgr.list().is_empty(), "nothing persisted");
}

// monitor-handoff U4 + R22 (dispatch core): the recursion gate precedes
// pointer resolution — a gated monitor create never touches the resume
// store or a transcript.
#[test]
fn recursion_gate_precedes_monitor_pointer_resolution_r22() {
    let (mgr, _d) = manager();
    let resolved = AtomicBool::new(false);
    let mut req = create_req("tok");
    req.monitor = true;

    let resp = dispatch_automation_op(&mgr, 6, "ws", true, req, &|| {
        resolved.store(true, Ordering::SeqCst);
        Some(sock_pointers())
    });
    assert!(!resp.ok);
    assert!(resp.error.unwrap().contains("automation-spawned"));
    assert!(
        !resolved.load(Ordering::SeqCst),
        "a gated create never attempts pointer resolution"
    );
    assert!(mgr.list().is_empty(), "nothing persisted");
}

#[test]
fn invalid_token_gets_no_response_the_security_boundary_holds() {
    let (mgr, _d) = manager();
    let tokens = Arc::new(TokenRegistry::new());
    let server = server_over(&mgr, &tokens);
    let _real = tokens.issue(PaneId(7));

    // A forged token never validates → the server dispatches nothing and writes
    // no response → the client reports the bounded-wait "may have committed".
    let mut req = create_req("deadbeef-not-a-real-token");
    req.name = Some("evil".to_string());
    let result = send_request(server.socket_path(), &req);
    assert!(result.is_err(), "no response for an invalid token");
    assert!(mgr.list().is_empty(), "nothing was created");
}

#[test]
fn create_persists_agent_model_and_effort_and_defaults_to_none_r9_r10() {
    use fly_lib::automations::model::Mode;
    let (mgr, _d) = manager();

    // With --model/--effort: they land on the stored Mode::Agent.
    let with = dispatch_automation_op(
        &mgr,
        1,
        "ws",
        false,
        AutomationRequest {
            op: "automation/create".to_string(),
            name: Some("pinned".to_string()),
            cron: Some("*/5 * * * *".to_string()),
            timezone: Some("UTC".to_string()),
            cwd: Some("/tmp".to_string()),
            prompt: Some("audit disk".to_string()),
            model: Some("opus".to_string()),
            effort: Some("high".to_string()),
            ..Default::default()
        },
        &no_pointers,
    );
    assert!(with.ok, "create with model/effort: {with:?}");
    let a = mgr.get(&with.id.unwrap()).unwrap();
    match a.mode {
        Mode::Agent {
            ref prompt,
            ref model,
            ref effort, headless: None,
        } => {
            assert_eq!(prompt, "audit disk");
            assert_eq!(model.as_deref(), Some("opus"));
            assert_eq!(effort.as_deref(), Some("high"));
        }
        other => panic!("expected agent mode, got {other:?}"),
    }

    // Without them: the stored Mode::Agent carries None for both.
    let plain = dispatch_automation_op(&mgr, 1, "ws", false, create_req("tok"), &no_pointers);
    assert!(plain.ok);
    let a = mgr.get(&plain.id.unwrap()).unwrap();
    match a.mode {
        Mode::Agent { model, effort, .. } => {
            assert_eq!(model, None, "no --model ⇒ None");
            assert_eq!(effort, None, "no --effort ⇒ None");
        }
        other => panic!("expected agent mode, got {other:?}"),
    }
}

#[test]
fn dispatch_core_routes_create_pause_resume_delete_and_rejects_unknown() {
    let (mgr, _d) = manager();

    // create
    let resp = dispatch_automation_op(&mgr, 1, "ws", false, create_req("tok"), &no_pointers);
    assert!(resp.ok);
    let id = resp.id.unwrap();

    // pause → resume → the automation survives both
    let pause = dispatch_automation_op(
        &mgr,
        1,
        "ws",
        false,
        AutomationRequest {
            op: "automation/pause".to_string(),
            id: Some(id.clone()),
            ..Default::default()
        },
        &no_pointers,
    );
    assert!(pause.ok);
    assert_eq!(mgr.get(&id).unwrap().next_run_at, None, "paused");

    let resume = dispatch_automation_op(
        &mgr,
        1,
        "ws",
        false,
        AutomationRequest {
            op: "automation/resume".to_string(),
            id: Some(id.clone()),
            ..Default::default()
        },
        &no_pointers,
    );
    assert!(resume.ok);
    assert!(mgr.get(&id).unwrap().next_run_at.is_some(), "re-armed");

    // unknown op → error, nothing changes
    let bogus = dispatch_automation_op(
        &mgr,
        1,
        "ws",
        false,
        AutomationRequest {
            op: "automation/frobnicate".to_string(),
            id: Some(id.clone()),
            ..Default::default()
        },
        &no_pointers,
    );
    assert!(!bogus.ok);
    assert!(bogus.error.unwrap().contains("unknown automation op"));

    // delete → gone
    let del = dispatch_automation_op(
        &mgr,
        1,
        "ws",
        false,
        AutomationRequest {
            op: "automation/delete".to_string(),
            id: Some(id.clone()),
            ..Default::default()
        },
        &no_pointers,
    );
    assert!(del.ok);
    assert!(mgr.get(&id).is_none(), "deleted");
}

// ---- monitor registration (monitor-handoff U4) --------------------------------

/// Wire a collector onto the manager's monitor-registered seam, returning the
/// captured `(paneId, automationId)` pairs. The emitter also probes the store
/// at emit time, proving the R13 ordering: the event fires only after the
/// create's flush made the automation readable.
fn collect_registered(mgr: &Arc<AutomationManager>) -> Arc<Mutex<Vec<(u64, String)>>> {
    let events: Arc<Mutex<Vec<(u64, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&events);
    let probe = Arc::clone(mgr);
    mgr.set_monitor_registered_emitter(Arc::new(move |ev| {
        assert!(
            probe.get(&ev.automation_id).is_some(),
            "monitor-registered fires only after the store flush (R13)"
        );
        seen.lock().unwrap().push((ev.pane_id, ev.automation_id.clone()));
    }));
    events
}

// R11 + R13 (backend half) + R9: a monitor create from a pane whose session
// resolves stores the pointers verbatim, threads the not-before floor,
// defaults retry-on-interrupt ON, and emits `automation://monitor-registered`
// { paneId, automationId } after the flush.
#[test]
fn monitor_create_stores_pointers_and_emits_registered_r11_r13() {
    let (mgr, _d) = manager();
    let events = collect_registered(&mgr);

    let want = MonitorPointers {
        session_id: "sess-hook".into(),
        transcript_path: "/root/-home-u-exp/sess-hook.jsonl".into(),
        session_cwd: "/home/u/exp".into(),
    };
    let resolved = want.clone();
    let mut req = create_req("tok");
    req.monitor = true;
    req.not_before_ms = Some(T0 + 3_600_000);

    let resp = dispatch_automation_op(&mgr, 9, "ws", false, req, &move || {
        Some(resolved.clone())
    });
    assert!(resp.ok, "monitor create succeeds: {resp:?}");
    let id = resp.id.expect("create returns the new id");

    let a = mgr.get(&id).expect("persisted");
    assert!(a.monitor, "monitor flag stamped");
    assert_eq!(
        a.pickup_pointers,
        Some(want),
        "pointers stored verbatim (R11)"
    );
    assert_eq!(
        a.not_before_ms,
        Some(T0 + 3_600_000),
        "not-before threaded through the spec (U2 consumes it at create)"
    );
    assert!(
        a.retry_on_interrupt,
        "R9: monitors default retry-on-interrupt on"
    );
    assert_eq!(
        *events.lock().unwrap(),
        vec![(9, id)],
        "one registered event carrying the origin pane + new id (R13)"
    );
}

// R12: an unresolvable/unqualified session (no resume record, metadata-only
// transcript, implausible id — the resolver abstains for all of them) refuses
// the create with THE distinct error string; nothing is stored, nothing emits.
#[test]
fn monitor_create_refuses_without_qualified_pointers_r12() {
    let (mgr, _d) = manager();
    let events = collect_registered(&mgr);
    let mut req = create_req("tok");
    req.monitor = true;

    let resp = dispatch_automation_op(&mgr, 9, "ws", false, req, &no_pointers);
    assert!(!resp.ok, "refused");
    assert_eq!(
        resp.error.as_deref(),
        Some(ERR_MONITOR_POINTERS),
        "the distinct R12 refusal string"
    );
    assert!(mgr.list().is_empty(), "NOTHING stored (R12)");
    assert!(events.lock().unwrap().is_empty(), "no registered event");
}

// fix(review) #14 (monitor-handoff R12/R13): a monitor create whose store
// flush FAILED still answers ok + warning — the record is live in memory
// (KTD-B), and the CLI prints the warning in the still-open parent tab —
// but emits NO monitor-registered event: closing that tab on a registration
// that dies at restart would discard the very session the monitor is
// supposed to hand back to (refuse-rather-than-lose).
#[test]
fn flush_failed_monitor_create_warns_but_never_emits_registered_r12() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let store = Store::load_at(
        data.join("automations.json"),
        data.join("automation-scripts"),
    );
    let mgr = Arc::new(AutomationManager::new(
        store,
        Arc::new(UnwiredDispatcher) as Arc<dyn Dispatcher>,
        Box::new(|| T0),
        Box::new(|_id: &str| {}),
    ));
    let events = collect_registered(&mgr);

    // Failure injection (the store.rs pattern): remove the store dir (the
    // construction-time recovery flush created it) and make its parent
    // read-only, so the create's flush fails at create_dir_all.
    let _ = std::fs::remove_dir_all(&data);
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
    if std::fs::create_dir(dir.path().join("probe")).is_ok() {
        // Running as root (or an ACL overrides the mode): the injection
        // cannot work — skip gracefully, like the store.rs tests.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        eprintln!("skipping flush-failure test: read-only dir is still writable");
        return;
    }

    let mut req = create_req("tok");
    req.monitor = true;
    let resp = dispatch_automation_op(&mgr, 9, "ws", false, req, &|| Some(sock_pointers()));
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();

    assert!(resp.ok, "the create still succeeds in memory (KTD-B): {resp:?}");
    assert!(
        resp.warning
            .as_deref()
            .unwrap_or("")
            .contains("store flush failed"),
        "the flush warning rides the response for the CLI to print: {:?}",
        resp.warning
    );
    let id = resp.id.expect("the new id is returned");
    assert!(mgr.get(&id).is_some(), "live in memory");
    assert!(
        events.lock().unwrap().is_empty(),
        "NO monitor-registered event — the parent tab must stay open (R12)"
    );
}

// The non-monitor path is untouched by U4: no pointer resolution is even
// attempted, nothing rides the new fields, and no registered event fires.
#[test]
fn non_monitor_create_never_resolves_pointers_or_emits_registered() {
    let (mgr, _d) = manager();
    let events = collect_registered(&mgr);
    let resolved = AtomicBool::new(false);

    let resp = dispatch_automation_op(&mgr, 1, "ws", false, create_req("tok"), &|| {
        resolved.store(true, Ordering::SeqCst);
        Some(sock_pointers())
    });
    assert!(resp.ok, "{resp:?}");
    assert!(
        !resolved.load(Ordering::SeqCst),
        "no pointer resolution attempted for a plain create"
    );
    let a = mgr.get(&resp.id.unwrap()).unwrap();
    assert!(!a.monitor);
    assert_eq!(a.pickup_pointers, None);
    assert_eq!(a.not_before_ms, None);
    assert!(
        !a.retry_on_interrupt,
        "the R9 default only flips for monitors"
    );
    assert!(events.lock().unwrap().is_empty(), "no registered event");
}

// monitor-handoff R1: a monitor is an agent-mode automation — a script-mode
// monitor create is rejected at the socket boundary (the wire is untrusted;
// the CLI-side rejection is U5), before any pointer resolution.
#[test]
fn monitor_create_rejects_script_mode_r1() {
    let (mgr, _d) = manager();
    let resolved = AtomicBool::new(false);
    let req = AutomationRequest {
        op: "automation/create".to_string(),
        name: Some("watch".to_string()),
        cron: Some("*/5 * * * *".to_string()),
        timezone: Some("UTC".to_string()),
        cwd: Some("/tmp".to_string()),
        script: Some("echo hi".to_string()),
        monitor: true,
        ..Default::default()
    };

    let resp = dispatch_automation_op(&mgr, 1, "ws", false, req, &|| {
        resolved.store(true, Ordering::SeqCst);
        Some(sock_pointers())
    });
    assert!(!resp.ok);
    assert!(
        resp.error.unwrap().contains("agent-mode"),
        "the mode conflict is the stated reason"
    );
    assert!(
        !resolved.load(Ordering::SeqCst),
        "mode validation precedes pointer resolution"
    );
    assert!(mgr.list().is_empty(), "nothing stored");
}

// ---- CLI create surface (monitor-handoff U5) ----------------------------------

/// String-vec sugar for driving the real CLI arg loop.
fn argv(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

// monitor-handoff U5 + R1: the monitor flag combinations the plan rejects are
// all refused CLI-side (exit 2) — before pane env resolution, so no socket
// (and no FLY_PANE_TOKEN) is ever needed: `--monitor` with `--script`;
// `--not-before` without `--monitor` (the floor stays monitor-only); a
// malformed `--not-before` timestamp.
#[test]
fn cli_create_rejects_monitor_flag_misuse_before_the_socket_u5_r1() {
    assert_eq!(
        fly_lib::cli::automation::run(&argv(&[
            "create", "--name", "w", "--cron", "*/30 * * * *", "--script", "echo hi",
            "--monitor",
        ])),
        2,
        "--monitor with --script is rejected CLI-side"
    );
    assert_eq!(
        fly_lib::cli::automation::run(&argv(&[
            "create", "--name", "w", "--cron", "*/30 * * * *", "--prompt", "check",
            "--not-before", "2099-01-01T00:00:00Z",
        ])),
        2,
        "--not-before without --monitor is rejected (monitor-only floor)"
    );
    assert_eq!(
        fly_lib::cli::automation::run(&argv(&[
            "create", "--name", "w", "--cron", "*/30 * * * *", "--prompt", "check",
            "--monitor", "--not-before", "next tuesday",
        ])),
        2,
        "a malformed --not-before timestamp is rejected CLI-side"
    );
    assert_eq!(
        fly_lib::cli::automation::run(&argv(&[
            "create", "--name", "w", "--cron", "*/30 * * * *", "--prompt", "check",
            "--monitor", "--not-before",
        ])),
        2,
        "--not-before with no value is rejected"
    );
}

// monitor-handoff U5 round-trip (R1/R8/R9/R10): the REAL CLI create path —
// arg loop → validation → R8 defaults → request → hook socket — carries
// `--monitor`/`--not-before`. The stored monitor gets Sonnet-at-xhigh stamped
// into its own model/effort slot when unspecified (R8), keeps an explicit
// `--model`/`--effort` verbatim, holds the parsed not-before floor (first
// next_run_at at/after it, R1), defaults retry-on-interrupt on (R9), and
// carries the server-resolved pointers.
#[test]
fn cli_create_monitor_over_socket_stamps_r8_defaults_and_floor() {
    use fly_lib::automations::model::Mode;
    let (mgr, _d) = manager();
    let tokens = Arc::new(TokenRegistry::new());
    let server = server_over(&mgr, &tokens);
    let tok = tokens.issue(PaneId(11));
    // The mutating CLI path resolves the pane env; only this test sets these.
    std::env::set_var("FLY_PANE_TOKEN", &tok);
    std::env::set_var("FLY_SOCKET_PATH", server.socket_path());

    // 2026-07-12T00:00:00Z — after the harness clock T0 (2026-01-06).
    let nb_ms: u64 = 1_783_814_400_000;
    let code = fly_lib::cli::automation::run(&argv(&[
        "create", "--name", "training watch", "--cron", "0 */6 * * *", "--tz", "UTC",
        "--cwd", "/tmp", "--prompt", "check the training run", "--monitor",
        "--not-before", "2026-07-12T00:00:00Z",
    ]));
    assert_eq!(code, 0, "monitor create succeeds through the real CLI");

    let list = mgr.list();
    let a = list
        .iter()
        .find(|a| a.name == "training watch")
        .expect("persisted");
    assert!(a.monitor, "the CLI set the monitor flag");
    assert_eq!(a.not_before_ms, Some(nb_ms), "the parsed floor crossed the wire");
    assert!(
        a.next_run_at.expect("scheduled") >= nb_ms,
        "parked: the first occurrence is clamped at/after the floor (R1)"
    );
    assert!(a.retry_on_interrupt, "R9: monitor default rides the CLI path too");
    assert_eq!(a.pickup_pointers, Some(sock_pointers()), "pointers captured");
    match &a.mode {
        Mode::Agent { model, effort, .. } => {
            assert_eq!(model.as_deref(), Some("sonnet"), "R8 default model stamped");
            assert_eq!(effort.as_deref(), Some("xhigh"), "R8 default effort stamped");
        }
        other => panic!("expected agent mode, got {other:?}"),
    }

    // Explicit --model/--effort win over the R8 default, per-field.
    let code = fly_lib::cli::automation::run(&argv(&[
        "create", "--name", "pinned watch", "--cron", "0 */6 * * *", "--tz", "UTC",
        "--cwd", "/tmp", "--prompt", "check", "--monitor", "--model", "opus",
        "--effort", "high",
    ]));
    assert_eq!(code, 0);
    let list = mgr.list();
    let a = list.iter().find(|a| a.name == "pinned watch").expect("persisted");
    match &a.mode {
        Mode::Agent { model, effort, .. } => {
            assert_eq!(model.as_deref(), Some("opus"), "explicit model wins");
            assert_eq!(effort.as_deref(), Some("high"), "explicit effort wins");
        }
        other => panic!("expected agent mode, got {other:?}"),
    }
    assert_eq!(a.not_before_ms, None, "no floor unless --not-before was passed");
}

// R11 over the wire: the `monitor` + `not_before_ms` request fields cross the
// socket (serde round-trip through the real server) and the created monitor
// carries the harness-resolved pointers — i.e. pointers come from the
// server-side resolver keyed on the validated pane, never from the payload.
// R8 backstop (fix(review) #12): a raw-socket monitor create that BYPASSES
// the CLI (no model/effort in the payload) still lands sonnet/xhigh —
// stamped by the socket create arm, never left to ride
// config.automation_defaults; explicit values still win per-field.
#[test]
fn monitor_create_over_socket_round_trips_flags_and_stores_pointers_r11() {
    use fly_lib::automations::model::Mode;
    use fly_lib::cli::automation::{MONITOR_DEFAULT_EFFORT, MONITOR_DEFAULT_MODEL};
    let (mgr, _d) = manager();
    let tokens = Arc::new(TokenRegistry::new());
    let server = server_over(&mgr, &tokens);
    let tok = tokens.issue(PaneId(8));

    let mut req = create_req(&tok);
    req.monitor = true;
    req.not_before_ms = Some(T0 + 86_400_000);
    let resp = send_request(server.socket_path(), &req).unwrap();
    assert!(resp.ok, "monitor create over the socket: {resp:?}");

    let a = mgr.get(&resp.id.unwrap()).expect("persisted");
    assert!(a.monitor, "monitor flag crossed the socket");
    assert_eq!(
        a.not_before_ms,
        Some(T0 + 86_400_000),
        "not-before floor crossed the socket"
    );
    assert_eq!(
        a.pickup_pointers,
        Some(sock_pointers()),
        "pointers are the server-resolved set for the validated pane"
    );
    assert_eq!(a.origin.pane_id, 8, "origin still stamps the caller");
    match &a.mode {
        Mode::Agent { model, effort, .. } => {
            assert_eq!(
                model.as_deref(),
                Some(MONITOR_DEFAULT_MODEL),
                "R8 default model backstopped socket-side (fix(review) #12)"
            );
            assert_eq!(
                effort.as_deref(),
                Some(MONITOR_DEFAULT_EFFORT),
                "R8 default effort backstopped socket-side"
            );
        }
        other => panic!("expected agent mode, got {other:?}"),
    }

    // Explicit payload values still win per-field over the backstop.
    let mut pinned = create_req(&tok);
    pinned.name = Some("pinned".to_string());
    pinned.monitor = true;
    pinned.model = Some("opus".to_string());
    let resp = send_request(server.socket_path(), &pinned).unwrap();
    assert!(resp.ok, "{resp:?}");
    let a = mgr.get(&resp.id.unwrap()).expect("persisted");
    match &a.mode {
        Mode::Agent { model, effort, .. } => {
            assert_eq!(model.as_deref(), Some("opus"), "explicit model wins");
            assert_eq!(
                effort.as_deref(),
                Some(MONITOR_DEFAULT_EFFORT),
                "unspecified effort still backstopped per-field"
            );
        }
        other => panic!("expected agent mode, got {other:?}"),
    }
}

// Headless-agent-automations U2 (R1/R2/R12): the wire's tri-state `headless`
// persists onto the stored Mode::Agent (an explicit `--paned` false is an
// override that must survive), and the untrusted-wire rejections mirror the
// CLI's: any disposition pin with --monitor, or with a script, refuses.
#[test]
fn create_persists_headless_tri_state_and_rejects_bad_combinations() {
    use fly_lib::automations::model::Mode;
    let (mgr, _d) = manager();

    // --paned rides the wire and lands as Some(false).
    let mut req = create_req("tok");
    req.headless = Some(false);
    let paned = dispatch_automation_op(&mgr, 1, "ws", false, req, &no_pointers);
    assert!(paned.ok, "{paned:?}");
    let a = mgr.get(&paned.id.unwrap()).unwrap();
    match a.mode {
        Mode::Agent { headless, .. } => assert_eq!(headless, Some(false)),
        other => panic!("expected agent mode, got {other:?}"),
    }

    // A disposition pin on a monitor create refuses (redundant/contradictory).
    let mut req = create_req("tok");
    req.name = Some("mon".into());
    req.monitor = true;
    req.headless = Some(true);
    let resp = dispatch_automation_op(&mgr, 1, "ws", false, req, &|| Some(sock_pointers()));
    assert!(!resp.ok);
    assert!(resp.error.unwrap().contains("always headless"));

    // A disposition pin on a script create refuses (agent-only).
    let mut req = create_req("tok");
    req.name = Some("scripted".into());
    req.prompt = None;
    req.script = Some("echo hi".into());
    req.headless = Some(true);
    let resp = dispatch_automation_op(&mgr, 1, "ws", false, req, &no_pointers);
    assert!(!resp.ok);
    assert!(resp.error.unwrap().contains("agent-mode only"));
}

/// A timeout the dispatcher would silently clamp must be REFUSED at create.
///
/// The regression this pins, from 2026-08-07: a scheduled job was "fixed" by
/// recreating it with `--timeout 4500000` (75 min). `create` accepted it, the
/// store recorded it, and `show` printed it — three surfaces agreeing on a
/// number that `clamp_timeout_ms` then ignored at every dispatch, so the job
/// went on being killed at the old ceiling. A silently-unhonoured value is
/// worse than a rejected one: it reads as a fix and is not one.
#[test]
fn create_rejects_a_timeout_above_the_dispatch_ceiling() {
    use fly_lib::automations::model::Mode;
    use fly_lib::automations::script::TIMEOUT_MAX_MS;
    let (mgr, _d) = manager();
    let no_pointers = || None;

    let script_req = |ms: u64| {
        let mut req = create_req("tok");
        req.prompt = None;
        req.script = Some("echo hi".into());
        req.timeout_ms = Some(ms);
        req
    };

    // Over the ceiling: refused, and the message names both numbers so the
    // author can see the gap rather than guessing at it.
    let resp = dispatch_automation_op(&mgr, 1, "ws", false, script_req(TIMEOUT_MAX_MS + 1), &no_pointers);
    assert!(!resp.ok, "over-ceiling timeout must not be accepted: {resp:?}");
    let err = resp.error.unwrap();
    assert!(err.contains("exceeds the maximum"), "got: {err}");
    assert!(err.contains(&TIMEOUT_MAX_MS.to_string()), "message names the cap: {err}");

    // Exactly at the ceiling is accepted AND stored verbatim — the boundary is
    // honoured, not clamped, so "accepted" and "enforced" mean the same thing.
    let resp = dispatch_automation_op(&mgr, 1, "ws", false, script_req(TIMEOUT_MAX_MS), &no_pointers);
    assert!(resp.ok, "a timeout at the ceiling is valid: {resp:?}");
    let a = mgr.get(&resp.id.unwrap()).unwrap();
    match a.mode {
        Mode::Script { timeout_ms, .. } => assert_eq!(timeout_ms, TIMEOUT_MAX_MS),
        other => panic!("expected script mode, got {other:?}"),
    }
}
