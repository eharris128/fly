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
pub fn send(
    socket_path: &Path,
    token: &str,
    reason: Reason,
    title: Option<&str>,
    body: Option<&str>,
) -> std::io::Result<()> {
    let payload = serde_json::json!({
        "token": token,
        "reason": reason.as_str(),
        "title": title,
        "body": body,
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

    // Refine reason/body from the Claude hook payload on stdin.
    if from_claude {
        if let Some(payload) = read_stdin_payload() {
            let (refined, msg) = parse_claude_payload(&payload);
            if let Some(r) = refined {
                reason = Some(r);
            }
            if body.is_none() {
                body = msg;
            }
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

/// Map a Claude Code Notification/Stop payload to a refined reason + message.
pub fn parse_claude_payload(json: &str) -> (Option<Reason>, Option<String>) {
    let v: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return (None, None),
    };
    let message = v
        .get("message")
        .and_then(|m| m.as_str())
        .map(|s| s.to_string());

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
    (reason, message)
}
