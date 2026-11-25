//! Fixtures shared by core's integration tests: an isolated scratch
//! directory ([`Scratch`]), two clones of one origin ([`Fleet`]), and what a
//! pull could disturb ([`snapshot`]).
#![allow(dead_code, unused_imports)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use sk1ll_core::draft::{Change, ItemRef, PullOutcome};
use sk1ll_core::{Error, Repo, Result};
use sk1ll_testkit::SkillsRepo;
pub use sk1ll_testkit::{Home, git::git, read, write};

/// An empty temporary directory (its canonical path) and a HOME of its own,
/// entered for core on this thread while it lives: for tests that build
/// their own repos.
pub struct Scratch {
    tmp: tempfile::TempDir,
    root: PathBuf,
    pub home: Home,
    _entered: Box<dyn std::fmt::Debug>,
}

impl Scratch {
    pub fn new() -> Self {
        let home = Home::new();
        let entered: Box<dyn std::fmt::Debug> = Box::new(home.enter());
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        Self { tmp, root, home, _entered: entered }
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    pub fn join(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.root.join(rel)
    }

    /// `Repo::init` at `repo`, with the test identity.
    pub fn init(&self) -> Repo {
        let repo = Repo::init(&self.join("repo")).unwrap();
        sk1ll_testkit::git::configure(repo.root());
        repo
    }

    /// `Repo::init_committed` at `repo`, with the test identity.
    pub fn committed(&self) -> Repo {
        let repo = Repo::init_committed(&self.join("repo")).unwrap();
        sk1ll_testkit::git::configure(repo.root());
        repo
    }
}

pub fn commit_all(dir: &Path, message: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "--quiet", "-m", message]);
}

/// A bare origin on `main` and two clones of it: `a`, with a git identity
/// of its own (`Machine A`), and `b`, with none. Both see the seed: skill
/// `pdf` (`SKILL.md`, `ref.md`), import `web`, rules `git` and `style`, an
/// empty manifest and lockfile. Core's git and state are isolated to the
/// fixture's HOME on this thread while it lives.
pub struct Fleet {
    pub tmp: tempfile::TempDir,
    pub origin: PathBuf,
    pub a: PathBuf,
    pub b: PathBuf,
    _home: Home,
    _entered: Box<dyn std::fmt::Debug>,
}

pub const SEED_PDF: &str = "---\nname: pdf\n---\nPDFs.\n";
pub const SEED_GIT: &str = "Use Conventional Commits.\n";

impl Fleet {
    pub fn new() -> Self {
        let home = Home::new();
        let entered: Box<dyn std::fmt::Debug> = Box::new(home.enter());
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let origin = root.join("origin.git");
        sk1ll_testkit::git::init_bare(&origin);
        let a = root.join("a");
        fs::create_dir(&a).unwrap();
        git(&a, &["init", "--quiet", "-b", "main"]);
        git(&a, &["config", "user.name", "Machine A"]);
        git(&a, &["config", "user.email", "a@example.com"]);
        git(&a, &["config", "commit.gpgsign", "false"]);
        write(&a.join("sk1ll.toml"), "");
        write(&a.join("sk1ll.lock"), "");
        write(&a.join("personal/pdf/SKILL.md"), SEED_PDF);
        write(&a.join("personal/pdf/ref.md"), "reference\n");
        write(&a.join("imported/web/SKILL.md"), "---\nname: web\n---\nWeb.\n");
        write(&a.join("instructions/git.md"), SEED_GIT);
        write(&a.join("instructions/style.md"), "Be terse.\n");
        commit_all(&a, "seed");
        git(&a, &["remote", "add", "origin", origin.to_str().unwrap()]);
        git(&a, &["push", "--quiet", "-u", "origin", "main"]);
        let b = root.join("b");
        let clone = SkillsRepo::clone_from(&origin, &b);
        for key in ["user.name", "user.email"] {
            clone.git(&["config", "--unset", key]);
        }
        Self { tmp, origin, a, b, _home: home, _entered: entered }
    }

    pub fn a(&self) -> Repo {
        Repo::open(&self.a).unwrap()
    }

    pub fn b(&self) -> Repo {
        Repo::open(&self.b).unwrap()
    }

    /// `a` writes `text` at `rel`, commits `item` with `message` and
    /// pushes; returns the new Baseline.
    pub fn publish(&self, rel: &str, text: &str, item: &str, message: &str) -> String {
        let a = self.a();
        write(&self.a.join(rel), text);
        let sha = a.commit(&[self::item(item)], message, None).unwrap();
        a.push().unwrap();
        sha
    }

    /// A copy of `b` at `<dir>/b` (same origin), for a sweep that runs on a
    /// fresh clone each time; its own HOME is entered while it lives.
    pub fn copy_b(&self) -> Copy {
        let home = Home::new();
        let entered: Box<dyn std::fmt::Debug> = Box::new(home.enter());
        let dir = tempfile::tempdir().unwrap();
        let b = dir.path().canonicalize().unwrap().join("b");
        let out = std::process::Command::new("cp").arg("-a").arg(&self.b).arg(&b).output().unwrap();
        assert!(out.status.success(), "{}", sk1ll_testkit::text(&out));
        Copy { _dir: dir, b, home, _entered: entered }
    }
}

/// See [`Fleet::copy_b`].
pub struct Copy {
    _dir: tempfile::TempDir,
    pub b: PathBuf,
    pub home: Home,
    _entered: Box<dyn std::fmt::Debug>,
}

impl Copy {
    pub fn repo(&self) -> Repo {
        Repo::open(&self.b).unwrap()
    }
}

pub fn item(s: &str) -> ItemRef {
    s.parse().unwrap()
}

pub fn items(repo: &Repo) -> Vec<(String, Change)> {
    repo.draft().unwrap().into_iter().map(|d| (d.item.to_string(), d.change)).collect()
}

pub fn items_of(repo: &Repo) -> Vec<ItemRef> {
    repo.draft().unwrap().into_iter().map(|d| d.item).collect()
}

pub fn pull(repo: &Repo) -> Result<PullOutcome> {
    repo.pull(&sk1ll_core::EngineLock::acquire(repo.root())?)
}

pub fn finish(repo: &Repo) -> Result<Option<PullOutcome>> {
    repo.finish_pull(&sk1ll_core::EngineLock::acquire(repo.root())?)
}

pub fn head(root: &Path) -> String {
    git(root, &["rev-parse", "HEAD"])
}

pub fn head_ref(root: &Path) -> String {
    git(root, &["symbolic-ref", "HEAD"])
}

/// The message of an [`Error::Invalid`]; panics on anything else.
#[track_caller]
pub fn invalid<T: std::fmt::Debug>(result: Result<T>) -> String {
    match result {
        Err(Error::Invalid(msg)) => msg,
        other => panic!("expected Error::Invalid, got {other:?}"),
    }
}

/// A working-tree entry, as [`snapshot`] records it.
#[derive(Debug, PartialEq, Eq)]
pub enum Node {
    /// The content, and whether it is executable.
    File(Vec<u8>, bool),
    Link(PathBuf),
}

/// Everything a pull could disturb: files (symlinks and the executable bit
/// included), HEAD and the branch it is on, index and stash.
#[derive(Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub files: BTreeMap<PathBuf, Node>,
    pub head: String,
    pub index: String,
    pub stash: String,
}

pub fn snapshot(root: &Path) -> Snapshot {
    let files = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| e.file_name() != ".git")
        .map(|entry| entry.unwrap())
        .filter(|e| !e.file_type().is_dir())
        .map(|entry| {
            let path = entry.path();
            let meta = fs::symlink_metadata(path).unwrap();
            let node = if meta.file_type().is_symlink() {
                Node::Link(fs::read_link(path).unwrap())
            } else {
                Node::File(fs::read(path).unwrap(), executable(path))
            };
            (path.strip_prefix(root).unwrap().to_path_buf(), node)
        })
        .collect();
    let head = git(root, &["rev-parse", "--symbolic-full-name", "HEAD"]) + " " + &head(root);
    Snapshot { files, head, index: git(root, &["ls-files", "-s"]), stash: git(root, &["stash", "list"]) }
}

pub fn executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

#[cfg(unix)]
pub fn chmod(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// This machine's hostname, as sk1ll finds it (a Machine without an ID).
pub fn hostname() -> String {
    let dir = tempfile::tempdir().unwrap();
    sk1ll_core::MachineConfig::load(&dir.path().join("none.toml")).unwrap().id
}

pub fn pending(repo: &Repo) -> bool {
    repo.pull_pending().unwrap()
}

pub fn keep_pending(repo: &Repo) -> bool {
    repo.root().join(".git/sk1ll-keep.json").exists()
}
