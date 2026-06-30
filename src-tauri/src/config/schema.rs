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
    /// Try WebGL, falling back to the DOM renderer on construction failure or a
    /// context-loss event. Opt-in, *not* the default: with multiple panes each
    /// holding a live WebGL context, WebKitGTK fails to keep them all composited
    /// and an inactive pane blanks until it next repaints. The KTD6 eviction
    /// policy meant to bound live contexts was never built, so WebGL is opt-in.
    Auto,
    /// Force WebGL (same WebKitGTK multi-context caveat as [`Renderer::Auto`]).
    Webgl,
    /// Force the DOM renderer — the default. No GL context to contend, so panes
    /// never blank; the cost is no GPU-accelerated glyph rendering (U4 flow
    /// control already bounds output floods, so this is rarely the bottleneck).
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
    /// Idle delay in ms before the attention-triage nudge appears once the
    /// focused agent stops needing you (R16). Integer so `Config` keeps `Eq`.
    pub nudge_idle_ms: u32,
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
            nudge_idle_ms: 1500,
            notification_coalesce_threshold: 3,
            osc_bel_fallback: false,
            // DOM by default (KTD6, superseded): WebGL blanks inactive panes on
            // WebKitGTK with multiple live contexts. WebGL is opt-in (auto/webgl).
            renderer: Renderer::Dom,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The default renderer is DOM, not WebGL (KTD6, superseded). Multiple live
    /// WebGL contexts blank inactive panes on WebKitGTK and the KTD6 eviction
    /// policy was never built, so DOM is the safe default; WebGL stays opt-in via
    /// `auto`/`webgl`. Guards against an accidental revert to the blanking default.
    #[test]
    fn default_renderer_is_dom() {
        assert_eq!(Config::default().renderer, Renderer::Dom);
    }
}
