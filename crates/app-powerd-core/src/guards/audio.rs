use tracing::debug;

/// Combined audio activity result from a single pw-dump call.
#[derive(Default)]
pub struct AudioActivity {
    pub playing: bool,
    pub mic_active: bool,
}

/// Check audio activity (playback and mic) for the given PIDs.
/// Uses pw-dump (PipeWire) to detect active audio streams.
pub async fn check_audio_activity(pids: &[u32]) -> AudioActivity {
    let output = match tokio::process::Command::new("pw-dump").output().await {
        Ok(o) => o,
        Err(e) => {
            debug!(error = %e, "pw-dump not available");
            return AudioActivity::default();
        }
    };

    if !output.status.success() {
        return AudioActivity {
            playing: false,
            mic_active: false,
        };
    }

    let json: serde_json::Value = match serde_json::from_slice(&output.stdout) {
        Ok(v) => v,
        Err(_) => {
            return AudioActivity::default()
        }
    };

    let Some(nodes) = json.as_array() else {
        return AudioActivity {
            playing: false,
            mic_active: false,
        };
    };

    let mut playing = false;
    let mut mic_active = false;

    for node in nodes {
        let Some(info) = node.get("info") else { continue };
        let Some(props) = info.get("props") else { continue };

        let class = props
            .get("media.class")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let is_playback = class == "Stream/Output/Audio";
        let is_capture = class == "Stream/Input/Audio";

        if !is_playback && !is_capture {
            continue;
        }

        let stream_pid = props
            .get("application.process.id")
            .and_then(|v| {
                v.as_str()
                    .and_then(|s| s.parse::<u32>().ok())
                    .or_else(|| v.as_u64().map(|n| n as u32))
            });

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

    AudioActivity { playing, mic_active }
}
