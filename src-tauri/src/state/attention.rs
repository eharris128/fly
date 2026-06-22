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

/// The result of feeding an event to the machine: the new state plus whether
/// this raise is worth recording, with the triggering signal's metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    pub state: AttentionState,
    /// This is a fresh raise worth recording to the notification history
    /// (KTD16): any non-debounced raise, **including** the Acknowledged-at-birth
    /// case (a raise on a pane the user is already viewing — recorded read).
    /// Only a debounced duplicate is non-recordable. The desktop/sound/record
    /// *effects* are decided separately by the policy (KTD14, U16); this field
    /// no longer encodes the desktop decision the way `notify` once did.
    pub recordable: bool,
    pub reason: Option<Reason>,
    pub tier: Option<Tier>,
}

/// Per-pane attention machine. Holds the authoritative visibility/foreground
/// tuple (replicated from the frontend via `set_visible`/`set_foreground`).
/// "Visible" is the active tab's pane in the active workspace — broader than
/// keyboard focus, so a visible split sibling still counts as "looking" (U17).
#[derive(Debug, Clone)]
pub struct AttentionMachine {
    state: AttentionState,
    visible: bool,
    foregrounded: bool,
    reason: Option<Reason>,
    tier: Option<Tier>,
    /// Time of the last recordable raise, for debounce coalescing.
    last_raise_ms: Option<u64>,
    debounce_ms: u64,
}

impl AttentionMachine {
    /// A new machine. Visibility/foreground default to "not looking" so early
    /// signals over-notify rather than get swallowed (KTD8).
    pub fn new(debounce_ms: u64) -> Self {
        Self {
            state: AttentionState::Idle,
            visible: false,
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

    /// Whether the pane is in the active tab of the active workspace.
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Whether the app window is foregrounded.
    pub fn is_foregrounded(&self) -> bool {
        self.foregrounded
    }

    fn outcome(&self, recordable: bool) -> Outcome {
        Outcome {
            state: self.state,
            recordable,
            reason: self.reason,
            tier: self.tier,
        }
    }

    /// Feed an agent attention signal.
    ///
    /// The looking-vs-duplicate split is the heart of U17: "the user is looking"
    /// and "this is a debounced duplicate" are now **independent**. Looking-ness
    /// decides only the in-app ring (a raise on a pane you're viewing goes
    /// straight to Acknowledged — no ring needed); it no longer suppresses the
    /// record. The debounce decides recordability. So a raise on a visible pane
    /// is `Acknowledged` *and* `recordable` (the policy then records it read),
    /// while a genuine debounced duplicate is non-recordable regardless of
    /// visibility. The desktop/sound/record effects are the policy's job (U16).
    pub fn signal(&mut self, sig: Signal, now_ms: u64) -> Outcome {
        self.reason = Some(sig.reason);
        self.tier = Some(sig.tier);

        // A debounced duplicate: a repeat raise landing while still Raised
        // within the window. Once Acknowledged or cleared the pane is no longer
        // Raised, so a later raise is always fresh (recordable).
        let duplicate = self.state == AttentionState::Raised
            && self
                .last_raise_ms
                .is_some_and(|t| now_ms.saturating_sub(t) < self.debounce_ms);

        // Looking → Acknowledged (no ring); otherwise Raised (ring shows).
        self.state = if is_user_viewing(self.visible, self.foregrounded) {
            AttentionState::Acknowledged
        } else {
            AttentionState::Raised
        };

        if duplicate {
            self.outcome(false)
        } else {
            self.last_raise_ms = Some(now_ms);
            self.outcome(true)
        }
    }

    /// The pane became visible (active tab of the active workspace) or hidden.
    /// Broader than keyboard focus, so a visible split sibling counts as
    /// "looking" for the Acknowledged transition (cmux workspace-active, U17).
    pub fn set_visible(&mut self, visible: bool) -> Outcome {
        self.visible = visible;
        self.reevaluate()
    }

    /// The window entered or left the foreground.
    pub fn set_foreground(&mut self, foregrounded: bool) -> Outcome {
        self.foregrounded = foregrounded;
        self.reevaluate()
    }

    /// Re-run the acknowledge rule after a visibility/foreground change: a Raised
    /// pane the user is now viewing becomes Acknowledged (stage 1 of the
    /// two-stage clear). Never recordable — a focus change creates no new raise.
    fn reevaluate(&mut self) -> Outcome {
        if self.state == AttentionState::Raised && is_user_viewing(self.visible, self.foregrounded) {
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
