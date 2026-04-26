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
    let config: Config = serde_yaml_ng::from_str(&content)?;
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

    // Check for duplicate rule IDs
    let mut seen_ids = std::collections::HashSet::new();
    for rule in &config.rules {
        if !seen_ids.insert(&rule.id) {
            return Err(ConfigError::Validation {
                message: format!("duplicate rule id: '{}'", rule.id),
            });
        }
    }

    // Validate numeric ranges in profiles
    for (name, profile) in &config.profiles {
        validate_throttle_params(
            &format!("profile '{name}'"),
            profile.nice,
            profile.cpu_weight,
            profile.cpu_quota.as_deref(),
        )?;
    }

    // Validate numeric ranges in rule policies
    for rule in &config.rules {
        validate_throttle_params(
            &format!("rule '{}'", rule.id),
            rule.policy.nice,
            rule.policy.cpu_weight,
            rule.policy.cpu_quota.as_deref(),
        )?;
    }

    Ok(())
}

fn validate_throttle_params(
    ctx: &str,
    nice: Option<i32>,
    cpu_weight: Option<u32>,
    cpu_quota: Option<&str>,
) -> Result<(), ConfigError> {
    if let Some(nice) = nice {
        if !(-20..=19).contains(&nice) {
            return Err(ConfigError::Validation {
                message: format!("{ctx}: nice must be in -20..19, got {nice}"),
            });
        }
    }
    if let Some(weight) = cpu_weight {
        if !(1..=10000).contains(&weight) {
            return Err(ConfigError::Validation {
                message: format!("{ctx}: cpu_weight must be in 1..10000, got {weight}"),
            });
        }
    }
    if let Some(quota) = cpu_quota {
        let numeric = quota.strip_suffix('%').unwrap_or(quota);
        let value = numeric
            .parse::<u32>()
            .map_err(|_| ConfigError::Validation {
                message: format!(
                    "{ctx}: cpu_quota must be a number with optional '%' suffix, got '{quota}'"
                ),
            })?;
        if !(1..=100).contains(&value) {
            return Err(ConfigError::Validation {
                message: format!("{ctx}: cpu_quota must be in 1..=100, got {value}"),
            });
        }
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
pub async fn watch_config(path: &Path) -> Result<tokio::sync::mpsc::Receiver<()>, ConfigError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, MatchCriteria, PolicyConfig, Rule};

    #[test]
    fn validate_rejects_wrong_version() {
        let config = Config {
            version: 2,
            ..Config::default()
        };
        assert!(validate(&config).is_err());
    }

    #[test]
    fn validate_rejects_empty_rule_id() {
        let mut config = Config::default();
        config.rules.push(Rule {
            id: String::new(),
            match_criteria: MatchCriteria {
                executable: vec!["test".into()],
                cmdline_regex: None,
                wm_class: vec![],
                app_id: vec![],
                desktop_file: vec![],
                window_title_regex: None,
            },
            policy: PolicyConfig {
                use_profile: None,
                action: None,
                suspend_delay: None,
                nice: None,
                cpu_weight: None,
                cpu_quota: None,
                maintenance_resume: None,
                guards: None,
            },
        });
        assert!(validate(&config).is_err());
    }

    #[test]
    fn validate_rejects_unknown_profile() {
        let mut config = Config::default();
        config.rules.push(Rule {
            id: "test".into(),
            match_criteria: MatchCriteria {
                executable: vec!["test".into()],
                cmdline_regex: None,
                wm_class: vec![],
                app_id: vec![],
                desktop_file: vec![],
                window_title_regex: None,
            },
            policy: PolicyConfig {
                use_profile: Some("nonexistent".into()),
                action: None,
                suspend_delay: None,
                nice: None,
                cpu_weight: None,
                cpu_quota: None,
                maintenance_resume: None,
                guards: None,
            },
        });
        assert!(validate(&config).is_err());
    }

    #[test]
    fn validate_accepts_valid_config() {
        let config = Config::default();
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn load_config_or_default_returns_defaults() {
        let config = load_config_or_default(Path::new("/nonexistent/path/config.yaml"));
        assert_eq!(config.version, 1);
        assert!(config.defaults.enabled);
    }

    #[test]
    fn nice_boundary_values() {
        // -20 is the valid minimum
        assert!(validate_throttle_params("ctx", Some(-20), None, None).is_ok());
        // -21 is below minimum
        assert!(validate_throttle_params("ctx", Some(-21), None, None).is_err());
        // 19 is the valid maximum
        assert!(validate_throttle_params("ctx", Some(19), None, None).is_ok());
        // 20 is above maximum
        assert!(validate_throttle_params("ctx", Some(20), None, None).is_err());
    }

    #[test]
    fn cpu_weight_boundary_values() {
        // 0 is below minimum
        assert!(validate_throttle_params("ctx", None, Some(0), None).is_err());
        // 1 is the valid minimum
        assert!(validate_throttle_params("ctx", None, Some(1), None).is_ok());
        // 10000 is the valid maximum
        assert!(validate_throttle_params("ctx", None, Some(10000), None).is_ok());
        // 10001 is above maximum
        assert!(validate_throttle_params("ctx", None, Some(10001), None).is_err());
    }

    #[test]
    fn cpu_quota_boundary_values() {
        // "0%" is below minimum (1%)
        assert!(validate_throttle_params("ctx", None, None, Some("0%")).is_err());
        // "1%" is the valid minimum
        assert!(validate_throttle_params("ctx", None, None, Some("1%")).is_ok());
        // "100%" is the valid maximum
        assert!(validate_throttle_params("ctx", None, None, Some("100%")).is_ok());
        // "101%" is above maximum
        assert!(validate_throttle_params("ctx", None, None, Some("101%")).is_err());
    }
}
