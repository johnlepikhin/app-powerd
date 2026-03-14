pub mod audio;
pub mod camera;
pub mod input;

use crate::config::{GuardAction, GuardsConfig};

/// Reason a guard blocked an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardBlockReason {
    AudioActive,
    MicActive,
    CameraActive,
    Fullscreen,
    RecentInput,
}

impl std::fmt::Display for GuardBlockReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AudioActive => write!(f, "audio_active"),
            Self::MicActive => write!(f, "mic_active"),
            Self::CameraActive => write!(f, "camera_active"),
            Self::Fullscreen => write!(f, "fullscreen"),
            Self::RecentInput => write!(f, "recent_input"),
        }
    }
}

/// Result of a guard check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardResult {
    /// All checks passed, action is allowed.
    Allow,
    /// A guard blocked the action.
    Block(GuardBlockReason),
}

/// Check all guards for an app. Returns Block if any guard prevents the action.
pub async fn check_guards(
    pids: &[u32],
    guards_config: &GuardsConfig,
    is_fullscreen: bool,
    idle_threshold: Option<std::time::Duration>,
) -> GuardResult {
    // Audio + mic guard (single pw-dump call)
    if guards_config.audio_active == GuardAction::Check
        || guards_config.mic_active == GuardAction::Check
    {
        let activity = audio::check_audio_activity(pids).await;

        if guards_config.audio_active == GuardAction::Check && activity.playing {
            return GuardResult::Block(GuardBlockReason::AudioActive);
        }
        if guards_config.mic_active == GuardAction::Check && activity.mic_active {
            return GuardResult::Block(GuardBlockReason::MicActive);
        }
    }

    // Camera guard
    if guards_config.camera_active == GuardAction::Check
        && camera::is_using_camera(pids).await
    {
        return GuardResult::Block(GuardBlockReason::CameraActive);
    }

    // Fullscreen guard
    if guards_config.fullscreen == GuardAction::Check && is_fullscreen {
        return GuardResult::Block(GuardBlockReason::Fullscreen);
    }

    // Recent input guard
    if let Some(threshold) = idle_threshold.or(guards_config.input_idle) {
        if input::has_recent_input(threshold) {
            return GuardResult::Block(GuardBlockReason::RecentInput);
        }
    }

    GuardResult::Allow
}
