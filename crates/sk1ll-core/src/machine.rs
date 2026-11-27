//! Machine-local configuration: this machine's identity and its harnesses.
//!
//! Harnesses are not repo content: which agent tools exist, and where they
//! read from, differs per machine. The effective set is the built-in presets
//! whose home directory exists (e.g. `~/.claude`), adjusted by
//! `machine.toml` in the user's config dir. A preset's paths are relative to
//! its home, so an entry with `preset` and `home` is an instance of it in
//! another config directory of the same tool (a second Claude Code account,
//! say), with the preset as its kind.
//!
//! Presets added after 0.1.0 (and gemini's skills directory) are opt-in:
//! synced into only while `machine.toml` enables them explicitly, so an
//! upgrade never starts writing somewhere new. First-run setup enables the
//! ones it finds ([`MachineConfig::adopt_detected_presets`]); anywhere else
//! they are listed as available until enabled. The instances environment
//! variables point at ([`candidates`]) are found by the process that has the
//! environment and offered the same way.

use std::collections::BTreeMap;
use std::ffi::OsString;
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
    /// The preset this harness is, or is an instance of (`claude` for
    /// `~/.claude_2`); `None` for a custom harness. Rules name it in
    /// `harnesses:` as they name a harness.
    #[serde(default)]
    pub kind: Option<String>,
    /// The config directory of a preset or preset instance, e.g.
    /// `~/.claude_2`, which the preset's paths are relative to; `None` for a
    /// custom harness.
    #[serde(default)]
    pub home: Option<String>,
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
/// `instructions`. Any other name defines a new harness: an instance of the
/// preset `preset` names, rooted at `home`, or else a custom harness.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessEntry {
    /// The preset this entry is an instance of. On an entry named after a
    /// preset it can only be that preset.
    pub preset: Option<String>,
    /// The instance's config directory, which the preset's paths are
    /// relative to (default: the preset's own, e.g. `~/.claude`).
    pub home: Option<String>,
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
    /// A preset instance is not one: its kind is in `harness`.
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

/// The environment variables that point a tool at another config directory,
/// with the preset each is for. Only these: no directory is ever globbed.
pub const HOME_VARS: &[(&str, &str)] =
    &[("CLAUDE_CONFIG_DIR", "claude"), ("CODEX_HOME", "codex"), ("COPILOT_HOME", "copilot")];

/// A preset instance an environment variable of [`HOME_VARS`] points at,
/// offered but not yet in `machine.toml` (see [`candidates`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessCandidate {
    /// The name it is offered under, free on this machine.
    pub name: String,
    /// The variable it was found in, e.g. `CLAUDE_CONFIG_DIR`.
    pub var: String,
    /// What it syncs into once added: its preset in `harness.home`.
    pub harness: Harness,
    /// Why it isn't offered as on: what it overlaps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

impl HarnessCandidate {
    /// The change that adds it to `machine.toml` as `preset` and `home`.
    pub fn change(&self) -> HarnessChange {
        HarnessChange::Set { name: self.name.clone(), harness: self.harness.clone() }
    }
}

/// The preset instances that the environment variables of [`HOME_VARS`]
/// point at, as `var` reads them, given the harnesses this machine has
/// (`choices`, e.g. [`MachineConfig::choices`]) and its skills repo. The
/// daemon usually runs without the user's shell environment, so the process
/// that has it (the CLI, the GUI) calls this, and adds a candidate as any
/// harness change through the engine.
///
/// A variable is skipped when it isn't an absolute or `~/` path to a
/// directory, or names the preset's own home or the home of a harness that
/// is on or in `machine.toml`. A candidate that overlaps a harness that is
/// on, or the repo, carries a warning. Its name is the directory's, without
/// leading dots, in the harness name charset, else `<preset>-<n>`.
pub fn candidates(
    choices: &[HarnessChoice],
    repo: &Path,
    var: impl Fn(&str) -> Option<OsString>,
) -> Vec<HarnessCandidate> {
    home_or_none(|home| candidates_in(choices, repo, &var, home))
}

fn candidates_in(
    choices: &[HarnessChoice],
    repo: &Path,
    var: &dyn Fn(&str) -> Option<OsString>,
    home: &Path,
) -> Vec<HarnessCandidate> {
    let repo = real(&std::path::absolute(repo).unwrap_or_else(|_| repo.to_path_buf()));
    let resolve = |path: &str| real(&expand_in(path, home));
    let mut taken: Vec<String> = presets().map(String::from).chain(choices.iter().map(|c| c.name.clone())).collect();
    let mut used: Vec<PathBuf> = choices
        .iter()
        .filter(|c| c.enabled || c.configured)
        .filter_map(|c| c.harness.home.as_deref())
        .map(resolve)
        .collect();
    // What adding one must not overlap, as the writer checks it; each
    // candidate offered claims its own locations for the ones after it.
    let mut claimed: Vec<(String, Vec<PathBuf>)> =
        choices.iter().filter_map(|c| Some((c.name.clone(), locations(&reserved(c)?, home)))).collect();
    let mut out = Vec::new();
    for (env, kind) in HOME_VARS {
        let Some(spelled) = var(env).as_deref().and_then(|v| v.to_str()).and_then(|v| home_spelling(v, home)) else {
            continue;
        };
        let p = preset(kind).expect("HOME_VARS names presets");
        let dir = resolve(&spelled);
        if !dir.is_dir() || dir == resolve(p.home) || used.contains(&dir) {
            continue;
        }
        let harness = p.instance(Some(&spelled));
        if check_harness(kind, &harness, home).is_err() {
            continue;
        }
        let mine = locations(&harness, home);
        // One the writer refuses on its own (its skills and instructions
        // overlap, through a symlink say) claims nothing from the next.
        let own = match mine.as_slice() {
            [skills, instructions] if overlaps(skills, instructions) => {
                Some("its skills directory and instructions overlap".to_string())
            }
            _ => None,
        };
        let warning = own.or_else(|| {
            claimed
                .iter()
                .find(|(_, theirs)| theirs.iter().any(|theirs| mine.iter().any(|m| overlaps(m, theirs))))
                .map(|(other, _)| format!("overlaps {other}"))
                .or_else(|| mine.iter().any(|m| overlaps(m, &repo)).then(|| "overlaps the skills repo".to_string()))
        });
        let name = candidate_name(&spelled, kind, &taken);
        taken.push(name.clone());
        used.push(dir);
        if warning.is_none() {
            claimed.push((name.clone(), mine));
        }
        out.push(HarnessCandidate { name, var: env.to_string(), harness, warning });
    }
    out
}

/// `value` as a `home` is written: `~/` for a path in the user's home,
/// without a trailing `/`; `None` unless it is absolute or starts with `~/`.
fn home_spelling(value: &str, home: &Path) -> Option<String> {
    let value = value.trim();
    let value = if value.len() > 1 { value.trim_end_matches('/') } else { value };
    if value.starts_with("~/") {
        return Some(value.to_string());
    }
    let path = Path::new(value);
    if !path.is_absolute() {
        return None;
    }
    match path.strip_prefix(home) {
        Ok(rest) if !rest.as_os_str().is_empty() => rest.to_str().map(|rest| format!("~/{rest}")),
        _ => Some(value.to_string()),
    }
}

/// The name a candidate in `home` is offered under: the directory's name
/// without leading dots, lowercased, other characters as `-`; else (empty,
/// a preset's name, or `taken`) `<kind>-2`, `<kind>-3`, ...
fn candidate_name(home: &str, kind: &str, taken: &[String]) -> String {
    let base = home.rsplit('/').next().unwrap_or_default().trim_start_matches('.').to_lowercase();
    let mut name: String = base
        .chars()
        .map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' { c } else { '-' })
        .skip_while(|c| !c.is_ascii_alphanumeric())
        .take(64)
        .collect();
    if name.is_empty() || taken.contains(&name) || check_name(&name).is_err() {
        name = (2..).map(|n| format!("{kind}-{n}")).find(|n| !taken.contains(n)).expect("a free name");
    }
    name
}

/// What a new harness must not overlap in `c`, read from a choice as
/// [`MachineConfig::checked_in`] reads it from the config: a harness that is
/// on, or a preset that isn't opt-in, has no entry and so comes on by
/// itself once its home directory appears.
fn reserved(c: &HarnessChoice) -> Option<Harness> {
    if c.enabled {
        return Some(c.harness.clone());
    }
    let p = preset(&c.name).filter(|p| c.preset && !c.configured && !p.opt_in)?;
    Some(p.harness(None))
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
    /// For a preset, only what differs from the preset is written; for a
    /// name that isn't a preset, a `harness.kind` makes it an instance of
    /// that preset (rooted at `harness.home`), written the same way.
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
    #[serde(default)]
    pub notifications: Notifications,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Notifications {
    pub enabled: bool,
}

impl Default for Notifications {
    fn default() -> Self {
        Self { enabled: true }
    }
}

struct Preset {
    name: &'static str,
    /// The config directory whose existence means the harness is installed.
    /// The other paths are relative to it, or to an instance's `home`.
    home: &'static str,
    skills: Option<&'static str>,
    instructions: Option<&'static str>,
    format: InstructionsFormat,
    mode: LinkMode,
    /// Added after 0.1.0: synced into only while `machine.toml` says
    /// `enabled = true`, so upgrading never starts writing somewhere new.
    opt_in: bool,
    /// A skills directory added after 0.1.0 (gemini's): synced into only
    /// while `machine.toml` sets it as the entry's `skills`. An instance's
    /// entry is explicit, so an instance gets it unless it sets `skills`.
    opt_in_skills: Option<&'static str>,
}

const PRESETS: &[Preset] = &[
    Preset {
        name: "claude",
        home: "~/.claude",
        skills: Some("skills"),
        instructions: Some("rules"),
        format: InstructionsFormat::Rules,
        mode: LinkMode::Symlink,
        opt_in: false,
        opt_in_skills: None,
    },
    Preset {
        name: "codex",
        home: "~/.codex",
        skills: Some("skills"),
        instructions: Some("AGENTS.md"),
        format: InstructionsFormat::Block,
        mode: LinkMode::Copy,
        opt_in: false,
        opt_in_skills: None,
    },
    Preset {
        name: "opencode",
        home: "~/.config/opencode",
        skills: Some("skill"),
        instructions: Some("AGENTS.md"),
        format: InstructionsFormat::Block,
        mode: LinkMode::Symlink,
        opt_in: false,
        opt_in_skills: None,
    },
    Preset {
        name: "gemini",
        home: "~/.gemini",
        skills: None,
        instructions: Some("GEMINI.md"),
        format: InstructionsFormat::Block,
        mode: LinkMode::Symlink,
        opt_in: false,
        opt_in_skills: Some("skills"),
    },
    // Presets below place copies wherever their docs don't say skill
    // symlinks are followed.
    Preset {
        name: "copilot",
        home: "~/.copilot",
        skills: Some("skills"),
        instructions: Some("copilot-instructions.md"),
        format: InstructionsFormat::Block,
        mode: LinkMode::Copy,
        opt_in: true,
        opt_in_skills: None,
    },
    Preset {
        name: "cursor",
        home: "~/.cursor",
        skills: Some("skills"),
        // Cursor's user rules live in its settings, not in a file.
        instructions: None,
        format: InstructionsFormat::Block,
        mode: LinkMode::Copy,
        opt_in: true,
        opt_in_skills: None,
    },
    Preset {
        name: "amp",
        home: "~/.config/amp",
        skills: Some("skills"),
        instructions: Some("AGENTS.md"),
        format: InstructionsFormat::Block,
        mode: LinkMode::Copy,
        opt_in: true,
        opt_in_skills: None,
    },
    Preset {
        name: "kiro",
        home: "~/.kiro",
        skills: Some("skills"),
        // Global steering: one Markdown file per rule, always included by default.
        instructions: Some("steering"),
        format: InstructionsFormat::Rules,
        mode: LinkMode::Copy,
        opt_in: true,
        opt_in_skills: None,
    },
    Preset {
        name: "cline",
        home: "~/.cline",
        skills: Some("skills"),
        instructions: None,
        format: InstructionsFormat::Block,
        mode: LinkMode::Copy,
        opt_in: true,
        opt_in_skills: None,
    },
    Preset {
        name: "roo",
        home: "~/.roo",
        skills: Some("skills"),
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

/// The preset's settings for an instance in `home` (`None`: its own), as
/// [`Harness::kind`] of an effective harness names it, with an instance's
/// opt-in skills directory taken up. `None` if there is no such preset.
pub fn preset_instance(kind: &str, home: Option<&str>) -> Option<Harness> {
    preset(kind).map(|p| p.instance(home))
}

impl Preset {
    /// The default instance's settings, rooted at `home` (`None`: its own).
    fn harness(&self, home: Option<&str>) -> Harness {
        let home = home.unwrap_or(self.home);
        Harness {
            kind: Some(self.name.into()),
            home: Some(home.into()),
            skills: self.skills.map(|p| under(home, p)),
            instructions: self.instructions.map(|p| under(home, p)),
            instructions_format: self.format,
            mode: self.mode,
        }
    }

    /// An instance's settings: an explicit entry, so with its opt-in skills directory.
    fn instance(&self, home: Option<&str>) -> Harness {
        let mut harness = self.harness(home);
        if harness.skills.is_none() {
            harness.skills = self.opt_in_skills(home);
        }
        harness
    }

    fn opt_in_skills(&self, home: Option<&str>) -> Option<String> {
        self.opt_in_skills.map(|p| under(home.unwrap_or(self.home), p))
    }

    /// Whether `home` (the default one if `None`) exists, with `user` as `~`.
    fn installed(&self, home: Option<&str>, user: &Path) -> bool {
        expand_in(home.unwrap_or(self.home), user).is_dir()
    }
}

/// Whether `home` is spelled as a config directory can be: absolute or
/// `~/`-relative, with no `..`, and not `~` or `/` themselves. Symlinks are
/// checked when a change turns the harness on ([`check_harness`]).
fn plain_home(home: &str) -> bool {
    let rest = match home.strip_prefix("~/") {
        Some(rest) => rest,
        None if home.starts_with('/') => home,
        None => return false,
    };
    home.trim() == home
        && !home.contains(['\0', '\n', '\r'])
        && rest.split('/').any(|c| !c.is_empty())
        && !rest.split('/').any(|c| c == "..")
}

/// `rel` inside the directory `home`, spelled as `home` is.
fn under(home: &str, rel: &str) -> String {
    format!("{}/{rel}", home.trim_end_matches('/'))
}

/// `path` with `~` meaning `home`.
fn expand_in(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        home.to_path_buf()
    } else {
        path.strip_prefix("~/").map_or_else(|| PathBuf::from(path), |rest| home.join(rest.trim_start_matches('/')))
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
    /// Changes the desktop notification preference, preserving the rest of the file.
    pub fn set_notifications(path: &Path, enabled: bool) -> Result<()> {
        let snapshot = Snapshot::take(path)?;
        let text = snapshot.text()?;
        Self::parse(&text, path)?;
        let mut config = ConfigDoc::parse(&text, path)?;
        if !config.doc.contains_key("notifications") {
            config.doc["notifications"] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        config.doc["notifications"]["enabled"] = toml_edit::value(enabled);
        snapshot.replace(&config.doc.to_string())
    }

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

    pub(crate) fn parse(text: &str, path: &Path) -> Result<Self> {
        let mut config: Self =
            toml::from_str(text).map_err(|e| Error::Parse { path: path.to_path_buf(), message: e.to_string() })?;
        config.validate(path)?;
        if config.id.trim().is_empty() {
            config.id = hostname();
        }
        Ok(config)
    }

    /// What deserializing can't check: `preset` names a preset (its own, on
    /// a preset's entry), and `home` roots a preset or preset instance.
    fn validate(&self, path: &Path) -> Result<()> {
        for (name, entry) in &self.harnesses {
            let message = match (&entry.preset, preset(name)) {
                (Some(kind), _) if preset(kind).is_none() => {
                    let known = presets().collect::<Vec<_>>().join(", ");
                    format!("harness `{name}`: no preset `{kind}` (presets: {known})")
                }
                (Some(kind), Some(own)) if kind != own.name => format!(
                    "harness `{name}` is the `{name}` preset, so its `preset` can only be `{name}`; give an instance \
                     of `{kind}` a name of its own"
                ),
                (None, None) if entry.home.is_some() => format!(
                    "harness `{name}`: `home` needs `preset`; a custom harness sets its own `skills` and `instructions`"
                ),
                // Every path of a preset (instance) is derived from `home`,
                // so a malformed one would send sync anywhere: refused
                // while loading, not only when a change turns it on.
                _ if entry.home.as_deref().is_some_and(|h| !plain_home(h)) => format!(
                    "harness `{name}`: `home` must be a directory path, absolute or starting with `~/`, without \
                     `..` or surrounding spaces, and not the home directory or `/`"
                ),
                _ => continue,
            };
            return Err(Error::Parse { path: path.to_path_buf(), message });
        }
        Ok(())
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
        for p in PRESETS {
            if config.defined_by_entry(p.name) {
                continue;
            }
            let mut entry = config.harnesses.get(p.name).cloned().unwrap_or_default();
            if !p.installed(entry.home.as_deref(), home) {
                continue;
            }
            let (key, value) = if p.opt_in && entry.enabled.is_none() {
                entry.enabled = Some(true);
                ("enabled", toml_edit::value(true))
            } else if let Some(skills) = p.opt_in_skills(entry.home.as_deref())
                && entry.enabled != Some(false)
                && entry.skills.is_none()
            {
                entry.skills = Some(skills.clone());
                ("skills", toml_edit::value(skills))
            } else {
                continue;
            };
            let mine = locations(&merge(p.harness(entry.home.as_deref()), &entry), home);
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
            .filter(|preset| !self.defined_by_entry(preset.name))
            .map(|preset| {
                let entry = self.harnesses.get(preset.name);
                let own_home = entry.and_then(|e| e.home.as_deref());
                let enabled = effective.contains_key(preset.name);
                let mut harness = preset_harness(preset, entry);
                // Its skills directory isn't taken up yet: shown as what enabling it syncs into.
                let skills_off = preset.opt_in_skills.is_some() && entry.is_none_or(|e| e.skills.is_none());
                if skills_off && !enabled {
                    harness.skills = preset.opt_in_skills(own_home);
                }
                HarnessChoice {
                    name: preset.name.to_string(),
                    installed: preset.installed(own_home, home),
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
            if self.defined_by_entry(name) {
                out.push(HarnessChoice {
                    name: name.clone(),
                    installed: true,
                    enabled: effective.contains_key(name),
                    harness: self.entry_harness(name, entry),
                    preset: false,
                    configured: true,
                    setup_warning: None,
                    available: false,
                });
            }
        }
        out
    }

    /// Whether `name` is a harness of its own entry (a custom harness or a
    /// preset instance) rather than a preset. An opt-in preset's name with
    /// its own path keys keeps the custom harness defaults, unless `preset`
    /// or `home` says it is the preset.
    fn defined_by_entry(&self, name: &str) -> bool {
        match (preset(name), self.harnesses.get(name)) {
            (None, _) => true,
            (Some(p), Some(e)) if p.opt_in => {
                e.preset.is_none() && e.home.is_none() && (e.skills.is_some() || e.instructions.is_some())
            }
            (Some(_), _) => false,
        }
    }

    /// The preset `name` is, unless an entry of its own took the name.
    fn preset_named(&self, name: &str) -> Option<&'static Preset> {
        preset(name).filter(|_| !self.defined_by_entry(name))
    }

    /// The preset the entry `name` is an instance of.
    fn instance_of(&self, name: &str) -> Option<&'static Preset> {
        match preset(name) {
            Some(_) => None,
            None => self.harnesses.get(name)?.preset.as_deref().and_then(preset),
        }
    }

    /// What the harness `name` becomes as the preset `kind` (or an instance
    /// of it) rooted at `home`: each setting its `machine.toml` entry sets
    /// stays, and the rest follow the new preset (the custom harness
    /// defaults with no `kind`). `kind`/`home` of `None` keep the entry's,
    /// and `home` resets to the preset's own when `kind` changes. Built from
    /// the entry, not from what sync uses now, so a derived path never turns
    /// into an override. For `sk1ll harnesses add`, ahead of a
    /// [`HarnessChange::Set`].
    pub fn rebased(&self, name: &str, kind: Option<&str>, home: Option<&str>) -> Result<Harness> {
        let entry = self.harnesses.get(name).cloned().unwrap_or_default();
        let current = self.preset_named(name).map(|p| p.name).or(entry.preset.as_deref());
        let kind = kind.or(current);
        let home = home.or(entry.home.as_deref().filter(|_| kind == current));
        let base = match kind {
            None if home.is_some() => {
                return Err(Error::Invalid(format!("`home` for `{name}` needs a preset (--preset)")));
            }
            None => Harness::default(),
            Some(kind) => {
                let p = preset(kind).ok_or_else(|| {
                    Error::Invalid(format!(
                        "no preset `{kind}` (presets: {})",
                        presets().collect::<Vec<_>>().join(", ")
                    ))
                })?;
                if kind == name { p.harness(home) } else { p.instance(home) }
            }
        };
        // A custom entry's paths are its own; an override of a preset (or
        // instance) keeps only the keys it sets.
        Ok(merge(base, &entry))
    }

    /// What the entry of a custom harness or preset instance syncs into.
    fn entry_harness(&self, name: &str, entry: &HarnessEntry) -> Harness {
        match self.instance_of(name) {
            Some(p) => merge(p.instance(entry.home.as_deref()), entry),
            None => merge(Harness::default(), entry),
        }
    }

    /// Switches harness `name` on or off in the `machine.toml` at `path`,
    /// keeping the rest of the file as written. On means dropping `enabled`,
    /// so a preset is synced again only while its home directory exists
    /// (an opt-in one once setup takes it up). First-run setup uses this;
    /// later changes go through [`HarnessChange`].
    /// Turning one on checks where it then syncs, as any change that does
    /// ([`Self::edit`]), against the skills repo at `repo`.
    pub fn set_enabled(path: &Path, name: &str, enabled: bool, repo: &Path) -> Result<()> {
        let snapshot = Snapshot::take(path)?;
        let mut doc = ConfigDoc::parse(&snapshot.text()?, path)?;
        if enabled {
            doc.remove_key(name, "enabled")?;
        } else {
            doc.entry(name)?["enabled"] = toml_edit::value(false);
        }
        let text = doc.doc.to_string();
        // Parse what we are about to write so a bad edit never reaches the daemon.
        let after = Self::parse(&text, path)?;
        if enabled {
            home_or_none(|home| after.check_on(name, repo, home))?;
        }
        snapshot.replace(&text)
    }

    /// `change` made to the `machine.toml` `snapshot` read, keeping the rest
    /// of it as written. A change that leaves the harness synced into is
    /// checked against the other harnesses and the skills repo at `repo`.
    /// Changes nothing on disk; see [`ConfigEdit::write`].
    pub fn edit(snapshot: &Snapshot, change: &HarnessChange, repo: &Path) -> Result<ConfigEdit> {
        home_or_none(|home| Self::edit_in(snapshot, change, repo, home))
    }

    /// Renames a harness in machine.toml. A renamed preset becomes an
    /// instance of itself with the same settings (`preset = "<name>"`), and
    /// the original preset is disabled.
    pub fn rename_harness(snapshot: &Snapshot, from: &str, to: &str) -> Result<ConfigEdit> {
        check_name(to)?;
        let before = Self::from_snapshot(snapshot)?;
        if before.harnesses.contains_key(to) || preset(to).is_some() {
            return Err(Error::Invalid(format!(
                "harness `{to}` already exists on this Machine; rename to a name no harness has (or remove `{to}` \
                 first with `sk1ll harnesses remove {to}`)"
            )));
        }
        let mut doc = ConfigDoc::parse(&snapshot.text()?, snapshot.path())?;
        if let Some(preset) = before.preset_named(from) {
            let entry = before.harnesses.get(from);
            let enabled = before.effective().contains_key(from);
            if entry.is_none() && !enabled {
                return Err(Error::Invalid(format!("no configured or enabled harness `{from}`")));
            }
            // An instance of the preset, so it keeps its kind and follows the preset's defaults.
            let harness = preset_harness(preset, entry);
            let table = doc.entry(to)?;
            table["preset"] = toml_edit::value(preset.name);
            if let Some(home) = harness.home.as_deref().filter(|h| *h != preset.home) {
                table["home"] = toml_edit::value(home);
            }
            write_settings(table, &harness, Some(&preset.instance(harness.home.as_deref())));
            if !enabled {
                table["enabled"] = toml_edit::value(false);
            }
        } else {
            let table = doc.harnesses()?.ok_or_else(|| Error::Invalid(format!("no harness `{from}`")))?;
            let item = table.remove(from).ok_or_else(|| Error::Invalid(format!("no harness `{from}`")))?;
            table.insert(to, item);
        }
        if preset(from).is_some() {
            doc.entry(from)?["enabled"] = toml_edit::value(false);
        }
        let text = doc.doc.to_string();
        let config = Self::parse(&text, snapshot.path())?;
        Ok(ConfigEdit { text, config })
    }

    /// Makes `change` to the `machine.toml` at `path`, checked as
    /// [`Self::edit`] checks it, against the skills repo at `repo`.
    pub fn write_change(path: &Path, change: &HarnessChange, repo: &Path) -> Result<Self> {
        let snapshot = Snapshot::take(path)?;
        let edit = Self::edit(&snapshot, change, repo)?;
        edit.write(&snapshot)?;
        Ok(edit.config)
    }

    fn edit_in(snapshot: &Snapshot, change: &HarnessChange, repo: &Path, home: &Path) -> Result<ConfigEdit> {
        let path = snapshot.path();
        let before = Self::from_snapshot(snapshot)?;
        let name = change.name();
        // The preset `name` means here, unless an entry of its own took the
        // name and this change doesn't make it that preset again.
        let preset_of = before.preset_named(name).or_else(|| match change {
            HarnessChange::Set { harness, .. } if harness.kind.as_deref() == Some(name) => preset(name),
            _ => None,
        });
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
                        let installed = preset_of.is_none_or(|p| p.installed(None, home));
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
                if let Some(skills) = preset_of.and_then(|p| p.opt_in_skills(entry.and_then(|e| e.home.as_deref())))
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
                // The preset this builds on: `name` itself, or the one it is an instance of.
                let instance = match (&harness.kind, preset(name)) {
                    (Some(kind), Some(_)) if kind != name => {
                        return Err(Error::Invalid(format!(
                            "`{name}` is a preset, so it can't be an instance of `{kind}`; give the instance a name \
                             of its own"
                        )));
                    }
                    (Some(kind), None) => Some(preset(kind).ok_or_else(|| {
                        let known = presets().collect::<Vec<_>>().join(", ");
                        Error::Invalid(format!("no preset `{kind}` for `{name}` (presets: {known})"))
                    })?),
                    _ => None,
                };
                let own = preset_of.or(instance);
                if own.is_none() && harness.home.is_some() {
                    return Err(Error::Invalid(format!(
                        "`home` for `{name}` needs a preset: a custom harness sets its own skills and instructions"
                    )));
                }
                let base = match (preset_of, instance) {
                    (Some(p), _) => Some(p.harness(harness.home.as_deref())),
                    (None, Some(p)) => Some(p.instance(harness.home.as_deref())),
                    (None, None) => None,
                };
                // An opt-in preset's name with paths of its own would be a
                // custom harness (see `defined_by_entry`) unless it says it is the preset.
                let rehomed = own.zip(harness.home.as_deref()).filter(|(p, h)| *h != p.home).map(|(_, h)| h);
                let own_paths =
                    base.as_ref().is_some_and(|b| b.skills != harness.skills || b.instructions != harness.instructions);
                let says_preset = match (instance, preset_of) {
                    (Some(p), _) => Some(p),
                    (None, Some(p)) if p.opt_in && own_paths && rehomed.is_none() => Some(p),
                    _ => None,
                };
                let table = doc.entry(name)?;
                set_str(table, "preset", says_preset.map(|p| p.name));
                set_str(table, "home", rehomed);
                write_settings(table, harness, base.as_ref());
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
            after.validate(path)?;
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
        if !matches!(change, HarnessChange::Disable { .. }) {
            after.check_on(name, repo, home)?;
        }
        Ok(ConfigEdit { text: doc.doc.to_string(), config: after })
    }

    /// If `name` is synced into, where it syncs: valid paths that overlap
    /// no other harness (nor a preset that comes on by itself later) and
    /// stay out of the skills repo at `repo`.
    fn check_on(&self, name: &str, repo: &Path, home: &Path) -> Result<()> {
        if let Some(h) = self.all_on_in(home).get(name) {
            check_harness(name, &h.harness, home)?;
            check_locations(&self.checked_in(home), name, home)?;
            check_outside_repo(name, &h.harness, repo, home)?;
        }
        Ok(())
    }

    fn effective_in(&self, home: &Path) -> BTreeMap<String, EffectiveHarness> {
        let mut out = self.all_on_in(home);
        // A preset found only by its directory yields to an entry that syncs
        // into the same places: an instance rooted at its home creates that
        // directory with its first placement, which must not bring in a
        // second harness over the same files.
        let claimed: Vec<PathBuf> = out
            .values()
            .filter(|e| e.origin == HarnessOrigin::Local)
            .flat_map(|e| locations(&e.harness, home))
            .collect();
        out.retain(|_, e| {
            e.origin != HarnessOrigin::Detected
                || !locations(&e.harness, home).iter().any(|p| claimed.iter().any(|c| overlaps(p, c)))
        });
        out
    }

    /// What a change must not overlap: every harness that is on, before any
    /// yields, and each preset that would come on by itself once its home
    /// directory appears.
    fn checked_in(&self, home: &Path) -> BTreeMap<String, EffectiveHarness> {
        let mut out = self.all_on_in(home);
        for p in PRESETS.iter().filter(|p| !p.opt_in && !self.defined_by_entry(p.name)) {
            if !self.harnesses.get(p.name).is_some_and(|e| e.enabled == Some(false)) {
                out.entry(p.name.to_string())
                    .or_insert(EffectiveHarness { harness: p.harness(None), origin: HarnessOrigin::Detected });
            }
        }
        out
    }

    fn all_on_in(&self, home: &Path) -> BTreeMap<String, EffectiveHarness> {
        let mut out = BTreeMap::new();
        for preset in PRESETS.iter().filter(|p| !self.defined_by_entry(p.name)) {
            let entry = self.harnesses.get(preset.name);
            let on = match preset.opt_in {
                true => entry.and_then(|e| e.enabled) == Some(true),
                false => preset.installed(None, home) || entry.is_some(),
            };
            if !on {
                continue;
            }
            let origin = if entry.is_some() { HarnessOrigin::Override } else { HarnessOrigin::Detected };
            out.insert(preset.name.to_string(), EffectiveHarness { harness: preset_harness(preset, entry), origin });
        }
        // Custom harnesses and preset instances, on because their entry says so.
        for (name, entry) in &self.harnesses {
            if self.defined_by_entry(name) {
                let harness = self.entry_harness(name, entry);
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

/// What preset `p`'s own entry makes of it.
fn preset_harness(p: &Preset, entry: Option<&HarnessEntry>) -> Harness {
    match entry {
        Some(entry) => merge(p.harness(entry.home.as_deref()), entry),
        None => p.harness(None),
    }
}

/// Writes into `table` the settings of `harness` that differ from `base`,
/// the preset (instance) it builds on, and drops the rest; with no `base` (a
/// custom harness), its paths and any non-default format or mode.
fn write_settings(table: &mut toml_edit::Table, harness: &Harness, base: Option<&Harness>) {
    for (key, want, preset) in [
        ("skills", &harness.skills, base.map(|b| &b.skills)),
        ("instructions", &harness.instructions, base.map(|b| &b.instructions)),
    ] {
        let value = match preset {
            Some(p) if p == want => None,
            Some(_) => Some(want.clone().unwrap_or_default()),
            None => want.clone(),
        };
        set_str(table, key, value.as_deref());
    }
    let base = base.cloned().unwrap_or_default();
    let format = harness.instructions_format;
    set_or_default(table, "instructions_format", format_name(format), format == base.instructions_format);
    set_or_default(table, "mode", mode_name(harness.mode), harness.mode == base.mode);
}

/// Sets `key` to `value` (`None`: removes it). An unchanged value is left
/// as written, and a changed one keeps the comments around it.
fn set_str(table: &mut toml_edit::Table, key: &str, value: Option<&str>) {
    let Some(value) = value else {
        table.remove(key);
        return;
    };
    match table.get_mut(key).and_then(|item| item.as_value_mut()) {
        Some(old) if old.as_str() == Some(value) => {}
        Some(old) => {
            let decor = old.decor().clone();
            *old = value.into();
            *old.decor_mut() = decor;
        }
        None => table[key] = toml_edit::value(value),
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
    set_str(entry, key, (!default).then_some(value));
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
    for (what, value) in [
        ("skills directory", &harness.skills),
        ("instructions path", &harness.instructions),
        ("home directory", &harness.home),
    ] {
        let Some(value) = value else { continue };
        let bad = |why: &str| Error::Invalid(format!("{what} `{value}` for `{name}` {why}"));
        if value.trim() != value || value.is_empty() || value.contains(['\0', '\n', '\r']) {
            return Err(bad("must be a plain path without surrounding spaces"));
        }
        let path = match value.strip_prefix("~/") {
            Some(rest) => home.join(rest.trim_start_matches('/')),
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
    [&harness.skills, &harness.instructions].into_iter().flatten().map(|path| real(&expand_in(path, home))).collect()
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

/// Syncing into the repo itself would mix harness copies with repo content.
fn check_outside_repo(name: &str, harness: &Harness, repo: &Path, home: &Path) -> Result<()> {
    let repo = real(&std::path::absolute(repo).at(repo)?);
    if let Some(path) = locations(harness, home).into_iter().find(|path| overlaps(path, &repo)) {
        return Err(Error::Invalid(format!(
            "`{name}` can't sync into {}: it overlaps the skills repo {}",
            path.display(),
            repo.display()
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
