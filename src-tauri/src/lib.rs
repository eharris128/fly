//! fly — a terminal for AI coding agents.
//!
//! This library backs the `fly` CLI subcommands (KTD12) and the `fly core`
//! headless backend the Electron shell drives; `main.rs` is a thin shim over
//! [`run`].

pub mod automations;
pub mod backend;
pub mod cli;
pub mod config;
pub mod control;
pub mod cwd;
pub mod feed;
pub mod hooks;
pub mod notify;
pub mod peer;
pub mod pty;
pub mod session;
pub mod state;
pub mod stream;
pub mod substrate;
pub mod usage;

use std::path::PathBuf;

use state::attention::{Reason, Signal, Tier};
use state::AttentionManager;

/// The per-flavor directory name used under the XDG config/data/runtime dirs.
///
/// Defaults to `fly`, but `FLY_APP_NAME` overrides it so a dev build can run
/// alongside an installed release without sharing settings, session state, or
/// the hook socket. The value is sanitized to a single safe path segment (it is
/// joined into XDG paths), falling back to `fly` if empty.
pub fn app_dir_name() -> String {
    let cleaned: String = std::env::var("FLY_APP_NAME")
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if cleaned.is_empty() {
        "fly".into()
    } else {
        cleaned
    }
}

/// Where the hook socket lives — under the XDG runtime dir at a **stable,
/// per-flavor path** (tmux-substrate plan U2/KTD8). Stability is load-bearing:
/// substrate sessions outlive the fly process, and their agents' hooks hold
/// `FLY_SOCKET_PATH` in long-lived process env — a PID-keyed path (the
/// pre-substrate scheme) would strand every surviving agent on restart.
/// Same-flavor duplicate instances are prevented by the shell's single-
/// instance lock and, independently, by the bind path refusing to unlink a
/// socket that still answers (see `HookServer` — the ga-h9z lesson applied
/// to our own socket).
pub(crate) fn hook_socket_path() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join(app_dir_name()).join("hook.sock")
}

/// U6 (R18): ring a pane through the attention pipeline for an automation
/// alert — the same seam the hook dispatch uses (`Signal { reason, tier }` →
/// the event sink), so an alert surfaces exactly like an agent raise. The
/// attention manager's lock is independent of the automations store lock
/// (KTD-B), so this is safe to call from the reaper thread's sink closure.
/// The control registry's `register_alert_sink` drains the pending backlog
/// through here.
pub(crate) fn raise_alert_with(
    events: &stream::EventSink,
    attention: &AttentionManager,
    pane_id: u64,
) {
    let pane = pty::PaneId(pane_id);
    if let Some(outcome) = attention.signal(
        pane,
        Signal {
            reason: Reason::Alert,
            tier: Tier::Cli,
        },
    ) {
        events(
            stream::PANE_ATTENTION_EVENT,
            stream::attention_event_payload(pane, &outcome),
        );
    }
}

/// How the app was launched (U7, KTD-B/G), read once by the frontend at restore.
/// `resume` is an app *launch mode*, not a CLI subcommand: it falls through the
/// `is_cli_subcommand` check and launches the app like a bare `fly`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LaunchMode {
    /// Bare `fly` after a clean exit — fresh shells, inert scrollback (R1).
    Normal,
    /// Explicit `fly resume` — re-attach detected agents directly.
    Resume,
    /// The previous run crashed (clean-exit marker absent) — offer to resume,
    /// never silently auto-run (KTD-G preserves KTD10's consent principle).
    Offer,
}

/// The pure launch-mode decision (testable). Explicit `fly resume` always
/// resumes; otherwise a missing marker (a prior crash) offers, and a present
/// marker (a prior clean exit) is normal.
fn decide_launch_mode(resume_requested: bool, prev_clean: bool) -> LaunchMode {
    if resume_requested {
        LaunchMode::Resume
    } else if !prev_clean {
        LaunchMode::Offer
    } else {
        LaunchMode::Normal
    }
}

/// Resolve the launch mode from argv + the clean-exit marker, and **clear the
/// marker** so an unclean exit of *this* run is detectable next launch (KTD-G).
/// `pub(crate)`: `fly core` resolves the same way at boot (U3) — whichever
/// role owns the backend consumes the marker.
pub(crate) fn resolve_launch_mode(args: &[String]) -> LaunchMode {
    let resume_requested = args.get(1).map(|s| s == "resume").unwrap_or(false);
    let marker = session::resume::clean_exit_path();
    let prev_clean = session::resume::took_clean_exit_at(&marker);
    let _ = session::resume::set_clean_exit_at(&marker, false);
    decide_launch_mode(resume_requested, prev_clean)
}

/// Run a `fly` CLI subcommand if argv selects one (KTD12); otherwise this is
/// a bare `fly` / `fly resume` — the desktop launch, which the Electron shell
/// owns (2026-08-27-001 KTD7: U3 wires the exec into the installed shell).
/// Until then a bare `fly` prints the overview and exits 2 rather than
/// pretending to open a window.
pub fn run() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(first) = args.get(1) {
        if cli::is_cli_subcommand(first) {
            std::process::exit(cli::run(&args));
        }
    }
    eprintln!("{}", cli::top_level_help());
    eprintln!("fly: the desktop app is launched by the Electron shell (fly-shell); this binary serves the CLI and `fly core`.");
    std::process::exit(2);
}

/// monitor-handoff U4 (R11): flatten a qualified [`session::handoff::HandoffTarget`]
/// into the [`automations::model::MonitorPointers`] stored on a monitor. The
/// pointer cwd is the **record's** captured cwd when it has one (the same
/// R12-precedence the transcript path was derived under), else the live-cwd
/// fallback the derivation actually used — so the stored cwd and transcript
/// path always cohere. No target, or no cwd from anywhere, abstains to `None`
/// (→ the R12 refusal). Pure, so it is unit-tested below without an app.
pub(crate) fn monitor_pointers_from_target(
    target: Option<session::handoff::HandoffTarget>,
    live_cwd: Option<&str>,
) -> Option<automations::model::MonitorPointers> {
    let t = target?;
    let session_cwd = t
        .session_cwd
        .clone()
        .or_else(|| live_cwd.map(str::to_string))?;
    Some(automations::model::MonitorPointers {
        session_id: t.session_id,
        transcript_path: t.transcript_path,
        session_cwd,
    })
}

/// Route a parsed `automation/*` request to the manager (U9). AppHandle-free so
/// it is directly testable: the caller supplies the validated `pane_id`, the
/// pane's `workspace_id` (for origin stamping, R9), whether the pane is
/// itself automation-spawned (`is_recursion`, the R22 gate), and the
/// pickup-pointer resolver for monitor creates (monitor-handoff U4, R11 —
/// the wrapper above wires the pane-precise attribution/handoff resolution;
/// tests wire a stub). Enforces the gate first, then routes
/// create/pause/resume/run/delete to the manager.
pub fn dispatch_automation_op(
    mgr: &automations::AutomationManager,
    pane_id: u64,
    workspace_id: &str,
    is_recursion: bool,
    req: cli::automation::AutomationRequest,
    resolve_pointers: &dyn Fn() -> Option<automations::model::MonitorPointers>,
) -> cli::automation::AutomationResponse {
    use automations::{CreateMode, CreateSpec, ManualRun};
    use cli::automation::AutomationResponse;

    // R22 recursion gate: a pane spawned by an automation may not create or
    // manage automations (the registry entry outlives a delete, cleared only on
    // the pane's exit, so create→delete can't un-gate a still-live pane).
    // Checked before any monitor pointer resolution (monitor-handoff U4): a
    // gated pane's create never touches the resume store or a transcript.
    if is_recursion {
        return AutomationResponse::err(
            "automations cannot be managed from an automation-spawned pane",
        );
    }

    match req.op.as_str() {
        "automation/create" => {
            let (Some(name), Some(cron), Some(timezone), Some(cwd)) =
                (req.name, req.cron, req.timezone, req.cwd)
            else {
                return AutomationResponse::err("create requires name, cron, timezone, and cwd");
            };
            // monitor-handoff R1: a monitor is an *agent-mode* automation.
            // The CLI rejects `--monitor --script` too (U5), but the socket
            // payload is untrusted — enforce it here as well.
            let monitor = req.monitor;
            let mode = if let Some(prompt) = req.prompt {
                // monitor-handoff R8 (fix(review) #12): the sonnet/xhigh
                // monitor default is stamped CLI-side
                // (`cli::automation::monitor_launch_defaults`) so `--json`
                // output and the local echo self-describe before the
                // round-trip — but the socket payload is the untrusted
                // boundary, and a raw-socket monitor create must not
                // silently ride `config.automation_defaults`. Backstop the
                // same per-field default here (explicit values still win);
                // the double stamp is deliberate defense in depth, mirroring
                // the R9 retry-on-interrupt default below. Non-monitor
                // creates pass through untouched.
                let (model, effort) =
                    cli::automation::monitor_launch_defaults(monitor, req.model, req.effort);
                // Headless-agent-automations U2 (R2/R3): the wire is
                // untrusted — re-reject the combinations the CLI already
                // refuses. `--headless` with `--monitor` is redundant (a
                // monitor is unconditionally headless), `--paned` with
                // `--monitor` contradicts it; a plain create passes the
                // tri-state through (None = follow the config default).
                if monitor && req.headless.is_some() {
                    return AutomationResponse::err(
                        "a monitor is always headless — drop --headless/--paned",
                    );
                }
                CreateMode::Agent {
                    prompt,
                    model,
                    effort,
                    headless: req.headless,
                }
            } else if let Some(content) = req.script {
                if monitor {
                    return AutomationResponse::err(
                        "a monitor must be agent-mode (a prompt, not a script)",
                    );
                }
                // Headless-agent-automations U2: agent-only, enforced on the
                // untrusted wire like the CLI-side rejection.
                if req.headless.is_some() {
                    return AutomationResponse::err(
                        "--headless/--paned are agent-mode only (a prompt, not a script)",
                    );
                }
                // fly-dag-primitives G1: verdict gating is agent-only (a
                // script has no assistant turn to parse a fenced verdict from)
                // — re-reject on the untrusted wire like the checks above.
                if req.verdict_gated {
                    return AutomationResponse::err(
                        "--verdict-gated is agent-mode only (a prompt, not a script)",
                    );
                }
                // A timeout the dispatcher will never honour must be REFUSED,
                // not quietly stored. `clamp_timeout_ms` bounds the stored
                // value at run time (the store is same-UID-writable), so
                // without this check `create` accepts e.g. 75 min, the store
                // reports 75 min and `show` prints 75 min while every run is
                // killed at the ceiling — three surfaces agreeing on a number
                // that is not the one enforced. That cost a real debugging
                // session on 2026-08-07: a scheduled job was "fixed" by
                // raising its timeout, verified on all three surfaces, and
                // went on dying at the old limit.
                let timeout_ms = req.timeout_ms.unwrap_or(automations::script::TIMEOUT_DEFAULT_MS);
                if timeout_ms > automations::script::TIMEOUT_MAX_MS {
                    return AutomationResponse::err(format!(
                        "--timeout {}ms exceeds the maximum {}ms ({} min); \
                         it would be silently clamped at run time",
                        timeout_ms,
                        automations::script::TIMEOUT_MAX_MS,
                        automations::script::TIMEOUT_MAX_MS / 60_000,
                    ));
                }
                CreateMode::Script {
                    content,
                    interpreter: req.interpreter.unwrap_or_else(|| "bash".to_string()),
                    timeout_ms,
                }
            } else {
                return AutomationResponse::err("create requires a prompt or a script");
            };
            // monitor-handoff U4 (R11/R12): a monitor create captures its
            // pickup pointers from the registering pane NOW — the parent tab
            // is about to close — or refuses with the distinct error and
            // stores NOTHING. Resolution is attempted only for monitor
            // creates; the non-monitor path is untouched.
            let pickup_pointers = if monitor {
                match resolve_pointers() {
                    Some(p) => Some(p),
                    None => return AutomationResponse::err(automations::ERR_MONITOR_POINTERS),
                }
            } else {
                None
            };
            let origin = automations::model::Origin {
                pane_id,
                workspace_id: workspace_id.to_string(),
                label: "cli".to_string(),
            };
            match mgr.create(CreateSpec {
                name,
                cron,
                timezone,
                cwd,
                mode,
                // monitor-handoff R9: monitors default retry-on-interrupt ON
                // (an app-restart-interrupted check re-runs once); an explicit
                // opt-in still wins for ordinary automations.
                retry_on_interrupt: req.retry_on_interrupt
                    || automations::model::default_retry_on_interrupt(monitor),
                // monitor-handoff U2/U4: the not-before floor rides the wire
                // (untrusted epoch-ms; schedule math saturates) and clamps
                // the initial next_run_at inside `create`.
                not_before_ms: req.not_before_ms,
                monitor,
                pickup_pointers,
                // Automation-dependencies U3 (R7): the raw wire edge; the
                // manager validates it against the live store (existence,
                // non-monitor upstream, chain depth/cycle, within range) —
                // the socket payload is untrusted, so nothing is trusted
                // from flag combinations alone. `--within` without
                // `--after` is rejected CLI-side and re-rejected here.
                after: match (&req.after, req.within_ms) {
                    (None, Some(_)) => {
                        return AutomationResponse::err(
                            "--within requires --after",
                        )
                    }
                    (after, within_ms) => after.clone().map(|upstream_id| {
                        automations::model::Dependency {
                            upstream_id,
                            within_ms,
                        }
                    }),
                },
                // fly-dag-primitives G1: agent-only, re-rejected above on the
                // script path; stamped as-is here.
                verdict_gated: req.verdict_gated,
                origin,
            }) {
                Ok(created) => {
                    // monitor-handoff U4 (R13's backend half): signal the
                    // frontend to close the registering pane's tab — after
                    // `create` returned, i.e. after the store flush and off
                    // the store lock (KTD-B). Non-monitor creates never emit,
                    // and neither does a create whose flush FAILED
                    // (fix(review) #14, R12 refuse-rather-than-lose): the
                    // registration is live in memory but dies at restart, so
                    // closing the parent tab would discard the session the
                    // monitor is supposed to hand back to. The response path
                    // is unchanged — the CLI still prints the flush warning,
                    // and the still-open tab is where the user sees it.
                    if monitor && created.flush_ok {
                        mgr.emit_monitor_registered(pane_id, &created.automation.id);
                    }
                    AutomationResponse::ok(Some(created.automation.id), created.warning)
                }
                Err(e) => AutomationResponse::err(e),
            }
        }
        // Automation-update U2 (R8, KTD6): patch a stored record in place.
        // The payload is untrusted regardless of what the CLI already
        // checked, so this arm re-validates exactly like the create arm
        // above — same closed sets, same ceilings, same refusals. The gates
        // that need the *record* (retired / monitor / mode-kind switch) run
        // inside the manager's store mutation (U1), not here.
        "automation/update" => {
            let Some(id) = req.id.clone() else {
                return AutomationResponse::err("update requires an automation id");
            };
            // KTD1: resolve the clear list against the closed name set; an
            // unknown member is refused, never ignored.
            let clear = match automations::UpdateClear::parse(&req.clear) {
                Ok(c) => c,
                Err(e) => return AutomationResponse::err(e),
            };
            // KTD2 — the exclusions are the design, each with its own error.
            if req.after.is_some() || req.within_ms.is_some() {
                return AutomationResponse::err(automations::ERR_UPDATE_SET_AFTER);
            }
            if req.cwd.is_some() {
                return AutomationResponse::err(
                    "update cannot change cwd — it would silently change which transcripts \
                     the output-capture guard can match; delete and recreate instead",
                );
            }
            if req.monitor || req.not_before_ms.is_some() {
                return AutomationResponse::err(automations::ERR_UPDATE_MONITOR);
            }
            // Both directions of a toggle at once is a client bug, not a
            // last-one-wins guess.
            if req.retry_on_interrupt && clear.retry_on_interrupt {
                return AutomationResponse::err(
                    "pass only one of --retry-on-interrupt / --no-retry-on-interrupt",
                );
            }
            if req.model.is_some() && clear.model {
                return AutomationResponse::err("pass only one of --model / --no-model");
            }
            if req.effort.is_some() && clear.effort {
                return AutomationResponse::err("pass only one of --effort / --no-effort");
            }
            if req.headless.is_some() && clear.disposition {
                return AutomationResponse::err(
                    "pass only one of --headless / --paned / --default-disposition",
                );
            }
            // fly-dag-primitives G1: both directions of the verdict-gating
            // toggle at once is a client bug, not a last-one-wins guess.
            if req.verdict_gated && clear.verdict_gated {
                return AutomationResponse::err(
                    "pass only one of --verdict-gated / --no-verdict-gated",
                );
            }
            // The same closed effort set the create path validates CLI-side.
            if let Some(level) = req.effort.as_deref() {
                if !cli::automation::VALID_EFFORTS.contains(&level) {
                    return AutomationResponse::err(format!(
                        "--effort must be one of {} (got {level:?})",
                        cli::automation::VALID_EFFORTS.join(", ")
                    ));
                }
            }
            if let Some(name) = req.interpreter.as_deref() {
                if let Err(e) = automations::script::resolve_interpreter(name) {
                    return AutomationResponse::err(e);
                }
            }
            // A timeout the dispatcher will never honour is REFUSED, not
            // clamped — the create arm's 2026-08-07 lesson applies verbatim
            // here, and an update is *exactly* the surface someone reaches for
            // when raising a timeout.
            if let Some(timeout_ms) = req.timeout_ms {
                if timeout_ms > automations::script::TIMEOUT_MAX_MS {
                    return AutomationResponse::err(format!(
                        "--timeout {}ms exceeds the maximum {}ms ({} min); \
                         it would be silently clamped at run time",
                        timeout_ms,
                        automations::script::TIMEOUT_MAX_MS,
                        automations::script::TIMEOUT_MAX_MS / 60_000,
                    ));
                }
            }
            let spec = automations::UpdateSpec {
                name: req.name.clone(),
                cron: req.cron.clone(),
                timezone: req.timezone.clone(),
                retry_on_interrupt: match (req.retry_on_interrupt, clear.retry_on_interrupt) {
                    (true, _) => Some(true),
                    (_, true) => Some(false),
                    _ => None,
                },
                prompt: req.prompt.clone(),
                // The nested Option is the tri-state: set / clear / leave.
                model: match (req.model.clone(), clear.model) {
                    (Some(m), _) => Some(Some(m)),
                    (_, true) => Some(None),
                    _ => None,
                },
                effort: match (req.effort.clone(), clear.effort) {
                    (Some(e), _) => Some(Some(e)),
                    (_, true) => Some(None),
                    _ => None,
                },
                headless: match (req.headless, clear.disposition) {
                    (Some(h), _) => Some(Some(h)),
                    (_, true) => Some(None),
                    _ => None,
                },
                // fly-dag-primitives G1: `--verdict-gated` sets, the
                // `verdictGated` clear member unsets, absent leaves unchanged.
                verdict_gated: match (req.verdict_gated, clear.verdict_gated) {
                    (true, _) => Some(true),
                    (_, true) => Some(false),
                    _ => None,
                },
                script: req.script.clone(),
                interpreter: req.interpreter.clone(),
                timeout_ms: req.timeout_ms,
                clear_after: clear.after,
            };
            match mgr.update(&id, spec) {
                Ok(updated) => {
                    AutomationResponse::ok(Some(updated.automation.id), updated.warning)
                }
                Err(e) => AutomationResponse::err(e),
            }
        }
        "automation/pause" => match req.id {
            Some(id) => match mgr.pause(&id) {
                Ok(a) => AutomationResponse::ok(Some(a.id), None),
                Err(e) => AutomationResponse::err(e),
            },
            None => AutomationResponse::err("pause requires an automation id"),
        },
        "automation/resume" => match req.id {
            Some(id) => match mgr.resume(&id) {
                Ok(a) => AutomationResponse::ok(Some(a.id), None),
                Err(e) => AutomationResponse::err(e),
            },
            None => AutomationResponse::err("resume requires an automation id"),
        },
        "automation/delete" => match req.id {
            Some(id) => match mgr.delete(&id) {
                Ok(a) => {
                    // Automation-dependencies R8: delete is allowed with
                    // dependents pointing here (no cascade, no refusal) —
                    // they withhold honestly from now on — but the operator
                    // is told which edges were left dangling.
                    let dependents: Vec<String> = mgr
                        .list()
                        .into_iter()
                        .filter(|d| {
                            d.after.as_ref().is_some_and(|e| e.upstream_id == a.id)
                        })
                        .map(|d| format!("{} ({})", d.id, d.name))
                        .collect();
                    let warning = (!dependents.is_empty()).then(|| {
                        format!(
                            "dependent automation(s) now have a missing upstream and will \
                             withhold: {}",
                            dependents.join(", ")
                        )
                    });
                    AutomationResponse::ok(Some(a.id), warning)
                }
                Err(e) => AutomationResponse::err(e),
            },
            None => AutomationResponse::err("delete requires an automation id"),
        },
        "automation/run" => match req.id {
            Some(id) => match mgr.manual_run(&id) {
                Ok(ManualRun::Started { run_id }) => AutomationResponse::ok(Some(run_id), None),
                Ok(ManualRun::Skipped { run_id }) => AutomationResponse {
                    ok: true,
                    id: Some(run_id),
                    warning: Some("a run was already in flight; this occurrence was skipped".into()),
                    error: None,
                },
                // Automation-dependencies R12: the dependency refused — the
                // honest reason reaches the operator synchronously (and is
                // on the withheld row).
                Ok(ManualRun::Withheld { run_id, reason }) => AutomationResponse {
                    ok: true,
                    id: Some(run_id),
                    warning: Some(format!("withheld: {reason}")),
                    error: None,
                },
                Err(e) => AutomationResponse::err(e),
            },
            None => AutomationResponse::err("run requires an automation id"),
        },
        other => AutomationResponse::err(format!("unknown automation op {other:?}")),
    }
}

/// Composite dispatcher (automations U7+U5, headless-monitor-checks U5): the
/// one [`automations::Dispatcher`] the manager routes through. Script
/// dispatch goes to the [`automations::script::ScriptRunner`]; agent dispatch
/// **forks on the claimed row's `headless` marker** (the "Routing lives in
/// the existing CompositeDispatcher" KTD, widened from the monitor flag by
/// headless-agent-automations U3): a headless-resolved run — every monitor
/// check, and by default every regular agent automation (R2) — hands to the
/// [`automations::headless::HeadlessRunner`] — no pane, no tab, no
/// `automation://agent-run` emission (headless-monitor-checks R1) — while an
/// explicitly-paned agent automation keeps the pane path via the
/// frontend-emitting [`automations::AgentDispatcher`]. The `agent` arm is
/// `dyn` so the routing tests below can inject a recorder without a live
/// event sink.

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn explicit_resume_always_resumes() {
        // `fly resume` resumes regardless of the marker (a clean prior exit too).
        assert_eq!(decide_launch_mode(true, true), LaunchMode::Resume);
        assert_eq!(decide_launch_mode(true, false), LaunchMode::Resume);
    }

    #[test]
    fn clean_prior_exit_is_normal() {
        // Bare `fly` after a clean shutdown (marker present) → fresh shells (R1).
        assert_eq!(decide_launch_mode(false, true), LaunchMode::Normal);
    }

    #[test]
    fn crashed_prior_run_offers_resume() {
        // Bare `fly` with the marker absent (a prior crash) → offer (KTD-G, R9).
        assert_eq!(decide_launch_mode(false, false), LaunchMode::Offer);
    }

    #[test]
    fn resume_is_not_a_cli_subcommand() {
        // `resume` falls through to a window launch, unlike notify/hooks (KTD-B).
        assert!(!cli::is_cli_subcommand("resume"));
        assert!(cli::is_cli_subcommand("notify"));
        assert!(cli::is_cli_subcommand("hooks"));
        assert!(cli::is_cli_subcommand("automation"));
    }

    #[test]
    fn help_is_a_cli_subcommand() {
        // `fly --help` must print and exit as a CLI, not launch the app.
        assert!(cli::is_cli_subcommand("help"));
        assert!(cli::is_cli_subcommand("--help"));
        assert!(cli::is_cli_subcommand("-h"));
        // The overview names the discovery target that motivated this.
        assert!(cli::top_level_help().contains("automation"));
    }

    // ---- monitor pickup pointers (monitor-handoff U4, R11) -------------------

    fn target(session_cwd: Option<&str>) -> session::handoff::HandoffTarget {
        session::handoff::HandoffTarget {
            session_id: "sess-1".into(),
            transcript_path: "/root/-proj-app/sess-1.jsonl".into(),
            session_cwd: session_cwd.map(str::to_string),
            last_turn_ms: 5,
            session_source: session::resume::SessionSource::Hook,
            divergence_pending: false,
        }
    }

    // R11: a qualified target flattens verbatim — and the record's captured
    // cwd wins over the live fallback (the same R12-precedence the transcript
    // path was derived under, so path and cwd cohere).
    #[test]
    fn monitor_pointers_flatten_the_target_with_the_records_cwd_winning() {
        let p = monitor_pointers_from_target(Some(target(Some("/proj/recorded"))), Some("/live"))
            .expect("a qualified target yields pointers");
        assert_eq!(p.session_id, "sess-1");
        assert_eq!(p.transcript_path, "/root/-proj-app/sess-1.jsonl");
        assert_eq!(p.session_cwd, "/proj/recorded", "record cwd wins");
    }

    // R11: a record that never captured its cwd falls back to the live cwd —
    // the directory the transcript was actually derived under.
    #[test]
    fn monitor_pointers_fall_back_to_the_live_cwd_when_the_record_has_none() {
        let p = monitor_pointers_from_target(Some(target(None)), Some("/proj/live"))
            .expect("live-cwd fallback qualifies");
        assert_eq!(p.session_cwd, "/proj/live");
    }

    // R12: no target (unresolvable/unqualified session) or no cwd from
    // anywhere abstains to None — the create arm turns that into the
    // distinct refusal and stores nothing.
    #[test]
    fn monitor_pointers_abstain_without_a_target_or_any_cwd() {
        assert_eq!(monitor_pointers_from_target(None, Some("/live")), None);
        assert_eq!(monitor_pointers_from_target(Some(target(None)), None), None);
    }

    // ---- headless-monitor-checks U5: the CompositeDispatcher monitor fork ----

    /// A recording stand-in for the pane-path agent arm (the real
    /// `AgentDispatcher` needs a live event sink, which no unit test has —
    /// exactly why `CompositeDispatcher.agent` is `dyn`).
    #[derive(Default)]
    struct RecordingAgentArm {
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl automations::Dispatcher for RecordingAgentArm {
        fn dispatch_agent(
            &self,
            a: &automations::model::Automation,
            _run_id: &str,
            _launch: &automations::ResolvedLaunch,
            _headless: bool,
        ) -> Result<(), String> {
            self.calls.lock().unwrap().push(a.id.clone());
            Ok(())
        }
        fn dispatch_script(
            &self,
            _a: &automations::model::Automation,
            _run_id: &str,
        ) -> Result<(), String> {
            Err("unused in these tests".into())
        }
    }

    fn dispatcher_automation(monitor: bool) -> automations::model::Automation {
        automations::model::Automation {
            id: "a1".into(),
            name: "watch".into(),
            cron: "*/5 * * * *".into(),
            timezone: "UTC".into(),
            enabled: true,
            retry_on_interrupt: false,
            monitor,
            not_before_ms: None,
            retired_at: None,
            pickup_pointers: None,
            after: None,
            verdict_gated: false,
            cwd: "/tmp".into(),
            mode: automations::model::Mode::Agent {
                prompt: "check the run".into(),
                model: None,
                effort: None, headless: None,
            },
            origin: automations::model::Origin {
                pane_id: 1,
                workspace_id: "ws-1".into(),
                label: "cli".into(),
            },
            created_at: 0,
            updated_at: 0,
            next_run_at: None,
            runs: Vec::new(),
        }
    }

    type CheckCloses = Arc<std::sync::Mutex<Vec<(String, String, automations::headless::CheckOutcome)>>>;

    /// A CompositeDispatcher whose headless arm points at a nonexistent
    /// binary: `run()` fails its spawn synchronously and reports through the
    /// collector closer — proving the monitor leg reached the runner without
    /// needing a real claude.
    fn fork_harness() -> (backend::CompositeDispatcher, Arc<RecordingAgentArm>, CheckCloses) {
        let agent = Arc::new(RecordingAgentArm::default());
        let closes: CheckCloses = Arc::new(std::sync::Mutex::new(Vec::new()));
        let c = Arc::clone(&closes);
        let headless = Arc::new(automations::headless::HeadlessRunner::with_config(
            Arc::new(
                move |aid: &str, rid: &str, outcome: automations::headless::CheckOutcome| {
                    c.lock().unwrap().push((aid.to_owned(), rid.to_owned(), outcome));
                },
            ),
            "/nonexistent/fly-test-claude",
            automations::headless::HeadlessTiming::default(),
        ));
        let script = Arc::new(automations::script::ScriptRunner::new(Arc::new(
            |_: &str, _: &str, _: automations::model::RunOutcome| {},
        )));
        let d = backend::CompositeDispatcher {
            agent: Arc::clone(&agent) as Arc<dyn automations::Dispatcher>,
            script,
            headless,
        };
        (d, agent, closes)
    }

    fn no_launch() -> automations::ResolvedLaunch {
        automations::ResolvedLaunch {
            model: None,
            effort: None,
            fallback: None,
        }
    }

    // The routing KTD ("Routing lives in the existing CompositeDispatcher"):
    // a monitor automation hands to the HeadlessRunner — the pane arm is
    // never consulted, and the dispatch returns Ok even though this spawn
    // failed, because `run()` reports every failure through the CheckCloser
    // as an infra Failed close (feeding the R7 escalation via the close
    // path), never as a dispatch Err.
    #[test]
    fn composite_dispatcher_routes_a_monitor_to_the_headless_runner() {
        let (d, agent, closes) = fork_harness();
        let res =
            automations::Dispatcher::dispatch_agent(&d, &dispatcher_automation(true), "r1", &no_launch(), true);
        assert_eq!(res, Ok(()), "handed off — failures ride the closer");
        assert!(
            agent.calls.lock().unwrap().is_empty(),
            "the pane arm is never consulted for a monitor"
        );
        let closes = closes.lock().unwrap();
        assert_eq!(closes.len(), 1, "the runner reported through the closer");
        assert_eq!(closes[0].0, "a1");
        assert_eq!(closes[0].1, "r1");
        assert!(
            matches!(
                &closes[0].2,
                automations::headless::CheckOutcome::Infra { reason }
                    if reason.starts_with("spawn failed:")
            ),
            "a spawn failure is an infra close, not a dispatch Err: {:?}",
            closes[0].2
        );
    }

    // Headless-agent-automations R2/AE3: an explicitly-paned agent claim
    // (row marker false) keeps the pane path — the headless runner is
    // untouched.
    #[test]
    fn composite_dispatcher_keeps_paned_agent_automations_on_the_pane_arm() {
        let (d, agent, closes) = fork_harness();
        let res =
            automations::Dispatcher::dispatch_agent(&d, &dispatcher_automation(false), "r2", &no_launch(), false);
        assert_eq!(res, Ok(()));
        assert_eq!(*agent.calls.lock().unwrap(), vec!["a1".to_string()]);
        assert!(closes.lock().unwrap().is_empty(), "runner untouched");
    }

    // Headless-agent-automations U3: the fork keys on the threaded row
    // marker, not the monitor flag — a REGULAR agent claim resolved
    // headless routes to the runner and never consults the pane arm.
    #[test]
    fn composite_dispatcher_routes_a_headless_regular_agent_to_the_runner() {
        let (d, agent, closes) = fork_harness();
        let res = automations::Dispatcher::dispatch_agent(
            &d,
            &dispatcher_automation(false),
            "r3",
            &no_launch(),
            true,
        );
        assert_eq!(res, Ok(()), "handed off — failures ride the closer");
        assert!(
            agent.calls.lock().unwrap().is_empty(),
            "the pane arm is never consulted for a headless-resolved run"
        );
        assert_eq!(closes.lock().unwrap().len(), 1, "runner reported through the closer");
    }
}
