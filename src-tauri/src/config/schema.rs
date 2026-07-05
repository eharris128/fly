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
/// `error`/`alert`) from `Default`; combined with the same on
/// [`ReasonEffects`], a partial effect object inside a reason also fills.
/// Without both, `serde`'s `default` on the parent `Config` would only cover
/// the whole-key-absent case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReasonEffectsConfig {
    pub question: ReasonEffects,
    pub permission: ReasonEffects,
    pub finished: ReasonEffects,
    pub error: ReasonEffects,
    /// Automation-alert raises (automations U-ID U12, R18) — the first
    /// non-agent reason. The struct-level `#[serde(default)]` above means a
    /// config predating it loads unchanged with the all-on default.
    pub alert: ReasonEffects,
}

impl Default for ReasonEffectsConfig {
    /// All effects on for every reason — the no-surprise default.
    fn default() -> Self {
        Self {
            question: ReasonEffects::default(),
            permission: ReasonEffects::default(),
            finished: ReasonEffects::default(),
            error: ReasonEffects::default(),
            alert: ReasonEffects::default(),
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
            Reason::Alert => self.alert,
        }
    }
}

/// Shared defaults for automation **agent** runs (automations-workspace-and-
/// model plan, U3 — R12/R15). The dispatch resolution order is
/// automation → this shared default → Claude's own default (U4a);
/// `fallback_model` is handed to `--fallback-model` so an unattended run
/// degrades when the resolved primary is unavailable or over quota (omitted
/// when it equals the resolved primary).
///
/// The nested-serde recipe (see [`ReasonEffectsConfig`]): struct-level
/// `#[serde(default)]` here **plus** the parent `Config`'s container default
/// means a partial object like `{"model":"opus"}` still fills the omitted
/// `effort`/`fallbackModel` from `Default` instead of dropping them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AutomationDefaults {
    /// Shared default launch model (alias or full id); `None` ⇒ Claude default.
    pub model: Option<String>,
    /// Shared default reasoning effort; `None` ⇒ Claude default.
    pub effort: Option<String>,
    /// Model handed to `--fallback-model` for unattended over-quota runs (R15).
    /// Non-optional — there is always a fallback; default `sonnet`.
    pub fallback_model: String,
}

impl Default for AutomationDefaults {
    fn default() -> Self {
        Self {
            model: None,
            effort: None,
            fallback_model: "sonnet".into(),
        }
    }
}

/// Settings for the local, read-only agent/automation feed
/// (feat-agent-state-local-feed). The feed binds a **loopback-only** HTTP
/// listener guarded by a bearer `token`, so an external local consumer (the
/// `game` portfolio) can read what agents/automations are live.
///
/// The nested-serde recipe (see [`ReasonEffectsConfig`]): struct-level
/// `#[serde(default)]` here **plus** the parent `Config`'s container default
/// means a partial `{"port":5000}` still fills the omitted `enabled`/`token`
/// from `Default`. `token` is `None` until minted on first run (see
/// `config::ensure_feed_token`); a `127.0.0.1` listener is reachable by any
/// local process, so the token — not the bind — is what scopes the feed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct FeedConfig {
    /// Whether the feed listener starts at all. On by default for this
    /// single-user tool; a broad distribution should reconsider (plan Open Q).
    pub enabled: bool,
    /// Loopback TCP port the SSE endpoint binds.
    pub port: u16,
    /// Bearer token a consumer must present. `None` until minted + persisted on
    /// first run. Never logged.
    pub token: Option<String>,
}

impl Default for FeedConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: 4939,
            token: None,
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
    /// Show the notifications (🔔) icon in the control bar. On by default; the
    /// settings menu toggles it and persists the choice. Purely a chrome
    /// affordance — hiding it never disables notifications: the unread badge
    /// vanishes with the button, but `leader n` still opens the panel and OS
    /// notifications still surface.
    pub show_notifications_icon: bool,
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
    /// Shared default model / effort + fallback model for automation agent runs
    /// (automations-workspace-and-model U3, R12/R15). See [`AutomationDefaults`].
    pub automation_defaults: AutomationDefaults,
    /// Local read-only agent/automation feed (feat-agent-state-local-feed).
    /// See [`FeedConfig`].
    pub feed: FeedConfig,
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
            show_notifications_icon: true,
            notifications_muted_default: false,
            notification_sound: Some("message-new-instant".into()),
            notification_command: None,
            reason_effects: ReasonEffectsConfig::default(),
            resume_default_args: vec!["--dangerously-skip-permissions".into()],
            automation_defaults: AutomationDefaults::default(),
            feed: FeedConfig::default(),
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

    // U3 (R12/R15): an empty config loads the shared automation defaults —
    // fallbackModel "sonnet", model/effort None.
    #[test]
    fn empty_config_has_automation_defaults() {
        let c: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(c.automation_defaults.fallback_model, "sonnet");
        assert_eq!(c.automation_defaults.model, None);
        assert_eq!(c.automation_defaults.effort, None);
    }

    // U3 nested-serde gotcha: a *partial* automationDefaults object keeps the
    // omitted siblings at their defaults (fallbackModel stays "sonnet") rather
    // than dropping them — the struct-level `#[serde(default)]` at work.
    #[test]
    fn partial_automation_defaults_retains_sibling_defaults() {
        let c: Config =
            serde_json::from_str(r#"{"automationDefaults":{"model":"opus"}}"#).unwrap();
        assert_eq!(c.automation_defaults.model.as_deref(), Some("opus"));
        assert_eq!(
            c.automation_defaults.fallback_model, "sonnet",
            "omitted sibling kept its default"
        );
        assert_eq!(c.automation_defaults.effort, None);
    }

    // U3: the shared defaults round-trip under camelCase.
    #[test]
    fn automation_defaults_round_trip_camel_case() {
        let mut c = Config::default();
        c.automation_defaults.model = Some("opus".into());
        c.automation_defaults.effort = Some("high".into());
        c.automation_defaults.fallback_model = "haiku".into();
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["automationDefaults"]["model"], "opus");
        assert_eq!(v["automationDefaults"]["effort"], "high");
        assert_eq!(v["automationDefaults"]["fallbackModel"], "haiku");
        let back: Config = serde_json::from_value(v).unwrap();
        assert_eq!(back, c);
    }

    /// The notifications control-bar icon is shown by default; the settings menu
    /// toggles it. Guards the no-surprise default so a config predating the field
    /// (loaded via `#[serde(default)]`) still shows the bell.
    #[test]
    fn default_shows_notifications_icon() {
        assert!(Config::default().show_notifications_icon);
    }

    /// An older config file that omits `show_notifications_icon` still loads and
    /// falls back to the shown-by-default, rather than deserializing to `false`.
    #[test]
    fn missing_show_notifications_icon_defaults_shown() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert!(cfg.show_notifications_icon);
    }

    /// A config predating the feed block loads with the feed enabled on the
    /// default port and no token yet (minted on first run).
    #[test]
    fn empty_config_has_feed_defaults() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert!(cfg.feed.enabled);
        assert_eq!(cfg.feed.port, 4939);
        assert_eq!(cfg.feed.token, None);
    }

    /// The nested-serde gotcha: a *partial* `feed` object keeps omitted siblings
    /// at their defaults (enabled stays true) rather than dropping them.
    #[test]
    fn partial_feed_retains_sibling_defaults() {
        let cfg: Config = serde_json::from_str(r#"{"feed":{"port":5000}}"#).unwrap();
        assert_eq!(cfg.feed.port, 5000);
        assert!(cfg.feed.enabled, "omitted sibling kept its default");
        assert_eq!(cfg.feed.token, None);
    }

    /// An explicit `enabled: false` is preserved across a load (the disable
    /// switch is honored, not reset by the default).
    #[test]
    fn feed_disabled_is_preserved() {
        let cfg: Config = serde_json::from_str(r#"{"feed":{"enabled":false}}"#).unwrap();
        assert!(!cfg.feed.enabled);
    }

    /// The feed block round-trips under camelCase.
    #[test]
    fn feed_round_trips_camel_case() {
        let mut c = Config::default();
        c.feed.port = 5050;
        c.feed.token = Some("abc123".into());
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["feed"]["port"], 5050);
        assert_eq!(v["feed"]["token"], "abc123");
        let back: Config = serde_json::from_value(v).unwrap();
        assert_eq!(back, c);
    }
}
