//! Where this Machine's Assignment lives ([`AssignmentStore`]): at the
//! server for a joined Machine ([ADR 0001]), in a machine-local file for a
//! standalone one ([ADR 0004]). A joined write is sent to the server without
//! the engine mutex, then the confirmed snapshot is applied under the mutex,
//! after checking that it is still in the [`World`](crate::daemon::World) it
//! was sent from. A machine-local write has no network step: it is checked,
//! saved and applied in one step under the engine mutex
//! ([`LocalAssignment::write`]), so nothing can take its revision meanwhile.
//!
//! Loading and reading change nothing: a pending Harness rename is finished
//! only under the engine mutex and the machine lock
//! ([`AssignmentStore::finish_interrupted_rename`]), never by a reader.
//!
//! What was just loaded is never written back unless something changed, and
//! every machine-local save is a compare-and-swap against the exact bytes of
//! the file and its revision record it was derived from
//! ([`LocalAssignment::swap`]): a hand edit that lands after the load fails
//! the save instead of being replaced by what was loaded before it.
//!
//! [ADR 0001]: ../../../docs/adr/0001-assignment-is-server-state.md
//! [ADR 0004]: ../../../docs/adr/0004-standalone-assignment-is-machine-local.md

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::future::BoxFuture;
use sk1ll_core::harness_rename::RenameIntent;
use sk1ll_core::{Assignment, MachineConfig, Repo};
use sk1ll_proto::{AssignmentChanges, AssignmentSnapshot, AssignmentUpdate, AssignmentWrite, ToggleKind};

use crate::daemon::{Daemon, Engine};
use crate::error::{Error, Result};
use crate::state::{self, DaemonState};
use crate::{Config, server};

/// Where the Assignment lives, and how a write reaches it.
pub(crate) trait AssignmentStore: Send + Sync {
    /// The stored snapshot to start from, read without writing anything (the
    /// caller saves it if it holds the machine lock). `None` until there is
    /// one: a joined Machine leaves its harnesses alone until the server
    /// sends it.
    fn load(&self) -> anyhow::Result<Option<AssignmentSnapshot>>;

    /// [`load`](AssignmentStore::load), for an engine that holds the machine
    /// lock, as it starts: what it loads is recorded where it lives if it
    /// isn't yet (a hand edit made while no engine ran becomes a revision of
    /// its own), and nothing is written if it is.
    fn settle(&self) -> anyhow::Result<Option<AssignmentSnapshot>>;

    /// Stores `snap`, a server's snapshot about to be applied, so a restart
    /// starts from it. Only for a store that isn't
    /// [`local`](AssignmentStore::local), whose writes are compare-and-swaps.
    fn save(&self, snap: &AssignmentSnapshot) -> Result<()>;

    /// What changed where the Assignment lives without the daemon (a hand
    /// edit of the file), as the snapshot that follows `applied`. `None`:
    /// nothing did. Writes nothing, so readers may call it too.
    fn reload(&self, applied: Option<&AssignmentSnapshot>) -> Result<Option<AssignmentSnapshot>>;

    /// Under the engine mutex and the machine lock: finishes a machine-local
    /// Harness rename that stopped between its writes. Nothing for a joined
    /// Machine, whose session finishes its own.
    fn finish_interrupted_rename(&self) -> Result<()>;

    /// The machine-local store, whose writes are made in one step under the
    /// engine mutex instead of being sent ([`AssignmentStore::send`]).
    fn local(&self) -> Option<&LocalAssignment>;

    /// A file whose edits change the Assignment: an Input of the reconcile,
    /// and watched.
    fn file(&self) -> Option<&Path>;

    /// Who may finish a pending Harness rename intent (see [`RenameIntent`]).
    fn rename_authority(&self) -> Option<String>;

    /// Whether a write can be sent now, without the engine mutex: checked
    /// before work a write depends on (an Adopt publishes the skill first).
    /// A daemon that connects on demand connects here.
    fn ready<'a>(&'a self, d: &'a Arc<Daemon>) -> BoxFuture<'a, Result<()>>;

    /// Sends `write` to the server and returns what it confirmed. Called
    /// without the engine mutex, one write at a time. Only for a store
    /// that isn't [`local`](AssignmentStore::local).
    fn send<'a>(&'a self, d: &'a Arc<Daemon>, write: AssignmentWrite) -> BoxFuture<'a, Result<AssignmentUpdate>>;

    /// This Machine's Assignment where it lives now (`GET`, and what a
    /// Harness rename starts from). Called without the engine mutex.
    fn get<'a>(&'a self, d: &'a Arc<Daemon>) -> BoxFuture<'a, Result<AssignmentSnapshot>>;
}

/// The store for `cfg`: the server's for a joined Machine, else the file.
pub(crate) fn for_machine(cfg: &Config, joined: Option<&DaemonState>) -> Box<dyn AssignmentStore> {
    match joined {
        Some(state) => Box::new(ServerAssignment {
            snapshot: cfg.snapshot_path(),
            mirror: cfg.assignment.clone(),
            owner: state.registration_key(),
        }),
        None => Box::new(LocalAssignment {
            file: cfg.assignment.clone(),
            revisions: cfg.standalone_assignment_path(),
            machine_config: cfg.machine_config.clone(),
            state_dir: cfg.state_dir.clone(),
        }),
    }
}

/// A joined Machine's Assignment: held by the server, which confirms each
/// write over the session and sends every new revision. The daemon keeps the
/// last snapshot, tagged with the registration it belongs to, and mirrors it
/// to the Assignment file for reading.
struct ServerAssignment {
    snapshot: PathBuf,
    mirror: PathBuf,
    owner: String,
}

impl AssignmentStore for ServerAssignment {
    fn load(&self) -> anyhow::Result<Option<AssignmentSnapshot>> {
        match state::load_snapshot(&self.snapshot, Some(&self.owner)) {
            Ok(Some(snap)) => Ok(Some(snap)),
            Ok(None) => {
                tracing::warn!("no stored assignment: leaving the harnesses alone until the server sends one");
                Ok(None)
            }
            Err(e) => {
                tracing::warn!("{e:#}; leaving the harnesses alone until the server sends an assignment");
                Ok(None)
            }
        }
    }

    fn settle(&self) -> anyhow::Result<Option<AssignmentSnapshot>> {
        let snapshot = self.load()?;
        // The mirror is only a copy of the stored snapshot, for reading.
        if let Some(snap) = &snapshot
            && sk1ll_core::Assignment::load(&self.mirror).ok().as_ref() != Some(&snap.assignment)
        {
            self.write_mirror(snap);
        }
        Ok(snapshot)
    }

    fn save(&self, snap: &AssignmentSnapshot) -> Result<()> {
        state::save_snapshot(&self.snapshot, snap, Some(&self.owner))?;
        self.write_mirror(snap);
        Ok(())
    }

    fn reload(&self, _: Option<&AssignmentSnapshot>) -> Result<Option<AssignmentSnapshot>> {
        Ok(None)
    }

    fn finish_interrupted_rename(&self) -> Result<()> {
        Ok(())
    }

    fn local(&self) -> Option<&LocalAssignment> {
        None
    }

    fn file(&self) -> Option<&Path> {
        None
    }

    fn rename_authority(&self) -> Option<String> {
        Some(self.owner.clone())
    }

    fn ready<'a>(&'a self, d: &'a Arc<Daemon>) -> BoxFuture<'a, Result<()>> {
        Box::pin(server::connected(d))
    }

    fn send<'a>(&'a self, d: &'a Arc<Daemon>, write: AssignmentWrite) -> BoxFuture<'a, Result<AssignmentUpdate>> {
        Box::pin(async move {
            server::connected(d).await?;
            server::write_assignment(d, write).await
        })
    }

    fn get<'a>(&'a self, d: &'a Arc<Daemon>) -> BoxFuture<'a, Result<AssignmentSnapshot>> {
        Box::pin(async move { server::fetch_machine_assignment(d.ident.joined()?, &d.http).await })
    }
}

impl ServerAssignment {
    fn write_mirror(&self, snap: &AssignmentSnapshot) {
        if let Err(e) = snap.assignment.save(&self.mirror) {
            tracing::warn!("writing the assignment cache {}: {e}", self.mirror.display());
        }
    }
}

/// A standalone Machine's Assignment: the machine-local file, which the
/// daemon and hand edits change. Its revision, kept next to the state file,
/// grows with every change the daemon sees; it never becomes fleet state.
pub(crate) struct LocalAssignment {
    file: PathBuf,
    revisions: PathBuf,
    machine_config: PathBuf,
    /// sk1ll's state directory, where a pending Harness rename is kept.
    state_dir: PathBuf,
}

impl AssignmentStore for LocalAssignment {
    fn load(&self) -> anyhow::Result<Option<AssignmentSnapshot>> {
        Ok(Some(self.observe()?.snapshot(None)))
    }

    fn settle(&self) -> anyhow::Result<Option<AssignmentSnapshot>> {
        Ok(Some(self.settle_after(None)?))
    }

    fn save(&self, _: &AssignmentSnapshot) -> Result<()> {
        Err(Error::Internal(anyhow::anyhow!("a machine-local Assignment is saved by a compare-and-swap, not stored")))
    }

    fn reload(&self, applied: Option<&AssignmentSnapshot>) -> Result<Option<AssignmentSnapshot>> {
        let observed = self.observe()?;
        if applied.is_some_and(|s| s.assignment == observed.assignment) {
            return Ok(None);
        }
        Ok(Some(observed.snapshot(applied)))
    }

    fn finish_interrupted_rename(&self) -> Result<()> {
        if let Some(intent) = RenameIntent::load(&self.state_dir, &self.machine_config, None)? {
            intent.apply_standalone(&self.machine_config, &self.file)?;
        }
        Ok(())
    }

    fn local(&self) -> Option<&LocalAssignment> {
        Some(self)
    }

    fn file(&self) -> Option<&Path> {
        Some(&self.file)
    }

    fn rename_authority(&self) -> Option<String> {
        None
    }

    fn ready<'a>(&'a self, _: &'a Arc<Daemon>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn send<'a>(&'a self, _: &'a Arc<Daemon>, _: AssignmentWrite) -> BoxFuture<'a, Result<AssignmentUpdate>> {
        Box::pin(async {
            Err(Error::Internal(anyhow::anyhow!("a machine-local Assignment write is made in one step, not sent")))
        })
    }

    /// The applied snapshot, with a hand edit of the file not applied yet
    /// as the revision after it.
    fn get<'a>(&'a self, d: &'a Arc<Daemon>) -> BoxFuture<'a, Result<AssignmentSnapshot>> {
        Box::pin(async move {
            let applied = d.applied();
            match self.reload(applied.as_deref())? {
                Some(snap) => Ok(snap),
                None => Ok(applied.expect("a standalone Machine always has its Assignment").as_ref().clone()),
            }
        })
    }
}

/// How often a machine-local write starts over because the file was edited
/// by hand while it was being made.
const WRITE_ATTEMPTS: usize = 3;

/// What a machine-local snapshot is derived from, read once: the exact bytes
/// of the Assignment file and of its revision record, and what they hold.
pub(crate) struct Observed {
    file: Option<Vec<u8>>,
    record: Option<Vec<u8>>,
    assignment: Assignment,
    recorded: Option<AssignmentSnapshot>,
}

impl Observed {
    /// The snapshot the file is: at the recorded revision, or at the one
    /// after it if the file changed since (a hand edit). `floor`, the
    /// applied snapshot, keeps revisions from going back if the record does.
    fn snapshot(&self, floor: Option<&AssignmentSnapshot>) -> AssignmentSnapshot {
        let previous = match (self.recorded.as_ref(), floor) {
            (Some(recorded), Some(floor)) if floor.revision > recorded.revision => Some(floor),
            (Some(recorded), _) => Some(recorded),
            (None, floor) => floor,
        };
        let revision = previous.map_or(1, |p| p.revision + u64::from(p.assignment != self.assignment));
        AssignmentSnapshot { revision, assignment: self.assignment.clone(), registration: None }
    }

    /// Whether `snap` is what the record holds already.
    fn records(&self, snap: &AssignmentSnapshot) -> bool {
        self.recorded.as_ref() == Some(snap)
    }
}

impl LocalAssignment {
    /// Reads the file and its revision record.
    pub(crate) fn observe(&self) -> Result<Observed> {
        let record = state::read_snapshot(&self.revisions)?;
        let recorded = state::parse_snapshot(&self.revisions, record.as_deref(), None)?;
        let (file, assignment) = Assignment::observe(&self.file)?;
        Ok(Observed { file, record, assignment, recorded })
    }

    /// Under the machine lock: the snapshot the file is, recorded as such
    /// unless it is already (a hand edit becomes the revision after
    /// `applied`), by a compare-and-swap; a hand edit that lands meanwhile is
    /// read again, never replaced. Writes nothing when nothing changed.
    pub(crate) fn settle_after(&self, applied: Option<&AssignmentSnapshot>) -> Result<AssignmentSnapshot> {
        for _ in 0..WRITE_ATTEMPTS {
            let observed = self.observe()?;
            let snap = observed.snapshot(applied);
            if observed.records(&snap) || self.swap(&observed, &snap)? {
                return Ok(snap);
            }
            tracing::info!("the Assignment file changed while its revision was recorded; reading it again");
        }
        Err(Error::Conflict("the Assignment file kept changing while its revision was recorded; try again".into()))
    }

    /// Makes `write` to the file, under the engine mutex and the machine
    /// lock, in one step: what it is made from (the applied revision, with a
    /// hand edit taken up first), the next revision, saving both and
    /// applying them to the harnesses. The save is a compare-and-swap: if
    /// the file or its revision is no longer what the write was made from
    /// (a hand edit landed meanwhile), nothing is written and the write is
    /// made again from the new revision, so it is never lost and never
    /// overwrites the edit.
    pub(crate) fn write(&self, d: &Daemon, eng: &mut Engine, write: &AssignmentWrite) -> Result<AssignmentUpdate> {
        d.ensure_managed()?;
        d.lock_repo(eng)?;
        for _ in 0..WRITE_ATTEMPTS {
            self.finish_interrupted_rename()?;
            // Made from the file as it is now, a hand edit included.
            let observed = self.observe()?;
            let applied = d.applied();
            let base = observed.snapshot(applied.as_deref());
            let mut assignment = base.assignment.clone();
            match write {
                AssignmentWrite::Changes(changes) => {
                    check_expected(changes, &base, d.ident.machine_id())?;
                    apply_changes(&mut assignment, changes, d)?;
                }
                AssignmentWrite::Harness { harness, enabled } => {
                    known_harness(d, harness)?;
                    assignment.set_harness(harness, *enabled);
                }
                AssignmentWrite::RenameHarness(_) => {
                    return Err(Error::Internal(anyhow::anyhow!("a machine-local Harness rename is made in one step")));
                }
            }
            if assignment == base.assignment {
                // Nothing to write but a hand edit's revision, if there was one.
                if !observed.records(&base) && !self.swap(&observed, &base)? {
                    continue;
                }
                let newer = (applied.as_deref() != Some(&base)).then(|| base.clone());
                let errors = d.install_assignment(eng, newer);
                return Ok(AssignmentUpdate { revision: base.revision, assignment, entries: Vec::new(), errors });
            }
            let snap = AssignmentSnapshot { revision: base.revision + 1, assignment, registration: None };
            if !self.swap(&observed, &snap)? {
                tracing::info!("the Assignment file changed while a write was made from it; making it again");
                continue;
            }
            let (revision, assignment) = (snap.revision, snap.assignment.clone());
            let errors = d.install_assignment(eng, Some(snap));
            return Ok(AssignmentUpdate { revision, assignment, entries: Vec::new(), errors });
        }
        Err(Error::Conflict("the Assignment file kept changing while this write was made; try again".into()))
    }

    /// Saves `next` if the file and its revision record still hold exactly
    /// the bytes `base` read: `false`, with nothing written, if not. Only
    /// what changed is written: the file if `next` changes the Assignment,
    /// the record if it isn't `next` already. The file is replaced
    /// atomically, its record after it, so a stop in between leaves a file
    /// that is newer than its record, which the next load counts as one more.
    pub(crate) fn swap(&self, base: &Observed, next: &AssignmentSnapshot) -> Result<bool> {
        // A hand edit landing after `base` was read.
        sk1ll_core::failpoint!(DaemonStoreBeforeSwap, &self.file);
        if state::read_snapshot(&self.revisions)? != base.record {
            return Ok(false);
        }
        if next.assignment != base.assignment {
            if !next.assignment.swap(&self.file, base.file.as_deref())? {
                return Ok(false);
            }
        } else if Assignment::observe(&self.file)?.0 != base.file {
            return Ok(false);
        }
        if !base.records(next) {
            state::save_snapshot(&self.revisions, next, None)?;
        }
        Ok(true)
    }
}

fn known_harness(d: &Daemon, harness: &str) -> Result<()> {
    if !MachineConfig::load(&d.cfg.machine_config)?.harnesses().contains_key(harness) {
        return Err(Error::BadRequest(format!("unknown harness `{harness}`")));
    }
    Ok(())
}

/// A write made from a stale view is refused, as the server refuses it.
fn check_expected(changes: &AssignmentChanges, applied: &AssignmentSnapshot, machine: &str) -> Result<()> {
    let Some(expected) = &changes.expected else { return Ok(()) };
    if expected.registration.is_some() {
        return Err(Error::Conflict("this Machine is standalone now; reload its Assignment before changing it".into()));
    }
    if applied.revision != expected.revision {
        return Err(Error::Conflict(format!(
            "the Assignment of `{machine}` changed since revision {} (now {}); reload it",
            expected.revision, applied.revision
        )));
    }
    Ok(())
}

/// Applies `changes` as the server does: changing a default first records
/// every item's current value by Item ID, so each item needs one.
fn apply_changes(assignment: &mut Assignment, changes: &AssignmentChanges, d: &Daemon) -> Result<()> {
    let root = d.ident.repo();
    let mut repo = Repo::open(root)?;
    // A pending pull's tree is partial: no Item IDs are written into it.
    if !repo.pull_pending()? && repo.assign_missing_ids()? > 0 {
        repo = Repo::open(root)?;
    }
    let skills: Vec<_> = repo.skills()?.into_iter().map(|s| s.id).collect();
    let rules: Vec<_> = repo.rules()?.into_iter().map(|r| r.id).collect();
    let unassigned = skills.iter().chain(&rules).any(Option::is_none);
    let (skills, rules): (Vec<_>, Vec<_>) =
        (skills.into_iter().flatten().collect(), rules.into_iter().flatten().collect());
    for change in &changes.changes {
        known_harness(d, &change.harness)?;
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
                match change.enabled {
                    Some(enabled) => toggles.insert(id.clone(), enabled),
                    None => toggles.remove(id),
                };
            }
        }
    }
    Ok(())
}
