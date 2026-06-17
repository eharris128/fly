//! fly — a terminal for AI coding agents.
//!
//! This library backs both the desktop app and the `fly` CLI subcommands
//! (KTD12); `main.rs` is a thin shim over [`run`].

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

/// Run the fly desktop application.
pub fn run() {
    #[cfg(target_os = "linux")]
    apply_linux_webview_env();

    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_store::Builder::default().build())
        .run(tauri::generate_context!())
        .expect("error while running fly");
}
