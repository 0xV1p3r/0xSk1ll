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
use crate::{Assignment, Change, Error, Harness, IoExt, ItemRef, Origin, Repo, Result};

#[cfg(test)]
thread_local! {
    static BEFORE_ACCESS: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = Default::default();
}
#[cfg(test)]
fn before_access() {
    BEFORE_ACCESS.with_borrow_mut(|h| {
        if let Some(h) = h.take() {
            h();
        }
    });
}

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

fn git(root: &Path) -> Git {
    Git::new(root).env("GIT_OPTIONAL_LOCKS", "0")
}

fn tracked(root: &Path, revision: &str) -> Result<Vec<(String, String, String)>> {
    if !git(root).args(["rev-parse", "--verify", "--quiet", revision]).succeeds()? {
        return Ok(Vec::new());
    }
    let raw = git(root).args(["ls-tree", "-r", "-t", "-z", "--full-tree", revision]).run()?;
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
    let rules = repo.rules()?;
    let mut entries = BTreeMap::new();
    let mut insert = |name: String, item: ItemRef, kind, path: PathBuf, description| {
        entries.insert(
            item.clone(),
            ContentEntry {
                name,
                draft: drafts.get(&item).copied(),
                in_baseline: baseline.contains(&item),
                item: Some(item),
                kind,
                description,
                path: Some(path),
                enabled_harnesses: Vec::new(),
                clashes_with_repo: false,
                variants: Vec::new(),
            },
        );
    };
    for s in repo.skills()? {
        insert(
            s.name.clone(),
            ItemRef::Skill(s.name),
            if s.origin == Origin::Personal { ContentType::Personal } else { ContentType::Import },
            s.path,
            s.description,
        );
    }
    for f in &rules {
        insert(f.name.clone(), ItemRef::Rule(f.name.clone()), ContentType::Rule, f.path.clone(), f.description.clone());
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
        let content = git(repo.root()).args(["cat-file", "-s", oid]).text()?.parse::<u64>().unwrap_or(u64::MAX);
        let description = if drafts.get(&item).is_some_and(|c| *c != Change::Deleted) {
            None
        } else if content <= MAX_FILE_BYTES {
            let bytes = git(repo.root()).args(["cat-file", "blob", oid]).run()?;
            std::str::from_utf8(&bytes).ok().and_then(|t| crate::skill::frontmatter_field(t, "description"))
        } else {
            None
        };
        let absolute = repo.root().join(path);
        let absolute = if kind == ContentType::Rule { absolute } else { absolute.parent().unwrap().to_path_buf() };
        entries.insert(
            item.clone(),
            ContentEntry {
                name: item.name().into(),
                item: Some(item.clone()),
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
            .filter(|(h, config)| match &entry.item {
                Some(ItemRef::Skill(n)) => config.skills_dir().is_some() && assignment.skill_enabled(h, n),
                Some(ItemRef::Rule(n)) => {
                    config.instructions_file().is_some()
                        && assignment.rule_enabled(h, n)
                        && rules.iter().find(|f| f.name == *n).is_none_or(|f| f.applies_to(h))
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

fn reserved_name(name: &std::ffi::OsStr) -> bool {
    crate::sync::SKILL_CONTENT_EXCLUSIONS.iter().any(|excluded| name == *excluded) || name == ".sk1ll-tmp"
}

pub fn tree(repo: &Repo, harnesses: &BTreeMap<String, Harness>, query: &ContentQuery) -> Result<Vec<FileEntry>> {
    match source(repo, harnesses, query)? {
        #[cfg(unix)]
        Source::Disk(root) => {
            let pinned = pinned::Item::open(&root, query.item.starts_with("rule:"))?;
            #[cfg(test)]
            before_access();
            pinned.tree()
        }
        #[cfg(not(unix))]
        Source::Disk(root) => {
            let base = if root.is_file() { root.parent().unwrap() } else { &root };
            let mut files = Vec::new();
            let walker = WalkDir::new(&root)
                .sort_by_file_name()
                .into_iter()
                .filter_entry(|e| e.depth() == 0 || !reserved_name(e.file_name()));
            #[cfg(test)]
            before_access();
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
                if !path.starts_with(&root) || path == base {
                    continue;
                }
                let size = if mode == "040000" {
                    0
                } else {
                    git(repo.root()).args(["cat-file", "-s", &oid]).text()?.parse().unwrap_or(0)
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
    let source = source(repo, harnesses, &query.content)?;
    let on_disk = matches!(source, Source::Disk(_));
    let (bytes, size) = match source {
        #[cfg(unix)]
        Source::Disk(root) => {
            let pinned = pinned::Item::open(&root, query.content.item.starts_with("rule:"))?;
            #[cfg(test)]
            before_access();
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
            #[cfg(test)]
            before_access();
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
            let size = git(repo.root()).args(["cat-file", "-s", oid]).text()?.parse().unwrap_or(u64::MAX);
            (
                if size <= MAX_FILE_BYTES {
                    git(repo.root()).args(["cat-file", "blob", oid]).run()?
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
        if crate::source::is_staging_path(rel) || rel.components().any(|c| reserved_name(c.as_os_str())) {
            return Err(Error::Invalid("cannot edit a reserved file".into()));
        }
        if query.content.item.starts_with("unmanaged:") {
            return Err(Error::Invalid("Adopt this Unmanaged skill before editing".into()));
        }
        let item: ItemRef = query.content.item.parse()?;
        let name = item.name();
        if relative(name)?.file_name().is_none_or(|leaf| leaf != name)
            || reserved_name(name.as_ref())
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

#[cfg(test)]
mod tests {
    use super::*;
    fn write(path: &Path, bytes: impl AsRef<[u8]>) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    fn query(item: &str) -> ContentQuery {
        ContentQuery { item: item.into(), hash: None, harness: None }
    }
    fn read(repo: &Repo, item: &str, path: &str) -> Result<FileContent> {
        file(repo, &BTreeMap::new(), &FileQuery { content: query(item), path: path.into() })
    }
    fn commit(repo: &Repo) {
        Git::new(repo.root()).args(["add", "-A"]).run().unwrap();
        Git::new(repo.root())
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "-qm", "baseline"])
            .run()
            .unwrap();
    }

    #[test]
    fn listing_and_browsing_use_draft_and_fall_back_to_head_for_deleted_items() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::init(tmp.path()).unwrap();
        write(&repo.personal_dir().join("pdf/SKILL.md"), "---\ndescription: baseline\n---\nold");
        write(&repo.imported_dir().join("remote/SKILL.md"), "import");
        write(&repo.root().join("instructions/git.md"), "---\ndescription: Git conventions\n---\nold rule");
        commit(&repo);
        write(&repo.personal_dir().join("pdf/SKILL.md"), "---\ndescription: draft\n---\nnew");
        write(&repo.personal_dir().join("new/SKILL.md"), "added");
        fs::remove_file(repo.root().join("instructions/git.md")).unwrap();
        let items = list(&repo, &BTreeMap::new(), &Assignment::default(), Some("HEAD")).unwrap();
        let pdf = items.iter().find(|s| s.name == "pdf").unwrap();
        assert_eq!(pdf.description.as_deref(), Some("draft"));
        assert_eq!(pdf.draft, Some(Change::Modified));
        assert!(pdf.in_baseline);
        let new = items.iter().find(|s| s.name == "new").unwrap();
        assert_eq!(new.draft, Some(Change::Added));
        assert!(!new.in_baseline);
        let rule = items.iter().find(|s| s.name == "git").unwrap();
        assert_eq!(rule.draft, Some(Change::Deleted));
        assert_eq!(rule.description.as_deref(), Some("Git conventions"));
        assert!(read(&repo, "skill:pdf", "SKILL.md").unwrap().text.unwrap().ends_with("new"));
        assert!(read(&repo, "rule:git", "git.md").unwrap().text.unwrap().ends_with("old rule"));
        fs::remove_dir_all(repo.personal_dir().join("pdf")).unwrap();
        assert!(read(&repo, "skill:pdf", "SKILL.md").unwrap().text.unwrap().ends_with("old"));
        let tree = tree(&repo, &BTreeMap::new(), &query("skill:pdf")).unwrap();
        assert_eq!(tree[0].path, "SKILL.md");
        assert!(
            editable_path(&repo, &BTreeMap::new(), "remote").unwrap_err().to_string().contains("sk1ll fork remote")
        );
    }

    #[test]
    fn files_reject_escape_flag_binary_and_bound_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::init(tmp.path()).unwrap();
        write(&repo.personal_dir().join("pdf/SKILL.md"), "skill");
        write(&repo.personal_dir().join("pdf/binary"), b"\x00\xff");
        write(&repo.personal_dir().join("pdf/huge"), vec![b'x'; MAX_FILE_BYTES as usize + 1]);
        for path in ["../sk1ll.toml", "/etc/passwd", "a/../../sk1ll.toml", ""] {
            assert!(read(&repo, "skill:pdf", path).is_err(), "{path}");
        }
        let binary = read(&repo, "skill:pdf", "binary").unwrap();
        assert!(binary.binary && binary.text.is_none());
        let huge = read(&repo, "skill:pdf", "huge").unwrap();
        assert!(huge.too_large && huge.text.is_none());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(repo.root().join("sk1ll.toml"), repo.personal_dir().join("pdf/escape")).unwrap();
            assert!(read(&repo, "skill:pdf", "escape").is_err());
        }
        commit(&repo);
        fs::remove_dir_all(repo.personal_dir().join("pdf")).unwrap();
        assert!(read(&repo, "skill:pdf", "binary").unwrap().binary);
        assert!(read(&repo, "skill:pdf", "huge").unwrap().too_large);
        #[cfg(unix)]
        assert!(read(&repo, "skill:pdf", "escape").is_err());
    }

    #[test]
    fn unmanaged_browsing_requires_variant_when_content_differs() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::init(&tmp.path().join("repo")).unwrap();
        let hs: BTreeMap<_, _> = ["a", "b"]
            .into_iter()
            .map(|h| {
                write(&tmp.path().join(h).join("pdf/SKILL.md"), h);
                (
                    h.into(),
                    Harness { skills: Some(tmp.path().join(h).to_string_lossy().into_owned()), ..Default::default() },
                )
            })
            .collect();
        assert!(tree(&repo, &hs, &query("unmanaged:pdf")).is_err());
        let hash = unmanaged::discover(&repo, &hs).unwrap()[0].variants[0].hash.clone();
        let q = ContentQuery { hash: Some(hash), ..query("unmanaged:pdf") };
        assert_eq!(tree(&repo, &hs, &q).unwrap()[0].path, "SKILL.md");
        assert!(file(&repo, &hs, &FileQuery { content: q, path: "SKILL.md".into() }).unwrap().text.is_some());
        assert!(editable_path(&repo, &hs, "pdf").unwrap_err().to_string().contains("sk1ll adopt"));
    }

    #[test]
    fn removing_only_skill_md_still_serves_the_remaining_draft_files() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::init(tmp.path()).unwrap();
        write(&repo.personal_dir().join("pdf/SKILL.md"), "baseline");
        write(&repo.personal_dir().join("pdf/data.txt"), "baseline data");
        commit(&repo);
        fs::remove_file(repo.personal_dir().join("pdf/SKILL.md")).unwrap();
        write(&repo.personal_dir().join("pdf/data.txt"), "draft data");
        assert_eq!(read(&repo, "skill:pdf", "data.txt").unwrap().text.as_deref(), Some("draft data"));
        assert!(read(&repo, "skill:pdf", "SKILL.md").is_err());
    }
    #[cfg(unix)]
    #[test]
    fn ancestor_swap_cannot_redirect_file_or_tree() {
        for listing in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let repo = Repo::init(&tmp.path().join("repo")).unwrap();
            let root = repo.personal_dir().join("pdf");
            write(&root.join("SKILL.md"), "safe");
            write(&root.join("docs/data"), "safe");
            let outside = tmp.path().join("outside");
            write(&outside.join("SKILL.md"), "secret");
            write(&outside.join("docs/data"), "secret");
            write(&outside.join("secret"), "secret");
            let swap = root.clone();
            BEFORE_ACCESS.with_borrow_mut(|h| {
                *h = Some(Box::new(move || {
                    fs::rename(&swap, swap.with_file_name("old-pdf")).unwrap();
                    std::os::unix::fs::symlink(outside, swap).unwrap();
                }))
            });
            if listing {
                let result = tree(&repo, &BTreeMap::new(), &query("skill:pdf"));
                if let Ok(entries) = result {
                    assert!(!entries.iter().any(|e| e.path == "secret"));
                }
            } else {
                let result = read(&repo, "skill:pdf", "docs/data");
                if let Ok(file) = result {
                    assert_eq!(file.text.as_deref(), Some("safe"));
                }
            }
        }
    }
}
