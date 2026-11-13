//! Changing a machine's assignment: every change is applied and logged in one
//! transaction, so the log always explains the stored state.

use std::collections::BTreeSet;

use rusqlite::Connection;
use sk1ll_proto::{Assignment, AssignmentSnapshot, HarnessAssignment, ItemRef, LogCause, LogEntry, ToggleKind};

use crate::db;
use crate::error::{ApiError, Result};

/// Author of changes the server makes on its own.
pub const SERVER_AUTHOR: &str = "server";

/// One toggle to set; `after: None` removes an item's explicit toggle.
#[derive(Debug, Clone)]
pub struct Change {
    pub harness: String,
    pub kind: ToggleKind,
    pub name: Option<String>,
    pub after: Option<bool>,
    pub cause: LogCause,
}

/// A harness that starts with nothing enabled.
pub fn all_off() -> HarnessAssignment {
    HarnessAssignment { new_skills_default: false, new_rules_default: false, ..Default::default() }
}

/// Current value of a toggle. Defaults are always `Some` (a harness without
/// an entry has the all-enabled defaults); item toggles are `None` when unset.
pub fn get(a: &Assignment, harness: &str, kind: ToggleKind, name: Option<&str>) -> Option<bool> {
    let h = a.harnesses.get(harness);
    match kind {
        ToggleKind::Skill => h.and_then(|h| h.skills.get(name?).copied()),
        ToggleKind::Rule => h.and_then(|h| h.rules.get(name?).copied()),
        ToggleKind::NewSkillsDefault => Some(a.harness(harness).new_skills_default),
        ToggleKind::NewRulesDefault => Some(a.harness(harness).new_rules_default),
    }
}

fn set(a: &mut Assignment, harness: &str, kind: ToggleKind, name: Option<&str>, value: Option<bool>) {
    let name = name.unwrap_or_default();
    match (kind, value) {
        (ToggleKind::Skill, Some(v)) => a.set_skill(harness, name, v),
        (ToggleKind::Rule, Some(v)) => a.set_rule(harness, name, v),
        (ToggleKind::Skill, None) => {
            if let Some(h) = a.harnesses.get_mut(harness) {
                h.skills.remove(name);
            }
        }
        (ToggleKind::Rule, None) => {
            if let Some(h) = a.harnesses.get_mut(harness) {
                h.rules.remove(name);
            }
        }
        (ToggleKind::NewSkillsDefault, Some(v)) => a.set_new_skills_default(harness, v),
        (ToggleKind::NewRulesDefault, Some(v)) => a.set_new_rules_default(harness, v),
        // Defaults can't be unset; `validate` rejects it.
        (ToggleKind::NewSkillsDefault | ToggleKind::NewRulesDefault, None) => {}
    }
}

/// Checks a change's shape: names only for item toggles, values for defaults.
pub fn validate(c: &Change) -> Result<()> {
    crate::check_name("harness", &c.harness)?;
    match (c.kind.has_name(), &c.name) {
        (true, Some(n)) => crate::check_name("item name", n)?,
        (true, None) => {
            return Err(ApiError::BadRequest("skill and rule toggles need a `name`".into()));
        }
        (false, Some(_)) => return Err(ApiError::BadRequest("defaults take no `name`".into())),
        (false, None) if c.after.is_none() => {
            return Err(ApiError::BadRequest("defaults need `enabled`: true or false".into()));
        }
        (false, None) => {}
    }
    Ok(())
}

/// The item an item toggle addresses; `None` for defaults.
fn item(kind: ToggleKind, name: &str) -> Option<ItemRef> {
    match kind {
        ToggleKind::Skill => Some(ItemRef::Skill(name.to_string())),
        ToggleKind::Rule => Some(ItemRef::Rule(name.to_string())),
        ToggleKind::NewSkillsDefault | ToggleKind::NewRulesDefault => None,
    }
}

/// Checks that every toggle that turns an item on names one the machine can
/// have: in the baseline, its reported draft, or its unpushed commits. Turning an
/// item off or removing its toggle is allowed for any item, so stale state
/// can always be cleaned up.
pub fn check_items(
    changes: &[Change],
    baseline: &BTreeSet<ItemRef>,
    drafts: &[ItemRef],
    unpushed: &[ItemRef],
) -> Result<()> {
    for c in changes {
        if c.after != Some(true) {
            continue;
        }
        let Some(item) = c.name.as_deref().and_then(|n| item(c.kind, n)) else {
            continue;
        };
        if !baseline.contains(&item) && !drafts.contains(&item) && !unpushed.contains(&item) {
            return Err(ApiError::Unprocessable(format!(
                "`{item}` is absent from the baseline, this machine's draft and its unpushed commits"
            )));
        }
    }
    Ok(())
}

/// The outcome of [`apply`]: the stored assignment and the entries logged.
pub struct Applied {
    pub assignment: Assignment,
    pub revision: u64,
    pub entries: Vec<LogEntry>,
}

impl Applied {
    pub fn snapshot(&self) -> AssignmentSnapshot {
        AssignmentSnapshot { revision: self.revision, assignment: self.assignment.clone() }
    }
}

/// Applies `changes` to `machine` and logs each one that changed something,
/// inside the caller's transaction. With `baseline`, a change to a harness's
/// new-items default first gives every existing item of that kind without a
/// toggle an explicit one with its current value, so the new default only
/// affects items that appear later. Existing items include the destination's
/// latest reported drafts and unpushed commits, even when another machine writes.
pub fn apply(
    tx: &Connection,
    machine: &str,
    author: &str,
    changes: Vec<Change>,
    now: i64,
    baseline: Option<&BTreeSet<ItemRef>>,
) -> Result<Applied> {
    let snapshot = db::snapshot(tx, machine)?.ok_or_else(|| ApiError::NotFound(format!("no machine `{machine}`")))?;
    let existing = baseline
        .map(|baseline| -> Result<BTreeSet<ItemRef>> {
            let report = db::machine(tx, machine)?.and_then(|m| m.report).unwrap_or_default();
            Ok(baseline.iter().cloned().chain(report.drafts).chain(report.unpushed).collect())
        })
        .transpose()?;
    let mut a = snapshot.assignment;
    let mut entries = Vec::new();
    let mut log = |a: &mut Assignment, c: Change, before: Option<bool>| -> Result<()> {
        set(a, &c.harness, c.kind, c.name.as_deref(), c.after);
        let mut entry = LogEntry {
            id: 0,
            at: now,
            author: author.to_string(),
            machine: machine.to_string(),
            harness: c.harness,
            kind: c.kind,
            name: c.name,
            before,
            after: c.after,
            cause: c.cause,
        };
        db::append_log(tx, &mut entry)?;
        entries.push(entry);
        Ok(())
    };
    for c in changes {
        let before = get(&a, &c.harness, c.kind, c.name.as_deref());
        if before == c.after {
            continue;
        }
        if let Some(existing) = &existing {
            for m in materialize_changes(&a, &c.harness, c.kind, existing) {
                log(&mut a, m, None)?;
            }
        }
        log(&mut a, c, before)?;
    }
    let revision = if entries.is_empty() { snapshot.revision } else { db::save_assignment(tx, machine, &a)? };
    Ok(Applied { assignment: a, revision, entries })
}

/// For a change of `kind`'s default in `harness`: an explicit toggle with the
/// current effective value for each existing item of that kind without one.
fn materialize_changes(a: &Assignment, harness: &str, kind: ToggleKind, existing: &BTreeSet<ItemRef>) -> Vec<Change> {
    let h = a.harness(harness);
    let (item_kind, current, toggles) = match kind {
        ToggleKind::NewSkillsDefault => (ToggleKind::Skill, h.new_skills_default, &h.skills),
        ToggleKind::NewRulesDefault => (ToggleKind::Rule, h.new_rules_default, &h.rules),
        ToggleKind::Skill | ToggleKind::Rule => return Vec::new(),
    };
    existing
        .iter()
        .filter_map(|i| match (item_kind, i) {
            (ToggleKind::Skill, ItemRef::Skill(n)) | (ToggleKind::Rule, ItemRef::Rule(n)) => Some(n),
            _ => None,
        })
        .filter(|n| !toggles.contains_key(*n) && crate::check_name("item name", n).is_ok())
        .map(|n| Change {
            harness: harness.to_string(),
            kind: item_kind,
            name: Some(n.clone()),
            after: Some(current),
            cause: LogCause::DefaultMaterialized,
        })
        .collect()
}

/// The change that undoes log entry `id`: restore its `before`.
pub fn undo_change(conn: &Connection, id: i64) -> Result<(String, Change)> {
    let e = db::log_entry(conn, id)?.ok_or_else(|| ApiError::NotFound(format!("no log entry {id}")))?;
    if e.cause == LogCause::MachineRemoved {
        return Err(ApiError::Conflict("a removed machine's assignment can't be restored; join it again".into()));
    }
    if e.before.is_none() && !e.kind.has_name() {
        return Err(ApiError::Conflict(format!("log entry {id} has no earlier value to restore")));
    }
    let change =
        Change { harness: e.harness, kind: e.kind, name: e.name, after: e.before, cause: LogCause::Undo { entry: id } };
    Ok((e.machine, change))
}

/// Harnesses in `reported` that `a` has no entry for, added all off.
pub fn new_harness_changes(a: &Assignment, reported: &[String]) -> Vec<Change> {
    reported
        .iter()
        .filter(|h| !a.harnesses.contains_key(*h) && crate::check_name("harness", h).is_ok())
        .flat_map(|h| {
            [ToggleKind::NewSkillsDefault, ToggleKind::NewRulesDefault].map(|kind| Change {
                harness: h.clone(),
                kind,
                name: None,
                after: Some(false),
                cause: LogCause::HarnessAdded,
            })
        })
        .collect()
}

/// Removes every toggle for an item that is in neither the baseline nor the
/// machine's own reported draft or unpushed commits (where it can still be assigned).
pub fn sweep_changes(
    a: &Assignment,
    baseline: &BTreeSet<ItemRef>,
    drafts: &[ItemRef],
    unpushed: &[ItemRef],
) -> Vec<Change> {
    let mut changes = Vec::new();
    for (harness, h) in &a.harnesses {
        for (toggles, kind) in [(&h.skills, ToggleKind::Skill), (&h.rules, ToggleKind::Rule)] {
            for name in toggles.keys() {
                let item = item(kind, name).expect("item toggle");
                if !baseline.contains(&item) && !drafts.contains(&item) && !unpushed.contains(&item) {
                    changes.push(Change {
                        harness: harness.clone(),
                        kind,
                        name: Some(name.clone()),
                        after: None,
                        cause: LogCause::ItemRemoved,
                    });
                }
            }
        }
    }
    changes
}

/// Drops `machine`'s toggles for items it can no longer have (see
/// [`sweep_changes`]), as the server, inside the caller's transaction. Idempotent;
/// `None` if the machine is gone or nothing changed.
/// With `discarded`, only those explicitly Restore items are considered.
/// A push keeps toggles for items in the machine's latest drafts or unpushed report.
/// Once they leave baseline, drafts and unpushed, an explicit `DraftDiscarded` or
/// a later push's sweep clears those exempt toggles. Status snapshots
/// never sweep: a stash during a pull or an unpushed commit can hide drafts.
pub fn sweep(
    tx: &Connection,
    machine: &str,
    baseline: &BTreeSet<ItemRef>,
    discarded: Option<&[ItemRef]>,
    now: i64,
) -> Result<Option<Applied>> {
    let Some(m) = db::machine(tx, machine)? else {
        return Ok(None);
    };
    let report = m.report.unwrap_or_default();
    let changes = sweep_changes(&m.assignment, baseline, &report.drafts, &report.unpushed)
        .into_iter()
        .filter(|c| {
            discarded
                .is_none_or(|items| c.name.as_deref().and_then(|n| item(c.kind, n)).is_some_and(|i| items.contains(&i)))
        })
        .collect::<Vec<_>>();
    if changes.is_empty() {
        return Ok(None);
    }
    let applied = apply(tx, machine, SERVER_AUTHOR, changes, now, None)?;
    for e in &applied.entries {
        if let Some(item) = e.name.as_deref().and_then(|n| item(e.kind, n)) {
            tracing::info!("dropped the `{item}` toggle on {machine}/{} (item_removed)", e.harness);
        }
    }
    Ok(Some(applied))
}

/// Deletes `machine` (and so its token), logging every toggle it had.
pub fn remove_machine(conn: &mut Connection, machine: &str, author: &str, now: i64) -> Result<Vec<LogEntry>> {
    let tx = conn.transaction()?;
    let a = db::assignment(&tx, machine)?.ok_or_else(|| ApiError::NotFound(format!("no machine `{machine}`")))?;
    let mut entries = Vec::new();
    for (harness, h) in &a.harnesses {
        let defaults = [
            (ToggleKind::NewSkillsDefault, None, h.new_skills_default),
            (ToggleKind::NewRulesDefault, None, h.new_rules_default),
        ];
        let items = h
            .skills
            .iter()
            .map(|(n, v)| (ToggleKind::Skill, Some(n.clone()), *v))
            .chain(h.rules.iter().map(|(n, v)| (ToggleKind::Rule, Some(n.clone()), *v)));
        for (kind, name, value) in defaults.into_iter().chain(items) {
            let mut entry = LogEntry {
                id: 0,
                at: now,
                author: author.to_string(),
                machine: machine.to_string(),
                harness: harness.clone(),
                kind,
                name,
                before: Some(value),
                after: None,
                cause: LogCause::MachineRemoved,
            };
            db::append_log(&tx, &mut entry)?;
            entries.push(entry);
        }
    }
    db::delete_machine(&tx, machine)?;
    tx.commit()?;
    Ok(entries)
}
