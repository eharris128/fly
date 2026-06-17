//! fly — a terminal for AI coding agents.
//!
//! This library backs both the desktop app and the `fly` CLI subcommands
//! (KTD12); `main.rs` is a thin shim over [`run`].

pub mod cli;
pub mod hooks;
pub mod pty;
pub mod state;
pub mod stream;

use std::sync::Arc;

use pty::PtyManager;

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

    let pty_manager = Arc::new(PtyManager::new());

    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_store::Builder::default().build())
        .manage(pty_manager)
        .invoke_handler(tauri::generate_handler![
            frontend_log,
            stream::spawn_pane,
            pty::pty_write,
            pty::pty_resize,
            pty::close_pane,
        ])
        .run(tauri::generate_context!())
        .expect("error while running fly");
}
