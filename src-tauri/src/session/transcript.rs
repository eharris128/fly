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

/// Pure core of [`session_last_turn_ms`]: the stamp half of the one turn scan.
fn last_turn_ms_from_str(body: &str) -> Option<u64> {
    turn_summary_from_str(body).map(|s| s.last_turn_ms)
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

/// The one turn scan: walk JSONL `body` keeping the last `user`/`assistant`
/// line bearing a parseable ISO-8601 `timestamp` (the real-turn stamp, KTD-C)
/// and, independently, the last such line whose `message.content` yields text
/// (the snippet — a turn may be tool-use-only, so the snippet can come from an
/// earlier turn than the stamp). Tolerant of a non-JSON or unterminated
/// trailing line (skipped), matching the thin-reader contract elsewhere. The
/// content shapes are undocumented Claude contracts, so extraction is
/// defensive: a string body, or the first `{"type":"text"}` block of an array
/// (KTD6). Linear scan; read at picker/restore time, never on a hot path.
fn turn_summary_from_str(body: &str) -> Option<TurnSummary> {
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
        if let Some(text) = turn_text(&v) {
            snippet = Some(text);
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
/// other control character — transcript text is agent-authored, so treat it
/// like the alerts log (R16 posture) — and truncate to [`SNIPPET_MAX_CHARS`]
/// on a char boundary with an ellipsis. `None` when nothing displayable is left.
fn sanitize_snippet(raw: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = 0usize;
    let mut in_ws = false;
    for c in raw.chars() {
        if c.is_whitespace() {
            in_ws = true;
            continue;
        }
        if c.is_control() {
            continue;
        }
        if chars >= SNIPPET_MAX_CHARS {
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
}
