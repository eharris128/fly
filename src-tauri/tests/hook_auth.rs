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
    let (_dir, tokens, rec, server) = setup();
    let tok = tokens.issue(PaneId(4));
    send(
        server.socket_path(),
        &format!(
            r#"{{"token":"{tok}","reason":"question","capture_only":true,
                 "session_id":"sess-flag","cwd":"/proj"}}"#
        ),
    );
    send(
        server.socket_path(),
        &format!(
            r#"{{"token":"{tok}","reason":"permission","hook_event":"SessionStart",
                 "session_id":"sess-event","cwd":"/proj"}}"#
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
            r#"{{"token":"{tok}","reason":"question","session_id":"sess-forged",
                 "session_source":"pick","sessionSource":"pick"}}"#
        ),
    );
    assert_eq!(wait_count(&rec, 1, Duration::from_secs(2)), 1);
    let got = rec.lock().unwrap();
    assert_eq!(got[0].1.session_id.as_deref(), Some("sess-forged"));
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
