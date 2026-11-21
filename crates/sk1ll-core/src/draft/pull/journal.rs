//! The journal of a pull that is changing the working tree: written (and
//! synced) before the first file changes, removed once the branch has
//! moved. It records everything the next pull needs to finish the job, so
//! nothing of it is only in memory.
//!
//! While a journal exists, the ref [`ANCHOR`] points at its target, so that
//! git never prunes the replayed commits and merged blobs only that target
//! reaches.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::super::{quote_path, unquote_path};
use super::{Change, Entry, Pull};
use crate::git::path_bytes;
use crate::{Error, IoExt, Result};

/// The journal's file name in the worktree's git directory.
const FILE: &str = "sk1ll-pull.json";

/// Keeps the target's objects while a journal exists; per worktree.
const ANCHOR: &str = "refs/worktree/sk1ll-pull";

/// Where the journal is, in the worktree's git directory `git_dir`.
pub(super) fn path(git_dir: &Path) -> PathBuf {
    git_dir.join(FILE)
}

/// Drops an [`ANCHOR`] a pull left behind with no journal (it stopped
/// between removing the one and the other). Best effort.
pub(super) fn forget_anchor(pull: &Pull) {
    if super::rev(pull.git(), ANCHOR).ok().flatten().is_some() {
        let _ = pull.git().args(["update-ref", "-d", ANCHOR]).output();
    }
}

/// A pull of `branch` from `old` to `target`: `changes` are the paths that
/// differ between them.
#[derive(Debug)]
pub(super) struct Journal {
    pub(super) branch: String,
    pub(super) old: String,
    pub(super) target: String,
    /// The remote commit `target` is built on, for a Blocked outcome.
    pub(super) baseline: String,
    pub(super) changes: Vec<Change>,
    pub(super) operations: BTreeMap<usize, Operation>,
}

/// [`Journal`] as stored. Paths are git-quoted, so any bytes round-trip.
#[derive(Serialize, Deserialize)]
struct Stored {
    branch: String,
    old: String,
    target: String,
    baseline: String,
    changes: Vec<StoredChange>,
    operations: BTreeMap<usize, Operation>,
}

#[derive(Serialize, Deserialize)]
struct StoredChange {
    path: String,
    old: Option<Entry>,
    new: Option<Entry>,
}

/// A prepared operation, recorded before its first worktree mutation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) enum Operation {
    Add,
    Remove { recovery: String },
    Replace { recovery: String, staged: Inode },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Inode {
    pub(super) device: u64,
    pub(super) number: u64,
}

impl Journal {
    pub(super) fn load(pull: &Pull) -> Result<Option<Self>> {
        let path = path(&pull.git_dir);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).at(&path),
        };
        let parse = |message: String| Error::Parse { path: path.clone(), message };
        let stored: Stored = serde_json::from_str(&text).map_err(|e| parse(e.to_string()))?;
        let changes = stored
            .changes
            .into_iter()
            .map(|c| {
                let raw = unquote_path(&c.path).ok_or_else(|| parse(format!("invalid path {}", c.path)))?;
                Ok(Change { path: crate::git::path_from_bytes(&raw), old: c.old, new: c.new })
            })
            .collect::<Result<_>>()?;
        let Stored { branch, old, target, baseline, operations, .. } = stored;
        Ok(Some(Self { branch, old, target, baseline, changes, operations }))
    }

    /// Anchors the target, then writes the journal where none is: another
    /// pull's journal is never replaced.
    pub(super) fn write(&self, pull: &Pull) -> Result<()> {
        pull.git().args(["update-ref", "-m", "sk1ll: pull", ANCHOR, &self.target]).run()?;
        let body = self.body()?;
        let path = path(&pull.git_dir);
        let tmp = pull.git_dir.join(format!("{FILE}.{}", std::process::id()));
        crate::config_file::write_new(&tmp, &body)?;
        let linked = fs::hard_link(&tmp, &path);
        let _ = fs::remove_file(&tmp);
        match linked {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(Error::Invalid(format!("another pull is in progress ({} exists)", path.display())));
            }
            Err(e) => return Err(e).at(&path),
        }
        sync_dir(&pull.git_dir)
    }

    fn body(&self) -> Result<Vec<u8>> {
        let stored = Stored {
            branch: self.branch.clone(),
            old: self.old.clone(),
            target: self.target.clone(),
            baseline: self.baseline.clone(),
            operations: self.operations.clone(),
            changes: self
                .changes
                .iter()
                .map(|c| StoredChange {
                    path: quote_path(&path_bytes(&c.path)),
                    old: c.old.clone(),
                    new: c.new.clone(),
                })
                .collect(),
        };
        serde_json::to_vec_pretty(&stored).map_err(|e| Error::Invalid(e.to_string()))
    }

    pub(super) fn record(&mut self, pull: &Pull, index: usize, operation: Operation) -> Result<()> {
        self.operations.insert(index, operation);
        crate::config_file::replace_private(&path(&pull.git_dir), &self.body()?)
    }

    /// Removes the journal, then the anchor. Once the journal is gone, a
    /// leftover anchor is only dropped later (see [`forget_anchor`]).
    pub(super) fn remove(&self, pull: &Pull) -> Result<()> {
        let path = path(&pull.git_dir);
        match fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e).at(&path),
            _ => {}
        }
        sync_dir(&pull.git_dir)?;
        forget_anchor(pull);
        Ok(())
    }

    /// The order pull goes through the changes in, as indices: removals
    /// first, deepest first, so a file can make way for a directory, then
    /// the rest in path order, parents before what goes in them.
    pub(super) fn order(&self) -> Vec<usize> {
        let mut removed: Vec<usize> = (0..self.changes.len()).filter(|i| self.changes[*i].new.is_none()).collect();
        let mut rest: Vec<usize> = (0..self.changes.len()).filter(|i| self.changes[*i].new.is_some()).collect();
        removed.sort_by(|a, b| self.changes[*b].path.cmp(&self.changes[*a].path));
        rest.sort_by(|a, b| self.changes[*a].path.cmp(&self.changes[*b].path));
        removed.extend(rest);
        removed
    }

    /// Whether the pull changes any `.gitattributes` file.
    pub(super) fn changes_attributes(&self) -> bool {
        self.changes.iter().any(|c| c.path.file_name().is_some_and(|name| name == ".gitattributes"))
    }
}

/// Makes a rename or removal in `dir` durable.
fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::File::open(dir).and_then(|d| d.sync_all()).at(dir)?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}
