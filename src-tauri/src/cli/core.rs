//! `fly core` — run the headless backend's control socket (Electron-shell
//! migration U1/U2). U2 state: the full ported command table over real
//! managers (config, PTY + substrate, attention, coalescers); events
//! broadcast to every connected client. Still absent until U3: pane spawning
//! with byte streams, the hook server, automations, the feed listener —
//! their commands answer a clear error. Internal-facing like
//! `substrate-event`: launched by a display shell, not typed by humans.

use std::sync::{Arc, Mutex};

use crate::control::registry::{build_registry, CoreHandles};
use crate::control::{control_socket_path, ControlServer};

pub fn run(args: &[String]) -> i32 {
    // `--socket <path>` override, for tests and side-by-side dev flavors
    // (the default is already per-flavor via FLY_APP_NAME).
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

    // The same construction lib.rs's Tauri setup performs for these managers
    // (substrate wiring included), minus the shell-coupled subsystems (U3).
    let config = Arc::new(crate::config::ConfigStore::load(
        crate::config::default_path(),
    ));
    let cfg = config.get();
    let pty = Arc::new(crate::pty::PtyManager::new());
    if cfg.substrate == crate::config::SubstrateKind::Tmux {
        let runtime_dir = socket_path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir);
        let hook_socket = runtime_dir.join("hook.sock");
        pty.set_substrate(Arc::new(crate::substrate::Substrate::new(
            crate::app_dir_name(),
            crate::session::data_dir().join("substrate-sessions.json"),
            runtime_dir,
            hook_socket,
            std::env::current_exe().unwrap_or_else(|_| "fly".into()),
        )));
    }
    let attention = Arc::new(crate::state::AttentionManager::new(
        cfg.attention_debounce_ms,
        cfg.notifications_muted_default,
    ));
    let coalescers = Arc::new(crate::stream::coalesce::CoalescerRegistry::default());

    // Events broadcast through the server; the server needs the handler
    // first, so the sink resolves through a slot filled after start.
    let server_slot: Arc<Mutex<Option<ControlServer>>> = Arc::new(Mutex::new(None));
    let sink_slot = Arc::clone(&server_slot);
    let events = Arc::new(move |name: &str, payload: serde_json::Value| {
        if let Some(server) = sink_slot.lock().expect("server slot").as_ref() {
            server.broadcast_event(name, payload);
        }
    });

    let handler = build_registry(CoreHandles {
        pty,
        attention,
        config,
        coalescers,
        automations: None,
        feed: None,
        events,
    });

    let server = match ControlServer::start(socket_path.clone(), handler, None) {
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
