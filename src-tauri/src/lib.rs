//! fly — a terminal for AI coding agents.
//!
//! This library backs both the desktop app and the `fly` CLI subcommands
//! (KTD12); `main.rs` is a thin shim over [`run`].

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

use std::path::PathBuf;
use std::sync::Arc;

use tauri::Manager;

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

/// Run the fly desktop application — or a `fly` CLI subcommand if argv selects
/// one (KTD12).
pub fn run() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(first) = args.get(1) {
        if cli::is_cli_subcommand(first) {
            std::process::exit(cli::run(&args));
        }
    }

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
            // The per-effect dispatch (U18): decouple the in-app ring, the
            // history record, the desktop banner, and the chime — each decided
            // independently by the policy (KTD14), not fused behind one boolean.
            let dispatch: Dispatch = Arc::new(move |pane, hook: ValidatedHook| {
                let reason = hook.reason;
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
                            notify::banner(&handle, "fly", &format!("{count} agents need attention"))
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
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            frontend_log,
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
            pty::pane_activity,
            session::save_session,
            session::load_session,
            session::save_scrollback,
            session::load_scrollback,
            session::resume::load_resume_records,
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
