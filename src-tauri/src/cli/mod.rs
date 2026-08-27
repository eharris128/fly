//! The `fly` CLI (KTD12): subcommands share the app binary so agents and
//! scripts invoke one tool and onboarding can run setup. v1 ships `notify`,
//! `hooks setup` / `hooks teardown`, and `automation …` (U9).
//!
//! `fly resume` is deliberately **not** a CLI subcommand: it launches the
//! desktop app (in resume mode) rather than running and exiting, so it falls
//! through this check to `lib.rs::run`, which execs the installed Electron
//! shell (KTD-B; 2026-08-27-001 KTD7). Usage:
//!   - `fly`           — launch normally (fresh shells)
//!   - `fly resume`    — launch and re-attach detected Claude agents
//!   - `fly notify …`  — report attention (used by the Claude hook)
//!   - `fly hooks …`   — install/remove the Claude hook
//!   - `fly --version` — print the version

pub mod automation;
pub mod core;
pub mod substrate;
pub mod hooks;
pub mod notify;
pub mod peer;

/// Whether the first argument selects a CLI subcommand (so the binary runs as
/// the `fly` CLI rather than launching the desktop app). `resume` is excluded on
/// purpose — it is a launch mode (KTD-B), handled in `lib.rs::run`.
///
/// `help`/`--help`/`-h` are included so a bare `fly --help` prints the CLI
/// overview and exits instead of launching the desktop app — the affordance an
/// agent reaches for first when it wants to discover subcommands like
/// `automation`.
pub fn is_cli_subcommand(arg: &str) -> bool {
    matches!(
        arg,
        "notify"
            | "hooks"
            | "automation"
            | "agents"
            | "send"
            | "substrate-event"
            | "core"
            | "help"
            | "--help"
            | "-h"
            | "version"
            | "--version"
            | "-V"
    )
}

/// The top-level `fly --help` overview: what the binary is, how to launch it,
/// and the CLI subcommands an agent or script can call. Kept terse and
/// example-forward so it's usable straight from a pane.
pub fn top_level_help() -> String {
    "fly — a terminal for AI coding agents.\n\
     \n\
     Running bare `fly` launches the desktop app (the Electron shell installed\n\
     beside this binary); the subcommands below run as a CLI and exit. Inside a\n\
     fly pane they talk to the running app over its authenticated socket.\n\
     \n\
     Usage:\n  \
       fly                      launch the desktop app (fresh shells)\n  \
       fly resume               launch and re-attach detected Claude agents\n  \
       fly notify …             report agent attention (used by the Claude hook)\n  \
       fly hooks <setup|teardown> [--agent claude]\n                           \
                                install or remove fly's Claude Code hooks\n  \
       fly automation <cmd> …   manage cron-scheduled agent/script runs\n  \
       fly agents [--json]      list the live agent roster (peer messaging)\n  \
       fly send <pane> <msg…>   deliver a message into another agent's pane\n  \
       fly core [--socket <p>]  run the headless backend host (internal)\n  \
       fly help | --help | -h   show this help\n  \
       fly --version | -V       print the version\n\
     \n\
     `fly agents` / `fly send` run inside a fly pane and talk over its\n\
     authenticated socket; a target pane receives only after the human toggles\n\
     'peers' on its dashboard row (off by default, every launch).\n\
     \n\
     Automation subcommands:\n  \
       fly automation list                     list automations\n  \
       fly automation show <id>                show one automation\n  \
       fly automation runs <id>                show an automation's run history\n  \
       fly automation create [options]         create one (see `create --help`)\n  \
       fly automation update <id> [options]    patch one in place (see `update --help`)\n  \
       fly automation pause|resume|run|delete <id>\n\
     \n\
     Read-only automation commands work anywhere; creating or mutating one must\n\
     run inside a fly pane. Run any subcommand with `--help` for its own usage.\n"
        .to_string()
}

/// Dispatch a CLI invocation. `args` is the full process argv. Returns the
/// process exit code.
pub fn run(args: &[String]) -> i32 {
    match args.get(1).map(String::as_str) {
        Some("help") | Some("--help") | Some("-h") | None => {
            print!("{}", top_level_help());
            0
        }
        // R6 (2026-08-27-001): the crate version — the same string `core/ping`
        // reports; the lockstep test keeps it equal to the package versions.
        Some("version") | Some("--version") | Some("-V") => {
            println!("fly {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Some("notify") => notify::run(&args[2..]),
        Some("automation") => automation::run(&args[2..]),
        Some("agents") => peer::run_agents(&args[2..]),
        // tmux plan U4b/KTD12: fired by tmux run-shell hooks, never humans —
        // deliberately absent from the help text. Always exits 0.
        Some("substrate-event") => {
            substrate::run(&args[2..]);
            0
        }
        Some("send") => peer::run_send(&args[2..]),
        // Electron-shell migration U1: the headless backend host — launched
        // by a display shell, listed in help only in passing.
        Some("core") => core::run(&args[2..]),
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
            eprintln!("run `fly --help` for usage");
            2
        }
    }
}
