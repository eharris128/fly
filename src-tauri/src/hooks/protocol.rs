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
//! { "token": "<hex>", "reason": "question|permission|finished|error",
//!   "title": "<optional>", "body": "<optional>" }
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
#[derive(Debug, Clone, Deserialize)]
pub struct HookMessage {
    pub token: String,
    pub reason: Reason,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
}
