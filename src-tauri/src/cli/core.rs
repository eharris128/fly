//! `fly core` — the headless backend host (Electron-shell migration
//! U1/U2/U3/U3.5). Boots the **full** backend through the same
//! `backend::build_backend` the Tauri shell uses — hook server + dispatch,
//! automations subsystem + sweep, feed listener, substrate — then serves the
//! complete command table over the control socket. The two shells differ
//! only in their seams: events broadcast to control clients instead of
//! `app.emit`, and desktop banners go through `notify-send` instead of the
//! Tauri notification plugin.
//!
//! Flavor safety: the hook server binds the flavor's stable socket with the
//! never-steal probe, so a `fly core` started while a same-flavor Tauri fly
//! runs refuses at boot instead of fighting it for the backend role.
//!
//! Shutdown: killing the process leaves tmux-substrate sessions alive (they
//! outlive fly by design) and abandons plain-PTY children to reparenting —
//! the Electron shell (U4) owns ordered shutdown, like lifecycle.rs does
//! under Tauri. Internal-facing like `substrate-event`: launched by a
//! display shell, not typed by humans.

use std::sync::{Arc, Mutex};

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
                eprintln!("usage: fly core [--socket <path>]");
                return 2;
            }
        },
        None => control_socket_path(),
    };

    // Consumes this flavor's clean-exit marker exactly like the Tauri boot
    // (KTD-G): whichever role owns the backend owns crash detection.
    let launch_mode = crate::resolve_launch_mode(args);

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
    // Desktop banners via `notify-send` (argv-passed, no shell — content is
    // data). The proposal's KTD8 endpoint (notify-rust over DBus) can replace
    // this without touching the seam.
    let banner: Arc<dyn Fn(&str, &str) + Send + Sync> = Arc::new(|title: &str, body: &str| {
        let _ = std::process::Command::new("notify-send")
            .arg("--app-name=fly")
            .arg(title)
            .arg(body)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
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

    // Serve until killed; `backend` (hook server, sweep, feed listener) lives
    // for the process. Socket residue on SIGKILL is reclaimed by the next
    // start's never-steal probe.
    let _backend = backend;
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
