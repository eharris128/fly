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
pub mod peer;
pub mod pty;
pub mod session;
pub mod state;
pub mod stream;
pub mod substrate;
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

/// Where the hook socket lives — under the XDG runtime dir at a **stable,
/// per-flavor path** (tmux-substrate plan U2/KTD8). Stability is load-bearing:
/// substrate sessions outlive the fly process, and their agents' hooks hold
/// `FLY_SOCKET_PATH` in long-lived process env — a PID-keyed path (the
/// pre-substrate scheme) would strand every surviving agent on restart.
/// Same-flavor duplicate instances are prevented by the single-instance
/// plugin; the bind path additionally refuses to unlink a socket that still
/// answers (see `HookServer` — the ga-h9z lesson applied to our own socket).
fn hook_socket_path() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join(app_dir_name()).join("hook.sock")
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
    // KTD10 (tmux-substrate U3): when the rollout flag selects tmux, every
    // leaf-keyed spawn becomes a marked session on the flavor server. The
    // substrate handle shares the flavor name with the hook-socket dir and
    // the FIFOs live beside that socket (same runtime-dir lifetime class).
    if cfg.substrate == config::SubstrateKind::Tmux {
        let runtime_dir = hook_socket_path()
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir);
        pty_manager.set_substrate(Arc::new(substrate::Substrate::new(
            app_dir_name(),
            session::data_dir().join("substrate-sessions.json"),
            runtime_dir,
        )));
    }
    let tokens = Arc::new(TokenRegistry::new());
    let attention = Arc::new(AttentionManager::new(
        cfg.attention_debounce_ms,
        cfg.notifications_muted_default,
    ));
    let gate = Arc::new(NotificationGate::new(cfg.notification_coalesce_threshold));
    // Ask-time raise stamps (feed-question-screen-fallback U3/KTD5): the hook
    // dispatch stamps question/permission raises; the feed's screen fallback
    // reads them as a screen-derived question's `askedAt`. Created
    // unconditionally (like FeedState) so stamping never depends on the feed
    // listener being enabled.
    let pending_signals = Arc::new(feed::pending::PendingSignals::new());

    // Clones for the hook server's dispatch (the originals are managed below).
    let tokens_for_hooks = Arc::clone(&tokens);
    let attention_for_hooks = Arc::clone(&attention);
    // The dispatch resolves PaneId → leaf_key to key resume records (U3).
    let pty_for_hooks = Arc::clone(&pty_manager);
    let config_for_hooks = cfg;
    let signals_for_hooks = Arc::clone(&pending_signals);

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
        // Per-pane output coalescers (performance-audit T1): spawn_pane
        // registers, set_visible_panes retunes flush deadlines.
        .manage(Arc::new(stream::coalesce::CoalescerRegistry::default()))
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
            // Held permission asks (hook-ask-channel U8): the registry is
            // managed unconditionally (like FeedState) — registration works
            // whether or not the feed listener is enabled, and lifecycle
            // shutdown releases every held hook (R9). A second Arc feeds the
            // ask handler below; the feed wiring further down clones from the
            // managed state.
            let ask_registry = Arc::new(feed::ask::AskRegistry::new());
            app.manage(Arc::clone(&ask_registry));
            let pty_for_ask = Arc::clone(&pty);
            // Clones for the peer-messaging handler below (agent-peer-messaging
            // U2/U6) — taken before the dispatch closure consumes the originals.
            let pty_for_peer = Arc::clone(&pty);
            let signals_for_peer = Arc::clone(&signals_for_hooks);
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

                // Ask-time stamp (feed-question-screen-fallback U3/KTD5): a
                // fresh question/permission raise records the wall-clock ask
                // time, keyed by the pane's stable leaf key. This is the
                // screen-derived question's `askedAt` — the value v2.1.206's
                // transcript can no longer provide at ask time. Debounced
                // duplicates were returned above, so a re-notify inside the
                // debounce window never moves an armed guard's stamp.
                if matches!(reason, Reason::Question | Reason::Permission) {
                    if let Some(leaf_key) = pty.leaf_key(pane) {
                        signals_for_hooks.stamp(&leaf_key, notify::now_unix_ms());
                    }
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
            // Held-ask registration (hook-ask-channel U8): resolve the pane to
            // its leaf, upsert the resume record at Hook rank (KTD6 — the ask
            // payload carries session_id/cwd exactly like a raising hook), and
            // register. Register and clear both bump the feed so a frame moves
            // without any roster change (R3); FeedState is resolved lazily via
            // try_state (managed later in this setup, same pattern as the
            // automation handler). Runs on the socket's per-connection thread;
            // touches only the registry lock and the resume file.
            let ask_handle = app.handle().clone();
            let ask_registry_for_hooks = Arc::clone(&ask_registry);
            let ask_handler: hooks::AskHandler = Arc::new(move |pane, payload| {
                let leaf_key = pty_for_ask.leaf_key(pane)?;
                if let Some(session_id) = payload.session_id.clone() {
                    let _ = session::resume::upsert_at(
                        &session::resume::resume_path(),
                        &leaf_key,
                        session::resume::ResumePartial {
                            session_id: Some(session_id),
                            session_cwd: payload.cwd.clone(),
                            session_source: Some(session::resume::SessionSource::Hook),
                            ..Default::default()
                        },
                    );
                }
                let (gen, rx) =
                    ask_registry_for_hooks.register(&leaf_key, payload, notify::now_unix_ms())?;
                if let Some(feed) = ask_handle.try_state::<Arc<feed::FeedState>>() {
                    feed.bump();
                }
                let drop_handle = ask_handle.clone();
                let drop_registry = Arc::clone(&ask_registry_for_hooks);
                let on_drop = Box::new(move || {
                    if drop_registry.clear_if(&leaf_key, gen) {
                        if let Some(feed) = drop_handle.try_state::<Arc<feed::FeedState>>() {
                            feed.bump();
                        }
                    }
                });
                Some(hooks::AskTicket {
                    decision_rx: rx,
                    on_drop,
                })
            });
            // Peer messaging (agent-peer-messaging U2/U6): the `peer/list` /
            // `peer/send` handler. FeedState/AttentionManager resolve lazily
            // via try_state (managed later in this setup — the automation
            // handler's pattern); the question resolver is this handler's own
            // FallbackResolver over the same seams the feed wires (shared
            // PendingSignals + AskRegistry Arcs, so the two never disagree on
            // a held ask), because the feed's instance is built only inside
            // the feed-enabled branch. The rate buckets are the KTD8 brake —
            // in-memory, reset on restart.
            let peer_handle = app.handle().clone();
            let peer_screen_pty = Arc::clone(&pty_for_peer);
            let peer_screen: feed::fallback::ScreenFn =
                Arc::new(move |leaf| peer_screen_pty.screen_tail_by_leaf(leaf));
            let peer_asks = Arc::clone(&ask_registry);
            let peer_ask_fn: feed::fallback::AskFn = Arc::new(move |leaf| peer_asks.get(leaf));
            let peer_resolver = Arc::new(feed::fallback::FallbackResolver::new(
                session::resume::resume_path(),
                signals_for_peer,
                peer_screen,
                peer_ask_fn,
            ));
            let peer_buckets = Arc::new(std::sync::Mutex::new(peer::rate::Buckets::new()));
            // Peer deliveries serialize behind one mutex (the plan's open
            // question, resolved): delivery is two writes a settle apart, so
            // two concurrent sends to one pane would otherwise splice —
            // paste A, paste B, Enter, Enter — into a merged composer line.
            // App-wide rather than per-leaf: the path is rate-limited to a
            // trickle, and the coarser lock cannot deadlock or leak an entry.
            let peer_delivery_lock = Arc::new(std::sync::Mutex::new(()));
            let peer_handler: hooks::PeerHandler = Arc::new(move |pane, buf: &[u8]| {
                handle_peer_request(
                    &peer_handle,
                    pane,
                    buf,
                    &pty_for_peer,
                    &peer_resolver,
                    &peer_buckets,
                    &peer_delivery_lock,
                )
                .to_bytes()
            });
            let server = HookServer::start_full(
                hook_socket_path(),
                tokens_for_hooks,
                dispatch,
                Some(automation_handler),
                Some(ask_handler),
                Some(peer_handler),
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
            // Headless monitor-check runner (headless-monitor-checks U3/U5):
            // owns every monitor check's `claude -p` child — spawn, stream,
            // deadline, kill. Same Weak-manager closer pattern as the
            // ScriptRunner above; the closer routes into the manager's
            // run-id-keyed close (`close_headless_run`: sanitize → scrub via
            // the shared redact helpers, then the one shared verdict-close
            // tail), so a headless verdict retires through exactly the
            // mutation a pane verdict does (R9). `run()` itself never errors:
            // every failure, spawn included, closes through this closer as an
            // infra Failed row (a post-shutdown close simply drops with the
            // Weak, like the script reaper's).
            let headless_closer_mgr = Arc::downgrade(&automations_mgr);
            let headless_runner = Arc::new(automations::headless::HeadlessRunner::new(Arc::new(
                move |automation_id: &str,
                      run_id: &str,
                      outcome: automations::headless::CheckOutcome| {
                    if let Some(mgr) = headless_closer_mgr.upgrade() {
                        mgr.close_headless_run(automation_id, run_id, outcome);
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
            automations_mgr.set_dispatcher(Arc::new(CompositeDispatcher {
                agent: agent_dispatcher,
                script: Arc::clone(&script_runner),
                headless: Arc::clone(&headless_runner),
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
                    // here by monitor-handoff U3). The composition lives in the
                    // ONE shared helper (headless-monitor-checks U5, R8) —
                    // `close_headless_run` cleans through it too, so the order
                    // invariant (control chars first, or one inside a token
                    // splits it past the scrub; the tail cap last, inside
                    // `close_run` after the verdict parse) cannot drift
                    // between the pane and headless capture paths.
                    automations::redact::clean_captured(&text)
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
            // monitor-handoff U4 (R13's backend half): after a successful
            // monitor create's store flush, tell the frontend which pane
            // registered it so the parent tab can close (U6 maps pane → leaf
            // → tab; an already-closed pane is a no-op there).
            let monitor_registered_handle = app.handle().clone();
            automations_mgr.set_monitor_registered_emitter(Arc::new(
                move |ev: &automations::MonitorRegisteredEvent| {
                    let _ =
                        monitor_registered_handle.emit(automations::MONITOR_REGISTERED_EVENT, ev);
                },
            ));
            let killer_runner = Arc::clone(&script_runner);
            automations_mgr
                .set_script_killer(Arc::new(move |run_id: &str| killer_runner.kill_run(run_id)));
            let capacity_runner = Arc::clone(&script_runner);
            automations_mgr.set_script_capacity(Arc::new(move || capacity_runner.has_capacity()));
            // Headless seams (headless-monitor-checks U4/U5 — R5/R7), all
            // resolving against the runner's in-flight registry: the killer
            // rides the delete / shutdown / backstop kill legs (SIGTERM →
            // short seam grace → SIGKILL + descendant sweep); the alive probe
            // widens the overlap check so a terminal-but-alive child blocks
            // the next claim and manual runs; the deadline gate is the
            // backstop's suspend-proof monotonic check (epoch age alone can
            // lapse across a laptop suspend while the check is healthy).
            let headless_killer = Arc::clone(&headless_runner);
            automations_mgr.set_headless_killer(Arc::new(move |run_id: &str| {
                headless_killer.kill_run(run_id)
            }));
            let headless_alive = Arc::clone(&headless_runner);
            automations_mgr.set_headless_check_alive(Arc::new(move |automation_id: &str| {
                headless_alive.automation_check_alive(automation_id)
            }));
            let headless_gate = Arc::clone(&headless_runner);
            automations_mgr.set_headless_deadline_gate(Arc::new(move |run_id: &str| {
                headless_gate.monotonic_deadline_lapsed(run_id)
            }));
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
            // Usage-limit-deferral U4: the plan-usage gate. The sweep consults
            // it before the store lock and only on a tick that could claim an
            // agent-mode occurrence (KTD2/KTD-C); the gate itself is the
            // OAuth-backed fetch (short timeout + TTL cache) over the same
            // request core as the dashboard gauges, fail-open on every
            // uncertainty (KTD3).
            let usage_gate = Arc::new(usage::gate::OauthUsageGate::new());
            automations_mgr.set_usage_gate(Arc::new(move |now_ms: u64| {
                usage_gate.defer_floor(now_ms)
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
                        // Headless-agent-automations U4 (R11): the entry's
                        // `headless` bit is the *effective* disposition, so
                        // the config default is read per projection (emit
                        // time, off any lock) exactly like the claim's
                        // pre-lock read.
                        let cfg_for_feed = Arc::clone(&config_store);
                        let automations_fn: feed::server::AutomationsFn = Arc::new(move || {
                            let headless_default =
                                cfg_for_feed.get().automation_defaults.headless;
                            mgr_for_feed
                                .list()
                                .iter()
                                .map(|a| {
                                    feed::wire::AutomationEntry::from_automation(
                                        a,
                                        headless_default,
                                    )
                                })
                                .collect()
                        });
                        let now_fn: feed::server::NowFn = Arc::new(notify::now_unix_ms);
                        // Per-leaf IO resolver (feed-agent-reply-io U3;
                        // feed-pending-question U3/U4): one shared, cached
                        // instance behind the /output endpoint and the frame's
                        // lastReplyAt + questionPendingAt stamps (R3/R4) — one
                        // transcript read serves every surface. Wrapped by the
                        // screen fallback (feed-question-screen-fallback U5):
                        // transcript primary, screen-derived question behind
                        // it for v2.1.206's flush-at-resolve transcripts, fed
                        // by each pane's output tail ring.
                        let pty_for_screen = app.state::<Arc<PtyManager>>().inner().clone();
                        let screen_fn: feed::fallback::ScreenFn =
                            Arc::new(move |leaf_key| pty_for_screen.screen_tail_by_leaf(leaf_key));
                        // Held asks feed the resolver's primary leg
                        // (hook-ask-channel U6/KTD3).
                        let ask_registry_for_io =
                            app.state::<Arc<feed::ask::AskRegistry>>().inner().clone();
                        let ask_fn: feed::fallback::AskFn =
                            Arc::new(move |leaf_key| ask_registry_for_io.get(leaf_key));
                        let resolver = Arc::new(feed::fallback::FallbackResolver::new(
                            session::resume::resume_path(),
                            Arc::clone(&pending_signals),
                            screen_fn,
                            ask_fn,
                        ));
                        let io_fn: feed::server::IoFn = Arc::new(move |leaf_key, reason, status| {
                            resolver.resolve_io(leaf_key, reason, status)
                        });
                        // Input delivery (U5; feed-pending-question U6): leaf
                        // → live pane → the action's bytes. Submit is the
                        // sanitized bracketed paste, then Enter as its OWN
                        // delayed write (a same-chunk \r is swallowed as
                        // pasted content — see `io::paste_payload`); Keys is
                        // the raw R9-filtered answer bytes, no wrap, no Enter
                        // (KTD6 — a digit selects a picker option instantly);
                        // Other (feed-other-answer) is the three-chunk picker
                        // free-text choreography below. ALL end in the same
                        // attention clear local typing
                        // performs (`pty::pty_write`) — the agent was just
                        // answered, so its ring must drop, and a remote keys
                        // answer must not leave `reason: permission` stale
                        // (KTD4/R9). The sleep rides the HTTP connection
                        // thread, never a dispatch/PTY thread.
                        let input_fn: feed::server::InputFn = {
                            let pty = app.state::<Arc<PtyManager>>().inner().clone();
                            let attention = app.state::<Arc<AttentionManager>>().inner().clone();
                            let input_handle = app.handle().clone();
                            let asks_for_input =
                                app.state::<Arc<feed::ask::AskRegistry>>().inner().clone();
                            Arc::new(move |leaf_key, action| {
                                let Some(pane) = pty.pane_by_leaf(leaf_key) else {
                                    return feed::server::InputOutcome::UnknownPane;
                                };
                                // Decision delivery (hook-ask-channel U8/KTD5):
                                // resolved through the held connection, never
                                // the PTY. The registry's atomic stamp check is
                                // the last line against the local-answer race —
                                // Gone maps to 409, the "already resolved"
                                // outcome. A delivered decision clears pane
                                // attention exactly like a typed answer, and
                                // the ask's removal bumps the feed so the
                                // pending marker drops on the next frame.
                                if let feed::server::InputAction::Decision { allow, if_asked_at } =
                                    &action
                                {
                                    return match asks_for_input.answer(
                                        leaf_key,
                                        *if_asked_at,
                                        *allow,
                                    ) {
                                        feed::ask::AnswerOutcome::Delivered => {
                                            if let Some(outcome) = attention.on_input(pane) {
                                                stream::emit_attention(
                                                    &input_handle,
                                                    pane,
                                                    &outcome,
                                                );
                                            }
                                            if let Some(feed) =
                                                input_handle.try_state::<Arc<feed::FeedState>>()
                                            {
                                                feed.bump();
                                            }
                                            feed::server::InputOutcome::Delivered
                                        }
                                        feed::ask::AnswerOutcome::Gone => {
                                            feed::server::InputOutcome::Conflict
                                        }
                                    };
                                }
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
                                    // Other answer (feed-other-answer KTD1):
                                    // digit → text → Enter as three separate,
                                    // delay-spaced chunks. The boundaries are
                                    // load-bearing — a digit coalesced with
                                    // the text is dropped by the picker and
                                    // the Enter would then select the
                                    // highlighted default (probed live) — and
                                    // no chunk carries an ESC byte, so nothing
                                    // can read as a picker cancel.
                                    feed::server::InputAction::Other { select, text } => pty
                                        .write(pane, select)
                                        .and_then(|()| {
                                            std::thread::sleep(feed::io::SUBMIT_DELAY);
                                            pty.write(pane, text)
                                        })
                                        .and_then(|()| {
                                            std::thread::sleep(feed::io::SUBMIT_DELAY);
                                            pty.write(pane, feed::io::SUBMIT)
                                        }),
                                    // Returned early above — never a PTY write.
                                    feed::server::InputAction::Decision { .. } => unreachable!(),
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
                        // Phone-drop delivery (phone-screenshot-drop U5/U6).
                        // The guard sequence itself lives in
                        // `feed::drop::deliver_with_guards`, which is unit
                        // tested; this closure only supplies the real
                        // implementations of its four seams. The order it
                        // enforces — resolve, identity, foreground probe,
                        // publish, paste, re-probe, Enter — is load-bearing:
                        // the image must be published after every refusal
                        // check (so a refusal leaves no residue) but before any
                        // text reaches the pane (so the agent is never told to
                        // read a path that has not been renamed into place).
                        // Like the input seam, the sleep rides the HTTP
                        // connection thread, never a dispatch/PTY thread, and a
                        // delivered drop clears pane attention exactly as local
                        // typing does.
                        let drop_fn: feed::server::DropFn = {
                            let pty = app.state::<Arc<PtyManager>>().inner().clone();
                            let attention = app.state::<Arc<AttentionManager>>().inner().clone();
                            let drop_handle = app.handle().clone();
                            Arc::new(move |leaf_key, delivery| {
                                let outcome = feed::drop::deliver_with_guards(
                                    delivery.expect_pane,
                                    delivery.text,
                                    || pty.pane_by_leaf(leaf_key).map(|p| p.0),
                                    |pane| pty.is_agent(crate::pty::PaneId(pane)),
                                    |pane, bytes| pty.write(crate::pty::PaneId(pane), bytes),
                                    || std::thread::sleep(feed::io::SUBMIT_DELAY),
                                    delivery.commit,
                                );
                                // The drop was answered into the pane, so its
                                // ring must drop just as a typed reply's would.
                                if matches!(outcome, feed::drop::DropOutcome::Delivered) {
                                    if let Some(pane) = pty.pane_by_leaf(leaf_key) {
                                        if let Some(o) = attention.on_input(pane) {
                                            stream::emit_attention(&drop_handle, pane, &o);
                                        }
                                    }
                                }
                                outcome
                            })
                        };
                        // The drop directory, prepared once. A failure here is
                        // retained as `None` and reported per request as
                        // `storageFailed` — it must never keep the feed (and
                        // the dashboard that reads it) from starting (AE8).
                        let drop_store = {
                            let raw = feed_cfg.drop_dir.clone();
                            let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
                            match feed::drop::resolve_drop_dir(
                                raw.as_deref(),
                                home.as_deref(),
                                &session::data_dir(),
                            ) {
                                Ok(dir) => match feed::drop::DropStore::new(&dir) {
                                    Ok(s) => Some(Arc::new(s)),
                                    Err(e) => {
                                        log::warn!(
                                            "phone drop directory {} unusable: {e}",
                                            dir.display()
                                        );
                                        None
                                    }
                                },
                                Err(e) => {
                                    log::warn!("feed.dropDir is not usable: {e}");
                                    None
                                }
                            }
                        };
                        let drop_cfg = feed::server::DropConfig {
                            deliver: drop_fn,
                            store: drop_store,
                            max_bytes: u64::from(feed_cfg.drop_max_bytes),
                            expected_tailnet_login: feed_cfg.expected_tailnet_login.clone(),
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
                            drop_cfg,
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
            pty::panes_status,
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
            session::transcript::resolve_resume_spawn_cwd,
            session::handoff::resolve_handoff_target,
            session::handoff::list_handoff_candidates,
            usage::usage_snapshot,
            automations::automations_frontend_ready,
            automations::list_automations,
            automations::delete_automation,
            automations::monitor_pickup_check,
            automations::read_monitor_bundle,
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
    // monitor-handoff U4 (R11): the pickup-pointer resolver for a monitor
    // create. Pane-precise by construction — it starts from the *validated
    // socket caller's* leaf key (the wire can never self-declare pointers)
    // and runs the exact handoff qualification (plausibility gate,
    // record-cwd-wins, ≥1 real transcript turn) via the shared
    // [`session::handoff::resolve_target_now`]. The request's `cwd` (the
    // CLI's own cwd, i.e. the pane's) is only the derivation fallback for a
    // record that never captured its cwd — the record's cwd wins. Invoked by
    // the dispatch core only for monitor creates, so ordinary ops never pay
    // the store/transcript read.
    let pty = app.try_state::<Arc<PtyManager>>().map(|s| s.inner().clone());
    let live_cwd = req.cwd.clone();
    let resolve_pointers = move || -> Option<automations::model::MonitorPointers> {
        let leaf_key = pty.as_ref()?.leaf_key(pane)?;
        monitor_pointers_from_target(
            session::handoff::resolve_target_now(&leaf_key, live_cwd.as_deref()),
            live_cwd.as_deref(),
        )
    };
    dispatch_automation_op(
        &mgr,
        pane.0,
        &workspace_id,
        is_recursion,
        req,
        &resolve_pointers,
    )
}

/// Peer-op entry (agent-peer-messaging U2/U6): parse the request, assemble
/// the live ports, and route to the pure gate sequence
/// (`peer::dispatch_peer_op` — where the ordering is unit-tested). `origin`
/// is the socket's token-resolved pane (KTD2); the wire never carries a
/// sender. Runs on the per-connection thread — the delivery settle sleep and
/// the question resolution both belong there, never on a dispatch/PTY thread.
fn handle_peer_request(
    app: &tauri::AppHandle,
    origin: pty::PaneId,
    buf: &[u8],
    pty: &Arc<PtyManager>,
    resolver: &Arc<feed::fallback::FallbackResolver>,
    buckets: &Arc<std::sync::Mutex<peer::rate::Buckets>>,
    delivery_lock: &Arc<std::sync::Mutex<()>>,
) -> cli::peer::PeerResponse {
    use cli::peer::{PeerRequest, PeerResponse};

    let req: PeerRequest = match serde_json::from_slice(buf) {
        Ok(r) => r,
        Err(_) => return PeerResponse::err("badRequest"),
    };
    // FeedState is managed unconditionally later in setup; a request can only
    // arrive from a pane, which exists only after setup completed.
    let Some(feed_state) = app
        .try_state::<Arc<feed::FeedState>>()
        .map(|s| s.inner().clone())
    else {
        return PeerResponse::err("unavailable");
    };

    let roster = || feed_state.roster();
    let leaf_for_pane = |p: u64| pty.leaf_key(pty::PaneId(p));
    let try_take = |pane: u64, at: u64| {
        buckets
            .lock()
            .map(|mut b| b.try_take(pane, at))
            .unwrap_or(false)
    };
    // The wide question gate (KTD5): existence/reason/status from ONE roster
    // snapshot (`agent_gate`), the drop route's own predicate behind it. A
    // leaf with no roster row can't be blocked — but it was already refused
    // `notOptedIn` before this gate runs.
    let ask_pending = |leaf: &str| match feed_state.agent_gate(leaf) {
        None => false,
        Some(gate) => {
            let resolved = resolver.resolve_io(leaf, gate.reason.as_deref(), &gate.status);
            feed::server::drop_blocked_by_question(resolved.question, gate.reason.as_deref())
        }
    };
    // Guarded delivery (KTD4): `deliver_with_guards` with a no-op commit —
    // there is nothing to publish on this path; the parameter exists so the
    // drop route's ordering stays owned in one place. The delivery mutex
    // spans both writes so concurrent sends can't splice a composer line. A
    // delivered message clears the recipient's ring exactly as local typing
    // would.
    let deliver = |expect: u64, leaf: &str, text: &str| {
        let _serialized = delivery_lock.lock().unwrap_or_else(|p| p.into_inner());
        let outcome = feed::drop::deliver_with_guards(
            expect,
            text,
            || pty.pane_by_leaf(leaf).map(|p| p.0),
            |pane| pty.is_agent(pty::PaneId(pane)),
            |pane, bytes| pty.write(pty::PaneId(pane), bytes),
            || std::thread::sleep(feed::io::SUBMIT_DELAY),
            || Ok(()),
        );
        if matches!(outcome, feed::drop::DropOutcome::Delivered) {
            if let Some(attention) = app.try_state::<Arc<AttentionManager>>() {
                if let Some(pane) = pty.pane_by_leaf(leaf) {
                    if let Some(o) = attention.on_input(pane) {
                        stream::emit_attention(app, pane, &o);
                    }
                }
            }
        }
        outcome
    };

    peer::dispatch_peer_op(
        origin.0,
        &req,
        &peer::PeerPorts {
            now_ms: notify::now_unix_ms(),
            roster: &roster,
            leaf_for_pane: &leaf_for_pane,
            try_take_rate: &try_take,
            ask_pending: &ask_pending,
            deliver: &deliver,
        },
    )
}

/// monitor-handoff U4 (R11): flatten a qualified [`session::handoff::HandoffTarget`]
/// into the [`automations::model::MonitorPointers`] stored on a monitor. The
/// pointer cwd is the **record's** captured cwd when it has one (the same
/// R12-precedence the transcript path was derived under), else the live-cwd
/// fallback the derivation actually used — so the stored cwd and transcript
/// path always cohere. No target, or no cwd from anywhere, abstains to `None`
/// (→ the R12 refusal). Pure, so it is unit-tested below without an app.
fn monitor_pointers_from_target(
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
/// `dyn` so the routing tests below can inject a recorder without a Tauri
/// `AppHandle`.
struct CompositeDispatcher {
    agent: std::sync::Arc<dyn automations::Dispatcher>,
    script: std::sync::Arc<automations::script::ScriptRunner>,
    headless: std::sync::Arc<automations::headless::HeadlessRunner>,
}

impl automations::Dispatcher for CompositeDispatcher {
    fn dispatch_agent(
        &self,
        a: &automations::model::Automation,
        run_id: &str,
        launch: &automations::ResolvedLaunch,
        headless: bool,
    ) -> Result<(), String> {
        // Headless-agent-automations U3: the fork widens from `a.monitor` to
        // the claimed row's marker (threaded by the manager) — a monitor's
        // marker is always true, and a regular agent claim carries its
        // resolved disposition (R1/R2), so one branch serves both.
        if headless {
            // Headless-monitor-checks R1: the check is backend-owned — hand
            // it to the runner and return Ok. `run()` returns fast (it never
            // blocks the sweep on the child) and routes EVERY failure, spawn
            // included, through its CheckCloser as an infra Failed close —
            // so post-handoff there is no dispatch error left to surface
            // here: `Ok` means "handed off", not "succeeded". A spawn
            // failure thus feeds the broken-monitor escalation as a Failed
            // close (via `close_run_stamping`'s Failed-close escalation leg)
            // rather than as the manager's dispatch-Err close — the same R7
            // accounting either way, only the error's carrier differs
            // (`row.error` instead of a recompute-and-close on Err).
            self.headless.run(a, run_id, launch);
            return Ok(());
        }
        self.agent.dispatch_agent(a, run_id, launch, headless)
    }
    fn dispatch_script(
        &self,
        a: &automations::model::Automation,
        run_id: &str,
    ) -> Result<(), String> {
        self.script.dispatch_script(a, run_id)
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
    /// `AgentDispatcher` needs a Tauri `AppHandle`, which no unit test has —
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
    fn fork_harness() -> (CompositeDispatcher, Arc<RecordingAgentArm>, CheckCloses) {
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
        let d = CompositeDispatcher {
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
