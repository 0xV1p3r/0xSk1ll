//! Assignment: which skills and rules are enabled in which harness.
//!
//! Toggles are keyed by [`ItemId`] (ADR 0006), so a rename never touches the
//! assignment, and a toggle for an id no item has is inert. Skills and rules
//! are kept apart: the new-items defaults are per kind, and a toggle's kind
//! is known without looking up its item. Imports are skills.
//!
//! The fleet-wide assignment lives on the server (ADR 0001); this is one
//! machine's slice of it, keyed by harness name and kept as a local file. A
//! missing file means everything is enabled everywhere.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::instructions::write_file;
use crate::{Error, ItemId, Result};

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
    pub skills: BTreeMap<ItemId, bool>,
    #[serde(default)]
    pub rules: BTreeMap<ItemId, bool>,
}

fn yes() -> bool {
    true
}

static ALL_ENABLED: HarnessAssignment = HarnessAssignment::ALL_ENABLED;

impl HarnessAssignment {
    const ALL_ENABLED: Self =
        Self { new_skills_default: true, new_rules_default: true, skills: BTreeMap::new(), rules: BTreeMap::new() };

    /// A harness that starts with nothing enabled.
    pub fn all_off() -> Self {
        Self { new_skills_default: false, new_rules_default: false, ..Self::ALL_ENABLED }
    }

    pub fn skill_enabled(&self, id: &ItemId) -> bool {
        self.skills.get(id).copied().unwrap_or(self.new_skills_default)
    }

    pub fn rule_enabled(&self, id: &ItemId) -> bool {
        self.rules.get(id).copied().unwrap_or(self.new_rules_default)
    }
}

impl Default for HarnessAssignment {
    fn default() -> Self {
        Self::ALL_ENABLED
    }
}

impl Assignment {
    /// `assignment.json` in sk1ll's cache directory ([`crate::paths::cache_dir`],
    /// `$XDG_CACHE_HOME/sk1ll` by default).
    pub fn default_path() -> Result<PathBuf> {
        Ok(crate::paths::cache_dir()?.join(ASSIGNMENT_FILE))
    }

    /// Loads `path`; a missing file is the all-enabled default.
    pub fn load(path: &Path) -> Result<Self> {
        Ok(Self::observe(path)?.1)
    }

    /// The file's exact bytes (`None`: there is none) and what they hold,
    /// read once: what a compare-and-swap of the file compares against.
    pub fn observe(path: &Path) -> Result<(Option<Vec<u8>>, Self)> {
        let bytes = crate::config_file::read_regular(path, true)?;
        let assignment = match &bytes {
            Some(bytes) => {
                let parse = |message: String| Error::Parse { path: path.to_path_buf(), message };
                let text = std::str::from_utf8(bytes).map_err(|e| parse(e.to_string()))?;
                serde_json::from_str(text).map_err(|e| parse(e.to_string()))?
            }
            None => Self::default(),
        };
        Ok((bytes, assignment))
    }

    /// Saves `self` over the file only if it still holds exactly `expected`
    /// (`None`: there is no file), the bytes what is saved was derived from;
    /// `false`, with nothing written, if not. The file is replaced
    /// atomically, so only a write landing between the check and the rename
    /// can still be replaced.
    pub fn swap(&self, path: &Path, expected: Option<&[u8]>) -> Result<bool> {
        if crate::config_file::read_regular(path, true)?.as_deref() != expected {
            return Ok(false);
        }
        self.save(path)?;
        Ok(true)
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

    pub fn skill_enabled(&self, harness: &str, id: &ItemId) -> bool {
        self.harness(harness).skill_enabled(id)
    }

    pub fn rule_enabled(&self, harness: &str, id: &ItemId) -> bool {
        self.harness(harness).rule_enabled(id)
    }

    pub fn set_skill(&mut self, harness: &str, id: &ItemId, enabled: bool) {
        self.entry(harness).skills.insert(id.clone(), enabled);
    }

    pub fn set_rule(&mut self, harness: &str, id: &ItemId, enabled: bool) {
        self.entry(harness).rules.insert(id.clone(), enabled);
    }

    /// Records current values for existing skills before changing their default.
    pub fn materialize_skills(&mut self, harness: &str, ids: &[ItemId]) {
        let a = self.entry(harness);
        for id in ids {
            a.skills.entry(id.clone()).or_insert(a.new_skills_default);
        }
    }

    /// Records current values for existing rules before changing their default.
    pub fn materialize_rules(&mut self, harness: &str, ids: &[ItemId]) {
        let a = self.entry(harness);
        for id in ids {
            a.rules.entry(id.clone()).or_insert(a.new_rules_default);
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

    /// Sets both defaults and every explicit toggle of `harness`, so that
    /// every item, known here or not, has that value there.
    pub fn set_harness(&mut self, harness: &str, enabled: bool) {
        let a = self.entry(harness);
        a.new_skills_default = enabled;
        a.new_rules_default = enabled;
        a.skills.values_mut().chain(a.rules.values_mut()).for_each(|v| *v = enabled);
    }

    /// Moves `from`'s toggles and defaults to `to`, replacing any `to` had.
    /// `false` if `from` has no entry (it was all-enabled; so is `to` then).
    pub fn rename_harness(&mut self, from: &str, to: &str) -> bool {
        match self.harnesses.remove(from) {
            Some(a) => {
                self.harnesses.insert(to.to_string(), a);
                true
            }
            None => {
                self.harnesses.remove(to);
                false
            }
        }
    }

    fn entry(&mut self, harness: &str) -> &mut HarnessAssignment {
        self.harnesses.entry(harness.to_string()).or_default()
    }
}
