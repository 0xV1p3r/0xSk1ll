//! The reconcile loop. Desired state is a function of the Inputs: the repo
//! (working tree, `HEAD` and refs, a pull's journal), `machine.toml` and a
//! pending Harness rename, the applied Assignment, the pull state the
//! lifecycle remembers, each Harness's placements (see
//! [`sync::harness_fingerprint`]) and the recovery area. [`Inputs::gather`]
//! fingerprints them from `lstat` alone, without reading a file or running
//! git, and [`Daemon::reconcile`] plans and applies only when that
//! fingerprint differs from the one it last reconciled successfully.
//!
//! Every trigger goes through it: file events and the periodic tick wake the
//! background task ([`Daemon::wake`], [`run`]); the server session's
//! Assignment snapshots and nudges, and API mutations, call it under the
//! engine mutex they hold, so their reply reflects the reconciled state.
//! A reconcile that failed, reported errors, or saw an input change within
//! the racy window (see [`HashCache`]) records no fingerprint, so the next
//! wake reconciles again. The first reconcile after startup, and then one at
//! most every [`Config::full_reconcile_every`], is full: it plans whatever
//! the fingerprint says and empties the hash cache first, bounding how long a
//! change metadata cannot show goes unnoticed.

use super::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use sk1ll_core::harness_rename::RenameIntent;
use sk1ll_core::hash::{HashCache, Stamp};
use sk1ll_core::sync::{self, ActionKind, PlanOptions};
use sk1ll_core::{ItemRef, MachineConfig, PullOutcome, Repo};
use sk1ll_proto::{ConflictRef, MachineReport};

use crate::client::{ConflictView, PlacementView};
use crate::error::{Error, Result};
use crate::gitx;
use crate::state::PullState;
use crate::watch::WatchRoot;

/// How long an input must have been left alone before its stamp proves it
/// unchanged ([`Engine::racy`]); tests write their trees just before
/// reconciling.
pub(super) const RACY_WINDOW: std::time::Duration =
    if cfg!(test) { std::time::Duration::ZERO } else { HashCache::RACY_WINDOW };

/// What a reconcile reads, each part as a cheap fingerprint: equal Inputs
/// plan and apply the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Inputs {
    /// The working tree, without `.git` and imports being staged.
    repo: String,
    /// `HEAD`, the refs, the index, the config and a pull's journal.
    git: String,
    /// `machine.toml` and a pending Harness rename's intent.
    machine: String,
    /// The applied Assignment's revision, and standalone its file.
    assignment: (Option<u64>, Option<String>),
    pull: PullState,
    harnesses: Vec<(String, String)>,
    displaced: String,
}

impl Inputs {
    /// The Inputs as they are now, and those among them changed within the
    /// racy window: only Inputs without any may be recorded.
    pub(super) fn gather(d: &Daemon, window: std::time::Duration) -> sk1ll_core::Result<(Self, Vec<String>)> {
        let at = SystemTime::now();
        let mut racy = Vec::new();
        let mut settled = |what: &dyn std::fmt::Display, stamp: Stamp| -> String {
            if !stamp.settled(at, window) {
                racy.push(what.to_string());
            }
            stamp.digest().to_string()
        };
        let root = d.repo_root();
        let config = &d.cfg.machine_config;
        let intent = config.with_extension("harness-rename.json");
        let assignment = match d.is_standalone() {
            true => Some(settled(&d.cfg.assignment.display(), Stamp::through(&d.cfg.assignment, None)?)),
            false => None,
        };
        let inputs = Self {
            repo: settled(&root.display(), working_tree(root)?),
            git: settled(&"git", git_dirs(root)?),
            machine: format!(
                "{}\n{}",
                settled(&config.display(), Stamp::through(config, None)?),
                settled(&intent.display(), Stamp::through(&intent, None)?)
            ),
            assignment: (d.applied().map(|s| s.revision), assignment),
            pull: d.lifecycle().pull().clone(),
            harnesses: MachineConfig::load(config)?
                .harnesses()
                .iter()
                .map(|(name, harness)| {
                    Ok((name.clone(), settled(&format_args!("harness {name}"), sync::harness_fingerprint(harness)?)))
                })
                .collect::<sk1ll_core::Result<_>>()?,
            // Unit tests share one recovery area, which they write at any time.
            displaced: if cfg!(test) { String::new() } else { settled(&"recovery area", sync::displaced::stamp()?) },
        };
        Ok((inputs, racy))
    }

    /// Which of them differ from `before`, for the log.
    fn changed_since(&self, before: &Self) -> Vec<&'static str> {
        let mut changed = Vec::new();
        let mut check = |differs: bool, name| {
            if differs {
                changed.push(name);
            }
        };
        check(self.repo != before.repo, "working tree");
        check(self.git != before.git, "git");
        check(self.machine != before.machine, "machine config");
        check(self.assignment != before.assignment, "assignment");
        check(self.pull != before.pull, "pull state");
        check(self.harnesses != before.harnesses, "harnesses");
        check(self.displaced != before.displaced, "recovery area");
        changed
    }
}

/// The directories of the working tree whose entries sk1ll stages imports
/// and renames in (see [`sk1ll_core::source::STAGE_PREFIX`]).
const STAGING_DIRS: [&str; 3] =
    [sk1ll_core::skill::PERSONAL_DIR, sk1ll_core::skill::IMPORTED_DIR, sk1ll_core::instructions::INSTRUCTIONS_DIR];

/// The working tree: everything but `.git`, and imports and renames being
/// staged. Only the staging directories sk1ll itself creates, where it
/// creates them, are left out: a skill's own file of that name is content.
fn working_tree(root: &Path) -> sk1ll_core::Result<Stamp> {
    fn top(depth: usize, name: &std::ffi::OsStr) -> bool {
        depth == 1 && (name == ".git" || STAGING_DIRS.iter().any(|dir| name == *dir))
    }
    fn staging(depth: usize, name: &std::ffi::OsStr) -> bool {
        depth == 1 && name.to_string_lossy().starts_with(sk1ll_core::source::STAGE_PREFIX)
    }
    let mut parts = vec![(String::new(), Stamp::of(root, Some(top), false)?)];
    for dir in STAGING_DIRS {
        parts.push((dir.to_string(), Stamp::through(&root.join(dir), Some(staging))?));
    }
    Ok(Stamp::combine("working tree", parts))
}

/// `.git`, and when it is a pointer file (a linked worktree, a
/// `--separate-git-dir`) the git directory it names and that one's common
/// directory: `HEAD`, the refs, the index, the config and a pull's journal
/// live there, not in the pointer.
pub(super) fn git_dirs(root: &Path) -> sk1ll_core::Result<Stamp> {
    /// The common directory's other worktrees are not this repo's.
    fn common_bookkeeping(depth: usize, name: &std::ffi::OsStr) -> bool {
        git_bookkeeping(depth, name) || (depth == 1 && name == "worktrees")
    }
    let dot = root.join(".git");
    let mut parts = vec![(String::new(), Stamp::through(&dot, Some(git_bookkeeping))?)];
    if let Some(git_dir) = pointed_git_dir(&dot) {
        parts.push((git_dir.display().to_string(), Stamp::through(&git_dir, Some(git_bookkeeping))?));
        if let Ok(common) = std::fs::read_to_string(git_dir.join("commondir")) {
            let common = git_dir.join(common.trim_end_matches(['\n', '\r']));
            parts.push((common.display().to_string(), Stamp::through(&common, Some(common_bookkeeping))?));
        }
    }
    Ok(Stamp::combine("git", parts))
}

/// The git directory a `.git` pointer file names (`gitdir: <path>`, relative
/// to the file's directory unless absolute); `None` for a real directory.
fn pointed_git_dir(dot: &Path) -> Option<PathBuf> {
    if !std::fs::symlink_metadata(dot).ok()?.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(dot).ok()?;
    let path = text.lines().next()?.strip_prefix("gitdir:")?.trim();
    Some(dot.parent()?.join(path))
}

/// What git writes for itself: objects, logs, fetch results, lock files.
fn git_bookkeeping(depth: usize, name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();
    name.ends_with(".lock")
        || (depth == 1 && matches!(&*name, "objects" | "logs" | "lfs" | "FETCH_HEAD" | "ORIG_HEAD" | "COMMIT_EDITMSG"))
}

impl Daemon {
    /// Wakes the background reconcile ([`run`]): something may have changed.
    pub(crate) fn wake(&self) {
        self.wakeups.notify_one();
    }

    /// Reconciles unless no Input changed since the last successful
    /// reconcile: finishes a stopped pull, recovers, writes missing Item IDs,
    /// plans and applies the sync, then publishes the report. Conflicts are
    /// reported, never applied. Without an assignment, harnesses are left
    /// alone. Returns what the sync failed to place (also published as the
    /// last error).
    pub(crate) fn reconcile(&self, eng: &mut Engine) -> Result<Vec<String>> {
        self.reconcile_with(eng, None).map(|outcome| outcome.errors)
    }

    /// [`Daemon::reconcile`]; a `reviewed` plan is applied instead of a new
    /// one, always, and leaves the rest to the next reconcile.
    pub(super) fn reconcile_with(&self, eng: &mut Engine, reviewed: Option<ReviewedSync>) -> Result<ReconcileOutcome> {
        if self.idle() {
            return Ok(ReconcileOutcome::default());
        }
        // An edit of the Assignment file is a new revision, an Input.
        if self.is_standalone() {
            self.reload_local_assignment()?;
        }
        let gathered = Inputs::gather(self, eng.racy);
        let started = std::time::Instant::now();
        let full = eng.full_at.is_none_or(|at| started.duration_since(at) >= self.cfg.full_reconcile_every);
        let why = match (&gathered, &eng.reconciled) {
            _ if reviewed.is_some() => "a reviewed sync".to_string(),
            _ if full => "a full reconcile, whatever the inputs".to_string(),
            (Err(e), _) => format!("the inputs could not be read ({e})"),
            (Ok(_), None) => "no reconcile succeeded since the last change".to_string(),
            (Ok((now, _)), Some(before)) if now == before => {
                tracing::trace!("nothing changed since the last reconcile");
                return Ok(ReconcileOutcome::default());
            }
            (Ok((now, _)), Some(before)) => format!("changed: {}", now.changed_since(before).join(", ")),
        };
        tracing::debug!("reconciling: {why}");
        eng.reconciled = None;
        let reviewed_plan = reviewed.is_some();
        if full && !reviewed_plan {
            // Metadata can't see everything (a write through a shared
            // mapping, a directory's extended attributes): read every tree.
            self.hashes.clear();
        }
        let outcome = self.apply_inputs(eng, reviewed);
        self.hashes.sweep();
        let outcome = outcome?;
        if full && !reviewed_plan {
            eng.full_at = Some(started);
        }
        match gathered {
            Ok((inputs, racy)) if racy.is_empty() && !reviewed_plan && outcome.errors.is_empty() => {
                eng.reconciled = Some(inputs);
            }
            Ok((_, racy)) if !racy.is_empty() => {
                tracing::debug!("changed within the racy window, so reconciled again next time: {racy:?}");
            }
            _ => {}
        }
        Ok(outcome)
    }

    /// Plans and applies, in the order the Item ID and pull guards need.
    fn apply_inputs(&self, eng: &mut Engine, reviewed: Option<ReviewedSync>) -> Result<ReconcileOutcome> {
        if let Ok(state) = self.ident.joined()
            && RenameIntent::load(&self.cfg.machine_config, Some(&state.rename_authority()))?.is_some()
        {
            return Err(Error::Unavailable("waiting for the pending Harness rename to finish".into()));
        }
        let root = self.repo_root().to_path_buf();
        if self.is_standalone() && !root.join(".git").exists() {
            // Moved or removed: what readers had of it is gone with it.
            let generation = self.snapshot().generation + 1;
            self.snapshot.send_replace(Snapshot::without_repo(generation, Self::no_repo));
            self.update(|v| {
                v.repo_ready = false;
                v.has_origin = false;
                v.last_error = None;
                v.report = MachineReport::default();
            });
            self.roots
                .send_replace(vec![WatchRoot::new(root, true), WatchRoot::new(self.cfg.assignment.clone(), false)]);
            return Ok(ReconcileOutcome::default());
        }
        if !root.join(".git").exists() {
            return Err(Error::Conflict(format!("{} is not set up yet (the server was unreachable)", root.display())));
        }
        self.lock_repo(eng)?;
        let machine = MachineConfig::load(&self.cfg.machine_config)?;
        let harnesses = machine.harnesses();
        let lock = eng.lock.as_ref().ok_or_else(|| Error::Conflict(format!("{} is not locked", root.display())))?;
        let mut outcome = ReconcileOutcome::default();
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
        let pull = self.pull_after_reconcile(&root, finished, pending);
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
        let mut conflict_views: snapshot::Part<Vec<ConflictView>> = Ok(Vec::new());
        let snapshot = self.applied();
        if !pending
            && ids_ready
            && let Some(snap) = &snapshot
        {
            let plan = |repo: &Repo| {
                self.plans.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                sync::plan_cached(repo, &harnesses, &snap.assignment, PlanOptions::default(), &self.hashes)
            };
            let planned = reviewed.map(Ok).unwrap_or_else(|| {
                Repo::open(&root).and_then(|repo| {
                    let actions = plan(&repo)?;
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
                        plan(&repo)?
                    } else {
                        actions.into_iter().map(|(action, _)| action).collect()
                    };
                    for action in remaining.iter().filter(|a| a.kind == ActionKind::Conflict) {
                        if let Ok(views) = &mut conflict_views {
                            views.push(conflict_view(action));
                        }
                        for item in conflict_items(action) {
                            let conflict = ConflictRef { item, harness: action.harness.clone() };
                            if !conflicts.contains(&conflict) {
                                conflicts.push(conflict);
                            }
                        }
                    }
                }
                Err(e) => {
                    outcome.errors.push(format!("sync: {e}"));
                    conflict_views = Err(e.into());
                }
            }
        }

        let displaced = sync::displaced::list()?;
        for entry in displaced.iter().filter(|entry| entry.changed) {
            let c = ConflictRef { item: displaced_item(entry, &root), harness: entry.harness.clone() };
            if !conflicts.contains(&c) {
                conflicts.push(c);
            }
            if let Ok(views) = &mut conflict_views {
                views.push(displaced_view(entry.clone(), &root));
            }
        }
        let draft = self.draft()?;
        let drafts = draft.iter().map(|entry| entry.item.clone()).collect();
        let mut report = repo_report(&root, harnesses.keys().cloned().collect(), drafts)?;
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
        // Readers see the new state before the events that send them to it.
        let generation = self.snapshot().generation + 1;
        self.snapshot.send_replace(Arc::new(Snapshot {
            generation,
            draft: Ok(draft),
            items: Daemon::items(&root),
            conflicts: conflict_views,
            recovered: Ok(displaced.into_iter().map(|entry| displaced_view(entry, &root)).collect()),
        }));
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
        Ok(outcome)
    }

    /// Where a reconcile leaves the pull: what finishing a stopped pull
    /// (`finished`) found, else what was known, checked against the repo. The
    /// journal decides whether a pull is pending. A remembered block whose
    /// Baseline `HEAD` has reached since (a pull while the daemon was
    /// stopped) is over. So is a remembered pending pull whose journal is
    /// gone and whose Baseline `HEAD` contains: it finished, and the process
    /// stopped before it could record that.
    pub(super) fn pull_after_reconcile(&self, root: &Path, finished: Option<PullState>, pending: bool) -> PullState {
        if let Some(pull) = finished {
            return pull;
        }
        let reached = |baseline: &str| {
            gitx::rev(root, "HEAD")
                .ok()
                .flatten()
                .is_some_and(|head| gitx::is_ancestor(root, baseline, &head).unwrap_or(false))
        };
        match self.lifecycle().pull().clone() {
            PullState::Pending { items } if pending => PullState::Pending { items },
            _ if pending => PullState::Pending { items: Vec::new() },
            PullState::Pending { items } if items.is_empty() => PullState::Clear,
            PullState::Pending { items } => match gitx::remote_head(root) {
                Ok(Some(baseline)) if !reached(&baseline) => PullState::Blocked { baseline, items },
                _ => PullState::Clear,
            },
            PullState::Blocked { baseline, .. } if reached(&baseline) => PullState::Clear,
            pull => pull,
        }
    }

    /// A background reconcile, after file changes or the periodic tick: and
    /// if the Draft changed, a stopped pull is tried again.
    async fn reconcile_in_background(self: &Arc<Self>) -> Result<()> {
        self.blocking(|d, eng| {
            if d.idle() {
                return Ok(());
            }
            sync::displaced::prune()?;
            d.reconcile(eng).map(drop)
        })
        .await?;
        self.retry_stopped_pull().await;
        Ok(())
    }
}

/// Reconciles whenever woken ([`Daemon::wake`]); wakes while one runs make
/// one more.
pub(crate) async fn run(d: Arc<Daemon>) {
    loop {
        d.wakeups.notified().await;
        if d.is_revoked() {
            return;
        }
        if let Err(e) = d.reconcile_in_background().await {
            d.set_error(format!("sync after file change: {e}"));
        }
    }
}

/// The working tree's items, by Item ID. Incomplete, so the server neither
/// materializes defaults nor prunes orphans by it, while metadata is
/// unreadable, a pull is pending (the tree is partial), an item has no
/// Item ID yet (a reconcile writes one; none is ever derived), or two items
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

pub(super) fn repo_report(root: &Path, harnesses: Vec<String>, drafts: Vec<ItemRef>) -> Result<MachineReport> {
    let inventory = repo_inventory(root, harnesses);
    Ok(MachineReport {
        baseline: gitx::baseline(root)?,
        drafts,
        unpushed: gitx::unpushed_items(root)?,
        harnesses: inventory.harnesses,
        items: inventory.items,
        inventory_complete: inventory.inventory_complete,
        inventory_error: inventory.inventory_error,
        ..Default::default()
    })
}

#[derive(Default)]
pub(super) struct ReconcileOutcome {
    pub(super) errors: Vec<String>,
    pub(super) completed: Vec<PlacementView>,
    pub(super) changed: Option<PlacementView>,
}
