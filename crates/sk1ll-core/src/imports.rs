use std::fs;

use serde::Serialize;

use crate::hash::hash_dir;
use crate::lock::LockEntry;
use crate::manifest::{self, Import};
use crate::{Error, IoExt, Repo, Result, source};

#[derive(Debug, Clone, Serialize)]
pub struct UpdateReport {
    pub name: String,
    pub old_commit: Option<String>,
    pub new_commit: String,
    pub changed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ImportState {
    /// Vendored content matches the lockfile.
    Clean,
    /// Vendored content was edited locally; `update` would overwrite it.
    Modified,
    /// Declared in the manifest but not vendored yet.
    Missing,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportStatus {
    pub name: String,
    pub state: ImportState,
    pub commit: Option<String>,
}

impl Repo {
    /// Fetches and vendors a new import, then records it in manifest + lock.
    pub fn add_import(&mut self, name: &str, import: Import) -> Result<UpdateReport> {
        validate_name(name)?;
        if self.manifest.import.contains_key(name) {
            return Err(Error::Invalid(format!("import `{name}` already exists")));
        }
        if self.personal_dir().join(name).exists() {
            return Err(Error::Invalid(format!("a personal skill named `{name}` already exists")));
        }
        let report = self.vendor(name, &import)?;
        manifest::write_import(self.root(), name, &import)?;
        self.manifest.import.insert(name.to_string(), import);
        Ok(report)
    }

    /// Re-fetches imports (all, or one by name). Refuses to clobber local edits
    /// unless `force` is set.
    pub fn update(&mut self, only: Option<&str>, force: bool) -> Result<Vec<UpdateReport>> {
        if let Some(name) = only
            && !self.manifest.import.contains_key(name)
        {
            return Err(Error::Invalid(format!("no import named `{name}`")));
        }
        let todo: Vec<(String, Import)> = self
            .manifest
            .import
            .iter()
            .filter(|(n, _)| only.is_none_or(|o| o == n.as_str()))
            .map(|(n, i)| (n.clone(), i.clone()))
            .collect();
        if !force {
            let modified: Vec<_> = self
                .import_status()?
                .into_iter()
                .filter(|s| s.state == ImportState::Modified && todo.iter().any(|(n, _)| *n == s.name))
                .map(|s| s.name)
                .collect();
            if !modified.is_empty() {
                return Err(Error::Invalid(format!(
                    "locally modified imports: {} (use `fork` to keep edits, or force to overwrite)",
                    modified.join(", ")
                )));
            }
        }
        todo.iter().map(|(name, import)| self.vendor(name, import)).collect()
    }

    pub fn import_status(&self) -> Result<Vec<ImportStatus>> {
        self.manifest
            .import
            .keys()
            .map(|name| {
                let dir = self.imported_dir().join(name);
                let lock = self.lock.skills.get(name);
                let state = match lock {
                    _ if !dir.is_dir() => ImportState::Missing,
                    None => ImportState::Missing,
                    Some(entry) if hash_dir(&dir)? == entry.hash => ImportState::Clean,
                    Some(_) => ImportState::Modified,
                };
                Ok(ImportStatus { name: name.clone(), state, commit: lock.map(|e| e.commit.clone()) })
            })
            .collect()
    }

    /// Deletes an import from manifest, lock and disk.
    pub fn remove_import(&mut self, name: &str) -> Result<()> {
        if self.manifest.import.remove(name).is_none() {
            return Err(Error::Invalid(format!("no import named `{name}`")));
        }
        manifest::remove_import(self.root(), name)?;
        self.lock.skills.remove(name);
        self.lock.save(self.root())?;
        let dir = self.imported_dir().join(name);
        if dir.exists() {
            fs::remove_dir_all(&dir).at(&dir)?;
        }
        Ok(())
    }

    /// Turns an import into a personal skill you own and edit freely.
    pub fn fork(&mut self, name: &str) -> Result<()> {
        let from = self.imported_dir().join(name);
        let to = self.personal_dir().join(name);
        if !from.is_dir() {
            return Err(Error::Invalid(format!("no vendored import named `{name}`")));
        }
        if to.exists() {
            return Err(Error::Invalid(format!("personal skill `{name}` already exists")));
        }
        fs::rename(&from, &to).at(&to)?;
        self.manifest.import.remove(name);
        manifest::remove_import(self.root(), name)?;
        self.lock.skills.remove(name);
        self.lock.save(self.root())
    }

    fn vendor(&mut self, name: &str, import: &Import) -> Result<UpdateReport> {
        let dest = self.imported_dir().join(name);
        let commit = source::fetch_into(import, &dest)?;
        let hash = hash_dir(&dest)?;
        let old = self.lock.skills.insert(
            name.to_string(),
            LockEntry {
                source: import.source.clone(),
                path: import.path.clone(),
                git_ref: import.git_ref.clone(),
                commit: commit.clone(),
                hash: hash.clone(),
            },
        );
        self.lock.save(self.root())?;
        let changed = old.as_ref().is_none_or(|o| o.hash != hash);
        Ok(UpdateReport { name: name.to_string(), old_commit: old.map(|o| o.commit), new_commit: commit, changed })
    }
}

fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok { Ok(()) } else { Err(Error::Invalid(format!("invalid skill name `{name}`"))) }
}
