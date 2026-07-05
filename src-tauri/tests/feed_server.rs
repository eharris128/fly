//! Local feed SSE server (feat-agent-state-local-feed, U3): the read-only,
//! loopback-only endpoint is bearer-token authenticated and streams the feed
//! snapshot. Mirrors `hook_auth.rs` for the trust-boundary posture — a
//! missing/wrong token is rejected silently; a valid one streams frames.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use fly_lib::feed::server::FeedServer;
use fly_lib::feed::wire::AgentEntry;
use fly_lib::feed::FeedState;

const TOKEN: &str = "test-token-abc123";

fn start() -> (Arc<FeedState>, FeedServer) {
    let state = Arc::new(FeedState::new());
    let server = FeedServer::start(
        0, // OS-chosen port
        TOKEN.to_string(),
        Arc::clone(&state),
        Arc::new(Vec::new), // no automations in these tests
        Arc::new(|| 42),    // fixed stamp
    )
    .unwrap();
    (state, server)
}

/// Send a raw HTTP/1.1 GET and return (status line + headers, body-bytes read
/// within `read_for`). Uses a read timeout so an open SSE stream doesn't hang.
fn get(addr: SocketAddr, path: &str, auth: Option<&str>, read_for: Duration) -> (String, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(read_for))
        .expect("set read timeout");
    let mut req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n");
    if let Some(a) = auth {
        req.push_str(&format!("Authorization: Bearer {a}\r\n"));
    }
    req.push_str("Connection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).unwrap();

    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    // Read until the timeout trips (SSE never closes) or EOF.
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break, // timeout / would-block → done reading
        }
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    (head.to_string(), body.to_string())
}

fn agent(leaf: &str, status: &str) -> AgentEntry {
    AgentEntry {
        leaf_key: leaf.into(),
        workspace: "home".into(),
        tab: "fly".into(),
        cwd: None,
        status: status.into(),
        needs_attention: false,
        reason: None,
        working_for_ms: None,
        live_task_count: 0,
        num: None,
    }
}

#[test]
fn binds_loopback_only() {
    let (_state, server) = start();
    assert!(
        server.local_addr().ip().is_loopback(),
        "feed must bind loopback only, bound {}",
        server.local_addr()
    );
}

#[test]
fn healthz_needs_no_auth() {
    let (_state, server) = start();
    let (head, body) = get(server.local_addr(), "/healthz", None, Duration::from_millis(300));
    assert!(head.starts_with("HTTP/1.1 200"), "head was: {head}");
    assert!(body.contains("ok"));
}

#[test]
fn feed_without_token_is_rejected() {
    let (_state, server) = start();
    let (head, _body) = get(server.local_addr(), "/feed", None, Duration::from_millis(300));
    assert!(head.starts_with("HTTP/1.1 401"), "head was: {head}");
}

#[test]
fn feed_with_wrong_token_is_rejected() {
    let (_state, server) = start();
    let (head, _body) = get(
        server.local_addr(),
        "/feed",
        Some("wrong-token"),
        Duration::from_millis(300),
    );
    assert!(head.starts_with("HTTP/1.1 401"), "head was: {head}");
}

#[test]
fn feed_with_valid_token_streams_initial_frame() {
    let (state, server) = start();
    state.publish(vec![agent("l1", "working")]);
    let (head, body) = get(
        server.local_addr(),
        "/feed",
        Some(TOKEN),
        Duration::from_millis(400),
    );
    assert!(head.starts_with("HTTP/1.1 200"), "head was: {head}");
    assert!(
        head.to_lowercase().contains("text/event-stream"),
        "expected SSE content-type, head was: {head}"
    );
    // The initial frame carries the published agent as an SSE `data:` line.
    assert!(body.contains("data:"), "body was: {body}");
    assert!(body.contains("\"leafKey\":\"l1\""), "body was: {body}");
    assert!(body.contains("\"status\":\"working\""), "body was: {body}");
}

#[test]
fn feed_emits_a_new_frame_after_a_version_bump() {
    let (state, server) = start();
    state.publish(vec![agent("l1", "working")]);
    let addr = server.local_addr();

    // Read the stream on a background thread; bump the roster mid-read.
    let handle = std::thread::spawn(move || {
        get(addr, "/feed", Some(TOKEN), Duration::from_millis(600)).1
    });
    std::thread::sleep(Duration::from_millis(150));
    state.publish(vec![agent("l1", "idle")]); // a real change → new frame
    let body = handle.join().unwrap();

    // Both the initial (working) and the post-bump (idle) frames appear.
    assert!(body.contains("\"status\":\"working\""), "body was: {body}");
    assert!(body.contains("\"status\":\"idle\""), "body was: {body}");
}
