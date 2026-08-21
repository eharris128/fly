//! The control-socket server (U1 scaffold): never-steal bind, same-uid gate,
//! per-connection read loop dispatching JSON requests, and broadcast fan-out
//! for events + pane-output frames. U2 replaced the U1 stub command table
//! with the ported Tauri commands (`registry.rs`); the seam between the two
//! is [`CommandHandler`].

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use super::envelope::{err_response, ok_response, Request};
use super::frame::{read_frame, write_frame, Frame};
use crate::hooks::server::{create_private_socket_dir, peer_uid_matches, ConnCap};

/// Same availability-guard cap as the hook socket (audit-remediation KTD6).
pub const MAX_CONNECTIONS: usize = 64;

/// A write that a stopped-reading client can't wedge (hook-socket rule); the
/// broadcaster drops the client on any write error instead.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// Handle one command: `(cmd, args) -> Ok(result) | Err(message)`. U1 wires
/// only `core/ping`; U2 registers the ported command table. Runs on the
/// connection's read thread — long-running commands spawn internally.
pub type CommandHandler =
    Arc<dyn Fn(&str, serde_json::Value) -> Result<serde_json::Value, String> + Send + Sync>;

/// Sink for client pane-input frames (KTD3's JSON-free keystroke path down).
/// U3 wires this to `pty_write`; U1 tests record into a buffer.
pub type PaneInputHandler = Arc<dyn Fn(u64, &[u8]) + Send + Sync>;

/// One connected client's write half. Reads happen on the connection's own
/// thread; writes (responses + broadcasts) serialize through the mutex so a
/// response can't splice a broadcast frame.
struct Client {
    id: u64,
    stream: Arc<Mutex<UnixStream>>,
}

/// Shared client registry the broadcaster walks.
type Clients = Arc<Mutex<Vec<Client>>>;

pub struct ControlServer {
    socket_path: PathBuf,
    clients: Clients,
    stopping: Arc<AtomicBool>,
    accept_handle: Option<JoinHandle<()>>,
}

impl ControlServer {
    /// Bind and serve. Fails `AddrInUse` if a live server answers on the path
    /// (never-steal — the hook-socket/ga-h9z discipline); reclaims only a
    /// dead socket.
    pub fn start(
        socket_path: PathBuf,
        handler: CommandHandler,
        pane_input: Option<PaneInputHandler>,
    ) -> io::Result<ControlServer> {
        if let Some(parent) = socket_path.parent() {
            create_private_socket_dir(parent)?;
        }
        if socket_path.exists() {
            match UnixStream::connect(&socket_path) {
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!(
                            "control socket {} is owned by a live instance",
                            socket_path.display()
                        ),
                    ));
                }
                Err(_) => {
                    let _ = std::fs::remove_file(&socket_path);
                }
            }
        }
        let listener = UnixListener::bind(&socket_path)?;
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;

        let clients: Clients = Arc::new(Mutex::new(Vec::new()));
        let stopping = Arc::new(AtomicBool::new(false));
        let accept_clients = Arc::clone(&clients);
        let accept_stopping = Arc::clone(&stopping);
        let accept_handle = std::thread::Builder::new()
            .name("fly-control-accept".into())
            .spawn(move || {
                accept_loop(listener, accept_clients, handler, pane_input, accept_stopping)
            })?;

        Ok(ControlServer {
            socket_path,
            clients,
            stopping,
            accept_handle: Some(accept_handle),
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Fan an event out to every connected client (KTD1 event names).
    pub fn broadcast_event(&self, event: &str, payload: serde_json::Value) {
        let body = serde_json::to_vec(&super::envelope::Event {
            event: event.to_string(),
            payload,
        })
        .expect("event serializes");
        self.broadcast(&Frame::Json(body));
    }

    /// Fan a pane's raw output bytes to every connected client (KTD3).
    pub fn broadcast_pane_output(&self, pane: u64, bytes: &[u8]) {
        self.broadcast(&Frame::PaneOutput {
            pane,
            bytes: bytes.to_vec(),
        });
    }

    fn broadcast(&self, frame: &Frame) {
        let mut clients = self.clients.lock().expect("clients lock");
        clients.retain(|c| {
            let mut stream = c.stream.lock().expect("client stream lock");
            write_frame(&mut *stream, frame).is_ok()
        });
    }

    /// Stop accepting, drop every client, join, unlink.
    pub fn shutdown(&mut self) {
        if self.accept_handle.is_none() {
            return;
        }
        self.stopping.store(true, Ordering::Release);
        let _ = UnixStream::connect(&self.socket_path); // wake blocking accept
        if let Some(h) = self.accept_handle.take() {
            let _ = h.join();
        }
        // Shut the read side of every client so their threads exit too.
        for c in self.clients.lock().expect("clients lock").drain(..) {
            let stream = c.stream.lock().expect("client stream lock");
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn accept_loop(
    listener: UnixListener,
    clients: Clients,
    handler: CommandHandler,
    pane_input: Option<PaneInputHandler>,
    stopping: Arc<AtomicBool>,
) {
    let cap = ConnCap::new(MAX_CONNECTIONS);
    let next_id = AtomicU64::new(1);
    for conn in listener.incoming() {
        if stopping.load(Ordering::Acquire) {
            break;
        }
        let Ok(stream) = conn else { continue };
        let Some(slot) = cap.try_claim() else {
            continue; // over cap: drop silently, like the hook socket
        };
        if !peer_uid_matches(&stream) {
            continue; // wrong uid: drop before reading a byte
        }
        let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
        let Ok(read_half) = stream.try_clone() else {
            continue;
        };
        let id = next_id.fetch_add(1, Ordering::Relaxed);
        let write_half = Arc::new(Mutex::new(stream));
        clients.lock().expect("clients lock").push(Client {
            id,
            stream: Arc::clone(&write_half),
        });
        let conn_clients = Arc::clone(&clients);
        let conn_handler = Arc::clone(&handler);
        let conn_pane_input = pane_input.clone();
        let _ = std::thread::Builder::new()
            .name("fly-control-conn".into())
            .spawn(move || {
                let _slot = slot;
                connection_loop(read_half, &write_half, conn_handler, conn_pane_input);
                // On any exit (EOF, framing error, skew) deregister the client.
                conn_clients
                    .lock()
                    .expect("clients lock")
                    .retain(|c| c.id != id);
            });
    }
}

/// Read frames until EOF or a protocol violation (which fail closed: drop).
fn connection_loop(
    mut read_half: UnixStream,
    write_half: &Arc<Mutex<UnixStream>>,
    handler: CommandHandler,
    pane_input: Option<PaneInputHandler>,
) {
    loop {
        let frame = match read_frame(&mut read_half) {
            Ok(Some(f)) => f,
            Ok(None) | Err(_) => return,
        };
        match frame {
            Frame::Json(body) => {
                let response = match serde_json::from_slice::<Request>(&body) {
                    Ok(req) => match handler(&req.cmd, req.args) {
                        Ok(result) => ok_response(req.id, result),
                        Err(msg) => err_response(req.id, &msg),
                    },
                    // No parseable id to answer under — protocol violation.
                    Err(_) => return,
                };
                let mut w = write_half.lock().expect("client stream lock");
                if write_frame(&mut *w, &Frame::Json(response)).is_err() {
                    return;
                }
            }
            Frame::PaneInput { pane, bytes } => {
                if let Some(sink) = &pane_input {
                    sink(pane, &bytes);
                }
            }
            // Output frames only flow server→client; a client sending one is
            // protocol-violating (version skew) — fail closed.
            Frame::PaneOutput { .. } => return,
        }
    }
}

/// The one built-in U1 command: `core/ping` → `{pong, version}` — the shell's
/// liveness/version probe (`docs/core-protocol.md`).
pub fn ping_handler() -> CommandHandler {
    Arc::new(|cmd, _args| match cmd {
        "core/ping" => Ok(serde_json::json!({
            "pong": true,
            "version": env!("CARGO_PKG_VERSION"),
        })),
        other => Err(format!("unknown command: {other}")),
    })
}

// Integration coverage lives in `tests/control_socket.rs` (bind/steal, ping,
// fan-out, pane frames, uid gate is implicit same-uid there).
