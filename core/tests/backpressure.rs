//! U4 backpressure: pausing a pane parks its read thread (so the kernel PTY
//! buffer backpressures the producer), and resuming restarts it (R3).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fly_lib::pty::{OutputSink, PtyManager, SpawnConfig};

fn bash() -> SpawnConfig {
    SpawnConfig {
        shell: Some("/bin/bash".into()),
        args: vec!["--norc".into(), "--noprofile".into(), "-i".into()],
        ..Default::default()
    }
}

fn counting_sink() -> (OutputSink, Arc<AtomicUsize>) {
    let count = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&count);
    let sink: OutputSink = Box::new(move |bytes: &[u8]| {
        c.fetch_add(bytes.len(), Ordering::Relaxed);
    });
    (sink, count)
}

#[test]
fn pause_parks_reads_and_resume_restarts_them() {
    let mgr = PtyManager::new();
    let (sink, count) = counting_sink();
    let id = mgr
        .spawn(bash(), "t".into(), sink, Box::new(|_, _| {}))
        .unwrap();

    // Start a high-rate flood.
    mgr.write(id, b"yes FLOOD_FLOOD_FLOOD\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while count.load(Ordering::Relaxed) < 200_000 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(count.load(Ordering::Relaxed) > 0, "flood produced no output");

    // Pause and let the in-flight read + kernel buffer drain.
    mgr.pause(id).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let after_pause = count.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(400));
    let still_paused = count.load(Ordering::Relaxed);

    // While parked, only a bounded amount can trickle (the kernel PTY buffer);
    // the flood rate is MB/s, so continued reading would add megabytes.
    let growth = still_paused - after_pause;
    assert!(
        growth < 256 * 1024,
        "reads did not park: {growth} bytes during pause"
    );

    // Resume restarts the flood.
    mgr.resume(id).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let after_resume = count.load(Ordering::Relaxed);
    assert!(
        after_resume > still_paused + 256 * 1024,
        "resume did not restart reads"
    );

    mgr.close(id).unwrap(); // must tear down cleanly even from a paused state
}
