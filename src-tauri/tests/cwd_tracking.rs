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
