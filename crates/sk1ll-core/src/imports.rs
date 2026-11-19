use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::hash::hash_dir;
use crate::lock::LockEntry;
use crate::manifest::{self, Import};
use crate::skill::IMPORTED_DIR;
use crate::{Error, IoExt, Repo, Result, skill, source};

#[derive(Debug, Clone, Serialize)]
pub struct UpdateReport {
    pub name: String,
    pub old_commit: Option<String>,
    pub new_commit: String,
    pub changed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ImportState {
    /// Vendored content matches the lockfile.
    Clean,
    /// Vendored content was edited locally; `update` would overwrite it.
    Modified,
    /// Declared in the manifest but not vendored yet.
    Missing,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportStatus {
    pub name: String,
    pub state: ImportState,
    pub commit: Option<String>,
}

/// What a link resolves to at its remote ([`Repo::resolve_source`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedSource {
    /// What the import records: `github:owner/repo`, or the URL/path as given.
    pub source: String,
    /// The ref as named by the link (or given), `HEAD` if none.
    pub git_ref: String,
    pub commit: String,
    pub candidates: Vec<SkillCandidate>,
}

/// A directory with a `SKILL.md` that the link leads to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillCandidate {
    /// Repo-relative; `""` for the repo root.
    pub path: String,
    pub default_name: String,
    /// The `SKILL.md` frontmatter's `description`.
    pub description: Option<String>,
    /// An import or personal skill already has `default_name`.
    pub taken: bool,
    pub total_bytes: u64,
    pub warnings: Vec<FileWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileWarning {
    pub path: String,
    pub kinds: Vec<WarningKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WarningKind {
    Executable,
    Script,
    Binary,
    Large,
}

impl WarningKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Executable => "executable",
            Self::Script => "script",
            Self::Binary => "binary",
            Self::Large => "large (> 1 MiB)",
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ImportChanges {
    pub locked: String,
    pub upstream: String,
    pub commits: Vec<ImportCommit>,
    pub truncated: bool,
    pub files: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ImportCommit {
    pub sha: String,
    pub date: String,
    pub subject: String,
}

impl Repo {
    /// Fetches and vendors a new import, then records it in manifest + lock.
    /// Like [`add_fetched`](Self::add_fetched), the name is checked again
    /// after staging, and the install never replaces anything.
    pub fn add_import(&mut self, name: &str, import: Import) -> Result<UpdateReport> {
        self.check_free(name)?;
        let cache = source::fetch(&import.source, self.root())?;
        let commit = source::resolve_commit(&cache, &import.git_ref)?;
        let staged = source::stage(&cache, &commit, &import, self.root(), &self.imported_dir())?;
        self.check_free(name)?;
        staged.install_new(&self.imported_dir().join(name))?;
        let report = self.lock_vendored(name, &import, commit)?;
        self.record_import(name, import)?;
        Ok(report)
    }

    /// Finishes import replacements that were interrupted (see
    /// [`source::recover_replaced`]); returns the names restored. Run it,
    /// holding the engine lock, before planning a sync, so a sync never
    /// removes an import that is only set aside.
    pub fn recover_imports(&self) -> Result<Vec<String>> {
        source::recover_replaced(self.root())
    }

    /// Resolves a pasted link or source without changing the repo (see
    /// [`fetch_link`]), with each candidate's `taken` filled in.
    pub fn resolve_source(&self, link: &str, path: Option<&str>, git_ref: Option<&str>) -> Result<ResolvedSource> {
        Ok(self.with_taken(fetch_link(self.root(), link, path, git_ref, None)?.inspect()?.resolved))
    }

    /// Fills in each candidate's `taken` from the repo as it is now.
    pub fn with_taken(&self, mut resolved: ResolvedSource) -> ResolvedSource {
        for c in &mut resolved.candidates {
            c.taken = self.is_taken(&c.default_name);
        }
        resolved
    }

    /// Adds an import from a pasted link or source (see [`fetch_link`]),
    /// which must lead to exactly one skill. `name` defaults to the skill's
    /// default name. With `commit` (e.g. the one a preview resolved to),
    /// exactly that commit is vendored, and the ref is still recorded in the
    /// manifest.
    pub fn add_link(
        &mut self,
        name: Option<&str>,
        link: &str,
        path: Option<&str>,
        git_ref: Option<&str>,
        commit: Option<&str>,
    ) -> Result<UpdateReport> {
        if let Some(name) = name.filter(|n| !n.is_empty()) {
            self.check_free(name)?;
        }
        let fetched = fetch_link(self.root(), link, path, git_ref, commit)?;
        self.add_fetched(name, fetched)
    }

    /// The local half of [`add_link`](Self::add_link): installs an already
    /// fetched link's skill (see [`FetchedLink::stage`]; staged here if it
    /// wasn't) and records it. Names and destinations are checked against the
    /// repo as it is now, so this is safe to run long after the fetch. The
    /// lock records the hash of exactly what was installed.
    pub fn add_fetched(&mut self, name: Option<&str>, mut fetched: FetchedLink) -> Result<UpdateReport> {
        let default_name = fetched.skill()?.default_name.clone();
        let name = name.filter(|n| !n.is_empty()).unwrap_or(&default_name).to_string();
        validate_name(&name)?;
        // Staging may take a while (smudge filters); the name is checked after
        // it, and the install never replaces anything that appeared since.
        fetched.stage()?;
        self.check_free(&name)?;
        let (import, staged) = (fetched.import()?, fetched.staged.take().expect("staged above"));
        staged.install_new(&self.imported_dir().join(&name))?;
        let report = self.lock_vendored(&name, &import, fetched.resolved.commit.clone())?;
        self.record_import(&name, import)?;
        Ok(report)
    }

    /// An import, personal skill or leftover `imported/` entry already goes by `name`.
    fn is_taken(&self, name: &str) -> bool {
        self.manifest.import.contains_key(name)
            || self.personal_dir().join(name).exists()
            || fs::symlink_metadata(self.imported_dir().join(name)).is_ok()
    }

    /// `name` is free for a new import. Vendoring replaces its destination,
    /// so anything already there (a dangling symlink too) is refused.
    fn check_free(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        if self.manifest.import.contains_key(name) {
            return Err(Error::Invalid(format!("import `{name}` already exists")));
        }
        if self.personal_dir().join(name).exists() {
            return Err(Error::Invalid(format!("a personal skill named `{name}` already exists")));
        }
        let dest = self.imported_dir().join(name);
        if fs::symlink_metadata(&dest).is_ok() {
            return Err(Error::Invalid(format!(
                "{} already exists but is not a recorded import; move it away or pick another name",
                dest.display()
            )));
        }
        Ok(())
    }

    fn record_import(&mut self, name: &str, import: Import) -> Result<()> {
        manifest::write_import(self.root(), name, &import)?;
        self.manifest.import.insert(name.to_string(), import);
        Ok(())
    }

    /// Re-fetches imports (all, or one by name). Refuses to clobber local edits
    /// unless `force` is set.
    pub fn update(&mut self, only: Option<&str>, force: bool) -> Result<Vec<UpdateReport>> {
        let todo = self.update_targets(only, force)?;
        let fetched = fetch_updates(self.root(), todo)?;
        self.apply_updates(fetched, force)
    }

    /// The imports an update of `only` (or all) re-fetches. Fails before any
    /// fetch on an unknown name, or (unless `force`) on local edits.
    pub fn update_targets(&self, only: Option<&str>, force: bool) -> Result<UpdateTargets> {
        if let Some(name) = only
            && !self.manifest.import.contains_key(name)
        {
            return Err(Error::Invalid(format!("no import named `{name}`")));
        }
        let todo: Vec<_> = self
            .manifest
            .import
            .iter()
            .filter(|(n, _)| only.is_none_or(|o| o == n.as_str()))
            .map(|(n, i)| (n.clone(), i.clone(), self.lock.skills.get(n).cloned()))
            .collect();
        if !force {
            self.check_unmodified(todo.iter().map(|(n, ..)| n.as_str()))?;
        }
        Ok(UpdateTargets(todo))
    }

    /// The local half of [`update`](Self::update): installs the fetched and
    /// staged imports (no git, no network). Each must still be declared and
    /// locked exactly as when its fetch started (an update that finished in
    /// the meantime is not rolled back), and (unless `force`) be unedited.
    pub fn apply_updates(&mut self, fetched: FetchedUpdates, force: bool) -> Result<Vec<UpdateReport>> {
        for u in &fetched.0 {
            if self.manifest.import.get(&u.name) != Some(&u.import) {
                return Err(Error::Invalid(format!("import `{}` changed while it was fetched; update again", u.name)));
            }
            if self.lock.skills.get(&u.name) != u.locked.as_ref() {
                return Err(Error::Invalid(format!(
                    "import `{}` was updated by another operation while fetching; update again",
                    u.name
                )));
            }
        }
        if !force {
            self.check_unmodified(fetched.0.iter().map(|u| u.name.as_str()))?;
        }
        fetched
            .0
            .into_iter()
            .map(|u| {
                let dest = self.imported_dir().join(&u.name);
                if fs::symlink_metadata(&dest).is_ok() {
                    let locked = self.lock.skills.get(&u.name).map(|e| e.hash.clone());
                    u.staged.replace(&dest, locked.as_deref())?;
                } else {
                    u.staged.install_new(&dest)?;
                }
                self.lock_vendored(&u.name, &u.import, u.commit)
            })
            .collect()
    }

    fn check_unmodified<'a>(&self, names: impl Iterator<Item = &'a str>) -> Result<()> {
        let names: Vec<&str> = names.collect();
        let modified: Vec<_> = self
            .import_status()?
            .into_iter()
            .filter(|s| s.state == ImportState::Modified && names.contains(&s.name.as_str()))
            .map(|s| s.name)
            .collect();
        if !modified.is_empty() {
            return Err(Error::Invalid(format!(
                "locally modified imports: {} (use `fork` to keep edits, or force to overwrite)",
                modified.join(", ")
            )));
        }
        Ok(())
    }

    pub fn import_status(&self) -> Result<Vec<ImportStatus>> {
        self.manifest
            .import
            .keys()
            .map(|name| {
                let dir = self.imported_dir().join(name);
                let lock = self.lock.skills.get(name);
                let state = match lock {
                    _ if !dir.is_dir() => ImportState::Missing,
                    None => ImportState::Missing,
                    Some(entry) if hash_dir(&dir)? == entry.hash => ImportState::Clean,
                    Some(_) => ImportState::Modified,
                };
                Ok(ImportStatus { name: name.clone(), state, commit: lock.map(|e| e.commit.clone()) })
            })
            .collect()
    }

    /// Deletes an import from manifest, lock and disk.
    pub fn remove_import(&mut self, name: &str) -> Result<()> {
        validate_name(name)?;
        let parent = self.imported_dir();
        let dir = parent.join(name);
        for path in [&parent, &dir] {
            match fs::symlink_metadata(path) {
                Ok(meta) if meta.is_symlink() => return Err(Error::Invalid("import path is a symlink".into())),
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e).at(path),
                _ => {}
            }
        }
        if dir.exists() {
            let base = fs::canonicalize(&parent).at(&parent)?;
            let target = fs::canonicalize(&dir).at(&dir)?;
            if target == base || !target.starts_with(&base) {
                return Err(Error::Invalid("import path escapes imported directory".into()));
            }
        }
        if self.manifest.import.remove(name).is_none() {
            return Err(Error::Invalid(format!("no import named `{name}`")));
        }
        manifest::remove_import(self.root(), name)?;
        self.lock.skills.remove(name);
        self.lock.save(self.root())?;
        let dir = self.imported_dir().join(name);
        if dir.exists() {
            fs::remove_dir_all(&dir).at(&dir)?;
        }
        Ok(())
    }

    /// Turns an import into a personal skill you own and edit freely.
    pub fn fork(&mut self, name: &str) -> Result<()> {
        validate_name(name)?;
        let from = self.imported_dir().join(name);
        let to = self.personal_dir().join(name);
        if !from.is_dir() {
            return Err(Error::Invalid(format!("no vendored import named `{name}`")));
        }
        if to.exists() {
            return Err(Error::Invalid(format!("personal skill `{name}` already exists")));
        }
        fs::rename(&from, &to).at(&to)?;
        self.manifest.import.remove(name);
        manifest::remove_import(self.root(), name)?;
        self.lock.skills.remove(name);
        self.lock.save(self.root())
    }

    /// Records `name`'s freshly vendored content at `commit` in the lockfile.
    fn lock_vendored(&mut self, name: &str, import: &Import, commit: String) -> Result<UpdateReport> {
        let hash = hash_dir(&self.imported_dir().join(name))?;
        let old = self.lock.skills.insert(
            name.to_string(),
            LockEntry {
                source: import.source.clone(),
                path: import.path.clone(),
                git_ref: import.git_ref.clone(),
                commit: commit.clone(),
                hash: hash.clone(),
            },
        );
        self.lock.save(self.root())?;
        let changed = old.as_ref().is_none_or(|o| o.hash != hash);
        Ok(UpdateReport { name: name.to_string(), old_commit: old.map(|o| o.commit), new_commit: commit, changed })
    }
}

/// A link fetched into the import cache and resolved ([`fetch_link`]): what
/// [`Repo::add_fetched`] installs without the network. Dropping it removes
/// anything it staged.
#[derive(Debug, Serialize)]
pub struct FetchedLink {
    /// `taken` is not filled in yet (see [`Repo::with_taken`]).
    pub resolved: ResolvedSource,
    link: String,
    /// The explicit path, if one was given.
    path: Option<String>,
    root: PathBuf,
    cache: PathBuf,
    #[serde(skip)]
    staged: Option<source::Staged>,
}

impl FetchedLink {
    /// Inspects candidate blobs for the preview without checking them out.
    pub fn inspect(mut self) -> Result<Self> {
        for candidate in &mut self.resolved.candidates {
            (candidate.total_bytes, candidate.warnings) =
                source::file_warnings(&self.cache, &self.resolved.commit, &candidate.path)?;
        }
        Ok(self)
    }

    /// The one skill the link leads to.
    fn skill(&self) -> Result<&SkillCandidate> {
        let resolved = &self.resolved;
        match resolved.candidates.as_slice() {
            [one] => Ok(one),
            [] => {
                let under = self.path.as_deref().filter(|p| !p.is_empty()).map(|p| format!(" under `{p}`"));
                Err(Error::Invalid(format!(
                    "no SKILL.md{} in {} @ {}",
                    under.unwrap_or_default(),
                    resolved.source,
                    resolved.git_ref
                )))
            }
            many => {
                let link = &self.link;
                let paths: Vec<&str> = many.iter().map(|c| if c.path.is_empty() { "." } else { &c.path }).collect();
                Err(Error::Invalid(format!(
                    "`{link}` has {} skills: {}; pick one with --path (see `sk1ll add --list {link}`)",
                    many.len(),
                    paths.join(", ")
                )))
            }
        }
    }

    fn import(&self) -> Result<Import> {
        let r = &self.resolved;
        Ok(Import { source: r.source.clone(), path: self.skill()?.path.clone(), git_ref: r.git_ref.clone() })
    }

    /// Checks the one skill out of the cache into a staging directory, ready
    /// for [`Repo::add_fetched`]. Part of the network phase: checkout runs
    /// smudge filters (Git LFS), which may fetch.
    pub fn stage(&mut self) -> Result<()> {
        if self.staged.is_none() {
            let import = self.import()?;
            let into = self.root.join(IMPORTED_DIR);
            self.staged = Some(source::stage(&self.cache, &self.resolved.commit, &import, &self.root, &into)?);
        }
        Ok(())
    }
}

/// The network half of resolving a pasted link or source
/// ([`source::parse_link`]): fetches it into the import cache and lists the
/// skills it leads to, reading nothing but `root` (the repo root, for
/// relative local sources). Inputs are checked before anything is fetched.
///
/// An explicit `path` or `git_ref` overrides what the link names. An
/// explicit ref must equal the link's ref, if it names one, unless `path` is
/// explicit too: then the link's ref and path are both ignored. `commit`
/// pins the commit (it must be in the source). When the path is a skill
/// directory, that is the only candidate; otherwise every directory with a
/// `SKILL.md` under it (or in the whole repo) is.
pub fn fetch_link(
    root: &Path,
    link: &str,
    path: Option<&str>,
    git_ref: Option<&str>,
    commit: Option<&str>,
) -> Result<FetchedLink> {
    let parsed = source::parse_link(link)?;
    let path = path.map(source::check_path).transpose()?;
    git_ref.map(source::check_ref).transpose()?;
    commit.map(source::check_commit).transpose()?;
    let cache = source::fetch(&parsed.source, root)?;
    let (git_ref, dir, commit) = source::resolve_link(&cache, &parsed, git_ref, path.as_deref(), commit)?;
    let dirs = source::skill_dirs(&cache, &commit, &dir)?;
    if dirs.is_empty() && !dir.is_empty() {
        source::check_folder(&cache, &commit, &git_ref, &dir)?;
    }
    let dirs = if dirs.contains(&dir) { vec![dir] } else { dirs };
    let candidates = dirs
        .into_iter()
        .map(|dir| {
            let default_name = source::default_name(&parsed.source, &dir);
            let file = if dir.is_empty() { "SKILL.md".into() } else { format!("{dir}/SKILL.md") };
            let description =
                source::show(&cache, &commit, &file).and_then(|t| skill::frontmatter_field(&t, "description"));
            SkillCandidate { path: dir, default_name, description, taken: false, total_bytes: 0, warnings: Vec::new() }
        })
        .collect();
    let resolved = ResolvedSource { source: parsed.source, git_ref, commit, candidates };
    Ok(FetchedLink { resolved, link: link.trim().to_string(), path, root: root.to_path_buf(), cache, staged: None })
}

/// What an update re-fetches ([`Repo::update_targets`]): each import as
/// declared, and its lock entry when the update started.
#[derive(Debug, Clone, Serialize)]
pub struct UpdateTargets(Vec<(String, Import, Option<LockEntry>)>);

/// Fetches the current ref (or previews a previously advertised commit).
/// Only the source cache is written.
pub fn fetch_changes(root: &Path, targets: UpdateTargets, upstream: Option<&str>) -> Result<ImportChanges> {
    let [(name, import, lock)] = targets.0.as_slice() else {
        return Err(Error::Invalid("select one import to preview".into()));
    };
    let locked = &lock.as_ref().ok_or_else(|| Error::Invalid(format!("import `{name}` has no locked commit")))?.commit;
    source::check_commit(locked)?;
    upstream.map(source::check_commit).transpose()?;
    let cache = source::fetch(&import.source, root)?;
    let upstream = match upstream {
        Some(commit) => source::verify_commit(&cache, &import.source, commit)?,
        None => source::resolve_commit(&cache, &import.git_ref)?,
    };
    source::changes(&cache, locked, &upstream, import.path.trim_matches('/'))
}

/// Imports fetched and staged for an update ([`fetch_updates`]): what
/// [`Repo::apply_updates`] installs without the network. Dropping it
/// removes what it staged.
#[derive(Debug, Serialize)]
pub struct FetchedUpdates(Vec<FetchedUpdate>);

#[derive(Debug, Serialize)]
struct FetchedUpdate {
    name: String,
    import: Import,
    locked: Option<LockEntry>,
    commit: String,
    #[serde(skip)]
    staged: source::Staged,
}

impl FetchedUpdates {
    pub fn check_commit(&self, expected: &str) -> Result<()> {
        if matches!(self.0.as_slice(), [update] if update.commit == expected) {
            return Ok(());
        }
        Err(Error::Changed("the upstream ref changed since the preview; check upstream changes again".into()))
    }
}

/// The network half of [`Repo::update`]: fetches each import's source once,
/// resolves its ref and stages its content. Reads nothing but `root` (for
/// relative sources).
pub fn fetch_updates(root: &Path, targets: UpdateTargets) -> Result<FetchedUpdates> {
    let mut caches: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut out = Vec::new();
    for (name, import, locked) in targets.0 {
        let cache = match caches.get(&import.source) {
            Some(c) => c.clone(),
            None => {
                let c = source::fetch(&import.source, root)?;
                caches.insert(import.source.clone(), c.clone());
                c
            }
        };
        let commit = source::resolve_commit(&cache, &import.git_ref)?;
        let staged = source::stage(&cache, &commit, &import, root, &root.join(IMPORTED_DIR))?;
        out.push(FetchedUpdate { name, import, locked, commit, staged });
    }
    Ok(FetchedUpdates(out))
}

/// Validates a single content name before constructing repository paths.
pub fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && !name.contains("..")
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok { Ok(()) } else { Err(Error::Invalid(format!("invalid skill name `{name}`"))) }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::git::Git;

    #[test]
    fn remove_rejects_unsafe_paths_before_mutating() {
        let src = Source::new();
        let mut repo = src.repo();
        repo.manifest.import.insert(
            "../personal".into(),
            Import { source: "unused".into(), path: String::new(), git_ref: "HEAD".into() },
        );
        fs::write(repo.root().join("sk1ll.toml"), toml::to_string(&repo.manifest).unwrap()).unwrap();
        fs::write(repo.personal_dir().join("keep.txt"), "personal content").unwrap();
        let before = fs::read(repo.root().join("sk1ll.toml")).unwrap();
        for name in ["../personal", "a..b", "a/b", "a\\b", "-bad", "_bad"] {
            assert!(repo.remove_import(name).is_err());
            assert!(repo.create_item("skill", name, "").is_err());
        }
        assert_eq!(fs::read(repo.root().join("sk1ll.toml")).unwrap(), before);
        assert_eq!(fs::read_to_string(repo.personal_dir().join("keep.txt")).unwrap(), "personal content");
        assert!(repo.manifest.import.contains_key("../personal"));
    }

    #[cfg(unix)]
    #[test]
    fn remove_rejects_symlink_target_and_parent() {
        let src = Source::new();
        let mut repo = src.repo();
        repo.add_link(None, &src.origin(), Some("skills/pdf"), None, None).unwrap();
        let before = fs::read(repo.root().join("sk1ll.toml")).unwrap();
        let target = repo.imported_dir().join("pdf");
        fs::rename(&target, repo.root().join("saved")).unwrap();
        std::os::unix::fs::symlink(repo.root().join("saved"), &target).unwrap();
        assert!(repo.remove_import("pdf").is_err());
        fs::remove_file(&target).unwrap();
        fs::rename(repo.root().join("saved"), &target).unwrap();
        fs::rename(repo.imported_dir(), repo.root().join("saved-imported")).unwrap();
        std::os::unix::fs::symlink(repo.root().join("saved-imported"), repo.imported_dir()).unwrap();
        assert!(repo.remove_import("pdf").is_err());
        assert_eq!(fs::read(repo.root().join("sk1ll.toml")).unwrap(), before);
        assert!(repo.manifest.import.contains_key("pdf"));
        assert!(target.join("SKILL.md").exists());
    }

    /// A local source repo with skills at `skills/pdf`, `skills/web` and
    /// `skills/web/nested`, on `main` and a slashed branch `feature/x`.
    struct Source {
        tmp: tempfile::TempDir,
        _cache: source::TestCache,
    }

    impl Source {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let cache = source::test_cache(&tmp.path().join("cache"));
            let origin = tmp.path().join("origin");
            let skills = [("skills/pdf", "Work with PDFs"), ("skills/web", "Browse"), ("skills/web/nested", "")];
            for (dir, description) in skills {
                fs::create_dir_all(origin.join(dir)).unwrap();
                fs::write(
                    origin.join(dir).join("SKILL.md"),
                    format!("---\nname: x\ndescription: {description}\n---\n"),
                )
                .unwrap();
            }
            fs::write(origin.join("README.md"), "skills\n").unwrap();
            git(&origin, &["init", "--quiet", "-b", "main"]);
            commit_all(&origin);
            git(&origin, &["branch", "feature/x"]);
            let root = tmp.path().join("repo");
            Repo::init(&root).unwrap();
            commit_all(&root);
            Self { tmp, _cache: cache }
        }

        fn origin(&self) -> String {
            self.tmp.path().join("origin").to_string_lossy().into_owned()
        }

        fn repo(&self) -> Repo {
            Repo::open(self.tmp.path().join("repo")).unwrap()
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        Git::new(dir).args(args).run().unwrap();
    }

    fn commit_all(dir: &Path) {
        git(dir, &["add", "-A"]);
        git(dir, &["-c", "user.name=T", "-c", "user.email=t@example.com", "commit", "--quiet", "-m", "seed"]);
    }

    fn paths(r: &ResolvedSource) -> Vec<&str> {
        r.candidates.iter().map(|c| c.path.as_str()).collect()
    }

    #[test]
    fn preview_reports_warnings_from_git_blobs() {
        let src = Source::new();
        let origin = PathBuf::from(src.origin());
        let skill = origin.join("skills/pdf");
        fs::write(skill.join("run"), "#!/bin/sh\n").unwrap();
        fs::write(skill.join("test.PY"), "print(1)\n").unwrap();
        fs::write(skill.join("big"), vec![0xff; 1024 * 1024 + 1]).unwrap();
        git(&origin, &["add", "-A"]);
        git(&origin, &["update-index", "--chmod=+x", "skills/pdf/run"]);
        git(&origin, &["-c", "user.name=T", "-c", "user.email=t@example.com", "commit", "-qm", "files"]);
        let resolved = src.repo().resolve_source(&src.origin(), Some("skills/pdf"), None).unwrap();
        let candidate = &resolved.candidates[0];
        assert_eq!(
            candidate.total_bytes,
            1024 * 1024 + 1 + 10 + 9 + fs::metadata(skill.join("SKILL.md")).unwrap().len()
        );
        let warnings: Vec<_> = candidate.warnings.iter().map(|w| (w.path.as_str(), w.kinds.as_slice())).collect();
        use WarningKind::{Binary, Executable, Large, Script};
        assert_eq!(warnings, [("big", &[Binary, Large][..]), ("run", &[Executable, Script]), ("test.PY", &[Script])]);
    }

    #[test]
    fn update_preview_lists_commits_and_only_files_in_the_skill() {
        let src = Source::new();
        let mut repo = src.repo();
        let added = repo.add_link(None, &src.origin(), Some("skills/pdf"), Some("main"), None).unwrap();
        let origin = PathBuf::from(src.origin());
        fs::write(origin.join("README.md"), "unrelated\n").unwrap();
        commit_all(&origin);
        fs::write(origin.join("skills/pdf/new.txt"), "new\n").unwrap();
        commit_all(&origin);
        let preview = fetch_changes(repo.root(), repo.update_targets(Some("pdf"), true).unwrap(), None).unwrap();
        assert_eq!(preview.locked, added.new_commit);
        assert_eq!(preview.commits.len(), 2);
        assert_eq!(preview.commits[0].subject, "seed");
        assert!(preview.upstream.starts_with(&preview.commits[0].sha));
        assert_eq!(preview.commits[0].date.len(), 10);
        assert_eq!(preview.files, ["skills/pdf/new.txt"]);
        assert!(!preview.truncated);
        assert_eq!(repo.import_status().unwrap()[0].commit.as_ref(), Some(&added.new_commit));
        assert!(!repo.imported_dir().join("pdf/new.txt").exists());
    }

    #[test]
    fn resolving_lists_skills_without_writing_the_repo() {
        let src = Source::new();
        let mut repo = src.repo();
        let all = repo.resolve_source(&src.origin(), None, None).unwrap();
        assert_eq!(all.source, src.origin());
        assert_eq!(all.git_ref, "HEAD");
        assert_eq!(all.commit.len(), 40);
        assert_eq!(paths(&all), ["skills/pdf", "skills/web", "skills/web/nested"]);
        let pdf = &all.candidates[0];
        assert_eq!((pdf.default_name.as_str(), pdf.description.as_deref()), ("pdf", Some("Work with PDFs")));
        assert!(!pdf.taken);
        assert_eq!(all.candidates[2].description, None);
        assert_eq!(repo.draft().unwrap(), Vec::new(), "resolving changes nothing in the repo");

        // A skill directory is the only candidate, even with skills below it.
        let web = repo.resolve_source(&src.origin(), Some("skills/web/"), Some("feature/x")).unwrap();
        assert_eq!((paths(&web), web.git_ref.as_str()), (vec!["skills/web"], "feature/x"));
        assert_eq!(paths(&repo.resolve_source(&src.origin(), Some("skills"), None).unwrap()).len(), 3);
        let missing = repo.resolve_source(&src.origin(), Some("nope"), None).unwrap_err().to_string();
        assert_eq!(missing, "there is no `nope` at `HEAD`");
        let file = repo.resolve_source(&src.origin(), Some("README.md"), None).unwrap_err().to_string();
        assert!(file.starts_with("`README.md` is a file, not a folder"), "{file}");

        fs::create_dir_all(repo.personal_dir().join("pdf")).unwrap();
        repo.add_link(Some("browse"), &src.origin(), Some("skills/web"), Some("main"), None).unwrap();
        let again = repo.resolve_source(&src.origin(), Some("skills"), None).unwrap();
        let taken: Vec<_> = again.candidates.iter().map(|c| (c.default_name.as_str(), c.taken)).collect();
        assert_eq!(taken, [("pdf", true), ("web", false), ("nested", false)]);
    }

    #[test]
    fn adding_a_link_needs_exactly_one_skill() {
        let src = Source::new();
        let mut repo = src.repo();
        let err = repo.add_link(None, &src.origin(), None, None, None).unwrap_err().to_string();
        assert!(err.contains("has 3 skills: skills/pdf, skills/web, skills/web/nested"), "{err}");
        assert!(err.contains("--path") && err.contains("sk1ll add --list"), "{err}");
        let err = repo.add_link(None, &src.origin(), Some("docs"), None, None).unwrap_err().to_string();
        assert_eq!(err, "there is no `docs` at `HEAD`");
        assert!(repo.manifest.import.is_empty() && !repo.imported_dir().join("pdf").exists());

        let r = repo.add_link(None, &src.origin(), Some("skills/pdf"), Some("feature/x"), None).unwrap();
        assert_eq!(r.name, "pdf");
        assert!(repo.imported_dir().join("pdf/SKILL.md").is_file());
        let import = &repo.manifest.import["pdf"];
        assert_eq!((import.path.as_str(), import.git_ref.as_str()), ("skills/pdf", "feature/x"));
        assert_eq!(repo.lock.skills["pdf"].commit, r.new_commit);
        let reopened = src.repo();
        assert_eq!(reopened.manifest.import["pdf"], *import);
        assert_eq!(reopened.import_status().unwrap()[0].state, ImportState::Clean);

        let err = repo.add_link(None, &src.origin(), Some("skills/pdf"), None, None).unwrap_err().to_string();
        assert!(err.contains("import `pdf` already exists"), "{err}");
    }

    /// Anything already at `imported/<name>` that isn't a recorded import is
    /// never replaced by a new import (updates of recorded ones still are).
    #[test]
    fn adding_never_replaces_an_unrecorded_imported_dir() {
        let src = Source::new();
        let mut repo = src.repo();
        let orphan = repo.imported_dir().join("pdf");
        fs::create_dir_all(&orphan).unwrap();
        fs::write(orphan.join("notes.md"), "mine\n").unwrap();
        let err = repo.add_link(None, &src.origin(), Some("skills/pdf"), None, None).unwrap_err().to_string();
        assert!(err.contains("not a recorded import"), "{err}");
        let import = Import { source: src.origin(), path: "skills/pdf".into(), git_ref: "HEAD".into() };
        assert!(repo.add_import("pdf", import.clone()).is_err());
        assert_eq!(fs::read_to_string(orphan.join("notes.md")).unwrap(), "mine\n");
        assert!(repo.manifest.import.is_empty());
        assert!(repo.resolve_source(&src.origin(), Some("skills/pdf"), None).unwrap().candidates[0].taken);

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(repo.root().join("missing"), repo.imported_dir().join("web")).unwrap();
            let err = repo.add_import("web", Import { path: "skills/web".into(), ..import.clone() }).unwrap_err();
            assert!(err.to_string().contains("not a recorded import"), "{err}");
        }

        fs::remove_dir_all(&orphan).unwrap();
        repo.add_import("pdf", import).unwrap();
        fs::write(repo.imported_dir().join("pdf/SKILL.md"), "edited").unwrap();
        repo.update(Some("pdf"), true).unwrap();
        assert_ne!(fs::read_to_string(repo.imported_dir().join("pdf/SKILL.md")).unwrap(), "edited");
    }

    /// What a preview showed is what gets added, even if upstream moved on.
    #[test]
    fn adding_a_previewed_commit_vendors_exactly_that_commit() {
        let src = Source::new();
        let mut repo = src.repo();
        let preview = repo.resolve_source(&src.origin(), Some("skills/pdf"), Some("main")).unwrap();
        let origin = src.tmp.path().join("origin");
        fs::write(origin.join("skills/pdf/SKILL.md"), "---\ndescription: v2\n---\n").unwrap();
        commit_all(&origin);

        let r = repo.add_link(None, &src.origin(), Some("skills/pdf"), Some("main"), Some(&preview.commit)).unwrap();
        assert_eq!(r.new_commit, preview.commit);
        let text = fs::read_to_string(repo.imported_dir().join("pdf/SKILL.md")).unwrap();
        assert!(text.contains("Work with PDFs"), "{text}");
        assert_eq!(repo.manifest.import["pdf"].git_ref, "main");
        assert_eq!(repo.lock.skills["pdf"].commit, preview.commit);

        let zero = "0000000000000000000000000000000000000000";
        for (bad, why) in [(zero, "is not in"), ("main", "not a SHA"), ("--all", "not a SHA")] {
            let err = repo.add_link(Some("x"), &src.origin(), Some("skills/web"), None, Some(bad)).unwrap_err();
            assert!(err.to_string().contains(why), "{bad}: {err}");
        }
        assert!(!repo.manifest.import.contains_key("x"));
    }

    /// Bad input is refused before anything is fetched.
    #[test]
    fn invalid_input_is_refused_before_fetching() {
        let src = Source::new();
        let mut repo = src.repo();
        let cache = src.tmp.path().join("cache");
        let o = src.origin();
        assert!(repo.resolve_source(&o, None, Some("main..other")).is_err());
        assert!(repo.resolve_source(&o, Some("skills/../x"), None).is_err());
        assert!(repo.add_link(Some("bad name"), &o, Some("skills/pdf"), None, None).is_err());
        assert!(repo.add_link(None, &o, Some("skills/pdf"), Some("-x"), None).is_err());
        assert!(repo.add_link(None, &o, Some("skills/pdf"), None, Some("HEAD~1")).is_err());
        assert!(repo.add_link(None, &format!("{o}\n--upload-pack=x"), None, None, None).is_err());
        assert!(!cache.exists(), "nothing was cloned");
    }

    /// Staging (and set-aside) directories left in `imported/`.
    fn stages(repo: &Repo) -> usize {
        fs::read_dir(repo.imported_dir())
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().starts_with(source::STAGE_PREFIX))
            .count()
    }

    fn smudge(command: &str) -> impl Drop + use<> {
        crate::git::test_env(&[
            ("GIT_CONFIG_COUNT", "2"),
            ("GIT_CONFIG_KEY_0", "filter.probe.smudge"),
            ("GIT_CONFIG_VALUE_0", command),
            ("GIT_CONFIG_KEY_1", "filter.probe.required"),
            ("GIT_CONFIG_VALUE_1", "true"),
        ])
    }

    /// The local phase needs no network: it installs from the cache even once
    /// the source is gone, and re-checks names against the repo as it is now.
    #[test]
    fn fetched_links_are_vendored_offline_and_rechecked() {
        let src = Source::new();
        let mut repo = src.repo();
        let pdf = fetch_link(repo.root(), &src.origin(), Some("skills/pdf"), None, None).unwrap();
        let mut web = fetch_link(repo.root(), &src.origin(), Some("skills/web"), None, None).unwrap();
        web.stage().unwrap();
        assert_eq!(stages(&repo), 1);
        fs::remove_dir_all(src.tmp.path().join("origin")).unwrap();
        // Something took the name between the fetch and the add.
        fs::create_dir_all(repo.personal_dir().join("pdf")).unwrap();
        assert!(repo.with_taken(pdf.resolved.clone()).candidates[0].taken);
        let err = repo.add_fetched(None, pdf).unwrap_err().to_string();
        assert!(err.contains("personal skill named `pdf`"), "{err}");
        let commit = web.resolved.commit.clone();
        let r = repo.add_fetched(None, web).unwrap();
        assert_eq!((r.name.as_str(), r.new_commit.as_str()), ("web", commit.as_str()));
        assert!(repo.imported_dir().join("web/SKILL.md").is_file());
        assert_eq!(repo.lock.skills["web"].hash, crate::hash::hash_dir(&repo.imported_dir().join("web")).unwrap());
        assert_eq!(stages(&repo), 0, "installed or dropped, nothing staged is left behind");
    }

    #[test]
    fn fetched_updates_apply_offline_unless_the_import_changed() {
        let src = Source::new();
        let mut repo = src.repo();
        repo.add_link(None, &src.origin(), Some("skills/pdf"), None, None).unwrap();
        let origin = src.tmp.path().join("origin");
        let release = |v: &str| {
            fs::write(origin.join("skills/pdf/SKILL.md"), format!("---\ndescription: {v}\n---\n")).unwrap();
            commit_all(&origin);
        };
        let installed = |repo: &Repo| fs::read_to_string(repo.imported_dir().join("pdf/SKILL.md")).unwrap();
        release("v2");

        // Edited locally after the fetch: refused unless forced.
        let fetched = fetch_updates(repo.root(), repo.update_targets(None, false).unwrap()).unwrap();
        fs::write(repo.imported_dir().join("pdf/SKILL.md"), "edited").unwrap();
        assert!(repo.apply_updates(fetched, false).unwrap_err().to_string().contains("locally modified"));
        let fetched = fetch_updates(repo.root(), repo.update_targets(None, true).unwrap()).unwrap();
        let r = repo.apply_updates(fetched, true).unwrap();
        assert!(r[0].changed && installed(&repo).contains("v2"));

        // A slow update (fetched v3) must not undo a quicker one (v4) that
        // finished while it was fetching.
        release("v3");
        let slow = fetch_updates(repo.root(), repo.update_targets(None, false).unwrap()).unwrap();
        release("v4");
        repo.update(Some("pdf"), false).unwrap();
        let err = repo.apply_updates(slow, false).unwrap_err().to_string();
        assert!(err.contains("updated by another operation while fetching"), "{err}");
        assert!(installed(&repo).contains("v4"));

        let gone = fetch_updates(repo.root(), repo.update_targets(None, false).unwrap()).unwrap();
        repo.remove_import("pdf").unwrap();
        let err = repo.apply_updates(gone, true).unwrap_err().to_string();
        assert!(err.contains("changed while it was fetched"), "{err}");
        assert!(!repo.imported_dir().join("pdf").exists());
        assert_eq!(stages(&repo), 0);
    }

    /// Checkout runs smudge filters (Git LFS fetches there), so it happens
    /// while staging, in the network phase. Installing runs no git at all: it
    /// works with a filter that would fail, and installs what was staged.
    #[test]
    fn smudge_filters_run_while_staging_not_while_installing() {
        let src = Source::new();
        let mut repo = src.repo();
        let origin = src.tmp.path().join("origin");
        fs::write(origin.join("skills/pdf/.gitattributes"), "SKILL.md filter=probe\n").unwrap();
        commit_all(&origin);
        let log = src.tmp.path().join("smudge.log");
        let probe = format!("sh -c 'echo ran >> \"{}\"; sed s/PDFs/smudged/'", log.display());
        let filter = smudge;
        let runs = || fs::read_to_string(&log).map_or(0, |t| t.lines().count());

        let working = filter(&probe);
        let mut pdf = fetch_link(repo.root(), &src.origin(), Some("skills/pdf"), None, None).unwrap();
        pdf.stage().unwrap();
        let fetched_runs = runs();
        assert!(fetched_runs > 0, "the filter ran while staging");
        drop(working);
        let _failing = filter("false");
        repo.add_fetched(None, pdf).unwrap();
        assert_eq!(runs(), fetched_runs, "installing ran no filter");
        let text = fs::read_to_string(repo.imported_dir().join("pdf/SKILL.md")).unwrap();
        assert!(text.contains("Work with smudged"), "{text}");
        assert_eq!(repo.lock.skills["pdf"].hash, crate::hash::hash_dir(&repo.imported_dir().join("pdf")).unwrap());
    }

    #[test]
    fn smudge_filters_run_while_fetching_updates_not_while_applying() {
        let src = Source::new();
        let mut repo = src.repo();
        let origin = src.tmp.path().join("origin");
        repo.add_link(None, &src.origin(), Some("skills/pdf"), None, None).unwrap();
        fs::write(origin.join("skills/pdf/.gitattributes"), "SKILL.md filter=probe\n").unwrap();
        fs::write(origin.join("skills/pdf/SKILL.md"), "---\ndescription: v2\n---\n").unwrap();
        commit_all(&origin);
        let env = smudge;
        let log = src.tmp.path().join("smudge.log");
        let working = env(&format!("sh -c 'echo ran >> \"{}\"; cat'", log.display()));
        let fetched = fetch_updates(repo.root(), repo.update_targets(None, false).unwrap()).unwrap();
        assert!(log.is_file(), "the filter ran while fetching");
        drop(working);
        let _failing = env("false");
        repo.apply_updates(fetched, false).unwrap();
        assert!(fs::read_to_string(repo.imported_dir().join("pdf/SKILL.md")).unwrap().contains("v2"));
    }

    /// Something that appears at `imported/<name>` while an import is being
    /// staged (a slow smudge filter) or installed is never removed.
    #[test]
    fn adding_never_removes_what_appeared_while_staging() {
        let src = Source::new();
        let mut repo = src.repo();
        let origin = src.tmp.path().join("origin");
        fs::write(origin.join("skills/pdf/.gitattributes"), "SKILL.md filter=probe\n").unwrap();
        commit_all(&origin);
        let dest = repo.imported_dir().join("pdf");
        let appear = format!("sh -c 'mkdir -p \"{0}\" && echo mine > \"{0}/notes.md\"; cat'", dest.display());
        let _filter = smudge(&appear);
        let err = repo.add_link(None, &src.origin(), Some("skills/pdf"), None, None).unwrap_err().to_string();
        assert!(err.contains("not a recorded import"), "{err}");
        assert_eq!(fs::read_to_string(dest.join("notes.md")).unwrap(), "mine\n");
        assert!(repo.manifest.import.is_empty() && repo.lock.skills.is_empty());

        // Appearing after the last check: the install itself refuses.
        let other = repo.imported_dir().join("late");
        let mut late = fetch_link(repo.root(), &src.origin(), Some("skills/web"), None, None).unwrap();
        late.stage().unwrap();
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("notes.md"), "mine\n").unwrap();
        let err = late.staged.take().unwrap().install_new(&other).unwrap_err().to_string();
        assert!(err.contains("appeared while the import was being added"), "{err}");
        assert_eq!(fs::read_to_string(other.join("notes.md")).unwrap(), "mine\n");
        assert_eq!(stages(&repo), 0);
    }

    /// A failed install (say EXDEV) leaves the previous version in place.
    #[test]
    fn a_failed_update_install_keeps_the_previous_import() {
        let src = Source::new();
        let mut repo = src.repo();
        repo.add_link(None, &src.origin(), Some("skills/pdf"), None, None).unwrap();
        let before = (fs::read_to_string(repo.imported_dir().join("pdf/SKILL.md")).unwrap(), repo.lock.clone());
        let origin = src.tmp.path().join("origin");
        fs::write(origin.join("skills/pdf/SKILL.md"), "---\ndescription: v2\n---\n").unwrap();
        commit_all(&origin);

        source::FAIL_INSTALL.set(true);
        assert!(repo.update(Some("pdf"), false).is_err());
        assert_eq!(fs::read_to_string(repo.imported_dir().join("pdf/SKILL.md")).unwrap(), before.0);
        assert_eq!(repo.lock.skills, before.1.skills);
        assert_eq!(Repo::open(repo.root()).unwrap().import_status().unwrap()[0].state, ImportState::Clean);
        assert_eq!(stages(&repo), 0);
        repo.update(Some("pdf"), false).unwrap();
        assert!(fs::read_to_string(repo.imported_dir().join("pdf/SKILL.md")).unwrap().contains("v2"));
    }

    /// With `imported/` on another directory (or file system), staging
    /// happens there, next to the destination.
    #[cfg(unix)]
    #[test]
    fn staging_happens_where_imported_really_is() {
        let src = Source::new();
        let mut repo = src.repo();
        let elsewhere = src.tmp.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::remove_dir_all(repo.imported_dir()).unwrap();
        std::os::unix::fs::symlink(&elsewhere, repo.imported_dir()).unwrap();
        let mut pdf = fetch_link(repo.root(), &src.origin(), Some("skills/pdf"), None, None).unwrap();
        pdf.stage().unwrap();
        let staged = fs::read_dir(&elsewhere).unwrap().filter_map(|e| e.ok()).count();
        assert_eq!(staged, 1, "staged next to the real destination");
        repo.add_fetched(None, pdf).unwrap();
        assert!(elsewhere.join("pdf/SKILL.md").is_file());
        repo.update(Some("pdf"), true).unwrap();
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 1, "only the import is left");
    }

    /// A worktree whose `.gitignore` doesn't mention staging directories
    /// still never shows one in its draft, and discovery skips them.
    #[test]
    fn staging_never_shows_in_the_draft_or_as_a_skill() {
        let src = Source::new();
        let main = src.tmp.path().join("repo");
        fs::write(main.join(".gitignore"), "notes/\n").unwrap();
        commit_all(&main);
        let wt = src.tmp.path().join("wt");
        git(&main, &["worktree", "add", "--quiet", wt.to_str().unwrap()]);
        let repo = Repo::open(&wt).unwrap();
        assert!(repo.root().join(".git").is_file());

        let mut pdf = fetch_link(repo.root(), &src.origin(), Some("skills/pdf"), None, None).unwrap();
        pdf.stage().unwrap();
        assert_eq!(stages(&repo), 1);
        let exclude = fs::read_to_string(main.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.matches(".sk1ll-stage-*").count(), 1, "{exclude}");
        assert_eq!(repo.draft().unwrap(), Vec::new());
        // Even without the exclude line, draft and discovery skip staging.
        fs::write(main.join(".git/info/exclude"), "").unwrap();
        fs::create_dir_all(repo.imported_dir().join(".sk1ll-stage-x")).unwrap();
        fs::write(repo.imported_dir().join(".sk1ll-stage-x/SKILL.md"), "staged").unwrap();
        assert_eq!(repo.draft().unwrap(), Vec::new());
        assert!(repo.skills().unwrap().is_empty());
        // Staging again restores the line, once.
        let mut web = fetch_link(repo.root(), &src.origin(), Some("skills/web"), None, None).unwrap();
        web.stage().unwrap();
        web.stage().unwrap();
        assert_eq!(fs::read_to_string(main.join(".git/info/exclude")).unwrap(), ".sk1ll-stage-*\n");
    }

    /// The legacy `add_import` takes the same safe path as `add_link`.
    #[test]
    fn add_import_never_removes_what_appeared_while_staging() {
        let src = Source::new();
        let mut repo = src.repo();
        let origin = src.tmp.path().join("origin");
        fs::write(origin.join("skills/pdf/.gitattributes"), "SKILL.md filter=probe\n").unwrap();
        commit_all(&origin);
        let dest = repo.imported_dir().join("pdf");
        let appear = format!("sh -c 'mkdir -p \"{0}\" && echo mine > \"{0}/notes.md\"; cat'", dest.display());
        let _filter = smudge(&appear);
        let import = Import { source: src.origin(), path: "skills/pdf".into(), git_ref: "HEAD".into() };
        let err = repo.add_import("pdf", import).unwrap_err().to_string();
        assert!(err.contains("not a recorded import"), "{err}");
        assert_eq!(fs::read_to_string(dest.join("notes.md")).unwrap(), "mine\n");
        assert!(repo.manifest.import.is_empty());
    }

    /// Install and rollback both fail: the error says where the previous
    /// version is, and recovery (before the next sync) puts it back, so a
    /// sync keeps its harness entry.
    #[test]
    fn a_failed_rollback_is_recovered_before_the_next_sync() {
        use crate::sync::{self, ActionKind, PlanOptions};
        let src = Source::new();
        let mut repo = src.repo();
        repo.add_link(None, &src.origin(), Some("skills/pdf"), None, None).unwrap();
        let skills = src.tmp.path().join("harness");
        let machine = format!("id = 't'\n[harnesses.h]\nskills = '{}'\nmode = 'copy'\n", skills.display());
        fs::write(src.tmp.path().join("machine.toml"), machine).unwrap();
        let harnesses = crate::MachineConfig::load(&src.tmp.path().join("machine.toml")).unwrap().harnesses();
        let harnesses: std::collections::BTreeMap<_, _> = harnesses.into_iter().filter(|(n, _)| n == "h").collect();
        let assignment = crate::Assignment::default();
        for a in sync::plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap() {
            sync::apply(&repo, &a).unwrap();
        }
        assert!(skills.join("pdf/SKILL.md").is_file());
        let before = fs::read_to_string(repo.imported_dir().join("pdf/SKILL.md")).unwrap();
        let origin = src.tmp.path().join("origin");
        fs::write(origin.join("skills/pdf/SKILL.md"), "---\ndescription: v2\n---\n").unwrap();
        commit_all(&origin);

        source::FAIL_INSTALL.set(true);
        source::FAIL_RESTORE.set(true);
        let err = repo.update(Some("pdf"), false).unwrap_err().to_string();
        let backup = repo.imported_dir().join(".sk1ll-stage-old-pdf");
        assert!(err.contains(&backup.display().to_string()) && err.contains("injected restore failure"), "{err}");
        assert!(!repo.imported_dir().join("pdf").exists() && backup.is_dir());

        assert_eq!(repo.recover_imports().unwrap(), ["pdf"]);
        assert_eq!(fs::read_to_string(repo.imported_dir().join("pdf/SKILL.md")).unwrap(), before);
        let actions = sync::plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        assert!(actions.iter().all(|a| a.kind != ActionKind::Remove), "{actions:?}");
        assert!(repo.recover_imports().unwrap().is_empty());

        // A backup left next to an installed import (a crash after the
        // install) is just deleted.
        fs::create_dir_all(&backup).unwrap();
        assert!(repo.recover_imports().unwrap().is_empty());
        assert!(!backup.exists() && repo.imported_dir().join("pdf/SKILL.md").is_file());
    }

    /// The exclude line is appended, never rewriting what's there (even
    /// bytes that aren't UTF-8), and only once with stages racing.
    #[test]
    fn the_exclude_line_is_only_ever_appended_once() {
        let src = Source::new();
        let repo = src.repo();
        let exclude = repo.root().join(".git/info/exclude");
        fs::write(&exclude, b"# mine\n\xff\xfe not utf-8").unwrap();
        let racers: Vec<_> = (0..8)
            .map(|_| {
                let root = repo.root().to_path_buf();
                std::thread::spawn(move || source::exclude_stages(&root))
            })
            .collect();
        for r in racers {
            r.join().unwrap();
        }
        assert_eq!(fs::read(&exclude).unwrap(), b"# mine\n\xff\xfe not utf-8\n.sk1ll-stage-*\n");
    }

    /// A copy of the flat directory `from` at `to`.
    fn copy_dir(from: &Path, to: &Path) {
        fs::create_dir_all(to).unwrap();
        for e in fs::read_dir(from).unwrap() {
            let e = e.unwrap();
            fs::copy(e.path(), to.join(e.file_name())).unwrap();
        }
    }

    /// Recovery never deletes a kept previous version unless what is
    /// installed is exactly what the lockfile records.
    #[test]
    fn recovery_never_deletes_the_backup_on_doubt() {
        let src = Source::new();
        let mut repo = src.repo();
        repo.add_link(None, &src.origin(), Some("skills/pdf"), None, None).unwrap();
        let dest = repo.imported_dir().join("pdf");
        let backup = repo.imported_dir().join(".sk1ll-stage-old-pdf");
        let recorded = repo.imported_dir().join("recorded");
        copy_dir(&dest, &recorded);
        let restore = |at_dest: &dyn Fn(&Path)| {
            let _ = fs::remove_dir_all(&dest).or_else(|_| fs::remove_file(&dest));
            let _ = fs::remove_dir_all(&backup);
            copy_dir(&recorded, &backup);
            at_dest(&dest);
        };
        let empty = |d: &Path| fs::create_dir_all(d).unwrap();
        let user = |d: &Path| {
            fs::create_dir_all(d).unwrap();
            fs::write(d.join("notes.md"), "mine").unwrap();
        };
        let file = |d: &Path| fs::write(d, "a file").unwrap();
        let partial = |d: &Path| {
            copy_dir(&recorded, d);
            fs::remove_file(d.join("SKILL.md")).unwrap();
        };
        // A crash after the install, before the lock was written.
        let unrecorded = |d: &Path| {
            copy_dir(&recorded, d);
            fs::write(d.join("SKILL.md"), "---\ndescription: v2\n---\n").unwrap();
        };
        type Make<'a> = &'a dyn Fn(&Path);
        let cases: [(&str, Make); 5] =
            [("empty", &empty), ("user", &user), ("file", &file), ("partial", &partial), ("unrecorded", &unrecorded)];
        for (case, at_dest) in cases {
            restore(at_dest);
            let err = repo.recover_imports().unwrap_err().to_string();
            assert!(err.contains(&backup.display().to_string()) && err.contains(&dest.display().to_string()), "{err}");
            assert!(err.contains("is the recorded version") && err.contains("sync again"), "{case}: {err}");
            assert_eq!(crate::hash::hash_dir(&backup).unwrap(), repo.lock.skills["pdf"].hash, "{case}");
            assert!(fs::symlink_metadata(&dest).is_ok(), "{case}: left alone");
            // An update starting on top of it refuses the same way.
            let err = repo.update(Some("pdf"), true).unwrap_err().to_string();
            assert!(err.contains("is the recorded version"), "{case}: {err}");
            assert!(backup.is_dir(), "{case}");
        }

        // Neither matches the lock: still both kept.
        restore(&user);
        fs::write(backup.join("SKILL.md"), "edited").unwrap();
        let err = repo.recover_imports().unwrap_err().to_string();
        assert!(err.contains("is not what sk1ll.lock records") && backup.is_dir(), "{err}");

        // The replacement completed and was recorded: only then does the
        // backup go.
        restore(&|d: &Path| copy_dir(&recorded, d));
        fs::write(backup.join("SKILL.md"), "the old version").unwrap();
        assert!(repo.recover_imports().unwrap().is_empty());
        assert!(!backup.exists() && dest.join("SKILL.md").is_file());

        // Nothing installed: the backup is put back.
        restore(&|_: &Path| {});
        assert_eq!(repo.recover_imports().unwrap(), ["pdf"]);
        assert_eq!(crate::hash::hash_dir(&dest).unwrap(), repo.lock.skills["pdf"].hash);
    }
}
