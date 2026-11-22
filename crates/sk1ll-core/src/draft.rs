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

mod pull;

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
        let kept = self.keep_in_recovery(selected.iter().filter(|d| keep.contains(&d.item)).flat_map(|d| &d.paths))?;
        let (in_head, new): (Vec<&PathBuf>, Vec<&PathBuf>) =
            selected.iter().flat_map(|d| &d.paths).partition(|p| st.head_paths.contains(*p));
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
        Ok(kept)
    }

    /// Moves each file or symlink at `paths` (repo-relative) into a recovery
    /// entry of its own in the git directory's recovery area, as pull does
    /// for what it displaces; a path with nothing there (a deletion) is
    /// skipped. Returns the entries' ids.
    #[cfg(unix)]
    fn keep_in_recovery<'a>(&self, paths: impl Iterator<Item = &'a PathBuf>) -> Result<Vec<String>> {
        use crate::sync::displaced::{Displacing, Kind, PULL};
        let paths: Vec<&PathBuf> = paths.collect();
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let git_dir = PathBuf::from(self.git()?.args(["rev-parse", "--absolute-git-dir"]).text()?);
        let mut kept = Vec::new();
        for rel in paths {
            let path = self.root().join(rel);
            match fs::symlink_metadata(&path) {
                Ok(meta) if !meta.is_dir() => {}
                _ => continue,
            }
            let mut held = Displacing::begin(Some(self), Kind::Draft, PULL, &path, None, None, &git_dir)?;
            if let Err(e) = held.take(&path) {
                held.abandon();
                return Err(e);
            }
            kept.push(held.id().to_string());
            held.finish(false)?;
        }
        Ok(kept)
    }

    #[cfg(not(unix))]
    fn keep_in_recovery<'a>(&self, paths: impl Iterator<Item = &'a PathBuf>) -> Result<Vec<String>> {
        match paths.count() {
            0 => Ok(Vec::new()),
            _ => Err(Error::Invalid("keeping a restored version in recovery needs Unix".into())),
        }
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
        let pulled = pull::run(self, true, &mut |_| Ok(()));
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
        let pulled = pull::run(self, false, &mut |_| Ok(()));
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
        match pull::finish(self, &mut |_| Ok(())) {
            Ok(None) => Ok(None),
            Ok(Some(outcome)) => self.check_pulled_ids(Ok(outcome)).map(Some),
            Err(e) => self.check_pulled_ids(Err(e)).map(Some),
        }
    }

    /// [`pull`](Self::pull), running `f` once when it reaches `point`.
    #[cfg(test)]
    fn pull_at(&self, point: pull::Before, f: impl FnOnce()) -> Result<PullOutcome> {
        let _lock = crate::EngineLock::acquire(self.root())?;
        let mut f = Some(f);
        pull::run(self, true, &mut |at| {
            if at == point
                && let Some(f) = f.take()
            {
                f();
            }
            Ok(())
        })
    }

    /// [`pull`](Self::pull), stopping at `point` as a crash would.
    #[cfg(test)]
    fn pull_crashing_at(&self, point: pull::Before) -> Result<PullOutcome> {
        let _lock = crate::EngineLock::acquire(self.root())?;
        pull::run(self, true, &mut |at| match at == point {
            true => Err(Error::Invalid(format!("crashed at {at:?}"))),
            false => Ok(()),
        })
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

#[cfg(test)]
mod tests {
    use super::pull::Before;
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

    /// No git command on the skills repo runs a filter driver: not the
    /// Draft's `status`, a diff, a commit's `add` or a restore's checkout,
    /// even for a driver a conditional include (`includeIf.gitdir`) names.
    #[test]
    fn draft_commands_run_no_filter_driver() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp = tmp.path().canonicalize().unwrap();
        let root = tmp.join("repo");
        let repo = Repo::init_committed(&root).unwrap();
        let marker = tmp.join("ran");
        let cmd = format!("touch '{}'; cat", marker.display());
        let filter = tmp.join("filter.gitconfig");
        write(&filter, &format!("[filter \"x\"]\n\tclean = {cmd}\n\tsmudge = {cmd}\n\trequired = true\n"));
        git(&root, &["config", &format!("includeIf.gitdir:{}/.path", root.display()), filter.to_str().unwrap()]);
        write(&root.join(".git/info/attributes"), "*.md filter=x\n");
        repo.create_item("skill", "pdf", "PDF").unwrap();
        let pdf = item("skill:pdf");
        repo.commit(std::slice::from_ref(&pdf), "Add PDF", None).unwrap();
        // Later than the index: status must look at the content.
        std::thread::sleep(std::time::Duration::from_millis(20));
        write(&root.join("personal/pdf/SKILL.md"), "Edited.\n");
        assert_eq!(items(&repo), [("skill:pdf".to_string(), Change::Modified)]);
        assert!(repo.diff(&pdf).unwrap().contains("Edited."));
        repo.restore(std::slice::from_ref(&pdf)).unwrap();
        assert!(repo.draft().unwrap().is_empty());
        assert!(!marker.exists(), "a filter driver ran");
        // The include does name the driver where git runs.
        write(&root.join("personal/pdf/SKILL.md"), "Edited again.\n");
        let _ = Git::new(&root).args(["add", "--all"]).output();
        assert!(marker.exists(), "the test's driver is not configured as git reads it");
    }

    #[test]
    fn one_side_of_an_explicit_id_rename_cannot_be_committed_or_restored() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::init_committed(tmp.path()).unwrap();
        let old = ItemRef::Skill("pdf".into());
        let new = ItemRef::Skill("docs".into());
        repo.create_item("skill", "pdf", "PDF").unwrap();
        let baseline = repo.commit(std::slice::from_ref(&old), "Add PDF", None).unwrap();
        let id = repo.skills().unwrap()[0].id.clone();
        repo.rename_item("skill", "pdf", "docs").unwrap();
        let errors = [&old, &new].into_iter().flat_map(|item| {
            [
                repo.commit(std::slice::from_ref(item), "Rename side", None).unwrap_err(),
                repo.restore(std::slice::from_ref(item)).unwrap_err(),
            ]
        });
        for err in errors {
            let message = err.to_string();
            for expected in ["both sides", "personal/pdf/.sk1ll-id", "personal/docs/.sk1ll-id"] {
                assert!(message.contains(expected), "{message}");
            }
        }
        assert_eq!(git(repo.root(), &["rev-parse", "HEAD"]), baseline);
        assert_eq!(repo.skills().unwrap()[0].name, "docs");
        repo.restore(&[old.clone(), new.clone()]).unwrap();
        assert_eq!(repo.skills().unwrap()[0].name, "pdf");
        repo.rename_item("skill", "pdf", "docs").unwrap();
        repo.commit(&[old, new], "Rename PDF", None).unwrap();
        assert_eq!(crate::content::item_ids_at(repo.root(), "HEAD").unwrap()[&ItemRef::Skill("docs".into())], id);
        assert!(repo.draft().unwrap().is_empty());
    }

    #[test]
    fn committing_a_handmade_import_persists_its_id_and_requires_the_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::init_committed(tmp.path()).unwrap();
        write(&repo.root().join("sk1ll.toml"), "import = { pdf = { source = 'upstream' } }\n");
        write(&repo.imported_dir().join("pdf/SKILL.md"), "PDFs.\n");
        let pdf = item("skill:pdf");
        let error = repo.commit(std::slice::from_ref(&pdf), "Import without manifest", None).unwrap_err();
        assert!(error.to_string().contains("Select `manifest` too"));
        let id = crate::Manifest::load(repo.root()).unwrap().import["pdf"].id.clone().unwrap();
        repo.commit(&[pdf.clone(), ItemRef::Manifest], "Import with manifest", None).unwrap();
        assert_eq!(crate::content::item_ids_at(repo.root(), "HEAD").unwrap()[&pdf], Some(id));
    }

    #[test]
    fn item_commit_tracks_an_ignored_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::init_committed(tmp.path()).unwrap();
        write(&repo.root().join(".gitignore"), ".sk1ll-*\n");
        repo.create_item("skill", "pdf", "PDF").unwrap();
        let sidecar = "personal/pdf/.sk1ll-id";
        assert_eq!(git(repo.root(), &["check-ignore", sidecar]), sidecar);
        let id = repo.skills().unwrap()[0].id.as_ref().unwrap().to_string();
        repo.commit(&[ItemRef::Skill("pdf".into())], "Add PDF", None).unwrap();
        assert_eq!(git(repo.root(), &["show", &format!("HEAD:{sidecar}")]), id);
        assert!(repo.draft().unwrap().iter().all(|change| matches!(change.item, ItemRef::Other(_))));
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

    fn pull_now(repo: &Repo) -> Result<PullOutcome> {
        repo.pull(&crate::EngineLock::acquire(repo.root())?)
    }

    fn finish_now(repo: &Repo) -> Result<Option<PullOutcome>> {
        repo.finish_pull(&crate::EngineLock::acquire(repo.root())?)
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

    /// A working-tree entry, as [`snapshot`] records it.
    #[derive(Debug, PartialEq, Eq)]
    enum Node {
        /// The content, and whether it is executable.
        File(Vec<u8>, bool),
        Link(PathBuf),
    }

    /// Everything a pull could disturb: files (symlinks and the executable
    /// bit included), HEAD and the branch it is on, index and stash.
    fn snapshot(root: &Path) -> (BTreeMap<PathBuf, Node>, String, String, String) {
        let files = WalkDir::new(root)
            .into_iter()
            .filter_entry(|e| e.file_name() != ".git")
            .filter_map(|e| e.ok())
            .filter(|e| !e.file_type().is_dir())
            .map(|e| {
                let node = if e.file_type().is_symlink() {
                    Node::Link(fs::read_link(e.path()).unwrap())
                } else {
                    Node::File(fs::read(e.path()).unwrap(), executable(e.path()))
                };
                (e.path().strip_prefix(root).unwrap().to_path_buf(), node)
            })
            .collect();
        let stash = Git::new(root).args(["stash", "list"]).text().unwrap();
        let head = git(root, &["rev-parse", "--symbolic-full-name", "HEAD"]) + " " + &git(root, &["rev-parse", "HEAD"]);
        (files, head, git(root, &["ls-files", "-s"]), stash)
    }

    fn executable(path: &Path) -> bool {
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
    fn chmod(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn item_refs_round_trip() {
        for s in ["skill:pdf", "rule:10-git", "manifest", "other:notes/a.txt"] {
            assert_eq!(item(s).to_string(), s);
            assert_eq!(serde_json::to_string(&item(s)).unwrap(), format!("\"{s}\""));
        }
        assert!("bogus:x".parse::<ItemRef>().is_err() && "skill:".parse::<ItemRef>().is_err());
        let for_path = |p: &str| ItemRef::for_path(Path::new(p)).to_string();
        assert_eq!(for_path("imported/web/sub/x.md"), "skill:web");
        assert_eq!(for_path("personal/.gitkeep"), "other:personal/.gitkeep");
        assert_eq!(for_path("instructions/git.md"), "rule:git");
        assert_eq!(for_path("instructions/.gitkeep"), "other:instructions/.gitkeep");
        assert_eq!(for_path("sk1ll.lock"), "manifest");
        assert_eq!(
            default_commit_message(&[item("skill:pdf"), item("rule:git-flow")], "laptop"),
            "commit pdf, git-flow from laptop"
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
            ("rule:git", Modified),
            ("rule:style", Deleted),
            ("rule:tone", Added),
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
        assert!(r.diff(&item("rule:tone")).unwrap().contains("+Be terse."));
        assert!(matches!(r.diff(&item("skill:nope")), Err(Error::Invalid(_))));
        assert_eq!(ItemRef::resolve("pdf", &draft).unwrap(), item("skill:pdf"));
        assert!(ItemRef::resolve("nope", &draft).is_err());
    }

    #[test]
    fn partial_commit_leaves_the_rest_in_draft() {
        let f = Fleet::new();
        let r = repo(&f.b); // b has no git identity
        assert!(!r.has_git_identity().unwrap());
        write(&f.b.join("personal/pdf/SKILL.md"), "committed\n");
        fs::remove_dir_all(f.b.join("imported/web")).unwrap();
        write(&f.b.join("instructions/git.md"), "draft only\n");
        write(&f.b.join("notes.txt"), "staged\n");
        git(&f.b, &["add", "notes.txt"]);

        assert!(matches!(r.commit(&[item("skill:nope")], "m", None), Err(Error::Invalid(_))));
        let sha = r.commit(&[item("skill:pdf"), item("skill:web")], "commit pdf, web from b", None).unwrap();
        assert_eq!(git(&f.b, &["rev-parse", "HEAD"]), sha);
        let files = git(&f.b, &["show", "--name-only", "--format=", "HEAD"]);
        assert_eq!(
            files.lines().collect::<Vec<_>>(),
            ["imported/web/SKILL.md", "personal/pdf/.sk1ll-id", "personal/pdf/SKILL.md"]
        );
        let expected = format!("{}|commit pdf, web from b", machine::hostname());
        assert_eq!(git(&f.b, &["log", "-1", "--format=%an|%s"]), expected);
        assert_eq!(items(&r), [("rule:git".into(), Change::Modified), ("other:notes.txt".into(), Change::Added)]);
        assert_eq!(git(&f.b, &["diff", "--cached", "--name-only"]), "notes.txt");
        assert_eq!(read(&f.b.join("instructions/git.md")), "draft only\n");

        let sha = r.commit(&[item("rule:git")], "git", Some("laptop <sk1ll@laptop>")).unwrap();
        assert_eq!(git(&f.b, &["log", "-1", "--format=%an <%ae>"]), "laptop <sk1ll@laptop>");
        r.push().unwrap();
        assert_eq!(git(&f.b, &["rev-parse", "origin/main"]), sha);
        assert_eq!(items(&r), [("other:notes.txt".into(), Change::Added)]);
    }

    #[test]
    fn restore_restores_baseline() {
        let f = Fleet::new();
        let r = repo(&f.a);
        let before = snapshot(&f.a);
        write(&f.a.join("personal/pdf/SKILL.md"), "changed\n");
        write(&f.a.join("personal/new/deep/SKILL.md"), "new\n");
        write(&f.a.join("instructions/new.md"), "staged new\n");
        git(&f.a, &["add", "instructions/new.md"]);
        fs::remove_dir_all(f.a.join("imported/web")).unwrap();
        write(&f.a.join("notes.txt"), "n\n");

        r.restore(&[item("skill:pdf"), item("skill:new"), item("rule:new"), item("skill:web")]).unwrap();
        assert_eq!(items(&r), [("other:notes.txt".into(), Change::Added)]);
        assert!(!f.a.join("personal/new").exists());
        assert!(f.a.join("personal").is_dir());
        r.restore_all().unwrap();
        assert!(r.draft().unwrap().is_empty());
        assert_eq!(snapshot(&f.a), before);
    }

    #[test]
    fn pull_with_clean_draft_fast_forwards() {
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        assert_eq!(pull_now(&b).unwrap(), PullOutcome::UpToDate);
        let old = git(&f.b, &["rev-parse", "HEAD"]);
        write(&f.a.join("personal/pdf/SKILL.md"), "v2\n");
        let new = a.commit(&[item("skill:pdf")], "v2", None).unwrap();
        a.push().unwrap();
        assert_eq!(pull_now(&b).unwrap(), PullOutcome::Updated { from: old, to: new });
        assert_eq!(read(&f.b.join("personal/pdf/SKILL.md")), "v2\n");
        assert_eq!(pull_now(&b).unwrap(), PullOutcome::UpToDate);
    }

    /// A caller can fetch and push without its engine lock: the local half
    /// of a pull only moves onto what was fetched, and a push sends the exact
    /// commit that was checked.
    #[test]
    fn fetching_and_pushing_split_from_the_local_steps() {
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        let old = git(&f.b, &["rev-parse", "HEAD"]);
        write(&f.a.join("personal/pdf/SKILL.md"), "v2\n");
        let checked = a.commit(&[item("skill:pdf")], "v2", None).unwrap();
        assert_eq!(a.prepare_push().unwrap().as_deref(), Some(checked.as_str()));
        // A commit made after the check is not pushed with it.
        write(&f.a.join("instructions/git.md"), "later\n");
        let later = a.commit(&[item("rule:git")], "later", None).unwrap();
        a.push_commit(&checked).unwrap();
        assert_eq!(git(&f.a, &["rev-parse", "refs/remotes/origin/main"]), checked);
        assert_eq!(git(&f.a, &["rev-parse", "HEAD"]), later);

        let lock = crate::EngineLock::acquire(&f.b).unwrap();
        assert_eq!(b.pull_fetched(&lock).unwrap(), PullOutcome::UpToDate, "nothing fetched yet");
        assert_eq!(git(&f.b, &["rev-parse", "HEAD"]), old);
        b.fetch().unwrap();
        assert_eq!(git(&f.b, &["rev-parse", "HEAD"]), old, "a fetch moves no branch");
        assert_eq!(b.pull_fetched(&lock).unwrap(), PullOutcome::Updated { from: old, to: checked });
        assert_eq!(read(&f.b.join("personal/pdf/SKILL.md")), "v2\n");
    }

    /// An import being staged stays where it is, even with a `.gitignore`
    /// that negates sk1ll's exclude.
    #[test]
    fn pull_leaves_imports_being_staged_alone() {
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        write(&f.b.join(".git/info/exclude"), ".sk1ll-stage-*\n");
        write(&f.b.join(".gitignore"), "!.sk1ll-stage-*/\n");
        let staged = f.b.join("imported/.sk1ll-stage-1-2-3/tree/SKILL.md");
        write(&staged, "being staged\n");
        // An unpushed commit, so the pull rebases.
        write(&f.b.join("instructions/style.md"), "b committed\n");
        b.commit(&[item("rule:style")], "style from b", None).unwrap();
        write(&f.b.join("instructions/git.md"), "b draft\n");
        let draft = b.draft().unwrap();
        assert!(draft.iter().all(|d| !d.item.to_string().contains(".sk1ll-stage-")), "{draft:?}");
        // Replacing the file would give it a new mtime.
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        fs::File::options().write(true).open(&staged).unwrap().set_modified(old).unwrap();
        let mtime = || fs::metadata(&staged).unwrap().modified().unwrap();

        write(&f.a.join("personal/pdf/SKILL.md"), "v2\n");
        a.commit(&[item("skill:pdf")], "v2", None).unwrap();
        a.push().unwrap();
        assert!(matches!(pull_now(&b).unwrap(), PullOutcome::Updated { .. }));
        assert_eq!(read(&staged), "being staged\n");
        assert_eq!(mtime(), old, "never swept away and put back");
        assert_eq!(b.draft().unwrap(), draft);
        assert_eq!(git(&f.b, &["stash", "list"]), "");
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
        let v2 = a.commit(&[item("skill:pdf")], "v2", None).unwrap();
        a.push().unwrap();
        assert!(matches!(pull_now(&b).unwrap(), PullOutcome::Updated { to, .. } if to == v2));
        assert_eq!(read(&f.b.join("personal/pdf/SKILL.md")), "v2\n");
        assert_eq!(b.draft().unwrap(), draft);
        assert_eq!(read(&f.b.join("instructions/git.md")), "b draft\n");
        assert_eq!(git(&f.b, &["diff", "--cached", "--name-only"]), "notes.txt");

        // Committed but unpushed commits are rebased onto the new baseline.
        write(&f.b.join("instructions/style.md"), "b committed\n");
        b.commit(&[item("rule:style")], "style from b", None).unwrap();
        write(&f.a.join("imported/web/SKILL.md"), "web v2\n");
        let web = a.commit(&[item("skill:web")], "web v2", None).unwrap();
        a.push().unwrap();
        assert!(matches!(pull_now(&b).unwrap(), PullOutcome::Updated { .. }));
        assert_eq!(git(&f.b, &["rev-parse", "HEAD~1"]), web);
        assert_eq!(git(&f.b, &["log", "-1", "--format=%s"]), "style from b");
        assert_eq!(read(&f.b.join("imported/web/SKILL.md")), "web v2\n");
        assert!(read(&f.b.join("instructions/style.md")).ends_with("b committed\n"));
        assert_eq!(b.draft().unwrap(), draft);
        assert_eq!(git(&f.b, &["diff", "--cached", "--name-only"]), "notes.txt");
        assert_eq!(git(&f.b, &["stash", "list"]), "");
    }

    #[test]
    fn pull_replay_detects_duplicate_ids_from_two_clones_before_push() {
        let f = Fleet::new();
        let a = repo(&f.a);
        let b = repo(&f.b);
        // Both machines committed a copy of the same skill at disjoint paths.
        a.create_item("skill", "a-copy", "").unwrap();
        let from = a.personal_dir().join("a-copy");
        let to = b.personal_dir().join("b-copy");
        fs::create_dir(&to).unwrap();
        for name in ["SKILL.md", ".sk1ll-id"] {
            fs::copy(from.join(name), to.join(name)).unwrap();
        }
        a.commit(&[item("skill:a-copy")], "Add copy on A", None).unwrap();
        b.commit(&[item("skill:b-copy")], "Add copy on B", None).unwrap();
        a.push().unwrap();
        let err = pull_now(&b).unwrap_err().to_string();
        assert!(err.contains("sk1ll repair-ids") && err.contains("a-copy") && err.contains("b-copy"), "{err}");
        assert!(b.push().unwrap_err().to_string().contains("sk1ll repair-ids"));
        // The copy, and the fixture's items that never had an Item ID.
        assert!(b.repair_ids().unwrap() > 1);
        assert!(b.draft().unwrap().iter().any(|change| change.item == item("skill:b-copy")));
        b.commit(&[item("skill:b-copy")], "Repair duplicate ID", None).unwrap();
        b.push().unwrap();
    }

    #[test]
    fn pull_blocked_by_clash_changes_nothing() {
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        write(&f.b.join("personal/pdf/ref.md"), "b's edit\n");
        write(&f.b.join("personal/new/SKILL.md"), "b new\n");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("SKILL.md", f.b.join("personal/new/link.md")).unwrap();
            chmod(&f.b.join("instructions/style.md"), 0o755);
        }
        write(&f.a.join("personal/pdf/SKILL.md"), "a's edit\n");
        let remote = a.commit(&[item("skill:pdf")], "a", None).unwrap();
        a.push().unwrap();

        let before = snapshot(&f.b);
        let outcome = pull_now(&b).unwrap();
        assert_eq!(
            outcome,
            PullOutcome::Blocked { baseline: remote.clone(), items: vec![item("skill:pdf")], uncommitted: Vec::new() }
        );
        assert_eq!(snapshot(&f.b), before);

        // An unpushed commit that conflicts with the new baseline backs out too.
        b.restore(&[item("skill:pdf")]).unwrap();
        write(&f.b.join("personal/pdf/SKILL.md"), "b's committed edit\n");
        b.commit(&[item("skill:pdf")], "b", None).unwrap();
        write(&f.b.join("personal/pdf/ref.md"), "unrelated?\n");
        let before = snapshot(&f.b);
        // The draft touches pdf too, so this is caught before anything moves.
        assert!(matches!(pull_now(&b).unwrap(), PullOutcome::Blocked { .. }));
        assert_eq!(snapshot(&f.b), before);

        // Without the draft on top, the conflicting commit turns back into
        // Draft: only the branch moves, every file stays as it was.
        b.restore(&[item("skill:pdf")]).unwrap();
        let (files, _, _, _) = snapshot(&f.b);
        let outcome = pull_now(&b).unwrap();
        let PullOutcome::Blocked { baseline, items, uncommitted } = outcome else { panic!("{outcome:?}") };
        assert_eq!((baseline, items), (remote, vec![item("skill:pdf")]));
        assert!(uncommitted.len() == 1 && uncommitted[0].ends_with(" b"), "{uncommitted:?}");
        assert_eq!(snapshot(&f.b).0, files);
        assert!(f.b.join("personal/new/SKILL.md").is_file());
        assert!(items_of(&b).contains(&item("skill:pdf")));
    }

    fn items_of(repo: &Repo) -> Vec<ItemRef> {
        repo.draft().unwrap().into_iter().map(|d| d.item).collect()
    }

    fn pending(repo: &Repo) -> bool {
        pull::pending(repo).unwrap()
    }

    fn head_ref(root: &Path) -> String {
        git(root, &["symbolic-ref", "HEAD"])
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
    fn restore_and_commit_refuse_to_cross_item_boundaries() {
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
        assert!(invalid(r.restore(&[item("other:notes")])).contains("other:notes/private.txt"));
        assert!(invalid(r.restore(&[item("other:notes/private.txt")])).contains("other:notes"));
        assert!(invalid(r.commit(&[item("other:docs")], "m", None)).contains("other:docs/private"));
        assert!(invalid(r.commit(&[item("other:docs/private")], "m", None)).contains("other:docs"));
        assert_eq!(snapshot(&f.a), before);
        assert_eq!(git(&f.a, &["rev-parse", "HEAD"]), head);

        // Together they're fine.
        r.restore(&[item("other:notes"), item("other:notes/private.txt")]).unwrap();
        assert_eq!(read(&f.a.join("notes")), "baseline file\n");
        r.commit(&[item("other:docs"), item("other:docs/private")], "docs", None).unwrap();
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
        let remote = a.commit(&[item("other:secret.txt"), item("other:build/out.txt")], "secret", None).unwrap();
        a.push().unwrap();

        // Fast-forward path.
        let before = snapshot(&f.b);
        let blocked = PullOutcome::Blocked {
            baseline: remote.clone(),
            items: vec![item("other:build/out.txt"), item("other:secret.txt")],
            uncommitted: Vec::new(),
        };
        assert_eq!(pull_now(&b).unwrap(), blocked);
        assert_eq!(snapshot(&f.b), before);

        // Rebase path.
        write(&f.b.join("instructions/style.md"), "b committed\n");
        b.commit(&[item("rule:style")], "style", None).unwrap();
        let before = snapshot(&f.b);
        assert_eq!(pull_now(&b).unwrap(), blocked);
        assert_eq!(snapshot(&f.b), before);
        assert_eq!(read(&f.b.join("secret.txt")), "mine\n");
    }

    /// An ignored file that appears in the way during the pull is caught by
    /// pull's last checks, or else when the new file goes in (which never
    /// replaces anything), on the fast-forward path and when rebasing.
    #[test]
    fn pull_protects_an_ignored_file_created_during_the_pull() {
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        write(&f.b.join(".git/info/exclude"), "private.txt\n");
        write(&f.a.join("private.txt"), "upstream\n");
        let remote = a.commit(&[item("other:private.txt")], "private", None).unwrap();
        a.push().unwrap();
        let path = f.b.join("private.txt");
        let blocked =
            PullOutcome::Blocked { baseline: remote, items: vec![item("other:private.txt")], uncommitted: Vec::new() };
        for rebase in [false, true] {
            if rebase {
                write(&f.b.join("instructions/style.md"), "b committed\n");
                b.commit(&[item("rule:style")], "style", None).unwrap();
            }
            for point in [Before::Checks, Before::Journal, Before::File(0)] {
                let before = snapshot(&f.b);
                assert_eq!(b.pull_at(point, || write(&path, "mine\n")).unwrap(), blocked, "{point:?}");
                assert_eq!(read(&path), "mine\n");
                assert!(!pending(&b), "nothing was changed, so nothing is left to finish");
                fs::remove_file(&path).unwrap();
                assert_eq!(snapshot(&f.b), before);
            }
        }
    }

    /// A save that lands while pull works, in an item the new baseline
    /// changes, blocks the pull like any draft edit there.
    #[test]
    fn pull_blocks_a_save_made_during_the_pull() {
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        write(&f.a.join("personal/pdf/SKILL.md"), "a's edit\n");
        let remote = a.commit(&[item("skill:pdf")], "pdf", None).unwrap();
        a.push().unwrap();
        let (skill, sibling) = (f.b.join("personal/pdf/SKILL.md"), f.b.join("personal/pdf/ref.md"));
        let blocked =
            PullOutcome::Blocked { baseline: remote, items: vec![item("skill:pdf")], uncommitted: Vec::new() };
        for rebase in [false, true] {
            if rebase {
                write(&f.b.join("instructions/style.md"), "b committed\n");
                b.commit(&[item("rule:style")], "style", None).unwrap();
            }
            let before = snapshot(&f.b);
            assert_eq!(b.pull_at(Before::Checks, || write(&sibling, "saved during pull\n")).unwrap(), blocked);
            assert_eq!(read(&sibling), "saved during pull\n");
            b.restore(&[item("skill:pdf")]).unwrap();
            assert_eq!(snapshot(&f.b), before);
        }
        // Each path is checked again right before it is updated. The first
        // one is the Item ID a's commit gave pdf (it sorts before SKILL.md).
        let id = f.b.join("personal/pdf/.sk1ll-id");
        assert_eq!(b.pull_at(Before::File(0), || write(&id, "saved\n")).unwrap(), blocked);
        assert_eq!(read(&id), "saved\n");
        assert_eq!(read(&skill), "---\nname: pdf\n---\nPDFs.\n");
        assert!(!pending(&b));
    }

    #[test]
    fn pull_rebase_conflict_leaves_head_index_and_files_unchanged() {
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        // The first commit replays cleanly. The second conflicts.
        write(&f.b.join("instructions/style.md"), "b committed\n");
        b.commit(&[item("rule:style")], "style", None).unwrap();
        write(&f.b.join("imported/web/SKILL.md"), "b committed\n");
        let head = b.commit(&[item("skill:web")], "web from b", None).unwrap();
        write(&f.a.join("imported/web/SKILL.md"), "web v2\n");
        let remote = a.commit(&[item("skill:web")], "web v2", None).unwrap();
        a.push().unwrap();
        write(&f.b.join("instructions/git.md"), "staged\n");
        git(&f.b, &["add", "instructions/git.md"]);
        write(&f.b.join("instructions/git.md"), "unstaged\n");
        write(&f.b.join("personal/new/SKILL.md"), "untracked\n");
        // Force stale stat information to catch a status call writing the index.
        fs::File::options()
            .write(true)
            .open(f.b.join("personal/pdf/SKILL.md"))
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000))
            .unwrap();

        let (files, ..) = snapshot(&f.b);
        let index = fs::read(f.b.join(".git/index")).unwrap();
        let style = git(&f.b, &["rev-parse", &format!("{head}^")]);
        let outcome = b.pull_at(Before::Checks, || panic!("conflicting replay reached the tree switch")).unwrap();
        let PullOutcome::Blocked { baseline, items, uncommitted } = outcome else { panic!("{outcome:?}") };
        assert_eq!((baseline, items), (remote, vec![item("skill:web")]));
        assert!(uncommitted.len() == 1 && uncommitted[0].ends_with(" web from b"), "{uncommitted:?}");
        // The branch is back on the commit that replays cleanly; the
        // conflicting one is Draft again, staged, and no byte of the
        // working tree or the index changed.
        assert_eq!(snapshot(&f.b).0, files);
        assert_eq!(git(&f.b, &["rev-parse", "refs/heads/main"]), style);
        assert_eq!(fs::read(f.b.join(".git/index")).unwrap(), index);
        assert!(items_of(&b).contains(&item("skill:web")));
        assert_eq!(fs::read_to_string(f.b.join("imported/web/SKILL.md")).unwrap(), "b committed\n");
    }

    /// The whole way out of a clash with a committed item: the commit turns
    /// back into Draft, a restore that keeps it in recovery clears the way,
    /// and adopting the entry brings it back on the new Baseline.
    #[test]
    fn a_conflicting_unpushed_commit_turns_back_into_draft_and_comes_back_after_the_pull() {
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        write(&f.b.join("personal/pdf/SKILL.md"), "b's edit\n");
        write(&f.a.join("personal/pdf/SKILL.md"), "a's edit\n");
        let remote = a.commit(&[item("skill:pdf")], "a", None).unwrap();
        a.push().unwrap();
        let base = git(&f.b, &["rev-parse", "HEAD"]);
        b.commit(&[item("skill:pdf")], "b", None).unwrap();

        let outcome = pull_now(&b).unwrap();
        assert!(matches!(&outcome, PullOutcome::Blocked { uncommitted, .. } if uncommitted.len() == 1), "{outcome:?}");
        assert_eq!(git(&f.b, &["rev-parse", "HEAD"]), base);
        assert_eq!(read(&f.b.join("personal/pdf/SKILL.md")), "b's edit\n");

        let kept = b.restore_keeping(&[item("skill:pdf")], &[item("skill:pdf")].into()).unwrap();
        let entries: Vec<_> = kept.iter().map(|id| crate::sync::displaced::get(id).unwrap()).collect();
        let skill = entries.iter().find(|e| e.dest.ends_with("personal/pdf/SKILL.md")).expect("SKILL.md is kept");
        assert_eq!(pull_now(&b).unwrap(), PullOutcome::Updated { from: base, to: remote });
        assert_eq!(read(&f.b.join("personal/pdf/SKILL.md")), "a's edit\n");
        let _lock = crate::EngineLock::acquire(b.root()).unwrap();
        crate::sync::displaced::adopt(&b, &skill.id, &_lock).unwrap();
        assert_eq!(read(&f.b.join("personal/pdf/SKILL.md")), "b's edit\n");
        assert!(items_of(&b).contains(&item("skill:pdf")));
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

        r.restore(&[item("other:notes\\b")]).unwrap();
        assert_eq!(git(&f.a, &["ls-files", "notes"]), "notes/b");
        assert_eq!(read(&f.a.join("notes/b")), "tracked\n");
        assert!(!f.a.join("notes\\b").exists());
        r.restore(&[draft[0].item.clone()]).unwrap();
        assert!(!f.a.join(bad).exists());
        assert!(r.draft().unwrap().is_empty());
    }

    #[test]
    fn restore_keeps_a_manifest_that_was_never_committed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        let r = Repo::init(&root).unwrap();
        r.restore_all().unwrap();
        assert!(root.join(MANIFEST_FILE).is_file());
        assert_eq!(items(&r), [("manifest".into(), Change::Added)]);
        assert!(invalid(r.restore(&[ItemRef::Manifest])).contains("commit it first"));

        // A baseline without the manifest is no better.
        git(&root, &["config", "user.name", "T"]);
        git(&root, &["config", "user.email", "t@example.com"]);
        write(&root.join("notes.txt"), "n\n");
        git(&root, &["add", "notes.txt"]);
        git(&root, &["commit", "--quiet", "-m", "notes"]);
        write(&root.join("instructions/x.md"), "x\n");
        r.restore_all().unwrap();
        assert!(root.join(MANIFEST_FILE).is_file());
        assert_eq!(items(&r), [("manifest".into(), Change::Added)]);
        write(&root.join("instructions/x.md"), "x\n");
        assert!(invalid(r.restore(&[ItemRef::Manifest, item("rule:x")])).contains("commit it first"));
        assert!(root.join("instructions/x.md").is_file());
    }

    /// Metadata HEAD can't read doesn't block its own repair: a commit that
    /// replaces it (a corrected manifest, a rule with its frontmatter fixed)
    /// goes through, its tree checked strictly; one that leaves it is
    /// refused, and so is a commit whose own metadata is unreadable.
    #[test]
    fn a_commit_can_replace_metadata_head_cannot_read() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        let repo = Repo::init_committed(&root).unwrap();
        git(&root, &["config", "user.name", "T"]);
        git(&root, &["config", "user.email", "t@example.invalid"]);
        repo.create_item("rule", "style", "Style").unwrap();
        repo.commit(&[item("rule:style")], "style", None).unwrap();
        let manifest = read(&root.join("sk1ll.toml"));
        let rule = read(&root.join("instructions/style.md"));
        write(&root.join("sk1ll.toml"), "[import.web\n");
        write(&root.join("instructions/style.md"), &rule.replacen("id:", "id: [", 1));
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "broken by hand"]);

        // The tree it makes is checked strictly: fixing only the manifest
        // leaves the rule HEAD can't read in it.
        write(&root.join("sk1ll.toml"), &manifest);
        write(&root.join("instructions/style.md"), &rule);
        let error = repo.commit(&[item("manifest")], "fix the manifest", None).unwrap_err().to_string();
        assert!(error.contains("instructions/style.md"), "{error}");
        repo.create_item("rule", "git", "Git").unwrap();
        let error = repo.commit(&[item("rule:git")], "git", None).unwrap_err().to_string();
        assert!(error.contains("sk1ll.toml"), "{error}");
        repo.commit(&[item("manifest"), item("rule:style"), item("rule:git")], "fix both", None).unwrap();
        assert!(repo.draft().unwrap().is_empty());
        assert!(crate::item_id::claims_at(repo.root(), "HEAD", |_| true, true).is_ok());

        // What it commits must be readable.
        write(&root.join("sk1ll.toml"), "[import.web\n");
        assert!(repo.commit(&[item("manifest")], "broken again", None).is_err());
    }

    #[test]
    fn commit_refuses_a_detached_head() {
        let f = Fleet::new();
        let r = repo(&f.a);
        git(&f.a, &["checkout", "--quiet", "--detach"]);
        write(&f.a.join("personal/pdf/SKILL.md"), "changed\n");
        let before = snapshot(&f.a);
        assert!(invalid(r.commit(&[item("skill:pdf")], "m", None)).contains("detached"));
        assert!(invalid(r.push()).contains("detached"));
        assert_eq!(snapshot(&f.a), before);
    }

    #[test]
    fn restore_refuses_to_delete_ignored_content() {
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
        assert!(invalid(r.restore(&[item("other:notes")])).contains("`notes/private.txt`"));
        assert!(invalid(r.restore(&[item("other:docs/private")])).contains("`docs`"));
        assert!(invalid(r.restore_all()).contains("ignored"));
        assert_eq!(snapshot(&f.a), before);

        // Commit doesn't touch the working tree, so the ignored files are safe.
        r.commit(&[item("other:notes")], "drop notes", None).unwrap();
        assert_eq!(read(&f.a.join("notes/private.txt")), "secret\n");
        // Once the ignored file is moved away, restore goes ahead.
        fs::remove_file(f.a.join("docs")).unwrap();
        r.restore(&[item("other:docs/private")]).unwrap();
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

        r.restore(&[bad_item]).unwrap();
        assert!(!f.a.join(bad).exists());
        assert_eq!(read(&f.a.join(literal)), "y\n");
        r.restore(&[ItemRef::resolve(&literal_item.to_string(), &r.draft().unwrap()).unwrap()]).unwrap();
        assert!(r.draft().unwrap().is_empty());
    }

    #[test]
    fn pull_rebases_without_touching_the_draft_or_author_metadata() {
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        {
            let _date = crate::git::test_env(&[("GIT_AUTHOR_DATE", "1700000000 +0230")]);
            write(&f.b.join("instructions/style.md"), "b committed\n");
            b.commit(&[item("rule:style")], "Style\n\nKeep this body.\n", Some("Éditor <editor@example.com>")).unwrap();
            write(&f.b.join("personal/local/SKILL.md"), "b committed too\n");
            b.commit(&[item("skill:local")], "Local skill", Some("Other Author <other@example.com>")).unwrap();
        }
        let authors_and_messages =
            |range: &str| Git::new(&f.b).args(["log", "--format=%an <%ae> %ai%n%B", "--reverse", range]).run().unwrap();
        let original = authors_and_messages("origin/main..HEAD");
        let head = git(&f.b, &["rev-parse", "HEAD"]);
        write(&f.a.join("imported/web/SKILL.md"), "web v2\n");
        let remote = a.commit(&[item("skill:web")], "web v2", None).unwrap();
        a.push().unwrap();

        let tracked = f.b.join("instructions/git.md");
        let untracked = f.b.join("personal/new/SKILL.md");
        write(&tracked, "staged\n");
        git(&f.b, &["add", "instructions/git.md"]);
        write(&tracked, "unstaged\n");
        write(&untracked, "untracked\n");
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        for path in [&tracked, &untracked, &f.b.join("imported/web/SKILL.md")] {
            fs::File::options().write(true).open(path).unwrap().set_modified(old).unwrap();
        }
        let check_draft = || {
            assert_eq!(fs::read(&tracked).unwrap(), b"unstaged\n");
            assert_eq!(fs::read(&untracked).unwrap(), b"untracked\n");
            for path in [&tracked, &untracked] {
                assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), old);
            }
            assert_eq!(git(&f.b, &["show", ":instructions/git.md"]), "staged");
            assert_eq!(git(&f.b, &["stash", "list"]), "");
        };
        let late = f.b.join("personal/new/late.md");
        check_draft();
        let outcome = b
            .pull_at(Before::Checks, || {
                check_draft();
                assert_eq!(git(&f.b, &["rev-parse", "HEAD"]), head);
                write(&late, "saved during pull\n");
            })
            .unwrap();
        assert!(matches!(outcome, PullOutcome::Updated { from, to } if from == head && to != head));
        check_draft();
        assert_eq!(fs::read(&late).unwrap(), b"saved during pull\n");
        assert_eq!(git(&f.b, &["rev-parse", "HEAD~2"]), remote);
        assert_eq!(authors_and_messages("origin/main..HEAD"), original);
        assert_eq!(git(&f.b, &["log", "-1", "--format=%cn", "HEAD"]), machine::hostname());
        assert_eq!(read(&f.b.join("imported/web/SKILL.md")), "web v2\n");
    }

    #[test]
    fn pull_leaves_an_identical_stash_in_place() {
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        // a: an unpushed commit, so the pull rebases; b moves the baseline.
        write(&f.a.join("instructions/style.md"), "a committed\n");
        a.commit(&[item("rule:style")], "style", None).unwrap();
        write(&f.b.join("imported/web/SKILL.md"), "web v2\n");
        b.commit(&[item("skill:web")], "web v2", None).unwrap();
        b.push().unwrap();
        write(&f.a.join("instructions/git.md"), "a draft\n");
        write(&f.a.join("notes.txt"), "untracked\n");
        git(&f.a, &["stash", "push", "--include-untracked", "--quiet", "-m", "sk1ll pull: draft"]);
        git(&f.a, &["stash", "apply", "--index", "--quiet"]);

        let stashes = || git(&f.a, &["stash", "list", "--format=%H %gs"]);
        let before = stashes();
        assert!(matches!(pull_now(&a).unwrap(), PullOutcome::Updated { .. }));
        assert_eq!(stashes(), before);
        assert_eq!(read(&f.a.join("instructions/git.md")), "a draft\n");
        assert_eq!(read(&f.a.join("notes.txt")), "untracked\n");
    }

    #[test]
    fn pull_drops_commits_that_become_empty() {
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        for root in [&f.a, &f.b] {
            write(&root.join("personal/pdf/SKILL.md"), "v2\n");
            // The same commit on both: with the same Item ID too.
            write(&root.join("personal/pdf/.sk1ll-id"), "pdf-id\n");
            repo(root).commit(&[item("skill:pdf")], "pdf v2", None).unwrap();
        }
        git(
            &f.b,
            &[
                "-c",
                "user.name=Machine B",
                "-c",
                "user.email=b@example.com",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "originally empty",
            ],
        );
        a.push().unwrap();

        assert!(matches!(pull_now(&b).unwrap(), PullOutcome::Updated { .. }));
        assert_eq!(git(&f.b, &["log", "--format=%s", "origin/main..HEAD"]), "originally empty");
        assert!(b.draft().unwrap().is_empty());
    }

    #[test]
    fn pull_refuses_unpushed_merge_commits() {
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        git(&f.a, &["checkout", "--quiet", "-b", "topic"]);
        write(&f.a.join("instructions/style.md"), "topic\n");
        commit_all(&f.a, "topic");
        git(&f.a, &["checkout", "--quiet", "main"]);
        git(&f.a, &["merge", "--quiet", "--no-ff", "topic"]);
        write(&f.b.join("imported/web/SKILL.md"), "web v2\n");
        b.commit(&[item("skill:web")], "web v2", None).unwrap();
        b.push().unwrap();

        let before = snapshot(&f.a);
        assert!(invalid(pull_now(&a)).contains("merge commits among the unpushed commits"));
        assert_eq!(snapshot(&f.a), before);
    }

    #[test]
    fn pull_refuses_unrelated_histories_without_touching_anything() {
        // Two machines joined an empty server and each made its own root commit.
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
            let message = invalid(pull_now(&r));
            assert!(message.contains("unrelated histories"), "{message}");
            assert_eq!(snapshot(&c), before);
        }
        assert_eq!(items(&r), [("rule:git".to_string(), Change::Added)]);
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
        for message in [invalid(pull_now(&r)), invalid(r.push()), invalid(r.commit(&[item("skill:pdf")], "m", None))] {
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
        let message = invalid(pull_now(&r));
        assert!(message.contains("no `main` branch") && message.contains("trunk"), "{message}");
        assert_eq!(snapshot(&c), before);

        // An empty origin (an older server): nothing to pull, and the first push creates `main`.
        let empty = f._tmp.path().join("empty.git");
        git(f._tmp.path(), &["init", "--quiet", "--bare", "-b", "main", empty.to_str().unwrap()]);
        git(&c, &["remote", "set-url", REMOTE, empty.to_str().unwrap()]);
        assert_eq!(pull_now(&r).unwrap(), PullOutcome::UpToDate);
        r.push().unwrap();
        assert_eq!(origin_branches(&empty), "refs/heads/main");
        assert_eq!(pull_now(&r).unwrap(), PullOutcome::UpToDate);
    }

    /// Config that would make git stash the draft, squash or run hooks
    /// doesn't reach pull's switch.
    #[test]
    fn pull_ignores_autostash_merge_options_and_hooks() {
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        git(&f.b, &["config", "merge.autoStash", "true"]);
        git(&f.b, &["config", "rebase.autoStash", "true"]);
        git(&f.b, &["config", "branch.main.mergeOptions", "--squash"]);
        write(&f.b.join("notes.txt"), "staged new\n");
        write(&f.b.join("instructions/git.md"), "staged\n");
        git(&f.b, &["add", "notes.txt", "instructions/git.md"]);
        write(&f.b.join("instructions/git.md"), "unstaged\n");
        let draft = b.draft().unwrap();
        #[cfg(unix)]
        for hook in ["post-checkout", "post-merge", "reference-transaction"] {
            let path = f.b.join(".git/hooks").join(hook);
            write(&path, "#!/bin/sh\ntouch hook-ran\nexit 1\n");
            chmod(&path, 0o755);
        }
        write(&f.a.join("personal/pdf/SKILL.md"), "v2\n");
        let remote = a.commit(&[item("skill:pdf")], "v2", None).unwrap();
        a.push().unwrap();
        let old = git(&f.b, &["rev-parse", "HEAD"]);

        let staged_new = || assert_eq!(git(&f.b, &["show", ":notes.txt"]), "staged new");
        let outcome = b.pull_at(Before::UpdateRef, staged_new).unwrap();
        assert_eq!(outcome, PullOutcome::Updated { from: old, to: remote.clone() });
        assert_eq!(git(&f.b, &["rev-parse", "HEAD"]), remote);
        assert_eq!(git(&f.b, &["symbolic-ref", "HEAD"]), "refs/heads/main");
        assert_eq!(read(&f.b.join("personal/pdf/SKILL.md")), "v2\n");
        assert_eq!(b.draft().unwrap(), draft);
        assert_eq!(git(&f.b, &["diff", "--cached", "--name-only"]), "instructions/git.md\nnotes.txt");
        assert_eq!(git(&f.b, &["show", ":instructions/git.md"]), "staged");
        assert_eq!(read(&f.b.join("instructions/git.md")), "unstaged\n");
        assert_eq!(git(&f.b, &["stash", "list"]), "");
        assert!(!f.b.join("hook-ran").exists());
    }

    #[test]
    fn pull_never_writes_into_or_replaces_a_staging_directory() {
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        write(&f.b.join(".git/info/exclude"), ".sk1ll-stage-*\n");
        let backup = f.b.join("imported/.sk1ll-stage-old-web/SKILL.md");
        write(&backup, "the previous web\n");
        let inside = "imported/.sk1ll-stage-old-web/extra.md";
        write(&f.a.join(inside), "upstream\n");
        git(&f.a, &["add", "--force", inside]);
        git(&f.a, &["commit", "--quiet", "-m", "into a staging directory"]);
        a.push().unwrap();
        let before = snapshot(&f.b);
        let outcome = pull_now(&b).unwrap();
        assert!(
            matches!(&outcome, PullOutcome::Blocked { items, .. } if *items == [item("skill:.sk1ll-stage-old-web")])
        );
        assert_eq!(snapshot(&f.b), before);

        // A file where a directory with an (empty) staging directory is: git
        // would remove the staging directory to make room.
        fs::remove_file(&backup).unwrap();
        fs::create_dir(f.b.join("imported/.sk1ll-stage-1-2-3")).unwrap();
        git(&f.a, &["rm", "-r", "--quiet", "imported"]);
        write(&f.a.join("imported"), "now a file\n");
        commit_all(&f.a, "imported is a file");
        a.push().unwrap();
        let outcome = pull_now(&b).unwrap();
        assert!(
            matches!(&outcome, PullOutcome::Blocked { items, .. } if items.contains(&item("other:imported"))),
            "{outcome:?}"
        );
        assert!(f.b.join("imported/.sk1ll-stage-1-2-3").is_dir());
        assert!(f.b.join("imported/.sk1ll-stage-old-web").is_dir());
    }

    /// The skills repo can be a subdirectory of a larger worktree. Checkout
    /// updates all of it, so pull checks all of it.
    #[test]
    fn pull_checks_the_whole_worktree_around_a_skills_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        let path = |p: &str| tmp.path().join(p);
        let origin = path("origin.git");
        let (a, b) = (path("a"), path("b"));
        git(tmp.path(), &["init", "--quiet", "--bare", "-b", "main", origin.to_str().unwrap()]);
        git(tmp.path(), &["init", "--quiet", "-b", "main", a.to_str().unwrap()]);
        git(&a, &["config", "user.name", "Machine A"]);
        git(&a, &["config", "user.email", "a@example.com"]);
        write(&a.join("skills/sk1ll.toml"), "");
        write(&a.join("skills/personal/pdf/SKILL.md"), "---\nname: pdf\n---\nPDFs.\n");
        write(&a.join("README.md"), "the larger project\n");
        commit_all(&a, "seed");
        git(&a, &["remote", "add", REMOTE, origin.to_str().unwrap()]);
        git(&a, &["push", "--quiet", "-u", REMOTE, "main"]);
        git(tmp.path(), &["clone", "--quiet", origin.to_str().unwrap(), b.to_str().unwrap()]);
        let r = repo(&b.join("skills"));

        // An ignored file next to the skills repo, where upstream adds one.
        write(&a.join("notes.txt"), "upstream\n");
        commit_all(&a, "notes");
        git(&a, &["push", "--quiet", REMOTE, "main"]);
        let remote = git(&a, &["rev-parse", "HEAD"]);
        write(&b.join(".git/info/exclude"), "/notes.txt\n");
        write(&b.join("notes.txt"), "mine\n");
        let before = snapshot(&b);
        let blocked = |items: &[&str]| PullOutcome::Blocked {
            baseline: git(&a, &["rev-parse", "HEAD"]),
            items: items.iter().map(|i| item(i)).collect(),
            uncommitted: Vec::new(),
        };
        assert_eq!(pull_now(&r).unwrap(), blocked(&["other:../notes.txt"]));
        // Also with an unpushed commit, so that pull rebases.
        write(&b.join("skills/personal/pdf/SKILL.md"), "b committed\n");
        r.commit(&[item("skill:pdf")], "pdf from b", Some("B <b@example.com>")).unwrap();
        let before_rebase = snapshot(&b);
        assert_eq!(pull_now(&r).unwrap(), blocked(&["other:../notes.txt"]));
        assert_eq!(snapshot(&b), before_rebase);
        assert_eq!(read(&b.join("notes.txt")), "mine\n");
        assert_ne!(before, before_rebase);

        // A local change outside the skills repo to a file upstream changes.
        fs::remove_file(b.join("notes.txt")).unwrap();
        write(&a.join("README.md"), "upstream README\n");
        commit_all(&a, "readme");
        git(&a, &["push", "--quiet", REMOTE, "main"]);
        write(&b.join("README.md"), "b's README\n");
        assert!(r.draft().unwrap().is_empty());
        let before = snapshot(&b);
        assert_eq!(pull_now(&r).unwrap(), blocked(&["other:../README.md"]));
        assert_eq!(snapshot(&b), before);

        git(&b, &["checkout", "--quiet", "--", "README.md"]);
        assert!(matches!(pull_now(&r).unwrap(), PullOutcome::Updated { .. }));
        assert_eq!(read(&b.join("notes.txt")), "upstream\n");
        assert_eq!(read(&b.join("README.md")), "upstream README\n");
        assert_eq!(read(&b.join("skills/personal/pdf/SKILL.md")), "b committed\n");
        assert_eq!(git(&b, &["rev-parse", "HEAD~1"]), git(&a, &["rev-parse", "HEAD"]));
        assert_ne!(remote, git(&a, &["rev-parse", "HEAD"]));
    }

    #[test]
    fn pull_refuses_submodules() {
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        let seed = git(&f.a, &["rev-parse", "HEAD"]);
        let gitlink = format!("160000,{seed},vendor/lib");
        let as_b = ["-c", "user.name=Machine B", "-c", "user.email=b@example.com"];
        // Only in the index, with nothing to pull, and with something.
        git(&f.b, &["update-index", "--add", "--cacheinfo", &gitlink]);
        let before = snapshot(&f.b);
        assert!(invalid(pull_now(&b)).contains("submodules aren't supported"));
        assert_eq!(snapshot(&f.b), before);
        write(&f.a.join("instructions/git.md"), "git v2\n");
        a.commit(&[item("rule:git")], "git v2", None).unwrap();
        a.push().unwrap();
        assert!(invalid(pull_now(&b)).contains("submodules aren't supported"));
        assert_eq!(snapshot(&f.b), before);
        git(&f.b, &["update-index", "--force-remove", "vendor/lib"]);
        assert!(matches!(pull_now(&b).unwrap(), PullOutcome::Updated { .. }));
        // In HEAD.
        git(&f.b, &["update-index", "--add", "--cacheinfo", &gitlink]);
        git(&f.b, &[&as_b[..], &["commit", "--quiet", "-m", "submodule"]].concat());
        let before = snapshot(&f.b);
        assert!(invalid(pull_now(&b)).contains("submodules aren't supported"));
        assert_eq!(snapshot(&f.b), before);
        write(&f.a.join("personal/pdf/SKILL.md"), "v2\n");
        a.commit(&[item("skill:pdf")], "v2", None).unwrap();
        a.push().unwrap();
        let before = snapshot(&f.b);
        assert!(invalid(pull_now(&b)).contains("submodules aren't supported"));
        assert_eq!(snapshot(&f.b), before);

        // In the new baseline, even with config that hides it or recurses into it.
        git(&f.b, &["reset", "--quiet", "--hard", "HEAD~1"]);
        git(&f.b, &["config", "submodule.recurse", "true"]);
        git(&f.b, &["config", "diff.ignoreSubmodules", "all"]);
        git(&f.a, &["update-index", "--add", "--cacheinfo", &gitlink]);
        git(&f.a, &["commit", "--quiet", "-m", "submodule"]);
        a.push().unwrap();
        let before = snapshot(&f.b);
        assert!(invalid(pull_now(&b)).contains("submodules aren't supported"));
        assert_eq!(snapshot(&f.b), before);

        // Adding and removing a gitlink in the same range is unsupported too.
        git(&f.a, &["rm", "--cached", "--quiet", "vendor/lib"]);
        git(&f.a, &["commit", "--quiet", "-m", "no submodule"]);
        a.push().unwrap();
        assert!(invalid(pull_now(&b)).contains("submodules aren't supported"));
        assert_eq!(snapshot(&f.b), before);
    }

    #[test]
    fn pull_refuses_sparse_checkouts() {
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        write(&f.a.join("personal/pdf/SKILL.md"), "v2\n");
        a.commit(&[item("skill:pdf")], "v2", None).unwrap();
        a.push().unwrap();
        let before = snapshot(&f.b);
        git(&f.b, &["config", "core.sparseCheckout", "true"]);
        assert!(invalid(pull_now(&b)).contains("sparse checkouts aren't supported"));
        assert_eq!(snapshot(&f.b), before);
        git(&f.b, &["config", "--unset", "core.sparseCheckout"]);
        assert!(matches!(pull_now(&b).unwrap(), PullOutcome::Updated { .. }));
        assert_eq!(read(&f.b.join("personal/pdf/SKILL.md")), "v2\n");
    }

    /// The conflict is named even when the unpushed commits end up where
    /// they started (an edit, then its revert), and both turn back into Draft.
    #[test]
    fn pull_names_the_items_a_replayed_commit_conflicts_in() {
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        write(&f.b.join("instructions/style.md"), "b's style\n");
        b.commit(&[item("rule:style")], "style", None).unwrap();
        write(&f.b.join("instructions/style.md"), "Be terse.\n");
        b.commit(&[item("rule:style")], "revert style", None).unwrap();
        write(&f.a.join("instructions/style.md"), "a's style\n");
        let remote = a.commit(&[item("rule:style")], "style", None).unwrap();
        a.push().unwrap();
        // Both commits turn back into Draft: the first conflicts, and the
        // second is built on it. What they leave (the rule's new Item ID)
        // is in the way as Draft.
        let base = git(&f.b, &["rev-parse", "HEAD~2"]);
        let (files, ..) = snapshot(&f.b);
        let outcome = pull_now(&b).unwrap();
        let PullOutcome::Blocked { baseline, items, uncommitted } = outcome else { panic!("{outcome:?}") };
        assert_eq!((baseline, items), (remote, vec![item("rule:style")]));
        assert_eq!(uncommitted.len(), 2, "{uncommitted:?}");
        assert_eq!(git(&f.b, &["rev-parse", "HEAD"]), base);
        assert_eq!(snapshot(&f.b).0, files);
    }

    /// A replayed commit is the original object with a new tree, parent and
    /// committer, minus its signature.
    #[test]
    fn pull_replays_commits_byte_for_byte() {
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        write(&f.b.join("instructions/style.md"), "b committed\n");
        git(&f.b, &["add", "instructions/style.md"]);
        let (tree, parent) = (git(&f.b, &["write-tree"]), git(&f.b, &["rev-parse", "HEAD"]));
        let author = b"author Caf\xe9 Author <c@example.com> 1700000000 +0230\n".as_slice();
        let kept = [author, b"encoding ISO-8859-1\nx-extra kept\n\nCaf\xe9\n\nBody \xe9.\n"].concat();
        let original = [
            format!("tree {tree}\nparent {parent}\n").as_bytes(),
            author,
            b"committer Old <old@example.com> 1700000001 +0100\nencoding ISO-8859-1\n",
            b"gpgsig -----BEGIN PGP SIGNATURE-----\n \n c2lnbmF0dXJl\n -----END PGP SIGNATURE-----\n",
            b"gpgsig-sha256 -----BEGIN PGP SIGNATURE-----\n c2lnbmF0dXJl\n -----END PGP SIGNATURE-----\n",
            b"x-extra kept\n\nCaf\xe9\n\nBody \xe9.\n",
        ]
        .concat();
        let commit = Git::new(&f.b).args(["hash-object", "-t", "commit", "-w", "--stdin"]).stdin(original);
        git(&f.b, &["update-ref", "refs/heads/main", &commit.text().unwrap()]);
        write(&f.a.join("imported/web/SKILL.md"), "web v2\n");
        let remote = a.commit(&[item("skill:web")], "web v2", None).unwrap();
        a.push().unwrap();

        assert!(matches!(pull_now(&b).unwrap(), PullOutcome::Updated { .. }));
        let replayed = Git::new(&f.b).args(["cat-file", "commit", "HEAD"]).run().unwrap();
        let (header, rest) = replayed.split_at(replayed.windows(2).position(|w| w == b"\n\n").unwrap() + 1);
        let lines: Vec<&[u8]> = header.split_inclusive(|b| *b == b'\n').collect();
        assert_eq!(lines[0], format!("tree {}\n", git(&f.b, &["rev-parse", "HEAD^{tree}"])).as_bytes());
        assert_eq!(lines[1], format!("parent {remote}\n").as_bytes());
        assert!(lines[3].starts_with(format!("committer {} <", machine::hostname()).as_bytes()));
        assert_eq!([lines[2], &lines[4..].concat(), rest].concat(), kept);
        assert_eq!(git(&f.b, &["show", "HEAD:instructions/style.md"]), "b committed");
    }

    /// Upstream changes for the tests of pulls that stop half-way: in pull's
    /// order, a removal, then a modification, an addition and an
    /// executable change in three items. `b` gets a draft pull carries:
    /// a staged new file, a staged and then edited rule, an untracked
    /// skill. Returns the new baseline.
    fn half_way_setup(f: &Fleet) -> String {
        fs::remove_dir_all(f.a.join("imported/web")).unwrap();
        write(&f.a.join("instructions/git.md"), "git v2\n");
        write(&f.a.join("personal/pdf/SKILL.md"), "---\nname: pdf\n---\nPDFs v2.\n");
        write(&f.a.join("personal/pdf/extra.md"), "added\n");
        #[cfg(unix)]
        chmod(&f.a.join("personal/pdf/ref.md"), 0o755);
        commit_all(&f.a, "upstream");
        git(&f.a, &["push", "--quiet", REMOTE, "main"]);
        write(&f.b.join("notes.txt"), "staged\n");
        write(&f.b.join("instructions/style.md"), "staged style\n");
        git(&f.b, &["add", "notes.txt", "instructions/style.md"]);
        write(&f.b.join("instructions/style.md"), "unstaged style\n");
        write(&f.b.join("personal/mine/SKILL.md"), "untracked\n");
        git(&f.a, &["rev-parse", "HEAD"])
    }

    fn assert_at_upstream(f: &Fleet) {
        assert!(!f.b.join("imported/web").exists());
        assert_eq!(read(&f.b.join("instructions/git.md")), "git v2\n");
        assert_eq!(read(&f.b.join("personal/pdf/SKILL.md")), "---\nname: pdf\n---\nPDFs v2.\n");
        assert_eq!(read(&f.b.join("personal/pdf/extra.md")), "added\n");
        assert!(!cfg!(unix) || executable(&f.b.join("personal/pdf/ref.md")));
        assert_eq!(git(&f.b, &["show", ":notes.txt"]), "staged");
        assert_eq!(git(&f.b, &["show", ":instructions/style.md"]), "staged style");
        assert_eq!(read(&f.b.join("instructions/style.md")), "unstaged style\n");
        assert_eq!(read(&f.b.join("personal/mine/SKILL.md")), "untracked\n");
        assert_eq!(head_ref(&f.b), "refs/heads/main");
        let anchor = Git::new(&f.b).args(["rev-parse", "--verify", "--quiet", "refs/worktree/sk1ll-pull"]);
        assert!(!anchor.succeeds().unwrap(), "the target's anchor is gone with the journal");
        assert_eq!(crate::sync::displaced::changed().unwrap(), []);
    }

    /// A pull that stops anywhere once it has written its journal (killed,
    /// say) is finished by the next pull, or by [`Repo::finish_pull`] (the
    /// daemon's refresh), in a new process: nothing of it is only in memory.
    #[test]
    fn a_pull_that_stopped_half_way_is_finished_later() {
        let points = [
            Before::File(0),
            Before::File(2),
            Before::Swap,
            Before::Published,
            Before::Index,
            Before::UpdateRef,
            Before::Done,
        ];
        for point in points {
            for (rebase, by_pull) in [(false, true), (false, false), (true, true), (true, false)] {
                let case =
                    format!("{point:?}, rebase {rebase}, finished by {}", if by_pull { "pull" } else { "refresh" });
                let f = Fleet::new();
                let b = repo(&f.b);
                let remote = half_way_setup(&f);
                if rebase {
                    write(&f.b.join("sk1ll.toml"), "# b committed\n");
                    b.commit(&[ItemRef::Manifest], "manifest from b", None).unwrap();
                }
                let old = git(&f.b, &["rev-parse", "HEAD"]);
                let draft = b.draft().unwrap();
                let crashed = b.pull_crashing_at(point).unwrap_err().to_string();
                assert!(crashed.contains("crashed"), "{case}: {crashed}");
                assert!(pending(&b), "{case}");
                assert_eq!(head_ref(&f.b), "refs/heads/main", "{case}");
                let anchor = git(&f.b, &["rev-parse", "refs/worktree/sk1ll-pull"]);

                let b = repo(&f.b);
                let outcome = if by_pull { pull_now(&b).unwrap() } else { finish_now(&b).unwrap().unwrap() };
                let target = git(&f.b, &["rev-parse", "refs/heads/main"]);
                assert_eq!(outcome, PullOutcome::Updated { from: old, to: target.clone() }, "{case}");
                assert_eq!(anchor, target, "{case}: the anchor kept the target's objects");
                assert_eq!(git(&f.b, &["rev-parse", if rebase { "HEAD~1" } else { "HEAD" }]), remote, "{case}");
                assert!(!pending(&b), "{case}");
                assert_at_upstream(&f);
                assert_eq!(b.draft().unwrap(), draft, "{case}");
                assert_eq!(finish_now(&b).unwrap(), None, "{case}");
                assert_eq!(pull_now(&b).unwrap(), PullOutcome::UpToDate, "{case}");
            }
        }
    }

    /// A save that lands while pull replaces a file is never lost: the swap
    /// stands and the save is kept in the recovery area, from where it can
    /// be adopted back as a draft change.
    #[cfg(unix)]
    #[test]
    fn a_save_during_a_file_swap_is_kept_in_recovery() {
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        let path = f.b.join("personal/pdf/SKILL.md");
        type Save<'a> = (&'a str, Box<dyn Fn() + 'a>);
        let mut saves: Vec<Save> = vec![(
            "saved in place\n",
            Box::new(|| {
                // Through the file being replaced, as most editors save.
                let mut file = fs::OpenOptions::new().write(true).truncate(true).open(&path).unwrap();
                std::io::Write::write_all(&mut file, b"saved in place\n").unwrap();
            }),
        )];
        // A new file renamed into place: only an exchange brings it back.
        #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
        saves.push((
            "saved by rename\n",
            Box::new(|| {
                let tmp = f.b.join("personal/pdf/.SKILL.md.swp");
                fs::write(&tmp, "saved by rename\n").unwrap();
                fs::rename(&tmp, &path).unwrap();
            }),
        ));
        for (n, (saved, save)) in saves.into_iter().enumerate() {
            let upstream = format!("v{n}\n");
            write(&f.a.join("personal/pdf/SKILL.md"), &upstream);
            let remote = a.commit(&[item("skill:pdf")], "pdf", None).unwrap();
            a.push().unwrap();
            let old = git(&f.b, &["rev-parse", "HEAD"]);
            match b.pull_at(Before::Swap, save) {
                Err(Error::Displaced { kept, id, .. }) => {
                    assert_eq!(read(&kept), saved);
                    let entry = crate::sync::displaced::get(&id).unwrap();
                    assert!(entry.changed && entry.kind == crate::sync::displaced::Kind::Draft, "{entry:?}");
                    assert_eq!(entry.dest, f.b.canonicalize().unwrap().join("personal/pdf/SKILL.md"));
                    // The pull is done all the same.
                    assert_eq!(read(&path), upstream);
                    assert_eq!(git(&f.b, &["rev-parse", "HEAD"]), remote);
                    assert_ne!(remote, old);
                    assert!(!pending(&b) && b.draft().unwrap().is_empty());
                    crate::sync::displaced::adopt(&b, &id, &crate::EngineLock::acquire(b.root()).unwrap()).unwrap();
                    assert_eq!(read(&path), saved);
                    assert_eq!(items(&b), [("skill:pdf".into(), Change::Modified)]);
                    b.restore(&[item("skill:pdf")]).unwrap();
                }
                other => panic!("expected the save to be displaced, got {other:?}"),
            }
        }
        assert!(
            crate::sync::displaced::changed().unwrap().is_empty(),
            "adopted entries are gone; the content adopt replaced was the baseline's"
        );
    }

    /// A draft change that appears, after the last checks, in a path pull
    /// has yet to update stops it there as Blocked. The pull already updated
    /// other paths, so its journal stays until that item is committed or
    /// restored; then the next pull finishes.
    #[test]
    fn a_draft_change_after_the_checks_blocks_the_rest_of_the_pull() {
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        write(&f.a.join("imported/web/SKILL.md"), "web v2\n");
        write(&f.a.join("personal/pdf/SKILL.md"), "pdf v2\n");
        commit_all(&f.a, "web and pdf");
        a.push().unwrap();
        let remote = git(&f.a, &["rev-parse", "HEAD"]);
        let old = git(&f.b, &["rev-parse", "HEAD"]);
        let pdf = f.b.join("personal/pdf/SKILL.md");
        let outcome = b.pull_at(Before::File(1), || write(&pdf, "draft edit\n")).unwrap();
        assert_eq!(
            outcome,
            PullOutcome::Blocked { baseline: remote.clone(), items: vec![item("skill:pdf")], uncommitted: Vec::new() }
        );
        assert_eq!(read(&pdf), "draft edit\n");
        assert_eq!(read(&f.b.join("imported/web/SKILL.md")), "web v2\n");
        assert!(pending(&b));
        assert_eq!((head_ref(&f.b), git(&f.b, &["rev-parse", "HEAD"])), ("refs/heads/main".into(), old.clone()));
        // Still Blocked, and nothing else can move the branch meanwhile.
        assert_eq!(finish_now(&b).unwrap().unwrap(), outcome);
        assert!(matches!(b.commit(&[item("skill:pdf")], "m", None), Err(Error::PullPending)));
        // Nor does anything write Item IDs into its partial tree, or push.
        assert!(matches!(b.push(), Err(Error::PullPending)));
        assert!(matches!(b.repair_ids(), Err(Error::PullPending)));
        assert!(matches!(b.assign_missing_ids(), Err(Error::PullPending)));

        b.restore(&[item("skill:pdf")]).unwrap();
        assert_eq!(pull_now(&b).unwrap(), PullOutcome::Updated { from: old, to: remote });
        assert_eq!(read(&pdf), "pdf v2\n");
        assert!(!pending(&b) && b.draft().unwrap().is_empty());
    }

    /// Item-level protection ends at the last checks: a save after them to
    /// a file of a changed item that the new baseline leaves alone is no
    /// clash, and stays as a draft change on top of it.
    #[test]
    fn a_save_to_an_untouched_file_of_a_changed_item_stays_in_the_draft() {
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        write(&f.a.join("personal/pdf/SKILL.md"), "pdf v2\n");
        let remote = a.commit(&[item("skill:pdf")], "pdf", None).unwrap();
        a.push().unwrap();
        let sibling = f.b.join("personal/pdf/ref.md");
        let outcome = b.pull_at(Before::File(0), || write(&sibling, "saved after the checks\n")).unwrap();
        assert!(matches!(outcome, PullOutcome::Updated { to, .. } if to == remote));
        assert_eq!(read(&f.b.join("personal/pdf/SKILL.md")), "pdf v2\n");
        assert_eq!(read(&sibling), "saved after the checks\n");
        assert_eq!(items(&b), [("skill:pdf".into(), Change::Modified)]);
    }

    /// Another Git process holding the index lock is an error, with git's
    /// message, never Blocked. Before the journal, nothing changes; after
    /// the files, the next pull finishes.
    #[test]
    fn an_index_lock_is_an_error_not_a_clash() {
        let f = Fleet::new();
        let b = repo(&f.b);
        let remote = half_way_setup(&f);
        let old = git(&f.b, &["rev-parse", "HEAD"]);
        let lock = f.b.join(".git/index.lock");
        write(&lock, "");
        let before = snapshot(&f.b);
        match pull_now(&b) {
            Err(Error::Git { stderr, .. }) => assert!(stderr.contains("index.lock"), "{stderr}"),
            other => panic!("expected git's error, got {other:?}"),
        }
        assert_eq!(snapshot(&f.b), before);
        assert!(!pending(&b));
        fs::remove_file(&lock).unwrap();

        match b.pull_at(Before::Index, || write(&lock, "")) {
            Err(Error::Git { args, stderr }) => {
                assert!(args.contains("read-tree") && stderr.contains("index.lock"), "{args}: {stderr}")
            }
            other => panic!("expected git's error, got {other:?}"),
        }
        assert!(pending(&b));
        fs::remove_file(&lock).unwrap();
        assert_eq!(pull_now(&b).unwrap(), PullOutcome::Updated { from: old, to: remote });
        assert_at_upstream(&f);
    }

    /// Files are compared and written as git converts them, so a pull that
    /// stopped after writing a converted file tells it from a draft change.
    #[test]
    fn a_pull_resumes_over_converted_line_endings() {
        // Conversion from config.
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        git(&f.b, &["config", "core.autocrlf", "true"]);
        write(&f.a.join("instructions/git.md"), "git v2\n");
        write(&f.a.join("personal/pdf/SKILL.md"), "pdf v2\n");
        commit_all(&f.a, "two files");
        a.push().unwrap();
        b.pull_crashing_at(Before::File(1)).unwrap_err();
        assert_eq!(fs::read(f.b.join("instructions/git.md")).unwrap(), b"git v2\r\n");
        assert!(matches!(pull_now(&repo(&f.b)).unwrap(), PullOutcome::Updated { .. }));
        assert_eq!(fs::read(f.b.join("personal/pdf/SKILL.md")).unwrap(), b"pdf v2\r\n");
        assert_eq!(git(&f.b, &["status", "--porcelain"]), "");

        // Conversion from `.gitattributes` the pull itself adds: each side
        // is read with its own commit's, whatever the working tree has yet.
        let f = Fleet::new();
        let b = repo(&f.b);
        write(&f.a.join(".gitattributes"), "*.md text eol=crlf\n");
        write(&f.a.join("instructions/git.md"), "git v2\n");
        write(&f.a.join("personal/pdf/SKILL.md"), "pdf v2\n");
        commit_all(&f.a, "crlf");
        git(&f.a, &["push", "--quiet", REMOTE, "main"]);
        b.pull_crashing_at(Before::File(2)).unwrap_err();
        assert_eq!(fs::read(f.b.join("instructions/git.md")).unwrap(), b"git v2\r\n");
        assert_eq!(fs::read(f.b.join("personal/pdf/SKILL.md")).unwrap(), b"---\nname: pdf\n---\nPDFs.\n");
        assert!(matches!(pull_now(&repo(&f.b)).unwrap(), PullOutcome::Updated { .. }));
        assert_eq!(fs::read(f.b.join("personal/pdf/SKILL.md")).unwrap(), b"pdf v2\r\n");
        assert_eq!(git(&f.b, &["status", "--porcelain"]), "");
    }

    /// Configures the filter driver `name` in `b`, whose smudge upper-cases
    /// `v2` and whose clean lower-cases it again, each noting in the
    /// returned log file that it ran.
    #[cfg(unix)]
    fn logged_driver(b: &Path, name: &str) -> PathBuf {
        let log = b.join(".git/driver-ran");
        git(b, &["config", &format!("filter.{name}.clean"), &format!("echo >>{}; sed s/V2/v2/", log.display())]);
        git(b, &["config", &format!("filter.{name}.smudge"), &format!("echo >>{}; sed s/v2/V2/", log.display())]);
        git(b, &["config", &format!("filter.{name}.required"), "true"]);
        log
    }

    /// A Baseline that writes a file through a filter driver (Git LFS, say:
    /// the server has no LFS endpoint) is refused before the pull changes
    /// anything, wherever the attribute comes from, and no driver runs.
    #[cfg(unix)]
    #[test]
    fn a_pull_through_a_filter_driver_is_refused_and_runs_no_driver() {
        let refused = |pulled: Result<PullOutcome>, want: &[&str]| match pulled {
            Err(Error::FilterDriver { paths, items, .. }) => {
                assert_eq!(paths, want.iter().map(PathBuf::from).collect::<Vec<_>>());
                assert!(items.contains(&item("skill:pdf")), "{items:?}");
            }
            other => panic!("not refused: {other:?}"),
        };
        // From the Baseline's own `.gitattributes`.
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        let log = logged_driver(&f.b, "up");
        write(&f.a.join(".gitattributes"), "*.md filter=up\n");
        write(&f.a.join("personal/pdf/SKILL.md"), "pdf v2\n");
        commit_all(&f.a, "filtered");
        a.push().unwrap();
        let before = git(&f.b, &["rev-parse", "HEAD"]);
        let pulled = pull_now(&b);
        let message = pulled.as_ref().map(|_| String::new()).unwrap_or_else(|e| e.to_string());
        assert!(message.contains("remove `filter=` from .gitattributes"), "{message}");
        refused(pulled, &["personal/pdf/SKILL.md"]);
        assert!(!log.exists(), "a filter driver ran");
        assert!(!b.pull_pending().unwrap(), "not started");
        assert_eq!(git(&f.b, &["rev-parse", "HEAD"]), before);
        assert_eq!(read(&f.b.join("personal/pdf/SKILL.md")), "---\nname: pdf\n---\nPDFs.\n");
        assert!(!f.b.join(".gitattributes").exists());

        // From the Machine's own attributes, for a driver it doesn't even
        // have: the Baseline is the same everywhere, the drivers are not.
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        write(&f.b.join(".git/info/attributes"), "*.md filter=lfs\n");
        write(&f.a.join("personal/pdf/SKILL.md"), "pdf v2\n");
        commit_all(&f.a, "v2");
        a.push().unwrap();
        refused(pull_now(&b), &["personal/pdf/SKILL.md"]);
        fs::remove_file(f.b.join(".git/info/attributes")).unwrap();
        assert!(matches!(pull_now(&b).unwrap(), PullOutcome::Updated { .. }));
    }

    /// `git check-attr` prints `set` for `filter` set without a value (no
    /// driver) and for `filter=set`, a driver of that name. One configured
    /// under that name counts, and never runs.
    #[cfg(unix)]
    #[test]
    fn a_filter_driver_named_set_is_a_driver() {
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        let log = logged_driver(&f.b, "set");
        write(&f.a.join(".gitattributes"), "*.md filter=set\n");
        write(&f.a.join("personal/pdf/SKILL.md"), "pdf v2\n");
        commit_all(&f.a, "filtered");
        a.push().unwrap();
        assert!(matches!(pull_now(&b), Err(Error::FilterDriver { .. })));
        assert!(!log.exists(), "a filter driver ran");
        assert!(!b.pull_pending().unwrap());

        // Set without a value, with no driver called `set`: no driver.
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        write(&f.a.join(".gitattributes"), "*.md filter\n");
        write(&f.a.join("personal/pdf/SKILL.md"), "pdf v2\n");
        commit_all(&f.a, "filter set");
        a.push().unwrap();
        assert!(matches!(pull_now(&b).unwrap(), PullOutcome::Updated { .. }));
        assert_eq!(read(&f.b.join("personal/pdf/SKILL.md")), "pdf v2\n");
    }

    /// A pull that started before attributes named a driver finishes, with
    /// every driver off: no driver runs, and the other conversions (line
    /// endings here) still apply.
    #[cfg(unix)]
    #[test]
    fn a_resumed_pull_finishes_with_filter_drivers_off() {
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        write(&f.a.join("instructions/git.md"), "git v2\n");
        write(&f.a.join("personal/pdf/SKILL.md"), "pdf v2\n");
        commit_all(&f.a, "two files");
        a.push().unwrap();
        b.pull_crashing_at(Before::File(1)).unwrap_err();
        assert!(b.pull_pending().unwrap());
        let log = logged_driver(&f.b, "up");
        write(&f.b.join(".git/info/attributes"), "*.md filter=up text eol=crlf\n");
        assert!(matches!(finish_now(&b).unwrap(), Some(PullOutcome::Updated { .. })));
        assert!(!log.exists(), "a filter driver ran");
        assert!(!b.pull_pending().unwrap());
        assert_eq!(fs::read(f.b.join("personal/pdf/SKILL.md")).unwrap(), b"pdf v2\r\n");
    }

    #[cfg(unix)]
    #[test]
    fn pull_keeps_permissions_and_sets_the_executable_bit() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        let mode = |path: &str| fs::metadata(f.b.join(path)).unwrap().permissions().mode() & 0o7777;
        write(&f.a.join("personal/pdf/run.sh"), "#!/bin/sh\n");
        chmod(&f.a.join("personal/pdf/run.sh"), 0o755);
        commit_all(&f.a, "a script");
        a.push().unwrap();
        assert!(matches!(pull_now(&b).unwrap(), PullOutcome::Updated { .. }));
        assert!(executable(&f.b.join("personal/pdf/run.sh")));

        chmod(&f.b.join("personal/pdf/SKILL.md"), 0o600);
        chmod(&f.b.join("personal/pdf/run.sh"), 0o700);
        chmod(&f.b.join("instructions/git.md"), 0o640);
        assert!(b.draft().unwrap().is_empty());
        write(&f.a.join("personal/pdf/SKILL.md"), "v2\n");
        write(&f.a.join("personal/pdf/run.sh"), "#!/bin/sh\necho v2\n");
        write(&f.a.join("instructions/git.md"), "git v2\n");
        chmod(&f.a.join("instructions/git.md"), 0o755);
        commit_all(&f.a, "changes");
        a.push().unwrap();
        assert!(matches!(pull_now(&b).unwrap(), PullOutcome::Updated { .. }));
        assert_eq!(read(&f.b.join("personal/pdf/SKILL.md")), "v2\n");
        assert_eq!(mode("personal/pdf/SKILL.md"), 0o600);
        assert_eq!(mode("personal/pdf/run.sh"), 0o700);
        assert_eq!(mode("instructions/git.md"), 0o750);
        assert_eq!(git(&f.b, &["status", "--porcelain"]), "");

        chmod(&f.a.join("instructions/git.md"), 0o644);
        commit_all(&f.a, "not executable");
        a.push().unwrap();
        assert!(matches!(pull_now(&b).unwrap(), PullOutcome::Updated { .. }));
        assert_eq!(mode("instructions/git.md"), 0o640);
    }

    /// Another Git command that switches branches while pull runs stops it
    /// with an error: pull changes nothing more, on either branch. Back on
    /// the branch, the next pull finishes.
    #[test]
    fn pull_stops_when_another_command_switches_branches() {
        for (n, point) in [Before::File(1), Before::Index, Before::UpdateRef].into_iter().enumerate() {
            let f = Fleet::new();
            let b = repo(&f.b);
            let remote = half_way_setup(&f);
            let old = git(&f.b, &["rev-parse", "HEAD"]);
            let staged = git(&f.b, &["diff", "--cached", "--name-only"]);
            let other = format!("other-{n}");
            let error = invalid(b.pull_at(point, || {
                git(&f.b, &["switch", "--quiet", "-c", &other]);
            }));
            assert!(error.contains("switched") && error.contains(&format!("refs/heads/{other}")), "{error}");
            assert_eq!(head_ref(&f.b), format!("refs/heads/{other}"));
            assert_eq!(git(&f.b, &["rev-parse", "main", &other]), format!("{old}\n{old}"));
            if point == Before::Index {
                assert_eq!(git(&f.b, &["diff", "--cached", "--name-only"]), staged);
            }
            if point == Before::File(1) {
                // Only the first path was updated, before the switch.
                assert_eq!(read(&f.b.join("instructions/git.md")), "Use Conventional Commits.\n");
            }
            assert!(pending(&b));
            git(&f.b, &["switch", "--quiet", "main"]);
            assert_eq!(pull_now(&b).unwrap(), PullOutcome::Updated { from: old.clone(), to: remote });
            assert_at_upstream(&f);
            assert_eq!(git(&f.b, &["rev-parse", &other]), old);
        }
    }

    /// Another Git command that moves the branch while pull runs makes
    /// pull stop with an error, and every later pull too: it only ever
    /// moves the branch from where it was, and says what is left.
    #[test]
    fn pull_stops_when_another_command_moves_the_branch() {
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        write(&f.a.join("personal/pdf/SKILL.md"), "v2\n");
        a.commit(&[item("skill:pdf")], "v2", None).unwrap();
        a.push().unwrap();
        let head = git(&f.b, &["rev-parse", "HEAD"]);
        let tree = format!("{head}^{{tree}}");
        let moved =
            git(&f.b, &["-c", "user.name=T", "-c", "user.email=t@e", "commit-tree", "-p", &head, "-m", "x", &tree]);
        let result = b.pull_at(Before::UpdateRef, || {
            git(&f.b, &["update-ref", "refs/heads/main", &moved]);
        });
        match result {
            Err(Error::Git { args, stderr }) => {
                assert!(args.starts_with("update-ref"), "{args}");
                assert!(stderr.contains("moved refs/heads/main"), "{stderr}");
            }
            other => panic!("expected an update-ref error, got {other:?}"),
        }
        assert_eq!(head_ref(&f.b), "refs/heads/main");
        assert_eq!(git(&f.b, &["rev-parse", "HEAD"]), moved);
        assert!(pending(&b));
        let before = snapshot(&f.b);
        for result in [pull_now(&b), finish_now(&b).map(|_| PullOutcome::UpToDate)] {
            let error = invalid(result);
            assert!(error.contains("another Git command moved it") && error.contains("sk1ll-pull.json"), "{error}");
            assert_eq!(snapshot(&f.b), before);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_write_rolls_forward_on_the_next_pull() {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return; // root can write into the read-only directory below
        }
        let f = Fleet::new();
        let b = repo(&f.b);
        let remote = half_way_setup(&f);
        let old = git(&f.b, &["rev-parse", "HEAD"]);
        let draft = b.draft().unwrap();
        // The removal goes first, `instructions/git.md` next.
        chmod(&f.b.join("instructions"), 0o555);
        let result = pull_now(&b);
        chmod(&f.b.join("instructions"), 0o755);
        match result {
            Err(Error::Io { path, .. }) => assert!(path.ends_with("instructions/git.md"), "{}", path.display()),
            other => panic!("expected the write to fail, got {other:?}"),
        }
        assert!(pending(&b));
        assert!(!f.b.join("imported/web/SKILL.md").exists(), "rolled forward, never back");
        assert_eq!(git(&f.b, &["rev-parse", "HEAD"]), old);

        // A save to a path pull already updated is a draft change now.
        let web = f.b.join("imported/web/SKILL.md");
        write(&web, "saved meanwhile\n");
        let outcome = pull_now(&repo(&f.b)).unwrap();
        assert_eq!(outcome, PullOutcome::Updated { from: old, to: remote });
        assert_eq!(read(&web), "saved meanwhile\n");
        let after = b.draft().unwrap();
        assert!(after.iter().any(|d| d.item == item("skill:web")));
        for original in draft {
            assert!(after.contains(&original));
        }
    }

    /// Recovery entries of what pull replaced are in the git directory,
    /// never in the working tree, and nothing is left staged.
    #[test]
    fn pull_keeps_the_working_tree_free_of_its_own_files() {
        let f = Fleet::new();
        let b = repo(&f.b);
        half_way_setup(&f);
        assert!(matches!(pull_now(&b).unwrap(), PullOutcome::Updated { .. }));
        let names: Vec<String> = WalkDir::new(&f.b)
            .into_iter()
            .filter_entry(|e| e.file_name() != ".git")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into())
            .collect();
        assert!(names.iter().all(|n| !n.starts_with(".sk1ll")), "{names:?}");
        let entries = crate::sync::displaced::list().unwrap();
        assert!(
            !entries.is_empty() && entries.iter().all(|e| e.path.starts_with(f.b.join(".git").canonicalize().unwrap()))
        );
        assert!(!f.b.join(".git/sk1ll-pull-stage").exists());
    }
    #[cfg(unix)]
    #[test]
    fn pull_probes_exchange_before_starting() {
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        write(&f.a.join("instructions/git.md"), "new\n");
        commit_all(&f.a, "replace a rule");
        a.push().unwrap();
        let before = snapshot(&f.b);
        pull::NO_EXCHANGE.set(true);
        let result = pull_now(&b);
        pull::NO_EXCHANGE.set(false);
        assert!(invalid(result).contains("atomic file exchange"));
        assert!(!pending(&b));
        assert_eq!(snapshot(&f.b), before);
    }

    #[cfg(unix)]
    #[test]
    fn recovery_keeps_open_writers_through_prune_and_adoption() {
        use crate::sync::displaced::{self, Kind};
        use std::io::Write;
        use std::time::Duration;

        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        let rel = "instructions/git.md";
        write(&f.a.join(rel), "new rule\n");
        write(&f.a.join("instructions/style.md"), "new style\n");
        commit_all(&f.a, "replace both rules");
        a.push().unwrap();
        let path = f.b.join(rel);
        let mut open = fs::OpenOptions::new().append(true).open(&path).unwrap();
        assert!(matches!(
            b.pull_at(Before::File(1), || {
                displaced::prune_with(Duration::ZERO, 0).unwrap();
                open.write_all(b"saved while pull was running\n").unwrap();
            })
            .unwrap(),
            PullOutcome::Updated { .. }
        ));
        let entry = displaced::changed().unwrap().into_iter().find(|e| e.kind == Kind::Draft).unwrap();
        assert_eq!(entry.dest, path);
        assert!(read(&entry.path).ends_with("saved while pull was running\n"));
        let wrong = crate::EngineLock::acquire(&f.a).unwrap();
        assert!(invalid(displaced::adopt(&b, &entry.id, &wrong)).contains("different repository"));
        let lock = crate::EngineLock::acquire(&f.b).unwrap();
        displaced::adopt(&b, &entry.id, &lock).unwrap();
        open.write_all(b"saved after adoption\n").unwrap();
        assert!(read(&path).ends_with("saved after adoption\n"));
        assert!(displaced::get(&entry.id).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn draft_adoption_refuses_an_ignored_destination_absent_from_head() {
        use crate::sync::displaced;
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        let rel = "instructions/git.md";
        fs::remove_file(f.a.join(rel)).unwrap();
        commit_all(&f.a, "remove the rule");
        a.push().unwrap();
        assert!(matches!(pull_now(&b).unwrap(), PullOutcome::Updated { .. }));
        let entry = displaced::list().unwrap().into_iter().find(|e| e.dest == f.b.join(rel)).unwrap();
        write(&f.b.join(".git/info/exclude"), &format!("/{rel}\n"));
        write(&f.b.join(rel), "private replacement\n");
        assert!(b.draft().unwrap().is_empty());
        let lock = crate::EngineLock::acquire(&f.b).unwrap();
        let before = snapshot(&f.b);
        assert!(invalid(displaced::adopt(&b, &entry.id, &lock)).contains("local content"));
        assert_eq!(snapshot(&f.b), before);
        assert_eq!(read(&entry.path), "Use Conventional Commits.\n");
        fs::remove_file(f.b.join(rel)).unwrap();
        displaced::adopt(&b, &entry.id, &lock).unwrap();
        assert_eq!(read(&f.b.join(rel)), "Use Conventional Commits.\n");
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_displaced_during_removal_is_reported_and_preserved() {
        use crate::sync::displaced;
        use std::time::Duration;
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        let rel = "instructions/git.md";
        fs::remove_file(f.a.join(rel)).unwrap();
        commit_all(&f.a, "delete rule");
        a.push().unwrap();
        let dest = f.b.join(rel);
        let result = b.pull_at(Before::Remove, || {
            fs::remove_file(&dest).unwrap();
            write(&dest.join("draft"), "important directory\n");
        });
        let Err(Error::Displaced { dest: from, kept, id }) = result else {
            panic!("expected a displaced directory, got {result:?}");
        };
        assert_eq!(from, dest);
        assert_eq!(read(&kept.join("draft")), "important directory\n");
        assert!(displaced::get(&id).unwrap().changed);
        // Even an old, malformed metadata file cannot make content prunable.
        write(&kept.with_file_name("entry.json"), "{");
        assert!(!displaced::prune_with(Duration::ZERO, 0).unwrap().contains(&id));
        assert_eq!(read(&kept.join("draft")), "important directory\n");
    }

    #[cfg(unix)]
    #[test]
    fn resumed_pull_leaves_later_edits_to_published_paths_as_draft() {
        for operation in ["add", "replace", "remove"] {
            let f = Fleet::new();
            let a = repo(&f.a);
            let rel = if operation == "add" { "instructions/new.md" } else { "instructions/git.md" };
            if operation == "remove" {
                fs::remove_file(f.a.join(rel)).unwrap();
            } else {
                write(&f.a.join(rel), "new baseline\n");
            }
            commit_all(&f.a, operation);
            a.push().unwrap();
            assert!(repo(&f.b).pull_crashing_at(Before::Published).is_err());
            let dest = f.b.join(rel);
            match operation {
                "add" => fs::remove_file(&dest).unwrap(),
                "remove" => {
                    use crate::sync::displaced;
                    let entry = displaced::list().unwrap().into_iter().find(|e| e.dest == dest).unwrap();
                    assert!(invalid(displaced::discard(&entry.id)).contains("pull pending"));
                    write(&dest, "recreated after publication\n");
                }
                _ => {
                    let temp = dest.with_extension("save");
                    write(&temp, "saved after publication\n");
                    fs::rename(&temp, &dest).unwrap();
                    git(&f.b, &["add", rel]);
                }
            }
            let b = repo(&f.b);
            assert!(matches!(finish_now(&b).unwrap(), Some(PullOutcome::Updated { .. })), "{operation}");
            assert!(!pending(&b));
            assert_eq!(b.draft().unwrap().len(), 1, "{operation}");
            match operation {
                "add" => assert!(!dest.exists()),
                "remove" => assert_eq!(read(&dest), "recreated after publication\n"),
                _ => {
                    assert_eq!(read(&dest), "saved after publication\n");
                    assert_eq!(git(&f.b, &["show", &format!(":{rel}")]), "saved after publication");
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn pull_replaces_a_nested_directory_with_a_file() {
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        write(&f.a.join("a/b/c"), "tracked\n");
        commit_all(&f.a, "nested directory");
        a.push().unwrap();
        pull_now(&b).unwrap();
        fs::create_dir_all(f.b.join("a/b/empty/deeper")).unwrap();
        fs::remove_dir_all(f.a.join("a")).unwrap();
        write(&f.a.join("a"), "now a file\n");
        commit_all(&f.a, "replace nested directory");
        a.push().unwrap();
        assert!(matches!(pull_now(&b).unwrap(), PullOutcome::Updated { .. }));
        assert_eq!(read(&f.b.join("a")), "now a file\n");
    }

    #[cfg(unix)]
    #[test]
    fn pull_recovers_byte_paths_and_full_length_leaf_names() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        let long = "x".repeat(255);
        let names = [OsStr::from_bytes(b"invalid-\xff"), OsStr::new(&long)];
        for path in names {
            write(&f.a.join(path), "old\n");
        }
        commit_all(&f.a, "add unusual paths");
        a.push().unwrap();
        pull_now(&b).unwrap();
        for path in names {
            write(&f.a.join(path), "new\n");
        }
        commit_all(&f.a, "replace unusual paths");
        a.push().unwrap();
        assert!(b.pull_crashing_at(Before::Published).is_err());
        finish_now(&repo(&f.b)).unwrap();
        for path in names {
            assert_eq!(read(&f.b.join(path)), "new\n");
            let entry = crate::sync::displaced::list().unwrap().into_iter().find(|e| e.dest == f.b.join(path)).unwrap();
            assert!(entry.id.len() < 255);
            assert_eq!(read(&entry.path), "old\n");
            assert!(serde_json::to_vec(&entry).is_ok());
        }
    }

    #[cfg(unix)]
    #[test]
    fn pull_applies_mode_only_changes_and_recovers_a_racing_chmod() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        let rel = "instructions/git.md";
        let path = f.b.join(rel);
        git(&f.b, &["config", "core.fileMode", "false"]);
        chmod(&f.a.join(rel), 0o755);
        commit_all(&f.a, "make rule executable");
        a.push().unwrap();
        assert!(matches!(pull_now(&b).unwrap(), PullOutcome::Updated { .. }));
        assert!(executable(&path));
        chmod(&f.a.join(rel), 0o644);
        commit_all(&f.a, "make rule nonexecutable");
        a.push().unwrap();
        let result = b.pull_at(Before::Swap, || chmod(&path, 0o700));
        let Err(Error::Displaced { kept, .. }) = result else { panic!("{result:?}") };
        assert_eq!(fs::metadata(&kept).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(read(&kept), read(&path));
        assert!(!executable(&path));
    }

    #[cfg(unix)]
    #[test]
    fn pull_accepts_a_case_only_rename_on_case_folding_filesystems() {
        let f = Fleet::new();
        let (a, b) = (repo(&f.a), repo(&f.b));
        write(&f.a.join("lowercase"), "baseline\n");
        commit_all(&f.a, "lowercase");
        a.push().unwrap();
        pull_now(&b).unwrap();
        let alias = f.b.join("Lowercase");
        let case_folding = alias.exists();
        if !case_folding {
            // On a case-sensitive filesystem a hard link with the exact
            // new spelling is a distinct untracked obstruction.
            write(&f.b.join(".git/info/exclude"), "/Lowercase\n");
            fs::hard_link(f.b.join("lowercase"), &alias).unwrap();
        }
        git(&f.a, &["mv", "-f", "lowercase", "Lowercase"]);
        git(&f.a, &["commit", "-qm", "change case"]);
        a.push().unwrap();
        assert_eq!(git(&f.a, &["ls-tree", "--name-only", "HEAD", "--", "Lowercase"]), "Lowercase");
        if !case_folding {
            assert!(matches!(pull_now(&b).unwrap(), PullOutcome::Blocked { .. }));
            fs::remove_file(&alias).unwrap();
        }
        assert!(matches!(pull_now(&b).unwrap(), PullOutcome::Updated { .. }));
        assert!(fs::read_dir(&f.b).unwrap().any(|e| e.unwrap().file_name() == "Lowercase"));
    }

    #[test]
    fn pull_requires_the_repos_engine_lock() {
        let f = Fleet::new();
        let b = repo(&f.b);
        let wrong = crate::EngineLock::acquire(&f.a).unwrap();
        assert!(invalid(b.pull(&wrong)).contains("different repository"));
        assert!(invalid(b.finish_pull(&wrong)).contains("different repository"));
    }
}
