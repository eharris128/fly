//! U2 PTY lifecycle contracts (R1, R2, R4). These drive `PtyManager` directly
//! with an mpsc sink, so they need no webview.
//!
//! Tests force `/bin/bash --norc --noprofile -i` for hermetic, dotfile-
//! independent behavior.

use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use fly_lib::pty::{OutputSink, PaneId, PtyManager, SpawnConfig};
use fly_lib::state::lifecycle::LifecycleState;

fn bash() -> SpawnConfig {
    SpawnConfig {
        shell: Some("/bin/bash".into()),
        args: vec!["--norc".into(), "--noprofile".into(), "-i".into()],
        ..Default::default()
    }
}

fn channel_sink() -> (OutputSink, Receiver<Vec<u8>>) {
    let (tx, rx) = mpsc::channel();
    let sink: OutputSink = Box::new(move |bytes: &[u8]| {
        let _ = tx.send(bytes.to_vec());
    });
    (sink, rx)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Accumulate output until `needle` appears or `timeout` elapses.
fn wait_for(rx: &Receiver<Vec<u8>>, needle: &[u8], timeout: Duration) -> Vec<u8> {
    let deadline = Instant::now() + timeout;
    let mut acc = Vec::new();
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(chunk) => {
                acc.extend_from_slice(&chunk);
                if find(&acc, needle).is_some() {
                    return acc;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    acc
}

fn poll_lifecycle<F: Fn(&LifecycleState) -> bool>(
    mgr: &PtyManager,
    id: PaneId,
    pred: F,
    timeout: Duration,
) -> Option<LifecycleState> {
    let deadline = Instant::now() + timeout;
    loop {
        let lc = mgr.lifecycle(id);
        if let Some(ref s) = lc {
            if pred(s) {
                return lc;
            }
        }
        if Instant::now() >= deadline {
            return lc;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn echo_executes_and_round_trips() {
    let mgr = PtyManager::new();
    let (sink, rx) = channel_sink();
    let id = mgr.spawn(bash(), "tok".into(), sink, Box::new(|_, _| {})).unwrap();

    // The computed value (not the literal echo of input) proves execution.
    mgr.write(id, b"echo FLYMARK$((6*7))\n").unwrap();
    let out = wait_for(&rx, b"FLYMARK42", Duration::from_secs(5));
    assert!(
        find(&out, b"FLYMARK42").is_some(),
        "expected computed output FLYMARK42, got: {}",
        String::from_utf8_lossy(&out)
    );
    mgr.close(id).unwrap();
}

#[test]
fn input_is_echoed_by_the_tty() {
    let mgr = PtyManager::new();
    let (sink, rx) = channel_sink();
    let id = mgr.spawn(bash(), "tok".into(), sink, Box::new(|_, _| {})).unwrap();

    mgr.write(id, b"ZZmarker").unwrap(); // no newline: pure tty echo
    let out = wait_for(&rx, b"ZZmarker", Duration::from_secs(5));
    assert!(find(&out, b"ZZmarker").is_some());
    mgr.close(id).unwrap();
}

/// End-to-end agent-dashboard backend on a real pane (U2/U3/U4): a plain bash
/// shell is not detected as an agent, and real output through the read thread
/// anchors a current work stretch.
#[test]
fn agent_detection_and_activity_on_a_real_pane() {
    let mgr = PtyManager::new();
    let (sink, rx) = channel_sink();
    let id = mgr.spawn(bash(), "tok".into(), sink, Box::new(|_, _| {})).unwrap();

    // Drive real output through the read thread.
    mgr.write(id, b"echo FLYACT$((2+3))\n").unwrap();
    let out = wait_for(&rx, b"FLYACT5", Duration::from_secs(5));
    assert!(find(&out, b"FLYACT5").is_some(), "expected command output");

    // A plain bash shell is not a Claude Code agent (U2, AE4), so its argv is
    // never captured into the resume store (U4, AE6).
    assert!(!mgr.is_agent(id), "bash must not be detected as an agent");
    assert_eq!(mgr.pane_command(id), None, "a bare shell's argv is not captured");

    // Output through the read thread recorded a current work stretch (U3/U4).
    let snap = mgr.pane_activity(id).expect("pane exists");
    assert!(
        snap.working_for_ms.is_some(),
        "recent output should anchor a current work stretch"
    );
    assert!(snap.last_output_ago_ms.is_some());

    mgr.close(id).unwrap();
}

#[test]
fn resize_propagates_winsize() {
    let mgr = PtyManager::new();
    let (sink, rx) = channel_sink();
    let id = mgr.spawn(bash(), "tok".into(), sink, Box::new(|_, _| {})).unwrap();

    // Distinctive geometry so a coincidental match is implausible.
    mgr.resize(id, 40, 137).unwrap();
    mgr.write(id, b"stty size\n").unwrap();
    let out = wait_for(&rx, b"40 137", Duration::from_secs(5));
    assert!(
        find(&out, b"40 137").is_some(),
        "expected stty to report 40 137, got: {}",
        String::from_utf8_lossy(&out)
    );
    mgr.close(id).unwrap();
}

#[test]
fn shell_exit_reaps_child_no_zombie() {
    let mgr = PtyManager::new();
    let (sink, _rx) = channel_sink();
    let id = mgr.spawn(bash(), "tok".into(), sink, Box::new(|_, _| {})).unwrap();

    mgr.write(id, b"exit\n").unwrap();
    // Lifecycle becomes Exited only after the read thread's wait() reaps the
    // child, so observing Exited proves the reap (no zombie).
    let lc = poll_lifecycle(&mgr, id, |s| s.is_terminal(), Duration::from_secs(5));
    match lc {
        Some(LifecycleState::Exited { code, .. }) => assert_eq!(code, 0),
        other => panic!("expected Exited{{code:0}}, got {other:?}"),
    }
    mgr.close(id).unwrap();
}

#[test]
fn close_kills_and_reaps_live_shell() {
    let mgr = PtyManager::new();
    let (sink, _rx) = channel_sink();
    let id = mgr.spawn(bash(), "tok".into(), sink, Box::new(|_, _| {})).unwrap();

    assert!(mgr.lifecycle(id).is_some());
    mgr.close(id).unwrap(); // must not hang; joins the reaping read thread
    assert!(mgr.lifecycle(id).is_none(), "pane should be removed after close");
    assert_eq!(mgr.count(), 0);
}

#[test]
fn binary_output_is_forwarded_verbatim() {
    let mgr = PtyManager::new();
    let (sink, rx) = channel_sink();
    let id = mgr.spawn(bash(), "tok".into(), sink, Box::new(|_, _| {})).unwrap();

    // Emit raw non-UTF-8 bytes; the read path must forward them verbatim and
    // never panic. (Any chunk boundary is safe because no decoding happens.)
    mgr.write(id, b"printf '\\xff\\xfe\\xfd'\n").unwrap();
    let out = wait_for(&rx, &[0xff, 0xfe, 0xfd], Duration::from_secs(5));
    assert!(
        find(&out, &[0xff, 0xfe, 0xfd]).is_some(),
        "expected raw bytes 0xff 0xfe 0xfd to be forwarded verbatim"
    );
    mgr.close(id).unwrap();
}

#[test]
fn close_during_flood_tears_down_in_order() {
    let mgr = PtyManager::new();
    let (sink, rx) = channel_sink();
    let id = mgr.spawn(bash(), "tok".into(), sink, Box::new(|_, _| {})).unwrap();

    // Start an unbounded flood, confirm it's producing, then close mid-stream.
    mgr.write(id, b"yes FLYFLOOD\n").unwrap();
    let out = wait_for(&rx, b"FLYFLOOD", Duration::from_secs(5));
    assert!(find(&out, b"FLYFLOOD").is_some(), "flood did not start");

    // close() must terminate the child and join the read thread without
    // hanging, even while output is in flight.
    mgr.close(id).unwrap();
    assert!(mgr.lifecycle(id).is_none());
}

#[test]
fn spawn_failure_is_reported() {
    let mgr = PtyManager::new();
    let (sink, _rx) = channel_sink();
    let cfg = SpawnConfig {
        shell: Some("/nonexistent/definitely-not-a-shell".into()),
        ..Default::default()
    };
    let res = mgr.spawn(cfg, "tok".into(), sink, Box::new(|_, _| {}));
    assert!(res.is_err(), "spawning a missing shell should fail");
}

#[test]
fn leaf_key_is_stored_and_returned_by_the_accessor() {
    // U3: the pane carries its frontend leaf key so the hook dispatch can key the
    // pane's resume record; a stale/ghost id resolves to None, never a panic.
    let mgr = PtyManager::new();
    let (sink, _rx) = channel_sink();
    let cfg = SpawnConfig {
        leaf_key: Some("leaf-42".into()),
        ..bash()
    };
    let id = mgr.spawn(cfg, "tok".into(), sink, Box::new(|_, _| {})).unwrap();
    assert_eq!(mgr.leaf_key(id).as_deref(), Some("leaf-42"));
    assert_eq!(mgr.leaf_key(PaneId(9999)), None);
    mgr.close(id).unwrap();
}
