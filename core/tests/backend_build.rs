//! Smoke coverage for `backend::build_backend` (Electron-shell migration
//! U3.5) — the shared constructor both shells boot through, whose stated
//! purpose is that the wiring cannot drift. Nothing else pinned it: this
//! builds the FULL backend (hook server + dispatch, automations + sweep,
//! feed plumbing) against an isolated flavor, asserts the load-bearing
//! surfaces exist, proves same-flavor exclusivity (the hook socket's
//! never-steal bind), and runs the U6 ordered shutdown to completion —
//! including the clean-exit marker that drives the next launch's resume
//! offer (KTD-G).
//!
//! One `#[test]` on purpose: isolation rides process-global env vars
//! (`FLY_APP_NAME`, `HOME`, `XDG_*`), and cargo runs a file's tests on
//! threads. Everything sequential lives in the single test body.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use fly_lib::backend::{build_backend, BackendSeams};

fn seams(events_log: Arc<Mutex<Vec<String>>>) -> BackendSeams {
    BackendSeams {
        events: Arc::new(move |event: &str, _payload: serde_json::Value| {
            events_log.lock().unwrap().push(event.to_string());
        }),
        banner: Arc::new(|_title: &str, _body: &str| {}),
    }
}

#[test]
fn builds_full_backend_isolated_refuses_second_and_shuts_down_clean() {
    // ---- isolated flavor: every path root derives from these ----
    let root = std::env::temp_dir().join(format!("fly-backend-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let home = root.join("home");
    let runtime = root.join("runtime");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&runtime).unwrap();
    let flavor = format!("fly-betest-{}", std::process::id());
    // SAFETY: this binary holds one test and nothing has spawned a thread yet,
    // so the env is written single-threaded, before the backend it configures.
    unsafe { std::env::set_var("FLY_APP_NAME", &flavor) };
    unsafe { std::env::set_var("HOME", &home) };
    unsafe { std::env::set_var("XDG_RUNTIME_DIR", &runtime) };
    unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
    unsafe { std::env::remove_var("XDG_DATA_HOME") };
    // Feed listener off: the default port may be held by a real running fly,
    // which would make `feed_server`'s presence nondeterministic here.
    let cfg_dir = home.join(".config").join(&flavor);
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("config.json"),
        br#"{"feed": {"enabled": false}}"#,
    )
    .unwrap();

    let events = Arc::new(Mutex::new(Vec::new()));
    let backend = build_backend(seams(Arc::clone(&events)))
        .expect("build_backend must construct the full backend on a fresh flavor");

    // The config store resolved OUR flavor's file (sparse: feed off, rest default).
    let cfg = backend.config.get();
    assert!(!cfg.feed.enabled, "the isolated config.json was not read");
    assert_eq!(cfg.font_size, 15, "defaults did not survive the sparse file");
    assert!(backend.feed_server.is_none(), "feed disabled ⇒ no listener");

    // The hook socket — the attention pipeline's security boundary — is
    // bound at the stable per-flavor path (tmux-substrate U2/KTD8).
    let sock = PathBuf::from(&runtime).join(&flavor).join("hook.sock");
    assert!(sock.exists(), "hook socket not bound at {}", sock.display());

    // Same-flavor exclusivity: a second backend must refuse to steal the
    // live socket (the never-steal discipline), not silently coexist.
    let second = build_backend(seams(Arc::new(Mutex::new(Vec::new()))));
    assert!(
        second.is_err(),
        "a second same-flavor backend must fail the never-steal bind"
    );

    // U6 ordered shutdown runs to completion and writes the clean-exit
    // marker (step 1, KTD-G) — the fact the next launch's resume offer
    // keys on.
    let marker = fly_lib::session::resume::clean_exit_path();
    let _ = fly_lib::session::resume::set_clean_exit_at(&marker, false);
    backend.shutdown();
    assert!(
        fly_lib::session::resume::took_clean_exit_at(&marker),
        "ordered shutdown must write the clean-exit marker"
    );

    // Drop the backend BEFORE removing the temp tree: HookServer's Drop wakes
    // its blocked accept thread by self-connecting to the socket path, and
    // joins it — with the socket file already unlinked the connect fails, the
    // accept never wakes, and the join blocks forever. (Real shutdowns are
    // safe: only shutdown itself removes the socket, after the join.)
    drop(backend);
    let sock_dir = PathBuf::from(&runtime).join(&flavor);
    assert!(
        !sock_dir.join("hook.sock").exists(),
        "HookServer teardown must remove its socket"
    );

    let _ = std::fs::remove_dir_all(&root);
}
