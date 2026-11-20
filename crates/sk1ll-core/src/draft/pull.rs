//! [`Repo::pull`]. Fetching and replaying unpushed commits happen in the
//! object store; the working tree then changes path by path, only where the
//! new commit differs from `HEAD`. Harnesses read skills straight from the
//! working tree, so pull never moves a draft file out of the way, not even
//! for a moment, and `HEAD` stays on the branch throughout.
//!
//! Once the last checks pass, pull writes a journal (see [`journal`]) and
//! from then on only rolls forward: the files, then the index, then the
//! branch. Each file is replaced only while it still holds what `HEAD` has,
//! and with the recovery area's swap (see [`worktree`]), so a save made
//! meanwhile is either left alone (the pull stops as Blocked) or kept in the
//! recovery area, never lost. A pull that stops half-way, whatever stopped
//! it (a crash, an error, a clash that appeared at the last moment), leaves
//! the journal, and the next pull, or the daemon's next refresh, finishes
//! it: paths that are done are skipped.
//!
//! Pull works on the whole Git worktree, which may be larger than the skills
//! repo (a subdirectory of it): it updates every path the new baseline
//! changes, so pull checks every one of them. Paths here are relative to the
//! worktree's top level.
//!
//! Every git command runs with [`Git::for_pull`]: no hooks, no autostash,
//! no submodule recursion. Config that only changes how content is
//! converted or merged (`core.autocrlf`, filters, merge drivers, rename
//! detection) is the user's and applies as it would to their own git.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use super::{
    FLEET_BRANCH, ItemRef, PullOutcome, REMOTE, has_git_identity, machine_author, on_fleet_branch, rev, split_author,
    status_paths, unrelated_histories,
};
use crate::git::{Git, path_from_bytes};
use crate::source::is_staging_path;
use crate::{Error, IoExt, Repo, Result, machine};

mod journal;
mod worktree;

use journal::Journal;
use worktree::{Content, Node, Outcome, Worktree};

/// Where tests can step in during a pull.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Before {
    /// Unpushed commits are replayed; the last checks against the draft are next.
    Checks,
    /// The last checks passed; the journal is written next.
    Journal,
    /// The journal is written; the `n`th path, in the order pull goes
    /// through them, is next. `File(0)` comes before any file.
    File(usize),
    /// Inside a file's replacement: what was there is held in the recovery
    /// area and checked, and the new content goes in next.
    Swap,
    /// Every path is done; the index is next.
    Index,
    /// The index is updated; the branch moves next.
    UpdateRef,
    /// The branch moved; the journal is removed next.
    Done,
}

/// Called at each [`Before`]. An error stops the pull right there, as a
/// crash would: whatever it changed stays, journal included.
pub(super) type Hook<'a> = &'a mut dyn FnMut(Before) -> Result<()>;

pub(super) fn run(repo: &Repo, hook: Hook) -> Result<PullOutcome> {
    with_pull(repo, |pull| pull.run(hook))
}

/// Finishes a pull that stopped half-way, if there is one, without fetching.
pub(super) fn finish(repo: &Repo, hook: Hook) -> Result<Option<PullOutcome>> {
    if !pending(repo)? {
        return Ok(None);
    }
    with_pull(repo, |pull| {
        let Some(journal) = Journal::load(pull)? else { return Ok(None) };
        match pull.finish(&Worktree::open(&pull.root, &pull.git_dir)?, &journal, false, hook)? {
            Finished::Blocked(items) => {
                Ok(Some(PullOutcome::Blocked { baseline: journal.baseline, items: items.into_iter().collect() }))
            }
            Finished::Updated { displaced: Some(displaced), .. } => Err(displaced),
            Finished::Updated { from, to, displaced: None } => Ok(Some(PullOutcome::Updated { from, to })),
        }
    })
}

/// Whether a pull stopped half-way and left its journal.
pub(super) fn pending(repo: &Repo) -> Result<bool> {
    let git_dir = Git::new(repo.root()).args(["rev-parse", "--absolute-git-dir"]).run()?;
    let git_dir = path_from_bytes(git_dir.strip_suffix(b"\n").unwrap_or(&git_dir));
    Ok(fs::symlink_metadata(journal::path(&git_dir)).is_ok())
}

/// Puts the content of a pull's recovery entry back at its path, as a draft
/// change. Only over a path without draft changes: what is there is in the
/// baseline (and goes to the recovery area in turn), so nothing is lost.
pub(super) fn adopt(repo: &Repo, entry: &crate::sync::displaced::Entry) -> Result<()> {
    if pending(repo)? {
        return Err(Error::Invalid("a pull stopped half-way; run `sk1ll pull` to finish it first".into()));
    }
    with_pull(repo, |pull| {
        let root = pull.root.canonicalize().at(&pull.root)?;
        let path = entry.dest.strip_prefix(&root).map_err(|_| {
            Error::Invalid(format!("displaced entry `{}` is from outside {}", entry.id, root.display()))
        })?;
        let spec = path.to_str().ok_or_else(|| {
            Error::Invalid(format!("{} has a name that isn't UTF-8; copy it back by hand", entry.dest.display()))
        })?;
        if !status_paths(pull.git(), &[spec])?.is_empty() {
            return Err(Error::Invalid(format!(
                "{} has draft changes; commit or restore `{}` first, then adopt displaced entry `{}`",
                entry.dest.display(),
                pull.item(path),
                entry.id
            )));
        }
        let meta = fs::symlink_metadata(&entry.path).at(&entry.path)?;
        let content = if meta.file_type().is_symlink() {
            Content::Link(crate::git::path_bytes(&fs::read_link(&entry.path).at(&entry.path)?))
        } else if meta.is_file() {
            #[cfg(unix)]
            let executable = std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o100 != 0;
            #[cfg(not(unix))]
            let executable = false;
            Content::File { bytes: fs::read(&entry.path).at(&entry.path)?, executable }
        } else {
            return Err(Error::Invalid(format!("displaced entry `{}` is not a file", entry.id)));
        };
        let tree = Worktree::open(&pull.root, &pull.git_dir)?;
        let node = tree.read(path)?;
        let outcome = match node {
            Node::Missing | Node::Directory => tree.create(usize::MAX, path, &content, &node)?,
            Node::File { .. } | Node::Link(_) => tree.replace(usize::MAX, path, &node, &content, &mut |_| Ok(()))?,
            Node::Other => return Err(Error::Invalid(format!("{} is not a file", entry.dest.display()))),
        };
        tree.clear();
        match outcome {
            Outcome::Done(displaced) => displaced.map_or(Ok(()), Err),
            Outcome::Changed => Err(Error::Changed(format!("{} changed meanwhile; try again", entry.dest.display()))),
        }
    })
}

fn with_pull<T>(repo: &Repo, f: impl FnOnce(&Pull) -> Result<T>) -> Result<T> {
    let hooks = NoHooks::new()?;
    let git = || Git::for_pull(repo.root(), &hooks.0);
    let line = |out: Vec<u8>| path_from_bytes(out.strip_suffix(b"\n").unwrap_or(&out));
    let root = line(git().args(["rev-parse", "--show-toplevel"]).run()?);
    let prefix = line(git().args(["rev-parse", "--show-prefix"]).run()?);
    let git_dir = line(git().args(["rev-parse", "--absolute-git-dir"]).run()?);
    f(&Pull { repo: repo.root(), root, prefix, git_dir, hooks: &hooks.0 })
}

/// An empty directory for `core.hooksPath`, removed again when dropped.
struct NoHooks(PathBuf);

impl NoHooks {
    fn new() -> Result<Self> {
        let tmp = std::env::temp_dir().canonicalize().at(std::env::temp_dir())?;
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        let mut n = 0;
        loop {
            let dir = tmp.join(format!("sk1ll-no-hooks-{}-{n}", std::process::id()));
            match builder.create(&dir) {
                Ok(()) => return Ok(Self(dir)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 1000 => n += 1,
                Err(e) => return Err(e).at(&dir),
            }
        }
    }
}

impl Drop for NoHooks {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.0);
    }
}

const GITLINK: &str = "160000";
const SYMLINK: &str = "120000";

/// A tree entry: mode and object id, as git spells them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    mode: String,
    oid: String,
}

impl Entry {
    /// `None` for the all-zero mode git uses for "no entry".
    fn new(mode: &str, oid: &str) -> Option<Self> {
        (mode.bytes().any(|b| b != b'0')).then(|| Self { mode: mode.into(), oid: oid.into() })
    }
}

/// One path that differs between two trees.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Change {
    path: PathBuf,
    old: Option<Entry>,
    new: Option<Entry>,
}

/// One index entry: its mode, object id and stage.
type Staged = (String, String, String);

/// How a journal's pull ended (see [`Pull::finish`]).
enum Finished {
    /// The branch moved from `from` and `HEAD` is now `to`. `displaced`: a
    /// file was written to while pull replaced it; that version is in the
    /// recovery area.
    Updated { from: String, to: String, displaced: Option<Error> },
    /// These items have changes in the way, and the pull stopped there.
    Blocked(BTreeSet<ItemRef>),
}

/// What [`Pull::update_path`] did.
enum Step {
    /// The path was already as the new commit has it.
    Done,
    /// The path was updated (see [`Finished::Updated`] for `displaced`).
    Updated { displaced: Option<Error> },
    /// The path holds neither version, so it was left alone.
    Changed,
}

struct Pull<'a> {
    /// The skills repo, for messages.
    repo: &'a Path,
    /// The top level of its Git worktree.
    root: PathBuf,
    /// The skills repo relative to `root` (empty if it is the top level).
    prefix: PathBuf,
    /// The worktree's own git directory, where the journal and the recovery
    /// entries of what pull replaces are.
    git_dir: PathBuf,
    hooks: &'a Path,
}

impl Pull<'_> {
    fn git(&self) -> Git {
        Git::for_pull(&self.root, self.hooks)
    }

    /// Finishes a pull that stopped half-way first, then pulls.
    fn run(&self, hook: Hook) -> Result<PullOutcome> {
        let resumed = match Journal::load(self)? {
            Some(journal) => {
                let baseline = journal.baseline.clone();
                match self.finish(&Worktree::open(&self.root, &self.git_dir)?, &journal, false, hook)? {
                    Finished::Blocked(items) => {
                        return Ok(PullOutcome::Blocked { baseline, items: items.into_iter().collect() });
                    }
                    Finished::Updated { from, displaced, .. } => Some((from, displaced)),
                }
            }
            None => {
                journal::forget_anchor(self);
                None
            }
        };
        let (outcome, displaced) = self.update(hook)?;
        let (outcome, displaced) = match resumed {
            None => (outcome, displaced),
            Some((from, earlier)) => {
                let outcome = match outcome {
                    PullOutcome::UpToDate => {
                        let to = rev(self.git(), "HEAD")?.ok_or_else(|| Error::Invalid("HEAD is missing".into()))?;
                        PullOutcome::Updated { from, to }
                    }
                    PullOutcome::Updated { to, .. } => PullOutcome::Updated { from, to },
                    blocked => blocked,
                };
                (outcome, earlier.or(displaced))
            }
        };
        displaced.map_or(Ok(outcome), Err)
    }

    /// Fetches and moves onto the new baseline.
    fn update(&self, hook: Hook) -> Result<(PullOutcome, Option<Error>)> {
        let branch = on_fleet_branch(self.git(), self.repo)?;
        // Submodules are refused, in HEAD or only in the index, before anything else.
        let head = rev(self.git(), "HEAD")?;
        let tracked = match &head {
            Some(head) => self.tracked(head)?,
            None => BTreeSet::new(),
        };
        self.index()?;
        let all = format!("+refs/heads/*:refs/remotes/{REMOTE}/*");
        self.git().args(["fetch", "--quiet", "--prune", "--no-recurse-submodules", REMOTE, &all]).run()?;
        let Some(remote) = rev(self.git(), &format!("refs/remotes/{REMOTE}/{FLEET_BRANCH}"))? else {
            let prefix = format!("refs/remotes/{REMOTE}/");
            let refs = self.git().args(["for-each-ref", "--format=%(refname)", &prefix]).text()?;
            let others: Vec<&str> =
                refs.lines().filter_map(|r| r.strip_prefix(prefix.as_str())).filter(|b| *b != "HEAD").collect();
            if others.is_empty() {
                return Ok((PullOutcome::UpToDate, None));
            }
            return Err(Error::Invalid(format!(
                "{REMOTE} has no `{FLEET_BRANCH}` branch (it has {}), so there is no baseline to pull; the fleet's \
                 baseline must be on `{FLEET_BRANCH}`",
                others.join(", ")
            )));
        };
        let Some(head) = head else {
            return Err(Error::Invalid(format!("`{branch}` has no commits yet; clone the repo from {REMOTE} instead")));
        };
        let base = self.merge_base(&branch, &head, &remote)?;
        if base == remote {
            return Ok((PullOutcome::UpToDate, None));
        }
        self.refuse_gitlinks(&base, &head, &remote)?;
        self.refuse_sparse_checkout()?;
        let blocked = |items: BTreeSet<ItemRef>| {
            Ok((PullOutcome::Blocked { baseline: remote.clone(), items: items.into_iter().collect() }, None))
        };

        let upstream = self.changes(&base, &remote)?;
        let items = self.blocking(&upstream, &[], &upstream, &tracked)?;
        if !items.is_empty() {
            return blocked(items);
        }
        let (target, ours, changes) = if base == head {
            (remote.clone(), Vec::new(), upstream.clone())
        } else {
            let target = match self.replay(&head, &base, &remote)? {
                Ok(tip) => tip,
                Err(items) => return blocked(items),
            };
            let (ours, changes) = (self.changes(&base, &target)?, self.changes(&head, &target)?);
            (target, ours, changes)
        };

        // The draft may have changed meanwhile; a replayed rename can also
        // bring an upstream change to a different path.
        hook(Before::Checks)?;
        on_fleet_branch(self.git(), self.repo)?;
        if rev(self.git(), "HEAD")?.as_deref() != Some(&head) {
            return Err(Error::Invalid("HEAD changed during pull; try again".into()));
        }
        let items = self.blocking(&upstream, &ours, &changes, &tracked)?;
        if !items.is_empty() {
            return blocked(items);
        }

        // Whatever stops a pull from starting has to stop it here, before
        // anything changes.
        let tree = Worktree::open(&self.root, &self.git_dir)?;
        if self.index()?.values().flatten().any(|(_, _, stage)| stage != "0") {
            return Err(Error::Invalid("the index has unresolved conflicts; resolve them before pulling".into()));
        }
        // Takes the index lock, as updating the index will: a lock another
        // Git process holds is an error now, not half-way. Exit status 1
        // only says some files have changes.
        let refresh = self.git().args(["update-index", "--refresh"]).output()?;
        if !matches!(refresh.status.code(), Some(0 | 1)) {
            return Err(git_error("update-index --refresh", &refresh.stderr));
        }
        let journal = Journal { branch, old: head.clone(), target, baseline: remote.clone(), changes };
        self.attributes_from(&journal, &journal.old)?;
        hook(Before::Journal)?;
        journal.write(self)?;
        match self.finish(&tree, &journal, true, hook)? {
            Finished::Updated { to, displaced, .. } => Ok((PullOutcome::Updated { from: head, to }, displaced)),
            Finished::Blocked(items) => blocked(items),
        }
    }

    /// Takes the working tree, the index and then the branch from the
    /// journal's `old` to its `target`, skipping what is done already.
    /// Never goes back: whatever stops it leaves the journal, for the next
    /// call to go on from there. Only a `fresh` journal that nothing was
    /// done for yet is removed again when a clash stops it.
    fn finish(&self, tree: &Worktree, journal: &Journal, fresh: bool, hook: Hook) -> Result<Finished> {
        let branch = format!("refs/heads/{}", journal.branch);
        self.require_attached(&branch, journal)?;
        match rev(self.git(), &branch)? {
            Some(at) if at == journal.target => {
                journal.remove(self)?;
                tree.clear();
                let to = rev(self.git(), "HEAD")?.ok_or_else(|| Error::Invalid("HEAD is missing".into()))?;
                return Ok(Finished::Updated { from: journal.old.clone(), to, displaced: None });
            }
            Some(at) if at == journal.old => {}
            at => return Err(self.branch_moved(journal, at.as_deref())),
        }
        let convert = Convert::new(self, journal)?;
        tree.tidy(journal, |entry, path| convert.content(entry, path))?;
        let (mut wrote, mut displaced) = (false, None);
        let blocked = |items: BTreeSet<ItemRef>, wrote: bool| {
            if fresh && !wrote {
                journal.remove(self)?;
            }
            tree.clear();
            Ok(Finished::Blocked(items))
        };
        for (n, i) in journal.order().into_iter().enumerate() {
            hook(Before::File(n))?;
            self.require_attached(&branch, journal)?;
            let change = &journal.changes[i];
            match self.update_path(tree, &convert, i, change, hook)? {
                Step::Done => {}
                Step::Updated { displaced: d } => {
                    wrote = true;
                    displaced = displaced.or(d);
                }
                Step::Changed => return blocked([self.item(&change.path)].into(), wrote),
            }
        }
        tree.prune(journal.changes.iter().filter(|c| c.new.is_none()).map(|c| c.path.as_path()));

        hook(Before::Index)?;
        // Each path's entry is `old`'s, or already `target`'s; anything else
        // was staged since the checks. Two-way read-tree takes `target`'s
        // entries for the paths that still have `old`'s and keeps every
        // other entry, so whatever the draft staged elsewhere stays staged.
        let index = self.index()?;
        let staged: BTreeSet<ItemRef> = journal
            .changes
            .iter()
            .filter(|c| {
                let entries = index.get(&c.path).map(Vec::as_slice).unwrap_or_default();
                !staged_as(entries, c.old.as_ref()) && !staged_as(entries, c.new.as_ref())
            })
            .map(|c| self.item(&c.path))
            .collect();
        if !staged.is_empty() {
            return blocked(staged, wrote);
        }
        self.git().args(["read-tree", "-m", "-i", &journal.old, &journal.target]).run()?;
        // Fresh stat information spares later status calls hashing these
        // files again; it changes no entry, so it may fail.
        let _ = self.git().args(["update-index", "-q", "--refresh"]).output();

        hook(Before::UpdateRef)?;
        self.require_attached(&branch, journal)?;
        self.update_branch(&branch, &journal.target, &journal.old)?;
        hook(Before::Done)?;
        journal.remove(self)?;
        tree.clear();
        let to = rev(self.git(), "HEAD")?.ok_or_else(|| Error::Invalid("HEAD is missing".into()))?;
        Ok(Finished::Updated { from: journal.old.clone(), to, displaced })
    }

    /// Brings one path from the journal's `old` to its `target`, if it is
    /// still at `old`; `i` is its place in the journal.
    fn update_path(&self, tree: &Worktree, convert: &Convert, i: usize, change: &Change, hook: Hook) -> Result<Step> {
        let node = tree.read(&change.path)?;
        if convert.holds(&node, change.new.as_ref(), &change.path, Side::Target)? {
            return Ok(Step::Done);
        }
        if !convert.holds(&node, change.old.as_ref(), &change.path, Side::Old)? {
            return Ok(Step::Changed);
        }
        let outcome = match &change.new {
            None => tree.remove(&change.path, &node)?,
            Some(entry) => {
                let content = convert.content(entry, &change.path)?;
                match node {
                    Node::Missing | Node::Directory => tree.create(i, &change.path, &content, &node)?,
                    _ => tree.replace(i, &change.path, &node, &content, hook)?,
                }
            }
        };
        Ok(match outcome {
            Outcome::Done(displaced) => Step::Updated { displaced },
            Outcome::Changed => Step::Changed,
        })
    }

    /// `HEAD` must stay on `branch`: another Git command switched branches
    /// (or detached `HEAD`) otherwise, and pull stops for good.
    fn require_attached(&self, branch: &str, journal: &Journal) -> Result<()> {
        let out = self.git().args(["symbolic-ref", "--quiet", "HEAD"]).output()?;
        let head = match out.status.code() {
            Some(0) => String::from_utf8_lossy(&out.stdout).trim().to_string(),
            Some(1) => "a detached HEAD".to_string(),
            _ => return Err(git_error("symbolic-ref --quiet HEAD", &out.stderr)),
        };
        if head == branch {
            return Ok(());
        }
        Err(Error::Invalid(format!(
            "another Git command switched {} to {head} while pull was moving `{}` from {} to {}; pull stopped \
             there. Switch back with `git -C {} switch {}`, then pull again to finish",
            self.root.display(),
            journal.branch,
            short(&journal.old),
            short(&journal.target),
            self.root.display(),
            journal.branch
        )))
    }

    fn branch_moved(&self, journal: &Journal, at: Option<&str>) -> Error {
        Error::Invalid(format!(
            "can't finish the pull that moves `{}` from {} to {}: another Git command moved it to {}. Nothing was \
             changed. Some files may already be as {} has them (see `git status`); to give up on that pull, \
             remove {}",
            journal.branch,
            short(&journal.old),
            short(&journal.target),
            at.map_or("nowhere".to_string(), short),
            short(&journal.target),
            journal::path(&self.git_dir).display()
        ))
    }

    fn update_branch(&self, branch: &str, to: &str, expected: &str) -> Result<()> {
        let args = ["update-ref", "--no-deref", "-m", "sk1ll: pull", branch, to, expected];
        let out = self.git().args(args).output()?;
        if out.status.success() {
            return Ok(());
        }
        let mut error = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if let Ok(Some(actual)) = rev(self.git(), branch)
            && actual != expected
        {
            error.push_str(&format!("; another Git command moved {branch} to {actual}"));
        }
        Err(Error::Git { args: args.join(" "), stderr: error })
    }

    /// Checks that git can read `.gitattributes` from `tree` if the journal
    /// changes any: see [`Convert`].
    fn attributes_from(&self, journal: &Journal, tree: &str) -> Result<()> {
        if !journal.changes_attributes() {
            return Ok(());
        }
        let out = self
            .git()
            .arg(format!("--attr-source={tree}"))
            .args(["hash-object", "--stdin", "--path=.gitattributes"])
            .stdin(Vec::new())
            .output()?;
        match out.status.code() {
            Some(0) => Ok(()),
            Some(129) => Err(Error::Invalid("pulling a change to .gitattributes requires Git 2.42 or later".into())),
            _ => Err(git_error("--attr-source hash-object", &out.stderr)),
        }
    }

    /// A boolean setting, `true` if it isn't set.
    fn flag(&self, key: &str) -> Result<bool> {
        let out = self.git().args(["config", "--bool", "--get", key]).output()?;
        match out.status.code() {
            Some(0) => Ok(String::from_utf8_lossy(&out.stdout).trim() != "false"),
            Some(1) => Ok(true),
            _ => Err(git_error(&format!("config --bool --get {key}"), &out.stderr)),
        }
    }

    /// The index, path by path. Submodules are refused: pull can't check
    /// their content, in a commit or only in the index.
    fn index(&self) -> Result<BTreeMap<PathBuf, Vec<Staged>>> {
        let out = self.git().args(["ls-files", "--stage", "-z"]).run()?;
        let mut index = BTreeMap::<PathBuf, Vec<Staged>>::new();
        for entry in out.split(|b| *b == 0).filter(|entry| !entry.is_empty()) {
            let invalid = || Error::Invalid("can't read the index".into());
            let tab = entry.iter().position(|b| *b == b'\t').ok_or_else(invalid)?;
            let fields = String::from_utf8_lossy(&entry[..tab]).into_owned();
            let &[mode, oid, stage] = fields.split(' ').collect::<Vec<_>>().as_slice() else { return Err(invalid()) };
            if mode == GITLINK {
                return Err(unsupported_submodules());
            }
            index.entry(path_from_bytes(&entry[tab + 1..])).or_default().push((mode.into(), oid.into(), stage.into()));
        }
        Ok(index)
    }

    fn merge_base(&self, branch: &str, head: &str, remote: &str) -> Result<String> {
        let out = self.git().args(["merge-base", head, remote]).output()?;
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        match out.status.code() {
            Some(0) if !stdout.is_empty() => Ok(stdout),
            // No output: they share no history at all.
            Some(1) if stdout.is_empty() && out.stderr.is_empty() => {
                Err(unrelated_histories(self.repo, branch, head, remote))
            }
            _ => Err(git_error(&format!("merge-base {head} {remote}"), &out.stderr)),
        }
    }

    /// The paths `head` tracks. Submodules are refused: pull can't check
    /// their content.
    fn tracked(&self, head: &str) -> Result<BTreeSet<PathBuf>> {
        let out = self.git().args(["ls-tree", "-r", "-z", "--full-tree", head]).run()?;
        let mut paths = BTreeSet::new();
        for entry in out.split(|b| *b == 0).filter(|e| !e.is_empty()) {
            let Some(tab) = entry.iter().position(|b| *b == b'\t') else {
                return Err(Error::Invalid(format!("can't read the tree of {head}")));
            };
            if entry.starts_with(GITLINK.as_bytes()) {
                return Err(unsupported_submodules());
            }
            paths.insert(path_from_bytes(&entry[tab + 1..]));
        }
        Ok(paths)
    }

    /// A link added and removed within either range is still unsupported.
    fn refuse_gitlinks(&self, base: &str, head: &str, remote: &str) -> Result<()> {
        let out = self
            .git()
            .args([
                "log",
                "--format=",
                "--no-notes",
                "--no-show-signature",
                "--raw",
                "--diff-merges=separate",
                "-z",
                "--no-color",
                "--no-relative",
                "--no-renames",
                "--no-ext-diff",
                "--ignore-submodules=none",
                &format!("^{base}"),
                head,
                remote,
                "--",
            ])
            .run()?;
        for entry in out.split(|b| *b == 0).filter(|e| !e.is_empty()).step_by(2) {
            if entry.starts_with(b":160000 ") || entry.get(8..15) == Some(b"160000 ") {
                return Err(unsupported_submodules());
            }
        }
        Ok(())
    }

    /// A sparse checkout leaves paths out of the working tree on purpose,
    /// which pull can't tell from a checkout that failed to write them.
    fn refuse_sparse_checkout(&self) -> Result<()> {
        let out = self.git().args(["config", "--bool", "core.sparseCheckout"]).output()?;
        if String::from_utf8_lossy(&out.stdout).trim() == "true" {
            return Err(Error::Invalid("sparse checkouts aren't supported in a skills repo".into()));
        }
        let entries = self.git().args(["ls-files", "-v", "-z"]).run()?;
        if entries
            .split(|b| *b == 0)
            .any(|entry| entry.first().is_some_and(|tag| *tag == b'S' || tag.is_ascii_lowercase()))
        {
            return Err(Error::Invalid("pull requires a full index without assume-unchanged entries".into()));
        }
        Ok(())
    }

    /// The paths that differ between the trees of `from` and `to`.
    fn changes(&self, from: &str, to: &str) -> Result<Vec<Change>> {
        let args = [
            "diff-tree",
            "-r",
            "-z",
            "--no-abbrev",
            "--no-relative",
            "--no-renames",
            "--ignore-submodules=none",
            from,
            to,
            "--",
        ];
        let out = self.git().args(args).run()?;
        let bad = || Error::Invalid(format!("can't read the changes from {from} to {to}"));
        let mut fields = out.split(|b| *b == 0);
        let mut changes = Vec::new();
        while let Some(header) = fields.next().filter(|h| !h.is_empty()) {
            let path = fields.next().ok_or_else(bad)?;
            let header = std::str::from_utf8(header).ok().and_then(|h| h.strip_prefix(':')).ok_or_else(bad)?;
            let &[old_mode, new_mode, old_oid, new_oid, _] = header.split(' ').collect::<Vec<_>>().as_slice() else {
                return Err(bad());
            };
            if old_mode == GITLINK || new_mode == GITLINK {
                return Err(unsupported_submodules());
            }
            let (old, new) = (Entry::new(old_mode, old_oid), Entry::new(new_mode, new_oid));
            changes.push(Change { path: path_from_bytes(path), old, new });
        }
        Ok(changes)
    }

    /// The item `path` belongs to, named from the skills repo. A path
    /// outside it is `other:../<path>`.
    fn item(&self, path: &Path) -> ItemRef {
        if let Ok(rel) = path.strip_prefix(&self.prefix) {
            return ItemRef::for_path(rel);
        }
        let mut prefix = self.prefix.components().peekable();
        let mut rest = path.components().peekable();
        while prefix.peek().is_some() && prefix.peek() == rest.peek() {
            prefix.next();
            rest.next();
        }
        ItemRef::for_path(&prefix.map(|_| Component::ParentDir).chain(rest).collect::<PathBuf>())
    }

    /// The items with changes in the whole worktree, the draft's and any
    /// outside the skills repo, leaving out imports being staged.
    fn draft(&self) -> Result<BTreeSet<ItemRef>> {
        let changed = status_paths(self.git(), &[])?;
        Ok(changed
            .into_iter()
            .map(|(raw, _)| path_from_bytes(&raw))
            .filter(|p| !is_staging_path(p))
            .map(|p| self.item(&p))
            .collect())
    }

    /// What stands in the way of checking out `changes`: draft items that
    /// `upstream` (the new baseline) or `changes` touch, staging directories
    /// that `ours` (the replayed commits) or `changes` would write into or
    /// replace, and local files git doesn't track in the way of the new tree.
    fn blocking(
        &self,
        upstream: &[Change],
        ours: &[Change],
        changes: &[Change],
        tracked: &BTreeSet<PathBuf>,
    ) -> Result<BTreeSet<ItemRef>> {
        let draft = self.draft()?;
        let mut items = BTreeSet::new();
        for change in upstream.iter().chain(changes) {
            let item = self.item(&change.path);
            if draft.contains(&item) {
                items.insert(item);
            }
        }
        for change in ours.iter().chain(changes).filter(|c| self.reaches_stage(c)) {
            items.insert(self.item(&change.path));
        }
        for change in changes.iter().filter(|c| c.new.is_some() && self.obstructed(&c.path, tracked)) {
            items.insert(self.item(&change.path));
        }
        Ok(items)
    }

    /// Whether `change` lands in an import's staging directory, or puts a
    /// file where a directory holding one is (git removes it if it is empty).
    fn reaches_stage(&self, change: &Change) -> bool {
        if is_staging_path(&change.path) {
            return true;
        }
        let abs = self.root.join(&change.path);
        change.new.is_some()
            && self.non_directory_ancestor(&change.path).is_none()
            && fs::symlink_metadata(&abs).is_ok_and(|m| m.is_dir())
            && WalkDir::new(&abs)
                .min_depth(1)
                .into_iter()
                .any(|e| e.map_or(true, |e| is_staging_path(Path::new(e.file_name()))))
    }

    /// Whether something HEAD doesn't track is in the way of the new tree's
    /// `path`: a file or symlink above it, at it, or (for a directory there)
    /// anywhere below it. Untracked files are in the draft too, but ignored
    /// ones are only caught here.
    fn obstructed(&self, path: &Path, tracked: &BTreeSet<PathBuf>) -> bool {
        if let Some(parent) = self.non_directory_ancestor(path) {
            return !tracked.contains(parent);
        }
        let abs = self.root.join(path);
        match fs::symlink_metadata(&abs) {
            Err(_) => false,
            Ok(m) if m.is_dir() => WalkDir::new(&abs).min_depth(1).into_iter().any(|e| {
                e.map_or(true, |e| {
                    !e.file_type().is_dir() && e.path().strip_prefix(&self.root).map_or(true, |p| !tracked.contains(p))
                })
            }),
            Ok(_) => !tracked.contains(path),
        }
    }

    fn non_directory_ancestor<'a>(&self, path: &'a Path) -> Option<&'a Path> {
        let above: Vec<_> = path.ancestors().skip(1).filter(|a| !a.as_os_str().is_empty()).collect();
        above.into_iter().rev().find(|parent| fs::symlink_metadata(self.root.join(parent)).is_ok_and(|m| !m.is_dir()))
    }

    /// Replays the unpushed commits `base..head` onto `remote`, like `git
    /// rebase` would, in the object store only. `Err(items)`: a commit
    /// conflicts there, in these items.
    fn replay(&self, head: &str, base: &str, remote: &str) -> Result<std::result::Result<String, BTreeSet<ItemRef>>> {
        let commits = self.git().args(["rev-list", "--parents", "--reverse", &format!("{base}..{head}")]).text()?;
        if commits.lines().any(|line| line.split_whitespace().count() != 2) {
            return Err(Error::Invalid("can't pull with merge commits among the unpushed commits".into()));
        }
        let mut ident = self.git().args(["var", "GIT_COMMITTER_IDENT"]);
        if !has_git_identity(|| self.git())? {
            let (name, email) = split_author(&machine_author(&machine::hostname()));
            ident = ident.env("GIT_COMMITTER_NAME", name).env("GIT_COMMITTER_EMAIL", email);
        }
        let ident = ident.run()?;
        let committer = ident.strip_suffix(b"\n").unwrap_or(&ident);
        let tree_of = |commit: &str| self.git().args(["rev-parse", &format!("{commit}^{{tree}}")]).text();
        let mut tip = remote.to_string();
        let mut tip_tree = tree_of(remote)?;
        for (commit, parent) in commits.lines().filter_map(|line| line.split_once(' ')) {
            let args = [
                "merge-tree",
                "--write-tree",
                "-z",
                "--name-only",
                "--no-messages",
                "--merge-base",
                parent,
                &tip,
                commit,
            ];
            let out = self.git().args(args).output()?;
            // `<tree>\0<conflicted path>\0...`
            let mut fields = out.stdout.split(|b| *b == 0);
            let tree = String::from_utf8_lossy(fields.next().unwrap_or_default()).trim().to_string();
            match out.status.code() {
                Some(0) => {}
                Some(1) => {
                    let mut items: BTreeSet<ItemRef> =
                        fields.filter(|p| !p.is_empty()).map(|p| self.item(&path_from_bytes(p))).collect();
                    if items.is_empty() {
                        items = self.changes(parent, commit)?.iter().map(|c| self.item(&c.path)).collect();
                    }
                    if items.is_empty() {
                        return Err(Error::Invalid(format!("replaying {commit} conflicts, but git names no path")));
                    }
                    return Ok(Err(items));
                }
                Some(129) => {
                    return Err(Error::Invalid(
                        "rebasing unpushed commits during pull requires Git 2.40 or later".into(),
                    ));
                }
                _ => return Err(git_error(&args.join(" "), &out.stderr)),
            }
            // Like git rebase, drop commits that become empty and keep ones
            // that were empty to begin with.
            if tree == tip_tree && tree_of(commit)? != tree_of(parent)? {
                continue;
            }
            let raw = self.git().args(["cat-file", "commit", commit]).run()?;
            let raw = replayed_commit(&raw, &tree, &tip, committer)
                .ok_or_else(|| Error::Invalid(format!("can't read commit {commit}")))?;
            tip = self.git().args(["hash-object", "-t", "commit", "-w", "--stdin"]).stdin(raw).text()?;
            tip_tree = tree;
        }
        Ok(Ok(tip))
    }
}

/// Which side of a journal a check is about.
#[derive(Debug, Clone, Copy)]
enum Side {
    Old,
    Target,
}

/// How git converts between blobs and the working tree here: what a path
/// must hold to count as a blob, and what checking one out writes there.
/// `.gitattributes` come from the working tree, as for git status, unless
/// the pull changes some: then each side reads them from its own commit, so
/// a pull that stopped half-way through finds the same either way.
struct Convert<'a> {
    pull: &'a Pull<'a>,
    /// `(old, target)`: the commits to read `.gitattributes` from.
    attributes: Option<(String, String)>,
    /// `core.fileMode`: whether the executable bit counts.
    file_mode: bool,
    /// `core.symlinks`: whether a symlink is checked out as one, or as a
    /// file that holds its target.
    symlinks: bool,
}

impl<'a> Convert<'a> {
    fn new(pull: &'a Pull<'a>, journal: &Journal) -> Result<Self> {
        let attributes = journal.changes_attributes().then(|| (journal.old.clone(), journal.target.clone()));
        Ok(Self { pull, attributes, file_mode: pull.flag("core.fileMode")?, symlinks: pull.flag("core.symlinks")? })
    }

    fn git(&self, side: Side) -> Git {
        let git = self.pull.git();
        match (&self.attributes, side) {
            (Some((old, _)), Side::Old) => git.arg(format!("--attr-source={old}")),
            (Some((_, target)), Side::Target) => git.arg(format!("--attr-source={target}")),
            (None, _) => git,
        }
    }

    /// Whether `node` is what checking out `entry` at `path` gives, as git
    /// itself decides (filters and line endings included). With no entry,
    /// that is nothing, or a directory.
    fn holds(&self, node: &Node, entry: Option<&Entry>, path: &Path, side: Side) -> Result<bool> {
        let Some(entry) = entry else { return Ok(matches!(node, Node::Missing | Node::Directory)) };
        let (bytes, filtered) = match (node, entry.mode.as_str()) {
            (Node::Link(target), SYMLINK) if self.symlinks => (target, false),
            (Node::File { bytes, .. }, SYMLINK) if !self.symlinks => (bytes, false),
            (Node::File { bytes, mode }, "100644" | "100755") => {
                if self.file_mode && (mode & 0o100 != 0) != (entry.mode == "100755") {
                    return Ok(false);
                }
                (bytes, true)
            }
            _ => return Ok(false),
        };
        let git = self.git(side).args(["hash-object", "--stdin"]);
        let git = if filtered { git.arg(path_option(path)) } else { git.arg("--no-filters") };
        Ok(git.stdin(bytes.clone()).text()? == entry.oid)
    }

    /// What checking out `entry` at `path` writes.
    fn content(&self, entry: &Entry, path: &Path) -> Result<Content> {
        if entry.mode == SYMLINK {
            let target = self.pull.git().args(["cat-file", "blob", &entry.oid]).run()?;
            return Ok(match self.symlinks {
                true => Content::Link(target),
                false => Content::File { bytes: target, executable: false },
            });
        }
        let filters = self.git(Side::Target).args(["cat-file", "--filters"]).arg(path_option(path)).arg(&entry.oid);
        Ok(Content::File { bytes: filters.run()?, executable: entry.mode == "100755" })
    }
}

/// `--path=<path>`, for `hash-object` and `cat-file --filters`.
fn path_option(path: &Path) -> std::ffi::OsString {
    let mut option = std::ffi::OsString::from("--path=");
    option.push(path.as_os_str());
    option
}

/// Whether the index `entries` of a path are just `entry` (`None`: none).
fn staged_as(entries: &[Staged], entry: Option<&Entry>) -> bool {
    match (entries, entry) {
        ([], None) => true,
        ([(mode, oid, stage)], Some(entry)) => stage == "0" && *mode == entry.mode && *oid == entry.oid,
        _ => false,
    }
}

fn short(commit: &str) -> String {
    commit.chars().take(12).collect()
}

/// The raw commit object `raw` with only its tree, parent and committer
/// replaced, so author, message, encoding and any other headers stay
/// byte for byte. Signatures (`gpgsig`, `gpgsig-sha256`) are dropped: they
/// no longer match. `None` if `raw` doesn't look like a commit.
fn replayed_commit(raw: &[u8], tree: &str, parent: &str, committer: &[u8]) -> Option<Vec<u8>> {
    let end = raw.windows(2).position(|w| w == b"\n\n")?;
    let mut out = Vec::with_capacity(raw.len());
    let mut in_signature = false;
    for line in raw[..end].split(|b| *b == b'\n') {
        // A header's continuation lines start with a space.
        if !line.starts_with(b" ") {
            in_signature = line.starts_with(b"gpgsig ") || line.starts_with(b"gpgsig-sha256 ");
        }
        if in_signature {
            continue;
        }
        let replace = |name: &[u8], value: &[u8], out: &mut Vec<u8>| {
            out.extend_from_slice(name);
            out.push(b' ');
            out.extend_from_slice(value);
        };
        match line.split(|b| *b == b' ').next()? {
            b"tree" => replace(b"tree", tree.as_bytes(), &mut out),
            b"parent" => replace(b"parent", parent.as_bytes(), &mut out),
            b"committer" => replace(b"committer", committer, &mut out),
            _ => out.extend_from_slice(line),
        }
        out.push(b'\n');
    }
    out.extend_from_slice(&raw[end + 1..]);
    Some(out)
}

fn unsupported_submodules() -> Error {
    Error::Invalid("submodules aren't supported in a skills repo".into())
}

fn git_error(args: &str, stderr: &[u8]) -> Error {
    Error::Git { args: args.into(), stderr: String::from_utf8_lossy(stderr).trim().to_string() }
}
