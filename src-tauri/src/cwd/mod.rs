//! `/proc`-based inspection of a pane's foreground process (U10, R13; U2 of the
//! agent-dashboard plan).
//!
//! Reads `/proc/<pid>/{cwd,comm,cmdline}` — robust on Linux and needs no shell
//! cooperation, since default Ubuntu shells don't emit OSC 7 outside VTE.
//! Sampled on a low cadence (focus change / before a save / the dashboard poll),
//! never on the hot output path. The optional OSC 7 fast path and the OSC/BEL
//! attention scanner are deferred with the multi-agent matrix (KTD9).

use std::path::PathBuf;

/// The current working directory of process `pid`, via `/proc/<pid>/cwd`.
/// Returns `None` if the process is gone or unreadable.
pub fn proc_cwd(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// The process name from `/proc/<pid>/comm` (kernel-truncated to 15 bytes),
/// trimmed of its trailing newline. `None` if the process is gone/unreadable.
pub fn proc_comm(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim_end().to_string())
}

/// The argv of process `pid` from `/proc/<pid>/cmdline` (NUL-separated), as a
/// vector of UTF-8-lossy strings with empty trailing fields dropped. An empty or
/// unreadable cmdline yields an empty vec.
pub fn proc_cmdline(pid: u32) -> Vec<String> {
    match std::fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(raw) => raw
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Snapshot the live process table for the background-task count (running-state
/// plan U2, KTD4) — one `/proc` scan per call, off the PTY hot path.
///
/// Enumerates `/proc`, keeps the all-numeric entries, and parses each
/// `/proc/<pid>/stat` via [`parse_stat_line`]. Best-effort: a directory that
/// vanishes mid-scan (the process exited), an unreadable `stat`, or a line that
/// fails to parse is silently skipped — it returns whatever was readable and never
/// panics, matching the thin-reader contract of [`proc_cmdline`]. The single
/// snapshot is what [`count_background_task_groups`] resolves the root and its
/// descendants against, so a per-call count is internally consistent against pid
/// reuse (KTD4).
pub fn read_proc_table() -> Vec<ProcEntry> {
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in dir.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{name}/stat")) {
            if let Some(e) = parse_stat_line(&stat) {
                out.push(e);
            }
        }
    }
    out
}

/// Parse one `/proc/<pid>/stat` line into a [`ProcEntry`] (U2). Factored out and
/// unit-tested directly because the format is a parsing trap: field 2 (`comm`) is
/// wrapped in parens **and may itself contain spaces and parens**. So we split on
/// the **last** `')'` — `comm` is everything between the first `'('` and that last
/// `')'`; the `pid` is the token before the first `'('`; and the space-separated
/// remainder after the last `')'` is `state` (field 0), `ppid` (1), `pgrp` (2),
/// per `proc(5)`. Any missing/non-numeric field yields `None` (a truncated read or
/// a line we don't understand is skipped, never a panic).
fn parse_stat_line(line: &str) -> Option<ProcEntry> {
    let lparen = line.find('(')?;
    let rparen = line.rfind(')')?;
    if rparen < lparen {
        return None;
    }
    let pid: u32 = line[..lparen].trim().parse().ok()?;
    let comm = line[lparen + 1..rparen].to_string();
    let mut rest = line[rparen + 1..].split_whitespace();
    let state = rest.next()?.chars().next()?;
    let ppid: u32 = rest.next()?.parse().ok()?;
    let pgid: u32 = rest.next()?.parse().ok()?;
    Some(ProcEntry { pid, ppid, pgid, state, comm })
}

/// The basename of a `/`-separated path (the part after the last `/`).
fn basename(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

/// JS runtimes that may wrap a JS Claude Code entrypoint (npm-global installs).
const JS_RUNTIMES: &[&str] = &["node", "bun", "deno"];

/// Whether a pane's foreground process is Claude Code, decided purely from its
/// `/proc` comm + cmdline (KTD-D, U2). Three install shapes are accepted:
///   1. `comm == "claude"` — the process names itself `claude`.
///   2. `argv[0]` basename is `claude` — a native binary or a `claude`-named
///      symlink invoked as `claude` (the common local install).
///   3. a JS runtime (`node`/`bun`/`deno`) as `argv[0]` **plus** a later argv
///      whose basename is `claude`, or `cli.js` under a `claude/` path — the
///      npm-global wrapper.
///
/// The wrapper rule (3) is deliberately narrow — it requires a JS-runtime
/// `argv[0]`, not just any argv mentioning `claude` — so a non-agent command
/// referencing a `claude`-named path (e.g. `tail -f ~/.claude/x.log`) does not
/// false-positive.
pub fn is_claude(comm: Option<&str>, argv: &[String]) -> bool {
    if comm == Some("claude") {
        return true;
    }
    let Some(arg0) = argv.first() else {
        return false;
    };
    if basename(arg0) == "claude" {
        return true;
    }
    if JS_RUNTIMES.contains(&basename(arg0)) {
        return argv[1..].iter().any(|a| {
            let b = basename(a);
            b == "claude" || (b == "cli.js" && a.contains("claude/"))
        });
    }
    false
}

/// One process's identity, distilled from `/proc/<pid>/stat` (dashboard
/// running-state plan, U1/U2). `comm` is carried for the KTD6 helper-vs-job
/// discrimination fallback and is **unused** by the v1 count logic — the empirical
/// check found Claude Code runs no persistent own-pgid helpers, so the pgid filter
/// alone suffices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcEntry {
    pub pid: u32,
    pub ppid: u32,
    pub pgid: u32,
    /// The single-char run state from `stat` (`R`/`S`/`D`/`Z`/`X`/…).
    pub state: char,
    pub comm: String,
}

/// Count the distinct **live background process groups** beneath an agent
/// (running-state plan U1, KTD2/KTD3; R2) — the honest "N tasks" number.
///
/// `root_pid` is the pane's foreground pid which — being the foreground
/// process-group leader (`tcgetpgrp`) — equals the agent's pgid. The count is the
/// number of distinct `pgid` values among the agent's **transitive descendants**
/// (walked via `ppid` edges) that are all of:
///   - **live** — state is not `Z` (zombie) or `X` (dead);
///   - **backgrounded** — `pgid != root_pid`; and
///   - **top-level** — anchored directly off the agent's group: some live member
///     has a parent in the root's group (`pgid == root_pid`, the root included),
///     so it is not nested inside another background group.
///
/// Backgrounding *is* "a descendant in a different process group", so the pgid
/// filter excludes the agent's own foreground children/same-group helpers, and
/// distinct-pgid collapses a pipeline (N procs, one group) to one task. The
/// top-level filter then collapses a *job that spans nested groups* to one task:
/// Claude Code wraps every command in a `bash -c` child (its own group/session)
/// whose real command re-forks into a further group, so one job is two nested
/// groups — only the wrapper, anchored at the agent, counts. This matches the
/// user's mental model and Claude Code's "N shells still running" line.
///
/// Pure over its argument table (no `/proc` I/O — that is [`read_proc_table`]),
/// mirroring [`is_claude`]. A reparented (double-forked) task reparents to pid 1,
/// escaping the descendant walk, and is undercounted — accepted (KTD3). A nested
/// background group (a job's inner `setsid`/re-forked group) folds into its
/// top-level parent group rather than re-inflating the count (KTD2). Traversal
/// carries a visited set, so a malformed table (self-parent,
/// `ppid` cycle, duplicate pid) cannot infinite-loop.
pub fn count_background_task_groups(table: &[ProcEntry], root_pid: u32) -> u32 {
    use std::collections::{HashMap, HashSet};

    // `ppid -> child pids` and `pid -> entry`, each one pass. Duplicate/malformed
    // entries fold in without special-casing (the visited set bounds the walk).
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for e in table {
        children.entry(e.ppid).or_default().push(e.pid);
    }
    let by_pid: HashMap<u32, &ProcEntry> = table.iter().map(|e| (e.pid, e)).collect();

    // Transitive descendants of the agent root. `visited.insert` both guards the
    // cycle/self-parent/duplicate cases and records membership in one step; the
    // root is pre-seeded so it can never re-enqueue itself or count as its own job.
    let mut visited: HashSet<u32> = HashSet::from([root_pid]);
    let mut descendants: HashSet<u32> = HashSet::new();
    let mut frontier = vec![root_pid];
    while let Some(parent) = frontier.pop() {
        let Some(kids) = children.get(&parent) else { continue };
        for &kid in kids {
            if visited.insert(kid) {
                descendants.insert(kid);
                frontier.push(kid);
            }
        }
    }

    // Distinct *top-level* background groups among the live descendants. A live,
    // backgrounded descendant counts its group as a real task only when it is
    // anchored directly off the agent's own group — i.e. its parent sits in the
    // root's foreground group (`pgid == root_pid`, which includes the root
    // itself). A backgrounded descendant whose parent is in *another* background
    // group is the inner group of a job the agent already launched, not a second
    // job: Claude Code runs every command through a `bash -c` wrapper (the
    // wrapper is the agent's child in its own group/session; the real command
    // re-forks into a further group), so one job spans two nested groups. Anchor
    // gating collapses that pair to one — without it, every background task
    // double-counts (KTD2).
    let anchored_to_agent = |ppid: u32| {
        ppid == root_pid || by_pid.get(&ppid).is_some_and(|e| e.pgid == root_pid)
    };
    let mut groups: HashSet<u32> = HashSet::new();
    for pid in &descendants {
        if let Some(e) = by_pid.get(pid) {
            if e.state != 'Z' && e.state != 'X' && e.pgid != root_pid && anchored_to_agent(e.ppid)
            {
                groups.insert(e.pgid);
            }
        }
    }
    groups.len() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn matches_comm_named_claude() {
        assert!(is_claude(Some("claude"), &argv(&["whatever"])));
    }

    #[test]
    fn matches_native_binary_or_symlink_argv0() {
        // The common local install: a `claude` symlink/binary invoked as `claude`.
        assert!(is_claude(Some("2.1.185"), &argv(&["claude"])));
        assert!(is_claude(None, &argv(&["/home/u/.local/bin/claude"])));
    }

    #[test]
    fn matches_js_wrapper() {
        assert!(is_claude(
            Some("node"),
            &argv(&["node", "/home/u/.npm/.../claude/cli.js"])
        ));
        assert!(is_claude(Some("bun"), &argv(&["bun", "/opt/claude"])));
    }

    #[test]
    fn rejects_plain_shell() {
        assert!(!is_claude(Some("bash"), &argv(&["-bash"])));
        assert!(!is_claude(Some("zsh"), &argv(&["-zsh"])));
    }

    #[test]
    fn rejects_substring_only_argument() {
        // `claude` as a non-basename substring is not a match.
        assert!(!is_claude(Some("mycmd"), &argv(&["mycmd", "--dir=/claude-tools"])));
    }

    #[test]
    fn rejects_non_agent_command_referencing_a_claude_path() {
        // The wrapper rule requires a JS-runtime argv[0], not any claude path.
        assert!(!is_claude(
            Some("tail"),
            &argv(&["tail", "-f", "/home/u/.claude/debug.log"])
        ));
        // A node process whose argv only references a claude *directory* (not an
        // entrypoint named claude / cli.js) does not match.
        assert!(!is_claude(
            Some("node"),
            &argv(&["node", "/home/u/.claude/notes/build.js"])
        ));
    }

    #[test]
    fn rejects_empty() {
        assert!(!is_claude(None, &argv(&[])));
        assert!(!is_claude(Some("node"), &argv(&["node"])));
    }

    #[test]
    fn proc_readers_handle_self() {
        // Thin-wrapper sanity: our own /proc is readable and non-empty.
        let pid = std::process::id();
        assert!(proc_comm(pid).is_some());
        assert!(!proc_cmdline(pid).is_empty());
    }

    // ---- count_background_task_groups (U1, R2) -----------------------------
    // Synthetic process tables. The agent root is pid 100, pgid 100 (it is the
    // foreground process-group leader, so its own pid == its pgid). A live
    // background job is a descendant whose pgid != 100.

    /// Build a `ProcEntry`; `comm` is irrelevant to the v1 count (carried only for
    /// the KTD6 fallback), so a placeholder keeps the cases readable.
    fn proc(pid: u32, ppid: u32, pgid: u32, state: char) -> ProcEntry {
        ProcEntry { pid, ppid, pgid, state, comm: "x".into() }
    }

    #[test]
    fn counts_two_distinct_background_groups() {
        // A watcher subtree (pgrp 200) + a 3-process pipeline (pgrp 300) → 2 tasks.
        let table = vec![
            proc(100, 1, 100, 'R'),   // claude (root); pgid == root → excluded
            proc(200, 100, 200, 'S'), // watcher leader (own group)
            proc(201, 200, 200, 'S'), // its child (same group)
            proc(300, 100, 300, 'S'), // pipeline a | b | c — one group, three procs
            proc(301, 100, 300, 'S'),
            proc(302, 100, 300, 'S'),
        ];
        assert_eq!(count_background_task_groups(&table, 100), 2);
    }

    #[test]
    fn a_pipeline_sharing_one_pgid_is_one_task() {
        // `a | b | c &` → three processes, one process group → 1, not 3. (AE5)
        let table = vec![
            proc(100, 1, 100, 'R'),
            proc(300, 100, 300, 'S'),
            proc(301, 100, 300, 'S'),
            proc(302, 100, 300, 'S'),
        ];
        assert_eq!(count_background_task_groups(&table, 100), 1);
    }

    #[test]
    fn a_background_subtree_of_same_pgid_is_one_task() {
        // `npm run dev &` and its child subtree — many procs, one group → 1. (AE5)
        let table = vec![
            proc(100, 1, 100, 'R'),
            proc(400, 100, 400, 'S'), // job leader
            proc(401, 400, 400, 'S'),
            proc(402, 401, 400, 'S'), // grandchild, still same group
            proc(403, 400, 400, 'S'),
        ];
        assert_eq!(count_background_task_groups(&table, 100), 1);
    }

    #[test]
    fn a_claude_wrapper_job_spanning_two_nested_groups_is_one_task() {
        // Empirically, one Claude Code background job is a `bash -c` wrapper in its
        // own group (a direct child of the agent) plus the real command, which
        // re-forks into a *further* group below the wrapper. Both are backgrounded
        // (pgid != root), but only the wrapper is anchored at the agent — the inner
        // group folds in, so the job counts once, not twice. (Regression: a single
        // `ping &` was read as "2 tasks".)
        let table = vec![
            proc(100, 1, 100, 'R'),   // claude (root)
            proc(110, 100, 110, 'S'), // bash -c wrapper: agent's child, own group
            proc(120, 110, 120, 'S'), // ping: child of the wrapper, its own group
        ];
        assert_eq!(count_background_task_groups(&table, 100), 1);
    }

    #[test]
    fn two_wrapper_jobs_each_spanning_nested_groups_are_two_tasks() {
        // Two independent background jobs, each a wrapper+command nested pair → 2,
        // not 4: each wrapper anchors one task; each inner command group folds in.
        let table = vec![
            proc(100, 1, 100, 'R'),
            proc(110, 100, 110, 'S'), // job A wrapper
            proc(120, 110, 120, 'S'), // job A command (nested)
            proc(210, 100, 210, 'S'), // job B wrapper
            proc(220, 210, 220, 'S'), // job B command (nested)
        ];
        assert_eq!(count_background_task_groups(&table, 100), 2);
    }

    #[test]
    fn foreground_children_sharing_the_agent_group_are_not_counted() {
        // Same-pgid descendants are the agent's own foreground work → 0.
        let table = vec![
            proc(100, 1, 100, 'R'),
            proc(101, 100, 100, 'S'),
            proc(102, 101, 100, 'S'),
        ];
        assert_eq!(count_background_task_groups(&table, 100), 0);
    }

    #[test]
    fn a_zombie_background_descendant_is_not_counted() {
        // A finished-but-unreaped job whose only proc is a zombie reads idle. (AE8)
        let table = vec![
            proc(100, 1, 100, 'R'),
            proc(500, 100, 500, 'Z'), // zombie in its own group
        ];
        assert_eq!(count_background_task_groups(&table, 100), 0);
        // 'X' (dead) is excluded too.
        let table = vec![proc(100, 1, 100, 'R'), proc(500, 100, 500, 'X')];
        assert_eq!(count_background_task_groups(&table, 100), 0);
    }

    #[test]
    fn a_live_group_is_counted_even_when_a_sibling_is_zombie() {
        // One zombie group + one live group → 1 (only the live one). (AE8)
        let table = vec![
            proc(100, 1, 100, 'R'),
            proc(500, 100, 500, 'Z'), // zombie group → ignored
            proc(600, 100, 600, 'S'), // live group → counted
        ];
        assert_eq!(count_background_task_groups(&table, 100), 1);
    }

    #[test]
    fn a_reparented_descendant_is_not_counted() {
        // A double-forked task reparents to pid 1, leaving the agent's subtree →
        // undercount by design (KTD3): it is not reachable from the root.
        let table = vec![
            proc(100, 1, 100, 'R'),
            proc(600, 1, 600, 'S'), // orphan, ppid == 1, not under root
        ];
        assert_eq!(count_background_task_groups(&table, 100), 0);
    }

    #[test]
    fn a_background_group_two_hops_below_the_root_is_found() {
        // Transitive: an intermediate child shares the agent group; the background
        // leader two hops down (own group) is still reached and counted once.
        let table = vec![
            proc(100, 1, 100, 'R'),
            proc(700, 100, 100, 'S'), // intermediate, same group as agent
            proc(701, 700, 701, 'S'), // background leader, two hops below root
            proc(702, 701, 701, 'S'), // its child, same background group
        ];
        assert_eq!(count_background_task_groups(&table, 100), 1);
    }

    #[test]
    fn empty_table_or_no_descendants_is_zero() {
        assert_eq!(count_background_task_groups(&[], 100), 0);
        assert_eq!(count_background_task_groups(&[proc(100, 1, 100, 'R')], 100), 0);
    }

    #[test]
    fn malformed_table_does_not_infinite_loop() {
        // Self-parent root (100 is its own parent) + a mutual ppid cycle (300↔400)
        // unreachable from root. Without a visited set the self-loop would re-enqueue
        // 100 forever; the test completing at all proves termination, and the real
        // background group (200) is still counted.
        let table = vec![
            proc(100, 100, 100, 'R'), // self-parent
            proc(200, 100, 200, 'S'), // a real background group
            proc(300, 400, 300, 'S'), // X: parent is Y
            proc(400, 300, 400, 'S'), // Y: parent is X (cycle, not under root)
        ];
        assert_eq!(count_background_task_groups(&table, 100), 1);
    }

    // ---- read_proc_table / parse_stat_line (U2, R3) ------------------------

    #[test]
    fn read_proc_table_includes_self() {
        // Thin-I/O sanity: our own process is in the table, live, with a real
        // parent and group. (The count logic itself is U1's synthetic-table tests.)
        let me = std::process::id();
        let table = read_proc_table();
        let mine = table
            .iter()
            .find(|e| e.pid == me)
            .expect("self should appear in the /proc table");
        assert!(mine.ppid >= 1, "self has a real parent pid");
        assert!(mine.pgid >= 1, "self has a real process group");
        assert!(
            !matches!(mine.state, 'Z' | 'X'),
            "self is live, got state {:?}",
            mine.state
        );
    }

    #[test]
    fn parse_stat_line_parses_a_plain_line() {
        let e = parse_stat_line("1 (systemd) S 0 1 1 0 -1 4194560 1234").unwrap();
        assert_eq!(e.pid, 1);
        assert_eq!(e.comm, "systemd");
        assert_eq!(e.state, 'S');
        assert_eq!(e.ppid, 0);
        assert_eq!(e.pgid, 1);
    }

    #[test]
    fn parse_stat_line_handles_spaces_and_parens_in_comm() {
        // The real risk: comm with spaces AND nested parens. Splitting on the last
        // ')' keeps state/ppid/pgrp correct where a naive first-')' split would not.
        let e = parse_stat_line("4242 (weird (proc) name) R 4200 4242 4242 0 -1 0")
            .expect("parses despite parens in comm");
        assert_eq!(e.pid, 4242);
        assert_eq!(e.comm, "weird (proc) name");
        assert_eq!(e.state, 'R');
        assert_eq!(e.ppid, 4200);
        assert_eq!(e.pgid, 4242);
    }

    #[test]
    fn parse_stat_line_rejects_malformed_lines() {
        assert!(parse_stat_line("").is_none()); // empty
        assert!(parse_stat_line("no parens here").is_none()); // no comm parens
        assert!(parse_stat_line("123 (only-comm)").is_none()); // truncated: no fields
        assert!(parse_stat_line("123 (c) S 456").is_none()); // missing pgrp
        assert!(parse_stat_line("abc (c) S 1 1").is_none()); // non-numeric pid
        assert!(parse_stat_line("123 (c) S xx 1").is_none()); // non-numeric ppid
    }
}
