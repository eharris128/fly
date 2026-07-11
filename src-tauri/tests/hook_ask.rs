//! hook-ask-channel U9: end-to-end held-ask round-trips over the real socket —
//! the `fly notify --permission-request` client (`cli::notify::hold_ask`)
//! against a `HookServer` wired to a real `AskRegistry` exactly like `lib.rs`
//! (minus Tauri): register → ack → hold; local kill → drop → clear; remote
//! answer → decision line; skew fast-fail; shutdown release.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
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

/// The lib.rs ask-handler wiring, minus Tauri: pane → `leaf-<id>`, register,
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
    let dir = tempfile::tempdir().unwrap();
    let tokens = Arc::new(TokenRegistry::new());
    let server = HookServer::start_full(
        sock_path(&dir),
        Arc::clone(&tokens),
        no_dispatch(),
        None,
        Some(handler(Arc::clone(registry))),
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
