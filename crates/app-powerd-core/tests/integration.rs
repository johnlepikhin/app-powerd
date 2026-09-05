use std::time::Duration;

use tokio::sync::oneshot;

use app_powerd_core::config::Config;
use app_powerd_core::desktop::{Desktop, WindowInfo};
use app_powerd_core::engine::{Engine, EngineEvent};
use app_powerd_core::ipc::protocol::{IpcRequest, IpcResponse, Target};
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

/// A real child process for tests to target.
///
/// Tests used to pass invented PIDs such as 1000 and 2000 straight into the
/// engine, which then sent them real SIGSTOPs — on a machine where those PIDs
/// belong to the user, `cargo test` would stop unrelated programs. Every test
/// that reaches a signalling path now owns an actual process instead.
struct TestProc(std::process::Child);

impl TestProc {
    fn spawn() -> Self {
        Self(
            std::process::Command::new("sleep")
                .arg("300")
                .spawn()
                .expect("spawn sleep helper"),
        )
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for TestProc {
    fn drop(&mut self) {
        // Resume before killing: a test that panics between SIGSTOP and SIGCONT
        // would otherwise leave a stopped process behind, and SIGKILL does not
        // reap a stopped process's parent bookkeeping reliably until it runs.
        let pid = nix::unistd::Pid::from_raw(self.0.id() as i32);
        let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGCONT);
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Whether a process is currently stopped, per `/proc/<pid>/stat`.
fn is_stopped(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some(tail) = stat.rfind(')').map(|i| &stat[i + 1..]) else {
        return false;
    };
    tail.split_whitespace().next() == Some("T")
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

    let firefox = TestProc::spawn();
    let chrome = TestProc::spawn();

    // Focus on window 1
    let window1 = make_window(1, firefox.pid(), "Firefox");
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
    let window2 = make_window(2, chrome.pid(), "Chrome");
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

    let player = TestProc::spawn();
    let editor = TestProc::spawn();

    // Create a fullscreen window
    let mut window = make_window(1, player.pid(), "Player");
    window.is_fullscreen = true;
    tx.send(EngineEvent::FocusChanged(window)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;

    // Focus another window to background the fullscreen one
    let window2 = make_window(2, editor.pid(), "Editor");
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

    let app1 = TestProc::spawn();
    let app2 = TestProc::spawn();

    // Track two apps
    tx.send(EngineEvent::FocusChanged(make_window(
        1,
        app1.pid(),
        "App1",
    )))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    tx.send(EngineEvent::FocusChanged(make_window(
        2,
        app2.pid(),
        "App2",
    )))
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
    assert!(
        !status.enabled,
        "follows detected AC after clearing override"
    );
    assert_eq!(status.forced_power_source, None);

    tx.send(EngineEvent::Shutdown).await.unwrap();
    engine_handle.await.unwrap();
}

#[derive(Debug)]
struct StatusSnapshot {
    enabled: bool,
    power_source: PowerSource,
    forced_power_source: Option<PowerSource>,
    cgroup_mode: String,
    protected_apps: usize,
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
            cgroup_mode,
            protected_apps,
            ..
        } => StatusSnapshot {
            enabled,
            power_source,
            forced_power_source,
            cgroup_mode,
            protected_apps,
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
        request: IpcRequest::Freeze {
            target: Target::pid(0),
        },
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
        request: IpcRequest::Thaw {
            target: Target::pid(0),
        },
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

    let tray = TestProc::spawn();
    let sticky = TestProc::spawn();

    // Track an app on desktop 3.
    let mut window = make_window(1, tray.pid(), "Tray");
    window.desktop = Some(Desktop::Index(3));
    tx.send(EngineEvent::FocusChanged(window)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;

    // Track a second app with desktop = None. WorkspaceChanged must never
    // pre-thaw it because the engine filters on `desktop.is_some_and(...)`.
    let window_no_desktop = make_window(2, sticky.pid(), "Sticky");
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
    tx.send(EngineEvent::FocusChanged(make_window(
        2, other_pid, "Other",
    )))
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
    tx.send(EngineEvent::FocusChanged(make_window(
        2, other_pid, "Other",
    )))
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

    let myapp = TestProc::spawn();
    let other = TestProc::spawn();

    // Track an app
    tx.send(EngineEvent::FocusChanged(make_window(
        1,
        myapp.pid(),
        "MyApp",
    )))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;

    // Background it
    tx.send(EngineEvent::FocusChanged(make_window(
        2,
        other.pid(),
        "Other",
    )))
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

// ---------------------------------------------------------------------------
// 2.0.0: process-liveness reconciliation, protection, journal recovery.
// ---------------------------------------------------------------------------

/// Config whose defaults freeze quickly, with a reconcile tick fast enough to
/// observe inside a test.
fn reactive_config(extra: &str) -> Config {
    let yaml = format!(
        r#"
version: 1
defaults:
  enabled: true
  reconcile_interval: "50ms"
  protection:
    dbus_check: false
  mode:
    ac: enable
    battery: enable
  timing:
    suspend_delay: "80ms"
    resume_grace: "10ms"
    min_suspend: "10ms"
  guards:
    audio_active: ignore
    mic_active: ignore
    camera_active: ignore
    fullscreen: ignore
{extra}
"#
    );
    serde_yaml_ng::from_str(&yaml).expect("test config parses")
}

fn temp_journal(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("app-powerd-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir.join("frozen.json")
}

/// Poll until `check` holds, so tests do not depend on a fixed sleep.
async fn wait_until<F: FnMut() -> bool>(mut check: F, what: &str) {
    for _ in 0..200 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// A closed application must disappear from the model on its own.
///
/// This is the defect that produced 62 641 log lines for one closed app: the
/// entry outlived its processes and was re-signalled forever. Removal is now
/// driven by process liveness, not by window events, so no window-close event is
/// sent here.
#[tokio::test]
async fn dead_processes_retire_the_application() {
    let config = reactive_config("");
    let (engine, tx) = Engine::new(config, std::path::PathBuf::from("/tmp/test-config.yaml"))
        .expect("engine init");
    let engine_handle = tokio::spawn(engine.run());

    let doomed = TestProc::spawn();
    let doomed_pid = doomed.pid();
    let survivor = TestProc::spawn();

    tx.send(EngineEvent::FocusChanged(make_window(
        1, doomed_pid, "Doomed",
    )))
    .await
    .unwrap();
    tx.send(EngineEvent::FocusChanged(make_window(
        2,
        survivor.pid(),
        "Survivor",
    )))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(query_app_list(&tx).await.len(), 2);

    // Kill the process without telling the engine its window closed.
    drop(doomed);
    wait_until(
        || !std::path::Path::new(&format!("/proc/{doomed_pid}")).exists(),
        "process to exit",
    )
    .await;

    // Reconciliation runs on a blocking pool and reports back through the event
    // channel, so poll instead of betting on one fixed sleep covering the
    // round-trip on a loaded machine. `wait_until` is synchronous and cannot
    // hold the async query, hence the explicit loop.
    let mut apps = Vec::new();
    for _ in 0..20 {
        tx.send(EngineEvent::Reconcile).await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        apps = query_app_list(&tx).await;
        if apps.len() == 1 {
            break;
        }
    }
    assert_eq!(
        apps.len(),
        1,
        "the dead application must be dropped, got {apps:?}"
    );
    assert_eq!(apps[0].app_id, "Survivor", "the live one must be kept");

    tx.send(EngineEvent::Shutdown).await.unwrap();
    engine_handle.await.unwrap();
}

/// The counterpart of the test above: reconciliation must not remove anything
/// that is still running.
#[tokio::test]
async fn live_processes_survive_reconciliation() {
    let config = reactive_config("");
    let (engine, tx) = Engine::new(config, std::path::PathBuf::from("/tmp/test-config.yaml"))
        .expect("engine init");
    let engine_handle = tokio::spawn(engine.run());

    let alive = TestProc::spawn();
    tx.send(EngineEvent::FocusChanged(make_window(
        1,
        alive.pid(),
        "Alive",
    )))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    for _ in 0..3 {
        tx.send(EngineEvent::Reconcile).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let apps = query_app_list(&tx).await;
    assert_eq!(apps.len(), 1, "a live application must not be reaped");
    assert_eq!(apps[0].pids, vec![alive.pid()]);

    tx.send(EngineEvent::Shutdown).await.unwrap();
    engine_handle.await.unwrap();
}

/// The built-in deny-list outranks the configuration.
///
/// The rule here explicitly asks for `freeze`, which is the case the acceptance
/// criteria call out: a user rule must not be able to suspend a modal dialog.
#[tokio::test]
async fn protected_app_is_never_suspended_even_when_a_rule_demands_it() {
    let config = reactive_config(
        r#"
rules:
  - id: force-freeze-zenity
    match:
      executable: [zenity]
    policy:
      action: freeze
      suspend_delay: "20ms"
"#,
    );
    let (engine, tx) = Engine::new(config, std::path::PathBuf::from("/tmp/test-config.yaml"))
        .expect("engine init");
    let engine_handle = tokio::spawn(engine.run());

    let dialog = TestProc::spawn();
    let dialog_pid = dialog.pid();
    let other = TestProc::spawn();

    // A window that identifies as zenity, matching the user's explicit rule.
    let mut window = make_window(1, dialog_pid, "Zenity");
    window.executable = Some("zenity".to_string());
    tx.send(EngineEvent::FocusChanged(window)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;

    // Background it and wait well past the 20ms suspend delay.
    tx.send(EngineEvent::FocusChanged(make_window(
        2,
        other.pid(),
        "Other",
    )))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    let apps = query_app_list(&tx).await;
    let dialog_info = apps
        .iter()
        .find(|a| a.app_id == "Zenity")
        .expect("dialog is tracked");
    assert_ne!(
        dialog_info.state,
        AppState::Frozen,
        "a protected app must never reach Frozen"
    );
    assert!(
        dialog_info.protected.is_some(),
        "the reason for the exemption must be reportable, got {dialog_info:?}"
    );
    assert!(
        !is_stopped(dialog_pid),
        "the dialog process must still be running"
    );

    let status = query_status(&tx).await;
    assert!(status.protected_apps >= 1, "status must surface the count");

    tx.send(EngineEvent::Shutdown).await.unwrap();
    engine_handle.await.unwrap();
}

/// Case differences must not split one application into two entries, nor make a
/// rule miss. `zenity` arrives as WM_CLASS "Zenity" and executable "zenity".
#[tokio::test]
async fn app_identity_is_case_insensitive() {
    let config = reactive_config("");
    let (engine, tx) = Engine::new(config, std::path::PathBuf::from("/tmp/test-config.yaml"))
        .expect("engine init");
    let engine_handle = tokio::spawn(engine.run());

    let proc = TestProc::spawn();
    tx.send(EngineEvent::FocusChanged(make_window(
        1,
        proc.pid(),
        "MyApp",
    )))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    tx.send(EngineEvent::FocusChanged(make_window(
        2,
        proc.pid(),
        "myapp",
    )))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;

    let apps = query_app_list(&tx).await;
    assert_eq!(
        apps.len(),
        1,
        "differing case must not create a second entry, got {apps:?}"
    );

    tx.send(EngineEvent::Shutdown).await.unwrap();
    engine_handle.await.unwrap();
}

/// Shutdown must leave nothing stopped, and must clear the journal.
///
/// SIGSTOP cannot be undone by the stopped process, so a daemon that exits
/// without releasing its charges strands them permanently.
#[tokio::test]
async fn shutdown_releases_every_frozen_process() {
    let journal_path = temp_journal("shutdown");
    let journal = app_powerd_core::state::FreezeJournal::load(journal_path.clone()).unwrap();
    let config = reactive_config("");
    let (engine, tx) = Engine::with_journal(
        config,
        std::path::PathBuf::from("/tmp/test-config.yaml"),
        journal,
    )
    .expect("engine init");
    let engine_handle = tokio::spawn(engine.run());

    let target = TestProc::spawn();
    let target_pid = target.pid();
    let other = TestProc::spawn();

    tx.send(EngineEvent::FocusChanged(make_window(
        1, target_pid, "Target",
    )))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    tx.send(EngineEvent::FocusChanged(make_window(
        2,
        other.pid(),
        "Other",
    )))
    .await
    .unwrap();

    wait_until(|| is_stopped(target_pid), "target to be frozen").await;
    assert!(journal_path.exists(), "freezing must be recorded on disk");

    tx.send(EngineEvent::Shutdown).await.unwrap();
    engine_handle.await.unwrap();

    assert!(
        !is_stopped(target_pid),
        "shutdown must resume every frozen process"
    );
    assert!(
        !journal_path.exists(),
        "a clean shutdown must clear the journal"
    );
    let _ = std::fs::remove_dir_all(journal_path.parent().unwrap());
}

/// The journal must be written before the first signal, so a daemon that dies
/// mid-freeze still leaves a recoverable record.
#[tokio::test]
async fn journal_records_frozen_processes_and_recovery_releases_them() {
    let journal_path = temp_journal("recovery");
    let journal = app_powerd_core::state::FreezeJournal::load(journal_path.clone()).unwrap();
    let config = reactive_config("");
    let (engine, tx) = Engine::with_journal(
        config,
        std::path::PathBuf::from("/tmp/test-config.yaml"),
        journal,
    )
    .expect("engine init");
    let engine_handle = tokio::spawn(engine.run());

    let target = TestProc::spawn();
    let target_pid = target.pid();
    let other = TestProc::spawn();

    tx.send(EngineEvent::FocusChanged(make_window(
        1, target_pid, "Target",
    )))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    tx.send(EngineEvent::FocusChanged(make_window(
        2,
        other.pid(),
        "Other",
    )))
    .await
    .unwrap();
    wait_until(|| is_stopped(target_pid), "target to be frozen").await;

    // Abandon the engine without a graceful shutdown, standing in for SIGKILL.
    engine_handle.abort();
    let _ = engine_handle.await;
    assert!(is_stopped(target_pid), "process is still stopped");

    // A fresh daemon replays the journal on startup.
    let report = app_powerd_core::state::FreezeJournal::recover(&journal_path).unwrap();
    assert!(
        report.thawed >= 1,
        "recovery must resume it, got {report:?}"
    );
    assert!(
        !is_stopped(target_pid),
        "recovery must leave nothing stopped"
    );
    assert!(!journal_path.exists(), "recovery clears the journal");
    let _ = std::fs::remove_dir_all(journal_path.parent().unwrap());
}

/// A manual `app-powerd freeze` must be journalled just like an automatic one.
///
/// It suspends real processes, so if the record is missing a killed daemon
/// strands them exactly as an unjournalled automatic freeze would — the CLI
/// must not be a way around the write-ahead rule.
#[tokio::test]
async fn manual_ipc_freeze_is_recorded_in_the_journal() {
    let journal_path = temp_journal("ipc-freeze");
    let journal = app_powerd_core::state::FreezeJournal::load(journal_path.clone()).unwrap();
    let config = reactive_config("");
    let (engine, tx) = Engine::with_journal(
        config,
        std::path::PathBuf::from("/tmp/test-config.yaml"),
        journal,
    )
    .expect("engine init");
    let engine_handle = tokio::spawn(engine.run());

    let target = TestProc::spawn();
    let target_pid = target.pid();

    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(EngineEvent::IpcRequest {
        request: IpcRequest::Freeze {
            target: Target::pid(target_pid),
        },
        reply: reply_tx,
    })
    .await
    .unwrap();
    assert!(
        matches!(reply_rx.await.unwrap(), IpcResponse::Ok { .. }),
        "manual freeze should succeed for an ordinary owned process"
    );
    wait_until(|| is_stopped(target_pid), "target to be frozen by IPC").await;

    // Abandon the daemon without a graceful shutdown, standing in for SIGKILL.
    engine_handle.abort();
    let _ = engine_handle.await;
    assert!(is_stopped(target_pid), "process is still stopped");

    let report = app_powerd_core::state::FreezeJournal::recover(&journal_path).unwrap();
    assert!(
        report.thawed >= 1,
        "recovery must resume a manually frozen process, got {report:?}"
    );
    assert!(
        !is_stopped(target_pid),
        "a manually frozen process must not be stranded"
    );
    let _ = std::fs::remove_dir_all(journal_path.parent().unwrap());
}

/// Owning a well-known bus name must not exempt an application from management.
///
/// Every media-capable browser registers `org.mpris.MediaPlayer2.*` and Telegram
/// claims `org.telegram.desktop`. Treating that as a reason to spare them put the
/// two heaviest applications on the system permanently beyond the daemon's reach.
#[tokio::test]
async fn an_app_owning_a_bus_name_is_still_managed() {
    let journal_path = temp_journal("bus-name-app");
    let journal = app_powerd_core::state::FreezeJournal::load(journal_path.clone()).unwrap();
    let config = reactive_config("");
    let (engine, tx) = Engine::with_journal(
        config,
        std::path::PathBuf::from("/tmp/test-config.yaml"),
        journal,
    )
    .expect("engine init");
    let engine_handle = tokio::spawn(engine.run());

    let target = TestProc::spawn();
    let target_pid = target.pid();
    let other = TestProc::spawn();

    // The application itself owns a well-known name, exactly like a browser
    // exposing MPRIS.
    tx.send(EngineEvent::ProtectionRefreshed(
        [(
            target_pid,
            "org.mpris.MediaPlayer2.chromium.instance1".to_string(),
        )]
        .into_iter()
        .collect(),
    ))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    tx.send(EngineEvent::FocusChanged(make_window(
        1, target_pid, "Chromium",
    )))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    tx.send(EngineEvent::FocusChanged(make_window(
        2,
        other.pid(),
        "Other",
    )))
    .await
    .unwrap();

    wait_until(
        || is_stopped(target_pid),
        "an app owning a bus name to still be frozen",
    )
    .await;

    let apps = query_app_list(&tx).await;
    let info = apps
        .iter()
        .find(|a| a.app_id == "Chromium")
        .expect("app is tracked");
    assert!(
        info.protected.is_none(),
        "owning a bus name must not mark the application protected, got {:?}",
        info.protected
    );

    tx.send(EngineEvent::Shutdown).await.unwrap();
    engine_handle.await.unwrap();
    let _ = std::fs::remove_dir_all(journal_path.parent().unwrap());
}

/// With management disabled, losing focus must still be recorded.
///
/// Withholding the `Background` state left every application `Active` forever,
/// so each later focus change reported all of them as newly backgrounded again.
/// On AC — where management is off all day — that produced thousands of
/// identical log lines: 1619 of them against 733 real focus changes on the
/// reference machine.
#[tokio::test]
async fn backgrounding_is_recorded_even_when_management_is_off() {
    let config: Config = serde_yaml_ng::from_str(
        r#"
version: 1
defaults:
  enabled: true
  reconcile_interval: "50ms"
  protection:
    dbus_check: false
  mode:
    ac: disable
    battery: enable
  timing:
    suspend_delay: "50ms"
    resume_grace: "10ms"
    min_suspend: "10ms"
"#,
    )
    .unwrap();
    let (engine, tx) = Engine::new(config, std::path::PathBuf::from("/tmp/test-config.yaml"))
        .expect("engine init");
    let engine_handle = tokio::spawn(engine.run());

    // On AC with `ac: disable`, nothing is managed.
    tx.send(EngineEvent::PowerSourceChanged(PowerSource::Ac))
        .await
        .unwrap();

    let first = TestProc::spawn();
    let second = TestProc::spawn();

    tx.send(EngineEvent::FocusChanged(make_window(
        1,
        first.pid(),
        "First",
    )))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    tx.send(EngineEvent::FocusChanged(make_window(
        2,
        second.pid(),
        "Second",
    )))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    let apps = query_app_list(&tx).await;
    let first_info = apps.iter().find(|a| a.app_id == "First").unwrap();
    assert_eq!(
        first_info.state,
        AppState::Background,
        "an unfocused app must be recorded as Background even with management off"
    );
    // And it must not be suspended: management is off.
    assert!(!is_stopped(first.pid()));

    let second_info = apps.iter().find(|a| a.app_id == "Second").unwrap();
    assert_eq!(second_info.state, AppState::Active);

    tx.send(EngineEvent::Shutdown).await.unwrap();
    engine_handle.await.unwrap();
}

/// Switching to AC with `ac: disable` must resume everything.
///
/// Regression cover for the interaction the report asked us to verify, and for
/// the failure mode where a thaw error left the entry marked `Frozen`.
#[tokio::test]
async fn switching_to_ac_resumes_frozen_apps() {
    let journal_path = temp_journal("ac");
    let journal = app_powerd_core::state::FreezeJournal::load(journal_path.clone()).unwrap();
    let config: Config = serde_yaml_ng::from_str(
        r#"
version: 1
defaults:
  enabled: true
  reconcile_interval: "50ms"
  protection:
    dbus_check: false
  mode:
    ac: disable
    battery: enable
  timing:
    suspend_delay: "50ms"
    resume_grace: "10ms"
    min_suspend: "10ms"
  guards:
    audio_active: ignore
    mic_active: ignore
    camera_active: ignore
    fullscreen: ignore
"#,
    )
    .unwrap();
    let (engine, tx) = Engine::with_journal(
        config,
        std::path::PathBuf::from("/tmp/test-config.yaml"),
        journal,
    )
    .expect("engine init");
    let engine_handle = tokio::spawn(engine.run());

    tx.send(EngineEvent::PowerSourceChanged(PowerSource::Battery))
        .await
        .unwrap();

    let target = TestProc::spawn();
    let target_pid = target.pid();
    let other = TestProc::spawn();

    tx.send(EngineEvent::FocusChanged(make_window(
        1, target_pid, "Target",
    )))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    tx.send(EngineEvent::FocusChanged(make_window(
        2,
        other.pid(),
        "Other",
    )))
    .await
    .unwrap();
    wait_until(|| is_stopped(target_pid), "target to be frozen on battery").await;

    tx.send(EngineEvent::PowerSourceChanged(PowerSource::Ac))
        .await
        .unwrap();
    wait_until(|| !is_stopped(target_pid), "target to resume on AC").await;

    let apps = query_app_list(&tx).await;
    let target_info = apps.iter().find(|a| a.app_id == "Target").unwrap();
    assert_ne!(
        target_info.state,
        AppState::Frozen,
        "state must reflect that the app was resumed"
    );

    tx.send(EngineEvent::Shutdown).await.unwrap();
    engine_handle.await.unwrap();
    let _ = std::fs::remove_dir_all(journal_path.parent().unwrap());
}

/// The suspension mechanism actually in use must be visible without reading logs.
#[tokio::test]
async fn status_reports_the_suspension_mode() {
    let config = reactive_config("");
    let (engine, tx) = Engine::new(config, std::path::PathBuf::from("/tmp/test-config.yaml"))
        .expect("engine init");
    let engine_handle = tokio::spawn(engine.run());

    let status = query_status(&tx).await;
    assert!(
        ["SignalOnly", "DirectWrite", "SystemdTransient"].contains(&status.cgroup_mode.as_str()),
        "unexpected mode: {}",
        status.cgroup_mode
    );

    tx.send(EngineEvent::Shutdown).await.unwrap();
    engine_handle.await.unwrap();
}

/// Thawing must reach processes recorded in the journal even after the
/// application's registry entry is gone — the case of helper processes that
/// outlive the window owner.
#[tokio::test]
async fn thaw_all_releases_processes_of_retired_apps() {
    let journal_path = temp_journal("thawall");
    let journal = app_powerd_core::state::FreezeJournal::load(journal_path.clone()).unwrap();
    let config = reactive_config("");
    let (engine, tx) = Engine::with_journal(
        config,
        std::path::PathBuf::from("/tmp/test-config.yaml"),
        journal,
    )
    .expect("engine init");
    let engine_handle = tokio::spawn(engine.run());

    let target = TestProc::spawn();
    let target_pid = target.pid();
    let other = TestProc::spawn();

    tx.send(EngineEvent::FocusChanged(make_window(
        1, target_pid, "Target",
    )))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    tx.send(EngineEvent::FocusChanged(make_window(
        2,
        other.pid(),
        "Other",
    )))
    .await
    .unwrap();
    wait_until(|| is_stopped(target_pid), "target to be frozen").await;

    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(EngineEvent::IpcRequest {
        request: IpcRequest::ThawAll,
        reply: reply_tx,
    })
    .await
    .unwrap();
    assert!(matches!(reply_rx.await.unwrap(), IpcResponse::Ok { .. }));

    assert!(!is_stopped(target_pid), "thaw-all must resume everything");

    tx.send(EngineEvent::Shutdown).await.unwrap();
    engine_handle.await.unwrap();
    let _ = std::fs::remove_dir_all(journal_path.parent().unwrap());
}
