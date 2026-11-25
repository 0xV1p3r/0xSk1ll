#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use sk1ll_core::failpoints::{self, Point};
use sk1ll_core::sync::*;
use sk1ll_core::{Assignment, Error, Harness, InstructionsFormat, ItemId, LinkMode, Repo, Result};
use sk1ll_testkit::{Home, SkillsRepo, write};

/// The Item ID of the repo's skill `name`.
pub fn skill_id(repo: &Repo, name: &str) -> ItemId {
    item_id_of(repo, &sk1ll_core::ItemRef::Skill(name.into()))
}

/// The Item ID of the repo's rule `name`.
pub fn rule_id(repo: &Repo, name: &str) -> ItemId {
    item_id_of(repo, &sk1ll_core::ItemRef::Rule(name.into()))
}

fn item_id_of(repo: &Repo, item: &sk1ll_core::ItemRef) -> ItemId {
    let ids = Repo::open(repo.root()).unwrap().item_ids().unwrap();
    ids.into_iter().find(|(found, _)| found == item).and_then(|(_, id)| id).expect("an item with an Item ID")
}

pub fn edit_rule_body(path: &Path, body: &str) {
    let before = fs::read_to_string(path).unwrap();
    let (metadata, _) = sk1ll_core::skill::split_frontmatter(&before);
    let after = metadata.map_or_else(|| body.to_string(), |metadata| format!("---\n{metadata}---\n{body}"));
    write(path, &after);
}

/// Runs `f` with `edit` hooked in at `point`, once.
pub fn with_hook<T>(point: Point, edit: impl FnOnce(&Path) + 'static, f: impl FnOnce() -> T) -> T {
    let _hook = failpoints::once(point, edit);
    f()
}

/// Runs `f` with `edit` hooked in at every point.
pub fn with_hooks<T>(edit: impl FnMut(Point, &Path) + 'static, f: impl FnOnce() -> T) -> T {
    let _hook = failpoints::observe(edit);
    f()
}

/// Stop after the selected hook, before any caller can settle the intent.
pub fn interrupt(repo: &Repo, action: &Action, point: Point) {
    let _hook = failpoints::install(
        move |at, _| {
            if at == point { failpoints::Action::Crash } else { failpoints::Action::Continue }
        },
    );
    let crash = failpoints::catch(|| apply(repo, action)).expect_err("sync reached the interruption point");
    assert_eq!(crash.point, point);
}

/// A copied skill edited in the harness, and the conflict to adopt it.
pub fn adoptable(tmp: &Home, repo: &Repo, harnesses: &BTreeMap<String, Harness>) -> (PathBuf, Action) {
    let assignment = Assignment::default();
    apply(repo, &plan(repo, harnesses, &assignment, PlanOptions::default()).unwrap()[0]).unwrap();
    let dest = tmp.path().join("skills/pdf");
    write(&dest.join("SKILL.md"), "Edited in the harness.\n");
    let actions = plan(repo, harnesses, &assignment, PlanOptions::default()).unwrap();
    assert_eq!(actions[0].kind, ActionKind::Conflict);
    (dest, actions.into_iter().next().unwrap())
}

pub fn adopt_test_entry(repo: &Repo, id: &str) -> Result<PathBuf> {
    displaced::adopt(repo, id, &sk1ll_core::EngineLock::acquire(repo.root())?)
}

/// The recovery entry a displacing apply reported.
pub fn reported(result: Result<()>) -> displaced::Entry {
    match result {
        Err(Error::Displaced { kept, id, .. }) => {
            let entry = displaced::get(&id).unwrap();
            assert_eq!(entry.path, kept);
            assert!(entry.changed, "{entry:?}");
            assert_eq!(displaced::changed().unwrap(), std::slice::from_ref(&entry));
            entry
        }
        other => panic!("expected the edit to be reported as displaced, got {other:?}"),
    }
}

/// What sk1ll left in `dir` besides its recovery area.
pub fn sk1ll_leftovers(dir: &Path) -> Vec<String> {
    let names = |dir: &Path| -> Vec<String> {
        let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
        entries.map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect()
    };
    let mut found: Vec<_> = names(dir)
        .into_iter()
        .filter(|n| n != TMP_DIR && ["sk1ll-tmp", "sk1ll-old", "sk1ll-stage"].iter().any(|s| n.contains(s)))
        .collect();
    let scratch = names(&dir.join(TMP_DIR)).into_iter().filter(|n| n != "displaced");
    found.extend(scratch.map(|n| format!("{TMP_DIR}/{n}")));
    found
}

/// What one displacement test works on: a harness set up and synced
/// once, then a plan that displaces `dest`.
pub struct Displacement {
    pub tmp: Home,
    pub repo: Repo,
    pub harnesses: BTreeMap<String, Harness>,
    pub assignment: Assignment,
    /// The planned destination.
    pub dest: PathBuf,
    /// A file of the displaced content, that writers may hold open.
    pub file: PathBuf,
    /// The directory whose `.sk1ll-tmp/displaced` must hold the entry.
    pub holder: PathBuf,
}

impl Displacement {
    /// `case`: "skill update", "skill removal", "link removal", "rule
    /// update", "rule removal" or "block update".
    pub fn new(case: &str) -> Self {
        let (kind, change) = case.split_once(' ').unwrap();
        let remove = change == "removal";
        let mut assignment = Assignment::default();
        let synced = |repo: &Repo, harnesses: &BTreeMap<String, Harness>, assignment: &Assignment| {
            for a in plan(repo, harnesses, assignment, PlanOptions::default()).unwrap() {
                apply(repo, &a).unwrap();
            }
        };
        match kind {
            "skill" | "link" => {
                let mode = if kind == "link" { LinkMode::Symlink } else { LinkMode::Copy };
                let (tmp, repo, harnesses) = skill_setup(mode);
                let _home = tmp.enter();
                synced(&repo, &harnesses, &assignment);
                write(&repo.personal_dir().join("pdf/SKILL.md"), "Updated source.\n");
                assignment.set_skill("h", &skill_id(&repo, "pdf"), !remove);
                let dest = tmp.path().join("skills/pdf");
                let (file, holder) = (dest.join("SKILL.md"), tmp.path().join("skills"));
                Self { tmp, repo, harnesses, assignment, dest, file, holder }
            }
            _ => {
                let format = if kind == "rule" { InstructionsFormat::Rules } else { InstructionsFormat::Block };
                let (tmp, repo, harnesses, path) = instructions_setup(format);
                let _home = tmp.enter();
                synced(&repo, &harnesses, &assignment);
                edit_rule_body(&repo.root().join("instructions/git.md"), "Use CC always.\n");
                assignment.set_rule("h", &rule_id(&repo, "git"), !remove);
                let dest = if kind == "rule" { path.join("git.md") } else { path.clone() };
                let holder = dest.parent().unwrap().to_path_buf();
                Self { tmp, repo, harnesses, assignment, file: dest.clone(), dest, holder }
            }
        }
    }

    pub fn action(&self) -> Action {
        let actions = plan(&self.repo, &self.harnesses, &self.assignment, PlanOptions::default()).unwrap();
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert!(matches!(actions[0].kind, ActionKind::Update | ActionKind::Remove), "{actions:?}");
        actions.into_iter().next().unwrap()
    }

    pub fn area(&self) -> PathBuf {
        displaced::area(&self.holder)
    }

    /// The one recovery entry for `dest`.
    pub fn entry(&self) -> displaced::Entry {
        let entries: Vec<_> = displaced::list().unwrap().into_iter().filter(|e| e.dest == self.dest).collect();
        assert_eq!(entries.len(), 1, "{entries:?}");
        entries.into_iter().next().unwrap()
    }

    /// The displaced copy of [`Self::file`] in `entry`.
    pub fn kept_file(&self, entry: &displaced::Entry) -> PathBuf {
        match self.file.strip_prefix(&self.dest) {
            Ok(rel) if !rel.as_os_str().is_empty() => entry.path.join(rel),
            _ => entry.path.clone(),
        }
    }
}

pub const INDEX_DIR: &str = ".sk1ll-state";
pub const TMP_DIR: &str = ".sk1ll-tmp";

pub fn skill_setup(mode: LinkMode) -> (Home, Repo, BTreeMap<String, Harness>) {
    let home = Home::new();
    let _isolation = home.enter();
    let fixture = SkillsRepo::init(home.path().join("repo"));
    fixture.skill(sk1ll_testkit::Skill::new("pdf").description(None).body("PDFs.\n").id("pdf-id"));
    fixture.commit_all("Add PDF skill");
    let harness =
        Harness { skills: Some(home.path().join("skills").display().to_string()), mode, ..Harness::default() };
    (home, fixture.open(), BTreeMap::from([("h".into(), harness)]))
}

pub fn instructions_setup(format: InstructionsFormat) -> (Home, Repo, BTreeMap<String, Harness>, PathBuf) {
    let home = Home::new();
    let _isolation = home.enter();
    let fixture = SkillsRepo::init(home.path().join("repo"));
    fixture.rule(sk1ll_testkit::Rule::new("git").body("Use CC.\n").id("git-id"));
    fixture.commit_all("Add Git rule");
    let dest = home.path().join(if format == InstructionsFormat::Rules { "rules" } else { "AGENTS.md" });
    let harnesses = instructions_harness(&dest, format);
    (home, fixture.open(), harnesses, dest)
}

pub fn instructions_harness(path: &Path, format: InstructionsFormat) -> BTreeMap<String, Harness> {
    let harness =
        Harness { instructions: Some(path.display().to_string()), instructions_format: format, ..Harness::default() };
    BTreeMap::from([("h".into(), harness)])
}

/// Inspect the published file format. A missing index represents an empty directory.
pub fn read_index(dir: &Path) -> serde_json::Value {
    let path = dir.join(INDEX_PATH);
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => empty_index(),
        Err(e) => panic!("{}: {e}", path.display()),
    }
}

pub fn empty_index() -> serde_json::Value {
    serde_json::json!({"version": 3, "entries": {}, "pending": {}})
}

pub fn index_entry(dest: &Path) -> Option<IndexEntry> {
    let index = read_index(dest.parent().unwrap());
    let name = dest.file_name().unwrap().to_str().unwrap();
    let entry = &index["entries"][name];
    (!entry.is_null()).then(|| serde_json::from_value(entry.clone()).unwrap())
}

/// Publish an external editor's save over the index.
pub fn replace_index(dir: &Path, index: &serde_json::Value) {
    let path = dir.join(INDEX_PATH);
    let stage = path.with_extension("editor");
    write(&stage, &index.to_string());
    fs::rename(stage, path).unwrap();
}

pub fn assert_copy_recorded(dest: &Path) {
    let Some(IndexEntry::Copy { token, .. }) = index_entry(dest) else {
        panic!("{} has no copy entry", dest.display())
    };
    assert_eq!(fs::read_to_string(dest.join(PLACEMENT_MARKER)).unwrap(), token);
}

#[cfg(unix)]
pub fn inode(path: &Path) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    let meta = path.symlink_metadata().unwrap();
    (meta.dev(), meta.ino())
}

pub fn assert_changed(result: Result<()>, path: &Path) {
    let message = result.unwrap_err().to_string();
    assert!(message.contains("changed since the plan") && message.contains(&*path.to_string_lossy()), "{message}");
}

pub fn copy_test_skill(src: &Path, dest: &Path) -> Result<()> {
    copy_skill_for_diff(src, dest, sk1ll_core::Origin::Personal)
}

pub fn with_read_hook<T>(mut edit: impl FnMut(&Path), f: impl FnOnce() -> T) -> T {
    failpoints::scoped(
        |point, path| {
            if point == Point::HashMidRead {
                edit(path);
            }
            failpoints::Action::Continue
        },
        f,
    )
}
