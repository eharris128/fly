//! Per-pane output coalescing between the PTY read thread and the Tauri
//! channel (T1 of `docs/notes/2026-07-23-performance-audit-follow-ups.md`).
//!
//! Why this exists: tauri 2.11.3 re-encodes a `Raw` channel chunk **< 1024
//! bytes** as a JSON decimal-number array (~3.4× expansion) inside an `eval()`
//! parsed on the webview main thread, and interactive TUI output is exactly
//! many small writes (measured: 60/60 reads under 1024 B during spinner
//! repaints, median 49 B). With several busy agents that per-chunk cost
//! saturates the single webview main thread — visible as multi-second input
//! lag and, on a workspace switch, queued keystrokes retargeting to the newly
//! focused pane. Batching here collapses the storm to a few messages per pane
//! per second and pushes most traffic onto the ≥ 1 KiB raw path. Batching must
//! live in this sink, not the read buffer: the kernel PTY layer caps a read at
//! ~2–8 KiB regardless of fly's buffer size (see `pty/pane.rs::READ_BUF`).
//!
//! Visibility-aware: a **visible** pane flushes on a ~[`VISIBLE_FLUSH`]
//! deadline (imperceptible on a keystroke echo), a **hidden** one on
//! ~[`HIDDEN_FLUSH`] — so main-thread cost tracks the visible set while every
//! hidden pane's xterm still receives (and VT-parses) every byte, preserving
//! buffer state, the frontend watermark backpressure (KTD4), and
//! guided-handoff readiness. Revealing a pane wakes its parked flush so
//! buffered output renders immediately on switch-in.
//!
//! Ordering: one forwarder thread per pane, one channel — the byte stream
//! stays ordered (KTD3's single-ordered-path invariant). Lossless: bytes are
//! only ever moved, never dropped; `close` drains before returning so a
//! pane's final output precedes its exit event.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Flush immediately once this much is buffered, deadline notwithstanding.
/// Matches the PTY read buffer: a flood fills it faster than any deadline.
const MAX_BUFFER: usize = 64 * 1024;
/// Flush deadline while the pane is visible. Bounded by perception: ≤ 4 ms
/// added echo latency is invisible; 20 Hz spinner repaints still coalesce
/// ~arrival-adjacent writes into one ≥ 1 KiB message.
const VISIBLE_FLUSH: Duration = Duration::from_millis(4);
/// Flush deadline while the pane is hidden (not in the active tab's leaf
/// set). Hidden panes have no reader; 4 flushes/s keeps their xterm buffer
/// and side signals current at ~1% of the per-chunk message cost.
const HIDDEN_FLUSH: Duration = Duration::from_millis(250);
/// Upper bound on how long `close` waits for the forwarder to drain — a
/// safety valve against a wedged channel send, not an expected path.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

/// The flush deadline for a pane in the given visibility state.
fn flush_delay(visible: bool) -> Duration {
    if visible {
        VISIBLE_FLUSH
    } else {
        HIDDEN_FLUSH
    }
}

struct State {
    buf: Vec<u8>,
    closed: bool,
    /// True while the forwarder is sending a swapped-out chunk, so `close`
    /// waits for the send in flight, not just an empty buffer.
    inflight: bool,
}

struct Shared {
    state: Mutex<State>,
    cv: Condvar,
    /// Read on every deadline computation; toggled by the visible-set push.
    /// Relaxed: a stale read is corrected within one flush interval.
    visible: AtomicBool,
}

/// One pane's coalescing sink: the read thread `push`es raw chunks, a
/// dedicated forwarder thread flushes them to the Tauri channel on a
/// visibility-dependent deadline.
pub struct Coalescer {
    shared: Arc<Shared>,
}

impl Coalescer {
    /// Start a coalescer whose forwarder delivers batched chunks via `send`.
    /// New panes start visible (the frontend replicates the real visible set
    /// on spawn, and fast-flush is the safe default until it does).
    pub fn spawn(pane: u64, send: impl Fn(Vec<u8>) + Send + 'static) -> Arc<Coalescer> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                buf: Vec::new(),
                closed: false,
                inflight: false,
            }),
            cv: Condvar::new(),
            visible: AtomicBool::new(true),
        });
        let thread_shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name(format!("fly-coal-{pane}"))
            .spawn(move || run(&thread_shared, send))
            .expect("spawn coalescer thread");
        Arc::new(Coalescer { shared })
    }

    /// Append a chunk from the read thread. Never blocks on the channel; a
    /// push after `close` is dropped (the pane is already torn down).
    pub fn push(&self, bytes: &[u8]) {
        let mut st = self.shared.state.lock().unwrap();
        if st.closed {
            return;
        }
        st.buf.extend_from_slice(bytes);
        self.shared.cv.notify_all();
    }

    /// Update visibility. Turning visible wakes a parked hidden-deadline wait
    /// so buffered output renders immediately on switch-in.
    pub fn set_visible(&self, visible: bool) {
        let was = self.shared.visible.swap(visible, Ordering::Relaxed);
        if visible && !was {
            self.shared.cv.notify_all();
        }
    }

    /// Stop accepting input and wait (bounded) until everything already
    /// pushed has been sent. Called from the pane's exit path *before* the
    /// exit event is emitted, so the final output precedes the exit note.
    pub fn close(&self) {
        let mut st = self.shared.state.lock().unwrap();
        st.closed = true;
        self.shared.cv.notify_all();
        let deadline = Instant::now() + DRAIN_TIMEOUT;
        while !st.buf.is_empty() || st.inflight {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let (guard, _) = self.shared.cv.wait_timeout(st, deadline - now).unwrap();
            st = guard;
        }
    }

    #[cfg(test)]
    fn is_visible(&self) -> bool {
        self.shared.visible.load(Ordering::Relaxed)
    }
}

/// The forwarder loop: wait for data, hold it until the visibility deadline
/// (or a size trip / close), swap the buffer out, send outside the lock.
fn run(shared: &Shared, send: impl Fn(Vec<u8>)) {
    let mut st = shared.state.lock().unwrap();
    loop {
        while st.buf.is_empty() && !st.closed {
            st = shared.cv.wait(st).unwrap();
        }
        if st.buf.is_empty() {
            return; // closed and drained
        }
        // First byte of this batch anchors the deadline; the delay is
        // re-read each wake so a hidden→visible flip flushes promptly.
        let anchor = Instant::now();
        loop {
            if st.closed || st.buf.len() >= MAX_BUFFER {
                break;
            }
            let target = anchor + flush_delay(shared.visible.load(Ordering::Relaxed));
            let now = Instant::now();
            if now >= target {
                break;
            }
            let (guard, _) = shared.cv.wait_timeout(st, target - now).unwrap();
            st = guard;
        }
        let chunk = std::mem::take(&mut st.buf);
        st.inflight = true;
        drop(st);
        send(chunk);
        st = shared.state.lock().unwrap();
        st.inflight = false;
        shared.cv.notify_all(); // wake a `close` waiting on the drain
    }
}

/// All live panes' coalescers, keyed by pane id, so the visible-set push
/// (`stream::set_visible_panes`) can retune every pane's flush deadline.
/// Entries are inserted before the pane spawns and removed on its exit path,
/// so removal always follows insertion.
#[derive(Default)]
pub struct CoalescerRegistry {
    map: Mutex<HashMap<u64, Arc<Coalescer>>>,
}

impl CoalescerRegistry {
    pub fn insert(&self, pane: u64, coalescer: Arc<Coalescer>) {
        self.map.lock().unwrap().insert(pane, coalescer);
    }

    pub fn remove(&self, pane: u64) {
        self.map.lock().unwrap().remove(&pane);
    }

    /// Mark exactly `visible` as visible; every other registered pane goes
    /// hidden. Mirrors the attention manager's visible-set replication (U17).
    pub fn set_visible_panes(&self, visible: &[u64]) {
        let map = self.map.lock().unwrap();
        for (id, coalescer) in map.iter() {
            coalescer.set_visible(visible.contains(id));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// A collecting sink: every flushed chunk, in order.
    fn collector() -> (Arc<StdMutex<Vec<Vec<u8>>>>, impl Fn(Vec<u8>) + Send + 'static) {
        let got = Arc::new(StdMutex::new(Vec::new()));
        let sink_got = Arc::clone(&got);
        (got, move |chunk: Vec<u8>| sink_got.lock().unwrap().push(chunk))
    }

    fn concat(chunks: &[Vec<u8>]) -> Vec<u8> {
        chunks.iter().flatten().copied().collect()
    }

    #[test]
    fn delivers_all_bytes_in_order() {
        let (got, sink) = collector();
        let c = Coalescer::spawn(1, sink);
        c.push(b"hello ");
        c.push(b"world");
        c.close();
        assert_eq!(concat(&got.lock().unwrap()), b"hello world");
    }

    #[test]
    fn close_drains_before_returning() {
        let (got, sink) = collector();
        let c = Coalescer::spawn(2, sink);
        c.set_visible(false); // park on the slow deadline
        c.push(b"final bytes");
        c.close(); // must not return until the forwarder flushed
        assert_eq!(concat(&got.lock().unwrap()), b"final bytes");
    }

    #[test]
    fn push_after_close_is_dropped() {
        let (got, sink) = collector();
        let c = Coalescer::spawn(3, sink);
        c.push(b"kept");
        c.close();
        c.push(b"dropped");
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(concat(&got.lock().unwrap()), b"kept");
    }

    #[test]
    fn size_trip_flushes_a_hidden_pane_before_its_deadline() {
        let (got, sink) = collector();
        let c = Coalescer::spawn(4, sink);
        c.set_visible(false);
        let start = Instant::now();
        c.push(&vec![b'x'; MAX_BUFFER]);
        // Poll: the flood must arrive well before the 250 ms hidden deadline.
        while got.lock().unwrap().is_empty() {
            assert!(
                start.elapsed() < Duration::from_millis(150),
                "size trip did not preempt the hidden deadline"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        c.close();
        assert_eq!(concat(&got.lock().unwrap()).len(), MAX_BUFFER);
    }

    #[test]
    fn reveal_wakes_a_parked_hidden_flush() {
        let (got, sink) = collector();
        let c = Coalescer::spawn(5, sink);
        c.set_visible(false);
        c.push(b"buffered while hidden");
        std::thread::sleep(Duration::from_millis(20)); // parked on ~250 ms
        assert!(got.lock().unwrap().is_empty(), "flushed before reveal");
        let revealed = Instant::now();
        c.set_visible(true);
        while got.lock().unwrap().is_empty() {
            assert!(
                revealed.elapsed() < Duration::from_millis(150),
                "reveal did not wake the parked flush"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        c.close();
        assert_eq!(concat(&got.lock().unwrap()), b"buffered while hidden");
    }

    #[test]
    fn registry_marks_only_the_visible_set() {
        let reg = CoalescerRegistry::default();
        let (_g1, s1) = collector();
        let (_g2, s2) = collector();
        let a = Coalescer::spawn(10, s1);
        let b = Coalescer::spawn(11, s2);
        reg.insert(10, Arc::clone(&a));
        reg.insert(11, Arc::clone(&b));
        reg.set_visible_panes(&[11]);
        assert!(!a.is_visible());
        assert!(b.is_visible());
        reg.set_visible_panes(&[10]);
        assert!(a.is_visible());
        assert!(!b.is_visible());
        a.close();
        b.close();
    }
}
