//! `fly core` — run the headless backend's control socket (Electron-shell
//! migration U1). At this stage it serves only the protocol scaffold
//! (`core/ping`); U2/U3 grow it into the full backend host. Internal-facing
//! like `substrate-event`: launched by a display shell, not typed by humans,
//! so the top-level help mentions it only in passing.

use crate::control::{control_socket_path, server::ping_handler, ControlServer};

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

    let server = match ControlServer::start(socket_path.clone(), ping_handler(), None) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fly core: cannot bind {}: {e}", socket_path.display());
            return 1;
        }
    };
    eprintln!("fly core: listening on {}", server.socket_path().display());

    // Serve until killed. A SIGTERM/SIGINT skips Drop and leaves the socket
    // file behind — harmless by design: the next start's never-steal probe
    // finds it dead and reclaims it (same crash-residue story as hook.sock).
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
