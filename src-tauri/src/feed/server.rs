//! The local SSE + per-agent HTTP server (U3; feed-agent-reply-io U4) — the
//! loopback realization of the deferred KTD7 endpoint. Mirrors the hook
//! socket's posture (`hooks/server.rs`): thread-per-connection, constant-time
//! token compare, silent rejection.
//!
//! **Security note.** This binds `127.0.0.1`, but a loopback TCP listener is
//! reachable by *any* local process (TCP has no `SO_PEERCRED` to gate on), so
//! the **bearer token is the only boundary**. Compare it in constant time; never
//! log it; reject a missing/bad token with a bare `401` and no body so a caller
//! can't probe whether agents exist — every route except the inert `/healthz`
//! authenticates before it routes, so an unauthenticated caller can't even map
//! the surface. One deliberate mutation route exists (feed-agent-reply-io
//! KTD3): `POST /agents/{key}/input` writes to a published agent's PTY exactly
//! as local typing would — the payload is control-stripped and bracketed-paste
//! wrapped (`io::input_payload`), so a token holder can send *text*, never raw
//! terminal control sequences. Everything else stays read-only.
//!
//! Routes: `GET /healthz` (unauthenticated liveness), `GET /feed` (SSE),
//! `GET /agents/{key}/output` (latest reply), `POST /agents/{key}/input`
//! (submit a prompt). `{key}` is the roster's `leafKey`, taken verbatim
//! (KTD2); the published roster is what makes a key known — an unpublished
//! one is 404.

use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use subtle::ConstantTimeEq;
use tiny_http::{Method, Response, Server};

use super::wire::{AgentOutputBody, AutomationEntry};
use super::FeedState;
use crate::session::transcript::LastReply;

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
/// Resolves a leaf key's latest reply (feed-agent-reply-io U3) — injected so
/// the server needs no resume-store/transcript dependency. The ONE source both
/// `GET /agents/{key}/output` and the frame's `lastReplyAt` read (R3).
pub type ReplyFn = Arc<dyn Fn(&str) -> Option<LastReply> + Send + Sync>;
/// Delivers submitted text to a leaf's pane (feed-agent-reply-io U5) —
/// injected because delivery needs the PTY registry + attention manager +
/// AppHandle, none of which the server should know.
pub type InputFn = Arc<dyn Fn(&str, &str) -> InputOutcome + Send + Sync>;

/// What became of one `POST /agents/{key}/input` delivery attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputOutcome {
    /// Written to the pane's PTY — the request contract's `200 {"ok":true}`.
    Delivered,
    /// The leaf resolves to no live pane (closed/exited) — `404`.
    UnknownPane,
    /// The pane exists but the PTY write failed — `500`.
    Failed(String),
}

/// Everything a connection handler needs, bundled once at server start so the
/// accept loop clones a single `Arc` per connection.
struct HandlerCtx {
    state: Arc<FeedState>,
    token: String,
    automations: AutomationsFn,
    now: NowFn,
    replies: ReplyFn,
    input: InputFn,
}

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
        replies: ReplyFn,
        input: InputFn,
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
            let ctx = Arc::new(HandlerCtx {
                state: Arc::clone(&state),
                token,
                automations,
                now,
                replies,
                input,
            });
            std::thread::Builder::new()
                .name("fly-feed-accept".into())
                .spawn(move || accept_loop(server, shutdown, ctx))?
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

fn accept_loop(server: Server, shutdown: Arc<AtomicBool>, ctx: Arc<HandlerCtx>) {
    while !shutdown.load(Ordering::SeqCst) {
        match server.recv_timeout(ACCEPT_POLL) {
            Ok(Some(req)) => {
                // Thread-per-connection: an SSE stream blocks for the life of
                // the client, so it must not stall the accept loop or other
                // consumers (mirrors the hook server's one-thread-per-conn).
                let ctx = Arc::clone(&ctx);
                std::thread::spawn(move || handle(req, &ctx));
            }
            Ok(None) => continue, // timeout — re-check the shutdown flag
            Err(_) => break,      // listener dead
        }
    }
}

/// Cap on a `POST /agents/{key}/input` body. Prompts are human-scale; a cap
/// keeps a token holder from streaming unbounded bytes into memory.
const MAX_INPUT_BODY: usize = 64 * 1024;

fn handle(mut req: tiny_http::Request, ctx: &HandlerCtx) {
    let path = req.url().split('?').next().unwrap_or("").to_string();

    // Liveness probe — no auth, leaks nothing.
    if path == "/healthz" && *req.method() == Method::Get {
        let _ = req.respond(Response::from_string("ok"));
        return;
    }

    // Auth precedes routing on everything else: a caller without the token
    // gets the same bare 401 for every path and method, so it can't even map
    // which routes exist (KTD3 silent-rejection posture).
    if !authorized(&req, &ctx.token) {
        let _ = req.respond(Response::empty(401));
        return;
    }

    match (req.method().clone(), path.as_str()) {
        (Method::Get, "/feed") => {
            // Take the raw socket and stream SSE frames ourselves, flushing per
            // frame. tiny_http's reader-based responses go through a chunked
            // encoder that buffers small writes (frames wouldn't reach the
            // client until ~32 KiB accrued or the stream ended) — fatal for a
            // live push feed. Writing the socket directly keeps each frame
            // prompt. A raw (unchunked, no Content-Length) body that stays open
            // is exactly SSE-over-HTTP/1.1: the client reads until close.
            let writer = req.into_writer();
            stream_sse(writer, ctx);
        }
        (method, p) => match (method, agent_route(p)) {
            (Method::Get, Some((key, AgentEndpoint::Output))) => {
                let _ = req.respond(agent_output_response(ctx, &key));
            }
            (Method::Post, Some((key, AgentEndpoint::Input))) => {
                let resp = agent_input_response(ctx, &key, &mut req);
                let _ = req.respond(resp);
            }
            // A known agent route with the other verb is a method error;
            // anything else is unknown. (Both post-auth, so nothing leaks.)
            (_, Some(_)) => {
                let _ = req.respond(Response::empty(405));
            }
            _ => {
                let _ = req.respond(Response::empty(404));
            }
        },
    }
}

/// The two per-agent endpoints under `/agents/{key}/…` (feed-agent-reply-io U4).
enum AgentEndpoint {
    Output,
    Input,
}

/// Parse `/agents/{key}/output|input` → the verbatim key + endpoint. The key
/// is everything between the prefix and the final segment, taken as-is (KTD2:
/// it is the roster's `leafKey`, an opaque token like `leaf-12`); an empty key
/// falls through to 404.
fn agent_route(path: &str) -> Option<(String, AgentEndpoint)> {
    let rest = path.strip_prefix("/agents/")?;
    if let Some(key) = rest.strip_suffix("/output") {
        return (!key.is_empty()).then(|| (key.to_string(), AgentEndpoint::Output));
    }
    if let Some(key) = rest.strip_suffix("/input") {
        return (!key.is_empty()).then(|| (key.to_string(), AgentEndpoint::Input));
    }
    None
}

/// A JSON 200 with the standard header.
fn json_response(body: String) -> Response<io::Cursor<Vec<u8>>> {
    let content_type = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
        .expect("static header is valid");
    Response::from_string(body).with_header(content_type)
}

/// A body-less status, in the same concrete `Response` type as
/// [`json_response`] so route handlers share one return type.
fn empty_response(status: u16) -> Response<io::Cursor<Vec<u8>>> {
    Response::from_data(Vec::new()).with_status_code(status)
}

/// `GET /agents/{key}/output`: the latest reply, `{"text": "", …}` when the
/// agent exists but has not replied, `404` for a key outside the published
/// roster (KTD2).
fn agent_output_response(ctx: &HandlerCtx, key: &str) -> Response<io::Cursor<Vec<u8>>> {
    if !ctx.state.agent_exists(key) {
        return empty_response(404);
    }
    let body = match (ctx.replies)(key) {
        Some(reply) => AgentOutputBody {
            text: reply.text,
            replied_at: reply.replied_at_ms,
        },
        None => AgentOutputBody {
            text: String::new(),
            replied_at: None,
        },
    };
    json_response(serde_json::to_string(&body).unwrap_or_else(|_| "{\"text\":\"\"}".into()))
}

/// `POST /agents/{key}/input`: parse `{"text": …}` (body capped, malformed →
/// 400) and deliver through the injected seam. Roster membership gates the
/// route (KTD2) and the seam re-checks the pane is live — a raced pane close
/// between the two is a 404, not a write into nothing.
fn agent_input_response(
    ctx: &HandlerCtx,
    key: &str,
    req: &mut tiny_http::Request,
) -> Response<io::Cursor<Vec<u8>>> {
    if !ctx.state.agent_exists(key) {
        return empty_response(404);
    }
    let mut body = Vec::new();
    // Read one byte past the cap so an at-cap body is distinguishable from an
    // oversized one.
    if req
        .as_reader()
        .take(MAX_INPUT_BODY as u64 + 1)
        .read_to_end(&mut body)
        .is_err()
        || body.len() > MAX_INPUT_BODY
    {
        return empty_response(400);
    }
    #[derive(serde::Deserialize)]
    struct InputBody {
        text: String,
    }
    let Ok(input) = serde_json::from_slice::<InputBody>(&body) else {
        return empty_response(400);
    };
    match (ctx.input)(key, &input.text) {
        InputOutcome::Delivered => json_response("{\"ok\":true}".into()),
        InputOutcome::UnknownPane => empty_response(404),
        InputOutcome::Failed(e) => {
            // Never echo pane/write details to the caller; stderr is ours.
            eprintln!("[fly] feed input delivery failed: {e}");
            empty_response(500)
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
fn stream_sse(mut w: Box<dyn Write + Send>, ctx: &HandlerCtx) {
    // Status line + SSE headers, written by hand (we own the socket now).
    let head = "HTTP/1.1 200 OK\r\n\
                Content-Type: text/event-stream\r\n\
                Cache-Control: no-cache\r\n\
                Connection: keep-alive\r\n\
                \r\n";
    if w.write_all(head.as_bytes()).and_then(|_| w.flush()).is_err() {
        return;
    }

    let mut last_version = match emit_frame(&mut w, ctx) {
        Some(v) => v,
        None => return, // client already gone
    };
    loop {
        let res = ctx.state.wait_for_change(last_version, KEEPALIVE);
        if res.shutting_down {
            return;
        }
        if res.version > last_version {
            match emit_frame(&mut w, ctx) {
                Some(v) => last_version = v,
                None => return, // client gone
            }
        } else if write_frame(&mut w, b": keepalive\n\n").is_err() {
            return; // idle keepalive failed → client gone
        }
    }
}

/// Build + write one `data:` frame; returns the emitted version, or `None` if
/// the write failed (client disconnected). Each agent's `lastReplyAt` is
/// stamped here, at emit time, through the same resolver `GET
/// /agents/{key}/output` reads (feed-agent-reply-io U1/R3/KTD4) — the pushed
/// roster never carries it, so the stamp can't go stale in the cache, and a
/// version bump (a roster change, an automation mutation, or the post-Stop
/// settle bump in `lib.rs`) is what refreshes it on the wire.
fn emit_frame(w: &mut Box<dyn Write + Send>, ctx: &HandlerCtx) -> Option<u64> {
    let mut snap = ctx.state.snapshot((ctx.automations)(), (ctx.now)());
    for agent in &mut snap.agents {
        agent.last_reply_at = (ctx.replies)(&agent.leaf_key).and_then(|r| r.replied_at_ms);
    }
    let json = serde_json::to_string(&snap).unwrap_or_else(|_| "{}".into());
    let frame = format!("data: {json}\n\n");
    write_frame(w, frame.as_bytes()).ok().map(|_| snap.version)
}

fn write_frame(w: &mut Box<dyn Write + Send>, bytes: &[u8]) -> io::Result<()> {
    w.write_all(bytes)?;
    w.flush()
}
