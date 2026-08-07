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

use super::protocol::{AskPayload, Envelope, HookMessage, ASK_ACK_LINE};
use super::token::TokenRegistry;
use crate::pty::PaneId;
use crate::state::attention::Reason;

/// Cap on a single callback payload — defends against a peer streaming forever.
const MAX_MESSAGE: u64 = 64 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(2);
/// Bound the response write so a peer that connects but never reads can't wedge
/// an automation handler thread indefinitely (U9).
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
/// Wall-clock bound on the whole request phase (hook-ask-channel R2): a peer
/// trickling bytes could previously stretch the per-read timeout indefinitely;
/// with newline framing (which never EOFs) that would be an unbounded
/// pre-validation hold, so the phase gets a hard deadline instead.
const REQUEST_DEADLINE: Duration = Duration::from_secs(5);
/// Cadence of the held-connection loop (hook-ask-channel U2): how often a
/// holding thread re-checks its decision mailbox and probes the peer for
/// death. Latency ceiling between a local dialog resolution and the registry
/// clearing.
const HOLD_POLL: Duration = Duration::from_millis(250);

/// Cap on concurrent connection-handler threads per server (audit-remediation
/// U6/KTD6). Availability guard only: 64 is far above any legitimate load —
/// the feed has ~1 consumer, hook connections are short-lived except held
/// asks, which `feed/ask.rs::MAX_HELD_ASKS` bounds at 64 anyway — but without
/// it a local flooder could grow handler threads without bound: both surfaces
/// are thread-per-connection, the hook socket reachable by any same-uid
/// process and the feed port by any local uid.
pub const MAX_CONNECTIONS: usize = 64;

/// Bounded concurrent-connection counter for a thread-per-connection accept
/// loop (audit-remediation U6/KTD6), shared with `feed/server.rs`. A slot is
/// claimed on accept — *before* any read or auth work — and released by RAII
/// when the handler thread finishes, so a panicking handler can never leak a
/// slot. Over-cap connections are dropped immediately.
pub(crate) struct ConnCap {
    active: Arc<std::sync::atomic::AtomicUsize>,
    cap: usize,
}

impl ConnCap {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            active: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            cap,
        }
    }

    /// Claim a slot, or `None` at cap (the caller drops the connection).
    pub(crate) fn try_claim(&self) -> Option<ConnSlot> {
        self.active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.cap).then_some(n + 1)
            })
            .ok()
            .map(|_| ConnSlot(Arc::clone(&self.active)))
    }
}

/// RAII slot handle — dropping it frees the slot ([`ConnCap`]).
pub(crate) struct ConnSlot(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for ConnSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

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

/// Handler for an authenticated `peer/…` request (agent-peer-messaging
/// U1/KTD1): same contract, lifecycle, and threading as [`RequestHandler`] —
/// the validated *origin* pane (the token's pane, never the wire's — KTD2)
/// plus the raw bytes in, one bounded `{ok,…}` response out. Distinct alias so
/// the two seams can't be cross-wired silently at the constructor.
pub type PeerHandler = RequestHandler;

/// What the app-side registrar hands back for one accepted held ask
/// (hook-ask-channel U2/KTD1). The connection thread acks, then parks on
/// `decision_rx`: a received line is written to the client (a remote answer);
/// a closed channel means release (registry replacement or shutdown) — close
/// with no decision. `on_drop` is called only when the *peer* vanishes first
/// (the local answer killed the hook) so the registrar clears its entry; it is
/// generation-guarded upstream, so a late call after replacement is harmless.
pub struct AskTicket {
    pub decision_rx: std::sync::mpsc::Receiver<String>,
    pub on_drop: Box<dyn FnOnce() + Send>,
}

/// Registers a held ask for a validated pane (hook-ask-channel U2). `None`
/// declines the hold (registry at cap, or the pane resolves to no leaf): the
/// connection closes without an ack and the hook exits immediately — the
/// dialog proceeds normally, detection degrades to the existing chain (KTD2).
pub type AskHandler = Arc<dyn Fn(PaneId, AskPayload) -> Option<AskTicket> + Send + Sync>;

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
        Self::start_full(socket_path, tokens, dispatch, request_handler, None, None)
    }

    /// Bind the socket and start accepting callbacks, with the automation
    /// request handler, the held-ask handler (hook-ask-channel U2), and the
    /// peer-messaging handler (agent-peer-messaging U1). The full production
    /// constructor; the narrower ones delegate here.
    pub fn start_full(
        socket_path: PathBuf,
        tokens: Arc<TokenRegistry>,
        dispatch: Dispatch,
        request_handler: Option<RequestHandler>,
        ask_handler: Option<AskHandler>,
        peer_handler: Option<PeerHandler>,
    ) -> std::io::Result<HookServer> {
        if let Some(parent) = socket_path.parent() {
            create_private_socket_dir(parent)?;
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
                accept_loop(
                    listener,
                    tokens,
                    dispatch,
                    request_handler,
                    ask_handler,
                    peer_handler,
                    accept_stopping,
                )
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
    ask_handler: Option<AskHandler>,
    peer_handler: Option<PeerHandler>,
    stopping: Arc<AtomicBool>,
) {
    let cap = ConnCap::new(MAX_CONNECTIONS);
    for incoming in listener.incoming() {
        if stopping.load(Ordering::Acquire) {
            break;
        }
        match incoming {
            Ok(stream) => {
                // U6/KTD6: claim a slot before any read or auth work; at cap,
                // drop the connection immediately (silent, like every other
                // rejection on this socket).
                let Some(slot) = cap.try_claim() else {
                    drop(stream);
                    continue;
                };
                let tokens = Arc::clone(&tokens);
                let dispatch = Arc::clone(&dispatch);
                let request_handler = request_handler.clone();
                let ask_handler = ask_handler.clone();
                let peer_handler = peer_handler.clone();
                // Handle each callback concurrently.
                let _ = std::thread::Builder::new()
                    .name("fly-hook-conn".into())
                    .spawn(move || {
                        let _slot = slot;
                        handle_conn(
                            stream,
                            &tokens,
                            &dispatch,
                            request_handler.as_ref(),
                            ask_handler.as_ref(),
                            peer_handler.as_ref(),
                        )
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

/// Read one request: bytes up to the first `\n` (the held ask/hold framing —
/// trailing same-chunk bytes are a protocol violation and are discarded) or
/// EOF (the classic framing), whichever comes first, bounded by
/// [`MAX_MESSAGE`] and the [`REQUEST_DEADLINE`] wall clock (hook-ask-channel
/// R2 — with newline framing a byte-trickling peer never EOFs, so the
/// pre-validation phase needs a hard deadline, which also tightens the
/// classic path's previous per-read-timeout-only bound). `None` rejects.
fn read_request(stream: &mut UnixStream) -> Option<Vec<u8>> {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let deadline = std::time::Instant::now() + REQUEST_DEADLINE;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if std::time::Instant::now() >= deadline {
            return None;
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Some(buf), // EOF framing
            Ok(n) => {
                if let Some(pos) = chunk[..n].iter().position(|&b| b == b'\n') {
                    buf.extend_from_slice(&chunk[..pos]);
                    return Some(buf); // newline framing
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() as u64 > MAX_MESSAGE {
                    return None; // oversized → reject silently
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue // re-check the deadline
            }
            Err(_) => return None,
        }
    }
}

fn handle_conn(
    mut stream: UnixStream,
    tokens: &TokenRegistry,
    dispatch: &Dispatch,
    request_handler: Option<&RequestHandler>,
    ask_handler: Option<&AskHandler>,
    peer_handler: Option<&PeerHandler>,
) {
    // The PTY is a trust boundary; only same-UID local peers may signal.
    if !peer_uid_matches(&stream) {
        return;
    }
    let Some(buf) = read_request(&mut stream) else {
        return;
    };
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
            write_response(&mut stream, &response);
        }
        return;
    }

    // Peer ops (agent-peer-messaging U1): same request/response lifecycle as
    // an automation op, through the peer seam. The handler receives the
    // *token-resolved* origin pane (KTD2) — the wire cannot claim another.
    // With no handler wired the op is silently dropped, like an old server.
    if envelope.is_peer() {
        if let Some(handler) = peer_handler {
            let response = handler(pane, &buf);
            write_response(&mut stream, &response);
        }
        return;
    }

    // Held permission asks (hook-ask-channel U2/KTD1): register, ack, then
    // hold the connection for the ask's lifetime. Declined registration (or
    // no handler wired) closes without an ack — the client's ack deadline
    // reads that exactly like an old server and exits immediately.
    if envelope.is_ask_hold() {
        let ask: AskPayload = match serde_json::from_slice(&buf) {
            Ok(a) => a,
            Err(_) => return, // malformed → reject silently
        };
        if let Some(ticket) = ask_handler.and_then(|h| h(pane, ask)) {
            hold_ask(stream, ticket);
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

/// Write one bounded response and close the write half — the shared
/// request/response tail for the automation and peer arms. The write timeout
/// (U9) keeps a peer that connects but never reads from wedging the thread.
fn write_response(stream: &mut UnixStream, response: &[u8]) {
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let _ = stream.write_all(response);
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Write);
}

/// Hold a registered ask's connection until it resolves (hook-ask-channel
/// U2/KTD1): write the ack line, then park on the decision mailbox with a
/// peer-death probe each [`HOLD_POLL`]. Three exits:
/// - **decision received** — write it as one line and close (the hook prints
///   it, Claude applies it; the registrar already removed the entry);
/// - **mailbox closed** — release: close with no decision (registry
///   replacement or shutdown already forgot this ask; the hook exits quietly
///   and the dialog proceeds normally);
/// - **peer gone** — the local answer killed the hook (probed contract): call
///   `on_drop` so the registrar clears the entry, then exit.
fn hold_ask(mut stream: UnixStream, ticket: AskTicket) {
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    if stream
        .write_all(format!("{ASK_ACK_LINE}\n").as_bytes())
        .and_then(|()| stream.flush())
        .is_err()
    {
        (ticket.on_drop)();
        return;
    }
    let _ = stream.set_read_timeout(Some(HOLD_POLL));
    let mut scratch = [0u8; 256];
    loop {
        match ticket.decision_rx.recv_timeout(HOLD_POLL) {
            Ok(line) => {
                let _ = stream
                    .write_all(format!("{line}\n").as_bytes())
                    .and_then(|()| stream.flush());
                let _ = stream.shutdown(Shutdown::Both);
                return;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                let _ = stream.shutdown(Shutdown::Both);
                return;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => match stream.read(&mut scratch) {
                Ok(0) => {
                    // Peer closed: the ask resolved locally (or the hook
                    // timed out / claude exited) — clear the registry entry.
                    (ticket.on_drop)();
                    return;
                }
                // Stray post-request bytes: tolerated and discarded (bounded
                // per poll tick — a spamming peer costs one small read per
                // HOLD_POLL, never a wedge).
                Ok(_) => {}
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) => {
                    (ticket.on_drop)();
                    return;
                }
            },
        }
    }
}

/// Verify the connecting peer's UID equals ours via `SO_PEERCRED`.
#[cfg(target_os = "linux")]
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

/// Verify the connecting peer's UID equals ours via `getpeereid(2)` — the
/// macOS equivalent of Linux's `SO_PEERCRED` (same kernel-attested effective
/// UID of the connecting peer; failure rejects, identical to the Linux arm).
#[cfg(target_os = "macos")]
fn peer_uid_matches(stream: &UnixStream) -> bool {
    let fd = stream.as_raw_fd();
    let mut euid: libc::uid_t = 0;
    let mut egid: libc::gid_t = 0;
    // SAFETY: getpeereid with valid out-params on a valid connected socket fd.
    let rc = unsafe { libc::getpeereid(fd, &mut euid, &mut egid) };
    if rc != 0 {
        return false;
    }
    euid == unsafe { libc::geteuid() }
}

/// Create the socket's parent dir owner-only, on every path (audit-remediation
/// U7/KTD7 — belt and braces): mode `0700` at creation time via
/// `DirBuilderExt` (never left to umask — with `XDG_RUNTIME_DIR` unset the
/// dir lands under the world-writable system temp at a predictable name), and
/// an explicit chmod when the dir pre-exists. A pre-existing dir owned by
/// another uid (a temp-path squat) fails the chmod, so server start errors
/// instead of silently serving out of an attacker-controlled directory.
pub(crate) fn create_private_socket_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Audit-remediation U6/KTD6: the cap admits exactly `cap` concurrent
    // slots, over-cap claims fail, and dropping a slot (RAII) frees it.
    #[test]
    fn conn_cap_claims_to_cap_and_raii_drop_frees_a_slot() {
        let cap = ConnCap::new(2);
        let a = cap.try_claim().expect("slot 1");
        let _b = cap.try_claim().expect("slot 2");
        assert!(cap.try_claim().is_none(), "over-cap claim refused");
        drop(a);
        let _c = cap.try_claim().expect("freed slot reusable");
        assert!(cap.try_claim().is_none(), "back at cap");
    }

    // Audit-remediation U7/KTD7: the socket dir is 0700 whether freshly
    // created (runtime-dir shape, nested temp-fallback shape) or pre-existing
    // with looser modes.
    #[test]
    fn socket_dir_is_0700_on_create_and_tightened_when_pre_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;

        // Fresh create, single segment ($XDG_RUNTIME_DIR/<app> shape).
        let runtime = tmp.path().join("fly");
        create_private_socket_dir(&runtime).unwrap();
        assert_eq!(mode(&runtime), 0o700);

        // Fresh create, nested (temp-fallback shape).
        let nested = tmp.path().join("deep").join("fly");
        create_private_socket_dir(&nested).unwrap();
        assert_eq!(mode(&nested), 0o700);

        // Pre-existing dir with a loose mode is tightened, not trusted.
        let loose = tmp.path().join("loose");
        std::fs::create_dir(&loose).unwrap();
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o755)).unwrap();
        create_private_socket_dir(&loose).unwrap();
        assert_eq!(mode(&loose), 0o700);
    }
}
