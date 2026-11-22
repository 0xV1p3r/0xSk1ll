use super::*;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use sk1ll_core::harness_rename::RenameIntent;
use sk1ll_core::sync::{self, ActionKind, PlanOptions};
use sk1ll_core::{MachineConfig, PullOutcome, Repo};
use sk1ll_proto::{ConflictRef, MachineReport};

use crate::client::PlacementView;
use crate::error::{Error, Result};
use crate::gitx;
use crate::state::PullState;
use crate::watch::WatchRoot;

impl Daemon {
    /// Plans and applies the sync, then recomputes the report. Conflicts are
    /// reported, never applied. Without an assignment, harnesses are left alone.
    ///
    /// This is the expensive operation (a full plan and a dozen git
    /// processes), so callers that know nothing it reads has changed since
    /// the last one skip it: see [`Daemon::pull_refreshed`], [`Daemon::nudge`]
    /// uses this to decide when it must recompute a report.
    pub(crate) fn refresh(&self, eng: &mut Engine) -> Result<()> {
        self.refresh_reporting(eng).map(drop)
    }

    /// [`Daemon::refresh`], returning what the sync failed to place (also
    /// published as the last error).
    pub(super) fn refresh_reporting(&self, eng: &mut Engine) -> Result<Vec<String>> {
        let result = self.refresh_with_plan(eng, None).map(|outcome| outcome.errors);
        eng.assignment_placement_failed = result.as_ref().map_or(true, |errors| !errors.is_empty());
        result
    }

    pub(super) fn refresh_with_plan(&self, eng: &mut Engine, reviewed: Option<ReviewedSync>) -> Result<RefreshOutcome> {
        if self.idle() {
            return Ok(RefreshOutcome::default());
        }
        if self.is_standalone() {
            self.reload_local_assignment()?;
        } else if let Ok(state) = self.ident.joined()
            && RenameIntent::load(&self.cfg.machine_config, Some(&state.rename_authority()))?.is_some()
        {
            return Err(Error::Unavailable("waiting for the pending Harness rename to finish".into()));
        }
        tracing::debug!("refreshing the local state");
        eng.stale = true;
        let root = self.repo_root().to_path_buf();
        if self.is_standalone() && !root.join(".git").exists() {
            self.update(|v| {
                v.repo_ready = false;
                v.has_origin = false;
                v.last_error = None;
            });
            self.roots
                .send_replace(vec![WatchRoot::new(root, true), WatchRoot::new(self.cfg.assignment.clone(), false)]);
            return Ok(RefreshOutcome::default());
        }
        if !root.join(".git").exists() {
            return Err(Error::Conflict(format!("{} is not set up yet (the server was unreachable)", root.display())));
        }
        self.lock_repo(eng)?;
        let machine = MachineConfig::load(&self.cfg.machine_config)?;
        let harnesses = machine.harnesses();
        let lock = eng.lock.as_ref().ok_or_else(|| Error::Conflict(format!("{} is not locked", root.display())))?;
        let mut outcome = RefreshOutcome::default();
        let repo = Repo::open_git_only(&root)?;
        let finish = repo.finish_pull(lock);
        let pending = repo.pull_pending()?;
        let finished = match finish {
            Ok(None) => None,
            Ok(Some(PullOutcome::Blocked { baseline, items })) if !pending => {
                Some(PullState::Blocked { baseline, items })
            }
            Ok(Some(PullOutcome::Blocked { items, .. })) => Some(PullState::Pending { items }),
            Ok(Some(_)) | Err(sk1ll_core::Error::Displaced { .. }) if !pending => {
                tracing::info!("finished a pull that had stopped half-way");
                Some(PullState::Clear)
            }
            Err(e) => {
                outcome.errors.push(format!("pull: {e}"));
                None
            }
            Ok(Some(_)) => None,
        };
        let pull = self.pull_after_refresh(&root, finished, pending);
        // Sync waits for every item to have an Item ID; the report doesn't.
        let mut ids_ready = true;
        if !pending && reviewed.is_none() {
            sync::recover(Some(lock), &harnesses)?;
            Self::recover_imports(&root)?;
            // After the pull is finished: a pending pull's tree is partial, and
            // writing into it could block the paths it still has to update.
            // Before sync, so placements carry the IDs.
            if let Err(e) = Repo::open(&root).and_then(|repo| repo.assign_missing_ids()) {
                outcome.errors.push(format!("Item IDs: {e}"));
                ids_ready = false;
            }
        }
        let is_reviewed = reviewed.is_some();
        let mut conflicts: Vec<ConflictRef> = Vec::new();
        let snapshot = self.snapshot();
        if !pending
            && ids_ready
            && let Some(snap) = &snapshot
        {
            let planned = reviewed.map(Ok).unwrap_or_else(|| {
                Repo::open(&root).and_then(|repo| {
                    let actions = sync::plan(&repo, &harnesses, &snap.assignment, PlanOptions::default())?;
                    Ok((repo, actions.into_iter().map(|action| (action, None)).collect()))
                })
            });
            match planned {
                Ok((repo, actions)) => {
                    for (action, digest) in &actions {
                        if action.kind == ActionKind::Conflict {
                            continue;
                        }
                        match sync::apply_reviewed(&repo, action, digest.as_deref()) {
                            Ok(()) if is_reviewed => outcome.completed.push(placement_view(action)),
                            Ok(()) | Err(sk1ll_core::Error::Displaced { .. }) => {}
                            Err(sk1ll_core::Error::Changed(_)) if digest.is_some() => {
                                outcome.changed = Some(placement_view(action));
                                break;
                            }
                            Err(e) => outcome.errors.push(format!("sync {}: {e}", action.dest.display())),
                        }
                    }

                    let remaining = if outcome.changed.is_some() {
                        sync::plan(&repo, &harnesses, &snap.assignment, PlanOptions::default())?
                    } else {
                        actions.into_iter().map(|(action, _)| action).collect()
                    };
                    for action in remaining.iter().filter(|a| a.kind == ActionKind::Conflict) {
                        for item in conflict_items(action) {
                            let conflict = ConflictRef { item, harness: action.harness.clone() };
                            if !conflicts.contains(&conflict) {
                                conflicts.push(conflict);
                            }
                        }
                    }
                }
                Err(e) => outcome.errors.push(format!("sync: {e}")),
            }
        }

        for entry in sync::displaced::changed()? {
            let c = ConflictRef { item: displaced_item(&entry, &root), harness: entry.harness.clone() };
            if !conflicts.contains(&c) {
                conflicts.push(c);
            }
        }
        let mut report = repo_report(&root, harnesses.keys().cloned().collect())?;
        if pending {
            // The partial checkout is not a Draft. Keep the last complete
            // inventory while the journal is active; `repo_report` already
            // marks it incomplete.
            let previous = self.view.borrow().report.clone();
            report.drafts = previous.drafts;
            report.items = previous.items;
        }
        report.conflicts = conflicts;
        let awaiting = snapshot.is_none();
        let has_origin = !self.is_standalone() || gitx::has_origin(&root).unwrap_or(false);
        // The lifecycle derives the report's Blocked, pending and behind.
        let ((), saved) = self.publish(
            |lc| lc.set_pull(pull),
            |v| {
                v.repo_ready = true;
                v.has_origin = has_origin;
                v.report = report;
                v.awaiting_assignment = awaiting;
                if let Some(e) = outcome.errors.first() {
                    v.last_error = Some(crate::auth::redact(e));
                }
            },
        );
        if let Err(e) = saved {
            outcome.errors.push(format!("saving the daemon state: {e:#}"));
            self.update(|v| v.last_error = Some(format!("saving the daemon state: {e:#}")));
        }
        for e in &outcome.errors {
            tracing::warn!("{}", crate::auth::redact(e));
        }

        let mut roots = vec![WatchRoot::new(root.canonicalize().unwrap_or(root), true)];
        if self.is_standalone() {
            roots.push(WatchRoot::new(self.cfg.assignment.clone(), false));
        }
        if let Some(dir) = self.cfg.machine_config.parent() {
            roots.push(WatchRoot::new(dir.to_path_buf(), false));
        }
        for h in harnesses.values() {
            roots.extend(h.skills_dir().map(|p| WatchRoot::new(p, true)));
            roots.extend(h.instructions_file().map(|p| WatchRoot::new(p, true)));
        }
        roots.sort();
        roots.dedup();
        self.roots.send_if_modified(|r| {
            let changed = *r != roots;
            *r = roots;
            changed
        });
        eng.stale = false;
        Ok(outcome)
    }

    /// Where a refresh leaves the pull: what finishing a stopped pull
    /// (`finished`) found, else what was known, checked against the repo. The
    /// journal decides whether a pull is pending. A remembered block whose
    /// Baseline `HEAD` has reached since (a pull while the daemon was
    /// stopped) is over.
    pub(super) fn pull_after_refresh(&self, root: &Path, finished: Option<PullState>, pending: bool) -> PullState {
        if let Some(pull) = finished {
            return pull;
        }
        match self.lifecycle().pull().clone() {
            PullState::Pending { items } if pending => PullState::Pending { items },
            _ if pending => PullState::Pending { items: Vec::new() },
            PullState::Pending { items } if items.is_empty() => PullState::Clear,
            PullState::Pending { items } => match gitx::remote_head(root) {
                Ok(Some(baseline)) => PullState::Blocked { baseline, items },
                _ => PullState::Clear,
            },
            PullState::Blocked { baseline, .. }
                if gitx::rev(root, "HEAD")
                    .ok()
                    .flatten()
                    .is_some_and(|head| gitx::is_ancestor(root, &baseline, &head).unwrap_or(false)) =>
            {
                PullState::Clear
            }
            pull => pull,
        }
    }

    /// After file changes (or the periodic reconciliation): refresh, and if
    /// Blocked and the draft changed, try the pull again.
    pub(crate) async fn on_files_changed(self: &Arc<Self>) -> Result<()> {
        let drafted = self
            .blocking(|d, eng| {
                if d.idle() {
                    return Ok(false);
                }
                sync::displaced::prune()?;
                let before = d.view.borrow().report.drafts.clone();
                d.refresh(eng)?;
                Ok(d.view.borrow().report.drafts != before)
            })
            .await?;
        if drafted {
            self.retry_stopped_pull().await;
        }
        Ok(())
    }
}

/// The working tree's items, by Item ID. Incomplete, so the server neither
/// materializes defaults nor prunes orphans by it, while metadata is
/// unreadable, a pull is pending (the tree is partial), an item has no
/// Item ID yet (a refresh writes one; none is ever derived), or two items
/// share one (until `repair-ids`).
pub(crate) fn repo_inventory(root: &Path, harnesses: Vec<String>) -> sk1ll_proto::InventoryReport {
    let inventory = sk1ll_core::item_id::working_inventory(root);
    let mut errors = inventory.errors;
    match Repo::open_git_only(root).and_then(|repo| repo.pull_pending()) {
        Ok(false) => {}
        Ok(true) => errors.insert(0, "a pull is pending, so the working tree is partial".into()),
        Err(e) => errors.insert(0, e.to_string()),
    }
    let mut items = Vec::new();
    let mut unassigned = Vec::new();
    for (item, id) in inventory.items {
        match id {
            Some(id) => items.push(sk1ll_proto::ReportedItem { item, id }),
            None => unassigned.push(item.to_string()),
        }
    }
    if !unassigned.is_empty() {
        errors.push(format!("no Item ID yet: {}", unassigned.join(", ")));
    }
    let mut seen = BTreeMap::new();
    for reported in &items {
        if let Some(first) = seen.insert(&reported.id, &reported.item) {
            errors.push(format!("{first} and {} share the Item ID {}", reported.item, reported.id));
        }
    }
    let inventory_error = errors.first().map(|error| crate::auth::redact(error));
    sk1ll_proto::InventoryReport {
        items,
        inventory_complete: inventory_error.is_none(),
        inventory_error,
        harnesses,
        ..Default::default()
    }
}

pub(super) fn repo_report(root: &Path, harnesses: Vec<String>) -> Result<MachineReport> {
    let repo = Repo::open_git_only(root)?;
    let inventory = repo_inventory(root, harnesses);
    Ok(MachineReport {
        baseline: gitx::baseline(root)?,
        drafts: repo.draft()?.into_iter().map(|d| d.item).collect(),
        unpushed: gitx::unpushed_items(root)?,
        harnesses: inventory.harnesses,
        items: inventory.items,
        inventory_complete: inventory.inventory_complete,
        inventory_error: inventory.inventory_error,
        ..Default::default()
    })
}

#[derive(Default)]
pub(super) struct RefreshOutcome {
    pub(super) errors: Vec<String>,
    pub(super) completed: Vec<PlacementView>,
    pub(super) changed: Option<PlacementView>,
}
