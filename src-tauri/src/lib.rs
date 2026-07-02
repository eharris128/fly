//! fly — a terminal for AI coding agents.
//!
//! This library backs both the desktop app and the `fly` CLI subcommands
//! (KTD12); `main.rs` is a thin shim over [`run`].

pub mod automations;
pub mod cli;
pub mod config;
pub mod cwd;
pub mod hooks;
pub mod lifecycle;
pub mod notify;
pub mod pty;
pub mod session;
pub mod state;
pub mod stream;
pub mod usage;

use std::path::PathBuf;
use std::sync::Arc;

use tauri::{Emitter, Manager};

use config::ConfigStore;
use hooks::{Dispatch, HookServer, TokenRegistry, ValidatedHook};
use notify::{NotificationGate, Surfaced};
use pty::PtyManager;
use state::attention::{Signal, Tier};
use state::AttentionManager;

/// Apply Linux-specific webview workarounds before the webview initializes.
///
/// The WebKitGTK DMABUF renderer causes blank windows on Wayland/NVIDIA
/// (KTD11). Disabling it is the documented fix, but it forces software
/// compositing — and on non-NVIDIA GPUs (Intel/AMD) that adds visible
/// per-keystroke render lag in the terminal panes for no benefit. So we only
/// apply the workaround when the NVIDIA driver is actually loaded, and only
/// when the user hasn't already chosen a value (it stays env-overridable).
#[cfg(target_os = "linux")]
fn apply_linux_webview_env() {
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() && nvidia_driver_active() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
}

/// Whether the NVIDIA kernel driver is loaded (the only case where the DMABUF
/// blank-window workaround is needed). On hybrid laptops rendering on the Intel
/// iGPU these nodes are absent, so the webview keeps hardware compositing.
#[cfg(target_os = "linux")]
fn nvidia_driver_active() -> bool {
    std::path::Path::new("/proc/driver/nvidia/version").exists()
        || std::path::Path::new("/dev/nvidiactl").exists()
}

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

/// Where the hook socket lives — under the XDG runtime dir, keyed by pid so
/// instances don't collide (single-instance enforcement arrives in U14).
fn hook_socket_path() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join(app_dir_name())
        .join(format!("hook-{}.sock", std::process::id()))
}

/// Surface webview errors to the app's stderr. The webview console is
/// otherwise invisible when running outside a browser devtools session, so the
/// frontend forwards uncaught errors here.
#[tauri::command]
fn frontend_log(msg: String) {
    eprintln!("[fly-webview] {msg}");
}

/// How the app was launched (U7, KTD-B/G), read once by the frontend at restore.
/// `resume` is an app *launch mode*, not a CLI subcommand: it falls through the
/// `is_cli_subcommand` check and launches a window like a bare `fly`.
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
fn resolve_launch_mode(args: &[String]) -> LaunchMode {
    let resume_requested = args.get(1).map(|s| s == "resume").unwrap_or(false);
    let marker = session::resume::clean_exit_path();
    let prev_clean = session::resume::took_clean_exit_at(&marker);
    let _ = session::resume::set_clean_exit_at(&marker, false);
    decide_launch_mode(resume_requested, prev_clean)
}

/// Command: the frontend reads how it was launched to decide whether to resume.
#[tauri::command]
fn get_launch_mode(mode: tauri::State<'_, LaunchMode>) -> LaunchMode {
    *mode
}

/// Run the fly desktop application — or a `fly` CLI subcommand if argv selects
/// one (KTD12).
pub fn run() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(first) = args.get(1) {
        if cli::is_cli_subcommand(first) {
            std::process::exit(cli::run(&args));
        }
    }

    // `resume` falls through here (it launches a window, not a CLI subcommand).
    // Resolve the launch mode and clear the clean-exit marker up front (KTD-B/G).
    let launch_mode = resolve_launch_mode(&args);

    #[cfg(target_os = "linux")]
    apply_linux_webview_env();

    let config = Arc::new(ConfigStore::load(config::default_path()));
    // Immutable snapshot of settings (no runtime config reload in v1). The
    // dispatch closure needs notification settings after `.manage(config)`
    // consumes the Arc, so it captures this instead.
    let cfg = config.get();
    let pty_manager = Arc::new(PtyManager::new());
    let tokens = Arc::new(TokenRegistry::new());
    let attention = Arc::new(AttentionManager::new(
        cfg.attention_debounce_ms,
        cfg.notifications_muted_default,
    ));
    let gate = Arc::new(NotificationGate::new(cfg.notification_coalesce_threshold));

    // Clones for the hook server's dispatch (the originals are managed below).
    let tokens_for_hooks = Arc::clone(&tokens);
    let attention_for_hooks = Arc::clone(&attention);
    // The dispatch resolves PaneId → leaf_key to key resume records (U3).
    let pty_for_hooks = Arc::clone(&pty_manager);
    let config_for_hooks = cfg;

    tauri::Builder::default()
        // single-instance must be registered first; a second launch focuses
        // the existing window instead of corrupting the shared session state.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_store::Builder::default().build())
        .manage(config)
        .manage(pty_manager)
        .manage(tokens)
        .manage(attention)
        .manage(launch_mode)
        .setup(move |app| {
            // A dev flavor (FLY_APP_NAME set) gets a distinct title so it's
            // obvious which window is the throwaway dev build next to a stable
            // install. The identifier (single-instance) + dirs are isolated
            // separately; this is just the visible marker.
            let flavor = app_dir_name();
            if flavor != "fly" {
                if let Some(win) = app.get_webview_window("main") {
                    let suffix = flavor.strip_prefix("fly-").unwrap_or(&flavor);
                    let _ = win.set_title(&format!("fly ({suffix})"));
                }
            }

            let handle = app.handle().clone();
            let attention = attention_for_hooks;
            let gate = gate;
            let config = config_for_hooks;
            let pty = pty_for_hooks;
            // The per-effect dispatch (U18): decouple the in-app ring, the
            // history record, the desktop banner, and the chime — each decided
            // independently by the policy (KTD14), not fused behind one boolean.
            let dispatch: Dispatch = Arc::new(move |pane, hook: ValidatedHook| {
                // Resume capture (U3, KTD-A): a session_id-bearing hook upserts the
                // pane's resume record (id + the payload's project cwd, KTD-H),
                // keyed by its stable leaf key. Backend-owned, so it survives a
                // renderer crash; write-through, so it survives an unclean kill.
                // Done before the attention gate so a debounced/suppressed raise
                // still captures. Best-effort — a write error never blocks the UI.
                if let Some(session_id) = hook.session_id.clone() {
                    if let Some(leaf_key) = pty.leaf_key(pane) {
                        let _ = session::resume::upsert_at(
                            &session::resume::resume_path(),
                            &leaf_key,
                            session::resume::ResumePartial {
                                session_id: Some(session_id),
                                session_cwd: hook.cwd.clone(),
                                ..Default::default()
                            },
                        );
                    }
                }

                let reason = hook.reason;
                let signal = Signal {
                    reason,
                    tier: Tier::Hook,
                };
                let Some(outcome) = attention.signal(pane, signal) else {
                    return;
                };
                stream::emit_attention(&handle, pane, &outcome);

                // U7 agent-run closure (KTD-F): a Stop hook on a pane linked to
                // an agent run closes it as succeeded on first occurrence. Only
                // "Stop" closes (not "SubagentStop"); a bare Finished with no
                // hook_event falls to the 30-min deadline (skew rule).
                if hook.hook_event.as_deref() == Some("Stop") {
                    if let Some(mgr) = handle.try_state::<Arc<automations::AutomationManager>>() {
                        // Find and close any automation run linked to this pane.
                        // The manager's close_run is idempotent (no-op if run not
                        // found or already closed), so a second Stop is safe.
                        let _ = mgr.close_run_by_pane(pane.0);
                    }
                }

                // Only a fresh (non-debounced) raise surfaces; duplicates drop.
                if !outcome.recordable {
                    return;
                }

                let reason_effects = config.reason_effects.for_reason(reason);
                let Some(decision) = attention.decide(pane, reason_effects) else {
                    return;
                };
                let effects = decision.effects;

                // History is decoupled from the banner gate (KTD16): record
                // every fresh raise — even a coalesced or rate-limited one —
                // carrying the read-at-birth bit. Text sanitized (R16/R24).
                if effects.record {
                    stream::emit_notification_added(
                        &handle,
                        gate.next_id(),
                        pane,
                        reason,
                        hook.title.as_deref().map(notify::sanitize_title),
                        hook.body.as_deref().map(notify::sanitize_body),
                        notify::now_unix_ms(),
                        decision.read,
                    );
                }

                // Desktop banner: away-only, coalesced + rate-limited. Consult
                // the gate only when the desktop effect is on, so a suppressed
                // banner doesn't burn a rate-limit slot.
                let banner_title = hook.title.as_deref().unwrap_or("fly: an agent needs you");
                let banner_body = hook.body.as_deref().unwrap_or("");
                let gate_verdict = if effects.desktop {
                    gate.decide(attention.raised_count(), banner_title, banner_body, gate.now_ms())
                } else {
                    Surfaced::Suppressed
                };
                let actions = notify::surface_actions(effects, &gate_verdict);
                if actions.banner {
                    match &gate_verdict {
                        Surfaced::Individual { title, body } => notify::banner(&handle, title, body),
                        Surfaced::Coalesced { count } => {
                            // "Panes", not "agents": alert raises count too
                            // (automations U-ID U12, KTD-H).
                            notify::banner(&handle, "fly", &notify::coalesced_body(*count))
                        }
                        Surfaced::Suppressed => {}
                    }
                }
                // Chime follows effects.sound, independent of the banner: a
                // foregrounded user still hears a hidden pane raise (the
                // resolved audible-cue decision).
                if effects.sound {
                    if let Some(name) = config.notification_sound.as_deref() {
                        notify::play_sound(name);
                    }
                }
                // Opt-in notification command, gated on the *fired* banner
                // (rides MIN_INTERVAL_MS) — non-blocking, sanitized, env-isolated
                // (KTD17/U19). Title/body come from the hook; subtitle is the
                // reason's default label.
                if actions.command {
                    if let Some(cmd) = config.notification_command.as_deref() {
                        notify::command::run(
                            cmd,
                            banner_title,
                            notify::reason_subtitle(reason),
                            banner_body,
                        );
                    }
                }
            });
            let server = HookServer::start(hook_socket_path(), tokens_for_hooks, dispatch)
                .map_err(|e| format!("failed to start hook server: {e}"))?;
            app.manage(server);

            // Automations (U4): manager over the write-through store, with the
            // real clock and a Tauri-event changed-emitter. Constructed with
            // the unwired placeholder dispatcher, then handed the real script
            // runner below (the runner's row closer needs the manager's Arc,
            // so injection is post-construction). Construction runs startup
            // recovery (R5: orphaned Running rows close failed("interrupted")).
            let changed_handle = app.handle().clone();
            let automations_mgr = Arc::new(automations::AutomationManager::new(
                automations::store::Store::load_default(),
                Arc::new(automations::UnwiredDispatcher),
                Box::new(notify::now_unix_ms),
                Box::new(move |id: &str| {
                    let _ = changed_handle.emit(automations::AUTOMATION_CHANGED_EVENT, id);
                }),
            ));
            // Agent dispatcher (U7): emits automation://agent-run to the frontend.
            // The frontend creates a background tab with the prompt-threaded
            // command, calls spawn_pane with the run id, and the backend links
            // run↔pane atomically on spawn.
            let agent_dispatcher = automations::AgentDispatcher::new(app.handle().clone());
            // Script runner (U5): the real dispatcher for script-mode runs.
            // Its reaper threads close rows via `close_run` — the Weak breaks the
            // manager↔runner Arc cycle (both live for the app's lifetime; a
            // post-shutdown reaper close simply drops). The killer/capacity
            // seams route delete/shutdown group kills and the sweep's KTD-D
            // pre-claim capacity skip to the runner's in-flight registry.
            // TODO(U6): `set_alert_sink` replaces the runner's no-op alert
            // sink with the alerts-log + Reason::Alert raise.
            let closer_mgr = Arc::downgrade(&automations_mgr);
            let script_runner = Arc::new(automations::script::ScriptRunner::new(Arc::new(
                move |automation_id: &str, run_id: &str, outcome: automations::model::RunOutcome| {
                    if let Some(mgr) = closer_mgr.upgrade() {
                        let _ = mgr.close_run(automation_id, run_id, outcome);
                    }
                },
            )));
            // Composite dispatcher (U7+U5): agent dispatch routes to
            // AgentDispatcher, script dispatch routes to ScriptRunner.
            struct CompositeDispatcher {
                agent: std::sync::Arc<automations::AgentDispatcher>,
                script: std::sync::Arc<automations::script::ScriptRunner>,
            }
            impl automations::Dispatcher for CompositeDispatcher {
                fn dispatch_agent(
                    &self,
                    a: &automations::model::Automation,
                    run_id: &str,
                ) -> Result<(), String> {
                    self.agent.dispatch_agent(a, run_id)
                }
                fn dispatch_script(
                    &self,
                    a: &automations::model::Automation,
                    run_id: &str,
                ) -> Result<(), String> {
                    self.script.dispatch_script(a, run_id)
                }
            }
            automations_mgr.set_dispatcher(Arc::new(CompositeDispatcher {
                agent: Arc::clone(&agent_dispatcher),
                script: Arc::clone(&script_runner),
            }));
            let killer_runner = Arc::clone(&script_runner);
            automations_mgr
                .set_script_killer(Arc::new(move |run_id: &str| killer_runner.kill_run(run_id)));
            let capacity_runner = Arc::clone(&script_runner);
            automations_mgr.set_script_capacity(Arc::new(move || capacity_runner.has_capacity()));
            // U7 pane-alive probe (KTD-D): R7 overlap check widens to include
            // deadline-failed agent runs whose linked pane is still alive
            // (stuck agent must skip, not fan out). Consulted inside the sweep's
            // mutate closure (cheap read, never re-entrant).
            // The pty_for_hooks above is moved into dispatch; get a fresh clone
            // for the pane-alive probe.
            let pty_mgr = app.state::<Arc<PtyManager>>().inner().clone();
            automations_mgr.set_agent_pane_alive(Arc::new(move |row: &automations::model::RunRow| {
                row.pane_id
                    .and_then(|id| pty_mgr.lifecycle(pty::PaneId(id)))
                    .is_some()
            }));
            app.manage(script_runner);
            app.manage(Arc::clone(&automations_mgr));
            // The sweep starts unconditionally (R5): script automations run
            // even if the webview never loads; agent dispatch waits for the
            // automations_frontend_ready command. Stopped + joined in
            // lifecycle::shutdown before the PTY reap.
            let sweep = automations::start_sweep(automations_mgr)
                .map_err(|e| format!("failed to start automation sweep: {e}"))?;
            app.manage(sweep);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            frontend_log,
            get_launch_mode,
            config::get_config,
            stream::spawn_pane,
            stream::set_visible_panes,
            stream::set_window_foreground,
            stream::set_panel_open,
            stream::set_muted,
            stream::set_workspace_muted,
            stream::set_pane_workspace,
            pty::pty_write,
            pty::pty_resize,
            pty::close_pane,
            pty::pty_pause,
            pty::pty_resume,
            pty::pane_cwd,
            pty::pane_command,
            pty::pane_session_id,
            pty::pane_activity,
            session::save_session,
            session::load_session,
            session::save_scrollback,
            session::load_scrollback,
            session::resume::load_resume_records,
            session::resume::save_resume_record,
            session::resume::save_resume_session,
            session::resume::prune_resume_records,
            session::transcript::continue_target,
            usage::usage_snapshot,
            automations::automations_frontend_ready,
        ])
        .build(tauri::generate_context!())
        .expect("error while building fly")
        // Ordered teardown on quit: reap every pane (no zombies/orphans, R4).
        .run(|app_handle, event| {
            if let tauri::RunEvent::ExitRequested { .. } = event {
                lifecycle::shutdown(app_handle);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
