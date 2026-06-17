//! Per-pane attention registry: owns one [`AttentionMachine`] per pane plus the
//! authoritative global window-foreground flag, and supplies the monotonic
//! clock the debounce needs (the machines themselves stay time-injected/pure).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use super::attention::{AttentionMachine, Outcome, Signal};
use crate::pty::PaneId;

pub struct AttentionManager {
    machines: Mutex<HashMap<PaneId, AttentionMachine>>,
    foregrounded: Mutex<bool>,
    debounce_ms: u64,
    epoch: Instant,
}

impl AttentionManager {
    pub fn new(debounce_ms: u64) -> Self {
        Self {
            machines: Mutex::new(HashMap::new()),
            foregrounded: Mutex::new(false),
            debounce_ms,
            epoch: Instant::now(),
        }
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// Start tracking a pane, inheriting the current window-foreground state.
    pub fn register(&self, pane: PaneId) {
        let fg = *self.foregrounded.lock().unwrap();
        let mut machine = AttentionMachine::new(self.debounce_ms);
        machine.set_foreground(fg);
        self.machines.lock().unwrap().insert(pane, machine);
    }

    pub fn remove(&self, pane: PaneId) {
        self.machines.lock().unwrap().remove(&pane);
    }

    /// Feed an authenticated agent signal; `None` if the pane is gone.
    pub fn signal(&self, pane: PaneId, signal: Signal) -> Option<Outcome> {
        let now = self.now_ms();
        let mut machines = self.machines.lock().unwrap();
        machines.get_mut(&pane).map(|m| m.signal(signal, now))
    }

    /// Replicate a pane's focus to the backend (KTD8). Only one pane is focused
    /// at a time, so focusing one blurs the rest.
    pub fn set_focus(&self, pane: PaneId, focused: bool) -> Vec<(PaneId, Outcome)> {
        let mut machines = self.machines.lock().unwrap();
        let mut out = Vec::new();
        for (id, m) in machines.iter_mut() {
            if *id == pane {
                out.push((*id, m.set_focus(focused)));
            } else if focused {
                // Focusing one pane blurs the others.
                out.push((*id, m.set_focus(false)));
            }
        }
        out
    }

    /// Replicate the window foreground state; re-evaluates every pane.
    pub fn set_foreground(&self, foregrounded: bool) -> Vec<(PaneId, Outcome)> {
        *self.foregrounded.lock().unwrap() = foregrounded;
        let mut machines = self.machines.lock().unwrap();
        machines
            .iter_mut()
            .map(|(id, m)| (*id, m.set_foreground(foregrounded)))
            .collect()
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
