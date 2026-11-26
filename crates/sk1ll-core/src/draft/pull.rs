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
//! no submodule recursion, and no filter driver (the skills repo supports
//! none, see [`crate::git::driver_paths`]): pull refuses a Baseline that
//! writes a file through one before it changes anything, and finishing a
//! pull that started anyway runs none. Other config that changes how
//! content is converted or merged (`core.autocrlf`, `text`/`eol`
//! attributes, merge drivers, rename detection) is the user's and applies
//! as it would to their own git.

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

/// `fetch`: fetch [`REMOTE`] first. Without it, the pull moves onto the
/// baseline fetched last (see [`fetch`]).
pub(super) fn run(repo: &Repo, fetch: bool) -> Result<PullOutcome> {
    let _ = crate::sync::displaced::prune();
    with_pull(repo, |pull| pull.run(fetch))
}

/// Fetches every branch of [`REMOTE`] into `refs/remotes`, pruning the ones
/// it no longer has. Only the object store and remote-tracking refs change:
/// neither `HEAD`, the index nor the working tree, so it needs no engine lock.
pub(super) fn fetch(repo: &Repo) -> Result<()> {
    with_pull(repo, |pull| pull.fetch())
}

/// Finishes a pull that stopped half-way, if there is one, without fetching.
pub(super) fn finish(repo: &Repo) -> Result<Option<PullOutcome>> {
    if !pending(repo)? {
        return Ok(None);
    }
    let _ = crate::sync::displaced::prune();
    with_pull(repo, |pull| {
        let Some(mut journal) = Journal::load(pull)? else { return Ok(None) };
        match pull.finish(&Worktree::open(&pull.root, &pull.git_dir)?, &mut journal, false)? {
            Finished::Blocked(items) => Ok(Some(PullOutcome::Blocked {
                baseline: journal.baseline,
                items: items.into_iter().collect(),
                uncommitted: Vec::new(),
            })),
            Finished::Updated { displaced: Some(displaced), .. } => Err(displaced),
            Finished::Updated { from, to, displaced: None } => Ok(Some(PullOutcome::Updated { from, to })),
        }
    })
}

/// The Baseline a pull that stopped half-way is moving onto, from its
/// journal; `None` if no pull is pending.
pub(super) fn pending_baseline(repo: &Repo) -> Result<Option<String>> {
    if !pending(repo)? {
        return Ok(None);
    }
    with_pull(repo, |pull| Ok(Journal::load(pull)?.map(|journal| journal.baseline)))
}

pub(super) fn journal_exists(git_dir: &Path) -> Result<bool> {
    let path = journal::path(git_dir);
    match fs::symlink_metadata(&path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).at(path),
    }
}

/// Whether a pull stopped half-way and left its journal. Content that is
/// not in a Git repository (yet) has nothing to pull.
pub(super) fn pending(repo: &Repo) -> Result<bool> {
    let git_dir = match Git::repo(repo.root()).and_then(|git| git.args(["rev-parse", "--absolute-git-dir"]).run()) {
        Err(Error::Git { stderr, .. }) if stderr.contains("not a git repository") => return Ok(false),
        git_dir => git_dir?,
    };
    let git_dir = path_from_bytes(git_dir.strip_suffix(b"\n").unwrap_or(&git_dir));
    journal_exists(&git_dir)
}

/// Puts the content of a pull's recovery entry back at its path, as a draft
/// change. Only where nothing is, or over a path without draft changes:
/// what is there is in the baseline (and goes to the recovery area in
/// turn), so nothing is lost. A vacant path takes it whatever `HEAD` and
/// the index say there: a Draft deletion, or a restore that stopped after
/// moving the file aside (see [`super::keep`]), holds nothing to lose.
pub(super) fn adopt(repo: &Repo, entry: &crate::sync::displaced::Entry) -> Result<()> {
    if pending(repo)? {
        return Err(Error::PullPending);
    }
    with_pull(repo, |pull| {
        let root = pull.root.canonicalize().at(&pull.root)?;
        let path = entry.dest.strip_prefix(&root).map_err(|_| {
            Error::Invalid(format!("displaced entry `{}` is from outside {}", entry.id, root.display()))
        })?;
        let head = rev(pull.git(), "HEAD")?;
        let tracked = head.as_deref().map(|head| pull.entry_at(head, path)).transpose()?.flatten();
        let index = pull.index()?;
        let staged = index.get(path).map(Vec::as_slice).unwrap_or_default();
        let tree = Worktree::open(&pull.root, &pull.git_dir)?;
        let node = tree.read(path)?;
        let convert = Convert::with_attributes(pull, None)?;
        let clean = match &tracked {
            Some(_) => convert.holds(&node, tracked.as_ref(), path)?,
            None => node == Node::Missing,
        };
        if node != Node::Missing && (!clean || !staged_as(staged, tracked.as_ref())) {
            return Err(Error::Invalid(format!(
                "{} has local content or a staged change. Move it aside or restore `{}` before adopting `{}`",
                entry.dest.display(),
                pull.item(path),
                entry.id,
            )));
        }
        tree.adopt(path, &node, entry)
    })
}

/// Writes `HEAD`'s version of each of `dests` where restore moved the
/// Draft's into recovery, only where nothing is (see [`Worktree::place_new`]):
/// returns the ones something new was at, left as they are.
pub(super) fn place_baseline(
    repo: &Repo,
    dests: &[PathBuf],
    staged: &mut dyn FnMut() -> Result<()>,
) -> Result<Vec<PathBuf>> {
    if dests.is_empty() {
        return Ok(Vec::new());
    }
    with_pull(repo, |pull| {
        let root = pull.root.canonicalize().at(&pull.root)?;
        let head = rev(pull.git(), "HEAD")?.ok_or_else(|| Error::Invalid("there is no baseline to restore".into()))?;
        let tree = Worktree::open(&pull.root, &pull.git_dir)?;
        let convert = Convert::with_attributes(pull, None)?;
        let mut taken = Vec::new();
        for dest in dests {
            let path = dest
                .strip_prefix(&root)
                .map_err(|_| Error::Invalid(format!("{} is outside {}", dest.display(), root.display())))?;
            let Some(entry) = pull.entry_at(&head, path)? else { continue };
            if !tree.place_new(path, &convert.content(&entry, path)?, staged)? {
                taken.push(dest.clone());
            }
        }
        Ok(taken)
    })
}

/// The index entries (mode and object id, or none) pull's un-commit left
/// at the paths of the commits it turned back into Draft, by absolute path
/// (git-quoted): the operation's staging, not the user's, while the entry
/// is still exactly that.
#[derive(Default, Serialize, Deserialize)]
struct Uncommitted(BTreeMap<String, Option<(String, String)>>);

impl Uncommitted {
    const FILE: &'static str = "sk1ll-uncommitted.json";

    fn load(git_dir: &Path) -> Result<Self> {
        let path = git_dir.join(Self::FILE);
        match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| Error::Parse { path, message: e.to_string() }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).at(path),
        }
    }

    fn save(&self, git_dir: &Path) -> Result<()> {
        let body = serde_json::to_vec_pretty(self).map_err(|e| Error::Invalid(e.to_string()))?;
        crate::config_file::replace_private(&git_dir.join(Self::FILE), &body)
    }
}

/// The mode and object id of the first `ls-files -s` record in `out`.
fn index_fields(out: &[u8]) -> Option<(String, String)> {
    let record = out.split(|b| *b == 0).find(|r| !r.is_empty())?;
    let tab = record.iter().position(|b| *b == b'\t')?;
    let mut fields = std::str::from_utf8(&record[..tab]).ok()?.split(' ');
    Some((fields.next()?.to_string(), fields.next()?.to_string()))
}

/// What pull's un-commit staged, by absolute path (see [`Uncommitted`]).
pub(super) fn uncommitted_staging(repo: &Repo) -> Result<BTreeMap<PathBuf, Option<(String, String)>>> {
    with_pull(repo, |pull| {
        let staged = Uncommitted::load(&pull.git_dir)?;
        Ok(staged
            .0
            .into_iter()
            .filter_map(|(quoted, entry)| Some((path_from_bytes(&super::unquote_path(&quoted)?), entry)))
            .collect())
    })
}

/// Removes what a keep's restore staged and never published (see
/// [`Worktree::place_new`]): a copy of a Baseline file, in a directory of
/// the keep's own.
pub(super) fn clear_keep_stage(git_dir: &Path) -> Result<()> {
    let stage = worktree::keep_stage(git_dir);
    let slot = stage.join(worktree::KEEP_SLOT);
    for removed in [fs::remove_file(&slot).at(&slot), fs::remove_dir(&stage).at(&stage)] {
        match removed {
            Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {}
            result => result?,
        }
    }
    Ok(())
}

/// Takes the file at `dest` out of the working tree, as a Draft deletion,
/// into a recovery entry of its own: only while it holds what `HEAD` has,
/// with nothing staged, so what goes is in the Baseline. Nothing there is
/// done already.
pub(super) fn take_out(repo: &Repo, dest: &Path) -> Result<()> {
    if pending(repo)? {
        return Err(Error::PullPending);
    }
    with_pull(repo, |pull| {
        let root = pull.root.canonicalize().at(&pull.root)?;
        let path = dest
            .strip_prefix(&root)
            .map_err(|_| Error::Invalid(format!("{} is outside {}", dest.display(), root.display())))?;
        let tree = Worktree::open(&pull.root, &pull.git_dir)?;
        let node = tree.read(path)?;
        if node == Node::Missing {
            return Ok(());
        }
        let head = rev(pull.git(), "HEAD")?;
        let tracked = head.as_deref().map(|head| pull.entry_at(head, path)).transpose()?.flatten();
        let index = pull.index()?;
        let staged = index.get(path).map(Vec::as_slice).unwrap_or_default();
        let convert = Convert::with_attributes(pull, None)?;
        if tracked.is_none() || !convert.holds(&node, tracked.as_ref(), path)? || !staged_as(staged, tracked.as_ref()) {
            return Err(Error::Invalid(format!(
                "{} has local content or a staged change. Move it aside or restore `{}`",
                dest.display(),
                pull.item(path),
            )));
        }
        tree.take_out(path, &node)
    })
}

fn with_pull<T>(repo: &Repo, f: impl FnOnce(&Pull) -> Result<T>) -> Result<T> {
    let hooks = NoHooks::new()?;
    let none = BTreeSet::new();
    let git = || Git::for_pull(repo.root(), &hooks.0, &none);
    let line = |out: Vec<u8>| path_from_bytes(out.strip_suffix(b"\n").unwrap_or(&out));
    let root = line(git().args(["rev-parse", "--show-toplevel"]).run()?);
    let prefix = line(git().args(["rev-parse", "--show-prefix"]).run()?);
    let git_dir = line(git().args(["rev-parse", "--absolute-git-dir"]).run()?);
    let drivers = crate::git::configured_drivers(Git::for_pull(&root, &hooks.0, &none))?;
    f(&Pull { repo: repo.root(), root, prefix, git_dir, hooks: &hooks.0, drivers })
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
    /// The filter drivers git's configuration has here, as the pull
    /// started; every git command runs with them turned off.
    drivers: BTreeSet<String>,
}

impl Pull<'_> {
    fn git(&self) -> Git {
        Git::for_pull(&self.root, self.hooks, &self.drivers)
    }

    fn fetch(&self) -> Result<()> {
        let all = format!("+refs/heads/*:refs/remotes/{REMOTE}/*");
        self.git().args(["fetch", "--quiet", "--prune", "--no-recurse-submodules", REMOTE, &all]).run()?;
        Ok(())
    }

    /// Finishes a pull that stopped half-way first, then pulls.
    fn run(&self, fetch: bool) -> Result<PullOutcome> {
        let resumed = match Journal::load(self)? {
            Some(mut journal) => {
                let baseline = journal.baseline.clone();
                match self.finish(&Worktree::open(&self.root, &self.git_dir)?, &mut journal, false)? {
                    Finished::Blocked(items) => {
                        return Ok(PullOutcome::Blocked {
                            baseline,
                            items: items.into_iter().collect(),
                            uncommitted: Vec::new(),
                        });
                    }
                    Finished::Updated { from, displaced, .. } => Some((from, displaced)),
                }
            }
            None => {
                journal::forget_anchor(self);
                None
            }
        };
        let (outcome, displaced) = self.update(fetch)?;
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

    /// Fetches (if `fetch`) and moves onto the new baseline.
    fn update(&self, fetch: bool) -> Result<(PullOutcome, Option<Error>)> {
        let branch = on_fleet_branch(self.git(), self.repo)?;
        // Submodules are refused, in HEAD or only in the index, before anything else.
        let head = rev(self.git(), "HEAD")?;
        let tracked = match &head {
            Some(head) => self.tracked(head)?,
            None => BTreeSet::new(),
        };
        self.index()?;
        if fetch {
            self.fetch()?;
        }
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
            Ok((
                PullOutcome::Blocked {
                    baseline: remote.clone(),
                    items: items.into_iter().collect(),
                    uncommitted: Vec::new(),
                },
                None,
            ))
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
                Err(clash) => return self.uncommit(&branch, &head, clash),
            };
            let (ours, changes) = (self.changes(&base, &target)?, self.changes(&head, &target)?);
            (target, ours, changes)
        };

        // The draft may have changed meanwhile; a replayed rename can also
        // bring an upstream change to a different path.
        crate::failpoint!(PullChecks, self.repo);
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
        self.refuse_filter_drivers(&target, &remote, &changes)?;
        let tree = Worktree::open(&self.root, &self.git_dir)?;
        tree.preflight(&changes)?;
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
        let mut journal = Journal {
            branch,
            old: head.clone(),
            target,
            baseline: remote.clone(),
            changes,
            operations: BTreeMap::new(),
        };
        self.attributes_from(&journal, &journal.old)?;
        crate::failpoint!(PullJournal, self.repo);
        journal.write(self)?;
        match self.finish(&tree, &mut journal, true)? {
            Finished::Updated { to, displaced, .. } => Ok((PullOutcome::Updated { from: head, to }, displaced)),
            Finished::Blocked(items) => blocked(items),
        }
    }

    /// An unpushed commit conflicts with the new Baseline: turns it and the
    /// commits after it back into Draft changes, then pulls as for a Draft
    /// clash. Only the branch moves, back to the commit before the first
    /// conflicting one; the index and the working tree keep their bytes, so
    /// nothing is lost and `git status` shows those commits' changes (staged)
    /// with the Draft that was on top of them. The commits before it are
    /// replayed as usual.
    fn uncommit(&self, branch: &str, head: &str, clash: Clash) -> Result<(PullOutcome, Option<Error>)> {
        let range = format!("{}..{head}", clash.parent);
        let log = self.git().args(["log", "--reverse", "--no-show-signature", "--format=%h %s", &range]).text()?;
        let commits: Vec<String> = log.lines().map(str::to_string).collect();
        let reference = format!("refs/heads/{branch}");
        let message = "sk1ll: pull turns unpushed commits that conflict with the new baseline back into Draft";
        // What the index holds at the paths these commits change is staged
        // by this un-commit, not by the user: recorded, so a keep can tell,
        // once the branch has moved. A stop in between leaves it unrecorded,
        // which a keep then refuses as the user's: never the other way round.
        let changed = self.git().args(["diff", "--name-only", "-z", "--no-renames", &clash.parent, head]).run()?;
        let paths: Vec<PathBuf> = crate::git::split_paths(&changed).collect();
        let mut staged = Uncommitted::load(&self.git_dir)?;
        for path in &paths {
            let out = self.git().literal().args(["ls-files", "-s", "-z", "--"]).arg(path).run()?;
            staged.0.insert(super::quote_path(&crate::git::path_bytes(&self.root.join(path))), index_fields(&out));
        }
        self.git().args(["update-ref", "--no-deref", "-m", message, &reference, &clash.parent, head]).run()?;
        crate::failpoint!(PullUncommitted, self.repo);
        staged.save(&self.git_dir)?;
        let (outcome, displaced) = self.update(false)?;
        let outcome = match outcome {
            PullOutcome::Blocked { baseline, mut items, uncommitted } => {
                items.extend(clash.items.iter().filter(|item| !items.contains(item)).cloned().collect::<Vec<_>>());
                items.sort();
                PullOutcome::Blocked { baseline, items, uncommitted: commits.into_iter().chain(uncommitted).collect() }
            }
            // The Draft they became matches the new Baseline after all.
            outcome => outcome,
        };
        Ok((outcome, displaced))
    }

    /// Takes the working tree, the index and then the branch from the
    /// journal's `old` to its `target`, skipping what is done already.
    /// Never goes back: whatever stops it leaves the journal, for the next
    /// call to go on from there. Only a `fresh` journal that nothing was
    /// done for yet is removed again when a clash stops it.
    fn finish(&self, tree: &Worktree, journal: &mut Journal, fresh: bool) -> Result<Finished> {
        let branch = format!("refs/heads/{}", journal.branch);
        self.require_attached(&branch, journal)?;
        match rev(self.git(), &branch)? {
            Some(at) if at == journal.target => {
                let mut displaced = None;
                for i in 0..journal.changes.len() {
                    let Some(operation) = journal.operations.get(&i) else {
                        return Err(self.branch_moved(journal, Some(&at)));
                    };
                    let Some(Outcome::Done(entry)) = tree.completed(i, operation)? else {
                        return Err(self.branch_moved(journal, Some(&at)));
                    };
                    displaced = displaced.or(entry);
                }
                journal.remove(self)?;
                tree.clear();
                drop_replaced(journal);
                let to = rev(self.git(), "HEAD")?.ok_or_else(|| Error::Invalid("HEAD is missing".into()))?;
                return Ok(Finished::Updated { from: journal.old.clone(), to, displaced });
            }
            Some(at) if at == journal.old => {}
            at => return Err(self.branch_moved(journal, at.as_deref())),
        }
        let convert = Convert::new(self, journal)?;
        let (mut wrote, mut displaced) = (false, None);
        let blocked = |items: BTreeSet<ItemRef>, wrote: bool, journal: &Journal| {
            if fresh && !wrote && journal.operations.is_empty() {
                journal.remove(self)?;
            }
            Ok(Finished::Blocked(items))
        };
        let mut pruned = false;
        for i in journal.order() {
            if !pruned && journal.changes[i].new.is_some() {
                tree.prune(journal.changes.iter().filter(|c| c.new.is_none()).map(|c| c.path.as_path()));
                pruned = true;
            }
            crate::failpoint!(PullFile, &self.root.join(&journal.changes[i].path));
            self.require_attached(&branch, journal)?;
            match self.update_path(tree, &convert, journal, i)? {
                Outcome::Done(d) => {
                    wrote = true;
                    displaced = displaced.or(d);
                }
                Outcome::Changed => {
                    if fresh && let Some(displaced) = displaced {
                        return Err(displaced);
                    }
                    return blocked([self.item(&journal.changes[i].path)].into(), wrote, journal);
                }
            }
        }
        if !pruned {
            tree.prune(journal.changes.iter().filter(|c| c.new.is_none()).map(|c| c.path.as_path()));
        }

        crate::failpoint!(PullIndex, self.repo);
        // Two-way read-tree carries staged Draft entries through paths that
        // agree on both sides. Make those sides agree if a path was staged
        // since our initial check, including after a previous index update.
        let index = self.index()?;
        let staged: Vec<_> = journal
            .changes
            .iter()
            .filter(|c| {
                let entries = index.get(&c.path).map(Vec::as_slice).unwrap_or_default();
                !staged_as(entries, c.old.as_ref()) && !staged_as(entries, c.new.as_ref())
            })
            .map(|c| &c.path)
            .collect();
        let index_target = self.index_target(journal, &staged)?;
        let at = rev(self.git(), &branch)?;
        if at.as_deref() != Some(journal.old.as_str()) {
            return Err(self.branch_moved(journal, at.as_deref()));
        }
        self.require_attached(&branch, journal)?;
        self.git().args(["read-tree", "-m", "-i", &journal.old, &index_target]).run()?;
        // Fresh stat information spares later status calls hashing these
        // files again; it changes no entry, so it may fail.
        let _ = self.git().args(["update-index", "-q", "--refresh"]).output();

        crate::failpoint!(PullUpdateRef, self.repo);
        self.require_attached(&branch, journal)?;
        self.update_branch(&branch, &journal.target, &journal.old)?;
        crate::failpoint!(PullDone, self.repo);
        journal.remove(self)?;
        tree.clear();
        drop_replaced(journal);
        let to = rev(self.git(), "HEAD")?.ok_or_else(|| Error::Invalid("HEAD is missing".into()))?;
        Ok(Finished::Updated { from: journal.old.clone(), to, displaced })
    }

    /// For paths edited and staged after publication, use the same entry on
    /// both sides of the index merge. This carries the staged entry through
    /// as Draft, including a staged file/directory replacement.
    fn index_target(&self, journal: &Journal, staged: &[&PathBuf]) -> Result<String> {
        if staged.is_empty() {
            return Ok(journal.target.clone());
        }
        let saved = journal
            .changes
            .iter()
            .filter(|change| staged.iter().any(|path| path.starts_with(&change.path) || change.path.starts_with(path)));
        let (mut remove, mut restore) = (Vec::new(), Vec::new());
        let zero = "0".repeat(journal.target.len());
        for change in saved {
            let path = crate::git::path_bytes(&change.path);
            remove.extend_from_slice(format!("0 {zero}\t").as_bytes());
            remove.extend_from_slice(&path);
            remove.push(0);
            if let Some(old) = &change.old {
                restore.extend_from_slice(format!("{} {}\t", old.mode, old.oid).as_bytes());
                restore.extend_from_slice(&path);
                restore.push(0);
            }
        }
        remove.extend(restore);
        let index = self.hooks.join("pull-index");
        let git = || self.git().args(["-c", "core.splitIndex=false"]).env("GIT_INDEX_FILE", &index);
        let result = (|| {
            git().args(["read-tree", &journal.target]).run()?;
            git().args(["update-index", "-z", "--index-info"]).stdin(remove).run()?;
            git().arg("write-tree").text()
        })();
        let _ = fs::remove_file(&index);
        result
    }

    /// Brings one path from the journal's `old` to its `target`, if it is
    /// still at `old`; `i` is its place in the journal.
    fn update_path(&self, tree: &Worktree, convert: &Convert, journal: &mut Journal, i: usize) -> Result<Outcome> {
        if let Some(operation) = journal.operations.get(&i)
            && let Some(outcome) = tree.completed(i, operation)?
        {
            return Ok(outcome);
        }

        let change = &journal.changes[i];
        let node = tree.read(&change.path)?;
        if !convert.holds(&node, change.old.as_ref(), &change.path)? {
            return Ok(Outcome::Changed);
        }
        if !journal.operations.contains_key(&i) {
            let content = change.new.as_ref().map(|entry| convert.content(entry, &change.path)).transpose()?;
            let keep_mode = !convert.file_mode
                && matches!((&change.old, &change.new), (Some(old), Some(new)) if old.mode == new.mode);
            let operation = tree.prepare(i, &change.path, &node, content.as_ref(), keep_mode)?;
            journal.record(self, i, operation)?;
        }
        tree.apply(i, &journal.changes[i].path, &node, &journal.operations[&i])
    }

    /// The exact leaf at `path`, without interpreting it as a Git pathspec.
    fn entry_at(&self, head: &str, path: &Path) -> Result<Option<Entry>> {
        let out = self.git().literal().args(["ls-tree", "-r", "-z", "--full-tree", head, "--"]).arg(path).run()?;
        let want = crate::git::path_bytes(path);
        for row in out.split(|b| *b == 0) {
            let Some(tab) = row.iter().position(|b| *b == b'\t') else { continue };
            if row[tab + 1..] != want {
                continue;
            }
            let header = std::str::from_utf8(&row[..tab]).map_err(|_| Error::Invalid("invalid tree entry".into()))?;
            let fields: Vec<_> = header.split(' ').collect();
            if let [mode, _, oid] = fields.as_slice() {
                return Ok(Entry::new(mode, oid));
            }
            return Err(Error::Invalid("invalid tree entry".into()));
        }
        Ok(None)
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
            "can't finish the pull that moves `{}` from {} to {}: another Git command moved it to {}. \
             Return the branch to its original commit before resuming. Its recovery state is recorded in {}",
            journal.branch,
            short(&journal.old),
            short(&journal.target),
            at.map_or("nowhere".to_string(), short),
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

    /// The skills repo supports no filter driver (see
    /// [`crate::git::driver_paths`]): a pull onto `target` that writes a
    /// file through one stops here, before anything changes. Its attributes
    /// are read where the pull reads them (see [`Convert`]).
    fn refuse_filter_drivers(&self, target: &str, baseline: &str, changes: &[Change]) -> Result<()> {
        let files: Vec<&Path> = changes
            .iter()
            .filter(|c| c.new.as_ref().is_some_and(|e| matches!(e.mode.as_str(), "100644" | "100755")))
            .map(|c| c.path.as_path())
            .collect();
        let source = changes_attributes(changes).then_some(target);
        let paths = crate::git::driver_paths(self.git(), source, &files, &self.drivers)?;
        if paths.is_empty() {
            return Ok(());
        }
        let items: BTreeSet<ItemRef> = paths.iter().map(|path| self.item(path)).collect();
        Err(Error::FilterDriver { baseline: baseline.to_string(), paths, items: items.into_iter().collect() })
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
        let removed: BTreeSet<_> = changes.iter().filter(|c| c.new.is_none()).map(|c| c.path.as_path()).collect();
        for change in changes.iter().filter(|c| c.new.is_some() && self.obstructed(&c.path, tracked, &removed)) {
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
    fn obstructed(&self, path: &Path, tracked: &BTreeSet<PathBuf>, removed: &BTreeSet<&Path>) -> bool {
        let tracked_here = |p: &Path| {
            tracked.contains(p)
                || (!removed.is_empty() && self.actual_spelling(p).is_some_and(|p| removed.contains(p.as_path())))
        };
        if let Some(parent) = self.non_directory_ancestor(path) {
            return !tracked_here(parent);
        }
        let abs = self.root.join(path);
        match fs::symlink_metadata(&abs) {
            Err(_) => false,
            Ok(m) if m.is_dir() => WalkDir::new(&abs).min_depth(1).into_iter().any(|e| {
                e.map_or(true, |e| {
                    !e.file_type().is_dir() && e.path().strip_prefix(&self.root).map_or(true, |p| !tracked_here(p))
                })
            }),
            Ok(_) => !tracked_here(path),
        }
    }

    /// Resolve the spelling the directory actually stores. Only a unique
    /// inode match can stand in for an absent spelling on a case-folding
    /// filesystem. A distinct, exactly named hard link is an obstruction.
    #[cfg(unix)]
    fn actual_spelling(&self, path: &Path) -> Option<PathBuf> {
        use std::os::unix::fs::MetadataExt;

        let mut resolved = PathBuf::new();
        let mut components = path.components().peekable();
        while let Some(component) = components.next() {
            let dir = self.root.join(&resolved);
            let name = component.as_os_str();
            let requested = fs::symlink_metadata(dir.join(name)).ok()?;
            if components.peek().is_some() && !requested.is_dir() {
                return None;
            }
            let mut alias = None;
            let mut count = 0;
            for entry in fs::read_dir(&dir).ok()? {
                let entry = entry.ok()?;
                if entry.file_name() == name {
                    alias = Some(entry.file_name());
                    count = 1;
                    break;
                }
                let meta = fs::symlink_metadata(entry.path()).ok()?;
                if (meta.dev(), meta.ino()) == (requested.dev(), requested.ino()) {
                    alias = Some(entry.file_name());
                    count += 1;
                }
            }
            if count != 1 {
                return None;
            }
            resolved.push(alias?);
        }
        Some(resolved)
    }

    #[cfg(not(unix))]
    fn actual_spelling(&self, _: &Path) -> Option<PathBuf> {
        None
    }

    fn non_directory_ancestor<'a>(&self, path: &'a Path) -> Option<&'a Path> {
        let above: Vec<_> = path.ancestors().skip(1).filter(|a| !a.as_os_str().is_empty()).collect();
        above.into_iter().rev().find(|parent| fs::symlink_metadata(self.root.join(parent)).is_ok_and(|m| !m.is_dir()))
    }

    /// Replays the unpushed commits `base..head` onto `remote`, like `git
    /// rebase` would, in the object store only. `Err(clash)`: a commit
    /// conflicts there, in these items.
    fn replay(&self, head: &str, base: &str, remote: &str) -> Result<std::result::Result<String, Clash>> {
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
                    return Ok(Err(Clash { items, parent: parent.to_string() }));
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

/// An unpushed commit that conflicts with the new Baseline when replayed.
struct Clash {
    /// The items it conflicts in.
    items: BTreeSet<ItemRef>,
    /// Its parent: the last commit that replays cleanly (or the Baseline it
    /// started from).
    parent: String,
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
        Self::with_attributes(pull, attributes)
    }

    fn with_attributes(pull: &'a Pull<'a>, attributes: Option<(String, String)>) -> Result<Self> {
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
    /// itself decides (line endings included, filter drivers off). With no entry,
    /// that is nothing, or a directory.
    fn holds(&self, node: &Node, entry: Option<&Entry>, path: &Path) -> Result<bool> {
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
        let git = self.git(Side::Old).args(["hash-object", "--stdin"]);
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
        let executable = entry.mode == "100755";
        let filters = self.git(Side::Target).args(["cat-file", "--filters"]).arg(path_option(path)).arg(&entry.oid);
        Ok(Content::File { bytes: filters.run()?, executable })
    }
}

/// Whether `changes` change any `.gitattributes` file, removals included.
fn changes_attributes(changes: &[Change]) -> bool {
    changes.iter().any(|c| c.path.file_name().is_some_and(|name| name == ".gitattributes"))
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

/// Drops the recovery entries of a finished pull that hold just what it
/// moved there. Pull only replaces or removes a path while it holds the
/// committed version at the journal's `old`, so such an entry holds nothing
/// the history lacks; one a save went into during the swap is changed, and
/// stays. Best effort: an entry left behind only shows as unchanged.
fn drop_replaced(journal: &Journal) {
    for operation in journal.operations.values() {
        if let journal::Operation::Remove { recovery } | journal::Operation::Replace { recovery, .. } = operation {
            let _ = crate::sync::displaced::drop_committed(recovery);
        }
    }
}
