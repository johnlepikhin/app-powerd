use std::fmt;

use serde::{Deserialize, Serialize};

/// Application power state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum AppState {
    /// Window has focus or was recently focused.
    Active,
    /// Window lost focus, waiting for suspend_delay timer.
    Background,
    /// CPU throttled via cgroup/nice.
    Throttled,
    /// Frozen via cgroup freezer or SIGSTOP.
    Frozen,
}

impl fmt::Display for AppState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppState::Active => write!(f, "ACTIVE"),
            AppState::Background => write!(f, "BACKGROUND"),
            AppState::Throttled => write!(f, "THROTTLED"),
            AppState::Frozen => write!(f, "FROZEN"),
        }
    }
}

/// Action to take on state transition.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TransitionAction {
    /// Start the suspend_delay timer.
    StartSuspendTimer,
    /// Cancel the suspend_delay timer (focus regained).
    CancelSuspendTimer,
    /// Apply throttle policy.
    ApplyThrottle,
    /// Apply freeze.
    ApplyFreeze,
    /// Remove throttle policy.
    RemoveThrottle,
    /// Thaw the application.
    Thaw,
    /// No action needed.
    NoOp,
}

impl TransitionAction {
    /// Whether this action requires active power management to be enabled.
    pub fn requires_management(&self) -> bool {
        matches!(
            self,
            TransitionAction::StartSuspendTimer
                | TransitionAction::ApplyThrottle
                | TransitionAction::ApplyFreeze
        )
    }
}

impl AppState {
    /// Compute transition when focus is lost.
    pub fn on_focus_lost(self) -> (AppState, TransitionAction) {
        match self {
            AppState::Active => (AppState::Background, TransitionAction::StartSuspendTimer),
            // Already in background/throttled/frozen — no change
            other => (other, TransitionAction::NoOp),
        }
    }

    /// Compute transition when focus is gained.
    pub fn on_focus_gained(self) -> (AppState, TransitionAction) {
        match self {
            AppState::Background => (AppState::Active, TransitionAction::CancelSuspendTimer),
            AppState::Throttled => (AppState::Active, TransitionAction::RemoveThrottle),
            AppState::Frozen => (AppState::Active, TransitionAction::Thaw),
            AppState::Active => (AppState::Active, TransitionAction::NoOp),
        }
    }

    /// Compute transition when suspend_delay timer fires.
    pub fn on_suspend_timer(self, mode: SuspendMode) -> (AppState, TransitionAction) {
        match self {
            AppState::Background => match mode {
                SuspendMode::Freeze => (AppState::Frozen, TransitionAction::ApplyFreeze),
                SuspendMode::Throttle => (AppState::Throttled, TransitionAction::ApplyThrottle),
            },
            // Timer fired but state already changed — ignore
            other => (other, TransitionAction::NoOp),
        }
    }
}

/// Mode for suspending a background application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuspendMode {
    /// Freeze via cgroup freezer or SIGSTOP.
    Freeze,
    /// Throttle CPU via cgroup/nice.
    Throttle,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_to_background_on_focus_lost() {
        let (state, action) = AppState::Active.on_focus_lost();
        assert_eq!(state, AppState::Background);
        assert_eq!(action, TransitionAction::StartSuspendTimer);
    }

    #[test]
    fn background_to_active_on_focus_gained() {
        let (state, action) = AppState::Background.on_focus_gained();
        assert_eq!(state, AppState::Active);
        assert_eq!(action, TransitionAction::CancelSuspendTimer);
    }

    #[test]
    fn frozen_to_active_on_focus_gained() {
        let (state, action) = AppState::Frozen.on_focus_gained();
        assert_eq!(state, AppState::Active);
        assert_eq!(action, TransitionAction::Thaw);
    }

    #[test]
    fn background_to_frozen_on_timer() {
        let (state, action) = AppState::Background.on_suspend_timer(SuspendMode::Freeze);
        assert_eq!(state, AppState::Frozen);
        assert_eq!(action, TransitionAction::ApplyFreeze);
    }

    #[test]
    fn background_to_throttled_on_timer() {
        let (state, action) = AppState::Background.on_suspend_timer(SuspendMode::Throttle);
        assert_eq!(state, AppState::Throttled);
        assert_eq!(action, TransitionAction::ApplyThrottle);
    }

    #[test]
    fn already_frozen_ignores_focus_lost() {
        let (state, action) = AppState::Frozen.on_focus_lost();
        assert_eq!(state, AppState::Frozen);
        assert_eq!(action, TransitionAction::NoOp);
    }

    #[test]
    fn active_ignores_suspend_timer() {
        let (state, action) = AppState::Active.on_suspend_timer(SuspendMode::Freeze);
        assert_eq!(state, AppState::Active);
        assert_eq!(action, TransitionAction::NoOp);
    }

    #[test]
    fn frozen_ignores_suspend_timer() {
        let (state, action) = AppState::Frozen.on_suspend_timer(SuspendMode::Throttle);
        assert_eq!(state, AppState::Frozen);
        assert_eq!(action, TransitionAction::NoOp);
    }
}
