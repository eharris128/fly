//! U14 graceful shutdown (R4): closing all panes reaps every child without
//! hanging, even while they're producing output.

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
fn close_all_reaps_every_pane() {
    let mgr = PtyManager::new();
    for _ in 0..3 {
        let id = mgr
            .spawn(bash(), "t".into(), null_sink(), Box::new(|_, _| {}))
            .unwrap();
        // Keep each pane busy so teardown must interrupt a live producer.
        mgr.write(id, b"yes BUSY\n").unwrap();
    }
    assert_eq!(mgr.count(), 3);

    // Must reap all three (joining each read thread) without hanging.
    mgr.close_all();
    assert_eq!(mgr.count(), 0);
}
