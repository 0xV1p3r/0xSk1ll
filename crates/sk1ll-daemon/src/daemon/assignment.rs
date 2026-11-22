use super::*;
use std::sync::Arc;

use sk1ll_core::harness_rename::RenameIntent;
use sk1ll_proto::{AssignmentSnapshot, AssignmentUpdate, AssignmentWrite};

use crate::client::Event;
use crate::error::{Error, Result};

impl Daemon {
    /// Under the engine mutex (and so the machine lock): finishes a
    /// machine-local Harness rename that stopped between its writes, then
    /// applies what changed where the Assignment lives without the daemon (a
    /// hand edit of the machine-local file), as a new revision readers see.
    /// It writes only that revision's record, by a compare-and-swap against
    /// the bytes it read, never the file it read it from (see
    /// [`crate::store::LocalAssignment::settle_after`]). Harnesses follow
    /// with the next reconcile.
    pub(crate) fn refresh_assignment(&self) -> Result<()> {
        self.require_writer()?;
        self.store.finish_interrupted_rename()?;
        let Some(local) = self.store.local() else { return Ok(()) };
        let applied = self.applied();
        let snap = local.settle_after(applied.as_deref())?;
        if applied.as_deref() != Some(&snap) {
            self.publish_assignment(snap);
        }
        Ok(())
    }

    /// Makes `snap` the applied Assignment and tells readers.
    fn publish_assignment(&self, snap: AssignmentSnapshot) {
        let revision = snap.revision;
        *self.applied.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(snap));
        self.update(|v| {
            v.assignment_revision = revision;
            v.awaiting_assignment = false;
        });
        self.event(Event::Assignment { revision });
    }

    pub(super) fn assignment_placement_outcome(&self, result: Result<Vec<String>>) -> Vec<String> {
        let errors = result.unwrap_or_else(|e| {
            let message = crate::auth::redact(&format!("applying the saved Assignment: {e}"));
            self.set_error(message.clone());
            vec![message]
        });
        errors.into_iter().map(|error| crate::auth::redact(&error)).collect()
    }

    /// Writes Assignment where it lives ([`crate::store`]). A machine-local
    /// write is made, saved and applied in one step under the engine mutex.
    /// For the server's, the engine mutex is held only to check whether it
    /// may be sent and, once confirmed, to apply it: the server may take up
    /// to its request timeout to confirm.
    pub(crate) async fn write_assignment(self: &Arc<Self>, write: AssignmentWrite) -> Result<AssignmentUpdate> {
        let _writes = self.assignment_writes.lock().await;
        if self.store.local().is_some() {
            return self
                .blocking(move |d, eng| d.store.local().expect("a machine-local store").write(d, eng, &write))
                .await;
        }
        let world = self
            .blocking(|d, _| {
                d.refresh_assignment()?;
                if RenameIntent::exists(&d.cfg.machine_config)? {
                    return Err(Error::Unavailable("waiting for the pending Harness rename to finish".into()));
                }
                d.world()
            })
            .await?;
        let update = self.store.send(self, write).await?;
        self.blocking(move |d, eng| Ok(d.apply_confirmed(eng, world, update))).await
    }

    /// Applies a confirmed Assignment, as of now rather than as of when it
    /// was sent. A daemon that stopped managing meanwhile applies nothing; a
    /// newer revision applied meanwhile (its snapshot overtook the
    /// confirmation) stands, and since revisions are numbered in order it
    /// already includes this write: the reply then carries that one.
    pub(super) fn apply_confirmed(
        &self,
        eng: &mut Engine,
        world: World,
        mut update: AssignmentUpdate,
    ) -> AssignmentUpdate {
        if let Err(why) = self.same_world(world) {
            tracing::info!("not applying Assignment revision {}: {why}", update.revision);
            return update;
        }
        let snapshot =
            AssignmentSnapshot { revision: update.revision, assignment: update.assignment.clone(), registration: None };
        update.errors = self.apply_assignment(eng, snapshot).unwrap_or_else(|e| {
            self.set_error(format!("applying the saved Assignment: {e}"));
            vec![crate::auth::redact(&e.to_string())]
        });
        if let Some(current) = self.applied()
            && current.revision > update.revision
        {
            tracing::info!(
                "Assignment revision {} was confirmed after revision {} had been applied",
                update.revision,
                current.revision
            );
            update.revision = current.revision;
            update.assignment = current.assignment.clone();
        }
        self.event(Event::Fleet);
        update
    }

    /// The Assignment as readers see it: the applied one, or one edited by
    /// hand since, which the next reconcile applies. Writes nothing.
    pub(super) fn current_assignment(&self) -> Result<sk1ll_core::Assignment> {
        let applied = self.applied();
        if let Some(snap) = self.store.reload(applied.as_deref())? {
            return Ok(snap.assignment);
        }
        applied.map(|snap| snap.assignment.clone()).ok_or_else(|| {
            Error::Conflict("waiting for the server's assignment; the harnesses are left alone until it arrives".into())
        })
    }

    /// What a preview plans with: the harnesses and the Assignment as the
    /// next reconcile would have them. A machine-local Harness rename that
    /// stopped between its writes is finished in memory only, as that
    /// reconcile will finish it on disk. Writes nothing.
    pub(super) fn preview_inputs(
        &self,
    ) -> Result<(std::collections::BTreeMap<String, sk1ll_core::Harness>, sk1ll_core::Assignment)> {
        let config = &self.cfg.machine_config;
        if let Some(file) = self.store.local().and(self.store.file())
            && let Some(intent) = RenameIntent::load(config, None)?
        {
            return Ok((intent.renamed_config(config)?.harnesses(), intent.renamed_assignment(file)?));
        }
        Ok((sk1ll_core::MachineConfig::load(config)?.harnesses(), self.current_assignment()?))
    }

    /// Under the engine mutex: makes `snap`, already saved where the
    /// Assignment lives, the applied one (`None`: the applied one stays) and
    /// brings the harnesses in line with it. Returns what failed to place.
    pub(crate) fn install_assignment(&self, eng: &mut Engine, snap: Option<AssignmentSnapshot>) -> Vec<String> {
        if let Some(snap) = snap {
            tracing::info!("applied assignment revision {}", snap.revision);
            self.publish_assignment(snap);
        }
        let result = self.reconcile(eng);
        let errors = self.assignment_placement_outcome(result);
        self.event(Event::Fleet);
        errors
    }

    /// Stores `snap` and syncs, unless it is older than what was applied. With
    /// no stored snapshot, any snapshot is taken (also one of the same
    /// revision as before), since the harnesses wait for it.
    pub(crate) fn apply_assignment(&self, eng: &mut Engine, snap: AssignmentSnapshot) -> Result<Vec<String>> {
        if self.idle() {
            return Ok(Vec::new());
        }
        if let Some(have) = self.applied()
            && !newer(have.revision, snap.revision)
        {
            // The same revision arrives on every connect; only an older one is news.
            if snap.revision < have.revision {
                tracing::info!("ignoring stale assignment revision {} (have {})", snap.revision, have.revision);
            }
            // A placement that failed for it is tried again, if anything
            // changed since or it failed: the reconcile knows.
            if snap.revision == have.revision {
                let result = self.reconcile(eng);
                return Ok(self.assignment_placement_outcome(result));
            }
            return Ok(Vec::new());
        }
        self.store.save(&snap)?;
        tracing::info!("applied assignment revision {}", snap.revision);
        self.publish_assignment(snap);
        let result = self.reconcile(eng);
        Ok(self.assignment_placement_outcome(result))
    }
}

/// Whether a snapshot at `incoming` replaces one at `applied`.
pub(crate) fn newer(applied: u64, incoming: u64) -> bool {
    incoming > applied
}
