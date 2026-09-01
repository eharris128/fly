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
    /// WebGL on **visible** panes with dispose-on-hide eviction — the default
    /// since perf-audit T4 (2026-08-08). The frontend attaches a `WebglAddon`
    /// only while a pane sits in the active tab and disposes it when the pane
    /// hides (`lib/renderer.ts` + `Terminal.svelte`), so live GL contexts stay
    /// bounded at one tab's panes and the historical WebKitGTK many-context
    /// blanking cannot accumulate. Construction failure or a context-loss
    /// event drops that pane to the DOM renderer for the rest of its life.
    Auto,
    /// Force a persistent WebGL context per pane, visible or not — the
    /// pre-eviction behavior, kept as a debugging switch. This is the mode
    /// with the multi-context caveat: WebKitGTK fails to keep many live
    /// contexts composited and an inactive pane blanks until it repaints.
    Webgl,
    /// Force the DOM renderer everywhere. No GL context at all, but the
    /// per-refresh row-DOM rebuild is xterm's slowest path — the 2026-08-08
    /// typing-latency diagnosis measured ~20 ms of webview main-thread work
    /// per coalesced output flush, which is exactly the typing/scroll stutter
    /// T4 removed. Kept as the escape hatch for GL-hostile environments.
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
    /// Usage-limit-deferral plan (R9): when true (the default), the sweep
    /// consults the plan-usage gate before claiming a due **agent-mode**
    /// occurrence and defers it to the limit's reset instead of dispatching
    /// into an exhausted window. The gate is fail-open (KTD3) — this knob
    /// exists to switch even the *attempt* off, not to make it safe.
    pub usage_gate: bool,
    /// Headless-agent-automations plan (R1/R2): the dispatch-disposition
    /// default for **agent-mode** automations that don't pin one
    /// (`--headless`/`--paned`). Ships **`true`** — closed-loop `claude -p`
    /// dispatch is the stated direction, and this knob (not a migration) is
    /// the escape hatch: applied at claim time, so flipping it flips every
    /// non-explicit automation at once. Existing automations therefore go
    /// headless on upgrade unless created `--paned`; their failure surface
    /// moves from the kept-open failed tab to the alert ring (R6). Scripts
    /// and monitors ignore it (monitors are unconditionally headless).
    pub headless: bool,
}

impl Default for AutomationDefaults {
    fn default() -> Self {
        Self {
            model: None,
            effort: None,
            fallback_model: "sonnet".into(),
            usage_gate: true,
            headless: true,
        }
    }
}

/// Settings for the local, read-only agent/automation feed
/// (feat-agent-state-local-feed). The feed binds a **loopback-only** HTTP
/// listener guarded by a bearer `token`, so an external local consumer
/// can read what agents/automations are live.
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
    /// Whether `mode: "keys"` on `POST /agents/{key}/input` may answer a
    /// **permission** dialog (feed-pending-question KTD6; Open Question
    /// resolved as opt-in). **Off by default**: a digit on a permission dialog
    /// can pick a durable "don't ask again", which would turn the bearer token
    /// into a remote permission-approval credential. AskUserQuestion *choice*
    /// answering is not gated by this — it needs no flag.
    pub allow_permission_answers: bool,
    /// Directory phone-dropped screenshots land in (phone-screenshot-drop U1,
    /// KTD4). `None` ⇒ `<data root>/inbox`.
    ///
    /// **Stored as the raw user string, deliberately unexpanded.** This is the
    /// one config field where a tilde survives deserialization, and it must:
    /// `set_config` round-trips the whole [`Config`] back to disk, so an
    /// expansion baked in here would be *persisted* the first time the settings
    /// menu saves anything — silently rewriting `~/projects/inbox` to an
    /// absolute path and freezing `$HOME` into the user's config file.
    ///
    /// Shape validation and tilde expansion therefore both live in
    /// [`crate::feed::drop::resolve_drop_dir`], applied where `backend.rs` builds the
    /// store. A bare relative path is *rejected* there rather than silently
    /// resolved against the process cwd — which for a GUI launched from a
    /// desktop file is `/`, not anywhere the user meant.
    ///
    /// Note this diverges from every other durable store fly owns, which live
    /// under the `FLY_APP_NAME` data root so a dev flavor stays isolated. That
    /// is deliberate (KTD4): there is no retention policy, so the user prunes by
    /// hand, and a directory they already browse is far likelier to actually get
    /// pruned than one buried in `~/.local/share`. Sharing the directory between
    /// the stable and dev flavors is safe because filenames are globally unique
    /// by construction (see `drop::mint_filename`).
    pub drop_dir: Option<String>,
    /// Largest accepted phone-drop image, in bytes (KTD8). Default 25 MiB —
    /// comfortably above a full-resolution phone screenshot (a 2–5 MB PNG) and
    /// low enough that a runaway upload cannot fill the disk.
    ///
    /// `u32`, not a float, because [`Config`] derives `Eq`.
    pub drop_max_bytes: u32,
    /// Tailnet login (`Tailscale-User-Login`) a phone-drop request must present
    /// when it presents one at all (KTD2). `None` (the default) disables the
    /// check entirely.
    ///
    /// **Additive only — the bearer token remains the boundary.** `tailscaled`
    /// proxies *to* loopback and loopback TCP carries no peer credentials, so a
    /// proxied request and one from an arbitrary local process are
    /// indistinguishable at the socket: any local process could hand-write this
    /// header. Absence of the header is therefore not a refusal.
    ///
    /// Be precise about what setting it buys, because it is less than it looks.
    /// `tailscale serve` stamps the *tailnet user* who owns the device, not the
    /// device — so on a personal single-user tailnet the realistic leak path
    /// (the token pasted into one of the user's own phones) passes the check
    /// unchanged. It defends against a token used from a device belonging to a
    /// *different* tailnet user, and does nothing about a forged header from a
    /// local process.
    pub expected_tailnet_login: Option<String>,
}

impl Default for FeedConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: 4939,
            token: None,
            allow_permission_answers: false,
            drop_dir: None,
            drop_max_bytes: 25 * 1024 * 1024,
            expected_tailnet_login: None,
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
    /// Terminal emulator command for the U7 native-attach chord
    /// (tmux-substrate KTD6): launched as `<terminal> …separator… tmux -L
    /// <flavor> attach-session -t <session>`. The separator adapts per
    /// terminal family (`--` for gnome-terminal, none for kitty, `-e`
    /// otherwise — see `substrate::attach_command`).
    pub terminal: String,
    /// Session substrate (tmux plan KTD10): `pty` (default) keeps the
    /// portable-pty path; `tmux` backs every leaf-keyed pane with a marked
    /// session on the flavor's tmux server. A **rollout-window flag, not a
    /// mode** — it is removed (with the pty path) once the substrate reaches
    /// parity; do not build on it.
    pub substrate: SubstrateKind,
}

/// KTD10 rollout switch. Lowercase on the wire to match sibling enums.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubstrateKind {
    #[default]
    Pty,
    Tmux,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            leader_key: "ctrl+a".into(),
            attention_debounce_ms: 400,
            nudge_idle_ms: 1500,
            notification_coalesce_threshold: 3,
            osc_bel_fallback: false,
            // Auto since perf-audit T4 (2026-08-08): WebGL on visible panes with
            // dispose-on-hide — the KTD6 eviction the old DOM default was
            // waiting for. See `Renderer::Auto`.
            renderer: Renderer::Auto,
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
            terminal: "x-terminal-emulator".into(),
            substrate: SubstrateKind::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default renderer is Auto — WebGL on visible panes with the KTD6
    /// dispose-on-hide eviction (perf-audit T4, 2026-08-08). DOM was the default
    /// only while the eviction policy was unbuilt; the DOM renderer's per-flush
    /// row rebuild is the measured typing/scroll-stutter ceiling, so guard
    /// against an accidental revert to it.
    #[test]
    fn default_renderer_is_auto() {
        assert_eq!(Config::default().renderer, Renderer::Auto);
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
        assert!(
            c.automation_defaults.usage_gate,
            "config predating the usage gate loads with it on (usage-limit-deferral R9)"
        );
    }

    // Usage-limit-deferral R9: the gate knob defaults on, can be switched off
    // in the file, and round-trips camelCase.
    #[test]
    fn usage_gate_defaults_on_and_can_be_disabled() {
        assert!(AutomationDefaults::default().usage_gate);
        let c: Config =
            serde_json::from_str(r#"{"automationDefaults":{"usageGate":false}}"#).unwrap();
        assert!(!c.automation_defaults.usage_gate);
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["automationDefaults"]["usageGate"], false);
    }

    // Headless-agent-automations R1/R2/R12: the dispatch-disposition default
    // ships ON (closed-loop is the direction — a legacy config with no key
    // flips its non-explicit agent automations headless on upgrade), can be
    // switched off in the file, and round-trips camelCase.
    #[test]
    fn headless_defaults_on_and_can_be_disabled() {
        assert!(AutomationDefaults::default().headless);
        let legacy: Config =
            serde_json::from_str(r#"{"automationDefaults":{"model":"opus"}}"#).unwrap();
        assert!(legacy.automation_defaults.headless, "absent key ⇒ the new default");
        let c: Config =
            serde_json::from_str(r#"{"automationDefaults":{"headless":false}}"#).unwrap();
        assert!(!c.automation_defaults.headless);
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["automationDefaults"]["headless"], false);
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
    /// default port, no token yet (minted on first run), and remote
    /// permission-answering OFF (feed-pending-question KTD6 — the safety
    /// default must hold for every pre-existing config).
    #[test]
    fn empty_config_has_feed_defaults() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert!(cfg.feed.enabled);
        assert_eq!(cfg.feed.port, 4939);
        assert_eq!(cfg.feed.token, None);
        assert!(!cfg.feed.allow_permission_answers);
    }

    /// A config written before the phone-drop block (phone-screenshot-drop U1)
    /// still parses, with the drop directory unconfigured (⇒ `<data root>/inbox`),
    /// the 25 MiB cap in force, and the tailnet identity check disabled.
    #[test]
    fn empty_config_has_drop_defaults() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.feed.drop_dir, None);
        assert_eq!(cfg.feed.drop_max_bytes, 26_214_400);
        assert_eq!(cfg.feed.expected_tailnet_login, None);
    }

    /// The nested-serde gotcha: a *partial* `feed` object keeps omitted siblings
    /// at their defaults (enabled stays true) rather than dropping them.
    #[test]
    fn partial_feed_retains_sibling_defaults() {
        let cfg: Config = serde_json::from_str(r#"{"feed":{"port":5000}}"#).unwrap();
        assert_eq!(cfg.feed.port, 5000);
        assert!(cfg.feed.enabled, "omitted sibling kept its default");
        assert_eq!(cfg.feed.token, None);
        // The drop knobs are siblings too — a pre-existing `{"feed":{"port":…}}`
        // must not lose the size cap and land at `drop_max_bytes: 0`, which
        // would refuse every upload.
        assert_eq!(cfg.feed.drop_max_bytes, 26_214_400);
        assert_eq!(cfg.feed.drop_dir, None);
        assert_eq!(cfg.feed.expected_tailnet_login, None);
    }

    /// A configured `dropDir` survives deserialization **unexpanded**. This is
    /// the invariant behind `set_config`'s round-trip: expanding at parse would
    /// persist an absolute path over the user's `~` the next time anything
    /// saves. Expansion belongs to `feed::drop::expand_tilde`.
    #[test]
    fn drop_dir_keeps_its_tilde_through_a_round_trip() {
        let cfg: Config =
            serde_json::from_str(r#"{"feed":{"dropDir":"~/projects/inbox"}}"#).unwrap();
        assert_eq!(cfg.feed.drop_dir.as_deref(), Some("~/projects/inbox"));
        let back = serde_json::to_value(&cfg).unwrap();
        assert_eq!(back["feed"]["dropDir"], "~/projects/inbox");
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

    /// All three drop fields survive a camelCase round trip together.
    #[test]
    fn drop_fields_round_trip_camel_case() {
        let mut c = Config::default();
        c.feed.drop_dir = Some("~/projects/inbox".into());
        c.feed.drop_max_bytes = 1024;
        c.feed.expected_tailnet_login = Some("alice@example.com".into());
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["feed"]["dropDir"], "~/projects/inbox");
        assert_eq!(v["feed"]["dropMaxBytes"], 1024);
        assert_eq!(v["feed"]["expectedTailnetLogin"], "alice@example.com");
        let back: Config = serde_json::from_value(v).unwrap();
        assert_eq!(back, c);
    }
}
