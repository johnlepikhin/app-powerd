use regex::Regex;
use tracing::{debug, warn};

use super::{Config, ResolvedPolicy, Rule};
use crate::desktop::window::WindowInfo;
use crate::error::ConfigError;

/// Pre-compiled rule for efficient matching.
#[derive(Debug)]
pub(crate) struct CompiledRule {
    pub id: String,
    pub executables: Vec<String>,
    pub wm_classes: Vec<String>,
    pub app_ids: Vec<String>,
    pub desktop_files: Vec<String>,
    pub cmdline_regex: Option<Regex>,
    pub window_title_regex: Option<Regex>,
    pub policy_index: usize,
}

/// Match context: info about the window/process being matched.
#[derive(Debug, Default)]
pub struct MatchContext {
    pub executable: String,
    pub cmdline: String,
    pub wm_class: String,
    pub app_id: String,
    pub desktop_file: String,
    pub window_title: String,
}

impl From<&WindowInfo> for MatchContext {
    fn from(w: &WindowInfo) -> Self {
        Self {
            executable: w.executable.as_deref().unwrap_or_default().to_owned(),
            cmdline: w.cmdline.as_deref().unwrap_or_default().to_owned(),
            wm_class: w.wm_class.as_deref().unwrap_or_default().to_owned(),
            app_id: w.app_id.as_deref().unwrap_or_default().to_owned(),
            desktop_file: String::new(),
            window_title: w.title.as_deref().unwrap_or_default().to_owned(),
        }
    }
}

/// Rules engine: compiles rules, matches windows.
pub struct RulesEngine {
    compiled: Vec<CompiledRule>,
    config: Config,
}

impl RulesEngine {
    /// Compile all rules from config.
    pub fn new(config: Config) -> Result<Self, ConfigError> {
        let mut compiled = Vec::with_capacity(config.rules.len());

        for (i, rule) in config.rules.iter().enumerate() {
            compiled.push(compile_rule(rule, i)?);
        }

        Ok(Self { compiled, config })
    }

    /// Find the first matching rule (first-match-wins). Returns resolved policy.
    pub fn match_window(&self, ctx: &MatchContext) -> ResolvedPolicy {
        for rule in &self.compiled {
            if matches_rule(rule, ctx) {
                debug!(rule_id = %rule.id, "rule matched");
                return self
                    .config
                    .resolve_policy(&self.config.rules[rule.policy_index].policy);
            }
        }

        debug!("no rule matched, using default policy");
        self.config.default_policy()
    }

    pub fn config(&self) -> &Config {
        &self.config
    }
}

fn compile_optional_regex(
    pattern: &Option<String>,
    rule_id: &str,
) -> Result<Option<Regex>, ConfigError> {
    pattern
        .as_ref()
        .map(|r| {
            regex::RegexBuilder::new(r)
                .size_limit(64 * 1024)
                .build()
                .map_err(|e| ConfigError::InvalidRegex {
                    rule_id: rule_id.to_owned(),
                    source: e,
                })
        })
        .transpose()
}

fn compile_rule(rule: &Rule, index: usize) -> Result<CompiledRule, ConfigError> {
    let mc = &rule.match_criteria;
    if mc.executable.is_empty()
        && mc.wm_class.is_empty()
        && mc.app_id.is_empty()
        && mc.desktop_file.is_empty()
        && mc.cmdline_regex.is_none()
        && mc.window_title_regex.is_none()
    {
        warn!(rule_id = %rule.id, "rule has empty match criteria — will match all windows (catch-all)");
    }

    let cmdline_regex = compile_optional_regex(&mc.cmdline_regex, &rule.id)?;
    let window_title_regex = compile_optional_regex(&mc.window_title_regex, &rule.id)?;

    Ok(CompiledRule {
        id: rule.id.clone(),
        executables: rule.match_criteria.executable.clone(),
        wm_classes: rule.match_criteria.wm_class.clone(),
        app_ids: rule.match_criteria.app_id.clone(),
        desktop_files: rule.match_criteria.desktop_file.clone(),
        cmdline_regex,
        window_title_regex,
        policy_index: index,
    })
}

/// AND across fields, OR within field values.
fn matches_rule(rule: &CompiledRule, ctx: &MatchContext) -> bool {
    if !rule.executables.is_empty() && !rule.executables.iter().any(|e| e == &ctx.executable) {
        return false;
    }

    if !rule.wm_classes.is_empty() && !rule.wm_classes.iter().any(|c| c == &ctx.wm_class) {
        return false;
    }

    if !rule.app_ids.is_empty() && !rule.app_ids.iter().any(|a| a == &ctx.app_id) {
        return false;
    }

    if !rule.desktop_files.is_empty() && !rule.desktop_files.iter().any(|d| d == &ctx.desktop_file)
    {
        return false;
    }

    if let Some(ref re) = rule.cmdline_regex {
        if !re.is_match(&ctx.cmdline) {
            return false;
        }
    }

    if let Some(ref re) = rule.window_title_regex {
        if !re.is_match(&ctx.window_title) {
            return false;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn make_config(yaml: &str) -> Config {
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    #[test]
    fn first_match_wins() {
        let config = make_config(
            r#"
version: 1
profiles:
  freeze:
    action: freeze
  throttle:
    action: throttle
rules:
  - id: chrome-throttle
    match:
      executable: [chrome]
    policy:
      use_profile: throttle
  - id: catch-all
    match: {}
    policy:
      use_profile: freeze
"#,
        );
        let engine = RulesEngine::new(config).unwrap();

        let ctx = MatchContext {
            executable: "chrome".into(),
            ..Default::default()
        };
        let policy = engine.match_window(&ctx);
        assert_eq!(policy.action, crate::config::Action::Throttle);
    }

    #[test]
    fn regex_match() {
        let config = make_config(
            r#"
version: 1
rules:
  - id: electron
    match:
      cmdline_regex: "--type=renderer"
    policy:
      action: throttle
"#,
        );
        let engine = RulesEngine::new(config).unwrap();

        let ctx = MatchContext {
            cmdline: "/usr/bin/chrome --type=renderer --field-trial".into(),
            ..Default::default()
        };
        let policy = engine.match_window(&ctx);
        assert_eq!(policy.action, crate::config::Action::Throttle);
    }

    #[test]
    fn and_across_fields() {
        let config = make_config(
            r#"
version: 1
rules:
  - id: specific
    match:
      executable: [firefox]
      wm_class: [Navigator]
    policy:
      action: throttle
"#,
        );
        let engine = RulesEngine::new(config).unwrap();

        // Only executable matches — should NOT match (AND logic)
        let ctx = MatchContext {
            executable: "firefox".into(),
            wm_class: "Other".into(),
            ..Default::default()
        };
        let policy = engine.match_window(&ctx);
        // Falls through to default which is Freeze, NOT Throttle
        assert_eq!(policy.action, crate::config::Action::Freeze);

        // Both fields match — should match the rule (Throttle)
        let ctx2 = MatchContext {
            executable: "firefox".into(),
            wm_class: "Navigator".into(),
            ..Default::default()
        };
        let policy2 = engine.match_window(&ctx2);
        assert_eq!(policy2.action, crate::config::Action::Throttle);
    }
}
