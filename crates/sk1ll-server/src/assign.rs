//! Changing a machine's assignment: every change is applied and logged in one
//! transaction, so the log always explains the stored state.
//!
//! Toggles are keyed by Item ID (ADR 0006). A write never checks that its
//! item exists: a toggle for an id no item has does nothing, and only an
//! explicit prune of the orphans removes it.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;
use sk1ll_proto::{
    Assignment, AssignmentSnapshot, CopyMode, Expected, HarnessAssignment, HarnessMapping, ItemId, ItemRef, LogCause,
    LogEntry, MachineAssignmentCopy, ToggleChange, ToggleKind,
};

use crate::db;
use crate::error::{ApiError, Result};

/// Author of changes the server makes on its own.
pub const SERVER_AUTHOR: &str = "server";

/// The longest item id or name a toggle may carry.
const MAX_ITEM: usize = 256;

/// Skills and rules and their Item IDs: the baseline's, or what a machine reported.
pub type Items = [(ItemRef, ItemId)];

/// One toggle to set; `after: None` removes an item's explicit toggle.
#[derive(Debug, Clone)]
pub struct Change {
    pub harness: String,
    pub kind: ToggleKind,
    pub item_id: Option<ItemId>,
    /// For the log; looked up among the known items when absent.
    pub name: Option<String>,
    pub after: Option<bool>,
    pub cause: LogCause,
}

impl Change {
    pub fn from_toggle(c: ToggleChange) -> Self {
        Self {
            harness: c.harness,
            kind: c.kind,
            item_id: c.item_id,
            name: c.name,
            after: c.enabled,
            cause: LogCause::Change,
        }
    }
}

/// Current value of a toggle. Defaults are always `Some` (a harness without
/// an entry has the defaults of a new harness, both off); item toggles are
/// `None` when unset.
pub fn get(a: &Assignment, harness: &str, kind: ToggleKind, id: Option<&ItemId>) -> Option<bool> {
    let h = a.harnesses.get(harness);
    match kind {
        ToggleKind::Skill => h.and_then(|h| h.skills.get(id?).copied()),
        ToggleKind::Rule => h.and_then(|h| h.rules.get(id?).copied()),
        ToggleKind::NewSkillsDefault => Some(a.harness(harness).new_skills_default),
        ToggleKind::NewRulesDefault => Some(a.harness(harness).new_rules_default),
    }
}

fn set(a: &mut Assignment, harness: &str, kind: ToggleKind, id: Option<&ItemId>, value: Option<bool>) {
    match (kind, id, value) {
        (ToggleKind::Skill, Some(id), Some(v)) => a.set_skill(harness, id, v),
        (ToggleKind::Rule, Some(id), Some(v)) => a.set_rule(harness, id, v),
        (ToggleKind::Skill | ToggleKind::Rule, Some(id), None) => {
            if let Some(h) = a.harnesses.get_mut(harness) {
                let toggles = if kind == ToggleKind::Skill { &mut h.skills } else { &mut h.rules };
                toggles.remove(id);
            }
        }
        (ToggleKind::NewSkillsDefault, _, Some(v)) => a.set_new_skills_default(harness, v),
        (ToggleKind::NewRulesDefault, _, Some(v)) => a.set_new_rules_default(harness, v),
        // `validate` rejects the rest: unset defaults, item toggles without an id.
        _ => {}
    }
}

/// Checks a change's shape: ids only for item toggles, values for defaults.
pub fn validate(c: &Change) -> Result<()> {
    crate::check_name("harness", &c.harness)?;
    match (c.kind.has_item(), &c.item_id) {
        (true, Some(id)) if id.to_string().len() > MAX_ITEM => {
            return Err(ApiError::BadRequest(format!("item id `{id}` is too long")));
        }
        (true, Some(_)) => {}
        (true, None) => return Err(ApiError::BadRequest("skill and rule toggles need an `item_id`".into())),
        (false, Some(_)) => return Err(ApiError::BadRequest("defaults take no `item_id`".into())),
        (false, None) if c.after.is_none() => {
            return Err(ApiError::BadRequest("defaults need `enabled`: true or false".into()));
        }
        (false, None) => {}
    }
    if c.name.as_ref().is_some_and(|n| n.len() > MAX_ITEM || n.chars().any(char::is_control)) {
        return Err(ApiError::BadRequest("an item name must be one short line".into()));
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
        AssignmentSnapshot { revision: self.revision, assignment: self.assignment.clone(), registration: None }
    }
}

/// Refuses a write meant for another state of `m`'s assignment: a later
/// revision, or a later registration of the same machine id.
pub fn check_expected(tx: &Connection, m: &db::MachineRecord, expected: &Expected) -> Result<()> {
    if expected.registration.as_ref().is_some_and(|r| *r != m.registration) {
        return Err(ApiError::Conflict(format!(
            "machine `{}` joined the fleet again since; reload its assignment",
            m.id
        )));
    }
    let revision = db::snapshot(tx, &m.id)?.map_or(0, |s| s.revision);
    if revision != expected.revision {
        return Err(ApiError::Conflict(format!(
            "the assignment of `{}` changed since revision {} (now {revision}); reload it",
            m.id, expected.revision
        )));
    }
    Ok(())
}

/// Join-time copying retains explicit toggles even for items the destination
/// has yet to see. Use the same policy here.
///
/// With `unreported`, a target Harness without a column yet is accepted and
/// starts from the all-off column it gets once reported: the Machine itself
/// seeds a Harness it is about to add (`sk1ll harnesses add
/// --copy-assignment-from`) before its next status names it.
pub fn machine_copy_changes(
    source: &Assignment,
    target: &Assignment,
    copy: &MachineAssignmentCopy,
    unreported: bool,
) -> Result<Vec<Change>> {
    let mappings = copy.harnesses.clone().unwrap_or_else(|| {
        source
            .harnesses
            .keys()
            .filter(|name| target.harnesses.contains_key(*name))
            .map(|name| HarnessMapping { from: name.clone(), to: name.clone() })
            .collect()
    });
    let mut destinations = BTreeSet::new();
    let mut changes = Vec::new();
    for HarnessMapping { from, to } in mappings {
        let src = source
            .harnesses
            .get(&from)
            .ok_or_else(|| ApiError::BadRequest(format!("source has no Harness `{from}`")))?;
        let dst = match target.harnesses.get(&to) {
            Some(dst) => dst,
            None if unreported => {
                crate::check_name("harness", &to)?;
                target.harness(&to)
            }
            None => return Err(ApiError::BadRequest(format!("target has no Harness `{to}`"))),
        };
        if !destinations.insert(to.clone()) {
            return Err(ApiError::BadRequest(format!("target Harness `{to}` is mapped more than once")));
        }
        let mut add = |kind, item_id, before, after| {
            if before != after {
                changes.push(Change { harness: to.clone(), kind, item_id, name: None, after, cause: LogCause::Change });
            }
        };
        if copy.mode == CopyMode::Replace {
            add(ToggleKind::NewSkillsDefault, None, Some(dst.new_skills_default), Some(src.new_skills_default));
            add(ToggleKind::NewRulesDefault, None, Some(dst.new_rules_default), Some(src.new_rules_default));
        }
        for (kind, src, dst) in
            [(ToggleKind::Skill, &src.skills, &dst.skills), (ToggleKind::Rule, &src.rules, &dst.rules)]
        {
            for (id, value) in src {
                let before = dst.get(id).copied();
                if copy.mode == CopyMode::Replace || before.is_none() {
                    add(kind, Some(id.clone()), before, Some(*value));
                }
            }
            if copy.mode == CopyMode::Replace {
                for (id, value) in dst.iter().filter(|(id, _)| !src.contains_key(*id)) {
                    add(kind, Some(id.clone()), Some(*value), None);
                }
            }
        }
    }
    Ok(changes)
}

/// The changes that give every item in `harness` the value `enabled`: both
/// defaults and each explicit toggle. Items without a toggle follow the new
/// default, so this needs no list of items and reaches unpulled ones too.
pub fn harness_changes(a: &Assignment, harness: &str, enabled: bool) -> Vec<Change> {
    let h = a.harness(harness);
    let change = |kind, item_id: Option<&ItemId>| Change {
        harness: harness.to_string(),
        kind,
        item_id: item_id.cloned(),
        name: None,
        after: Some(enabled),
        cause: LogCause::Change,
    };
    let toggles = h.skills.keys().map(|id| change(ToggleKind::Skill, Some(id)));
    let toggles = toggles.chain(h.rules.keys().map(|id| change(ToggleKind::Rule, Some(id))));
    [change(ToggleKind::NewSkillsDefault, None), change(ToggleKind::NewRulesDefault, None)]
        .into_iter()
        .chain(toggles)
        .collect()
}

/// Applies `changes` to `machine` and logs each one that changed something,
/// inside the caller's transaction. `baseline` and the machine's reported
/// items name the entries. With `materialize`, a change to a harness's
/// new-items default first gives every one of those items of that kind
/// without a toggle an explicit one with its current value, so the new
/// default only affects items that appear later.
pub fn apply(
    tx: &Connection,
    machine: &str,
    author: &str,
    changes: Vec<Change>,
    now: i64,
    baseline: &Items,
    materialize: bool,
) -> Result<Applied> {
    let m = db::machine(tx, machine)?.ok_or_else(|| ApiError::NotFound(format!("no machine `{machine}`")))?;
    let snapshot = db::snapshot(tx, machine)?.ok_or_else(|| ApiError::NotFound(format!("no machine `{machine}`")))?;
    let reported = m.report.map(|r| r.items).unwrap_or_default();
    let known: BTreeMap<(&ItemId, ToggleKind), &ItemRef> = baseline
        .iter()
        .map(|(item, id)| (id, item))
        .chain(reported.iter().map(|r| (&r.id, &r.item)))
        .filter_map(|(id, item)| {
            let kind = match item {
                ItemRef::Skill(_) => ToggleKind::Skill,
                ItemRef::Rule(_) => ToggleKind::Rule,
                _ => return None,
            };
            Some(((id, kind), item))
        })
        .collect();
    let mut a = snapshot.assignment;
    // Registering a reported harness ([`new_harness_changes`]) adds its
    // entry even though it sets what the harness already had without one,
    // and logs it with no earlier value, so it can't be undone. A user's
    // change to a harness without an entry is an ordinary one: its earlier
    // value is the one it had (the all-off defaults of a new harness).
    let registered: BTreeSet<String> = changes
        .iter()
        .filter(|c| c.cause == LogCause::HarnessAdded && !a.harnesses.contains_key(&c.harness))
        .map(|c| c.harness.clone())
        .collect();
    let mut entries = Vec::new();
    let mut log = |a: &mut Assignment, c: Change, before: Option<bool>| -> Result<()> {
        set(a, &c.harness, c.kind, c.item_id.as_ref(), c.after);
        let name = match c.item_id.as_ref() {
            Some(id) => c
                .name
                .or_else(|| known.get(&(id, c.kind)).map(|item| item.name().to_string()))
                .map_or_else(|| db::last_name(tx, id), |name| Ok(Some(name)))?,
            None => None,
        };
        let mut entry = LogEntry {
            id: 0,
            at: now,
            author: author.to_string(),
            machine: machine.to_string(),
            harness: c.harness,
            kind: c.kind,
            item_id: c.item_id,
            name,
            before,
            after: c.after,
            cause: c.cause,
        };
        db::append_log(tx, &mut entry)?;
        entries.push(entry);
        Ok(())
    };
    for c in changes {
        let before = match c.cause == LogCause::HarnessAdded && registered.contains(&c.harness) {
            true => None,
            false => get(&a, &c.harness, c.kind, c.item_id.as_ref()),
        };
        if before == c.after {
            continue;
        }
        if materialize {
            for m in materialize_changes(&a, &c.harness, c.kind, &known) {
                log(&mut a, m, None)?;
            }
        }
        log(&mut a, c, before)?;
    }
    let revision = if entries.is_empty() { snapshot.revision } else { db::save_assignment(tx, machine, &a)? };
    Ok(Applied { assignment: a, revision, entries })
}

/// For a change of `kind`'s default in `harness`: an explicit toggle with the
/// current effective value for each known item of that kind without one.
fn materialize_changes(
    a: &Assignment,
    harness: &str,
    kind: ToggleKind,
    known: &BTreeMap<(&ItemId, ToggleKind), &ItemRef>,
) -> Vec<Change> {
    let h = a.harness(harness);
    let (item_kind, current, toggles) = match kind {
        ToggleKind::NewSkillsDefault => (ToggleKind::Skill, h.new_skills_default, &h.skills),
        ToggleKind::NewRulesDefault => (ToggleKind::Rule, h.new_rules_default, &h.rules),
        ToggleKind::Skill | ToggleKind::Rule => return Vec::new(),
    };
    known
        .iter()
        .filter(|((id, kind), _)| *kind == item_kind && !toggles.contains_key(*id))
        .map(|((id, _), item)| Change {
            harness: harness.to_string(),
            kind: item_kind,
            item_id: Some((*id).clone()),
            name: Some(item.name().to_string()),
            after: Some(current),
            cause: LogCause::DefaultMaterialized,
        })
        .collect()
}

/// The change that undoes log entry `id`: restore its `before`. `allowed`
/// vets the entry's machine before anything else is checked.
pub fn undo_change(conn: &Connection, id: i64, allowed: impl FnOnce(&str) -> Result<()>) -> Result<(String, Change)> {
    let e = db::log_entry(conn, id)?.ok_or_else(|| ApiError::NotFound(format!("no log entry {id}")))?;
    allowed(&e.machine)?;
    if e.cause == LogCause::MachineRemoved {
        return Err(ApiError::Conflict("a removed machine's assignment can't be restored; join it again".into()));
    }
    if e.before.is_none() && !e.kind.has_item() {
        return Err(ApiError::Conflict(format!("log entry {id} has no earlier value to restore")));
    }
    let change = Change {
        harness: e.harness,
        kind: e.kind,
        item_id: e.item_id,
        name: e.name,
        after: e.before,
        cause: LogCause::Undo { entry: id },
    };
    Ok((e.machine, change))
}

/// Harnesses in `reported` that `a` has no entry for, added all off.
pub fn new_harness_changes(a: &Assignment, reported: &[sk1ll_proto::ReportedHarness]) -> Vec<Change> {
    reported
        .iter()
        .map(|h| &h.name)
        .filter(|h| !a.harnesses.contains_key(*h) && crate::check_name("harness", h).is_ok())
        .flat_map(|h| {
            [ToggleKind::NewSkillsDefault, ToggleKind::NewRulesDefault].map(|kind| Change {
                harness: h.clone(),
                kind,
                item_id: None,
                name: None,
                after: Some(false),
                cause: LogCause::HarnessAdded,
            })
        })
        .collect()
}

/// Every value of one harness entry, as `(kind, item, value)`.
fn values(h: &HarnessAssignment) -> Vec<(ToggleKind, Option<ItemId>, bool)> {
    let defaults = [
        (ToggleKind::NewSkillsDefault, None, h.new_skills_default),
        (ToggleKind::NewRulesDefault, None, h.new_rules_default),
    ];
    let skills = h.skills.iter().map(|(id, v)| (ToggleKind::Skill, Some(id.clone()), *v));
    let rules = h.rules.iter().map(|(id, v)| (ToggleKind::Rule, Some(id.clone()), *v));
    defaults.into_iter().chain(skills).chain(rules).collect()
}

/// Moves `machine`'s harness `from` (toggles and defaults) to `to`,
/// replacing what `to` had, in the caller's transaction: one entry for every
/// value `from` loses and one for every value `to` gets.
pub fn rename_harness(
    tx: &Connection,
    machine: &db::MachineRecord,
    author: &str,
    rename: sk1ll_proto::HarnessRename,
    now: i64,
) -> Result<Applied> {
    let request = serde_json::to_string(&rename).expect("serializing harness rename");
    let sk1ll_proto::HarnessRename { from, to, operation_id, replace } = rename;
    let (from, to, registration) = (from.as_str(), to.as_str(), &machine.registration);
    crate::check_name("harness", from)?;
    crate::check_name("harness", to)?;
    crate::check_name("operation ID", &operation_id)?;
    if from == to {
        return Err(ApiError::BadRequest("the harness keeps its name".into()));
    }
    let id = &machine.id;
    let snapshot = db::snapshot(tx, id)?.ok_or_else(|| ApiError::NotFound(format!("no machine `{id}`")))?;
    let mut a = snapshot.assignment;
    if let Some(prior) = db::harness_rename(tx, registration, &operation_id)? {
        if prior != request {
            return Err(ApiError::Conflict("harness rename operation ID was already used".into()));
        }
        return Ok(Applied { assignment: a, revision: snapshot.revision, entries: Vec::new() });
    }
    if a.harnesses.contains_key(to) && !replace {
        return Err(ApiError::Conflict(format!("harness `{to}` already has an Assignment")));
    }
    db::save_harness_rename(tx, registration, &operation_id, &request)?;
    let moved = a.harnesses.get(from).cloned();
    let before_to = a.harnesses.get(to).cloned();
    if moved.is_none() && before_to.is_none() {
        return Ok(Applied { assignment: a, revision: snapshot.revision, entries: Vec::new() });
    }
    a.rename_harness(from, to);
    let cause = LogCause::HarnessRenamed { from: from.into(), to: to.into() };
    let mut entries = Vec::new();
    let mut log = |harness: &str, kind, item_id: Option<ItemId>, before, after| -> Result<()> {
        if before == after {
            return Ok(());
        }
        let name = item_id.as_ref().map(|id| db::last_name(tx, id)).transpose()?.flatten();
        let mut entry = LogEntry {
            id: 0,
            at: now,
            author: author.into(),
            machine: machine.id.clone(),
            harness: harness.into(),
            kind,
            item_id,
            name,
            before,
            after,
            cause: cause.clone(),
        };
        db::append_log(tx, &mut entry)?;
        entries.push(entry);
        Ok(())
    };
    for (kind, id, value) in moved.iter().flat_map(values) {
        log(from, kind, id, Some(value), None)?;
    }
    let after_to = a.harnesses.get(to);
    let keys: BTreeSet<_> = before_to.iter().chain(after_to).flat_map(values).map(|(k, id, _)| (k, id)).collect();
    for (kind, id) in keys {
        let before = before_to.as_ref().and_then(|a| get_in(a, kind, id.as_ref()));
        let after = after_to.and_then(|a| get_in(a, kind, id.as_ref()));
        log(to, kind, id, before, after)?;
    }
    let revision = db::save_assignment(tx, id, &a)?;
    Ok(Applied { assignment: a, revision, entries })
}

fn get_in(h: &HarnessAssignment, kind: ToggleKind, id: Option<&ItemId>) -> Option<bool> {
    match kind {
        ToggleKind::Skill => h.skills.get(id?).copied(),
        ToggleKind::Rule => h.rules.get(id?).copied(),
        ToggleKind::NewSkillsDefault => Some(h.new_skills_default),
        ToggleKind::NewRulesDefault => Some(h.new_rules_default),
    }
}

/// `(harness, kind, id)` of each of `a`'s toggles whose id is not `known`.
pub fn orphans<'a>(a: &'a Assignment, known: &BTreeSet<&ItemId>) -> Vec<(&'a str, ToggleKind, &'a ItemId)> {
    a.harnesses
        .iter()
        .flat_map(|(harness, h)| {
            let skills = h.skills.keys().map(move |id| (harness.as_str(), ToggleKind::Skill, id));
            skills.chain(h.rules.keys().map(move |id| (harness.as_str(), ToggleKind::Rule, id)))
        })
        .filter(|(_, _, id)| !known.contains(id))
        .collect()
}

/// Deletes `machine` (and so its token), logging every toggle it had.
pub fn remove_machine(conn: &mut Connection, machine: &str, author: &str, now: i64) -> Result<Vec<LogEntry>> {
    let tx = conn.transaction()?;
    let a = db::assignment(&tx, machine)?.ok_or_else(|| ApiError::NotFound(format!("no machine `{machine}`")))?;
    let mut entries = Vec::new();
    for (harness, h) in &a.harnesses {
        for (kind, item_id, value) in values(h) {
            let name = item_id.as_ref().map(|id| db::last_name(&tx, id)).transpose()?.flatten();
            let mut entry = LogEntry {
                id: 0,
                at: now,
                author: author.to_string(),
                machine: machine.to_string(),
                harness: harness.clone(),
                kind,
                item_id,
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
