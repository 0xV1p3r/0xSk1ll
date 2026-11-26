#![cfg(unix)]

mod sync_common;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::time::Duration;

use sk1ll_core::failpoints::{self, Action as Fault, Hit, Point};
use sk1ll_core::sync::{self, IndexEntry, PLACEMENT_MARKER, displaced};
use sk1ll_core::{ActionKind, Assignment, EngineLock, InstructionsFormat, LinkMode, PlanOptions, Repo, Result};
use sk1ll_testkit::{child, read, write};

use sync_common::{
    Placed, Target, assert_changed, inode, instructions_harness, read_index, reported, sk1ll_leftovers, with_hook,
};

const HARNESS_EDIT: &str = "Hand edit in the harness.";
const REPO_EDIT: &str = "Unpublished repo edit.";
const CONCURRENT_EDIT: &str = "Concurrent editor save.";
const UPDATED: &str = "Updated in the repo.\n";

#[derive(Clone, Copy, Debug)]
enum Shape {
    FirstCopy,
    FirstLink,
    Update,
    CopyToLink,
    LinkToCopy,
    Remove,
    RemoveLink,
    Force,
    AdoptSkill,
    AdoptRule,
    BlockCreate,
    BlockAppend,
    BlockUpdate,
    LinkedBlockUpdate,
    BlockRemove,
    RuleCreate,
    RuleUpdate,
    RuleRemove,
}

impl Shape {
    fn is_skill(self) -> bool {
        matches!(
            self,
            Self::FirstCopy
                | Self::FirstLink
                | Self::Update
                | Self::CopyToLink
                | Self::LinkToCopy
                | Self::Remove
                | Self::RemoveLink
                | Self::Force
                | Self::AdoptSkill
        )
    }

    fn is_adopt(self) -> bool {
        matches!(self, Self::AdoptSkill | Self::AdoptRule)
    }

    fn is_block(self) -> bool {
        matches!(
            self,
            Self::BlockCreate | Self::BlockAppend | Self::BlockUpdate | Self::LinkedBlockUpdate | Self::BlockRemove
        )
    }
}

struct Scenario {
    shape: Shape,
    placed: Placed,
}

impl std::ops::Deref for Scenario {
    type Target = Placed;
    fn deref(&self) -> &Placed {
        &self.placed
    }
}

impl Scenario {
    fn new(shape: Shape) -> Self {
        use Shape::*;
        let target = match shape {
            FirstLink | LinkToCopy | RemoveLink => Target::Link,
            shape if shape.is_skill() => Target::Copy,
            shape if shape.is_block() => Target::Block,
            _ => Target::Rules,
        };
        let mut placed = Placed::new(target);
        let _home = placed.home.enter();
        if matches!(shape, BlockAppend) {
            write(&placed.dest, "# User instructions\n");
        } else if matches!(shape, LinkedBlockUpdate) {
            let target = placed.home.path().join("dotfiles/AGENTS.md");
            write(&target, "# User instructions\n");
            symlink(target, &placed.dest).unwrap();
        }
        if !matches!(shape, FirstCopy | FirstLink | RuleCreate | BlockCreate | BlockAppend) {
            placed.sync();
        }
        match shape {
            CopyToLink => placed.harnesses.get_mut("h").unwrap().mode = LinkMode::Symlink,
            LinkToCopy => placed.harnesses.get_mut("h").unwrap().mode = LinkMode::Copy,
            Update | RuleUpdate | BlockUpdate | LinkedBlockUpdate => placed.update(UPDATED),
            Remove | RemoveLink | RuleRemove | BlockRemove => placed.unassign(),
            Force | AdoptSkill | AdoptRule => {
                write(&placed.file(&placed.dest), &format!("{HARNESS_EDIT}\n"));
                placed.update(&format!("{REPO_EDIT}\n"));
            }
            FirstCopy | FirstLink | RuleCreate | BlockCreate | BlockAppend => {}
        }
        Self { shape, placed }
    }

    fn plan(&self, repo: &Repo, force: bool) -> Result<Vec<sk1ll_core::Action>> {
        sync::plan(repo, &self.harnesses, &self.assignment, PlanOptions { force })
    }

    fn run(&self) -> Result<()> {
        let actions = self.plan(&self.repo, matches!(self.shape, Shape::Force))?;
        for action in actions {
            if self.shape.is_adopt() {
                sync::adopt(&self.repo, &action)?;
            } else {
                sync::apply(&self.repo, &action)?;
            }
        }
        Ok(())
    }

    fn restart(&self, retry_adopt: bool, label: &str) {
        let repo = Repo::open(self.repo.root()).unwrap();
        let lock = EngineLock::acquire(repo.root()).unwrap();
        sync::recover(Some(&lock), &self.harnesses).unwrap_or_else(|e| panic!("{label}: recover: {e}"));
        // A crash after detach can make the repo item look deleted.
        // Recover the approved Adopt from the harness placement in either case.
        if retry_adopt && self.shape.is_adopt() {
            let placements = sync::placed(&repo, "h", &self.harnesses["h"]).unwrap();
            if let Some(conflict) = placements.iter().find(|a| a.dest == self.dest && a.kind == ActionKind::Conflict) {
                sync::adopt(&repo, conflict).unwrap_or_else(|e| panic!("{label}: resume Adopt: {e}"));
            }
        }
        for action in self.plan(&repo, false).unwrap_or_else(|e| panic!("{label}: replan: {e}")) {
            if action.kind != ActionKind::Conflict {
                sync::apply(&repo, &action).unwrap_or_else(|e| panic!("{label}: apply: {e}"));
            }
        }
        let remaining = self.plan(&repo, false).unwrap();
        assert!(remaining.iter().all(|a| a.kind == ActionKind::Conflict), "{label}: {remaining:?}");
        if retry_adopt && (remaining.is_empty() || !matches!(self.shape, Shape::Force)) {
            assert!(remaining.is_empty(), "{label}: {remaining:?}");
            self.assert_desired(label);
        }
        self.assert_index(label);
    }

    fn assert_desired(&self, label: &str) {
        match self.shape {
            Remove | RemoveLink | BlockRemove | RuleRemove => {
                assert!(!self.dest.exists() && !self.dest.is_symlink(), "{label}: removed");
            }
            shape if shape.is_skill() => {
                assert_eq!(self.dest.is_symlink(), matches!(shape, FirstLink | CopyToLink), "{label}");
                assert_eq!(read(&self.file(&self.dest)), read(&self.file(&self.source)), "{label}");
            }
            shape => {
                let expected = match shape {
                    RuleUpdate | BlockUpdate | LinkedBlockUpdate => UPDATED,
                    AdoptRule => HARNESS_EDIT,
                    _ => "Use CC.",
                };
                assert!(contains(&self.dest, expected), "{label}: {expected}");
            }
        }
    }

    fn assert_index(&self, label: &str) {
        if self.shape.is_block() {
            return;
        }
        let index = read_index(self.dest.parent().unwrap());
        assert_eq!(index["pending"], serde_json::json!({}), "{label}");
        let entries = index["entries"].as_object().unwrap();
        for (name, entry) in entries {
            let dest = self.dest.parent().unwrap().join(name);
            let anchors = dest.parent().unwrap().join(".sk1ll-state/anchors");
            match serde_json::from_value::<IndexEntry>(entry.clone()).unwrap() {
                IndexEntry::Copy { token, .. } => {
                    let marker = dest.join(PLACEMENT_MARKER);
                    assert_eq!(read(&marker), token, "{label}");
                    assert_eq!(inode(&marker), inode(&anchors.join(format!("copy-{token}"))), "{label}");
                }
                IndexEntry::Link { target, .. } => {
                    let link = fs::read_link(&dest).unwrap();
                    if link != target {
                        // An edited symlink is a conflict for as long as its
                        // previous target is still recorded.
                        let actions = self.plan(&self.repo, false).unwrap();
                        assert!(actions.iter().any(|a| a.dest == dest && a.kind == ActionKind::Conflict), "{label}");
                    }
                }
                IndexEntry::Rule { .. } => {
                    let (dev, ino) = inode(&dest);
                    assert_eq!(entry["file"], serde_json::json!({"dev": dev, "ino": ino}), "{label}");
                    assert_eq!(inode(&anchors.join(format!("rule-{dev}-{ino}"))), (dev, ino), "{label}");
                }
            }
        }
        let placements = sync::placed(&self.repo, "h", &self.harnesses["h"]).unwrap();
        if placements.iter().any(|a| a.dest == self.dest && a.kind == ActionKind::Remove) {
            assert!(entries.contains_key(self.dest.file_name().unwrap().to_str().unwrap()), "{label}");
        }
    }

    fn assert_preserved(&self, needle: &str, label: &str) {
        let entries = displaced::list().unwrap();
        let live = [&self.dest, &self.source].into_iter().any(|path| contains(&self.file(path), needle));
        let kept = entries.iter().any(|entry| contains(&self.file(&entry.path), needle));
        assert!(live || kept, "{label}: lost {needle:?}, recovery {entries:?}");
    }

    fn assert_original_edits(&self, label: &str) {
        if matches!(self.shape, Shape::Force) || self.shape.is_adopt() {
            self.assert_preserved(HARNESS_EDIT, label);
            self.assert_preserved(REPO_EDIT, label);
        }
        if matches!(self.shape, Shape::BlockAppend | Shape::LinkedBlockUpdate) {
            self.assert_preserved("# User instructions", label);
        }
    }

    fn concurrent_edit(&self) -> impl FnOnce() + '_ {
        let edit_path = if self.shape.is_adopt() { &self.source } else { &self.dest };
        let file = self.file(edit_path);
        let is_skill_link = self.shape.is_skill() && edit_path.is_symlink();
        let mut open = (!is_skill_link).then(|| OpenOptions::new().append(true).open(&file).ok()).flatten();
        let bookmark = self.home.path().join("unmanaged");
        move || {
            if let Some(open) = &mut open {
                writeln!(open, "{CONCURRENT_EDIT}").unwrap();
            } else if self.shape.is_skill() && (edit_path.is_symlink() || is_skill_link && !edit_path.exists()) {
                // Save a new link without editing the repo skill it targets.
                write(&bookmark.join("SKILL.md"), CONCURRENT_EDIT);
                let stage = edit_path.with_extension("editor");
                symlink(&bookmark, &stage).unwrap();
                fs::rename(stage, edit_path).unwrap();
            } else if let Ok(mut published) = OpenOptions::new().append(true).open(&file) {
                writeln!(published, "{CONCURRENT_EDIT}").unwrap();
            } else {
                write(&file, CONCURRENT_EDIT);
            }
        }
    }
}

fn contains(path: &Path, needle: &str) -> bool {
    fs::read_to_string(path).is_ok_and(|text| text.contains(needle))
}

fn hits(shape: Shape) -> Vec<Hit> {
    let case = Scenario::new(shape);
    let _home = case.home.enter();
    let recording = failpoints::record();
    case.run().unwrap();
    let hits = recording.hits();
    drop(recording);
    assert!(!hits.is_empty(), "{shape:?}");
    assert!(case.plan(&case.repo, false).unwrap().is_empty(), "{shape:?}");
    hits
}

fn crash_sweep(shapes: &[Shape]) {
    for &shape in shapes {
        for (n, hit) in hits(shape).into_iter().enumerate() {
            let case = Scenario::new(shape);
            let _home = case.home.enter();
            let label = format!("{shape:?}, hit {n} {}", hit.point.name());
            let repo_version = fs::read(case.file(&case.source)).ok();
            let crash = {
                let _fault = failpoints::crash_at(n);
                failpoints::catch(|| case.run()).unwrap_err()
            };
            assert_eq!(crash.point, hit.point, "{label}");
            case.assert_original_edits(&label);
            if shape.is_adopt() && hit.point == Point::AfterRename {
                // The repo version, moved aside for the adoption: owned by
                // the repo, for its own path, and never pruned meanwhile.
                let [entry] = &displaced::list().unwrap()[..] else { panic!("{label}: one entry") };
                assert_eq!((entry.harness.as_str(), &entry.dest), ("repo", &case.source), "{label}");
                assert!(!displaced::prune_with(Duration::ZERO).unwrap().contains(&entry.id), "{label}");
                assert_eq!(fs::read(case.file(&entry.path)).ok(), repo_version, "{label}");
            }
            let backups: Vec<_> = displaced::list()
                .unwrap()
                .into_iter()
                .map(|entry| {
                    let payload = fs::read(case.file(&entry.path)).ok();
                    (entry, payload)
                })
                .collect();
            case.restart(true, &label);
            case.assert_original_edits(&label);
            for (entry, payload) in backups {
                assert_eq!(displaced::get(&entry.id).unwrap().path, entry.path, "{label}");
                assert_eq!(fs::read(case.file(&entry.path)).ok(), payload, "{label}: recovery {}", entry.id);
            }
        }
    }
}

fn edit_sweep(shapes: &[Shape]) {
    for &shape in shapes {
        for (n, hit) in hits(shape).into_iter().enumerate() {
            let case = Scenario::new(shape);
            let _home = case.home.enter();
            let label = format!("{shape:?}, hit {n} {}", hit.point.name());
            let mut edit = Some(case.concurrent_edit());
            let mut seen = 0;
            let _ = failpoints::scoped(
                |point, _| {
                    if seen == n {
                        assert_eq!(point, hit.point, "{label}");
                        edit.take().unwrap()();
                    }
                    seen += 1;
                    Fault::Continue
                },
                || case.run(),
            );
            assert!(edit.is_none(), "{label}: edit ran");
            case.assert_preserved(CONCURRENT_EDIT, &label);
            case.assert_original_edits(&label);
            case.restart(false, &label);
            case.assert_preserved(CONCURRENT_EDIT, &label);
            case.assert_original_edits(&label);
        }
    }
}

use Shape::*;

const SKILLS: &[Shape] = &[FirstCopy, FirstLink, Update, CopyToLink, LinkToCopy, Remove, RemoveLink, Force];
const INSTRUCTIONS: &[Shape] =
    &[BlockCreate, BlockAppend, BlockUpdate, LinkedBlockUpdate, BlockRemove, RuleCreate, RuleUpdate, RuleRemove];
const ADOPT: &[Shape] = &[AdoptSkill, AdoptRule];

#[test]
fn a_crash_at_any_skill_placement_step_keeps_edits_and_allows_retry() {
    crash_sweep(SKILLS);
}

#[test]
fn a_crash_at_any_instructions_step_keeps_edits_and_allows_retry() {
    crash_sweep(INSTRUCTIONS);
}

#[test]
fn a_crash_at_any_adopt_step_keeps_both_versions_and_allows_retry() {
    crash_sweep(ADOPT);
}

#[test]
fn an_edit_at_any_skill_placement_step_survives_sync_and_retry() {
    edit_sweep(SKILLS);
}

#[test]
fn an_edit_at_any_instructions_step_survives_sync_and_retry() {
    edit_sweep(INSTRUCTIONS);
}

#[test]
fn an_edit_at_any_adopt_step_survives_adopt_and_retry() {
    edit_sweep(ADOPT);
}

/// An editor's atomic save over a managed rule, or a regular file that
/// appears at a vacant rule, block or skill link destination, after sync's
/// last check stays live byte for byte, and sync stops with a conflict.
#[test]
fn an_atomic_save_or_newcomer_after_the_last_check_stays_live() {
    let save = |dest: &Path| {
        let stage = dest.with_extension("editor");
        write(&stage, "Editor save.\n");
        fs::rename(stage, dest).unwrap();
    };
    let cases = [
        (RuleUpdate, Point::AfterCheck),
        (RuleUpdate, Point::AfterAside),
        (RuleCreate, Point::AfterCheck),
        (BlockCreate, Point::AfterCheck),
        (FirstLink, Point::AfterCheck),
    ];
    for (shape, point) in cases {
        let case = Scenario::new(shape);
        let _home = case.home.enter();
        let label = format!("{shape:?} at {}", point.name());
        let before = fs::read(&case.dest).ok();
        let result = with_hook(point, save, || case.run());
        assert!(fs::symlink_metadata(&case.dest).unwrap().is_file(), "{label}");
        assert_eq!(read(&case.dest), "Editor save.\n", "{label}");
        if point == Point::AfterAside {
            let entry = reported(result);
            assert_eq!((&entry.dest, fs::read(&entry.path).ok()), (&case.dest, before), "{label}");
        } else {
            assert_changed(result, &case.dest);
            assert!(displaced::list().unwrap().is_empty(), "{label}");
        }
        let parent = case.dest.parent().unwrap();
        assert!(sk1ll_leftovers(parent).is_empty(), "{label}: {:?}", sk1ll_leftovers(parent));
        if !shape.is_block() {
            let kinds: Vec<_> = case.plan(&case.repo, false).unwrap().into_iter().map(|a| (a.dest, a.kind)).collect();
            assert_eq!(kinds, [(case.dest.clone(), ActionKind::Conflict)], "{label}");
        }
    }
}

#[test]
fn process_death_after_detach_preserves_both_editor_saves() {
    if let Some(root) = child::dir() {
        let repo = Repo::open(root.join("repo")).unwrap();
        let dirs = sk1ll_core::paths::Dirs { state: root.join("state"), cache: root.join("cache") };
        let _dirs = failpoints::dirs(dirs);
        let harnesses = instructions_harness(&root.join("rules"), InstructionsFormat::Rules);
        let update = sync::plan(&repo, &harnesses, &Assignment::default(), PlanOptions::default()).unwrap().remove(0);
        let _exit = failpoints::install(|point, _| {
            if point == Point::AfterRename { Fault::Exit(child::CRASHED) } else { Fault::Continue }
        });
        sync::apply(&repo, &update).unwrap();
        panic!("child did not reach AfterRename");
    }
    let case = Scenario::new(RuleUpdate);
    let state = case.home.path().join("state");
    let _home = case.home.enter();
    let _dirs = failpoints::dirs(sk1ll_core::paths::Dirs { state, cache: case.home.cache_dir() });
    let mut cmd = child::command("process_death_after_detach_preserves_both_editor_saves", case.home.path());
    case.home.apply(&mut cmd);
    let output = cmd.output().unwrap();
    assert_eq!(output.status.code(), Some(child::CRASHED), "{}", sk1ll_testkit::text(&output));
    assert!(!case.dest.exists());
    let backup = displaced::list().unwrap().remove(0);
    write(&case.dest, "New live file.\n");
    write(&backup.path, "Edit to backup.\n");

    case.restart(false, "restart after process death");
    assert_eq!(read(&case.dest), "New live file.\n");
    assert_eq!(read(&backup.path), "Edit to backup.\n");
    assert!(displaced::get(&backup.id).unwrap().changed);
}
