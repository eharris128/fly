//! `fly core` — the headless backend host (Electron-shell migration
//! U1/U2/U3/U3.5). Boots the **full** backend through
//! `backend::build_backend` — hook server + dispatch, automations subsystem +
//! sweep, feed listener, substrate — then serves the complete command table
//! over the control socket. Its seams: events broadcast to control clients,
//! desktop banners go through `notify::banner` (`notify-send`).
//!
//! Flavor safety: the hook server binds the flavor's stable socket with the
//! never-steal probe, so a `fly core` started while a same-flavor core runs
//! refuses at boot instead of fighting it for the backend role.
//!
//! Shutdown (U6): ordered. `core/shutdown` from the shell — or
//! SIGTERM/SIGINT — runs `backend::ordered_shutdown` (clean-exit marker,
//! sweep join + interrupted-run closes, feed/ask release, pane reap with
//! substrate DETACH) and exits 0. Only a SIGKILL skips it, and the next
//! boot's never-steal probe reclaims the socket residue. Internal-facing like
//! `substrate-event`: launched by a display shell, not typed by humans.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// One flag, two writers: the `core/shutdown` command and the signal
/// handlers. An atomic store is async-signal-safe; the run loop polls it.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

use crate::backend::{build_backend, BackendSeams};
use crate::control::registry::{build_registry, CoreHandles};
use crate::control::{control_socket_path, ControlServer};
use crate::pty::PaneId;

pub fn run(args: &[String]) -> i32 {
    // `--socket <path>` moves only the CONTROL socket (tests / side-by-side
    // dev). The hook socket stays at the flavor's stable path regardless —
    // surviving substrate agents point there (tmux-substrate KTD8).
    let socket_path = match args.iter().position(|a| a == "--socket") {
        Some(i) => match args.get(i + 1) {
            Some(p) => std::path::PathBuf::from(p),
            None => {
                eprintln!("usage: fly core [--socket <path>] [resume]");
                return 2;
            }
        },
        None => control_socket_path(),
    };

    // Consumes this flavor's clean-exit marker (KTD-G): the role that owns
    // the backend owns crash detection. `resume` arrives as a plain arg the
    // shell forwards from its own argv (`fly resume` → exec shell → spawn
    // `fly core resume`; 2026-08-27-001 KTD7).
    let launch_mode = crate::resolve_launch_mode(args.iter().any(|a| a == "resume"));

    // Both sinks broadcast through the server, resolved via a slot filled
    // after start (the server needs the handler first).
    let server_slot: Arc<Mutex<Option<ControlServer>>> = Arc::new(Mutex::new(None));
    let event_slot = Arc::clone(&server_slot);
    let events: crate::stream::EventSink =
        Arc::new(move |name: &str, payload: serde_json::Value| {
            if let Some(server) = event_slot.lock().expect("server slot").as_ref() {
                server.broadcast_event(name, payload);
            }
        });
    // Desktop banners via `notify::banner` (`notify-send`, argv-passed, no
    // shell — content is data; 2026-08-27-001 KTD4).
    let banner: Arc<dyn Fn(&str, &str) + Send + Sync> = Arc::new(|title: &str, body: &str| {
        crate::notify::banner(title, body);
    });

    let backend = match build_backend(BackendSeams {
        events: Arc::clone(&events),
        banner,
    }) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("fly core: {e}");
            return 1;
        }
    };

    let bytes_slot = Arc::clone(&server_slot);
    let pane_bytes: crate::control::registry::PaneBytesSink =
        Arc::new(move |pane: u64, bytes: Vec<u8>| {
            if let Some(server) = bytes_slot.lock().expect("server slot").as_ref() {
                server.broadcast_pane_output(pane, &bytes);
            }
        });

    // Keystrokes down the 0x03 frames: same behavior as the `pty_write`
    // command — write, then clear attention on input (frames on one
    // connection are handled in order, so per-pane write order holds).
    let input_pty = Arc::clone(&backend.pty);
    let input_attention = Arc::clone(&backend.attention);
    let input_events = Arc::clone(&events);
    let pane_input: crate::control::server::PaneInputHandler =
        Arc::new(move |pane: u64, bytes: &[u8]| {
            let id = PaneId(pane);
            if input_pty.write(id, bytes).is_ok() {
                if let Some(outcome) = input_attention.on_input(id) {
                    (input_events)(
                        crate::stream::PANE_ATTENTION_EVENT,
                        crate::stream::attention_event_payload(id, &outcome),
                    );
                }
            }
        });

    let handler = build_registry(CoreHandles {
        pty: Arc::clone(&backend.pty),
        tokens: Arc::clone(&backend.tokens),
        attention: Arc::clone(&backend.attention),
        config: Arc::clone(&backend.config),
        coalescers: Arc::clone(&backend.coalescers),
        automations: Some(Arc::clone(&backend.automations)),
        alerts: Some(Arc::clone(&backend.alerts)),
        feed: Some(Arc::clone(&backend.feed_state)),
        hook_socket_path: backend.hook_server.socket_path().to_path_buf(),
        launch_mode,
        events,
        pane_bytes,
        shutdown: Some(Arc::new(|| SHUTDOWN.store(true, Ordering::SeqCst))),
    });

    let server = match ControlServer::start(socket_path.clone(), handler, Some(pane_input)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fly core: cannot bind {}: {e}", socket_path.display());
            return 1;
        }
    };
    eprintln!("fly core: listening on {}", server.socket_path().display());
    *server_slot.lock().expect("server slot") = Some(server);

    // Serve until asked to stop: `core/shutdown` over the socket or
    // SIGTERM/SIGINT (the Electron shell's before-quit sends the command and
    // falls back to SIGTERM — both land on the same flag).
    unsafe {
        let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGINT, handler);
    }
    while !SHUTDOWN.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    // Ordered teardown (U6): clean-exit marker, automations sweep join +
    // interrupted-run closes, feed/ask release, then the pane reap (tmux
    // sessions DETACH).
    // The poll gap above also lets the `core/shutdown` response flush before
    // the server drops below.
    eprintln!("fly core: shutting down (ordered)");
    backend.shutdown();
    // Drop the control server explicitly (unbind + close connections), then
    // the backend (hook server / feed listener join in their Drop impls).
    drop(server_slot.lock().expect("server slot").take());
    drop(backend);
    0
}
