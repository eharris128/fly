//! OS notification surfacing + window urgency (U11 backend half; U18 per-effect
//! split).
//!
//! All notification text is untrusted (it can originate from agent output), so
//! it is stripped of control characters and length-capped before display (R16).
//!
//! The desktop banner, the audible chime, and the history record are three
//! independent effects (KTD14): [`banner`] shows the OS notification *without*
//! sound, [`play_sound`] plays the chime on its own, and [`surface_actions`]
//! composes a raise's policy [`Effects`] with the rate-limit gate into the
//! concrete set of actions — so a foregrounded user can still hear a hidden
//! pane raise even when no banner shows.

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

use crate::state::policy::Effects;

const TITLE_CAP: usize = 120;
const BODY_CAP: usize = 400;
/// Minimum gap between OS notifications, so a looping agent can't thrash the
/// notification daemon or the window urgency hint (R16/U11). Applies to the
/// banner only; `record` and `sound` are decided independently (U18).
const MIN_INTERVAL_MS: u64 = 800;

/// Strip control characters and length-cap untrusted notification text (R16).
pub fn sanitize(text: &str, cap: usize) -> String {
    text.chars().filter(|c| !c.is_control()).take(cap).collect()
}

/// Sanitize untrusted text for a notification title (R16).
pub fn sanitize_title(text: &str) -> String {
    sanitize(text, TITLE_CAP)
}

/// Sanitize untrusted text for a notification body (R16).
pub fn sanitize_body(text: &str) -> String {
    sanitize(text, BODY_CAP)
}

/// Wall-clock milliseconds since the Unix epoch, stamped on a notification so
/// the frontend can sort and show relative times — and keep them stable across a
/// restart (the backend epoch-relative clock would not survive one).
pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Fire an OS notification (no sound — the chime is decoupled into
/// [`play_sound`], U18) and flash the window. Best-effort: with no notification
/// daemon present it no-ops rather than erroring.
pub fn banner(app: &AppHandle, title: &str, body: &str) {
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

/// Play an XDG theme sound by name, independent of any banner (the desktop and
/// sound effects are decoupled — KTD14/U18). Best-effort: if `canberra-gtk-play`
/// is absent the chime is silently skipped, like a missing notification daemon.
pub fn play_sound(name: &str) {
    if name.is_empty() {
        return;
    }
    let mut cmd = Command::new("canberra-gtk-play");
    cmd.arg("-i").arg(name);
    spawn_detached(cmd);
}

/// Spawn a best-effort background process and reap it on a short-lived thread,
/// so it never lingers as a zombie. Tauri/GTK may own process-global `SIGCHLD`,
/// so each child is `wait`ed explicitly rather than via a global reaper. Shared
/// by [`play_sound`] and the notification command runner ([`command`], U19).
pub(crate) fn spawn_detached(mut command: Command) {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Ok(mut child) = command.spawn() {
        let _ = std::thread::Builder::new()
            .name("fly-notify-reap".into())
            .spawn(move || {
                let _ = child.wait();
            });
    }
}

/// What to actually surface for an attention signal's desktop banner.
#[derive(Debug, PartialEq, Eq)]
pub enum Surfaced {
    Individual { title: String, body: String },
    /// Above the coalesce threshold — one summary notification.
    Coalesced { count: usize },
    /// Rate-limited; show nothing this time.
    Suppressed,
}

/// The concrete actions a recordable raise triggers (U18), composed from its
/// policy [`Effects`] and the banner gate's verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceActions {
    /// Emit `notification://added` — decoupled from the gate, so a coalesced or
    /// rate-limited banner still records every raise.
    pub record: bool,
    /// Fire the OS banner.
    pub banner: bool,
    /// Play the chime — follows `effects.sound`, independent of the banner.
    pub sound: bool,
    /// Run the opt-in notification command — rides the gated banner (U19).
    pub command: bool,
}

/// Compose a recordable raise's effects with the rate-limit gate's verdict.
///
/// The caller must avoid *calling* `gate.decide` when `effects.desktop` is false
/// (pass [`Surfaced::Suppressed`]) so a suppressed banner doesn't consume the
/// rate-limit slot — `banner` already requires `effects.desktop`, so the verdict
/// is only honored when a banner could fire.
pub fn surface_actions(effects: Effects, gate_verdict: &Surfaced) -> SurfaceActions {
    let banner = effects.desktop && !matches!(gate_verdict, Surfaced::Suppressed);
    SurfaceActions {
        record: effects.record,
        banner,
        sound: effects.sound,
        command: banner,
    }
}

/// Decides whether/how to fire an OS banner: coalesces when too many panes are
/// raised at once, and rate-limits the aggregate stream (U11). Also mints the
/// monotonic notification ids the frontend keys history on (U18).
pub struct NotificationGate {
    coalesce_threshold: usize,
    epoch: Instant,
    last_fire_ms: Mutex<Option<u64>>,
    id_counter: AtomicU64,
}

impl NotificationGate {
    pub fn new(coalesce_threshold: usize) -> Self {
        Self {
            coalesce_threshold,
            epoch: Instant::now(),
            last_fire_ms: Mutex::new(None),
            id_counter: AtomicU64::new(0),
        }
    }

    pub fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// A monotonic id for a recorded notification, so the frontend can key and
    /// dedupe history entries.
    pub fn next_id(&self) -> u64 {
        self.id_counter.fetch_add(1, Ordering::Relaxed)
    }

    /// Decide what to surface for one raised signal. `raised_count` is the
    /// number of panes currently needing attention. Consumes a rate-limit slot,
    /// so the caller only invokes it when a banner could actually fire.
    pub fn decide(&self, raised_count: usize, title: &str, body: &str, now_ms: u64) -> Surfaced {
        let mut last = self.last_fire_ms.lock().unwrap();
        if let Some(prev) = *last {
            if now_ms.saturating_sub(prev) < MIN_INTERVAL_MS {
                return Surfaced::Suppressed;
            }
        }
        *last = Some(now_ms);
        if raised_count > self.coalesce_threshold {
            Surfaced::Coalesced {
                count: raised_count,
            }
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
    use super::{sanitize, surface_actions, NotificationGate, Surfaced};
    use crate::state::policy::Effects;

    const ALL_ON: Effects = Effects {
        desktop: true,
        sound: true,
        record: true,
    };

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
        assert_eq!(gate.decide(4, "t", "b", 1000), Surfaced::Coalesced { count: 4 });
    }

    #[test]
    fn rate_limits_bursts() {
        let gate = NotificationGate::new(3);
        assert!(matches!(gate.decide(1, "t", "b", 0), Surfaced::Individual { .. }));
        assert_eq!(gate.decide(1, "t", "b", 100), Surfaced::Suppressed);
        assert!(matches!(gate.decide(1, "t", "b", 1000), Surfaced::Individual { .. }));
    }

    #[test]
    fn ids_are_monotonic() {
        let gate = NotificationGate::new(3);
        assert_eq!(gate.next_id(), 0);
        assert_eq!(gate.next_id(), 1);
        assert_eq!(gate.next_id(), 2);
    }

    #[test]
    fn record_is_decoupled_from_the_coalesced_banner() {
        // A coalesced banner still records every raise (the panel must be
        // complete). banner + sound + command all fire too.
        let a = surface_actions(ALL_ON, &Surfaced::Coalesced { count: 5 });
        assert!(a.record && a.banner && a.sound && a.command);
    }

    #[test]
    fn suppressed_banner_still_records_and_chimes() {
        // Hidden foregrounded pane: desktop off, but the chime + record stay
        // (the resolved audible-cue choice). No banner ⇒ no command.
        let effects = Effects {
            desktop: false,
            sound: true,
            record: true,
        };
        let a = surface_actions(effects, &Surfaced::Suppressed);
        assert!(a.record);
        assert!(!a.banner);
        assert!(a.sound);
        assert!(!a.command, "no banner ⇒ command does not ride");
    }

    #[test]
    fn sound_is_independent_of_the_banner() {
        // reason.sound=false drops only the chime under an otherwise-on banner.
        let effects = Effects {
            desktop: true,
            sound: false,
            record: true,
        };
        let a = surface_actions(effects, &Surfaced::Individual {
            title: "t".into(),
            body: "b".into(),
        });
        assert!(a.banner && a.command && a.record);
        assert!(!a.sound);
    }

    #[test]
    fn command_only_rides_a_firing_banner() {
        // A rate-limited (suppressed) banner does not run the command, but the
        // record + chime still go.
        let a = surface_actions(ALL_ON, &Surfaced::Suppressed);
        assert!(!a.banner && !a.command);
        assert!(a.record && a.sound);
    }
}
