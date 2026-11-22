use super::*;
use std::collections::BTreeMap;
use std::path::Path;

use sk1ll_core::sync::{self, Action, ActionKind, PlanOptions};
use sk1ll_core::{MachineConfig, Repo};

use crate::client::{Event, InstructionPreview, SyncResult};
use crate::error::{Error, Result};
use sk1ll_core::git::plumbing;

impl Daemon {
    /// The current plan; `force` plans as if overwriting. For engine
    /// operations: interrupted imports are put back first.
    pub(super) fn plan(&self, force: bool) -> Result<(Repo, Vec<Action>)> {
        let Some(snap) = self.applied() else {
            return Err(Error::Conflict(
                "waiting for the server's assignment; the harnesses are left alone until it arrives".into(),
            ));
        };
        Self::recover_imports(self.repo_root())?;
        let repo = Repo::open(self.repo_root())?;
        Self::require_sync_ready(&repo)?;
        let harnesses = MachineConfig::load(&self.cfg.machine_config)?.harnesses();
        let actions = sync::plan(&repo, &harnesses, &snap.assignment, PlanOptions { force })?;
        Ok((repo, actions))
    }

    /// Puts back imports an interrupted (or failed, unrolled-back) update
    /// left set aside, before anything plans a sync: otherwise the sync would
    /// remove their harness entries. Failing to stops the sync. It changes
    /// the repo: only with the engine mutex and the repo's engine lock held.
    pub(super) fn recover_imports(root: &Path) -> Result<()> {
        for name in sk1ll_core::source::recover_replaced(root)? {
            tracing::warn!("restored import {name}, which an interrupted update had set aside");
        }
        Ok(())
    }

    pub(crate) fn preview_sync(&self, force: bool) -> Result<SyncResult> {
        if self.view().revoked {
            return Err(Error::Conflict(
                "this machine was removed from the fleet; the daemon no longer manages it".into(),
            ));
        }
        let repo = Repo::open(self.repo_root())?;
        Self::require_sync_ready(&repo)?;
        let harnesses = MachineConfig::load(&self.cfg.machine_config)?.harnesses();
        let actions = sync::plan(&repo, &harnesses, &self.current_assignment()?, PlanOptions { force })?;
        let sources = Self::sync_source_digests(&actions, sync::deploy_hash)?;
        Self::sync_preview_result(&actions, &sources)
    }

    pub(super) fn require_sync_ready(repo: &Repo) -> Result<()> {
        if repo.pull_pending()? {
            return Err(Error::Conflict("pull pending. Finish `sk1ll pull` before syncing harnesses".into()));
        }
        Ok(())
    }

    pub(super) fn sync_source_digests(
        actions: &[Action],
        mut hash: impl FnMut(&Path) -> sk1ll_core::Result<String>,
    ) -> Result<Vec<Option<String>>> {
        let mut digests = BTreeMap::new();
        Ok(actions
            .iter()
            .map(|action| match (&action.kind, &action.item) {
                (
                    ActionKind::Create | ActionKind::Update,
                    sync::Item::Skill { mode: sk1ll_core::LinkMode::Copy, source: Some(path), .. },
                ) => Some(path),
                _ => None,
            })
            .map(|path| {
                path.map(|path| {
                    use std::collections::btree_map::Entry;
                    let digest = match digests.entry(path) {
                        Entry::Occupied(entry) => entry.into_mut(),
                        Entry::Vacant(entry) => entry.insert(hash(path)?),
                    };
                    Ok(digest.clone())
                })
                .transpose()
            })
            .collect::<sk1ll_core::Result<Vec<_>>>()?)
    }

    pub(super) fn sync_preview_result(actions: &[Action], sources: &[Option<String>]) -> Result<SyncResult> {
        let serialized = serde_json::to_string(&(actions, sources)).map_err(|e| Error::Internal(e.into()))?;
        Ok(SyncResult {
            fingerprint: Some(sk1ll_core::hash::hash_str(&serialized)),
            conflicts: actions.iter().filter(|a| a.kind == ActionKind::Conflict).map(conflict_view).collect(),
            actions: Some(actions.iter().map(placement_view).collect()),
            completed: Vec::new(),
            changed: None,
            errors: Vec::new(),
        })
    }

    pub(crate) fn render_instructions(&self, name: &str) -> Result<Vec<InstructionPreview>> {
        let repo = Repo::open(self.repo_root())?;
        let harnesses = MachineConfig::load(&self.cfg.machine_config)?.harnesses();
        let files = sync::render_instructions(&repo, &harnesses, &self.current_assignment()?, name)?;
        let format = harnesses[name].instructions_format;
        files
            .into_iter()
            .map(|file| {
                let (before, next_content) = sync::instruction_file_text(&file, format)?;
                let diff = plumbing::diff_text(before.as_deref(), &next_content)?;
                Ok(InstructionPreview {
                    path: file.path,
                    format,
                    content: file.content,
                    differs: before.as_deref() != Some(&next_content),
                    next_content,
                    diff,
                })
            })
            .collect()
    }

    /// `POST /sync`: re-syncs now; `force` also overwrites what a forced plan
    /// may overwrite (hand edits inside managed blocks).
    pub(crate) fn sync(&self, eng: &mut Engine, force: bool, fingerprint: Option<&str>) -> Result<SyncResult> {
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        Self::require_sync_ready(&Repo::open_git_only(self.repo_root())?)?;
        let reviewed = if let Some(fingerprint) = fingerprint {
            self.refresh_assignment()?;
            let snapshot =
                self.applied().ok_or_else(|| Error::Conflict("waiting for the server's assignment".into()))?;
            let root = self.repo_root();
            let lock = eng.lock.as_ref().ok_or_else(|| Error::Conflict(format!("{} is not locked", root.display())))?;
            let harnesses = MachineConfig::load(&self.cfg.machine_config)?.harnesses();
            // Recovery may restore a source or change a placement. Compare the
            // plan for the recovered state, before applying any placements.
            sync::recover(Some(lock), &harnesses)?;
            Self::recover_imports(root)?;
            let repo = Repo::open(root)?;
            repo.assign_missing_ids()?;
            let actions = sync::plan(&repo, &harnesses, &snapshot.assignment, PlanOptions { force })?;
            let sources = Self::sync_source_digests(&actions, sync::deploy_hash)?;
            let preview = Self::sync_preview_result(&actions, &sources)?;
            if preview.fingerprint.as_deref() != Some(fingerprint) {
                return Ok(preview);
            }
            #[cfg(test)]
            if let Some(after_review) = eng.after_sync_review.take() {
                after_review();
            }
            Some((repo, actions.into_iter().zip(sources).collect()))
        } else {
            None
        };
        if force && reviewed.is_none() {
            Repo::open(self.repo_root())?.assign_missing_ids()?;
            let (repo, actions) = self.plan(true)?;
            for a in actions.iter().filter(|a| a.kind != ActionKind::Conflict) {
                sync::apply(&repo, a)?;
            }
        }
        let ReconcileOutcome { errors, completed, changed } = self.reconcile_with(eng, reviewed)?;
        self.event(Event::Status);
        if changed.is_some() {
            let mut preview = self.preview_sync(force)?;
            preview.errors = errors;
            preview.completed = completed;
            preview.changed = changed;
            return Ok(preview);
        }
        Ok(SyncResult {
            fingerprint: None,
            conflicts: self.snapshot().conflicts()?,
            actions: None,
            errors,
            completed: Vec::new(),
            changed: None,
        })
    }
}

pub(super) type ReviewedSync = (Repo, Vec<(Action, Option<String>)>);
