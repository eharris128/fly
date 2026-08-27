//! hook-ask-channel U9: end-to-end held-ask round-trips over the real socket —
//! the `fly notify --permission-request` client (`cli::notify::hold_ask`)
//! against a `HookServer` wired to a real `AskRegistry` exactly like `lib.rs`
//! (minus the shell): register → ack → hold; local kill → drop → clear; remote
//! answer → decision line; skew fast-fail; shutdown release.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fly_lib::cli::notify::hold_ask;
use fly_lib::feed::ask::{AnswerOutcome, AskRegistry};
use fly_lib::hooks::protocol::AskPayload;
use fly_lib::hooks::{AskHandler, AskTicket, Dispatch, HookServer, TokenRegistry};
use fly_lib::pty::PaneId;

/// The fixed receipt stamp the test handler registers with (`answer` keys on it).
const STAMP: u64 = 7_777;

fn sock_path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("hook.sock")
}

fn no_dispatch() -> Dispatch {
    Arc::new(|_, _| {})
}

/// The backend's ask-handler wiring, minus the shell: pane → `leaf-<id>`, register,
/// arm the generation-guarded drop.
fn handler(registry: Arc<AskRegistry>) -> AskHandler {
    Arc::new(move |pane, payload: AskPayload| {
        let leaf = format!("leaf-{}", pane.0);
        let (gen, rx) = registry.register(&leaf, payload, STAMP)?;
        let reg = Arc::clone(&registry);
        Some(AskTicket {
            decision_rx: rx,
            on_drop: Box::new(move || {
                reg.clear_if(&leaf, gen);
            }),
        })
    })
}

fn start(registry: &Arc<AskRegistry>) -> (tempfile::TempDir, Arc<TokenRegistry>, HookServer) {
    start_with_peer(registry, None)
}

/// agent-peer-messaging U7: same harness with a peer handler wired, for the
/// held-ask × peer-send interaction cases below.
fn start_with_peer(
    registry: &Arc<AskRegistry>,
    peer_handler: Option<fly_lib::hooks::PeerHandler>,
) -> (tempfile::TempDir, Arc<TokenRegistry>, HookServer) {
    let dir = tempfile::tempdir().unwrap();
    let tokens = Arc::new(TokenRegistry::new());
    let server = HookServer::start_full(
        sock_path(&dir),
        Arc::clone(&tokens),
        no_dispatch(),
        None,
        Some(handler(Arc::clone(registry))),
        peer_handler,
    )
    .unwrap();
    (dir, tokens, server)
}

fn wait_until(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + deadline;
    while Instant::now() < end {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    cond()
}

fn bash_ask() -> AskPayload {
    AskPayload {
        tool: Some("Bash".into()),
        request: Some("touch /tmp/x".into()),
        session_id: Some("sess-1".into()),
        ..Default::default()
    }
}

#[test]
fn a_remote_answer_reaches_the_held_client_as_the_decision_line() {
    let registry = Arc::new(AskRegistry::new());
    let (_dir, tokens, server) = start(&registry);
    let tok = tokens.issue(PaneId(3));
    let path = server.socket_path().to_path_buf();

    let client = std::thread::spawn(move || hold_ask(&path, &tok, &bash_ask()));

    // The ask registers (KTD1) and carries the payload…
    assert!(wait_until(Duration::from_secs(2), || registry.get("leaf-3").is_some()));
    let held = registry.get("leaf-3").unwrap();
    assert_eq!(held.asked_at_ms, STAMP);
    assert_eq!(held.payload.tool.as_deref(), Some("Bash"));

    // …and a remote allow resolves the client with the exact decision line.
    assert_eq!(registry.answer("leaf-3", STAMP, true), AnswerOutcome::Delivered);
    let line = client.join().unwrap().unwrap().expect("decision line");
    let v: serde_json::Value = serde_json::from_str(&line).expect("valid JSON line");
    assert_eq!(v["hookSpecificOutput"]["decision"]["behavior"], "allow");
    assert_eq!(registry.get("leaf-3"), None, "answered ask is gone");
}

#[test]
fn a_killed_hook_clears_the_registry_within_the_hold_poll() {
    // Claude kills the hook on a local answer (live-verified) — from fly's
    // side that is a peer close; the hold loop's probe must clear the entry.
    let registry = Arc::new(AskRegistry::new());
    let (_dir, tokens, server) = start(&registry);
    let tok = tokens.issue(PaneId(5));

    // A raw client standing in for the hook process (we can't kill a thread).
    let mut stream = UnixStream::connect(server.socket_path()).unwrap();
    let msg = format!(r#"{{"token":"{tok}","op":"ask/hold","tool":"Bash"}}"#);
    stream.write_all(format!("{msg}\n").as_bytes()).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut ack = String::new();
    reader.read_line(&mut ack).unwrap();
    assert!(ack.contains("\"held\":true"), "ack was: {ack}");
    assert!(wait_until(Duration::from_secs(2), || registry.get("leaf-5").is_some()));

    drop(reader);
    drop(stream); // the "kill": full close
    assert!(
        wait_until(Duration::from_secs(2), || registry.get("leaf-5").is_none()),
        "peer death must clear the held ask"
    );
}

#[test]
fn no_ack_means_old_server_and_the_client_exits_fast() {
    // R8 skew: a server without the ask handler (an old fly app) silently
    // ignores the op — the client must give up at the ack deadline, quickly
    // and decision-less, so the dialog proceeds normally.
    let dir = tempfile::tempdir().unwrap();
    let tokens = Arc::new(TokenRegistry::new());
    let server =
        HookServer::start(sock_path(&dir), Arc::clone(&tokens), no_dispatch()).unwrap();
    let tok = tokens.issue(PaneId(1));
    let started = Instant::now();
    let out = hold_ask(&server.socket_path().to_path_buf(), &tok, &bash_ask()).unwrap();
    assert_eq!(out, None, "no decision from an old server");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "ack deadline must bound the wait, took {:?}",
        started.elapsed()
    );
}

#[test]
fn an_invalid_token_registers_nothing_and_releases_the_client() {
    // The security boundary is unchanged (R2): silent rejection, no held state.
    let registry = Arc::new(AskRegistry::new());
    let (_dir, tokens, server) = start(&registry);
    tokens.issue(PaneId(1));
    let out = hold_ask(
        &server.socket_path().to_path_buf(),
        "00000000deadbeef",
        &bash_ask(),
    )
    .unwrap();
    assert_eq!(out, None);
    assert_eq!(registry.get("leaf-1"), None);
}

#[test]
fn registry_shutdown_releases_every_held_client_decision_less() {
    // R9 ordered shutdown: the hook exits quietly, the dialog proceeds.
    let registry = Arc::new(AskRegistry::new());
    let (_dir, tokens, server) = start(&registry);
    let tok = tokens.issue(PaneId(8));
    let path = server.socket_path().to_path_buf();
    let client = std::thread::spawn(move || hold_ask(&path, &tok, &bash_ask()));
    assert!(wait_until(Duration::from_secs(2), || registry.get("leaf-8").is_some()));
    registry.shutdown();
    assert_eq!(client.join().unwrap().unwrap(), None, "released, no decision");
}

#[test]
fn a_replacement_ask_releases_the_older_held_client() {
    // KTD2 last-write-wins: Claude shows one dialog per session at a time, so
    // a second ask for the same pane supersedes the first — the older hook is
    // released (exits quietly), the newer one holds and is answerable.
    let registry = Arc::new(AskRegistry::new());
    let (_dir, tokens, server) = start(&registry);
    let tok = tokens.issue(PaneId(9));
    let path = server.socket_path().to_path_buf();
    let tok1 = tok.clone();
    let path1 = path.clone();
    let first = std::thread::spawn(move || hold_ask(&path1, &tok1, &bash_ask()));
    assert!(wait_until(Duration::from_secs(2), || registry.get("leaf-9").is_some()));

    let second_payload = AskPayload {
        tool: Some("Write".into()),
        ..Default::default()
    };
    let second = std::thread::spawn(move || hold_ask(&path, &tok, &second_payload));
    assert!(wait_until(Duration::from_secs(2), || {
        registry
            .get("leaf-9")
            .is_some_and(|a| a.payload.tool.as_deref() == Some("Write"))
    }));
    assert_eq!(first.join().unwrap().unwrap(), None, "older hook released");
    assert_eq!(registry.answer("leaf-9", STAMP, false), AnswerOutcome::Delivered);
    let line = second.join().unwrap().unwrap().expect("decision");
    assert!(line.contains("\"behavior\":\"deny\""));
}

// ---- held asks × peer sends (agent-peer-messaging U7) -----------------------
// The peer send path's wide question gate (KTD5) consumes the REAL resolver
// chain here — a held `ask/hold` is the resolver's primary leg, so a send at
// a pane whose hook is holding the socket must refuse `askPending` without
// disturbing the held connection.

use fly_lib::cli::peer::{PeerRequest, PeerResponse};
use fly_lib::feed::pending::PendingSignals;
use fly_lib::feed::server::drop_blocked_by_question;
use fly_lib::hooks::PeerHandler;
use fly_lib::peer::{self, rate, PeerPorts};

type PtyLog = Arc<Mutex<Vec<(u64, Vec<u8>)>>>;

/// The backend's peer wiring, minus the shell: a real `FallbackResolver` whose ask
/// leg reads the same `AskRegistry` the socket's ask handler registers into,
/// gated through the real `drop_blocked_by_question` predicate.
fn peer_handler_with_real_ask_gate(
    registry: Arc<AskRegistry>,
    resume_dir: &tempfile::TempDir,
    writes: PtyLog,
) -> PeerHandler {
    use fly_lib::feed::fallback::{AskFn, FallbackResolver, ScreenFn};
    use fly_lib::feed::wire::AgentEntry;

    let ask_reg = Arc::clone(&registry);
    let ask_fn: AskFn = Arc::new(move |leaf| ask_reg.get(leaf));
    let screen_fn: ScreenFn = Arc::new(|_| None);
    let resolver = Arc::new(FallbackResolver::with_roots(
        resume_dir.path().join("resume.json"),
        None,
        None,
        Arc::new(PendingSignals::new()),
        screen_fn,
        ask_fn,
    ));
    let roster_rows = || -> Vec<AgentEntry> {
        vec![AgentEntry {
            leaf_key: "leaf-12".into(),
            workspace: "home".into(),
            tab: "fly".into(),
            cwd: Some("/p".into()),
            status: "waiting".into(),
            needs_attention: false,
            reason: None,
            working_for_ms: None,
            live_task_count: 0,
            num: None,
            last_reply_at: None,
            question_pending_at: None,
            pane_id: Some(12),
            peer_opt_in: true,
        }]
    };
    Arc::new(move |origin, buf: &[u8]| {
        let req: PeerRequest = match serde_json::from_slice(buf) {
            Ok(r) => r,
            Err(_) => return PeerResponse::err("badRequest").to_bytes(),
        };
        let now = 1_000_000u64;
        let roster = || (roster_rows(), Some(now - 1_000));
        let leaf_for_pane =
            |p: u64| (p == 12).then(|| "leaf-12".to_string());
        let buckets = Mutex::new(rate::Buckets::new());
        let try_take = |pane: u64, at: u64| buckets.lock().unwrap().try_take(pane, at);
        // The production gate: resolver (held ask = primary leg) → the drop
        // route's own predicate, roster reason/status from the same snapshot.
        let ask_pending = |leaf: &str| {
            let resolved = resolver.resolve_io(leaf, None, "waiting");
            drop_blocked_by_question(resolved.question, None)
        };
        let w = Arc::clone(&writes);
        let deliver = |_expect: u64, _leaf: &str, text: &str| {
            w.lock().unwrap().push((12, text.as_bytes().to_vec()));
            fly_lib::feed::drop::DropOutcome::Delivered
        };
        peer::dispatch_peer_op(
            origin.0,
            &req,
            &PeerPorts {
                now_ms: now,
                roster: &roster,
                leaf_for_pane: &leaf_for_pane,
                try_take_rate: &try_take,
                ask_pending: &ask_pending,
                deliver: &deliver,
            },
        )
        .to_bytes()
    })
}

fn peer_send(path: &std::path::Path, token: &str, pane: u64, message: &str) -> PeerResponse {
    use std::io::Read;
    let mut s = UnixStream::connect(path).unwrap();
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let req = PeerRequest {
        token: token.into(),
        op: "peer/send".into(),
        pane: Some(pane),
        message: Some(message.into()),
    };
    s.write_all(&serde_json::to_vec(&req).unwrap()).unwrap();
    s.shutdown(std::net::Shutdown::Write).unwrap();
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    serde_json::from_slice(&buf).expect("peer op answers")
}

#[test]
fn a_send_targeting_a_pane_with_a_held_ask_refuses_ask_pending() {
    let registry = Arc::new(AskRegistry::new());
    let resume_dir = tempfile::tempdir().unwrap();
    let writes: PtyLog = Arc::new(Mutex::new(Vec::new()));
    let (_dir, tokens, server) = start_with_peer(
        &registry,
        Some(peer_handler_with_real_ask_gate(
            Arc::clone(&registry),
            &resume_dir,
            Arc::clone(&writes),
        )),
    );
    let target_tok = tokens.issue(PaneId(12));
    let sender_tok = tokens.issue(PaneId(7));
    let path = server.socket_path().to_path_buf();

    // Before any ask is held, the send lands (the gate is live, not latched).
    let resp = peer_send(&path, &sender_tok, 12, "pre-ask ping");
    assert!(resp.ok, "{resp:?}");
    assert_eq!(writes.lock().unwrap().len(), 1);

    // The target's hook registers a held permission ask…
    let hold_path = path.clone();
    let hold = std::thread::spawn(move || hold_ask(&hold_path, &target_tok, &bash_ask()));
    assert!(wait_until(Duration::from_secs(2), || registry
        .get("leaf-12")
        .is_some()));

    // …and now the same send refuses askPending, writes nothing, and the held
    // connection is undisturbed (the ask is still registered afterwards).
    let resp = peer_send(&path, &sender_tok, 12, "mid-ask ping");
    assert_eq!(resp.error.as_deref(), Some("askPending"), "{resp:?}");
    assert_eq!(writes.lock().unwrap().len(), 1, "nothing new reached the PTY");
    assert!(registry.get("leaf-12").is_some(), "held ask survived");

    // Resolve the ask remotely so the held client exits cleanly.
    assert_eq!(registry.answer("leaf-12", STAMP, false), AnswerOutcome::Delivered);
    let _ = hold.join().unwrap();
}

#[test]
fn peer_traffic_does_not_perturb_held_ask_lifecycle() {
    let registry = Arc::new(AskRegistry::new());
    let resume_dir = tempfile::tempdir().unwrap();
    let writes: PtyLog = Arc::new(Mutex::new(Vec::new()));
    let (_dir, tokens, server) = start_with_peer(
        &registry,
        Some(peer_handler_with_real_ask_gate(
            Arc::clone(&registry),
            &resume_dir,
            Arc::clone(&writes),
        )),
    );
    let target_tok = tokens.issue(PaneId(12));
    let sender_tok = tokens.issue(PaneId(7));
    let path = server.socket_path().to_path_buf();

    // A raw held client standing in for the hook process.
    let mut stream = UnixStream::connect(&path).unwrap();
    let msg = format!(r#"{{"token":"{target_tok}","op":"ask/hold","tool":"Bash"}}"#);
    stream.write_all(format!("{msg}\n").as_bytes()).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut ack = String::new();
    reader.read_line(&mut ack).unwrap();
    assert!(ack.contains("\"held\":true"));
    assert!(wait_until(Duration::from_secs(2), || registry
        .get("leaf-12")
        .is_some()));

    // A burst of peer traffic — refused sends at the held pane, list ops.
    for _ in 0..5 {
        let resp = peer_send(&path, &sender_tok, 12, "ping");
        assert_eq!(resp.error.as_deref(), Some("askPending"));
    }
    assert!(registry.get("leaf-12").is_some(), "ask survived the burst");

    // The local answer kills the hook (peer close) → registry clears, exactly
    // as without any peer traffic.
    drop(reader);
    drop(stream);
    assert!(
        wait_until(Duration::from_secs(2), || registry.get("leaf-12").is_none()),
        "peer traffic must not perturb the drop-clears contract"
    );
}
