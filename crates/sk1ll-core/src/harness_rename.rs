//! Durable intent for the two writes involved in renaming a Harness.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config_file::{Snapshot, read_text, replace_private};
use crate::{Assignment, Error, IoExt, MachineConfig, Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenameRequest {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub replace: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RenameIntent {
    pub request: RenameRequest,
    pub operation_id: String,
    pub routed_rules: Vec<String>,
    authority: Option<String>,
    before_config: String,
    after_config: String,
    before_assignment: Option<Assignment>,
}

impl RenameIntent {
    /// Where the intent for the machine config at `config` is kept.
    pub fn path(config: &Path) -> PathBuf {
        config.with_extension("harness-rename.json")
    }

    pub fn exists(config: &Path) -> Result<bool> {
        let path = Self::path(config);
        match std::fs::symlink_metadata(&path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error).at(path),
        }
    }

    pub fn load(config: &Path, authority: Option<&str>) -> Result<Option<Self>> {
        let path = Self::path(config);
        let Some(text) = read_text(&path, false)? else { return Ok(None) };
        let intent: Self = serde_json::from_str(&text).map_err(|e| Error::Parse { path, message: e.to_string() })?;
        if intent.authority.as_deref() != authority {
            return Err(Error::Changed(
                "the pending Harness rename belongs to a different Assignment authority".into(),
            ));
        }
        Ok(Some(intent))
    }

    /// Record both possible file contents before either is changed.
    pub fn prepare(
        config: &Path,
        repo: &Path,
        request: RenameRequest,
        authoritative: &Assignment,
        authority: Option<&str>,
    ) -> Result<Self> {
        if let Some(intent) = Self::load(config, authority)? {
            if intent.request == request {
                return Ok(intent);
            }
            return Err(Error::Invalid(format!(
                "finish the pending Harness rename from `{}` to `{}` first",
                intent.request.from, intent.request.to
            )));
        }
        if authoritative.harnesses.contains_key(&request.to) && !request.replace {
            return Err(Error::Changed(format!(
                "harness `{}` already has an Assignment; use --replace to overwrite it",
                request.to
            )));
        }
        let before = Snapshot::take(config)?;
        let edit = MachineConfig::rename_harness(&before, &request.from, &request.to)?;
        let routed_rules = crate::instructions::discover(repo)?
            .into_iter()
            .filter(|rule| rule.harnesses.is_some() && rule.applies_to(&request.from) && !rule.applies_to(&request.to))
            .map(|rule| rule.name)
            .collect();
        let intent = Self {
            request,
            operation_id: ulid::Ulid::generate().to_string(),
            routed_rules,
            authority: authority.map(str::to_string),
            before_config: before.text()?,
            after_config: edit.text,
            before_assignment: authority.is_none().then(|| authoritative.clone()),
        };
        let body = serde_json::to_vec(&intent).expect("serializing Harness rename intent");
        let path = Self::path(config);
        if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).at(dir)?;
        }
        replace_private(&path, &body)?;
        Ok(intent)
    }

    pub fn apply_config(&self, config: &Path) -> Result<()> {
        self.set_config(config, &self.before_config, &self.after_config)
    }

    pub fn rollback_config(&self, config: &Path) -> Result<()> {
        self.set_config(config, &self.after_config, &self.before_config)?;
        self.finish(config)
    }

    fn set_config(&self, config: &Path, from: &str, to: &str) -> Result<()> {
        let file = Snapshot::take(config)?;
        let current = file.text()?;
        if current == to {
            return Ok(());
        }
        if current != from {
            return Err(Error::Changed(format!("{} changed during the pending Harness rename", config.display())));
        }
        file.replace(to)
    }

    /// The machine config at `config` once renamed, without writing it:
    /// the file must still be as the intent found it, or renamed already.
    pub fn renamed_config(&self, config: &Path) -> Result<MachineConfig> {
        let current = Snapshot::take(config)?.text()?;
        if current != self.before_config && current != self.after_config {
            return Err(Error::Changed(format!("{} changed during the pending Harness rename", config.display())));
        }
        MachineConfig::parse(&self.after_config, config)
    }

    /// The machine-local Assignment at `path` once renamed: the file must
    /// still be as the intent found it, or renamed already.
    pub fn renamed_assignment(&self, path: &Path) -> Result<Assignment> {
        Ok(self.renamed_from(path)?.2)
    }

    /// The Assignment file's exact bytes, what they hold and what the
    /// rename makes of it (see [`RenameIntent::renamed_assignment`]).
    fn renamed_from(&self, path: &Path) -> Result<(Option<Vec<u8>>, Assignment, Assignment)> {
        let before = self
            .before_assignment
            .as_ref()
            .ok_or_else(|| Error::Invalid("this Harness rename needs the Server".into()))?;
        let mut after = before.clone();
        after.rename_harness(&self.request.from, &self.request.to);
        let (bytes, current) = Assignment::observe(path)?;
        if current != *before && current != after {
            return Err(Error::Changed(format!("{} changed during the pending Harness rename", path.display())));
        }
        Ok((bytes, current, after))
    }

    /// Makes both writes of a machine-local rename and finishes it. The
    /// Assignment file is written only if it still holds what the rename
    /// was made from: a hand edit that landed meanwhile fails the rename,
    /// which stays pending, rather than being replaced.
    pub fn apply_standalone(&self, config: &Path, path: &Path) -> Result<()> {
        let (bytes, current, after) = self.renamed_from(path)?;
        self.apply_config(config)?;
        if current != after && !after.swap(path, bytes.as_deref())? {
            return Err(Error::Changed(format!("{} changed during the pending Harness rename", path.display())));
        }
        self.finish(config)
    }

    pub fn finish(&self, config: &Path) -> Result<()> {
        let path = Self::path(config);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).at(path),
        }
    }
}
