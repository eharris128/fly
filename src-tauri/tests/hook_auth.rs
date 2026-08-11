//! U8 hook authentication (R10): only authenticated, pane-scoped signals raise
//! attention; spoofing attempts are rejected.

use std::io::Write;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fly_lib::hooks::{Dispatch, HookServer, TokenRegistry, ValidatedHook};
use fly_lib::pty::PaneId;
use fly_lib::state::attention::Reason;

type Recorder = Arc<Mutex<Vec<(PaneId, ValidatedHook)>>>;

fn setup() -> (tempfile::TempDir, Arc<TokenRegistry>, Recorder, HookServer) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hook.sock");
    let tokens = Arc::new(TokenRegistry::new());
    let rec: Recorder = Arc::new(Mutex::new(Vec::new()));
    let rec2 = Arc::clone(&rec);
    let dispatch: Dispatch = Arc::new(move |pane, hook| {
        rec2.lock().unwrap().push((pane, hook));
    });
    let server = HookServer::start(path, Arc::clone(&tokens), dispatch).unwrap();
    (dir, tokens, rec, server)
}

fn send(path: &Path, json: &str) {
    if let Ok(mut s) = UnixStream::connect(path) {
        let _ = s.write_all(json.as_bytes());
        let _ = s.shutdown(Shutdown::Write); // EOF so the server's read returns
    }
}

fn wait_count(rec: &Recorder, n: usize, timeout: Duration) -> usize {
    let deadline = Instant::now() + timeout;
    loop {
        let c = rec.lock().unwrap().len();
        if c >= n || Instant::now() >= deadline {
            return c;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn valid_token_routes_to_the_right_pane() {
    let (_dir, tokens, rec, server) = setup();
    let tok = tokens.issue(PaneId(7));
    send(
        server.socket_path(),
        &format!(r#"{{"token":"{tok}","reason":"permission"}}"#),
    );
    assert_eq!(wait_count(&rec, 1, Duration::from_secs(2)), 1);
    let got = rec.lock().unwrap();
    assert_eq!(got[0].0, PaneId(7));
    assert_eq!(got[0].1.reason, Reason::Permission);
}

#[test]
fn unknown_token_is_rejected() {
    let (_dir, tokens, rec, server) = setup();
    tokens.issue(PaneId(1));
    send(
        server.socket_path(),
        r#"{"token":"00000000deadbeef","reason":"error"}"#,
    );
    assert_eq!(wait_count(&rec, 1, Duration::from_millis(400)), 0);
}

#[test]
fn malformed_messages_are_rejected_without_crashing() {
    let (_dir, _tokens, rec, server) = setup();
    send(server.socket_path(), "this is not json");
    send(server.socket_path(), r#"{"token":123,"reason":true}"#);
    send(server.socket_path(), "");
    assert_eq!(wait_count(&rec, 1, Duration::from_millis(400)), 0);
}

#[test]
fn revoked_pane_token_is_rejected() {
    let (_dir, tokens, rec, server) = setup();
    let tok = tokens.issue(PaneId(3));
    tokens.revoke(PaneId(3));
    send(
        server.socket_path(),
        &format!(r#"{{"token":"{tok}","reason":"finished"}}"#),
    );
    assert_eq!(wait_count(&rec, 1, Duration::from_millis(400)), 0);
}

#[test]
fn two_panes_tokens_never_cross_map() {
    let (_dir, tokens, rec, server) = setup();
    let t1 = tokens.issue(PaneId(1));
    let t2 = tokens.issue(PaneId(2));
    send(
        server.socket_path(),
        &format!(r#"{{"token":"{t1}","reason":"permission"}}"#),
    );
    send(
        server.socket_path(),
        &format!(r#"{{"token":"{t2}","reason":"finished"}}"#),
    );
    assert_eq!(wait_count(&rec, 2, Duration::from_secs(2)), 2);
    let got = rec.lock().unwrap();
    assert!(got
        .iter()
        .any(|(p, h)| *p == PaneId(1) && h.reason == Reason::Permission));
    assert!(got
        .iter()
        .any(|(p, h)| *p == PaneId(2) && h.reason == Reason::Finished));
}

#[test]
fn concurrent_callbacks_are_handled() {
    let (_dir, tokens, rec, server) = setup();
    let tok = tokens.issue(PaneId(5));
    let path = server.socket_path().to_path_buf();
    let mut handles = Vec::new();
    for _ in 0..20 {
        let p = path.clone();
        let t = tok.clone();
        handles.push(std::thread::spawn(move || {
            send(&p, &format!(r#"{{"token":"{t}","reason":"question"}}"#));
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(wait_count(&rec, 20, Duration::from_secs(3)), 20);
}

#[test]
fn socket_file_is_removed_on_shutdown() {
    let (_dir, _tokens, _rec, mut server) = setup();
    let path = server.socket_path().to_path_buf();
    assert!(path.exists());
    server.shutdown();
    assert!(!path.exists());
}

// ---- capture-only messages (fix-session-pane-attribution U2, KTD1/KTD2) ----

#[test]
fn capture_gates_thread_through_the_authenticated_path() {
    // Both capture gates reach the dispatch so lib.rs can short-circuit before
    // the attention machine (R2): the explicit --capture flag, and — for a
    // stale installed binary that forwards the event without the flag — a
    // SessionStart hook_event. A raising reason on the same message is ignored
    // by is_capture_only (KTD1).
    // Single-line payloads: the wire contract is compact JSON (hook-ask-channel
    // U2 — the request read stops at a newline for the held-ask framing, so an
    // embedded newline truncates the message).
    let (_dir, tokens, rec, server) = setup();
    let tok = tokens.issue(PaneId(4));
    send(
        server.socket_path(),
        &format!(
            r#"{{"token":"{tok}","reason":"question","capture_only":true,"session_id":"sess-flag","cwd":"/proj"}}"#
        ),
    );
    send(
        server.socket_path(),
        &format!(
            r#"{{"token":"{tok}","reason":"permission","hook_event":"SessionStart","session_id":"sess-event","cwd":"/proj"}}"#
        ),
    );
    assert_eq!(wait_count(&rec, 2, Duration::from_secs(2)), 2);
    let got = rec.lock().unwrap();
    let flagged = got
        .iter()
        .find(|(_, h)| h.session_id.as_deref() == Some("sess-flag"))
        .expect("--capture message dispatched");
    assert!(flagged.1.capture_only);
    assert!(flagged.1.is_capture_only());
    let evented = got
        .iter()
        .find(|(_, h)| h.session_id.as_deref() == Some("sess-event"))
        .expect("SessionStart message dispatched");
    assert!(!evented.1.capture_only, "no flag from a stale binary");
    assert!(evented.1.is_capture_only(), "the event name still gates");
}

#[test]
fn normal_messages_are_not_capture_only() {
    // A plain Notification/Stop raises exactly as before the fix.
    let (_dir, tokens, rec, server) = setup();
    let tok = tokens.issue(PaneId(9));
    send(
        server.socket_path(),
        &format!(r#"{{"token":"{tok}","reason":"finished","hook_event":"Stop"}}"#),
    );
    assert_eq!(wait_count(&rec, 1, Duration::from_secs(2)), 1);
    assert!(!rec.lock().unwrap()[0].1.is_capture_only());
}

#[test]
fn capture_message_with_bad_token_is_rejected() {
    // Auth unchanged: capture rides the same trust boundary as every message.
    let (_dir, tokens, rec, server) = setup();
    tokens.issue(PaneId(1));
    send(
        server.socket_path(),
        r#"{"token":"00000000deadbeef","capture_only":true,"reason":"question","session_id":"s"}"#,
    );
    assert_eq!(wait_count(&rec, 1, Duration::from_millis(400)), 0);
}

#[test]
fn a_wire_session_source_field_is_inert() {
    // KTD2: rank is call-site-assigned, never read from the wire. A message
    // injecting "session_source" still dispatches (serde ignores unknown
    // fields) and there is no field for it to land in — ValidatedHook carries
    // no source, so the dispatch's Hook stamp is the only rank a socket write
    // can ever get.
    let (_dir, tokens, rec, server) = setup();
    let tok = tokens.issue(PaneId(6));
    send(
        server.socket_path(),
        &format!(
            r#"{{"token":"{tok}","reason":"question","session_id":"sess-forged","session_source":"pick","sessionSource":"pick"}}"#
        ),
    );
    assert_eq!(wait_count(&rec, 1, Duration::from_secs(2)), 1);
    let got = rec.lock().unwrap();
    assert_eq!(got[0].1.session_id.as_deref(), Some("sess-forged"));
}

// ---- request framing (hook-ask-channel U2/R2) -------------------------------

#[test]
fn a_newline_terminated_notify_message_still_dispatches() {
    // The held-ask framing tolerance applies to every op: a client that
    // terminates its compact JSON with \n (instead of closing its write half)
    // dispatches identically.
    let (_dir, tokens, rec, server) = setup();
    let tok = tokens.issue(PaneId(7));
    send(
        server.socket_path(),
        &format!("{{\"token\":\"{tok}\",\"reason\":\"finished\"}}\n"),
    );
    assert_eq!(wait_count(&rec, 1, Duration::from_secs(2)), 1);
}

#[test]
fn a_multi_line_message_is_rejected_silently() {
    // The one-line contract: an embedded newline truncates the message at the
    // frame boundary → malformed → silent reject (the pinned trade-off for
    // held-connection framing; every fly client sends compact JSON).
    let (_dir, tokens, rec, server) = setup();
    let tok = tokens.issue(PaneId(8));
    send(
        server.socket_path(),
        &format!("{{\"token\":\"{tok}\",\n\"reason\":\"finished\"}}"),
    );
    assert_eq!(wait_count(&rec, 1, Duration::from_millis(400)), 0);
}

#[test]
fn repeated_invalid_tokens_trip_a_lockout() {
    // Registry-level: deterministic, no socket timing.
    let tokens = TokenRegistry::new();
    let valid = tokens.issue(PaneId(1));
    assert_eq!(tokens.validate(&valid), Some(PaneId(1)));
    for _ in 0..50 {
        assert_eq!(tokens.validate("badtoken"), None);
    }
    assert!(tokens.is_locked());
    // While locked, even the valid token is refused.
    assert_eq!(tokens.validate(&valid), None);
}

// Audit-remediation U6/KTD6: concurrent connection threads are bounded. With
// every slot held by an idle connection, a further (valid!) notify is dropped
// before any read — never dispatched; once slots free (peer close → EOF →
// handler exit), the same notify dispatches again.
#[test]
fn over_cap_connections_are_dropped_and_slots_free_on_close() {
    use fly_lib::hooks::server::MAX_CONNECTIONS;
    let (_dir, tokens, rec, server) = setup();
    let tok = tokens.issue(PaneId(7));
    let msg = format!(r#"{{"token":"{tok}","reason":"permission"}}"#);

    // Fill the cap with idle connections (a slot is claimed on accept, before
    // any read, so these hold slots while sending nothing).
    let mut idle: Vec<UnixStream> = (0..MAX_CONNECTIONS)
        .map(|_| UnixStream::connect(server.socket_path()).unwrap())
        .collect();
    // Let the accept loop drain its backlog and claim every slot.
    std::thread::sleep(Duration::from_millis(400));

    // Over-cap: dropped pre-read, so a valid message never dispatches.
    send(server.socket_path(), &msg);
    assert_eq!(
        wait_count(&rec, 1, Duration::from_millis(700)),
        0,
        "over-cap connection must be dropped before dispatch"
    );

    // Close the idle peers → their handlers see EOF, reject silently, and the
    // RAII slots free → the same message now dispatches.
    idle.clear();
    let deadline = Instant::now() + Duration::from_secs(10);
    while rec.lock().unwrap().is_empty() && Instant::now() < deadline {
        send(server.socket_path(), &msg);
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        !rec.lock().unwrap().is_empty(),
        "a freed slot must admit and dispatch a valid notify"
    );
}

// ---- peer ops (agent-peer-messaging U1) -------------------------------------
// The `peer/*` request/response family rides the same boundary as everything
// else: token-validated before any op work, silent on auth failure, bounded.

use fly_lib::hooks::PeerHandler;

type PeerCalls = Arc<Mutex<Vec<(PaneId, Vec<u8>)>>>;

/// A server with a recording peer handler wired (the automation-handler test
/// shape): every authenticated peer op is logged with its token-resolved pane
/// and answered with a canned ok line.
fn setup_with_peer() -> (
    tempfile::TempDir,
    Arc<TokenRegistry>,
    Recorder,
    PeerCalls,
    HookServer,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hook.sock");
    let tokens = Arc::new(TokenRegistry::new());
    let rec: Recorder = Arc::new(Mutex::new(Vec::new()));
    let rec2 = Arc::clone(&rec);
    let dispatch: Dispatch = Arc::new(move |pane, hook| {
        rec2.lock().unwrap().push((pane, hook));
    });
    let calls: PeerCalls = Arc::new(Mutex::new(Vec::new()));
    let calls2 = Arc::clone(&calls);
    let peer: PeerHandler = Arc::new(move |pane, buf: &[u8]| {
        calls2.lock().unwrap().push((pane, buf.to_vec()));
        b"{\"ok\":true}".to_vec()
    });
    let server =
        HookServer::start_full(path, Arc::clone(&tokens), dispatch, None, None, Some(peer))
            .unwrap();
    (dir, tokens, rec, calls, server)
}

/// One request/response round-trip: write, half-close, read to EOF (bounded).
fn request(path: &Path, json: &str) -> Vec<u8> {
    use std::io::Read;
    let mut s = UnixStream::connect(path).unwrap();
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    s.write_all(json.as_bytes()).unwrap();
    s.shutdown(Shutdown::Write).unwrap();
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    buf
}

#[test]
fn peer_ops_reject_invalid_tokens_silently_and_count_toward_lockout() {
    let (_dir, tokens, _rec, calls, server) = setup_with_peer();
    let _valid = tokens.issue(PaneId(1));
    // A bad token gets no response bytes at all — indistinguishable from a
    // dead socket — and never reaches the handler.
    let resp = request(
        server.socket_path(),
        r#"{"token":"0000deadbeef","op":"peer/list"}"#,
    );
    assert!(resp.is_empty(), "bad token must be silent");
    assert!(calls.lock().unwrap().is_empty(), "handler never invoked");
    // Repeated bad presentations trip the same registry-wide lockout the
    // notify path faces (the validate call is shared, pre-op).
    for _ in 0..60 {
        let _ = request(
            server.socket_path(),
            r#"{"token":"0000deadbeef","op":"peer/list"}"#,
        );
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while !tokens.is_locked() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(tokens.is_locked(), "peer-op failures count toward lockout");
}

#[test]
fn peer_send_origin_is_the_token_resolved_pane_not_the_wire() {
    let (_dir, tokens, _rec, calls, server) = setup_with_peer();
    let tok = tokens.issue(PaneId(7));
    // The payload tries to smuggle a different origin; unknown fields are
    // ignored and the handler receives the token's pane (KTD2).
    let resp = request(
        server.socket_path(),
        &format!(
            r#"{{"token":"{tok}","op":"peer/send","pane":12,"message":"m","from":999,"origin":999}}"#
        ),
    );
    assert_eq!(resp, b"{\"ok\":true}");
    let got = calls.lock().unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0, PaneId(7), "origin is the authenticated token's pane");
}

#[test]
fn peer_ops_respect_the_message_bound() {
    let (_dir, tokens, _rec, calls, server) = setup_with_peer();
    let tok = tokens.issue(PaneId(7));
    // Over MAX_MESSAGE (64 KiB): rejected silently by the shared read path,
    // before any op work.
    let big = "x".repeat(80 * 1024);
    let resp = request(
        server.socket_path(),
        &format!(r#"{{"token":"{tok}","op":"peer/send","pane":12,"message":"{big}"}}"#),
    );
    assert!(resp.is_empty(), "oversized request must be silent");
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn peer_ops_without_a_wired_handler_are_dropped_silently() {
    // The behavioral half of the skew rule (agent-peer-messaging KTD1): a
    // server with no peer handler — the old-server shape — neither answers a
    // peer op nor lets it fall through to the notify dispatch (the payload
    // carries no `reason`, so even the fallthrough parse would fail; here the
    // routing returns first).
    let (_dir, tokens, rec, server) = setup();
    let tok = tokens.issue(PaneId(7));
    let resp = request(
        server.socket_path(),
        &format!(r#"{{"token":"{tok}","op":"peer/send","pane":12,"message":"m"}}"#),
    );
    assert!(resp.is_empty(), "no handler → no response");
    assert_eq!(
        wait_count(&rec, 1, Duration::from_millis(400)),
        0,
        "a peer op must never dispatch as notify"
    );
}

/// tmux-substrate U4b/KTD12: `substrate/event` ops route to the substrate
/// handler (which owns the server-scope token check) and never to the pane
/// path; an invalid substrate token is silently rejected AND coupled into the
/// registry's lockout counting; with no handler wired the op falls through to
/// notify, where it dies on the missing `reason` — never a dispatch.
#[test]
fn substrate_events_route_validate_and_couple_lockout() {
    use fly_lib::hooks::SubstrateHandler;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hook.sock");
    let tokens = Arc::new(TokenRegistry::new());
    let rec: Recorder = Arc::new(Mutex::new(Vec::new()));
    let rec2 = Arc::clone(&rec);
    let dispatch: Dispatch = Arc::new(move |pane, hook| {
        rec2.lock().unwrap().push((pane, hook));
    });
    let seen: Arc<Mutex<Vec<(String, bool)>>> = Arc::new(Mutex::new(Vec::new()));
    let seen2 = Arc::clone(&seen);
    const GOOD: &str = "feedfacefeedfacefeedfacefeedfacefeedfacefeedfacefeedfacefeedface";
    let handler: SubstrateHandler = Arc::new(move |buf: &[u8]| {
        let ev: fly_lib::hooks::protocol::SubstrateEvent =
            serde_json::from_slice(buf).unwrap();
        let ok = ev.token == GOOD; // test double; production is constant-time
        seen2.lock().unwrap().push((ev.session, ok));
        ok
    });
    let server = HookServer::start_all(
        path.clone(),
        Arc::clone(&tokens),
        dispatch,
        None,
        None,
        None,
        Some(handler),
    )
    .unwrap();

    // Valid substrate token: reaches the handler, never the dispatch.
    send(
        &path,
        &format!(
            r#"{{"token":"{GOOD}","op":"substrate/event","kind":"pane-died","session":"fly-fly-a","status":7}}"#
        ),
    );
    // Invalid token: handler says no; the failure must count toward lockout.
    for _ in 0..60 {
        send(
            &path,
            r#"{"token":"wrong","op":"substrate/event","kind":"pane-died","session":"fly-fly-a","status":1}"#,
        );
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while seen.lock().unwrap().len() < 61 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let calls = seen.lock().unwrap().clone();
    assert!(calls.len() >= 61, "handler saw all events, got {}", calls.len());
    // Connection threads race, so assert by content, not position.
    assert_eq!(
        calls.iter().filter(|(_, ok)| *ok).count(),
        1,
        "exactly the one valid token validated"
    );
    // The 60 coupled failures tripped the registry lockout (MAX_FAILURES=50):
    // a real pane token must now be rejected during the cooldown.
    let pane_token = tokens.issue(PaneId(9));
    send(
        &path,
        &format!(r#"{{"token":"{pane_token}","op":"notify","reason":"permission"}}"#),
    );
    assert_eq!(
        wait_count(&rec, 1, Duration::from_millis(600)),
        0,
        "registry lockout applies after coupled substrate failures"
    );
    assert!(rec.lock().unwrap().is_empty());
    drop(server);
}
