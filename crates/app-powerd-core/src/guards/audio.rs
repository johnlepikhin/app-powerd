use std::time::Duration;

use serde::Deserialize;
use tracing::{debug, warn};

/// Timeout for `pw-dump` subprocess.
const PW_DUMP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Deserialize)]
struct PwNode {
    #[serde(default)]
    info: Option<PwNodeInfo>,
}

#[derive(Deserialize)]
struct PwNodeInfo {
    #[serde(default)]
    props: PwProps,
}

#[derive(Deserialize, Default)]
struct PwProps {
    #[serde(rename = "media.class", default)]
    media_class: Option<String>,
    #[serde(rename = "application.process.id", default)]
    app_pid: Option<PidValue>,
}

/// PipeWire emits `application.process.id` as either a string or a number.
#[derive(Deserialize)]
#[serde(untagged)]
enum PidValue {
    Number(u64),
    Text(String),
}

impl PidValue {
    fn as_u32(&self) -> Option<u32> {
        match self {
            PidValue::Number(n) => u32::try_from(*n).ok(),
            PidValue::Text(s) => s.parse().ok(),
        }
    }
}

/// Combined audio activity result from a single pw-dump call.
#[derive(Default)]
pub struct AudioActivity {
    pub playing: bool,
    pub mic_active: bool,
}

/// Check audio activity (playback and mic) for the given PIDs.
/// Uses pw-dump (PipeWire) to detect active audio streams.
pub async fn check_audio_activity(pids: &[u32]) -> AudioActivity {
    let output = match tokio::time::timeout(
        PW_DUMP_TIMEOUT,
        tokio::process::Command::new("pw-dump").output(),
    )
    .await
    {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            warn!(error = %e, "pw-dump not available");
            return AudioActivity::default();
        }
        Err(_) => {
            warn!("pw-dump timed out after 5s");
            return AudioActivity::default();
        }
    };

    if !output.status.success() {
        warn!(status = %output.status, "pw-dump exited with error");
        return AudioActivity::default();
    }

    let nodes: Vec<PwNode> = match serde_json::from_slice(&output.stdout) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "failed to parse pw-dump JSON output");
            return AudioActivity::default();
        }
    };

    let mut playing = false;
    let mut mic_active = false;

    for node in &nodes {
        let Some(info) = &node.info else { continue };
        let class = info.props.media_class.as_deref().unwrap_or("");

        let is_playback = class == "Stream/Output/Audio";
        let is_capture = class == "Stream/Input/Audio";

        if !is_playback && !is_capture {
            continue;
        }

        let stream_pid = info.props.app_pid.as_ref().and_then(PidValue::as_u32);

        if let Some(stream_pid) = stream_pid {
            if pids.contains(&stream_pid) {
                if is_playback {
                    debug!(pid = stream_pid, class, "active audio playback found");
                    playing = true;
                }
                if is_capture {
                    debug!(pid = stream_pid, class, "active mic stream found");
                    mic_active = true;
                }
                if playing && mic_active {
                    break;
                }
            }
        }
    }

    AudioActivity {
        playing,
        mic_active,
    }
}
