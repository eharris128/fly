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
fn claude_projects_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Some(PathBuf::from(dir).join("projects"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude/projects"))
}

/// Encode an absolute cwd to Claude's project-dir name: replace every `/` **and**
/// `.` with `-` (KTD-E). A trailing slash is normalized away first, so `…/play/`
/// and `…/play` map to the same dir.
fn encode_cwd(cwd: &str) -> String {
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
/// (KTD-A), so this is the one place that knowledge lives.
fn jsonl_id(name: &str) -> Option<&str> {
    let id = name.strip_suffix(".jsonl")?;
    (!id.is_empty()).then_some(id)
}

/// The session id of the **actively-written** transcript among `entries`
/// (`(file-name, mtime)` pairs from the project dir): the basename of the newest
/// `.jsonl` whose mtime is within `max_age` of `now` (KTD-A). The recency floor is
/// what keeps a fresh agent that has not written its own transcript yet from
/// adopting an ancient unrelated session in the same dir: an all-stale set yields
/// `None`, so the poll captures nothing and the restore path's stale-guard (U3)
/// stays in charge. A future mtime (clock skew) counts as fresh, never stale. Pure
/// over its arguments.
pub fn active_session_id(
    entries: &[(String, SystemTime)],
    now: SystemTime,
    max_age: Duration,
) -> Option<String> {
    entries
        .iter()
        .filter_map(|(name, mtime)| jsonl_id(name).map(|id| (id, *mtime)))
        .filter(|(_, mtime)| {
            now.duration_since(*mtime)
                .map(|age| age <= max_age)
                .unwrap_or(true)
        })
        .max_by_key(|(_, mtime)| *mtime)
        .map(|(id, _)| id.to_string())
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

/// Pure core of [`session_last_turn_ms`]: scan JSONL `body` for the last
/// `user`/`assistant` line bearing a parseable ISO-8601 `timestamp`. Tolerant of
/// a non-JSON or unterminated trailing line (skipped), matching the thin-reader
/// contract elsewhere. Reads the whole body and keeps the last match — a
/// linear-scan "tail-parse"; the file is only read at restore, never on a hot
/// path, so a from-the-end optimization is unwarranted.
fn last_turn_ms_from_str(body: &str) -> Option<u64> {
    let mut last: Option<u64> = None;
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
        if let Some(ms) = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(iso8601_to_ms)
        {
            last = Some(ms);
        }
    }
    last
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

/// Read a project dir's `(file-name, mtime)` entries for [`active_session_id`].
/// Best-effort: a missing dir or an unreadable entry yields an empty/partial list,
/// never an error (the caller degrades to "no id"). An entry whose mtime is
/// unreadable is kept with a `UNIX_EPOCH` stamp so it sorts oldest rather than
/// being dropped.
fn read_project_entries(dir: &Path) -> Vec<(String, SystemTime)> {
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
/// unresolvable/missing/empty or nothing was written recently. The single I/O
/// entry point the `pane_session_id` command (U1) and the resume poll (U2) share.
pub fn active_session_for_cwd(cwd: &Path) -> Option<String> {
    let dir = claude_project_dir(cwd)?;
    let entries = read_project_entries(&dir);
    active_session_id(&entries, SystemTime::now(), ACTIVE_SESSION_MAX_AGE)
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

/// The session `claude --continue` would re-open in a project dir (the newest
/// transcript) paired with its **last real-turn** timestamp — the freshness signal
/// the restore-time stale-guard compares against the pane's own activity (U3,
/// KTD-C). `last_turn_ms` is `None` for a transcript with no timestamped turn; the
/// guard treats that as stale, so a contentless candidate never resurrects a pane.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContinueTarget {
    pub session_id: String,
    pub last_turn_ms: Option<u64>,
}

/// Path-taking core of [`continue_target`]: the newest transcript in `dir` plus
/// its last real-turn time. `None` for a missing/empty dir (no `--continue` target
/// → the leaf falls to a bare shell).
fn continue_target_in_dir(dir: &Path) -> Option<ContinueTarget> {
    let entries = read_project_entries(dir);
    let session_id = newest_session_basename(&entries)?;
    let last_turn_ms = session_last_turn_ms(&dir.join(format!("{session_id}.jsonl")));
    Some(ContinueTarget { session_id, last_turn_ms })
}

/// Command: the session `claude --continue` would re-open in `cwd`, plus its last
/// real-turn time, so the frontend's stale-guard can decide keep-vs-bare-shell in
/// one round-trip (U3, KTD-C). `None` when the project dir is unresolvable/empty.
#[tauri::command]
pub fn continue_target(cwd: String) -> Option<ContinueTarget> {
    let dir = claude_project_dir(Path::new(&cwd))?;
    continue_target_in_dir(&dir)
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

    // ---- active_session_id (KTD-A) -----------------------------------------

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn picks_the_max_mtime_jsonl_basename() {
        let entries = vec![
            ("old-session.jsonl".to_string(), at(100)),
            ("new-session.jsonl".to_string(), at(200)),
        ];
        assert_eq!(
            active_session_id(&entries, at(200), Duration::from_secs(3600)),
            Some("new-session".to_string())
        );
    }

    #[test]
    fn ignores_non_jsonl_entries() {
        // A newer non-`.jsonl` file (e.g. a stray note) must not be picked.
        let entries = vec![
            ("notes.txt".to_string(), at(900)),
            ("the-session.jsonl".to_string(), at(100)),
        ];
        assert_eq!(
            active_session_id(&entries, at(901), Duration::from_secs(3600)),
            Some("the-session".to_string())
        );
    }

    #[test]
    fn empty_list_is_none() {
        assert_eq!(
            active_session_id(&[], at(100), Duration::from_secs(3600)),
            None
        );
    }

    #[test]
    fn an_all_old_set_is_none_under_the_recency_floor() {
        // The only transcript was written long before `now` → not the live
        // session → None (so the poll captures nothing).
        let entries = vec![("ancient.jsonl".to_string(), at(10))];
        assert_eq!(
            active_session_id(&entries, at(10_000), Duration::from_secs(100)),
            None
        );
    }

    #[test]
    fn a_future_mtime_counts_as_fresh() {
        // Clock skew: an mtime after `now` must not be treated as stale.
        let entries = vec![("future.jsonl".to_string(), at(5_000))];
        assert_eq!(
            active_session_id(&entries, at(1_000), Duration::from_secs(60)),
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
        assert_eq!(target.session_id, "04d56f41");
        assert_eq!(target.last_turn_ms, Some(1_781_896_636_402)); // 06-19, not mtime
    }

    #[test]
    fn continue_target_is_none_for_an_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(continue_target_in_dir(dir.path()), None);
    }
}
