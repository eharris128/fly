//! OS notification surfacing + window urgency (U11 backend half).
//!
//! All notification text is untrusted (it can originate from agent output), so
//! it is stripped of control characters and length-capped before display (R16).

use std::sync::Mutex;
use std::time::Instant;

use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

const TITLE_CAP: usize = 120;
const BODY_CAP: usize = 400;
/// XDG sound-theme name played with each notification so an agent's "I need
/// you" is audible, not just visual. A standard freedesktop sound present in
/// the default and Yaru themes; the desktop maps it to the `sound-name` hint
/// (requires the DE's event sounds to be enabled).
const NOTIFICATION_SOUND: &str = "message-new-instant";
/// Minimum gap between OS notifications, so a looping agent can't thrash the
/// notification daemon or the window urgency hint (R16/U11).
const MIN_INTERVAL_MS: u64 = 800;

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
        .sound(NOTIFICATION_SOUND)
        .show();
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.request_user_attention(Some(tauri::UserAttentionType::Critical));
    }
}

/// What to actually surface for an attention signal.
#[derive(Debug, PartialEq, Eq)]
pub enum Surfaced {
    Individual { title: String, body: String },
    /// Above the coalesce threshold — one summary notification.
    Coalesced { count: usize },
    /// Rate-limited; show nothing this time.
    Suppressed,
}

/// Decides whether/how to fire an OS notification: coalesces when too many
/// panes are raised at once, and rate-limits the aggregate stream (U11).
pub struct NotificationGate {
    coalesce_threshold: usize,
    epoch: Instant,
    last_fire_ms: Mutex<Option<u64>>,
}

impl NotificationGate {
    pub fn new(coalesce_threshold: usize) -> Self {
        Self {
            coalesce_threshold,
            epoch: Instant::now(),
            last_fire_ms: Mutex::new(None),
        }
    }

    pub fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// Decide what to surface for one raised signal. `raised_count` is the
    /// number of panes currently needing attention.
    pub fn decide(
        &self,
        raised_count: usize,
        title: &str,
        body: &str,
        now_ms: u64,
    ) -> Surfaced {
        let mut last = self.last_fire_ms.lock().unwrap();
        if let Some(prev) = *last {
            if now_ms.saturating_sub(prev) < MIN_INTERVAL_MS {
                return Surfaced::Suppressed;
            }
        }
        *last = Some(now_ms);
        if raised_count > self.coalesce_threshold {
            Surfaced::Coalesced { count: raised_count }
        } else {
            Surfaced::Individual {
                title: title.to_string(),
                body: body.to_string(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{sanitize, NotificationGate, Surfaced};

    #[test]
    fn strips_control_chars_and_caps_length() {
        assert_eq!(sanitize("hi\x1b[31m\x07there\n", 100), "hi[31mthere");
        assert_eq!(sanitize(&"x".repeat(50), 10).len(), 10);
    }

    #[test]
    fn coalesces_above_the_threshold() {
        let gate = NotificationGate::new(3);
        assert_eq!(
            gate.decide(2, "t", "b", 0),
            Surfaced::Individual {
                title: "t".into(),
                body: "b".into()
            }
        );
        // 4 raised > threshold 3 → coalesced summary.
        assert_eq!(gate.decide(4, "t", "b", 1000), Surfaced::Coalesced { count: 4 });
    }

    #[test]
    fn rate_limits_bursts() {
        let gate = NotificationGate::new(3);
        assert!(matches!(gate.decide(1, "t", "b", 0), Surfaced::Individual { .. }));
        // Within the min interval → suppressed.
        assert_eq!(gate.decide(1, "t", "b", 100), Surfaced::Suppressed);
        // After the interval → fires again.
        assert!(matches!(gate.decide(1, "t", "b", 1000), Surfaced::Individual { .. }));
    }
}
