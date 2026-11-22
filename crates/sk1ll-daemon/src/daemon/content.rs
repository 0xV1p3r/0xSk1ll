use super::*;
use std::collections::BTreeSet;
use std::sync::Arc;

use sk1ll_core::{ItemRef, Repo};

use crate::client::{RestoreRequest, RestoreResponse};
use crate::error::{Error, Result};

impl Daemon {
    /// Gives each item whose Item ID is a duplicate a new one, then reconciles.
    pub(crate) fn repair_ids(&self, eng: &mut Engine) -> Result<usize> {
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        let count = Repo::open(self.repo_root())?.repair_ids()?;
        self.reconcile(eng)?;
        Ok(count)
    }

    /// Restores draft items and reconciles.
    pub(crate) async fn restore(self: &Arc<Self>, req: RestoreRequest) -> Result<RestoreResponse> {
        let response = self.blocking(move |d, eng| d.restore_locally(eng, req)).await?;
        self.retry_stopped_pull().await;
        Ok(response)
    }

    pub(super) fn restore_locally(&self, eng: &mut Engine, req: RestoreRequest) -> Result<RestoreResponse> {
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        let repo = Repo::open_git_only(self.repo_root())?;
        let draft = repo.draft()?;
        let before: BTreeSet<ItemRef> = draft.iter().map(|d| d.item.clone()).collect();
        match (req.all, req.items.is_empty()) {
            (true, true) => repo.restore_all()?,
            (false, false) => {
                let items = req.items.iter().map(|i| ItemRef::resolve(i, &draft)).collect::<Result<Vec<_>, _>>()?;
                repo.restore(&items)?;
            }
            (true, false) => {
                return Err(Error::BadRequest("pass either items or all, not both".into()));
            }
            (false, true) => {
                return Err(Error::BadRequest("name the draft items to restore, or pass all".into()));
            }
        }
        let after: BTreeSet<ItemRef> = repo.draft()?.into_iter().map(|d| d.item).collect();
        let discarded: Vec<ItemRef> = before.difference(&after).cloned().collect();
        self.reconcile(eng)?;
        Ok(RestoreResponse { discarded })
    }

    pub(crate) fn write_skill_file(&self, eng: &mut Engine, req: &crate::client::WriteFileRequest) -> Result<()> {
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        if !sk1ll_core::content::write_file(&Repo::open(self.repo_root())?, req)? {
            return Err(Error::Conflict("File changed on disk".into()));
        }
        self.reconcile(eng).map(drop)
    }

    pub(crate) fn create_item(&self, eng: &mut Engine, req: crate::client::CreateItemRequest) -> Result<()> {
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        let repo = Repo::open(self.repo_root())?;
        match (&req.bundled, req.kind.as_str()) {
            (Some(bundled), "skill") => repo.add_bundled(bundled, &req.name)?,
            (Some(_), _) => return Err(Error::BadRequest("only skills are bundled".into())),
            (None, _) => repo.create_item(&req.kind, &req.name, req.description.as_deref().unwrap_or(""))?,
        }
        self.reconcile(eng)?;
        Ok(())
    }
    /// Restore brings the same Item ID and its toggles back.
    pub(crate) fn delete_item(&self, eng: &mut Engine, req: crate::client::DeleteItemRequest) -> Result<()> {
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        let deleted = Repo::open(self.repo_root())?.delete_item(&req.kind, &req.name);
        self.reconcile(eng)?;
        Ok(deleted?)
    }
    pub(crate) fn rename_item(
        &self,
        eng: &mut Engine,
        req: crate::client::RenameItemRequest,
    ) -> Result<crate::client::RenameItemResponse> {
        use sk1ll_core::repo::RenameError;
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        let repo = Repo::open(self.repo_root())?;
        // A hand-made item gets its Item ID first, so the rename keeps it.
        repo.assign_missing_ids()?;
        repo.check_rename(&req.kind, &req.name, &req.to)?;
        let item = if req.kind == "skill" { ItemRef::Skill(req.to.clone()) } else { ItemRef::Rule(req.to.clone()) };
        match repo.rename_item(&req.kind, &req.name, &req.to) {
            Ok(()) => {
                if let Err(e) = self.reconcile(eng) {
                    self.set_error(format!("reconciling after renaming {} to {item}: {e}", req.name));
                }
                Ok(crate::client::RenameItemResponse { item })
            }
            Err(RenameError::Moved(e)) => {
                let _ = self.reconcile(eng);
                Err(Error::Internal(anyhow::anyhow!(
                    "`{}` was renamed to `{}`, but updating its metadata failed: {e}",
                    req.name,
                    req.to
                )))
            }
            Err(e @ RenameError::Stranded { .. }) => {
                self.set_error(e.to_string());
                Err(Error::Conflict(e.to_string()))
            }
            Err(RenameError::NotMoved(e)) => Err(e.into()),
        }
    }
}
