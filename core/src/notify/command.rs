//! Opt-in notification command runner (KTD17, U19).
//!
//! Runs a user-configured shell command on each *surfaced* notification, fed
//! sanitized title/subtitle/body as **environment** values. It is off unless
//! configured, never reachable by the agent or any socket peer, non-blocking,
//! concurrency-bounded, and reaped — a misbehaving or looping agent cannot use
//! it to fan out unbounded processes or block the dispatch path.
//!
//! Three hardening points the sanitization alone does **not** cover:
//!
//! - `sanitize` strips control chars; it does **not** shell-escape. Title/body
//!   originate from agent output and can contain `$()`, backticks, `;`, `|`.
//!   They are safe only because fly passes them **exclusively as env values**
//!   (never interpolated into the command string), inert as long as the user's
//!   command references `"$FLY_NOTIFICATION_*"` *inside double quotes*. Unquoted
//!   use re-exposes them to word-splitting/globbing (documented for the config
//!   field).
//! - The child env is built from an **allowlist**, not inheritance. A bare
//!   `Command` inherits the app-process env; `FLY_PANE_TOKEN` / `FLY_SOCKET_PATH`
//!   are not in it today (injected only into PTY children), but `env_clear` + a
//!   fixed allowlist pins "neither token reaches the command" so a future
//!   refactor cannot silently arm the leak.
//! - Concurrency is capped, and the child is reaped via `Child::wait` on a
//!   short-lived thread (not process-global `SIGCHLD`, which a host may own).

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;

use super::{sanitize_body, sanitize_title};

/// Max concurrent in-flight notification commands. Extra invocations are
/// dropped, so a looping agent cannot fan out unbounded processes/threads.
const MAX_INFLIGHT: usize = 4;

/// Env vars allowed through to the child, in addition to the three
/// `FLY_NOTIFICATION_*` values. Everything else — including any app-process
/// secret — is cleared.
const ENV_ALLOWLIST: [&str; 6] = ["PATH", "HOME", "USER", "LANG", "LC_ALL", "LC_CTYPE"];

fn global_inflight() -> Arc<AtomicUsize> {
    static G: OnceLock<Arc<AtomicUsize>> = OnceLock::new();
    G.get_or_init(|| Arc::new(AtomicUsize::new(0))).clone()
}

/// Build the (un-spawned) child command: `sh -c <command>` with a cleared env
/// re-populated from the allowlist plus the three sanitized `FLY_NOTIFICATION_*`
/// values. Factored out so the env construction is unit-tested without spawning.
fn build_command(command: &str, title: &str, subtitle: &str, body: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);
    // Allowlist, not inheritance (see module docs).
    cmd.env_clear();
    for key in ENV_ALLOWLIST {
        if let Some(val) = std::env::var_os(key) {
            cmd.env(key, val);
        }
    }
    cmd.env("FLY_NOTIFICATION_TITLE", sanitize_title(title))
        .env("FLY_NOTIFICATION_SUBTITLE", sanitize_title(subtitle))
        .env("FLY_NOTIFICATION_BODY", sanitize_body(body));
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

/// Run the user's notification command, non-blocking and best-effort (KTD17).
/// Returns the reaper thread's handle (so tests can await completion); the
/// dispatch path ignores it.
pub fn run(command: &str, title: &str, subtitle: &str, body: &str) -> Option<JoinHandle<()>> {
    run_inner(command, title, subtitle, body, global_inflight(), MAX_INFLIGHT)
}

fn run_inner(
    command: &str,
    title: &str,
    subtitle: &str,
    body: &str,
    inflight: Arc<AtomicUsize>,
    cap: usize,
) -> Option<JoinHandle<()>> {
    if command.trim().is_empty() {
        return None;
    }
    // Reserve a concurrency slot, or drop.
    if inflight.fetch_add(1, Ordering::SeqCst) >= cap {
        inflight.fetch_sub(1, Ordering::SeqCst);
        return None;
    }

    let mut cmd = build_command(command, title, subtitle, body);
    match cmd.spawn() {
        Ok(mut child) => {
            let counter = Arc::clone(&inflight);
            match std::thread::Builder::new()
                .name("fly-notify-cmd".into())
                .spawn(move || {
                    let _ = child.wait();
                    counter.fetch_sub(1, Ordering::SeqCst);
                }) {
                Ok(handle) => Some(handle),
                Err(_) => {
                    // The reaper didn't start; release the slot (rare).
                    inflight.fetch_sub(1, Ordering::SeqCst);
                    None
                }
            }
        }
        Err(_) => {
            // Spawn failure (e.g. no `sh`) is a no-op; release the slot.
            inflight.fetch_sub(1, Ordering::SeqCst);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counter() -> Arc<AtomicUsize> {
        Arc::new(AtomicUsize::new(0))
    }

    /// Run a command that writes to `path`, await it, return the file contents.
    fn run_to_file(
        command_to: impl Fn(&str) -> String,
        title: &str,
        subtitle: &str,
        body: &str,
    ) -> String {
        let out = tempfile::NamedTempFile::new().unwrap();
        let path = out.path().to_str().unwrap().to_string();
        let handle = run_inner(&command_to(&path), title, subtitle, body, counter(), MAX_INFLIGHT)
            .expect("command should spawn");
        handle.join().unwrap();
        std::fs::read_to_string(&path).unwrap()
    }

    #[test]
    fn env_vars_are_set_and_sanitized() {
        // Control chars in the title are stripped before they reach the env.
        let contents = run_to_file(
            |p| format!(
                "printf '%s|%s|%s' \"$FLY_NOTIFICATION_TITLE\" \"$FLY_NOTIFICATION_SUBTITLE\" \"$FLY_NOTIFICATION_BODY\" > '{p}'"
            ),
            "ti\x07tle",
            "subtitle",
            "bo\ndy",
        );
        assert_eq!(contents, "title|subtitle|body");
    }

    #[test]
    fn title_is_length_capped() {
        let contents = run_to_file(
            |p| format!("printf '%s' \"$FLY_NOTIFICATION_TITLE\" > '{p}'"),
            &"x".repeat(500),
            "s",
            "b",
        );
        assert_eq!(contents.len(), 120, "title capped at TITLE_CAP");
    }

    #[test]
    fn pane_token_and_socket_never_reach_the_command() {
        // Simulate a future where the app process carries the pane token: the
        // command must still not see it (env_clear + allowlist). Guards a
        // refactor that might put the token in the app env.
        // SAFETY: unsynchronized against other test threads reading the env —
        // the pre-2024 race made explicit, not a new one. Test-only; the child's
        // env_clear is what the assertion checks, so a concurrent writer can't flip it.
        unsafe { std::env::set_var("FLY_PANE_TOKEN", "super-secret-token") };
        unsafe { std::env::set_var("FLY_SOCKET_PATH", "/run/fly/hook.sock") };
        let contents = run_to_file(
            |p| format!(
                "printf 'T=%s S=%s' \"${{FLY_PANE_TOKEN:-none}}\" \"${{FLY_SOCKET_PATH:-none}}\" > '{p}'"
            ),
            "t",
            "s",
            "b",
        );
        unsafe { std::env::remove_var("FLY_PANE_TOKEN") };
        unsafe { std::env::remove_var("FLY_SOCKET_PATH") };
        assert_eq!(contents, "T=none S=none", "tokens were cleared from the child env");
    }

    #[test]
    fn agent_metacharacters_in_the_body_are_inert() {
        // A quoted reference passes `$(...)` through literally — it must not
        // execute. Use a unique sentinel path the body would create if it ran.
        let pwned = std::env::temp_dir().join("fly_pwned_u19_marker");
        let _ = std::fs::remove_file(&pwned);
        let body = format!("$(touch '{}')", pwned.display());
        let contents = run_to_file(
            |p| format!("printf '%s' \"$FLY_NOTIFICATION_BODY\" > '{p}'"),
            "t",
            "s",
            &body,
        );
        assert_eq!(contents, body, "the body is passed through literally");
        assert!(!pwned.exists(), "command substitution must not have executed");
    }

    #[test]
    fn concurrency_is_bounded_and_children_are_reaped() {
        let inflight = counter();
        let mut handles = Vec::new();
        for _ in 0..20 {
            if let Some(h) = run_inner("sleep 0.3", "t", "s", "b", Arc::clone(&inflight), MAX_INFLIGHT) {
                handles.push(h);
            }
        }
        assert!(
            handles.len() <= MAX_INFLIGHT,
            "in-flight commands capped at {MAX_INFLIGHT}, spawned {}",
            handles.len()
        );
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            inflight.load(Ordering::SeqCst),
            0,
            "every slot released — no leak, no defunct children"
        );
    }

    #[test]
    fn empty_command_is_a_noop() {
        assert!(run_inner("   ", "t", "s", "b", counter(), MAX_INFLIGHT).is_none());
    }

    #[test]
    fn nonexistent_command_does_not_panic() {
        // `sh -c <missing>` spawns sh (which exits 127); our spawn succeeds and
        // reaps cleanly — a failing user command never crashes dispatch.
        let h = run_inner(
            "this_command_xyz_does_not_exist",
            "t",
            "s",
            "b",
            counter(),
            MAX_INFLIGHT,
        );
        if let Some(h) = h {
            h.join().unwrap();
        }
    }
}
