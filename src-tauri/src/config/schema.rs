//! Typed user-settings schema with defaults (U13).
//!
//! `#[serde(default)]` means a partial or empty config file still loads — every
//! missing field falls back to its default, so adding fields never breaks an
//! older config.

use serde::{Deserialize, Serialize};

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
    /// Persist scrollback across restart — off by default for privacy (KTD10).
    pub save_scrollback: bool,
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
            save_scrollback: false,
        }
    }
}
