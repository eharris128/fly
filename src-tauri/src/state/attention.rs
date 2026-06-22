//! Per-pane attention state machine (KTD8, the orthogonal partner of
//! [`super::lifecycle`]).
//!
//! Attention is `Idle → Raised → Acknowledged → Idle`, driven by agent signals
//! plus the authoritative focus/foreground tuple replicated from the frontend.
//! The "is the user looking?" predicate comes from [`super::policy`]
//! ([`is_user_viewing`]); rapid duplicate signals coalesce within a debounce
//! window. This is pure logic — time is passed in, so it is fully testable.

use serde::{Deserialize, Serialize};

use super::policy::is_user_viewing;

/// What the agent needs from the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    Question,
    Permission,
    Finished,
    Error,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Question => "question",
            Reason::Permission => "permission",
            Reason::Finished => "finished",
            Reason::Error => "error",
        }
    }

    pub fn parse(s: &str) -> Option<Reason> {
        match s {
            "question" => Some(Reason::Question),
            "permission" => Some(Reason::Permission),
            "finished" => Some(Reason::Finished),
            "error" => Some(Reason::Error),
            _ => None,
        }
    }
}

/// Where a signal came from — its detection tier (KTD9). v1 only produces
/// `Hook` (authenticated, Tier 1); the rest are the forward design.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Authenticated typed hook (Tier 1) — high confidence.
    Hook,
    /// `fly notify` command hook (Tier 2).
    Cli,
    /// Terminal BEL (Tier 3).
    Bel,
    /// Generic OSC scan (Tier 4) — low confidence, untrusted.
    Osc,
}

impl Tier {
    /// Authenticated tiers are trusted; PTY-stream tiers are not (KTD9).
    pub fn confidence(self) -> Confidence {
        match self {
            Tier::Hook | Tier::Cli => Confidence::High,
            Tier::Bel | Tier::Osc => Confidence::Low,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    High,
    Low,
}

/// An attention signal from an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signal {
    pub reason: Reason,
    pub tier: Tier,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionState {
    Idle,
    Raised,
    Acknowledged,
}

/// The result of feeding an event to the machine: the new state plus whether an
/// OS notification should fire now, with the triggering signal's metadata for
/// rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    pub state: AttentionState,
    /// Fire an OS notification now (already run through the suppression matrix
    /// and debounce).
    pub notify: bool,
    pub reason: Option<Reason>,
    pub tier: Option<Tier>,
}

/// Per-pane attention machine. Holds the authoritative focus/foreground tuple
/// (replicated from the frontend via `set_focus`/`set_foreground`).
#[derive(Debug, Clone)]
pub struct AttentionMachine {
    state: AttentionState,
    focused: bool,
    foregrounded: bool,
    reason: Option<Reason>,
    tier: Option<Tier>,
    /// Time of the last notification-worthy raise, for debounce coalescing.
    last_raise_ms: Option<u64>,
    debounce_ms: u64,
}

impl AttentionMachine {
    /// A new machine. Focus/foreground default to "not looking" so early
    /// signals over-notify rather than get swallowed (KTD8).
    pub fn new(debounce_ms: u64) -> Self {
        Self {
            state: AttentionState::Idle,
            focused: false,
            foregrounded: false,
            reason: None,
            tier: None,
            last_raise_ms: None,
            debounce_ms,
        }
    }

    pub fn state(&self) -> AttentionState {
        self.state
    }

    pub fn reason(&self) -> Option<Reason> {
        self.reason
    }

    pub fn tier(&self) -> Option<Tier> {
        self.tier
    }

    fn outcome(&self, notify: bool) -> Outcome {
        Outcome {
            state: self.state,
            notify,
            reason: self.reason,
            tier: self.tier,
        }
    }

    /// Feed an agent attention signal.
    pub fn signal(&mut self, sig: Signal, now_ms: u64) -> Outcome {
        self.reason = Some(sig.reason);
        self.tier = Some(sig.tier);

        // If the user is already looking, acknowledge directly with no
        // notification (Idle/Acknowledged → Acknowledged). `is_user_viewing` is
        // the negation of the old `should_notify`, so the guard flips from
        // `!should_notify(..)` to a direct `is_user_viewing(..)`.
        if is_user_viewing(self.focused, self.foregrounded) {
            self.state = AttentionState::Acknowledged;
            self.last_raise_ms = None; // a later unfocused signal re-notifies
            return self.outcome(false);
        }

        // Coalesce a duplicate signal that lands while already Raised within
        // the debounce window: stay Raised, do not re-notify.
        let duplicate = self.state == AttentionState::Raised
            && self
                .last_raise_ms
                .is_some_and(|t| now_ms.saturating_sub(t) < self.debounce_ms);

        self.state = AttentionState::Raised;
        if duplicate {
            self.outcome(false)
        } else {
            self.last_raise_ms = Some(now_ms);
            self.outcome(true)
        }
    }

    /// The pane gained or lost keyboard focus.
    pub fn set_focus(&mut self, focused: bool) -> Outcome {
        self.focused = focused;
        self.reevaluate()
    }

    /// The window entered or left the foreground.
    pub fn set_foreground(&mut self, foregrounded: bool) -> Outcome {
        self.foregrounded = foregrounded;
        self.reevaluate()
    }

    /// Re-run the acknowledge rule after a focus/foreground change: a Raised
    /// pane that is now focused+foregrounded becomes Acknowledged (stage 1 of
    /// the two-stage clear).
    fn reevaluate(&mut self) -> Outcome {
        if self.state == AttentionState::Raised && self.focused && self.foregrounded {
            self.state = AttentionState::Acknowledged;
            self.last_raise_ms = None;
        }
        self.outcome(false)
    }

    /// User typed into the pane — definitively engaged; clear to Idle (stage 2).
    pub fn on_input(&mut self) -> Outcome {
        self.clear()
    }

    /// An explicit resolve signal (e.g. the agent reported it's unblocked).
    pub fn on_resolve(&mut self) -> Outcome {
        self.clear()
    }

    /// The pane's process exited — attention is moot; force Idle.
    pub fn on_exit(&mut self) -> Outcome {
        self.clear()
    }

    fn clear(&mut self) -> Outcome {
        self.state = AttentionState::Idle;
        self.last_raise_ms = None;
        self.reason = None;
        self.tier = None;
        self.outcome(false)
    }
}
