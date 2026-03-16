use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use app_powerd_core::config::{self, load_config, load_config_or_default};
use app_powerd_core::desktop;
use app_powerd_core::engine::{Engine, EngineEvent};
use app_powerd_core::ipc::protocol::{self, socket_path, IpcRequest, IpcResponse};
use app_powerd_core::ipc::{send_request, IpcServer};
use app_powerd_core::system::power;

#[derive(Parser)]
#[command(
    name = "app-powerd",
    version,
    about = "User-level daemon for battery-saving app management"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the daemon.
    Run {
        /// Path to config file.
        #[arg(short, long, default_value_os_t = config::config_path())]
        config: PathBuf,
    },
    /// Show daemon status.
    Status,
    /// List tracked applications.
    List,
    /// Show daemon metrics.
    Stats,
    /// Force freeze a process.
    Freeze {
        /// Process ID to freeze.
        pid: u32,
    },
    /// Force thaw a process.
    Thaw {
        /// Process ID to thaw.
        pid: u32,
    },
    /// Reload configuration.
    ReloadConfig,
    /// Shutdown the daemon.
    Shutdown,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Run { config } => run_daemon(config).await,
        cmd => run_client(cmd).await,
    }
}

async fn run_daemon(config_path: PathBuf) -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    info!(version = env!("CARGO_PKG_VERSION"), "app-powerd starting");

    // Acquire exclusive lock to prevent multiple instances
    let lock_path = protocol::socket_path().with_extension("lock");
    let _lock_file = {
        let f = std::fs::File::create(&lock_path).context("failed to create lock file")?;
        use nix::fcntl::{Flock, FlockArg};
        match Flock::lock(f, FlockArg::LockExclusiveNonblock) {
            Ok(locked) => locked,
            Err(_) => bail!(
                "another app-powerd instance is already running (lock: {})",
                lock_path.display()
            ),
        }
    };

    // Load config
    let config = load_config_or_default(&config_path);

    // Create engine
    let (engine, event_tx) =
        Engine::new(config, config_path.clone()).context("failed to initialize engine")?;

    // Start IPC server
    let socket_path = protocol::socket_path();
    let ipc_server =
        IpcServer::bind(&socket_path, event_tx.clone()).context("failed to start IPC server")?;
    tokio::spawn(ipc_server.run());

    // Start focus backend
    let backend = desktop::detect_backend().context("failed to detect display server")?;

    let focus_tx = event_tx.clone();
    tokio::spawn(async move {
        let (ftx, mut frx) = tokio::sync::mpsc::channel(64);

        tokio::spawn(async move {
            if let Err(e) = backend.run(ftx).await {
                error!(error = %e, "focus backend error");
            }
        });

        while let Some(event) = frx.recv().await {
            if focus_tx.send(EngineEvent::from(event)).await.is_err() {
                break;
            }
        }

        warn!("Focus event channel closed, sending shutdown");
        let _ = focus_tx.send(EngineEvent::Shutdown).await;
    });

    // Start power source monitoring
    let (power_tx, mut power_rx) = tokio::sync::mpsc::channel(4);
    power::watch_power_source(Duration::from_secs(30), power_tx);

    let power_event_tx = event_tx.clone();
    tokio::spawn(async move {
        while let Some(source) = power_rx.recv().await {
            if let Err(e) = power_event_tx
                .send(EngineEvent::PowerSourceChanged(source))
                .await
            {
                warn!("Failed to send power event: {}", e);
            }
        }
    });

    // Start config file watcher
    let config_watch_tx = event_tx.clone();
    let config_watch_path = config_path.clone();
    tokio::spawn(async move {
        match config::watch_config(&config_watch_path).await {
            Ok(mut rx) => {
                while rx.recv().await.is_some() {
                    info!("config file changed, reloading");
                    reload_config_from_file(&config_watch_path, &config_watch_tx).await;
                }
            }
            Err(e) => {
                info!(error = %e, "config watcher not available");
            }
        }
    });

    // Handle signals
    let signal_tx = event_tx.clone();
    tokio::spawn(async move {
        let mut sighup = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        {
            Ok(s) => s,
            Err(e) => {
                error!(error = %e, "failed to register SIGHUP handler");
                return;
            }
        };

        loop {
            sighup.recv().await;
            info!("SIGHUP received, reloading config");
            reload_config_from_file(&config_path, &signal_tx).await;
        }
    });

    // Handle SIGTERM/SIGINT for graceful shutdown
    let shutdown_tx = event_tx.clone();
    tokio::spawn(async move {
        let mut sigterm =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    error!(error = %e, "failed to register SIGTERM handler");
                    return;
                }
            };

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("SIGINT received, shutting down");
            }
            _ = sigterm.recv() => {
                info!("SIGTERM received, shutting down");
            }
        }
        let _ = shutdown_tx.send(EngineEvent::Shutdown).await;
    });

    // Run the engine (blocks until shutdown)
    engine.run().await;

    // Cleanup socket
    let _ = std::fs::remove_file(&socket_path);

    info!("app-powerd stopped");
    Ok(())
}

async fn run_client(command: Commands) -> Result<()> {
    let path = socket_path();

    let request = match command {
        Commands::Status => IpcRequest::Status,
        Commands::List => IpcRequest::List,
        Commands::Stats => IpcRequest::Stats,
        Commands::Freeze { pid } => IpcRequest::Freeze { pid },
        Commands::Thaw { pid } => IpcRequest::Thaw { pid },
        Commands::ReloadConfig => IpcRequest::ReloadConfig,
        Commands::Shutdown => IpcRequest::Shutdown,
        Commands::Run { .. } => unreachable!(),
    };

    let response = send_request(&path, request)
        .await
        .context("failed to connect to daemon (is app-powerd running?)")?;

    match response {
        IpcResponse::Ok { message } => {
            println!("{message}");
        }
        IpcResponse::Error { message } => {
            eprintln!("error: {message}");
            std::process::exit(1);
        }
        IpcResponse::AppList { apps } => {
            if apps.is_empty() {
                println!("No tracked applications.");
            } else {
                println!("{:<20} {:<12} {:<8} TITLE", "APP", "STATE", "PIDs");
                println!("{}", "-".repeat(72));
                for app in apps {
                    let pids = app
                        .pids
                        .iter()
                        .map(|p| p.to_string())
                        .collect::<Vec<_>>()
                        .join(",");
                    let title = app.window_title.as_deref().unwrap_or("-");
                    println!(
                        "{:<20} {:<12} {:<8} {}",
                        app.app_id,
                        app.state,
                        pids,
                        truncate(title, 30),
                    );
                }
            }
        }
        IpcResponse::Status {
            enabled,
            power_source,
            tracked_apps,
            uptime_secs,
        } => {
            println!("app-powerd status:");
            println!("  enabled:      {enabled}");
            println!("  power source: {power_source:?}");
            println!("  tracked apps: {tracked_apps}");
            println!("  uptime:       {uptime_secs}s");
        }
        IpcResponse::Stats { metrics } => {
            println!("app-powerd metrics:");
            println!("  apps_frozen_total:     {}", metrics.apps_frozen_total);
            println!("  apps_thawed_total:     {}", metrics.apps_thawed_total);
            println!("  apps_throttled_total:  {}", metrics.apps_throttled_total);
            println!(
                "  apps_unthrottled_total:{}",
                metrics.apps_unthrottled_total
            );
            println!("  focus_changes_total:   {}", metrics.focus_changes_total);
            println!("  guard_blocks_total:    {}", metrics.guard_blocks_total);
            println!("  config_reloads_total:  {}", metrics.config_reloads_total);
            println!("  time_in_frozen_ms:     {}", metrics.time_in_frozen_ms);
            println!("  time_in_throttled_ms:  {}", metrics.time_in_throttled_ms);
        }
        _ => {
            eprintln!("unexpected response from daemon");
            std::process::exit(1);
        }
    }

    Ok(())
}

/// Load config from file and send it to the engine. Used by both inotify watcher and SIGHUP handler.
async fn reload_config_from_file(path: &Path, tx: &tokio::sync::mpsc::Sender<EngineEvent>) {
    match load_config(path) {
        Ok(new_config) => {
            let _ = tx.send(EngineEvent::ConfigReloaded(new_config)).await;
        }
        Err(e) => {
            error!(error = %e, "config reload failed");
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let end = s
            .char_indices()
            .nth(max - 3)
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        format!("{}...", &s[..end])
    }
}
