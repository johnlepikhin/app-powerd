use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use app_powerd_core::config::{self, load_config, load_config_or_default};
use app_powerd_core::desktop;
use app_powerd_core::engine::{Engine, EngineEvent};
use app_powerd_core::ipc::protocol::{self, socket_path, IpcRequest, IpcResponse};
use app_powerd_core::ipc::{send_request, IpcServer};
use app_powerd_core::metrics::METRICS;
use app_powerd_core::system::power::{self, PowerSource};

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
    /// Force freeze a process or a tracked application.
    Freeze {
        /// Process ID, or the name of a tracked application.
        target: String,
    },
    /// Force thaw a process or a tracked application.
    Thaw {
        /// Process ID, or the name of a tracked application.
        target: String,
    },
    /// Resume every process app-powerd has suspended.
    ///
    /// Works without a running daemon: if no daemon holds the lock, the
    /// on-disk freeze journal is replayed directly. This is the recovery path
    /// after the daemon was killed, since SIGSTOP cannot be undone by the
    /// stopped process itself.
    ThawAll,
    /// Reload configuration.
    ReloadConfig,
    /// Force the daemon to treat the system as if running on a specific power source.
    /// Use `auto` to clear the override and return to the detected source.
    /// The override is in-memory only and is reset to `auto` on daemon restart.
    Force {
        /// Forced mode: battery, ac, or auto.
        mode: ForcedMode,
    },
    /// Shutdown the daemon.
    Shutdown,
}

#[derive(Copy, Clone, ValueEnum)]
enum ForcedMode {
    Battery,
    Ac,
    Auto,
}

impl From<ForcedMode> for Option<PowerSource> {
    fn from(mode: ForcedMode) -> Self {
        match mode {
            ForcedMode::Battery => Some(PowerSource::Battery),
            ForcedMode::Ac => Some(PowerSource::Ac),
            ForcedMode::Auto => None,
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Run { config } => run_daemon(config).await,
        cmd => run_client(cmd).await,
    }
}

/// Path of the single-instance lock, which also arbitrates emergency recovery.
fn lock_path() -> PathBuf {
    protocol::socket_path().with_extension("lock")
}

/// Take the single-instance lock, or report that a daemon already holds it.
///
/// `Ok(None)` means, and only means, "the lock is held by someone else".
/// Every other `flock` failure (no lock support on the filesystem, `ENOLCK`,
/// …) is an error: reporting it as "busy" would make `thaw-all` defer to a
/// daemon that does not exist and refuse emergency recovery.
fn try_lock_instance() -> Result<Option<nix::fcntl::Flock<std::fs::File>>> {
    use nix::errno::Errno;
    use nix::fcntl::{Flock, FlockArg};
    let path = lock_path();
    let file = std::fs::File::create(&path).context("failed to create lock file")?;
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(locked) => Ok(Some(locked)),
        // EAGAIN is EWOULDBLOCK on Linux; EACCES is the spelling some
        // filesystems use for a contended lock.
        Err((_, Errno::EAGAIN | Errno::EACCES)) => Ok(None),
        Err((_, errno)) => bail!(
            "failed to acquire the lock file {}: {errno}",
            path.display()
        ),
    }
}

async fn run_daemon(config_path: PathBuf) -> Result<()> {
    // Log to stderr, and only colourise when a terminal is actually attached.
    // The daemon is normally started from a session script with its output
    // redirected to a file, where escape sequences are noise that inflates the
    // log and makes it unreadable.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    info!(version = env!("CARGO_PKG_VERSION"), "app-powerd starting");

    // Hold the lock for the whole run, including recovery below: it is what
    // tells a concurrent `thaw-all` that a daemon owns the journal.
    let _lock_file = match try_lock_instance()? {
        Some(lock) => lock,
        None => bail!(
            "another app-powerd instance is already running (lock: {})",
            lock_path().display()
        ),
    };

    // Release anything left suspended by a previous run before doing anything
    // else. SIGSTOP survives the process that sent it, so without this a
    // killed daemon leaves the session frozen with no way back.
    let journal_path = app_powerd_core::state::FreezeJournal::default_path();
    match app_powerd_core::state::FreezeJournal::recover(&journal_path) {
        Ok(report) if !report.is_empty() => {
            METRICS
                .journal_recovered_total
                .fetch_add(report.thawed as u64, Ordering::Relaxed);
            info!(
                thawed = report.thawed,
                stale = report.stale,
                not_owned = report.not_owned,
                "recovered processes suspended by a previous run"
            );
        }
        Ok(_) => {}
        // An unreadable journal may still describe stopped processes, so it is
        // preserved for inspection — but moved aside, because leaving it in
        // place would make every subsequent load fail too and the daemon would
        // never be able to record a freeze again.
        Err(e) => {
            let quarantine = journal_path.with_extension("json.corrupt");
            match std::fs::rename(&journal_path, &quarantine) {
                Ok(()) => warn!(
                    error = %e,
                    saved_to = %quarantine.display(),
                    "freeze journal is unreadable; saved a copy and starting fresh. \
                     Processes it described may still be stopped — check with \
                     `ps -eo pid,stat | awk '$2 ~ /T/'`"
                ),
                Err(rename_err) => warn!(
                    error = %e,
                    rename_error = %rename_err,
                    "freeze journal is unreadable and could not be moved aside"
                ),
            }
        }
    }

    // Load the journal only after recovery, so it starts from a clean slate
    // instead of re-persisting entries that were just released.
    let journal = app_powerd_core::state::FreezeJournal::load(journal_path)
        .context("failed to open freeze journal")?;

    // Load config
    let config = load_config_or_default(&config_path);

    // Create engine
    let (engine, event_tx) = Engine::with_journal(config, config_path.clone(), journal)
        .context("failed to initialize engine")?;

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

/// Resume everything, with or without a running daemon.
///
/// The lock, not the socket, decides which path to take. "Socket unavailable"
/// does not mean "no daemon": it may still be starting, or busy. Replaying and
/// deleting the journal underneath a live daemon would leave it believing
/// applications are frozen and destroy the record that a later crash would need.
async fn run_thaw_all() -> Result<()> {
    let lock = try_lock_instance()?;
    if lock.is_none() {
        let response = send_request(&socket_path(), IpcRequest::ThawAll)
            .await
            .context(
                "a daemon holds the lock but its socket is not answering; \
                 refusing to touch the freeze journal behind its back",
            )?;
        return print_response(response);
    }

    // We hold the lock, so no daemon is running: replay the journal ourselves.
    let path = app_powerd_core::state::FreezeJournal::default_path();
    let report = app_powerd_core::state::FreezeJournal::recover(&path)
        .context("failed to replay the freeze journal")?;

    if report.is_empty() {
        println!("Nothing to resume: no processes are recorded as suspended.");
    } else {
        println!(
            "Resumed {} process(es) ({} already gone, {} not owned by this user).",
            report.thawed, report.stale, report.not_owned
        );
    }
    Ok(())
}

async fn run_client(command: Commands) -> Result<()> {
    if matches!(command, Commands::ThawAll) {
        return run_thaw_all().await;
    }

    let path = socket_path();

    let request = match command {
        Commands::Status => IpcRequest::Status,
        Commands::List => IpcRequest::List,
        Commands::Stats => IpcRequest::Stats,
        Commands::Freeze { target } => IpcRequest::Freeze {
            target: protocol::Target::parse(&target),
        },
        Commands::Thaw { target } => IpcRequest::Thaw {
            target: protocol::Target::parse(&target),
        },
        Commands::ThawAll => unreachable!("handled above"),
        Commands::ReloadConfig => IpcRequest::ReloadConfig,
        Commands::Force { mode } => IpcRequest::SetPowerOverride {
            source: mode.into(),
        },
        Commands::Shutdown => IpcRequest::Shutdown,
        Commands::Run { .. } => unreachable!(),
    };

    let response = send_request(&path, request)
        .await
        .context("failed to connect to daemon (is app-powerd running?)")?;

    print_response(response)
}

fn print_response(response: IpcResponse) -> Result<()> {
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
                println!(
                    "{:<20} {:<11} {:<10} {:<12} {:<18} {:<9} TITLE",
                    "APP", "STATE", "IN STATE", "PROFILE", "RULE", "PIDS"
                );
                println!("{}", "-".repeat(110));
                for app in apps {
                    // A protected application is not "background": it is
                    // structurally exempt, and saying so here is the only way a
                    // user finds out why their rule appears to be ignored.
                    let state = match &app.protected {
                        Some(_) => "PROTECTED".to_string(),
                        None => app.state.to_string(),
                    };
                    let rule = app
                        .protected
                        .clone()
                        .or_else(|| app.rule_id.clone())
                        .unwrap_or_else(|| "-".into());
                    let title = app.window_title.as_deref().unwrap_or("-");
                    // `app_id`, `rule` and `title` originate from window
                    // properties any display-server client can set, so they are
                    // sanitised before they ever reach the terminal. Sanitising
                    // before truncation keeps the truncation limit expressed in
                    // the characters that are actually printed.
                    println!(
                        "{:<20} {:<11} {:<10} {:<12} {:<18} {:<9} {}",
                        truncate(&sanitize_for_display(&app.app_id), 20),
                        state,
                        format_duration(app.state_since_secs),
                        truncate(
                            &sanitize_for_display(app.profile.as_deref().unwrap_or("-")),
                            12
                        ),
                        truncate(&sanitize_for_display(&rule), 18),
                        app.pids.len(),
                        truncate(&sanitize_for_display(title), 30),
                    );
                }
            }
        }
        IpcResponse::Status {
            enabled,
            power_source,
            forced_power_source,
            tracked_apps,
            uptime_secs,
            cgroup_mode,
            cpu_control,
            protected_apps,
            protocol_version,
        } => {
            println!("app-powerd status:");
            println!("  enabled:       {enabled}");
            match forced_power_source {
                Some(forced) => {
                    println!("  power source:  {forced} (forced; detected: {power_source})")
                }
                None => println!("  power source:  {power_source} (auto)"),
            }
            println!("  cgroup mode:   {cgroup_mode}");
            println!(
                "  cpu control:   {}",
                if cpu_control {
                    "available"
                } else {
                    "unavailable (cpu_weight/cpu_quota are NOT applied)"
                }
            );
            println!("  tracked apps:  {tracked_apps}");
            println!("  protected:     {protected_apps}");
            println!("  uptime:        {uptime_secs}s");
            println!("  protocol:      v{protocol_version}");
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
            // These make the quiet mechanisms observable: the log deliberately
            // stays silent when reaping and protection are working.
            println!("  reconcile_ticks_total: {}", metrics.reconcile_ticks_total);
            println!("  pids_reaped_total:     {}", metrics.pids_reaped_total);
            println!(
                "  apps_removed_stale:    {}",
                metrics.apps_removed_stale_total
            );
            println!(
                "  protection_blocks:     {}",
                metrics.protection_blocks_total
            );
            println!(
                "  journal_recovered:     {}",
                metrics.journal_recovered_total
            );
            println!(
                "  journal_stale_dropped: {}",
                metrics.journal_stale_dropped_total
            );
            println!(
                "  warns_suppressed:      {}",
                metrics.warns_suppressed_total
            );
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

/// Render a duration compactly for the `IN STATE` column.
fn format_duration(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m{}s", secs / 60, secs % 60),
        _ => format!("{}h{}m", secs / 3600, (secs % 3600) / 60),
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

/// Neutralise control characters in strings that come from other programs.
///
/// Window titles and `WM_CLASS` are set by arbitrary display-server clients.
/// An ESC there is executed by the terminal on `app-powerd list` — enough to
/// rewrite rows already printed — and a newline breaks the table alignment.
/// Every control character is replaced by U+FFFD, one per character, so the
/// visible width still matches the character count `truncate` measures.
fn sanitize_for_display(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '\u{fffd}' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_control_characters() {
        assert_eq!(sanitize_for_display("plain title"), "plain title");
        assert_eq!(
            sanitize_for_display("evil\x1b[2K\r\ntitle"),
            "evil\u{fffd}[2K\u{fffd}\u{fffd}title"
        );
        assert_eq!(sanitize_for_display("tab\there"), "tab\u{fffd}here");
        // DEL and other C1 controls count too.
        assert_eq!(sanitize_for_display("a\x7fb\u{9b}c"), "a\u{fffd}b\u{fffd}c");
        // Non-control multibyte text is preserved verbatim.
        assert_eq!(sanitize_for_display("окно — 1"), "окно — 1");
    }

    #[test]
    fn sanitize_preserves_character_count_for_truncate() {
        let dirty = "\x1b\x1b\x1b\x1b\x1b";
        let clean = sanitize_for_display(dirty);
        assert_eq!(clean.chars().count(), dirty.chars().count());
        assert_eq!(truncate(&clean, 4), "\u{fffd}...");
    }
}
