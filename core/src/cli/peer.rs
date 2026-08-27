//! `fly agents` / `fly send` — the peer-messaging CLI half
//! (agent-peer-messaging U1/U2/U6).
//!
//! Both verbs are **socket ops** (`peer/list`, `peer/send`) and require a pane:
//! unlike `fly automation list` there is deliberately no direct-file read here,
//! because the roster is live in-memory state with no honest at-rest form
//! (KTD3) — a durable snapshot would list ghost agents after a crash with no
//! way to know. Staleness is instead answered with data: the response carries
//! the roster's `publishedAt` + the server's `now`, and this CLI *prints* the
//! staleness rather than inferring silently (R2).
//!
//! The wire structs here are the single request/response contract for the
//! `peer/*` ops, deserialized by the app-side handler (`lib.rs` →
//! `peer::dispatch_peer_op`) from the same bytes. Serde is camelCase, matching
//! the feed roster vocabulary these rows project (`paneId`, `publishedAt`).
//! **`PeerRequest` must never gain a field named `reason`** — that absence is
//! the version-skew guarantee (an old server routes an unknown op to notify,
//! whose parse then fails; see `hooks/protocol.rs::Envelope::is_peer`).

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Request posted for a `peer/*` op. The client fills `token`, `op`, and the
/// fields the op needs; op-specific fields are optional so one shape serves
/// both ops. `pane` is the **target** — the sender's identity is never on the
/// wire (KTD2: the server resolves it from the authenticated token).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerRequest {
    pub token: String,
    pub op: String,
    /// `peer/send`: the target pane id, exactly as `fly agents` printed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<u64>,
    /// `peer/send`: the raw message body (sanitized + framed server-side).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// One agent row of a `peer/list` response — a projection of the pushed
/// roster (`feed/wire.rs::AgentEntry`), not the entry itself: `lastReplyAt` /
/// `questionPendingAt` are backend-stamped at SSE emit and always null on the
/// pushed copy this op serves, so carrying them here would be an always-null
/// trap. `workingForMs` is the live activity signal (R1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerAgentRow {
    /// Identity for `fly send` (monotonic, never reused). Null while a pane
    /// is still being assigned — such a row is listed but untargetable.
    pub pane_id: Option<u64>,
    pub workspace: String,
    pub tab: String,
    pub cwd: Option<String>,
    pub status: String,
    pub working_for_ms: Option<f64>,
    /// Whether the human opted this pane into receiving peer messages (KTD6).
    pub peer_opt_in: bool,
    /// Whether this row is the calling pane itself.
    pub is_self: bool,
}

/// The `peer/list` payload: rows plus the explicit freshness facts (R2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerListPayload {
    pub agents: Vec<PeerAgentRow>,
    /// Epoch ms of the webview's last roster push, or null if nothing was
    /// ever pushed (webview still booting — treated as stale).
    pub published_at: Option<u64>,
    /// The server's clock at response time, so the caller can age the stamp.
    pub now: u64,
    /// Derived: `published_at` absent or older than the staleness threshold.
    pub stale: bool,
}

/// Response for a `peer/*` op. `error` carries a machine-readable code from
/// the closed refusal set (R7): `badRequest`, `selfSend`, `tooLong`,
/// `unknownPane`, `rosterStale`, `notOptedIn`, `rateLimited`, `askPending`,
/// `paneChanged`, `notAgent`, `deliveryFailed`, `submitIncomplete`,
/// `unavailable`, `unknownOp`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Optional human-oriented detail riding an error code (e.g. the PTY
    /// write failure text). Sanitized before printing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<PeerListPayload>,
}

impl PeerResponse {
    pub fn delivered() -> Self {
        PeerResponse {
            ok: true,
            ..Default::default()
        }
    }
    pub fn listed(list: PeerListPayload) -> Self {
        PeerResponse {
            ok: true,
            list: Some(list),
            ..Default::default()
        }
    }
    pub fn err(code: impl Into<String>) -> Self {
        PeerResponse {
            ok: false,
            error: Some(code.into()),
            ..Default::default()
        }
    }
    pub fn err_detail(code: impl Into<String>, detail: impl Into<String>) -> Self {
        PeerResponse {
            ok: false,
            error: Some(code.into()),
            detail: Some(detail.into()),
            ..Default::default()
        }
    }
    /// A response must always serialize; fall back to a minimal error object.
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self)
            .unwrap_or_else(|_| b"{\"ok\":false,\"error\":\"internal\"}".to_vec())
    }
}

// ---- socket plumbing --------------------------------------------------------

/// Resolve the pane token + socket path from the environment. Both peer verbs
/// are socket ops, so — unlike the automation read ops — even the listing
/// requires a pane (KTD3).
fn pane_env(verb: &str) -> Result<(String, String), String> {
    match (
        std::env::var("FLY_PANE_TOKEN"),
        std::env::var("FLY_SOCKET_PATH"),
    ) {
        (Ok(t), Ok(s)) if !t.is_empty() && !s.is_empty() => Ok((t, s)),
        _ => Err(format!(
            "fly {verb} must run inside a fly pane (FLY_PANE_TOKEN unset)"
        )),
    }
}

/// Post a peer request over the hook socket and read the response, with a
/// bounded wait (the `cli/automation.rs::send_request` shape).
pub fn send_request(socket: &Path, req: &PeerRequest) -> Result<PeerResponse, String> {
    let bytes = serde_json::to_vec(req).map_err(|e| e.to_string())?;
    let mut stream = UnixStream::connect(socket)
        .map_err(|e| format!("cannot reach fly at {}: {e}", socket.display()))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    stream.write_all(&bytes).map_err(|e| e.to_string())?;
    stream
        .shutdown(Shutdown::Write)
        .map_err(|e| e.to_string())?;
    let mut resp = Vec::new();
    if (&stream).take(64 * 1024).read_to_end(&mut resp).is_err() || resp.is_empty() {
        return Err("no response from fly within 5s".to_string());
    }
    serde_json::from_slice(&resp).map_err(|e| format!("bad response from fly: {e}"))
}

// ---- rendering --------------------------------------------------------------

/// Human text for a refusal code (R7). Unknown codes print as-is, so a newer
/// app's code degrades to something greppable rather than a lie.
pub fn refusal_text(code: &str) -> String {
    match code {
        "badRequest" => "malformed request (missing target pane or empty message)".into(),
        "selfSend" => "that is this pane — sending to yourself is refused".into(),
        "tooLong" => "message is over the size cap — shorten it and retry".into(),
        "unknownPane" => "no live pane has that id (run `fly agents` for current ids)".into(),
        "rosterStale" => {
            "fly's roster is stale (the window may be wedged) — refusing to act on it".into()
        }
        "notOptedIn" => {
            "that pane is not accepting peer messages (the human must toggle \
             'peers' on its dashboard row)"
                .into()
        }
        "rateLimited" => "rate limited — this pane is sending too fast; retry later".into(),
        "askPending" => {
            "that agent is blocked on a question at the machine — retry after it is answered"
                .into()
        }
        "paneChanged" => "that pane's session was replaced — run `fly agents` again".into(),
        "notAgent" => "that pane is no longer running an agent".into(),
        "deliveryFailed" => "delivery failed — nothing reached the pane".into(),
        "submitIncomplete" => {
            "the text was pasted but not submitted — it needs an Enter at the machine".into()
        }
        "unavailable" => "peer messaging is unavailable (fly is still starting?)".into(),
        other => other.into(),
    }
}

/// Render the listing as an aligned table. Pure, so the layout is testable.
pub fn render_agents_table(list: &PeerListPayload) -> String {
    let mut out = String::new();
    if list.agents.is_empty() {
        out.push_str("no agents on the roster\n");
        return out;
    }
    out.push_str(&format!(
        "{:<6} {:<8} {:<5} {:<20} {:<24} {}\n",
        "PANE", "STATUS", "PEERS", "WORKSPACE/TAB", "CWD", ""
    ));
    for a in &list.agents {
        let pane = a
            .pane_id
            .map(|p| p.to_string())
            .unwrap_or_else(|| "—".into());
        let wt = format!("{}/{}", a.workspace, a.tab);
        let peers = if a.peer_opt_in { "on" } else { "off" };
        let selfmark = if a.is_self { "(this pane)" } else { "" };
        out.push_str(&format!(
            "{:<6} {:<8} {:<5} {:<20} {:<24} {}\n",
            pane,
            a.status,
            peers,
            wt,
            a.cwd.as_deref().unwrap_or(""),
            selfmark
        ));
    }
    out
}

/// The staleness banner (R2): printed above the table whenever the roster
/// cannot be trusted as current. Empty when fresh.
pub fn staleness_banner(list: &PeerListPayload) -> Option<String> {
    if !list.stale {
        return None;
    }
    Some(match list.published_at {
        Some(p) => {
            let age_s = list.now.saturating_sub(p) / 1000;
            format!(
                "warning: roster is stale ({age_s}s since the last push) — \
                 the fly window may be wedged"
            )
        }
        None => "warning: no roster has been published yet — the fly window may still be starting"
            .into(),
    })
}

// ---- entry points -----------------------------------------------------------

/// `fly agents [--json]`.
pub fn run_agents(args: &[String]) -> i32 {
    let json = args.iter().any(|a| a == "--json");
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "usage: fly agents [--json]\n\
             List fly's live agent roster (must run inside a fly pane)."
        );
        return 0;
    }
    let (token, socket) = match pane_env("agents") {
        Ok(v) => v,
        Err(e) => {
            eprintln!("fly agents: {e}");
            return 1;
        }
    };
    let req = PeerRequest {
        token,
        op: "peer/list".into(),
        ..Default::default()
    };
    match send_request(Path::new(&socket), &req) {
        Ok(resp) if resp.ok => {
            if json {
                println!("{}", serde_json::to_string(&resp).unwrap_or_default());
                return 0;
            }
            match resp.list {
                Some(list) => {
                    if let Some(banner) = staleness_banner(&list) {
                        eprintln!("{banner}");
                    }
                    print!("{}", render_agents_table(&list));
                    0
                }
                None => {
                    eprintln!("fly agents: response carried no listing");
                    1
                }
            }
        }
        Ok(resp) => {
            let code = resp.error.unwrap_or_else(|| "unknown error".into());
            eprintln!(
                "fly agents: {}",
                crate::notify::sanitize_body(&refusal_text(&code))
            );
            1
        }
        Err(e) => {
            eprintln!("fly agents: {e}");
            1
        }
    }
}

/// `fly send <pane-id> <message…>` — the trailing args join with spaces; a
/// single `-` reads the message from stdin (bounded).
pub fn run_send(args: &[String]) -> i32 {
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!(
            "usage: fly send <pane-id> <message…>\n\
             Deliver a message into another agent's pane (must run inside a \
             fly pane; the target pane must have peer messages toggled on).\n\
             Pass `-` as the message to read it from stdin."
        );
        return if args.is_empty() { 2 } else { 0 };
    }
    let Ok(pane) = args[0].parse::<u64>() else {
        eprintln!(
            "fly send: {:?} is not a pane id — run `fly agents` for current ids",
            args[0]
        );
        return 2;
    };
    let rest = &args[1..];
    let message = if rest == ["-"] {
        let mut buf = String::new();
        // Bounded read: the socket rejects over-64KiB requests anyway, so cap
        // the local read at the same order rather than slurping unbounded.
        let mut limited = std::io::stdin().take(64 * 1024);
        if limited.read_to_string(&mut buf).is_err() {
            eprintln!("fly send: could not read the message from stdin");
            return 1;
        }
        buf
    } else {
        rest.join(" ")
    };
    if message.trim().is_empty() {
        eprintln!("fly send: the message is empty");
        return 2;
    }
    let (token, socket) = match pane_env("send") {
        Ok(v) => v,
        Err(e) => {
            eprintln!("fly send: {e}");
            return 1;
        }
    };
    let req = PeerRequest {
        token,
        op: "peer/send".into(),
        pane: Some(pane),
        message: Some(message),
    };
    match send_request(Path::new(&socket), &req) {
        Ok(resp) if resp.ok => {
            println!("delivered to pane {pane}");
            0
        }
        Ok(resp) => {
            let code = resp.error.unwrap_or_else(|| "unknown error".into());
            let mut line = refusal_text(&code);
            if let Some(d) = &resp.detail {
                line.push_str(&format!(" ({d})"));
            }
            eprintln!("fly send: {}", crate::notify::sanitize_body(&line));
            1
        }
        Err(e) => {
            eprintln!("fly send: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_request_wire_shape_is_minimal_and_reason_free() {
        // KTD2: the wire carries token/op/target/message and nothing else —
        // in particular no "from" (origin is token-resolved) and no "reason"
        // (the skew rule: a peer payload must fail the notify parse).
        let req = PeerRequest {
            token: "t".into(),
            op: "peer/send".into(),
            pane: Some(12),
            message: Some("hello".into()),
        };
        let v: serde_json::Value =
            serde_json::from_slice(&serde_json::to_vec(&req).unwrap()).unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["message", "op", "pane", "token"]);
    }

    #[test]
    fn peer_response_roundtrips_and_falls_back_on_serialize() {
        let resp = PeerResponse::err_detail("deliveryFailed", "pane write failed");
        let back: PeerResponse = serde_json::from_slice(&resp.to_bytes()).unwrap();
        assert!(!back.ok);
        assert_eq!(back.error.as_deref(), Some("deliveryFailed"));
        assert_eq!(back.detail.as_deref(), Some("pane write failed"));
    }

    fn row(pane: Option<u64>, opt_in: bool, is_self: bool) -> PeerAgentRow {
        PeerAgentRow {
            pane_id: pane,
            workspace: "home".into(),
            tab: "fly".into(),
            cwd: Some("/p".into()),
            status: "idle".into(),
            working_for_ms: None,
            peer_opt_in: opt_in,
            is_self,
        }
    }

    #[test]
    fn table_marks_self_opt_in_and_unassigned_panes() {
        let list = PeerListPayload {
            agents: vec![row(Some(7), true, true), row(None, false, false)],
            published_at: Some(1_000),
            now: 2_000,
            stale: false,
        };
        let t = render_agents_table(&list);
        assert!(t.contains("(this pane)"));
        assert!(t.contains(" on "));
        assert!(t.contains(" off "));
        assert!(t.contains("—"), "unassigned pane renders a dash");
        assert!(staleness_banner(&list).is_none());
    }

    #[test]
    fn staleness_banner_covers_old_and_never_pushed() {
        let mut list = PeerListPayload {
            agents: vec![],
            published_at: Some(1_000),
            now: 61_000,
            stale: true,
        };
        assert!(staleness_banner(&list).unwrap().contains("60s"));
        list.published_at = None;
        assert!(staleness_banner(&list).unwrap().contains("no roster"));
    }

    #[test]
    fn refusal_text_covers_every_wire_code() {
        // R7: the closed set — every code the dispatch can emit has a human
        // rendering that isn't the bare code.
        for code in [
            "badRequest",
            "selfSend",
            "tooLong",
            "unknownPane",
            "rosterStale",
            "notOptedIn",
            "rateLimited",
            "askPending",
            "paneChanged",
            "notAgent",
            "deliveryFailed",
            "submitIncomplete",
            "unavailable",
        ] {
            assert_ne!(refusal_text(code), code, "no rendering for {code}");
        }
    }
}
