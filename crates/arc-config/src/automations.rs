//! User automations ("routines"): named multi-step actions triggered by a
//! phrase, stored in `~/.config/arc/automations.toml`.
//!
//! ```toml
//! [[automation]]
//! name = "start coding"
//! triggers = ["start coding", "coding mode"]
//! description = "Editor, terminal and browser on workspace 2"
//! steps = [
//!   { tool = "hyprland.switch_workspace", args = { workspace = 2 } },
//!   { tool = "apps.launch", args = { name = "code" } },
//!   { wait_ms = 800 },
//!   { tool = "terminal.open", args = { cwd = "~/Projects" } },
//!   { say = "Development environment is up." },
//! ]
//! ```

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AutomationFile {
    #[serde(default, rename = "automation")]
    pub automations: Vec<Automation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Automation {
    pub name: String,
    /// Phrases that run this automation (matched after normalisation).
    #[serde(default)]
    pub triggers: Vec<String>,
    #[serde(default)]
    pub description: String,
    pub steps: Vec<Step>,
    /// Continue with later steps when one fails.
    #[serde(default)]
    pub continue_on_error: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Step {
    Tool {
        tool: String,
        #[serde(default, skip_serializing_if = "Value::is_null")]
        args: Value,
    },
    Wait {
        wait_ms: u64,
    },
    Say {
        say: String,
    },
}

impl Automation {
    /// All phrases that trigger this automation, including its name.
    pub fn phrases(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.name.as_str()).chain(self.triggers.iter().map(String::as_str))
    }
}

pub fn load(path: &Path) -> Result<AutomationFile, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AutomationFile::default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

pub fn parse(text: &str) -> Result<AutomationFile, String> {
    let f: AutomationFile = toml::from_str(text).map_err(|e| e.to_string())?;
    let mut seen = std::collections::HashSet::new();
    for a in &f.automations {
        if a.name.trim().is_empty() {
            return Err("an automation has an empty name".into());
        }
        if !seen.insert(a.name.to_lowercase()) {
            return Err(format!("duplicate automation name {:?}", a.name));
        }
        if a.steps.is_empty() {
            return Err(format!("automation {:?} has no steps", a.name));
        }
        if a.steps.len() > 50 {
            return Err(format!("automation {:?} has more than 50 steps", a.name));
        }
        for s in &a.steps {
            if let Step::Wait { wait_ms } = s
                && *wait_ms > 60_000
            {
                return Err(format!("automation {:?}: waits are limited to 60000 ms", a.name));
            }
        }
    }
    Ok(f)
}

/// Serialise back to TOML (used when Arc creates/deletes automations; a
/// timestamped backup of the previous file is kept by the caller).
pub fn to_toml(f: &AutomationFile) -> String {
    let mut out = String::from(
        "# Arc automations. Edit freely; `arc automations` lists them and\n\
         # `arc config check` validates this file.\n\n",
    );
    out.push_str(&toml::to_string_pretty(f).unwrap_or_default());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../../../config/automations.toml");

    #[test]
    fn shipped_example_parses() {
        let f = parse(EXAMPLE).expect("example automations parse");
        assert!(f.automations.len() >= 3);
        let coding = f.automations.iter().find(|a| a.name == "start coding").unwrap();
        assert!(coding.phrases().any(|p| p == "coding mode"));
        assert!(coding.steps.iter().any(|s| matches!(s, Step::Say { .. })));
        assert!(coding.steps.iter().any(|s| matches!(s, Step::Wait { .. })));
    }

    #[test]
    fn rejects_duplicates_and_empty() {
        assert!(parse("[[automation]]\nname=\"a\"\nsteps=[{say=\"x\"}]\n[[automation]]\nname=\"A\"\nsteps=[{say=\"y\"}]\n").is_err());
        assert!(parse("[[automation]]\nname=\"a\"\nsteps=[]\n").is_err());
        assert!(parse("[[automation]]\nname=\"a\"\nsteps=[{wait_ms=999999}]\n").is_err());
    }

    #[test]
    fn roundtrip() {
        let f = parse(EXAMPLE).unwrap();
        let back = parse(&to_toml(&f)).unwrap();
        assert_eq!(f, back);
    }
}
