//! Machine-local configuration: this machine's identity and its harnesses.
//!
//! Harnesses are not repo content: which agent tools exist, and where they
//! read from, differs per machine. The effective set is the built-in presets
//! whose home directory exists (e.g. `~/.claude`), adjusted by
//! `machine.toml` in the user's config dir.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

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
}

const PRESETS: &[Preset] = &[
    Preset {
        name: "claude",
        home: ".claude",
        skills: Some("~/.claude/skills"),
        instructions: Some("~/.claude/rules"),
        format: InstructionsFormat::Rules,
        mode: LinkMode::Symlink,
    },
    Preset {
        name: "codex",
        home: ".codex",
        skills: Some("~/.codex/skills"),
        instructions: Some("~/.codex/AGENTS.md"),
        format: InstructionsFormat::Block,
        mode: LinkMode::Copy,
    },
    Preset {
        name: "opencode",
        home: ".config/opencode",
        skills: Some("~/.config/opencode/skill"),
        instructions: Some("~/.config/opencode/AGENTS.md"),
        format: InstructionsFormat::Block,
        mode: LinkMode::Symlink,
    },
    Preset {
        name: "gemini",
        home: ".gemini",
        skills: None,
        instructions: Some("~/.gemini/GEMINI.md"),
        format: InstructionsFormat::Block,
        mode: LinkMode::Symlink,
    },
];

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
        let mut config: Self = match fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).map_err(|e| Error::Parse { path: path.to_path_buf(), message: e.to_string() })?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(e).at(path),
        };
        if config.id.trim().is_empty() {
            config.id = hostname();
        }
        Ok(config)
    }

    /// The harnesses sync writes into on this machine, by name.
    pub fn harnesses(&self) -> BTreeMap<String, Harness> {
        self.effective().into_iter().map(|(name, e)| (name, e.harness)).collect()
    }

    /// Like [`harnesses`](Self::harnesses), with where each one came from.
    pub fn effective(&self) -> BTreeMap<String, EffectiveHarness> {
        match dirs::home_dir() {
            Some(home) => self.effective_in(&home),
            None => self.effective_in(Path::new("/nonexistent")),
        }
    }

    /// Every preset and every `machine.toml` entry, with what it would sync to.
    pub fn choices(&self) -> Vec<HarnessChoice> {
        match dirs::home_dir() {
            Some(home) => self.choices_in(&home),
            None => self.choices_in(Path::new("/nonexistent")),
        }
    }

    fn choices_in(&self, home: &Path) -> Vec<HarnessChoice> {
        let effective = self.effective_in(home);
        let mut out: Vec<HarnessChoice> = PRESETS
            .iter()
            .map(|preset| {
                let harness = match self.harnesses.get(preset.name) {
                    Some(entry) => merge(preset.harness(), entry),
                    None => preset.harness(),
                };
                HarnessChoice {
                    name: preset.name.to_string(),
                    installed: home.join(preset.home).is_dir(),
                    enabled: effective.contains_key(preset.name),
                    harness,
                }
            })
            .collect();
        for (name, entry) in &self.harnesses {
            if !PRESETS.iter().any(|p| p.name == name) {
                out.push(HarnessChoice {
                    name: name.clone(),
                    installed: true,
                    enabled: effective.contains_key(name),
                    harness: merge(Harness::default(), entry),
                });
            }
        }
        out
    }

    /// Switches harness `name` on or off in the `machine.toml` at `path`,
    /// keeping the rest of the file as written. On means dropping `enabled`,
    /// so a preset is synced again only while its home directory exists.
    pub fn set_enabled(path: &Path, name: &str, enabled: bool) -> Result<()> {
        use toml_edit::{DocumentMut, Item, Table, value};
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).at(path),
        };
        let mut doc: DocumentMut = text
            .parse()
            .map_err(|e: toml_edit::TomlError| Error::Parse { path: path.to_path_buf(), message: e.to_string() })?;
        let invalid = || Error::Invalid(format!("{}: `harnesses` must be a table of tables", path.display()));
        if enabled {
            let Some(harnesses) = doc.get_mut("harnesses") else { return Ok(()) };
            let harnesses = harnesses.as_table_mut().ok_or_else(invalid)?;
            if let Some(entry) = harnesses.get_mut(name) {
                let entry = entry.as_table_mut().ok_or_else(invalid)?;
                entry.remove("enabled");
                if entry.is_empty() {
                    harnesses.remove(name);
                }
            }
        } else {
            let harnesses = doc
                .entry("harnesses")
                .or_insert_with(|| {
                    let mut t = Table::new();
                    t.set_implicit(true);
                    Item::Table(t)
                })
                .as_table_mut()
                .ok_or_else(invalid)?;
            let entry = harnesses
                .entry(name)
                .or_insert_with(|| Item::Table(Table::new()))
                .as_table_mut()
                .ok_or_else(invalid)?;
            entry["enabled"] = value(false);
        }
        let text = doc.to_string();
        // Parse what we are about to write so a bad edit never reaches the daemon.
        toml::from_str::<MachineConfig>(&text)
            .map_err(|e| Error::Parse { path: path.to_path_buf(), message: e.to_string() })?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).at(dir)?;
        }
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, text).at(&tmp)?;
        fs::rename(&tmp, path).at(path)
    }

    fn effective_in(&self, home: &Path) -> BTreeMap<String, EffectiveHarness> {
        let mut out = BTreeMap::new();
        for preset in PRESETS {
            let entry = self.harnesses.get(preset.name);
            if !home.join(preset.home).is_dir() && entry.is_none() {
                continue;
            }
            let (harness, origin) = match entry {
                Some(entry) => (merge(preset.harness(), entry), HarnessOrigin::Override),
                None => (preset.harness(), HarnessOrigin::Detected),
            };
            out.insert(preset.name.to_string(), EffectiveHarness { harness, origin });
        }
        for (name, entry) in &self.harnesses {
            if !out.contains_key(name) && !PRESETS.iter().any(|p| p.name == name) {
                let harness = merge(Harness::default(), entry);
                out.insert(name.clone(), EffectiveHarness { harness, origin: HarnessOrigin::Local });
            }
        }
        out.retain(|name, _| self.harnesses.get(name).and_then(|e| e.enabled) != Some(false));
        out
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
            [harnesses.cursor]
            skills = "/opt/cursor/skills"
            "#,
        );
        assert_eq!(cfg.id, "workstation");
        let eff = cfg.effective_in(home.path());
        assert_eq!(eff.keys().collect::<Vec<_>>(), ["claude", "cursor", "gemini"]);
        let claude = &eff["claude"];
        assert_eq!(claude.origin, HarnessOrigin::Override);
        assert_eq!(claude.harness.mode, LinkMode::Copy);
        assert_eq!(claude.harness.skills.as_deref(), Some("~/.claude/skills"));
        assert_eq!(claude.harness.instructions, None);
        assert_eq!(eff["cursor"].origin, HarnessOrigin::Local);
        assert_eq!(eff["cursor"].harness.skills.as_deref(), Some("/opt/cursor/skills"));
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

        fs::write(&path, "[harnesses.cursor]\nskills = \"/opt/s\"\n").unwrap();
        MachineConfig::set_enabled(&path, "cursor", false).unwrap();
        MachineConfig::set_enabled(&path, "cursor", true).unwrap();
        let cfg = MachineConfig::load(&path).unwrap();
        assert_eq!(cfg.harnesses["cursor"].skills.as_deref(), Some("/opt/s"));
        assert_eq!(cfg.harnesses["cursor"].enabled, None);
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
