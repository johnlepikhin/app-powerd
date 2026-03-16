pub(crate) mod loader;
pub(crate) mod matching;

pub use loader::{config_path, load_config, load_config_or_default, watch_config};
pub use matching::RulesEngine;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

/// Parameters for throttling an application.
#[derive(Debug, Clone)]
pub struct ThrottleParams {
    pub nice: Option<i32>,
    pub cpu_weight: Option<u32>,
    pub cpu_quota: Option<String>,
}

/// Top-level configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub profiles: HashMap<String, Profile>,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

fn default_version() -> u32 {
    1
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Defaults {
    pub enabled: bool,
    pub mode: ModeConfig,
    pub timing: TimingConfig,
    pub maintenance_resume: MaintenanceResumeConfig,
    pub guards: GuardsConfig,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: ModeConfig::default(),
            timing: TimingConfig::default(),
            maintenance_resume: MaintenanceResumeConfig::default(),
            guards: GuardsConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModeConfig {
    pub ac: PowerMode,
    pub battery: PowerMode,
}

impl Default for ModeConfig {
    fn default() -> Self {
        Self {
            ac: PowerMode::Disable,
            battery: PowerMode::Enable,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum PowerMode {
    Enable,
    Disable,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TimingConfig {
    #[serde(with = "humantime_serde")]
    pub suspend_delay: Duration,
    #[serde(with = "humantime_serde")]
    pub resume_grace: Duration,
    #[serde(with = "humantime_serde")]
    pub min_suspend: Duration,
}

impl Default for TimingConfig {
    fn default() -> Self {
        Self {
            suspend_delay: Duration::from_secs(30),
            resume_grace: Duration::from_secs(3),
            min_suspend: Duration::from_secs(5),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MaintenanceResumeConfig {
    pub enabled: bool,
    #[serde(with = "humantime_serde")]
    pub interval: Duration,
    #[serde(with = "humantime_serde")]
    pub duration: Duration,
}

impl Default for MaintenanceResumeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval: Duration::from_secs(30),
            duration: Duration::from_secs(1),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct GuardsConfig {
    pub audio_active: GuardAction,
    pub mic_active: GuardAction,
    pub camera_active: GuardAction,
    pub fullscreen: GuardAction,
    #[serde(with = "humantime_serde::option")]
    pub input_idle: Option<Duration>,
}

impl Default for GuardsConfig {
    fn default() -> Self {
        Self {
            audio_active: GuardAction::Check,
            mic_active: GuardAction::Check,
            camera_active: GuardAction::Check,
            fullscreen: GuardAction::Check,
            input_idle: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum GuardAction {
    Check,
    #[serde(alias = "skip")]
    Ignore,
}

/// Named profile for reuse across rules.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub action: Action,
    #[serde(default, with = "humantime_serde::option")]
    pub suspend_delay: Option<Duration>,
    pub nice: Option<i32>,
    pub cpu_weight: Option<u32>,
    pub cpu_quota: Option<String>,
    pub maintenance_resume: Option<MaintenanceResumeConfig>,
    pub guards: Option<GuardsConfig>,
}

/// Action to apply to background applications.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Action {
    Ignore,
    Throttle,
    Freeze,
}

/// Per-application matching rule.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    #[serde(rename = "match")]
    pub match_criteria: MatchCriteria,
    pub policy: PolicyConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MatchCriteria {
    #[serde(default)]
    pub executable: Vec<String>,
    pub cmdline_regex: Option<String>,
    #[serde(default)]
    pub wm_class: Vec<String>,
    #[serde(default)]
    pub app_id: Vec<String>,
    #[serde(default)]
    pub desktop_file: Vec<String>,
    pub window_title_regex: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyConfig {
    pub use_profile: Option<String>,
    pub action: Option<Action>,
    #[serde(default, with = "humantime_serde::option")]
    pub suspend_delay: Option<Duration>,
    pub nice: Option<i32>,
    pub cpu_weight: Option<u32>,
    pub cpu_quota: Option<String>,
    pub maintenance_resume: Option<MaintenanceResumeConfig>,
    pub guards: Option<GuardsConfig>,
}

/// Fully resolved policy after profile inheritance.
#[derive(Debug, Clone)]
pub struct ResolvedPolicy {
    pub action: Action,
    pub suspend_delay: Duration,
    pub resume_grace: Duration,
    pub min_suspend: Duration,
    pub nice: Option<i32>,
    pub cpu_weight: Option<u32>,
    pub cpu_quota: Option<String>,
    pub maintenance_resume: MaintenanceResumeConfig,
    pub guards: GuardsConfig,
}

impl Default for ResolvedPolicy {
    fn default() -> Self {
        Self {
            action: Action::Freeze,
            suspend_delay: Duration::from_secs(30),
            resume_grace: Duration::from_secs(3),
            min_suspend: Duration::from_secs(5),
            nice: None,
            cpu_weight: None,
            cpu_quota: None,
            maintenance_resume: MaintenanceResumeConfig::default(),
            guards: GuardsConfig::default(),
        }
    }
}

impl ResolvedPolicy {
    /// Extract throttle-specific parameters.
    pub fn throttle_params(&self) -> ThrottleParams {
        ThrottleParams {
            nice: self.nice,
            cpu_weight: self.cpu_weight,
            cpu_quota: self.cpu_quota.clone(),
        }
    }
}

impl Config {
    /// Resolve a policy from rule + profile + defaults.
    pub fn resolve_policy(&self, policy: &PolicyConfig) -> ResolvedPolicy {
        let profile = policy
            .use_profile
            .as_ref()
            .and_then(|name| self.profiles.get(name));

        let action = policy
            .action
            .or_else(|| profile.map(|p| p.action))
            .unwrap_or(Action::Freeze);

        let suspend_delay = policy
            .suspend_delay
            .or_else(|| profile.and_then(|p| p.suspend_delay))
            .unwrap_or(self.defaults.timing.suspend_delay);

        let nice = policy.nice.or_else(|| profile.and_then(|p| p.nice));

        let cpu_weight = policy
            .cpu_weight
            .or_else(|| profile.and_then(|p| p.cpu_weight));

        let cpu_quota = policy
            .cpu_quota
            .clone()
            .or_else(|| profile.and_then(|p| p.cpu_quota.clone()));

        let maintenance_resume = policy
            .maintenance_resume
            .clone()
            .or_else(|| profile.and_then(|p| p.maintenance_resume.clone()))
            .unwrap_or_else(|| self.defaults.maintenance_resume.clone());

        let guards = policy
            .guards
            .clone()
            .or_else(|| profile.and_then(|p| p.guards.clone()))
            .unwrap_or_else(|| self.defaults.guards.clone());

        ResolvedPolicy {
            action,
            suspend_delay,
            resume_grace: self.defaults.timing.resume_grace,
            min_suspend: self.defaults.timing.min_suspend,
            nice,
            cpu_weight,
            cpu_quota,
            maintenance_resume,
            guards,
        }
    }

    /// Resolve the default policy (no rule matched).
    pub fn default_policy(&self) -> ResolvedPolicy {
        ResolvedPolicy {
            suspend_delay: self.defaults.timing.suspend_delay,
            resume_grace: self.defaults.timing.resume_grace,
            min_suspend: self.defaults.timing.min_suspend,
            maintenance_resume: self.defaults.maintenance_resume.clone(),
            guards: self.defaults.guards.clone(),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_config() {
        let yaml = "version: 1\n";
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.version, 1);
        assert!(config.defaults.enabled);
        assert_eq!(
            config.defaults.timing.suspend_delay,
            Duration::from_secs(30)
        );
    }

    #[test]
    fn parse_full_config() {
        let yaml = r#"
version: 1
defaults:
  enabled: true
  mode:
    ac: disable
    battery: enable
  timing:
    suspend_delay: "30s"
    resume_grace: "3s"
    min_suspend: "5s"
  guards:
    audio_active: check
    fullscreen: check
profiles:
  freeze:
    action: freeze
    suspend_delay: "60s"
  throttle:
    action: throttle
    nice: 5
    cpu_weight: 20
    cpu_quota: "40%"
rules:
  - id: chrome
    match:
      executable: [google-chrome, chromium]
    policy:
      use_profile: throttle
  - id: telegram
    match:
      executable: [telegram-desktop]
    policy:
      use_profile: freeze
      suspend_delay: "2m"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.profiles.len(), 2);
        assert_eq!(config.rules.len(), 2);
        assert_eq!(config.rules[0].id, "chrome");
    }

    #[test]
    fn resolve_policy_with_profile_and_override() {
        let yaml = r#"
version: 1
profiles:
  throttle:
    action: throttle
    nice: 5
    cpu_weight: 20
rules:
  - id: test
    match:
      executable: [test]
    policy:
      use_profile: throttle
      suspend_delay: "60s"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let resolved = config.resolve_policy(&config.rules[0].policy);
        assert_eq!(resolved.action, Action::Throttle);
        assert_eq!(resolved.suspend_delay, Duration::from_secs(60));
        assert_eq!(resolved.nice, Some(5));
        assert_eq!(resolved.cpu_weight, Some(20));
    }

    #[test]
    fn resolve_policy_rule_overrides_profile() {
        // Rule-level fields must take precedence over the profile's fields.
        let yaml = r#"
version: 1
profiles:
  base:
    action: throttle
    nice: 10
    cpu_weight: 50
    suspend_delay: "45s"
rules:
  - id: override-test
    match:
      executable: [myapp]
    policy:
      use_profile: base
      action: freeze
      nice: 1
      suspend_delay: "120s"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let resolved = config.resolve_policy(&config.rules[0].policy);
        // Rule overrides action, nice, and suspend_delay; profile supplies cpu_weight.
        assert_eq!(resolved.action, Action::Freeze);
        assert_eq!(resolved.nice, Some(1));
        assert_eq!(resolved.suspend_delay, Duration::from_secs(120));
        assert_eq!(resolved.cpu_weight, Some(50));
    }

    #[test]
    fn resolve_policy_defaults_only() {
        // Empty policy (no rule or profile match) should use config defaults.
        let config: Config = serde_yaml_ng::from_str("version: 1\n").unwrap();
        let policy = PolicyConfig {
            use_profile: None,
            action: None,
            suspend_delay: None,
            nice: None,
            cpu_weight: None,
            cpu_quota: None,
            maintenance_resume: None,
            guards: None,
        };
        let resolved = config.resolve_policy(&policy);
        assert_eq!(resolved.action, Action::Freeze);
        assert_eq!(resolved.suspend_delay, Duration::from_secs(30));
        assert_eq!(resolved.resume_grace, Duration::from_secs(3));
        assert_eq!(resolved.min_suspend, Duration::from_secs(5));
        assert_eq!(resolved.nice, None);
        assert_eq!(resolved.cpu_weight, None);
        assert_eq!(resolved.cpu_quota, None);
    }

    #[test]
    fn default_policy_uses_config_defaults() {
        // When config has custom defaults, default_policy() should reflect them.
        let yaml = r#"
version: 1
defaults:
  timing:
    suspend_delay: "60s"
    resume_grace: "10s"
    min_suspend: "15s"
  maintenance_resume:
    enabled: true
    interval: "5m"
    duration: "2s"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let resolved = config.default_policy();
        assert_eq!(resolved.suspend_delay, Duration::from_secs(60));
        assert_eq!(resolved.resume_grace, Duration::from_secs(10));
        assert_eq!(resolved.min_suspend, Duration::from_secs(15));
        assert!(resolved.maintenance_resume.enabled);
        assert_eq!(
            resolved.maintenance_resume.interval,
            Duration::from_secs(300)
        );
    }
}
