//! Skill and Rule details and bounded content browsing and editing.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
#[cfg(not(unix))]
use walkdir::WalkDir;
#[cfg(unix)]
mod pinned;

use crate::git::Git;
use crate::sync::unmanaged::{self, Variant};
use crate::{Assignment, Change, Error, Harness, IoExt, ItemId, ItemRef, Origin, Repo, Result};

pub const MAX_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContentType {
    Personal,
    Import,
    Rule,
    Unmanaged,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentEntry {
    pub name: String,
    pub item: Option<ItemRef>,
    pub id: Option<ItemId>,
    #[serde(rename = "type")]
    pub kind: ContentType,
    pub description: Option<String>,
    pub draft: Option<Change>,
    pub in_baseline: bool,
    /// Absolute repo folder, or the Rule file. Null for unmanaged skills.
    pub path: Option<PathBuf>,
    pub enabled_harnesses: Vec<String>,
    pub clashes_with_repo: bool,
    pub variants: Vec<Variant>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentQuery {
    /// skill:<name>, rule:<name>, or unmanaged:<name>.
    pub item: String,
    /// Required for an unmanaged name with differing variants.
    pub hash: Option<String>,
    /// Optional location of an unmanaged variant; defaults to its first location.
    pub harness: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileQuery {
    #[serde(flatten)]
    pub content: ContentQuery,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    /// file, directory, or symlink. Symlink directories are never traversed.
    pub kind: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteFileRequest {
    #[serde(flatten)]
    pub file: FileQuery,
    pub content: String,
    pub expected: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileContent {
    #[serde(default)]
    pub fingerprint: Option<String>,
    pub path: String,
    pub text: Option<String>,
    pub binary: bool,
    pub too_large: bool,
    pub size: u64,
}

/// A git command on the skills repo, its filter drivers turned off (see
/// [`crate::git::RepoGit`]).
fn git(root: &Path) -> Result<Git> {
    Ok(crate::git::RepoGit::with_env(root, &[("GIT_OPTIONAL_LOCKS".into(), "0".into())])?.git())
}

/// Inventory identities for the working tree. Missing content can be looked
/// up in a commit, but malformed metadata is an error.
pub fn item_ids_on_disk(root: &Path) -> Result<BTreeMap<ItemRef, Option<ItemId>>> {
    let manifest = crate::item_id::manifest_on_disk(root)?;
    let ids = crate::item_id::claims_on_disk(root, &manifest, |_| true)?;
    crate::item_id::check_claims(&ids)?;
    Ok(ids
        .into_iter()
        .filter(|(path, _)| !path.starts_with(crate::skill::IMPORTED_DIR) || root.join(path).join("SKILL.md").is_file())
        .map(|(path, id)| (crate::item_id::item_for_path(&path), id))
        .collect())
}

/// Inventory identities at a commit, including the IDs of draft-deleted items.
pub fn item_ids_at(root: &Path, revision: &str) -> Result<BTreeMap<ItemRef, Option<ItemId>>> {
    let claims = crate::item_id::claims_at(root, revision, |_| true, true)?;
    crate::item_id::check_claims(&claims)?;
    Ok(claims.into_iter().map(|(path, id)| (crate::item_id::item_for_path(&path), id)).collect())
}

fn tracked(root: &Path, revision: &str) -> Result<Vec<(String, String, String)>> {
    if !git(root)?.args(["rev-parse", "--verify", "--quiet", revision]).succeeds()? {
        return Ok(Vec::new());
    }
    let raw = git(root)?.args(["ls-tree", "-r", "-t", "-z", "--full-tree", revision]).run()?;
    raw.split(|b| *b == 0)
        .filter(|b| !b.is_empty())
        .map(|row| {
            let text = std::str::from_utf8(row).map_err(|_| Error::Invalid("content path is not UTF-8".into()))?;
            let (meta, path) = text.split_once('\t').ok_or_else(|| Error::Invalid("invalid git tree".into()))?;
            let mut fields = meta.split_whitespace();
            let mode = fields.next().unwrap_or_default().to_string();
            fields.next();
            let oid = fields.next().unwrap_or_default().to_string();
            Ok((path.into(), mode, oid))
        })
        .collect()
}

/// Every repo Skill and Rule, including draft-deleted items, then unmanaged
/// skills. `baseline` is the applied fleet revision, not an unpushed HEAD.
pub fn list(
    repo: &Repo,
    harnesses: &BTreeMap<String, Harness>,
    assignment: &Assignment,
    baseline: Option<&str>,
) -> Result<Vec<ContentEntry>> {
    let mut head = tracked(repo.root(), "HEAD")?;
    head.sort_by_key(|(path, _, _)| !path.starts_with("personal/"));
    let baseline = baseline.map(|rev| tracked(repo.root(), rev)).transpose()?.unwrap_or_default();
    let baseline: BTreeSet<_> =
        baseline.iter().filter(|(p, _, _)| is_item(p)).map(|(p, _, _)| ItemRef::for_path(Path::new(p))).collect();
    let drafts: BTreeMap<_, _> = repo.draft()?.into_iter().map(|d| (d.item, d.change)).collect();
    let (skills, rules) = repo.content()?;
    let mut head_ids = None;
    let mut entries = BTreeMap::new();
    let mut insert = |name: String, item: ItemRef, id, kind, path: PathBuf, description| {
        entries.insert(
            item.clone(),
            ContentEntry {
                name,
                draft: drafts.get(&item).copied(),
                in_baseline: baseline.contains(&item),
                item: Some(item),
                id,
                kind,
                description,
                path: Some(path),
                enabled_harnesses: Vec::new(),
                clashes_with_repo: false,
                variants: Vec::new(),
            },
        );
    };
    for s in skills {
        insert(
            s.name.clone(),
            ItemRef::Skill(s.name),
            s.id,
            if s.origin == Origin::Personal { ContentType::Personal } else { ContentType::Import },
            s.path,
            s.description,
        );
    }
    for f in &rules {
        insert(
            f.name.clone(),
            ItemRef::Rule(f.name.clone()),
            f.id.clone(),
            ContentType::Rule,
            f.path.clone(),
            f.description.clone(),
        );
    }
    for (path, _, oid) in &head {
        if !is_item(path) {
            continue;
        }
        let item = ItemRef::for_path(Path::new(path));
        if entries.contains_key(&item) {
            continue;
        }
        let kind = if path.starts_with("personal/") {
            ContentType::Personal
        } else if path.starts_with("imported/") {
            ContentType::Import
        } else {
            ContentType::Rule
        };
        let content = git(repo.root())?.args(["cat-file", "-s", oid]).text()?.parse::<u64>().unwrap_or(u64::MAX);
        let description = if drafts.get(&item).is_some_and(|c| *c != Change::Deleted) {
            None
        } else if content <= MAX_FILE_BYTES {
            let bytes = git(repo.root())?.args(["cat-file", "blob", oid]).run()?;
            std::str::from_utf8(&bytes).ok().and_then(|t| crate::skill::frontmatter_field(t, "description"))
        } else {
            None
        };
        let absolute = repo.root().join(path);
        let absolute = if kind == ContentType::Rule { absolute } else { absolute.parent().unwrap().to_path_buf() };
        if head_ids.is_none() {
            head_ids = Some(item_ids_at(repo.root(), "HEAD")?);
        }
        entries.insert(
            item.clone(),
            ContentEntry {
                name: item.name().into(),
                item: Some(item.clone()),
                id: head_ids.as_ref().and_then(|ids| ids.get(&item)).cloned().flatten(),
                kind,
                path: Some(absolute),
                description,
                draft: drafts.get(&item).copied(),
                in_baseline: baseline.contains(&item),
                enabled_harnesses: Vec::new(),
                clashes_with_repo: false,
                variants: Vec::new(),
            },
        );
    }
    let mut entries: Vec<_> = entries.into_values().collect();
    for entry in &mut entries {
        entry.enabled_harnesses = harnesses
            .iter()
            // An item without an Item ID yet has no toggle: the default applies.
            .filter(|(h, config)| match (&entry.item, &entry.id) {
                (Some(ItemRef::Skill(_)), id) => {
                    config.skills_dir().is_some()
                        && id
                            .as_ref()
                            .map_or(assignment.harness(h).new_skills_default, |id| assignment.skill_enabled(h, id))
                }
                (Some(ItemRef::Rule(n)), id) => {
                    config.instructions_file().is_some()
                        && id
                            .as_ref()
                            .map_or(assignment.harness(h).new_rules_default, |id| assignment.rule_enabled(h, id))
                        && rules.iter().find(|f| f.name == *n).is_none_or(|f| f.applies_to(h, config.kind.as_deref()))
                }
                _ => false,
            })
            .map(|(h, _)| h.clone())
            .collect();
    }
    for skill in unmanaged::discover(repo, harnesses)? {
        entries.push(ContentEntry {
            name: skill.name,
            item: None,
            id: None,
            kind: ContentType::Unmanaged,
            description: skill.description,
            draft: None,
            in_baseline: false,
            path: None,
            enabled_harnesses: Vec::new(),
            clashes_with_repo: skill.clashes_with_repo,
            variants: skill.variants,
        });
    }
    Ok(entries)
}

fn is_item(path: &str) -> bool {
    match ItemRef::for_path(Path::new(path)) {
        ItemRef::Skill(_) => path.split('/').count() == 3 && path.ends_with("/SKILL.md"),
        ItemRef::Rule(_) => true,
        _ => false,
    }
}

enum Source {
    Disk(PathBuf),
    Head(PathBuf),
}

fn source(repo: &Repo, harnesses: &BTreeMap<String, Harness>, query: &ContentQuery) -> Result<Source> {
    if let Some(name) = query.item.strip_prefix("unmanaged:") {
        let skill = unmanaged::discover(repo, harnesses)?
            .into_iter()
            .find(|s| s.name == name)
            .ok_or_else(|| Error::Invalid(format!("no unmanaged skill `{name}`")))?;
        if query.hash.is_none() && skill.variants.len() > 1 {
            return Err(Error::Invalid("differing unmanaged variants require a hash".into()));
        }
        let variant = skill
            .variants
            .iter()
            .find(|v| query.hash.as_ref().is_none_or(|hash| *hash == v.hash))
            .ok_or_else(|| Error::Invalid("unknown unmanaged variant hash".into()))?;
        let location = variant
            .locations
            .iter()
            .find(|l| query.harness.as_ref().is_none_or(|h| *h == l.harness))
            .ok_or_else(|| Error::Invalid("unknown harness for this unmanaged variant".into()))?;
        return Ok(Source::Disk(location.path.canonicalize().at(&location.path)?));
    }
    let item: ItemRef = query.item.parse()?;
    let path = match &item {
        ItemRef::Skill(name) => repo.skills()?.into_iter().find(|s| s.name == *name).map(|s| s.path),
        ItemRef::Rule(name) => repo.rules()?.into_iter().find(|f| f.name == *name).map(|f| f.path),
        _ => {
            return Err(Error::Invalid("only Skills and Rules have browsable content".into()));
        }
    };
    if let Some(path) = path {
        let canonical = path.canonicalize().at(&path)?;
        if !canonical.starts_with(repo.root()) {
            return Err(Error::Invalid("item resolves outside the repo".into()));
        }
        return Ok(Source::Disk(canonical));
    }
    let head = tracked(repo.root(), "HEAD")?;
    let (path, _, _) = head
        .iter()
        .filter(|(p, _, _)| is_item(p) && ItemRef::for_path(Path::new(p)) == item)
        .min_by_key(|(p, _, _)| !p.starts_with("personal/"))
        .ok_or_else(|| Error::Invalid(format!("no item `{item}`")))?;
    // Removing SKILL.md alone leaves a modified Skill whose other Draft files
    // must still come from disk. Only a fully deleted item falls back to HEAD.
    if matches!(item, ItemRef::Skill(_)) && !repo.draft()?.iter().any(|d| d.item == item && d.change == Change::Deleted)
    {
        let directory = repo.root().join(Path::new(path).parent().unwrap());
        if directory.is_dir() {
            let canonical = directory.canonicalize().at(&directory)?;
            if !canonical.starts_with(repo.root()) {
                return Err(Error::Invalid("item resolves outside the repo".into()));
            }
            return Ok(Source::Disk(canonical));
        }
    }
    Ok(Source::Head(if matches!(item, ItemRef::Rule(_)) {
        PathBuf::from(path)
    } else {
        Path::new(path).parent().unwrap().to_path_buf()
    }))
}

fn is_sidecar(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|name| name.eq_ignore_ascii_case(crate::skill::ID_FILE))
}

/// Whether an item's tree leaves out the entry `name`, found `depth`
/// levels below the item's root (1: directly inside it): what skill hashes
/// and copies leave out, a personal skill's Item ID sidecar, and sk1ll's
/// scratch directory.
fn reserved_name(depth: usize, name: &std::ffi::OsStr) -> bool {
    is_sidecar(name) || crate::sync::excluded_from_skill(depth, name) || name == ".sk1ll-tmp"
}

/// Names no item can have: sk1ll's own entries in the directories it manages.
fn reserved_item(name: &str) -> bool {
    matches!(name, ".git" | ".sk1ll-state" | ".sk1ll-tmp")
}

pub fn tree(repo: &Repo, harnesses: &BTreeMap<String, Harness>, query: &ContentQuery) -> Result<Vec<FileEntry>> {
    match source(repo, harnesses, query)? {
        #[cfg(unix)]
        Source::Disk(root) => {
            let pinned = pinned::Item::open(&root, query.item.starts_with("rule:"))?;
            crate::failpoint!(ContentAccess, &root);
            pinned.tree()
        }
        #[cfg(not(unix))]
        Source::Disk(root) => {
            let base = if root.is_file() { root.parent().unwrap() } else { &root };
            let mut files = Vec::new();
            let walker = WalkDir::new(&root)
                .sort_by_file_name()
                .into_iter()
                .filter_entry(|e| e.depth() == 0 || !reserved_name(e.depth(), e.file_name()));
            crate::failpoint!(ContentAccess, &root);
            for entry in walker {
                let entry = entry.map_err(std::io::Error::from).at(&root)?;
                if entry.path() == base {
                    continue;
                }
                let meta = fs::symlink_metadata(entry.path()).at(entry.path())?;
                files.push(FileEntry {
                    path: entry.path().strip_prefix(base).unwrap().to_string_lossy().into_owned(),
                    kind: if meta.is_symlink() {
                        "symlink"
                    } else if meta.is_dir() {
                        "directory"
                    } else {
                        "file"
                    }
                    .into(),
                    size: if meta.is_dir() { 0 } else { meta.len() },
                });
            }
            Ok(files)
        }
        Source::Head(root) => {
            let base = if query.item.starts_with("rule:") { root.parent().unwrap() } else { &root };
            let mut files = Vec::new();
            for (path, mode, oid) in tracked(repo.root(), "HEAD")? {
                let path = Path::new(&path);
                if !path.starts_with(&root)
                    || path == base
                    || path
                        .strip_prefix(base)
                        .unwrap()
                        .components()
                        .enumerate()
                        .any(|(depth, c)| reserved_name(depth + 1, c.as_os_str()))
                {
                    continue;
                }
                let size = if mode == "040000" {
                    0
                } else {
                    git(repo.root())?.args(["cat-file", "-s", &oid]).text()?.parse().unwrap_or(0)
                };
                files.push(FileEntry {
                    path: path.strip_prefix(base).unwrap().to_string_lossy().into_owned(),
                    kind: if mode == "120000" {
                        "symlink"
                    } else if mode == "040000" {
                        "directory"
                    } else {
                        "file"
                    }
                    .into(),
                    size,
                });
            }
            Ok(files)
        }
    }
}

fn relative(path: &str) -> Result<&Path> {
    let path = Path::new(path);
    if path.as_os_str().is_empty() || path.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(Error::Invalid("file path must stay inside the item".into()));
    }
    Ok(path)
}

pub fn file(repo: &Repo, harnesses: &BTreeMap<String, Harness>, query: &FileQuery) -> Result<FileContent> {
    let rel = relative(&query.path)?;
    if rel.components().any(|c| is_sidecar(c.as_os_str())) {
        return Err(Error::Invalid("cannot read a reserved file".into()));
    }
    let source = source(repo, harnesses, &query.content)?;
    let on_disk = matches!(source, Source::Disk(_));
    let (bytes, size) = match source {
        #[cfg(unix)]
        Source::Disk(root) => {
            let pinned = pinned::Item::open(&root, query.content.item.starts_with("rule:"))?;
            crate::failpoint!(ContentAccess, &root);
            let file = pinned.file(rel)?;
            let meta = file.metadata().at(&root)?;
            if !meta.is_file() {
                return Err(Error::Invalid("path is not a regular file".into()));
            }
            let mut bytes = Vec::new();
            if meta.len() <= MAX_FILE_BYTES {
                file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes).at(&root)?;
            }
            let size = meta.len().max(bytes.len() as u64);
            (bytes, size)
        }
        #[cfg(not(unix))]
        Source::Disk(root) => {
            let base = if root.is_file() { root.parent().unwrap() } else { &root };
            let path = base.join(rel);
            let canonical = path.canonicalize().at(&path)?;
            if !canonical.starts_with(&root) || (root.is_file() && canonical != root) {
                return Err(Error::Invalid("file path escapes the item".into()));
            }
            crate::failpoint!(ContentAccess, &root);
            let mut options = fs::OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                // Do not block on a FIFO or follow a last-moment symlink swap.
                options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
            }
            let file = options.open(&canonical).at(&canonical)?;
            let meta = file.metadata().at(&canonical)?;
            if !meta.is_file() {
                return Err(Error::Invalid("path is not a regular file".into()));
            }
            let mut bytes = Vec::new();
            if meta.len() <= MAX_FILE_BYTES {
                file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes).at(&canonical)?;
            }
            let size = meta.len().max(bytes.len() as u64);
            (bytes, size)
        }
        Source::Head(root) => {
            let path = if query.content.item.starts_with("rule:") {
                if root.file_name() != rel.file_name() || rel.components().count() != 1 {
                    return Err(Error::Invalid("file path escapes the Rule".into()));
                }
                root
            } else {
                root.join(rel)
            };
            let files = tracked(repo.root(), "HEAD")?;
            let (_, mode, oid) = files
                .iter()
                .find(|(p, _, _)| Path::new(p) == path)
                .ok_or_else(|| Error::Invalid("no such file".into()))?;
            if mode != "100644" && mode != "100755" {
                return Err(Error::Invalid("path is not a regular file".into()));
            }
            let size = git(repo.root())?.args(["cat-file", "-s", oid]).text()?.parse().unwrap_or(u64::MAX);
            (
                if size <= MAX_FILE_BYTES {
                    git(repo.root())?.args(["cat-file", "blob", oid]).run()?
                } else {
                    Vec::new()
                },
                size,
            )
        }
    };
    let too_large = size > MAX_FILE_BYTES;
    let text = if too_large { None } else { String::from_utf8(bytes).ok().filter(|s| !s.contains('\0')) };
    let binary = !too_large && text.is_none();
    let fingerprint = text.as_deref().filter(|_| on_disk).map(crate::hash::hash_str);
    Ok(FileContent { fingerprint, path: query.path.clone(), text, binary, too_large, size })
}

/// Returns false when the loaded fingerprint no longer matches.
pub fn write_file(repo: &Repo, request: &WriteFileRequest) -> Result<bool> {
    if request.content.len() as u64 > MAX_FILE_BYTES || request.content.contains('\0') {
        return Err(Error::Invalid("file content must be UTF-8 text of at most 1 MiB".into()));
    }
    #[cfg(not(unix))]
    {
        let _ = repo;
        Err(Error::Invalid("in-app editing is unavailable on this platform".into()))
    }
    #[cfg(unix)]
    {
        let query = &request.file;
        let rel = relative(&query.path)?;
        if crate::source::is_staging_path(rel)
            || rel.components().enumerate().any(|(depth, c)| reserved_name(depth + 1, c.as_os_str()))
        {
            return Err(Error::Invalid("cannot edit a reserved file".into()));
        }
        if query.content.item.starts_with("unmanaged:") {
            return Err(Error::Invalid("Adopt this Unmanaged skill before editing".into()));
        }
        let item: ItemRef = query.content.item.parse()?;
        let name = item.name();
        if relative(name)?.file_name().is_none_or(|leaf| leaf != name)
            || reserved_item(name)
            || crate::source::is_staging_path(Path::new(name))
        {
            return Err(Error::Invalid("invalid item name".into()));
        }
        let (root, rule) = match item {
            ItemRef::Skill(name) => {
                let path = repo.root().join(crate::skill::PERSONAL_DIR).join(&name);
                if !path.join("SKILL.md").is_file() {
                    if repo.root().join(crate::skill::IMPORTED_DIR).join(&name).join("SKILL.md").is_file() {
                        return Err(Error::Invalid("Fork this Import before editing".into()));
                    }
                    return Ok(false);
                }
                (path, false)
            }
            ItemRef::Rule(name) => {
                (repo.root().join(crate::instructions::INSTRUCTIONS_DIR).join(format!("{name}.md")), true)
            }
            _ => return Err(Error::Invalid("only Personal Skills and Rules are writable".into())),
        };
        pinned::Item::open(&root, rule)?.replace(rel, request)
    }
}

/// The editor destination for a personal Skill or Rule. Imports must be
/// forked, and unmanaged skills adopted, before editing repo content.
pub fn editable_path(repo: &Repo, harnesses: &BTreeMap<String, Harness>, input: &str) -> Result<PathBuf> {
    let requested = if input.contains(':') { Some(input.parse::<ItemRef>()?) } else { None };
    let mut matches = Vec::new();
    for skill in repo.skills()? {
        if requested.as_ref().map_or(skill.name == input, |r| *r == ItemRef::Skill(skill.name.clone())) {
            matches.push((skill.path.join("SKILL.md"), Some((skill.name, skill.origin))));
        }
    }
    for rule in repo.rules()? {
        if requested.as_ref().map_or(rule.name == input, |r| *r == ItemRef::Rule(rule.name.clone())) {
            matches.push((rule.path, None));
        }
    }
    if matches.len() > 1 {
        return Err(Error::Invalid(format!("`{input}` is ambiguous; use skill:<name> or rule:<name>")));
    }
    if let Some((path, origin)) = matches.pop() {
        if let Some((name, Origin::Imported)) = origin {
            return Err(Error::Invalid(format!("`{name}` is an Import; run `sk1ll fork {name}` before editing")));
        }
        let canonical = path.canonicalize().at(&path)?;
        if !canonical.starts_with(repo.root()) {
            return Err(Error::Invalid("editor path escapes the repo".into()));
        }
        return Ok(canonical);
    }
    let name = requested.as_ref().map_or(input, ItemRef::name);
    if !matches!(requested, Some(ItemRef::Rule(_)))
        && let Some(skill) = unmanaged::discover(repo, harnesses)?.into_iter().find(|s| s.name == name)
    {
        let harness = &skill.variants[0].locations[0].harness;
        return Err(Error::Invalid(format!(
            "`{name}` is an unmanaged skill; run `sk1ll adopt {harness} {name}` before editing"
        )));
    }
    Err(Error::Invalid(format!("no Skill or Rule `{input}`")))
}
