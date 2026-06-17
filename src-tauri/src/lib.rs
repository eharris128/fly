//! fly — a terminal for AI coding agents.
//!
//! This library backs both the desktop app and the `fly` CLI subcommands
//! (KTD12); `main.rs` is a thin shim over [`run`].

pub mod cli;
pub mod config;
pub mod cwd;
pub mod hooks;
pub mod notify;
pub mod pty;
pub mod state;
pub mod stream;

use std::path::PathBuf;
use std::sync::Arc;

use tauri::Manager;

use config::ConfigStore;
use hooks::{Dispatch, HookServer, TokenRegistry, ValidatedHook};
use pty::PtyManager;
use state::attention::{Signal, Tier};
use state::AttentionManager;

/// Apply Linux-specific webview workarounds before the webview initializes.
///
/// The WebKitGTK DMABUF renderer causes blank windows on Wayland/NVIDIA
/// (KTD11). Disabling it is the documented fix. We only set it when the user
/// hasn't already chosen a value, so it stays overridable from the environment.
#[cfg(target_os = "linux")]
fn apply_linux_webview_env() {
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
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

    // Clones for the hook server's dispatch (the originals are managed below).
    let tokens_for_hooks = Arc::clone(&tokens);
    let attention_for_hooks = Arc::clone(&attention);

    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_store::Builder::default().build())
        .manage(config)
        .manage(pty_manager)
        .manage(tokens)
        .manage(attention)
        .setup(move |app| {
            let handle = app.handle().clone();
            let attention = attention_for_hooks;
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
                        notify::surface(&handle, title, body);
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running fly");
}
