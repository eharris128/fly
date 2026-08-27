//! U10 live cwd tracking (R13): a pane's cwd follows `cd`, read from
//! /proc/<pid>/cwd of the foreground process.

use std::time::{Duration, Instant};

use fly_lib::pty::{OutputSink, PtyManager, SpawnConfig};

fn bash() -> SpawnConfig {
    SpawnConfig {
        shell: Some("/bin/bash".into()),
        args: vec!["--norc".into(), "--noprofile".into(), "-i".into()],
        ..Default::default()
    }
}

fn null_sink() -> OutputSink {
    Box::new(|_: &[u8]| {})
}

#[test]
fn proc_cwd_reads_a_process_cwd() {
    let cwd = fly_lib::cwd::proc_cwd(std::process::id()).unwrap();
    assert_eq!(cwd, std::env::current_dir().unwrap());
}

#[test]
fn proc_cwd_of_a_missing_process_is_none() {
    assert!(fly_lib::cwd::proc_cwd(u32::MAX).is_none());
}

#[test]
fn pane_cwd_follows_cd_not_the_spawn_dir() {
    let mgr = PtyManager::new();
    let dir = tempfile::tempdir().unwrap();
    let target = std::fs::canonicalize(dir.path()).unwrap();
    let id = mgr
        .spawn(bash(), "t".into(), null_sink(), Box::new(|_, _| {}))
        .unwrap();

    // The shell starts in $HOME; cd elsewhere and confirm tracking follows.
    mgr.write(id, format!("cd '{}'\n", target.display()).as_bytes())
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut got = None;
    while Instant::now() < deadline {
        got = mgr.cwd(id);
        if got.as_deref() == Some(target.as_path()) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(got, Some(target), "pane cwd should follow cd");
    mgr.close(id).unwrap();
}

/// Wait (up to 5s) for a freshly spawned pane's foreground pid to resolve.
fn wait_foreground_pid(mgr: &PtyManager, id: fly_lib::pty::PaneId) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(pid) = mgr.foreground_pid(id) {
            return pid;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("pane foreground pid should resolve within 5s");
}

#[test]
fn agent_task_count_is_none_for_a_bare_shell_pane() {
    // running-state U3: a bare bash pane is not a Claude agent, so the dashboard
    // count path returns None (→ is_agent:false, live_task_count:0) — exercised
    // over a real PTY through the real /proc is_claude gate.
    let mgr = PtyManager::new();
    let id = mgr
        .spawn(bash(), "t".into(), null_sink(), Box::new(|_, _| {}))
        .unwrap();
    wait_foreground_pid(&mgr, id);
    assert_eq!(
        mgr.agent_task_count(id),
        None,
        "a bare shell is not an agent"
    );
    mgr.close(id).unwrap();
}

#[test]
fn counts_a_real_backgrounded_job_under_a_pane() {
    // running-state U1+U2 end-to-end over a LIVE process tree: a backgrounded
    // `sleep` under a pane's shell is a live descendant in its own process group,
    // so the pure counter (U1) fed by the real /proc reader (U2), rooted at the
    // pane's foreground pid, sees >= 1 task. Validates the descendant walk + pgid
    // filter against real /proc data, not just synthetic tables. (The is_claude
    // gate needs a real `claude` and is covered by U1/U2 + live verification; here
    // the count is rooted directly at the bash foreground pid.)
    let mgr = PtyManager::new();
    let id = mgr
        .spawn(bash(), "t".into(), null_sink(), Box::new(|_, _| {}))
        .unwrap();
    let fg = wait_foreground_pid(&mgr, id);

    // Interactive bash on a tty has job control on, so `&` puts the job in its own
    // process group — exactly one background task under the foreground shell.
    mgr.write(id, b"sleep 300 &\n").unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut count = 0;
    while Instant::now() < deadline {
        let table = fly_lib::cwd::read_proc_table();
        count = fly_lib::cwd::count_background_task_groups(&table, fg);
        if count >= 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        count >= 1,
        "a backgrounded sleep should surface as >= 1 task, got {count}"
    );

    mgr.close(id).unwrap();
}
