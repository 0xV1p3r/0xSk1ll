//! Baseline and draft: the repo's committed `HEAD` (pushed to [`REMOTE`]) is
//! the baseline; uncommitted changes in the working tree, untracked files
//! included, are this machine's draft. A draft is split into items (a skill,
//! a fragment, the manifest) so that promote and reset work per item.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use walkdir::WalkDir;

use crate::git::{Git, nul_list, path_bytes, path_from_bytes, split_paths};
use crate::instructions::INSTRUCTIONS_DIR;
use crate::lock::LOCK_FILE;
use crate::manifest::MANIFEST_FILE;
use crate::skill::{IMPORTED_DIR, PERSONAL_DIR};
use crate::{Error, IoExt, Repo, Result, machine};

/// The only remote: the hub (see ADR 0002).
pub const REMOTE: &str = "origin";

/// The one branch the fleet's baseline lives on, locally and on the hub.
/// Pull and push always use it, never "whatever branch is checked out": a
/// clone on another branch would otherwise find nothing to pull and push a
/// separate branch the fleet never sees.
pub const FLEET_BRANCH: &str = "main";

/// One unit of repo content, as promoted or reset. String form: `skill:pdf`,
/// `fragment:git`, `manifest`, `other:<path>`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ItemRef {
    /// `personal/<name>/` and/or `imported/<name>/`.
    Skill(String),
    /// `instructions/<name>.md`.
    Fragment(String),
    /// `sk1ll.toml` and `sk1ll.lock` together.
    Manifest,
    /// Any other path; only promoted or reset when named explicitly.
    Other(String),
}

impl ItemRef {
    /// The item a repo-relative path belongs to. A path that isn't valid
    /// UTF-8 is always `other:`, named by its git-style quoted form
    /// (`other:"bad\377.txt"`). So that distinct paths always get distinct
    /// names, a path that starts with `"` or holds a control character is
    /// quoted too: an unquoted name never starts with `"`, and quoting is
    /// reversible (see [`other_path`](Self::other_path)).
    pub fn for_path(path: &Path) -> Self {
        let Some(s) = path.to_str() else { return Self::Other(quote_path(&path_bytes(path))) };
        let s = slash(s);
        let parts: Vec<&str> = s.split('/').collect();
        match parts.as_slice() {
            [dir, name, _, ..] if *dir == PERSONAL_DIR || *dir == IMPORTED_DIR => Self::Skill(name.to_string()),
            [dir, file] if *dir == INSTRUCTIONS_DIR => match file.strip_suffix(".md") {
                Some(name) if !name.is_empty() && !name.starts_with('.') => Self::Fragment(name.to_string()),
                _ => Self::other(s),
            },
            [file] if *file == MANIFEST_FILE || *file == LOCK_FILE => Self::Manifest,
            _ => Self::other(s),
        }
    }

    fn other(path: String) -> Self {
        let quote = path.starts_with('"') || path.chars().any(|c| c.is_ascii_control());
        Self::Other(if quote { quote_path(path.as_bytes()) } else { path })
    }

    /// The path an `other:` item names: the inverse of [`for_path`](Self::for_path).
    /// `None` for other kinds, or a quoted name that doesn't parse.
    pub fn other_path(&self) -> Option<PathBuf> {
        let Self::Other(name) = self else { return None };
        if !name.starts_with('"') {
            return Some(PathBuf::from(name));
        }
        Some(path_from_bytes(&unquote_path(name)?))
    }

    /// Short name for messages: the skill or fragment name, `manifest`, or the path.
    pub fn name(&self) -> &str {
        match self {
            Self::Skill(n) | Self::Fragment(n) | Self::Other(n) => n,
            Self::Manifest => "manifest",
        }
    }

    /// Parses `kind:name`, or resolves a bare name (`pdf`) against `draft`
    /// when exactly one draft item has that name.
    pub fn resolve(input: &str, draft: &[DraftItem]) -> Result<Self> {
        if input == "manifest" || input.contains(':') {
            return input.parse();
        }
        let mut found = draft.iter().filter(|d| d.item.name() == input).map(|d| &d.item);
        match (found.next(), found.next()) {
            (Some(item), None) => Ok(item.clone()),
            (Some(a), Some(b)) => Err(Error::Invalid(format!("`{input}` is ambiguous ({a}, {b}); use kind:name"))),
            (None, _) => Err(Error::Invalid(format!("`{input}` is not in the draft (see `sk1ll draft`)"))),
        }
    }
}

impl fmt::Display for ItemRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Skill(n) => write!(f, "skill:{n}"),
            Self::Fragment(n) => write!(f, "fragment:{n}"),
            Self::Manifest => f.write_str("manifest"),
            Self::Other(p) => write!(f, "other:{p}"),
        }
    }
}

impl FromStr for ItemRef {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let item = match s.split_once(':') {
            None if s == "manifest" => Self::Manifest,
            Some(("skill", n)) if !n.is_empty() => Self::Skill(n.into()),
            Some(("fragment", n)) if !n.is_empty() => Self::Fragment(n.into()),
            Some(("other", p)) if !p.is_empty() => Self::Other(p.into()),
            _ => {
                return Err(Error::Invalid(format!(
                    "invalid item `{s}` (expected skill:<name>, fragment:<name>, manifest or other:<path>)"
                )));
            }
        };
        Ok(item)
    }
}

impl Serialize for ItemRef {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ItemRef {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        String::deserialize(d)?.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Change {
    Added,
    Modified,
    Deleted,
}

/// One item of the draft and the changed paths (repo-relative) it covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DraftItem {
    pub item: ItemRef,
    pub change: Change,
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PullOutcome {
    UpToDate,
    /// `HEAD` moved; the draft was carried on top.
    Updated { from: String, to: String },
    /// The new baseline clashes with the draft (or with promoted, unpushed
    /// commits, or with local files the draft leaves out, such as ignored
    /// ones). Nothing was changed; `baseline` is the remote commit.
    Blocked { baseline: String, items: Vec<ItemRef> },
}

/// `"<id> <sk1ll@<id>>"`: the commit identity for a machine without a git identity.
pub fn machine_author(machine_id: &str) -> String {
    let slug: String =
        machine_id.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '.') { c } else { '-' }).collect();
    format!("{machine_id} <sk1ll@{slug}>")
}

/// E.g. `promote pdf, git-flow from laptop`.
pub fn default_promote_message(items: &[ItemRef], machine_id: &str) -> String {
    let names: Vec<&str> = items.iter().map(ItemRef::name).collect();
    format!("promote {} from {machine_id}", names.join(", "))
}

/// Makes index entries match the working tree for the paths on stdin (adds,
/// updates and removals).
const UPDATE_INDEX: [&str; 6] = ["update-index", "--add", "--remove", "--replace", "-z", "--stdin"];

/// What `git status` says about the working tree, relative to the repo root.
struct Status {
    head: Option<String>,
    head_paths: BTreeSet<PathBuf>,
    /// Changed path -> untracked.
    changed: BTreeMap<PathBuf, bool>,
}

impl Repo {
    /// This machine's draft: uncommitted changes (untracked files included),
    /// grouped into items.
    pub fn draft(&self) -> Result<Vec<DraftItem>> {
        Ok(self.draft_with_status()?.0)
    }

    /// Unified diff of `item` against the baseline.
    pub fn diff(&self, item: &ItemRef) -> Result<String> {
        let (draft, st) = self.draft_with_status()?;
        let selected = pick(&draft, std::slice::from_ref(item))?;
        let (untracked, tracked): (Vec<&PathBuf>, Vec<&PathBuf>) =
            selected[0].paths.iter().partition(|p| st.changed.get(*p).copied().unwrap_or(false));
        let mut out = String::new();
        if !tracked.is_empty() {
            let base = match &st.head {
                Some(h) => h.clone(),
                None => self.empty_tree()?,
            };
            let diff = self.git().literal().args(["diff", "--no-color", "--no-ext-diff", &base, "--"]);
            let diff = diff.args(&tracked).run()?;
            out.push_str(&String::from_utf8_lossy(&diff));
        }
        for path in untracked {
            let o = self.git().args(["diff", "--no-color", "--no-ext-diff", "--no-index", "--", "/dev/null"]).arg(path);
            let o = o.output()?;
            if !matches!(o.status.code(), Some(0 | 1)) {
                return Err(Error::Git {
                    args: format!("diff --no-index /dev/null {}", path.display()),
                    stderr: String::from_utf8_lossy(&o.stderr).trim().to_string(),
                });
            }
            out.push_str(&String::from_utf8_lossy(&o.stdout));
        }
        Ok(out)
    }

    /// Commits exactly `items` (as they are in the working tree, deletions
    /// included) on top of the baseline and returns the new commit. Other draft
    /// items stay as they were, in the working tree and the index. `author`
    /// (`"Name <email>"`) overrides git's identity; with neither, the commit
    /// is made as [`machine_author`] of the hostname.
    pub fn promote(&self, items: &[ItemRef], message: &str, author: Option<&str>) -> Result<String> {
        if message.trim().is_empty() {
            return Err(Error::Invalid("promote needs a message".into()));
        }
        // A promote is only useful if it can be pushed, and that needs the fleet branch.
        self.on_fleet_branch()?;
        let (selected, st) = self.select(items, "promoted")?;
        let paths: Vec<&PathBuf> = selected.iter().flat_map(|d| &d.paths).collect();

        // Build the commit in a throwaway index so the real one is untouched.
        let index = PathBuf::from(
            self.git().args(["rev-parse", "--path-format=absolute", "--git-path", "sk1ll-promote.index"]).text()?,
        );
        let _ = fs::remove_file(&index);
        let tree = (|| {
            let g = || self.git().env("GIT_INDEX_FILE", &index);
            match &st.head {
                Some(h) => g().args(["read-tree", h]).run()?,
                None => g().args(["read-tree", "--empty"]).run()?,
            };
            g().args(UPDATE_INDEX).stdin(nul_list(&paths)).run()?;
            g().arg("write-tree").text()
        })();
        let _ = fs::remove_file(&index);
        let tree = tree?;
        if let Some(h) = &st.head
            && self.git().args(["rev-parse", &format!("{h}^{{tree}}")]).text()? == tree
        {
            return Err(Error::Invalid("nothing to promote: those items already match the baseline".into()));
        }

        let mut commit = self.git().args(["commit-tree", &tree]);
        if let Some(h) = &st.head {
            commit = commit.args(["-p", h]);
        }
        let sha = commit.args(["-m", message]).envs(&self.identity_env(author)?).text()?;
        let mut update = self.git().args(["update-ref", "-m", "sk1ll: promote", "HEAD", &sha]);
        if let Some(h) = &st.head {
            update = update.arg(h);
        }
        update.run()?;
        // The promoted paths now match HEAD; bring the real index in line for them only.
        self.git().args(UPDATE_INDEX).stdin(nul_list(&paths)).run()?;
        Ok(sha)
    }

    /// Pushes [`FLEET_BRANCH`] to [`REMOTE`]; it must be checked out.
    pub fn push(&self) -> Result<()> {
        self.on_fleet_branch()?;
        let refspec = format!("refs/heads/{FLEET_BRANCH}:refs/heads/{FLEET_BRANCH}");
        self.git().args(["push", "--quiet", REMOTE, &refspec]).run()?;
        Ok(())
    }

    /// Returns `items` to the baseline: tracked paths are checked out from
    /// `HEAD`, paths the baseline doesn't have are deleted. The manifest can't
    /// be reset while the baseline has none, since that would delete it, and
    /// nothing is reset that would delete or replace an ignored file.
    pub fn reset(&self, items: &[ItemRef]) -> Result<()> {
        let (selected, st) = self.select(items, "reset")?;
        if selected.iter().any(|d| d.item == ItemRef::Manifest) && !st.head_paths.contains(Path::new(MANIFEST_FILE)) {
            return Err(Error::Invalid(format!(
                "there is no baseline to restore the manifest to; promote it first (it would delete {MANIFEST_FILE})"
            )));
        }
        // Ignored files aren't in the draft, but git would delete or overwrite
        // one that sits at, under or above a path being reset.
        let ignored = self.ignored_paths()?;
        for d in &selected {
            let clash = d.paths.iter().find_map(|p| ignored.iter().find(|i| i.starts_with(p) || p.starts_with(i)));
            if let Some(i) = clash {
                return Err(Error::Invalid(format!(
                    "`{}` can't be reset: that would delete or replace the ignored `{}`; move it out of the way first",
                    d.item,
                    i.display()
                )));
            }
        }
        let (in_head, new): (Vec<&PathBuf>, Vec<&PathBuf>) =
            selected.iter().flat_map(|d| &d.paths).partition(|p| st.head_paths.contains(*p));
        // New paths go first: one may be in the way of a baseline file (a file
        // that became a directory) that the checkout below brings back.
        if !new.is_empty() {
            self.git().args(["update-index", "--force-remove", "-z", "--stdin"]).stdin(nul_list(&new)).run()?;
            for rel in new {
                let path = self.root().join(rel);
                match fs::symlink_metadata(&path) {
                    Ok(m) if m.is_dir() => fs::remove_dir_all(&path).at(&path)?,
                    Ok(_) => fs::remove_file(&path).at(&path)?,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e).at(&path),
                }
                prune_empty_parents(self.root(), rel);
            }
        }
        if let (Some(head), false) = (&st.head, in_head.is_empty()) {
            self.git().literal().args(["checkout", "--quiet", head, "--"]).args(&in_head).run()?;
        }
        Ok(())
    }

    /// Resets the whole draft, including `other:` items, except a manifest
    /// the baseline doesn't have yet (see [`reset`](Self::reset)).
    pub fn reset_all(&self) -> Result<()> {
        let (draft, st) = self.draft_with_status()?;
        let keep_manifest = !st.head_paths.contains(Path::new(MANIFEST_FILE));
        let items: Vec<ItemRef> =
            draft.into_iter().map(|d| d.item).filter(|i| !(keep_manifest && *i == ItemRef::Manifest)).collect();
        if items.is_empty() { Ok(()) } else { self.reset(&items) }
    }

    /// Fetches [`REMOTE`] and moves to its baseline, carrying the draft on
    /// top. Promoted but unpushed commits are rebased onto it. If the new
    /// baseline touches an item the draft also changes (or the rebase
    /// conflicts), nothing changes and the outcome is
    /// [`PullOutcome::Blocked`]. A baseline with no commit in common with
    /// `HEAD` (unrelated histories) is an [`Error::Invalid`]; that
    /// also changes nothing.
    ///
    /// The baseline is [`REMOTE`]'s [`FLEET_BRANCH`], which must also be
    /// checked out here. A remote without that branch is an error too, unless
    /// it has no branches at all (an empty hub, which only older hubs start
    /// as): then there is nothing to pull yet, and the first push creates it.
    pub fn pull(&self) -> Result<PullOutcome> {
        let branch = self.on_fleet_branch()?;
        let all = format!("+refs/heads/*:refs/remotes/{REMOTE}/*");
        self.git().args(["fetch", "--quiet", "--prune", REMOTE, &all]).run()?;
        let Some(remote) = self.rev(&format!("refs/remotes/{REMOTE}/{FLEET_BRANCH}"))? else {
            let prefix = format!("refs/remotes/{REMOTE}/");
            let refs = self.git().args(["for-each-ref", "--format=%(refname)", &prefix]).text()?;
            let others: Vec<&str> =
                refs.lines().filter_map(|r| r.strip_prefix(prefix.as_str())).filter(|b| *b != "HEAD").collect();
            if others.is_empty() {
                return Ok(PullOutcome::UpToDate);
            }
            return Err(Error::Invalid(format!(
                "{REMOTE} has no `{FLEET_BRANCH}` branch (it has {}), so there is no baseline to pull; the fleet's \
                 baseline must be on `{FLEET_BRANCH}`",
                others.join(", ")
            )));
        };
        let Some(head) = self.rev("HEAD")? else {
            return Err(Error::Invalid(format!("`{branch}` has no commits yet; clone the repo from {REMOTE} instead")));
        };
        if self.is_ancestor(&remote, &head)? {
            return Ok(PullOutcome::UpToDate);
        }
        let Some(base) = self.merge_base(&head, &remote)? else {
            return Err(unrelated_histories(self.root(), &branch, &head, &remote));
        };
        let upstream = self.changed_items(&base, &remote)?;
        let (draft, st) = self.draft_with_status()?;
        let clashing: Vec<ItemRef> =
            draft.iter().filter(|d| upstream.contains(&d.item)).map(|d| d.item.clone()).collect();
        if !clashing.is_empty() {
            return Ok(PullOutcome::Blocked { baseline: remote, items: clashing });
        }
        // Local files the draft doesn't cover (ignored ones), which git would overwrite.
        let obstructed = self.obstructions(&base, &remote, &st.head_paths)?;
        if !obstructed.is_empty() {
            return Ok(PullOutcome::Blocked { baseline: remote, items: obstructed.into_iter().collect() });
        }

        if base == head {
            // Disjoint from the draft, so git carries it along untouched.
            self.git().args(["merge", "--ff-only", "--no-overwrite-ignore", "--quiet", &remote]).run()?;
        } else if let Some(items) = self.rebase_onto(&head, &base, &remote, &upstream, &draft)? {
            return Ok(PullOutcome::Blocked { baseline: remote, items });
        }
        let to = self.rev("HEAD")?.unwrap_or_default();
        Ok(PullOutcome::Updated { from: head, to })
    }

    /// Whether git has a configured `user.name` and `user.email`.
    pub fn has_git_identity(&self) -> Result<bool> {
        Ok(self.git().args(["config", "user.name"]).succeeds()?
            && self.git().args(["config", "user.email"]).succeeds()?)
    }

    /// Rebases the unpushed commits `base..head` onto `remote`, with the draft
    /// stashed meanwhile. Returns the clashing items if it had to back out.
    fn rebase_onto(
        &self,
        head: &str,
        base: &str,
        remote: &str,
        upstream: &BTreeSet<ItemRef>,
        draft: &[DraftItem],
    ) -> Result<Option<Vec<ItemRef>>> {
        let committer = self.committer_env()?;
        let stash = if draft.is_empty() { None } else { self.stash_draft(&committer)? };
        let unstash = || stash.as_deref().map_or(Ok(()), |oid| self.unstash(oid));

        let rebased = self.git().args(["rebase", "--quiet", "--no-autostash", remote]).envs(&committer).output()?;
        if !rebased.status.success() {
            // A rebase rejected before it starts (e.g. by a `pre-rebase` hook)
            // leaves nothing to abort; either way the draft comes back.
            let aborted = match self.rebase_in_progress()? {
                true => self.git().args(["rebase", "--abort"]).run().map(drop),
                false => Ok(()),
            };
            let restored = unstash();
            aborted?;
            restored?;
            let ours = self.changed_items(base, head)?;
            let both: Vec<ItemRef> = ours.intersection(upstream).cloned().collect();
            return Ok(Some(if both.is_empty() { ours.into_iter().collect() } else { both }));
        }
        if let Some(oid) = &stash
            && self.unstash(oid).is_err()
        {
            // Shouldn't happen (the draft is disjoint from upstream), but if it
            // does: back to the old baseline with the draft exactly as it was.
            self.git().args(["reset", "--hard", "--quiet", head]).run()?;
            let untracked = format!("{oid}^3");
            if self.rev(&untracked)?.is_some() {
                let listed = self.git().args(["ls-tree", "-r", "-z", "--name-only", &untracked]).run()?;
                for rel in split_paths(&listed) {
                    let _ = fs::remove_file(self.root().join(rel));
                }
            }
            self.unstash(oid)?;
            return Ok(Some(draft.iter().map(|d| d.item.clone()).collect()));
        }
        Ok(None)
    }

    /// Stashes the draft (untracked files included) and returns the stash
    /// commit, or `None` if there was nothing to stash.
    ///
    /// Each pull's stash has a message of its own, so its commit can't equal
    /// an older entry (same draft, same timestamps); and it only counts as
    /// ours if the stash list grew by one entry carrying that message.
    fn stash_draft(&self, committer: &[(String, String)]) -> Result<Option<String>> {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let message = format!("sk1ll pull: draft {}-{nanos}-{seq}", std::process::id());
        let list = || -> Result<Vec<(String, String)>> {
            let out = self.git().args(["stash", "list", "--format=%H %gs"]).text()?;
            Ok(out.lines().filter_map(|l| l.split_once(' ')).map(|(h, s)| (h.into(), s.into())).collect())
        };
        let before = list()?;
        self.git().args(["stash", "push", "--include-untracked", "--quiet", "-m", &message]).envs(committer).run()?;
        let after = list()?;
        match after.first() {
            _ if after.len() == before.len() => Ok(None),
            Some((oid, subject)) if after.len() == before.len() + 1 && subject.ends_with(&message) => {
                Ok(Some(oid.clone()))
            }
            _ => Err(Error::Invalid(format!("can't find the stash `{message}` pull just made (see `git stash list`)"))),
        }
    }

    /// Applies the stash commit `oid` (index included) and drops that entry,
    /// wherever it now is in the stash list; no other entry is touched.
    fn unstash(&self, oid: &str) -> Result<()> {
        self.git().args(["stash", "apply", "--index", "--quiet", oid]).run()?;
        let list = self.git().args(["stash", "list", "--format=%H"]).text()?;
        if let Some(n) = list.lines().position(|l| l == oid) {
            self.git().args(["stash", "drop", "--quiet", &format!("stash@{{{n}}}")]).run()?;
        }
        Ok(())
    }

    fn rebase_in_progress(&self) -> Result<bool> {
        for dir in ["rebase-merge", "rebase-apply"] {
            let out = self.git().args(["rev-parse", "--path-format=absolute", "--git-path", dir]).run()?;
            if path_from_bytes(out.strip_suffix(b"\n").unwrap_or(&out)).exists() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Items with paths `from..to` adds or changes that local files the
    /// baseline doesn't track are in the way of (ignored files, say): git
    /// would overwrite or delete them.
    fn obstructions(&self, from: &str, to: &str, head_paths: &BTreeSet<PathBuf>) -> Result<BTreeSet<ItemRef>> {
        let args = ["diff", "--name-only", "-z", "--no-renames", "--relative", "--diff-filter=AMT", from, to];
        let out = self.git().args(args).run()?;
        let root = self.root();
        let tracked = |abs: &Path| abs.strip_prefix(root).is_ok_and(|p| head_paths.contains(p));
        let untracked_at = |rel: &Path| match fs::symlink_metadata(root.join(rel)) {
            Err(_) => false,
            Ok(m) if m.is_dir() => WalkDir::new(root.join(rel))
                .min_depth(1)
                .into_iter()
                .filter_map(|e| e.ok())
                .any(|e| !e.file_type().is_dir() && !tracked(e.path())),
            Ok(_) => !head_paths.contains(rel),
        };
        let mut items = BTreeSet::new();
        for rel in split_paths(&out) {
            // The path itself, or a file where the new baseline needs a directory.
            let file_in_the_way = rel.ancestors().skip(1).filter(|a| !a.as_os_str().is_empty()).any(|a| {
                fs::symlink_metadata(root.join(a)).is_ok_and(|m| !m.is_dir()) && !head_paths.contains(a)
            });
            if file_in_the_way || untracked_at(&rel) {
                items.insert(ItemRef::for_path(&rel));
            }
        }
        Ok(items)
    }

    /// Ignored, untracked files under the repo root, relative to it.
    fn ignored_paths(&self) -> Result<Vec<PathBuf>> {
        let out = self.git().args(["ls-files", "-z", "--others", "--ignored", "--exclude-standard", "--", "."]).run()?;
        Ok(split_paths(&out).collect())
    }

    fn git(&self) -> Git {
        Git::new(self.root())
    }

    fn rev(&self, spec: &str) -> Result<Option<String>> {
        let out = self.git().args(["rev-parse", "--verify", "--quiet", &format!("{spec}^{{commit}}")]).output()?;
        Ok(out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()))
    }

    fn is_ancestor(&self, ancestor: &str, of: &str) -> Result<bool> {
        let out = self.git().args(["merge-base", "--is-ancestor", ancestor, of]).output()?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(Error::Git {
                args: format!("merge-base --is-ancestor {ancestor} {of}"),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            }),
        }
    }

    /// The best common ancestor of `a` and `b`; `None` if they share no
    /// history at all (git exits 1 with no output).
    fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>> {
        let out = self.git().args(["merge-base", a, b]).output()?;
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        match out.status.code() {
            Some(0) if !stdout.is_empty() => Ok(Some(stdout)),
            Some(1) if stdout.is_empty() && out.stderr.is_empty() => Ok(None),
            _ => Err(Error::Git {
                args: format!("merge-base {a} {b}"),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            }),
        }
    }

    fn current_branch(&self) -> Result<String> {
        self.git()
            .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
            .text()
            .map_err(|_| Error::Invalid(format!("HEAD is detached; check out `{FLEET_BRANCH}` first")))
    }

    /// [`FLEET_BRANCH`], if that is the branch checked out; else how to get there.
    fn on_fleet_branch(&self) -> Result<String> {
        let branch = self.current_branch()?;
        if branch != FLEET_BRANCH {
            return Err(Error::Invalid(format!(
                "{} is on branch `{branch}`, but the fleet's baseline is `{FLEET_BRANCH}`; switch with `git -C {} \
                 checkout {FLEET_BRANCH}`, or rename the branch with `git -C {} branch -m {branch} {FLEET_BRANCH}` \
                 if it holds this machine's work",
                self.root().display(),
                self.root().display(),
                self.root().display()
            )));
        }
        Ok(branch)
    }

    fn empty_tree(&self) -> Result<String> {
        self.git().args(["hash-object", "-t", "tree", "--stdin"]).stdin(Vec::new()).text()
    }

    fn changed_items(&self, from: &str, to: &str) -> Result<BTreeSet<ItemRef>> {
        let out = self.git().args(["diff", "--name-only", "-z", "--no-renames", "--relative", from, to]).run()?;
        Ok(split_paths(&out).map(|p| ItemRef::for_path(&p)).collect())
    }

    /// Author/committer env for a commit: `author` overrides the author;
    /// without a git identity, the fallback covers both.
    fn identity_env(&self, author: Option<&str>) -> Result<Vec<(String, String)>> {
        let configured = self.has_git_identity()?;
        let author = match author {
            Some(a) => a.to_string(),
            None if configured => return Ok(Vec::new()),
            None => machine_author(&machine::hostname()),
        };
        let (name, email) = split_author(&author);
        let mut env = vec![("GIT_AUTHOR_NAME".into(), name.clone()), ("GIT_AUTHOR_EMAIL".into(), email.clone())];
        if !configured {
            env.extend([("GIT_COMMITTER_NAME".into(), name), ("GIT_COMMITTER_EMAIL".into(), email)]);
        }
        Ok(env)
    }

    /// Committer fallback for git operations that write commits (stash, rebase).
    fn committer_env(&self) -> Result<Vec<(String, String)>> {
        let mut env = self.identity_env(None)?;
        env.retain(|(k, _)| k.starts_with("GIT_COMMITTER"));
        Ok(env)
    }

    fn draft_with_status(&self) -> Result<(Vec<DraftItem>, Status)> {
        let st = self.status()?;
        let mut groups: BTreeMap<ItemRef, Vec<PathBuf>> = BTreeMap::new();
        for path in st.changed.keys() {
            groups.entry(ItemRef::for_path(path)).or_default().push(path.clone());
        }
        let in_head: BTreeSet<ItemRef> = st.head_paths.iter().map(|p| ItemRef::for_path(p)).collect();
        let items = groups
            .into_iter()
            .map(|(item, paths)| {
                let change = match (in_head.contains(&item), self.in_tree(&item, &paths)) {
                    (true, true) => Change::Modified,
                    (false, true) => Change::Added,
                    (_, false) => Change::Deleted,
                };
                DraftItem { item, change, paths }
            })
            .collect();
        Ok((items, st))
    }

    /// The draft entries for `items`, to be `verb` (promoted, reset) as a
    /// whole: no other draft item may share a path with them (a file that
    /// became a directory, or the reverse), since git would change it too.
    fn select(&self, items: &[ItemRef], verb: &str) -> Result<(Vec<DraftItem>, Status)> {
        let (draft, st) = self.draft_with_status()?;
        let selected = pick(&draft, items)?;
        for other in draft.iter().filter(|d| !selected.iter().any(|s| s.item == d.item)) {
            for s in &selected {
                let overlap = s
                    .paths
                    .iter()
                    .find_map(|p| other.paths.iter().find(|o| o.starts_with(p) || p.starts_with(o)).map(|o| (p, o)));
                if let Some((p, o)) = overlap {
                    return Err(Error::Invalid(format!(
                        "`{}` can't be {verb} without `{}` (`{}` and `{}` overlap); name both",
                        s.item,
                        other.item,
                        p.display(),
                        o.display()
                    )));
                }
            }
        }
        Ok((selected, st))
    }

    fn in_tree(&self, item: &ItemRef, paths: &[PathBuf]) -> bool {
        let root = self.root();
        let exists = |rel: &str| fs::symlink_metadata(root.join(rel)).is_ok();
        match item {
            ItemRef::Skill(name) => [PERSONAL_DIR, IMPORTED_DIR].iter().any(|d| has_files(&root.join(d).join(name))),
            ItemRef::Fragment(name) => exists(&format!("{INSTRUCTIONS_DIR}/{name}.md")),
            ItemRef::Manifest => exists(MANIFEST_FILE) || exists(LOCK_FILE),
            // The name may be quoted (non-UTF-8); `paths` is exact.
            ItemRef::Other(_) => paths.iter().any(|p| fs::symlink_metadata(root.join(p)).is_ok()),
        }
    }

    fn status(&self) -> Result<Status> {
        let head = self.rev("HEAD")?;
        let head_paths = match &head {
            Some(h) => split_paths(&self.git().args(["ls-tree", "-r", "-z", "--name-only", h]).run()?).collect(),
            None => BTreeSet::new(),
        };
        // Porcelain paths are relative to the top level; the repo may be a subdirectory.
        let prefix = self.git().args(["rev-parse", "--show-prefix"]).run()?;
        let prefix = prefix.strip_suffix(b"\n").unwrap_or(&prefix);
        let out = self
            .git()
            .literal()
            .args(["status", "--porcelain=v1", "-z", "--untracked-files=all", "--no-renames", "--", "."])
            .run()?;
        let mut changed = BTreeMap::new();
        let mut add = |raw: &[u8], untracked: bool| {
            let rel = raw.strip_prefix(prefix).unwrap_or(raw);
            changed.insert(path_from_bytes(rel), untracked);
        };
        let mut entries = out.split(|b| *b == 0).filter(|e| !e.is_empty());
        while let Some(entry) = entries.next() {
            if entry.len() < 4 {
                continue;
            }
            // `XY to\0from` for renames/copies (only if rename detection is forced on).
            if matches!(entry[0], b'R' | b'C')
                && let Some(from) = entries.next()
                && entry[0] == b'R'
            {
                add(from, false);
            }
            add(&entry[3..], entry[0] == b'?');
        }
        Ok(Status { head, head_paths, changed })
    }
}

/// The draft entries for `items`; every one must be in the draft.
/// Why a pull can't move onto a baseline that shares no commit with `HEAD`
/// (say, two machines that each made a root commit on an empty hub). This is
/// an error rather than [`PullOutcome::Blocked`]: Blocked means "promote or
/// reset these items, then pull again", which can never resolve this.
fn unrelated_histories(root: &Path, branch: &str, head: &str, remote: &str) -> Error {
    let short = |s: &str| s.chars().take(12).collect::<String>();
    Error::Invalid(format!(
        "can't pull: this machine's `{branch}` ({}) and the hub's baseline ({}) have unrelated histories (no \
         common commit), so local commits can't be carried onto it. Nothing was changed. To keep this machine's \
         commits, stash the draft and run `git -C {} rebase --root --onto {REMOTE}/{branch}`; or move the repo \
         aside and clone it again from the hub",
        short(head),
        short(remote),
        root.display(),
    ))
}

fn pick(draft: &[DraftItem], items: &[ItemRef]) -> Result<Vec<DraftItem>> {
    if items.is_empty() {
        return Err(Error::Invalid("no items given".into()));
    }
    let mut selected = Vec::new();
    for item in items {
        match draft.iter().find(|d| d.item == *item) {
            Some(d) if !selected.contains(d) => selected.push(d.clone()),
            Some(_) => {}
            None => return Err(Error::Invalid(format!("`{item}` is not in the draft"))),
        }
    }
    Ok(selected)
}

fn split_author(author: &str) -> (String, String) {
    match author.rsplit_once('<') {
        Some((name, email)) => (name.trim().to_string(), email.trim().trim_end_matches('>').trim().to_string()),
        None => {
            let name = author.trim();
            let email = machine_author(name).rsplit_once('<').map(|(_, e)| e.trim_end_matches('>').to_string());
            (name.to_string(), email.unwrap_or_default())
        }
    }
}

/// Item names use `/`; only on Windows is `\\` a separator too.
fn slash(path: &str) -> String {
    if cfg!(windows) { path.replace('\\', "/") } else { path.to_string() }
}

/// Git's C-style quoting (as in `core.quotePath`), but keeping valid UTF-8 as is.
fn quote_path(raw: &[u8]) -> String {
    let mut out = String::from("\"");
    for chunk in raw.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '"' | '\\' => {
                    out.push('\\');
                    out.push(c);
                }
                '\t' => out.push_str("\\t"),
                '\n' => out.push_str("\\n"),
                c if c.is_ascii_control() => out.push_str(&format!("\\{:03o}", c as u32)),
                c => out.push(c),
            }
        }
        for b in chunk.invalid() {
            out.push_str(&format!("\\{b:03o}"));
        }
    }
    out.push('"');
    out
}

/// The inverse of [`quote_path`]; `None` if `quoted` isn't one of its outputs.
fn unquote_path(quoted: &str) -> Option<Vec<u8>> {
    let inner = quoted.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = Vec::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return None,
            '\\' => match chars.next()? {
                c @ ('"' | '\\') => out.push(c as u8),
                't' => out.push(b'\t'),
                'n' => out.push(b'\n'),
                d @ '0'..='3' => {
                    let mut byte = d as u8 - b'0';
                    for _ in 0..2 {
                        byte = byte * 8 + chars.next()?.to_digit(8)? as u8;
                    }
                    out.push(byte);
                }
                _ => return None,
            },
            c => out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
        }
    }
    Some(out)
}

fn has_files(dir: &Path) -> bool {
    WalkDir::new(dir).min_depth(1).into_iter().filter_map(|e| e.ok()).any(|e| !e.file_type().is_dir())
}

/// Removes now-empty directories above `rel`, keeping top-level ones like `personal/`.
fn prune_empty_parents(root: &Path, rel: &Path) {
    let mut dir = rel.parent();
    while let Some(d) = dir {
        if d.components().count() < 2 || fs::remove_dir(root.join(d)).is_err() {
            break;
        }
        dir = d.parent();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) -> String {
        Git::new(dir).args(args).text().unwrap()
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    /// A bare origin and two clones: `a` with a local git identity, `b` without one.
    struct Fleet {
        _tmp: tempfile::TempDir,
        a: PathBuf,
        b: PathBuf,
    }

    impl Fleet {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let origin = tmp.path().join("origin.git");
            let a = tmp.path().join("a");
            git(tmp.path(), &["init", "--quiet", "--bare", "-b", "main", origin.to_str().unwrap()]);
            git(tmp.path(), &["init", "--quiet", "-b", "main", a.to_str().unwrap()]);
            git(&a, &["config", "user.name", "Machine A"]);
            git(&a, &["config", "user.email", "a@example.com"]);
            write(&a.join("sk1ll.toml"), "");
            write(&a.join("sk1ll.lock"), "");
            write(&a.join("personal/pdf/SKILL.md"), "---\nname: pdf\n---\nPDFs.\n");
            write(&a.join("personal/pdf/ref.md"), "reference\n");
            write(&a.join("imported/web/SKILL.md"), "---\nname: web\n---\nWeb.\n");
            write(&a.join("instructions/git.md"), "Use Conventional Commits.\n");
            write(&a.join("instructions/style.md"), "Be terse.\n");
            git(&a, &["add", "-A"]);
            git(&a, &["commit", "--quiet", "-m", "seed"]);
            git(&a, &["remote", "add", REMOTE, origin.to_str().unwrap()]);
            git(&a, &["push", "--quiet", "-u", REMOTE, "main"]);
            let b = tmp.path().join("b");
            git(tmp.path(), &["clone", "--quiet", origin.to_str().unwrap(), b.to_str().unwrap()]);
            Self { _tmp: tmp, a, b }
        }
    }

    fn repo(root: &Path) -> Repo {
        Repo::open(root).unwrap()
    }

    fn items(repo: &Repo) -> Vec<(String, Change)> {
        repo.draft().unwrap().into_iter().map(|d| (d.item.to_string(), d.change)).collect()
    }

    fn item(s: &str) -> ItemRef {
        s.parse().unwrap()
    }

    /// Everything a pull could disturb: files, HEAD, index and stash.
    fn snapshot(root: &Path) -> (BTreeMap<PathBuf, Vec<u8>>, String, String, String) {
        let files = WalkDir::new(root)
            .into_iter()
            .filter_entry(|e| e.file_name() != ".git")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .map(|e| (e.path().strip_prefix(root).unwrap().to_path_buf(), fs::read(e.path()).unwrap()))
            .collect();
        let stash = Git::new(root).args(["stash", "list"]).text().unwrap();
        (files, git(root, &["rev-parse", "HEAD"]), git(root, &["ls-files", "-s"]), stash)
    }

    #[test]
    fn item_refs_round_trip() {
        for s in ["skill:pdf", "fragment:10-git", "manifest", "other:notes/a.txt"] {
            assert_eq!(item(s).to_string(), s);
            assert_eq!(serde_json::to_string(&item(s)).unwrap(), format!("\"{s}\""));
        }
        assert!("bogus:x".parse::<ItemRef>().is_err() && "skill:".parse::<ItemRef>().is_err());
        let for_path = |p: &str| ItemRef::for_path(Path::new(p)).to_string();
        assert_eq!(for_path("imported/web/sub/x.md"), "skill:web");
        assert_eq!(for_path("personal/.gitkeep"), "other:personal/.gitkeep");
        assert_eq!(for_path("instructions/git.md"), "fragment:git");
        assert_eq!(for_path("instructions/.gitkeep"), "other:instructions/.gitkeep");
        assert_eq!(for_path("sk1ll.lock"), "manifest");
        assert_eq!(
            default_promote_message(&[item("skill:pdf"), item("fragment:git-flow")], "laptop"),
            "promote pdf, git-flow from laptop"
        );
    }

    #[test]
    fn detects_draft_items() {
        let f = Fleet::new();
        let r = repo(&f.a);
        assert!(r.draft().unwrap().is_empty());

        write(&f.a.join("personal/pdf/SKILL.md"), "changed\n");
        write(&f.a.join("personal/new/SKILL.md"), "new\n");
        write(&f.a.join("personal/new/deep/x.md"), "x\n");
        fs::remove_dir_all(f.a.join("imported/web")).unwrap();
        write(&f.a.join("instructions/git.md"), "changed\n");
        git(&f.a, &["mv", "instructions/style.md", "instructions/tone.md"]);
        write(&f.a.join("sk1ll.toml"), "# edited\n");
        write(&f.a.join("notes.txt"), "n\n");

        use Change::*;
        let expect = [
            ("skill:new", Added),
            ("skill:pdf", Modified),
            ("skill:web", Deleted),
            ("fragment:git", Modified),
            ("fragment:style", Deleted),
            ("fragment:tone", Added),
            ("manifest", Modified),
            ("other:notes.txt", Added),
        ];
        assert_eq!(items(&r), expect.map(|(i, c)| (i.to_string(), c)));
        let draft = r.draft().unwrap();
        let new = draft.iter().find(|d| d.item == item("skill:new")).unwrap();
        assert_eq!(new.paths, [PathBuf::from("personal/new/SKILL.md"), PathBuf::from("personal/new/deep/x.md")]);

        let diff = r.diff(&item("skill:new")).unwrap();
        assert!(diff.contains("+++ b/personal/new/SKILL.md") && diff.contains("+new"), "{diff}");
        let diff = r.diff(&item("skill:pdf")).unwrap();
        assert!(diff.contains("-PDFs.") && diff.contains("+changed"), "{diff}");
        assert!(r.diff(&item("fragment:tone")).unwrap().contains("+Be terse."));
        assert!(matches!(r.diff(&item("skill:nope")), Err(Error::Invalid(_))));
        assert_eq!(ItemRef::resolve("pdf", &draft).unwrap(), item("skill:pdf"));
        assert!(ItemRef::resolve("nope", &draft).is_err());
    }

    #[test]
    fn partial_promote_leaves_the_rest_in_draft() {
        let f = Fleet::new();
        let r = repo(&f.b); // b has no git identity
        assert!(!r.has_git_identity().unwrap());
        write(&f.b.join("personal/pdf/SKILL.md"), "promoted\n");
        fs::remove_dir_all(f.b.join("imported/web")).unwrap();
        write(&f.b.join("instructions/git.md"), "draft only\n");
        write(&f.b.join("notes.txt"), "staged\n");
        git(&f.b, &["add", "notes.txt"]);

        assert!(matches!(r.promote(&[item("skill:nope")], "m", None), Err(Error::Invalid(_))));
        let sha = r.promote(&[item("skill:pdf"), item("skill:web")], "promote pdf, web from b", None).unwrap();
        assert_eq!(git(&f.b, &["rev-parse", "HEAD"]), sha);
        let files = git(&f.b, &["show", "--name-only", "--format=", "HEAD"]);
        assert_eq!(files.lines().collect::<Vec<_>>(), ["imported/web/SKILL.md", "personal/pdf/SKILL.md"]);
        let expected = format!("{}|promote pdf, web from b", machine::hostname());
        assert_eq!(git(&f.b, &["log", "-1", "--format=%an|%s"]), expected);
        assert_eq!(items(&r), [("fragment:git".into(), Change::Modified), ("other:notes.txt".into(), Change::Added)]);
        assert_eq!(git(&f.b, &["diff", "--cached", "--name-only"]), "notes.txt");
        assert_eq!(read(&f.b.join("instructions/git.md")), "draft only\n");

        let sha = r.promote(&[item("fragment:git")], "git", Some("laptop <sk1ll@laptop>")).unwrap();
        assert_eq!(git(&f.b, &["log", "-1", "--format=%an <%ae>"]), "laptop <sk1ll@laptop>");
        r.push().unwrap();
        assert_eq!(git(&f.b, &["rev-parse", "origin/main"]), sha);
        assert_eq!(items(&r), [("other:notes.txt".into(), Change::Added)]);
    }

    #[test]
    fn reset_restores_baseline() {
        let f = Fleet::new();
        let r = repo(&f.a);
        let before = snapshot(&f.a);
        write(&f.a.join("personal/pdf/SKILL.md"), "changed\n");
        write(&f.a.join("personal/new/deep/SKILL.md"), "new\n");
        write(&f.a.join("instructions/new.md"), "staged new\n");
        git(&f.a, &["add", "instructions/new.md"]);
        fs::remove_dir_all(f.a.join("imported/web")).unwrap();
        write(&f.a.join("notes.txt"), "n\n");

        r.reset(&[item("skill:pdf"), item("skill:new"), item("fragment:new"), item("skill:web")]).unwrap();
        assert_eq!(items(&r), [("other:notes.txt".into(), Change::Added)]);
        assert!(!f.a.join("personal/new").exists());
        assert!(f.a.join("personal").is_dir());
        r.reset_all().unwrap();
        assert!(r.draft().unwrap().is_empty());
        assert_eq!(snapshot(&f.a), before);
    }

    #[test]
    fn pull_with_clean_draft_fast_forwards() {
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        assert_eq!(b.pull().unwrap(), PullOutcome::UpToDate);
        let old = git(&f.b, &["rev-parse", "HEAD"]);
        write(&f.a.join("personal/pdf/SKILL.md"), "v2\n");
        let new = a.promote(&[item("skill:pdf")], "v2", None).unwrap();
        a.push().unwrap();
        assert_eq!(b.pull().unwrap(), PullOutcome::Updated { from: old, to: new });
        assert_eq!(read(&f.b.join("personal/pdf/SKILL.md")), "v2\n");
        assert_eq!(b.pull().unwrap(), PullOutcome::UpToDate);
    }

    #[test]
    fn pull_carries_a_disjoint_draft() {
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        write(&f.b.join("instructions/git.md"), "b draft\n");
        write(&f.b.join("personal/new/SKILL.md"), "b new\n");
        write(&f.b.join("notes.txt"), "staged\n");
        git(&f.b, &["add", "notes.txt"]);
        let draft = b.draft().unwrap();

        write(&f.a.join("personal/pdf/SKILL.md"), "v2\n");
        let v2 = a.promote(&[item("skill:pdf")], "v2", None).unwrap();
        a.push().unwrap();
        assert!(matches!(b.pull().unwrap(), PullOutcome::Updated { to, .. } if to == v2));
        assert_eq!(read(&f.b.join("personal/pdf/SKILL.md")), "v2\n");
        assert_eq!(b.draft().unwrap(), draft);
        assert_eq!(read(&f.b.join("instructions/git.md")), "b draft\n");
        assert_eq!(git(&f.b, &["diff", "--cached", "--name-only"]), "notes.txt");

        // Promoted but unpushed commits are rebased onto the new baseline.
        write(&f.b.join("instructions/style.md"), "b promoted\n");
        b.promote(&[item("fragment:style")], "style from b", None).unwrap();
        write(&f.a.join("imported/web/SKILL.md"), "web v2\n");
        let web = a.promote(&[item("skill:web")], "web v2", None).unwrap();
        a.push().unwrap();
        assert!(matches!(b.pull().unwrap(), PullOutcome::Updated { .. }));
        assert_eq!(git(&f.b, &["rev-parse", "HEAD~1"]), web);
        assert_eq!(git(&f.b, &["log", "-1", "--format=%s"]), "style from b");
        assert_eq!(read(&f.b.join("imported/web/SKILL.md")), "web v2\n");
        assert_eq!(read(&f.b.join("instructions/style.md")), "b promoted\n");
        assert_eq!(b.draft().unwrap(), draft);
        assert_eq!(git(&f.b, &["diff", "--cached", "--name-only"]), "notes.txt");
        assert_eq!(git(&f.b, &["stash", "list"]), "");
    }

    #[test]
    fn pull_blocked_by_clash_changes_nothing() {
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        write(&f.b.join("personal/pdf/ref.md"), "b's edit\n");
        write(&f.b.join("personal/new/SKILL.md"), "b new\n");
        write(&f.a.join("personal/pdf/SKILL.md"), "a's edit\n");
        let remote = a.promote(&[item("skill:pdf")], "a", None).unwrap();
        a.push().unwrap();

        let before = snapshot(&f.b);
        let outcome = b.pull().unwrap();
        assert_eq!(outcome, PullOutcome::Blocked { baseline: remote.clone(), items: vec![item("skill:pdf")] });
        assert_eq!(snapshot(&f.b), before);

        // An unpushed promote that conflicts with the new baseline backs out too.
        b.reset(&[item("skill:pdf")]).unwrap();
        write(&f.b.join("personal/pdf/SKILL.md"), "b's promoted edit\n");
        b.promote(&[item("skill:pdf")], "b", None).unwrap();
        write(&f.b.join("personal/pdf/ref.md"), "unrelated?\n");
        let before = snapshot(&f.b);
        // The draft touches pdf too, so this is caught before anything moves.
        assert!(matches!(b.pull().unwrap(), PullOutcome::Blocked { .. }));
        assert_eq!(snapshot(&f.b), before);

        b.reset(&[item("skill:pdf")]).unwrap();
        let before = snapshot(&f.b);
        let outcome = b.pull().unwrap();
        assert_eq!(outcome, PullOutcome::Blocked { baseline: remote, items: vec![item("skill:pdf")] });
        assert_eq!(snapshot(&f.b), before);
        assert!(f.b.join("personal/new/SKILL.md").is_file());
    }

    fn commit_all(root: &Path, message: &str) {
        git(root, &["add", "-A"]);
        git(root, &["commit", "--quiet", "-m", message]);
    }

    fn invalid<T: fmt::Debug>(result: Result<T>) -> String {
        match result {
            Err(Error::Invalid(msg)) => msg,
            other => panic!("expected Error::Invalid, got {other:?}"),
        }
    }

    #[test]
    fn reset_and_promote_refuse_to_cross_item_boundaries() {
        let f = Fleet::new();
        let r = repo(&f.a);
        write(&f.a.join("notes"), "baseline file\n");
        write(&f.a.join("docs/private"), "baseline\n");
        commit_all(&f.a, "notes and docs");
        let head = git(&f.a, &["rev-parse", "HEAD"]);
        // A file replaced by a directory, and a directory replaced by a file.
        fs::remove_file(f.a.join("notes")).unwrap();
        write(&f.a.join("notes/private.txt"), "secret\n");
        fs::remove_dir_all(f.a.join("docs")).unwrap();
        write(&f.a.join("docs"), "now a file\n");
        let names: Vec<String> = items(&r).into_iter().map(|(i, _)| i).collect();
        assert_eq!(names, ["other:docs", "other:docs/private", "other:notes", "other:notes/private.txt"]);

        let before = snapshot(&f.a);
        assert!(invalid(r.reset(&[item("other:notes")])).contains("other:notes/private.txt"));
        assert!(invalid(r.reset(&[item("other:notes/private.txt")])).contains("other:notes"));
        assert!(invalid(r.promote(&[item("other:docs")], "m", None)).contains("other:docs/private"));
        assert!(invalid(r.promote(&[item("other:docs/private")], "m", None)).contains("other:docs"));
        assert_eq!(snapshot(&f.a), before);
        assert_eq!(git(&f.a, &["rev-parse", "HEAD"]), head);

        // Together they're fine.
        r.reset(&[item("other:notes"), item("other:notes/private.txt")]).unwrap();
        assert_eq!(read(&f.a.join("notes")), "baseline file\n");
        r.promote(&[item("other:docs"), item("other:docs/private")], "docs", None).unwrap();
        assert_eq!(git(&f.a, &["show", "HEAD:docs"]), "now a file");
        assert!(r.draft().unwrap().is_empty());
    }

    #[test]
    fn pull_never_overwrites_ignored_files() {
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        write(&f.b.join(".git/info/exclude"), "secret.txt\nbuild\n");
        write(&f.b.join("secret.txt"), "mine\n");
        write(&f.b.join("build"), "an ignored file where upstream wants a directory\n");
        assert!(b.draft().unwrap().is_empty());
        write(&f.a.join("secret.txt"), "upstream\n");
        write(&f.a.join("build/out.txt"), "upstream\n");
        let remote = a.promote(&[item("other:secret.txt"), item("other:build/out.txt")], "secret", None).unwrap();
        a.push().unwrap();

        // Fast-forward path.
        let before = snapshot(&f.b);
        let blocked = PullOutcome::Blocked {
            baseline: remote.clone(),
            items: vec![item("other:build/out.txt"), item("other:secret.txt")],
        };
        assert_eq!(b.pull().unwrap(), blocked);
        assert_eq!(snapshot(&f.b), before);

        // Rebase path.
        write(&f.b.join("instructions/style.md"), "b promoted\n");
        b.promote(&[item("fragment:style")], "style", None).unwrap();
        let before = snapshot(&f.b);
        assert_eq!(b.pull().unwrap(), blocked);
        assert_eq!(snapshot(&f.b), before);
        assert_eq!(read(&f.b.join("secret.txt")), "mine\n");
    }

    #[cfg(unix)]
    #[test]
    fn rebase_that_never_starts_restores_the_draft() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        // The user's own stash must survive untouched.
        write(&f.b.join("personal/pdf/ref.md"), "user stash\n");
        git(&f.b, &["stash", "push", "--quiet", "-m", "user's own"]);
        write(&f.b.join("instructions/style.md"), "b promoted\n");
        b.promote(&[item("fragment:style")], "style", None).unwrap();
        write(&f.b.join("instructions/git.md"), "b draft\n");
        write(&f.b.join("notes.txt"), "untracked\n");
        write(&f.a.join("imported/web/SKILL.md"), "web v2\n");
        a.promote(&[item("skill:web")], "web v2", None).unwrap();
        a.push().unwrap();
        let hook = f.b.join(".git/hooks/pre-rebase");
        write(&hook, "#!/bin/sh\nexit 1\n");
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

        let before = snapshot(&f.b);
        assert!(matches!(b.pull().unwrap(), PullOutcome::Blocked { .. }));
        assert_eq!(snapshot(&f.b), before);
    }

    #[cfg(unix)]
    #[test]
    fn paths_are_byte_exact() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let f = Fleet::new();
        let r = repo(&f.a);
        write(&f.a.join("notes/b"), "tracked\n");
        commit_all(&f.a, "notes/b");
        write(&f.a.join("notes\\b"), "untracked\n");
        let bad = OsStr::from_bytes(b"bad\xff.txt");
        fs::write(f.a.join(bad), "x\n").unwrap();

        assert_eq!(
            items(&r),
            [("other:\"bad\\377.txt\"".into(), Change::Added), ("other:notes\\b".into(), Change::Added)]
        );
        let draft = r.draft().unwrap();
        assert_eq!(draft[0].paths, [PathBuf::from(bad)]);
        assert_eq!(ItemRef::resolve("\"bad\\377.txt\"", &draft).unwrap(), draft[0].item);

        r.reset(&[item("other:notes\\b")]).unwrap();
        assert_eq!(git(&f.a, &["ls-files", "notes"]), "notes/b");
        assert_eq!(read(&f.a.join("notes/b")), "tracked\n");
        assert!(!f.a.join("notes\\b").exists());
        r.reset(&[draft[0].item.clone()]).unwrap();
        assert!(!f.a.join(bad).exists());
        assert!(r.draft().unwrap().is_empty());
    }

    #[test]
    fn reset_keeps_a_manifest_that_was_never_promoted() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        let r = Repo::init(&root).unwrap();
        r.reset_all().unwrap();
        assert!(root.join(MANIFEST_FILE).is_file());
        assert_eq!(items(&r), [("manifest".into(), Change::Added)]);
        assert!(invalid(r.reset(&[ItemRef::Manifest])).contains("promote it first"));

        // A baseline without the manifest is no better.
        git(&root, &["config", "user.name", "T"]);
        git(&root, &["config", "user.email", "t@example.com"]);
        write(&root.join("notes.txt"), "n\n");
        git(&root, &["add", "notes.txt"]);
        git(&root, &["commit", "--quiet", "-m", "notes"]);
        write(&root.join("instructions/x.md"), "x\n");
        r.reset_all().unwrap();
        assert!(root.join(MANIFEST_FILE).is_file());
        assert_eq!(items(&r), [("manifest".into(), Change::Added)]);
        write(&root.join("instructions/x.md"), "x\n");
        assert!(invalid(r.reset(&[ItemRef::Manifest, item("fragment:x")])).contains("promote it first"));
        assert!(root.join("instructions/x.md").is_file());
    }

    #[test]
    fn promote_refuses_a_detached_head() {
        let f = Fleet::new();
        let r = repo(&f.a);
        git(&f.a, &["checkout", "--quiet", "--detach"]);
        write(&f.a.join("personal/pdf/SKILL.md"), "changed\n");
        let before = snapshot(&f.a);
        assert!(invalid(r.promote(&[item("skill:pdf")], "m", None)).contains("detached"));
        assert!(invalid(r.push()).contains("detached"));
        assert_eq!(snapshot(&f.a), before);
    }

    #[test]
    fn reset_refuses_to_delete_ignored_content() {
        let f = Fleet::new();
        let r = repo(&f.a);
        write(&f.a.join("notes"), "baseline file\n");
        write(&f.a.join("docs/private"), "baseline\n");
        commit_all(&f.a, "notes and docs");
        write(&f.a.join(".git/info/exclude"), "notes/private.txt\n/docs\n");
        // Ignored content below a selected path (the file became a directory)
        // and above one (the directory became a file).
        fs::remove_file(f.a.join("notes")).unwrap();
        write(&f.a.join("notes/private.txt"), "secret\n");
        fs::remove_dir_all(f.a.join("docs")).unwrap();
        write(&f.a.join("docs"), "ignored file\n");
        let names: Vec<String> = items(&r).into_iter().map(|(i, _)| i).collect();
        assert_eq!(names, ["other:docs/private", "other:notes"]);

        let before = snapshot(&f.a);
        assert!(invalid(r.reset(&[item("other:notes")])).contains("`notes/private.txt`"));
        assert!(invalid(r.reset(&[item("other:docs/private")])).contains("`docs`"));
        assert!(invalid(r.reset_all()).contains("ignored"));
        assert_eq!(snapshot(&f.a), before);

        // Promote doesn't touch the working tree, so the ignored files are safe.
        r.promote(&[item("other:notes")], "drop notes", None).unwrap();
        assert_eq!(read(&f.a.join("notes/private.txt")), "secret\n");
        // Once the ignored file is moved away, reset goes ahead.
        fs::remove_file(f.a.join("docs")).unwrap();
        r.reset(&[item("other:docs/private")]).unwrap();
        assert_eq!(read(&f.a.join("docs/private")), "baseline\n");
    }

    /// Byte strings that are hard to name: quotes, backslashes, escapes
    /// spelled out literally, control characters and invalid UTF-8.
    #[cfg(unix)]
    fn tricky_names() -> Vec<Vec<u8>> {
        let atoms: [&[u8]; 11] =
            [b"\"", b"\\", b"\\377", b"\\\"", b"\n", b"\t", b"\xff", b"\xc3", "é".as_bytes(), b"a", b"/"];
        let mut names: Vec<Vec<u8>> = vec![b"x".to_vec()];
        for a in atoms {
            for b in atoms {
                for c in atoms {
                    names.push([a, b, c].concat());
                    names.push([a, b, c, b".txt"].concat());
                }
            }
        }
        names.extend([b"bad\xff.txt".to_vec(), b"\"bad\\377.txt\"".to_vec(), b"\"\"".to_vec()]);
        // Only well-formed relative paths: no empty, `.` or `..` components.
        names.retain(|n| n.split(|b| *b == b'/').all(|c| !c.is_empty() && c != b"." && c != b".."));
        names.sort();
        names.dedup();
        names
    }

    #[cfg(unix)]
    #[test]
    fn other_item_names_are_injective_and_round_trip() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let names = tricky_names();
        assert!(names.len() > 1000);
        let mut seen = BTreeMap::new();
        for raw in &names {
            let path = Path::new(OsStr::from_bytes(raw));
            let it = ItemRef::for_path(path);
            let ItemRef::Other(_) = &it else { continue };
            assert_eq!(it.to_string().parse::<ItemRef>().unwrap(), it, "{raw:?}");
            assert_eq!(it.other_path().as_deref(), Some(path), "{it}");
            if let Some(prev) = seen.insert(it.clone(), raw.clone()) {
                panic!("{prev:?} and {raw:?} are both {it}");
            }
        }
        let bad = Path::new(OsStr::from_bytes(b"bad\xff.txt"));
        assert_eq!(ItemRef::for_path(bad).to_string(), r#"other:"bad\377.txt""#);
        assert_eq!(ItemRef::for_path(Path::new(r#""bad\377.txt""#)).to_string(), r#"other:"\"bad\\377.txt\"""#);
        assert_eq!(ItemRef::for_path(Path::new(r"notes\b")).to_string(), r"other:notes\b");
        assert_eq!(ItemRef::for_path(Path::new("a\nb")).to_string(), r#"other:"a\nb""#);
        // Malformed quoted names name no path.
        for bad in [r#"other:""#, r#"other:"a"#, r#"other:"\q""#, r#"other:"\777""#, r#"other:"a"b""#] {
            assert_eq!(item(bad).other_path(), None, "{bad}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn quoted_and_literal_names_stay_distinct() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let f = Fleet::new();
        let r = repo(&f.a);
        let bad = OsStr::from_bytes(b"bad\xff.txt");
        let literal = r#""bad\377.txt""#;
        fs::write(f.a.join(bad), "x\n").unwrap();
        fs::write(f.a.join(literal), "y\n").unwrap();
        let draft = r.draft().unwrap();
        assert_eq!(draft.len(), 2);
        let of = |p: &Path| draft.iter().find(|d| d.paths == [p.to_path_buf()]).unwrap().item.clone();
        let (bad_item, literal_item) = (of(Path::new(bad)), of(Path::new(literal)));
        assert_ne!(bad_item, literal_item);

        r.reset(&[bad_item]).unwrap();
        assert!(!f.a.join(bad).exists());
        assert_eq!(read(&f.a.join(literal)), "y\n");
        r.reset(&[ItemRef::resolve(&literal_item.to_string(), &r.draft().unwrap()).unwrap()]).unwrap();
        assert!(r.draft().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn rejected_rebase_restores_the_draft_despite_an_identical_stash() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        // a: an unpushed promote, so the pull rebases; b moves the baseline.
        write(&f.a.join("instructions/style.md"), "a promoted\n");
        a.promote(&[item("fragment:style")], "style", None).unwrap();
        write(&f.b.join("imported/web/SKILL.md"), "web v2\n");
        b.promote(&[item("skill:web")], "web v2", None).unwrap();
        b.push().unwrap();
        write(&f.a.join("instructions/git.md"), "a draft\n");
        write(&f.a.join("notes.txt"), "untracked\n");

        // Frozen clocks: a stash of the same draft, made the way pull used to
        // make its own, already sits on top of the stash list.
        let date = "1700000000 +0000";
        let _clock = crate::git::test_env(&[("GIT_COMMITTER_DATE", date), ("GIT_AUTHOR_DATE", date)]);
        git(&f.a, &["stash", "push", "--include-untracked", "--quiet", "-m", "sk1ll pull: draft"]);
        git(&f.a, &["stash", "apply", "--index", "--quiet"]);
        let hook = f.a.join(".git/hooks/pre-rebase");
        write(&hook, "#!/bin/sh\nexit 1\n");
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

        let before = snapshot(&f.a);
        assert!(matches!(a.pull().unwrap(), PullOutcome::Blocked { .. }));
        assert_eq!(snapshot(&f.a), before);
        assert_eq!(read(&f.a.join("instructions/git.md")), "a draft\n");
    }

    #[test]
    fn pull_refuses_unrelated_histories_without_touching_anything() {
        // Two machines joined an empty hub and each made its own root commit.
        let f = Fleet::new();
        let origin = f._tmp.path().join("origin.git");
        let c = f._tmp.path().join("c");
        git(f._tmp.path(), &["init", "--quiet", "-b", "main", c.to_str().unwrap()]);
        git(&c, &["config", "user.name", "Machine C"]);
        git(&c, &["config", "user.email", "c@example.com"]);
        write(&c.join("sk1ll.toml"), "# c\n");
        write(&c.join("personal/pdf/SKILL.md"), "c's own root\n");
        commit_all(&c, "c root");
        git(&c, &["remote", "add", REMOTE, origin.to_str().unwrap()]);
        write(&c.join("instructions/git.md"), "c draft\n");
        let r = repo(&c);
        let before = snapshot(&c);
        for _ in 0..2 {
            let message = invalid(r.pull());
            assert!(message.contains("unrelated histories"), "{message}");
            assert_eq!(snapshot(&c), before);
        }
        assert_eq!(items(&r), [("fragment:git".to_string(), Change::Added)]);
    }

    /// A repo started on its own (not cloned) on `c`'s branch, with the fleet's origin.
    fn independent(f: &Fleet, branch: &str, origin: &Path) -> PathBuf {
        let c = f._tmp.path().join(format!("c-{branch}"));
        git(f._tmp.path(), &["init", "--quiet", "-b", branch, c.to_str().unwrap()]);
        git(&c, &["config", "user.name", "Machine C"]);
        git(&c, &["config", "user.email", "c@example.com"]);
        write(&c.join("sk1ll.toml"), "# c\n");
        commit_all(&c, "c root");
        git(&c, &["remote", "add", REMOTE, origin.to_str().unwrap()]);
        c
    }

    fn origin_branches(origin: &Path) -> String {
        git(origin, &["for-each-ref", "--format=%(refname)", "refs/heads/"])
    }

    #[test]
    fn pull_and_push_refuse_a_branch_other_than_the_fleet_branch() {
        // Adopted from an independent `git init` on `master`: there is no
        // `origin/master`, which must not read as "up to date".
        let f = Fleet::new();
        let origin = f._tmp.path().join("origin.git");
        let c = independent(&f, "master", &origin);
        let r = repo(&c);
        write(&c.join("personal/pdf/SKILL.md"), "c's pdf\n");
        let before = snapshot(&c);
        for message in [invalid(r.pull()), invalid(r.push()), invalid(r.promote(&[item("skill:pdf")], "m", None))] {
            assert!(message.contains("`master`") && message.contains("`main`"), "{message}");
            assert!(message.contains("branch -m master main"), "{message}");
        }
        assert_eq!(snapshot(&c), before);
        assert_eq!(origin_branches(&origin), "refs/heads/main");
    }

    #[test]
    fn pull_needs_the_fleet_branch_on_origin_unless_origin_is_empty() {
        let f = Fleet::new();
        // An origin whose only branch isn't `main`.
        let other = f._tmp.path().join("other.git");
        git(f._tmp.path(), &["init", "--quiet", "--bare", "-b", "main", other.to_str().unwrap()]);
        git(&f.a, &["push", "--quiet", other.to_str().unwrap(), "main:refs/heads/trunk"]);
        let c = independent(&f, "main", &other);
        let r = repo(&c);
        let before = snapshot(&c);
        let message = invalid(r.pull());
        assert!(message.contains("no `main` branch") && message.contains("trunk"), "{message}");
        assert_eq!(snapshot(&c), before);

        // An empty origin (an older hub): nothing to pull, and the first push creates `main`.
        let empty = f._tmp.path().join("empty.git");
        git(f._tmp.path(), &["init", "--quiet", "--bare", "-b", "main", empty.to_str().unwrap()]);
        git(&c, &["remote", "set-url", REMOTE, empty.to_str().unwrap()]);
        assert_eq!(r.pull().unwrap(), PullOutcome::UpToDate);
        r.push().unwrap();
        assert_eq!(origin_branches(&empty), "refs/heads/main");
        assert_eq!(r.pull().unwrap(), PullOutcome::UpToDate);
    }
}
