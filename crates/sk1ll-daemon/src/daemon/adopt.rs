use super::*;
use std::path::PathBuf;
use std::sync::Arc;

use sk1ll_core::sync::{self, Action, ActionKind};
use sk1ll_core::{ItemRef, MachineConfig, Repo};
use sk1ll_proto::{AssignmentUpdate, AssignmentWrite};

use crate::client::{ConflictTarget, Event};
use crate::error::{Error, Result};

impl Daemon {
    pub(crate) fn resolve_displaced(&self, eng: &mut Engine, id: &str, adopt: bool) -> Result<Option<PathBuf>> {
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        let result = if adopt {
            let root = self.repo_root();
            let lock = eng.lock.as_ref().ok_or_else(|| Error::Conflict(format!("{} is not locked", root.display())))?;
            sync::displaced::adopt(&Repo::open_git_only(root)?, id, lock).map(Some)
        } else {
            sync::displaced::discard_from(&Repo::open_git_only(self.repo_root())?, id).map(|()| None)
        };
        self.reconcile(eng)?;
        self.event(Event::Status);
        if adopt {
            self.event(Event::Draft);
        }
        Ok(result?)
    }

    pub(super) fn find_conflict<'a>(actions: &'a [Action], t: &ConflictTarget) -> Result<&'a Action> {
        actions.iter().find(|a| a.kind == ActionKind::Conflict && matches_target(a, t)).ok_or_else(|| {
            Error::NotFound(format!("no conflict for `{}` in `{}` (see GET /conflicts)", t.name, t.harness))
        })
    }

    /// Takes the harness's content into the draft. Adopting an unmanaged
    /// skill also enables it where it was found: the Assignment write runs
    /// without the engine mutex, between publishing the skill and removing
    /// the copies it came from.
    pub(crate) async fn adopt(self: &Arc<Self>, t: ConflictTarget) -> Result<()> {
        let Some(pending) = self.blocking(move |d, eng| d.adopt_locally(eng, t)).await? else {
            self.retry_stopped_pull().await;
            return Ok(());
        };
        let write = AssignmentWrite::Changes(pending.changes.clone());
        let written = self.write_assignment(write).await;
        self.blocking(move |d, eng| d.finish_adoption(eng, pending, written)).await
    }

    /// Adopts a conflict (`None`), or publishes an unmanaged skill into the
    /// draft and returns the Assignment write that enables it.
    pub(super) fn adopt_locally(&self, eng: &mut Engine, t: ConflictTarget) -> Result<Option<PendingAdoption>> {
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        let repo = Repo::open(self.repo_root())?;
        let harnesses = MachineConfig::load(&self.cfg.machine_config)?.harnesses();
        let unmanaged = sync::unmanaged::discover(&repo, &harnesses)?
            .iter()
            .any(|s| s.name == t.name && s.variants.iter().any(|v| v.locations.iter().any(|l| l.harness == t.harness)));
        let adopted = if unmanaged { None } else { sync::unmanaged::adopted(&repo, &harnesses, &t.harness, &t.name)? };
        if !unmanaged && adopted.is_none() {
            let (repo, actions) = self.plan(false)?;
            sync::adopt(&repo, Self::find_conflict(&actions, &t)?)?;
            tracing::info!("adopted {} from {} into the draft", t.name, t.harness);
            self.reconcile(eng)?;
            return Ok(None);
        }
        if !self.store.writable(self) {
            return Err(Error::Unavailable("Adopt needs a connection to the Server".into()));
        }
        // A pending pull's tree is partial: nothing is published into it.
        repo.require_no_pending_pull()?;
        let world = self.world()?;
        let adoption = match adopted {
            Some(adoption) => adoption,
            None => sync::unmanaged::select(&repo, &harnesses, &t.harness, &t.name)?,
        };
        let created = repo.personal_dir().join(&adoption.name).symlink_metadata().is_err();
        sync::unmanaged::publish(&repo, &adoption)?;
        // Its toggles are keyed by Item ID: one published before (by
        // hand, or by an interrupted Adopt) may still lack it.
        if !repo.pull_pending()? {
            repo.assign_missing_ids()?;
        }
        let id = Self::adopted_id(&repo, &adoption)?
            .ok_or_else(|| Error::Conflict(format!("Adopt did not give skill:{} an Item ID", adoption.name)))?;
        let changes = sk1ll_proto::AssignmentChanges {
            changes: adoption
                .locations
                .iter()
                .map(|location| sk1ll_proto::ToggleChange {
                    harness: location.harness.clone(),
                    kind: sk1ll_proto::ToggleKind::Skill,
                    item_id: Some(id.clone()),
                    name: Some(adoption.name.clone()),
                    enabled: Some(true),
                })
                .collect(),
            ..Default::default()
        };
        // Publish the draft before enabling it. On a lost reply it stays
        // available to retry with the same ID. Original copies stay until
        // the Assignment has been confirmed.
        self.reconcile(eng)?;
        Ok(Some(PendingAdoption { adoption, created, id, changes, world }))
    }

    /// The Item ID of an adopted skill as the repo has it now.
    pub(super) fn adopted_id(repo: &Repo, adoption: &sync::unmanaged::Adoption) -> Result<Option<sk1ll_core::ItemId>> {
        let item = ItemRef::Skill(adoption.name.clone());
        repo.item_ids()?
            .into_iter()
            .find_map(|(name, id)| (name == item).then_some(id))
            .ok_or_else(|| Error::NotFound(format!("Adopt did not create {item}")))
    }

    /// After the Assignment write: removes the copies an enabled adoption
    /// came from, or rolls back a refused one, as of now. Nothing is done
    /// once the daemon stopped managing the registration it wrote to. A
    /// rollback keeps the skill if it was committed meanwhile or no longer
    /// has its Item ID. If the published skill no longer has the Item ID that
    /// was enabled, or a pull is pending (sync is paused), the copies stay
    /// where they are: adopt it again (once the pull has finished).
    pub(super) fn finish_adoption(
        &self,
        eng: &mut Engine,
        pending: PendingAdoption,
        written: Result<AssignmentUpdate>,
    ) -> Result<()> {
        let PendingAdoption { mut adoption, created, id, world, .. } = pending;
        self.same_world(world)?;
        self.lock_repo(eng)?;
        let repo = Repo::open(self.repo_root())?;
        if let Err(error) = written {
            let kept = if created && matches!(error, Error::Upstream(_, _)) {
                match sync::unmanaged::rollback(&repo, &adoption, &id)? {
                    sync::unmanaged::RolledBack::Kept(why) => Some(why),
                    _ => None,
                }
            } else {
                None
            };
            self.reconcile(eng)?;
            self.event(Event::Draft);
            return Err(match (kept, error) {
                (Some(why), Error::Upstream(status, message)) => {
                    Error::Upstream(status, format!("{message}; skill:{} stays in the Draft: {why}", adoption.name))
                }
                (_, error) => error,
            });
        }
        if Self::adopted_id(&repo, &adoption).ok().flatten().as_ref() != Some(&id) {
            self.reconcile(eng)?;
            return Err(Error::Conflict(format!(
                "skill:{} changed while its Assignment was being written; its copies in the harnesses are kept, \
                 adopt it again",
                adoption.name
            )));
        }
        if let Err(why) = self.same_world_settled(world) {
            self.reconcile(eng)?;
            self.event(Event::Fleet);
            return Err(Error::Conflict(format!(
                "skill:{} is enabled, but its copies in the harnesses are kept ({why}); adopt it again once the \
                 pull has finished",
                adoption.name
            )));
        }
        let harnesses = MachineConfig::load(&self.cfg.machine_config)?.harnesses();
        let snapshot = self.applied();
        adoption.locations.retain(|l| snapshot.as_ref().is_some_and(|s| s.assignment.skill_enabled(&l.harness, &id)));
        sync::unmanaged::complete(&repo, &harnesses, &adoption)?;
        self.reconcile(eng)?;
        self.event(Event::Draft);
        self.event(Event::Fleet);
        Ok(())
    }

    /// Replaces the harness's content with what the draft says: a forced plan,
    /// applied for this one item only.
    pub(crate) fn overwrite(&self, eng: &mut Engine, t: ConflictTarget) -> Result<()> {
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        let (_, actions) = self.plan(false)?;
        let conflict = Self::find_conflict(&actions, &t)?;
        let (repo, forced) = self.plan(true)?;
        let fixes: Vec<&Action> =
            forced.iter().filter(|a| a.kind != ActionKind::Conflict && matches_target(a, &t)).collect();
        if fixes.is_empty() {
            let why = conflict.reason.as_deref().unwrap_or("not managed by sk1ll");
            return Err(Error::Conflict(format!(
                "`{}` in `{}` can't be overwritten ({why}); sk1ll never replaces content it doesn't manage",
                t.name, t.harness
            )));
        }
        for a in fixes {
            sync::apply(&repo, a)?;
        }
        tracing::info!("overwrote {} in {}", t.name, t.harness);
        self.reconcile(eng).map(drop)
    }
}

/// An unmanaged skill published into the draft, waiting for the Assignment
/// write that enables it (see [`Daemon::adopt`]).
pub(super) struct PendingAdoption {
    pub(super) adoption: sync::unmanaged::Adoption,
    /// Publishing created it, so a refused write takes it back out.
    pub(super) created: bool,
    pub(super) id: sk1ll_core::ItemId,
    pub(super) changes: sk1ll_proto::AssignmentChanges,
    /// What the write's result is applied in (see [`Daemon::same_world`]).
    pub(super) world: World,
}
