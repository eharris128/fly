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
        command: Some(vec!["sh".into(), "-c".into(), "sleep 0.3".into()]),
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

    let (_, state) = exit_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(
        format!("{state:?}").contains("Exited"),
        "natural end reports Exited, got {state:?}"
    );
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
