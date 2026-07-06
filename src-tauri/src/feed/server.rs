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

use super::io::ResolvedIo;
use super::wire::{AgentOutputBody, AutomationEntry, QuestionBody};
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
/// Resolves a leaf key's IO facts — latest reply + pending question
/// (feed-agent-reply-io U3; widened by feed-pending-question U4) — injected so
/// the server needs no resume-store/transcript dependency. The ONE source
/// `GET /agents/{key}/output`, the frame's `lastReplyAt`, and the frame's
/// `questionPendingAt` all read (R3/R4).
pub type IoFn = Arc<dyn Fn(&str) -> ResolvedIo + Send + Sync>;
/// Delivers one input action to a leaf's pane (feed-agent-reply-io U5;
/// widened by feed-pending-question U6) — injected because delivery needs the
/// PTY registry + attention manager + AppHandle, none of which the server
/// should know. Both actions clear pane attention on delivery (the agent was
/// just answered), which also keeps a `reason: permission` from going stale
/// after a remote answer (KTD6/R9).
pub type InputFn = Arc<dyn Fn(&str, InputAction) -> InputOutcome + Send + Sync>;

/// Reads the live "may keys-mode answer a *permission* dialog?" opt-in
/// (feed-pending-question KTD6, Open Question resolved as config opt-in,
/// default off) — injected so the server needs no ConfigStore dependency and
/// a settings change applies without a restart.
pub type PermissionAnswersFn = Arc<dyn Fn() -> bool + Send + Sync>;

/// What one `POST /agents/{key}/input` delivers (KTD6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputAction {
    /// Today's contract: bracketed-paste the text, then Enter as its own
    /// delayed chunk — "type a prompt and submit it".
    Submit(String),
    /// Answer keys: raw R9-filtered bytes (`io::keys_payload`), NO paste wrap,
    /// NO auto-Enter — digit keys select picker options instantly, so a
    /// wrapped or entered payload would misfire (KTD6).
    Keys(Vec<u8>),
}

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
    io: IoFn,
    input: InputFn,
    permission_answers: PermissionAnswersFn,
    /// The per-leaf answered latch (feed-pending-question U6/R11): the
    /// `askedAt` of the last guarded delivery per leaf. A second delivery
    /// carrying the same `ifAskedAt` 409s until the pending question changes —
    /// the resolver lags a terminal answer by ≥100ms, so `ifAskedAt` alone
    /// leaves a TOCTOU window two racing consumers would both pass (AE7).
    /// Entries go stale naturally: a new question has a new `askedAt`, which
    /// no longer equals the latched value.
    latch: std::sync::Mutex<std::collections::HashMap<String, u64>>,
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
    #[allow(clippy::too_many_arguments)] // one injected seam per capability
    pub fn start(
        port: u16,
        token: String,
        state: Arc<FeedState>,
        automations: AutomationsFn,
        now: NowFn,
        io: IoFn,
        input: InputFn,
        permission_answers: PermissionAnswersFn,
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
                io,
                input,
                permission_answers,
                latch: std::sync::Mutex::new(std::collections::HashMap::new()),
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

/// The KTD3/KTD4 exposure gate, shared verbatim by the frame stamp and the
/// `/output` body so the two surfaces cannot apply different rules: a
/// **choice** question is always exposed (an AskUserQuestion never "executes",
/// so pending means waiting, from the transcript alone); a **permission**
/// question is exposed only while the roster entry's live attention reason is
/// `"permission"` (without that corroboration, a pending `tool_use` just means
/// the tool is executing). The reason is read outside the resolver cache, at
/// emit/response time — staleness degrades to "not exposed".
fn gated_question(question: Option<QuestionBody>, reason: Option<&str>) -> Option<QuestionBody> {
    question.filter(|q| q.kind != "permission" || reason == Some("permission"))
}

/// `GET /agents/{key}/output`: the latest reply plus the gated pending
/// question, `{"text": "", …}` when the agent exists but has no data, `404`
/// for a key outside the published roster (KTD2). Existence and reason come
/// from ONE roster snapshot ([`FeedState::agent_reason`]) so the gate can't
/// straddle a roster swap.
fn agent_output_response(ctx: &HandlerCtx, key: &str) -> Response<io::Cursor<Vec<u8>>> {
    let Some(reason) = ctx.state.agent_reason(key) else {
        return empty_response(404);
    };
    let resolved = (ctx.io)(key);
    let (text, replied_at) = match resolved.reply {
        Some(reply) => (reply.text, reply.replied_at_ms),
        None => (String::new(), None),
    };
    let body = AgentOutputBody {
        text,
        replied_at,
        question: gated_question(resolved.question, reason.as_deref()),
    };
    json_response(serde_json::to_string(&body).unwrap_or_else(|_| "{\"text\":\"\"}".into()))
}

/// `POST /agents/{key}/input`: parse `{"text", mode?, ifAskedAt?}` and deliver
/// through the injected seam (feed-agent-reply-io U5; feed-pending-question
/// U6/KTD6). Status precedence is pinned: 401 (upstream) → 404 (unpublished
/// key — before any pending comparison) → 400 (bad body / unknown mode / keys
/// without `ifAskedAt` / over-cap or empty-after-filter keys text) → 403
/// (keys answer to a permission dialog without the config opt-in) → 409
/// (stale-answer guard or answered latch). The seam re-checks the pane is
/// live — a raced pane close is a 404, not a write into nothing.
///
/// `mode` defaults to `"submit"` (today's paste + Enter, inject-anytime when
/// `ifAskedAt` is absent). `ifAskedAt` — mandatory for `"keys"`, optional for
/// `"submit"` — arms the R11 guard: the value must equal the current gated
/// pending question's `askedAt`, and the per-leaf latch admits one guarded
/// delivery per `askedAt` (reserved *before* the PTY write, released on a
/// failed delivery so a transient failure stays retryable).
fn agent_input_response(
    ctx: &HandlerCtx,
    key: &str,
    req: &mut tiny_http::Request,
) -> Response<io::Cursor<Vec<u8>>> {
    // One roster snapshot for existence AND the reason the guard gates on.
    let Some(reason) = ctx.state.agent_reason(key) else {
        return empty_response(404);
    };
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
    #[serde(rename_all = "camelCase")]
    struct InputBody {
        text: String,
        #[serde(default)]
        mode: Option<String>,
        #[serde(default)]
        if_asked_at: Option<u64>,
    }
    let Ok(input) = serde_json::from_slice::<InputBody>(&body) else {
        return empty_response(400);
    };

    let action = match input.mode.as_deref().unwrap_or("submit") {
        "submit" => InputAction::Submit(input.text.clone()),
        "keys" => {
            // KTD6: a keys answer without a guard could approve whatever
            // dialog happens to be up — ifAskedAt is mandatory, the text is
            // hard-capped (never truncated into the pane), and the R9 filter
            // must leave something deliverable.
            if input.if_asked_at.is_none()
                || input.text.chars().count() > super::io::KEYS_MAX_CHARS
            {
                return empty_response(400);
            }
            match super::io::keys_payload(&input.text) {
                Some(bytes) => InputAction::Keys(bytes),
                None => return empty_response(400),
            }
        }
        _ => return empty_response(400),
    };

    // The stale-answer guard + answered latch (R11), armed by ifAskedAt.
    let mut reserved = false;
    if let Some(asked) = input.if_asked_at {
        // Guard against the SAME gated view the read surfaces expose: an
        // unexposed question (nothing pending, or a permission fact without
        // the corroborating reason) is not answerable either.
        let Some(q) = gated_question((ctx.io)(key).question, reason.as_deref()) else {
            return empty_response(409);
        };
        if q.asked_at != asked {
            return empty_response(409);
        }
        if matches!(action, InputAction::Keys(_)) {
            if q.kind == "permission" && !(ctx.permission_answers)() {
                // The resolved Open Question: remote *permission* approval is
                // config-gated, default off — a digit can pick a durable
                // "don't ask again". Post-auth, so the 403 leaks nothing.
                return empty_response(403);
            }
            if q.kind == "choice" && !q.answerable {
                // R7: the guard rejects the shapes v1 can't answer
                // (multi-question, multiSelect, dropped-option mapping).
                return empty_response(409);
            }
        }
        // Reserve before delivering: of two racing same-ifAskedAt answers,
        // exactly one proceeds (AE7).
        let mut latch = ctx.latch.lock().unwrap();
        if latch.get(key) == Some(&asked) {
            return empty_response(409);
        }
        latch.insert(key.to_string(), asked);
        reserved = true;
    }

    let outcome = (ctx.input)(key, action);
    if reserved && outcome != InputOutcome::Delivered {
        // A failed delivery must stay retryable — release the reservation.
        ctx.latch.lock().unwrap().remove(key);
    }
    match outcome {
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
/// the write failed (client disconnected). Each agent's `lastReplyAt` **and**
/// `questionPendingAt` are stamped here, at emit time, through the same
/// resolver `GET /agents/{key}/output` reads (feed-agent-reply-io U1/R3;
/// feed-pending-question R4/KTD4) — the pushed roster never carries them, so
/// the stamps can't go stale in the cache, and a version bump (a roster
/// change, an automation mutation, or a settle bump in `lib.rs`) is what
/// refreshes them on the wire. The permission gate reads the entry's own
/// pushed `reason` — already in this snapshot — so a frame's marker and its
/// roster status are one consistent read (a `/output` served later re-reads
/// the reason live and may briefly disagree; R4 scopes strict equality to
/// choice-kind for exactly this reason).
fn emit_frame(w: &mut Box<dyn Write + Send>, ctx: &HandlerCtx) -> Option<u64> {
    let mut snap = ctx.state.snapshot((ctx.automations)(), (ctx.now)());
    for agent in &mut snap.agents {
        let resolved = (ctx.io)(&agent.leaf_key);
        agent.last_reply_at = resolved.reply.and_then(|r| r.replied_at_ms);
        agent.question_pending_at =
            gated_question(resolved.question, agent.reason.as_deref()).map(|q| q.asked_at);
    }
    let json = serde_json::to_string(&snap).unwrap_or_else(|_| "{}".into());
    let frame = format!("data: {json}\n\n");
    write_frame(w, frame.as_bytes()).ok().map(|_| snap.version)
}

fn write_frame(w: &mut Box<dyn Write + Send>, bytes: &[u8]) -> io::Result<()> {
    w.write_all(bytes)?;
    w.flush()
}
