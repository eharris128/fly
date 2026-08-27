//! Per-effect notification policy (KTD14, KTD15) — the pure suppression core
//! that replaces the single-boolean `should_notify` matrix (absorbed from the
//! former `suppress` module).
//!
//! A raise produces three **independent** effects — `desktop` (the OS banner),
//! `sound` (the audible chime), and `record` (the history entry) — each decided
//! separately from the same input tuple. This mirrors cmux's
//! `desktop`/`sound`/`record` hook-effect model in fly-native typed config (no
//! `cmux.json`, no shell-chain). Like its sibling [`super::attention`] it is a
//! free function over explicit inputs — no clock, no I/O — so every row of the
//! decision matrix is unit-tested.

use serde::{Deserialize, Serialize};

/// Which effects a single attention `Reason` is even eligible to produce — the
/// user-configured AND-mask. A `false` here forces that effect off regardless
/// of runtime state (e.g. `desktop=false` ⇒ never banner on `finished`;
/// `record=false` ⇒ skip history for a noisy reason). All default on; the
/// persisted form is `config::ReasonEffectsConfig` (U23), one of these per
/// `Reason`. `#[serde(default)]` so a *partial* `{"desktop": false}` in a
/// config file fills the omitted effects from `Default` (the nested-fill case).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReasonEffects {
    pub desktop: bool,
    pub sound: bool,
    pub record: bool,
}

impl Default for ReasonEffects {
    /// All-effects-on — the no-surprise default (the user opts *out* per reason).
    fn default() -> Self {
        Self {
            desktop: true,
            sound: true,
            record: true,
        }
    }
}

/// The three independent things a raise can do, computed by [`decide`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Effects {
    /// Fire an OS banner (desktop banners are for when you are *away*).
    pub desktop: bool,
    /// Play the audible "an agent needs you" chime.
    pub sound: bool,
    /// Append to the notification history / panel.
    pub record: bool,
}

/// The full situation a raise is decided against: the reason's configured
/// effect mask plus the runtime user/app state replicated from the frontend
/// (visibility, foreground, panel-open, mute), mirroring the existing
/// `set_pane_focus` / `set_window_foreground` replication (KTD16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyInputs {
    /// The reason's configured per-effect eligibility (the AND-mask).
    pub reason: ReasonEffects,
    /// The raising pane sits in the active tab of the active workspace (cmux's
    /// "workspace active"). NB a pane can be `pane_visible` while the window is
    /// backgrounded — the tab is active but the window isn't focused.
    pub pane_visible: bool,
    /// The app window is foregrounded (the user is "in the app").
    pub window_foregrounded: bool,
    /// The notification panel is open.
    pub panel_open: bool,
    /// Global do-not-disturb is on.
    pub muted_global: bool,
    /// The raising pane's workspace is muted.
    pub muted_workspace: bool,
}

/// Whether the user is *actually looking at* this pane right now: it is visible
/// **and** the window is foregrounded. This is the predicate the attention
/// machine uses for the Acknowledged transition (U17), and the `read`-at-birth
/// bit the dispatch path stamps on a recorded notification (KTD16).
///
/// It is the **negation** of the old `suppress::should_notify`: `should_notify`
/// answered "fire a banner?" (true *unless* looking); this answers "is the user
/// looking?" (true *only when* looking). The polarity is flipped on purpose —
/// `is_user_viewing(true, true) == true` where `should_notify(true, true)` was
/// `false`.
pub fn is_user_viewing(pane_visible: bool, window_foregrounded: bool) -> bool {
    pane_visible && window_foregrounded
}

/// Decide the three effects for a raise from the full input tuple (KTD14/KTD15).
///
/// Each rule, tied to the cmux-parity intent it implements:
///
/// - **`record`** — append to history *unless the reason opts out*. Mute does
///   **not** kill it (the panel must stay complete so it can back-fill a
///   suppressed banner); only `reason.record=false` removes it.
/// - **`desktop`** — the OS banner is for when you are *away*. Suppressed
///   whenever the window is foregrounded (the in-app ring + unread badges carry
///   attention instead), whenever muted, and gated by `reason.desktop`.
///   Panel-open suppresses the banner *only while foregrounded* (KTD15) — which
///   the foreground term already covers — so a user who left the panel open and
///   walked away (backgrounded) still gets a banner.
/// - **`sound`** — the audible cue, **decoupled from `desktop`** (resolved scope
///   decision): it still plays for a raise on a pane the user is **not**
///   actively viewing, even while foregrounded, so multi-agent users keep the
///   signal while working in fly. It is suppressed only when an in-app surface
///   is already showing the raise — the user is foregrounded *and* either
///   looking at the pane *or* has the panel open — and when muted, gated by
///   `reason.sound`.
pub fn decide(inputs: PolicyInputs) -> Effects {
    let muted = inputs.muted_global || inputs.muted_workspace;

    // History is complete by design: it survives mute and foreground; only the
    // reason can opt a noisy event out.
    let record = inputs.reason.record;

    // Banner = away-only. Foreground or mute suppresses; panel-open only matters
    // while foregrounded, already subsumed by the foreground term (so an
    // away-with-panel-open user still banners — KTD15).
    let desktop = inputs.reason.desktop && !muted && !inputs.window_foregrounded;

    // Chime plays unless an in-app surface already shows the raise (foregrounded
    // AND viewing-this-pane-or-panel-open) or muted. This is the deliberate
    // decoupling from `desktop`: a foregrounded user with a hidden pane raising
    // (panel closed) still hears it.
    let in_app_surface =
        inputs.window_foregrounded && (inputs.pane_visible || inputs.panel_open);
    let sound = inputs.reason.sound && !muted && !in_app_surface;

    Effects {
        desktop,
        sound,
        record,
    }
}

#[cfg(test)]
mod tests {
    use super::{decide, is_user_viewing, Effects, PolicyInputs, ReasonEffects};

    /// Build inputs with an all-effects-on reason and the given runtime tuple,
    /// so each test reads as one row of the KTD14/KTD15 matrix.
    fn inputs(
        pane_visible: bool,
        window_foregrounded: bool,
        panel_open: bool,
        muted_global: bool,
        muted_workspace: bool,
    ) -> PolicyInputs {
        PolicyInputs {
            reason: ReasonEffects::default(),
            pane_visible,
            window_foregrounded,
            panel_open,
            muted_global,
            muted_workspace,
        }
    }

    const ALL: Effects = Effects {
        desktop: true,
        sound: true,
        record: true,
    };
    const RECORD_ONLY: Effects = Effects {
        desktop: false,
        sound: false,
        record: true,
    };

    // ---- the decision matrix (the contract; pinned per row) ----

    #[test]
    fn away_notifies_fully() {
        // window backgrounded, all reason effects on, nothing muted.
        assert_eq!(decide(inputs(false, false, false, false, false)), ALL);
    }

    #[test]
    fn in_app_hidden_pane_keeps_the_chime() {
        // Foregrounded + pane not visible + panel closed + unmuted.
        // The *one* deliberate deviation from cmux's printed matrix: the audible
        // cue survives so a multi-agent user working in fly still hears a
        // background pane raise (resolved scope decision). Banner stays off.
        assert_eq!(
            decide(inputs(false, true, false, false, false)),
            Effects {
                desktop: false,
                sound: true,
                record: true,
            }
        );
    }

    #[test]
    fn pane_visible_is_silent_and_bannerless() {
        // Foregrounded + visible (the user is looking) → record only (the caller
        // stamps it read-at-birth).
        assert_eq!(decide(inputs(true, true, false, false, false)), RECORD_ONLY);
    }

    #[test]
    fn panel_open_while_foregrounded_suppresses_banner_and_chime() {
        // Panel open + foregrounded → an in-app surface shows the raise: no
        // banner (foreground), no chime (panel). Matches matrix row 4 even for a
        // hidden pane.
        assert_eq!(decide(inputs(false, true, true, false, false)), RECORD_ONLY);
    }

    #[test]
    fn panel_open_but_backgrounded_still_alerts() {
        // KTD15: panel-open suppresses only while foregrounded. A user who left
        // the panel open and walked away still gets the full away alert. (This is
        // the row the stray U16 "panel_open backgrounded → desktop false" test
        // line got wrong; KTD15 + the matrix table + U17 are authoritative.)
        assert_eq!(decide(inputs(false, false, true, false, false)), ALL);
    }

    #[test]
    fn global_mute_suppresses_banner_and_chime_but_records() {
        // muted_global, backgrounded → desktop+sound off, record survives.
        assert_eq!(decide(inputs(false, false, false, true, false)), RECORD_ONLY);
    }

    #[test]
    fn workspace_mute_matches_global_for_that_pane() {
        assert_eq!(decide(inputs(false, false, false, false, true)), RECORD_ONLY);
    }

    // ---- the per-reason / per-effect AND-mask (independently gated) ----

    #[test]
    fn per_reason_desktop_off_does_not_drag_down_sound() {
        // reason.desktop=false silences the banner; under the resolved decoupled
        // model `sound` is its own switch and still fires (backgrounded raise).
        let mut i = inputs(false, false, false, false, false);
        i.reason = ReasonEffects {
            desktop: false,
            sound: true,
            record: true,
        };
        assert_eq!(
            decide(i),
            Effects {
                desktop: false,
                sound: true,
                record: true,
            }
        );
    }

    #[test]
    fn per_reason_sound_off_keeps_the_banner() {
        // reason.sound=false drops only the chime under an otherwise-on banner
        // (cmux "suppress sounds independently").
        let mut i = inputs(false, false, false, false, false);
        i.reason = ReasonEffects {
            desktop: true,
            sound: false,
            record: true,
        };
        assert_eq!(
            decide(i),
            Effects {
                desktop: true,
                sound: false,
                record: true,
            }
        );
    }

    #[test]
    fn per_reason_record_off_skips_history() {
        // reason.record=false removes the one effect mute can't (noisy-reason
        // opt-out), even on a fully-away raise.
        let mut i = inputs(false, false, false, false, false);
        i.reason = ReasonEffects {
            desktop: true,
            sound: true,
            record: false,
        };
        assert_eq!(
            decide(i),
            Effects {
                desktop: true,
                sound: true,
                record: false,
            }
        );
    }

    // ---- is_user_viewing: the absorbed suppress quadrants, polarity flipped ----

    #[test]
    fn is_user_viewing_is_the_flipped_should_notify() {
        // Old `should_notify` four-quadrant test, ported with **flipped**
        // expectations (the sign-error magnet from KTD14):
        //   should_notify(true,  true)  == false → is_user_viewing == true
        //   should_notify(true,  false) == true  → is_user_viewing == false
        //   should_notify(false, true)  == true  → is_user_viewing == false
        //   should_notify(false, false) == true  → is_user_viewing == false
        assert!(is_user_viewing(true, true)); // visible + foregrounded → looking
        assert!(!is_user_viewing(true, false)); // window backgrounded
        assert!(!is_user_viewing(false, true)); // pane not visible
        assert!(!is_user_viewing(false, false)); // neither
    }
}
