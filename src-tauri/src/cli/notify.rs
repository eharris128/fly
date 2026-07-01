//! `fly notify` — the bridge an agent hook uses to report attention (U9).
//!
//! Reads `FLY_PANE_TOKEN` and `FLY_SOCKET_PATH` from the environment and posts
//! a callback to the hook socket. Because the Claude Code hook is installed
//! globally, this is invoked for every Claude session — including ones not
//! running inside fly — so a missing env is a graceful no-op, not an error.

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;

use crate::state::attention::Reason;

/// Post a callback to the hook socket.
///
/// `session_id`/`cwd` ride the same authenticated message (U1, R6): they come
/// from the Claude payload and feed the resume store. Both are optional — a
/// manual `fly notify` (no `--claude`) sends neither, and an older installed
/// binary that predates these fields still deserializes server-side.
#[allow(clippy::too_many_arguments)]
pub fn send(
    socket_path: &Path,
    token: &str,
    reason: Reason,
    title: Option<&str>,
    body: Option<&str>,
    session_id: Option<&str>,
    cwd: Option<&str>,
) -> std::io::Result<()> {
    let payload = serde_json::json!({
        "token": token,
        "reason": reason.as_str(),
        "title": title,
        "body": body,
        "session_id": session_id,
        "cwd": cwd,
    });
    let bytes = serde_json::to_vec(&payload)?;
    let mut stream = UnixStream::connect(socket_path)?;
    stream.write_all(&bytes)?;
    stream.shutdown(Shutdown::Write)?;
    Ok(())
}

/// CLI entry for `fly notify <reason> [--claude] [--title T] [--body B]`.
pub fn run(args: &[String]) -> i32 {
    let mut reason: Option<Reason> = None;
    let mut from_claude = false;
    let mut title: Option<String> = None;
    let mut body: Option<String> = None;
    // Captured from the Claude payload and forwarded to the app for resume (U1).
    let mut session_id: Option<String> = None;
    let mut cwd: Option<String> = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--claude" => from_claude = true,
            "--title" => title = it.next().cloned(),
            "--body" => body = it.next().cloned(),
            other if !other.starts_with('-') && reason.is_none() => {
                reason = Reason::parse(other);
                if reason.is_none() {
                    eprintln!("fly notify: unknown reason {other:?}");
                    return 2;
                }
            }
            other => eprintln!("fly notify: ignoring unrecognized argument {other:?}"),
        }
    }

    // Refine reason/body and capture session_id/cwd from the Claude hook payload.
    if from_claude {
        if let Some(payload) = read_stdin_payload() {
            let parsed = parse_claude_payload(&payload);
            if let Some(r) = parsed.reason {
                reason = Some(r);
            }
            if body.is_none() {
                body = parsed.message;
            }
            session_id = parsed.session_id;
            cwd = parsed.cwd;
        }
    }

    let reason = reason.unwrap_or(Reason::Question);
    if title.is_none() {
        title = Some(default_title(reason).to_string());
    }

    // Not running inside a fly pane → quiet no-op (the hook is global).
    let (token, socket) = match (
        std::env::var("FLY_PANE_TOKEN"),
        std::env::var("FLY_SOCKET_PATH"),
    ) {
        (Ok(t), Ok(s)) if !t.is_empty() && !s.is_empty() => (t, s),
        _ => {
            eprintln!("fly notify: not inside a fly pane (FLY_PANE_TOKEN unset); skipping");
            return 0;
        }
    };

    match send(
        Path::new(&socket),
        &token,
        reason,
        title.as_deref(),
        body.as_deref(),
        session_id.as_deref(),
        cwd.as_deref(),
    ) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("fly notify: failed to reach fly socket {socket}: {e}");
            1
        }
    }
}

fn default_title(reason: Reason) -> &'static str {
    match reason {
        Reason::Question => "Claude is waiting for you",
        Reason::Permission => "Claude needs permission",
        Reason::Finished => "Claude finished",
        Reason::Error => "Claude hit an error",
    }
}

/// Read the Claude hook JSON from stdin, unless stdin is a terminal (manual run).
fn read_stdin_payload() -> Option<String> {
    // SAFETY: isatty on fd 0 is always safe.
    if unsafe { libc::isatty(0) } == 1 {
        return None;
    }
    let mut buf = String::new();
    std::io::stdin().take(64 * 1024).read_to_string(&mut buf).ok()?;
    if buf.trim().is_empty() {
        None
    } else {
        Some(buf)
    }
}

/// The fields fly extracts from a Claude Code Notification/Stop payload: the
/// refined attention reason + message (as before), plus the `session_id` and
/// `cwd` that ride the same payload and drive resume capture (U1, R2/R4/R6).
/// fly used to parse the payload for the reason and discard the rest.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ClaudePayload {
    pub reason: Option<Reason>,
    pub message: Option<String>,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
}

/// Map a Claude Code Notification/Stop payload to the fields fly cares about.
/// `session_id` and `cwd` are top-level strings on both event types; a payload
/// missing them (or any unparseable JSON) yields `None` for those fields with
/// the others unaffected.
pub fn parse_claude_payload(json: &str) -> ClaudePayload {
    let v: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return ClaudePayload::default(),
    };
    let str_field = |key: &str| {
        v.get(key)
            .and_then(|m| m.as_str())
            .map(|s| s.to_string())
    };

    // `Stop` events resolve to "finished"; Notification types refine question
    // vs permission when present.
    let reason = match v.get("hook_event_name").and_then(|e| e.as_str()) {
        Some("Stop") | Some("SubagentStop") => Some(Reason::Finished),
        _ => match v.get("notification_type").and_then(|t| t.as_str()) {
            Some(t) if t.contains("permission") => Some(Reason::Permission),
            Some(t) if t.contains("idle") || t.contains("input") => Some(Reason::Question),
            _ => None,
        },
    };
    ClaudePayload {
        reason,
        message: str_field("message"),
        session_id: str_field("session_id"),
        cwd: str_field("cwd"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_payload_extracts_session_id_and_cwd() {
        let p = parse_claude_payload(
            r#"{"hook_event_name":"Stop","message":"all done",
                "session_id":"sess-abc","cwd":"/home/u/proj"}"#,
        );
        assert_eq!(p.reason, Some(Reason::Finished));
        assert_eq!(p.message.as_deref(), Some("all done"));
        assert_eq!(p.session_id.as_deref(), Some("sess-abc"));
        assert_eq!(p.cwd.as_deref(), Some("/home/u/proj"));
    }

    #[test]
    fn notification_payload_extracts_session_id_and_cwd() {
        let p = parse_claude_payload(
            r#"{"hook_event_name":"Notification","notification_type":"permission_request",
                "session_id":"sess-xyz","cwd":"/srv/code"}"#,
        );
        assert_eq!(p.reason, Some(Reason::Permission));
        assert_eq!(p.session_id.as_deref(), Some("sess-xyz"));
        assert_eq!(p.cwd.as_deref(), Some("/srv/code"));
    }

    #[test]
    fn payload_without_session_fields_yields_none() {
        // The reason mapping is unaffected when session_id/cwd are absent.
        let p = parse_claude_payload(r#"{"hook_event_name":"Stop","message":"done"}"#);
        assert_eq!(p.reason, Some(Reason::Finished));
        assert_eq!(p.session_id, None);
        assert_eq!(p.cwd, None);
    }

    #[test]
    fn malformed_json_yields_default() {
        assert_eq!(parse_claude_payload("{ not json"), ClaudePayload::default());
    }

    #[test]
    fn notification_idle_prompt_is_question() {
        // `idle_prompt` (Claude waiting for your input) refines to Question — the
        // fast-unlock reason the dashboard triage ranks ahead of finished (R1/R2).
        let p = parse_claude_payload(
            r#"{"hook_event_name":"Notification","notification_type":"idle_prompt",
                "session_id":"sess-1","cwd":"/w"}"#,
        );
        assert_eq!(p.reason, Some(Reason::Question));
    }

    #[test]
    fn notification_unrecognized_type_yields_no_reason() {
        // An unrecognized or absent notification_type leaves reason None, so the
        // installed hook's CLI-arg fallback (Permission) stands (KTD5). It never
        // silently refines to Question.
        let unknown = parse_claude_payload(
            r#"{"hook_event_name":"Notification","notification_type":"auth_success"}"#,
        );
        assert_eq!(unknown.reason, None);
        let absent =
            parse_claude_payload(r#"{"hook_event_name":"Notification","message":"hi"}"#);
        assert_eq!(absent.reason, None);
    }

    #[test]
    fn error_reason_is_never_derived() {
        // Error is not produced from a hook payload in v1 (R3); no input shape
        // yields it.
        for json in [
            r#"{"hook_event_name":"Stop"}"#,
            r#"{"hook_event_name":"Notification","notification_type":"idle_prompt"}"#,
            r#"{"hook_event_name":"Notification","notification_type":"permission_prompt"}"#,
            r#"{"hook_event_name":"Notification","notification_type":"whatever"}"#,
            "{ not json",
        ] {
            assert_ne!(parse_claude_payload(json).reason, Some(Reason::Error));
        }
    }
}
