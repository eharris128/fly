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
/// `hook_event` is also optional (U7, KTD-F) and threads the event name for
/// agent-run closure; older servers ignore it via `#[serde(default)]`.
/// `capture_only` (fix-attribution U2, KTD1) marks a session-capture message
/// that must never raise; an older server ignores the field (accepted
/// mixed-version risk — the plan's skew note).
#[allow(clippy::too_many_arguments)]
pub fn send(
    socket_path: &Path,
    token: &str,
    reason: Reason,
    title: Option<&str>,
    body: Option<&str>,
    session_id: Option<&str>,
    cwd: Option<&str>,
    hook_event: Option<&str>,
    capture_only: bool,
) -> std::io::Result<()> {
    let payload = serde_json::json!({
        "token": token,
        "reason": reason.as_str(),
        "title": title,
        "body": body,
        "session_id": session_id,
        "cwd": cwd,
        "hook_event": hook_event,
        "capture_only": capture_only,
    });
    let bytes = serde_json::to_vec(&payload)?;
    let mut stream = UnixStream::connect(socket_path)?;
    stream.write_all(&bytes)?;
    stream.shutdown(Shutdown::Write)?;
    Ok(())
}

/// CLI entry for `fly notify <reason> [--claude] [--capture] [--title T] [--body B]`,
/// plus the held-ask path `fly notify --claude --permission-request`
/// (hook-ask-channel U4).
pub fn run(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--permission-request") {
        return run_permission_request();
    }
    let mut reason: Option<Reason> = None;
    let mut from_claude = false;
    // fix-attribution U2 (KTD1): the SessionStart capture path — update the
    // pane's resume record, raise nothing. The event name is a second gate
    // server-side, so the flag and the payload reinforce each other.
    let mut capture_only = false;
    let mut title: Option<String> = None;
    let mut body: Option<String> = None;
    // Captured from the Claude payload and forwarded to the app for resume (U1).
    let mut session_id: Option<String> = None;
    let mut cwd: Option<String> = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--claude" => from_claude = true,
            "--capture" => capture_only = true,
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

    // Refine reason/body and capture session_id/cwd/hook_event from the Claude hook payload.
    let mut hook_event: Option<String> = None;
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
            hook_event = parsed.hook_event;
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
        hook_event.as_deref(),
        capture_only,
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
        // Any pane may send `fly notify alert` — a valid, pane-authored alert
        // (automations KTD-H); no "Claude" phrasing, it need not be an agent.
        Reason::Alert => "Alert",
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
/// U7 adds `hook_event` (the raw event name) for agent-run closure (KTD-F).
/// fly used to parse the payload for the reason and discard the rest.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ClaudePayload {
    pub reason: Option<Reason>,
    pub message: Option<String>,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub hook_event: Option<String>,
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
    let hook_event_name = str_field("hook_event_name");
    let reason = match hook_event_name.as_deref() {
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
        hook_event: hook_event_name,
    }
}

// ---- the held-ask path (hook-ask-channel U4, KTD1/KTD4) --------------------

/// Stdin cap for a `PermissionRequest` payload — far above the notify path's
/// 64 KiB because `tool_input` can carry a whole file (a `Write` permission);
/// the extraction below discards the bulk before anything crosses the socket.
const ASK_STDIN_CAP: u64 = 1024 * 1024;
/// Per-string transport cap (chars) for forwarded question text. A transport
/// bound only — the feed re-caps at serve time (`feed::io::clean`).
const ASK_STRING_CAP: usize = 2048;
/// Count caps mirroring the feed's serve ceilings. Exceeding a COUNT cap
/// drops the questions wholesale (a truncated option list would misalign the
/// on-screen digit mapping — the screen fallback then supplies rendered-digit
/// truth); exceeding a STRING cap merely truncates (text never moves digits).
const ASK_MAX_QUESTIONS: usize = 4;
const ASK_MAX_OPTIONS: usize = 8;
/// Belt on the serialized message (< the server's 64 KiB `MAX_MESSAGE`): an
/// over-belt payload is rebuilt without `questions` (KTD4 degrade).
const ASK_WIRE_BELT: usize = 56 * 1024;
/// How long to wait for the server's ack line before concluding the app is
/// old or absent (R8) and exiting so the dialog proceeds normally.
const ASK_ACK_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

/// `fly notify --claude --permission-request`: forward the ask over the
/// socket, then HOLD the connection for the ask's lifetime (KTD1). Claude
/// Code kills this process when the dialog resolves locally — the drop is the
/// signal — and the dialog renders while we run (live-verified), so blocking
/// here never delays the user. Exit is always 0: a permission hook must never
/// surface an error into the session.
fn run_permission_request() -> i32 {
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
    let payload = read_ask_stdin()
        .map(|raw| extract_ask(&raw))
        .unwrap_or_default();
    match hold_ask(Path::new(&socket), &token, &payload) {
        Ok(Some(decision_line)) => {
            // The exact hookSpecificOutput JSON Claude applies (R7).
            println!("{decision_line}");
            0
        }
        Ok(None) => 0, // released / resolved locally — dialog proceeds normally
        Err(e) => {
            eprintln!("fly notify: failed to reach fly socket {socket}: {e}");
            0
        }
    }
}

/// Read the hook payload from stdin (up to [`ASK_STDIN_CAP`]), unless stdin is
/// a terminal (manual run). Mirrors [`read_stdin_payload`] with the larger cap.
fn read_ask_stdin() -> Option<String> {
    // SAFETY: isatty on fd 0 is always safe.
    if unsafe { libc::isatty(0) } == 1 {
        return None;
    }
    let mut buf = String::new();
    std::io::stdin()
        .take(ASK_STDIN_CAP)
        .read_to_string(&mut buf)
        .ok()?;
    if buf.trim().is_empty() {
        None
    } else {
        Some(buf)
    }
}

/// Extract the bounded, typed [`AskPayload`] subset from a raw
/// `PermissionRequest` payload (KTD4). Unparseable JSON yields the default
/// (body-less) payload — the ask still registers "a dialog is up".
pub fn extract_ask(raw: &str) -> crate::hooks::protocol::AskPayload {
    use crate::hooks::protocol::AskPayload;
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return AskPayload::default();
    };
    let s = |key: &str| v.get(key).and_then(|x| x.as_str()).map(str::to_string);
    let tool = s("tool_name");
    let input = v.get("tool_input");
    let questions = match (tool.as_deref(), input) {
        (Some("AskUserQuestion"), Some(input)) => bound_questions(input),
        _ => None,
    };
    let request = match (tool.as_deref(), input) {
        (Some("Bash"), Some(i)) => i.get("command"),
        (Some("Edit") | Some("Write"), Some(i)) => i.get("file_path"),
        _ => None,
    }
    .and_then(|x| x.as_str())
    .map(|x| cap_chars(x, ASK_STRING_CAP));
    AskPayload {
        tool,
        permission_mode: s("permission_mode"),
        session_id: s("session_id"),
        cwd: s("cwd"),
        questions,
        request,
    }
}

fn cap_chars(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        s.to_string()
    } else {
        s.chars().take(cap).collect()
    }
}

/// Bound an AskUserQuestion `tool_input` for the wire (KTD4): string caps
/// truncate (text never moves the picker's digits); COUNT caps abstain — a
/// dropped question or option would misalign the wire's index-derived digits
/// with the rendered picker, so over-count batches ship no questions at all
/// and the feed's screen leg supplies rendered-digit truth instead.
fn bound_questions(input: &serde_json::Value) -> Option<serde_json::Value> {
    let questions = input.get("questions")?.as_array()?;
    if questions.is_empty() || questions.len() > ASK_MAX_QUESTIONS {
        return None;
    }
    let mut out_questions = Vec::new();
    for q in questions {
        let opts = q.get("options").and_then(|o| o.as_array());
        if opts.is_some_and(|o| o.len() > ASK_MAX_OPTIONS) {
            return None;
        }
        let mut out_q = serde_json::Map::new();
        if let Some(text) = q.get("question").and_then(|x| x.as_str()) {
            out_q.insert("question".into(), cap_chars(text, ASK_STRING_CAP).into());
        }
        if let Some(h) = q.get("header").and_then(|x| x.as_str()) {
            out_q.insert("header".into(), cap_chars(h, ASK_STRING_CAP).into());
        }
        if let Some(m) = q.get("multiSelect").and_then(|x| x.as_bool()) {
            out_q.insert("multiSelect".into(), m.into());
        }
        if let Some(opts) = opts {
            let out_opts: Vec<serde_json::Value> = opts
                .iter()
                .map(|o| {
                    let mut out_o = serde_json::Map::new();
                    if let Some(l) = o.get("label").and_then(|x| x.as_str()) {
                        out_o.insert("label".into(), cap_chars(l, ASK_STRING_CAP).into());
                    }
                    if let Some(d) = o.get("description").and_then(|x| x.as_str()) {
                        out_o.insert("description".into(), cap_chars(d, ASK_STRING_CAP).into());
                    }
                    serde_json::Value::Object(out_o)
                })
                .collect();
            out_q.insert("options".into(), out_opts.into());
        }
        out_questions.push(serde_json::Value::Object(out_q));
    }
    Some(serde_json::json!({ "questions": out_questions }))
}

/// Connect, send the newline-framed `ask/hold` message, await the ack within
/// [`ASK_ACK_DEADLINE`] (no ack → old/absent app, R8 → `Ok(None)`), then park
/// until the server writes a decision line (→ `Ok(Some(line))`) or closes
/// (release / fly shutdown → `Ok(None)`). Claude killing this process is the
/// third, invisible exit. Public for the U9 integration test.
pub fn hold_ask(
    socket_path: &Path,
    token: &str,
    payload: &crate::hooks::protocol::AskPayload,
) -> std::io::Result<Option<String>> {
    let mut msg = serde_json::to_value(payload)?;
    let obj = msg.as_object_mut().expect("AskPayload serializes as object");
    obj.insert("token".into(), token.into());
    obj.insert("op".into(), "ask/hold".into());
    let mut bytes = serde_json::to_vec(&msg)?;
    if bytes.len() > ASK_WIRE_BELT {
        // KTD4 belt: rebuild body-less rather than risk the server's bound.
        let mut lean = payload.clone();
        lean.questions = None;
        let mut msg = serde_json::to_value(&lean)?;
        let obj = msg.as_object_mut().expect("object");
        obj.insert("token".into(), token.into());
        obj.insert("op".into(), "ask/hold".into());
        bytes = serde_json::to_vec(&msg)?;
    }
    bytes.push(b'\n');

    let mut stream = UnixStream::connect(socket_path)?;
    stream.write_all(&bytes)?;
    // Write half stays OPEN (KTD1): the server distinguishes a live-but-quiet
    // peer (read timeout) from a dead one (EOF), which is the whole
    // resolution signal.

    // Ack phase: one line within the deadline, or conclude old/absent server.
    stream.set_read_timeout(Some(ASK_ACK_DEADLINE))?;
    let mut buf: Vec<u8> = Vec::new();
    if read_line(&mut stream, &mut buf).is_none() {
        return Ok(None); // no ack (R8 skew fast-fail) — exit, dialog proceeds
    }

    // Hold phase: block until a decision line or close. No timeout — Claude
    // kills us when the dialog resolves locally, and its own hook timeout is
    // the outer bound.
    stream.set_read_timeout(None)?;
    Ok(read_line(&mut stream, &mut buf))
}

/// Read one `\n`-terminated line from `stream` into/continuing `buf` (which
/// may already hold bytes past the previous line). `None` on EOF/timeout
/// before a full line arrives.
fn read_line(stream: &mut UnixStream, buf: &mut Vec<u8>) -> Option<String> {
    loop {
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            return Some(String::from_utf8_lossy(&line[..line.len() - 1]).into_owned());
        }
        let mut chunk = [0u8; 4096];
        match stream.read(&mut chunk) {
            Ok(0) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => return None,
        }
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
        assert_eq!(p.hook_event.as_deref(), Some("Stop"));
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

    // ---- hook-ask-channel U4: extraction ------------------------------------

    /// The live-captured 2.1.207 PermissionRequest shape for a Bash dialog.
    const PERMREQ_BASH: &str = r#"{
        "session_id":"sess-1","transcript_path":"/t.jsonl","cwd":"/p",
        "prompt_id":"pr-1","permission_mode":"default",
        "hook_event_name":"PermissionRequest","tool_name":"Bash",
        "tool_input":{"command":"touch /tmp/x","description":"Create marker"},
        "permission_suggestions":[{"type":"addDirectories","directories":["/tmp"]}]
    }"#;

    /// The live-captured shape for an AskUserQuestion dialog.
    const PERMREQ_ASK: &str = r#"{
        "session_id":"sess-1","cwd":"/p","permission_mode":"bypassPermissions",
        "hook_event_name":"PermissionRequest","tool_name":"AskUserQuestion",
        "tool_input":{"questions":[{"question":"Favorite color?","header":"Color",
            "multiSelect":false,
            "options":[{"label":"Red","description":"The color red"},
                       {"label":"Blue","description":"The color blue"}]}]}
    }"#;

    #[test]
    fn extract_ask_maps_a_bash_permission_payload() {
        let a = extract_ask(PERMREQ_BASH);
        assert_eq!(a.tool.as_deref(), Some("Bash"));
        assert_eq!(a.permission_mode.as_deref(), Some("default"));
        assert_eq!(a.session_id.as_deref(), Some("sess-1"));
        assert_eq!(a.cwd.as_deref(), Some("/p"));
        assert_eq!(a.request.as_deref(), Some("touch /tmp/x"));
        assert_eq!(a.questions, None, "a permission ask carries no questions");
    }

    #[test]
    fn extract_ask_maps_an_ask_user_question_payload() {
        let a = extract_ask(PERMREQ_ASK);
        assert_eq!(a.tool.as_deref(), Some("AskUserQuestion"));
        assert_eq!(a.permission_mode.as_deref(), Some("bypassPermissions"));
        assert_eq!(a.request, None);
        let q = a.questions.expect("questions forwarded");
        assert_eq!(q["questions"][0]["question"], "Favorite color?");
        assert_eq!(q["questions"][0]["options"][1]["label"], "Blue");
    }

    #[test]
    fn extract_ask_degrades_to_a_body_less_payload() {
        // KTD4: unparseable stdin still registers "a dialog is up".
        let a = extract_ask("{ not json");
        assert_eq!(a, crate::hooks::protocol::AskPayload::default());
        // Unknown tools forward the name but no request summary (KTD1 posture:
        // never guess at arbitrary input shapes).
        let a = extract_ask(
            r#"{"tool_name":"WebFetch","tool_input":{"url":"http://x"},"session_id":"s"}"#,
        );
        assert_eq!(a.tool.as_deref(), Some("WebFetch"));
        assert_eq!(a.request, None);
    }

    #[test]
    fn bound_questions_truncates_strings_but_abstains_on_counts() {
        // String over-cap truncates (text never moves digits)…
        let long = "x".repeat(ASK_STRING_CAP + 100);
        let input = serde_json::json!({"questions":[
            {"question": long, "options":[{"label":"A"}]}
        ]});
        let bounded = bound_questions(&input).expect("served");
        let text = bounded["questions"][0]["question"].as_str().unwrap();
        assert_eq!(text.chars().count(), ASK_STRING_CAP);
        // …but an over-count option list drops the questions wholesale — a
        // truncated list would misalign the picker's digit mapping.
        let opts: Vec<serde_json::Value> = (0..ASK_MAX_OPTIONS + 1)
            .map(|i| serde_json::json!({"label": format!("o{i}")}))
            .collect();
        let input = serde_json::json!({"questions":[{"question":"q","options":opts}]});
        assert_eq!(bound_questions(&input), None);
        // Over-count question batches likewise.
        let qs: Vec<serde_json::Value> = (0..ASK_MAX_QUESTIONS + 1)
            .map(|i| serde_json::json!({"question": format!("q{i}")}))
            .collect();
        let input = serde_json::json!({ "questions": qs });
        assert_eq!(bound_questions(&input), None);
    }
}
