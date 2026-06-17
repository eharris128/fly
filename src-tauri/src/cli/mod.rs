//! The `fly` CLI (KTD12): subcommands share the app binary so agents and
//! scripts invoke one tool and onboarding can run setup. v1 ships `notify`
//! and `hooks setup` / `hooks teardown`.

pub mod hooks;
pub mod notify;

/// Whether the first argument selects a CLI subcommand (so the binary runs as
/// the `fly` CLI rather than launching the desktop app).
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
