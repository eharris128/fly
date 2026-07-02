//! Unix-domain socket server for the hook channel (U8, R10).

use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use super::protocol::HookMessage;
use super::token::TokenRegistry;
use crate::pty::PaneId;
use crate::state::attention::Reason;

/// Cap on a single callback payload — defends against a peer streaming forever.
const MAX_MESSAGE: u64 = 64 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// An authenticated callback, resolved to its pane.
#[derive(Debug, Clone)]
pub struct ValidatedHook {
    pub reason: Reason,
    pub title: Option<String>,
    pub body: Option<String>,
    /// The Claude session id + cwd carried by the payload (U1), consumed by the
    /// dispatch to upsert the pane's resume record. `None` on a manual/older
    /// `fly notify`.
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    /// U7: the hook event name (e.g. "Stop", "SubagentStop"), used to close
    /// agent runs on first Stop occurrence (KTD-F). `None` on older hooks.
    pub hook_event: Option<String>,
}

/// Sink for authenticated callbacks. The app wires this to the attention
/// machine + frontend; tests record into a buffer.
pub type Dispatch = Arc<dyn Fn(PaneId, ValidatedHook) + Send + Sync>;

/// A running hook socket server. Dropping it shuts the server down and removes
/// the socket file.
pub struct HookServer {
    socket_path: PathBuf,
    stopping: Arc<AtomicBool>,
    accept_handle: Option<JoinHandle<()>>,
}

impl HookServer {
    /// Bind the socket and start accepting callbacks.
    pub fn start(
        socket_path: PathBuf,
        tokens: Arc<TokenRegistry>,
        dispatch: Dispatch,
    ) -> std::io::Result<HookServer> {
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Reclaim a stale socket left by a prior crash.
        let _ = std::fs::remove_file(&socket_path);

        let listener = UnixListener::bind(&socket_path)?;
        // Owner-only; the peer-UID check is the real gate, but tighten anyway.
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;

        let stopping = Arc::new(AtomicBool::new(false));
        let accept_stopping = Arc::clone(&stopping);
        let accept_handle = std::thread::Builder::new()
            .name("fly-hook-accept".into())
            .spawn(move || accept_loop(listener, tokens, dispatch, accept_stopping))?;

        Ok(HookServer {
            socket_path,
            stopping,
            accept_handle: Some(accept_handle),
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Stop accepting callbacks, join the accept thread, and remove the socket.
    pub fn shutdown(&mut self) {
        if self.accept_handle.is_none() {
            return;
        }
        self.stopping.store(true, Ordering::Release);
        // Wake the blocking accept by self-connecting once.
        let _ = UnixStream::connect(&self.socket_path);
        if let Some(h) = self.accept_handle.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

impl Drop for HookServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn accept_loop(
    listener: UnixListener,
    tokens: Arc<TokenRegistry>,
    dispatch: Dispatch,
    stopping: Arc<AtomicBool>,
) {
    for incoming in listener.incoming() {
        if stopping.load(Ordering::Acquire) {
            break;
        }
        match incoming {
            Ok(stream) => {
                let tokens = Arc::clone(&tokens);
                let dispatch = Arc::clone(&dispatch);
                // Handle each callback concurrently.
                let _ = std::thread::Builder::new()
                    .name("fly-hook-conn".into())
                    .spawn(move || handle_conn(stream, &tokens, &dispatch));
            }
            Err(_) => {
                if stopping.load(Ordering::Acquire) {
                    break;
                }
            }
        }
    }
}

fn handle_conn(stream: UnixStream, tokens: &TokenRegistry, dispatch: &Dispatch) {
    // The PTY is a trust boundary; only same-UID local peers may signal.
    if !peer_uid_matches(&stream) {
        return;
    }
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));

    let mut buf = Vec::new();
    if stream.take(MAX_MESSAGE).read_to_end(&mut buf).is_err() {
        return;
    }
    let msg: HookMessage = match serde_json::from_slice(&buf) {
        Ok(m) => m,
        Err(_) => return, // malformed → reject silently
    };
    if let Some(pane) = tokens.validate(&msg.token) {
        dispatch(
            pane,
            ValidatedHook {
                reason: msg.reason,
                title: msg.title,
                body: msg.body,
                session_id: msg.session_id,
                cwd: msg.cwd,
                hook_event: msg.hook_event,
            },
        );
    }
}

/// Verify the connecting peer's UID equals ours via `SO_PEERCRED`.
fn peer_uid_matches(stream: &UnixStream) -> bool {
    let fd = stream.as_raw_fd();
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt with a correctly-sized ucred out-param on a valid fd.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return false;
    }
    cred.uid == unsafe { libc::geteuid() }
}
