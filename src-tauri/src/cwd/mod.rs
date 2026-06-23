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
/// (walked via `ppid` edges) that are both:
///   - **live** — state is not `Z` (zombie) or `X` (dead); and
///   - **backgrounded** — `pgid != root_pid`.
///
/// Backgrounding *is* "a descendant in a different process group", so the pgid
/// filter excludes the agent's own foreground children/same-group helpers while
/// distinct-pgid collapses a pipeline (N procs, one group) or a job's subtree to a
/// single logical task — matching the user's mental model and Claude Code's
/// "N shells still running" line.
///
/// Pure over its argument table (no `/proc` I/O — that is [`read_proc_table`]),
/// mirroring [`is_claude`]. A reparented (double-forked) task reparents to pid 1,
/// escaping the descendant walk, and is undercounted — accepted (KTD3). A
/// background job that itself calls `setsid` re-inflates the count — accepted
/// (KTD2). Traversal carries a visited set, so a malformed table (self-parent,
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

    // Distinct process groups among the live, backgrounded descendants.
    let mut groups: HashSet<u32> = HashSet::new();
    for pid in &descendants {
        if let Some(e) = by_pid.get(pid) {
            if e.state != 'Z' && e.state != 'X' && e.pgid != root_pid {
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
}
