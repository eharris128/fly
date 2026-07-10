//! Hook-independent session-id capture from Claude's transcript store
//! (fix-resume-session-selection U1; KTD-A/C/E, R1/R2/R7).
//!
//! Claude Code writes every session's live conversation to
//! `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`: the **filename is the
//! session id**, and `<encoded-cwd>` is the project's absolute path with every
//! `/` and `.` replaced by `-` (verified against real dirs:
//! `/home/evan/projects/play` → `-home-evan-projects-play`; a `/.obsidian`
//! segment → `--obsidian`). Deriving the id from this store makes precise capture
//! independent of the installed `fly` binary's wire version — the version-skew
//! root cause this fix removes — and lets it fire before the first
//! `Notification`/`Stop` hook (KTD-A).
//!
//! The path-munging, active-pick, and last-turn helpers are pure (filesystem
//! entries injected as `(name, mtime)`; the file body parsed from a borrowed
//! string) so the encoding and the metadata-skipping turn scan — the parts most
//! likely to harbor a bug — are unit-tested without disk, mirroring
//! [`super::resume`]. fly only ever **reads** this store; it writes nothing under
//! `~/.claude` (R7: fly's own state stays under the `FLY_APP_NAME` root).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How recently a transcript must have been written to count as a pane's
/// *active* session during capture (KTD-A recency floor). Generous enough to span
/// a brief idle between turns, yet far short of a day, so an ancient unrelated
/// transcript in the same project dir is never mistaken for the live session.
/// Capture is change-tracked (U2), so a single in-window write pins the id for the
/// session's life; the restore-time stale-guard (U3) is the real freshness
/// authority. Tunable (plan Open Questions: capture cadence).
const ACTIVE_SESSION_MAX_AGE: Duration = Duration::from_secs(15 * 60);

/// `~/.claude/projects` — the root of Claude's per-project transcript store.
/// Honors `CLAUDE_CONFIG_DIR` (Claude's own config-dir override) when set, else
/// `$HOME`. `None` when neither resolves — the caller degrades to "no id".
/// Crate-visible so [`super::handoff`]'s command wrapper roots its path-taking
/// core here (session-handoff U1).
pub(crate) fn claude_projects_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Some(PathBuf::from(dir).join("projects"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude/projects"))
}

/// Encode an absolute cwd to Claude's project-dir name: replace every `/` **and**
/// `.` with `-` (KTD-E). A trailing slash is normalized away first, so `…/play/`
/// and `…/play` map to the same dir. Crate-visible so [`super::handoff`] derives
/// transcript paths under an injected root (session-handoff U1).
pub(crate) fn encode_cwd(cwd: &str) -> String {
    // Trim a trailing slash except on the bare root, so "/" still encodes to "-".
    let trimmed = if cwd.len() > 1 {
        cwd.trim_end_matches('/')
    } else {
        cwd
    };
    trimmed
        .chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect()
}

/// The Claude project dir for `cwd` (`<root>/<encoded-cwd>`), or `None` when no
/// home/config root is resolvable. The dir may legitimately not exist yet (a fresh
/// agent that has written no transcript) — the caller treats a missing dir as
/// "no id" (KTD-E graceful degrade).
pub fn claude_project_dir(cwd: &Path) -> Option<PathBuf> {
    let root = claude_projects_root()?;
    Some(root.join(encode_cwd(&cwd.to_string_lossy())))
}

/// The session id of a transcript file name — its basename sans `.jsonl` — or
/// `None` for a non-transcript or empty name. The filename *is* the session id
/// (KTD-A), so this is the one place that knowledge lives. Crate-visible for
/// [`super::handoff`]'s candidate lister (fix-attribution U5).
pub(crate) fn jsonl_id(name: &str) -> Option<&str> {
    let id = name.strip_suffix(".jsonl")?;
    (!id.is_empty()).then_some(id)
}

/// The session ids of every **actively-written** transcript among `entries`
/// (`(file-name, mtime)` pairs from the project dir): the basenames of the
/// `.jsonl` files whose mtime is within `max_age` of `now`, newest first
/// (KTD-A recency floor; fix-session-pane-attribution U4). The recency floor
/// keeps a fresh agent that has not written its own transcript yet from
/// adopting an ancient unrelated session in the same dir. A future mtime
/// (clock skew) counts as fresh, never stale. Pure over its arguments.
///
/// Returning the whole fresh set — not just the newest — is what lets the
/// caller tell "one live session" from "several sharing this cwd": post-hoc
/// attribution between same-cwd sessions is impossible, so the poll must
/// abstain rather than guess (R4, KTD3).
pub fn fresh_session_ids(
    entries: &[(String, SystemTime)],
    now: SystemTime,
    max_age: Duration,
) -> Vec<String> {
    let mut fresh: Vec<(&str, SystemTime)> = entries
        .iter()
        .filter_map(|(name, mtime)| jsonl_id(name).map(|id| (id, *mtime)))
        .filter(|(_, mtime)| {
            now.duration_since(*mtime)
                .map(|age| age <= max_age)
                .unwrap_or(true)
        })
        .collect();
    fresh.sort_by(|a, b| b.1.cmp(&a.1));
    fresh.into_iter().map(|(id, _)| id.to_string()).collect()
}

/// The Unix-ms timestamp of the last real conversational turn in a transcript —
/// the last `user`/`assistant` entry that carries a `timestamp` — ignoring the
/// trailing metadata-only entries (`mode`, `permission-mode`, `ai-title`, …) a
/// metadata-only `--continue` open appends without a new turn (KTD-C). This, not
/// the file mtime, is the freshness signal the stale-guard compares against: a
/// `--continue` open bumps the mtime to "now" while the last real turn stays days
/// old — exactly the reported bug. `None` for an empty/corrupt file or one with no
/// timestamped turn.
pub fn session_last_turn_ms(path: &Path) -> Option<u64> {
    let body = std::fs::read_to_string(path).ok()?;
    last_turn_ms_from_str(&body)
}

/// Pure core of [`session_last_turn_ms`]: the stamp half of the turn scan —
/// snippet extraction skipped, since the stamp-only callers (`continue_target`,
/// `qualifying_session_count`) run per-file on the restore path and would pay
/// O(turns) of discarded text extraction otherwise.
fn last_turn_ms_from_str(body: &str) -> Option<u64> {
    scan_turns(body, false).map(|s| s.last_turn_ms)
}

/// What the pick-list shows for one transcript (fix-session-pane-attribution
/// U5, R7): when the last real turn happened, and a short recognizable excerpt
/// of the most recent turn that carried extractable text (`None` when no turn
/// does — e.g. a tool-use-only tail). `None` overall means no real turn — the
/// transcript doesn't qualify as a candidate at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSummary {
    pub last_turn_ms: u64,
    pub snippet: Option<String>,
}

/// Read a transcript and summarize its last real turn for the pick-list (U5).
pub fn session_turn_summary(path: &Path) -> Option<TurnSummary> {
    let body = std::fs::read_to_string(path).ok()?;
    turn_summary_from_str(&body)
}

/// The snippet-bearing scan (see [`scan_turns`]) — the pick-list's entry point.
fn turn_summary_from_str(body: &str) -> Option<TurnSummary> {
    scan_turns(body, true)
}

/// The one turn scan: walk JSONL `body` keeping the last `user`/`assistant`
/// line bearing a parseable ISO-8601 `timestamp` (the real-turn stamp, KTD-C)
/// and — only when `want_snippet` — the last such line whose `message.content`
/// yields text (the snippet — a turn may be tool-use-only, so the snippet can
/// come from an earlier turn than the stamp). Tolerant of a non-JSON or
/// unterminated trailing line (skipped), matching the thin-reader contract
/// elsewhere. The content shapes are undocumented Claude contracts, so
/// extraction is defensive: a string body, or the first `{"type":"text"}`
/// block of an array (KTD6). Linear scan; read at picker/restore time, never
/// on a hot path.
fn scan_turns(body: &str, want_snippet: bool) -> Option<TurnSummary> {
    let mut last_ms: Option<u64> = None;
    let mut snippet: Option<String> = None;
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let ty = v.get("type").and_then(|t| t.as_str());
        if ty != Some("user") && ty != Some("assistant") {
            continue;
        }
        let Some(ms) = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(iso8601_to_ms)
        else {
            continue;
        };
        last_ms = Some(ms);
        if want_snippet {
            if let Some(text) = turn_text(&v) {
                snippet = Some(text);
            }
        }
    }
    last_ms.map(|last_turn_ms| TurnSummary {
        last_turn_ms,
        snippet,
    })
}

/// Extract displayable text from one turn's `message.content`: a plain string,
/// or the first `{"type":"text","text":…}` block of an array. Sanitized and
/// truncated for the picker row; `None` when the turn carries no usable text.
fn turn_text(v: &serde_json::Value) -> Option<String> {
    let content = v.get("message")?.get("content")?;
    let raw = match content {
        serde_json::Value::String(s) => s.as_str(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .find(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))?
            .get("text")?
            .as_str()?,
        _ => return None,
    };
    sanitize_snippet(raw)
}

/// Cap on a pick-list snippet, in characters (deferred plan question, resolved
/// here): long enough to recognize a conversation, short enough for a row.
const SNIPPET_MAX_CHARS: usize = 100;

/// Collapse whitespace runs (newlines included) to single spaces, drop every
/// control and format character via the shared [`crate::notify::is_stripped_char`]
/// policy — transcript text is agent-authored, so treat it like the alerts log
/// (R16 posture) — and truncate to [`SNIPPET_MAX_CHARS`] on a char boundary with
/// an ellipsis. `None` when nothing displayable is left.
fn sanitize_snippet(raw: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = 0usize;
    let mut in_ws = false;
    for c in raw.chars() {
        if c.is_whitespace() {
            in_ws = true;
            continue;
        }
        if crate::notify::is_stripped_char(c) {
            continue;
        }
        // A pending collapsed-whitespace separator costs a space *before* this
        // char, so count it toward the cap too — otherwise " x" at chars ==
        // SNIPPET_MAX_CHARS - 1 pushes the space then the char before the next
        // iteration's check trips, overshooting the cap by one.
        let needed = if in_ws && !out.is_empty() { 2 } else { 1 };
        if chars + needed > SNIPPET_MAX_CHARS {
            out.push('…');
            break;
        }
        if in_ws && !out.is_empty() {
            out.push(' ');
            chars += 1;
        }
        in_ws = false;
        out.push(c);
        chars += 1;
    }
    (!out.is_empty()).then_some(out)
}

/// Parse an ISO-8601 UTC timestamp (`2026-06-19T19:17:04.506Z`, the form Claude
/// writes on each transcript turn) to milliseconds since the Unix epoch. The
/// fractional part is optional; the trailing `Z` (UTC) is required — Claude always
/// writes UTC, so no timezone-offset handling is needed. Hand-rolled to avoid a
/// date-crate dependency. `None` for any shape we don't recognize.
fn iso8601_to_ms(s: &str) -> Option<u64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;

    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    if d.next().is_some() {
        return None;
    }

    let (hms, frac_ms) = match time.split_once('.') {
        Some((hms, frac)) => {
            let digits: String = frac.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() {
                return None;
            }
            // Take up to 3 fractional digits as ms, right-padding to 3.
            let ms3: String = digits.chars().chain(std::iter::repeat('0')).take(3).collect();
            (hms, ms3.parse::<i64>().ok()?)
        }
        None => (time, 0),
    };
    let mut t = hms.split(':');
    let hour: i64 = t.next()?.parse().ok()?;
    let min: i64 = t.next()?.parse().ok()?;
    let sec: i64 = t.next()?.parse().ok()?;
    if t.next().is_some() {
        return None;
    }

    // Defensive range checks (not a full calendar validator). The year bound is
    // load-bearing, not cosmetic: without it a corrupt/garbage year (a partial
    // transcript write, a foreign file, a future format change) overflows the i64
    // ms arithmetic below — and release builds compile with `overflow-checks =
    // false`, so the multiply *wraps silently* into a huge positive value that the
    // stale-guard then reads as "fresh", resurrecting the very stale session this
    // fix exists to block (debug builds panic instead). 9999 keeps every product
    // far inside i64 (fix-003 review).
    if !(1..=9999).contains(&year) {
        return None;
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || min > 59 || sec > 60 {
        return None;
    }

    // Checked arithmetic as belt-and-suspenders with the year bound: any
    // unanticipated overflow degrades to `None` rather than wrapping.
    let days = days_from_civil(year, month, day);
    let ms = days
        .checked_mul(86_400)
        .and_then(|d| d.checked_add(hour * 3_600 + min * 60 + sec))
        .and_then(|s| s.checked_mul(1_000))
        .and_then(|s| s.checked_add(frac_ms))?;
    (ms >= 0).then_some(ms as u64)
}

/// Days since the Unix epoch (1970-01-01) for a civil (proleptic Gregorian) date —
/// Howard Hinnant's well-known constant-time algorithm, valid for any year.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Read a project dir's `(file-name, mtime)` entries for [`fresh_session_ids`]
/// and the U5 candidate lister ([`super::handoff`]). Best-effort: a missing dir
/// or an unreadable entry yields an empty/partial list, never an error (the
/// caller degrades to "no id"). An entry whose mtime is unreadable is kept with
/// a `UNIX_EPOCH` stamp so it sorts oldest rather than being dropped.
pub(crate) fn read_project_entries(dir: &Path) -> Vec<(String, SystemTime)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        out.push((name.to_string(), mtime));
    }
    out
}

/// Resolve the active session id for a pane's `cwd` by reading Claude's store
/// (KTD-A): encode the cwd to its project dir, then pick the actively-written
/// transcript's basename within the recency window. `None` when the dir is
/// unresolvable/missing/empty, nothing was written recently, **or more than
/// one fresh transcript shares the cwd** — an ambiguous cwd yields abstention,
/// not a newest-mtime guess (fix-session-pane-attribution U4, R4/KTD3): the
/// poll can't tell which same-cwd session is this pane's, and a wrong guess
/// silently redirects resume and handoff. The frontend's change-tracked
/// capture treats `None` as a no-op, so abstention never clears a stored id;
/// disambiguation falls to the SessionStart hook or the pick-list. The single
/// I/O entry point the `pane_session_id` command (U1) and the resume poll (U2)
/// share.
pub fn active_session_for_cwd(cwd: &Path) -> Option<String> {
    let dir = claude_project_dir(cwd)?;
    let entries = read_project_entries(&dir);
    let mut fresh = fresh_session_ids(&entries, SystemTime::now(), ACTIVE_SESSION_MAX_AGE);
    if fresh.len() == 1 {
        fresh.pop()
    } else {
        None
    }
}

/// The basename (sans `.jsonl`) of the most-recently-modified transcript among
/// `entries`, with **no** recency floor — the session `claude --continue` would
/// re-open in this project dir (it always opens the newest transcript, however old
/// its last real turn). Pure; the freshness judgment is the restore-time
/// stale-guard's job (U3, KTD-C), not this pick's.
fn newest_session_basename(entries: &[(String, SystemTime)]) -> Option<String> {
    entries
        .iter()
        .filter_map(|(name, mtime)| jsonl_id(name).map(|id| (id, *mtime)))
        .max_by_key(|(_, mtime)| *mtime)
        .map(|(id, _)| id.to_string())
}

/// The freshness signal for the session `claude --continue` would re-open in a
/// project dir (the newest transcript): its **last real-turn** timestamp, which
/// the restore-time stale-guard compares against the pane's own activity (U3,
/// KTD-C). `last_turn_ms` is `None` for a transcript with no timestamped turn; the
/// guard treats that as stale, so a contentless candidate never resurrects a pane.
/// The session id itself is intentionally **not** returned — the imprecise path
/// resumes via `--continue`, not by id, so the frontend needs only the freshness.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContinueTarget {
    pub last_turn_ms: Option<u64>,
}

/// Path-taking core of [`continue_target`]: the last real-turn time of the newest
/// transcript in `dir`. `None` for a missing/empty dir (no `--continue` target →
/// the leaf falls to a bare shell).
fn continue_target_in_dir(dir: &Path) -> Option<ContinueTarget> {
    let session_id = newest_session_basename(&read_project_entries(dir))?;
    let last_turn_ms = session_last_turn_ms(&dir.join(format!("{session_id}.jsonl")));
    Some(ContinueTarget { last_turn_ms })
}

/// Command: the session `claude --continue` would re-open in `cwd`, plus its last
/// real-turn time, so the frontend's stale-guard can decide keep-vs-bare-shell in
/// one round-trip (U3, KTD-C). `None` when the project dir is unresolvable/empty.
#[tauri::command]
pub fn continue_target(cwd: String) -> Option<ContinueTarget> {
    let dir = claude_project_dir(Path::new(&cwd))?;
    continue_target_in_dir(&dir)
}

/// Path-taking core of [`qualifying_session_count`]: how many transcripts in
/// `dir` hold at least one real turn — the same qualification the handoff
/// candidates use (R5), keyed on content, **not** freshness: crash-resume runs
/// at startup when nothing is live, so a freshness signal would be
/// structurally zero and never fire (fix-session-pane-attribution U9, R13).
fn qualifying_count_in_dir(dir: &Path) -> u32 {
    let mut n = 0u32;
    for (name, _) in read_project_entries(dir) {
        if jsonl_id(&name).is_none() {
            continue;
        }
        if session_last_turn_ms(&dir.join(&name)).is_some() {
            n = n.saturating_add(1);
        }
    }
    n
}

/// Command: how many real-turn-qualified transcripts `cwd`'s project dir holds
/// (fix-attribution U9). The resume offer marks a `Poll`/unset-source leaf in a
/// cwd counting >1 as higher-risk — its `--resume`/`--continue` could re-attach
/// a sibling's session (R13/AE5). 0 for an unresolvable/missing dir.
#[tauri::command]
pub fn qualifying_session_count(cwd: String) -> u32 {
    match claude_project_dir(Path::new(&cwd)) {
        Some(dir) => qualifying_count_in_dir(&dir),
        None => 0,
    }
}

// ---- agent-run output capture (automations-workspace-and-model U4b) --------

/// The full text of the **last assistant turn** in a transcript (automations-
/// workspace-and-model plan, U4b — R8): the concatenation of every
/// `{"type":"text"}` block of the last `type == "assistant"` JSONL entry that
/// carries text (or its plain-string content). `None` for a missing / empty /
/// corrupt transcript, or one whose assistant tail is tool-use only. Uncapped
/// and unsanitized — the caller scrubs secrets, strips control chars, and
/// tail-caps before persisting. Linear scan; read once per agent-run close.
pub fn last_assistant_text(path: &Path) -> Option<String> {
    let body = std::fs::read_to_string(path).ok()?;
    last_assistant_text_from_str(&body)
}

/// Pure core of [`last_assistant_text`] — the text half of
/// [`last_assistant_reply_from_str`], kept as the automations capture's entry
/// point (its callers never need the stamp).
fn last_assistant_text_from_str(body: &str) -> Option<String> {
    last_assistant_reply_from_str(body).map(|r| r.text)
}

/// An agent's latest textual reply (feed-agent-reply-io U2): the last assistant
/// turn's text plus, when that turn carries a parseable ISO-8601 `timestamp`,
/// its epoch-ms stamp. The stamp is the feed's `lastReplyAt`/`repliedAt` value,
/// so text and stamp MUST come from the same turn — which this pairing
/// guarantees by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastReply {
    pub text: String,
    pub replied_at_ms: Option<u64>,
}

/// Read a transcript and return its last assistant reply (text + stamp), or
/// `None` for a missing/empty/corrupt transcript or a text-free assistant tail
/// (same contract as [`last_assistant_text`]).
pub fn last_assistant_reply(path: &Path) -> Option<LastReply> {
    let body = std::fs::read_to_string(path).ok()?;
    last_assistant_reply_from_str(&body)
}

/// The one last-assistant scan: walk JSONL `body` keeping the last
/// `type == "assistant"` entry that yields text ([`assistant_text_blocks`]),
/// paired with that same entry's timestamp. Tolerant of a non-JSON /
/// unterminated trailing line (skipped), matching the thin-reader contract
/// elsewhere.
fn last_assistant_reply_from_str(body: &str) -> Option<LastReply> {
    let mut last: Option<LastReply> = None;
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }
        if let Some(text) = assistant_text_blocks(&v) {
            last = Some(LastReply {
                text,
                replied_at_ms: v
                    .get("timestamp")
                    .and_then(|t| t.as_str())
                    .and_then(iso8601_to_ms),
            });
        }
    }
    last
}

/// Concatenate every `{"type":"text"}` block of an assistant turn's
/// `message.content` (joined by blank lines), or its plain-string content.
/// `None` when the turn carries no text (defensive: the content shapes are
/// undocumented Claude contracts, same posture as [`turn_text`]).
fn assistant_text_blocks(v: &serde_json::Value) -> Option<String> {
    let content = v.get("message")?.get("content")?;
    match content {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Array(blocks) => {
            let parts: Vec<&str> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .filter(|t| !t.is_empty())
                .collect();
            (!parts.is_empty()).then(|| parts.join("\n\n"))
        }
        _ => None,
    }
}

/// Resolve the transcript for an automation agent run by its `cwd`, guarded by
/// the run's dispatch time (automations-workspace-and-model U4b — the
/// confidentiality guard): return the **sole** `.jsonl` under the cwd's project
/// dir whose mtime is at or after `dispatched_ms`, or `None` when zero or more
/// than one qualify. This never reads another session's content into this run's
/// record (a cross-automation leak) — an ambiguous cwd abstains, exactly as the
/// attribution poll ([`active_session_for_cwd`]) does. A pane-precise SessionStart
/// id would be more accurate, but the automation manager has no backend
/// pane→session path, so cwd + dispatch-time disambiguation is the resolver.
pub fn sole_transcript_since(cwd: &Path, dispatched_ms: u64) -> Option<PathBuf> {
    let dir = claude_project_dir(cwd)?;
    sole_transcript_in_dir_since(&dir, dispatched_ms)
}

/// Path-taking core of [`sole_transcript_since`]: the sole transcript in `dir`
/// modified at/after `dispatched_ms`, else `None` (0 or >1 qualify → abstain).
fn sole_transcript_in_dir_since(dir: &Path, dispatched_ms: u64) -> Option<PathBuf> {
    let mut matches: Vec<PathBuf> = Vec::new();
    for (name, mtime) in read_project_entries(dir) {
        if jsonl_id(&name).is_none() {
            continue;
        }
        let mtime_ms = mtime
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        if mtime_ms >= dispatched_ms {
            matches.push(dir.join(&name));
        }
    }
    if matches.len() == 1 {
        matches.pop()
    } else {
        None
    }
}

/// Retry budget for [`capture_final_assistant_since`]. Claude Code fires the
/// `Stop` hook that triggers automation output capture ~100ms **before** it
/// flushes the run's final assistant turn to the transcript `.jsonl`
/// (empirically observed on Claude Code 2.1.200: the Stop-close reads the file
/// at T, the transcript's final write lands at ~T+100ms). A single-shot read on
/// Stop therefore sees a transcript that still lacks the final turn and records
/// an empty run output. Re-reading for up to ~2s (20 × 100ms) closes the race
/// while staying well under the agent-run close/deadline paths; both close call
/// sites run capture off their thread, so these waits never stall dispatch.
pub const CAPTURE_ATTEMPTS: u32 = 20;
pub const CAPTURE_RETRY_DELAY: Duration = Duration::from_millis(100);

/// U4b capture with Stop/flush-race tolerance. Resolves the sole fresh
/// transcript for `cwd` since `dispatched_ms` ([`sole_transcript_since`]) and
/// returns its last assistant turn's text, re-reading up to `attempts` times
/// spaced by `delay` because the final turn flushes shortly **after** the `Stop`
/// hook that triggers capture (see [`CAPTURE_ATTEMPTS`]). Abstains (`None`) if no
/// assistant text appears within the budget, if the cwd is ambiguous (>1 fresh
/// transcript), or if the transcript never materializes — the same
/// no-cross-session-leak posture as the single-shot [`sole_transcript_since`].
///
/// The caller sleeps on its own (spawned) thread, never the hook-dispatch or PTY
/// read thread — Claude Code blocks on the Stop hook, so a slow/abstaining
/// capture must not ride that path.
pub fn capture_final_assistant_since(
    cwd: &Path,
    dispatched_ms: u64,
    attempts: u32,
    delay: Duration,
) -> Option<String> {
    let dir = claude_project_dir(cwd)?;
    capture_final_assistant_in_dir_since(&dir, dispatched_ms, attempts, delay)
}

/// Dir-taking core of [`capture_final_assistant_since`] — tested directly on a
/// temp project dir (no `$HOME`). Loops resolve+read so a transcript that gains
/// its final assistant turn *between* reads is still captured, bounded by
/// `attempts` (never an unbounded wait).
fn capture_final_assistant_in_dir_since(
    dir: &Path,
    dispatched_ms: u64,
    attempts: u32,
    delay: Duration,
) -> Option<String> {
    for attempt in 0..attempts {
        if let Some(path) = sole_transcript_in_dir_since(dir, dispatched_ms) {
            if let Some(text) = last_assistant_text(&path) {
                return Some(text);
            }
        }
        // No sleep after the final attempt (nothing would re-read the result).
        if attempt + 1 < attempts {
            std::thread::sleep(delay);
        }
    }
    None
}

// ---- pending interaction scan (feed-pending-question U1) -------------------

/// One selectable option of a pending AskUserQuestion question. Parsed
/// defensively from the undocumented `input.questions[].options[]` shape
/// (KTD1): `label` is required, `description` defaults to empty, and a
/// malformed option entry is skipped rather than failing the question.
#[derive(Debug, Clone, PartialEq)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

/// One question of a pending AskUserQuestion batch (verified live shape:
/// `question`/`header`/`multiSelect`/`options[{label,description}]`).
#[derive(Debug, Clone, PartialEq)]
pub struct QuestionSpec {
    pub question: String,
    pub header: String,
    pub multi_select: bool,
    pub options: Vec<QuestionOption>,
}

/// Which kind of interaction the transcript tail is blocked on (KTD3):
/// `Choice` is an AskUserQuestion picker — its pending `tool_use` *always*
/// means waiting, classified from the transcript alone; `Permission` is any
/// other tool's unresolved `tool_use`, which only means "waiting on a
/// permission dialog" when the caller corroborates it against the pane's live
/// attention reason (the transcript can't distinguish waiting from executing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingKind {
    Choice,
    Permission,
}

/// The transcript's pending interaction (feed-pending-question U1, R1/R2):
/// what the agent stopped to ask, parsed from the unresolved `tool_use` tail.
/// All strings are **raw and untrusted** — agent-authored, uncapped,
/// unsanitized; the caller scrubs secrets, control-sanitizes, and truncates
/// before exposing anything (R8, same posture as [`last_assistant_text`]).
#[derive(Debug, Clone, PartialEq)]
pub struct PendingInteraction {
    pub kind: PendingKind,
    /// Epoch ms of the pending `tool_use` entry — when the question was asked.
    /// Always present: a stampless pending entry abstains instead (the stamp
    /// keys the wire marker and the answer guard, so exposure without it would
    /// be unguardable).
    pub asked_at_ms: u64,
    /// The tool name: `AskUserQuestion` for `Choice`, the pending tool for
    /// `Permission`.
    pub tool: String,
    /// R7: true only for the one shape v1 can answer remotely — a `Choice`
    /// carrying exactly one single-select question with at least one option.
    pub answerable: bool,
    /// R2 context sentence: text from the same assistant entry as the ask, or
    /// from the immediately preceding assistant entry with nothing but
    /// metadata / sidechain / thinking-only entries between. `None` otherwise —
    /// never an older reply masquerading as context.
    pub context: Option<String>,
    /// The parsed question batch (`Choice` only; empty for `Permission`).
    pub questions: Vec<QuestionSpec>,
    /// The pending tool's raw `input` object (`Permission` only) — the caller
    /// builds a scrubbed summary from well-known fields (U3).
    pub input: Option<serde_json::Value>,
}

/// The per-agent IO facts from one transcript read (U1 → U3 seam): the last
/// *completed* assistant reply, the *pending* interaction, and the trailing
/// conversation window (feed-conversation-tail U1), so the resolver caches all
/// three from a single parse instead of reading the file multiple times.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptIo {
    pub reply: Option<LastReply>,
    pub pending: Option<PendingInteraction>,
    /// The last [`RAW_TURN_BUFFER`] conversational turns, oldest → newest,
    /// raw ([`RawTurn`]) — the caller shapes/cleans them for the wire.
    pub turns: Vec<RawTurn>,
}

/// Read a transcript once and resolve every IO fact. `None` for a missing /
/// unreadable file (the caller's "no data" state, never an error).
pub fn transcript_io(path: &Path) -> Option<TranscriptIo> {
    let body = std::fs::read_to_string(path).ok()?;
    Some(TranscriptIo {
        reply: last_assistant_reply_from_str(&body),
        pending: pending_interaction_from_str(&body),
        turns: conversation_turns_from_str(&body),
    })
}

// ---- conversation-tail scan (feed-conversation-tail U1) --------------------

/// One conversational turn of the transcript tail (feed-conversation-tail U1):
/// a prompt delivered TO the agent (`User`) or a textual reply FROM it
/// (`Agent`). `text` is **raw and untrusted** — user-/agent-authored, uncapped,
/// unsanitized; the caller scrubs, sanitizes, and truncates before exposing it
/// (R7, same posture as [`PendingInteraction`]).
#[derive(Debug, Clone, PartialEq)]
pub struct RawTurn {
    pub role: TurnRole,
    /// Epoch ms of the turn's transcript entry, when it carried a parseable
    /// stamp. The wire requires a numeric `at` per turn (R2), so a stampless
    /// turn is dropped at shaping time — never served unstamped.
    pub at_ms: Option<u64>,
    pub text: String,
}

/// Who spoke a [`RawTurn`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnRole {
    User,
    Agent,
}

/// How many trailing raw turns the tail scan retains — 2× the wire serving
/// depth (`feed::io::MAX_TURNS`, statically checked there — KTD3) so the
/// shaping pass can still fill its window after cutting post-reply prompts and
/// dropping stampless or blank-after-clean turns.
pub(crate) const RAW_TURN_BUFFER: usize = 24;

/// The conversation-tail scan: the last [`RAW_TURN_BUFFER`] conversational
/// turns of a transcript, oldest → newest.
///
/// An **agent** turn is the **last** text-bearing `type == "assistant"` entry
/// of each assistant run (a stretch uninterrupted by a user prompt): one
/// working stretch usually flushes several text entries — narration between
/// tool calls — and those are intermediate output, not conversation (R6), so
/// a run collapses to its final text, exactly the entry the pane surfaced as
/// that stretch's reply. The per-entry text predicate is
/// [`last_assistant_reply_from_str`]'s ([`assistant_text_blocks`], no other
/// filtering), and collapse keeps the newest, so the scan's newest agent turn
/// IS the entry `reply`/`repliedAt` are served from (KTD1) and the wire's
/// ends-with-the-reply correlation (R3) holds by construction.
///
/// A **user** turn is a `type == "user"` entry bearing real text (string body
/// or `text` blocks, [`user_text_blocks`]) — which structurally excludes tool
/// chatter (R6): `tool_result`-only entries (tool returns, remote
/// `mode:"keys"` digit answers) carry no text and are transparent to the run
/// collapse, and a permission approval writes no user entry at all.
/// Sidechain (subagent), meta (caveats, injected notes), and compact-summary
/// user entries are skipped (also transparently to the collapse — they are
/// not conversation, so they don't delimit a run).
fn conversation_turns_from_str(body: &str) -> Vec<RawTurn> {
    let mut turns: std::collections::VecDeque<RawTurn> = std::collections::VecDeque::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue; // corrupt / unterminated line: skipped, thin-reader contract
        };
        let ts = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(iso8601_to_ms);
        let turn = match v.get("type").and_then(|t| t.as_str()) {
            Some("assistant") => assistant_text_blocks(&v).map(|text| RawTurn {
                role: TurnRole::Agent,
                at_ms: ts,
                text,
            }),
            Some("user")
                if v.get("isSidechain").and_then(|s| s.as_bool()) != Some(true)
                    && v.get("isMeta").and_then(|m| m.as_bool()) != Some(true)
                    && v.get("isCompactSummary").and_then(|c| c.as_bool()) != Some(true) =>
            {
                user_text_blocks(&v).map(|text| RawTurn {
                    role: TurnRole::User,
                    at_ms: ts,
                    text,
                })
            }
            _ => None,
        };
        if let Some(turn) = turn {
            // Collapse an assistant run to its last text entry (R6): a new
            // agent turn replaces a trailing agent turn instead of appending.
            if turn.role == TurnRole::Agent
                && turns.back().map(|t| t.role) == Some(TurnRole::Agent)
            {
                *turns.back_mut().expect("non-empty: back() was Some") = turn;
                continue;
            }
            if turns.len() == RAW_TURN_BUFFER {
                turns.pop_front();
            }
            turns.push_back(turn);
        }
    }
    turns.into()
}

/// A user entry's prompt text: its plain-string body, or every non-blank
/// `{"type":"text"}` block joined by blank lines (mirroring
/// [`assistant_text_blocks`]). `None` when the entry carries no real text —
/// e.g. `tool_result`-only — matching [`user_blocks`]'s boundary test, or
/// when the text is harness bookkeeping ([`is_harness_bookkeeping`]).
fn user_text_blocks(v: &serde_json::Value) -> Option<String> {
    let content = v.get("message")?.get("content")?;
    let text = match content {
        serde_json::Value::String(s) if !s.trim().is_empty() => s.clone(),
        serde_json::Value::Array(blocks) => {
            let parts: Vec<&str> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .filter(|t| !t.trim().is_empty())
                .collect();
            if parts.is_empty() {
                return None;
            }
            parts.join("\n\n")
        }
        _ => return None,
    };
    (!is_harness_bookkeeping(&text)).then_some(text)
}

/// Whether a user entry's text is Claude Code harness bookkeeping rather than
/// a prompt delivered to the agent (R6, verified on real transcripts — these
/// carry **no** `isMeta` flag, so only the content shape identifies them): a
/// slash-command invocation record (`<command-name>…` / `<command-message>…`)
/// is a control signal like a `mode:"keys"` digit, and local-command output
/// (`<local-command-stdout>`/`stderr`) is harness output *to the user*, not a
/// prompt at all. Prefix-matched on the trimmed text — the harness writes the
/// tag first; a real prompt starting with these exact tags is vanishingly
/// unlikely, and exclusion is the abstain-shaped failure.
fn is_harness_bookkeeping(text: &str) -> bool {
    let t = text.trim_start();
    ["<command-name>", "<command-message>", "<local-command-stdout>", "<local-command-stderr>"]
        .iter()
        .any(|tag| t.starts_with(tag))
}

/// Tools that delegate to a subagent whose own dialog is what's actually on
/// screen (KTD3 rule 1): the main chain shows only the delegating `tool_use`
/// (the inner conversation lives in a separate sidechain/session file), so the
/// named tool and summary would not match what a keystroke approves — a
/// confused-deputy hole, not a benign miss. Abstain. `Task` is the historical
/// name; `Agent` is the name current Claude Code writes (verified live,
/// 2026-07-06 — no `Task` occurrences remain in this machine's transcripts).
fn is_delegating_tool(name: &str) -> bool {
    matches!(name, "Task" | "Agent")
}

/// One real conversational entry of the tail window, newest-first, as the
/// backward walk sees it (KTD2). Metadata, sidechain, and thinking-only
/// entries never become items (transparent to contiguity *and* to context
/// adjacency); `tool_result`-only user entries become [`WalkItem::Results`]
/// (transparent to the pending walk, but they *break* R2 context adjacency —
/// a tool ran between the candidate context text and the ask).
enum WalkItem {
    Assistant {
        /// This entry's `tool_use` blocks: `(id, name, input)`.
        tools: Vec<(String, String, serde_json::Value)>,
        /// This entry's own text (context candidate), if any.
        text: Option<String>,
        /// This entry's parsed timestamp.
        ts: Option<u64>,
    },
    Results,
}

/// The KTD2 backward walk: report the transcript's pending interaction, or
/// `None` when nothing is pending / the shape is ambiguous (abstain-on-surprise,
/// KTD1). Walks from the tail so the parallel-batch shape — every sibling's
/// `tool_result` appended *after* all the `tool_use` lines — resolves
/// correctly, and stops at the first text-bearing `user` entry (real
/// conversation resuming, which clears pending). Verified live shapes this
/// rests on (2026-07-06): the ask's `tool_use` flushes at ask time with its
/// own timestamp; an Esc/reject writes an `is_error` `tool_result` for the
/// same id (consumed like any other), often followed by a text user entry
/// (the boundary).
pub(crate) fn pending_interaction_from_str(body: &str) -> Option<PendingInteraction> {
    use std::collections::HashSet;

    let mut consumed: HashSet<String> = HashSet::new();
    let mut items: Vec<WalkItem> = Vec::new();

    for line in body.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue; // corrupt / unterminated line: transparent
        };
        if v.get("isSidechain").and_then(|s| s.as_bool()) == Some(true) {
            continue; // subagent activity: transparent (KTD2)
        }
        match v.get("type").and_then(|t| t.as_str()) {
            Some("user") => {
                let (result_ids, has_text) = user_blocks(&v);
                let had_results = !result_ids.is_empty();
                for id in result_ids {
                    consumed.insert(id);
                }
                if has_text {
                    break; // the boundary: real conversation resumed
                }
                if had_results {
                    items.push(WalkItem::Results);
                }
                // else: attachment-only/empty user entry — transparent.
            }
            Some("assistant") => {
                let tools = tool_use_blocks(&v);
                let text = assistant_text_blocks(&v);
                if tools.is_empty() && text.is_none() {
                    continue; // thinking-only entry — transparent
                }
                let ts = v
                    .get("timestamp")
                    .and_then(|t| t.as_str())
                    .and_then(iso8601_to_ms);
                items.push(WalkItem::Assistant { tools, text, ts });
            }
            _ => {} // metadata (`mode`, `ai-title`, `system`, …) — transparent
        }
    }

    // Unconsumed tool_use candidates, newest-first, each with its item index
    // (for the R2 context adjacency look-behind).
    let mut unconsumed: Vec<(usize, &str, &str, &serde_json::Value, Option<u64>)> = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        if let WalkItem::Assistant { tools, ts, .. } = item {
            for (id, name, input) in tools {
                if !consumed.contains(id.as_str()) {
                    unconsumed.push((idx, id, name, input, *ts));
                }
            }
        }
    }
    if unconsumed.is_empty() {
        return None;
    }

    // An AskUserQuestion among the unconsumed ids wins outright (KTD2): it
    // never "executes", so pending means waiting even beside an unresolved
    // sibling. Newest one if several.
    if let Some(&(idx, _, _, input, ts)) = unconsumed
        .iter()
        .find(|(_, _, name, _, _)| *name == "AskUserQuestion")
    {
        let asked_at_ms = ts?; // stampless → unguardable → abstain
        let (questions, dropped) = parse_questions(input)?;
        let answerable = !dropped
            && questions.len() == 1
            && !questions[0].multi_select
            && !questions[0].options.is_empty();
        return Some(PendingInteraction {
            kind: PendingKind::Choice,
            asked_at_ms,
            tool: "AskUserQuestion".to_string(),
            answerable,
            context: context_for(&items, idx),
            questions,
            input: None,
        });
    }

    // No ask: expose the sole unconsumed tool as a permission candidate.
    // More than one → the on-screen dialog serializes to the first, which
    // post-hoc parsing can't identify — abstain (the `sole_transcript_since`
    // posture). A delegating tool → abstain (KTD3).
    if unconsumed.len() != 1 {
        return None;
    }
    let (idx, _, name, input, ts) = unconsumed[0];
    if is_delegating_tool(name) {
        return None;
    }
    let asked_at_ms = ts?;
    Some(PendingInteraction {
        kind: PendingKind::Permission,
        asked_at_ms,
        tool: name.to_string(),
        answerable: false,
        context: context_for(&items, idx),
        questions: Vec::new(),
        input: Some(input.clone()),
    })
}

/// A user entry's blocks, split for the walk: the `tool_use_id`s of its
/// `tool_result` blocks, and whether it bears real text (a non-empty string
/// body, or a non-empty `{"type":"text"}` block) — the KTD2 boundary signal.
fn user_blocks(v: &serde_json::Value) -> (Vec<String>, bool) {
    let Some(content) = v.get("message").and_then(|m| m.get("content")) else {
        return (Vec::new(), false);
    };
    match content {
        serde_json::Value::String(s) => (Vec::new(), !s.trim().is_empty()),
        serde_json::Value::Array(blocks) => {
            let mut ids = Vec::new();
            let mut has_text = false;
            for b in blocks {
                match b.get("type").and_then(|t| t.as_str()) {
                    Some("tool_result") => {
                        if let Some(id) = b.get("tool_use_id").and_then(|i| i.as_str()) {
                            ids.push(id.to_string());
                        }
                    }
                    Some("text") => {
                        if b.get("text")
                            .and_then(|t| t.as_str())
                            .is_some_and(|t| !t.trim().is_empty())
                        {
                            has_text = true;
                        }
                    }
                    _ => {}
                }
            }
            (ids, has_text)
        }
        _ => (Vec::new(), false),
    }
}

/// An assistant entry's `tool_use` blocks as `(id, name, input)` — blocks
/// missing an id or name are skipped (KTD1 defensive posture).
fn tool_use_blocks(v: &serde_json::Value) -> Vec<(String, String, serde_json::Value)> {
    let Some(serde_json::Value::Array(blocks)) = v.get("message").and_then(|m| m.get("content"))
    else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
        .filter_map(|b| {
            let id = b.get("id").and_then(|i| i.as_str())?;
            let name = b.get("name").and_then(|n| n.as_str())?;
            let input = b.get("input").cloned().unwrap_or(serde_json::Value::Null);
            Some((id.to_string(), name.to_string(), input))
        })
        .collect()
}

/// The R2 context sentence for the pending entry at `items[idx]` (items are
/// newest-first): text in the same assistant entry, else the immediately
/// preceding entry's text — but only when that entry is an assistant one
/// (`items[idx + 1]`, since transparent entries never made it into `items`).
/// A `tool_result`-only user entry in between ([`WalkItem::Results`]) breaks
/// adjacency: the candidate text narrated the *tool*, not the ask.
fn context_for(items: &[WalkItem], idx: usize) -> Option<String> {
    if let WalkItem::Assistant {
        text: Some(text), ..
    } = &items[idx]
    {
        return Some(text.clone());
    }
    match items.get(idx + 1) {
        Some(WalkItem::Assistant {
            text: Some(text), ..
        }) => Some(text.clone()),
        _ => None,
    }
}

/// Parse an AskUserQuestion `input.questions[]` batch defensively (KTD1): a
/// question needs a non-empty `question` string (header defaults empty,
/// `multiSelect` false — cosmetic omissions shouldn't suppress exposure); an
/// option needs a `label`. Unparseable questions/options are skipped; zero
/// parseable questions → `None` (abstain rather than expose an empty shell).
///
/// Returns `(specs, dropped)` where `dropped` is true if **any** question or
/// option present in the raw arrays failed to parse. A parse-time drop shifts
/// the wire's question/option indices out of alignment with the on-screen
/// picker (which renders every raw entry), so a digit answer would select the
/// wrong option — `pending_interaction_from_str` folds `dropped` into
/// `answerable` so such an ask is never marked remotely answerable (the
/// display-layer `question_body` catches blank-after-sanitize drops; this
/// catches the earlier parse-layer drops it can't see).
fn parse_questions(input: &serde_json::Value) -> Option<(Vec<QuestionSpec>, bool)> {
    let questions = input.get("questions")?.as_array()?;
    let mut dropped = false;
    let mut parsed: Vec<QuestionSpec> = Vec::new();
    for q in questions {
        let Some(question) = q.get("question").and_then(|s| s.as_str()) else {
            dropped = true;
            continue;
        };
        if question.trim().is_empty() {
            dropped = true;
            continue;
        }
        let header = q
            .get("header")
            .and_then(|h| h.as_str())
            .unwrap_or_default()
            .to_string();
        let multi_select = q
            .get("multiSelect")
            .and_then(|m| m.as_bool())
            .unwrap_or(false);
        let mut options: Vec<QuestionOption> = Vec::new();
        if let Some(opts) = q.get("options").and_then(|o| o.as_array()) {
            for o in opts {
                let Some(label) = o.get("label").and_then(|l| l.as_str()) else {
                    dropped = true;
                    continue;
                };
                options.push(QuestionOption {
                    label: label.to_string(),
                    description: o
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or_default()
                        .to_string(),
                });
            }
        }
        parsed.push(QuestionSpec {
            question: question.to_string(),
            header,
            multi_select,
            options,
        });
    }
    (!parsed.is_empty()).then_some((parsed, dropped))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- encode_cwd / claude_project_dir (KTD-E) ---------------------------

    #[test]
    fn encodes_slashes_to_dashes() {
        assert_eq!(encode_cwd("/home/evan/projects/play"), "-home-evan-projects-play");
    }

    #[test]
    fn encodes_a_dot_segment_to_a_double_dash() {
        // `/` → `-` AND `.` → `-`, so a `/.obsidian` segment yields `--obsidian`.
        assert_eq!(encode_cwd("/home/evan/.obsidian/notes"), "-home-evan--obsidian-notes");
    }

    #[test]
    fn normalizes_a_trailing_slash() {
        assert_eq!(encode_cwd("/home/evan/projects/play/"), "-home-evan-projects-play");
        // The bare root still encodes to a single dash.
        assert_eq!(encode_cwd("/"), "-");
    }

    #[test]
    fn project_dir_basename_is_the_encoded_cwd() {
        // Independent of where the home/config root resolves, the dir's final
        // component is the encoded cwd. (HOME is set in the test environment.)
        let dir = claude_project_dir(Path::new("/home/evan/projects/play"))
            .expect("a home/config root resolves in tests");
        assert_eq!(
            dir.file_name().and_then(|s| s.to_str()),
            Some("-home-evan-projects-play")
        );
    }

    // ---- fresh_session_ids (KTD-A recency floor; fix-attribution U4) --------

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// The single-fresh pick `active_session_for_cwd` applies over the pure
    /// helper (U4): exactly one fresh id captures; zero or several yield `None`.
    fn single_fresh(
        entries: &[(String, SystemTime)],
        now: SystemTime,
        max_age: Duration,
    ) -> Option<String> {
        let mut fresh = fresh_session_ids(entries, now, max_age);
        if fresh.len() == 1 {
            fresh.pop()
        } else {
            None
        }
    }

    #[test]
    fn exactly_one_fresh_transcript_captures_it() {
        // The unchanged single-session behavior: one in-window transcript is
        // the pane's active session, however many stale siblings sit beside it.
        let entries = vec![
            ("stale-session.jsonl".to_string(), at(100)),
            ("live-session.jsonl".to_string(), at(9_990)),
        ];
        assert_eq!(
            single_fresh(&entries, at(10_000), Duration::from_secs(60)),
            Some("live-session".to_string())
        );
    }

    #[test]
    fn two_fresh_transcripts_abstain() {
        // R4/KTD3: two same-cwd sessions both in-window are indistinguishable
        // post-hoc — the poll must capture nothing, not guess newest-mtime
        // (the guess IS the reported bug).
        let entries = vec![
            ("pane-a-session.jsonl".to_string(), at(9_980)),
            ("pane-b-session.jsonl".to_string(), at(9_990)),
        ];
        assert_eq!(
            fresh_session_ids(&entries, at(10_000), Duration::from_secs(60)).len(),
            2
        );
        assert_eq!(
            single_fresh(&entries, at(10_000), Duration::from_secs(60)),
            None
        );
    }

    #[test]
    fn fresh_ids_order_newest_first_ignoring_non_jsonl() {
        // A newer non-`.jsonl` file (e.g. a stray note) never appears.
        let entries = vec![
            ("notes.txt".to_string(), at(990)),
            ("older.jsonl".to_string(), at(900)),
            ("newer.jsonl".to_string(), at(950)),
        ];
        assert_eq!(
            fresh_session_ids(&entries, at(1_000), Duration::from_secs(3600)),
            vec!["newer".to_string(), "older".to_string()]
        );
    }

    #[test]
    fn empty_list_is_none() {
        assert!(fresh_session_ids(&[], at(100), Duration::from_secs(3600)).is_empty());
        assert_eq!(single_fresh(&[], at(100), Duration::from_secs(3600)), None);
    }

    #[test]
    fn an_all_old_set_is_none_under_the_recency_floor() {
        // The only transcript was written long before `now` → not the live
        // session → None (so the poll captures nothing).
        let entries = vec![("ancient.jsonl".to_string(), at(10))];
        assert_eq!(
            single_fresh(&entries, at(10_000), Duration::from_secs(100)),
            None
        );
    }

    #[test]
    fn a_future_mtime_counts_as_fresh() {
        // Clock skew: an mtime after `now` must not be treated as stale.
        let entries = vec![("future.jsonl".to_string(), at(5_000))];
        assert_eq!(
            single_fresh(&entries, at(1_000), Duration::from_secs(60)),
            Some("future".to_string())
        );
    }

    // ---- iso8601_to_ms -----------------------------------------------------

    #[test]
    fn parses_iso8601_to_epoch_ms() {
        assert_eq!(iso8601_to_ms("1970-01-01T00:00:00.000Z"), Some(0));
        // No fractional part.
        assert_eq!(iso8601_to_ms("2000-01-01T00:00:00Z"), Some(946_684_800_000));
        // The reported bug's last real turn (cross-checked against the std lib).
        assert_eq!(iso8601_to_ms("2026-06-19T19:17:16.402Z"), Some(1_781_896_636_402));
    }

    #[test]
    fn rejects_malformed_timestamps() {
        assert_eq!(iso8601_to_ms(""), None);
        assert_eq!(iso8601_to_ms("not-a-date"), None);
        assert_eq!(iso8601_to_ms("2026-06-19 19:17:16Z"), None); // space, no 'T'
        assert_eq!(iso8601_to_ms("2026-06-19T19:17:16"), None); // no trailing 'Z'
        assert_eq!(iso8601_to_ms("2026-13-01T00:00:00Z"), None); // month out of range
        assert_eq!(iso8601_to_ms("2026-06-19T19:17Z"), None); // missing seconds
    }

    #[test]
    fn rejects_out_of_range_year_without_overflowing() {
        // A garbage/corrupt year must degrade to None, never wrap to a huge
        // "fresh" timestamp that defeats the stale-guard (fix-003 review). In a
        // release build the unbounded multiply would wrap silently; the year bound
        // turns it into a clean rejection. (This test also asserts no panic in a
        // debug build, where the overflow would otherwise abort.)
        assert_eq!(iso8601_to_ms("999999-12-31T23:59:59.999Z"), None);
        assert_eq!(iso8601_to_ms("0000-01-01T00:00:00Z"), None); // year 0 rejected
    }

    #[test]
    fn rejects_pre_epoch_dates() {
        // A pre-1970 date yields negative ms; the `ms >= 0` guard rejects it so the
        // `as u64` cast never sees a negative value. Claude never writes these.
        assert_eq!(iso8601_to_ms("1969-12-31T23:59:59Z"), None);
    }

    // ---- session_last_turn_ms / last_turn_ms_from_str (KTD-C) --------------

    /// A transcript whose tail is metadata-only after a 06-19 turn — the exact
    /// shape a metadata-only `--continue` open produces (bumps file mtime, adds no
    /// real turn). The newline-joined JSONL mirrors the real store.
    const METADATA_TAIL_FIXTURE: &str = concat!(
        r#"{"type":"mode","sessionId":"s"}"#,
        "\n",
        r#"{"type":"user","timestamp":"2026-06-19T19:17:04.506Z","message":{"role":"user"}}"#,
        "\n",
        r#"{"type":"assistant","timestamp":"2026-06-19T19:17:16.402Z","message":{"role":"assistant"}}"#,
        "\n",
        r#"{"type":"ai-title"}"#,
        "\n",
        r#"{"type":"mode"}"#,
        "\n",
        r#"{"type":"permission-mode"}"#,
        "\n",
    );

    #[test]
    fn last_turn_ignores_trailing_metadata_entries() {
        // Returns the 06-19 assistant turn, NOT a later (metadata-only) write.
        assert_eq!(
            last_turn_ms_from_str(METADATA_TAIL_FIXTURE),
            Some(1_781_896_636_402)
        );
    }

    #[test]
    fn last_turn_is_none_for_empty_or_metadata_only() {
        assert_eq!(last_turn_ms_from_str(""), None);
        // Metadata lines carry no timestamped user/assistant turn.
        assert_eq!(
            last_turn_ms_from_str("{\"type\":\"mode\"}\n{\"type\":\"permission-mode\"}\n"),
            None
        );
        // A corrupt/non-JSON line is skipped, not a panic.
        assert_eq!(last_turn_ms_from_str("{ not json\ngarbage"), None);
    }

    #[test]
    fn session_last_turn_ms_reads_a_file_and_returns_the_last_real_turn() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("04d56f41.jsonl");
        std::fs::write(&path, METADATA_TAIL_FIXTURE).unwrap();
        // Bump mtime to "now" the way a --continue open would — the function must
        // ignore mtime and report the 06-19 turn from the body.
        assert_eq!(session_last_turn_ms(&path), Some(1_781_896_636_402));
        // A missing file degrades to None.
        assert_eq!(session_last_turn_ms(&dir.path().join("gone.jsonl")), None);
    }

    // ---- turn_summary_from_str / sanitize_snippet (fix-attribution U5) ------

    #[test]
    fn turn_summary_extracts_string_and_block_content() {
        // The two content shapes Claude writes: a plain string (typed user
        // prompt) and an array of blocks (assistant turns). The snippet is the
        // LAST text-bearing turn's text.
        let body = concat!(
            r#"{"type":"user","timestamp":"2026-06-19T19:17:04.506Z","message":{"role":"user","content":"fix the bug"}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-06-19T19:17:16.402Z","message":{"role":"assistant","content":[{"type":"text","text":"done, tests pass"}]}}"#,
            "\n",
        );
        let s = turn_summary_from_str(body).unwrap();
        assert_eq!(s.last_turn_ms, 1_781_896_636_402);
        assert_eq!(s.snippet.as_deref(), Some("done, tests pass"));
    }

    #[test]
    fn turn_summary_snippet_falls_back_past_a_tool_only_turn() {
        // A tool-use-only tail (no text block) must not blank the snippet —
        // the stamp comes from the last turn, the snippet from the last turn
        // WITH text.
        let body = concat!(
            r#"{"type":"user","timestamp":"2026-06-19T19:17:04.506Z","message":{"role":"user","content":"refactor the store"}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-06-19T19:17:16.402Z","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash"}]}}"#,
            "\n",
        );
        let s = turn_summary_from_str(body).unwrap();
        assert_eq!(s.last_turn_ms, 1_781_896_636_402, "stamp: the last turn");
        assert_eq!(
            s.snippet.as_deref(),
            Some("refactor the store"),
            "snippet: the last text-bearing turn"
        );
    }

    #[test]
    fn turn_summary_matches_last_turn_ms_and_none_for_metadata_only() {
        // The summary is the same scan session_last_turn_ms delegates to; a
        // metadata-only body summarizes to None (the disqualifier R5/R11 use).
        assert_eq!(
            turn_summary_from_str(METADATA_TAIL_FIXTURE).map(|s| s.last_turn_ms),
            last_turn_ms_from_str(METADATA_TAIL_FIXTURE)
        );
        assert_eq!(
            turn_summary_from_str("{\"type\":\"mode\"}\n{\"type\":\"ai-title\"}\n"),
            None
        );
        // A turnful body whose turns carry no content still summarizes (the
        // fixture's turns have no `content`) — snippet None, stamp present.
        let s = turn_summary_from_str(METADATA_TAIL_FIXTURE).unwrap();
        assert_eq!(s.snippet, None);
    }

    #[test]
    fn snippets_are_sanitized_and_truncated() {
        // Agent-authored text: control chars stripped, whitespace runs (and
        // newlines) collapsed, over-long text cut at a char boundary with an
        // ellipsis (R16 posture). The printable residue of an ANSI sequence
        // ("[2J") is harmless in a DOM-rendered row — only the control byte
        // itself is the threat.
        assert_eq!(
            sanitize_snippet("a\u{1b}[2J  b\n\nc"),
            Some("a[2J b c".to_string())
        );
        assert_eq!(sanitize_snippet("   \n\t "), None);
        let long: String = std::iter::repeat('é').take(150).collect();
        let cut = sanitize_snippet(&long).unwrap();
        assert_eq!(cut.chars().count(), SNIPPET_MAX_CHARS + 1, "100 chars + ellipsis");
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn snippets_strip_bidi_and_zero_width_format_chars() {
        // Format (Cf) chars pass char::is_control but can visually reorder or
        // hide a picker row — a transcript spoofing the human-disambiguation
        // step. The shared notify::is_stripped_char policy drops them (R16):
        // RLO (U+202E), zero-width space (U+200B), BOM/ZWNBSP (U+FEFF).
        assert_eq!(
            sanitize_snippet("a\u{202e}b\u{200b}c\u{feff}d"),
            Some("abcd".to_string())
        );
    }

    #[test]
    fn snippet_cap_accounts_for_collapsed_whitespace_separator() {
        // R16 off-by-one guard: the collapsed-whitespace separator counts toward
        // the cap, so truncation at a word boundary still yields at most
        // SNIPPET_MAX_CHARS content chars plus the ellipsis — never one over.
        // 98 chars, the separator (99th), one char (100th), then overflow → the
        // exact SNIPPET_MAX_CHARS + 1 contract with a real collapsed separator.
        let at_cap = format!("{} {}", "a".repeat(98), "b".repeat(5));
        let cut = sanitize_snippet(&at_cap).unwrap();
        assert_eq!(
            cut.chars().count(),
            SNIPPET_MAX_CHARS + 1,
            "100 content chars + ellipsis"
        );
        assert!(cut.ends_with('…'));
        assert_eq!(
            cut.chars().take_while(|&c| c != '…').count(),
            SNIPPET_MAX_CHARS,
            "content is capped at SNIPPET_MAX_CHARS"
        );

        // The pre-fix overshoot shape: 99 content chars, then " bb". The separator
        // used to push content to 101 before the ellipsis check tripped; now the
        // boundary check breaks first, so content never exceeds the cap.
        let overshoot = format!("{} {}", "a".repeat(99), "bb");
        let cut = sanitize_snippet(&overshoot).unwrap();
        assert!(cut.ends_with('…'));
        assert!(
            cut.chars().take_while(|&c| c != '…').count() <= SNIPPET_MAX_CHARS,
            "collapsed-whitespace boundary must not overshoot the cap"
        );
    }

    // ---- newest_session_basename / continue_target_in_dir (U3, KTD-C) ------

    #[test]
    fn newest_session_has_no_recency_floor() {
        // Unlike active_session_id, --continue's pick ignores age: an ancient
        // transcript is still the target if it is the newest present.
        let entries = vec![("ancient.jsonl".to_string(), at(10))];
        assert_eq!(newest_session_basename(&entries), Some("ancient".to_string()));
        // Newest wins; non-jsonl ignored; empty → None.
        let entries = vec![
            ("notes.txt".to_string(), at(999)),
            ("a.jsonl".to_string(), at(100)),
            ("b.jsonl".to_string(), at(200)),
        ];
        assert_eq!(newest_session_basename(&entries), Some("b".to_string()));
        assert_eq!(newest_session_basename(&[]), None);
    }

    #[test]
    fn continue_target_reports_the_newest_session_and_its_last_real_turn() {
        let dir = tempfile::tempdir().unwrap();
        // A single transcript whose tail is metadata-only after a 06-19 turn — the
        // exact play-bug shape: a recent mtime but an ancient last real turn.
        std::fs::write(dir.path().join("04d56f41.jsonl"), METADATA_TAIL_FIXTURE).unwrap();
        let target = continue_target_in_dir(dir.path()).expect("a target exists");
        // Reports the newest transcript's last REAL turn (06-19), not its mtime.
        // (Which session is picked is covered by newest_session_has_no_recency_floor.)
        assert_eq!(target.last_turn_ms, Some(1_781_896_636_402));
    }

    #[test]
    fn continue_target_is_none_for_an_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(continue_target_in_dir(dir.path()), None);
    }

    // ---- qualifying_count_in_dir (fix-attribution U9, R13) ------------------

    #[test]
    fn qualifying_count_ignores_metadata_only_and_non_transcripts() {
        // The count keys on real turns, not freshness or file count: two
        // turnful transcripts (one ancient by mtime — irrelevant), one
        // metadata-only, one stray file → 2.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.jsonl"), METADATA_TAIL_FIXTURE).unwrap();
        std::fs::write(dir.path().join("b.jsonl"), METADATA_TAIL_FIXTURE).unwrap();
        std::fs::write(
            dir.path().join("meta.jsonl"),
            "{\"type\":\"mode\"}\n{\"type\":\"ai-title\"}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
        assert_eq!(qualifying_count_in_dir(dir.path()), 2);
        // Empty dir → 0 (the benign single/none paths stay unflagged).
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(qualifying_count_in_dir(empty.path()), 0);
    }

    // ---- last_assistant_text (automations-workspace-and-model U4b) ----------

    // A transcript whose last assistant turn carries a two-block text answer,
    // preceded by an earlier assistant turn and a trailing tool-use-only turn.
    const AGENT_RUN_FIXTURE: &str = concat!(
        r#"{"type":"user","message":{"role":"user","content":"check disk"}}"#,
        "\n",
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Let me look."}]}}"#,
        "\n",
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Disk is 82% full."},{"type":"text","text":"Clean ~/.cache to recover 4 GB."}]}}"#,
        "\n",
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{}}]}}"#,
        "\n",
    );

    #[test]
    fn last_assistant_text_returns_last_texted_turn_concatenating_blocks() {
        // The tool-use-only tail does NOT clobber the prior texted turn, and the
        // two text blocks of that turn are concatenated (blank-line joined).
        assert_eq!(
            last_assistant_text_from_str(AGENT_RUN_FIXTURE).as_deref(),
            Some("Disk is 82% full.\n\nClean ~/.cache to recover 4 GB.")
        );
    }

    #[test]
    fn last_assistant_text_handles_string_content_and_degenerate_inputs() {
        // Plain-string content is returned as-is.
        let s = r#"{"type":"assistant","message":{"role":"assistant","content":"done"}}"#;
        assert_eq!(last_assistant_text_from_str(s).as_deref(), Some("done"));
        // Empty, metadata-only, and corrupt bodies → None (never a panic).
        assert_eq!(last_assistant_text_from_str(""), None);
        assert_eq!(last_assistant_text_from_str("{\"type\":\"user\"}\n"), None);
        assert_eq!(last_assistant_text_from_str("{ not json\ngarbage"), None);
        // A tool-use-only transcript (no assistant text at all) → None.
        let tools = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use"}]}}"#;
        assert_eq!(last_assistant_text_from_str(tools), None);
    }

    #[test]
    fn last_assistant_reply_pairs_text_with_that_turns_timestamp() {
        // The stamp must come from the SAME turn as the text (U2): a later
        // tool-use-only turn's newer timestamp must not restamp the reply.
        let body = concat!(
            r#"{"type":"assistant","timestamp":"2026-06-19T19:17:04.506Z","message":{"role":"assistant","content":[{"type":"text","text":"early"}]}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-06-19T19:17:16.402Z","message":{"role":"assistant","content":[{"type":"text","text":"the reply"}]}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-06-19T19:18:00.000Z","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash"}]}}"#,
            "\n",
        );
        assert_eq!(
            last_assistant_reply_from_str(body),
            Some(LastReply {
                text: "the reply".into(),
                replied_at_ms: Some(1_781_896_636_402),
            })
        );
    }

    #[test]
    fn last_assistant_reply_without_timestamp_still_carries_text() {
        // A stampless turn (or unparseable timestamp) degrades to a stampless
        // reply, never a dropped one — the feed then serves text without
        // repliedAt rather than pretending the agent never replied.
        let s = r#"{"type":"assistant","message":{"role":"assistant","content":"done"}}"#;
        assert_eq!(
            last_assistant_reply_from_str(s),
            Some(LastReply {
                text: "done".into(),
                replied_at_ms: None,
            })
        );
        // And the degenerate inputs match last_assistant_text's contract.
        assert_eq!(last_assistant_reply_from_str(""), None);
        assert_eq!(last_assistant_reply_from_str("{ not json"), None);
    }

    #[test]
    fn last_assistant_text_reads_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, AGENT_RUN_FIXTURE).unwrap();
        assert!(last_assistant_text(&path).unwrap().contains("82% full"));
        // A missing file degrades to None.
        assert_eq!(last_assistant_text(&dir.path().join("nope.jsonl")), None);
    }

    // ---- sole_transcript_since disambiguation (U4b confidentiality guard) ---

    #[test]
    fn sole_transcript_since_abstains_unless_exactly_one_qualifies() {
        let dir = tempfile::tempdir().unwrap();
        // No transcripts yet → None.
        assert_eq!(sole_transcript_in_dir_since(dir.path(), 0), None);

        // Exactly one .jsonl (mtime ~ now ≥ 0) → that path.
        std::fs::write(dir.path().join("one.jsonl"), AGENT_RUN_FIXTURE).unwrap();
        std::fs::write(dir.path().join("notes.txt"), "ignored").unwrap();
        assert_eq!(
            sole_transcript_in_dir_since(dir.path(), 0),
            Some(dir.path().join("one.jsonl"))
        );

        // A second transcript modified after dispatch ⇒ ambiguous ⇒ None (never
        // read another session's content into this run's row).
        std::fs::write(dir.path().join("two.jsonl"), AGENT_RUN_FIXTURE).unwrap();
        assert_eq!(sole_transcript_in_dir_since(dir.path(), 0), None);

        // A dispatch time in the far future excludes every existing transcript.
        assert_eq!(sole_transcript_in_dir_since(dir.path(), u64::MAX), None);
    }

    // ---- capture_final_assistant flush-race retry (U4b) --------------------

    // A transcript in the state it is in the instant the `Stop` hook fires: the
    // user turn is written, but the final assistant turn has NOT flushed yet.
    const USER_ONLY_FIXTURE: &str =
        concat!(r#"{"type":"user","message":{"role":"user","content":"run it"}}"#, "\n");

    #[test]
    fn capture_single_shot_misses_a_turn_that_has_not_flushed_yet() {
        // Reproduces the bug: on Stop the transcript exists (user turn written)
        // but the final assistant turn has not flushed, so one read finds no
        // assistant text — the single-shot behaviour that recorded empty output.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("s.jsonl"), USER_ONLY_FIXTURE).unwrap();
        assert_eq!(
            capture_final_assistant_in_dir_since(dir.path(), 0, 1, Duration::ZERO),
            None
        );
        // Once the assistant turn IS flushed, even a single-shot read captures it.
        std::fs::write(dir.path().join("s.jsonl"), AGENT_RUN_FIXTURE).unwrap();
        assert!(capture_final_assistant_in_dir_since(dir.path(), 0, 1, Duration::ZERO)
            .unwrap()
            .contains("82% full"));
    }

    #[test]
    fn capture_retries_until_the_final_turn_is_flushed() {
        // The fix: the assistant turn lands AFTER capture starts (as it does
        // ~100ms after Stop). A background flush appends it while the retry loop
        // polls; the loop must return the turn rather than abstain. The budget
        // (50 × 20ms = 1s) is a 20× margin over the ~50ms flush delay, so this
        // is not timing-fragile.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, USER_ONLY_FIXTURE).unwrap();
        let flush_path = path.clone();
        let flusher = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            std::fs::write(&flush_path, AGENT_RUN_FIXTURE).unwrap();
        });
        let got = capture_final_assistant_in_dir_since(
            dir.path(),
            0,
            50,
            Duration::from_millis(20),
        );
        flusher.join().unwrap();
        assert_eq!(
            got.as_deref(),
            Some("Disk is 82% full.\n\nClean ~/.cache to recover 4 GB.")
        );
    }

    #[test]
    fn capture_gives_up_within_budget_when_no_turn_ever_flushes() {
        // Bounded: a transcript that never gains an assistant turn abstains after
        // `attempts` reads instead of looping forever (ZERO delay ⇒ instant).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("s.jsonl"), USER_ONLY_FIXTURE).unwrap();
        assert_eq!(
            capture_final_assistant_in_dir_since(dir.path(), 0, 4, Duration::ZERO),
            None
        );
    }

    // ---- pending_interaction_from_str (feed-pending-question U1, KTD2) ------

    /// The verified live shape of an AskUserQuestion ask (2026-07-06): one
    /// `tool_use` flushed on its own assistant line, stamped at ask time, with
    /// `questions[].question/header/multiSelect/options[{label,description}]`.
    const ASK_ENTRY: &str = r#"{"type":"assistant","timestamp":"2026-07-06T13:12:27.103Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_ask1","name":"AskUserQuestion","input":{"questions":[{"question":"Lag feel?","header":"Lag","multiSelect":false,"options":[{"label":"Snappy","description":"Fast and tight"},{"label":"Floaty","description":"Loose"}]}]}}]}}"#;
    const ASKED_AT: u64 = 1_783_343_547_103;

    #[test]
    fn a_pending_ask_reports_choice_with_stamp_and_options() {
        let body = format!("{ASK_ENTRY}\n");
        let p = pending_interaction_from_str(&body).expect("pending");
        assert_eq!(p.kind, PendingKind::Choice);
        assert_eq!(p.asked_at_ms, ASKED_AT);
        assert_eq!(p.tool, "AskUserQuestion");
        assert!(p.answerable, "one single-select question is answerable");
        assert_eq!(p.questions.len(), 1);
        assert_eq!(p.questions[0].question, "Lag feel?");
        assert_eq!(p.questions[0].header, "Lag");
        assert!(!p.questions[0].multi_select);
        assert_eq!(p.questions[0].options.len(), 2);
        assert_eq!(p.questions[0].options[0].label, "Snappy");
        assert_eq!(p.questions[0].options[0].description, "Fast and tight");
        assert_eq!(p.input, None);
    }

    #[test]
    fn multi_select_and_multi_question_asks_are_not_answerable() {
        // multiSelect: exposed but read-only in v1 (R7).
        let multi = r#"{"type":"assistant","timestamp":"2026-07-06T13:12:27.103Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"AskUserQuestion","input":{"questions":[{"question":"Which?","header":"H","multiSelect":true,"options":[{"label":"A"},{"label":"B"}]}]}}]}}"#;
        let p = pending_interaction_from_str(&format!("{multi}\n")).unwrap();
        assert!(!p.answerable);
        assert!(p.questions[0].multi_select);
        // Two questions: the answered latch keys on one askedAt, so a batch is
        // read-only too.
        let two = r#"{"type":"assistant","timestamp":"2026-07-06T13:12:27.103Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"AskUserQuestion","input":{"questions":[{"question":"One?","options":[{"label":"A"}]},{"question":"Two?","options":[{"label":"B"}]}]}}]}}"#;
        let p = pending_interaction_from_str(&format!("{two}\n")).unwrap();
        assert!(!p.answerable);
        assert_eq!(p.questions.len(), 2);
        // Cosmetic omissions (header/multiSelect/description) default, not
        // abstain — the question text and options are what matter.
        assert_eq!(p.questions[0].header, "");
        assert!(!p.questions[0].multi_select);
        assert_eq!(p.questions[0].options[0].description, "");
    }

    #[test]
    fn a_matching_tool_result_clears_pending_but_a_different_id_does_not() {
        // Resolved: the answering tool_result lands as its own user entry.
        let resolved = format!(
            "{ASK_ENTRY}\n{}\n",
            r#"{"type":"user","timestamp":"2026-07-06T13:14:06.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_ask1","content":"chose Snappy"}]}}"#
        );
        assert_eq!(pending_interaction_from_str(&resolved), None);
        // Non-subsumption: a result for a DIFFERENT id leaves the ask pending.
        let other = format!(
            "{ASK_ENTRY}\n{}\n",
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_other","content":"ok"}]}}"#
        );
        let p = pending_interaction_from_str(&other).expect("still pending");
        assert_eq!(p.kind, PendingKind::Choice);
    }

    #[test]
    fn parallel_batch_tail_results_resolve_by_id_not_position() {
        // The verified batch shape: both tool_use lines flush first, the
        // sibling results append after them. A result for A only leaves B
        // pending — the backward walk consumes ids through the transparent
        // tail results (KTD2, AE6).
        let use_a = r#"{"type":"assistant","timestamp":"2026-07-06T10:00:00.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"tA","name":"Bash","input":{"command":"ls"}}]}}"#;
        let use_b = r#"{"type":"assistant","timestamp":"2026-07-06T10:00:01.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"tB","name":"Bash","input":{"command":"pwd"}}]}}"#;
        let res_a = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tA","content":"done"}]}}"#;
        let res_b = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tB","content":"done"}]}}"#;
        let one_resolved = format!("{use_a}\n{use_b}\n{res_a}\n");
        let p = pending_interaction_from_str(&one_resolved).expect("B pending");
        assert_eq!(p.kind, PendingKind::Permission);
        assert_eq!(p.tool, "Bash");
        assert_eq!(p.input.as_ref().unwrap()["command"], "pwd");
        // Both resolved → nothing pending.
        let both = format!("{use_a}\n{use_b}\n{res_a}\n{res_b}\n");
        assert_eq!(pending_interaction_from_str(&both), None);
        // Both unresolved → ambiguous (>1 non-ask) → abstain.
        let neither = format!("{use_a}\n{use_b}\n");
        assert_eq!(pending_interaction_from_str(&neither), None);
    }

    #[test]
    fn an_ask_wins_even_beside_an_unresolved_sibling() {
        // KTD2: AskUserQuestion never "executes", so it is exposed even when a
        // parallel sibling is also unconsumed (where a non-ask pair abstains).
        let use_b = r#"{"type":"assistant","timestamp":"2026-07-06T10:00:01.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"tB","name":"Bash","input":{"command":"pwd"}}]}}"#;
        let body = format!("{use_b}\n{ASK_ENTRY}\n");
        let p = pending_interaction_from_str(&body).expect("ask pending");
        assert_eq!(p.kind, PendingKind::Choice);
    }

    #[test]
    fn conversation_moving_on_or_an_interrupt_clears_pending() {
        // A plain text user entry (no tool_result) is the boundary: the
        // conversation resumed, whatever ids sit beyond it.
        let moved_on = format!(
            "{ASK_ENTRY}\n{}\n",
            r#"{"type":"user","message":{"role":"user","content":"actually, never mind"}}"#
        );
        assert_eq!(pending_interaction_from_str(&moved_on), None);
        // The verified Esc/reject shape: an is_error tool_result for the same
        // id (consumed like any answer), then a text user entry.
        let rejected = format!(
            "{ASK_ENTRY}\n{}\n{}\n",
            r#"{"type":"user","timestamp":"2026-07-06T16:36:27.791Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_ask1","is_error":true,"content":"The user doesn't want to proceed with this tool use."}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]}}"#
        );
        assert_eq!(pending_interaction_from_str(&rejected), None);
        // The synthesized single-entry interrupt shape: tool_result + text in
        // ONE user entry — consumed AND the boundary.
        let synthesized = format!(
            "{ASK_ENTRY}\n{}\n",
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_ask1","content":"[Request interrupted]"},{"type":"text","text":"[Request interrupted by user]"}]}}"#
        );
        assert_eq!(pending_interaction_from_str(&synthesized), None);
    }

    #[test]
    fn trailing_metadata_and_sidechain_entries_are_transparent() {
        // Metadata after the ask must not hide it…
        let with_meta = format!(
            "{ASK_ENTRY}\n{}\n{}\n",
            r#"{"type":"ai-title"}"#, r#"{"type":"mode"}"#
        );
        assert!(pending_interaction_from_str(&with_meta).is_some());
        // …and a sidechain tool_use at the tail is ignored outright (a
        // subagent's pending tool is not THIS conversation's question).
        let sidechain_tail = format!(
            "{ASK_ENTRY}\n{}\n{}\n",
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_ask1","content":"ok"}]}}"#,
            r#"{"type":"assistant","isSidechain":true,"timestamp":"2026-07-06T13:20:00.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"sc1","name":"Bash","input":{}}]}}"#
        );
        assert_eq!(pending_interaction_from_str(&sidechain_tail), None);
    }

    #[test]
    fn a_sole_pending_delegating_tool_abstains() {
        // KTD3: the on-screen dialog belongs to the subagent's inner tool, so
        // exposing (or answering) the delegating tool would mislabel it.
        // `Agent` is the live name (verified 2026-07-06); `Task` the historical.
        for tool in ["Agent", "Task"] {
            let body = format!(
                "{}\n",
                format!(
                    r#"{{"type":"assistant","timestamp":"2026-07-06T10:00:00.000Z","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"t1","name":"{tool}","input":{{"prompt":"go"}}}}]}}}}"#
                )
            );
            assert_eq!(pending_interaction_from_str(&body), None, "{tool} must abstain");
        }
    }

    #[test]
    fn a_sole_pending_ordinary_tool_reports_permission_with_input() {
        let body = format!(
            "{}\n",
            r#"{"type":"assistant","timestamp":"2026-07-06T10:00:00.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"rm -rf build"}}]}}"#
        );
        let p = pending_interaction_from_str(&body).expect("pending");
        assert_eq!(p.kind, PendingKind::Permission);
        assert_eq!(p.tool, "Bash");
        assert!(!p.answerable);
        assert!(p.questions.is_empty());
        assert_eq!(p.input.as_ref().unwrap()["command"], "rm -rf build");
    }

    #[test]
    fn context_comes_from_the_same_or_immediately_preceding_assistant_entry() {
        // Same-entry text beside the tool_use → context.
        let same = r#"{"type":"assistant","timestamp":"2026-07-06T13:12:27.103Z","message":{"role":"assistant","content":[{"type":"text","text":"Pick a lag feel for the game."},{"type":"tool_use","id":"t1","name":"AskUserQuestion","input":{"questions":[{"question":"Lag?","options":[{"label":"A"}]}]}}]}}"#;
        let p = pending_interaction_from_str(&format!("{same}\n")).unwrap();
        assert_eq!(p.context.as_deref(), Some("Pick a lag feel for the game."));
        // Immediately preceding assistant text entry → context; thinking-only
        // entries between are transparent (the verified live shape: text,
        // thinking, ask).
        let text = r#"{"type":"assistant","timestamp":"2026-07-06T13:12:20.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Two options stand out."}]}}"#;
        let thinking = r#"{"type":"assistant","timestamp":"2026-07-06T13:12:25.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hmm"}]}}"#;
        let p = pending_interaction_from_str(&format!("{text}\n{thinking}\n{ASK_ENTRY}\n")).unwrap();
        assert_eq!(p.context.as_deref(), Some("Two options stand out."));
        // A tool_result-only user entry between breaks adjacency: that text
        // narrated the tool, not the ask (R2 — never stale context).
        let use_x = r#"{"type":"assistant","timestamp":"2026-07-06T13:12:22.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"tX","name":"Bash","input":{}}]}}"#;
        let res_x = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tX","content":"out"}]}}"#;
        let p = pending_interaction_from_str(&format!("{text}\n{use_x}\n{res_x}\n{ASK_ENTRY}\n"))
            .unwrap();
        assert_eq!(p.context, None);
    }

    #[test]
    fn a_parse_time_dropped_option_forces_not_answerable() {
        // A single single-select question whose options array carries a
        // malformed (label-less) entry: the good options still parse, but the
        // on-screen picker renders all three positions, so a wire digit would
        // map to the wrong option. `dropped` must force answerable=false even
        // though the surviving shape looks answerable (a dropped question does
        // too — covered by the len check).
        let entry = r#"{"type":"assistant","timestamp":"2026-07-06T10:00:00.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"AskUserQuestion","input":{"questions":[{"question":"Pick?","options":[{"label":"A"},{"description":"no label"},{"label":"C"}]}]}}]}}"#;
        let p = pending_interaction_from_str(&format!("{entry}\n")).expect("pending");
        assert_eq!(p.kind, PendingKind::Choice);
        assert_eq!(p.questions[0].options.len(), 2, "only the two labelled options parse");
        assert!(!p.answerable, "a parse-time option drop breaks the digit mapping");
    }

    #[test]
    fn surprise_shapes_abstain_rather_than_guess() {
        // A stampless pending entry can't key the wire marker or the answer
        // guard → abstain.
        let stampless = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"AskUserQuestion","input":{"questions":[{"question":"Q?","options":[{"label":"A"}]}]}}]}}"#;
        assert_eq!(pending_interaction_from_str(&format!("{stampless}\n")), None);
        // An ask whose questions are all unparseable → abstain, not an empty shell.
        let empty_qs = r#"{"type":"assistant","timestamp":"2026-07-06T10:00:00.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"AskUserQuestion","input":{"questions":[{"header":"no question field"}]}}]}}"#;
        assert_eq!(pending_interaction_from_str(&format!("{empty_qs}\n")), None);
        // Malformed lines are skipped; empty body → None.
        assert_eq!(pending_interaction_from_str("{ not json\ngarbage\n"), None);
        assert_eq!(pending_interaction_from_str(""), None);
    }

    #[test]
    fn transcript_io_resolves_both_halves_from_one_read() {
        // A completed reply AND a later pending ask coexist: during a pending
        // question the last text-bearing assistant entry legitimately doubles
        // as the reply (the design blesses the duplication).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let reply = r#"{"type":"assistant","timestamp":"2026-07-06T13:12:20.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Here is the summary."}]}}"#;
        std::fs::write(&path, format!("{reply}\n{ASK_ENTRY}\n")).unwrap();
        let io = transcript_io(&path).expect("readable");
        assert_eq!(io.reply.as_ref().unwrap().text, "Here is the summary.");
        let p = io.pending.expect("pending");
        assert_eq!(p.asked_at_ms, ASKED_AT);
        assert_eq!(p.context.as_deref(), Some("Here is the summary."));
        // The turns window rides the same read (feed-conversation-tail U1).
        assert_eq!(io.turns.len(), 1);
        assert_eq!(io.turns[0].text, "Here is the summary.");
        // Missing file → None (never an error).
        assert_eq!(transcript_io(&dir.path().join("gone.jsonl")), None);
    }

    // ---- conversation-tail scan (feed-conversation-tail U1) -----------------

    /// A stamped user prompt entry (string body).
    fn user_entry(ts: &str, text: &str) -> String {
        format!(
            r#"{{"type":"user","timestamp":"{ts}","message":{{"role":"user","content":"{text}"}}}}"#
        )
    }

    /// A stamped assistant text entry.
    fn agent_entry(ts: &str, text: &str) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","message":{{"role":"assistant","content":[{{"type":"text","text":"{text}"}}]}}}}"#
        )
    }

    #[test]
    fn turns_scan_keeps_conversation_and_excludes_tool_chatter() {
        // A prompt → narration → tool_use → tool_result → final reply
        // exchange yields exactly two turns: the prompt and the run's FINAL
        // text (R6 collapse — the mid-run narration is intermediate output,
        // caught live on a real transcript where narration crowded out every
        // prompt). The tool_use-only assistant entry and the tool_result-only
        // user entry (the shape a tool return AND a remote keys-mode digit
        // answer both take) never become turns, nor do they delimit the run.
        let body = [
            user_entry("2026-07-09T10:00:00.000Z", "run the tests"),
            agent_entry("2026-07-09T10:00:05.000Z", "Running them now."),
            r#"{"type":"assistant","timestamp":"2026-07-09T10:00:06.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test"}}]}}"#.to_string(),
            r#"{"type":"user","timestamp":"2026-07-09T10:00:20.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#.to_string(),
            agent_entry("2026-07-09T10:00:25.000Z", "All tests pass."),
        ]
        .join("\n");
        let turns = conversation_turns_from_str(&body);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].role, TurnRole::User);
        assert_eq!(turns[0].text, "run the tests");
        assert!(turns[0].at_ms.is_some());
        assert_eq!(turns[1].role, TurnRole::Agent);
        assert_eq!(turns[1].text, "All tests pass.", "the run's final text");
    }

    #[test]
    fn turns_scan_a_new_prompt_ends_the_collapse_run() {
        // Collapse only spans ONE working stretch: a user prompt delimits it,
        // so reply → prompt → reply keeps both replies.
        let body = [
            agent_entry("2026-07-09T10:00:00.000Z", "First reply."),
            user_entry("2026-07-09T10:00:10.000Z", "and now?"),
            agent_entry("2026-07-09T10:00:20.000Z", "Second reply."),
        ]
        .join("\n");
        let turns = conversation_turns_from_str(&body);
        assert_eq!(
            turns.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
            vec!["First reply.", "and now?", "Second reply."]
        );
    }

    #[test]
    fn turns_scan_newest_agent_turn_is_the_reply_entry() {
        // KTD1: the scan's agent-turn predicate is last_assistant_reply's, so
        // the newest agent turn carries the reply's exact text + stamp — the
        // by-construction guarantee the wire's R3 correlation rests on.
        let body = [
            user_entry("2026-07-09T10:00:00.000Z", "status?"),
            agent_entry("2026-07-09T10:00:05.000Z", "Checking."),
            agent_entry("2026-07-09T10:00:25.000Z", "All green."),
        ]
        .join("\n");
        let reply = last_assistant_reply_from_str(&body).expect("reply");
        let turns = conversation_turns_from_str(&body);
        let newest_agent = turns
            .iter()
            .rev()
            .find(|t| t.role == TurnRole::Agent)
            .expect("agent turn");
        assert_eq!(newest_agent.text, reply.text);
        assert_eq!(newest_agent.at_ms, reply.replied_at_ms);
    }

    #[test]
    fn turns_scan_skips_meta_sidechain_and_summary_user_entries() {
        // isMeta (caveats/injected notes), isSidechain (subagent traffic), and
        // isCompactSummary user entries are not prompts delivered to THIS
        // agent — none become turns. Thinking-only assistant entries carry no
        // text and drop out on the text predicate.
        let body = [
            r#"{"type":"user","isMeta":true,"timestamp":"2026-07-09T10:00:00.000Z","message":{"role":"user","content":"Caveat: injected note"}}"#.to_string(),
            r#"{"type":"user","isSidechain":true,"timestamp":"2026-07-09T10:00:01.000Z","message":{"role":"user","content":"subagent prompt"}}"#.to_string(),
            r#"{"type":"user","isCompactSummary":true,"timestamp":"2026-07-09T10:00:02.000Z","message":{"role":"user","content":"compacted history"}}"#.to_string(),
            r#"{"type":"assistant","timestamp":"2026-07-09T10:00:03.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hmm"}]}}"#.to_string(),
            user_entry("2026-07-09T10:00:04.000Z", "a real prompt"),
        ]
        .join("\n");
        let turns = conversation_turns_from_str(&body);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].text, "a real prompt");
    }

    #[test]
    fn turns_scan_excludes_harness_bookkeeping_user_entries() {
        // Slash-command records and local-command output are harness
        // bookkeeping, not prompts delivered to the agent — and real
        // transcripts carry NO isMeta on them, so only the content shape (the
        // leading tag) identifies them. None become turns, and like every
        // non-conversation entry they are transparent to the run collapse
        // (the two replies straddling them collapse to the final one). The
        // match is prefix-only: a tag mentioned mid-prompt still surfaces.
        let body = [
            agent_entry("2026-07-09T10:00:00.000Z", "First reply."),
            user_entry("2026-07-09T10:00:01.000Z", "<command-name>/clear</command-name>"),
            user_entry("2026-07-09T10:00:02.000Z", "<command-message>clear</command-message>"),
            user_entry("2026-07-09T10:00:03.000Z", "<local-command-stdout>ok</local-command-stdout>"),
            user_entry("2026-07-09T10:00:04.000Z", "<local-command-stderr>err</local-command-stderr>"),
            agent_entry("2026-07-09T10:00:05.000Z", "Second reply."),
            user_entry("2026-07-09T10:00:06.000Z", "what does <command-name> mean?"),
        ]
        .join("\n");
        let turns = conversation_turns_from_str(&body);
        assert_eq!(
            turns.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
            vec!["Second reply.", "what does <command-name> mean?"],
            "bookkeeping yields no turns and does not delimit the run"
        );
    }

    #[test]
    fn turns_scan_is_bounded_to_the_buffer_keeping_the_newest() {
        // KTD3: the scan retains only the trailing RAW_TURN_BUFFER turns.
        let body: Vec<String> = (0..RAW_TURN_BUFFER + 10)
            .map(|i| user_entry("2026-07-09T10:00:00.000Z", &format!("prompt {i}")))
            .collect();
        let turns = conversation_turns_from_str(&body.join("\n"));
        assert_eq!(turns.len(), RAW_TURN_BUFFER);
        assert_eq!(turns[0].text, "prompt 10", "oldest retained is the 11th");
        assert_eq!(
            turns.last().unwrap().text,
            format!("prompt {}", RAW_TURN_BUFFER + 9)
        );
    }

    #[test]
    fn turns_scan_stampless_and_blockless_shapes_degrade() {
        // A stampless turn is kept raw with at_ms None (the shaping pass drops
        // it); a user entry mixing tool_result + text blocks joins the text.
        let body = [
            r#"{"type":"user","message":{"role":"user","content":"no stamp"}}"#.to_string(),
            r#"{"type":"user","timestamp":"2026-07-09T10:00:01.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"},{"type":"text","text":"and a"},{"type":"text","text":"real prompt"}]}}"#.to_string(),
        ]
        .join("\n");
        let turns = conversation_turns_from_str(&body);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].at_ms, None);
        assert_eq!(turns[1].text, "and a\n\nreal prompt");
        // Empty / corrupt bodies yield no turns, never an error.
        assert!(conversation_turns_from_str("").is_empty());
        assert!(conversation_turns_from_str("{ not json\n").is_empty());
    }
}
