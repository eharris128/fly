//! Typed user-settings schema with defaults (U13).
//!
//! `#[serde(default)]` means a partial or empty config file still loads — every
//! missing field falls back to its default, so adding fields never breaks an
//! older config.

use serde::{Deserialize, Serialize};

use crate::state::attention::Reason;
use crate::state::policy::ReasonEffects;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Renderer {
    /// WebGL with a DOM fallback (KTD6) — the default.
    Auto,
    /// Force WebGL.
    Webgl,
    /// Force the DOM renderer.
    Dom,
}

/// Per-reason notification effect masks (KTD14, U23): one [`ReasonEffects`] per
/// attention [`Reason`]. The persisted form the policy reads — `decide` ANDs the
/// matching reason's mask onto the runtime decision.
///
/// `#[serde(default)]` here (struct-level) makes a *partial* object such as
/// `{"question": {...}}` fill the omitted reasons (`permission`/`finished`/
/// `error`) from `Default`; combined with the same on [`ReasonEffects`], a
/// partial effect object inside a reason also fills. Without both, `serde`'s
/// `default` on the parent `Config` would only cover the whole-key-absent case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReasonEffectsConfig {
    pub question: ReasonEffects,
    pub permission: ReasonEffects,
    pub finished: ReasonEffects,
    pub error: ReasonEffects,
}

impl Default for ReasonEffectsConfig {
    /// All effects on for every reason — the no-surprise default.
    fn default() -> Self {
        Self {
            question: ReasonEffects::default(),
            permission: ReasonEffects::default(),
            finished: ReasonEffects::default(),
            error: ReasonEffects::default(),
        }
    }
}

impl ReasonEffectsConfig {
    /// The configured effect mask for a given reason.
    pub fn for_reason(&self, reason: Reason) -> ReasonEffects {
        match reason {
            Reason::Question => self.question,
            Reason::Permission => self.permission,
            Reason::Finished => self.finished,
            Reason::Error => self.error,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    /// Leader key for app actions (tmux-style), e.g. "ctrl+a" (U6).
    pub leader_key: String,
    /// Attention debounce window in ms (KTD8).
    pub attention_debounce_ms: u64,
    /// Above this many simultaneously-raised panes, coalesce notifications (U11).
    pub notification_coalesce_threshold: usize,
    /// Generic OSC/BEL attention fallback for hookless agents — deferred, off.
    pub osc_bel_fallback: bool,
    /// Renderer choice (KTD6).
    pub renderer: Renderer,
    /// xterm.js scrollback cap in lines (KTD4).
    pub scrollback_lines: usize,
    /// Terminal font size in px. Integer so `Config` keeps `Eq`.
    pub font_size: u16,
    /// Persist scrollback across restart — off by default for privacy (KTD10).
    pub save_scrollback: bool,
    /// Start with global do-not-disturb on (R17). Per-workspace mute is runtime
    /// only in v1; this is the one mute startup default.
    pub notifications_muted_default: bool,
    /// XDG sound-theme name played with a surfaced notification, or `None` for
    /// silent (R23). Default `message-new-instant` (the freedesktop sound used
    /// since v1). Configurable so a user can pick another theme sound or mute it.
    pub notification_sound: Option<String>,
    /// Opt-in user command run on each surfaced notification, receiving
    /// sanitized `FLY_NOTIFICATION_{TITLE,SUBTITLE,BODY}` env vars (R23, KTD17).
    /// `None` (default) = disabled. See `notify::command` for the env/quoting
    /// contract.
    pub notification_command: Option<String>,
    /// Per-reason, per-effect notification mask (R18). All effects on by default.
    pub reason_effects: ReasonEffectsConfig,
    /// Flag floor replayed when resuming an agent whose launch argv was not
    /// captured (renderer crash, or a pane the poll never saw) — R8/KTD-C.
    /// Defaults to `--dangerously-skip-permissions` so the permission posture is
    /// never silently lost on resume (Claude drops it across `--resume`, #21974).
    pub resume_default_args: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            leader_key: "ctrl+a".into(),
            attention_debounce_ms: 400,
            notification_coalesce_threshold: 3,
            osc_bel_fallback: false,
            renderer: Renderer::Auto,
            scrollback_lines: 10_000,
            font_size: 15,
            save_scrollback: false,
            notifications_muted_default: false,
            notification_sound: Some("message-new-instant".into()),
            notification_command: None,
            reason_effects: ReasonEffectsConfig::default(),
            resume_default_args: vec!["--dangerously-skip-permissions".into()],
        }
    }
}
