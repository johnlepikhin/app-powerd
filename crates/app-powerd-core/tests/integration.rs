use std::time::Duration;

use tokio::sync::oneshot;

use app_powerd_core::config::Config;
use app_powerd_core::desktop::window::WindowInfo;
use app_powerd_core::engine::{Engine, EngineEvent};
use app_powerd_core::ipc::protocol::{IpcRequest, IpcResponse};

fn test_config() -> Config {
    serde_yaml::from_str(
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
    let (engine, tx) = Engine::new(config, config_path);

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
        assert_eq!(apps[0].state, "ACTIVE");
        assert_eq!(apps[0].wm_class, Some("Firefox".to_string()));
    } else {
        panic!("expected AppList, got {:?}", response);
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
        let firefox = apps.iter().find(|a| a.wm_class == Some("Firefox".to_string())).unwrap();
        let chrome = apps.iter().find(|a| a.wm_class == Some("Chrome".to_string())).unwrap();
        assert_eq!(firefox.state, "BACKGROUND");
        assert_eq!(chrome.state, "ACTIVE");
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
    let (engine, tx) = Engine::new(config, config_path);

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
    std::fs::write(
        &config_path,
        "version: 1\ndefaults:\n  enabled: true\n",
    )
    .unwrap();

    let (engine, tx) = Engine::new(config, config_path.clone());
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
    let (engine, tx) = Engine::new(config, config_path);

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
        let player = apps.iter().find(|a| a.wm_class == Some("Player".to_string())).unwrap();
        assert_eq!(player.state, "BACKGROUND", "fullscreen guard should block freeze");
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
        assert!(metrics.guard_blocks_total >= 1, "guard should have blocked at least once");
    }

    tx.send(EngineEvent::Shutdown).await.unwrap();
    engine_handle.await.unwrap();
}
