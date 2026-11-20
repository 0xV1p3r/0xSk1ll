//! [`Repo::pull`]. Fetching and replaying unpushed commits happen in the
//! object store; the working tree is touched only by one checkout at the
//! end, which carries the draft along. Harnesses read skills straight from
//! the working tree, so pull never moves a draft file out of the way, not
//! even for a moment.
//!
//! Pull works on the whole Git worktree, which may be larger than the skills
//! repo (a subdirectory of it): checkout updates every path the new
//! baseline changes, so pull checks every one of them. Paths here are
//! relative to the worktree's top level.
//!
//! Every git command runs with [`Git::for_pull`]: no hooks, no autostash,
//! no submodule recursion. Config that only changes how content is
//! converted or merged (`core.autocrlf`, filters, merge drivers, rename
//! detection) is the user's and applies as it would to their own git.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use walkdir::WalkDir;

use super::{
    FLEET_BRANCH, ItemRef, PullOutcome, REMOTE, has_git_identity, machine_author, on_fleet_branch, rev, split_author,
    status_paths, unquote_path, unrelated_histories,
};
use crate::git::{Git, path_from_bytes};
use crate::source::is_staging_path;
use crate::{Error, IoExt, Repo, Result, machine};

mod recovery;

/// Where tests can step in during a pull.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Before {
    /// Unpushed commits are replayed; the last checks against the draft are next.
    Checks,
    /// The last checks passed; checkout is next.
    Checkout,
    /// Checkout is done; the branch moves next.
    UpdateRef,
    /// The branch moved; HEAD is about to be reattached.
    Attach,
    /// Git returned an error from checkout, before recovery.
    CheckoutFailed,
}

pub(super) fn run(repo: &Repo, hook: &mut dyn FnMut(Before)) -> Result<PullOutcome> {
    let hooks = NoHooks::new()?;
    let git = || Git::for_pull(repo.root(), &hooks.0);
    let line = |out: Vec<u8>| path_from_bytes(out.strip_suffix(b"\n").unwrap_or(&out));
    let root = line(git().args(["rev-parse", "--show-toplevel"]).run()?);
    let prefix = line(git().args(["rev-parse", "--show-prefix"]).run()?);
    Pull { repo: repo.root(), root, prefix, hooks: &hooks.0 }.run(hook)
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

/// A tree entry: mode and object id, as git spells them.
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone)]
struct Change {
    path: PathBuf,
    old: Option<Entry>,
    new: Option<Entry>,
}

/// What [`Pull::switch`] did.
enum Switched {
    /// HEAD is on the branch, at this commit.
    To(String),
    /// Checkout refused because of these items; nothing changed.
    Refused(BTreeSet<ItemRef>),
}

struct Pull<'a> {
    /// The skills repo, for messages.
    repo: &'a Path,
    /// The top level of its Git worktree.
    root: PathBuf,
    /// The skills repo relative to `root` (empty if it is the top level).
    prefix: PathBuf,
    hooks: &'a Path,
}

impl Pull<'_> {
    fn git(&self) -> Git {
        Git::for_pull(&self.root, self.hooks)
    }

    fn run(&self, hook: &mut dyn FnMut(Before)) -> Result<PullOutcome> {
        let branch = on_fleet_branch(self.git(), self.repo)?;
        let all = format!("+refs/heads/*:refs/remotes/{REMOTE}/*");
        self.git().args(["fetch", "--quiet", "--prune", "--no-recurse-submodules", REMOTE, &all]).run()?;
        let Some(remote) = rev(self.git(), &format!("refs/remotes/{REMOTE}/{FLEET_BRANCH}"))? else {
            let prefix = format!("refs/remotes/{REMOTE}/");
            let refs = self.git().args(["for-each-ref", "--format=%(refname)", &prefix]).text()?;
            let others: Vec<&str> =
                refs.lines().filter_map(|r| r.strip_prefix(prefix.as_str())).filter(|b| *b != "HEAD").collect();
            if others.is_empty() {
                if let Some(head) = rev(self.git(), "HEAD")? {
                    self.tracked(&head)?;
                }
                return Ok(PullOutcome::UpToDate);
            }
            return Err(Error::Invalid(format!(
                "{REMOTE} has no `{FLEET_BRANCH}` branch (it has {}), so there is no baseline to pull; the fleet's \
                 baseline must be on `{FLEET_BRANCH}`",
                others.join(", ")
            )));
        };
        let Some(head) = rev(self.git(), "HEAD")? else {
            return Err(Error::Invalid(format!("`{branch}` has no commits yet; clone the repo from {REMOTE} instead")));
        };
        let tracked = self.tracked(&head)?;
        let base = self.merge_base(&branch, &head, &remote)?;
        if base == remote {
            return Ok(PullOutcome::UpToDate);
        }
        self.refuse_gitlinks(&base, &head, &remote)?;
        self.refuse_sparse_checkout()?;
        let blocked = |items: BTreeSet<ItemRef>| {
            Ok(PullOutcome::Blocked { baseline: remote.clone(), items: items.into_iter().collect() })
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
        let check = || {
            on_fleet_branch(self.git(), self.repo)?;
            if rev(self.git(), "HEAD")?.as_deref() != Some(&head) {
                return Err(Error::Invalid("HEAD changed during pull; try again".into()));
            }
            self.blocking(&upstream, &ours, &changes, &tracked)
        };
        match self.switch(&branch, &head, &target, &changes, hook, check)? {
            Switched::To(to) => Ok(PullOutcome::Updated { from: head, to }),
            Switched::Refused(items) => blocked(items),
        }
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

    /// The paths `head` tracks. Submodules are refused: checkout would
    /// leave their content alone and pull can't check it.
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

/// The paths a checkout refusal names, if `stderr` is one: checkout found
/// local changes, untracked or ignored files in the way and changed nothing.
fn refusal(stderr: &[u8]) -> Option<Vec<PathBuf>> {
    let text = String::from_utf8_lossy(stderr);
    let refused = ["would be overwritten", "would be removed", "would lose untracked files"];
    if !refused.iter().any(|r| text.contains(r)) {
        return None;
    }
    let paths = stderr
        .split(|b| *b == b'\n')
        .filter_map(|line| line.strip_prefix(b"\t"))
        .map(|raw| match std::str::from_utf8(raw).ok().filter(|s| s.starts_with('"')).and_then(unquote_path) {
            Some(unquoted) => path_from_bytes(&unquoted),
            None => path_from_bytes(raw),
        })
        .collect();
    Some(paths)
}

fn has_errors(stderr: &[u8]) -> bool {
    let errors: &[&[u8]] = &[b"error:", b"fatal:", b"warning: unable to unlink", b"warning: unable to rmdir"];
    stderr.split(|b| *b == b'\n').any(|line| errors.iter().any(|prefix| line.starts_with(prefix)))
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
