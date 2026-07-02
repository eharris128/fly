//! Alert surfacing for non-silent script runs (U6 of
//! `docs/plans/2026-07-01-002-feat-automations-plan.md`; R16, R17, R18).
//!
//! An **alert-classified** script run (exit 0 with non-silent stdout, no
//! `{"wakeAgent": false}` sentinel — the R15 wake gate in [`super::script`])
//! reaches the user through two surfaces this module owns:
//!
//! - **the alerts log** (R16): a single append-only file
//!   (`automation-alerts.log` under the app data dir, `0600` in a `0700` dir)
//!   holding one `[name] first-line` line per alert, **sanitized at write
//!   time** via [`crate::notify::sanitize_title`] / [`sanitize_body`], which
//!   strip control characters *including newlines* — so a script cannot forge
//!   a multi-line entry (the only newline in a record is the one this module
//!   appends). The log is truncated to a 64 KiB tail at startup so it never
//!   grows without bound; the frontend `tail -f`s it in the sink pane.
//!
//! - **the attention ring** (R18): a `Signal { reason: Alert, tier: Cli }`
//!   raised on the **sink pane** — a background "Automations" tab the frontend
//!   opens on demand. Alerts that arrive *before* that pane registers are held
//!   in a bounded **pending queue** (R17) and drained (each re-raised) the
//!   moment the pane registers. The raise itself lives in `lib.rs` (it needs
//!   the attention manager + app handle); this module owns only the log, the
//!   queue, and the sink registry.
//!
//! **Locking.** [`AlertsLog`] carries its own small mutex over the sink id +
//! pending queue; it is **independent of the automations store lock** (KTD-B).
//! The alert sink closure runs on the script reaper thread and touches only
//! this lock + the log file, never the store — so appending or queuing an
//! alert can never contend with (or deadlock against) a sweep/claim flush.

use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::notify::{sanitize_body, sanitize_title};

/// The alerts log file name under the app data dir (the `session::data_dir`
/// convention — honors `FLY_APP_NAME`, same root the store uses).
const ALERTS_LOG_FILE: &str = "automation-alerts.log";

/// Startup truncate target: keep at most this many bytes (the trailing tail),
/// cut at a line boundary. Bounds an unbounded-growth log for a long-lived
/// install. Byte math here is saturating (release builds have overflow checks
/// off — the `release-overflow-checks-off` lesson).
const ALERTS_LOG_TAIL_CAP: usize = 64 * 1024;

/// Bound on the pending queue (R17): if the sink pane never registers (e.g. a
/// headless run where scripts fire but the webview never loads), alerts would
/// otherwise accumulate forever. At the cap the oldest queued alert is dropped
/// — the ring is per-pane binary anyway, so losing an old queued raise only
/// costs one redundant ring on eventual registration.
const MAX_PENDING: usize = 256;

/// One queued alert awaiting a sink pane (R17). Carries the same fields the log
/// records so a drained alert could later enrich its raise; today the raise is
/// a pane-level ring, so the payload is retained for the log/tests and future
/// use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedAlert {
    pub automation_name: String,
    pub first_line: String,
}

/// Sink id + pending queue under one mutex (see the module doc).
#[derive(Default)]
struct SinkState {
    /// The registered sink pane, or `None` before the frontend opens the
    /// Automations tab (or after it exits).
    sink_pane: Option<u64>,
    /// Alerts that arrived while `sink_pane` was `None` (R17).
    pending: Vec<QueuedAlert>,
}

/// The alerts log + sink registry (U6). Shared as `Arc<AlertsLog>`: the sink
/// closure appends + queues from the reaper thread, the `register_alert_sink`
/// command drains on the main thread, and the pane-exit tap clears the sink.
pub struct AlertsLog {
    path: PathBuf,
    state: Mutex<SinkState>,
}

impl AlertsLog {
    /// Construct over an explicit path (no I/O — tests point this at a
    /// tempdir). Call [`AlertsLog::startup_truncate`] once at app start.
    pub fn new(path: PathBuf) -> AlertsLog {
        AlertsLog {
            path,
            state: Mutex::new(SinkState::default()),
        }
    }

    /// Open the default log (`automation-alerts.log` under the app data dir)
    /// and truncate its tail. Never fails: a broken log must not stop the app.
    pub fn open_default() -> Arc<AlertsLog> {
        let log = Arc::new(AlertsLog::new(default_path()));
        log.startup_truncate();
        log
    }

    /// The log file path (the frontend `tail -f`s it; carried in the
    /// `automation://alert-pending` event).
    pub fn log_path(&self) -> &Path {
        &self.path
    }

    // ---- the log (R16) --------------------------------------------------------

    /// Append one sanitized `[name] first-line` record (R16). Sanitizing here —
    /// at write time — strips control characters *including newlines*, so the
    /// only newline in the record is the trailing one this method adds: a
    /// script's stdout can never forge a second log line. Best-effort: a write
    /// error is logged, never surfaced (the ring still fires).
    pub fn append(&self, name: &str, first_line: &str) {
        let line = format!("[{}] {}\n", sanitize_title(name), sanitize_body(first_line));
        if let Err(e) = self.append_bytes(line.as_bytes()) {
            eprintln!(
                "[fly-automations] could not append alert to {} ({e})",
                self.path.display()
            );
        }
    }

    /// Append with `O_APPEND` (atomic per-write on Linux for records this
    /// small, so concurrent reaper appends never interleave) into a `0600` file
    /// in a `0700` dir, both asserted here rather than left to umask.
    fn append_bytes(&self, bytes: &[u8]) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            create_private_dir(parent)?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)?;
        f.write_all(bytes)
    }

    /// Truncate the log to its trailing [`ALERTS_LOG_TAIL_CAP`] bytes at a line
    /// boundary (so it never starts mid-record). Missing file is fine; errors
    /// are logged, not surfaced.
    pub fn startup_truncate(&self) {
        if let Err(e) = self.truncate_to_tail() {
            eprintln!(
                "[fly-automations] could not truncate alerts log {} ({e})",
                self.path.display()
            );
        }
    }

    fn truncate_to_tail(&self) -> io::Result<()> {
        let bytes = match std::fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        if bytes.len() <= ALERTS_LOG_TAIL_CAP {
            return Ok(());
        }
        // Saturating byte math (overflow checks are off in release).
        let start = bytes.len().saturating_sub(ALERTS_LOG_TAIL_CAP);
        let tail = &bytes[start..];
        // Advance past the first partial line so the file starts on a record.
        let cut = tail
            .iter()
            .position(|&b| b == b'\n')
            .map(|i| i.saturating_add(1))
            .unwrap_or(0);
        let tail = &tail[cut.min(tail.len())..];
        write_atomic_owner_only(&self.path, tail)
    }

    // ---- pending queue + sink registry (R17) ---------------------------------

    /// Atomic check-and-queue for the reaper's sink closure: return the
    /// registered sink pane (the caller raises), or `None` after queuing the
    /// alert for later (the caller asks the frontend to open the sink pane).
    /// Doing it under one lock hold closes the window where a sink could
    /// register between a `has_sink` check and a `queue_alert`.
    pub fn sink_or_queue(&self, alert: QueuedAlert) -> Option<u64> {
        let mut st = self.state.lock().unwrap();
        match st.sink_pane {
            Some(pane) => Some(pane),
            None => {
                if st.pending.len() >= MAX_PENDING {
                    st.pending.remove(0); // drop oldest (see MAX_PENDING)
                }
                st.pending.push(alert);
                None
            }
        }
    }

    /// Queue an alert unconditionally (R17). [`AlertsLog::sink_or_queue`] is the
    /// path the sink closure uses; this is the explicit primitive for tests.
    pub fn queue_alert(&self, alert: QueuedAlert) {
        let mut st = self.state.lock().unwrap();
        if st.pending.len() >= MAX_PENDING {
            st.pending.remove(0);
        }
        st.pending.push(alert);
    }

    /// Register the sink pane and **atomically drain** the pending backlog
    /// (R17): the drained alerts are returned so `lib.rs` re-raises one ring per
    /// alert that arrived before the pane existed. Setting the id and taking the
    /// queue under one lock hold means an alert concluding mid-registration
    /// either lands in the returned backlog or rings the just-set pane — never
    /// lost.
    pub fn register_sink(&self, pane_id: u64) -> Vec<QueuedAlert> {
        let mut st = self.state.lock().unwrap();
        st.sink_pane = Some(pane_id);
        std::mem::take(&mut st.pending)
    }

    /// Clear the sink **only if** `pane_id` is the current sink (called from the
    /// pane-exit tap for every pane, so it must ignore unrelated exits). A later
    /// alert then re-queues and re-opens a fresh sink pane.
    pub fn clear_sink_if(&self, pane_id: u64) {
        let mut st = self.state.lock().unwrap();
        if st.sink_pane == Some(pane_id) {
            st.sink_pane = None;
        }
    }

    /// Whether a sink pane is currently registered.
    pub fn has_sink(&self) -> bool {
        self.state.lock().unwrap().sink_pane.is_some()
    }

    /// Drain the pending queue without registering a sink (tests).
    pub fn drain_pending(&self) -> Vec<QueuedAlert> {
        std::mem::take(&mut self.state.lock().unwrap().pending)
    }
}

/// The default alerts-log path under the app data root.
fn default_path() -> PathBuf {
    crate::session::data_dir().join(ALERTS_LOG_FILE)
}

// ---- private-file helpers (mirror store.rs; kept local, R16) -----------------

/// Temp + chmod-0600 + rename in `path`'s own dir (atomic replace). Mirrors
/// `store::write_atomic_owner_only` — recreated locally rather than sharing so
/// neither module reaches into the other's privates.
fn write_atomic_owner_only(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, path)
}

/// `create_dir_all` + explicit `0700` (never left to umask).
fn create_private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn qa(name: &str, line: &str) -> QueuedAlert {
        QueuedAlert {
            automation_name: name.into(),
            first_line: line.into(),
        }
    }

    // R16: the append sanitizes at write time — ANSI/OSC escapes and embedded
    // newlines are stripped, so a record is exactly one line and carries no
    // control characters. This is the forged-multi-line-entry guard.
    #[test]
    fn append_sanitizes_control_chars_and_newlines_r16() {
        let dir = tempfile::tempdir().unwrap();
        let log = AlertsLog::new(dir.path().join("automation-alerts.log"));

        log.append(
            "watch\x1b[31m disk\nEVIL",
            "Disk\x07 at 93%\n[fake] injected line",
        );
        let content = std::fs::read_to_string(log.log_path()).unwrap();

        assert_eq!(
            content.matches('\n').count(),
            1,
            "exactly the one trailing newline — no forged second line: {content:?}"
        );
        assert!(content.ends_with('\n'));
        assert!(
            !content.contains('\x1b') && !content.contains('\x07'),
            "control chars stripped: {content:?}"
        );
        assert!(content.starts_with('['), "record is [name] first-line: {content:?}");
        // The escape's ESC byte is gone; its printable residue may remain, but
        // the injected newline is what mattered (only one newline above).
        assert!(content.contains("Disk"));

        // 0600 file in a 0700 dir.
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(log.log_path()), 0o600);
        assert_eq!(mode(dir.path()), 0o700);
    }

    // R17: alerts queued before a sink registers drain exactly once on
    // registration; a second registration drains nothing.
    #[test]
    fn pending_drains_once_on_sink_registration_r17() {
        let dir = tempfile::tempdir().unwrap();
        let log = AlertsLog::new(dir.path().join("automation-alerts.log"));

        assert!(!log.has_sink());
        assert_eq!(log.sink_or_queue(qa("a", "1")), None, "no sink → queued");
        assert_eq!(log.sink_or_queue(qa("b", "2")), None);

        let drained = log.register_sink(7);
        assert_eq!(drained, vec![qa("a", "1"), qa("b", "2")], "FIFO drain");
        assert!(log.has_sink());
        assert!(
            log.register_sink(7).is_empty(),
            "re-registering drains nothing new"
        );

        // With a sink registered, an alert rings the pane and is not queued.
        assert_eq!(log.sink_or_queue(qa("c", "3")), Some(7));
        assert!(log.drain_pending().is_empty(), "sink path never queued");
    }

    // R17: the sink clears only for the matching pane's exit — an unrelated
    // pane exit leaves the sink in place; the sink pane's own exit clears it.
    #[test]
    fn sink_clears_on_matching_pane_exit_only_r17() {
        let dir = tempfile::tempdir().unwrap();
        let log = AlertsLog::new(dir.path().join("automation-alerts.log"));

        log.register_sink(7);
        assert!(log.has_sink());
        log.clear_sink_if(6); // a different pane exited
        assert!(log.has_sink(), "unrelated exit doesn't clear the sink");
        log.clear_sink_if(7); // the sink pane exited
        assert!(!log.has_sink());

        // After clearing, alerts queue again and a fresh registration drains them.
        assert_eq!(log.sink_or_queue(qa("d", "4")), None);
        assert_eq!(log.register_sink(9), vec![qa("d", "4")]);
    }

    // Startup truncate keeps the trailing 64 KiB at a line boundary — the file
    // shrinks, the tail is preserved byte-for-byte, and it starts on a record.
    #[test]
    fn startup_truncate_keeps_line_aligned_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("automation-alerts.log");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        // ~110 KiB of 11-byte records, well over the 64 KiB cap.
        let mut big = String::new();
        for i in 0..10_000 {
            big.push_str(&format!("[w] {i:05}\n"));
        }
        std::fs::write(&path, &big).unwrap();

        let log = AlertsLog::new(path.clone());
        log.startup_truncate();

        let out = std::fs::read(&path).unwrap();
        assert!(out.len() <= ALERTS_LOG_TAIL_CAP, "truncated to the cap");
        assert!(!out.is_empty(), "kept a non-empty tail");
        assert!(
            big.as_bytes().ends_with(&out),
            "the tail is preserved byte-for-byte"
        );
        // Starts on a record boundary: the byte before the kept tail is a '\n'.
        let cut_at = big.len() - out.len();
        assert!(cut_at > 0 && big.as_bytes()[cut_at - 1] == b'\n', "line-aligned cut");
        assert!(out.ends_with(b"\n"), "ends on a full record");

        // A file already under the cap is left untouched.
        let small = dir.path().join("small.log");
        std::fs::write(&small, b"[w] hi\n").unwrap();
        let log2 = AlertsLog::new(small.clone());
        log2.startup_truncate();
        assert_eq!(std::fs::read(&small).unwrap(), b"[w] hi\n");
    }

    // MAX_PENDING bounds the queue: past the cap the oldest is dropped, newest
    // survive, and the count never exceeds the cap.
    #[test]
    fn pending_queue_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let log = AlertsLog::new(dir.path().join("automation-alerts.log"));
        for i in 0..(MAX_PENDING + 10) {
            log.queue_alert(qa("w", &i.to_string()));
        }
        let drained = log.register_sink(1);
        assert_eq!(drained.len(), MAX_PENDING, "capped at MAX_PENDING");
        assert_eq!(drained.last().unwrap().first_line, (MAX_PENDING + 9).to_string());
        assert_eq!(drained.first().unwrap().first_line, "10", "oldest dropped");
    }
}
