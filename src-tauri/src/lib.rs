//! fly — a terminal for AI coding agents.
//!
//! This library backs both the desktop app and the `fly` CLI subcommands
//! (KTD12); `main.rs` is a thin shim over [`run`].

pub mod automations;
pub mod cli;
pub mod config;
pub mod cwd;
pub mod feed;
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

use tauri::{Emitter, Listener, Manager};

use config::ConfigStore;
use hooks::{Dispatch, HookServer, TokenRegistry, ValidatedHook};
use notify::{NotificationGate, Surfaced};
use pty::PtyManager;
use state::attention::{Reason, Signal, Tier};
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

/// U6 event payload: an alert arrived with no sink pane registered (R17). The
/// frontend single-flights a background "Automations" tab that `tail -f`s
/// `log_path`, then calls `register_alert_sink` with the new pane id.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AlertPendingEvent {
    log_path: String,
}

/// U6 (R18): ring a pane through the attention pipeline for an automation alert
/// — the same seam the hook dispatch uses (`Signal { reason, tier }` →
/// `emit_attention`), so an alert surfaces exactly like an agent raise. The
/// attention manager's lock is independent of the automations store lock
/// (KTD-B), so this is safe to call from the reaper thread's sink closure.
fn raise_alert(app: &tauri::AppHandle, attention: &AttentionManager, pane_id: u64) {
    let pane = pty::PaneId(pane_id);
    if let Some(outcome) = attention.signal(
        pane,
        Signal {
            reason: Reason::Alert,
            tier: Tier::Cli,
        },
    ) {
        stream::emit_attention(app, pane, &outcome);
    }
}

/// U6 command (R17): the frontend registers its "Automations" sink pane so
/// queued and future alerts ring it. Draining the pending backlog raises one
/// ring per alert that arrived before the pane existed (the attention debounce
/// collapses a burst into a single visible ring, which is the intent).
#[tauri::command]
fn register_alert_sink(
    app: tauri::AppHandle,
    attention: tauri::State<'_, Arc<AttentionManager>>,
    alerts: tauri::State<'_, Arc<automations::alerts::AlertsLog>>,
    pane_id: u64,
) {
    for _ in alerts.register_sink(pane_id) {
        raise_alert(&app, &attention, pane_id);
    }
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
                // Stamped `Hook` HERE, at the call site — the socket authenticates
                // the pane, not the honesty of in-pane code, so no wire field may
                // select a rank (fix-session-pane-attribution KTD2): pane-precise,
                // above the poll's guess, below a human pick.
                if let Some(session_id) = hook.session_id.clone() {
                    if let Some(leaf_key) = pty.leaf_key(pane) {
                        let _ = session::resume::upsert_at(
                            &session::resume::resume_path(),
                            &leaf_key,
                            session::resume::ResumePartial {
                                session_id: Some(session_id),
                                session_cwd: hook.cwd.clone(),
                                session_source: Some(session::resume::SessionSource::Hook),
                                ..Default::default()
                            },
                        );
                    }
                }

                // Capture-only short-circuit (fix-attribution U2, KTD1/R2):
                // a SessionStart capture ends here, after the upsert and before
                // any Signal — no ring, no history, no banner, and (ordered
                // before the Stop-close below, an accepted self-scoped
                // interaction) never a run closure. The reason is ignored: a
                // message carrying both a raising reason and a capture gate
                // raises nothing.
                if hook.is_capture_only() {
                    return;
                }

                // U7 agent-run closure (KTD-F): a Stop hook on a pane linked to
                // an agent run closes it succeeded on first occurrence. Hoisted
                // ABOVE the attention raise so it is independent of it — U8 (KTD5)
                // suppresses the completion raise for an automation pane, and a
                // suppressed completion must still close its run. Only "Stop"
                // closes (not "SubagentStop"); a bare Finished with no hook_event
                // falls to the 30-min deadline (skew rule). close_run_by_pane is
                // idempotent (no-op if not found / already closed → a second Stop
                // is safe) and a no-op for any non-automation pane.
                if hook.hook_event.as_deref() == Some("Stop") {
                    if let Some(mgr) = handle.try_state::<Arc<automations::AutomationManager>>() {
                        // Off the dispatch thread: close_run_by_pane's U4b
                        // capture retries the transcript read for up to ~2s
                        // (Claude flushes the final turn ~100ms AFTER this Stop),
                        // and Claude Code BLOCKS on this hook — so a slow capture
                        // must not ride the dispatch path. Idempotent; and the
                        // Finished-suppression below keys only on the pane's
                        // automation linkage, not on the run still being open, so
                        // it stays correct without awaiting the close.
                        let mgr = mgr.inner().clone();
                        let pane_id = pane.0;
                        std::thread::spawn(move || {
                            let _ = mgr.close_run_by_pane(pane_id);
                        });
                    }
                    // Feed settle bump (feed-agent-reply-io U6/KTD4): a Stop
                    // means a reply just landed, but its final turn flushes to
                    // the transcript ~100ms AFTER this hook — a frame emitted
                    // on the immediate status change would still read the
                    // PREVIOUS reply's stamp, and nothing else would re-emit
                    // until the roster next changed. One delayed bump re-emits
                    // after the flush settles, so every connected consumer's
                    // `lastReplyAt` catches up. Off-thread: Claude blocks on
                    // this hook. Bumping an idle/disabled feed is harmless.
                    if let Some(feed) = handle.try_state::<Arc<feed::FeedState>>() {
                        let feed = feed.inner().clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_secs(2));
                            feed.bump();
                        });
                    }
                }

                let reason = hook.reason;
                // U8 (KTD5): suppress a background automation pane's *normal
                // completion* raise — a Stop → `Reason::Finished`. Its status
                // surface is the dashboard and its run auto-closes (U8); without
                // this the never-focused pane is always left "raised", so the
                // `succeeded && !isRaised` auto-close guard would never fire. A
                // genuine mid-run raise (Question/Permission/Notification) is a
                // DIFFERENT reason, so it still rings and keeps the tab (R7).
                if reason == Reason::Finished
                    && handle
                        .try_state::<Arc<automations::AutomationManager>>()
                        .is_some_and(|mgr| mgr.is_automation_pane(pane.0))
                {
                    return;
                }

                // Feed settle bump for Notification (feed-pending-question
                // U5/KTD5): a permission dialog raises attention *now*, but its
                // `tool_use` may not have flushed to the transcript yet — the
                // frame emitted on this roster change would carry no
                // `questionPendingAt`, and `FeedState::publish` dedups
                // identical rosters, so nothing would re-emit until the roster
                // next moved. One delayed bump re-reads after the flush
                // settles. Placed after the capture-only and
                // automation-suppression early returns, NOT gated on
                // `recordable` (a debounced duplicate still means a dialog is
                // up), off-thread because Claude Code blocks on this hook.
                // Mirrors the post-Stop bump above; AskUserQuestion fires no
                // hook at all, so its marker rides the normal roster poll.
                if hook.hook_event.as_deref() == Some("Notification") {
                    if let Some(feed) = handle.try_state::<Arc<feed::FeedState>>() {
                        let feed = feed.inner().clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_secs(2));
                            feed.bump();
                        });
                    }
                }

                let signal = Signal {
                    reason,
                    tier: Tier::Hook,
                };
                let Some(outcome) = attention.signal(pane, signal) else {
                    return;
                };
                stream::emit_attention(&handle, pane, &outcome);

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
            // U9 automation request handler: `automation/*` socket ops (create,
            // pause, resume, run, delete) are validated + answered here. It
            // resolves the AutomationManager lazily via `try_state` (like the
            // pane-exit tap), so it needs no construction-order coupling with the
            // manager built below — by the time any socket request arrives the
            // manager is managed. The notify path is unchanged.
            let automation_handle = app.handle().clone();
            let automation_handler: hooks::RequestHandler = Arc::new(move |pane, buf: &[u8]| {
                handle_automation_request(&automation_handle, pane, buf).to_bytes()
            });
            let server = HookServer::start_with_handler(
                hook_socket_path(),
                tokens_for_hooks,
                dispatch,
                Some(automation_handler),
            )
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
            let closer_mgr = Arc::downgrade(&automations_mgr);
            let script_runner = Arc::new(automations::script::ScriptRunner::new(Arc::new(
                move |automation_id: &str, run_id: &str, outcome: automations::model::RunOutcome| {
                    if let Some(mgr) = closer_mgr.upgrade() {
                        let _ = mgr.close_run(automation_id, run_id, outcome);
                    }
                },
            )));

            // U6 alert surfacing (R16/R17/R18): the alerts log + sink registry.
            // `surface_alert` is the one shared path — append the sanitized
            // `[name] first-line` (R16), then either ring the registered sink
            // pane through the attention pipeline (R18) or queue it and ask the
            // frontend to open the "Automations" sink pane (R17). It runs on the
            // reaper thread and takes only the AlertsLog lock + the log file —
            // never the store lock (KTD-B). Both the script alert path (U6) and
            // the interrupt-resilience path (interrupt-resilience U3) go through
            // it, so the two cannot drift on the R16/R17/R18 machinery.
            let alerts_log = automations::alerts::AlertsLog::open_default();
            let surface_alert: Arc<dyn Fn(&str, &str) + Send + Sync> = {
                let log = Arc::clone(&alerts_log);
                let attention = app.state::<Arc<AttentionManager>>().inner().clone();
                let handle = app.handle().clone();
                Arc::new(move |name: &str, first_line: &str| {
                    log.append(name, first_line); // R16
                    let queued = automations::alerts::QueuedAlert {
                        automation_name: name.to_string(),
                        first_line: first_line.to_string(),
                    };
                    match log.sink_or_queue(queued) {
                        // R18: a live sink pane rings via the attention pipeline.
                        Some(pane_id) => raise_alert(&handle, &attention, pane_id),
                        // R17: no sink yet — queued; ask the frontend to open
                        // the sink pane, which registers and drains the backlog.
                        None => {
                            let _ = handle.emit(
                                "automation://alert-pending",
                                AlertPendingEvent {
                                    log_path: log.log_path().to_string_lossy().into_owned(),
                                },
                            );
                        }
                    }
                })
            };
            // Script alert sink (U6).
            let script_alert = Arc::clone(&surface_alert);
            script_runner.set_alert_sink(Arc::new(
                move |ev: automations::script::AlertEvent| {
                    script_alert(&ev.automation_name, &ev.first_line);
                },
            ));
            // Interrupt-resilience alert sink (interrupt-resilience U3): a run an
            // app crash/restart left interrupted surfaces exactly like a script
            // alert. The line notes whether a one-shot retry will follow, so the
            // operator sees the difference between "lost, act if you care" and
            // "lost but being re-run".
            let interrupt_alert = Arc::clone(&surface_alert);
            automations_mgr.set_interrupt_sink(Arc::new(
                move |ir: &automations::InterruptedRun| {
                    let line = if ir.is_retry_eligible() {
                        "interrupted by restart — retrying"
                    } else {
                        "interrupted by restart"
                    };
                    interrupt_alert(&ir.name, line);
                },
            ));
            // Monitor verdict / broken-monitor alerts (monitor-handoff U3 —
            // R14/R15/R7): they ride the same surface_alert path (Reason::Alert
            // / Tier::Cli, per the plan's KTD — no new attention producer), and
            // the KTD5 completion-raise suppression for the check pane itself
            // stays untouched: only the sink pane rings.
            automations_mgr.set_monitor_alert_sink(Arc::clone(&surface_alert));
            // Monitor failure bundles (monitor-handoff U3, R15) live under the
            // FLY_APP_NAME data root — durable, outside the run-output tail
            // cap, isolated per dev flavor like every other store.
            automations_mgr.set_bundle_dir(session::data_dir().join("monitor-bundles"));
            app.manage(alerts_log);
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
                    launch: &automations::ResolvedLaunch,
                ) -> Result<(), String> {
                    self.agent.dispatch_agent(a, run_id, launch)
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
            // U4a: give the manager the real config store so agent-launch
            // resolution reads the user's shared automation defaults (R12/R15).
            automations_mgr.set_config(app.state::<Arc<ConfigStore>>().inner().clone());
            // U4b: capture an agent run's final assistant turn into its run row
            // on close (R8). Resolve the run's transcript by cwd + dispatch time
            // (abstains on ambiguity — no cross-session leak), extract the last
            // assistant message, scrub secrets (an agent runs with
            // --dangerously-skip-permissions and may quote one), then strip
            // control chars. `close_run` tail-caps the result. The read RETRIES
            // (bounded): Claude Code flushes the final assistant turn to the
            // transcript ~100ms AFTER firing the Stop hook that triggers this
            // capture, so a single-shot read loses the race and records empty
            // output. Both close call sites run the capturer off their thread
            // (dispatch / PTY read), so the retry sleeps never stall dispatch.
            automations_mgr.set_output_capturer(Arc::new(
                |cwd: &str, dispatched_ms: u64| {
                    let text = session::transcript::capture_final_assistant_since(
                        std::path::Path::new(cwd),
                        dispatched_ms,
                        session::transcript::CAPTURE_ATTEMPTS,
                        session::transcript::CAPTURE_RETRY_DELAY,
                    )?;
                    // Order: sanitize → scrub → truncate (the feed io.rs::clean
                    // order; the accepted residual finding in docs/residual-
                    // review-findings/feat-feed-pending-question.md, applied
                    // here by monitor-handoff U3). Sanitizing FIRST means a
                    // control/zero-width char inside a token can't split it
                    // past the scrub and be re-formed into cleartext later;
                    // truncation is `close_run`'s tail cap, always last, so
                    // the scrub (and the U3 verdict parse) see the full text.
                    let sane = notify::sanitize_multiline(&text);
                    let scrubbed = automations::redact::scrub_secrets(&sane);
                    (!scrubbed.trim().is_empty()).then_some(scrubbed)
                },
            ));
            // U5: forward agent-run closes to the frontend so the tab lifecycle
            // (U8) can auto-close a succeeded run's tab or keep a failed one.
            let run_closed_handle = app.handle().clone();
            automations_mgr.set_run_closed_emitter(Arc::new(
                move |ev: &automations::RunClosedEvent| {
                    let _ = run_closed_handle.emit("automation://run-closed", ev);
                },
            ));
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
                // Only a *live* pane counts: `lifecycle(id)` is still `Some`
                // for an exited-but-not-yet-closed pane, which would strand the
                // automation (R7 alive-probe would read a dead pane as alive
                // and skip forever). Gate on `is_live()`.
                row.pane_id
                    .and_then(|id| pty_mgr.lifecycle(pty::PaneId(id)))
                    .is_some_and(|s| s.is_live())
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

            // Local read-only agent/automation feed (feat-agent-state-local-feed,
            // U6). FeedState is managed UNCONDITIONALLY so the publish command
            // never errors on a disabled feed — the pushed roster is just cached,
            // not served. The HTTP listener starts only when enabled and a bearer
            // token exists. A bind failure is non-fatal (never block launch): log
            // and continue without the feed (plan Open Question).
            let feed_state = Arc::new(feed::FeedState::new());
            app.manage(Arc::clone(&feed_state));
            let config_store = app.state::<Arc<ConfigStore>>().inner().clone();
            let feed_cfg = config_store.get().feed;
            if feed_cfg.enabled {
                match config::ensure_feed_token(&config_store) {
                    Some(token) => {
                        // Automations half of the snapshot: projected from the
                        // authoritative store at emit time (KTD4), so it needs no
                        // frontend push and stays live on its own.
                        let mgr_for_feed =
                            app.state::<Arc<automations::AutomationManager>>().inner().clone();
                        let automations_fn: feed::server::AutomationsFn = Arc::new(move || {
                            mgr_for_feed
                                .list()
                                .iter()
                                .map(feed::wire::AutomationEntry::from_automation)
                                .collect()
                        });
                        let now_fn: feed::server::NowFn = Arc::new(notify::now_unix_ms);
                        // Per-leaf IO resolver (feed-agent-reply-io U3;
                        // feed-pending-question U3/U4): one shared, cached
                        // instance behind the /output endpoint and the frame's
                        // lastReplyAt + questionPendingAt stamps (R3/R4) — one
                        // transcript read serves every surface.
                        let resolver =
                            Arc::new(feed::io::ReplyResolver::new(session::resume::resume_path()));
                        let io_fn: feed::server::IoFn =
                            Arc::new(move |leaf_key| resolver.resolve_io(leaf_key));
                        // Input delivery (U5; feed-pending-question U6): leaf
                        // → live pane → the action's bytes. Submit is the
                        // sanitized bracketed paste, then Enter as its OWN
                        // delayed write (a same-chunk \r is swallowed as
                        // pasted content — see `io::paste_payload`); Keys is
                        // the raw R9-filtered answer bytes, no wrap, no Enter
                        // (KTD6 — a digit selects a picker option instantly).
                        // BOTH end in the same attention clear local typing
                        // performs (`pty::pty_write`) — the agent was just
                        // answered, so its ring must drop, and a remote keys
                        // answer must not leave `reason: permission` stale
                        // (KTD4/R9). The sleep rides the HTTP connection
                        // thread, never a dispatch/PTY thread.
                        let input_fn: feed::server::InputFn = {
                            let pty = app.state::<Arc<PtyManager>>().inner().clone();
                            let attention = app.state::<Arc<AttentionManager>>().inner().clone();
                            let input_handle = app.handle().clone();
                            Arc::new(move |leaf_key, action| {
                                let Some(pane) = pty.pane_by_leaf(leaf_key) else {
                                    return feed::server::InputOutcome::UnknownPane;
                                };
                                let delivered = match &action {
                                    feed::server::InputAction::Submit(text) => pty
                                        .write(pane, &feed::io::paste_payload(text))
                                        .and_then(|()| {
                                            std::thread::sleep(feed::io::SUBMIT_DELAY);
                                            pty.write(pane, feed::io::SUBMIT)
                                        }),
                                    feed::server::InputAction::Keys(bytes) => {
                                        pty.write(pane, bytes)
                                    }
                                };
                                match delivered {
                                    Ok(()) => {
                                        if let Some(outcome) = attention.on_input(pane) {
                                            stream::emit_attention(&input_handle, pane, &outcome);
                                        }
                                        feed::server::InputOutcome::Delivered
                                    }
                                    Err(e) => feed::server::InputOutcome::Failed(e),
                                }
                            })
                        };
                        // Live read of the keys-answer permission opt-in
                        // (KTD6, default off) — a settings change applies to
                        // the next request without a restart.
                        let permission_answers_fn: feed::server::PermissionAnswersFn = {
                            let cfg = Arc::clone(&config_store);
                            Arc::new(move || cfg.get().feed.allow_permission_answers)
                        };
                        match feed::FeedServer::start(
                            feed_cfg.port,
                            token,
                            Arc::clone(&feed_state),
                            automations_fn,
                            now_fn,
                            io_fn,
                            input_fn,
                            permission_answers_fn,
                        ) {
                            Ok(server) => {
                                // An automation mutation bumps the feed so every
                                // connected consumer re-reads (KTD4).
                                let feed_for_changed = Arc::clone(&feed_state);
                                app.listen(automations::AUTOMATION_CHANGED_EVENT, move |_| {
                                    feed_for_changed.bump()
                                });
                                app.manage(server);
                            }
                            Err(e) => {
                                eprintln!("[fly] feed server disabled: bind failed: {e}");
                            }
                        }
                    }
                    None => {
                        eprintln!("[fly] feed server disabled: could not persist bearer token");
                    }
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            frontend_log,
            get_launch_mode,
            config::get_config,
            config::set_config,
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
            session::resume::save_session_pick,
            session::resume::reset_pane_attribution,
            session::resume::prune_resume_records,
            session::transcript::continue_target,
            session::transcript::qualifying_session_count,
            session::handoff::resolve_handoff_target,
            session::handoff::list_handoff_candidates,
            usage::usage_snapshot,
            automations::automations_frontend_ready,
            automations::list_automations,
            feed::publish_agent_feed,
            register_alert_sink,
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

/// Handle one authenticated `automation/*` socket request (U9). The thin
/// `AppHandle` wrapper: runs on a hook connection thread after token validation
/// (the pane is the validated caller), resolves the manager + the pane's
/// workspace + the recursion flag from app state, and delegates the actual
/// routing to [`dispatch_automation_op`] (which is AppHandle-free, so it is
/// integration-tested directly). Returns the response the server writes back.
fn handle_automation_request(
    app: &tauri::AppHandle,
    pane: pty::PaneId,
    buf: &[u8],
) -> cli::automation::AutomationResponse {
    use cli::automation::{AutomationRequest, AutomationResponse};

    let Some(mgr) = app.try_state::<Arc<automations::AutomationManager>>() else {
        return AutomationResponse::err("automations are unavailable");
    };
    let req: AutomationRequest = match serde_json::from_slice(buf) {
        Ok(r) => r,
        Err(e) => return AutomationResponse::err(format!("malformed request: {e}")),
    };
    let is_recursion = mgr.is_automation_pane(pane.0);
    // The pane's workspace stamps origin (R9) so an agent run's tab lands back
    // in it; falls back to empty (→ first workspace) if not yet replicated.
    let workspace_id = app
        .try_state::<Arc<AttentionManager>>()
        .and_then(|a| a.pane_workspace(pane))
        .unwrap_or_default();
    dispatch_automation_op(&mgr, pane.0, &workspace_id, is_recursion, req)
}

/// Route a parsed `automation/*` request to the manager (U9). AppHandle-free so
/// it is directly testable: the caller supplies the validated `pane_id`, the
/// pane's `workspace_id` (for origin stamping, R9), and whether the pane is
/// itself automation-spawned (`is_recursion`, the R22 gate). Enforces the gate
/// first, then routes create/pause/resume/run/delete to the manager.
pub fn dispatch_automation_op(
    mgr: &automations::AutomationManager,
    pane_id: u64,
    workspace_id: &str,
    is_recursion: bool,
    req: cli::automation::AutomationRequest,
) -> cli::automation::AutomationResponse {
    use automations::{CreateMode, CreateSpec, ManualRun};
    use cli::automation::AutomationResponse;

    // R22 recursion gate: a pane spawned by an automation may not create or
    // manage automations (the registry entry outlives a delete, cleared only on
    // the pane's exit, so create→delete can't un-gate a still-live pane).
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
            let mode = if let Some(prompt) = req.prompt {
                CreateMode::Agent {
                    prompt,
                    model: req.model,
                    effort: req.effort,
                }
            } else if let Some(content) = req.script {
                CreateMode::Script {
                    content,
                    interpreter: req.interpreter.unwrap_or_else(|| "bash".to_string()),
                    timeout_ms: req.timeout_ms.unwrap_or(120_000),
                }
            } else {
                return AutomationResponse::err("create requires a prompt or a script");
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
                retry_on_interrupt: req.retry_on_interrupt,
                // monitor-handoff U2: U5 threads `--not-before` through the
                // socket request; until then plain creates carry no floor.
                not_before_ms: None,
                origin,
            }) {
                Ok(created) => AutomationResponse::ok(Some(created.automation.id), created.warning),
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
                Ok(a) => AutomationResponse::ok(Some(a.id), None),
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
                Err(e) => AutomationResponse::err(e),
            },
            None => AutomationResponse::err("run requires an automation id"),
        },
        other => AutomationResponse::err(format!("unknown automation op {other:?}")),
    }
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
}
