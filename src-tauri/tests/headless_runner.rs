//! U3 of `docs/plans/2026-07-11-003-feat-headless-monitor-checks-plan.md`:
//! the headless runner end-to-end against fake-claude fixture scripts
//! (`tests/fixtures/headless/`) — spawn shape (R1), env hygiene (R13), the
//! monotonic deadline (R4), the stream-end policy, and the SIGTERM-first
//! kill-and-sweep discipline (R5, incl. the setsid-grandchild case group
//! kills cannot reach). Every kill scenario asserts on the live process
//! table: no fixture process survives the discipline.
//!
//! The closer seam is the sync point: `run()` returns immediately (the
//! sweep must never block), so tests wait on the collector the closer
//! feeds — the `script.rs` harness shape.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fly_lib::automations::headless::{CheckOutcome, HeadlessRunner, HeadlessTiming};
use fly_lib::automations::ResolvedLaunch;

/// Absolute path of a fixture script (executable `sh`, checked in with the
/// exec bit — they stand in for the claude binary).
fn fixture(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/headless")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

/// Fast timings for tests that never exercise the deadline.
fn fast_timing() -> HeadlessTiming {
    HeadlessTiming {
        deadline: Duration::from_secs(30),
        linger_exit_grace: Duration::from_millis(250),
        term_grace: Duration::from_secs(2),
        seam_grace: Duration::from_millis(200),
        poll: Duration::from_millis(10),
    }
}

type Closed = (String, String, CheckOutcome);

struct Harness {
    runner: Arc<HeadlessRunner>,
    closed: Arc<Mutex<Vec<Closed>>>,
    dir: tempfile::TempDir,
}

fn harness(fixture_name: &str, timing: HeadlessTiming) -> Harness {
    let closed: Arc<Mutex<Vec<Closed>>> = Arc::new(Mutex::new(Vec::new()));
    let c = Arc::clone(&closed);
    let runner = Arc::new(HeadlessRunner::with_config(
        Arc::new(move |aid: &str, rid: &str, outcome: CheckOutcome| {
            c.lock().unwrap().push((aid.into(), rid.into(), outcome));
        }),
        fixture(fixture_name),
        timing,
    ));
    Harness {
        runner,
        closed,
        dir: tempfile::tempdir().unwrap(),
    }
}

impl Harness {
    fn cwd(&self) -> String {
        self.dir.path().to_string_lossy().into_owned()
    }

    /// Wait (bounded) for the first closure and return it.
    fn one_closed(&self, timeout: Duration) -> Closed {
        let start = Instant::now();
        loop {
            {
                let closed = self.closed.lock().unwrap();
                if let Some(first) = closed.first() {
                    return first.clone();
                }
            }
            assert!(
                start.elapsed() < timeout,
                "timed out waiting for the closer to fire"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn infra_reason(outcome: CheckOutcome) -> String {
    match outcome {
        CheckOutcome::Infra { reason } => reason,
        other => panic!("expected Infra, got {other:?}"),
    }
}

/// Whether any live process's cmdline contains `marker` (the survivor scan
/// for kill tests; markers ride the prompt — the runner's final positional —
/// so they land in fixture argv, never in this test binary's own cmdline).
fn cmdline_marker_alive(marker: &str) -> bool {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in dir.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if let Ok(raw) = std::fs::read(format!("/proc/{name}/cmdline")) {
            if raw
                .windows(marker.len())
                .any(|w| w == marker.as_bytes())
            {
                return true;
            }
        }
    }
    false
}

/// Poll (bounded, 5s) until `still_there` reads false — the "no zombies, no
/// orphans" assertion, tolerant of the short reparent-and-reap window.
fn wait_gone(still_there: impl Fn() -> bool, what: &str) {
    let end = Instant::now() + Duration::from_secs(5);
    while still_there() {
        assert!(
            Instant::now() < end,
            "{what} still present after the kill discipline"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// The pids a fixture recorded into its cwd (`pids.txt`).
fn recorded_pids(dir: &Path) -> Vec<u32> {
    let text = std::fs::read_to_string(dir.join("pids.txt"))
        .expect("fixture wrote pids.txt before blocking");
    text.lines().filter_map(|l| l.trim().parse().ok()).collect()
}

fn assert_pids_gone(pids: &[u32]) {
    assert!(!pids.is_empty(), "fixture recorded at least one pid");
    for &pid in pids {
        wait_gone(
            || Path::new(&format!("/proc/{pid}")).exists(),
            &format!("fixture process {pid}"),
        );
    }
}

// ---- happy paths -------------------------------------------------------------------

// Scenario 1: the captured real stream shape with a PASS verdict fence →
// the closer receives Clean with the EXACT text and the init session id
// (verdict parsing/retire is U4's; nothing here interprets the fence).
#[test]
fn happy_verdict_stream_closes_clean_with_exact_text_and_session_id() {
    let h = harness("happy.sh", fast_timing());
    h.runner
        .run("auto-1", "r1", &h.cwd(), "check the experiment", &ResolvedLaunch::default());

    let (aid, rid, outcome) = h.one_closed(Duration::from_secs(10));
    assert_eq!((aid.as_str(), rid.as_str()), ("auto-1", "r1"));
    assert_eq!(
        outcome,
        CheckOutcome::Clean {
            text: "Experiment done.\n\n```verdict\nstatus: PASS\nsummary: converged\n```"
                .to_owned(),
            session_id: Some("11111111-2222-3333-4444-555555555555".to_owned()),
        }
    );
    assert_eq!(h.runner.inflight_count(), 0, "entry removed on close");
}

// Scenario 2: a success result with no verdict block is still Clean with
// its text (a healthy not-done check; retire semantics are U4's).
#[test]
fn no_verdict_success_result_closes_clean_with_text() {
    let h = harness("no-verdict.sh", fast_timing());
    h.runner
        .run("auto-nv", "r1", &h.cwd(), "look around", &ResolvedLaunch::default());

    let (_, _, outcome) = h.one_closed(Duration::from_secs(10));
    assert_eq!(
        outcome,
        CheckOutcome::Clean {
            text: "All quiet; nothing to report.".to_owned(),
            session_id: Some("s-noverdict".to_owned()),
        }
    );
}

// Scenario 8: a result line far larger than the 8 KiB read buffer, packed
// with multibyte characters and with one character flushed split across two
// writes, survives byte-exact (bytes-per-line parsing, R11's no-lossy rule).
#[test]
fn utf8_split_across_read_chunks_inside_a_line_survives_intact() {
    let h = harness("utf8-split.sh", fast_timing());
    h.runner
        .run("auto-utf8", "r1", &h.cwd(), "emit unicode", &ResolvedLaunch::default());

    let (_, _, outcome) = h.one_closed(Duration::from_secs(10));
    let expected = format!("{}🚀", "héllo☃世界—".repeat(800));
    assert!(expected.len() > 8 * 1024, "line must cross the read buffer");
    assert_eq!(
        outcome,
        CheckOutcome::Clean {
            text: expected,
            session_id: Some("s-utf8".to_owned()),
        }
    );
}

// ---- infra paths -------------------------------------------------------------------

// Scenario 3: garbage lines then exit 0 — EOF with no result event → Infra.
#[test]
fn malformed_stream_with_exit_zero_is_infra() {
    let h = harness("garbage.sh", fast_timing());
    h.runner
        .run("auto-garbage", "r1", &h.cwd(), "speak json", &ResolvedLaunch::default());

    let (_, _, outcome) = h.one_closed(Duration::from_secs(10));
    let reason = infra_reason(outcome);
    assert!(reason.contains("no result event"), "reason: {reason}");
}

// Scenario 4: nonzero exit with no result → Infra carrying the exit code
// and the stderr tail (raw here by the U1 contract; U4/U5 scrub it before
// any surface).
#[test]
fn nonzero_exit_without_result_is_infra_with_code_and_stderr_tail() {
    let h = harness("fail.sh", fast_timing());
    h.runner
        .run("auto-fail", "r1", &h.cwd(), "try anyway", &ResolvedLaunch::default());

    let (_, _, outcome) = h.one_closed(Duration::from_secs(10));
    let reason = infra_reason(outcome);
    assert!(reason.contains('3'), "reason carries the exit code: {reason}");
    assert!(
        reason.contains("boom: config missing"),
        "reason carries the stderr tail: {reason}"
    );
}

// Spawn failure (R10's spawn-failed row): a missing binary closes
// immediately through the closer — nothing registers in flight.
#[test]
fn spawn_failure_closes_spawn_failed_and_registers_nothing() {
    let closed: Arc<Mutex<Vec<Closed>>> = Arc::new(Mutex::new(Vec::new()));
    let c = Arc::clone(&closed);
    let runner = HeadlessRunner::with_config(
        Arc::new(move |aid: &str, rid: &str, outcome: CheckOutcome| {
            c.lock().unwrap().push((aid.into(), rid.into(), outcome));
        }),
        "/nonexistent/fly-headless-claude-missing",
        fast_timing(),
    );
    let dir = tempfile::tempdir().unwrap();
    runner.run(
        "auto-spawn",
        "r1",
        &dir.path().to_string_lossy(),
        "never runs",
        &ResolvedLaunch::default(),
    );

    // Spawn failure closes synchronously inside run().
    let closed = closed.lock().unwrap();
    assert_eq!(closed.len(), 1);
    let reason = infra_reason(closed[0].2.clone());
    assert!(reason.contains("spawn failed"), "reason: {reason}");
    assert_eq!(runner.inflight_count(), 0, "no stranded registry entry");
}

// ---- deadline / kill discipline (R4/R5) ---------------------------------------------

// Scenario 5: a check still streaming at a (test-shortened) deadline is
// killed — SIGTERM-first, descendants swept — closes Infra("timed out"),
// and NO fixture process survives (asserted on the live process table).
#[test]
fn hang_past_deadline_is_killed_timed_out_with_no_survivors() {
    let mut timing = fast_timing();
    timing.deadline = Duration::from_millis(700);
    timing.term_grace = Duration::from_secs(2);
    let h = harness("hang.sh", timing);
    let start = Instant::now();
    h.runner
        .run("auto-hang", "r1", &h.cwd(), "hang forever", &ResolvedLaunch::default());

    let (_, _, outcome) = h.one_closed(Duration::from_secs(15));
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "closure must arrive within deadline + graces, took {:?}",
        start.elapsed()
    );
    let reason = infra_reason(outcome);
    assert!(reason.contains("timed out"), "reason: {reason}");
    // Kill-confirm-then-close: by the time the closer fired, the fixture sh
    // AND its backgrounded sleep (unreachable by the direct SIGTERM — only
    // the snapshot sweep gets it) must be gone.
    assert_pids_gone(&recorded_pids(h.dir.path()));
    assert_eq!(h.runner.inflight_count(), 0);
}

// Scenario 6: a fixture that leaks a NEW-SESSION (`setsid`) grandchild —
// the shape claude's tool children take, which no group kill can reach —
// then hangs. The pre-signal descendant snapshot must catch it: after the
// timeout kill, no process whose cmdline carries the unique marker survives.
#[test]
fn kill_sweep_reaps_a_setsid_grandchild_from_the_snapshot() {
    let mut timing = fast_timing();
    timing.deadline = Duration::from_millis(700);
    let h = harness("grandchild.sh", timing);
    // The marker rides the prompt (the final positional) into the fixture's
    // argv and from there into the grandchild's `sh -c` string.
    let marker = format!("fly-headless-grandchild-{}", std::process::id());
    h.runner
        .run("auto-gc", "r1", &h.cwd(), &marker, &ResolvedLaunch::default());

    let (_, _, outcome) = h.one_closed(Duration::from_secs(15));
    let reason = infra_reason(outcome);
    assert!(reason.contains("timed out"), "reason: {reason}");
    wait_gone(
        || cmdline_marker_alive(&marker),
        "setsid grandchild (marker in cmdline)",
    );
    assert_eq!(h.runner.inflight_count(), 0);
}

// Stream-end policy: stdout EOF with NO result kills immediately — the run
// must close long before a deliberately huge deadline, classified from the
// child's actual exit facts ("killed by a signal with no result event").
#[test]
fn eof_without_result_kills_immediately_never_waiting_out_the_deadline() {
    let mut timing = fast_timing();
    timing.deadline = Duration::from_secs(60); // must NOT be waited out
    timing.term_grace = Duration::from_secs(2);
    let h = harness("close-stdout-hang.sh", timing);
    let start = Instant::now();
    h.runner
        .run("auto-eof", "r1", &h.cwd(), "close stdout", &ResolvedLaunch::default());

    let (_, _, outcome) = h.one_closed(Duration::from_secs(15));
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "EOF-no-result must not ride to the deadline, took {:?}",
        start.elapsed()
    );
    let reason = infra_reason(outcome);
    assert!(reason.contains("no result event"), "reason: {reason}");
    assert_pids_gone(&recorded_pids(h.dir.path()));
}

// Stream-end policy, the LingerKilled leg: a success result streamed, then
// the child lingers holding stdout open (the observed backgrounding quirk).
// The runner kills it after the (short, injected) linger grace and the check
// still closes CLEAN — bounded fast, no survivors.
#[test]
fn result_then_linger_is_killed_but_closes_clean() {
    let mut timing = fast_timing();
    timing.linger_exit_grace = Duration::from_millis(250);
    timing.term_grace = Duration::from_millis(1500);
    let h = harness("linger.sh", timing);
    let start = Instant::now();
    h.runner
        .run("auto-linger", "r1", &h.cwd(), "report early", &ResolvedLaunch::default());

    // The in-flight liveness probe (U4's overlap check) sees the live child
    // while it lingers — pid + start-time, not mere entry presence.
    assert!(
        h.runner.automation_check_alive("auto-linger"),
        "probe sees the lingering child as alive"
    );
    assert!(
        !h.runner.automation_check_alive("some-other-automation"),
        "probe is per-automation"
    );

    let (_, _, outcome) = h.one_closed(Duration::from_secs(10));
    assert!(
        start.elapsed() < Duration::from_secs(6),
        "linger leg must be bounded by linger + term graces, took {:?}",
        start.elapsed()
    );
    assert_eq!(
        outcome,
        CheckOutcome::Clean {
            text: "done; task still running".to_owned(),
            session_id: Some("s-linger".to_owned()),
        }
    );
    assert_eq!(h.runner.inflight_count(), 0);
    assert!(!h.runner.automation_check_alive("auto-linger"));
}

// The seam kill (delete/shutdown/backstop leg, R5): kill_run mid-check
// SIGTERMs the pinned child with the SHORT grace and sweeps its snapshot;
// the owning runner thread observes the death and closes Infra. The
// monotonic backstop gate reads false while the entry is young and true
// once it is gone.
#[test]
fn kill_run_seam_terminates_the_check_and_the_runner_closes() {
    let timing = fast_timing(); // 30s deadline — the seam, not the deadline, ends this
    let h = harness("hang.sh", timing);
    h.runner
        .run("auto-seam", "r-seam", &h.cwd(), "hang for the seam", &ResolvedLaunch::default());

    assert!(h.runner.automation_check_alive("auto-seam"));
    assert!(
        !h.runner.monotonic_deadline_lapsed("r-seam"),
        "young entry has not lapsed its monotonic deadline"
    );
    assert!(
        h.runner.monotonic_deadline_lapsed("r-unknown"),
        "an absent entry reads lapsed (the entry-gone backstop arm)"
    );

    h.runner.kill_run("r-unknown"); // unknown id: no-op
    let start = Instant::now();
    h.runner.kill_run("r-seam");

    let (_, _, outcome) = h.one_closed(Duration::from_secs(10));
    assert!(
        start.elapsed() < Duration::from_secs(6),
        "seam kill must be fast, took {:?}",
        start.elapsed()
    );
    let reason = infra_reason(outcome);
    assert!(
        reason.contains("no result event"),
        "the runner classifies the seam death truthfully: {reason}"
    );
    assert_pids_gone(&recorded_pids(h.dir.path()));
    assert_eq!(h.runner.inflight_count(), 0);
    assert!(
        h.runner.monotonic_deadline_lapsed("r-seam"),
        "a closed run's gone entry reads lapsed"
    );
    h.runner.kill_all_inflight(); // idempotent on an empty registry
}

// ---- env / argv / cwd hygiene (R13, refactor guard) ---------------------------------

// Scenario 7 (the script.rs env-test pattern): the child env is
// inherit-MINUS-strip — FLY_PANE_TOKEN / FLY_SOCKET_PATH and the shared
// child-session marker list are absent even when the app env carries them
// (this single property keeps installed hooks a no-op AND closes the
// automation-mutation socket surface — the headless R22 equivalent), while
// ordinary ambient env still flows through (NOT env_clear). Also pins the
// R1 argv shape (prompt LAST) and cwd = the automation's cwd.
#[test]
fn env_is_inherited_minus_strip_list_and_argv_has_pane_parity() {
    const STRIPPED: [&str; 6] = [
        "FLY_PANE_TOKEN",
        "FLY_SOCKET_PATH",
        "CLAUDECODE",
        "CLAUDE_CODE_CHILD_SESSION",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_CODE_ENTRYPOINT",
    ];
    for key in STRIPPED {
        std::env::set_var(key, "leaked-value");
    }
    std::env::set_var("FLY_HEADLESS_TEST_CANARY", "inherited");

    let h = harness("env-dump.sh", fast_timing());
    let launch = ResolvedLaunch {
        model: Some("sonnet".into()),
        effort: Some("low".into()),
        fallback: Some("haiku".into()),
    };
    h.runner
        .run("auto-env", "r1", &h.cwd(), "dump the env", &launch);
    let (_, _, outcome) = h.one_closed(Duration::from_secs(10));
    for key in STRIPPED {
        std::env::remove_var(key);
    }
    std::env::remove_var("FLY_HEADLESS_TEST_CANARY");
    assert!(matches!(outcome, CheckOutcome::Clean { .. }));

    let env_dump = std::fs::read_to_string(h.dir.path().join("env-dump.txt")).unwrap();
    for key in STRIPPED {
        assert!(
            !env_dump.lines().any(|l| l.starts_with(&format!("{key}="))),
            "{key} must be stripped from the check env"
        );
    }
    assert!(
        env_dump.lines().any(|l| l == "FLY_HEADLESS_TEST_CANARY=inherited"),
        "ordinary ambient env must flow through (inherit-minus-strip, not env_clear)"
    );
    assert!(
        env_dump.lines().any(|l| l.starts_with("HOME=")),
        "HOME must survive — claude needs it for ~/.claude credentials"
    );

    // cwd = the automation's cwd (physical paths on both sides).
    let cwd_dump = std::fs::read_to_string(h.dir.path().join("cwd-dump.txt")).unwrap();
    let expected_cwd = std::fs::canonicalize(h.dir.path()).unwrap();
    assert_eq!(Path::new(cwd_dump.trim()), expected_cwd.as_path());

    // R1 argv, pane parity (`App.buildAgentArgv` flag order; prompt LAST).
    let argv_dump = std::fs::read_to_string(h.dir.path().join("argv-dump.txt")).unwrap();
    let argv: Vec<&str> = argv_dump.lines().collect();
    assert_eq!(
        argv,
        vec![
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--dangerously-skip-permissions",
            "--model",
            "sonnet",
            "--effort",
            "low",
            "--fallback-model",
            "haiku",
            "dump the env",
        ]
    );
}
