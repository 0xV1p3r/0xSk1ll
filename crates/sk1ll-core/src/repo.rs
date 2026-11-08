use std::fs;
use std::path::{Path, PathBuf};

use crate::instructions::{self, Rule, INSTRUCTIONS_DIR};
use crate::lock::Lockfile;
use crate::manifest::{MANIFEST_FILE, Manifest};
use crate::skill::{self, IMPORTED_DIR, PERSONAL_DIR, Skill};
use crate::{Error, IoExt, Result};

const MANIFEST_TEMPLATE: &str = r#"# 0xSk1ll manifest. The repo holds content only.
# Personal skills live in personal/<name>/SKILL.md.
# Imports are vendored into imported/<name> and pinned in sk1ll.lock.
# Project-agnostic instructions live in instructions/*.md and are rendered
# into each harness's global instructions.
#
# Harnesses are configured per machine, not here: built-in presets are
# detected, and you can adjust them in machine.toml (see `sk1ll harnesses`).
# Which skills and rules each harness gets is assignment (`sk1ll assign`).

# [import.pdf]
# source = "github:owner/repo"
# path = "skills/pdf"
# ref = "main"
"#;

/// A skills repository on disk (`sk1ll.toml` + `personal/` + `imported/` +
/// `instructions/`).
#[derive(Debug, Clone)]
pub struct Repo {
    root: PathBuf,
    pub manifest: Manifest,
    pub lock: Lockfile,
}

impl Repo {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        if !root.join(MANIFEST_FILE).is_file() {
            return Err(Error::Invalid(format!(
                "no {MANIFEST_FILE} in {} (run `sk1ll init`?)",
                root.display()
            )));
        }
        let root = root.canonicalize().at(root)?;
        Ok(Self { manifest: Manifest::load(&root)?, lock: Lockfile::load(&root)?, root })
    }

    /// Opens a Git worktree for recovery without parsing the manifest or lockfile.
    /// Configuration fields are empty; use `open` for content/configuration commands.
    pub fn open_git_only(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let root = root.canonicalize().at(root)?;
        let inside = crate::git::Git::new(&root).args(["rev-parse", "--is-inside-work-tree"]).text()?;
        if inside != "true" {
            return Err(Error::Invalid(format!("{} is not a Git worktree", root.display())));
        }
        Ok(Self { root, manifest: Manifest::default(), lock: Lockfile::default() })
    }

    /// Locates a recovery repo even when its manifest has been deleted: the
    /// nearest ancestor of `cwd` with `sk1ll.toml`, or one that git identifies
    /// as a skills repo (see [`is_skills_repo`](Self::is_skills_repo)), else
    /// `~/skills`. Any other git repo is never picked, so recovery commands
    /// can't discard an unrelated project's changes.
    pub fn locate_git(cwd: &Path) -> Option<PathBuf> {
        cwd.ancestors()
            .find(|d| d.join(MANIFEST_FILE).is_file() || Self::is_skills_repo(d))
            .map(Path::to_path_buf)
            .or_else(|| dirs::home_dir().map(|h| h.join("skills")))
    }

    /// Whether `dir`, which has no `sk1ll.toml` on disk, is the root of a
    /// skills repo in a git worktree: the baseline (`HEAD`) has `sk1ll.toml`
    /// there, or `dir` has the repo layout and the history of `HEAD` has had
    /// a `sk1ll.toml` or `sk1ll.lock` there. Only directories that look like
    /// candidates (a `.git`, or a layout folder) cost a git call.
    fn is_skills_repo(dir: &Path) -> bool {
        let layout = [PERSONAL_DIR, IMPORTED_DIR, INSTRUCTIONS_DIR].map(|d| dir.join(d).is_dir());
        if !dir.join(".git").exists() && !layout.contains(&true) {
            return false;
        }
        let git = || crate::git::Git::new(dir);
        // `HEAD:./path` is relative to `dir`, wherever the worktree's top is.
        if git().args(["cat-file", "-e", &format!("HEAD:./{MANIFEST_FILE}")]).succeeds().unwrap_or(false) {
            return true;
        }
        layout.iter().all(|&d| d)
            && git()
                .literal()
                .args(["log", "-1", "--format=%H", "HEAD", "--", MANIFEST_FILE, crate::lock::LOCK_FILE])
                .text()
                .is_ok_and(|h| !h.is_empty())
    }

    /// Nearest ancestor of `cwd` containing `sk1ll.toml`, else `~/skills`.
    pub fn locate(cwd: &Path) -> Option<PathBuf> {
        cwd.ancestors()
            .find(|d| d.join(MANIFEST_FILE).is_file())
            .map(Path::to_path_buf)
            .or_else(|| dirs::home_dir().map(|h| h.join("skills")))
    }

    /// Creates the repo layout and a starter manifest, and `git init`s it.
    pub fn init(root: &Path) -> Result<Self> {
        let manifest = root.join(MANIFEST_FILE);
        if manifest.exists() {
            return Err(Error::Invalid(format!("{} already exists", manifest.display())));
        }
        for sub in [PERSONAL_DIR, IMPORTED_DIR, INSTRUCTIONS_DIR] {
            let dir = root.join(sub);
            fs::create_dir_all(&dir).at(&dir)?;
            let keep = dir.join(".gitkeep");
            fs::write(&keep, "").at(&keep)?;
        }
        fs::write(&manifest, MANIFEST_TEMPLATE).at(&manifest)?;
        let ignore = root.join(".gitignore");
        if !ignore.exists() {
            fs::write(&ignore, ".sk1ll-stage-*\n").at(&ignore)?;
        }
        if !root.join(".git").exists() {
            // Always on the fleet's branch (commit and the daemon need it),
            // whatever git defaults to (`master` without `init.defaultBranch`).
            // `symbolic-ref` on the unborn HEAD works where `init -b` doesn't (git < 2.28).
            let git = || crate::git::Git::new(root);
            if git().args(["init", "--quiet"]).run().is_ok() {
                git().args(["symbolic-ref", "HEAD", &format!("refs/heads/{}", crate::FLEET_BRANCH)]).run()?;
            }
        }
        Self::open(root)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn personal_dir(&self) -> PathBuf {
        self.root.join(PERSONAL_DIR)
    }

    pub fn imported_dir(&self) -> PathBuf {
        self.root.join(IMPORTED_DIR)
    }

    pub fn skills(&self) -> Result<Vec<Skill>> {
        skill::discover(&self.root)
    }

    pub fn rules(&self) -> Result<Vec<Rule>> {
        instructions::discover(&self.root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        crate::git::Git::new(dir).args(args).run().unwrap();
    }

    fn commit_all(dir: &Path, message: &str) {
        git(dir, &["add", "-A"]);
        git(dir, &["-c", "user.name=T", "-c", "user.email=t@example.com", "commit", "--quiet", "-m", message]);
    }

    fn default_location() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join("skills"))
    }

    /// Commit and the daemon only work on the fleet's branch, so a fresh
    /// repo starts on it even where git's own default is still `master`
    /// (unit tests run git without the developer's config).
    #[test]
    fn init_starts_on_the_fleet_branch_whatever_git_defaults_to() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        Repo::init(&root).unwrap();
        let head = crate::git::Git::new(&root).args(["symbolic-ref", "--short", "HEAD"]).text().unwrap();
        assert_eq!(head, crate::FLEET_BRANCH);
        commit_all(&root, "init");
        let branch = crate::git::Git::new(&root).args(["branch", "--show-current"]).text().unwrap();
        assert_eq!(branch, crate::FLEET_BRANCH);
    }

    #[test]
    fn recovery_discovery_finds_a_deleted_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        Repo::init(&root).unwrap();
        commit_all(&root, "init");
        fs::remove_file(root.join(MANIFEST_FILE)).unwrap();
        fs::create_dir_all(root.join("personal/pdf")).unwrap();
        assert_eq!(Repo::locate_git(&root.join("personal/pdf")), Some(root.clone()));
        assert!(Repo::open_git_only(&root).is_ok());
        assert!(Repo::open(&root).is_err());
        assert!(Repo::open_git_only(tmp.path()).is_err());
    }

    #[test]
    fn recovery_discovery_never_picks_an_unrelated_repo() {
        let tmp = tempfile::tempdir().unwrap();
        // An unrelated project, even one with skills-like folders.
        let project = tmp.path().join("project");
        fs::create_dir_all(project.join("src/deep")).unwrap();
        fs::write(project.join("src/main.rs"), "fn main() {}\n").unwrap();
        git(&project, &["init", "--quiet"]);
        commit_all(&project, "code");
        fs::write(project.join("src/main.rs"), "unsaved edits\n").unwrap();
        for cwd in [project.clone(), project.join("src/deep")] {
            assert_eq!(Repo::locate_git(&cwd), default_location(), "{}", cwd.display());
        }
        for sub in [PERSONAL_DIR, IMPORTED_DIR, INSTRUCTIONS_DIR] {
            fs::create_dir_all(project.join(sub)).unwrap();
        }
        assert_eq!(Repo::locate_git(&project), default_location());
        // Not in a git repo at all.
        let plain = tmp.path().join("plain");
        fs::create_dir_all(&plain).unwrap();
        assert_eq!(Repo::locate_git(&plain), default_location());
        // A skills repo that was never committed can't be told apart either.
        let fresh = tmp.path().join("fresh");
        Repo::init(&fresh).unwrap();
        fs::remove_file(fresh.join(MANIFEST_FILE)).unwrap();
        assert_eq!(Repo::locate_git(&fresh), default_location());
    }

    #[test]
    fn recovery_discovery_recognises_a_skills_repo_by_layout_and_history() {
        let tmp = tempfile::tempdir().unwrap();
        // The skills repo lives in a subdirectory of a larger repo, and its
        // manifest was deleted in a later commit, not just locally.
        let top = tmp.path().join("dotfiles");
        let root = top.join("skills");
        fs::create_dir_all(&top).unwrap();
        git(&top, &["init", "--quiet"]);
        fs::write(top.join("vimrc"), "set nu\n").unwrap();
        Repo::init(&root).unwrap();
        fs::remove_dir_all(root.join(".git")).unwrap();
        commit_all(&top, "skills");
        assert_eq!(Repo::locate_git(&top), default_location());
        fs::remove_file(root.join(MANIFEST_FILE)).unwrap();
        assert_eq!(Repo::locate_git(&root.join(PERSONAL_DIR)), Some(root.clone()));
        commit_all(&top, "drop manifest");
        assert_eq!(Repo::locate_git(&root.join(PERSONAL_DIR)), Some(root.clone()));
        assert_eq!(Repo::locate_git(&top), default_location());
    }
}
