use std::fs;
use std::path::{Path, PathBuf};

use crate::instructions::{self, INSTRUCTIONS_DIR, Rule};
use crate::lock::Lockfile;
use crate::manifest::{MANIFEST_FILE, Manifest};
use crate::skill::{self, IMPORTED_DIR, PERSONAL_DIR, Skill};
use crate::{Error, IoExt, ItemId, ItemRef, Result};

/// The message of the commit [`Repo::init_committed`] makes.
const INIT_COMMIT_MESSAGE: &str = "Initialize skills repo";

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

/// Why [`Repo::rename_item`] failed.
#[derive(Debug)]
pub enum RenameError {
    /// Nothing was renamed: the new name was taken (it may have appeared
    /// since a [`Repo::check_rename`]), say.
    NotMoved(Error),
    /// The item has its new name, but writing its metadata failed.
    Moved(Error),
}

impl std::fmt::Display for RenameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotMoved(e) => e.fmt(f),
            Self::Moved(e) => write!(f, "renamed, but updating its metadata failed: {e}"),
        }
    }
}

impl std::error::Error for RenameError {}

impl From<RenameError> for Error {
    fn from(e: RenameError) -> Self {
        match e {
            RenameError::NotMoved(e) => e,
            RenameError::Moved(e) => Error::Invalid(format!("renamed, but updating its metadata failed: {e}")),
        }
    }
}

impl Repo {
    /// Creates a new personal Skill or Rule in the Draft.
    pub fn create_item(&self, kind: &str, name: &str, description: &str) -> Result<()> {
        let quoted = serde_json::to_string(description).expect("string");
        self.create_item_with(kind, name, &format!("---\nname: {name}\ndescription: {quoted}\n---\n\n"))
    }

    /// Creates a new personal Skill (its `SKILL.md`) or Rule with `body`.
    pub(crate) fn create_item_with(&self, kind: &str, name: &str, body: &str) -> Result<()> {
        crate::imports::validate_name(name)?;
        let path = match kind {
            "skill" => {
                if self.manifest.import.contains_key(name)
                    || self.root.join("imported").join(name).symlink_metadata().is_ok()
                {
                    return Err(Error::Invalid(format!("name `{name}` is taken")));
                }
                self.root.join("personal").join(name)
            }
            "rule" => self.root.join(INSTRUCTIONS_DIR).join(format!("{name}.md")),
            _ => return Err(Error::Invalid("expected skill or rule".into())),
        };
        if path.symlink_metadata().is_ok() {
            return Err(Error::Invalid(format!("name `{name}` is taken")));
        }
        let parent = path.parent().unwrap();
        if parent.symlink_metadata().is_ok_and(|m| m.is_symlink()) {
            return Err(Error::Invalid("content directory is a symlink".into()));
        }
        fs::create_dir_all(parent).at(&path)?;
        let id = self.new_item_id()?;
        if kind == "skill" {
            let stage = crate::source::unique(parent);
            fs::create_dir(&stage).at(&stage)?;
            let file = stage.join("SKILL.md");
            let created = fs::write(&file, body)
                .at(&file)
                .and_then(|()| id.write_skill(&stage))
                .and_then(|()| crate::sync::rename_noreplace(&stage, &path).at(&path));
            if created.is_err() {
                let _ = fs::remove_dir_all(&stage);
            }
            created
        } else {
            let text = instructions::with_id(body, &path, &id)?;
            let staged = crate::config_file::Staged::write(&path, text.as_bytes(), None)?;
            if staged.place_new()? { Ok(()) } else { Err(Error::Invalid(format!("name `{name}` is taken"))) }
        }
    }

    /// An existing personal Skill directory or Rule file. Imports are removed
    /// with `remove_import`, so a skill must be under `personal/`.
    fn personal_item(&self, kind: &str, name: &str) -> Result<PathBuf> {
        crate::imports::validate_name(name)?;
        let (parent, path) = match kind {
            "skill" => (self.personal_dir(), self.personal_dir().join(name)),
            "rule" => (self.root.join(INSTRUCTIONS_DIR), self.root.join(INSTRUCTIONS_DIR).join(format!("{name}.md"))),
            _ => return Err(Error::Invalid("expected skill or rule".into())),
        };
        if parent.symlink_metadata().is_ok_and(|m| m.is_symlink()) {
            return Err(Error::Invalid("content directory is a symlink".into()));
        }
        let found = path.symlink_metadata().is_ok_and(|m| if kind == "skill" { m.is_dir() } else { m.is_file() });
        if !found {
            return Err(Error::Invalid(format!("no personal {kind} named `{name}`")));
        }
        Ok(path)
    }

    /// Deletes a personal Skill or Rule; the deletion is a Draft change.
    pub fn delete_item(&self, kind: &str, name: &str) -> Result<()> {
        let path = self.personal_item(kind, name)?;
        if kind == "skill" { fs::remove_dir_all(&path).at(&path) } else { fs::remove_file(&path).at(&path) }
    }

    /// Checks that [`Repo::rename_item`] can rename `name` to `to`, without
    /// changing anything.
    pub fn check_rename(&self, kind: &str, name: &str, to: &str) -> Result<()> {
        self.rename_paths(kind, name, to).map(drop)
    }

    /// The item to rename, its destination and the file whose frontmatter
    /// names it.
    fn rename_paths(&self, kind: &str, name: &str, to: &str) -> Result<(PathBuf, PathBuf, PathBuf)> {
        let from = self.personal_item(kind, name)?;
        crate::imports::validate_name(to)?;
        if name == to {
            return Err(Error::Invalid("the new name is the current name".into()));
        }
        let (dest, file) = match kind {
            "skill" => {
                if self.manifest.import.contains_key(to) || self.imported_dir().join(to).symlink_metadata().is_ok() {
                    return Err(Error::Invalid(format!("name `{to}` is taken")));
                }
                let dest = self.personal_dir().join(to);
                (dest.clone(), dest.join("SKILL.md"))
            }
            _ => {
                let dest = self.root.join(INSTRUCTIONS_DIR).join(format!("{to}.md"));
                (dest.clone(), dest)
            }
        };
        if dest.symlink_metadata().is_ok() && !case_alias(&from, &dest)? {
            return Err(Error::Invalid(format!("name `{to}` is taken")));
        }
        Ok((from, dest, file))
    }

    /// Renames a personal Skill or Rule in the Draft (the old name deleted, the
    /// new one added), keeping its Item ID and updating a matching frontmatter `name:`.
    /// A failure says whether the item had moved by then.
    pub fn rename_item(&self, kind: &str, name: &str, to: &str) -> std::result::Result<(), RenameError> {
        let (from, dest, file) = self.rename_paths(kind, name, to).map_err(RenameError::NotMoved)?;
        let case_only = case_alias(&from, &dest).map_err(RenameError::NotMoved)?;
        let relative = from.strip_prefix(&self.root).unwrap();
        let ids = self.resolved_ids().map_err(RenameError::NotMoved)?;
        let id = ids
            .get(relative)
            .ok_or_else(|| RenameError::NotMoved(Error::Invalid(format!("no personal {kind} named `{name}`"))))?;
        let mut pinned_skill = false;
        let mut original_rule = None;
        let renamed_rule = if kind == "rule" {
            let original = fs::read_to_string(&from).at(&from).map_err(RenameError::NotMoved)?;
            let renamed = rename_frontmatter(&original, name, to);
            if instructions::explicit_id(&original, &from).map_err(RenameError::NotMoved)?.is_none() {
                let pinned = instructions::with_id(&original, &from, id).map_err(RenameError::NotMoved)?;
                crate::config_file::Staged::write(&from, pinned.as_bytes(), crate::config_file::existing_mode(&from))
                    .and_then(crate::config_file::Staged::replace)
                    .map_err(RenameError::NotMoved)?;
                original_rule = Some(original);
                rename_frontmatter(&pinned, name, to)
            } else {
                renamed
            }
        } else {
            if ItemId::read_skill(&from).map_err(RenameError::NotMoved)?.is_none() {
                id.write_skill(&from).map_err(RenameError::NotMoved)?;
                pinned_skill = true;
            }
            None
        };
        let moved = if case_only {
            let stage = crate::source::unique(from.parent().unwrap());
            match crate::sync::rename_noreplace(&from, &stage) {
                Ok(()) => {
                    let moved = crate::sync::rename_noreplace(&stage, &dest);
                    if moved.is_err() {
                        crate::sync::rename_noreplace(&stage, &from).at(&stage).map_err(RenameError::Moved)?;
                    }
                    moved
                }
                Err(error) => Err(error),
            }
        } else {
            crate::sync::rename_noreplace(&from, &dest)
        };
        if let Err(error) = moved {
            if pinned_skill {
                let sidecar = from.join(skill::ID_FILE);
                fs::remove_file(&sidecar).at(&sidecar).map_err(RenameError::NotMoved)?;
            }
            if let Some(original) = original_rule {
                crate::config_file::Staged::write(&from, original.as_bytes(), crate::config_file::existing_mode(&from))
                    .and_then(crate::config_file::Staged::replace)
                    .map_err(RenameError::NotMoved)?;
            }
            return Err(RenameError::NotMoved(if error.kind() == std::io::ErrorKind::AlreadyExists {
                Error::Invalid(format!("name `{to}` is taken"))
            } else {
                Error::Io { path: dest, source: error }
            }));
        }
        // Only a regular file is rewritten: a symlinked SKILL.md points elsewhere.
        let Ok(meta) = file.symlink_metadata() else { return Ok(()) };
        if !meta.is_file() {
            return Ok(());
        }
        let renamed = if kind == "rule" {
            renamed_rule
        } else {
            fs::read_to_string(&file).ok().and_then(|text| rename_frontmatter(&text, name, to))
        };
        let Some(renamed) = renamed else { return Ok(()) };
        crate::config_file::Staged::write(&file, renamed.as_bytes(), crate::config_file::existing_mode(&file))
            .and_then(crate::config_file::Staged::replace)
            .map_err(RenameError::Moved)
    }

    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        if !root.join(MANIFEST_FILE).is_file() {
            return Err(Error::Invalid(format!("no {MANIFEST_FILE} in {} (run `sk1ll init`?)", root.display())));
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
    /// the [default location](Self::default_location). Any other git repo is
    /// never picked, so recovery commands can't discard an unrelated
    /// project's changes.
    pub fn locate_git(cwd: &Path) -> Option<PathBuf> {
        cwd.ancestors()
            .find(|d| d.join(MANIFEST_FILE).is_file() || Self::is_skills_repo(d))
            .map(Path::to_path_buf)
            .or_else(Self::default_location)
    }

    /// Where the repo lives unless one is picked: `$XDG_DATA_HOME/sk1ll/repo`,
    /// usually `~/.local/share/sk1ll/repo`. sk1ll manages it, so it stays out
    /// of sight like its config and cache.
    pub fn default_location() -> Option<PathBuf> {
        dirs::data_dir().map(|d| d.join("sk1ll").join("repo"))
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

    /// Nearest ancestor of `cwd` containing `sk1ll.toml`, else the
    /// [default location](Self::default_location).
    pub fn locate(cwd: &Path) -> Option<PathBuf> {
        cwd.ancestors().find(|d| d.join(MANIFEST_FILE).is_file()).map(Path::to_path_buf).or_else(Self::default_location)
    }

    /// Creates the repo layout and a starter manifest, and `git init`s it.
    /// The starter files stay uncommitted; see [`init_committed`](Self::init_committed).
    pub fn init(root: &Path) -> Result<Self> {
        Self::init_with(root).map(|(repo, _)| repo)
    }

    /// Like [`init`](Self::init), and when it creates the git repo, commits
    /// the starter files it wrote, so a new repo starts with an empty Draft.
    /// An existing git repo gets the files as Draft, as with `init`.
    pub fn init_committed(root: &Path) -> Result<Self> {
        let (repo, created) = Self::init_with(root)?;
        if let Some(written) = created {
            let mut items: Vec<crate::draft::ItemRef> =
                written.iter().map(|p| crate::draft::ItemRef::for_path(p)).collect();
            items.sort();
            items.dedup();
            repo.commit(&items, INIT_COMMIT_MESSAGE, None)?;
        }
        Ok(repo)
    }

    /// The starter files it wrote, when it also created the git repo.
    fn init_with(root: &Path) -> Result<(Self, Option<Vec<PathBuf>>)> {
        let directory = crate::placement::Directory::open(root, true).at(root)?;
        let mut files = vec![(PathBuf::from(MANIFEST_FILE), MANIFEST_TEMPLATE)];
        for sub in [PERSONAL_DIR, IMPORTED_DIR, INSTRUCTIONS_DIR] {
            files.push((Path::new(sub).join(".gitkeep"), ""));
        }
        // Preserve a regular existing ignore file, but never follow a link.
        let ignore = root.join(".gitignore");
        match ignore.symlink_metadata() {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => return Err(Error::Invalid(format!("{} is not a regular file", ignore.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => files.push((".gitignore".into(), ".sk1ll-stage-*\n")),
            Err(e) => return Err(e).at(&ignore),
        }
        // Preserve existing empty placeholders without opening them for write.
        // Every other existing destination is refused before any publication.
        let mut missing = Vec::new();
        for (path, bytes) in files {
            match directory.check_missing(&path) {
                Err(e)
                    if e.kind() == std::io::ErrorKind::AlreadyExists
                        && path.file_name().is_some_and(|n| n == ".gitkeep")
                        && root.join(&path).symlink_metadata().is_ok_and(|m| m.is_file() && m.len() == 0) => {}
                result => {
                    result.at(root.join(&path))?;
                    missing.push((path, bytes));
                }
            }
        }
        let written: Vec<PathBuf> = missing.iter().map(|(path, _)| path.clone()).collect();
        for (path, bytes) in missing {
            directory.create_file(&path, bytes.as_bytes(), false).at(root.join(path))?;
        }
        let mut created = None;
        if !root.join(".git").exists() {
            // Always on the fleet's branch (commit and the daemon need it),
            // whatever git defaults to (`master` without `init.defaultBranch`).
            // `symbolic-ref` on the unborn HEAD works where `init -b` doesn't (git < 2.28).
            let git = || crate::git::Git::new(root);
            if git().args(["init", "--quiet"]).run().is_ok() {
                git().args(["symbolic-ref", "HEAD", &format!("refs/heads/{}", crate::FLEET_BRANCH)]).run()?;
                created = Some(written);
            }
        }
        Ok((Self::open(root)?, created))
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

    pub(crate) fn resolved_ids(&self) -> Result<crate::item_id::Resolved> {
        crate::item_id::resolve(self.item_id_claims()?)
    }

    fn item_id_claims(&self) -> Result<crate::item_id::Claims> {
        let manifest = Manifest::load(&self.root)?;
        crate::item_id::claims_on_disk(&self.root, &manifest, |_| true)
    }

    pub(crate) fn new_item_id(&self) -> Result<ItemId> {
        let ids = self.resolved_ids()?;
        loop {
            let id = ItemId::generate();
            if !ids.values().any(|claimed| claimed == &id) {
                return Ok(id);
            }
        }
    }

    pub(crate) fn check_item_id(&self, path: PathBuf, id: ItemId) -> Result<()> {
        let mut claims = self.item_id_claims()?;
        claims.insert(path, Some(id));
        crate::item_id::check_claims(&claims)
    }

    pub fn skills(&self) -> Result<Vec<Skill>> {
        skill::discover_with(&self.root, &self.resolved_ids()?)
    }

    pub fn rules(&self) -> Result<Vec<Rule>> {
        instructions::discover_with(&self.root, &self.resolved_ids()?)
    }

    pub fn content(&self) -> Result<(Vec<Skill>, Vec<Rule>)> {
        let ids = self.resolved_ids()?;
        Ok((skill::discover_with(&self.root, &ids)?, instructions::discover_with(&self.root, &ids)?))
    }

    /// The Item IDs of repo Skills and Rules. Personal skills take precedence
    /// over Imports of the same name, as they do for sync.
    pub fn item_ids(&self) -> Result<Vec<(ItemRef, ItemId)>> {
        let (skills, rules) = self.content()?;
        Ok(skills
            .into_iter()
            .map(|s| (ItemRef::Skill(s.name), s.id))
            .chain(rules.into_iter().map(|r| (ItemRef::Rule(r.name), r.id)))
            .collect())
    }
}

/// A case-insensitive lookup must alias the source, with no separate directory
/// entry at the destination spelling (a distinct hard link can share an inode).
pub(crate) fn case_alias(from: &Path, dest: &Path) -> Result<bool> {
    let (Some(from_name), Some(dest_name)) =
        (from.file_name().and_then(|n| n.to_str()), dest.file_name().and_then(|n| n.to_str()))
    else {
        return Ok(false);
    };
    if !from_name.eq_ignore_ascii_case(dest_name) || !dest.symlink_metadata().is_ok_and(|m| !m.is_symlink()) {
        return Ok(false);
    }
    let parent = from.parent().unwrap();
    for entry in fs::read_dir(parent).at(parent)? {
        if entry.at(parent)?.file_name() == dest_name {
            return Ok(false);
        }
    }
    #[cfg(unix)]
    return Ok(crate::sync::displaced::same_file(from, dest));
    #[cfg(not(unix))]
    Ok(from.canonicalize().at(from)? == dest.canonicalize().at(dest)?)
}

/// `text` with a frontmatter `name: <from>` line (quoted or not) set to `to`.
fn rename_frontmatter(text: &str, from: &str, to: &str) -> Option<String> {
    let (Some(frontmatter), _) = skill::split_frontmatter(text) else { return None };
    // `frontmatter` borrows from `text`, so this is its byte offset.
    let mut at = frontmatter.as_ptr() as usize - text.as_ptr() as usize;
    for line in frontmatter.split_inclusive('\n') {
        let value = line.strip_prefix("name:").map(|v| v.trim().trim_matches(|c| c == '"' || c == '\''));
        if value == Some(from) {
            let ending = if line.ends_with("\r\n") {
                "\r\n"
            } else if line.ends_with('\n') {
                "\n"
            } else {
                ""
            };
            // Quote names YAML would read as another type (`true`, `null`, `123`).
            let plain =
                matches!(serde_yaml_ng::from_str::<serde_yaml_ng::Value>(to), Ok(serde_yaml_ng::Value::String(_)));
            let value = if plain { to.to_string() } else { format!("\"{to}\"") };
            return Some(format!("{}name: {value}{ending}{}", &text[..at], &text[at + line.len()..]));
        }
        at += line.len();
    }
    None
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
    fn a_committed_init_starts_with_an_empty_draft_but_leaves_an_existing_repo_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        let repo = Repo::init_committed(&root).unwrap();
        assert!(repo.draft().unwrap().is_empty());
        let log = crate::git::Git::new(&root).args(["log", "--format=%s"]).text().unwrap();
        assert_eq!(log, INIT_COMMIT_MESSAGE);

        // An existing git repo keeps the starter files as Draft.
        let existing = tmp.path().join("existing");
        fs::create_dir_all(&existing).unwrap();
        git(&existing, &["init", "--quiet", "-b", crate::FLEET_BRANCH]);
        let repo = Repo::init_committed(&existing).unwrap();
        assert!(!repo.draft().unwrap().is_empty());
        assert!(crate::git::Git::new(&existing).args(["rev-parse", "--verify", "--quiet", "HEAD"]).text().is_err());
    }

    #[test]
    fn init_preserves_existing_destinations_and_refuses_symlink_ancestors() {
        for scenario in ["keep", "ancestor", "manifest-link", "ignore-link"] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("repo");
            fs::create_dir(&root).unwrap();
            let outside = tmp.path().join("outside");
            fs::create_dir(&outside).unwrap();
            fs::write(outside.join("notes"), "private").unwrap();
            match scenario {
                "keep" => {
                    fs::create_dir(root.join("personal")).unwrap();
                    fs::write(root.join("personal/.gitkeep"), "private").unwrap();
                }
                "ancestor" => std::os::unix::fs::symlink(&outside, root.join("personal")).unwrap(),
                "manifest-link" => {
                    std::os::unix::fs::symlink(outside.join("missing"), root.join(MANIFEST_FILE)).unwrap()
                }
                _ => std::os::unix::fs::symlink(outside.join("missing"), root.join(".gitignore")).unwrap(),
            }
            assert!(Repo::init(&root).is_err(), "{scenario}");
            assert_eq!(fs::read(outside.join("notes")).unwrap(), b"private");
            assert!(!outside.join("missing").exists());
            assert!(!outside.join(".gitkeep").exists());
            if scenario == "keep" {
                assert_eq!(fs::read(root.join("personal/.gitkeep")).unwrap(), b"private");
            }
        }
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
            assert_eq!(Repo::locate_git(&cwd), Repo::default_location(), "{}", cwd.display());
        }
        for sub in [PERSONAL_DIR, IMPORTED_DIR, INSTRUCTIONS_DIR] {
            fs::create_dir_all(project.join(sub)).unwrap();
        }
        assert_eq!(Repo::locate_git(&project), Repo::default_location());
        // Not in a git repo at all.
        let plain = tmp.path().join("plain");
        fs::create_dir_all(&plain).unwrap();
        assert_eq!(Repo::locate_git(&plain), Repo::default_location());
        // A skills repo that was never committed can't be told apart either.
        let fresh = tmp.path().join("fresh");
        Repo::init(&fresh).unwrap();
        fs::remove_file(fresh.join(MANIFEST_FILE)).unwrap();
        assert_eq!(Repo::locate_git(&fresh), Repo::default_location());
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
        assert_eq!(Repo::locate_git(&top), Repo::default_location());
        fs::remove_file(root.join(MANIFEST_FILE)).unwrap();
        assert_eq!(Repo::locate_git(&root.join(PERSONAL_DIR)), Some(root.clone()));
        commit_all(&top, "drop manifest");
        assert_eq!(Repo::locate_git(&root.join(PERSONAL_DIR)), Some(root.clone()));
        assert_eq!(Repo::locate_git(&top), Repo::default_location());
    }
}

#[cfg(test)]
mod create_tests {
    use super::*;
    #[test]
    fn creates_draft_content_and_refuses_invalid_or_taken_names() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path()).unwrap();
        repo.create_item("skill", "new-skill", "quotes: \" and\nnewlines").unwrap();
        assert!(dir.path().join("personal/new-skill/SKILL.md").exists());
        assert!(repo.create_item("skill", "new-skill", "").is_err());
        for name in ["", "..", "../escape", ".hidden", "a/b", "with space"] {
            assert!(repo.create_item("rule", name, "").is_err());
        }
        repo.create_item("rule", "style", "House style").unwrap();
        assert!(dir.path().join("instructions/style.md").exists());
        let ids = repo.item_ids().unwrap();
        assert_eq!(ids.len(), 2);
        assert!(ids.iter().all(|(_, id)| ulid::Ulid::from_string(&id.to_string()).is_ok()));
        assert_eq!(ItemId::read_skill(&repo.personal_dir().join("new-skill")).unwrap().as_ref(), Some(&ids[0].1));
        assert!(repo.create_item("rule", "style", "").is_err());
        assert!(repo.create_item("other", "x", "").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rename_never_writes_through_a_planted_stage_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("secret");
        fs::write(&target, "secret").unwrap();
        let repo = Repo::init(dir.path()).unwrap();
        repo.create_item("skill", "pdf", "PDF").unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("personal/pdf/.sk1ll-stage-rename-documents")).unwrap();
        repo.rename_item("skill", "pdf", "documents").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "secret");
        let skill = dir.path().join("personal/documents/SKILL.md");
        assert!(skill.symlink_metadata().unwrap().is_file());
        assert!(fs::read_to_string(skill).unwrap().contains("name: documents"));
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_rename_says_whether_the_item_moved() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path()).unwrap();
        repo.create_item("skill", "pdf", "PDF").unwrap();
        repo.create_item("skill", "other", "").unwrap();
        assert!(matches!(repo.rename_item("skill", "pdf", "other"), Err(RenameError::NotMoved(_))));
        // The directory moves, but its frontmatter can't be rewritten.
        let skill = dir.path().join("personal/pdf");
        fs::set_permissions(&skill, fs::Permissions::from_mode(0o555)).unwrap();
        let moved = repo.rename_item("skill", "pdf", "documents");
        let documents = dir.path().join("personal/documents");
        fs::set_permissions(&documents, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(moved, Err(RenameError::Moved(_))), "{moved:?}");
        assert!(documents.is_dir() && !skill.exists());
    }

    #[test]
    fn deletes_and_renames_only_personal_items() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path()).unwrap();
        repo.create_item("skill", "pdf", "PDF").unwrap();
        fs::write(dir.path().join("personal/pdf/notes.md"), "kept").unwrap();
        repo.create_item("rule", "style", "Style").unwrap();
        fs::create_dir_all(dir.path().join("imported/vendored")).unwrap();
        fs::write(dir.path().join("imported/vendored/SKILL.md"), "---\nname: vendored\n---\n").unwrap();

        // Imports, missing items and escapes are refused.
        for (kind, name) in [
            ("skill", "vendored"),
            ("skill", "missing"),
            ("rule", "missing"),
            ("skill", "../imported"),
            ("other", "pdf"),
        ] {
            assert!(repo.delete_item(kind, name).is_err(), "{kind}:{name}");
            assert!(repo.rename_item(kind, name, "fresh").is_err(), "{kind}:{name}");
        }
        for to in ["vendored", "pdf", "../x", ""] {
            assert!(repo.rename_item("skill", "pdf", to).is_err(), "{to}");
        }
        repo.create_item("skill", "other", "").unwrap();
        assert!(repo.rename_item("skill", "pdf", "other").is_err());

        let before = repo.skills().unwrap().into_iter().find(|s| s.name == "pdf").unwrap();
        repo.rename_item("skill", "pdf", "documents").unwrap();
        assert!(!dir.path().join("personal/pdf").exists());
        let text = fs::read_to_string(dir.path().join("personal/documents/SKILL.md")).unwrap();
        assert_eq!(text, "---\nname: documents\ndescription: \"PDF\"\n---\n\n");
        assert_eq!(fs::read_to_string(dir.path().join("personal/documents/notes.md")).unwrap(), "kept");
        assert_eq!(ItemId::read_skill(&repo.personal_dir().join("documents")).unwrap(), Some(before.id));
        assert!(
            fs::read_dir(repo.personal_dir().join("documents"))
                .unwrap()
                .all(|e| !crate::source::is_staging_path(Path::new(&e.unwrap().file_name())))
        );

        // A `name:` that doesn't match the old name is left alone.
        let rule_id = repo.rules().unwrap().remove(0).id;
        let rule = format!("---\nname: House\nid: \"{rule_id}\"\n---\nbody\n");
        fs::write(dir.path().join("instructions/style.md"), &rule).unwrap();
        repo.rename_item("rule", "style", "house-style").unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("instructions/house-style.md")).unwrap(), rule);
        assert_eq!(repo.rules().unwrap()[0].id, rule_id);

        repo.delete_item("skill", "documents").unwrap();
        repo.delete_item("rule", "house-style").unwrap();
        assert!(!dir.path().join("personal/documents").exists());
        assert!(!dir.path().join("instructions/house-style.md").exists());
        assert!(dir.path().join("imported/vendored/SKILL.md").exists());
    }

    #[test]
    fn derived_ids_agree_across_repos_and_become_explicit_on_rename() {
        let tmp = tempfile::tempdir().unwrap();
        let hand_made = |dir: &str| {
            let repo = Repo::init(&tmp.path().join(dir)).unwrap();
            let skill = repo.personal_dir().join("style");
            fs::create_dir(&skill).unwrap();
            fs::write(skill.join("SKILL.md"), "Skill.\n").unwrap();
            fs::write(repo.root().join("instructions/style.md"), "Rule.\n").unwrap();
            fs::create_dir(repo.imported_dir().join("vendored")).unwrap();
            fs::write(repo.imported_dir().join("vendored/SKILL.md"), "Import.\n").unwrap();
            repo
        };
        let repo = hand_made("first");
        let ids: std::collections::BTreeMap<_, _> = repo.item_ids().unwrap().into_iter().collect();
        for (item, id) in hand_made("second").item_ids().unwrap() {
            assert!(id.to_string().starts_with("d:"));
            assert_eq!(ids[&item], id);
        }
        let skill = &ids[&ItemRef::Skill("style".into())];
        let rule = &ids[&ItemRef::Rule("style".into())];
        assert_ne!(skill, rule);
        assert!(!repo.personal_dir().join("style").join(crate::skill::ID_FILE).exists());
        repo.rename_item("skill", "style", "new-style").unwrap();
        repo.rename_item("rule", "style", "new-style").unwrap();
        assert_eq!(ItemId::read_skill(&repo.personal_dir().join("new-style")).unwrap().as_ref(), Some(skill));
        assert_eq!(repo.rules().unwrap()[0].id, *rule);
        let diff = repo.diff(&ItemRef::Skill("new-style".into())).unwrap();
        assert!(diff.contains("+Skill.") && !diff.contains(crate::skill::ID_FILE));
    }

    #[test]
    fn an_explicit_rename_claim_shadows_a_new_items_derived_id() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::init(tmp.path()).unwrap();
        let write = |name: &str| {
            let path = repo.personal_dir().join(name);
            fs::create_dir(&path).unwrap();
            fs::write(path.join("SKILL.md"), "Skill.\n").unwrap();
        };
        write("pdf");
        let original = repo.skills().unwrap().remove(0).id;
        repo.rename_item("skill", "pdf", "docs").unwrap();
        write("pdf");
        let skills = repo.skills().unwrap();
        assert_eq!(skills[0].id, original);
        assert_ne!(skills[1].id, original);
        assert_eq!(repo.skills().unwrap()[1].id, skills[1].id);
        repo.rename_item("skill", "pdf", "papers").unwrap();
        assert_eq!(repo.skills().unwrap()[1].id, skills[1].id);
    }

    #[test]
    fn copied_skill_ids_report_both_paths_and_block_creation() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::init(tmp.path()).unwrap();
        repo.create_item("skill", "pdf", "PDF").unwrap();
        let copy = repo.personal_dir().join("copy");
        fs::create_dir(&copy).unwrap();
        for file in ["SKILL.md", skill::ID_FILE] {
            fs::copy(repo.personal_dir().join("pdf").join(file), copy.join(file)).unwrap();
        }
        let err = repo.item_ids().unwrap_err();
        assert!(matches!(err, Error::DuplicateItemId { .. }));
        for path in ["personal/pdf/.sk1ll-id", "personal/copy/.sk1ll-id"] {
            assert!(err.to_string().contains(path), "{err}");
        }
        assert!(repo.create_item("rule", "style", "").is_err());
        assert!(!repo.root().join("instructions/style.md").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_derived_skill_stays_at_its_old_name_if_the_id_cannot_be_written() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::init(tmp.path()).unwrap();
        let source = repo.personal_dir().join("pdf");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("SKILL.md"), "PDF\n").unwrap();
        let before = repo.skills().unwrap()[0].id.clone();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o555)).unwrap();
        let result = repo.rename_item("skill", "pdf", "docs");
        let actual = if result.is_ok() { repo.personal_dir().join("docs") } else { source.clone() };
        fs::set_permissions(&actual, fs::Permissions::from_mode(0o755)).unwrap();
        // Users with DAC override can write despite the mode bits.
        if let Err(error) = result {
            assert!(matches!(error, RenameError::NotMoved(_)));
            assert!(source.is_dir());
            assert!(!repo.personal_dir().join("docs").exists());
        }
        assert_eq!(repo.skills().unwrap()[0].id, before);
    }

    #[test]
    fn a_case_only_rename_keeps_the_id_and_uses_the_requested_spelling() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::init_committed(tmp.path()).unwrap();
        repo.create_item("rule", "pdf", "PDF").unwrap();
        let id = repo.rules().unwrap()[0].id.clone();
        let old = ItemRef::Rule("pdf".into());
        let new = ItemRef::Rule("PDF".into());
        repo.commit(std::slice::from_ref(&old), "Add Rule", None).unwrap();
        repo.check_rename("rule", "pdf", "PDF").unwrap();
        repo.rename_item("rule", "pdf", "PDF").unwrap();
        let rules = repo.rules().unwrap();
        assert_eq!(rules[0].id, id);
        assert_eq!(rules[0].name, "PDF");
        assert_eq!(rules.len(), 1);
        repo.restore(&[old.clone(), new.clone()]).unwrap();
        assert_eq!(repo.rules().unwrap()[0].name, "pdf");
        repo.rename_item("rule", "pdf", "PDF").unwrap();
        repo.commit(&[old, new.clone()], "Rename Rule", None).unwrap();
        assert_eq!(crate::content::item_ids_at(repo.root(), "HEAD").unwrap(), [(new, id)].into());
        assert!(repo.draft().unwrap().is_empty());
    }

    #[test]
    fn frontmatter_rename_keeps_everything_else() {
        assert_eq!(
            rename_frontmatter("---\r\nname: 'a'\r\nx: a\r\n---\r\nname: a\r\n", "a", "b").as_deref(),
            Some("---\r\nname: b\r\nx: a\r\n---\r\nname: a\r\n")
        );
        assert_eq!(rename_frontmatter("name: a\n", "a", "b"), None);
        assert_eq!(rename_frontmatter("---\nname: c\n---\n", "a", "b"), None);
        for to in ["true", "null", "123", "1.5"] {
            assert_eq!(rename_frontmatter("---\nname: a\n---\n", "a", to), Some(format!("---\nname: \"{to}\"\n---\n")));
        }
    }
}
