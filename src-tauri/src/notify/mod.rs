//! OS notification surfacing + window urgency (U11 backend half).
//!
//! All notification text is untrusted (it can originate from agent output), so
//! it is stripped of control characters and length-capped before display (R16).

use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

const TITLE_CAP: usize = 120;
const BODY_CAP: usize = 400;

/// Strip control characters and length-cap untrusted notification text (R16).
pub fn sanitize(text: &str, cap: usize) -> String {
    text.chars().filter(|c| !c.is_control()).take(cap).collect()
}

/// Fire an OS notification and flash the window. Best-effort: with no
/// notification daemon present it no-ops rather than erroring.
pub fn surface(app: &AppHandle, title: &str, body: &str) {
    let title = sanitize(title, TITLE_CAP);
    let body = sanitize(body, BODY_CAP);
    let _ = app
        .notification()
        .builder()
        .title(title)
        .body(body)
        .show();
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.request_user_attention(Some(tauri::UserAttentionType::Critical));
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize;

    #[test]
    fn strips_control_chars_and_caps_length() {
        assert_eq!(sanitize("hi\x1b[31m\x07there\n", 100), "hi[31mthere");
        assert_eq!(sanitize(&"x".repeat(50), 10).len(), 10);
    }
}
