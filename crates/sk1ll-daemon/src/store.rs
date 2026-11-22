//! Where this Machine's Assignment lives ([`AssignmentStore`]): at the
//! server for a joined Machine ([ADR 0001]), in a machine-local file for a
//! standalone one ([ADR 0004]). The daemon has one code path over it: it
//! sends a write where the Assignment lives without the engine mutex, then
//! applies the confirmed snapshot under the mutex, after checking that it is
//! still in the [`World`](crate::daemon::World) it sent it from.
//!
//! [ADR 0001]: ../../../docs/adr/0001-assignment-is-server-state.md
//! [ADR 0004]: ../../../docs/adr/0004-standalone-assignment-is-machine-local.md

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::future::BoxFuture;
use sk1ll_core::harness_rename::RenameIntent;
use sk1ll_core::{Assignment, MachineConfig, Repo};
use sk1ll_proto::{AssignmentChanges, AssignmentSnapshot, AssignmentUpdate, AssignmentWrite, ToggleKind};

use crate::daemon::Daemon;
use crate::error::{Error, Result};
use crate::state::{self, DaemonState};
use crate::{Config, server};

/// Where the Assignment lives, and how a write reaches it.
pub(crate) trait AssignmentStore: Send + Sync {
    /// The stored snapshot to start from. `None` until there is one: a
    /// joined Machine leaves its harnesses alone until the server sends it.
    fn load(&self) -> anyhow::Result<Option<AssignmentSnapshot>>;

    /// Stores `snap`, about to be applied, so a restart starts from it.
    fn save(&self, snap: &AssignmentSnapshot) -> Result<()>;

    /// Under the engine mutex: what changed where the Assignment lives
    /// without the daemon (a hand edit of the file), as the snapshot that
    /// follows `applied`. `None`: nothing did.
    fn reload(&self, applied: Option<&AssignmentSnapshot>) -> Result<Option<AssignmentSnapshot>>;

    /// A file whose edits change the Assignment: an Input of the reconcile,
    /// and watched.
    fn file(&self) -> Option<&Path>;

    /// Who may finish a pending Harness rename intent (see [`RenameIntent`]).
    fn rename_authority(&self) -> Option<String>;

    /// Whether a write can be sent now, without the engine mutex: checked
    /// before work a write depends on (an Adopt publishes the skill first).
    /// A daemon that connects on demand connects here.
    fn ready<'a>(&'a self, d: &'a Arc<Daemon>) -> BoxFuture<'a, Result<()>>;

    /// Sends `write` where the Assignment lives and returns what it
    /// confirmed. Called without the engine mutex, one write at a time.
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
            revisions: cfg.state.with_file_name("standalone-assignment.json"),
            machine_config: cfg.machine_config.clone(),
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
        let snap = match state::load_snapshot(&self.snapshot, Some(&self.owner)) {
            Ok(Some(snap)) => snap,
            Ok(None) => {
                tracing::warn!("no stored assignment: leaving the harnesses alone until the server sends one");
                return Ok(None);
            }
            Err(e) => {
                tracing::warn!("{e:#}; leaving the harnesses alone until the server sends an assignment");
                return Ok(None);
            }
        };
        self.write_mirror(&snap);
        Ok(Some(snap))
    }

    fn save(&self, snap: &AssignmentSnapshot) -> Result<()> {
        state::save_snapshot(&self.snapshot, snap, Some(&self.owner))?;
        self.write_mirror(snap);
        Ok(())
    }

    fn reload(&self, _: Option<&AssignmentSnapshot>) -> Result<Option<AssignmentSnapshot>> {
        Ok(None)
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
struct LocalAssignment {
    file: PathBuf,
    revisions: PathBuf,
    machine_config: PathBuf,
}

impl AssignmentStore for LocalAssignment {
    fn load(&self) -> anyhow::Result<Option<AssignmentSnapshot>> {
        let assignment = Assignment::load(&self.file)?;
        let previous = state::load_snapshot(&self.revisions, None)?;
        let revision = previous.as_ref().map_or(1, |s| s.revision + u64::from(s.assignment != assignment));
        let snap = AssignmentSnapshot { revision, assignment, registration: None };
        state::save_snapshot(&self.revisions, &snap, None)?;
        Ok(Some(snap))
    }

    fn save(&self, snap: &AssignmentSnapshot) -> Result<()> {
        // Assignment::save replaces the file atomically.
        if Assignment::load(&self.file)? != snap.assignment {
            snap.assignment.save(&self.file)?;
        }
        Ok(state::save_snapshot(&self.revisions, snap, None)?)
    }

    fn reload(&self, applied: Option<&AssignmentSnapshot>) -> Result<Option<AssignmentSnapshot>> {
        // A rename that stopped between its two writes is finished first.
        if let Some(intent) = RenameIntent::load(&self.machine_config, None)? {
            intent.apply_standalone(&self.machine_config, &self.file)?;
        }
        let assignment = Assignment::load(&self.file)?;
        if applied.is_some_and(|s| s.assignment == assignment) {
            return Ok(None);
        }
        let revision = applied.map_or(1, |s| s.revision + 1);
        Ok(Some(AssignmentSnapshot { revision, assignment, registration: None }))
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

    /// The file is where the Assignment lives: the write is made to it under
    /// the engine mutex, as the server would make it, and confirmed at once.
    fn send<'a>(&'a self, d: &'a Arc<Daemon>, write: AssignmentWrite) -> BoxFuture<'a, Result<AssignmentUpdate>> {
        let file = self.file.clone();
        Box::pin(d.blocking(move |d, eng| {
            d.lock_repo(eng)?;
            d.refresh_assignment()?;
            let applied = d.applied().expect("a standalone Machine always has its Assignment");
            let mut assignment = Assignment::load(&file)?;
            match write {
                AssignmentWrite::Changes(changes) => {
                    check_expected(&changes, &applied, d.ident.machine_id())?;
                    apply_changes(&mut assignment, &changes, d)?;
                }
                AssignmentWrite::Harness { harness, enabled } => {
                    known_harness(d, &harness)?;
                    assignment.set_harness(&harness, enabled);
                }
                // A reconcile that ran since the intent was written finished
                // the rename already: the file has it.
                AssignmentWrite::RenameHarness(rename) => {
                    if let Some(intent) = RenameIntent::load(&d.cfg.machine_config, None)?
                        .filter(|intent| intent.operation_id == rename.operation_id)
                    {
                        assignment = intent.renamed_assignment(&file)?;
                    }
                }
            }
            let revision = applied.revision + u64::from(assignment != applied.assignment);
            Ok(AssignmentUpdate { revision, assignment, entries: Vec::new(), errors: Vec::new() })
        }))
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
