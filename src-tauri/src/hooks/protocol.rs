//! Hook channel wire protocol (U8, R10).
//!
//! Transport: a Unix-domain socket (never TCP). The loopback HTTP endpoint
//! KTD7 originally deferred has since shipped as the *separate* `feed/`
//! surface (bearer-token, loopback-only, its own boundary); this hook socket
//! remains Unix-only and receives Claude Code callbacks via the `command`
//! hook → `fly notify` → this socket.
//!
//! Framing: the client opens a connection, writes one UTF-8 JSON object, and
//! either closes its write half (the fire-and-forget notify path and the
//! `automation/*` request path — the server reads to EOF, bounded) **or**
//! terminates the object with `\n` and keeps the connection open (the
//! hook-ask-channel `op:"ask/hold"` path, U1/KTD1: the server stops reading at
//! the newline, writes an ack line, and *holds* the connection — its lifetime
//! mirrors the ask's lifetime, because Claude Code kills the hook process when
//! the dialog resolves locally). Both framings face the same size bound and a
//! request-phase deadline (`server.rs`), so neither can wedge a
//! pre-validation thread. Consequence: a message must be **one line** —
//! compact JSON with no raw newlines (what `serde_json::to_vec` always emits,
//! and what every fly client sends); a pretty-printed message truncates at
//! its first newline and is silently rejected as malformed.
//!
//! Schema (notify path):
//! ```json
//! { "token": "<hex>", "reason": "question|permission|finished|error|alert",
//!   "title": "<optional>", "body": "<optional>",
//!   "session_id": "<optional>", "cwd": "<optional>",
//!   "hook_event": "<optional>", "capture_only": false }
//! ```
//!
//! Schema (`op:"ask/hold"`, hook-ask-channel U1): the envelope's token/op plus
//! [`AskPayload`]'s all-optional fields. Server → client responses are single
//! JSON lines: the ack `{"ok":true,"held":true}` immediately after
//! registration, then either one decision object (the exact
//! `hookSpecificOutput` JSON the hook prints to stdout, hook-ask-channel R7)
//! or a bare close (release — the hook exits printing nothing and the dialog
//! proceeds normally).
//!
//! `capture_only` (fix-session-pane-attribution U2, KTD1) marks a message that
//! only updates the pane's resume record — the dispatch returns before the
//! attention machine, so it can never ring, record history, or banner. There is
//! deliberately **no** wire field selecting a trust rank: the dispatch stamps
//! every socket write `SessionSource::Hook` at the call site (KTD2), so a
//! client cannot self-declare `Pick`/`Poll` authority.
//!
//! Authentication & rejection rules:
//! - The connecting peer's UID must equal the app's UID (`SO_PEERCRED`).
//! - `token` must resolve to a live pane via a constant-time compare; unknown,
//!   missing, or malformed messages are rejected with no signal.
//! - Repeated invalid tokens trip a registry-wide lockout.
//! - Oversized payloads are truncated/rejected.

use serde::Deserialize;

use crate::state::attention::Reason;

/// The op a socket message selects (automations U9). Absent → `"notify"`, so
/// every pre-U9 `fly notify` message (which carries no `op`) still routes to
/// the attention path. `"automation/…"` ops route to the request handler
/// (which writes a response); anything else is treated as `notify`.
pub fn default_op() -> String {
    "notify".to_string()
}

/// The two-stage discriminator read before the full parse: the token (for the
/// same constant-time validation + lockout every message faces) and the `op`.
/// Kept minimal so both a [`HookMessage`] and an automation request deserialize
/// from the same bytes afterwards.
#[derive(Debug, Clone, Deserialize)]
pub struct Envelope {
    pub token: String,
    #[serde(default = "default_op")]
    pub op: String,
}

impl Envelope {
    /// Whether this op routes to the automations request handler (vs the
    /// fire-and-forget notify path). Only the `automation/` prefix qualifies;
    /// an unknown op degrades to notify (backward-compat).
    pub fn is_automation(&self) -> bool {
        self.op.starts_with("automation/")
    }

    /// Whether this op is a held permission-ask registration
    /// (hook-ask-channel U1/KTD1). On an OLD server this op falls through to
    /// the notify path, whose `HookMessage` parse fails on the missing
    /// `reason` — a silent reject, so skew can never raise attention (R8);
    /// the client's ack deadline handles its side.
    pub fn is_ask_hold(&self) -> bool {
        self.op == "ask/hold"
    }

    /// Whether this op is a tmux-substrate event report (tmux plan
    /// U4b/KTD12): `pane-died` / `attach-state` from a tmux `run-shell`
    /// hook via `fly substrate-event`. Authenticated by the SERVER-scope
    /// substrate token (the hook runs in the tmux server's context, which
    /// holds no pane token), validated inside the substrate handler —
    /// constant-time, silent reject, registry-lockout coupled. On an OLD
    /// server the op falls through to notify and dies on the missing
    /// `reason` field — the same skew rule as `ask/hold`.
    pub fn is_substrate(&self) -> bool {
        self.op == "substrate/event"
    }

    /// Whether this op routes to the peer-messaging request handler
    /// (agent-peer-messaging U1/KTD1): `peer/list` (`fly agents`) and
    /// `peer/send` (`fly send`). Same request/response lifecycle as an
    /// automation op — one bounded request in, one `{ok,…}` line out, never a
    /// held connection. Skew rule as `ask/hold`: on an OLD server the op
    /// falls through to notify, whose `HookMessage` parse fails on the
    /// missing `reason` — silent reject, so `PeerRequest` must never gain a
    /// field named `reason` (pinned by a test below).
    pub fn is_peer(&self) -> bool {
        self.op.starts_with("peer/")
    }
}

/// A tmux-substrate event report (tmux plan U4b/KTD12), sent by
/// `fly substrate-event` from a tmux hook. `kind` ∈ {"pane-died",
/// "attach-state"}; `session` is the marked session name (re-validated
/// against the KTD4 charset AND resolved against fly's own pane registry —
/// the wire never directly names a pane); `status` carries
/// `#{pane_dead_status}` or the attach flag. Events are hints: fly
/// re-verifies against tmux where the action is destructive.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct SubstrateEvent {
    pub token: String,
    pub op: String,
    pub kind: String,
    pub session: String,
    #[serde(default)]
    pub status: Option<i32>,
}

/// A callback payload sent by an agent (via `fly notify`).
///
/// `session_id`/`cwd`/`hook_event` are `#[serde(default)]` so an older `fly notify`
/// that predates them still deserializes — the installed binary and the app update
/// independently (U1, KTD-F). `hook_event` carries the triggering event name
/// (e.g. "Stop", "SubagentStop") for the U7 agent-run closure (KTD-F): a bare
/// `Finished` with no `hook_event` does not close the run, falling through to
/// the 30-min deadline (matches "hooks not installed" degradation).
/// `capture_only` (fix-attribution U2/KTD1, also `#[serde(default)]`) is set by
/// `fly notify --capture` — the `SessionStart` capture path that must never
/// raise; the dispatch equally treats a `SessionStart` `hook_event` as capture,
/// so a stale installed binary that forwards the event without the flag still
/// can't turn session birth into a ring.
#[derive(Debug, Clone, Deserialize)]
pub struct HookMessage {
    pub token: String,
    pub reason: Reason,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub hook_event: Option<String>,
    #[serde(default)]
    pub capture_only: bool,
}

/// A held permission-ask registration (hook-ask-channel U1, KTD4): the
/// bounded, typed subset `fly notify --permission-request` extracts from the
/// `PermissionRequest` hook payload. Every field is optional — an over-cap or
/// unparseable hook payload degrades to a body-less ask that still registers
/// "a dialog is up" (the held connection carries the resolution signal either
/// way). All strings are **raw and untrusted** here: the feed re-runs its
/// sanitize → scrub → truncate pipeline at serve time; the client-side caps
/// are a transport bound, not the sanitization boundary.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, Deserialize)]
pub struct AskPayload {
    /// The pending tool's name (`AskUserQuestion` → a choice ask; anything
    /// else → a permission ask).
    #[serde(default)]
    pub tool: Option<String>,
    /// The session's `permission_mode` at ask time (informational).
    #[serde(default)]
    pub permission_mode: Option<String>,
    /// Session attribution, same semantics as the notify path (upserted at
    /// `Hook` rank by the registration handler — hook-ask-channel KTD6).
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// The raw AskUserQuestion `{"questions":[…]}` input object, count- and
    /// string-capped client-side (KTD4); absent for permission asks.
    #[serde(default)]
    pub questions: Option<serde_json::Value>,
    /// One-line request summary for known permission tools (`Bash.command`,
    /// `Edit`/`Write.file_path`), pre-capped client-side.
    #[serde(default)]
    pub request: Option<String>,
}

/// The ack line written to a held client immediately after registration
/// (hook-ask-channel R8): its arrival within the client's deadline is what
/// distinguishes a holding server from an old one that silently ignored the
/// op. Newline-terminated on the wire.
pub const ASK_ACK_LINE: &str = "{\"ok\":true,\"held\":true}";

/// The decision line for a remotely answered permission ask
/// (hook-ask-channel R7): the exact `hookSpecificOutput` object the hook
/// process prints to stdout for Claude Code to apply (schema live-verified on
/// 2.1.207 — a mid-dialog decision dismisses the dialog). `allow_` selects the
/// behavior; both carry a fixed provenance message. Built here, next to the
/// wire schema, so the CLI and the answer path can never drift.
pub fn ask_decision_line(allow: bool) -> String {
    let (behavior, message) = if allow {
        ("allow", "Approved remotely via fly")
    } else {
        ("deny", "Denied remotely via fly")
    };
    format!(
        "{{\"hookSpecificOutput\":{{\"hookEventName\":\"PermissionRequest\",\
         \"decision\":{{\"behavior\":\"{behavior}\",\"message\":\"{message}\"}}}}}}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_a_wire_object_without_the_new_fields() {
        // An older `fly notify` sends no session_id/cwd; the message still
        // parses with both as None (backward-compatible, U1).
        let msg: HookMessage = serde_json::from_str(
            r#"{"token":"abc","reason":"finished","title":"t","body":"b"}"#,
        )
        .unwrap();
        assert_eq!(msg.token, "abc");
        assert_eq!(msg.reason, Reason::Finished);
        assert_eq!(msg.session_id, None);
        assert_eq!(msg.cwd, None);
    }

    #[test]
    fn deserializes_session_id_and_cwd_when_present() {
        let msg: HookMessage = serde_json::from_str(
            r#"{"token":"abc","reason":"finished","session_id":"s1","cwd":"/p"}"#,
        )
        .unwrap();
        assert_eq!(msg.session_id.as_deref(), Some("s1"));
        assert_eq!(msg.cwd.as_deref(), Some("/p"));
    }

    #[test]
    fn deserializes_a_hook_borne_alert_reason() {
        // `Reason` rides this wire format, so any pane may send
        // `reason: "alert"` via `fly notify` — accepted as valid (automations
        // KTD-H: the socket is per-pane authenticated and panes already
        // control title/body).
        let msg: HookMessage =
            serde_json::from_str(r#"{"token":"abc","reason":"alert"}"#).unwrap();
        assert_eq!(msg.reason, Reason::Alert);
    }

    #[test]
    fn deserializes_hook_event_when_present() {
        // `hook_event` threads the triggering event name for U7 agent-run
        // closure (KTD-F): `Finished` with `hook_event: "Stop"` closes the run
        // on first occurrence.
        let msg: HookMessage = serde_json::from_str(
            r#"{"token":"abc","reason":"finished","hook_event":"Stop"}"#,
        )
        .unwrap();
        assert_eq!(msg.hook_event.as_deref(), Some("Stop"));
    }

    #[test]
    fn envelope_defaults_op_to_notify_for_pre_u9_messages() {
        // A pre-U9 `fly notify` message carries no `op` — it must route to the
        // attention path, not the automations handler.
        let e: Envelope =
            serde_json::from_str(r#"{"token":"abc","reason":"finished"}"#).unwrap();
        assert_eq!(e.op, "notify");
        assert!(!e.is_automation());
    }

    #[test]
    fn envelope_routes_automation_ops_and_degrades_unknown_to_notify() {
        let create: Envelope =
            serde_json::from_str(r#"{"token":"t","op":"automation/create"}"#).unwrap();
        assert!(create.is_automation());
        // An op the server doesn't recognize is treated as notify (never a
        // response-writing path), so a future op can't wedge an old server.
        let unknown: Envelope =
            serde_json::from_str(r#"{"token":"t","op":"something/else"}"#).unwrap();
        assert!(!unknown.is_automation());
    }

    #[test]
    fn envelope_routes_peer_ops_and_their_payloads_never_parse_as_notify() {
        // agent-peer-messaging U1/KTD1: both peer ops route to the peer
        // handler, not the automation handler…
        let list: Envelope =
            serde_json::from_str(r#"{"token":"t","op":"peer/list"}"#).unwrap();
        assert!(list.is_peer());
        assert!(!list.is_automation());
        assert!(!list.is_ask_hold());
        let send_env: Envelope =
            serde_json::from_str(r#"{"token":"t","op":"peer/send"}"#).unwrap();
        assert!(send_env.is_peer());
        // …and on an OLD server (unknown op → notify), a peer payload — which
        // carries no `reason` — fails the HookMessage parse: silent reject,
        // never a spurious raise. This is the skew rule PeerRequest must
        // preserve by never gaining a `reason` field.
        let list_wire = r#"{"token":"t","op":"peer/list"}"#;
        assert!(serde_json::from_str::<HookMessage>(list_wire).is_err());
        let send_wire =
            r#"{"token":"t","op":"peer/send","pane":12,"message":"the run finished"}"#;
        assert!(serde_json::from_str::<HookMessage>(send_wire).is_err());
    }

    #[test]
    fn envelope_routes_ask_hold_and_its_payload_never_parses_as_notify() {
        // hook-ask-channel U1: the op routes to the held-ask path…
        let e: Envelope = serde_json::from_str(r#"{"token":"t","op":"ask/hold"}"#).unwrap();
        assert!(e.is_ask_hold());
        assert!(!e.is_automation());
        // …and on an OLD server (which treats an unknown op as notify), the
        // ask payload — which carries no `reason` — fails the HookMessage
        // parse: silent reject, never a spurious raise (R8 skew rule).
        let ask_wire = r#"{"token":"t","op":"ask/hold","tool":"Bash","request":"ls"}"#;
        assert!(serde_json::from_str::<HookMessage>(ask_wire).is_err());
    }

    #[test]
    fn ask_payload_all_fields_default_and_roundtrip() {
        // KTD4 degrade: a body-less ask (unparseable hook payload) is valid.
        let bare: AskPayload = serde_json::from_str("{}").unwrap();
        assert_eq!(bare, AskPayload::default());
        // Full shape round-trips.
        let full: AskPayload = serde_json::from_str(
            r#"{"tool":"AskUserQuestion","permission_mode":"bypassPermissions",
                "session_id":"s1","cwd":"/p",
                "questions":{"questions":[{"question":"Which?","options":[{"label":"A"}]}]}}"#,
        )
        .unwrap();
        assert_eq!(full.tool.as_deref(), Some("AskUserQuestion"));
        assert!(full.questions.is_some());
        assert_eq!(full.request, None);
    }

    #[test]
    fn ask_decision_line_is_the_verified_hook_schema() {
        // R7: exactly the shape the 2.1.207 probe confirmed Claude applies.
        let allow: serde_json::Value =
            serde_json::from_str(&ask_decision_line(true)).unwrap();
        assert_eq!(
            allow["hookSpecificOutput"]["hookEventName"],
            "PermissionRequest"
        );
        assert_eq!(allow["hookSpecificOutput"]["decision"]["behavior"], "allow");
        let deny: serde_json::Value =
            serde_json::from_str(&ask_decision_line(false)).unwrap();
        assert_eq!(deny["hookSpecificOutput"]["decision"]["behavior"], "deny");
        assert!(deny["hookSpecificOutput"]["decision"]["message"]
            .as_str()
            .unwrap()
            .contains("via fly"));
    }

    #[test]
    fn deserializes_without_capture_only_for_backward_compat() {
        // Every pre-fix message carries no capture_only — it must parse as a
        // normal (raising) message (fix-attribution U2).
        let msg: HookMessage =
            serde_json::from_str(r#"{"token":"abc","reason":"finished"}"#).unwrap();
        assert!(!msg.capture_only);
    }

    #[test]
    fn deserializes_capture_only_when_present() {
        // `fly notify --capture` sets the flag; the reason still parses (and is
        // ignored downstream, KTD1).
        let msg: HookMessage = serde_json::from_str(
            r#"{"token":"abc","reason":"question","capture_only":true,"session_id":"s1"}"#,
        )
        .unwrap();
        assert!(msg.capture_only);
        assert_eq!(msg.session_id.as_deref(), Some("s1"));
    }

    #[test]
    fn deserializes_without_hook_event_for_backward_compat() {
        // An older `fly notify` sends no hook_event; the message still parses
        // with it as None. A bare Finished (without hook_event) does not close
        // the run (degradation for hooks not installed, KTD-F).
        let msg: HookMessage =
            serde_json::from_str(r#"{"token":"abc","reason":"finished"}"#).unwrap();
        assert_eq!(msg.hook_event, None);
    }
}
