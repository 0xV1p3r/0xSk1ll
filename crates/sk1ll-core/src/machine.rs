//! Machine-local configuration: this machine's identity and its harnesses.
//!
//! Harnesses are not repo content: which agent tools exist, and where they
//! read from, differs per machine. The effective set is the built-in presets
//! whose home directory exists (e.g. `~/.claude`), adjusted by
//! `machine.toml` in the user's config dir.
//!
//! Presets added after 0.1.0 (and gemini's skills directory) are opt-in:
//! synced into only while `machine.toml` enables them explicitly, so an
//! upgrade never starts writing somewhere new. First-run setup enables the
//! ones it finds ([`MachineConfig::adopt_detected_presets`]); anywhere else
//! they are listed as available until enabled.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::config_file::Snapshot;
use crate::{Error, IoExt, Result, expand_tilde};

pub const MACHINE_FILE: &str = "machine.toml";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkMode {
    #[default]
    Symlink,
    Copy,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstructionsFormat {
    /// All rules in one marked block inside a single file.
    #[default]
    Block,
    /// One copied file per rule in a directory.
    Rules,
}

/// An agent tool on this machine: where it reads skills from and its global
/// instructions location.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Harness {
    /// Skills directory, e.g. `~/.claude/skills`.
    pub skills: Option<String>,
    /// Global instructions file (`block`), e.g. `~/.codex/AGENTS.md`, or a
    /// rules directory (`rules`), e.g. `~/.claude/rules`.
    pub instructions: Option<String>,
    #[serde(default)]
    pub instructions_format: InstructionsFormat,
    /// How skills are placed into `skills`.
    #[serde(default)]
    pub mode: LinkMode,
}

impl Harness {
    pub fn skills_dir(&self) -> Option<PathBuf> {
        self.skills.as_deref().map(expand_tilde)
    }

    pub fn instructions_file(&self) -> Option<PathBuf> {
        self.instructions.as_deref().map(expand_tilde)
    }
}

/// One `[harnesses.<name>]` table in `machine.toml`. For a preset name, set
/// fields replace the preset's; an empty string clears `skills` or
/// `instructions`. Any other name defines a new harness.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessEntry {
    pub skills: Option<String>,
    pub instructions: Option<String>,
    pub instructions_format: Option<InstructionsFormat>,
    pub mode: Option<LinkMode>,
    /// `false` drops the harness, e.g. a detected preset you don't want synced.
    pub enabled: Option<bool>,
}

/// Where an effective harness came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HarnessOrigin {
    /// A built-in preset whose home directory exists, unchanged.
    Detected,
    /// A built-in preset adjusted (or force-included) by `machine.toml`.
    Override,
    /// Defined only in `machine.toml`.
    Local,
}

#[derive(Debug, Clone, Serialize)]
pub struct EffectiveHarness {
    pub harness: Harness,
    pub origin: HarnessOrigin,
}

/// A harness the user can switch on or off, including detected presets that
/// `machine.toml` currently disables.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessChoice {
    pub name: String,
    /// A preset whose home directory exists, or an entry `machine.toml` defines.
    pub installed: bool,
    /// Whether sync writes into it.
    pub enabled: bool,
    pub harness: Harness,
    /// A built-in preset (`machine.toml` can only override or disable it).
    #[serde(default)]
    pub preset: bool,
    /// `machine.toml` has an entry for it.
    #[serde(default)]
    pub configured: bool,
    /// An opt-in preset that isn't enabled, or gemini synced into without
    /// its opt-in skills directory: enabling it takes that up (`harness`
    /// shows what it then syncs into).
    #[serde(default)]
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_warning: Option<String>,
}

/// A change to this machine's harnesses, made in `machine.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum HarnessChange {
    /// Sync into a preset or `machine.toml` harness again. An opt-in preset,
    /// or one whose home directory doesn't exist, is included with
    /// `enabled = true`; gemini also gets its skills directory unless its
    /// entry sets `skills`.
    Enable { name: String },
    /// Stop syncing into it; its entry and the paths it sets stay.
    Disable { name: String },
    /// Adds a harness, or replaces what an entry sets (its `enabled` stays).
    /// For a preset, only what differs from the preset is written.
    Set { name: String, harness: Harness },
    /// Deletes the entry: a custom harness goes away, and a preset goes back
    /// to its built-in settings.
    Remove { name: String },
}

impl HarnessChange {
    pub fn name(&self) -> &str {
        match self {
            Self::Enable { name } | Self::Disable { name } | Self::Set { name, .. } | Self::Remove { name } => name,
        }
    }
}

/// `machine.toml`: this machine's identity and harness overrides.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineConfig {
    /// Stable machine identity; defaults to the hostname.
    #[serde(default)]
    pub id: String,
    /// Raw entries from the file. Use [`MachineConfig::harnesses`] for the
    /// effective set (presets + entries).
    #[serde(default)]
    pub harnesses: BTreeMap<String, HarnessEntry>,
}

struct Preset {
    name: &'static str,
    /// Home-relative directory whose existence means the harness is installed.
    home: &'static str,
    skills: Option<&'static str>,
    instructions: Option<&'static str>,
    format: InstructionsFormat,
    mode: LinkMode,
    /// Added after 0.1.0: synced into only while `machine.toml` says
    /// `enabled = true`, so upgrading never starts writing somewhere new.
    opt_in: bool,
    /// A skills directory added after 0.1.0 (gemini's): synced into only
    /// while `machine.toml` sets it as the entry's `skills`.
    opt_in_skills: Option<&'static str>,
}

const PRESETS: &[Preset] = &[
    Preset {
        name: "claude",
        home: ".claude",
        skills: Some("~/.claude/skills"),
        instructions: Some("~/.claude/rules"),
        format: InstructionsFormat::Rules,
        mode: LinkMode::Symlink,
        opt_in: false,
        opt_in_skills: None,
    },
    Preset {
        name: "codex",
        home: ".codex",
        skills: Some("~/.codex/skills"),
        instructions: Some("~/.codex/AGENTS.md"),
        format: InstructionsFormat::Block,
        mode: LinkMode::Copy,
        opt_in: false,
        opt_in_skills: None,
    },
    Preset {
        name: "opencode",
        home: ".config/opencode",
        skills: Some("~/.config/opencode/skill"),
        instructions: Some("~/.config/opencode/AGENTS.md"),
        format: InstructionsFormat::Block,
        mode: LinkMode::Symlink,
        opt_in: false,
        opt_in_skills: None,
    },
    Preset {
        name: "gemini",
        home: ".gemini",
        skills: None,
        instructions: Some("~/.gemini/GEMINI.md"),
        format: InstructionsFormat::Block,
        mode: LinkMode::Symlink,
        opt_in: false,
        opt_in_skills: Some("~/.gemini/skills"),
    },
    // Presets below place copies wherever their docs don't say skill
    // symlinks are followed.
    Preset {
        name: "copilot",
        home: ".copilot",
        skills: Some("~/.copilot/skills"),
        instructions: Some("~/.copilot/copilot-instructions.md"),
        format: InstructionsFormat::Block,
        mode: LinkMode::Copy,
        opt_in: true,
        opt_in_skills: None,
    },
    Preset {
        name: "cursor",
        home: ".cursor",
        skills: Some("~/.cursor/skills"),
        // Cursor's user rules live in its settings, not in a file.
        instructions: None,
        format: InstructionsFormat::Block,
        mode: LinkMode::Copy,
        opt_in: true,
        opt_in_skills: None,
    },
    Preset {
        name: "amp",
        home: ".config/amp",
        skills: Some("~/.config/amp/skills"),
        instructions: Some("~/.config/amp/AGENTS.md"),
        format: InstructionsFormat::Block,
        mode: LinkMode::Copy,
        opt_in: true,
        opt_in_skills: None,
    },
    Preset {
        name: "kiro",
        home: ".kiro",
        skills: Some("~/.kiro/skills"),
        // Global steering: one Markdown file per rule, always included by default.
        instructions: Some("~/.kiro/steering"),
        format: InstructionsFormat::Rules,
        mode: LinkMode::Copy,
        opt_in: true,
        opt_in_skills: None,
    },
    Preset {
        name: "cline",
        home: ".cline",
        skills: Some("~/.cline/skills"),
        instructions: None,
        format: InstructionsFormat::Block,
        mode: LinkMode::Copy,
        opt_in: true,
        opt_in_skills: None,
    },
    Preset {
        name: "roo",
        home: ".roo",
        skills: Some("~/.roo/skills"),
        instructions: None,
        format: InstructionsFormat::Block,
        mode: LinkMode::Symlink,
        opt_in: true,
        opt_in_skills: None,
    },
];

/// The names of the built-in presets.
pub fn presets() -> impl Iterator<Item = &'static str> {
    PRESETS.iter().map(|p| p.name)
}

fn preset(name: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.name == name)
}

impl Preset {
    fn harness(&self) -> Harness {
        Harness {
            skills: self.skills.map(String::from),
            instructions: self.instructions.map(String::from),
            instructions_format: self.format,
            mode: self.mode,
        }
    }
}

/// A change to `machine.toml`, not yet written.
#[derive(Debug, Clone)]
pub struct ConfigEdit {
    /// The new `machine.toml` text.
    pub text: String,
    /// What it parses to.
    pub config: MachineConfig,
}

impl ConfigEdit {
    /// Writes the change, unless the file changed since `snapshot` was
    /// taken: then nothing is written and the error is [`Error::Changed`].
    pub fn write(&self, snapshot: &Snapshot) -> Result<()> {
        if snapshot.text()? != self.text {
            snapshot.replace(&self.text)?;
        }
        Ok(())
    }
}

impl MachineConfig {
    /// `$XDG_CONFIG_HOME/sk1ll/machine.toml` (or the platform equivalent).
    pub fn default_path() -> Result<PathBuf> {
        dirs::config_dir()
            .map(|d| d.join("sk1ll").join(MACHINE_FILE))
            .ok_or_else(|| Error::Invalid("cannot determine the config directory".into()))
    }

    /// Loads `path`; a missing file means presets only. An empty `id` is
    /// filled in from the hostname.
    pub fn load(path: &Path) -> Result<Self> {
        Self::parse(&crate::config_file::read_text(path, true)?.unwrap_or_default(), path)
    }

    /// The config `snapshot` read.
    pub fn from_snapshot(snapshot: &Snapshot) -> Result<Self> {
        Self::parse(&snapshot.text()?, snapshot.path())
    }

    fn parse(text: &str, path: &Path) -> Result<Self> {
        let mut config: Self =
            toml::from_str(text).map_err(|e| Error::Parse { path: path.to_path_buf(), message: e.to_string() })?;
        if config.id.trim().is_empty() {
            config.id = hostname();
        }
        Ok(config)
    }

    /// First-run setup: takes up the opt-in presets found on this machine
    /// (and gemini's skills directory) that setup didn't switch off, by
    /// writing `enabled = true` (`skills = "~/.gemini/skills"`) into the
    /// `machine.toml` at `path`, in one write. Nothing else ever does, so an
    /// upgraded machine syncs only into what it synced into before.
    pub fn adopt_detected_presets(path: &Path, repo: &Path) -> Result<Vec<String>> {
        let snapshot = Snapshot::take(path)?;
        let (text, skipped) = home_or_none(|home| Self::adopted_text(&snapshot, home, repo))?;
        if text != snapshot.text()? {
            snapshot.replace(&text)?;
        }
        Ok(skipped.into_iter().map(|(name, warning)| format!("{name}: {warning}")).collect())
    }

    /// The choices first-run setup offers, without writing anything.
    pub fn setup_choices(path: &Path, repo: &Path) -> Result<Vec<HarnessChoice>> {
        let snapshot = Snapshot::take(path)?;
        home_or_none(|home| {
            let (text, mut skipped) = Self::adopted_text(&snapshot, home, repo)?;
            let mut choices = Self::parse(&text, path)?.choices_in(home);
            for choice in &mut choices {
                choice.setup_warning = skipped.remove(&choice.name);
            }
            Ok(choices)
        })
    }

    fn adopted_text(snapshot: &Snapshot, home: &Path, repo: &Path) -> Result<(String, BTreeMap<String, String>)> {
        let mut config = Self::from_snapshot(snapshot)?;
        let mut doc = ConfigDoc::parse(&snapshot.text()?, snapshot.path())?;
        let repo = real(&std::path::absolute(repo).at(repo)?);
        let mut skipped = BTreeMap::new();
        for p in PRESETS.iter().filter(|p| home.join(p.home).is_dir()) {
            if config.custom(p.name) {
                continue;
            }
            let mut entry = config.harnesses.get(p.name).cloned().unwrap_or_default();
            let (key, value) = if p.opt_in && entry.enabled.is_none() {
                entry.enabled = Some(true);
                ("enabled", toml_edit::value(true))
            } else if let Some(skills) = p.opt_in_skills
                && entry.enabled != Some(false)
                && entry.skills.is_none()
            {
                entry.skills = Some(skills.into());
                ("skills", toml_edit::value(skills))
            } else {
                continue;
            };
            let mine = locations(&merge(p.harness(), &entry), home);
            let effective = config.effective_in(home);
            let conflict = overlapping_harness(&effective, p.name, &mine, home)
                .map(|(other, _, _)| other)
                .or_else(|| match mine.as_slice() {
                    [a, b] if overlaps(a, b) => Some(p.name),
                    _ => None,
                })
                .or_else(|| mine.iter().any(|path| overlaps(path, &repo)).then_some("the skills repo"));
            if let Some(other) = conflict {
                let warning = if p.opt_in { "not enabled" } else { "skills not enabled" };
                skipped.insert(p.name.to_string(), format!("{warning}: overlaps {other}"));
                continue;
            }
            config.harnesses.insert(p.name.to_string(), entry);
            doc.entry(p.name)?[key] = value;
        }
        let text = doc.doc.to_string();
        Self::parse(&text, snapshot.path())?;
        Ok((text, skipped))
    }

    /// The harnesses sync writes into on this machine, by name.
    pub fn harnesses(&self) -> BTreeMap<String, Harness> {
        self.effective().into_iter().map(|(name, e)| (name, e.harness)).collect()
    }

    /// Like [`harnesses`](Self::harnesses), with where each one came from.
    pub fn effective(&self) -> BTreeMap<String, EffectiveHarness> {
        home_or_none(|home| self.effective_in(home))
    }

    /// Every preset and every `machine.toml` entry, with what it would sync to.
    pub fn choices(&self) -> Vec<HarnessChoice> {
        home_or_none(|home| self.choices_in(home))
    }

    fn choices_in(&self, home: &Path) -> Vec<HarnessChoice> {
        let effective = self.effective_in(home);
        let mut out: Vec<HarnessChoice> = PRESETS
            .iter()
            .filter(|preset| !self.custom(preset.name))
            .map(|preset| {
                let entry = self.harnesses.get(preset.name);
                let enabled = effective.contains_key(preset.name);
                let mut harness = match entry {
                    Some(entry) => merge(preset.harness(), entry),
                    None => preset.harness(),
                };
                // Its skills directory isn't taken up yet: shown as what enabling it syncs into.
                let skills_off = preset.opt_in_skills.is_some() && entry.is_none_or(|e| e.skills.is_none());
                if skills_off && !enabled {
                    harness.skills = preset.opt_in_skills.map(String::from);
                }
                HarnessChoice {
                    name: preset.name.to_string(),
                    installed: home.join(preset.home).is_dir(),
                    enabled,
                    harness,
                    preset: true,
                    configured: entry.is_some(),
                    setup_warning: None,
                    available: if preset.opt_in { !enabled } else { skills_off && enabled },
                }
            })
            .collect();
        for (name, entry) in &self.harnesses {
            if self.custom(name) {
                out.push(HarnessChoice {
                    name: name.clone(),
                    installed: true,
                    enabled: effective.contains_key(name),
                    harness: merge(Harness::default(), entry),
                    preset: false,
                    configured: true,
                    setup_warning: None,
                    available: false,
                });
            }
        }
        out
    }

    /// An opt-in name with its own path keys keeps the custom harness defaults.
    fn custom(&self, name: &str) -> bool {
        match (preset(name), self.harnesses.get(name)) {
            (None, _) => true,
            (Some(p), Some(entry)) if p.opt_in => entry.skills.is_some() || entry.instructions.is_some(),
            (Some(_), _) => false,
        }
    }

    /// Switches harness `name` on or off in the `machine.toml` at `path`,
    /// keeping the rest of the file as written. On means dropping `enabled`,
    /// so a preset is synced again only while its home directory exists
    /// (an opt-in one once setup takes it up). First-run setup uses this;
    /// later changes go through [`HarnessChange`].
    pub fn set_enabled(path: &Path, name: &str, enabled: bool) -> Result<()> {
        let snapshot = Snapshot::take(path)?;
        let mut doc = ConfigDoc::parse(&snapshot.text()?, path)?;
        if enabled {
            doc.remove_key(name, "enabled")?;
        } else {
            doc.entry(name)?["enabled"] = toml_edit::value(false);
        }
        let text = doc.doc.to_string();
        // Parse what we are about to write so a bad edit never reaches the daemon.
        Self::parse(&text, path)?;
        snapshot.replace(&text)
    }

    /// `change` made to the `machine.toml` `snapshot` read, keeping the rest
    /// of it as written. Changes nothing on disk; see [`ConfigEdit::write`].
    pub fn edit(snapshot: &Snapshot, change: &HarnessChange) -> Result<ConfigEdit> {
        home_or_none(|home| Self::edit_in(snapshot, change, home))
    }

    /// Makes `change` to the `machine.toml` at `path`.
    pub fn write_change(path: &Path, change: &HarnessChange) -> Result<Self> {
        let snapshot = Snapshot::take(path)?;
        let edit = Self::edit(&snapshot, change)?;
        edit.write(&snapshot)?;
        Ok(edit.config)
    }

    fn edit_in(snapshot: &Snapshot, change: &HarnessChange, home: &Path) -> Result<ConfigEdit> {
        let path = snapshot.path();
        let before = Self::from_snapshot(snapshot)?;
        let name = change.name();
        // The preset `name` means here, unless an entry of its own took the name.
        let preset_of = preset(name).filter(|_| !before.custom(name));
        let known = before.harnesses.contains_key(name) || preset_of.is_some();
        let entry = before.harnesses.get(name);
        let mut doc = ConfigDoc::parse(&snapshot.text()?, path)?;
        match change {
            HarnessChange::Enable { .. } | HarnessChange::Disable { .. } if !known => {
                return Err(Error::Invalid(format!("no harness `{name}` (see `sk1ll harnesses --all`)")));
            }
            HarnessChange::Enable { .. } => {
                match preset_of {
                    Some(p) if p.opt_in => doc.entry(name)?["enabled"] = toml_edit::value(true),
                    _ => {
                        // A preset without its home directory is included only by an entry.
                        let installed = preset_of.is_none_or(|p| home.join(p.home).is_dir());
                        let other_keys = entry
                            .is_some_and(|e| HarnessEntry { enabled: None, ..e.clone() } != HarnessEntry::default());
                        if installed || other_keys {
                            doc.remove_key(name, "enabled")?;
                        } else {
                            doc.entry(name)?["enabled"] = toml_edit::value(true);
                        }
                    }
                }
                // Enabling a preset takes up its opt-in skills directory, unless one is set.
                if let Some(skills) = preset_of.and_then(|p| p.opt_in_skills)
                    && entry.is_none_or(|e| e.skills.is_none())
                {
                    doc.entry(name)?["skills"] = toml_edit::value(skills);
                }
            }
            HarnessChange::Disable { .. } => doc.entry(name)?["enabled"] = toml_edit::value(false),
            HarnessChange::Set { harness, .. } => {
                if !known {
                    check_name(name)?;
                }
                check_harness(name, harness, home)?;
                let base = preset_of.map(Preset::harness);
                let table = doc.entry(name)?;
                let path_key = |want: &Option<String>, preset: Option<&Option<String>>| match preset {
                    Some(p) if p == want => None,
                    Some(_) => Some(want.clone().unwrap_or_default()),
                    None => want.clone(),
                };
                let keys = [
                    ("skills", path_key(&harness.skills, base.as_ref().map(|b| &b.skills))),
                    ("instructions", path_key(&harness.instructions, base.as_ref().map(|b| &b.instructions))),
                ];
                // An opt-in preset whose paths differ is written out in full,
                // so it reads the same should it ever count as custom (see `custom`).
                let full = preset_of.is_some_and(|p| p.opt_in) && keys.iter().any(|(_, v)| v.is_some());
                if full {
                    for (key, value) in [("skills", &harness.skills), ("instructions", &harness.instructions)] {
                        table[key] = toml_edit::value(value.clone().unwrap_or_default());
                    }
                } else {
                    for (key, value) in keys {
                        match value {
                            Some(v) => table[key] = toml_edit::value(v),
                            None => drop(table.remove(key)),
                        }
                    }
                }
                let base = base.unwrap_or_default();
                let format = harness.instructions_format;
                let (same_format, same_mode) =
                    (format == base.instructions_format && !full, harness.mode == base.mode && !full);
                set_or_default(table, "instructions_format", format_name(format), same_format);
                set_or_default(table, "mode", mode_name(harness.mode), same_mode);
                // Setting an opt-in preset that says nothing about `enabled` takes it up.
                if preset_of.is_some_and(|p| p.opt_in) && entry.and_then(|e| e.enabled).is_none() {
                    table["enabled"] = toml_edit::value(true);
                }
                doc.drop_if_empty(name)?;
            }
            HarnessChange::Remove { .. } => {
                if entry.is_none() {
                    let why = if preset_of.is_some() { "uses the built-in preset already" } else { "doesn't exist" };
                    return Err(Error::Invalid(format!("harness `{name}` {why}; nothing to remove")));
                }
                doc.remove(name)?;
            }
        }
        let parse = |doc: &ConfigDoc| -> Result<Self> {
            // Parse what we are about to write so a bad edit never reaches the daemon.
            let mut after: Self = toml::from_str(&doc.doc.to_string())
                .map_err(|e| Error::Parse { path: path.to_path_buf(), message: e.to_string() })?;
            if after.id.trim().is_empty() {
                after.id = before.id.clone();
            }
            Ok(after)
        };
        let mut after = parse(&doc)?;
        // Dropping the last override of a harness that was synced into (a
        // preset not installed here, say) never switches it off on the way.
        if matches!(change, HarnessChange::Set { .. })
            && before.effective_in(home).contains_key(name)
            && !after.effective_in(home).contains_key(name)
        {
            doc.entry(name)?["enabled"] = toml_edit::value(true);
            after = parse(&doc)?;
        }
        // Whatever leaves the harness synced into (enabling it, resetting a
        // preset, setting its paths) checks where it now syncs. Turning it
        // off or removing it never does, so a bad entry can always be fixed.
        let effective = after.effective_in(home);
        if !matches!(change, HarnessChange::Disable { .. })
            && let Some(h) = effective.get(name)
        {
            check_harness(name, &h.harness, home)?;
            check_locations(&effective, name, home)?;
        }
        Ok(ConfigEdit { text: doc.doc.to_string(), config: after })
    }

    fn effective_in(&self, home: &Path) -> BTreeMap<String, EffectiveHarness> {
        let mut out = BTreeMap::new();
        for preset in PRESETS.iter().filter(|p| !self.custom(p.name)) {
            let entry = self.harnesses.get(preset.name);
            let on = match preset.opt_in {
                true => entry.and_then(|e| e.enabled) == Some(true),
                false => home.join(preset.home).is_dir() || entry.is_some(),
            };
            if !on {
                continue;
            }
            let (harness, origin) = match entry {
                Some(entry) => (merge(preset.harness(), entry), HarnessOrigin::Override),
                None => (preset.harness(), HarnessOrigin::Detected),
            };
            out.insert(preset.name.to_string(), EffectiveHarness { harness, origin });
        }
        for (name, entry) in &self.harnesses {
            if self.custom(name) {
                let harness = merge(Harness::default(), entry);
                out.insert(name.clone(), EffectiveHarness { harness, origin: HarnessOrigin::Local });
            }
        }
        out.retain(|name, _| self.harnesses.get(name).and_then(|e| e.enabled) != Some(false));
        out
    }
}

/// `f` with the user's home directory (or one that doesn't exist).
fn home_or_none<T>(f: impl FnOnce(&Path) -> T) -> T {
    match dirs::home_dir() {
        Some(home) => f(&home),
        None => f(Path::new("/nonexistent")),
    }
}

fn merge(mut base: Harness, entry: &HarnessEntry) -> Harness {
    let path = |v: &String| Some(v.clone()).filter(|s| !s.is_empty());
    if let Some(v) = &entry.skills {
        base.skills = path(v);
    }
    if let Some(v) = &entry.instructions {
        base.instructions = path(v);
    }
    if let Some(v) = entry.instructions_format {
        base.instructions_format = v;
    }
    if let Some(v) = entry.mode {
        base.mode = v;
    }
    base
}

/// `machine.toml` as written, for edits that keep its comments and layout.
struct ConfigDoc {
    path: PathBuf,
    doc: toml_edit::DocumentMut,
}

impl ConfigDoc {
    fn parse(text: &str, path: &Path) -> Result<Self> {
        let doc = text
            .parse()
            .map_err(|e: toml_edit::TomlError| Error::Parse { path: path.to_path_buf(), message: e.to_string() })?;
        Ok(Self { path: path.to_path_buf(), doc })
    }

    fn invalid(&self) -> Error {
        Error::Invalid(format!("{}: `harnesses` must be a table of tables", self.path.display()))
    }

    /// `[harnesses]`, if the file has it.
    fn harnesses(&mut self) -> Result<Option<&mut toml_edit::Table>> {
        let invalid = self.invalid();
        match self.doc.get_mut("harnesses") {
            Some(item) => item.as_table_mut().map(Some).ok_or(invalid),
            None => Ok(None),
        }
    }

    /// `[harnesses.<name>]`, created if missing.
    fn entry(&mut self, name: &str) -> Result<&mut toml_edit::Table> {
        use toml_edit::{Item, Table};
        let invalid = self.invalid();
        let harnesses = self
            .doc
            .entry("harnesses")
            .or_insert_with(|| {
                let mut t = Table::new();
                t.set_implicit(true);
                Item::Table(t)
            })
            .as_table_mut()
            .ok_or(invalid)?;
        let invalid = Error::Invalid(format!("{}: `harnesses.{name}` must be a table", self.path.display()));
        harnesses.entry(name).or_insert_with(|| Item::Table(Table::new())).as_table_mut().ok_or(invalid)
    }

    /// Removes `key` from `[harnesses.<name>]`, and the entry once it's empty.
    fn remove_key(&mut self, name: &str, key: &str) -> Result<()> {
        let invalid = self.invalid();
        let Some(harnesses) = self.harnesses()? else { return Ok(()) };
        if let Some(entry) = harnesses.get_mut(name) {
            entry.as_table_mut().ok_or(invalid)?.remove(key);
        }
        self.drop_if_empty(name)
    }

    fn drop_if_empty(&mut self, name: &str) -> Result<()> {
        if let Some(harnesses) = self.harnesses()?
            && harnesses.get(name).and_then(|e| e.as_table()).is_some_and(|e| e.is_empty())
        {
            harnesses.remove(name);
        }
        Ok(())
    }

    fn remove(&mut self, name: &str) -> Result<()> {
        if let Some(harnesses) = self.harnesses()? {
            harnesses.remove(name);
        }
        Ok(())
    }
}

/// Sets `key` to `value`, or drops it when `value` is the default (the
/// preset's, for a preset): the entry then follows the default again.
fn set_or_default(entry: &mut toml_edit::Table, key: &str, value: &str, default: bool) {
    if default {
        entry.remove(key);
    } else {
        entry[key] = toml_edit::value(value);
    }
}

fn format_name(format: InstructionsFormat) -> &'static str {
    match format {
        InstructionsFormat::Block => "block",
        InstructionsFormat::Rules => "rules",
    }
}

fn mode_name(mode: LinkMode) -> &'static str {
    match mode {
        LinkMode::Symlink => "symlink",
        LinkMode::Copy => "copy",
    }
}

/// A new harness name: lowercase letters, digits, `-` and `_`, starting
/// with a letter or digit, at most 64 characters.
pub fn check_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let ok = name.len() <= 64
        && chars.next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if !ok {
        return Err(Error::Invalid(format!(
            "invalid harness name `{name}`: use lowercase letters, digits, `-` and `_` (at most 64)"
        )));
    }
    Ok(())
}

/// A harness's paths: absolute or `~/`-relative, without `..`, never the
/// home directory or `/` itself (however a symlink spells them), and at
/// least one of them.
fn check_harness(name: &str, harness: &Harness, home: &Path) -> Result<()> {
    if harness.skills.is_none() && harness.instructions.is_none() {
        return Err(Error::Invalid(format!("harness `{name}` needs a skills directory, an instructions path or both")));
    }
    for (what, value) in [("skills directory", &harness.skills), ("instructions path", &harness.instructions)] {
        let Some(value) = value else { continue };
        let bad = |why: &str| Error::Invalid(format!("{what} `{value}` for `{name}` {why}"));
        if value.trim() != value || value.is_empty() || value.contains(['\0', '\n', '\r']) {
            return Err(bad("must be a plain path without surrounding spaces"));
        }
        let path = match value.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None if Path::new(value).is_absolute() => PathBuf::from(value),
            None => return Err(bad("must be absolute or start with `~/`")),
        };
        // `..` after a symlink means its target's parent, not the
        // lexical one: easier to refuse than to explain.
        if path.components().any(|c| c == Component::ParentDir) {
            return Err(bad("must not contain `..`"));
        }
        let path = real(&path);
        if path == real(home) || path.parent().is_none() {
            return Err(bad("must not be the home directory or `/`"));
        }
    }
    Ok(())
}

/// Two locations sync writes into never overlap, in one harness or across
/// two: one inside the other (or the same) means each would take the
/// other's content for its own to replace or remove. Compared where they
/// resolve to, so a symlink alias is no way around it.
fn locations(harness: &Harness, home: &Path) -> Vec<PathBuf> {
    [&harness.skills, &harness.instructions]
        .into_iter()
        .flatten()
        .map(|path| {
            let path = if path == "~" {
                home.to_path_buf()
            } else {
                path.strip_prefix("~/").map_or_else(|| PathBuf::from(path), |rest| home.join(rest))
            };
            real(&path)
        })
        .collect()
}

pub(crate) fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

fn overlapping_harness<'a, 'b>(
    harnesses: &'a BTreeMap<String, EffectiveHarness>,
    name: &str,
    mine: &'b [PathBuf],
    home: &Path,
) -> Option<(&'a str, &'b Path, PathBuf)> {
    for (other, e) in harnesses.iter().filter(|(n, _)| n.as_str() != name) {
        for theirs in locations(&e.harness, home) {
            if let Some(path) = mine.iter().find(|p| overlaps(p, &theirs)) {
                return Some((other, path, theirs));
            }
        }
    }
    None
}

fn check_locations(harnesses: &BTreeMap<String, EffectiveHarness>, name: &str, home: &Path) -> Result<()> {
    let Some(changed) = harnesses.get(name) else { return Ok(()) };
    let mine = locations(&changed.harness, home);
    if let Some((other, path, theirs)) = overlapping_harness(harnesses, name, &mine, home) {
        return Err(Error::Invalid(format!(
            "`{name}` would sync into {}, which overlaps {} that `{other}` uses; harnesses can't share              a location or sync into one another",
            path.display(),
            theirs.display()
        )));
    }
    if let [skills, instructions] = mine.as_slice()
        && overlaps(skills, instructions)
    {
        return Err(Error::Invalid(format!(
            "`{name}`'s skills directory {} and instructions {} overlap",
            skills.display(),
            instructions.display()
        )));
    }
    Ok(())
}

/// Where `path` leads, resolved in filesystem order: each prefix that
/// exists has its symlinks resolved before the next component (so `..`
/// after a symlink is its target's parent), the rest is taken as written.
pub(crate) fn real(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => {
                out.push(name);
                if let Ok(p) = out.canonicalize() {
                    out = p;
                }
            }
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// Best-effort hostname without extra dependencies.
pub(crate) fn hostname() -> String {
    let file = fs::read_to_string("/etc/hostname").ok();
    let env = || std::env::var("HOSTNAME").or_else(|_| std::env::var("COMPUTERNAME")).ok();
    let cmd = || {
        let out = Command::new("hostname").output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    file.or_else(env)
        .or_else(cmd)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(toml: &str) -> MachineConfig {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MACHINE_FILE);
        fs::write(&path, toml).unwrap();
        MachineConfig::load(&path).unwrap()
    }

    #[test]
    fn detects_presets_by_home_dir() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        fs::create_dir_all(home.path().join(".config/opencode")).unwrap();
        let eff = MachineConfig::default().effective_in(home.path());
        assert_eq!(eff.keys().collect::<Vec<_>>(), ["claude", "opencode"]);
        assert_eq!(eff["claude"].origin, HarnessOrigin::Detected);
        assert_eq!(eff["claude"].harness.instructions_format, InstructionsFormat::Rules);
        assert_eq!(eff["claude"].harness.skills.as_deref(), Some("~/.claude/skills"));
    }

    #[test]
    fn entries_override_extend_and_disable() {
        let home = tempfile::tempdir().unwrap();
        for d in [".claude", ".codex", ".gemini"] {
            fs::create_dir_all(home.path().join(d)).unwrap();
        }
        let cfg = config(
            r#"
            id = "workstation"
            [harnesses.claude]
            mode = "copy"
            instructions = ""
            [harnesses.codex]
            enabled = false
            [harnesses.agents]
            skills = "/opt/agents/skills"
            "#,
        );
        assert_eq!(cfg.id, "workstation");
        let eff = cfg.effective_in(home.path());
        assert_eq!(eff.keys().collect::<Vec<_>>(), ["agents", "claude", "gemini"]);
        let claude = &eff["claude"];
        assert_eq!(claude.origin, HarnessOrigin::Override);
        assert_eq!(claude.harness.mode, LinkMode::Copy);
        assert_eq!(claude.harness.skills.as_deref(), Some("~/.claude/skills"));
        assert_eq!(claude.harness.instructions, None);
        assert_eq!(eff["agents"].origin, HarnessOrigin::Local);
        assert_eq!(eff["agents"].harness.skills.as_deref(), Some("/opt/agents/skills"));
        assert_eq!(eff["gemini"].origin, HarnessOrigin::Detected);
    }

    #[test]
    fn choices_list_disabled_presets_and_set_enabled_keeps_comments() {
        let home = tempfile::tempdir().unwrap();
        for d in [".claude", ".codex"] {
            fs::create_dir_all(home.path().join(d)).unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sk1ll").join(MACHINE_FILE);
        MachineConfig::set_enabled(&path, "codex", false).unwrap();
        fs::write(&path, format!("# mine\nid = \"box\"\n{}", fs::read_to_string(&path).unwrap())).unwrap();
        let choices = MachineConfig::load(&path).unwrap().choices_in(home.path());
        let get = |name: &str| choices.iter().find(|c| c.name == name).unwrap().clone();
        assert!(get("claude").installed && get("claude").enabled);
        assert!(get("codex").installed && !get("codex").enabled);
        assert!(!get("gemini").installed && !get("gemini").enabled);
        assert_eq!(get("codex").harness.skills.as_deref(), Some("~/.codex/skills"));

        MachineConfig::set_enabled(&path, "codex", true).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# mine\nid = \"box\"\n"), "{text}");
        assert!(!text.contains("codex"), "{text}");
        assert!(MachineConfig::load(&path).unwrap().effective_in(home.path()).contains_key("codex"));
        assert!(!path.with_extension("toml.tmp").exists());

        fs::write(&path, "[harnesses.agents]\nskills = \"/opt/s\"\n").unwrap();
        MachineConfig::set_enabled(&path, "agents", false).unwrap();
        MachineConfig::set_enabled(&path, "agents", true).unwrap();
        let cfg = MachineConfig::load(&path).unwrap();
        assert_eq!(cfg.harnesses["agents"].skills.as_deref(), Some("/opt/s"));
        assert_eq!(cfg.harnesses["agents"].enabled, None);
    }

    #[test]
    fn harness_changes_keep_the_file_and_write_only_what_differs() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".codex")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MACHINE_FILE);
        fs::write(&path, "# mine\nid = \"box\" # stays\n").unwrap();
        let edit = |change: HarnessChange| edit(&path, change, home.path());
        let set = |name: &str, harness: Harness| HarnessChange::Set { name: name.into(), harness };

        // A custom harness writes its paths and non-default keys.
        let custom = Harness {
            skills: Some("~/.agents/skills".into()),
            instructions: Some("/opt/agents/rules".into()),
            instructions_format: InstructionsFormat::Rules,
            mode: LinkMode::Symlink,
        };
        let (text, cfg) = edit(set("agents", custom.clone())).unwrap();
        assert!(text.starts_with("# mine\nid = \"box\" # stays\n"), "{text}");
        assert!(!text.contains("mode"), "{text}");
        assert_eq!(cfg.choices_in(home.path()).into_iter().find(|c| c.name == "agents").unwrap().harness, custom);

        // A preset gets only its differences; the same settings drop the entry.
        let mut codex = preset("codex").unwrap().harness();
        codex.instructions = None;
        let (text, _) = edit(set("codex", codex)).unwrap();
        assert!(text.contains("[harnesses.codex]\ninstructions = \"\"\n"), "{text}");
        let (text, _) = edit(set("codex", preset("codex").unwrap().harness())).unwrap();
        assert!(!text.contains("codex"), "{text}");

        // Enabling a preset that isn't installed includes it explicitly.
        let (_, cfg) = edit(HarnessChange::Enable { name: "gemini".into() }).unwrap();
        assert!(cfg.effective_in(home.path()).contains_key("gemini"));
        let (_, cfg) = edit(HarnessChange::Disable { name: "gemini".into() }).unwrap();
        assert!(!cfg.effective_in(home.path()).contains_key("gemini"));
        let (_, cfg) = edit(HarnessChange::Remove { name: "agents".into() }).unwrap();
        assert!(!cfg.harnesses.contains_key("agents"));

        let refused = [
            set("Bad Name", custom.clone()),
            set("x", Harness::default()),
            set("x", Harness { skills: Some("relative/skills".into()), ..Harness::default() }),
            set("x", Harness { skills: Some("~/".into()), ..Harness::default() }),
            set("x", Harness { skills: Some("~/.codex/skills".into()), ..Harness::default() }),
            HarnessChange::Enable { name: "nope".into() },
            HarnessChange::Remove { name: "claude".into() },
        ];
        let before = fs::read_to_string(&path).unwrap();
        for change in refused {
            assert!(edit(change.clone()).is_err(), "{change:?}");
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }

    /// Makes `change` to the `machine.toml` at `path` with `home` as home.
    fn edit(path: &Path, change: HarnessChange, home: &Path) -> Result<(String, MachineConfig)> {
        let snapshot = Snapshot::take(path)?;
        let edit = MachineConfig::edit_in(&snapshot, &change, home)?;
        edit.write(&snapshot)?;
        Ok((edit.text, edit.config))
    }

    fn home_with(dirs: &[&str]) -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        for d in dirs {
            fs::create_dir_all(home.path().join(d)).unwrap();
        }
        home
    }

    #[test]
    fn opt_in_presets_stay_off_on_an_upgraded_machine() {
        let home = home_with(&[".claude", ".gemini", ".kiro", ".cursor"]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MACHINE_FILE);
        // A 0.1.0 machine.toml.
        fs::write(&path, "id = \"box\"\n[harnesses.codex]\nenabled = false\n").unwrap();
        let cfg = MachineConfig::load(&path).unwrap();
        let eff = cfg.effective_in(home.path());
        assert_eq!(eff.keys().collect::<Vec<_>>(), ["claude", "gemini"]);
        assert_eq!(eff["gemini"].harness.skills, None, "gemini syncs only its instructions, as before");
        let choices = cfg.choices_in(home.path());
        let get = |name: &str| choices.iter().find(|c| c.name == name).unwrap().clone();
        assert!(get("kiro").installed && !get("kiro").enabled && get("kiro").available);
        assert_eq!(get("kiro").harness.skills.as_deref(), Some("~/.kiro/skills"), "what enabling it syncs into");
        assert!(get("gemini").enabled && get("gemini").available);
        assert!(get("claude").enabled && !get("claude").available);
        assert!(!get("copilot").installed && !get("copilot").enabled);

        // Opting in is written explicitly, and stays so.
        edit(&path, HarnessChange::Enable { name: "kiro".into() }, home.path()).unwrap();
        edit(&path, HarnessChange::Enable { name: "gemini".into() }, home.path()).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("[harnesses.kiro]\nenabled = true\n"), "{text}");
        assert!(text.contains("[harnesses.gemini]\nskills = \"~/.gemini/skills\"\n"), "{text}");
        let eff = MachineConfig::load(&path).unwrap().effective_in(home.path());
        assert_eq!(eff.keys().collect::<Vec<_>>(), ["claude", "gemini", "kiro"]);
        assert_eq!(eff["gemini"].harness.skills.as_deref(), Some("~/.gemini/skills"));
        assert_eq!(eff["kiro"].harness.instructions.as_deref(), Some("~/.kiro/steering"));
        // Off again, and on: the entry says so each time.
        edit(&path, HarnessChange::Disable { name: "kiro".into() }, home.path()).unwrap();
        assert!(!MachineConfig::load(&path).unwrap().effective_in(home.path()).contains_key("kiro"));

        // Without machine.toml at all, the same.
        let bare = tempfile::tempdir().unwrap();
        let eff = MachineConfig::load(&bare.path().join(MACHINE_FILE)).unwrap().effective_in(home.path());
        assert_eq!(eff.keys().collect::<Vec<_>>(), ["claude", "gemini"]);
    }

    #[test]
    fn first_run_setup_enables_the_opt_in_presets_it_finds() {
        let home = home_with(&[".claude", ".gemini", ".kiro", ".cursor"]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MACHINE_FILE);
        // Setup turned cursor off; the listing shows the rest as setup leaves them, writing nothing.
        MachineConfig::set_enabled(&path, "cursor", false).unwrap();
        let before = fs::read_to_string(&path).unwrap();
        let snapshot = Snapshot::take(&path).unwrap();
        let adopted = MachineConfig::adopted_text(&snapshot, home.path(), &dir.path().join("repo")).unwrap().0;
        let listed = MachineConfig::parse(&adopted, &path).unwrap().choices_in(home.path());
        let on: Vec<_> = listed.iter().filter(|c| c.enabled).map(|c| c.name.as_str()).collect();
        assert_eq!(on, ["claude", "gemini", "kiro"]);
        assert_eq!(fs::read_to_string(&path).unwrap(), before, "listing writes nothing");

        let snapshot = Snapshot::take(&path).unwrap();
        snapshot
            .replace(&MachineConfig::adopted_text(&snapshot, home.path(), &dir.path().join("repo")).unwrap().0)
            .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let eff = MachineConfig::load(&path).unwrap().effective_in(home.path());
        assert_eq!(eff.keys().collect::<Vec<_>>(), ["claude", "gemini", "kiro"], "{text}");
        assert_eq!(eff["gemini"].harness.skills.as_deref(), Some("~/.gemini/skills"));
        assert!(text.contains("[harnesses.cursor]\nenabled = false\n"), "{text}");
        // Taken up once: running it again changes nothing.
        let snapshot = Snapshot::take(&path).unwrap();
        assert_eq!(MachineConfig::adopted_text(&snapshot, home.path(), &dir.path().join("repo")).unwrap().0, text);
    }

    #[test]
    fn setup_skips_adoption_when_destinations_overlap_harnesses_or_repo() {
        let home = home_with(&[".kiro", ".cursor", ".roo"]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MACHINE_FILE);
        fs::write(&path, "[harnesses.local]\nskills = '~/.kiro/skills/nested'\n").unwrap();
        let snapshot = Snapshot::take(&path).unwrap();
        let repo = home.path().join(".cursor/skills/repo");
        let (text, skipped) = MachineConfig::adopted_text(&snapshot, home.path(), &repo).unwrap();
        assert_eq!(skipped["kiro"], "not enabled: overlaps local");
        assert_eq!(skipped["cursor"], "not enabled: overlaps the skills repo");
        snapshot.replace(&text).unwrap();
        let effective = MachineConfig::load(&path).unwrap().effective_in(home.path());
        assert_eq!(effective.keys().map(String::as_str).collect::<Vec<_>>(), ["local", "roo"]);
    }

    #[cfg(unix)]
    #[test]
    fn loading_machine_config_rejects_a_fifo() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MACHINE_FILE);
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(MachineConfig::load(&path).unwrap_err().to_string().contains("not a regular file"));
    }

    #[test]
    fn a_custom_harness_named_like_an_opt_in_preset_keeps_its_paths() {
        let home = home_with(&[".kiro"]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MACHINE_FILE);
        fs::write(&path, "[harnesses.kiro]\nenabled = true\nskills = \"/my/skills\"\n").unwrap();
        let own = Harness { skills: Some("/my/skills".into()), ..Harness::default() };
        let cfg = MachineConfig::load(&path).unwrap();
        let eff = cfg.effective_in(home.path());
        assert_eq!(eff["kiro"].origin, HarnessOrigin::Local);
        assert_eq!(eff["kiro"].harness, own);
        let choices = cfg.choices_in(home.path());
        assert_eq!(choices.iter().filter(|c| c.name == "kiro").count(), 1);
        assert!(choices.iter().any(|c| c.name == "kiro" && !c.preset && c.enabled));
        // First-run setup leaves it alone.
        let snapshot = Snapshot::take(&path).unwrap();
        assert_eq!(
            MachineConfig::adopted_text(&snapshot, home.path(), &dir.path().join("repo")).unwrap().0,
            snapshot.text().unwrap()
        );

        // Off and on again: still exactly its own paths.
        edit(&path, HarnessChange::Disable { name: "kiro".into() }, home.path()).unwrap();
        let (_, cfg) = edit(&path, HarnessChange::Enable { name: "kiro".into() }, home.path()).unwrap();
        assert_eq!(cfg.effective_in(home.path())["kiro"].harness, own);
        // Editing it keeps it custom: nothing of the preset comes in.
        let (_, cfg) = edit(
            &path,
            HarnessChange::Set {
                name: "kiro".into(),
                harness: Harness { skills: Some("/opt/kiro2".into()), mode: LinkMode::Copy, ..Harness::default() },
            },
            home.path(),
        )
        .unwrap();
        assert_eq!(cfg.effective_in(home.path())["kiro"].harness.instructions, None);
        // Removing it leaves the preset available, still off.
        let (_, cfg) = edit(&path, HarnessChange::Remove { name: "kiro".into() }, home.path()).unwrap();
        assert!(!cfg.effective_in(home.path()).contains_key("kiro"));
        assert!(cfg.choices_in(home.path()).iter().any(|c| c.name == "kiro" && c.preset && c.available));
    }

    #[test]
    fn an_opt_in_preset_with_paths_of_its_own_reads_the_same_when_off() {
        let home = home_with(&[".kiro"]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MACHINE_FILE);
        let mut kiro = preset("kiro").unwrap().harness();
        kiro.skills = Some("/opt/kiro-skills".into());
        // Setting it takes it up, written out in full.
        let (_, cfg) =
            edit(&path, HarnessChange::Set { name: "kiro".into(), harness: kiro.clone() }, home.path()).unwrap();
        assert_eq!(cfg.effective_in(home.path())["kiro"].harness, kiro);
        edit(&path, HarnessChange::Disable { name: "kiro".into() }, home.path()).unwrap();
        let (_, cfg) = edit(&path, HarnessChange::Enable { name: "kiro".into() }, home.path()).unwrap();
        assert_eq!(cfg.effective_in(home.path())["kiro"].harness, kiro);
    }

    #[test]
    fn setting_never_switches_off_a_harness_that_was_on() {
        // Not installed here, so on only through its entry.
        let home = home_with(&[]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MACHINE_FILE);
        fs::write(
            &path,
            "[harnesses.codex]\nmode = \"symlink\"\n[harnesses.kiro]\nenabled = true\nmode = \"symlink\"\n",
        )
        .unwrap();
        for name in ["codex", "kiro"] {
            let set = HarnessChange::Set { name: name.into(), harness: preset(name).unwrap().harness() };
            let (text, cfg) = edit(&path, set, home.path()).unwrap();
            assert!(cfg.effective_in(home.path()).contains_key(name), "{name}: {text}");
            assert!(text.contains(&format!("[harnesses.{name}]\nenabled = true\n")), "{name}: {text}");
        }
    }

    #[test]
    fn setting_a_preset_value_again_drops_the_override() {
        let home = home_with(&[".claude"]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MACHINE_FILE);
        let set = |harness: Harness| HarnessChange::Set { name: "claude".into(), harness };
        let mut claude = preset("claude").unwrap().harness();
        claude.mode = LinkMode::Copy;
        claude.instructions_format = InstructionsFormat::Block;
        claude.instructions = Some("~/.claude/CLAUDE.md".into());
        let (text, _) = edit(&path, set(claude.clone()), home.path()).unwrap();
        assert!(text.contains("mode = \"copy\"") && text.contains("instructions_format = \"block\""), "{text}");
        claude.mode = LinkMode::Symlink;
        let (text, _) = edit(&path, set(claude.clone()), home.path()).unwrap();
        assert!(!text.contains("mode"), "{text}");
        let (text, cfg) = edit(&path, set(preset("claude").unwrap().harness()), home.path()).unwrap();
        assert!(!text.contains("claude"), "{text}");
        assert_eq!(cfg.effective_in(home.path())["claude"].origin, HarnessOrigin::Detected);
    }

    #[cfg(unix)]
    #[test]
    fn paths_are_checked_where_they_lead() {
        let home = home_with(&[".claude"]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MACHINE_FILE);
        let t = dir.path();
        fs::create_dir_all(t.join("s/inner")).unwrap();
        std::os::unix::fs::symlink(home.path(), t.join("home-alias")).unwrap();
        std::os::unix::fs::symlink(t.join("s"), t.join("alias")).unwrap();
        let p = |rel: &str| Some(t.join(rel).to_string_lossy().into_owned());
        let set = |name: &str, skills: Option<String>, instructions: Option<String>| HarnessChange::Set {
            name: name.into(),
            harness: Harness { skills, instructions, ..Harness::default() },
        };
        edit(&path, set("a", p("s"), None), home.path()).unwrap();
        let refused = [
            set("x", Some("~/alias/../x".into()), None),
            set("x", p("s/../x"), None),
            set("x", p("home-alias"), None),
            set("x", Some("/".into()), None),
            // Nested in another harness's location, either way, or by an alias.
            set("x", p("s/inner"), None),
            set("x", Some(t.to_string_lossy().into_owned()), None),
            set("x", None, p("s/AGENTS.md")),
            set("x", p("alias/inner"), None),
            set("x", p("alias"), None),
            // Or within one harness.
            set("x", p("y"), p("y/AGENTS.md")),
            set("x", Some("~/.claude/skills/x".into()), None),
        ];
        for change in refused {
            assert!(edit(&path, change.clone(), home.path()).is_err(), "{change:?}");
        }
        edit(&path, set("x", p("y/skills"), p("y/AGENTS.md")), home.path()).unwrap();

        // An entry written by hand is checked again whenever it is turned
        // on, but can always be turned off or removed.
        fs::write(&path, format!("[harnesses.bad]\nskills = {:?}\nenabled = false\n", t.join("s/../s/x"))).unwrap();
        assert!(edit(&path, HarnessChange::Enable { name: "bad".into() }, home.path()).is_err());
        edit(&path, HarnessChange::Disable { name: "bad".into() }, home.path()).unwrap();
        edit(&path, HarnessChange::Remove { name: "bad".into() }, home.path()).unwrap();
        // So is a preset reset to paths that now overlap another harness.
        fs::write(
            &path,
            format!(
                "[harnesses.claude]\nskills = {:?}\n[harnesses.mine]\nskills = \"~/.claude/skills\"\n",
                t.join("c")
            ),
        )
        .unwrap();
        assert!(edit(&path, HarnessChange::Remove { name: "claude".into() }, home.path()).is_err());
    }

    #[test]
    fn missing_file_uses_hostname_and_rejects_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = MachineConfig::load(&dir.path().join("none.toml")).unwrap();
        assert!(!cfg.id.is_empty());
        assert!(cfg.harnesses.is_empty());
        let path = dir.path().join(MACHINE_FILE);
        fs::write(&path, "[harnesses.claude]\nskill = \"x\"\n").unwrap();
        assert!(matches!(MachineConfig::load(&path), Err(Error::Parse { .. })));
    }
}
