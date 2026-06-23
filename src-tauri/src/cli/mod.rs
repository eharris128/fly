//! The `fly` CLI (KTD12): subcommands share the app binary so agents and
//! scripts invoke one tool and onboarding can run setup. v1 ships `notify`
//! and `hooks setup` / `hooks teardown`.
//!
//! `fly resume` is deliberately **not** a CLI subcommand: it launches the
//! desktop window (in resume mode) rather than running and exiting, so it falls
//! through this check to `lib.rs::run` (KTD-B). Usage:
//!   - `fly`           — launch normally (fresh shells)
//!   - `fly resume`    — launch and re-attach detected Claude agents
//!   - `fly notify …`  — report attention (used by the Claude hook)
//!   - `fly hooks …`   — install/remove the Claude hook

pub mod hooks;
pub mod notify;

/// Whether the first argument selects a CLI subcommand (so the binary runs as
/// the `fly` CLI rather than launching the desktop app). `resume` is excluded on
/// purpose — it is a launch mode (KTD-B), handled in `lib.rs::run`.
pub fn is_cli_subcommand(arg: &str) -> bool {
    matches!(arg, "notify" | "hooks")
}

/// Dispatch a CLI invocation. `args` is the full process argv. Returns the
/// process exit code.
pub fn run(args: &[String]) -> i32 {
    match args.get(1).map(String::as_str) {
        Some("notify") => notify::run(&args[2..]),
        Some("hooks") => match args.get(2).map(String::as_str) {
            Some("setup") => hooks::run_setup(&args[3..]),
            Some("teardown") => hooks::run_teardown(&args[3..]),
            _ => {
                eprintln!("usage: fly hooks <setup|teardown> [--agent claude]");
                2
            }
        },
        other => {
            eprintln!("fly: unknown command {:?}", other.unwrap_or(""));
            2
        }
    }
}
