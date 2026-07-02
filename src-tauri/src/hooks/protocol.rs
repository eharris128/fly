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
//!   "session_id": "<optional>", "cwd": "<optional>" }
//! ```
//!
//! Authentication & rejection rules:
//! - The connecting peer's UID must equal the app's UID (`SO_PEERCRED`).
//! - `token` must resolve to a live pane via a constant-time compare; unknown,
//!   missing, or malformed messages are rejected with no signal.
//! - Repeated invalid tokens trip a registry-wide lockout.
//! - Oversized payloads are truncated/rejected.

use serde::Deserialize;

use crate::state::attention::Reason;

/// A callback payload sent by an agent (via `fly notify`).
///
/// `session_id`/`cwd` are `#[serde(default)]` so an older `fly notify` that
/// predates them still deserializes — the installed binary and the app update
/// independently (U1).
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
}
