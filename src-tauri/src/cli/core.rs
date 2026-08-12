//! `fly core` — run the headless backend's control socket (Electron-shell
//! migration U1/U2/U3). U3 state: the full pane lifecycle works over the
//! socket — spawn (output riding 0x02 binary frames, exits as `pane://exit`
//! events), keystrokes down the 0x03 pane-input frames, launch-mode
//! resolution (consumes this flavor's clean-exit marker, KTD-G). Still
//! absent until U3.5's full host: the hook *server* (pane env already points
//! at its stable path), automations, and the feed listener — their commands
//! answer a clear error. Internal-facing like `substrate-event`: launched by
//! a display shell, not typed by humans.

use std::sync::{Arc, Mutex};

use crate::control::registry::{build_registry, CoreHandles};
use crate::control::{control_socket_path, ControlServer};
use crate::pty::PaneId;

pub fn run(args: &[String]) -> i32 {
    // `--socket <path>` override, for tests and side-by-side dev flavors
    // (the default is already per-flavor via FLY_APP_NAME). The hook socket
    // lands beside whichever control socket is chosen.
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
    let runtime_dir = socket_path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir);
    let hook_socket_path = runtime_dir.join("hook.sock");

    // The same construction lib.rs's Tauri setup performs for these managers
    // (substrate wiring included), minus the shell-coupled subsystems (U3.5).
    let config = Arc::new(crate::config::ConfigStore::load(
        crate::config::default_path(),
    ));
    let cfg = config.get();
    let pty = Arc::new(crate::pty::PtyManager::new());
    if cfg.substrate == crate::config::SubstrateKind::Tmux {
        pty.set_substrate(Arc::new(crate::substrate::Substrate::new(
            crate::app_dir_name(),
            crate::session::data_dir().join("substrate-sessions.json"),
            runtime_dir,
            hook_socket_path.clone(),
            std::env::current_exe().unwrap_or_else(|_| "fly".into()),
        )));
    }
    let tokens = Arc::new(crate::hooks::TokenRegistry::new());
    let attention = Arc::new(crate::state::AttentionManager::new(
        cfg.attention_debounce_ms,
        cfg.notifications_muted_default,
    ));
    let coalescers = Arc::new(crate::stream::coalesce::CoalescerRegistry::default());
    // Consumes this flavor's clean-exit marker exactly like the Tauri boot
    // (KTD-G): whichever role owns the backend owns crash detection.
    let launch_mode = crate::resolve_launch_mode(args);

    // Events + pane bytes broadcast through the server; the server needs the
    // handler first, so both sinks resolve through a slot filled after start.
    let server_slot: Arc<Mutex<Option<ControlServer>>> = Arc::new(Mutex::new(None));
    let event_slot = Arc::clone(&server_slot);
    let events: crate::stream::EventSink =
        Arc::new(move |name: &str, payload: serde_json::Value| {
            if let Some(server) = event_slot.lock().expect("server slot").as_ref() {
                server.broadcast_event(name, payload);
            }
        });
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
    let input_pty = Arc::clone(&pty);
    let input_attention = Arc::clone(&attention);
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
        pty,
        tokens,
        attention,
        config,
        coalescers,
        automations: None,
        alerts: None,
        feed: None,
        hook_socket_path,
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

    // Serve until killed. A SIGTERM/SIGINT skips Drop and leaves the socket
    // file behind — harmless by design: the next start's never-steal probe
    // finds it dead and reclaims it (same crash-residue story as hook.sock).
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
