use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::hash::vendor_hash;
use crate::lock::LockEntry;
use crate::manifest::{self, Import};
use crate::skill::IMPORTED_DIR;
use crate::{Error, IoExt, ItemId, Repo, Result, skill, source};

#[derive(Debug, Clone, Serialize)]
pub struct UpdateReport {
    pub name: String,
    pub old_commit: Option<String>,
    pub new_commit: String,
    pub changed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImportState {
    /// Vendored content matches the lockfile.
    Clean,
    /// Vendored content was edited locally; `update` would overwrite it.
    Modified,
    /// Declared in the manifest but not vendored yet.
    Missing,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub fn add_import(&mut self, name: &str, mut import: Import) -> Result<UpdateReport> {
        self.check_free(name)?;
        if let Some(id) = &import.id {
            self.check_item_id(Path::new(skill::IMPORTED_DIR).join(name), id.clone())?;
        } else {
            import.id = Some(self.new_item_id()?);
        }
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
        let mut import = fetched.import()?;
        import.id = Some(self.new_item_id()?);
        let staged = fetched.staged.take().expect("staged above");
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
                    Some(entry) if vendor_hash(&dir)? == entry.hash => ImportState::Clean,
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
        for parent in [self.imported_dir(), self.personal_dir()] {
            if !parent.symlink_metadata().at(&parent)?.is_dir() {
                return Err(Error::Invalid(format!("{} is not a content directory", parent.display())));
            }
        }
        if !from.symlink_metadata().is_ok_and(|meta| meta.is_dir()) {
            return Err(Error::Invalid(format!("{} is not a vendored import directory", from.display())));
        }
        if to.symlink_metadata().is_ok() {
            return Err(Error::Invalid(format!("personal skill `{name}` already exists")));
        }
        let relative = Path::new(skill::IMPORTED_DIR).join(name);
        self.assign_item_ids(|path| path == relative)?;
        let id = self
            .resolved_ids()?
            .get(&relative)
            .cloned()
            .flatten()
            .ok_or_else(|| Error::Invalid(format!("no vendored import named `{name}`")))?;
        let manifest = manifest::stage_remove_import(self.root(), name)?;
        let sidecar = to.join(skill::ID_FILE);
        let backup = source::unique(&to);
        crate::sync::rename_noreplace(&from, &to).at(&to)?;
        let mut saved = false;
        let result = (|| {
            if sidecar.symlink_metadata().is_ok() {
                crate::sync::rename_noreplace(&sidecar, &backup).at(&sidecar)?;
                saved = true;
            }
            id.write_skill(&to)?;
            manifest.replace()
        })();
        if let Err(error) = result {
            if saved {
                fs::rename(&backup, &sidecar).at(&sidecar)?;
            } else if sidecar.symlink_metadata().is_ok() {
                // Only remove a sidecar we made, never the one we failed to save.
                if ItemId::read_skill(&to).is_ok_and(|found| found.as_ref() == Some(&id)) {
                    fs::remove_file(&sidecar).at(&sidecar)?;
                }
            }
            crate::sync::rename_noreplace(&to, &from).at(&from)?;
            return Err(error);
        }
        if saved {
            if backup.symlink_metadata().at(&backup)?.is_dir() {
                fs::remove_dir_all(&backup).at(&backup)?;
            } else {
                fs::remove_file(&backup).at(&backup)?;
            }
        }
        self.manifest.import.remove(name);
        self.lock.skills.remove(name);
        self.lock.save(self.root())
    }

    /// Records `name`'s freshly vendored content at `commit` in the lockfile.
    fn lock_vendored(&mut self, name: &str, import: &Import, commit: String) -> Result<UpdateReport> {
        let hash = vendor_hash(&self.imported_dir().join(name))?;
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
        Ok(Import { id: None, source: r.source.clone(), path: self.skill()?.path.clone(), git_ref: r.git_ref.clone() })
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
