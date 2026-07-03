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

pub mod command;

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

use crate::state::attention::Reason;
use crate::state::policy::Effects;

const TITLE_CAP: usize = 120;
const BODY_CAP: usize = 400;
/// Minimum gap between OS notifications, so a looping agent can't thrash the
/// notification daemon or the window urgency hint (R16/U11). Applies to the
/// banner only; `record` and `sound` are decided independently (U18).
const MIN_INTERVAL_MS: u64 = 800;

/// Characters dropped from untrusted display text (R16 posture), shared by
/// [`sanitize`] (OS-notification / alert text) and the session pick-list snippet
/// sanitizer so a hardening can't land in one and be missed in the other:
///
/// - **control characters (Cc)** — `char::is_control`; strips the ESC that
///   begins an ANSI/OSC escape sequence, defanging it in a rendered line;
/// - **format characters (Cf)** — *not* covered by `char::is_control`: bidi
///   overrides/isolates (`U+202A`–`U+202E`, `U+2066`–`U+2069`) that visually
///   reorder a line, and zero-width / joiner chars (`U+200B`–`U+200F`,
///   `U+2060`–`U+2064`, `U+FEFF`) that hide or disguise content. Left in, a
///   transcript could spoof its own picker row to defeat the human
///   disambiguation step (Svelte escaping already blocks XSS, so this is
///   display-only).
pub(crate) fn is_stripped_char(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{200B}'..='\u{200F}'   // zero-width space/non-joiner/joiner, LRM/RLM/ALM
            | '\u{202A}'..='\u{202E}' // bidi embeddings / overrides
            | '\u{2060}'..='\u{2064}' // word joiner, invisible math operators
            | '\u{2066}'..='\u{2069}' // bidi isolates
            | '\u{FEFF}',             // zero-width no-break space / BOM
        )
}

/// Strip control/format characters and length-cap untrusted notification text
/// (R16); see [`is_stripped_char`] for the shared character policy.
pub fn sanitize(text: &str, cap: usize) -> String {
    text.chars()
        .filter(|&c| !is_stripped_char(c))
        .take(cap)
        .collect()
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

/// A human-readable subtitle for a reason, used as `FLY_NOTIFICATION_SUBTITLE`
/// for the notification command (the reason's default title — KTD17/U19).
pub fn reason_subtitle(reason: Reason) -> &'static str {
    match reason {
        Reason::Question => "An agent is waiting for you",
        Reason::Permission => "An agent needs permission",
        Reason::Finished => "An agent finished",
        Reason::Error => "An agent hit an error",
        // The first non-agent reason (automations U-ID U12, R18/KTD-H).
        Reason::Alert => "An automation raised an alert",
    }
}

/// Body text for a coalesced banner: `"N panes need attention"`. "Panes", not
/// "agents" — alert raises count toward the coalesce tally too, and their
/// producer is an automation, not an agent (automations U-ID U12, KTD-H: the
/// reason contract widened to "why this pane is raised for the user").
pub fn coalesced_body(count: usize) -> String {
    format!("{count} panes need attention")
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
    // Handle ignored; the cap bounds the fan-out (see `spawn_detached`).
    spawn_detached(cmd);
}

/// Max concurrent in-flight detached helper processes (the chime). Without it, N
/// panes raising at once would fan out N `canberra-gtk-play` processes with no
/// aggregate bound — a milder version of the unbounded fan-out the command
/// runner caps (KTD17). The per-pane attention debounce throttles a single
/// looping pane; this caps the across-pane burst.
const MAX_DETACHED: usize = 8;

fn detached_inflight() -> &'static AtomicUsize {
    static INFLIGHT: AtomicUsize = AtomicUsize::new(0);
    &INFLIGHT
}

/// Spawn a best-effort background process, bounded by a concurrency cap and
/// reaped on a short-lived thread so it never lingers as a zombie. Tauri/GTK may
/// own process-global `SIGCHLD`, so each child is `wait`ed explicitly rather than
/// via a global reaper. Returns the reaper handle (tests await it); callers
/// ignore it. Used by [`play_sound`] (the notification command runner has its
/// own equivalent cap in [`command`], U19).
pub(crate) fn spawn_detached(command: Command) -> Option<JoinHandle<()>> {
    spawn_detached_capped(command, detached_inflight(), MAX_DETACHED)
}

fn spawn_detached_capped(
    mut command: Command,
    inflight: &'static AtomicUsize,
    cap: usize,
) -> Option<JoinHandle<()>> {
    if inflight.fetch_add(1, Ordering::SeqCst) >= cap {
        inflight.fetch_sub(1, Ordering::SeqCst);
        return None;
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match command.spawn() {
        Ok(mut child) => match std::thread::Builder::new()
            .name("fly-notify-reap".into())
            .spawn(move || {
                let _ = child.wait();
                inflight.fetch_sub(1, Ordering::SeqCst);
            }) {
            Ok(handle) => Some(handle),
            Err(_) => {
                inflight.fetch_sub(1, Ordering::SeqCst);
                None
            }
        },
        Err(_) => {
            inflight.fetch_sub(1, Ordering::SeqCst);
            None
        }
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
            // Seed the id from wall-clock, NOT 0 — the id doubles as the
            // frontend's persisted dedup key, so a counter that reset to 0 each
            // launch would mint ids colliding with the *restored* history and
            // `addNotification` would silently drop the first N new
            // notifications after every restart-with-history. Wall-clock seeding
            // keeps the live-id space disjoint from any restored id: notification
            // count grows far slower than wall-clock ms (the per-pane debounce
            // caps it), so a later launch's base always exceeds the prior
            // session's max id.
            id_counter: AtomicU64::new(now_unix_ms()),
        }
    }

    pub fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// A monotonic, wall-clock-seeded id for a recorded notification, unique
    /// across process restarts so the frontend can key + dedupe history without
    /// colliding with restored entries.
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
    use super::{
        coalesced_body, reason_subtitle, sanitize, surface_actions, NotificationGate, Surfaced,
    };
    use crate::state::attention::Reason;
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
    fn strips_bidi_and_zero_width_format_chars() {
        // Format (Cf) chars — bidi overrides, zero-width — pass char::is_control
        // but can spoof how untrusted text renders (R16); the shared
        // is_stripped_char policy drops them here as well as in the pick-list.
        assert_eq!(sanitize("a\u{202e}b\u{200b}c\u{2066}d\u{feff}e", 100), "abcde");
    }

    /// The alert reason has its own subtitle naming the automation producer
    /// (automations U-ID U12, R18) — never a misleading "agent" phrase.
    #[test]
    fn alert_subtitle_names_the_automation_producer() {
        assert_eq!(reason_subtitle(Reason::Alert), "An automation raised an alert");
    }

    /// The coalesced banner counts *panes*, not "agents" — alert raises (from
    /// automations, not agents) are in the tally too (automations U12/KTD-H).
    #[test]
    fn coalesced_body_counts_panes_not_agents() {
        assert_eq!(coalesced_body(4), "4 panes need attention");
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
    fn ids_are_monotonic_and_wall_clock_seeded() {
        let gate = NotificationGate::new(3);
        let a = gate.next_id();
        let b = gate.next_id();
        let c = gate.next_id();
        assert_eq!(b, a + 1);
        assert_eq!(c, b + 1);
        // Seeded from wall-clock (not 0), so the live-id space never collides
        // with a restored history's ids after a restart. 1.7e12 ms ≈ 2023.
        assert!(a >= 1_700_000_000_000, "id should be wall-clock-seeded, got {a}");
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

    #[test]
    fn detached_spawns_are_bounded_and_reaped() {
        // The chime path (play_sound -> spawn_detached) must not fan out
        // unboundedly when many panes raise at once; extra spawns are dropped
        // and every child is reaped (no zombies, no slot leak).
        use std::process::Command;
        use std::sync::atomic::{AtomicUsize, Ordering};
        static INFLIGHT: AtomicUsize = AtomicUsize::new(0);
        let mut handles = Vec::new();
        for _ in 0..20 {
            let mut cmd = Command::new("sleep");
            cmd.arg("0.3");
            if let Some(h) = super::spawn_detached_capped(cmd, &INFLIGHT, 8) {
                handles.push(h);
            }
        }
        assert!(handles.len() <= 8, "detached spawns capped, got {}", handles.len());
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(INFLIGHT.load(Ordering::SeqCst), 0, "all slots released, no leak");
    }
}
