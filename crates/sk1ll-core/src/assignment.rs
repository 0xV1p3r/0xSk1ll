//! Assignment: which skills and rules are enabled in which harness.
//!
//! The fleet-wide assignment will live on the server (ADR 0001). Until then this
//! is this machine's slice of it, keyed by harness name and kept as a local
//! cache file. A missing file means everything is enabled everywhere.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::instructions::write_file;
use crate::{Error, IoExt, Result};

pub const ASSIGNMENT_FILE: &str = "assignment.json";

/// Assignment for this machine, by harness name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assignment {
    #[serde(default)]
    pub harnesses: BTreeMap<String, HarnessAssignment>,
}

/// One harness's toggles. An item is enabled if explicitly toggled on, or if
/// it has no toggle and the matching default is on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessAssignment {
    #[serde(default = "yes")]
    pub new_skills_default: bool,
    #[serde(default = "yes")]
    pub new_rules_default: bool,
    #[serde(default)]
    pub skills: BTreeMap<String, bool>,
    #[serde(default)]
    pub rules: BTreeMap<String, bool>,
}

fn yes() -> bool {
    true
}

static ALL_ENABLED: HarnessAssignment = HarnessAssignment::ALL_ENABLED;

impl HarnessAssignment {
    const ALL_ENABLED: Self = Self {
        new_skills_default: true,
        new_rules_default: true,
        skills: BTreeMap::new(),
        rules: BTreeMap::new(),
    };

    pub fn skill_enabled(&self, name: &str) -> bool {
        self.skills.get(name).copied().unwrap_or(self.new_skills_default)
    }

    pub fn rule_enabled(&self, name: &str) -> bool {
        self.rules.get(name).copied().unwrap_or(self.new_rules_default)
    }
}

impl Default for HarnessAssignment {
    fn default() -> Self {
        Self::ALL_ENABLED
    }
}

impl Assignment {
    /// `$XDG_CACHE_HOME/sk1ll/assignment.json` (or the platform equivalent).
    pub fn default_path() -> Result<PathBuf> {
        dirs::cache_dir()
            .map(|d| d.join("sk1ll").join(ASSIGNMENT_FILE))
            .ok_or_else(|| Error::Invalid("cannot determine the cache directory".into()))
    }

    /// Loads `path`; a missing file is the all-enabled default.
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => {
                serde_json::from_str(&text).map_err(|e| Error::Parse { path: path.to_path_buf(), message: e.to_string() })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).at(path),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let body = serde_json::to_string_pretty(self)
            .map_err(|e| Error::Parse { path: path.to_path_buf(), message: e.to_string() })?;
        write_file(path, &format!("{body}\n"))
    }

    /// A harness's toggles; all-enabled if it has none.
    pub fn harness(&self, harness: &str) -> &HarnessAssignment {
        self.harnesses.get(harness).unwrap_or(&ALL_ENABLED)
    }

    pub fn skill_enabled(&self, harness: &str, name: &str) -> bool {
        self.harness(harness).skill_enabled(name)
    }

    pub fn rule_enabled(&self, harness: &str, name: &str) -> bool {
        self.harness(harness).rule_enabled(name)
    }

    pub fn set_skill(&mut self, harness: &str, name: &str, enabled: bool) {
        self.entry(harness).skills.insert(name.to_string(), enabled);
    }

    pub fn set_rule(&mut self, harness: &str, name: &str, enabled: bool) {
        self.entry(harness).rules.insert(name.to_string(), enabled);
    }

    /// Records current values for existing skills before changing their default.
    pub fn materialize_skills(&mut self, harness: &str, names: &[String]) {
        let a = self.entry(harness);
        for name in names {
            a.skills.entry(name.clone()).or_insert(a.new_skills_default);
        }
    }

    /// Records current values for existing rules before changing their default.
    pub fn materialize_rules(&mut self, harness: &str, names: &[String]) {
        let a = self.entry(harness);
        for name in names {
            a.rules.entry(name.clone()).or_insert(a.new_rules_default);
        }
    }

    /// Changes the fallback for skills without a toggle, including existing ones.
    pub fn set_new_skills_default(&mut self, harness: &str, enabled: bool) {
        self.entry(harness).new_skills_default = enabled;
    }

    /// Changes the fallback for rules without a toggle, including existing ones.
    pub fn set_new_rules_default(&mut self, harness: &str, enabled: bool) {
        self.entry(harness).new_rules_default = enabled;
    }

    fn entry(&mut self, harness: &str) -> &mut HarnessAssignment {
        self.harnesses.entry(harness.to_string()).or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materializing_preserves_existing_items_when_defaults_change() {
        let mut a = Assignment::default();
        a.set_skill("h", "off", false);
        a.set_rule("h", "off", false);
        let names = vec!["implicit".to_string(), "off".to_string()];
        a.materialize_skills("h", &names);
        a.materialize_rules("h", &names);
        a.set_new_skills_default("h", false);
        a.set_new_rules_default("h", false);
        assert!(a.skill_enabled("h", "implicit") && a.rule_enabled("h", "implicit"));
        assert!(!a.skill_enabled("h", "off") && !a.rule_enabled("h", "off"));
        assert!(!a.skill_enabled("h", "later") && !a.rule_enabled("h", "later"));
        let names = vec!["implicit".to_string(), "later".to_string()];
        a.materialize_skills("h", &names);
        a.materialize_rules("h", &names);
        a.set_new_skills_default("h", true);
        a.set_new_rules_default("h", true);
        assert!(!a.skill_enabled("h", "later") && !a.rule_enabled("h", "later"));
        assert!(a.skill_enabled("h", "new") && a.rule_enabled("h", "new"));
    }

    #[test]
    fn toggles_override_defaults() {
        let mut a = Assignment::default();
        assert!(a.skill_enabled("claude", "pdf") && a.rule_enabled("claude", "git"));

        a.set_skill("claude", "pdf", false);
        a.set_new_skills_default("codex", false);
        a.set_skill("codex", "pdf", true);
        a.set_new_rules_default("codex", false);
        assert!(!a.skill_enabled("claude", "pdf"));
        assert!(a.skill_enabled("claude", "other"));
        assert!(a.skill_enabled("codex", "pdf"));
        assert!(!a.skill_enabled("codex", "other"));
        assert!(!a.rule_enabled("codex", "git"));

        a.set_rule("codex", "git", true);
        assert!(a.rule_enabled("codex", "git"));
        assert!(a.skill_enabled("gemini", "pdf"));
    }

    #[test]
    fn round_trips_and_missing_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(ASSIGNMENT_FILE);
        assert_eq!(Assignment::load(&path).unwrap(), Assignment::default());

        let mut a = Assignment::default();
        a.set_skill("claude", "pdf", false);
        a.set_new_rules_default("claude", false);
        a.save(&path).unwrap();
        assert_eq!(Assignment::load(&path).unwrap(), a);

        fs::write(&path, r#"{"harnesses":{"claude":{"skills":{"x":false}}}}"#).unwrap();
        let partial = Assignment::load(&path).unwrap();
        assert!(!partial.skill_enabled("claude", "x"));
        assert!(partial.harness("claude").new_skills_default);
    }
}
