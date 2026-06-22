//! U7/U17 attention state machine (R8, R12). Pure logic, so these are
//! deterministic (time is passed in). The machine now decides only the in-app
//! ring + whether a raise is recordable; the desktop/sound/record *effects* are
//! the policy's job (unit-tested in `state::policy` / `state::manager`).
//! Lifecycle transitions are exercised end-to-end in `pty_lifecycle.rs`.

use fly_lib::state::attention::{AttentionMachine, AttentionState, Reason, Signal, Tier};
use fly_lib::state::lifecycle::LifecycleState;

const DEBOUNCE: u64 = 400;

fn hook(reason: Reason) -> Signal {
    Signal {
        reason,
        tier: Tier::Hook,
    }
}

#[test]
fn full_cycle_idle_raised_acknowledged_idle() {
    let mut m = AttentionMachine::new(DEBOUNCE);
    // A raise the user isn't looking at rings and is recordable.
    let o = m.signal(hook(Reason::Permission), 1000);
    assert_eq!(o.state, AttentionState::Raised);
    assert!(o.recordable);

    // Making the pane visible + foregrounding the window acknowledges it; a
    // visibility change records nothing.
    m.set_foreground(true);
    let o = m.set_visible(true);
    assert_eq!(o.state, AttentionState::Acknowledged);
    assert!(!o.recordable);

    // Input clears to Idle (stage two).
    let o = m.on_input();
    assert_eq!(o.state, AttentionState::Idle);
}

#[test]
fn signal_while_visible_and_foregrounded_acknowledges_but_records() {
    // The U17 decoupling: looking decides the ring (Acknowledged — no ring), but
    // the raise is still recordable (the policy records it read).
    let mut m = AttentionMachine::new(DEBOUNCE);
    m.set_visible(true);
    m.set_foreground(true);
    let o = m.signal(hook(Reason::Question), 1000);
    assert_eq!(o.state, AttentionState::Acknowledged);
    assert!(o.recordable, "a visible raise still records (read at birth)");
}

#[test]
fn ring_state_across_visibility_quadrants() {
    // The machine rings (Raised) unless the user is looking (visible AND
    // foregrounded), in which case it acknowledges. Every fresh raise records.
    let cases = [
        (true, true, AttentionState::Acknowledged), // looking → no ring
        (true, false, AttentionState::Raised),      // visible tab, window backgrounded
        (false, true, AttentionState::Raised),      // hidden pane, foregrounded
        (false, false, AttentionState::Raised),     // neither
    ];
    for (visible, fg, expect) in cases {
        let mut m = AttentionMachine::new(DEBOUNCE);
        m.set_visible(visible);
        m.set_foreground(fg);
        let o = m.signal(hook(Reason::Error), 1000);
        assert_eq!(o.state, expect, "visible={visible} fg={fg}");
        assert!(o.recordable, "a fresh raise is always recordable");
    }
}

#[test]
fn rapid_duplicates_coalesce_to_one_record() {
    let mut m = AttentionMachine::new(DEBOUNCE);
    assert!(m.signal(hook(Reason::Permission), 1000).recordable);
    // Within the debounce window: coalesced, not re-recorded.
    assert!(!m.signal(hook(Reason::Permission), 1100).recordable);
    assert!(!m.signal(hook(Reason::Permission), 1399).recordable);
    // Past the window: records again.
    assert!(m.signal(hook(Reason::Permission), 1500).recordable);
}

#[test]
fn answer_then_new_signal_re_records() {
    let mut m = AttentionMachine::new(DEBOUNCE);
    m.signal(hook(Reason::Question), 1000); // Raised + recordable
    m.set_visible(true);
    m.set_foreground(true); // → Acknowledged, debounce reset
    assert_eq!(m.state(), AttentionState::Acknowledged);

    // User looks away, then a genuine follow-up arrives → rings + records.
    m.set_visible(false);
    let o = m.signal(hook(Reason::Permission), 1200);
    assert_eq!(o.state, AttentionState::Raised);
    assert!(o.recordable, "a new signal after acknowledge is not a duplicate");
}

#[test]
fn process_exit_forces_idle() {
    let mut m = AttentionMachine::new(DEBOUNCE);
    m.signal(hook(Reason::Finished), 1000);
    assert_eq!(m.state(), AttentionState::Raised);
    let o = m.on_exit();
    assert_eq!(o.state, AttentionState::Idle);
}

#[test]
fn visibility_change_reevaluates_already_raised_pane() {
    let mut m = AttentionMachine::new(DEBOUNCE);
    // Raised while not looking.
    m.signal(hook(Reason::Permission), 1000);
    assert_eq!(m.state(), AttentionState::Raised);

    // Foreground alone (pane still hidden) does not acknowledge.
    m.set_foreground(true);
    assert_eq!(m.state(), AttentionState::Raised);

    // Visibility arrives → acknowledged via the authoritative tuple.
    let o = m.set_visible(true);
    assert_eq!(o.state, AttentionState::Acknowledged);
}

#[test]
fn signal_uses_backend_authoritative_visibility_tuple() {
    // A signal racing a visibility switch reads the tuple the backend holds, not
    // a value carried on the signal: the backend believes the user is looking,
    // so it acknowledges (no ring) rather than raising.
    let mut m = AttentionMachine::new(DEBOUNCE);
    m.set_visible(true);
    m.set_foreground(true);
    assert_eq!(
        m.signal(hook(Reason::Question), 1000).state,
        AttentionState::Acknowledged
    );
}

#[test]
fn lifecycle_terminal_and_live_predicates() {
    assert!(LifecycleState::Live.is_live());
    assert!(!LifecycleState::Live.is_terminal());
    assert!(LifecycleState::Exited { code: 0, signal: None }.is_terminal());
    assert!(LifecycleState::Killed.is_terminal());
    assert!(LifecycleState::Failed { error: "x".into() }.is_terminal());
    assert!(!LifecycleState::Spawning.is_terminal());
    assert!(!LifecycleState::RestoredInert.is_live());
}
