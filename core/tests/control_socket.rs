//! Control-socket integration coverage (Electron-shell migration U1):
//! never-steal bind, request/response, event + pane-output fan-out, the
//! pane-input seam, and fail-closed framing. The same-uid gate can't be
//! negatively tested here (every test process is our uid); it reuses the
//! hook socket's `peer_uid_matches`, which `hook_auth` exercises.

use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fly_lib::control::server::ping_handler;
use fly_lib::control::{ControlServer, Frame};
use fly_lib::control::frame::{read_frame, write_frame};

fn temp_socket(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("fly-control-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir.join("control.sock")
}

fn request(stream: &mut UnixStream, id: u64, cmd: &str) -> serde_json::Value {
    let body = serde_json::to_vec(&serde_json::json!({"id": id, "cmd": cmd})).unwrap();
    write_frame(stream, &Frame::Json(body)).unwrap();
    match read_frame(stream).unwrap().unwrap() {
        Frame::Json(b) => serde_json::from_slice(&b).unwrap(),
        other => panic!("expected JSON response, got {other:?}"),
    }
}

#[test]
fn ping_answers_and_unknown_cmd_errs() {
    let path = temp_socket("ping");
    let server = ControlServer::start(path.clone(), ping_handler(), None).unwrap();
    let mut c = UnixStream::connect(&path).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

    let v = request(&mut c, 1, "core/ping");
    assert_eq!(v["id"], 1);
    assert_eq!(v["ok"]["pong"], true);
    assert!(v["ok"]["version"].as_str().unwrap().contains('.'));

    let v = request(&mut c, 2, "no/such");
    assert_eq!(v["id"], 2);
    assert!(v["err"].as_str().unwrap().contains("unknown command"));
    drop(server);
}

#[test]
fn live_socket_is_never_stolen_dead_one_is_reclaimed() {
    let path = temp_socket("steal");
    let first = ControlServer::start(path.clone(), ping_handler(), None).unwrap();
    // Live: second bind must refuse.
    let err = match ControlServer::start(path.clone(), ping_handler(), None) {
        Err(e) => e,
        Ok(_) => panic!("second bind on a live socket must refuse"),
    };
    assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
    drop(first); // clean shutdown unlinks…
    // …but simulate crash residue: a dead socket file at the path.
    let residue = std::os::unix::net::UnixListener::bind(&path).unwrap();
    drop(residue); // listener gone, file remains
    assert!(path.exists());
    let second = ControlServer::start(path.clone(), ping_handler(), None).unwrap();
    drop(second);
}

#[test]
fn events_and_pane_output_fan_out_to_all_clients() {
    let path = temp_socket("fanout");
    let server = ControlServer::start(path.clone(), ping_handler(), None).unwrap();
    let mut a = UnixStream::connect(&path).unwrap();
    let mut b = UnixStream::connect(&path).unwrap();
    for c in [&mut a, &mut b] {
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    }
    // Round-trip a ping on each so both registrations are complete before
    // broadcasting (accept is async to connect()).
    request(&mut a, 1, "core/ping");
    request(&mut b, 1, "core/ping");

    server.broadcast_event("pane://attention", serde_json::json!({"paneId": 3}));
    let raw = vec![0x00u8, 0x1b, 0xff, b'!'];
    server.broadcast_pane_output(9, &raw);

    for c in [&mut a, &mut b] {
        match read_frame(c).unwrap().unwrap() {
            Frame::Json(body) => {
                let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(v["event"], "pane://attention");
                assert_eq!(v["payload"]["paneId"], 3);
            }
            other => panic!("expected event, got {other:?}"),
        }
        match read_frame(c).unwrap().unwrap() {
            Frame::PaneOutput { pane, bytes } => {
                assert_eq!(pane, 9);
                assert_eq!(bytes, raw); // byte-exact incl. NUL/ESC/high bytes
            }
            other => panic!("expected pane output, got {other:?}"),
        }
    }
    drop(server);
}

#[test]
fn pane_input_frames_reach_the_seam() {
    let path = temp_socket("input");
    let seen: Arc<Mutex<Vec<(u64, Vec<u8>)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = {
        let seen = Arc::clone(&seen);
        Arc::new(move |pane: u64, bytes: &[u8]| {
            seen.lock().unwrap().push((pane, bytes.to_vec()));
        })
    };
    let server = ControlServer::start(path.clone(), ping_handler(), Some(sink)).unwrap();
    let mut c = UnixStream::connect(&path).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let keystrokes = vec![0x1b, b'[', b'A', 0x00];
    write_frame(&mut c, &Frame::PaneInput { pane: 4, bytes: keystrokes.clone() }).unwrap();
    // Pane input has no response; use a ping barrier to know it was consumed
    // (frames on one connection are processed in order).
    request(&mut c, 1, "core/ping");
    assert_eq!(seen.lock().unwrap().as_slice(), &[(4, keystrokes)]);
    drop(server);
}

#[test]
fn malformed_json_drops_the_connection_but_not_the_server() {
    let path = temp_socket("badjson");
    let server = ControlServer::start(path.clone(), ping_handler(), None).unwrap();
    let mut bad = UnixStream::connect(&path).unwrap();
    bad.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write_frame(&mut bad, &Frame::Json(b"not json".to_vec())).unwrap();
    // Server hangs up on the violator…
    assert!(matches!(read_frame(&mut bad), Ok(None) | Err(_)));
    // …while a well-behaved client is unaffected.
    let mut good = UnixStream::connect(&path).unwrap();
    good.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let v = request(&mut good, 1, "core/ping");
    assert_eq!(v["ok"]["pong"], true);
    drop(server);
}

#[test]
fn client_sent_pane_output_is_a_protocol_violation() {
    let path = temp_socket("wrongway");
    let server = ControlServer::start(path.clone(), ping_handler(), None).unwrap();
    let mut c = UnixStream::connect(&path).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write_frame(&mut c, &Frame::PaneOutput { pane: 1, bytes: vec![b'x'] }).unwrap();
    assert!(matches!(read_frame(&mut c), Ok(None) | Err(_)));
    drop(server);
}

#[test]
fn shutdown_unlinks_the_socket() {
    let path = temp_socket("shutdown");
    let mut server = ControlServer::start(path.clone(), ping_handler(), None).unwrap();
    assert!(path.exists());
    server.shutdown();
    assert!(!path.exists());
}
