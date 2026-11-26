use super::*;
use std::path::Path;

use sk1ll_core::ItemRef;
use sk1ll_core::sync::{self, Action, ActionKind, Item};

use crate::client::{ConflictTarget, ConflictView, PlacementView};

/// The items a conflict action is about.
pub(super) fn conflict_items(a: &Action) -> Vec<ItemRef> {
    match &a.item {
        Item::Skill { name, .. } => vec![ItemRef::Skill(name.clone())],
        Item::Rule { file, .. } => vec![ItemRef::Rule(file.strip_suffix(".md").unwrap_or(file).to_string())],
        Item::Instructions { rules, .. } if !rules.is_empty() => {
            rules.iter().map(|f| ItemRef::Rule(f.clone())).collect()
        }
        Item::Instructions { .. } => vec![ItemRef::Other(a.dest.display().to_string())],
    }
}

/// `resolver`: of the skills repo at `root`, which tells whether Adopt can
/// place the entry (none: it can't).
pub(super) fn displaced_view(
    entry: sync::displaced::Entry,
    root: &Path,
    resolver: Option<&sync::displaced::Resolver>,
) -> ConflictView {
    let item = displaced_item(&entry, root);
    let adoptable = resolver.is_some_and(|resolver| resolver.adoptable(&entry));
    ConflictView {
        kind: Some("displaced".into()),
        id: Some(entry.id),
        item: Some(item.clone()),
        name: entry.dest.file_name().unwrap_or_default().to_string_lossy().into_owned(),
        items: vec![item],
        harness: entry.harness,
        dest: entry.dest,
        path: Some(entry.path),
        displaced_at: Some(entry.displaced_at),
        detected_at: None,
        reason: None,
        adoptable,
        changed: entry.changed,
    }
}

/// `root`: the skills repo, which a pull's entries are from.
pub(super) fn displaced_item(entry: &sync::displaced::Entry, root: &Path) -> ItemRef {
    let name = entry.dest.file_name().unwrap_or_default().to_string_lossy();
    match entry.kind {
        sync::displaced::Kind::Skill => ItemRef::Skill(name.into_owned()),
        sync::displaced::Kind::Rule => ItemRef::Rule(name.strip_suffix(".md").unwrap_or(&name).to_string()),
        sync::displaced::Kind::Instructions => ItemRef::Other(entry.dest.display().to_string()),
        sync::displaced::Kind::Draft => {
            let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
            match entry.dest.strip_prefix(&root) {
                Ok(path) => ItemRef::for_path(path),
                Err(_) => ItemRef::Other(entry.dest.display().to_string()),
            }
        }
    }
}

pub(super) fn placement_view(a: &Action) -> PlacementView {
    let (item, name) = match &a.item {
        Item::Skill { name, .. } => ("skill", name.clone()),
        Item::Rule { file, .. } => ("rule", file.clone()),
        Item::Instructions { rules, .. } => ("instructions", rules.join(", ")),
    };
    let kind = match a.kind {
        ActionKind::Create => "create",
        ActionKind::Update => "update",
        ActionKind::Remove => "remove",
        ActionKind::Conflict => "conflict",
    };
    PlacementView {
        harness: a.harness.clone(),
        kind: kind.into(),
        item: item.into(),
        name,
        dest: a.dest.clone(),
        reason: a.reason.clone(),
    }
}

pub(super) fn conflict_view(a: &Action) -> ConflictView {
    let (name, adoptable) = match &a.item {
        Item::Skill { name, .. } => (name.clone(), true),
        Item::Rule { file, .. } => (file.strip_suffix(".md").unwrap_or(file).to_string(), true),
        Item::Instructions { .. } => (BLOCK_NAME.to_string(), false),
    };
    ConflictView {
        kind: None,
        id: None,
        item: None,
        path: None,
        displaced_at: None,
        detected_at: modified_at(&a.dest),
        harness: a.harness.clone(),
        name,
        items: conflict_items(a),
        dest: a.dest.clone(),
        reason: a.reason.clone(),
        adoptable,
        changed: false,
    }
}

/// Newest mtime of `path` and everything under it (directories too, so a
/// deleted file counts), without following symlinks or looking at what a
/// skill's hash leaves out. Bounded, so a huge tree can't stall `/conflicts`.
pub(super) fn modified_at(path: &Path) -> Option<u64> {
    let seconds =
        |m: &std::fs::Metadata| m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs());
    let meta = std::fs::symlink_metadata(path).ok()?;
    let (mut newest, mut stack, mut budget) = (seconds(&meta), Vec::new(), 10_000usize);
    if meta.is_dir() {
        stack.push((path.to_path_buf(), 1));
    }
    while let Some((dir, depth)) = stack.pop() {
        for entry in std::fs::read_dir(&dir).ok()?.flatten() {
            budget = budget.checked_sub(1)?;
            if sync::excluded_from_personal_skill(depth, &entry.file_name()) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            newest = newest.max(seconds(&meta));
            if meta.is_dir() {
                stack.push((entry.path(), depth + 1));
            }
        }
    }
    newest
}

pub(super) fn matches_target(a: &Action, t: &ConflictTarget) -> bool {
    a.harness == t.harness
        && match &a.item {
            Item::Skill { name, .. } => *name == t.name,
            Item::Rule { file, .. } => *file == t.name || *file == format!("{}.md", t.name),
            Item::Instructions { rules, .. } => t.name == BLOCK_NAME || rules.contains(&t.name),
        }
}
