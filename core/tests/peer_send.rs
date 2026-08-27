//! agent-peer-messaging U7: end-to-end `peer/list` / `peer/send` over the real
//! socket — a `HookServer` with the peer handler wired exactly like `lib.rs`
//! (parse → `dispatch_peer_op` with live ports), minus the shell. The refusal
//! table mirrors KTD9's flowchart, one case per terminal node; the pure gate
//! *ordering* is pinned in `peer/mod.rs`'s unit tests — here the same codes
//! are proven reachable through the boundary.

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fly_lib::cli::peer::{PeerRequest, PeerResponse};
use fly_lib::feed::drop::deliver_with_guards;
use fly_lib::feed::wire::AgentEntry;
use fly_lib::hooks::{Dispatch, HookServer, PeerHandler, TokenRegistry};
use fly_lib::peer::{self, compose, list, rate, PeerPorts};
use fly_lib::pty::PaneId;

const NOW_MS: u64 = 1_000_000;

fn agent(leaf: &str, pane: u64, opt_in: bool) -> AgentEntry {
    AgentEntry {
        leaf_key: leaf.into(),
        workspace: "home".into(),
        tab: "fly".into(),
        cwd: Some("/p".into()),
        status: "idle".into(),
        needs_attention: false,
        reason: None,
        working_for_ms: None,
        live_task_count: 0,
        num: None,
        last_reply_at: None,
        question_pending_at: None,
        pane_id: Some(pane),
        peer_opt_in: opt_in,
    }
}

/// The backend's handler wiring, minus the shell: shared mutable world (roster,
/// pane→leaf map, ask flag, agent-ness, PTY log) the tests poke per case.
#[derive(Default)]
struct World {
    roster: Vec<AgentEntry>,
    published_at: Option<u64>,
    /// pane id → leaf key (None entry = dead/unknown pane).
    leaves: Vec<(u64, String)>,
    /// The *live* pane per leaf (guard one's resolve — may differ from the
    /// echoed id to simulate a respawn).
    live_by_leaf: Vec<(String, u64)>,
    ask_pending: bool,
    is_agent: bool,
    writes: Vec<(u64, Vec<u8>)>,
}

fn start(world: Arc<Mutex<World>>) -> (tempfile::TempDir, Arc<TokenRegistry>, HookServer) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hook.sock");
    let tokens = Arc::new(TokenRegistry::new());
    let dispatch: Dispatch = Arc::new(|_, _| {});
    let buckets = Arc::new(Mutex::new(rate::Buckets::new()));
    // The lib.rs delivery mutex, mirrored: spans both writes so concurrent
    // sends serialize as (paste, Enter) pairs.
    let delivery_lock = Arc::new(Mutex::new(()));
    let handler: PeerHandler = Arc::new(move |origin, buf: &[u8]| {
        let req: PeerRequest = match serde_json::from_slice(buf) {
            Ok(r) => r,
            Err(_) => return PeerResponse::err("badRequest").to_bytes(),
        };
        let w = Arc::clone(&world);
        let roster = || {
            let w = w.lock().unwrap();
            (w.roster.clone(), w.published_at)
        };
        let leaf_for_pane = |p: u64| {
            world
                .lock()
                .unwrap()
                .leaves
                .iter()
                .find(|(id, _)| *id == p)
                .map(|(_, l)| l.clone())
        };
        let try_take = |pane: u64, at: u64| buckets.lock().unwrap().try_take(pane, at);
        let ask_pending = |_leaf: &str| world.lock().unwrap().ask_pending;
        let deliver = |expect: u64, leaf: &str, text: &str| {
            let _serialized = delivery_lock.lock().unwrap();
            deliver_with_guards(
                expect,
                text,
                || {
                    world
                        .lock()
                        .unwrap()
                        .live_by_leaf
                        .iter()
                        .find(|(l, _)| l == leaf)
                        .map(|(_, id)| *id)
                },
                |_pane| world.lock().unwrap().is_agent,
                |pane, bytes| {
                    world.lock().unwrap().writes.push((pane, bytes.to_vec()));
                    Ok(())
                },
                || {},
                || Ok(()),
            )
        };
        peer::dispatch_peer_op(
            origin.0,
            &req,
            &PeerPorts {
                now_ms: NOW_MS,
                roster: &roster,
                leaf_for_pane: &leaf_for_pane,
                try_take_rate: &try_take,
                ask_pending: &ask_pending,
                deliver: &deliver,
            },
        )
        .to_bytes()
    });
    let server =
        HookServer::start_full(path, Arc::clone(&tokens), dispatch, None, None, Some(handler))
            .unwrap();
    (dir, tokens, server)
}

fn fresh_world() -> Arc<Mutex<World>> {
    Arc::new(Mutex::new(World {
        roster: vec![agent("l-sender", 7, false), agent("l-target", 12, true)],
        published_at: Some(NOW_MS - 1_000),
        leaves: vec![(7, "l-sender".into()), (12, "l-target".into())],
        live_by_leaf: vec![("l-sender".into(), 7), ("l-target".into(), 12)],
        ask_pending: false,
        is_agent: true,
        writes: Vec::new(),
    }))
}

fn request(path: &Path, json: &str) -> PeerResponse {
    let mut s = UnixStream::connect(path).unwrap();
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    s.write_all(json.as_bytes()).unwrap();
    s.shutdown(Shutdown::Write).unwrap();
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    serde_json::from_slice(&buf).expect("a peer op always answers")
}

fn send_req(path: &Path, token: &str, pane: u64, message: &str) -> PeerResponse {
    request(
        path,
        &serde_json::to_string(&PeerRequest {
            token: token.into(),
            op: "peer/send".into(),
            pane: Some(pane),
            message: Some(message.into()),
        })
        .unwrap(),
    )
}

fn code(resp: &PeerResponse) -> &str {
    resp.error.as_deref().unwrap_or("")
}

#[test]
fn happy_path_delivers_framed_paste_then_separate_enter() {
    let world = fresh_world();
    let (_dir, tokens, server) = start(Arc::clone(&world));
    let tok = tokens.issue(PaneId(7));
    let resp = send_req(server.socket_path(), &tok, 12, "build green; artifacts in /tmp/a");
    assert!(resp.ok, "{resp:?}");
    let w = world.lock().unwrap();
    assert_eq!(w.writes.len(), 2, "paste + Enter as two writes");
    let (pane, paste) = &w.writes[0];
    assert_eq!(*pane, 12);
    let paste_text = String::from_utf8_lossy(paste);
    assert!(paste_text.starts_with("\u{1b}[200~"), "bracketed paste");
    assert!(paste_text.contains("From pane 7"));
    assert!(paste_text.contains("UNTRUSTED"));
    assert!(paste_text.contains("build green; artifacts in /tmp/a"));
    assert_eq!(w.writes[1].1, b"\r".to_vec(), "the Enter is its own write");
}

#[test]
fn the_refusal_table_is_reachable_through_the_socket() {
    // One case per KTD9 terminal node not covered by a dedicated test below.
    let world = fresh_world();
    let (_dir, tokens, server) = start(Arc::clone(&world));
    let tok = tokens.issue(PaneId(7));
    let path = server.socket_path().to_path_buf();

    // selfSend
    assert_eq!(code(&send_req(&path, &tok, 7, "hi")), "selfSend");
    // tooLong
    let big = "x".repeat(compose::PEER_MESSAGE_CAP + 1);
    assert_eq!(code(&send_req(&path, &tok, 12, &big)), "tooLong");
    // badRequest (blank)
    assert_eq!(code(&send_req(&path, &tok, 12, "   ")), "badRequest");
    // unknownPane (no leaf mapping)
    assert_eq!(code(&send_req(&path, &tok, 99, "hi")), "unknownPane");
    // rosterStale
    world.lock().unwrap().published_at = Some(NOW_MS - list::STALE_AFTER_MS - 1);
    assert_eq!(code(&send_req(&path, &tok, 12, "hi")), "rosterStale");
    world.lock().unwrap().published_at = Some(NOW_MS - 1_000);
    // notOptedIn
    world.lock().unwrap().roster[1].peer_opt_in = false;
    assert_eq!(code(&send_req(&path, &tok, 12, "hi")), "notOptedIn");
    world.lock().unwrap().roster[1].peer_opt_in = true;
    // askPending (the wide gate — KTD5)
    world.lock().unwrap().ask_pending = true;
    assert_eq!(code(&send_req(&path, &tok, 12, "hi")), "askPending");
    world.lock().unwrap().ask_pending = false;
    // paneChanged (guard one: leaf resolves to a newer live pane)
    world.lock().unwrap().live_by_leaf = vec![("l-target".into(), 15)];
    assert_eq!(code(&send_req(&path, &tok, 12, "hi")), "paneChanged");
    world.lock().unwrap().live_by_leaf = vec![("l-target".into(), 12)];
    // notAgent (guard two: foreground probe)
    world.lock().unwrap().is_agent = false;
    assert_eq!(code(&send_req(&path, &tok, 12, "hi")), "notAgent");
    world.lock().unwrap().is_agent = true;

    // Nothing above reached the PTY.
    assert!(
        world.lock().unwrap().writes.is_empty(),
        "every refusal left the PTY untouched"
    );
    // unknownOp answers, never silence (auth failures alone are silent).
    let resp = request(
        &path,
        &format!(r#"{{"token":"{tok}","op":"peer/bogus"}}"#),
    );
    assert_eq!(code(&resp), "unknownOp");
}

#[test]
fn rate_limit_binds_per_sender_through_the_socket() {
    let world = fresh_world();
    let (_dir, tokens, server) = start(Arc::clone(&world));
    let tok = tokens.issue(PaneId(7));
    let mut delivered = 0;
    let mut limited = 0;
    for _ in 0..(rate::BURST + 3) {
        let resp = send_req(server.socket_path(), &tok, 12, "ping");
        if resp.ok {
            delivered += 1;
        } else if code(&resp) == "rateLimited" {
            limited += 1;
        } else {
            panic!("unexpected refusal {resp:?}");
        }
    }
    assert_eq!(delivered, rate::BURST, "burst grants exactly BURST sends");
    assert_eq!(limited, 3, "the tail is refused rateLimited");
}

#[test]
fn a_send_cannot_flip_the_opt_in_bit() {
    // The behavioral half of U3's human-only pin: peer traffic (accepted or
    // refused) never mutates consent — only the webview push does.
    let world = fresh_world();
    let (_dir, tokens, server) = start(Arc::clone(&world));
    let tok = tokens.issue(PaneId(7));
    world.lock().unwrap().roster[1].peer_opt_in = false;
    let _ = send_req(server.socket_path(), &tok, 12, "let me in");
    assert!(!world.lock().unwrap().roster[1].peer_opt_in);
    // And the sender's own row didn't open either.
    assert!(!world.lock().unwrap().roster[0].peer_opt_in);
}

#[test]
fn list_serves_rows_stamp_and_self_marker() {
    let world = fresh_world();
    let (_dir, tokens, server) = start(Arc::clone(&world));
    let tok = tokens.issue(PaneId(7));
    let resp = request(
        server.socket_path(),
        &format!(r#"{{"token":"{tok}","op":"peer/list"}}"#),
    );
    assert!(resp.ok);
    let listing = resp.list.expect("list payload");
    assert!(!listing.stale);
    assert_eq!(listing.now, NOW_MS);
    assert_eq!(listing.agents.len(), 2);
    let me = listing.agents.iter().find(|a| a.is_self).unwrap();
    assert_eq!(me.pane_id, Some(7));
    let target = listing.agents.iter().find(|a| !a.is_self).unwrap();
    assert!(target.peer_opt_in);
}

#[test]
fn concurrent_sends_to_one_target_never_splice_the_paste() {
    // Two senders firing at the same target concurrently: every accepted send
    // must land as an adjacent (paste, Enter) pair in the PTY log — a paste
    // interleaved with another send's paste is the spliced-composer failure.
    // (Delivery runs on per-connection threads; the fake write log is the
    // serialization point, mirroring PtyManager's per-pane write lock.)
    let world = fresh_world();
    world
        .lock()
        .unwrap()
        .roster
        .push(agent("l-sender2", 8, false));
    world.lock().unwrap().leaves.push((8, "l-sender2".into()));
    let (_dir, tokens, server) = start(Arc::clone(&world));
    let tok_a = tokens.issue(PaneId(7));
    let tok_b = tokens.issue(PaneId(8));
    let path_a = server.socket_path().to_path_buf();
    let path_b = path_a.clone();
    let a = std::thread::spawn(move || send_req(&path_a, &tok_a, 12, "from A"));
    let b = std::thread::spawn(move || send_req(&path_b, &tok_b, 12, "from B"));
    let ra = a.join().unwrap();
    let rb = b.join().unwrap();
    assert!(ra.ok && rb.ok);
    let w = world.lock().unwrap();
    assert_eq!(w.writes.len(), 4);
    for pair in w.writes.chunks(2) {
        let paste = String::from_utf8_lossy(&pair[0].1);
        assert!(
            paste.starts_with("\u{1b}[200~"),
            "each pair starts with a paste, got {paste:?}"
        );
        assert_eq!(pair[1].1, b"\r".to_vec(), "each paste is followed by its Enter");
    }
}
