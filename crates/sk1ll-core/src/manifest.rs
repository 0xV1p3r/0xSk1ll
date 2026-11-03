use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, Item, Table, value};

use crate::{Error, IoExt, Result, expand_tilde};

pub const MANIFEST_FILE: &str = "sk1ll.toml";

/// `sk1ll.toml`: what to import, where harnesses look for skills and global
/// instructions, and what each machine profile wants.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    #[serde(default)]
    pub import: BTreeMap<String, Import>,
    #[serde(default)]
    pub targets: BTreeMap<String, Target>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Import {
    /// Git URL, local path, or `github:owner/repo`.
    pub source: String,
    /// Subdirectory within the source repo containing `SKILL.md`.
    #[serde(default)]
    pub path: String,
    #[serde(rename = "ref", default = "default_ref")]
    pub git_ref: String,
}

fn default_ref() -> String {
    "HEAD".into()
}

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
    /// All fragments in one marked block inside a single file.
    #[default]
    Block,
    /// One copied file per fragment in a directory.
    Rules,
}

/// A harness: where it reads skills from and its global instructions file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
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

impl Target {
    pub fn skills_dir(&self) -> Option<PathBuf> {
        self.skills.as_deref().map(expand_tilde)
    }

    pub fn instructions_file(&self) -> Option<PathBuf> {
        self.instructions.as_deref().map(expand_tilde)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Glob patterns over skill names.
    #[serde(default = "match_all")]
    pub skills: Vec<String>,
    #[serde(default)]
    pub exclude_skills: Vec<String>,
    /// Glob patterns over instruction fragment names.
    #[serde(default = "match_all")]
    pub instructions: Vec<String>,
    #[serde(default)]
    pub exclude_instructions: Vec<String>,
    /// Target names; empty means all targets.
    #[serde(default)]
    pub targets: Vec<String>,
}

fn match_all() -> Vec<String> {
    vec!["*".into()]
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            skills: match_all(),
            exclude_skills: Vec::new(),
            instructions: match_all(),
            exclude_instructions: Vec::new(),
            targets: Vec::new(),
        }
    }
}

impl Manifest {
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join(MANIFEST_FILE);
        let text = fs::read_to_string(&path).at(&path)?;
        toml::from_str(&text).map_err(|e| Error::Parse { path, message: e.to_string() })
    }

    /// Looks up a profile; `default` is implicit if not declared.
    pub fn profile(&self, name: &str) -> Result<Profile> {
        match self.profiles.get(name) {
            Some(p) => Ok(p.clone()),
            None if name == "default" => Ok(Profile::default()),
            None => Err(Error::Invalid(format!("unknown profile `{name}`"))),
        }
    }
}

// Writes go through toml_edit so user comments and formatting survive.

pub(crate) fn write_import(root: &Path, name: &str, import: &Import) -> Result<()> {
    edit(root, |doc| {
        let imports = doc
            .entry("import")
            .or_insert_with(|| {
                let mut t = Table::new();
                t.set_implicit(true);
                Item::Table(t)
            })
            .as_table_mut()
            .ok_or_else(|| Error::Invalid("`import` in sk1ll.toml must be a table".into()))?;
        let mut t = Table::new();
        t["source"] = value(import.source.as_str());
        if !import.path.is_empty() {
            t["path"] = value(import.path.as_str());
        }
        t["ref"] = value(import.git_ref.as_str());
        imports.insert(name, Item::Table(t));
        Ok(())
    })
}

pub(crate) fn remove_import(root: &Path, name: &str) -> Result<()> {
    edit(root, |doc| {
        if let Some(t) = doc.get_mut("import").and_then(Item::as_table_mut) {
            t.remove(name);
        }
        Ok(())
    })
}

fn edit(root: &Path, f: impl FnOnce(&mut DocumentMut) -> Result<()>) -> Result<()> {
    let path = root.join(MANIFEST_FILE);
    let text = fs::read_to_string(&path).at(&path)?;
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e: toml_edit::TomlError| Error::Parse { path: path.clone(), message: e.to_string() })?;
    f(&mut doc)?;
    fs::write(&path, doc.to_string()).at(&path)
}
