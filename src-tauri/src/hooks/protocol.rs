//! Hook channel wire protocol (U8, R10).
//!
//! Transport: a Unix-domain socket (never TCP). The browser-reachable loopback
//! HTTP endpoint is deferred (KTD7) — v1 receives Claude Code callbacks via the
//! `command` hook → `fly notify` → this socket.
//!
//! Framing: the client opens a connection, writes one UTF-8 JSON object, and
//! closes its write half. The server reads to EOF (bounded), parses, and
//! authenticates.
//!
//! Schema:
//! ```json
//! { "token": "<hex>", "reason": "question|permission|finished|error|alert",
//!   "title": "<optional>", "body": "<optional>",
//!   "session_id": "<optional>", "cwd": "<optional>",
//!   "hook_event": "<optional>", "capture_only": false }
//! ```
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
