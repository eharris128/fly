//! Notification-suppression matrix (KTD8).
//!
//! An OS notification is suppressed only when the user is already looking at
//! the pane: the pane is focused AND its window is foregrounded. Every other
//! quadrant notifies. An unknown foreground state is treated as backgrounded
//! by the caller (over-notify rather than swallow).

/// Returns whether an OS notification should fire for a freshly-raised pane.
pub fn should_notify(pane_focused: bool, window_foregrounded: bool) -> bool {
    !(pane_focused && window_foregrounded)
}

#[cfg(test)]
mod tests {
    use super::should_notify;

    #[test]
    fn suppresses_only_when_focused_and_foregrounded() {
        assert!(!should_notify(true, true)); // user is looking → suppress
        assert!(should_notify(true, false)); // window backgrounded → notify
        assert!(should_notify(false, true)); // another pane focused → notify
        assert!(should_notify(false, false)); // nothing focused → notify
    }
}
