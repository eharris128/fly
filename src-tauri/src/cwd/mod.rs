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
}
