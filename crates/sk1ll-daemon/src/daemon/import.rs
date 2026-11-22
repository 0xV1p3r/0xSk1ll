use super::*;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use sk1ll_core::Repo;
use sk1ll_core::imports;

use crate::client::{
    AddImportRequest, ImportUpdated, ResolveImportRequest, ResolvedSource, SkillCandidate, UpdateImportsRequest,
};
use crate::error::{Error, Result};

impl Daemon {
    // Import operations fetch in two or three steps, so a slow or hanging
    // remote never holds the engine mutex: check what can be checked under
    // it, fetch into the import cache and stage the checkout without it (core
    // serializes fetches per cache repo; checkout may run smudge filters that
    // fetch), then take it again to re-check against the repo as it is now
    // and install the staged content: a rename, with no git or network.

    /// Re-vendors imports into the draft: `name`, or all of them.
    pub(crate) async fn update_imports(self: &Arc<Self>, req: UpdateImportsRequest) -> Result<Vec<ImportUpdated>> {
        let UpdateImportsRequest { name, force, expected } = req;
        let (root, todo) = self
            .blocking(move |d, _| {
                d.ensure_managed()?;
                let todo = Repo::open(d.repo_root())?.update_targets(name.as_deref(), force)?;
                Ok((d.repo_root().to_path_buf(), todo))
            })
            .await?;
        let fetched = unlocked(move || imports::fetch_updates(&root, todo)).await?;
        if let Some(expected) = &expected {
            fetched.check_commit(expected)?;
        }
        self.blocking(move |d, eng| {
            d.ensure_managed()?;
            d.lock_repo(eng)?;
            let reports = Repo::open(d.repo_root())?.apply_updates(fetched, force);
            d.reconcile(eng)?;
            Ok(reports?.into_iter().map(import_updated).collect())
        })
        .await
    }

    /// Re-vendors one import into the draft.
    pub(crate) async fn update_import(
        self: &Arc<Self>,
        name: String,
        force: bool,
        expected: Option<String>,
    ) -> Result<ImportUpdated> {
        let req = UpdateImportsRequest { name: Some(name), force, expected };
        self.update_imports(req).await?.into_iter().next().context("no update report").map_err(Error::Internal)
    }

    pub(crate) async fn import_changes(
        self: &Arc<Self>,
        name: String,
        upstream: Option<String>,
    ) -> Result<imports::ImportChanges> {
        let (root, targets) = self
            .blocking(move |d, _| {
                let root = d.import_root()?;
                let targets = Repo::open(&root)?.update_targets(Some(&name), true)?;
                Ok((root, targets))
            })
            .await?;
        unlocked(move || imports::fetch_changes(&root, targets, upstream.as_deref())).await
    }

    /// What a link leads to; only the import cache is written.
    pub(crate) async fn resolve_import(self: &Arc<Self>, req: ResolveImportRequest) -> Result<ResolvedSource> {
        let root = self.blocking(|d, _| d.import_root()).await?;
        let fetched = unlocked(move || {
            imports::fetch_link(&root, &req.link, given(&req.path), given(&req.git_ref), None)?.inspect()
        })
        .await?;
        // `taken` reflects the repo after the fetch, not before it.
        let r = self
            .blocking(move |d, _| {
                d.ensure_managed()?;
                Ok(Repo::open(d.repo_root())?.with_taken(fetched.resolved))
            })
            .await?;
        let candidates = r
            .candidates
            .into_iter()
            .map(|c| SkillCandidate {
                path: c.path,
                default_name: c.default_name,
                description: c.description,
                taken: c.taken,
                total_bytes: c.total_bytes,
                warnings: c.warnings,
            })
            .collect();
        Ok(ResolvedSource { source: r.source, git_ref: r.git_ref, commit: r.commit, candidates })
    }

    /// Adds an import, from a link or plain source, to the draft.
    pub(crate) async fn add_import(self: &Arc<Self>, req: AddImportRequest) -> Result<ImportUpdated> {
        let root = self.blocking(|d, _| d.import_root()).await?;
        let AddImportRequest { name, source, path, git_ref, commit } = req;
        // Staging runs checkout (and any smudge filter) here too, outside the mutex.
        let fetched = unlocked(move || {
            let git_ref = git_ref.as_deref().and_then(given);
            let mut fetched = imports::fetch_link(&root, &source, path.as_deref(), git_ref, commit.as_deref())?;
            fetched.stage()?;
            Ok(fetched)
        })
        .await?;
        self.blocking(move |d, eng| {
            d.ensure_managed()?;
            d.lock_repo(eng)?;
            let added = Repo::open(d.repo_root())?.add_fetched(given(&name), fetched);
            d.reconcile(eng)?;
            Ok(import_updated(added?))
        })
        .await
    }

    /// The repo an import operation fetches for, if the daemon manages it.
    pub(super) fn import_root(&self) -> Result<PathBuf> {
        self.ensure_managed()?;
        Ok(self.repo_root().to_path_buf())
    }

    /// Removes an import (a draft change).
    pub(crate) fn remove_import(&self, eng: &mut Engine, name: &str) -> Result<()> {
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        let removed = Repo::open(self.repo_root())?.remove_import(name);
        self.reconcile(eng)?;
        Ok(removed?)
    }

    /// Turns an import into a personal skill (a draft change).
    pub(crate) fn fork(&self, eng: &mut Engine, name: &str) -> Result<()> {
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        let forked = Repo::open(self.repo_root())?.fork(name);
        self.reconcile(eng)?;
        Ok(forked?)
    }

    pub(crate) async fn local_imports(self: &Arc<Self>, check: bool) -> Result<crate::client::ImportsView> {
        let (root, entries) = self
            .reading(|d| {
                let repo = Repo::open(d.repo_root())?;
                Ok((
                    repo.root().to_path_buf(),
                    repo.manifest
                        .import
                        .iter()
                        .map(|(name, i)| crate::client::ImportEntry {
                            name: name.clone(),
                            source: i.source.clone(),
                            git_ref: i.git_ref.clone(),
                            path: i.path.clone(),
                            locked: repo.lock.skills.get(name).map(|l| l.commit.clone()),
                        })
                        .collect::<Vec<_>>(),
                ))
            })
            .await?;
        tokio::task::spawn_blocking(move || {
            static CACHE: std::sync::OnceLock<
                std::sync::Mutex<std::collections::BTreeMap<PathBuf, crate::client::ImportsView>>,
            > = std::sync::OnceLock::new();
            let cache = CACHE.get_or_init(Default::default);
            let previous = cache.lock().unwrap().get(&root).cloned().unwrap_or_default();
            let mut view = crate::client::ImportsView { imports: entries, ..previous };
            if check {
                view.updates.clear();
                view.errors.clear();
                // This machine checks every import itself.
                view.skipped.clear();
            }
            if check {
                for i in &view.imports {
                    match sk1ll_core::imports::fetch_link(&root, &i.source, Some(&i.path), Some(&i.git_ref), None) {
                        Ok(f) => {
                            if let Some(locked) = &i.locked
                                && locked != &f.resolved.commit
                            {
                                view.updates.push(sk1ll_proto::ImportUpdate {
                                    name: i.name.clone(),
                                    source: i.source.clone(),
                                    git_ref: i.git_ref.clone(),
                                    locked: locked.clone(),
                                    upstream: f.resolved.commit,
                                });
                            }
                        }
                        Err(e) => {
                            view.errors.push(sk1ll_proto::ImportError { name: i.name.clone(), message: e.to_string() })
                        }
                    }
                }
                view.checked_at = Some(
                    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()
                        as i64,
                );
            }
            view.updates
                .retain(|u| view.imports.iter().any(|i| i.name == u.name && i.locked.as_ref() == Some(&u.locked)));
            view.errors.retain(|e| view.imports.iter().any(|i| i.name == e.name));
            cache.lock().unwrap().insert(root, view.clone());
            Ok(view)
        })
        .await
        .map_err(|e| Error::Internal(e.into()))?
    }
}

/// Runs `f` (an import's network phase) on a blocking thread without the
/// engine mutex.
pub(super) async fn unlocked<T: Send + 'static>(
    f: impl FnOnce() -> sk1ll_core::Result<T> + Send + 'static,
) -> Result<T> {
    let out = tokio::task::spawn_blocking(f).await;
    Ok(out.map_err(|e| Error::Internal(anyhow::anyhow!("import fetch task failed: {e}")))??)
}

/// An optional request field: empty means not given.
pub(super) fn given(s: &str) -> Option<&str> {
    Some(s).filter(|s| !s.is_empty())
}

pub(super) fn import_updated(r: sk1ll_core::UpdateReport) -> ImportUpdated {
    ImportUpdated { name: r.name, old_commit: r.old_commit, new_commit: r.new_commit, changed: r.changed }
}
