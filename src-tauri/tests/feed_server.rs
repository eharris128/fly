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
use fly_lib::feed::server::{FeedServer, InputAction, InputOutcome, IoFn};
use fly_lib::feed::wire::{AgentEntry, QuestionBody, QuestionOption, QuestionSpec, TurnEntry};
use fly_lib::feed::FeedState;
use fly_lib::session::transcript::LastReply;

const TOKEN: &str = "test-token-abc123";
const REPLIED_AT: u64 = 1_781_896_636_402;
const ASKED_AT: u64 = 1_781_896_640_000;
const PROMPTED_AT: u64 = 1_781_896_600_000;

/// An IO resolver that knows three leaves: `leaf-replied` → a stamped reply
/// with a two-turn conversation tail, `leaf-choice` → that reply + a pending
/// choice question, `leaf-permission` → a pending Bash permission question
/// (exposure of which the server must gate on the roster reason —
/// feed-pending-question KTD3/KTD4).
fn fake_io() -> IoFn {
    Arc::new(|leaf_key, _reason| {
        let reply = || LastReply {
            text: "All tests pass.".into(),
            replied_at_ms: Some(REPLIED_AT),
        };
        // The tail ends with the reply's own turn (feed-conversation-tail R3).
        let turns = || {
            vec![
                TurnEntry {
                    role: "user".into(),
                    at: PROMPTED_AT,
                    text: "run the tests".into(),
                },
                TurnEntry {
                    role: "agent".into(),
                    at: REPLIED_AT,
                    text: "All tests pass.".into(),
                },
            ]
        };
        match leaf_key {
            "leaf-replied" => ResolvedIo {
                reply: Some(reply()),
                question: None,
                turns: turns(),
                pending_fallback_at: None,
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
                            key: "1".into(),
                            label: "Alpha".into(),
                            description: "first".into(),
                        }],
                        // The free-text row's digit: 1 authored option + 1.
                        other_key: Some("2".into()),
                    }],
                    request: None,
                    source: None,
                }),
                turns: Vec::new(),
                pending_fallback_at: None,
            },
            // An answerable choice whose free-text digit is unknown (an older
            // render, or a >9-option ask): keys-answerable, never
            // other-answerable (feed-other-answer R4).
            "leaf-choice-no-other" => ResolvedIo {
                reply: None,
                question: Some(QuestionBody {
                    asked_at: ASKED_AT,
                    kind: "choice".into(),
                    tool: "AskUserQuestion".into(),
                    answerable: true,
                    context: None,
                    questions: vec![QuestionSpec {
                        question: "Which?".into(),
                        header: String::new(),
                        multi_select: false,
                        options: vec![QuestionOption {
                            key: "1".into(),
                            label: "Alpha".into(),
                            description: String::new(),
                        }],
                        other_key: None,
                    }],
                    request: None,
                    source: None,
                }),
                turns: Vec::new(),
                pending_fallback_at: None,
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
                    source: None,
                }),
                turns: Vec::new(),
                pending_fallback_at: None,
            },
            // A screen-derived choice (feed-question-screen-fallback): the
            // fallback resolver synthesized the body from the pane's rendered
            // picker; askedAt is the raise stamp and the tier-1 marker rides
            // alongside it.
            "leaf-screen-choice" => ResolvedIo {
                reply: None,
                question: Some(QuestionBody {
                    asked_at: ASKED_AT,
                    kind: "choice".into(),
                    tool: "AskUserQuestion".into(),
                    answerable: true,
                    context: None,
                    questions: vec![QuestionSpec {
                        question: "Which color?".into(),
                        header: "Color".into(),
                        multi_select: false,
                        options: vec![QuestionOption {
                            key: "1".into(),
                            label: "Red".into(),
                            description: String::new(),
                        }],
                        // Screen bodies read the digit off the rendered
                        // "Type something." row (feed-other-answer R3).
                        other_key: Some("3".into()),
                    }],
                    request: None,
                    source: Some("screen".into()),
                }),
                turns: Vec::new(),
                pending_fallback_at: Some(ASKED_AT),
            },
            // Tier-1 degrade (feed-question-screen-fallback R2): the pane is
            // corroborated waiting but the screen parse abstained — pending
            // signal only, no body.
            "leaf-tier1" => ResolvedIo {
                reply: None,
                question: None,
                turns: Vec::new(),
                pending_fallback_at: Some(ASKED_AT),
            },
            _ => ResolvedIo::default(),
        }
    })
}

/// Start a server whose input seam records deliveries into the returned log
/// as `("leaf", "submit:<text>" | "keys:<bytes>")`; any leaf except
/// `leaf-gone` (roster-listed but its pane just exited) delivers. Keys-mode
/// permission answering follows `allow_permission_answers` (the KTD6 opt-in).
fn start_with(
    allow_permission_answers: bool,
) -> (Arc<FeedState>, FeedServer, Arc<Mutex<Vec<(String, String)>>>) {
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
        Arc::new(move |leaf_key: &str, action: InputAction| {
            if leaf_key == "leaf-gone" {
                return InputOutcome::UnknownPane;
            }
            let describe = match &action {
                InputAction::Submit(text) => format!("submit:{text}"),
                InputAction::Keys(bytes) => {
                    format!("keys:{}", String::from_utf8_lossy(bytes))
                }
                InputAction::Other { select, text } => format!(
                    "other:{}+{}",
                    String::from_utf8_lossy(select),
                    String::from_utf8_lossy(text)
                ),
            };
            log.lock().unwrap().push((leaf_key.to_string(), describe));
            InputOutcome::Delivered
        }),
        Arc::new(move || allow_permission_answers),
    )
    .unwrap();
    (state, server, delivered)
}

fn start() -> (Arc<FeedState>, FeedServer, Arc<Mutex<Vec<(String, String)>>>) {
    start_with(false)
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
    let io_fn: IoFn = Arc::new(move |leaf, _reason| resolver.resolve_io(leaf));

    let state = Arc::new(FeedState::new());
    let server = FeedServer::start(
        0,
        TOKEN.to_string(),
        Arc::clone(&state),
        Arc::new(Vec::new),
        Arc::new(|| 42),
        io_fn,
        Arc::new(|_: &str, _: InputAction| InputOutcome::Delivered),
        Arc::new(|| true), // even fully opted in, the abstention holds
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

// ---- screen fallback surfaces (feed-question-screen-fallback U6) ------------

#[test]
fn tier1_pending_marker_rides_the_frame_without_a_body() {
    // R2 two-tier degrade: the resolver stamped pending_fallback_at but
    // synthesized no body (screen abstained) → the SSE marker still surfaces,
    // and /output has no question key (R8).
    let (state, server, _) = start();
    state.publish(vec![agent_with_reason("leaf-tier1", "waiting", "question")]);
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
        "/agents/leaf-tier1/output",
        Some(TOKEN),
        Duration::from_millis(300),
    );
    let v: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert!(v.get("question").is_none(), "body was: {body}");
}

#[test]
fn a_screen_choice_serves_with_provenance_and_answers_without_the_opt_in() {
    // Under a `question` reason a screen-derived choice behaves like any
    // choice: exposed with its provenance tag, keys-answerable un-gated.
    let (state, server, delivered) = start(); // opt-in OFF
    state.publish(vec![agent_with_reason("leaf-screen-choice", "waiting", "question")]);
    let (_, body) = get(
        server.local_addr(),
        "/agents/leaf-screen-choice/output",
        Some(TOKEN),
        Duration::from_millis(300),
    );
    let v: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(v["question"]["source"], "screen");
    assert_eq!(v["question"]["askedAt"], ASKED_AT);
    let (head, _) = post(
        server.local_addr(),
        "/agents/leaf-screen-choice/input",
        Some(TOKEN),
        &format!(r#"{{"text":"1","mode":"keys","ifAskedAt":{ASKED_AT}}}"#),
    );
    assert!(head.starts_with("HTTP/1.1 200"), "head was: {head}");
    assert_eq!(
        delivered.lock().unwrap().as_slice(),
        &[("leaf-screen-choice".to_string(), "keys:1".to_string())]
    );
}

#[test]
fn a_screen_choice_under_a_permission_reason_requires_the_opt_in() {
    // KTD6 belt-and-braces: v2.1.206 labels an ask's wait a "permission
    // prompt", so a screen body answered while the live reason is
    // `permission` is treated as remote permission approval — the config
    // opt-in gates it even though the body classified as a choice.
    let (state, server, delivered) = start(); // opt-in OFF
    state.publish(vec![agent_with_reason(
        "leaf-screen-choice",
        "waiting",
        "permission",
    )]);
    let (head, body) = post(
        server.local_addr(),
        "/agents/leaf-screen-choice/input",
        Some(TOKEN),
        &format!(r#"{{"text":"1","mode":"keys","ifAskedAt":{ASKED_AT}}}"#),
    );
    assert!(head.starts_with("HTTP/1.1 403"), "head was: {head}");
    assert!(body.contains("permissionAnswersDisabled"), "body was: {body}");
    assert!(delivered.lock().unwrap().is_empty(), "nothing delivered");

    // With the opt-in ON the same answer delivers.
    let (state, server, delivered) = start_with(true);
    state.publish(vec![agent_with_reason(
        "leaf-screen-choice",
        "waiting",
        "permission",
    )]);
    let (head, _) = post(
        server.local_addr(),
        "/agents/leaf-screen-choice/input",
        Some(TOKEN),
        &format!(r#"{{"text":"1","mode":"keys","ifAskedAt":{ASKED_AT}}}"#),
    );
    assert!(head.starts_with("HTTP/1.1 200"), "head was: {head}");
    assert_eq!(delivered.lock().unwrap().len(), 1);
}

#[test]
fn a_transcript_takeover_makes_a_screen_stamped_answer_409() {
    // R5 timestamp discipline end-to-end: the consumer read a screen-derived
    // body (askedAt = raise stamp), then the transcript flushed — the
    // transcript body takes over under its own stamp, so the in-flight
    // screen-stamped guard must 409, never deliver against the new question.
    use std::sync::atomic::{AtomicBool, Ordering};
    let flipped = Arc::new(AtomicBool::new(false));
    let flipped_io = Arc::clone(&flipped);
    const SCREEN_AT: u64 = 1_000;
    const TRANSCRIPT_AT: u64 = 2_000;
    let io_fn: IoFn = Arc::new(move |_leaf, _reason| {
        let (asked_at, source, pending) = if flipped_io.load(Ordering::SeqCst) {
            (TRANSCRIPT_AT, None, None)
        } else {
            (SCREEN_AT, Some("screen".to_string()), Some(SCREEN_AT))
        };
        ResolvedIo {
            reply: None,
            question: Some(QuestionBody {
                asked_at,
                kind: "choice".into(),
                tool: "AskUserQuestion".into(),
                answerable: true,
                context: None,
                questions: vec![QuestionSpec {
                    question: "Which?".into(),
                    header: String::new(),
                    multi_select: false,
                    options: vec![QuestionOption {
                        key: "1".into(),
                        label: "Alpha".into(),
                        description: String::new(),
                    }],
                    other_key: None,
                }],
                request: None,
                source,
            }),
            turns: Vec::new(),
            pending_fallback_at: pending,
        }
    });
    let state = Arc::new(FeedState::new());
    let server = FeedServer::start(
        0,
        TOKEN.to_string(),
        Arc::clone(&state),
        Arc::new(Vec::new),
        Arc::new(|| 42),
        io_fn,
        Arc::new(|_: &str, _: InputAction| InputOutcome::Delivered),
        Arc::new(|| true),
    )
    .unwrap();
    state.publish(vec![agent_with_reason("leaf-x", "waiting", "question")]);

    // The consumer reads the screen-derived question…
    let (_, body) = get(
        server.local_addr(),
        "/agents/leaf-x/output",
        Some(TOKEN),
        Duration::from_millis(300),
    );
    let v: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(v["question"]["askedAt"], SCREEN_AT);

    // …the transcript flushes (takeover) before the answer arrives…
    flipped.store(true, Ordering::SeqCst);

    // …so the screen-stamped guard is stale: 409, nothing delivered.
    let (head, _) = post(
        server.local_addr(),
        "/agents/leaf-x/input",
        Some(TOKEN),
        &format!(r#"{{"text":"1","mode":"keys","ifAskedAt":{SCREEN_AT}}}"#),
    );
    assert!(head.starts_with("HTTP/1.1 409"), "head was: {head}");
    // Re-armed against the transcript stamp, the answer goes through.
    let (head, _) = post(
        server.local_addr(),
        "/agents/leaf-x/input",
        Some(TOKEN),
        &format!(r#"{{"text":"1","mode":"keys","ifAskedAt":{TRANSCRIPT_AT}}}"#),
    );
    assert!(head.starts_with("HTTP/1.1 200"), "head was: {head}");
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
    // `question` key is absent.
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
    // And no history → the `turns` key is absent, never an empty array
    // (feed-conversation-tail R5).
    assert!(v.get("turns").is_none(), "body was: {body}");
}

// ---- turns on /output (feed-conversation-tail R1/R3/R5) ---------------------

#[test]
fn output_serves_the_conversation_tail_ending_at_the_reply() {
    let (state, server, _) = start();
    state.publish(vec![agent("leaf-replied", "waiting")]);
    let (_, body) = get(
        server.local_addr(),
        "/agents/leaf-replied/output",
        Some(TOKEN),
        Duration::from_millis(300),
    );
    let v: serde_json::Value = serde_json::from_str(&body).expect("json body");
    let turns = v["turns"].as_array().expect("turns array");
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0]["role"], "user");
    assert_eq!(turns[0]["at"], PROMPTED_AT);
    assert_eq!(turns[0]["text"], "run the tests");
    assert_eq!(turns[1]["role"], "agent");
    // R3: the final turn IS the current reply — `at` equals `repliedAt`.
    assert_eq!(turns[1]["at"], v["repliedAt"]);
    assert_eq!(turns[1]["text"], "All tests pass.");
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
        &[("l1".to_string(), "submit:looks good, ship it".to_string())]
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

// ---- POST /agents/{key}/input mode:keys + guard + latch (U6, KTD6/R11) -----

#[test]
fn keys_mode_requires_if_asked_at_and_a_sane_body() {
    let (state, server, delivered) = start();
    state.publish(vec![agent("leaf-choice", "waiting")]);
    // keys without ifAskedAt → 400 (mandatory, KTD6); unknown mode → 400;
    // over-cap keys text → 400; control-only keys text → 400.
    for bad in [
        r#"{"text":"2","mode":"keys"}"#,
        r#"{"text":"2","mode":"noise","ifAskedAt":1781896640000}"#,
        r#"{"text":"22222222222222222","mode":"keys","ifAskedAt":1781896640000}"#,
        "{\"text\":\"\\u001b\\r\\n\",\"mode\":\"keys\",\"ifAskedAt\":1781896640000}",
    ] {
        let (head, _) = post(server.local_addr(), "/agents/leaf-choice/input", Some(TOKEN), bad);
        assert!(head.starts_with("HTTP/1.1 400"), "body {bad:?} → head: {head}");
    }
    assert!(delivered.lock().unwrap().is_empty());
}

#[test]
fn keys_mode_answers_a_choice_with_raw_bytes_and_no_submit() {
    let (state, server, delivered) = start();
    state.publish(vec![agent("leaf-choice", "waiting")]);
    let (head, body) = post(
        server.local_addr(),
        "/agents/leaf-choice/input",
        Some(TOKEN),
        &format!(r#"{{"text":"2","mode":"keys","ifAskedAt":{ASKED_AT}}}"#),
    );
    assert!(head.starts_with("HTTP/1.1 200"), "head was: {head}");
    let v: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(v["ok"], true);
    // Raw bytes, no paste markers, no trailing Enter — the fake records the
    // exact action shape (KTD6).
    assert_eq!(
        delivered.lock().unwrap().as_slice(),
        &[("leaf-choice".to_string(), "keys:2".to_string())]
    );
}

#[test]
fn a_stale_or_absent_pending_question_409s_and_delivers_nothing() {
    let (state, server, delivered) = start();
    state.publish(vec![agent("leaf-choice", "waiting"), agent("l-idle", "idle")]);
    // ifAskedAt mismatch (an older question's stamp) → 409.
    let (head, _) = post(
        server.local_addr(),
        "/agents/leaf-choice/input",
        Some(TOKEN),
        &format!(r#"{{"text":"2","mode":"keys","ifAskedAt":{}}}"#, ASKED_AT - 5_000),
    );
    assert!(head.starts_with("HTTP/1.1 409"), "head was: {head}");
    // A leaf with nothing pending: any guarded delivery (either mode) → 409.
    let (head, _) = post(
        server.local_addr(),
        "/agents/l-idle/input",
        Some(TOKEN),
        &format!(r#"{{"text":"ok","ifAskedAt":{ASKED_AT}}}"#),
    );
    assert!(head.starts_with("HTTP/1.1 409"), "head was: {head}");
    assert!(delivered.lock().unwrap().is_empty());
    // An unpublished key 404s BEFORE any pending comparison (precedence).
    let (head, _) = post(
        server.local_addr(),
        "/agents/leaf-ghost/input",
        Some(TOKEN),
        &format!(r#"{{"text":"2","mode":"keys","ifAskedAt":{ASKED_AT}}}"#),
    );
    assert!(head.starts_with("HTTP/1.1 404"), "head was: {head}");
}

#[test]
fn the_answered_latch_admits_exactly_one_guarded_delivery() {
    // AE7: two answers carrying the same valid ifAskedAt — the first delivers,
    // the second 409s before the transcript could possibly reflect the first
    // (the fake resolver never clears), and only ONE write reaches the seam.
    let (state, server, delivered) = start();
    state.publish(vec![agent("leaf-choice", "waiting")]);
    let body = format!(r#"{{"text":"2","mode":"keys","ifAskedAt":{ASKED_AT}}}"#);
    let (head, _) = post(server.local_addr(), "/agents/leaf-choice/input", Some(TOKEN), &body);
    assert!(head.starts_with("HTTP/1.1 200"), "head was: {head}");
    let (head, _) = post(server.local_addr(), "/agents/leaf-choice/input", Some(TOKEN), &body);
    assert!(head.starts_with("HTTP/1.1 409"), "second same-ifAskedAt: {head}");
    assert_eq!(delivered.lock().unwrap().len(), 1, "exactly one delivery");
    // A guarded submit against the same askedAt is latched too (R11 applies
    // to any guarded delivery, not only keys).
    let (head, _) = post(
        server.local_addr(),
        "/agents/leaf-choice/input",
        Some(TOKEN),
        &format!(r#"{{"text":"Alpha please","ifAskedAt":{ASKED_AT}}}"#),
    );
    assert!(head.starts_with("HTTP/1.1 409"), "guarded submit after latch: {head}");
}

// ---- POST /agents/{key}/input mode:other (feed-other-answer U2/R1/R4/R6) ----

#[test]
fn other_mode_types_the_free_text_row_with_the_questions_own_digit() {
    // The happy path: the consumer reads otherKey off /output and posts the
    // free text; the seam receives the digit + filtered text as ONE action
    // (fly owns the chunk choreography, not the consumer).
    let (state, server, delivered) = start();
    state.publish(vec![agent("leaf-choice", "waiting")]);
    let (_, body) = get(
        server.local_addr(),
        "/agents/leaf-choice/output",
        Some(TOKEN),
        Duration::from_millis(300),
    );
    let v: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(v["question"]["questions"][0]["otherKey"], "2");
    let (head, resp) = post(
        server.local_addr(),
        "/agents/leaf-choice/input",
        Some(TOKEN),
        &format!(
            r#"{{"text":"use the staging bucket\ninstead","mode":"other","ifAskedAt":{ASKED_AT}}}"#
        ),
    );
    assert!(head.starts_with("HTTP/1.1 200"), "head was: {head}");
    let v: serde_json::Value = serde_json::from_str(&resp).expect("json body");
    assert_eq!(v["ok"], true);
    // Digit from the question (never the consumer), newline collapsed to a
    // space, no paste markers, no inline Enter.
    assert_eq!(
        delivered.lock().unwrap().as_slice(),
        &[(
            "leaf-choice".to_string(),
            "other:2+use the staging bucket instead".to_string()
        )]
    );
}

#[test]
fn other_mode_requires_the_guard_and_a_sane_body() {
    let (state, server, delivered) = start();
    state.publish(vec![agent("leaf-choice", "waiting")]);
    let over_cap = "a".repeat(513);
    for bad in [
        // ifAskedAt is mandatory (R1) — an unguarded Other could type into
        // whatever dialog happens to be up.
        r#"{"text":"free text","mode":"other"}"#.to_string(),
        // Over the sentence cap → rejected outright, never truncated (R6).
        format!(r#"{{"text":"{over_cap}","mode":"other","ifAskedAt":{ASKED_AT}}}"#),
        // Nothing printable survives the filter → nothing to type.
        format!(r#"{{"text":"\r\n","mode":"other","ifAskedAt":{ASKED_AT}}}"#),
    ] {
        let (head, _) = post(server.local_addr(), "/agents/leaf-choice/input", Some(TOKEN), &bad);
        assert!(head.starts_with("HTTP/1.1 400"), "body {bad:?} → head: {head}");
    }
    // A stale stamp 409s like any guarded answer.
    let (head, _) = post(
        server.local_addr(),
        "/agents/leaf-choice/input",
        Some(TOKEN),
        &format!(r#"{{"text":"free text","mode":"other","ifAskedAt":{}}}"#, ASKED_AT - 5_000),
    );
    assert!(head.starts_with("HTTP/1.1 409"), "head was: {head}");
    assert!(delivered.lock().unwrap().is_empty());
}

#[test]
fn other_mode_without_a_known_digit_409s() {
    // R4: an answerable choice whose otherKey is absent (older render, >9
    // options) — typing would end with Enter selecting the highlighted
    // default, so the route refuses instead. Keys answers stay available.
    let (state, server, delivered) = start();
    state.publish(vec![agent("leaf-choice-no-other", "waiting")]);
    let (head, _) = post(
        server.local_addr(),
        "/agents/leaf-choice-no-other/input",
        Some(TOKEN),
        &format!(r#"{{"text":"free text","mode":"other","ifAskedAt":{ASKED_AT}}}"#),
    );
    assert!(head.starts_with("HTTP/1.1 409"), "head was: {head}");
    assert!(delivered.lock().unwrap().is_empty());
    let (head, _) = post(
        server.local_addr(),
        "/agents/leaf-choice-no-other/input",
        Some(TOKEN),
        &format!(r#"{{"text":"1","mode":"keys","ifAskedAt":{ASKED_AT}}}"#),
    );
    assert!(head.starts_with("HTTP/1.1 200"), "keys still works: {head}");
}

#[test]
fn other_mode_never_answers_a_permission_dialog() {
    // A permission dialog has no free-text row. Opt-in off → the pinned
    // precedence surfaces the 403 first; opt-in ON → still 409 (no otherKey,
    // kind != choice) — the mode is structurally impossible against it.
    let (state, server, delivered) = start();
    state.publish(vec![agent_with_reason("leaf-permission", "waiting", "permission")]);
    let body = format!(r#"{{"text":"free text","mode":"other","ifAskedAt":{ASKED_AT}}}"#);
    let (head, _) = post(server.local_addr(), "/agents/leaf-permission/input", Some(TOKEN), &body);
    assert!(head.starts_with("HTTP/1.1 403"), "head was: {head}");
    let (state, server, delivered2) = start_with(true);
    state.publish(vec![agent_with_reason("leaf-permission", "waiting", "permission")]);
    let (head, _) = post(server.local_addr(), "/agents/leaf-permission/input", Some(TOKEN), &body);
    assert!(head.starts_with("HTTP/1.1 409"), "head was: {head}");
    assert!(delivered.lock().unwrap().is_empty());
    assert!(delivered2.lock().unwrap().is_empty());
}

#[test]
fn a_screen_choice_other_answer_follows_the_keys_gating() {
    // Under a question reason a screen body Other-answers un-gated; under a
    // permission reason the KTD6 widened opt-in applies exactly as for keys.
    let (state, server, delivered) = start(); // opt-in OFF
    state.publish(vec![agent_with_reason("leaf-screen-choice", "waiting", "question")]);
    let body = format!(r#"{{"text":"neither, make it green","mode":"other","ifAskedAt":{ASKED_AT}}}"#);
    let (head, _) = post(server.local_addr(), "/agents/leaf-screen-choice/input", Some(TOKEN), &body);
    assert!(head.starts_with("HTTP/1.1 200"), "head was: {head}");
    assert_eq!(
        delivered.lock().unwrap().as_slice(),
        &[(
            "leaf-screen-choice".to_string(),
            "other:3+neither, make it green".to_string()
        )]
    );
    // Same answer under a live permission reason: opt-in required.
    let (state, server, delivered) = start(); // opt-in OFF
    state.publish(vec![agent_with_reason("leaf-screen-choice", "waiting", "permission")]);
    let (head, resp) = post(server.local_addr(), "/agents/leaf-screen-choice/input", Some(TOKEN), &body);
    assert!(head.starts_with("HTTP/1.1 403"), "head was: {head}");
    assert!(resp.contains("permissionAnswersDisabled"), "body was: {resp}");
    assert!(delivered.lock().unwrap().is_empty());
}

#[test]
fn the_answered_latch_spans_keys_and_other() {
    // One question, one answer — whichever mode lands first wins; the other
    // 409s against the same askedAt (R11 is mode-agnostic).
    let (state, server, delivered) = start();
    state.publish(vec![agent("leaf-choice", "waiting")]);
    let (head, _) = post(
        server.local_addr(),
        "/agents/leaf-choice/input",
        Some(TOKEN),
        &format!(r#"{{"text":"1","mode":"keys","ifAskedAt":{ASKED_AT}}}"#),
    );
    assert!(head.starts_with("HTTP/1.1 200"), "head was: {head}");
    let (head, _) = post(
        server.local_addr(),
        "/agents/leaf-choice/input",
        Some(TOKEN),
        &format!(r#"{{"text":"free text","mode":"other","ifAskedAt":{ASKED_AT}}}"#),
    );
    assert!(head.starts_with("HTTP/1.1 409"), "other after keys: {head}");
    assert_eq!(delivered.lock().unwrap().len(), 1, "exactly one delivery");
}

#[test]
fn keys_answers_to_permission_dialogs_are_config_gated() {
    // Default off: a keys answer to a pending *permission* question is 403 —
    // the bearer token must not be a remote permission-approval credential
    // unless explicitly opted in (KTD6, resolved Open Question). The 403
    // carries a JSON discriminator body so a consumer doesn't mistake this
    // policy refusal for an auth failure (api-contract review — the game
    // consumer blanket-maps 403→auth-error).
    let (state, server, delivered) = start();
    state.publish(vec![agent_with_reason("leaf-permission", "waiting", "permission")]);
    let body = format!(r#"{{"text":"1","mode":"keys","ifAskedAt":{ASKED_AT}}}"#);
    let (head, resp_body) =
        post(server.local_addr(), "/agents/leaf-permission/input", Some(TOKEN), &body);
    assert!(head.starts_with("HTTP/1.1 403"), "head was: {head}");
    assert!(
        resp_body.contains("permissionAnswersDisabled"),
        "403 must carry the discriminator body, was: {resp_body}"
    );
    assert!(delivered.lock().unwrap().is_empty());

    // Opted in → delivers.
    let (state, server, delivered) = start_with(true);
    state.publish(vec![agent_with_reason("leaf-permission", "waiting", "permission")]);
    let (head, _) = post(server.local_addr(), "/agents/leaf-permission/input", Some(TOKEN), &body);
    assert!(head.starts_with("HTTP/1.1 200"), "head was: {head}");
    assert_eq!(
        delivered.lock().unwrap().as_slice(),
        &[("leaf-permission".to_string(), "keys:1".to_string())]
    );

    // The un-corroborated case, isolated from the latch (a FRESH server so no
    // prior reservation confounds it): with the reason not "permission" the
    // question is unexposed, so the guard 409s BEFORE the opt-in 403 — proving
    // the reason re-check itself gates, even fully opted in.
    let (state, server, delivered) = start_with(true);
    state.publish(vec![agent("leaf-permission", "working")]); // no permission reason
    let (head, _) = post(server.local_addr(), "/agents/leaf-permission/input", Some(TOKEN), &body);
    assert!(head.starts_with("HTTP/1.1 409"), "unexposed permission → 409: {head}");
    assert!(delivered.lock().unwrap().is_empty());
}

#[test]
fn a_guarded_submit_to_a_permission_dialog_is_gated_like_keys() {
    // The review's cross-cutting finding: a guarded SUBMIT (mode omitted) whose
    // ifAskedAt matches a permission question confirms the dialog via its
    // trailing Enter, so it is remote permission approval and requires the same
    // opt-in as keys. Default off → 403, nothing delivered.
    let (state, server, delivered) = start();
    state.publish(vec![agent_with_reason("leaf-permission", "waiting", "permission")]);
    let body = format!(r#"{{"text":"yes","ifAskedAt":{ASKED_AT}}}"#);
    let (head, _) = post(server.local_addr(), "/agents/leaf-permission/input", Some(TOKEN), &body);
    assert!(head.starts_with("HTTP/1.1 403"), "guarded submit to permission: {head}");
    assert!(delivered.lock().unwrap().is_empty());
    // Opted in → the guarded submit delivers (paste text recorded).
    let (state, server, delivered) = start_with(true);
    state.publish(vec![agent_with_reason("leaf-permission", "waiting", "permission")]);
    let (head, _) = post(server.local_addr(), "/agents/leaf-permission/input", Some(TOKEN), &body);
    assert!(head.starts_with("HTTP/1.1 200"), "head was: {head}");
    assert_eq!(
        delivered.lock().unwrap().as_slice(),
        &[("leaf-permission".to_string(), "submit:yes".to_string())]
    );
}

#[test]
fn a_guarded_answer_carries_the_option_key_the_consumer_posts() {
    // The wire hands the consumer the digit; a keys answer echoes it back
    // through the seam verbatim (feed-pending-question, agent-native review).
    let (state, server, delivered) = start();
    state.publish(vec![agent("leaf-choice", "waiting")]);
    let (_, body) = get(
        server.local_addr(),
        "/agents/leaf-choice/output",
        Some(TOKEN),
        Duration::from_millis(300),
    );
    let v: serde_json::Value = serde_json::from_str(&body).expect("json body");
    let key = v["question"]["questions"][0]["options"][0]["key"].as_str().unwrap();
    assert_eq!(key, "1");
    let (head, _) = post(
        server.local_addr(),
        "/agents/leaf-choice/input",
        Some(TOKEN),
        &format!(r#"{{"text":"{key}","mode":"keys","ifAskedAt":{ASKED_AT}}}"#),
    );
    assert!(head.starts_with("HTTP/1.1 200"), "head was: {head}");
    assert_eq!(
        delivered.lock().unwrap().as_slice(),
        &[("leaf-choice".to_string(), format!("keys:{key}"))]
    );
}

#[test]
fn an_explicit_submit_mode_delivers_like_the_omitted_default() {
    // api-contract gap: an explicit "mode":"submit" is the same inject-anytime
    // paste+Enter as the omitted-mode default.
    let (state, server, delivered) = start();
    state.publish(vec![agent("l1", "waiting")]);
    let (head, _) = post(
        server.local_addr(),
        "/agents/l1/input",
        Some(TOKEN),
        r#"{"text":"ship it","mode":"submit"}"#,
    );
    assert!(head.starts_with("HTTP/1.1 200"), "head was: {head}");
    assert_eq!(
        delivered.lock().unwrap().as_slice(),
        &[("l1".to_string(), "submit:ship it".to_string())]
    );
}

#[test]
fn unguarded_submit_keeps_the_inject_anytime_contract() {
    // No ifAskedAt → no guard, no latch: two identical submits both deliver
    // (today's behavior, byte-for-byte).
    let (state, server, delivered) = start();
    state.publish(vec![agent("l1", "waiting")]);
    for _ in 0..2 {
        let (head, _) = post(
            server.local_addr(),
            "/agents/l1/input",
            Some(TOKEN),
            r#"{"text":"keep going"}"#,
        );
        assert!(head.starts_with("HTTP/1.1 200"), "head was: {head}");
    }
    assert_eq!(delivered.lock().unwrap().len(), 2);
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
