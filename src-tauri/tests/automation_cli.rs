//! U9: the `fly automation` socket path end-to-end — the request handler's
//! routing, the R22 recursion gate, origin stamping, and the security boundary
//! (invalid token → no response), plus the AppHandle-free dispatch core.

use std::sync::Arc;

use fly_lib::automations::store::Store;
use fly_lib::automations::{AutomationManager, Dispatcher, UnwiredDispatcher};
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

/// A hook server whose automation handler routes to `mgr` via the real
/// (AppHandle-free) dispatch core, stamping a fixed workspace.
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
        dispatch_automation_op(&mgr_h, pane.0, "ws-test", is_recursion, req).to_bytes()
    });
    HookServer::start_with_handler(sock, Arc::clone(tokens), noop, Some(handler)).unwrap()
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
    );
    assert!(with.ok, "create with model/effort: {with:?}");
    let a = mgr.get(&with.id.unwrap()).unwrap();
    match a.mode {
        Mode::Agent {
            ref prompt,
            ref model,
            ref effort,
        } => {
            assert_eq!(prompt, "audit disk");
            assert_eq!(model.as_deref(), Some("opus"));
            assert_eq!(effort.as_deref(), Some("high"));
        }
        other => panic!("expected agent mode, got {other:?}"),
    }

    // Without them: the stored Mode::Agent carries None for both.
    let plain = dispatch_automation_op(&mgr, 1, "ws", false, create_req("tok"));
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
    let resp = dispatch_automation_op(&mgr, 1, "ws", false, create_req("tok"));
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
    );
    assert!(del.ok);
    assert!(mgr.get(&id).is_none(), "deleted");
}
