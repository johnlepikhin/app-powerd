use std::path::{Path, PathBuf};

use inotify::{Inotify, WatchMask};
use tracing::{info, warn};

use super::Config;
use crate::error::ConfigError;

/// Default config path: `$XDG_CONFIG_HOME/app-powerd/config.yaml`.
pub fn config_path() -> PathBuf {
    let config_dir = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home).join(".config")
        });
    config_dir.join("app-powerd").join("config.yaml")
}

/// Load and validate config from a path.
pub fn load_config(path: &Path) -> Result<Config, ConfigError> {
    if !path.exists() {
        return Err(ConfigError::NotFound {
            path: path.to_path_buf(),
        });
    }

    let content = std::fs::read_to_string(path)?;
    let config: Config = serde_yaml::from_str(&content)?;
    validate(&config)?;

    info!(path = %path.display(), "config loaded");
    Ok(config)
}

/// Load config or return defaults if file doesn't exist.
pub fn load_config_or_default(path: &Path) -> Config {
    match load_config(path) {
        Ok(config) => config,
        Err(ConfigError::NotFound { path }) => {
            warn!(path = %path.display(), "config not found, using defaults");
            Config::default()
        }
        Err(e) => {
            warn!(error = %e, "failed to load config, using defaults");
            Config::default()
        }
    }
}

fn validate(config: &Config) -> Result<(), ConfigError> {
    if config.version != 1 {
        return Err(ConfigError::Validation {
            message: format!("unsupported config version: {}", config.version),
        });
    }

    for rule in &config.rules {
        if rule.id.is_empty() {
            return Err(ConfigError::Validation {
                message: "rule id cannot be empty".into(),
            });
        }

        if let Some(ref profile_name) = rule.policy.use_profile {
            if !config.profiles.contains_key(profile_name) {
                return Err(ConfigError::UnknownProfile {
                    rule_id: rule.id.clone(),
                    profile: profile_name.clone(),
                });
            }
        }

        // Regex validation is deferred to RulesEngine::new() which compiles
        // and validates all regexes during rule compilation.
    }

    Ok(())
}

impl Default for Config {
    fn default() -> Self {
        Config {
            version: 1,
            defaults: super::Defaults::default(),
            profiles: Default::default(),
            rules: Vec::new(),
        }
    }
}

/// Watch config file for changes using inotify. Returns an async stream.
pub async fn watch_config(
    path: &Path,
) -> Result<tokio::sync::mpsc::Receiver<()>, ConfigError> {
    let (tx, rx) = tokio::sync::mpsc::channel(4);

    let dir = path
        .parent()
        .ok_or_else(|| ConfigError::Validation {
            message: "config path has no parent directory".into(),
        })?
        .to_path_buf();

    let filename = path
        .file_name()
        .ok_or_else(|| ConfigError::Validation {
            message: "config path has no filename".into(),
        })?
        .to_os_string();

    let inotify = Inotify::init()?;
    inotify.watches().add(
        &dir,
        WatchMask::MODIFY | WatchMask::CREATE | WatchMask::MOVED_TO,
    )?;

    tokio::spawn(async move {
        let mut stream = match inotify.into_event_stream([0u8; 4096]) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "failed to create inotify event stream");
                return;
            }
        };

        use tokio_stream::StreamExt;
        while let Some(Ok(event)) = stream.next().await {
            if event.name.as_ref() == Some(&std::ffi::OsStr::new(&filename).into()) {
                let _ = tx.send(()).await;
            }
        }
    });

    Ok(rx)
}
