use std::time::Duration;

use tokio::sync::oneshot;

use app_powerd_core::config::Config;
use app_powerd_core::desktop::{Desktop, WindowInfo};
use app_powerd_core::engine::{Engine, EngineEvent};
use app_powerd_core::ipc::protocol::{IpcRequest, IpcResponse};
use app_powerd_core::state::AppState;
use app_powerd_core::system::power::PowerSource;

fn test_config() -> Config {
    serde_yaml_ng::from_str(
        r#"
version: 1
defaults:
  enabled: true
  mode:
    ac: enable
    battery: enable
  timing:
    suspend_delay: "100ms"
    resume_grace: "50ms"
    min_suspend: "10ms"
  guards:
    audio_active: check
    fullscreen: check
"#,
    )
    .unwrap()
}

fn make_window(id: u64, pid: u32, wm_class: &str) -> WindowInfo {
    let mut info = WindowInfo::new(id);
    info.pid = Some(pid);
    info.title = Some(format!("Window {id}"));
    info.wm_class = Some(wm_class.to_string());
    info.executable = Some(wm_class.to_lowercase());
    info.cmdline = Some(format!("/usr/bin/{}", wm_class.to_lowercase()));
    info
}

/// Test 1: Focus changes trigger correct state transitions.
#[tokio::test]
async fn focus_changed_sets_active_and_background() {
    let config = test_config();
    let config_path = std::path::PathBuf::from("/tmp/test-config.yaml");
    let (engine, tx) = Engine::new(config, config_path).expect("engine init");

    let engine_handle = tokio::spawn(engine.run());

    // Focus on window 1
    let window1 = make_window(1, 1000, "Firefox");
    tx.send(EngineEvent::FocusChanged(window1)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;

    // Check: Firefox should be Active
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(EngineEvent::IpcRequest {
        request: IpcRequest::List,
        reply: reply_tx,
    })
    .await
    .unwrap();

    let response = reply_rx.await.unwrap();
    if let IpcResponse::AppList { apps } = &response {
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].state, AppState::Active);
        assert_eq!(apps[0].wm_class, Some("Firefox".to_string()));
    } else {
        panic!("expected AppList, got {response:?}");
    }

    // Focus on window 2 — should background window 1
    let window2 = make_window(2, 2000, "Chrome");
    tx.send(EngineEvent::FocusChanged(window2)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;

    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(EngineEvent::IpcRequest {
        request: IpcRequest::List,
        reply: reply_tx,
    })
    .await
    .unwrap();

    let response = reply_rx.await.unwrap();
    if let IpcResponse::AppList { apps } = &response {
        assert_eq!(apps.len(), 2);
        let firefox = apps
            .iter()
            .find(|a| a.wm_class == Some("Firefox".to_string()))
            .unwrap();
        let chrome = apps
            .iter()
            .find(|a| a.wm_class == Some("Chrome".to_string()))
            .unwrap();
        assert_eq!(firefox.state, AppState::Background);
        assert_eq!(chrome.state, AppState::Active);
    } else {
        panic!("expected AppList");
    }

    // Shutdown
    tx.send(EngineEvent::Shutdown).await.unwrap();
    engine_handle.await.unwrap();
}

/// Test 2: IPC status round-trip.
#[tokio::test]
async fn ipc_status_roundtrip() {
    let config = test_config();
    let config_path = std::path::PathBuf::from("/tmp/test-config.yaml");
    let (engine, tx) = Engine::new(config, config_path).expect("engine init");

    let engine_handle = tokio::spawn(engine.run());

    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(EngineEvent::IpcRequest {
        request: IpcRequest::Status,
        reply: reply_tx,
    })
    .await
    .unwrap();

    let response = reply_rx.await.unwrap();
    if let IpcResponse::Status {
        enabled,
        tracked_apps,
        ..
    } = response
    {
        assert!(enabled);
        assert_eq!(tracked_apps, 0);
    } else {
        panic!("expected Status");
    }

    tx.send(EngineEvent::Shutdown).await.unwrap();
    engine_handle.await.unwrap();
}

/// Test 3: Config reload via IPC writes new config then triggers reload.
#[tokio::test]
async fn config_reload_flow() {
    let config = test_config();
    let tmp_dir = std::env::temp_dir().join("app-powerd-test");
    let _ = std::fs::create_dir_all(&tmp_dir);
    let config_path = tmp_dir.join("config.yaml");

    // Write initial config
    std::fs::write(&config_path, "version: 1\ndefaults:\n  enabled: true\n").unwrap();

    let (engine, tx) = Engine::new(config, config_path.clone()).expect("engine init");
    let engine_handle = tokio::spawn(engine.run());

    // Write updated config with a rule
    std::fs::write(
        &config_path,
        r#"
version: 1
defaults:
  enabled: true
rules:
  - id: test-rule
    match:
      executable: [test-app]
    policy:
      action: throttle
"#,
    )
    .unwrap();

    // Trigger reload via IPC
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(EngineEvent::IpcRequest {
        request: IpcRequest::ReloadConfig,
        reply: reply_tx,
    })
    .await
    .unwrap();

    let response = reply_rx.await.unwrap();
    assert!(matches!(response, IpcResponse::Ok { .. }));

    // Give the engine time to process the ConfigReloaded event
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Check via stats that config was reloaded
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(EngineEvent::IpcRequest {
        request: IpcRequest::Stats,
        reply: reply_tx,
    })
    .await
    .unwrap();

    let response = reply_rx.await.unwrap();
    if let IpcResponse::Stats { metrics } = response {
        assert!(metrics.config_reloads_total >= 1);
    } else {
        panic!("expected Stats");
    }

    tx.send(EngineEvent::Shutdown).await.unwrap();
    engine_handle.await.unwrap();

    // Cleanup
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// Test 4: Guards block suspend when audio is active (simulated via fullscreen guard).
#[tokio::test]
async fn guards_block_suspend() {
    let config = test_config();
    let config_path = std::path::PathBuf::from("/tmp/test-config.yaml");
    let (engine, tx) = Engine::new(config, config_path).expect("engine init");

    let engine_handle = tokio::spawn(engine.run());

    // Create a fullscreen window
    let mut window = make_window(1, 1000, "Player");
    window.is_fullscreen = true;
    tx.send(EngineEvent::FocusChanged(window)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;

    // Focus another window to background the fullscreen one
    let window2 = make_window(2, 2000, "Editor");
    tx.send(EngineEvent::FocusChanged(window2)).await.unwrap();

    // Wait longer than suspend_delay (100ms in test config)
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Check: Player should still be BACKGROUND (not FROZEN) because fullscreen guard blocks
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(EngineEvent::IpcRequest {
        request: IpcRequest::List,
        reply: reply_tx,
    })
    .await
    .unwrap();

    let response = reply_rx.await.unwrap();
    if let IpcResponse::AppList { apps } = &response {
        let player = apps
            .iter()
            .find(|a| a.wm_class == Some("Player".to_string()))
            .unwrap();
        assert_eq!(
            player.state,
            AppState::Background,
            "fullscreen guard should block freeze"
        );
    } else {
        panic!("expected AppList");
    }

    // Check guard_blocks_total metric
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(EngineEvent::IpcRequest {
        request: IpcRequest::Stats,
        reply: reply_tx,
    })
    .await
    .unwrap();

    let response = reply_rx.await.unwrap();
    if let IpcResponse::Stats { metrics } = response {
        assert!(
            metrics.guard_blocks_total >= 1,
            "guard should have blocked at least once"
        );
    }

    tx.send(EngineEvent::Shutdown).await.unwrap();
    engine_handle.await.unwrap();
}

/// Helper: query the app list from the engine via IPC.
async fn query_app_list(
    tx: &tokio::sync::mpsc::Sender<EngineEvent>,
) -> Vec<app_powerd_core::ipc::protocol::AppInfo> {
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(EngineEvent::IpcRequest {
        request: IpcRequest::List,
        reply: reply_tx,
    })
    .await
    .unwrap();
    match reply_rx.await.unwrap() {
        IpcResponse::AppList { apps } => apps,
        other => panic!("expected AppList, got {other:?}"),
    }
}

/// Test 5: WindowClosed removes tracked app and restores its state.
#[tokio::test]
async fn window_closed_cleanup() {
    let config = test_config();
    let config_path = std::path::PathBuf::from("/tmp/test-config.yaml");
    let (engine, tx) = Engine::new(config, config_path).expect("engine init");
    let engine_handle = tokio::spawn(engine.run());

    // Track two apps
    tx.send(EngineEvent::FocusChanged(make_window(1, 1000, "App1")))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    tx.send(EngineEvent::FocusChanged(make_window(2, 2000, "App2")))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;

    // App1 is now Background, App2 is Active
    let apps = query_app_list(&tx).await;
    assert_eq!(apps.len(), 2);

    // Close App1's window
    tx.send(EngineEvent::WindowClosed { window_id: 1 })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;

    // App1 should be removed
    let apps = query_app_list(&tx).await;
    assert_eq!(apps.len(), 1);
    assert_eq!(apps[0].wm_class, Some("App2".to_string()));

    tx.send(EngineEvent::Shutdown).await.unwrap();
    engine_handle.await.unwrap();
}

/// Test 6: Power source toggle disables/enables management.
#[tokio::test]
async fn power_source_toggle() {
    // Config: ac=disable, battery=enable (default mode settings)
    let config: Config = serde_yaml_ng::from_str(
        r#"
version: 1
defaults:
  enabled: true
  mode:
    ac: disable
    battery: enable
  timing:
    suspend_delay: "100ms"
    resume_grace: "50ms"
    min_suspend: "10ms"
"#,
    )
    .unwrap();
    let config_path = std::path::PathBuf::from("/tmp/test-config.yaml");
    let (engine, tx) = Engine::new(config, config_path).expect("engine init");
    let engine_handle = tokio::spawn(engine.run());

    // Start on Battery — management should be enabled
    tx.send(EngineEvent::PowerSourceChanged(PowerSource::Battery))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;

    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(EngineEvent::IpcRequest {
        request: IpcRequest::Status,
        reply: reply_tx,
    })
    .await
    .unwrap();
    if let IpcResponse::Status { enabled, .. } = reply_rx.await.unwrap() {
        assert!(enabled, "should be enabled on battery");
    } else {
        panic!("expected Status");
    }

    // Switch to AC — management should be disabled
    tx.send(EngineEvent::PowerSourceChanged(PowerSource::Ac))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;

    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(EngineEvent::IpcRequest {
        request: IpcRequest::Status,
        reply: reply_tx,
    })
    .await
    .unwrap();
    if let IpcResponse::Status { enabled, .. } = reply_rx.await.unwrap() {
        assert!(!enabled, "should be disabled on AC");
    } else {
        panic!("expected Status");
    }

    // Switch back to Battery — management should be re-enabled
    tx.send(EngineEvent::PowerSourceChanged(PowerSource::Battery))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;

    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(EngineEvent::IpcRequest {
        request: IpcRequest::Status,
        reply: reply_tx,
    })
    .await
    .unwrap();
    if let IpcResponse::Status { enabled, .. } = reply_rx.await.unwrap() {
        assert!(enabled, "should be re-enabled on battery");
    } else {
        panic!("expected Status");
    }

    tx.send(EngineEvent::Shutdown).await.unwrap();
    engine_handle.await.unwrap();
}

/// Test: SetPowerOverride forces effective power source and clearing returns to detected.
#[tokio::test]
async fn power_source_override_flow() {
    // Config: ac=disable, battery=enable
    let config: Config = serde_yaml_ng::from_str(
        r#"
version: 1
defaults:
  enabled: true
  mode:
    ac: disable
    battery: enable
  timing:
    suspend_delay: "100ms"
    resume_grace: "50ms"
    min_suspend: "10ms"
"#,
    )
    .unwrap();
    let config_path = std::path::PathBuf::from("/tmp/test-config.yaml");
    let (engine, tx) = Engine::new(config, config_path).expect("engine init");
    let engine_handle = tokio::spawn(engine.run());

    // Detected source = AC → management disabled. Round-tripping Status
    // here also serves as a synchronization barrier ensuring the prior
    // PowerSourceChanged event has been processed (single-threaded engine).
    tx.send(EngineEvent::PowerSourceChanged(PowerSource::Ac))
        .await
        .unwrap();
    let status = query_status(&tx).await;
    assert!(!status.enabled, "should be disabled on AC");
    assert_eq!(status.power_source, PowerSource::Ac);
    assert_eq!(status.forced_power_source, None);

    // Force battery → management enabled, override visible
    let resp = send_set_override(&tx, Some(PowerSource::Battery)).await;
    assert!(matches!(resp, IpcResponse::Ok { .. }));

    let status = query_status(&tx).await;
    assert!(status.enabled, "should be enabled when forced to battery");
    assert_eq!(status.power_source, PowerSource::Ac, "detected unchanged");
    assert_eq!(status.forced_power_source, Some(PowerSource::Battery));

    // Detection change while forced: no effect on management
    tx.send(EngineEvent::PowerSourceChanged(PowerSource::Battery))
        .await
        .unwrap();
    let status = query_status(&tx).await;
    assert!(status.enabled);
    assert_eq!(status.power_source, PowerSource::Battery);
    assert_eq!(status.forced_power_source, Some(PowerSource::Battery));

    // Detected flips back to AC; still forced to battery → still managing
    tx.send(EngineEvent::PowerSourceChanged(PowerSource::Ac))
        .await
        .unwrap();
    let status = query_status(&tx).await;
    assert!(status.enabled, "still managing while forced");

    // Edge case: re-applying the same override is idempotent
    let resp = send_set_override(&tx, Some(PowerSource::Battery)).await;
    assert!(matches!(resp, IpcResponse::Ok { .. }));
    let status = query_status(&tx).await;
    assert_eq!(status.forced_power_source, Some(PowerSource::Battery));

    // Edge case: forcing source equal to detected source still tracks override
    let resp = send_set_override(&tx, Some(PowerSource::Ac)).await;
    assert!(matches!(resp, IpcResponse::Ok { .. }));
    let status = query_status(&tx).await;
    assert!(!status.enabled, "forced AC behaves like detected AC");
    assert_eq!(status.forced_power_source, Some(PowerSource::Ac));

    // Edge case: forcing Unknown is rejected
    let resp = send_set_override(&tx, Some(PowerSource::Unknown)).await;
    assert!(
        matches!(resp, IpcResponse::Error { .. }),
        "forcing Unknown should be rejected, got {resp:?}"
    );
    let status = query_status(&tx).await;
    assert_eq!(
        status.forced_power_source,
        Some(PowerSource::Ac),
        "rejected request must not mutate state"
    );

    // Clear override → effective source = detected (AC) → disabled
    let resp = send_set_override(&tx, None).await;
    assert!(matches!(resp, IpcResponse::Ok { .. }));

    let status = query_status(&tx).await;
    assert!(!status.enabled, "follows detected AC after clearing override");
    assert_eq!(status.forced_power_source, None);

    tx.send(EngineEvent::Shutdown).await.unwrap();
    engine_handle.await.unwrap();
}

#[derive(Debug)]
struct StatusSnapshot {
    enabled: bool,
    power_source: PowerSource,
    forced_power_source: Option<PowerSource>,
}

/// Round-trip a Status request; doubles as a sync barrier on the engine task.
async fn query_status(tx: &tokio::sync::mpsc::Sender<EngineEvent>) -> StatusSnapshot {
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(EngineEvent::IpcRequest {
        request: IpcRequest::Status,
        reply: reply_tx,
    })
    .await
    .unwrap();
    match reply_rx.await.unwrap() {
        IpcResponse::Status {
            enabled,
            power_source,
            forced_power_source,
            ..
        } => StatusSnapshot {
            enabled,
            power_source,
            forced_power_source,
        },
        other => panic!("expected Status, got {other:?}"),
    }
}

async fn send_set_override(
    tx: &tokio::sync::mpsc::Sender<EngineEvent>,
    source: Option<PowerSource>,
) -> IpcResponse {
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(EngineEvent::IpcRequest {
        request: IpcRequest::SetPowerOverride { source },
        reply: reply_tx,
    })
    .await
    .unwrap();
    reply_rx.await.unwrap()
}

/// Test 7: IPC Freeze/Thaw commands and error on pid=0.
#[tokio::test]
async fn ipc_freeze_thaw() {
    let config = test_config();
    let config_path = std::path::PathBuf::from("/tmp/test-config.yaml");
    let (engine, tx) = Engine::new(config, config_path).expect("engine init");
    let engine_handle = tokio::spawn(engine.run());

    // Freeze pid=0 should error
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(EngineEvent::IpcRequest {
        request: IpcRequest::Freeze { pid: 0 },
        reply: reply_tx,
    })
    .await
    .unwrap();
    let response = reply_rx.await.unwrap();
    assert!(
        matches!(response, IpcResponse::Error { .. }),
        "freeze pid=0 should error"
    );

    // Thaw pid=0 should error
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(EngineEvent::IpcRequest {
        request: IpcRequest::Thaw { pid: 0 },
        reply: reply_tx,
    })
    .await
    .unwrap();
    let response = reply_rx.await.unwrap();
    assert!(
        matches!(response, IpcResponse::Error { .. }),
        "thaw pid=0 should error"
    );

    tx.send(EngineEvent::Shutdown).await.unwrap();
    engine_handle.await.unwrap();
}

/// Test: WorkspaceChanged and ActivationRequested events are accepted by the
/// engine and leave the running event loop intact. Real pre-thaw requires a
/// process actually frozen via SIGSTOP, which integration tests can't simulate
/// safely; this test just verifies the dispatch wiring.
#[tokio::test]
async fn pre_thaw_events_dispatch_cleanly() {
    let config = test_config();
    let config_path = std::path::PathBuf::from("/tmp/test-config.yaml");
    let (engine, tx) = Engine::new(config, config_path).expect("engine init");
    let engine_handle = tokio::spawn(engine.run());

    // Track an app on desktop 3.
    let mut window = make_window(1, 1000, "Tray");
    window.desktop = Some(Desktop::Index(3));
    tx.send(EngineEvent::FocusChanged(window)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;

    // Track a second app with desktop = None. WorkspaceChanged must never
    // pre-thaw it because the engine filters on `desktop.is_some_and(...)`.
    let window_no_desktop = make_window(2, 1001, "Sticky");
    tx.send(EngineEvent::FocusChanged(window_no_desktop))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;

    // Snapshot states before sending WorkspaceChanged.
    let before = query_app_list(&tx).await;
    let sticky_before = before
        .iter()
        .find(|a| a.wm_class == Some("Sticky".to_string()))
        .expect("Sticky tracked")
        .state;

    // Non-matching desktop — no-op.
    tx.send(EngineEvent::WorkspaceChanged { desktop: 99 })
        .await
        .unwrap();
    // Matching desktop — no-op too, because the app is Active, not Frozen.
    tx.send(EngineEvent::WorkspaceChanged { desktop: 3 })
        .await
        .unwrap();
    // Activation for unknown window — no-op.
    tx.send(EngineEvent::ActivationRequested { window_id: 999 })
        .await
        .unwrap();

    // Sanity barrier: engine still responsive.
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(EngineEvent::IpcRequest {
        request: IpcRequest::Status,
        reply: reply_tx,
    })
    .await
    .unwrap();
    let response = reply_rx.await.unwrap();
    assert!(matches!(response, IpcResponse::Status { .. }));

    // Negative case: Sticky (desktop=None) must remain in its prior state —
    // WorkspaceChanged for any desktop must not touch it.
    let after = query_app_list(&tx).await;
    let sticky_after = after
        .iter()
        .find(|a| a.wm_class == Some("Sticky".to_string()))
        .expect("Sticky still tracked")
        .state;
    assert_eq!(
        sticky_after, sticky_before,
        "WorkspaceChanged must not touch apps with desktop=None"
    );

    tx.send(EngineEvent::Shutdown).await.unwrap();
    engine_handle.await.unwrap();
}

/// Test: workspace change pre-thaws a real Frozen app.
///
/// Uses two child `sleep` processes whose PIDs can safely receive SIGSTOP /
/// SIGCONT without affecting the test harness. The "Other" focus window must
/// not target a critical PID (e.g. the test process itself) because its app
/// would also get frozen after the suspend timer fires.
#[tokio::test]
async fn workspace_change_pre_thaws_frozen_app() {
    // Spawn two sleeping children: one for the tracked app, one to steal focus.
    let mut tray_child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn tray sleep");
    let mut other_child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn other sleep");
    let tray_pid = tray_child.id();
    let other_pid = other_child.id();

    let config = test_config();
    let config_path = std::path::PathBuf::from("/tmp/test-config.yaml");
    let (engine, tx) = Engine::new(config, config_path).expect("engine init");
    let engine_handle = tokio::spawn(engine.run());

    // Track an app on desktop 7.
    let mut window = make_window(1, tray_pid, "Tray");
    window.desktop = Some(Desktop::Index(7));
    tx.send(EngineEvent::FocusChanged(window)).await.unwrap();

    // Background it by focusing another (different-PID) window.
    tx.send(EngineEvent::FocusChanged(make_window(2, other_pid, "Other")))
        .await
        .unwrap();

    // Wait > suspend_delay (100ms) so the suspend timer fires → Tray becomes Frozen.
    tokio::time::sleep(Duration::from_millis(250)).await;

    let apps = query_app_list(&tx).await;
    let tray = apps
        .iter()
        .find(|a| a.wm_class == Some("Tray".to_string()))
        .expect("Tray tracked");
    assert_eq!(
        tray.state,
        AppState::Frozen,
        "expected Tray to be Frozen after suspend_delay"
    );

    // Non-matching desktop — must stay Frozen.
    tx.send(EngineEvent::WorkspaceChanged { desktop: 999 })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let apps = query_app_list(&tx).await;
    let tray = apps
        .iter()
        .find(|a| a.wm_class == Some("Tray".to_string()))
        .expect("Tray tracked");
    assert_eq!(
        tray.state,
        AppState::Frozen,
        "non-matching desktop must not pre-thaw"
    );

    // Matching desktop — must transition out of Frozen into Background.
    tx.send(EngineEvent::WorkspaceChanged { desktop: 7 })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let apps = query_app_list(&tx).await;
    let tray = apps
        .iter()
        .find(|a| a.wm_class == Some("Tray".to_string()))
        .expect("Tray tracked");
    assert_eq!(
        tray.state,
        AppState::Background,
        "matching desktop must pre-thaw Frozen → Background"
    );

    tx.send(EngineEvent::Shutdown).await.unwrap();
    engine_handle.await.unwrap();

    // Best-effort cleanup of the helper children.
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(tray_pid as i32),
        nix::sys::signal::Signal::SIGCONT,
    );
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(tray_pid as i32),
        nix::sys::signal::Signal::SIGTERM,
    );
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(other_pid as i32),
        nix::sys::signal::Signal::SIGCONT,
    );
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(other_pid as i32),
        nix::sys::signal::Signal::SIGTERM,
    );
    let _ = tray_child.wait();
    let _ = other_child.wait();
}

/// Test: workspace change pre-thaws a Frozen app whose desktop is `Desktop::All`
/// (sticky window), regardless of which workspace we switch to.
#[tokio::test]
async fn workspace_change_pre_thaws_sticky_frozen_app() {
    let mut tray_child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn tray sleep");
    let mut other_child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn other sleep");
    let tray_pid = tray_child.id();
    let other_pid = other_child.id();

    let config = test_config();
    let config_path = std::path::PathBuf::from("/tmp/test-config.yaml");
    let (engine, tx) = Engine::new(config, config_path).expect("engine init");
    let engine_handle = tokio::spawn(engine.run());

    // Sticky tray (visible on every workspace).
    let mut window = make_window(1, tray_pid, "StickyTray");
    window.desktop = Some(Desktop::All);
    tx.send(EngineEvent::FocusChanged(window)).await.unwrap();
    tx.send(EngineEvent::FocusChanged(make_window(2, other_pid, "Other")))
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(250)).await;
    let apps = query_app_list(&tx).await;
    let tray = apps
        .iter()
        .find(|a| a.wm_class == Some("StickyTray".to_string()))
        .expect("StickyTray tracked");
    assert_eq!(tray.state, AppState::Frozen);

    // Any workspace switch should pre-thaw a sticky app.
    tx.send(EngineEvent::WorkspaceChanged { desktop: 42 })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let apps = query_app_list(&tx).await;
    let tray = apps
        .iter()
        .find(|a| a.wm_class == Some("StickyTray".to_string()))
        .expect("StickyTray tracked");
    assert_eq!(
        tray.state,
        AppState::Background,
        "Desktop::All must pre-thaw on any workspace switch"
    );

    tx.send(EngineEvent::Shutdown).await.unwrap();
    engine_handle.await.unwrap();

    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(tray_pid as i32),
        nix::sys::signal::Signal::SIGCONT,
    );
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(tray_pid as i32),
        nix::sys::signal::Signal::SIGTERM,
    );
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(other_pid as i32),
        nix::sys::signal::Signal::SIGCONT,
    );
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(other_pid as i32),
        nix::sys::signal::Signal::SIGTERM,
    );
    let _ = tray_child.wait();
    let _ = other_child.wait();
}

/// Test 8: Config reload rematches tracked apps — policy changes take effect.
#[tokio::test]
async fn config_reload_rematches() {
    let tmp_dir = std::env::temp_dir().join("app-powerd-test-rematch");
    let _ = std::fs::create_dir_all(&tmp_dir);
    let config_path = tmp_dir.join("config.yaml");

    // Write initial config: all apps get default freeze policy
    let initial_yaml = r#"
version: 1
defaults:
  enabled: true
  mode:
    ac: enable
    battery: enable
  timing:
    suspend_delay: "100ms"
    resume_grace: "50ms"
    min_suspend: "10ms"
"#;
    std::fs::write(&config_path, initial_yaml).unwrap();

    let config: Config = serde_yaml_ng::from_str(initial_yaml).unwrap();
    let (engine, tx) = Engine::new(config, config_path.clone()).expect("engine init");
    let engine_handle = tokio::spawn(engine.run());

    // Track an app
    tx.send(EngineEvent::FocusChanged(make_window(1, 1000, "MyApp")))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;

    // Background it
    tx.send(EngineEvent::FocusChanged(make_window(2, 2000, "Other")))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;

    // Write new config with an ignore rule for MyApp
    std::fs::write(
        &config_path,
        r#"
version: 1
defaults:
  enabled: true
  mode:
    ac: enable
    battery: enable
  timing:
    suspend_delay: "100ms"
    resume_grace: "50ms"
    min_suspend: "10ms"
rules:
  - id: ignore-myapp
    match:
      wm_class: [MyApp]
    policy:
      action: ignore
"#,
    )
    .unwrap();

    // Trigger reload via IPC
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(EngineEvent::IpcRequest {
        request: IpcRequest::ReloadConfig,
        reply: reply_tx,
    })
    .await
    .unwrap();
    let _response = reply_rx.await.unwrap();

    // Give engine time to process ConfigReloaded event
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Wait longer than suspend_delay to verify MyApp doesn't get frozen
    tokio::time::sleep(Duration::from_millis(200)).await;

    // MyApp should still be Background (not Frozen), because its policy is now Ignore
    let apps = query_app_list(&tx).await;
    let my_app = apps
        .iter()
        .find(|a| a.wm_class == Some("MyApp".to_string()))
        .expect("MyApp should be tracked");
    assert_eq!(
        my_app.state,
        AppState::Background,
        "MyApp should stay Background with ignore policy (not frozen)"
    );

    tx.send(EngineEvent::Shutdown).await.unwrap();
    engine_handle.await.unwrap();

    let _ = std::fs::remove_dir_all(&tmp_dir);
}
