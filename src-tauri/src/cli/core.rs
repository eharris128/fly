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
//! Shutdown (U6): ordered, exactly like the Tauri quit. `core/shutdown` from
//! the shell — or SIGTERM/SIGINT — runs `backend::ordered_shutdown` (clean-
//! exit marker, sweep join + interrupted-run closes, feed/ask release, pane
//! reap with substrate DETACH) and exits 0. Only a SIGKILL skips it, and the
//! next boot's never-steal probe reclaims the socket residue. Internal-facing
//! like `substrate-event`: launched by a display shell, not typed by humans.

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

/// The `notify-send` argv for one banner: title and body ride as separate
/// argv entries (never through a shell), `--app-name=fly` so the daemon
/// groups them under the app.
fn banner_command(title: &str, body: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new("notify-send");
    cmd.arg("--app-name=fly").arg(title).arg(body);
    cmd
}

/// In-flight `notify-send` helpers. The banner seam runs on the hook
/// dispatch / reaper path, so the child is reaped on its own short-lived
/// thread (`notify::spawn_detached_capped`) rather than waited inline — and
/// it IS reaped: the 2026-08-22 incident core had 233 `<defunct>`
/// `notify-send` children from a fire-and-forget `spawn()` that never
/// `wait`ed. Own slot counter, distinct from the chime's pool (see the
/// helper's doc); the `NotificationGate` already coalesces and rate-limits
/// upstream, so the cap only backstops a runaway daemon.
static BANNER_INFLIGHT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
const BANNER_CAP: usize = 16;

/// Fire one desktop banner, non-blocking, child reaped. Returns the reaper
/// handle so a test can await the `wait()`; production ignores it.
fn send_banner(title: &str, body: &str) -> Option<std::thread::JoinHandle<()>> {
    crate::notify::spawn_detached_capped(banner_command(title, body), &BANNER_INFLIGHT, BANNER_CAP)
}

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
        send_banner(title, body);
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

    // Ordered teardown (U6) — the identical sequence lifecycle::shutdown runs
    // under Tauri: clean-exit marker, automations sweep join + interrupted-run
    // closes, feed/ask release, then the pane reap (tmux sessions DETACH).
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_argv_passes_title_and_body_as_data() {
        let cmd = banner_command("fly: pane 3", "needs you; $(rm -rf /) `x`");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(cmd.get_program(), "notify-send");
        assert_eq!(
            args,
            vec!["--app-name=fly", "fly: pane 3", "needs you; $(rm -rf /) `x`"],
            "no shell: title/body are argv entries verbatim"
        );
    }

    #[test]
    fn banner_helpers_are_reaped_not_left_defunct() {
        // The incident shape: many banners from one long-lived core. Every
        // spawned child must be waited on (the reaper handle joins only after
        // `child.wait()` returned) and every slot released.
        use std::sync::atomic::Ordering;
        static INFLIGHT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let mut handles = Vec::new();
        for i in 0..6 {
            let mut cmd = std::process::Command::new("true");
            cmd.arg(format!("banner-{i}"));
            if let Some(h) = crate::notify::spawn_detached_capped(cmd, &INFLIGHT, BANNER_CAP) {
                handles.push(h);
            }
        }
        assert_eq!(handles.len(), 6, "under the cap nothing is dropped");
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(INFLIGHT.load(Ordering::SeqCst), 0, "every child waited, no slot leak");
    }
}
