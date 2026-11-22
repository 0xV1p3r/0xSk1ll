use super::*;
use std::sync::Arc;

use sk1ll_core::harness_rename::RenameIntent;
use sk1ll_proto::{AssignmentSnapshot, AssignmentUpdate};

use crate::client::Event;
use crate::error::{Error, Result};

impl Daemon {
    /// Renames a Harness with its Assignment. Joined, the server's
    /// Assignment is fetched and the rename written without the engine
    /// mutex; the intent and the machine config change under it.
    pub(crate) async fn rename_harness(
        self: &Arc<Self>,
        request: crate::client::HarnessRenameRequest,
    ) -> Result<crate::client::HarnessRenameResult> {
        if self.is_standalone() {
            return self.blocking(move |d, eng| d.rename_local_harness(eng, request)).await;
        }
        let _writes = self.assignment_writes.lock().await;
        let state = self.ident.joined()?.clone();
        let authority = state.rename_authority();
        let pending = self
            .blocking(move |d, eng| {
                d.ensure_managed()?;
                d.lock_repo(eng)?;
                Ok(RenameIntent::load(&d.cfg.machine_config, Some(&authority))?)
            })
            .await?;
        // A rename that is already pending is resumed, not prepared again.
        let authoritative = match pending {
            Some(intent) if intent.request == request => None,
            Some(_) => return Err(Error::Conflict("finish the pending Harness rename first".into())),
            None => Some(crate::server::fetch_machine_assignment(&state, &self.http).await?.assignment),
        };
        let (intent, world) = self
            .blocking(move |d, eng| {
                let world = d.world()?;
                d.lock_repo(eng)?;
                let intent = crate::server::prepare_rename(&d.cfg, &state, request, authoritative.as_ref())?;
                intent.apply_config(&d.cfg.machine_config)?;
                Ok((intent, world))
            })
            .await?;
        let result = crate::server::write_assignment(self, crate::server::rename_write(&intent)).await;
        let routed_rules = intent.routed_rules.clone();
        match self.blocking(move |d, eng| d.finish_harness_rename(eng, world, intent, result)).await {
            Ok(update) => Ok(crate::client::HarnessRenameResult { update, routed_rules }),
            Err(error) => {
                self.restart_assignment_session.notify_one();
                Err(error)
            }
        }
    }

    pub(super) fn rename_local_harness(
        &self,
        eng: &mut Engine,
        request: crate::client::HarnessRenameRequest,
    ) -> Result<crate::client::HarnessRenameResult> {
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        let config = &self.cfg.machine_config;
        let authoritative = sk1ll_core::Assignment::load(&self.cfg.assignment)?;
        let intent = RenameIntent::prepare(config, self.repo_root(), request, &authoritative, None)?;
        intent.apply_standalone(config, &self.cfg.assignment)?;
        let errors = self.saved_local_assignment(eng);
        let snap = self.applied().expect("standalone always has assignment");
        Ok(crate::client::HarnessRenameResult {
            update: AssignmentUpdate {
                revision: snap.revision,
                assignment: snap.assignment.clone(),
                entries: Vec::new(),
                errors,
            },
            routed_rules: intent.routed_rules,
        })
    }

    pub(crate) fn pending_harness_rename(&self, eng: &mut Engine) -> Result<Option<(RenameIntent, World)>> {
        let authority = self.ident.joined()?.rename_authority();
        let config = &self.cfg.machine_config;
        let Some(intent) = RenameIntent::load(config, Some(&authority))? else { return Ok(None) };
        let world = self.world()?;
        self.lock_repo(eng)?;
        intent.apply_config(config)?;
        Ok(Some((intent, world)))
    }

    /// Applies the server's answer to a rename sent from `world`. A daemon
    /// that was removed, or halted for a rejoin, since applies nothing and
    /// saves no snapshot: the state files may already be the next
    /// registration's.
    pub(crate) fn finish_harness_rename(
        &self,
        eng: &mut Engine,
        world: World,
        intent: RenameIntent,
        result: Result<AssignmentUpdate>,
    ) -> Result<AssignmentUpdate> {
        self.same_world(world)?;
        // If the process stops after clearing the intent, startup must
        // already have the Assignment for the new config.
        if let Ok(update) = &result
            && self.applied().is_none_or(|current| current.revision < update.revision)
        {
            crate::server::save_assignment_snapshot(&self.cfg, self.ident.joined()?, update)?;
        }
        let mut update = crate::server::resolve_rename(&self.cfg.machine_config, &intent, result)?;
        let snapshot =
            AssignmentSnapshot { revision: update.revision, assignment: update.assignment.clone(), registration: None };
        if self.applied().is_some_and(|current| current.revision >= snapshot.revision) {
            update.errors = self.reconcile(eng)?;
        } else {
            update.errors = self.apply_assignment(eng, snapshot)?;
        }
        self.event(Event::Fleet);
        Ok(update)
    }
}
