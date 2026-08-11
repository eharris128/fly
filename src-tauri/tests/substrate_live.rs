//! Live tmux-substrate integration checks (tmux plan U3).
//!
//! These need a real `tmux` binary, so they are `#[ignore]` by default — run
//! explicitly with `cargo test --test substrate_live -- --ignored`. Each test
//! uses a scratch `-L` server named per test + pid, killed on exit, so no
//! user server is ever touched.

use std::collections::BTreeMap;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fly_lib::pty::{PaneId, PtyManager, SpawnConfig};
use fly_lib::substrate::{Substrate, Tmux, TmuxConfig};

struct Scratch {
    flavor: String,
    dir: std::path::PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let flavor = format!("flyspike-{}-{}", tag, std::process::id());
        let dir = std::env::temp_dir().join(&flavor);
        std::fs::create_dir_all(&dir).unwrap();
        Self { flavor, dir }
    }
    fn substrate(&self) -> Arc<Substrate> {
        Arc::new(Substrate::new(
            self.flavor.clone(),
            self.dir.join("substrate-sessions.json"),
            self.dir.clone(),
            self.dir.join("hook.sock"),
            std::path::PathBuf::from(env!("CARGO_BIN_EXE_fly")),
        ))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let tmux = Tmux::new(TmuxConfig {
            socket_name: self.flavor.clone(),
            history_limit: 100,
        });
        let _ = tmux.kill_server();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn wait_for<F: FnMut() -> bool>(mut cond: F, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
#[ignore = "needs a tmux binary; run with -- --ignored"]
fn tmux_pane_output_input_resize_teardown_roundtrip() {
    let scratch = Scratch::new("u3");
    let substrate = scratch.substrate();

    let mgr = PtyManager::new();
    mgr.set_substrate(Arc::clone(&substrate));

    let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>();
    let (exit_tx, exit_rx) = mpsc::channel();
    let id = mgr.reserve_id();
    let cfg = SpawnConfig {
        command: Some(vec![
            "bash".into(),
            "--norc".into(),
            "--noprofile".into(),
        ]),
        leaf_key: Some("live-leaf.0".into()),
        env: vec![("FLY_PANE_TOKEN".into(), "ab".repeat(32))],
        rows: 24,
        cols: 80,
        ..Default::default()
    };
    mgr.spawn_with_id(
        id,
        cfg,
        "ab".repeat(32),
        Box::new(move |bytes: &[u8]| {
            let _ = out_tx.send(bytes.to_vec());
        }),
        Box::new(move |pane: PaneId, state| {
            let _ = exit_tx.send((pane, state));
        }),
    )
    .expect("tmux-backed spawn");

    // Session exists under the injective mark for the leaf key.
    let session = substrate.session_name("live-leaf.0");
    assert!(substrate.tmux().has_session(&session).unwrap());

    // The store holds the binding.
    let records =
        fly_lib::substrate::store::read_records(substrate.store_path());
    assert_eq!(records["live-leaf.0"].session_name, session);

    // Input via the send-hex writer: run a command, observe its output
    // through the pipe-pane FIFO.
    mgr.write(id, b"echo fly-roundtrip-$((6*7))\r").unwrap();
    let mut seen = String::new();
    assert!(
        wait_for(
            || {
                while let Ok(chunk) = out_rx.try_recv() {
                    seen.push_str(&String::from_utf8_lossy(&chunk));
                }
                seen.contains("fly-roundtrip-42")
            },
            Duration::from_secs(10)
        ),
        "echoed output should arrive through the FIFO; saw: {seen:?}"
    );

    // Resize drives the detached window grid (KTD2 manual mode).
    mgr.resize(id, 40, 120).unwrap();
    let dims = substrate
        .tmux()
        .display_message(&session, "#{window_width}x#{window_height}")
        .unwrap();
    assert_eq!(dims.trim(), "120x40");

    // Activity was recorded off the FIFO reads.
    assert!(mgr.pane_activity(id).is_some());

    // Teardown: session killed, store pruned, exit surfaced as Killed.
    mgr.close(id).unwrap();
    let (_, state) = exit_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        format!("{state:?}").contains("Killed"),
        true,
        "explicit close reports Killed, got {state:?}"
    );
    assert!(!substrate.tmux().has_session(&session).unwrap());
    assert!(
        fly_lib::substrate::store::read_records(substrate.store_path()).is_empty()
    );
}

#[test]
#[ignore = "needs a tmux binary; run with -- --ignored"]
fn tmux_pane_child_exit_surfaces_exited_state() {
    let scratch = Scratch::new("u3exit");
    let substrate = scratch.substrate();
    let mgr = PtyManager::new();
    mgr.set_substrate(Arc::clone(&substrate));

    let (exit_tx, exit_rx) = mpsc::channel();
    let id = mgr.reserve_id();
    let cfg = SpawnConfig {
        command: Some(vec!["sh".into(), "-c".into(), "sleep 0.3; exit 7".into()]),
        leaf_key: Some("exit-leaf".into()),
        rows: 24,
        cols: 80,
        ..Default::default()
    };
    mgr.spawn_with_id(
        id,
        cfg,
        "cd".repeat(32),
        Box::new(|_bytes: &[u8]| {}),
        Box::new(move |pane: PaneId, state| {
            let _ = exit_tx.send((pane, state));
        }),
    )
    .expect("spawn");

    // remain-on-exit (U4) keeps the session alive past the child's death;
    // the exit surfaces via pane_dead — through tmux's own pipe teardown or
    // the panes_status backstop, whichever lands first. Drive the backstop
    // the way the app's 1.5 s poll would.
    let mut state = None;
    for _ in 0..40 {
        let _ = mgr.panes_status(&[id]);
        if let Ok((_, s)) = exit_rx.recv_timeout(Duration::from_millis(250)) {
            state = Some(s);
            break;
        }
    }
    let state = state.expect("exit should surface via pane_dead within 10s");
    let dbg = format!("{state:?}");
    assert!(
        dbg.contains("Exited") && dbg.contains("code: 7"),
        "natural end reports Exited with pane_dead_status, got {dbg}"
    );

    // KTD4: the dead pane's session survives for its final screen.
    let session = substrate.session_name("exit-leaf");
    assert!(substrate.tmux().has_session(&session).unwrap());
}

#[test]
#[ignore = "needs a tmux binary; run with -- --ignored"]
fn server_env_is_scrubbed_of_claude_markers() {
    // The overlay trap (KTD3): the server's global env is every pane's
    // baseline, so the marker strip must hold THERE.
    std::env::set_var("CLAUDECODE", "1");
    let scratch = Scratch::new("u3env");
    let substrate = scratch.substrate();
    substrate.ensure_server().unwrap();
    let tmux = substrate.tmux();
    let mut env = BTreeMap::new();
    env.insert("FLY_PANE_TOKEN".to_string(), "ee".repeat(32));
    tmux.new_session(
        &substrate.session_name("envleaf"),
        "",
        &env,
        &["sh".to_string(), "-c".to_string(), "env > /tmp/../${TMPDIR:-/tmp}/fly-envleaf-$PPID.out; sleep 5".to_string()],
        80,
        24,
    )
    .unwrap();
    // Simpler assertion: read the server's own global environment.
    let out = std::process::Command::new("tmux")
        .args(["-L", &scratch.flavor, "show-environment", "-g"])
        .output()
        .unwrap();
    let global = String::from_utf8_lossy(&out.stdout);
    assert!(
        !global.lines().any(|l| l.starts_with("CLAUDECODE=")),
        "server global env must not carry CLAUDECODE; got:\n{global}"
    );
    std::env::remove_var("CLAUDECODE");
}

#[test]
#[ignore = "needs tmux + the fly binary; run with -- --ignored"]
fn pane_died_hook_reports_exit_over_the_socket() {
    // The full KTD12 chain, no poll assistance: child exits → tmux pane-died
    // hook → `fly substrate-event` (auth: server-scope token from the tmux
    // server env) → hook socket → substrate handler → force_dead → the
    // pane's 500 ms poll loop surfaces Exited{status}.
    let scratch = Scratch::new("u4hook");
    let substrate = scratch.substrate();
    let mgr = Arc::new(PtyManager::new());
    mgr.set_substrate(Arc::clone(&substrate));

    // A real HookServer at the scratch socket path with the substrate
    // handler wired exactly as lib.rs wires it.
    let tokens = Arc::new(fly_lib::hooks::TokenRegistry::new());
    let dispatch: fly_lib::hooks::Dispatch = Arc::new(|_pane, _hook| {});
    let sub_for_handler = Arc::clone(&substrate);
    let mgr_for_handler = Arc::clone(&mgr);
    let handler: fly_lib::hooks::SubstrateHandler = Arc::new(move |buf: &[u8]| {
        let Ok(ev) =
            serde_json::from_slice::<fly_lib::hooks::protocol::SubstrateEvent>(buf)
        else {
            return false;
        };
        if !sub_for_handler.validate_event_token(&ev.token) {
            return false;
        }
        if fly_lib::substrate::validate_session_name(&ev.session).is_err() {
            return true;
        }
        if ev.kind == "pane-died" {
            mgr_for_handler.force_dead_by_session(&ev.session, ev.status.unwrap_or(0));
        }
        true
    });
    let _server = fly_lib::hooks::HookServer::start_all(
        scratch.dir.join("hook.sock"),
        tokens,
        dispatch,
        None,
        None,
        None,
        Some(handler),
    )
    .expect("hook server");

    let (exit_tx, exit_rx) = mpsc::channel();
    let id = mgr.reserve_id();
    let cfg = SpawnConfig {
        command: Some(vec!["sh".into(), "-c".into(), "sleep 0.4; exit 3".into()]),
        leaf_key: Some("hook-leaf".into()),
        rows: 24,
        cols: 80,
        ..Default::default()
    };
    mgr.spawn_with_id(
        id,
        cfg,
        "ef".repeat(32),
        Box::new(|_bytes: &[u8]| {}),
        Box::new(move |pane: PaneId, state| {
            let _ = exit_tx.send((pane, state));
        }),
    )
    .expect("spawn");

    // NO panes_status driving — the hook must carry the exit end-to-end.
    let (_, state) = exit_rx
        .recv_timeout(Duration::from_secs(8))
        .expect("hook-driven exit should surface without any poll");
    let dbg = format!("{state:?}");
    assert!(
        dbg.contains("Exited") && dbg.contains("code: 3"),
        "hook chain delivers Exited{{3}}, got {dbg}"
    );
}
