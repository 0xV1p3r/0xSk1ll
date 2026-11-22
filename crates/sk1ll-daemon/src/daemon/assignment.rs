use super::*;
use std::sync::Arc;

use sk1ll_core::harness_rename::RenameIntent;
use sk1ll_core::{MachineConfig, Repo};
use sk1ll_proto::{AssignmentSnapshot, AssignmentUpdate, AssignmentWrite};

use crate::client::Event;
use crate::error::{Error, Result};
use crate::state::{self};

impl Daemon {
    /// Publishes local file edits through the same snapshot the engine's
    /// readers use. The separate revision file never becomes fleet state.
    pub(super) fn reload_local_assignment(&self) -> Result<()> {
        if let Some(intent) = RenameIntent::load(&self.cfg.machine_config, None)? {
            intent.apply_standalone(&self.cfg.machine_config, &self.cfg.assignment)?;
        }
        let assignment = sk1ll_core::Assignment::load(&self.cfg.assignment)?;
        let previous = self.applied();
        if previous.as_ref().is_some_and(|s| s.assignment == assignment) {
            return Ok(());
        }
        let revision = previous.as_ref().map_or(1, |s| s.revision + 1);
        let snap = AssignmentSnapshot { revision, assignment, registration: None };
        state::save_snapshot(&self.cfg.state.with_file_name("standalone-assignment.json"), &snap)?;
        *self.applied.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(snap));
        self.update(|v| {
            v.assignment_revision = revision;
            v.awaiting_assignment = false;
        });
        self.event(Event::Assignment { revision });
        Ok(())
    }

    pub(crate) fn set_local_assignment(
        &self,
        eng: &mut Engine,
        changes: &sk1ll_proto::AssignmentChanges,
    ) -> Result<sk1ll_proto::AssignmentUpdate> {
        use sk1ll_proto::ToggleKind;
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        // An edit made outside the daemon counts as a newer revision.
        self.reload_local_assignment()?;
        let mut repo = Repo::open(self.repo_root())?;
        // Changing a default records every item's current value by Item ID,
        // so each item needs one. A pending pull's tree is partial: no writes.
        if !repo.pull_pending()? && repo.assign_missing_ids()? > 0 {
            repo = Repo::open(self.repo_root())?;
        }
        let harnesses = MachineConfig::load(&self.cfg.machine_config)?.harnesses();
        let skills: Vec<_> = repo.skills()?.into_iter().map(|s| s.id).collect();
        let rules: Vec<_> = repo.rules()?.into_iter().map(|r| r.id).collect();
        let unassigned = skills.iter().chain(&rules).any(Option::is_none);
        let (skills, rules): (Vec<_>, Vec<_>) =
            (skills.into_iter().flatten().collect(), rules.into_iter().flatten().collect());
        if let Some(expected) = &changes.expected {
            let revision = self.applied().map_or(0, |s| s.revision);
            if expected.registration.is_some() {
                return Err(Error::Conflict(
                    "this Machine is standalone now; reload its Assignment before changing it".into(),
                ));
            }
            if revision != expected.revision {
                return Err(Error::Conflict(format!(
                    "the Assignment of `{}` changed since revision {} (now {revision}); reload it",
                    self.ident.machine_id(),
                    expected.revision
                )));
            }
        }
        let mut assignment = sk1ll_core::Assignment::load(&self.cfg.assignment)?;
        for change in &changes.changes {
            if !harnesses.contains_key(&change.harness) {
                return Err(Error::BadRequest(format!("unknown harness `{}`", change.harness)));
            }
            if change.kind.has_item() != change.item_id.is_some()
                || (!change.kind.has_item() && (change.enabled.is_none() || change.name.is_some()))
            {
                return Err(Error::BadRequest(
                    "item toggles need an Item ID; defaults need enabled and take no item".into(),
                ));
            }
            if unassigned && matches!(change.kind, ToggleKind::NewSkillsDefault | ToggleKind::NewRulesDefault) {
                return Err(Error::Conflict(
                    "an item has no Item ID yet (a pull is pending); change the defaults once it has finished".into(),
                ));
            }
            match change.kind {
                ToggleKind::NewSkillsDefault => {
                    assignment.materialize_skills(&change.harness, &skills);
                    assignment.set_new_skills_default(&change.harness, change.enabled.unwrap());
                }
                ToggleKind::NewRulesDefault => {
                    assignment.materialize_rules(&change.harness, &rules);
                    assignment.set_new_rules_default(&change.harness, change.enabled.unwrap());
                }
                ToggleKind::Skill | ToggleKind::Rule => {
                    let h = assignment.harnesses.entry(change.harness.clone()).or_default();
                    let toggles = if change.kind == ToggleKind::Skill { &mut h.skills } else { &mut h.rules };
                    let id = change.item_id.as_ref().unwrap();
                    if let Some(enabled) = change.enabled {
                        toggles.insert(id.clone(), enabled);
                    } else {
                        toggles.remove(id);
                    }
                }
            }
        }
        // Assignment::save replaces the file atomically, like the direct CLI.
        assignment.save(&self.cfg.assignment)?;
        let errors = self.saved_local_assignment(eng);
        let snap = self.applied().expect("standalone always has assignment");
        Ok(sk1ll_proto::AssignmentUpdate {
            revision: snap.revision,
            assignment: snap.assignment.clone(),
            entries: Vec::new(),
            errors,
        })
    }

    /// Publishes and applies a just saved local Assignment. The save is the
    /// write: a failure from here on shows in the status, never as a failed write.
    pub(super) fn saved_local_assignment(&self, eng: &mut Engine) -> Vec<String> {
        let result = self.reload_local_assignment().and_then(|()| self.reconcile(eng));
        self.assignment_placement_outcome(result)
    }

    pub(super) fn assignment_placement_outcome(&self, result: Result<Vec<String>>) -> Vec<String> {
        let errors = result.unwrap_or_else(|e| {
            let message = crate::auth::redact(&format!("applying the saved Assignment: {e}"));
            self.set_error(message.clone());
            vec![message]
        });
        errors.into_iter().map(|error| crate::auth::redact(&error)).collect()
    }

    /// Writes Assignment: to the machine-local file standalone, else through
    /// the server session. The server may take up to its request timeout to
    /// confirm, so a joined write holds the engine mutex only to check
    /// whether it may be sent and, once confirmed, to apply it.
    pub(crate) async fn write_assignment(self: &Arc<Self>, write: AssignmentWrite) -> Result<AssignmentUpdate> {
        if self.is_standalone() {
            return self.blocking(move |d, eng| d.write_local_assignment(eng, write)).await;
        }
        let _writes = self.assignment_writes.lock().await;
        self.blocking(|d, _| {
            if RenameIntent::exists(&d.cfg.machine_config)? {
                return Err(Error::Unavailable("waiting for the pending Harness rename to finish".into()));
            }
            Ok(())
        })
        .await?;
        let update = crate::server::write_assignment(self, write).await?;
        self.blocking(move |d, eng| Ok(d.apply_confirmed(eng, update))).await
    }

    pub(super) fn write_local_assignment(&self, eng: &mut Engine, write: AssignmentWrite) -> Result<AssignmentUpdate> {
        match write {
            AssignmentWrite::Changes(changes) => self.set_local_assignment(eng, &changes),
            AssignmentWrite::Harness { harness, enabled } => {
                self.ensure_managed()?;
                self.lock_repo(eng)?;
                self.reload_local_assignment()?;
                if !MachineConfig::load(&self.cfg.machine_config)?.harnesses().contains_key(&harness) {
                    return Err(Error::BadRequest(format!("unknown harness `{harness}`")));
                }
                let mut assignment = sk1ll_core::Assignment::load(&self.cfg.assignment)?;
                assignment.set_harness(&harness, enabled);
                assignment.save(&self.cfg.assignment)?;
                let errors = self.saved_local_assignment(eng);
                let snap = self.applied().expect("standalone always has assignment");
                Ok(AssignmentUpdate {
                    revision: snap.revision,
                    assignment: snap.assignment.clone(),
                    entries: Vec::new(),
                    errors,
                })
            }
            AssignmentWrite::RenameHarness(_) => unreachable!("handled with the machine config edit"),
        }
    }

    /// Applies an Assignment the server confirmed, as of now rather than as
    /// of when it was sent. A daemon that stopped managing meanwhile applies
    /// nothing; a newer revision applied meanwhile (its snapshot overtook the
    /// confirmation) stands, and since the server numbers revisions in order
    /// it already includes this write: the reply then carries that one.
    pub(super) fn apply_confirmed(&self, eng: &mut Engine, mut update: AssignmentUpdate) -> AssignmentUpdate {
        if self.idle() {
            tracing::info!("not applying Assignment revision {}: no longer managing", update.revision);
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

    pub(super) fn current_assignment(&self) -> Result<sk1ll_core::Assignment> {
        if self.is_standalone() {
            return Ok(sk1ll_core::Assignment::load(&self.cfg.assignment)?);
        }
        self.applied().map(|snap| snap.assignment.clone()).ok_or_else(|| {
            Error::Conflict("waiting for the server's assignment; the harnesses are left alone until it arrives".into())
        })
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
        state::save_snapshot(&self.cfg.snapshot_path(), &snap)?;
        if let Err(e) = snap.assignment.save(&self.cfg.assignment) {
            tracing::warn!("writing the assignment cache {}: {e}", self.cfg.assignment.display());
        }
        let revision = snap.revision;
        *self.applied.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(snap));
        tracing::info!("applied assignment revision {revision}");
        self.update(|v| {
            v.assignment_revision = revision;
            v.awaiting_assignment = false;
        });
        self.event(Event::Assignment { revision });
        let result = self.reconcile(eng);
        Ok(self.assignment_placement_outcome(result))
    }
}

/// Whether a snapshot at `incoming` replaces one at `applied`.
pub(crate) fn newer(applied: u64, incoming: u64) -> bool {
    incoming > applied
}
