//! U8 hook authentication (R10): only authenticated, pane-scoped signals raise
//! attention; spoofing attempts are rejected.

use std::io::Write;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fly_lib::hooks::{Dispatch, HookServer, TokenRegistry};
use fly_lib::pty::PaneId;
use fly_lib::state::attention::Reason;

type Recorder = Arc<Mutex<Vec<(PaneId, Reason)>>>;

fn setup() -> (tempfile::TempDir, Arc<TokenRegistry>, Recorder, HookServer) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hook.sock");
    let tokens = Arc::new(TokenRegistry::new());
    let rec: Recorder = Arc::new(Mutex::new(Vec::new()));
    let rec2 = Arc::clone(&rec);
    let dispatch: Dispatch = Arc::new(move |pane, hook| {
        rec2.lock().unwrap().push((pane, hook.reason));
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
    assert_eq!(rec.lock().unwrap()[0], (PaneId(7), Reason::Permission));
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
    let got = rec.lock().unwrap().clone();
    assert!(got.contains(&(PaneId(1), Reason::Permission)));
    assert!(got.contains(&(PaneId(2), Reason::Finished)));
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
