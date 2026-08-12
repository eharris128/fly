//! Per-pane attention registry: owns one [`AttentionMachine`] per pane plus the
//! authoritative global suppression inputs replicated from the frontend (window
//! foreground, notification-panel-open, global + per-workspace mute) and each
//! pane's workspace, and supplies the monotonic clock the debounce needs (the
//! machines themselves stay time-injected/pure).

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Instant;

use super::attention::{AttentionMachine, Outcome, Signal};
use super::policy::{self, is_user_viewing, Effects, PolicyInputs, ReasonEffects};
use crate::pty::PaneId;

/// The policy decision for one pane plus the read-at-birth bit (KTD16): when the
/// user is already viewing the pane the recorded notification is born read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneNotify {
    pub effects: Effects,
    pub read: bool,
}

pub struct AttentionManager {
    machines: Mutex<HashMap<PaneId, AttentionMachine>>,
    foregrounded: Mutex<bool>,
    /// The notification panel is open (a desktop/sound suppressor while
    /// foregrounded — KTD15). Runtime state, reset on launch.
    panel_open: Mutex<bool>,
    /// Global do-not-disturb. Seeded from `notifications_muted_default` (U23),
    /// then toggled at runtime.
    muted_global: Mutex<bool>,
    /// Workspace keys currently muted (runtime only in v1).
    muted_workspaces: Mutex<HashSet<String>>,
    /// Each pane's workspace key, so a per-workspace mute scopes to its panes.
    pane_workspace: Mutex<HashMap<PaneId, String>>,
    /// Panes whose tmux session has an external client attached
    /// (tmux-substrate U7/R9). An attached pane is effectively
    /// visible-and-foregrounded — the user is literally looking at it in
    /// another terminal — so raises acknowledge and notifications suppress
    /// exactly as for the in-window focused pane. Fed by the KTD12
    /// attach-state events; the raw frontend visible set is kept so a
    /// detach restores the true in-window state.
    attached: Mutex<HashSet<PaneId>>,
    raw_visible: Mutex<HashSet<PaneId>>,
    debounce_ms: u64,
    epoch: Instant,
}

impl AttentionManager {
    pub fn new(debounce_ms: u64, muted_default: bool) -> Self {
        Self {
            machines: Mutex::new(HashMap::new()),
            foregrounded: Mutex::new(false),
            panel_open: Mutex::new(false),
            muted_global: Mutex::new(muted_default),
            muted_workspaces: Mutex::new(HashSet::new()),
            pane_workspace: Mutex::new(HashMap::new()),
            attached: Mutex::new(HashSet::new()),
            raw_visible: Mutex::new(HashSet::new()),
            debounce_ms,
            epoch: Instant::now(),
        }
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// Start tracking a pane, inheriting the current window-foreground state
    /// (an already-attached pane — a reattach race — counts as foregrounded).
    pub fn register(&self, pane: PaneId) {
        let fg = *self.foregrounded.lock().unwrap()
            || self.attached.lock().unwrap().contains(&pane);
        let mut machine = AttentionMachine::new(self.debounce_ms);
        machine.set_foreground(fg);
        self.machines.lock().unwrap().insert(pane, machine);
    }

    pub fn remove(&self, pane: PaneId) {
        self.machines.lock().unwrap().remove(&pane);
        self.pane_workspace.lock().unwrap().remove(&pane);
    }

    /// Feed an authenticated agent signal; `None` if the pane is gone.
    pub fn signal(&self, pane: PaneId, signal: Signal) -> Option<Outcome> {
        let now = self.now_ms();
        let mut machines = self.machines.lock().unwrap();
        machines.get_mut(&pane).map(|m| m.signal(signal, now))
    }

    /// Replicate the set of visible panes — the active tab's leaves in the
    /// active workspace (U17). A pane in the set is "visible"; all others are
    /// hidden. Re-evaluates every pane (a now-visible Raised pane acknowledges),
    /// mirroring [`set_foreground`].
    pub fn set_visible_panes(&self, visible: &[PaneId]) -> Vec<(PaneId, Outcome)> {
        let set: HashSet<PaneId> = visible.iter().copied().collect();
        *self.raw_visible.lock().unwrap() = set.clone();
        let attached = self.attached.lock().unwrap().clone();
        let mut machines = self.machines.lock().unwrap();
        machines
            .iter_mut()
            .map(|(id, m)| {
                (*id, m.set_visible(set.contains(id) || attached.contains(id)))
            })
            .collect()
    }

    /// Replicate the window foreground state; re-evaluates every pane. An
    /// externally-attached pane stays effectively foregrounded regardless
    /// (U7/R9).
    pub fn set_foreground(&self, foregrounded: bool) -> Vec<(PaneId, Outcome)> {
        *self.foregrounded.lock().unwrap() = foregrounded;
        let attached = self.attached.lock().unwrap().clone();
        let mut machines = self.machines.lock().unwrap();
        machines
            .iter_mut()
            .map(|(id, m)| {
                (*id, m.set_foreground(foregrounded || attached.contains(id)))
            })
            .collect()
    }

    /// Record a pane's external-attach state (tmux-substrate U7/R9, fed by
    /// the KTD12 attach-state events). Attached ⇒ the pane is effectively
    /// visible + foregrounded (raises acknowledge, notifications suppress);
    /// detach restores the true in-window visibility and the global
    /// foreground state.
    pub fn set_attached(&self, pane: PaneId, attached: bool) -> Option<Outcome> {
        {
            let mut set = self.attached.lock().unwrap();
            if attached {
                set.insert(pane);
            } else {
                set.remove(&pane);
            }
        }
        let raw_visible = self.raw_visible.lock().unwrap().contains(&pane);
        let fg = *self.foregrounded.lock().unwrap();
        let mut machines = self.machines.lock().unwrap();
        let m = machines.get_mut(&pane)?;
        let _ = m.set_visible(raw_visible || attached);
        // The second call sees the machine's settled state; its outcome is
        // the final word (state after both inputs applied), which is what
        // the caller emits to the frontend.
        Some(m.set_foreground(fg || attached))
    }

    /// The notification panel opened or closed. Affects only the policy
    /// decision (desktop/sound), not attention state, so it stores the flag and
    /// re-evaluates nothing.
    pub fn set_panel_open(&self, open: bool) {
        *self.panel_open.lock().unwrap() = open;
    }

    /// Toggle global do-not-disturb. Like panel-open, mute suppresses
    /// desktop/sound but leaves the in-app ring + record alone, so no
    /// re-evaluation is needed.
    pub fn set_muted(&self, muted: bool) {
        *self.muted_global.lock().unwrap() = muted;
    }

    pub fn muted(&self) -> bool {
        *self.muted_global.lock().unwrap()
    }

    /// Mute or unmute a workspace (scoped to its panes via `pane_workspace`).
    pub fn set_workspace_muted(&self, workspace: String, muted: bool) {
        let mut set = self.muted_workspaces.lock().unwrap();
        if muted {
            set.insert(workspace);
        } else {
            set.remove(&workspace);
        }
    }

    /// Record which workspace a pane belongs to (for per-workspace mute).
    pub fn set_pane_workspace(&self, pane: PaneId, workspace: String) {
        self.pane_workspace.lock().unwrap().insert(pane, workspace);
    }

    /// The workspace a pane belongs to, if the frontend has replicated it
    /// (automations U9: origin stamping resolves the creating pane's workspace
    /// so an agent run's tab can land back in it, R9/R22). `None` before the
    /// frontend pushes the mapping, or for a gone pane.
    pub fn pane_workspace(&self, pane: PaneId) -> Option<String> {
        self.pane_workspace.lock().unwrap().get(&pane).cloned()
    }

    /// The per-effect policy decision for a pane's raise (KTD14): combines the
    /// reason's configured effect mask with the pane's replicated runtime state.
    /// `None` if the pane is gone.
    pub fn decide(&self, pane: PaneId, reason: ReasonEffects) -> Option<PaneNotify> {
        let (pane_visible, window_foregrounded) = {
            let machines = self.machines.lock().unwrap();
            let m = machines.get(&pane)?;
            (m.is_visible(), m.is_foregrounded())
        };
        let panel_open = *self.panel_open.lock().unwrap();
        let muted_global = *self.muted_global.lock().unwrap();
        // Clone the key out so the `pane_workspace` lock is dropped before the
        // `muted_workspaces` lock is taken (no nested-lock ordering hazard).
        let workspace = self.pane_workspace.lock().unwrap().get(&pane).cloned();
        let muted_workspace =
            workspace.is_some_and(|w| self.muted_workspaces.lock().unwrap().contains(&w));

        let effects = policy::decide(PolicyInputs {
            reason,
            pane_visible,
            window_foregrounded,
            panel_open,
            muted_global,
            muted_workspace,
        });
        Some(PaneNotify {
            effects,
            read: is_user_viewing(pane_visible, window_foregrounded),
        })
    }

    /// User typed into a pane — clears attention, but only reports a change
    /// when there was attention to clear (so it's cheap on every keystroke).
    pub fn on_input(&self, pane: PaneId) -> Option<Outcome> {
        let mut machines = self.machines.lock().unwrap();
        let m = machines.get_mut(&pane)?;
        if m.state() == super::attention::AttentionState::Idle {
            return None;
        }
        Some(m.on_input())
    }

    /// The pane's process exited — force its attention to Idle.
    pub fn on_exit(&self, pane: PaneId) -> Option<Outcome> {
        let mut machines = self.machines.lock().unwrap();
        machines.get_mut(&pane).map(|m| m.on_exit())
    }

    /// How many panes are currently raised (for notification coalescing, U11).
    pub fn raised_count(&self) -> usize {
        use super::attention::AttentionState;
        self.machines
            .lock()
            .unwrap()
            .values()
            .filter(|m| m.state() == AttentionState::Raised)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::attention::{AttentionState, Reason, Signal, Tier};

    const DEBOUNCE: u64 = 400;
    const PANE: PaneId = PaneId(1);

    fn hook(reason: Reason) -> Signal {
        Signal {
            reason,
            tier: Tier::Hook,
        }
    }

    fn mgr() -> AttentionManager {
        let m = AttentionManager::new(DEBOUNCE, false);
        m.register(PANE);
        m
    }

    #[test]
    fn visible_raise_records_as_read() {
        // The headline decoupling (U17): a raise on a visible, foregrounded pane
        // acknowledges in-app, is still recordable, and the policy records it
        // read with no banner/sound.
        let m = mgr();
        m.set_foreground(true);
        m.set_visible_panes(&[PANE]);
        let outcome = m.signal(PANE, hook(Reason::Permission)).unwrap();
        assert_eq!(outcome.state, AttentionState::Acknowledged);
        assert!(outcome.recordable, "a visible raise still records");

        let dec = m.decide(PANE, ReasonEffects::default()).unwrap();
        assert_eq!(
            dec.effects,
            Effects {
                desktop: false,
                sound: false,
                record: true
            }
        );
        assert!(dec.read, "the user is viewing → born read");
    }

    #[test]
    fn hidden_pane_stays_raised_and_records_unread() {
        let m = mgr();
        m.set_foreground(true);
        m.set_visible_panes(&[]); // PANE not visible
        let outcome = m.signal(PANE, hook(Reason::Question)).unwrap();
        assert_eq!(outcome.state, AttentionState::Raised);
        assert!(outcome.recordable);

        let dec = m.decide(PANE, ReasonEffects::default()).unwrap();
        // Foregrounded + hidden + panel closed: no banner, but the chime stays
        // (the resolved audible-cue choice), and it records unread.
        assert_eq!(
            dec.effects,
            Effects {
                desktop: false,
                sound: true,
                record: true
            }
        );
        assert!(!dec.read, "not viewing → unread");
    }

    #[test]
    fn debounce_duplicate_does_not_record() {
        let m = mgr();
        m.set_foreground(true);
        m.set_visible_panes(&[]);
        assert!(m.signal(PANE, hook(Reason::Permission)).unwrap().recordable);
        // A second identical raise within the window is a duplicate.
        assert!(!m.signal(PANE, hook(Reason::Permission)).unwrap().recordable);
    }

    #[test]
    fn panel_open_is_a_desktop_suppressor_only_while_foregrounded() {
        let m = mgr();
        m.set_foreground(true);
        m.set_visible_panes(&[]);
        m.set_panel_open(true);
        let outcome = m.signal(PANE, hook(Reason::Error)).unwrap();
        // Still Raised (ring shows) and recordable, but the banner is suppressed.
        assert_eq!(outcome.state, AttentionState::Raised);
        let dec = m.decide(PANE, ReasonEffects::default()).unwrap();
        assert!(!dec.effects.desktop, "panel open + foregrounded → no banner");
        assert!(!dec.effects.sound, "panel open + foregrounded → no chime");
        assert!(dec.effects.record);

        // Walk away (background) with the panel still open → full away alert.
        m.set_foreground(false);
        let dec = m.decide(PANE, ReasonEffects::default()).unwrap();
        assert!(dec.effects.desktop, "away with panel open still banners");
    }

    #[test]
    fn mute_toggles_re_evaluate_the_desktop_decision() {
        let m = mgr();
        // backgrounded raise would normally banner.
        m.set_muted(true);
        m.signal(PANE, hook(Reason::Permission));
        let dec = m.decide(PANE, ReasonEffects::default()).unwrap();
        assert!(!dec.effects.desktop, "muted → no banner");
        assert!(dec.effects.record, "record survives mute");

        m.set_muted(false);
        let dec = m.decide(PANE, ReasonEffects::default()).unwrap();
        assert!(dec.effects.desktop, "unmuted → banners again");
    }

    #[test]
    fn workspace_mute_scopes_to_its_panes() {
        let m = AttentionManager::new(DEBOUNCE, false);
        let other = PaneId(2);
        m.register(PANE);
        m.register(other);
        m.set_pane_workspace(PANE, "ws-a".into());
        m.set_pane_workspace(other, "ws-b".into());
        m.set_workspace_muted("ws-a".into(), true);

        m.signal(PANE, hook(Reason::Permission));
        m.signal(other, hook(Reason::Permission));
        assert!(
            !m.decide(PANE, ReasonEffects::default()).unwrap().effects.desktop,
            "muted workspace → no banner"
        );
        assert!(
            m.decide(other, ReasonEffects::default()).unwrap().effects.desktop,
            "other workspace unaffected"
        );
    }

    #[test]
    fn decide_defaults_unregistered_workspace_to_unmuted() {
        // A hook can raise in the window between pane registration and the
        // frontend's set_pane_workspace push (onSpawned). With no workspace
        // entry, the pane is treated as not workspace-muted (it banners), not
        // dropped — the accepted spawn-race default.
        let m = mgr(); // registers PANE, never calls set_pane_workspace
        m.signal(PANE, hook(Reason::Permission)); // backgrounded raise
        let dec = m.decide(PANE, ReasonEffects::default()).unwrap();
        assert!(dec.effects.desktop, "unregistered workspace is not muted");
    }

    #[test]
    fn visible_set_membership_drives_looking() {
        let m = AttentionManager::new(DEBOUNCE, false);
        let a = PaneId(1);
        let b = PaneId(2);
        m.register(a);
        m.register(b);
        m.set_foreground(true);
        m.set_visible_panes(&[a, b]); // both in the active tab
        // A raise on b (a sibling holds keyboard focus) still acknowledges,
        // because visibility is set membership, not keyboard focus.
        let outcome = m.signal(b, hook(Reason::Question)).unwrap();
        assert_eq!(outcome.state, AttentionState::Acknowledged);
        assert!(outcome.recordable);

        // Switch the visible set away → b becomes hidden; a later raise rings.
        m.set_visible_panes(&[]);
        let outcome = m.signal(b, hook(Reason::Question)).unwrap();
        assert_eq!(outcome.state, AttentionState::Raised);
    }
}
