//! Local feed HTTP server (feat-agent-state-local-feed U3; feed-agent-reply-io
//! U4): the loopback endpoint is bearer-token authenticated; `/feed` streams
//! SSE snapshots, `/agents/{key}/output` serves the latest reply, and
//! `/agents/{key}/input` delivers a prompt. Mirrors `hook_auth.rs` for the
//! trust-boundary posture — a missing/wrong token is rejected silently with the
//! same bare 401 on every route, and the reply/input seams are injected fakes
//! so each route's status contract (2xx / 401 / 404 / 400) is pinned here.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fly_lib::feed::io::{ReplyResolver, ResolvedIo};
use fly_lib::feed::server::{FeedServer, InputOutcome, IoFn};
use fly_lib::feed::wire::{AgentEntry, QuestionBody, QuestionOption, QuestionSpec};
use fly_lib::feed::FeedState;
use fly_lib::session::transcript::LastReply;

const TOKEN: &str = "test-token-abc123";
const REPLIED_AT: u64 = 1_781_896_636_402;
const ASKED_AT: u64 = 1_781_896_640_000;

/// An IO resolver that knows three leaves: `leaf-replied` → a stamped reply,
/// `leaf-choice` → that reply + a pending choice question, `leaf-permission` →
/// a pending Bash permission question (exposure of which the server must gate
/// on the roster reason — feed-pending-question KTD3/KTD4).
fn fake_io() -> IoFn {
    Arc::new(|leaf_key| {
        let reply = || LastReply {
            text: "All tests pass.".into(),
            replied_at_ms: Some(REPLIED_AT),
        };
        match leaf_key {
            "leaf-replied" => ResolvedIo {
                reply: Some(reply()),
                question: None,
            },
            "leaf-choice" => ResolvedIo {
                reply: Some(reply()),
                question: Some(QuestionBody {
                    asked_at: ASKED_AT,
                    kind: "choice".into(),
                    tool: "AskUserQuestion".into(),
                    answerable: true,
                    context: Some("Pick one.".into()),
                    questions: vec![QuestionSpec {
                        question: "Which?".into(),
                        header: "Pick".into(),
                        multi_select: false,
                        options: vec![QuestionOption {
                            label: "Alpha".into(),
                            description: "first".into(),
                        }],
                    }],
                    request: None,
                }),
            },
            "leaf-permission" => ResolvedIo {
                reply: None,
                question: Some(QuestionBody {
                    asked_at: ASKED_AT,
                    kind: "permission".into(),
                    tool: "Bash".into(),
                    answerable: false,
                    context: None,
                    questions: vec![],
                    request: Some("cargo build".into()),
                }),
            },
            _ => ResolvedIo::default(),
        }
    })
}

/// Start a server whose input seam records deliveries into the returned log;
/// any leaf except `leaf-gone` (roster-listed but its pane just exited)
/// delivers.
fn start() -> (Arc<FeedState>, FeedServer, Arc<Mutex<Vec<(String, String)>>>) {
    let state = Arc::new(FeedState::new());
    let delivered: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&delivered);
    let server = FeedServer::start(
        0, // OS-chosen port
        TOKEN.to_string(),
        Arc::clone(&state),
        Arc::new(Vec::new), // no automations in these tests
        Arc::new(|| 42),    // fixed stamp
        fake_io(),
        Arc::new(move |leaf_key: &str, text: &str| {
            if leaf_key == "leaf-gone" {
                return InputOutcome::UnknownPane;
            }
            log.lock()
                .unwrap()
                .push((leaf_key.to_string(), text.to_string()));
            InputOutcome::Delivered
        }),
    )
    .unwrap();
    (state, server, delivered)
}

/// Send a raw HTTP/1.1 request and return (status line + headers, body read
/// within `read_for`). Uses a read timeout so an open SSE stream doesn't hang.
fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    auth: Option<&str>,
    body: Option<&str>,
    read_for: Duration,
) -> (String, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(read_for))
        .expect("set read timeout");
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\n");
    if let Some(a) = auth {
        req.push_str(&format!("Authorization: Bearer {a}\r\n"));
    }
    if let Some(b) = body {
        req.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            b.len()
        ));
    }
    req.push_str("Connection: close\r\n\r\n");
    if let Some(b) = body {
        req.push_str(b);
    }
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

fn get(addr: SocketAddr, path: &str, auth: Option<&str>, read_for: Duration) -> (String, String) {
    request(addr, "GET", path, auth, None, read_for)
}

fn post(addr: SocketAddr, path: &str, auth: Option<&str>, body: &str) -> (String, String) {
    request(addr, "POST", path, auth, Some(body), Duration::from_millis(400))
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
        last_reply_at: None,
        question_pending_at: None,
    }
}

/// An [`agent`] whose pushed roster entry carries an attention reason — the
/// corroboration signal the permission gate reads (feed-pending-question KTD3).
fn agent_with_reason(leaf: &str, status: &str, reason: &str) -> AgentEntry {
    AgentEntry {
        reason: Some(reason.into()),
        needs_attention: true,
        ..agent(leaf, status)
    }
}

#[test]
fn binds_loopback_only() {
    let (_state, server, _) = start();
    assert!(
        server.local_addr().ip().is_loopback(),
        "feed must bind loopback only, bound {}",
        server.local_addr()
    );
}

#[test]
fn healthz_needs_no_auth() {
    let (_state, server, _) = start();
    let (head, body) = get(server.local_addr(), "/healthz", None, Duration::from_millis(300));
    assert!(head.starts_with("HTTP/1.1 200"), "head was: {head}");
    assert!(body.contains("ok"));
}

#[test]
fn feed_without_token_is_rejected() {
    let (_state, server, _) = start();
    let (head, _body) = get(server.local_addr(), "/feed", None, Duration::from_millis(300));
    assert!(head.starts_with("HTTP/1.1 401"), "head was: {head}");
}

#[test]
fn feed_with_wrong_token_is_rejected() {
    let (_state, server, _) = start();
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
    let (state, server, _) = start();
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
    let (state, server, _) = start();
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

// ---- lastReplyAt on frames (feed-agent-reply-io U1/R3) ----------------------

#[test]
fn feed_frames_stamp_last_reply_at_from_the_resolver() {
    let (state, server, _) = start();
    // Two agents: one with a resolved reply, one that never replied. The
    // pushed roster carries no stamp — emit-time enrichment fills it.
    state.publish(vec![agent("leaf-replied", "waiting"), agent("l2", "working")]);
    let (_, body) = get(
        server.local_addr(),
        "/feed",
        Some(TOKEN),
        Duration::from_millis(400),
    );
    // The replied agent carries the resolver's stamp — the same value
    // /agents/leaf-replied/output serves as repliedAt (R3).
    assert!(
        body.contains(&format!("\"lastReplyAt\":{REPLIED_AT}")),
        "body was: {body}"
    );
    // The never-replied agent carries an explicit null.
    assert!(body.contains("\"lastReplyAt\":null"), "body was: {body}");
}

// ---- questionPendingAt + /output question (feed-pending-question U4) --------

#[test]
fn a_pending_choice_stamps_the_frame_and_matches_output_asked_at() {
    // R4's choice-kind invariant: the SSE marker and /output's askedAt are the
    // same resolver-cached value, on both surfaces, regardless of reason.
    let (state, server, _) = start();
    state.publish(vec![agent_with_reason("leaf-choice", "waiting", "question")]);
    let (_, sse) = get(
        server.local_addr(),
        "/feed",
        Some(TOKEN),
        Duration::from_millis(400),
    );
    assert!(
        sse.contains(&format!("\"questionPendingAt\":{ASKED_AT}")),
        "sse was: {sse}"
    );
    let (_, body) = get(
        server.local_addr(),
        "/agents/leaf-choice/output",
        Some(TOKEN),
        Duration::from_millis(300),
    );
    let v: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(v["question"]["askedAt"], ASKED_AT);
    assert_eq!(v["question"]["kind"], "choice");
    assert_eq!(v["question"]["answerable"], true);
    assert_eq!(v["question"]["questions"][0]["options"][0]["label"], "Alpha");
    // The reply half rides along unchanged (blessed duplication).
    assert_eq!(v["text"], "All tests pass.");
}

#[test]
fn a_pending_choice_on_a_working_row_is_still_exposed() {
    // KTD5's blessed consequence: `status: "working"` with a pending question
    // for up to ~IDLE_GAP_MS after the picker draws — not exclusive to waiting.
    let (state, server, _) = start();
    state.publish(vec![agent("leaf-choice", "working")]); // no reason at all
    let (_, sse) = get(
        server.local_addr(),
        "/feed",
        Some(TOKEN),
        Duration::from_millis(400),
    );
    assert!(
        sse.contains(&format!("\"questionPendingAt\":{ASKED_AT}")),
        "sse was: {sse}"
    );
}

#[test]
fn a_permission_question_is_gated_on_the_permission_reason() {
    // With the roster reason "permission" (fly backgrounded, pane raised) the
    // question reaches both surfaces…
    let (state, server, _) = start();
    state.publish(vec![agent_with_reason("leaf-permission", "waiting", "permission")]);
    let (_, sse) = get(
        server.local_addr(),
        "/feed",
        Some(TOKEN),
        Duration::from_millis(400),
    );
    assert!(
        sse.contains(&format!("\"questionPendingAt\":{ASKED_AT}")),
        "sse was: {sse}"
    );
    let (_, body) = get(
        server.local_addr(),
        "/agents/leaf-permission/output",
        Some(TOKEN),
        Duration::from_millis(300),
    );
    let v: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(v["question"]["kind"], "permission");
    assert_eq!(v["question"]["tool"], "Bash");
    assert_eq!(v["question"]["request"], "cargo build");

    // …and with no (or another) reason, the same pending tool_use means
    // "executing", so neither surface exposes it (KTD3).
    state.publish(vec![agent("leaf-permission", "working")]);
    let (_, sse) = get(
        server.local_addr(),
        "/feed",
        Some(TOKEN),
        Duration::from_millis(400),
    );
    assert!(
        sse.contains("\"questionPendingAt\":null"),
        "sse was: {sse}"
    );
    let (_, body) = get(
        server.local_addr(),
        "/agents/leaf-permission/output",
        Some(TOKEN),
        Duration::from_millis(300),
    );
    let v: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert!(v.get("question").is_none(), "body was: {body}");
}

#[test]
fn a_delegating_tool_pending_abstains_end_to_end() {
    // KTD3's confused-deputy rule through the REAL resolver: a sole pending
    // `Agent` tool_use (a subagent's inner dialog is what's on screen) is not
    // exposed even with reason "permission" — the transcript layer abstains,
    // so no marker and no question object reach either surface.
    let dir = tempfile::tempdir().unwrap();
    let resume_path = dir.path().join("resume.json");
    fly_lib::session::resume::upsert_at(
        &resume_path,
        "leaf-task",
        fly_lib::session::resume::ResumePartial {
            session_id: Some("sid-task".into()),
            session_cwd: Some("/p".into()),
            session_source: Some(fly_lib::session::resume::SessionSource::Hook),
            ..Default::default()
        },
    )
    .unwrap();
    let root = dir.path().join("projects");
    let project = root.join("-p");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("sid-task.jsonl"),
        concat!(
            r#"{"type":"assistant","timestamp":"2026-06-19T19:17:20.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Agent","input":{"prompt":"go"}}]}}"#,
            "\n"
        ),
    )
    .unwrap();
    let resolver = Arc::new(ReplyResolver::with_projects_root(resume_path, Some(root)));
    let io_fn: IoFn = Arc::new(move |leaf| resolver.resolve_io(leaf));

    let state = Arc::new(FeedState::new());
    let server = FeedServer::start(
        0,
        TOKEN.to_string(),
        Arc::clone(&state),
        Arc::new(Vec::new),
        Arc::new(|| 42),
        io_fn,
        Arc::new(|_: &str, _: &str| InputOutcome::Delivered),
    )
    .unwrap();
    state.publish(vec![agent_with_reason("leaf-task", "waiting", "permission")]);

    let (_, sse) = get(
        server.local_addr(),
        "/feed",
        Some(TOKEN),
        Duration::from_millis(400),
    );
    assert!(sse.contains("\"questionPendingAt\":null"), "sse was: {sse}");
    let (_, body) = get(
        server.local_addr(),
        "/agents/leaf-task/output",
        Some(TOKEN),
        Duration::from_millis(300),
    );
    let v: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert!(v.get("question").is_none(), "body was: {body}");
}

// ---- GET /agents/{key}/output (feed-agent-reply-io U4) ----------------------

#[test]
fn output_without_token_is_rejected() {
    let (state, server, _) = start();
    state.publish(vec![agent("leaf-replied", "idle")]);
    let (head, _) = get(
        server.local_addr(),
        "/agents/leaf-replied/output",
        None,
        Duration::from_millis(300),
    );
    assert!(head.starts_with("HTTP/1.1 401"), "head was: {head}");
}

#[test]
fn output_for_an_unpublished_key_is_404() {
    let (state, server, _) = start();
    state.publish(vec![agent("l1", "idle")]);
    let (head, _) = get(
        server.local_addr(),
        "/agents/leaf-ghost/output",
        Some(TOKEN),
        Duration::from_millis(300),
    );
    assert!(head.starts_with("HTTP/1.1 404"), "head was: {head}");
}

#[test]
fn output_serves_the_latest_reply_with_its_stamp() {
    let (state, server, _) = start();
    state.publish(vec![agent("leaf-replied", "waiting")]);
    let (head, body) = get(
        server.local_addr(),
        "/agents/leaf-replied/output",
        Some(TOKEN),
        Duration::from_millis(300),
    );
    assert!(head.starts_with("HTTP/1.1 200"), "head was: {head}");
    let v: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(v["text"], "All tests pass.");
    assert_eq!(v["repliedAt"], REPLIED_AT);
    // Regression guard (feed-pending-question R5): nothing pending → the
    // `question` key is absent, the reply shape byte-identical to before.
    assert!(v.get("question").is_none(), "body was: {body}");
}

#[test]
fn output_for_a_never_replied_agent_is_empty_text_200() {
    let (state, server, _) = start();
    state.publish(vec![agent("l1", "working")]);
    let (head, body) = get(
        server.local_addr(),
        "/agents/l1/output",
        Some(TOKEN),
        Duration::from_millis(300),
    );
    assert!(head.starts_with("HTTP/1.1 200"), "head was: {head}");
    let v: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(v["text"], "");
    // No stamp at all — the consumer requires a finite number when present.
    assert!(v.get("repliedAt").is_none(), "body was: {body}");
}

// ---- POST /agents/{key}/input (feed-agent-reply-io U4/U5) -------------------

#[test]
fn input_without_token_is_rejected_and_delivers_nothing() {
    let (state, server, delivered) = start();
    state.publish(vec![agent("l1", "waiting")]);
    let (head, _) = post(
        server.local_addr(),
        "/agents/l1/input",
        None,
        r#"{"text":"hello"}"#,
    );
    assert!(head.starts_with("HTTP/1.1 401"), "head was: {head}");
    assert!(delivered.lock().unwrap().is_empty());
}

#[test]
fn input_for_an_unpublished_key_is_404() {
    let (state, server, delivered) = start();
    state.publish(vec![agent("l1", "waiting")]);
    let (head, _) = post(
        server.local_addr(),
        "/agents/leaf-ghost/input",
        Some(TOKEN),
        r#"{"text":"hello"}"#,
    );
    assert!(head.starts_with("HTTP/1.1 404"), "head was: {head}");
    assert!(delivered.lock().unwrap().is_empty());
}

#[test]
fn input_delivers_and_acks_ok() {
    let (state, server, delivered) = start();
    state.publish(vec![agent("l1", "waiting")]);
    let (head, body) = post(
        server.local_addr(),
        "/agents/l1/input",
        Some(TOKEN),
        r#"{"text":"looks good, ship it"}"#,
    );
    assert!(head.starts_with("HTTP/1.1 200"), "head was: {head}");
    let v: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(v["ok"], true);
    assert_eq!(
        delivered.lock().unwrap().as_slice(),
        &[("l1".to_string(), "looks good, ship it".to_string())]
    );
}

#[test]
fn input_to_a_published_but_dead_pane_is_404() {
    // Roster listing races pane exit: the seam's UnknownPane maps to 404
    // ("not available"), never a 200 that silently dropped the text.
    let (state, server, delivered) = start();
    state.publish(vec![agent("leaf-gone", "idle")]);
    let (head, _) = post(
        server.local_addr(),
        "/agents/leaf-gone/input",
        Some(TOKEN),
        r#"{"text":"hello"}"#,
    );
    assert!(head.starts_with("HTTP/1.1 404"), "head was: {head}");
    assert!(delivered.lock().unwrap().is_empty());
}

#[test]
fn input_with_a_malformed_body_is_400() {
    let (state, server, delivered) = start();
    state.publish(vec![agent("l1", "waiting")]);
    for bad in [r#"{"no_text":1}"#, "not json", ""] {
        let (head, _) = post(server.local_addr(), "/agents/l1/input", Some(TOKEN), bad);
        assert!(head.starts_with("HTTP/1.1 400"), "body {bad:?} → head: {head}");
    }
    assert!(delivered.lock().unwrap().is_empty());
}

#[test]
fn input_with_the_wrong_method_is_405() {
    // GET on /input (and POST on /output) is a method error post-auth — the
    // route exists, the verb is wrong; unauthenticated callers still see 401.
    let (state, server, _) = start();
    state.publish(vec![agent("l1", "waiting")]);
    let (head, _) = get(
        server.local_addr(),
        "/agents/l1/input",
        Some(TOKEN),
        Duration::from_millis(300),
    );
    assert!(head.starts_with("HTTP/1.1 405"), "head was: {head}");
    let (head, _) = post(server.local_addr(), "/agents/l1/output", Some(TOKEN), "{}");
    assert!(head.starts_with("HTTP/1.1 405"), "head was: {head}");
}

#[test]
fn unknown_routes_are_401_without_a_token_and_404_with_one() {
    // Silent posture: route existence is not probeable unauthenticated.
    let (_state, server, _) = start();
    let (head, _) = get(server.local_addr(), "/nope", None, Duration::from_millis(300));
    assert!(head.starts_with("HTTP/1.1 401"), "head was: {head}");
    let (head, _) = get(
        server.local_addr(),
        "/nope",
        Some(TOKEN),
        Duration::from_millis(300),
    );
    assert!(head.starts_with("HTTP/1.1 404"), "head was: {head}");
}
