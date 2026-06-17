//! U7 attention state machine + suppression matrix (R8, R12). Pure logic, so
//! these are deterministic (time is passed in). Lifecycle transitions are
//! exercised end-to-end in `pty_lifecycle.rs`.

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
    // Unfocused signal raises and notifies.
    let o = m.signal(hook(Reason::Permission), 1000);
    assert_eq!(o.state, AttentionState::Raised);
    assert!(o.notify);

    // Focusing the pane (and foregrounding the window) acknowledges it.
    m.set_foreground(true);
    let o = m.set_focus(true);
    assert_eq!(o.state, AttentionState::Acknowledged);
    assert!(!o.notify);

    // Input clears to Idle (stage two).
    let o = m.on_input();
    assert_eq!(o.state, AttentionState::Idle);
}

#[test]
fn signal_while_focused_and_foregrounded_acknowledges_without_notifying() {
    let mut m = AttentionMachine::new(DEBOUNCE);
    m.set_focus(true);
    m.set_foreground(true);
    let o = m.signal(hook(Reason::Question), 1000);
    assert_eq!(o.state, AttentionState::Acknowledged);
    assert!(!o.notify, "user is already looking → suppress");
}

#[test]
fn suppression_matrix_all_quadrants() {
    let cases = [
        (true, true, false), // focused + foreground → suppress
        (true, false, true), // focused, backgrounded → notify
        (false, true, true), // unfocused, foreground → notify
        (false, false, true), // neither → notify
    ];
    for (focused, fg, expect_notify) in cases {
        let mut m = AttentionMachine::new(DEBOUNCE);
        m.set_focus(focused);
        m.set_foreground(fg);
        let o = m.signal(hook(Reason::Error), 1000);
        assert_eq!(
            o.notify, expect_notify,
            "focused={focused} fg={fg} should notify={expect_notify}"
        );
    }
}

#[test]
fn rapid_duplicates_coalesce_to_one_notification() {
    let mut m = AttentionMachine::new(DEBOUNCE);
    assert!(m.signal(hook(Reason::Permission), 1000).notify);
    // Within the debounce window: coalesced, no re-notify.
    assert!(!m.signal(hook(Reason::Permission), 1100).notify);
    assert!(!m.signal(hook(Reason::Permission), 1399).notify);
    // Past the window: notifies again.
    assert!(m.signal(hook(Reason::Permission), 1500).notify);
}

#[test]
fn answer_then_new_signal_renotifies() {
    let mut m = AttentionMachine::new(DEBOUNCE);
    m.signal(hook(Reason::Question), 1000); // Raised + notify
    m.set_focus(true);
    m.set_foreground(true); // → Acknowledged, debounce reset
    assert_eq!(m.state(), AttentionState::Acknowledged);

    // User looks away, then a genuine follow-up arrives → re-notify.
    m.set_focus(false);
    let o = m.signal(hook(Reason::Permission), 1200);
    assert_eq!(o.state, AttentionState::Raised);
    assert!(o.notify, "a new signal after acknowledge is not a duplicate");
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
fn focus_change_reevaluates_already_raised_pane() {
    let mut m = AttentionMachine::new(DEBOUNCE);
    // Raised while not looking.
    m.signal(hook(Reason::Permission), 1000);
    assert_eq!(m.state(), AttentionState::Raised);

    // Foreground alone (pane still unfocused) does not acknowledge.
    m.set_foreground(true);
    assert_eq!(m.state(), AttentionState::Raised);

    // Focus arrives → acknowledged via the authoritative tuple.
    let o = m.set_focus(true);
    assert_eq!(o.state, AttentionState::Acknowledged);
}

#[test]
fn signal_uses_backend_authoritative_focus_tuple() {
    // A signal racing a focus switch reads the tuple the backend holds, not a
    // value carried on the signal.
    let mut m = AttentionMachine::new(DEBOUNCE);
    m.set_focus(true);
    m.set_foreground(true);
    // Backend believes the user is looking → suppress despite the signal.
    assert!(!m.signal(hook(Reason::Question), 1000).notify);
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
