//! Baseline and draft: the repo's committed `HEAD` (pushed to [`REMOTE`]) is
//! the baseline; uncommitted changes in the working tree, untracked files
//! included, are this machine's draft. A draft is split into items (a skill,
//! a rule, the manifest) so that commit and restore work per item.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use walkdir::WalkDir;

use crate::git::{Git, RepoGit, nul_list, path_bytes, path_from_bytes, split_paths};
use crate::instructions::INSTRUCTIONS_DIR;
use crate::lock::LOCK_FILE;
use crate::manifest::MANIFEST_FILE;
use crate::skill::{IMPORTED_DIR, PERSONAL_DIR};
use crate::{Error, IoExt, Repo, Result, machine};

mod keep;
mod pull;

pub use keep::{Keep, KeptPull, Unkept, report_text};

/// The only remote: the server (see ADR 0002).
pub const REMOTE: &str = "origin";

/// The one branch the fleet's baseline lives on, locally and on the server.
/// Pull and push always use it, never "whatever branch is checked out": a
/// clone on another branch would otherwise find nothing to pull and push a
/// separate branch the fleet never sees.
pub const FLEET_BRANCH: &str = "main";

/// One unit of repo content, as committed or restored. String form: `skill:pdf`,
/// `rule:git`, `manifest`, `other:<path>`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ItemRef {
    /// `personal/<name>/` and/or `imported/<name>/`.
    Skill(String),
    /// `instructions/<name>.md`.
    Rule(String),
    /// `sk1ll.toml` and `sk1ll.lock` together.
    Manifest,
    /// Any other path; only committed or restored when named explicitly.
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
                Some(name) if !name.is_empty() && !name.starts_with('.') => Self::Rule(name.to_string()),
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

    /// Short name for messages: the skill or rule name, `manifest`, or the path.
    pub fn name(&self) -> &str {
        match self {
            Self::Skill(n) | Self::Rule(n) | Self::Other(n) => n,
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
            Self::Rule(n) => write!(f, "rule:{n}"),
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
            Some(("rule", n)) if !n.is_empty() => Self::Rule(n.into()),
            Some(("other", p)) if !p.is_empty() => Self::Other(p.into()),
            _ => {
                return Err(Error::Invalid(format!(
                    "invalid item `{s}` (expected skill:<name>, rule:<name>, manifest or other:<path>)"
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    Updated {
        from: String,
        to: String,
    },
    /// The new baseline clashes with the draft (or with local files the
    /// draft leaves out, such as ignored ones). Nothing else was changed;
    /// `baseline` is the remote commit. Unpushed commits that conflict with
    /// it were turned back into Draft changes first (`uncommitted`: each
    /// one's short id and subject), keeping every byte of the working tree
    /// and the index, so they block the pull as Draft does.
    Blocked {
        baseline: String,
        items: Vec<ItemRef>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        uncommitted: Vec<String>,
    },
}

/// `"<id> <sk1ll@<id>>"`: the commit identity for a machine without a git identity.
pub fn machine_author(machine_id: &str) -> String {
    let slug: String =
        machine_id.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '.') { c } else { '-' }).collect();
    format!("{machine_id} <sk1ll@{slug}>")
}

/// E.g. `commit pdf, git-flow from laptop`.
pub fn default_commit_message(items: &[ItemRef], machine_id: &str) -> String {
    let names: Vec<&str> = items.iter().map(ItemRef::name).collect();
    format!("commit {} from {machine_id}", names.join(", "))
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
    /// Old spellings Git can still open on a case-insensitive filesystem.
    case_removed: BTreeSet<PathBuf>,
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
        let (untracked, tracked): (Vec<&PathBuf>, Vec<&PathBuf>) = selected[0]
            .paths
            .iter()
            .filter(|p| {
                !matches!(item, ItemRef::Skill(_)) || !p.components().any(|c| c.as_os_str() == crate::skill::ID_FILE)
            })
            .partition(|p| st.changed.get(*p).copied().unwrap_or(false));
        let mut out = String::new();
        if !tracked.is_empty() {
            let base = match &st.head {
                Some(h) => h.clone(),
                None => self.empty_tree()?,
            };
            let diff = self.git()?.literal().args(["diff", "--no-color", "--no-ext-diff", &base, "--"]);
            let diff = diff.args(&tracked).run()?;
            out.push_str(&String::from_utf8_lossy(&diff));
        }
        for path in untracked {
            let o =
                self.git()?.args(["diff", "--no-color", "--no-ext-diff", "--no-index", "--", "/dev/null"]).arg(path);
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
    pub fn commit(&self, items: &[ItemRef], message: &str, author: Option<&str>) -> Result<String> {
        if message.trim().is_empty() {
            return Err(Error::Invalid("commit needs a message".into()));
        }
        // A commit is only useful if it can be pushed, and that needs the fleet branch.
        self.on_fleet_branch()?;
        // It would move the branch a pull that stopped half-way still has to move.
        self.require_no_pending_pull()?;
        let (_, st) = self.select(items, "committed")?;
        self.check_id_transitions(st.head.as_deref(), items, "commit")?;
        self.assign_item_ids(|path| {
            items.contains(&claim_item(path)) || items.contains(&crate::item_id::item_for_path(path))
        })?;
        let (selected, st) = self.select(items, "committed")?;
        let (case_removed, paths): (Vec<&PathBuf>, Vec<&PathBuf>) =
            selected.iter().flat_map(|d| &d.paths).partition(|path| st.case_removed.contains(*path));
        let repo_git = self.repo_git()?;
        let update_index = |index: Option<&Path>| -> Result<()> {
            let git = || {
                let git = repo_git.git().args(["-c", "core.ignoreCase=false"]);
                match index {
                    Some(index) => git.env("GIT_INDEX_FILE", index),
                    None => git,
                }
            };
            if !case_removed.is_empty() {
                git().args(["update-index", "--force-remove", "-z", "--stdin"]).stdin(nul_list(&case_removed)).run()?;
            }
            git().args(UPDATE_INDEX).stdin(nul_list(&paths)).run()?;
            Ok(())
        };

        // Build the commit in a throwaway index so the real one is untouched.
        let index = PathBuf::from(
            self.git()?.args(["rev-parse", "--path-format=absolute", "--git-path", "sk1ll-commit.index"]).text()?,
        );
        let _ = fs::remove_file(&index);
        let tree = (|| {
            let g = || repo_git.git().env("GIT_INDEX_FILE", &index);
            match &st.head {
                Some(h) => g().args(["read-tree", h]).run()?,
                None => g().args(["read-tree", "--empty"]).run()?,
            };
            update_index(Some(&index))?;
            g().arg("write-tree").text()
        })();
        let _ = fs::remove_file(&index);
        let tree = tree?;
        let claims = crate::item_id::claims_at(self.root(), &tree, |_| true, true)?;
        crate::item_id::check_claims(&claims).map_err(|e| repair_before(e, "commit"))?;
        let working_manifest = crate::item_id::manifest_on_disk(self.root())?;
        for (path, id) in &claims {
            let item = crate::item_id::item_for_path(path);
            if id.is_none() && (items.contains(&item) || items.contains(&claim_item(path))) {
                if path.starts_with(IMPORTED_DIR) {
                    if working_manifest.import.contains_key(item.name()) {
                        return Err(Error::Invalid(format!(
                            "`{item}` needs its Item ID in the commit. Select `manifest` too"
                        )));
                    }
                } else {
                    return Err(Error::Invalid(format!(
                        "can't commit {} without its Item ID",
                        crate::item_id::claim_path(path).display()
                    )));
                }
            }
        }
        if let Some(h) = &st.head
            && self.git()?.args(["rev-parse", &format!("{h}^{{tree}}")]).text()? == tree
        {
            return Err(Error::Invalid("nothing to commit: those items already match the baseline".into()));
        }
        // What the server would refuse for good is never committed.
        // An Import's Item ID is the manifest's: it counts only with the manifest.
        let committed = |item: &ItemRef, claim: Option<&str>| {
            items.contains(item)
                && (!claim.is_some_and(|claim| claim.starts_with(MANIFEST_FILE)) || items.contains(&ItemRef::Manifest))
        };
        if let Some(reason) = crate::baseline_check::check_tree(self.root(), &tree, committed)? {
            return Err(Error::Invalid(format!(
                "can't commit {}: the server refuses a baseline like that: {reason}",
                items.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
            )));
        }

        let mut commit = self.git()?.args(["commit-tree", &tree]);
        if let Some(h) = &st.head {
            commit = commit.args(["-p", h]);
        }
        let sha = commit.args(["-m", message]).envs(&self.identity_env(author)?).text()?;
        let mut update = self.git()?.args(["update-ref", "-m", "sk1ll: commit", "HEAD", &sha]);
        if let Some(h) = &st.head {
            update = update.arg(h);
        }
        update.run()?;
        // The committed paths now match HEAD; bring the real index in line for them only.
        update_index(None)?;
        Ok(sha)
    }

    /// Pushes [`FLEET_BRANCH`] to [`REMOTE`]; it must be checked out.
    pub fn push(&self) -> Result<()> {
        match self.prepare_push()? {
            Some(commit) => self.push_commit(&commit),
            None => Ok(()),
        }
    }

    /// The checks [`push`](Self::push) makes before it talks to [`REMOTE`]:
    /// returns the commit to push (`HEAD`), or `None` on an unborn branch.
    /// Only reads, so a caller can check under its engine lock and push the
    /// commit without it ([`push_commit`](Self::push_commit)).
    pub fn prepare_push(&self) -> Result<Option<String>> {
        self.on_fleet_branch()?;
        // Its branch has yet to move to the replayed commits.
        self.require_no_pending_pull()?;
        let Some(head) = rev(self.git()?, "HEAD")? else { return Ok(None) };
        let claims = crate::item_id::claims_at(self.root(), &head, |_| true, true)?;
        crate::item_id::check_claims(&claims).map_err(|e| repair_before(e, "push"))?;
        Ok(Some(head))
    }

    /// Pushes `commit` (from [`prepare_push`](Self::prepare_push)) as
    /// [`REMOTE`]'s [`FLEET_BRANCH`], and records it as that remote branch.
    /// It pushes that exact commit, whatever the local branch has become
    /// since: a pull that rebased it meanwhile has moved off what the server
    /// had, so git refuses the push as not fast-forward.
    pub fn push_commit(&self, commit: &str) -> Result<()> {
        let refspec = format!("{commit}:refs/heads/{FLEET_BRANCH}");
        self.git()?.args(["push", "--quiet", REMOTE, &refspec]).run()?;
        let tracking = format!("refs/remotes/{REMOTE}/{FLEET_BRANCH}");
        self.git()?.args(["update-ref", "-m", "sk1ll: push", &tracking, commit]).run()?;
        Ok(())
    }

    /// Returns `items` to the baseline: tracked paths are checked out from
    /// `HEAD`, paths the baseline doesn't have are deleted. The manifest can't
    /// be restored while the baseline has none, since that would delete it, and
    /// nothing is restored that would delete or replace an ignored file.
    pub fn restore(&self, items: &[ItemRef]) -> Result<()> {
        self.restore_keeping(items, &BTreeSet::new()).map(drop)
    }

    /// [`restore`](Self::restore), first moving what the working tree has
    /// for the items in `keep` into the recovery area, file by file, as
    /// pull's recovery entries ([`crate::sync::displaced::Kind::Draft`]):
    /// they stay until adopted (`sk1ll recover adopt`, which puts a file
    /// back as a Draft change over a path that has none) or discarded.
    /// That is how the version of an item a pull is Blocked on survives the
    /// restore that clears the way. Returns the entries' ids.
    pub fn restore_keeping(&self, items: &[ItemRef], keep: &BTreeSet<ItemRef>) -> Result<Vec<String>> {
        self.restore_recording(items, keep, None, &mut |_| Ok(()))
    }

    /// [`restore_keeping`](Self::restore_keeping), telling `record` about
    /// each kept file before it moves into recovery, and about each file the
    /// Draft deletes, before anything changes: what "keep mine" journals.
    ///
    /// A kept item is restored without overwriting or deleting anything in
    /// the working tree: once its files are in recovery, the Baseline's go
    /// in only where nothing is, and only the index changes otherwise. So a
    /// save made at one of its paths meanwhile stays where it is (the error
    /// says so). A kept path whose staged version differs from its file is
    /// refused before anything changes: only one of the two could be kept.
    ///
    /// `user_staged`, for keep mine: the paths the user had staged before
    /// it began. A kept path among them is refused; a kept path staged
    /// since (by the pull's un-commit) is the operation's own, and `record`
    /// hears what is staged there before the restore resets it.
    fn restore_recording(
        &self,
        items: &[ItemRef],
        keep: &BTreeSet<ItemRef>,
        user_staged: Option<&BTreeSet<PathBuf>>,
        record: &mut dyn FnMut(keep::Event) -> Result<()>,
    ) -> Result<Vec<String>> {
        let (selected, st) = self.select(items, "restored")?;
        if selected.iter().any(|d| d.item == ItemRef::Manifest) && !st.head_paths.contains(Path::new(MANIFEST_FILE)) {
            return Err(Error::Invalid(format!(
                "there is no baseline to restore the manifest to; commit it first (it would delete {MANIFEST_FILE})"
            )));
        }
        self.check_id_transitions(st.head.as_deref(), items, "restore")?;
        let selected_items: BTreeSet<_> = selected.iter().map(|d| &d.item).collect();
        let restores_manifest = selected_items.contains(&ItemRef::Manifest);
        let restores = |path: &Path| selected_items.contains(&crate::item_id::item_for_path(path));
        let manifest =
            if restores_manifest { crate::Manifest::default() } else { crate::item_id::manifest_on_disk(self.root())? };
        let mut claims = crate::item_id::claims_on_disk(self.root(), &manifest, |path| !restores(path))?;
        if let Some(head) = &st.head {
            for (path, id) in crate::item_id::claims_at(self.root(), head, restores, restores_manifest)? {
                if id.is_some() {
                    claims.insert(path, id);
                } else {
                    claims.entry(path).or_default();
                }
            }
        }
        crate::item_id::check_claims(&claims).map_err(|e| duplicate_after(e, "restore"))?;
        // Ignored files aren't in the draft, but git would delete or overwrite
        // one that sits at, under or above a path being restored.
        let ignored = self.ignored_paths()?;
        for d in &selected {
            let clash = d.paths.iter().find_map(|p| {
                ignored.iter().find(|i| (i.starts_with(p) || p.starts_with(i)) && !(i == &p && is_sidecar(p)))
            });
            if let Some(i) = clash {
                return Err(Error::Invalid(format!(
                    "`{}` can't be restored: that would delete or replace the ignored `{}`; move it out of the way first",
                    d.item,
                    i.display()
                )));
            }
        }
        let kept_paths: Vec<(&ItemRef, &PathBuf)> = selected
            .iter()
            .filter(|d| keep.contains(&d.item))
            .flat_map(|d| d.paths.iter().map(move |p| (&d.item, p)))
            .collect();
        if let Some(user_staged) = user_staged {
            self.refuse_user_staged(&kept_paths, user_staged)?;
        }
        self.refuse_staged_differing(&kept_paths, st.head.as_deref())?;
        self.refuse_obstructed(&kept_paths, &st)?;
        // Kept paths staged now are the pull's un-commit's (the user's are
        // refused above): journaled as the operation's own, path by path.
        let owned = match user_staged {
            Some(_) => self.staged_paths()?,
            None => BTreeSet::new(),
        };
        let kept = self.keep_and_restore(&kept_paths, &st, &owned, record)?;
        let (in_head, new): (Vec<&PathBuf>, Vec<&PathBuf>) = selected
            .iter()
            .filter(|d| !keep.contains(&d.item))
            .flat_map(|d| &d.paths)
            .partition(|p| st.head_paths.contains(*p));
        // New paths go first: one may be in the way of a baseline file (a file
        // that became a directory) that the checkout below brings back.
        if !new.is_empty() {
            self.git()?.args(["update-index", "--force-remove", "-z", "--stdin"]).stdin(nul_list(&new)).run()?;
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
            self.git()?.literal().args(["checkout", "--quiet", head, "--"]).args(&in_head).run()?;
        }
        self.restore_spellings(&selected, &st)?;
        Ok(kept)
    }

    /// The items committed here but not on [`REMOTE`]'s [`FLEET_BRANCH`] as
    /// fetched last: what a pull can turn back into Draft.
    pub(crate) fn unpushed_items(&self) -> Result<BTreeSet<ItemRef>> {
        let remote = format!("refs/remotes/{REMOTE}/{FLEET_BRANCH}");
        let (Some(head), Some(remote)) = (rev(self.git()?, "HEAD")?, rev(self.git()?, &remote)?) else {
            return Ok(BTreeSet::new());
        };
        let base = self.git()?.args(["merge-base", &head, &remote]).output()?;
        if !base.status.success() {
            return Ok(BTreeSet::new());
        }
        let base = String::from_utf8_lossy(&base.stdout).trim().to_string();
        let out = self.git()?.args(["diff", "--name-only", "-z", "--relative", "--no-renames", &base, &head]).run()?;
        Ok(split_paths(&out).map(|p| ItemRef::for_path(&p)).collect())
    }

    /// The paths (repo-relative) whose index entry differs from `HEAD`'s:
    /// staged changes, additions and removals.
    pub(crate) fn staged_paths(&self) -> Result<BTreeSet<PathBuf>> {
        let mut git = self.git()?.args(["diff", "--cached", "--name-only", "-z", "--relative", "--no-renames"]);
        if let Some(head) = rev(self.git()?, "HEAD")? {
            git = git.arg(head);
        }
        Ok(split_paths(&git.run()?).collect())
    }

    /// [`staged_paths`](Self::staged_paths) the user staged: all but those
    /// whose index entry is still exactly what pull's un-commit left there.
    pub(crate) fn user_staged_paths(&self) -> Result<BTreeSet<PathBuf>> {
        let staged = self.staged_paths()?;
        if staged.is_empty() {
            return Ok(staged);
        }
        let uncommitted = pull::uncommitted_staging(self)?;
        let root = self.root().canonicalize().at(self.root())?;
        let mut user = BTreeSet::new();
        for rel in staged {
            let current = self.index_entry(&rel)?.map(|keep::IndexEntry(mode, oid)| (mode, oid));
            if uncommitted.get(&root.join(&rel)) != Some(&current) {
                user.insert(rel);
            }
        }
        Ok(user)
    }

    /// Refuses to keep a path the user had staged: keep mine works on the
    /// files, so it would leave that staging out of what it keeps.
    fn refuse_user_staged(&self, paths: &[(&ItemRef, &PathBuf)], staged: &BTreeSet<PathBuf>) -> Result<()> {
        if let Some((item, path)) = paths.iter().find(|(_, p)| staged.contains(*p)) {
            let root = shell_quote(&self.root().to_string_lossy());
            let path = shell_quote(&path.to_string_lossy());
            return Err(Error::Invalid(format!(
                "`{item}` has a staged change at {path}, which keep mine would leave out. Unstage it with `git -C \
                 {root} restore --staged -- {path}`, or commit it, then try again"
            )));
        }
        Ok(())
    }

    /// Refuses to keep a path whose staged version differs from the file:
    /// recovery would keep only the file, and the restore drops the staged one.
    fn refuse_staged_differing(&self, paths: &[(&ItemRef, &PathBuf)], head: Option<&str>) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let list = |cached: bool| -> Result<BTreeSet<PathBuf>> {
            let mut git = self.git()?.literal().args(["diff", "--name-only", "-z", "--relative", "--no-renames"]);
            if cached {
                git = git.arg("--cached").args(head);
            }
            Ok(split_paths(&git.arg("--").args(paths.iter().map(|(_, p)| *p)).run()?).collect())
        };
        let (staged, unstaged) = (list(true)?, list(false)?);
        // A staged removal leaves no index entry, which `git diff` doesn't
        // compare with a file made there since.
        let out = self
            .git()?
            .literal()
            .args(["ls-files", "-z", "--cached", "--"])
            .args(paths.iter().map(|(_, p)| *p))
            .run()?;
        let indexed: BTreeSet<PathBuf> = split_paths(&out).collect();
        let differs = |p: &PathBuf| {
            unstaged.contains(p) || (!indexed.contains(p) && fs::symlink_metadata(self.root().join(p)).is_ok())
        };
        if let Some((item, path)) = paths.iter().find(|(_, p)| staged.contains(*p) && differs(p)) {
            let root = shell_quote(&self.root().to_string_lossy());
            let path = shell_quote(&path.to_string_lossy());
            return Err(Error::Invalid(format!(
                "`{item}` has a staged version of {path} that differs from the file, and only one of them could be \
                 kept. Stage the file with `git -C {root} add -- {path}`, or drop the staged version with `git -C \
                 {root} restore --staged -- {path}`, then try again"
            )));
        }
        Ok(())
    }

    /// Refuses to keep paths the Baseline's files couldn't go back to: a
    /// parent that is a file (or a link) that isn't itself kept, and moved
    /// aside first. Checked before anything changes.
    fn refuse_obstructed(&self, paths: &[(&ItemRef, &PathBuf)], st: &Status) -> Result<()> {
        let kept: BTreeSet<&PathBuf> = paths.iter().map(|(_, p)| *p).collect();
        for (item, rel) in paths.iter().filter(|(_, p)| st.head_paths.contains(*p)) {
            for parent in rel.ancestors().skip(1).filter(|a| !a.as_os_str().is_empty()) {
                let in_the_way = fs::symlink_metadata(self.root().join(parent)).is_ok_and(|m| !m.is_dir());
                if in_the_way && !kept.contains(&parent.to_path_buf()) {
                    return Err(Error::Invalid(format!(
                        "`{item}` can't be restored: {} is in the way of {}; move it aside, then try again",
                        parent.display(),
                        rel.display()
                    )));
                }
            }
        }
        Ok(())
    }

    /// After restoring both sides of a case-only rename (`pdf` to `PDF`) on
    /// a case-insensitive filesystem, the checkout writes the Baseline's
    /// files into the directory as it is spelled now: one that still holds
    /// an ignored file was never removed. Gives it back the Baseline's
    /// spelling, everything in it moving along.
    fn restore_spellings(&self, selected: &[DraftItem], st: &Status) -> Result<()> {
        let mut wanted = BTreeSet::new();
        for path in selected.iter().flat_map(|d| &d.paths).filter(|p| st.head_paths.contains(*p)) {
            match ItemRef::for_path(path) {
                ItemRef::Skill(name) if path.starts_with(PERSONAL_DIR) => {
                    wanted.insert(Path::new(PERSONAL_DIR).join(name));
                }
                ItemRef::Rule(_) => {
                    wanted.insert(path.clone());
                }
                _ => {}
            }
        }
        for rel in wanted {
            let path = self.root().join(&rel);
            if let Some(stored) = crate::repo::stored_spelling(&path)? {
                crate::repo::respell(&stored, &path)?;
            }
        }
        Ok(())
    }

    /// Keeps each of `paths` (repo-relative, with their item) in recovery
    /// and restores it, one path at a time, telling `record` first what it
    /// will do there (and, for tests, each step): the file or symlink moves
    /// into a recovery entry of its own in the git directory's recovery
    /// area, as pull does for what it displaces; its index entry follows
    /// `HEAD`; the Baseline's file goes in where nothing is
    /// ([`pull::place_baseline`]). Nothing in the working tree is ever
    /// removed or replaced. A path with nothing there that the Baseline has
    /// (a deletion) is only recorded and restored. New paths go first: one
    /// may be in the way of a Baseline file (a file that became a
    /// directory). So a stop leaves at most one path between its move and
    /// its Baseline file; an interrupted run puts nothing back (see
    /// [`keep`]), and the journal records each step before it is taken: a
    /// path's recovery entry before its file moves, its staging as the
    /// operation's own only after that, and before its index changes. A
    /// path something new is at stays as it is, and is an error. Returns
    /// the recovery entries' ids.
    #[cfg(unix)]
    fn keep_and_restore(
        &self,
        paths: &[(&ItemRef, &PathBuf)],
        st: &Status,
        owned: &BTreeSet<PathBuf>,
        record: &mut dyn FnMut(keep::Event) -> Result<()>,
    ) -> Result<Vec<String>> {
        // Journals a path's staging as the operation's own, once its recovery
        // entry is journaled and its file moved, and before its index changes.
        let own = |item: &ItemRef, rel: &Path, record: &mut dyn FnMut(keep::Event) -> Result<()>| -> Result<()> {
            match owned.contains(rel) {
                true => record(keep::Event::Staged(item.clone(), self.root().join(rel), self.index_entry(rel)?)),
                false => Ok(()),
            }
        };
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let git_dir = PathBuf::from(self.git()?.args(["rev-parse", "--absolute-git-dir"]).text()?);
        let root = self.root().canonicalize().at(self.root())?;
        let (in_head, new): (Vec<_>, Vec<_>) = paths.iter().copied().partition(|(_, p)| st.head_paths.contains(*p));
        let (mut kept, mut taken) = (Vec::new(), Vec::new());
        for (item, rel) in &new {
            kept.extend(self.keep_one(item, rel, &git_dir, record)?);
            own(item, rel, record)?;
        }
        if !new.is_empty() {
            let rels: Vec<&PathBuf> = new.iter().map(|(_, p)| *p).collect();
            self.git()?.args(["update-index", "--force-remove", "-z", "--stdin"]).stdin(nul_list(&rels)).run()?;
            for rel in rels {
                prune_empty_parents(self.root(), rel);
                if fs::symlink_metadata(root.join(rel)).is_ok() {
                    taken.push(root.join(rel));
                }
            }
        }
        if let Some(head) = &st.head {
            for (item, rel) in &in_head {
                kept.extend(self.keep_one(item, rel, &git_dir, record)?);
                own(item, rel, record)?;
                self.git()?.literal().args(["reset", "--quiet", head, "--"]).arg(rel).run()?;
                let dest = root.join(rel);
                let staged = &mut || record(keep::Event::Placing);
                match pull::place_baseline(self, std::slice::from_ref(&dest), staged)?.is_empty() {
                    true => record(keep::Event::Published(self.root().join(rel)))?,
                    false => taken.push(dest),
                }
            }
        }
        if !taken.is_empty() {
            let names: Vec<String> = taken.iter().map(|p| p.display().to_string()).collect();
            return Err(Error::Changed(format!(
                "{} {} written to while it was restored; that new version stays where it is, and the one before it \
                 is kept in recovery (see `sk1ll recover list`)",
                names.join(", "),
                if taken.len() == 1 { "was" } else { "were" }
            )));
        }
        Ok(kept)
    }

    /// Journals `rel`, then moves what is there into a recovery entry: its
    /// id, or `None` for a deletion. A directory is left alone.
    #[cfg(unix)]
    fn keep_one(
        &self,
        item: &ItemRef,
        rel: &Path,
        git_dir: &Path,
        record: &mut dyn FnMut(keep::Event) -> Result<()>,
    ) -> Result<Option<String>> {
        use crate::sync::displaced::{Displacing, Kind, PULL};
        let path = self.root().join(rel);
        let kept = |entry| keep::Kept { item: item.clone(), dest: path.clone(), entry };
        match fs::symlink_metadata(&path) {
            Ok(meta) if !meta.is_dir() => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                record(keep::Event::Kept(kept(None)))?;
                return Ok(None);
            }
            _ => return Ok(None),
        }
        let mut held = Displacing::begin(Some(self), Kind::Draft, PULL, &path, None, None, git_dir)?;
        let id = held.id().to_string();
        if let Err(e) = record(keep::Event::Kept(kept(Some(id.clone())))).and_then(|()| held.take(&path)) {
            held.abandon();
            return Err(e);
        }
        held.finish(false)?;
        record(keep::Event::Moved(path))?;
        Ok(Some(id))
    }

    #[cfg(not(unix))]
    fn keep_and_restore(
        &self,
        paths: &[(&ItemRef, &PathBuf)],
        _: &Status,
        _: &BTreeSet<PathBuf>,
        _: &mut dyn FnMut(keep::Event) -> Result<()>,
    ) -> Result<Vec<String>> {
        match paths.len() {
            0 => Ok(Vec::new()),
            _ => Err(Error::Invalid("keeping a restored version in recovery needs Unix".into())),
        }
    }

    /// The index entry at `rel` (repo-relative): mode and blob, or `None`.
    fn index_entry(&self, rel: &Path) -> Result<Option<keep::IndexEntry>> {
        let out = self.git()?.literal().args(["ls-files", "-s", "-z", "--"]).arg(rel).run()?;
        Ok(entry_fields(&out, 0, 1))
    }

    /// `HEAD`'s entry at `rel` (repo-relative): mode and blob, or `None`.
    fn head_entry(&self, rel: &Path) -> Result<Option<keep::IndexEntry>> {
        let Some(head) = rev(self.git()?, "HEAD")? else { return Ok(None) };
        let out = self.git()?.literal().args(["ls-tree", "-z", &head, "--"]).arg(rel).run()?;
        Ok(entry_fields(&out, 0, 2))
    }

    fn check_id_transitions(&self, head: Option<&str>, selected: &[ItemRef], operation: &str) -> Result<()> {
        use crate::item_id::{claim_path, claims_at, claims_at_only, claims_on_disk, manifest_on_disk};
        let Some(head) = head else { return Ok(()) };
        // Restore still has to work when the selected metadata itself is broken.
        let restoring = operation == "restore";
        let committed = match claims_at(self.root(), head, |_| true, true) {
            Ok(claims) => claims,
            // A commit may replace what HEAD can't read: the rest of HEAD
            // constrains the moves, and the tree it makes is checked strictly.
            Err(Error::Parse { .. } | Error::Invalid(_)) if !restoring => {
                let replaced = |path: &Path| selected.contains(&claim_item(path));
                let manifest = !selected.contains(&ItemRef::Manifest);
                claims_at_only(self.root(), head, |path| !replaced(path), manifest).map_err(|e| {
                    Error::Invalid(format!(
                        "can't {operation}: the baseline has Item ID metadata that can't be read ({e}). Fix it and \
                         {operation} it together with these items"
                    ))
                })?
            }
            Err(e) => return Err(e),
        };
        let manifest = match manifest_on_disk(self.root()) {
            Err(Error::Parse { .. }) if restoring && selected.contains(&ItemRef::Manifest) => {
                crate::Manifest::default()
            }
            result => result?,
        };
        let working = if restoring {
            match claims_on_disk(self.root(), &manifest, |_| true) {
                Err(Error::Parse { .. } | Error::Invalid(_)) => {
                    claims_on_disk(self.root(), &manifest, |path| !selected.contains(&claim_item(path)))?
                }
                result => result?,
            }
        } else {
            claims_on_disk(self.root(), &manifest, |_| true)?
        };
        let locations = |claims: crate::item_id::Claims| {
            let mut by_id = BTreeMap::<_, Vec<_>>::new();
            for (path, id) in claims {
                if let Some(id) = id {
                    by_id.entry(id).or_default().push(path);
                }
            }
            by_id
        };
        let committed = locations(committed);
        let working = locations(working);
        for (id, old) in &committed {
            let [old] = old.as_slice() else { continue };
            let Some([new]) = working.get(id).map(Vec::as_slice) else { continue };
            if old == new {
                continue;
            }
            let mut sides = BTreeSet::from([claim_item(old), claim_item(new)]);
            if old.starts_with(IMPORTED_DIR) || new.starts_with(IMPORTED_DIR) {
                let import = if old.starts_with(IMPORTED_DIR) { old } else { new };
                let skill = crate::item_id::item_for_path(import);
                // A Fork changes both the manifest and the vendored/personal paths.
                sides.insert(skill);
            }
            if sides.iter().any(|item| selected.contains(item)) && !sides.iter().all(|item| selected.contains(item)) {
                let items = sides.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
                return Err(Error::Invalid(format!(
                    "can't {operation} one side of Item ID `{id}` moving from {} to {}. Select both sides: {items}",
                    claim_path(old).display(),
                    claim_path(new).display()
                )));
            }
        }
        Ok(())
    }

    /// Restores the whole draft, including `other:` items, except a manifest
    /// the baseline doesn't have yet (see [`restore`](Self::restore)).
    pub fn restore_all(&self) -> Result<()> {
        self.restore_all_keeping(&BTreeSet::new()).map(drop)
    }

    /// [`restore_all`](Self::restore_all), keeping the items in `keep` in
    /// recovery as [`restore_keeping`](Self::restore_keeping) does.
    pub fn restore_all_keeping(&self, keep: &BTreeSet<ItemRef>) -> Result<Vec<String>> {
        let (draft, st) = self.draft_with_status()?;
        let keep_manifest = !st.head_paths.contains(Path::new(MANIFEST_FILE));
        let items: Vec<ItemRef> =
            draft.into_iter().map(|d| d.item).filter(|i| !(keep_manifest && *i == ItemRef::Manifest)).collect();
        if items.is_empty() { Ok(Vec::new()) } else { self.restore_keeping(&items, keep) }
    }

    /// Fetches [`REMOTE`] and moves to its baseline, carrying the draft on
    /// top. Committed but unpushed commits are rebased onto it. If the new
    /// baseline touches an item the draft also changes (or the rebase
    /// conflicts, or local files the draft leaves out are in the way), nothing
    /// changes and the outcome is [`PullOutcome::Blocked`]. A baseline with no
    /// commit in common with `HEAD` (unrelated histories), unpushed merge
    /// commits, submodules and sparse checkouts are an [`Error::Invalid`];
    /// that also changes nothing. So is a pull that would change files on a
    /// platform other than Unix.
    ///
    /// Once it starts changing files, pull only goes forward: if anything
    /// stops it half-way (an error, a crash, or a draft change that appears in
    /// a path it is about to update, which is [`PullOutcome::Blocked`]), the
    /// next pull or [`finish_pull`](Self::finish_pull) finishes it first.
    /// `HEAD` stays on the branch throughout. A file written to while pull
    /// replaces it keeps that version in the recovery area (see
    /// [`crate::sync::displaced`]); the pull still finishes, then returns
    /// the [`Error::Displaced`] that says so.
    ///
    /// The baseline is [`REMOTE`]'s [`FLEET_BRANCH`], which must also be
    /// checked out here. A remote without that branch is an error too, unless
    /// it has no branches at all (an empty server, which only older servers start
    /// as): then there is nothing to pull yet, and the first push creates it.
    ///
    /// A Baseline that writes a file through a git filter driver (Git LFS)
    /// is an [`Error::FilterDriver`], before anything changes: the skills
    /// repo supports none. No pull, nor finishing one, runs a filter driver.
    pub fn pull(&self, lock: &crate::EngineLock) -> Result<PullOutcome> {
        lock.check_for(self.root())?;
        let pulled = pull::run(self, true);
        self.check_pulled_ids(pulled)
    }

    /// Fetches [`REMOTE`]'s branches, the network half of a [`pull`](Self::pull).
    /// It changes no file, no `HEAD` and no index, so it needs no engine lock:
    /// a caller fetches first and then, holding its lock, moves onto what it
    /// fetched with [`pull_fetched`](Self::pull_fetched).
    pub fn fetch(&self) -> Result<()> {
        pull::fetch(self)
    }

    /// [`pull`](Self::pull) onto the baseline [`fetch`](Self::fetch) fetched
    /// last, without talking to [`REMOTE`]: the local half of a pull.
    pub fn pull_fetched(&self, lock: &crate::EngineLock) -> Result<PullOutcome> {
        lock.check_for(self.root())?;
        let pulled = pull::run(self, false);
        self.check_pulled_ids(pulled)
    }

    /// Checks the Item IDs of a pull that went through: the replayed
    /// commits, before they are pushed, and the Draft on top of them. Pull
    /// never refuses a Baseline over duplicate IDs, since only a Draft change
    /// on top of it can repair them; it says so once it is done instead.
    fn check_pulled_ids(&self, pulled: Result<PullOutcome>) -> Result<PullOutcome> {
        match &pulled {
            Ok(PullOutcome::Updated { .. }) => {}
            Err(Error::Displaced { .. }) if !self.pull_pending()? => {}
            _ => return pulled,
        }
        let to = rev(self.git()?, "HEAD")?.unwrap_or_default();
        let claims = crate::item_id::claims_at(self.root(), &to, |_| true, true)?;
        crate::item_id::check_claims(&claims).map_err(|e| repair_before(e, "push the replayed commits"))?;
        // The pull went through: Draft metadata it can't read is reported by
        // the inventory, not as a failed pull. Duplicates are refused.
        let draft = crate::item_id::manifest_on_disk(self.root())
            .and_then(|manifest| crate::item_id::claims_on_disk(self.root(), &manifest, |_| true));
        if let Ok(draft) = draft {
            crate::item_id::check_claims(&draft).map_err(|e| repair_before(e, "sync the replayed Draft"))?;
        }
        pulled
    }

    /// Whether a [`pull`](Self::pull) stopped half-way and has yet to finish.
    pub fn pull_pending(&self) -> Result<bool> {
        pull::pending(self)
    }

    /// The remote Baseline a pull that stopped half-way is moving onto, as
    /// its journal records it; `None` if no pull is pending.
    pub fn pending_pull_baseline(&self) -> Result<Option<String>> {
        pull::pending_baseline(self)
    }

    /// The Item ID claims of the Baseline (`HEAD`). An unborn or unreadable
    /// Baseline has none.
    pub(crate) fn baseline_claims(&self) -> Result<crate::item_id::Claims> {
        Ok(match rev(self.git()?, "HEAD")? {
            Some(head) => crate::item_id::claims_at(self.root(), &head, |_| true, true).unwrap_or_default(),
            None => crate::item_id::Claims::new(),
        })
    }

    /// [`Error::PullPending`] if a [`pull`](Self::pull) stopped half-way.
    /// Whatever writes Item IDs, syncs harnesses or pushes checks this: a
    /// pending pull's tree is partial, and writing into it could block the
    /// paths the pull still has to update.
    pub fn require_no_pending_pull(&self) -> Result<()> {
        if self.pull_pending()? { Err(Error::PullPending) } else { Ok(()) }
    }

    /// Finishes a [`pull`](Self::pull) that stopped half-way, without
    /// fetching: `None` if there is none. Blocked if a draft change is still
    /// in the way. Like [`pull`](Self::pull), a pull it finishes has its
    /// replayed commits and Draft checked for duplicate Item IDs.
    pub fn finish_pull(&self, lock: &crate::EngineLock) -> Result<Option<PullOutcome>> {
        lock.check_for(self.root())?;
        match pull::finish(self) {
            Ok(None) => Ok(None),
            Ok(Some(outcome)) => self.check_pulled_ids(Ok(outcome)).map(Some),
            Err(e) => self.check_pulled_ids(Err(e)).map(Some),
        }
    }

    /// "Keep mine" for a Blocked pull, in one step (see [`keep`]): onto the
    /// baseline [`fetch`](Self::fetch) fetched last, with the Draft's
    /// version of the clashing items in `keep` (each whole file) put back
    /// on top as Draft changes. Unless `keep` names every item the pull is
    /// Blocked on, nothing changes and the outcome is that Blocked. A step
    /// that fails leaves every kept file in recovery, and the error names
    /// the command that finishes by hand.
    pub fn pull_keeping(&self, lock: &crate::EngineLock, keep: &Keep) -> Result<KeptPull> {
        lock.check_for(self.root())?;
        keep::run(self, lock, keep)
    }

    /// Reports a [`pull_keeping`](Self::pull_keeping) that was interrupted,
    /// once no pull is pending, and removes its journal: `None` if there is
    /// none, else each kept version, left in recovery for `sk1ll recover
    /// adopt`. Nothing is adopted (see [`keep`]).
    pub fn finish_keeping(&self, lock: &crate::EngineLock) -> Result<Option<Vec<Unkept>>> {
        lock.check_for(self.root())?;
        keep::finish(self)
    }

    /// The saved report of interrupted [`pull_keeping`](Self::pull_keeping)
    /// runs: each kept version still in recovery, until adopted or
    /// discarded. Only reads, apart from dropping what was acted on.
    pub fn keep_report(&self) -> Result<Vec<Unkept>> {
        keep::report(self)
    }

    /// [`keep_report`](Self::keep_report), writing nothing, for previews.
    pub fn peek_keep_report(&self) -> Result<Vec<Unkept>> {
        keep::peek_report(self)
    }

    /// Dismisses the notes of [`keep_report`](Self::keep_report) (the
    /// entries with no recovery id); returns how many there were.
    pub fn dismiss_keep_notes(&self) -> Result<usize> {
        keep::dismiss_notes(self)
    }

    /// Whether git has a configured `user.name` and `user.email`.
    pub fn has_git_identity(&self) -> Result<bool> {
        let git = self.repo_git()?;
        has_git_identity(|| git.git())
    }

    /// Ignored, untracked files under the repo root, relative to it.
    fn ignored_paths(&self) -> Result<Vec<PathBuf>> {
        let out =
            self.git()?.args(["ls-files", "-z", "--others", "--ignored", "--exclude-standard", "--", "."]).run()?;
        Ok(split_paths(&out).collect())
    }

    /// Every git command on the repo, built by [`RepoGit`]: with its filter
    /// drivers turned off, so none runs (see [`crate::git::RepoGit`]).
    fn repo_git(&self) -> Result<RepoGit> {
        RepoGit::with_env(self.root(), &[("GIT_OPTIONAL_LOCKS".into(), "0".into())])
    }

    fn git(&self) -> Result<Git> {
        Ok(self.repo_git()?.git())
    }

    /// [`FLEET_BRANCH`], if that is the branch checked out; else how to get there.
    fn on_fleet_branch(&self) -> Result<String> {
        on_fleet_branch(self.git()?, self.root())
    }

    fn empty_tree(&self) -> Result<String> {
        self.git()?.args(["hash-object", "-t", "tree", "--stdin"]).stdin(Vec::new()).text()
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
                let exists = !paths.iter().all(|path| st.case_removed.contains(path)) && self.in_tree(&item, &paths);
                let change = match (in_head.contains(&item), exists) {
                    (true, true) => Change::Modified,
                    (false, true) => Change::Added,
                    (_, false) => Change::Deleted,
                };
                DraftItem { item, change, paths }
            })
            .collect();
        Ok((items, st))
    }

    /// The draft entries for `items`, to be `verb` (committed, restored) as a
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
            ItemRef::Rule(name) => exists(&format!("{INSTRUCTIONS_DIR}/{name}.md")),
            ItemRef::Manifest => exists(MANIFEST_FILE) || exists(LOCK_FILE),
            // The name may be quoted (non-UTF-8); `paths` is exact.
            ItemRef::Other(_) => paths.iter().any(|p| fs::symlink_metadata(root.join(p)).is_ok()),
        }
    }

    fn status(&self) -> Result<Status> {
        for dir in [self.personal_dir(), self.root().join(INSTRUCTIONS_DIR)] {
            for (path, _) in crate::item_id::entries(&dir)? {
                crate::repo::check_rename_stage(&path)?;
            }
        }
        let head = rev(self.git()?, "HEAD")?;
        let head_paths = match &head {
            Some(h) => split_paths(&self.git()?.args(["ls-tree", "-r", "-z", "--name-only", h]).run()?).collect(),
            None => BTreeSet::new(),
        };
        // Porcelain paths are relative to the top level; the repo may be a subdirectory.
        let prefix = self.git()?.args(["rev-parse", "--show-prefix"]).run()?;
        let prefix = prefix.strip_suffix(b"\n").unwrap_or(&prefix);
        let mut changed: BTreeMap<PathBuf, bool> =
            status_paths(self.git()?.args(["-c", "core.ignoreCase=false", "-c", "core.fsmonitor=false"]), &["."])?
                .into_iter()
                .filter_map(|(path, untracked)| {
                    let rel = path_from_bytes(path.strip_prefix(prefix).unwrap_or(&path));
                    // An import being staged, whatever `.gitignore` says.
                    (!crate::source::is_staging_path(&rel)).then_some((rel, untracked))
                })
                .collect();
        // Identity is part of a personal skill even when a user's ignore rule
        // hides it. update-index takes these exact paths, bypassing ignores.
        let personal = self.personal_dir();
        if personal.symlink_metadata().is_ok_and(|m| m.is_dir()) {
            for (dir, ty) in crate::item_id::entries(&personal)? {
                if !ty.is_dir() || !dir.join("SKILL.md").is_file() {
                    continue;
                }
                let sidecar = dir.join(crate::skill::ID_FILE);
                let relative = sidecar.strip_prefix(self.root()).unwrap();
                if !head_paths.contains(relative)
                    && !crate::source::is_staging_path(relative)
                    && sidecar.symlink_metadata().is_ok()
                {
                    changed.entry(relative.into()).or_insert(true);
                }
            }
        }
        let case_removed = self.case_removals(&head_paths, &changed)?;
        changed.extend(case_removed.iter().map(|path| (path.clone(), false)));
        Ok(Status { head, head_paths, changed, case_removed })
    }

    fn case_removals(&self, head: &BTreeSet<PathBuf>, changed: &BTreeMap<PathBuf, bool>) -> Result<BTreeSet<PathBuf>> {
        let item_path = |path: &Path| match ItemRef::for_path(path) {
            ItemRef::Skill(name) if path.starts_with(PERSONAL_DIR) => Some(Path::new(PERSONAL_DIR).join(name)),
            ItemRef::Rule(_) => Some(path.to_path_buf()),
            _ => None,
        };
        let mut added = BTreeMap::<String, BTreeSet<PathBuf>>::new();
        for path in changed.keys() {
            if !head.contains(path)
                && let Some(item) = item_path(path)
            {
                added.entry(item.to_string_lossy().to_ascii_lowercase()).or_default().insert(item);
            }
        }
        let mut removed_items = BTreeSet::new();
        for old in head.iter().filter_map(|path| item_path(path)).collect::<BTreeSet<_>>() {
            if let Some(candidates) = added.get(&old.to_string_lossy().to_ascii_lowercase()) {
                for new in candidates {
                    if old != *new && crate::repo::case_alias(&self.root().join(new), &self.root().join(&old))? {
                        removed_items.insert(old.clone());
                        break;
                    }
                }
            }
        }
        Ok(head.iter().filter(|p| item_path(p).is_some_and(|item| removed_items.contains(&item))).cloned().collect())
    }
}

fn claim_item(path: &Path) -> ItemRef {
    if path.starts_with(IMPORTED_DIR) { ItemRef::Manifest } else { crate::item_id::item_for_path(path) }
}

fn repair_before(error: Error, operation: &str) -> Error {
    match error {
        Error::DuplicateItemId { .. } => Error::Invalid(format!(
            "can't {operation}: {error}. Run `sk1ll repair-ids`, then commit the repaired items"
        )),
        error => error,
    }
}

fn is_sidecar(path: &Path) -> bool {
    path.starts_with(PERSONAL_DIR) && path.components().count() == 3 && path.ends_with(crate::skill::ID_FILE)
}

fn duplicate_after(error: Error, operation: &str) -> Error {
    match error {
        Error::DuplicateItemId { .. } => {
            Error::Invalid(format!("can't {operation}: {error}. For a rename or fork, select both sides together"))
        }
        error => error,
    }
}

pub(crate) fn pull_journal_exists(git_dir: &Path) -> Result<bool> {
    pull::journal_exists(git_dir)
}

/// Puts back a working-tree file pull displaced (see [`sync::displaced::adopt`](crate::sync::displaced::adopt)).
pub(crate) fn adopt_displaced(
    repo: &Repo,
    entry: &crate::sync::displaced::Entry,
    lock: &crate::EngineLock,
) -> Result<()> {
    lock.check_for(repo.root())?;
    pull::adopt(repo, entry)
}

/// The paths `git status` reports as changed under `pathspec` (everything
/// if empty), as git spells them (relative to the top level), each with
/// whether it is untracked.
fn status_paths(git: Git, pathspec: &[&str]) -> Result<Vec<(Vec<u8>, bool)>> {
    let out = git
        .literal()
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all", "--no-renames", "--"])
        .args(pathspec)
        .run()?;
    let mut changed = Vec::new();
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
            changed.push((from.to_vec(), false));
        }
        changed.push((entry[3..].to_vec(), entry[0] == b'?'));
    }
    Ok(changed)
}

fn rev(git: Git, spec: &str) -> Result<Option<String>> {
    let out = git.args(["rev-parse", "--verify", "--quiet", &format!("{spec}^{{commit}}")]).output()?;
    Ok(out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()))
}

/// [`FLEET_BRANCH`], if that is the branch checked out in `root`; else how to get there.
fn on_fleet_branch(git: Git, root: &Path) -> Result<String> {
    let branch = git
        .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
        .text()
        .map_err(|_| Error::Invalid(format!("HEAD is detached; check out `{FLEET_BRANCH}` first")))?;
    if branch != FLEET_BRANCH {
        return Err(Error::Invalid(format!(
            "{} is on branch `{branch}`, but the fleet's baseline is `{FLEET_BRANCH}`; switch with `git -C {} \
             checkout {FLEET_BRANCH}`, or rename the branch with `git -C {} branch -m {branch} {FLEET_BRANCH}` \
             if it holds this machine's work",
            root.display(),
            root.display(),
            root.display()
        )));
    }
    Ok(branch)
}

/// Whether git has a configured `user.name` and `user.email`.
fn has_git_identity(git: impl Fn() -> Git) -> Result<bool> {
    Ok(git().args(["config", "user.name"]).succeeds()? && git().args(["config", "user.email"]).succeeds()?)
}

/// Why a pull can't move onto a baseline that shares no commit with `HEAD`
/// (say, two machines that each made a root commit on an empty server). This is
/// an error rather than [`PullOutcome::Blocked`]: Blocked means "commit or
/// restore these items, then pull again", which can never resolve this.
fn unrelated_histories(root: &Path, branch: &str, head: &str, remote: &str) -> Error {
    let short = |s: &str| s.chars().take(12).collect::<String>();
    Error::Invalid(format!(
        "can't pull: this machine's `{branch}` ({}) and the server's baseline ({}) have unrelated histories (no \
         common commit), so local commits can't be carried onto it. Nothing was changed. To keep this machine's \
         commits, stash the draft and run `git -C {} rebase --root --onto {REMOTE}/{branch}`; or move the repo \
         aside and clone it again from the server",
        short(head),
        short(remote),
        root.display(),
    ))
}

/// The draft entries for `items`; every one must be in the draft.
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

/// The mode and object of the first `<fields>\t<path>` record in `out`,
/// at the given field positions.
fn entry_fields(out: &[u8], mode: usize, oid: usize) -> Option<keep::IndexEntry> {
    let record = out.split(|b| *b == 0).find(|r| !r.is_empty())?;
    let tab = record.iter().position(|b| *b == b'\t')?;
    let fields: Vec<&str> = std::str::from_utf8(&record[..tab]).ok()?.split(' ').collect();
    Some(keep::IndexEntry(fields.get(mode)?.to_string(), fields.get(oid)?.to_string()))
}

/// `s` as one word for a POSIX shell, for commands in messages.
fn shell_quote(s: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | ':' | '@' | '+' | ',');
    if !s.is_empty() && s.chars().all(plain) { s.to_string() } else { format!("'{}'", s.replace('\'', "'\\''")) }
}

/// Item names use `/`; only on Windows is `\\` a separator too.
fn slash(path: &str) -> String {
    if cfg!(windows) { path.replace('\\', "/") } else { path.to_string() }
}

/// Git's C-style quoting (as in `core.quotePath`), but keeping valid UTF-8 as is.
pub(crate) fn quote_path(raw: &[u8]) -> String {
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
pub(crate) fn unquote_path(quoted: &str) -> Option<Vec<u8>> {
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
