//! Changing this machine's harnesses after setup, and what that does to the
//! content sync already placed in them.
//!
//! Sync only looks at the harnesses `machine.toml` currently names. Content
//! it placed in a harness that is disabled or removed, or at a path a
//! harness no longer uses, is never revisited: it stays where it is,
//! unmanaged from then on. A change never removes anything. [`plan`] lists
//! that content first, so it can be taken out the usual way before the
//! change: turn the harness's items off in its assignment
//! (`sk1ll assign <harness> --all off`) and let sync remove them, with all
//! its checks for hand edits.
//!
//! A change never touches the assignment: re-enabling a harness brings back
//! its toggles.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config_file::Snapshot;
use crate::machine::{Harness, HarnessChange, InstructionsFormat, MachineConfig, overlaps, real};
use crate::sync::{self, Action, PlanOptions};
use crate::{Assignment, Error, Repo, Result};

/// What a harness change would do, computed without changing anything.
#[derive(Debug, Clone, Serialize)]
pub struct ChangePlan {
    pub change: HarnessChange,
    pub fingerprint: String,
    /// The changed harness as it is now (`None`: not synced into).
    pub before: Option<Harness>,
    /// The changed harness once the change is made (`None`: not synced into).
    pub after: Option<Harness>,
    /// Content sk1ll placed where the change stops syncing, which stays
    /// there: managed items as Removes (what sync would do were they turned
    /// off), hand-edited copies as Conflicts.
    pub placed: Vec<Action>,
    pub warnings: Vec<String>,
    /// What the next sync does in the changed harness, given the assignment.
    pub sync: Vec<Action>,
    /// The new `machine.toml` text.
    #[serde(skip)]
    pub text: String,
    #[serde(skip)]
    snapshot: Snapshot,
}

/// What `change` to the `machine.toml` at `path` would do. Without an
/// `assignment` (a joined machine still waiting for its own), `sync` is empty.
pub fn plan(repo: &Repo, path: &Path, change: &HarnessChange, assignment: Option<&Assignment>) -> Result<ChangePlan> {
    plan_snapshot(repo, change, assignment, Snapshot::take(path)?)
}

fn plan_snapshot(
    repo: &Repo,
    change: &HarnessChange,
    assignment: Option<&Assignment>,
    snapshot: Snapshot,
) -> Result<ChangePlan> {
    let name = change.name();
    let before = MachineConfig::from_snapshot(&snapshot)?.harnesses();
    let edit = MachineConfig::edit(&snapshot, change)?;
    let after = edit.config.harnesses();
    if let Some(h) = after.get(name) {
        check_outside_repo(repo, name, h)?;
    }
    let mut placed = Vec::new();
    let mut warnings = Vec::new();
    if let Some(old) = before.get(name) {
        let left = left_part(old, &after);
        for part in [Harness { instructions: None, ..left.clone() }, Harness { skills: None, ..left }] {
            match sync::placed(repo, name, &part) {
                Ok(items) => placed.extend(items),
                Err(e) => warnings.push(format!("Could not list old placements for {name}: {e}")),
            }
        }
    }
    let sync = match (after.get(name), assignment) {
        (Some(h), Some(a)) => {
            let one = BTreeMap::from([(name.to_string(), h.clone())]);
            sync::plan(repo, &one, a, PlanOptions::default())?
        }
        _ => Vec::new(),
    };
    Ok(ChangePlan {
        change: change.clone(),
        fingerprint: snapshot.fingerprint(),
        before: before.get(name).cloned(),
        after: after.get(name).cloned(),
        placed,
        warnings,
        sync,
        text: edit.text,
        snapshot,
    })
}

/// Makes `change`: plans it, and writes `machine.toml` only if it is still
/// as read ([`Error::Changed`] otherwise). Nothing placed is removed. The
/// caller syncs afterwards, so the changed harness receives its content.
/// Hold the engine lock.
pub fn apply(
    _lock: &crate::EngineLock,
    repo: &Repo,
    path: &Path,
    change: &HarnessChange,
    expected: &str,
) -> Result<ChangePlan> {
    let snapshot = Snapshot::take(path)?;
    if snapshot.fingerprint() != expected {
        return Err(Error::Changed("machine.toml changed since the preview — preview again".into()));
    }
    let plan = plan_snapshot(repo, change, None, snapshot)?;
    if plan.snapshot.text()? != plan.text {
        plan.snapshot.replace(&plan.text)?;
    }
    Ok(plan)
}

/// [`apply`] for a change made after taking out what sk1ll placed: the
/// plan it writes from must have scanned every placement, and found none
/// still managed, or nothing is written ([`Error::Changed`]).
pub fn apply_taken_out(
    _lock: &crate::EngineLock,
    repo: &Repo,
    path: &Path,
    change: &HarnessChange,
    expected: &str,
) -> Result<ChangePlan> {
    let snapshot = Snapshot::take(path)?;
    if snapshot.fingerprint() != expected {
        return Err(Error::Changed("machine.toml changed since the preview — preview again".into()));
    }
    let plan = plan_snapshot(repo, change, None, snapshot)?;
    if let Some(warning) = plan.warnings.first() {
        return Err(Error::Changed(format!("{warning}; machine.toml was not changed")));
    }
    let left = plan.placed.iter().filter(|a| a.kind == sync::ActionKind::Remove).count();
    if left > 0 {
        return Err(Error::Changed(format!(
            "{left} placed {} still there; machine.toml was not changed",
            if left == 1 { "item is" } else { "items are" }
        )));
    }
    if plan.snapshot.text()? != plan.text {
        plan.snapshot.replace(&plan.text)?;
    }
    Ok(plan)
}

/// The part of `old` that no harness in `after` syncs into any more.
fn left_part(old: &Harness, after: &BTreeMap<String, Harness>) -> Harness {
    let skills = old.skills_dir().map(|p| real(&p));
    let instructions = old.instructions_file().map(|p| (real(&p), old.instructions_format));
    let still = |h: &Harness| -> (Option<PathBuf>, Option<(PathBuf, InstructionsFormat)>) {
        (h.skills_dir().map(|p| real(&p)), h.instructions_file().map(|p| (real(&p), h.instructions_format)))
    };
    let used: Vec<_> = after.values().map(still).collect();
    Harness {
        skills: old.skills.clone().filter(|_| !used.iter().any(|(s, _)| s.is_some() && *s == skills)),
        instructions: old.instructions.clone().filter(|_| !used.iter().any(|(_, i)| i.is_some() && *i == instructions)),
        ..old.clone()
    }
}

/// Syncing into the repo itself would mix harness copies with repo content.
fn check_outside_repo(repo: &Repo, name: &str, h: &Harness) -> Result<()> {
    for path in [h.skills_dir(), h.instructions_file()].into_iter().flatten() {
        let path = real(&path);
        if overlaps(&path, repo.root()) {
            return Err(Error::Invalid(format!(
                "`{name}` can't sync into {}: it overlaps the skills repo {}",
                path.display(),
                repo.root().display()
            )));
        }
    }
    Ok(())
}
