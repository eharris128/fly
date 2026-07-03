//! Unix-domain socket server for the hook channel (U8, R10).

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use super::protocol::{Envelope, HookMessage};
use super::token::TokenRegistry;
use crate::pty::PaneId;
use crate::state::attention::Reason;

/// Cap on a single callback payload — defends against a peer streaming forever.
const MAX_MESSAGE: u64 = 64 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(2);
/// Bound the response write so a peer that connects but never reads can't wedge
/// an automation handler thread indefinitely (U9).
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

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
    /// fix-attribution U2: `fly notify --capture` — see [`Self::is_capture_only`].
    pub capture_only: bool,
}

impl ValidatedHook {
    /// Whether this message only captures a session id and must never raise
    /// attention (fix-session-pane-attribution U2, KTD1/R2). Two independent
    /// gates for binary-skew safety: the explicit `--capture` flag, **or** a
    /// `SessionStart` event name — so a stale installed `fly notify` that
    /// forwards the event without the flag still can't ring at session birth.
    /// The message's `reason` is deliberately ignored: a payload carrying both
    /// a raising reason and either gate raises nothing.
    pub fn is_capture_only(&self) -> bool {
        self.capture_only || self.hook_event.as_deref() == Some("SessionStart")
    }
}

/// Sink for authenticated callbacks. The app wires this to the attention
/// machine + frontend; tests record into a buffer.
pub type Dispatch = Arc<dyn Fn(PaneId, ValidatedHook) + Send + Sync>;

/// Handler for an authenticated `automation/…` request (U9). Given the
/// validated pane and the raw request bytes, it returns the response bytes to
/// write back (a JSON `{ok,…}` object). `lib.rs` injects one that closes over
/// the `AutomationManager`; the notify path never uses it, so it stays `None`
/// in tests that only exercise attention. Runs on the per-connection thread,
/// after token validation, outside every app lock the caller must avoid.
pub type RequestHandler = Arc<dyn Fn(PaneId, &[u8]) -> Vec<u8> + Send + Sync>;

/// A running hook socket server. Dropping it shuts the server down and removes
/// the socket file.
pub struct HookServer {
    socket_path: PathBuf,
    stopping: Arc<AtomicBool>,
    accept_handle: Option<JoinHandle<()>>,
}

impl HookServer {
    /// Bind the socket and start accepting callbacks (attention only — no
    /// automation request handler). Used by tests that exercise the notify path.
    pub fn start(
        socket_path: PathBuf,
        tokens: Arc<TokenRegistry>,
        dispatch: Dispatch,
    ) -> std::io::Result<HookServer> {
        Self::start_with_handler(socket_path, tokens, dispatch, None)
    }

    /// Bind the socket and start accepting callbacks, with an optional
    /// automation request handler (U9). The app wires the handler; the notify
    /// path is unchanged whether or not it is present.
    pub fn start_with_handler(
        socket_path: PathBuf,
        tokens: Arc<TokenRegistry>,
        dispatch: Dispatch,
        request_handler: Option<RequestHandler>,
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
            .spawn(move || {
                accept_loop(listener, tokens, dispatch, request_handler, accept_stopping)
            })?;

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
    request_handler: Option<RequestHandler>,
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
                let request_handler = request_handler.clone();
                // Handle each callback concurrently.
                let _ = std::thread::Builder::new()
                    .name("fly-hook-conn".into())
                    .spawn(move || {
                        handle_conn(stream, &tokens, &dispatch, request_handler.as_ref())
                    });
            }
            Err(_) => {
                if stopping.load(Ordering::Acquire) {
                    break;
                }
            }
        }
    }
}

fn handle_conn(
    mut stream: UnixStream,
    tokens: &TokenRegistry,
    dispatch: &Dispatch,
    request_handler: Option<&RequestHandler>,
) {
    // The PTY is a trust boundary; only same-UID local peers may signal.
    if !peer_uid_matches(&stream) {
        return;
    }
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));

    let mut buf = Vec::new();
    if (&stream).take(MAX_MESSAGE).read_to_end(&mut buf).is_err() {
        return;
    }
    // Two-stage parse (U9): read the envelope first so token validation (the
    // constant-time compare + lockout, the security boundary) happens once for
    // every message — notify and automation alike — before any op-specific work.
    let envelope: Envelope = match serde_json::from_slice(&buf) {
        Ok(e) => e,
        Err(_) => return, // malformed → reject silently
    };
    let Some(pane) = tokens.validate(&envelope.token) else {
        return; // unknown/invalid token → reject silently (lockout applies)
    };

    // Automation ops route to the request handler and get a response written
    // back; everything else (including an unknown op) is the fire-and-forget
    // notify path, unchanged.
    if envelope.is_automation() {
        if let Some(handler) = request_handler {
            let response = handler(pane, &buf);
            let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
            let _ = stream.write_all(&response);
            let _ = stream.flush();
            let _ = stream.shutdown(Shutdown::Write);
        }
        return;
    }

    let msg: HookMessage = match serde_json::from_slice(&buf) {
        Ok(m) => m,
        Err(_) => return, // malformed → reject silently
    };
    dispatch(
        pane,
        ValidatedHook {
            reason: msg.reason,
            title: msg.title,
            body: msg.body,
            session_id: msg.session_id,
            cwd: msg.cwd,
            hook_event: msg.hook_event,
            capture_only: msg.capture_only,
        },
    );
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
