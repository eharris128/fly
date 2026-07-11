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
//! as local typing would — every mode's payload is control-stripped
//! (`io::{paste_payload,keys_payload,other_payload}`), so a token holder can
//! send *text* and answer keys, never raw terminal control sequences.
//! Everything else stays read-only.
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
/// `questionPendingAt` all read (R3/R4). The second and third arguments are
/// the leaf's live attention reason and dashboard status from the caller's
/// single roster read (feed-question-screen-fallback KTD4): the screen
/// fallback behind this seam uses the reason as an accelerator and declines
/// to engage its uncorroborated leg on a `working` pane, and passing the same
/// snapshot the caller gates on keeps the resolution and the gate coherent.
pub type IoFn = Arc<dyn Fn(&str, Option<&str>, &str) -> ResolvedIo + Send + Sync>;
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
    /// A free-text answer typed into the pending picker's own
    /// "Type something." row (feed-other-answer KTD1): fly owns the keystroke
    /// choreography — `select` (the row's digit, from the guarded question's
    /// `otherKey`), then `text` (`io::other_payload` bytes), then Enter, each
    /// as its OWN delay-spaced PTY chunk. The live probe (2.1.206) pinned all
    /// three boundaries: a digit coalesced with the text is dropped, text at
    /// an unfocused picker is ignored, and a bracketed paste's leading ESC
    /// cancels the picker outright — so no chunk here ever contains an ESC
    /// byte, and the gaps are mandatory, not a nicety.
    Other { select: Vec<u8>, text: Vec<u8> },
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

/// A JSON body with a non-200 status — used only for the post-auth policy
/// refusal that needs a discriminator (see the `403` below). A body here is
/// safe: the caller is already authenticated, so it leaks nothing the
/// silent-rejection posture protects (that posture is about *unauthenticated*
/// probing, which stays a bare 401).
fn json_status(status: u16, body: &str) -> Response<io::Cursor<Vec<u8>>> {
    let content_type = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
        .expect("static header is valid");
    Response::from_string(body)
        .with_header(content_type)
        .with_status_code(status)
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
/// question and the conversation tail (feed-conversation-tail R1 — ungated:
/// turns are completed history, already scrubbed/capped by the resolver),
/// `{"text": "", …}` when the agent exists but has no data, `404` for a key
/// outside the published roster (KTD2). Existence, reason, and status come
/// from ONE roster snapshot ([`FeedState::agent_gate`]) so the gate can't
/// straddle a roster swap.
fn agent_output_response(ctx: &HandlerCtx, key: &str) -> Response<io::Cursor<Vec<u8>>> {
    let Some(gate) = ctx.state.agent_gate(key) else {
        return empty_response(404);
    };
    let resolved = (ctx.io)(key, gate.reason.as_deref(), &gate.status);
    let (text, replied_at) = match resolved.reply {
        Some(reply) => (reply.text, reply.replied_at_ms),
        None => (String::new(), None),
    };
    let body = AgentOutputBody {
        text,
        replied_at,
        question: gated_question(resolved.question, gate.reason.as_deref()),
        turns: resolved.turns,
    };
    json_response(serde_json::to_string(&body).unwrap_or_else(|_| "{\"text\":\"\"}".into()))
}

/// `POST /agents/{key}/input`: parse `{"text", mode?, ifAskedAt?}` and deliver
/// through the injected seam (feed-agent-reply-io U5; feed-pending-question
/// U6/KTD6; feed-other-answer U2). Status precedence is pinned, in the order
/// the code checks it: 401 (upstream auth) → 404 (unpublished key, before any
/// pending comparison) → 400 (bad body / unknown mode / keys or other without
/// `ifAskedAt` / over-cap or empty-after-filter answer text) → **409** (the
/// guarded question is not exposed — nothing pending, reason gone, or
/// `ifAskedAt` mismatch) → **403** (a guarded answer to a *permission* dialog
/// without the config opt-in) → **409** (a keys/other answer to a shape it
/// cannot complete, or the answered latch already holds this `askedAt`). The
/// 403 carries a JSON discriminator body
/// (`{"error":"permissionAnswersDisabled"}`) so a consumer does not mistake
/// this policy refusal for an auth failure — auth failures in this feed are
/// always a bare 401, never a 403.
///
/// `mode` defaults to `"submit"` (today's paste + Enter, inject-anytime when
/// `ifAskedAt` is absent); `"keys"` sends raw answer keys; `"other"`
/// (feed-other-answer) types `text` into the pending picker's own
/// "Type something." free-text row and submits it — fly resolves the row's
/// digit from the guarded question's `otherKey` and owns the three-chunk
/// choreography, so the payload never contains an ESC byte the picker could
/// read as cancel. `ifAskedAt` — mandatory for `"keys"` and `"other"`,
/// optional for `"submit"` — arms the R11 guard against the freshly re-read
/// reason (not the entry snapshot, so a slow body read can't gate on a stale
/// dialog state): the value must equal the current gated pending question's
/// `askedAt`, and the per-leaf latch admits one guarded delivery per `askedAt`
/// (reserved *before* the PTY write, released on a failed delivery — but only
/// if this request's own reservation is still the one held, so a concurrent
/// newer answer's reservation is never clobbered).
///
/// The permission opt-in gate covers **any** guarded delivery whose gated
/// question is `permission`-kind, not only `mode:"keys"` — a guarded submit's
/// trailing Enter confirms the dialog's default just as a digit does, so both
/// are remote permission approval and both require the opt-in. An *unguarded*
/// submit (no `ifAskedAt`) stays the pre-existing inject-anytime contract,
/// byte-for-byte.
fn agent_input_response(
    ctx: &HandlerCtx,
    key: &str,
    req: &mut tiny_http::Request,
) -> Response<io::Cursor<Vec<u8>>> {
    // Existence gate only (404). The reason the guard acts on is re-read fresh
    // below, after the body read, so it can't be a stale pre-body snapshot.
    if ctx.state.agent_gate(key).is_none() {
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

    let mut action = match input.mode.as_deref().unwrap_or("submit") {
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
        "other" => {
            // feed-other-answer R1/R6: same mandatory-guard posture as keys —
            // an unguarded Other answer could type into whatever dialog is up
            // — with the sentence-scale cap instead of the digit cap. The
            // select digit is resolved from the guarded question below; only
            // the text half is built here.
            if input.if_asked_at.is_none()
                || input.text.chars().count() > super::io::OTHER_MAX_CHARS
            {
                return empty_response(400);
            }
            match super::io::other_payload(&input.text) {
                // A placeholder select — the guard block below either fills
                // it from the question's otherKey or 409s.
                Some(bytes) => InputAction::Other {
                    select: Vec::new(),
                    text: bytes,
                },
                None => return empty_response(400),
            }
        }
        _ => return empty_response(400),
    };

    // The stale-answer guard + answered latch (R11), armed by ifAskedAt.
    let mut reserved = false;
    if let Some(asked) = input.if_asked_at {
        // Re-read the roster gate NOW (after the body read) and gate the
        // question on it, so reason + question are one fresh, body-independent
        // read — a paced/chunked POST can't slip a locally-dismissed dialog's
        // stale reason past the gate. A key that vanished meanwhile →
        // unexposed (409, the same as "nothing pending").
        let Some(gate) = ctx.state.agent_gate(key) else {
            return empty_response(409);
        };
        let reason = gate.reason;
        let Some(q) = gated_question(
            (ctx.io)(key, reason.as_deref(), &gate.status).question,
            reason.as_deref(),
        ) else {
            return empty_response(409);
        };
        if q.asked_at != asked {
            return empty_response(409);
        }
        // Any guarded answer to a permission dialog is remote permission
        // approval (a digit or a submit's Enter both confirm it), so the
        // config opt-in gates both modes. Post-auth JSON body disambiguates
        // the policy refusal from an auth 401.
        //
        // Widened for screen-derived bodies (feed-question-screen-fallback
        // KTD6, belt and braces): v2.1.206 labels an AskUserQuestion wait a
        // "permission prompt", so a screen classification could in principle
        // read a permission dialog as a choice picker. The opt-in therefore
        // also gates any SCREEN-derived guarded answer delivered while the
        // pane's live reason is `permission` — the failure direction is "an
        // ask needs the opt-in", never "a permission bypasses it". A
        // transcript-derived choice under a permission reason stays un-gated
        // (its classification comes from the tool name, which is authoritative).
        let screen_under_permission = q.source.as_deref() == Some("screen")
            && reason.as_deref() == Some("permission");
        if (q.kind == "permission" || screen_under_permission) && !(ctx.permission_answers)() {
            return json_status(403, "{\"error\":\"permissionAnswersDisabled\"}");
        }
        // R7: a keys answer can only complete the one v1-answerable shape;
        // reject the rest (multi-question, multiSelect, dropped-option mapping)
        // rather than fire a mis-mapped digit into the picker.
        if matches!(action, InputAction::Keys(_)) && q.kind == "choice" && !q.answerable {
            return empty_response(409);
        }
        // feed-other-answer R4: an Other answer additionally needs a known
        // free-text-row digit (`otherKey`), which only an answerable choice
        // body carries — a permission dialog has no Other row, and an
        // unanswerable shape's digits can't be trusted. Anything else 409s
        // rather than type into an unknown UI state (where the trailing Enter
        // would select whatever option is highlighted — the live-probed
        // failure this guard exists to prevent).
        if let InputAction::Other { select, .. } = &mut action {
            match (q.kind == "choice" && q.answerable)
                .then(|| q.questions.first().and_then(|s| s.other_key.clone()))
                .flatten()
            {
                Some(digit) => *select = digit.into_bytes(),
                None => return empty_response(409),
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
        // A failed delivery must stay retryable — release the reservation, but
        // ONLY this request's own generation (see [`release_own_reservation`]).
        if let Some(asked) = input.if_asked_at {
            release_own_reservation(&ctx.latch, key, asked);
        }
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

/// Release a leaf's answered-latch reservation **only when it still holds this
/// request's own `asked` generation** (feed-pending-question R11). Between a
/// request's reserve and its post-delivery release, a concurrent newer
/// question can reserve a *different* `askedAt` for the same leaf; an
/// unconditional `remove(key)` would erase that newer reservation and re-open
/// the exactly-once guard for an answer that already delivered. The
/// compare-and-remove makes a failed delivery release nothing but its own
/// reservation.
fn release_own_reservation(
    latch: &std::sync::Mutex<std::collections::HashMap<String, u64>>,
    key: &str,
    asked: u64,
) {
    let mut latch = latch.lock().unwrap();
    if latch.get(key) == Some(&asked) {
        latch.remove(key);
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
        let resolved = (ctx.io)(&agent.leaf_key, agent.reason.as_deref(), &agent.status);
        agent.last_reply_at = resolved.reply.and_then(|r| r.replied_at_ms);
        // The marker: a (gated) question body's own stamp, else the tier-1
        // pending signal (feed-question-screen-fallback R2) — the resolver
        // sets `pending_fallback_at` only for a corroborated-waiting pane, so
        // "question pending · body unavailable" still surfaces.
        agent.question_pending_at =
            gated_question(resolved.question, agent.reason.as_deref())
                .map(|q| q.asked_at)
                .or(resolved.pending_fallback_at);
    }
    let json = serde_json::to_string(&snap).unwrap_or_else(|_| "{}".into());
    let frame = format!("data: {json}\n\n");
    write_frame(w, frame.as_bytes()).ok().map(|_| snap.version)
}

fn write_frame(w: &mut Box<dyn Write + Send>, bytes: &[u8]) -> io::Result<()> {
    w.write_all(bytes)?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    fn choice(asked_at: u64, answerable: bool) -> QuestionBody {
        QuestionBody {
            asked_at,
            kind: "choice".into(),
            tool: "AskUserQuestion".into(),
            answerable,
            context: None,
            questions: vec![],
            request: None,
            source: None,
        }
    }

    fn permission(asked_at: u64) -> QuestionBody {
        QuestionBody {
            asked_at,
            kind: "permission".into(),
            tool: "Bash".into(),
            answerable: false,
            context: None,
            questions: vec![],
            request: Some("cargo build".into()),
            source: None,
        }
    }

    #[test]
    fn gated_question_always_exposes_choice_but_gates_permission_on_reason() {
        // Choice rides regardless of reason (an ask never "executes").
        assert!(gated_question(Some(choice(1, true)), None).is_some());
        assert!(gated_question(Some(choice(1, true)), Some("permission")).is_some());
        // Permission needs the corroborating reason; anything else hides it.
        assert!(gated_question(Some(permission(1)), Some("permission")).is_some());
        assert!(gated_question(Some(permission(1)), None).is_none());
        assert!(gated_question(Some(permission(1)), Some("question")).is_none());
        // Nothing pending stays nothing.
        assert!(gated_question(None, Some("permission")).is_none());
    }

    #[test]
    fn release_own_reservation_only_removes_this_generation() {
        let latch: Mutex<HashMap<String, u64>> = Mutex::new(HashMap::new());
        // Holds our value → removed (a failed delivery stays retryable).
        latch.lock().unwrap().insert("leaf".into(), 100);
        release_own_reservation(&latch, "leaf", 100);
        assert!(latch.lock().unwrap().get("leaf").is_none());

        // Holds a NEWER generation's value → left intact (no clobber): a
        // concurrent newer answer reserved 200 between our reserve and release.
        latch.lock().unwrap().insert("leaf".into(), 200);
        release_own_reservation(&latch, "leaf", 100);
        assert_eq!(latch.lock().unwrap().get("leaf"), Some(&200));

        // Absent key → no-op, no panic.
        release_own_reservation(&latch, "other", 100);
        assert!(latch.lock().unwrap().get("other").is_none());
    }
}
