use std::time::Duration;

use tokio::sync::oneshot;

use app_powerd_core::config::Config;
use app_powerd_core::desktop::WindowInfo;
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
    WindowInfo {
        window_id: id,
        pid: Some(pid),
        title: Some(format!("Window {id}")),
        wm_class: Some(wm_class.to_string()),
        app_id: None,
        executable: Some(wm_class.to_lowercase()),
        cmdline: Some(format!("/usr/bin/{}", wm_class.to_lowercase())),
        is_fullscreen: false,
    }
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
