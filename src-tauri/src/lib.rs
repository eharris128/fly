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

/// Where the hook socket lives — under the XDG runtime dir, keyed by pid so
/// instances don't collide (single-instance enforcement arrives in U14).
fn hook_socket_path() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("fly").join(format!("hook-{}.sock", std::process::id()))
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
    let pty_manager = Arc::new(PtyManager::new());
    let tokens = Arc::new(TokenRegistry::new());
    let attention = Arc::new(AttentionManager::new(config.get().attention_debounce_ms));
    let gate = Arc::new(NotificationGate::new(
        config.get().notification_coalesce_threshold,
    ));

    // Clones for the hook server's dispatch (the originals are managed below).
    let tokens_for_hooks = Arc::clone(&tokens);
    let attention_for_hooks = Arc::clone(&attention);

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
            let handle = app.handle().clone();
            let attention = attention_for_hooks;
            let gate = gate;
            let dispatch: Dispatch = Arc::new(move |pane, hook: ValidatedHook| {
                let signal = Signal {
                    reason: hook.reason,
                    tier: Tier::Hook,
                };
                if let Some(outcome) = attention.signal(pane, signal) {
                    stream::emit_attention(&handle, pane, &outcome);
                    if outcome.notify {
                        let title = hook.title.as_deref().unwrap_or("fly: an agent needs you");
                        let body = hook.body.as_deref().unwrap_or("");
                        // Coalesce when many panes are raised; rate-limit bursts.
                        match gate.decide(attention.raised_count(), title, body, gate.now_ms()) {
                            Surfaced::Individual { title, body } => {
                                notify::surface(&handle, &title, &body)
                            }
                            Surfaced::Coalesced { count } => notify::surface(
                                &handle,
                                "fly",
                                &format!("{count} agents need attention"),
                            ),
                            Surfaced::Suppressed => {}
                        }
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
            stream::set_pane_focus,
            stream::set_window_foreground,
            pty::pty_write,
            pty::pty_resize,
            pty::close_pane,
            pty::pty_pause,
            pty::pty_resume,
            pty::pane_cwd,
            session::save_session,
            session::load_session,
            session::save_scrollback,
            session::load_scrollback,
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
