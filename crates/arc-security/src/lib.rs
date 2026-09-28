//! Arc's permission and safety layer.
//!
//! Every action Arc takes flows through [`Policy::decide`]. The language model
//! never gets to decide whether something is safe: a tool declares a base
//! [`RiskLevel`], the tool may *raise* it for specific arguments (see
//! [`shell::classify`] for command lines and [`paths::PathGuard`] for file
//! paths), and the user's configuration can tighten or loosen individual tools.
//! Anything at or above `permissions.confirm_at` needs an explicit
//! confirmation, which is issued as a single-use, expiring token
//! ([`confirm::Confirmations`]). Everything is appended to an audit log.

pub mod audit;
pub mod confirm;
pub mod paths;
pub mod redact;
pub mod shell;

use arc_config::{Permissions, ToolPolicy};
use arc_proto::RiskLevel;

/// What the policy decided for one invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Needs explicit user confirmation before running.
    Confirm {
        reason: String,
    },
    Deny {
        reason: String,
    },
}

/// Permission policy compiled from configuration.
#[derive(Debug, Clone)]
pub struct Policy {
    confirm_at: RiskLevel,
    overrides: Vec<(Matcher, ToolPolicy)>,
    disabled: Vec<Matcher>,
}

#[derive(Debug, Clone)]
enum Matcher {
    Exact(String),
    Prefix(String),
}

impl Matcher {
    fn parse(s: &str) -> Self {
        let s = s.trim();
        match s.strip_suffix(".*") {
            Some(p) => Matcher::Prefix(format!("{p}.")),
            None if s == "*" => Matcher::Prefix(String::new()),
            None => Matcher::Exact(s.to_string()),
        }
    }
    fn matches(&self, tool: &str) -> bool {
        match self {
            Matcher::Exact(e) => e == tool,
            Matcher::Prefix(p) => tool.starts_with(p.as_str()),
        }
    }
    fn specificity(&self) -> usize {
        match self {
            Matcher::Exact(e) => 10_000 + e.len(),
            Matcher::Prefix(p) => p.len(),
        }
    }
}

impl Policy {
    pub fn new(perms: &Permissions, disabled_tools: &[String]) -> Self {
        let mut overrides: Vec<(Matcher, ToolPolicy)> =
            perms.tools.iter().map(|(k, v)| (Matcher::parse(k), *v)).collect();
        // Most specific rule wins.
        overrides.sort_by_key(|(m, _)| std::cmp::Reverse(m.specificity()));
        Self {
            confirm_at: perms.confirm_at,
            overrides,
            disabled: disabled_tools.iter().map(|s| Matcher::parse(s)).collect(),
        }
    }

    /// True when the tool is disabled by `[tools] disabled`.
    pub fn is_disabled(&self, tool: &str) -> bool {
        self.disabled.iter().any(|m| m.matches(tool))
    }

    pub fn tool_override(&self, tool: &str) -> Option<ToolPolicy> {
        self.overrides.iter().find(|(m, _)| m.matches(tool)).map(|(_, p)| *p)
    }

    /// Decide what to do with `tool` at effective `risk`.
    ///
    /// Rules, in order:
    /// 1. disabled tools and `deny` overrides are refused;
    /// 2. a `confirm` override always asks;
    /// 3. an `allow` override skips confirmation — except for DANGEROUS
    ///    actions, which always ask (an allow rule cannot silently enable a
    ///    destructive operation);
    /// 4. otherwise ask when `risk >= confirm_at`.
    pub fn decide(&self, tool: &str, risk: RiskLevel) -> Decision {
        if self.is_disabled(tool) {
            return Decision::Deny { reason: format!("{tool} is disabled in the configuration") };
        }
        match self.tool_override(tool) {
            Some(ToolPolicy::Deny) => {
                Decision::Deny { reason: format!("{tool} is denied by permissions.tools") }
            }
            Some(ToolPolicy::Confirm) => {
                Decision::Confirm { reason: format!("permissions.tools requires confirmation for {tool}") }
            }
            Some(ToolPolicy::Allow) if risk < RiskLevel::Dangerous => Decision::Allow,
            _ if risk >= self.confirm_at || risk == RiskLevel::Dangerous => {
                Decision::Confirm { reason: format!("{risk} action") }
            }
            _ => Decision::Allow,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn perms(confirm_at: RiskLevel, tools: &[(&str, ToolPolicy)]) -> Permissions {
        let mut t = BTreeMap::new();
        for (k, v) in tools {
            t.insert(k.to_string(), *v);
        }
        Permissions { confirm_at, tools: t, ..Permissions::default() }
    }

    #[test]
    fn default_thresholds() {
        let p = Policy::new(&perms(RiskLevel::Dangerous, &[]), &[]);
        assert_eq!(p.decide("hyprland.switch_workspace", RiskLevel::Safe), Decision::Allow);
        assert_eq!(p.decide("files.move", RiskLevel::Caution), Decision::Allow);
        assert!(matches!(p.decide("files.delete", RiskLevel::Dangerous), Decision::Confirm { .. }));
    }

    /// The `code` tool drops to Caution once it has screened a task, so that
    /// hands-free coding is possible. That only works if Caution is allowed
    /// under the default threshold. If this ever starts confirming, the code
    /// tool silently goes back to prompting and nothing else fails.
    #[test]
    fn a_screened_caution_action_is_allowed_by_default() {
        let p = Policy::new(&perms(RiskLevel::Dangerous, &[]), &[]);
        assert!(matches!(p.decide("code", RiskLevel::Caution), Decision::Allow));
        // And Dangerous is still refused without an override, so a tool that
        // forgets to screen cannot slip through this path.
        assert!(matches!(p.decide("files.delete", RiskLevel::Dangerous), Decision::Confirm { .. }));
    }

    #[test]
    fn stricter_threshold() {
        let p = Policy::new(&perms(RiskLevel::Caution, &[]), &[]);
        assert_eq!(p.decide("x", RiskLevel::Safe), Decision::Allow);
        assert!(matches!(p.decide("x", RiskLevel::Caution), Decision::Confirm { .. }));
    }

    #[test]
    fn allow_override_never_bypasses_dangerous() {
        let p = Policy::new(&perms(RiskLevel::Dangerous, &[("files.delete", ToolPolicy::Allow)]), &[]);
        assert!(matches!(p.decide("files.delete", RiskLevel::Dangerous), Decision::Confirm { .. }));
        let p = Policy::new(&perms(RiskLevel::Caution, &[("files.move", ToolPolicy::Allow)]), &[]);
        assert_eq!(p.decide("files.move", RiskLevel::Caution), Decision::Allow);
    }

    #[test]
    fn deny_and_confirm_overrides_and_specificity() {
        let p = Policy::new(
            &perms(
                RiskLevel::Dangerous,
                &[
                    ("files.*", ToolPolicy::Confirm),
                    ("files.find", ToolPolicy::Allow),
                    ("shell.run", ToolPolicy::Deny),
                ],
            ),
            &[],
        );
        assert!(matches!(p.decide("files.move", RiskLevel::Caution), Decision::Confirm { .. }));
        assert_eq!(p.decide("files.find", RiskLevel::Safe), Decision::Allow);
        assert!(matches!(p.decide("shell.run", RiskLevel::Safe), Decision::Deny { .. }));
    }

    #[test]
    fn disabled_tools() {
        let p = Policy::new(&perms(RiskLevel::Dangerous, &[]), &["web.*".into(), "shell.run".into()]);
        assert!(p.is_disabled("web.search"));
        assert!(p.is_disabled("shell.run"));
        assert!(!p.is_disabled("files.find"));
        assert!(matches!(p.decide("web.lookup", RiskLevel::Safe), Decision::Deny { .. }));
    }
}
