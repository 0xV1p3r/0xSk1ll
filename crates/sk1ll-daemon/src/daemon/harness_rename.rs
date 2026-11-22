use super::*;
use std::path::Path;
use std::sync::Arc;

use axum::http::StatusCode;
use sk1ll_core::harness_rename::{RenameIntent, RenameRequest};
use sk1ll_proto::{AssignmentSnapshot, AssignmentUpdate, AssignmentWrite};

use crate::client::Event;
use crate::error::{Error, Result};

impl Daemon {
    /// Renames a Harness with its Assignment, in two writes a durable
    /// intent ties together: the machine config under the engine mutex, and
    /// the Assignment where it lives ([`crate::store`]), without it.
    pub(crate) async fn rename_harness(
        self: &Arc<Self>,
        request: crate::client::HarnessRenameRequest,
    ) -> Result<crate::client::HarnessRenameResult> {
        let _writes = self.assignment_writes.lock().await;
        let authority = self.store.rename_authority();
        let pending = self
            .blocking({
                let authority = authority.clone();
                move |d, eng| {
                    d.ensure_managed()?;
                    d.lock_repo(eng)?;
                    d.refresh_assignment()?;
                    Ok(RenameIntent::load(&d.cfg.machine_config, authority.as_deref())?)
                }
            })
            .await?;
        // A rename that is already pending is resumed, not prepared again.
        let authoritative = match pending {
            Some(intent) if intent.request == request => None,
            Some(_) => return Err(Error::Conflict("finish the pending Harness rename first".into())),
            None => Some(self.store.get(self).await?.assignment),
        };
        let (intent, world) = self
            .blocking(move |d, eng| {
                let world = d.world()?;
                d.lock_repo(eng)?;
                let config = &d.cfg.machine_config;
                let intent = prepare_rename(config, d.repo_root(), authority.as_deref(), request, authoritative)?;
                intent.apply_config(config)?;
                Ok((intent, world))
            })
            .await?;
        let result = self.store.send(self, rename_write(&intent)).await;
        let routed_rules = intent.routed_rules.clone();
        match self.blocking(move |d, eng| d.finish_harness_rename(eng, world, intent, result)).await {
            Ok(update) => Ok(crate::client::HarnessRenameResult { update, routed_rules }),
            Err(error) => {
                self.restart_assignment_session.notify_one();
                Err(error)
            }
        }
    }

    /// A rename the server session finds pending when it connects.
    pub(crate) fn pending_harness_rename(&self, eng: &mut Engine) -> Result<Option<(RenameIntent, World)>> {
        let config = &self.cfg.machine_config;
        let Some(intent) = RenameIntent::load(config, self.store.rename_authority().as_deref())? else {
            return Ok(None);
        };
        let world = self.world()?;
        self.lock_repo(eng)?;
        intent.apply_config(config)?;
        Ok(Some((intent, world)))
    }

    /// Applies the answer to a rename sent from `world`. A daemon that was
    /// removed, or halted for a rejoin, since applies nothing and saves no
    /// snapshot: the state files may already be the next registration's.
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
            let snap = AssignmentSnapshot {
                revision: update.revision,
                assignment: update.assignment.clone(),
                registration: None,
            };
            self.store.save(&snap).map_err(|e| {
                Error::Internal(anyhow::anyhow!("the Assignment was renamed, but saving it failed: {e}"))
            })?;
        }
        let mut update = resolve_rename(&self.cfg.machine_config, &intent, result)?;
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

/// The rename intent for `request`: the pending one if it is for the same
/// request, else a new one from `authoritative` (the Assignment where it
/// lives). Only local files: callers fetch first.
fn prepare_rename(
    config: &Path,
    repo: &Path,
    authority: Option<&str>,
    request: RenameRequest,
    authoritative: Option<sk1ll_core::Assignment>,
) -> Result<RenameIntent> {
    if let Some(intent) = RenameIntent::load(config, authority)? {
        if intent.request == request {
            return Ok(intent);
        }
        return Err(Error::Conflict("finish the pending Harness rename first".into()));
    }
    let authoritative = authoritative
        .ok_or_else(|| Error::Conflict("the pending Harness rename finished meanwhile; reload and retry".into()))?;
    Ok(RenameIntent::prepare(config, repo, request, &authoritative, authority)?)
}

pub(crate) fn rename_write(intent: &RenameIntent) -> AssignmentWrite {
    AssignmentWrite::RenameHarness(sk1ll_proto::HarnessRename {
        from: intent.request.from.clone(),
        to: intent.request.to.clone(),
        replace: intent.request.replace,
        operation_id: intent.operation_id.clone(),
    })
}

/// A refusal no retry changes: the config goes back to the old name.
fn definitive_refusal(error: &Error) -> bool {
    matches!(error, Error::Upstream(status, _) if matches!(*status,
        StatusCode::BAD_REQUEST | StatusCode::CONFLICT | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND))
}

fn resolve_rename(config: &Path, intent: &RenameIntent, result: Result<AssignmentUpdate>) -> Result<AssignmentUpdate> {
    match result {
        Ok(update) => {
            intent.apply_config(config)?;
            intent.finish(config)?;
            Ok(update)
        }
        Err(error) => {
            if definitive_refusal(&error) {
                intent.rollback_config(config)?;
            }
            Err(error)
        }
    }
}
