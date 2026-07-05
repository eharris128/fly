//! The local, read-only SSE HTTP server (U3) — the loopback realization of the
//! deferred KTD7 endpoint. Mirrors the hook socket's posture (`hooks/server.rs`):
//! thread-per-connection, constant-time token compare, silent rejection.
//!
//! **Security note.** This binds `127.0.0.1`, but a loopback TCP listener is
//! reachable by *any* local process (TCP has no `SO_PEERCRED` to gate on), so
//! the **bearer token is the only boundary**. Compare it in constant time; never
//! log it; reject a missing/bad token with a bare `401` and no body so a caller
//! can't probe whether agents exist. Read-only: no route mutates fly state.

use std::io::{self, Write};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use subtle::ConstantTimeEq;
use tiny_http::{Method, Response, Server};

use super::wire::AutomationEntry;
use super::FeedState;

/// How long the accept loop blocks per `recv` before re-checking the shutdown
/// flag — the max latency between teardown and the loop exiting.
const ACCEPT_POLL: Duration = Duration::from_millis(250);
/// SSE keepalive cadence: with no change for this long, emit a `: comment` frame
/// so a dead peer's socket write eventually errors and frees its thread.
const KEEPALIVE: Duration = Duration::from_secs(15);

/// Pulls the current automations projection at emit time (KTD4) — injected so
/// the server needs no direct `AutomationManager` dependency (testable).
pub type AutomationsFn = Arc<dyn Fn() -> Vec<AutomationEntry> + Send + Sync>;
/// Wall clock for the frame stamp — injected for deterministic tests.
pub type NowFn = Arc<dyn Fn() -> u64 + Send + Sync>;

/// A running feed server. Dropping it tears down cleanly: the accept loop stops
/// and every blocked SSE reader wakes (via [`FeedState::shutdown`]) and exits.
pub struct FeedServer {
    shutdown: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
    state: Arc<FeedState>,
    addr: SocketAddr,
}

impl FeedServer {
    /// Bind `127.0.0.1:port` and start accepting. `port` 0 lets the OS choose
    /// (used by tests; read it back via [`local_addr`](Self::local_addr)).
    pub fn start(
        port: u16,
        token: String,
        state: Arc<FeedState>,
        automations: AutomationsFn,
        now: NowFn,
    ) -> io::Result<Self> {
        // tiny_http returns a boxed error; normalize to io::Error for the caller.
        let server = Server::http((std::net::Ipv4Addr::LOCALHOST, port))
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        let addr = match server.server_addr() {
            tiny_http::ListenAddr::IP(a) => a,
            // Unix-socket listen addr is never used here (we bind an IP).
            _ => SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port)),
        };

        let shutdown = Arc::new(AtomicBool::new(false));
        let accept = {
            let shutdown = Arc::clone(&shutdown);
            let state = Arc::clone(&state);
            let token = Arc::new(token);
            std::thread::Builder::new()
                .name("fly-feed-accept".into())
                .spawn(move || accept_loop(server, shutdown, state, token, automations, now))?
        };

        Ok(Self {
            shutdown,
            accept: Some(accept),
            state,
            addr,
        })
    }

    /// The bound address (the concrete port when started with `port: 0`).
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for FeedServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Wake every blocked SSE reader so it observes shutdown and EOFs.
        self.state.shutdown();
        if let Some(h) = self.accept.take() {
            let _ = h.join();
        }
    }
}

fn accept_loop(
    server: Server,
    shutdown: Arc<AtomicBool>,
    state: Arc<FeedState>,
    token: Arc<String>,
    automations: AutomationsFn,
    now: NowFn,
) {
    while !shutdown.load(Ordering::SeqCst) {
        match server.recv_timeout(ACCEPT_POLL) {
            Ok(Some(req)) => {
                // Thread-per-connection: an SSE stream blocks for the life of
                // the client, so it must not stall the accept loop or other
                // consumers (mirrors the hook server's one-thread-per-conn).
                let state = Arc::clone(&state);
                let token = Arc::clone(&token);
                let automations = Arc::clone(&automations);
                let now = Arc::clone(&now);
                std::thread::spawn(move || handle(req, &state, &token, automations, now));
            }
            Ok(None) => continue, // timeout — re-check the shutdown flag
            Err(_) => break,      // listener dead
        }
    }
}

fn handle(
    req: tiny_http::Request,
    state: &Arc<FeedState>,
    token: &str,
    automations: AutomationsFn,
    now: NowFn,
) {
    // Only GET; the feed is strictly read-only.
    if *req.method() != Method::Get {
        let _ = req.respond(Response::empty(405));
        return;
    }
    let path = req.url().split('?').next().unwrap_or("").to_string();
    match path.as_str() {
        // Liveness probe — no auth, leaks nothing.
        "/healthz" => {
            let _ = req.respond(Response::from_string("ok"));
        }
        "/feed" => {
            if !authorized(&req, token) {
                // Silent rejection: bare 401, no body, no hint (KTD3).
                let _ = req.respond(Response::empty(401));
                return;
            }
            // Take the raw socket and stream SSE frames ourselves, flushing per
            // frame. tiny_http's reader-based responses go through a chunked
            // encoder that buffers small writes (frames wouldn't reach the
            // client until ~32 KiB accrued or the stream ended) — fatal for a
            // live push feed. Writing the socket directly keeps each frame
            // prompt. A raw (unchunked, no Content-Length) body that stays open
            // is exactly SSE-over-HTTP/1.1: the client reads until close.
            let writer = req.into_writer();
            stream_sse(writer, state, &automations, &now);
        }
        _ => {
            let _ = req.respond(Response::empty(404));
        }
    }
}

/// Constant-time bearer-token check. Missing header, wrong scheme, or mismatch
/// all fail identically. Never short-circuits on length in a way that leaks.
fn authorized(req: &tiny_http::Request, token: &str) -> bool {
    let presented = req
        .headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
        .map(|h| h.value.as_str())
        .and_then(|v| v.strip_prefix("Bearer "));
    match presented {
        Some(p) => p.len() == token.len() && bool::from(p.as_bytes().ct_eq(token.as_bytes())),
        None => false,
    }
}

/// Stream SSE frames on the raw socket until the client disconnects (a write
/// errors) or the feed tears down. Emits the current snapshot immediately, then
/// parks on the state's condvar and emits a fresh `data:` frame per version
/// bump, or a `: keepalive` comment when a wait times out (so a dead peer's
/// write eventually errors and frees the thread). Each write is flushed so
/// frames reach the consumer live.
fn stream_sse(
    mut w: Box<dyn Write + Send>,
    state: &Arc<FeedState>,
    automations: &AutomationsFn,
    now: &NowFn,
) {
    // Status line + SSE headers, written by hand (we own the socket now).
    let head = "HTTP/1.1 200 OK\r\n\
                Content-Type: text/event-stream\r\n\
                Cache-Control: no-cache\r\n\
                Connection: keep-alive\r\n\
                \r\n";
    if w.write_all(head.as_bytes()).and_then(|_| w.flush()).is_err() {
        return;
    }

    let mut last_version = match emit_frame(&mut w, state, automations, now) {
        Some(v) => v,
        None => return, // client already gone
    };
    loop {
        let res = state.wait_for_change(last_version, KEEPALIVE);
        if res.shutting_down {
            return;
        }
        if res.version > last_version {
            match emit_frame(&mut w, state, automations, now) {
                Some(v) => last_version = v,
                None => return, // client gone
            }
        } else if write_frame(&mut w, b": keepalive\n\n").is_err() {
            return; // idle keepalive failed → client gone
        }
    }
}

/// Build + write one `data:` frame; returns the emitted version, or `None` if
/// the write failed (client disconnected).
fn emit_frame(
    w: &mut Box<dyn Write + Send>,
    state: &Arc<FeedState>,
    automations: &AutomationsFn,
    now: &NowFn,
) -> Option<u64> {
    let snap = state.snapshot(automations(), now());
    let json = serde_json::to_string(&snap).unwrap_or_else(|_| "{}".into());
    let frame = format!("data: {json}\n\n");
    write_frame(w, frame.as_bytes()).ok().map(|_| snap.version)
}

fn write_frame(w: &mut Box<dyn Write + Send>, bytes: &[u8]) -> io::Result<()> {
    w.write_all(bytes)?;
    w.flush()
}
