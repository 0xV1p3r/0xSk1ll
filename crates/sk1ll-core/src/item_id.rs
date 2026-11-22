//! Item IDs (ADR 0006): what Assignment toggles are keyed by.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::{fmt, fs};

use serde::{Deserialize, Deserializer, Serialize};

use crate::git::{RepoGit, path_from_bytes};
use crate::instructions::{self, INSTRUCTIONS_DIR};
use crate::manifest::{MANIFEST_FILE, Manifest};
use crate::skill::{ID_FILE, IMPORTED_DIR, PERSONAL_DIR};
use crate::{Error, IoExt, ItemRef, Result};

/// The stable identity of a personal skill, Rule or Import.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ItemId(String);

impl ItemId {
    pub(crate) fn generate() -> Self {
        Self(ulid::Ulid::generate().to_string())
    }

    /// A nonempty, single-line id, trimmed.
    pub fn from_text(text: &str) -> Option<Self> {
        let line = text.strip_suffix("\r\n").or_else(|| text.strip_suffix('\n')).unwrap_or(text);
        let id = line.trim();
        (!id.is_empty() && !line.contains(['\r', '\n', '\0'])).then(|| Self(id.into()))
    }

    pub(crate) fn read_skill(dir: &Path) -> Result<Option<Self>> {
        use crate::config_file::{Entry, open_entry};
        let path = dir.join(ID_FILE);
        #[cfg(not(unix))]
        if path.symlink_metadata().is_ok_and(|meta| meta.is_symlink()) {
            return Err(invalid_sidecar(&path));
        }
        let file = match open_entry(&path)? {
            Entry::Missing => return Ok(None),
            Entry::File(file, _) => file,
            Entry::Other(_) => return Err(invalid_sidecar(&path)),
        };
        let mut bytes = Vec::new();
        file.take(crate::content::MAX_FILE_BYTES + 1).read_to_end(&mut bytes).at(&path)?;
        Self::parse_sidecar(&bytes, &path).map(Some)
    }

    fn parse_sidecar(bytes: &[u8], path: &Path) -> Result<Self> {
        if bytes.len() as u64 <= crate::content::MAX_FILE_BYTES
            && let Ok(text) = std::str::from_utf8(bytes)
            && let Some(id) = Self::from_text(text)
        {
            return Ok(id);
        }
        Err(invalid_sidecar(path))
    }

    pub(crate) fn write_skill(&self, dir: &Path) -> Result<()> {
        let text = format!("{self}\n");
        let path = dir.join(ID_FILE);
        #[cfg(unix)]
        {
            let parent = crate::placement::open_directory(dir, false).at(dir)?;
            crate::placement::replace_file(&parent, ID_FILE.as_ref(), text.as_bytes(), 0o644, || Ok::<_, Error>(true))
                .at(&path)??;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            if !dir.symlink_metadata().at(dir)?.is_dir() {
                return Err(Error::Invalid(format!("{} is not a directory", dir.display())));
            }
            crate::config_file::Staged::write(&path, text.as_bytes(), None)?.replace()
        }
    }
}

impl std::str::FromStr for ItemId {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, String> {
        Self::from_text(s).ok_or_else(|| "an item id must be a nonempty, single-line string".into())
    }
}

impl<'de> Deserialize<'de> for ItemId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::from_text(&text).ok_or_else(|| serde::de::Error::custom("id must be a nonempty, single-line string"))
    }
}

impl fmt::Display for ItemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

fn invalid_sidecar(path: &Path) -> Error {
    Error::Parse { path: path.into(), message: "Item ID must be a regular file with a nonempty, single-line ID".into() }
}

/// Repo-relative paths, including Imports declared in the manifest even if
/// their vendored content is missing.
pub(crate) type Claims = BTreeMap<PathBuf, Option<ItemId>>;

pub(crate) fn entries(dir: &Path) -> Result<Vec<(PathBuf, fs::FileType)>> {
    match fs::metadata(dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Ok(Vec::new()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).at(dir),
    }
    fs::read_dir(dir)
        .at(dir)?
        .map(|entry| {
            let entry = entry.at(dir)?;
            Ok((entry.path(), entry.file_type().at(entry.path())?))
        })
        .collect()
}

pub(crate) fn claims_on_disk(root: &Path, manifest: &Manifest, include: impl Fn(&Path) -> bool) -> Result<Claims> {
    let mut claims = import_claims(manifest)?;
    for dir in [PERSONAL_DIR, IMPORTED_DIR, INSTRUCTIONS_DIR] {
        for (path, ty) in entries(&root.join(dir))? {
            let relative = path.strip_prefix(root).unwrap();
            crate::repo::check_rename_stage(&path)?;
            if !include(relative) || crate::source::is_staging_path(relative) {
                continue;
            }
            if dir == INSTRUCTIONS_DIR {
                if path.extension().is_some_and(|ext| ext == "md") && (ty.is_file() || ty.is_symlink()) {
                    if ty.is_symlink() {
                        return Err(not_a_rule_file(&path));
                    }
                    let file = crate::config_file::open_entry(&path)?;
                    let crate::config_file::Entry::File(file, _) = file else {
                        return Err(not_a_rule_file(&path));
                    };
                    claims.insert(relative.into(), instructions::read_explicit_id(file, &path)?);
                }
            } else if (ty.is_dir() || ty.is_symlink()) && path.join("SKILL.md").is_file() {
                if dir == PERSONAL_DIR {
                    claims.insert(relative.into(), ItemId::read_skill(&path)?);
                } else {
                    claims.entry(relative.into()).or_default();
                }
            }
        }
    }
    Ok(claims)
}

fn import_claims(manifest: &Manifest) -> Result<Claims> {
    manifest
        .import
        .iter()
        .map(|(name, import)| {
            crate::imports::validate_name(name)?;
            Ok((Path::new(IMPORTED_DIR).join(name), import.id.clone()))
        })
        .collect()
}

pub(crate) fn manifest_on_disk(root: &Path) -> Result<Manifest> {
    let path = root.join(MANIFEST_FILE);
    match path.symlink_metadata() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Manifest::default()),
        _ => Manifest::load(root),
    }
}

pub(crate) fn claims_at(root: &Path, rev: &str, include: impl Fn(&Path) -> bool, manifest: bool) -> Result<Claims> {
    let claims = cached_claims(root, rev)?;
    Ok(claims
        .iter()
        .filter(|(path, _)| if path.starts_with(IMPORTED_DIR) { manifest } else { include(path) })
        .map(|(path, id)| (path.clone(), id.clone()))
        .collect())
}

struct CachedClaims {
    root: PathBuf,
    tree: String,
    claims: std::sync::Arc<Claims>,
}

fn cached_claims(root: &Path, revision: &str) -> Result<std::sync::Arc<Claims>> {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    const CAPACITY: usize = 32;
    static CACHE: Mutex<VecDeque<CachedClaims>> = Mutex::new(VecDeque::new());
    let repo = RepoGit::with_env(root, &[("GIT_OPTIONAL_LOCKS".into(), "0".into())])?;
    let git = || repo.git();
    let tree = git().args(["rev-parse", "--verify", &format!("{revision}^{{tree}}")]).text()?;
    {
        let mut cache = CACHE.lock().unwrap();
        if let Some(index) = cache.iter().position(|item| item.root == root && item.tree == tree) {
            let hit = cache.remove(index).unwrap();
            let claims = hit.claims.clone();
            cache.push_front(hit);
            return Ok(claims);
        }
    }
    let claims = Arc::new(read_claims(root, &tree)?);
    let mut cache = CACHE.lock().unwrap();
    cache.push_front(CachedClaims { root: root.into(), tree, claims: claims.clone() });
    cache.truncate(CAPACITY);
    Ok(claims)
}

enum Blob {
    Manifest,
    Sidecar(PathBuf),
    Rule(PathBuf),
}

fn read_claims(root: &Path, tree: &str) -> Result<Claims> {
    let repo = RepoGit::with_env(root, &[("GIT_OPTIONAL_LOCKS".into(), "0".into())])?;
    let git = || repo.git();
    let tree = git().args(["ls-tree", "-r", "-t", "-z", tree]).run()?;
    let entries: BTreeMap<_, _> = tree
        .split(|&b| b == 0)
        .filter_map(|row| {
            let tab = row.iter().position(|&b| b == b'\t')?;
            let mut meta = std::str::from_utf8(&row[..tab]).ok()?.split_whitespace();
            Some((path_from_bytes(&row[tab + 1..]), (meta.next()?, meta.nth(1)?)))
        })
        .collect();

    let mut claims = Claims::new();
    let mut jobs = Vec::new();
    if let Some(&(mode, oid)) = entries.get(Path::new(MANIFEST_FILE)) {
        if !regular(mode) {
            return Err(Error::Invalid(format!("{MANIFEST_FILE} is not a regular file")));
        }
        jobs.push((oid, Blob::Manifest));
    }
    for (path, &(mode, oid)) in &entries {
        let Some(parent) = path.parent() else { continue };
        if path.ends_with("SKILL.md")
            && matches!(parent.parent(), Some(dir) if dir == Path::new(PERSONAL_DIR) || dir == Path::new(IMPORTED_DIR))
        {
            if mode == "040000" || crate::source::is_staging_path(parent) {
                continue;
            }
            claims.insert(parent.into(), None);
            if parent.starts_with(PERSONAL_DIR) {
                let sidecar = parent.join(ID_FILE);
                match entries.get(&sidecar) {
                    Some(&(mode, oid)) if regular(mode) => jobs.push((oid, Blob::Sidecar(parent.into()))),
                    Some(_) => return Err(invalid_sidecar(&sidecar)),
                    None => {}
                }
            }
        } else if parent == Path::new(INSTRUCTIONS_DIR)
            && path.extension().is_some_and(|ext| ext == "md")
            && !crate::source::is_staging_path(path)
        {
            if regular(mode) {
                jobs.push((oid, Blob::Rule(path.clone())));
            } else if mode == "120000" {
                return Err(not_a_rule_file(path));
            }
        }
    }
    if jobs.is_empty() {
        return Ok(claims);
    }
    let input = jobs.iter().flat_map(|(oid, _)| [oid.as_bytes(), b"\n"]).flatten().copied().collect();
    let jobs: Vec<_> = jobs.into_iter().map(|(_, blob)| blob).collect();
    git().args(["cat-file", "--batch"]).stdin(input).run_stream(move |pipe| batch_claims(pipe, jobs, claims))?
}

fn batch_claims(reader: impl Read, jobs: Vec<Blob>, mut claims: Claims) -> std::io::Result<Result<Claims>> {
    use std::io::{self, BufRead, BufReader};
    let malformed = || io::Error::new(io::ErrorKind::InvalidData, "invalid git cat-file batch response");
    let mut reader = BufReader::new(reader);
    let mut header = String::new();
    let mut first_error = None;
    for job in jobs {
        header.clear();
        reader.read_line(&mut header)?;
        let mut fields = header.split_whitespace();
        let (Some(_), Some("blob"), Some(size)) = (fields.next(), fields.next(), fields.next()) else {
            return Err(malformed());
        };
        let size = size.parse::<u64>().map_err(|_| malformed())?;
        let mut blob = reader.by_ref().take(size);
        let found = match job {
            Blob::Manifest => {
                let mut bytes = Vec::new();
                blob.read_to_end(&mut bytes)?;
                Manifest::parse(&bytes, Path::new(MANIFEST_FILE))
                    .and_then(|manifest| import_claims(&manifest))
                    .map(|imports| claims.extend(imports))
            }
            Blob::Sidecar(path) => {
                let sidecar = path.join(ID_FILE);
                let cap = crate::content::MAX_FILE_BYTES;
                if size > cap {
                    Err(invalid_sidecar(&sidecar))
                } else {
                    let mut bytes = Vec::new();
                    blob.read_to_end(&mut bytes)?;
                    ItemId::parse_sidecar(&bytes, &sidecar).map(|id| {
                        claims.insert(path, Some(id));
                    })
                }
            }
            Blob::Rule(path) => instructions::read_explicit_id(&mut blob, &path).map(|id| {
                claims.insert(path, id);
            }),
        };
        if let Err(error) = found {
            first_error.get_or_insert(error);
        }
        io::copy(&mut blob, &mut io::sink())?;
        let mut newline = [0];
        reader.read_exact(&mut newline)?;
        if newline != *b"\n" {
            return Err(malformed());
        }
    }
    Ok(first_error.map_or(Ok(claims), Err))
}

fn not_a_rule_file(path: &Path) -> Error {
    Error::Invalid(format!("{} is not a regular Rule file", path.display()))
}

fn regular(mode: &str) -> bool {
    matches!(mode, "100644" | "100755")
}

pub(crate) fn item_for_path(path: &Path) -> ItemRef {
    if path.starts_with(PERSONAL_DIR) || path.starts_with(IMPORTED_DIR) {
        ItemRef::Skill(path.file_name().unwrap_or_default().to_string_lossy().into_owned())
    } else {
        ItemRef::Rule(path.file_stem().unwrap_or_default().to_string_lossy().into_owned())
    }
}

pub(crate) fn claim_path(path: &Path) -> PathBuf {
    if path.starts_with(PERSONAL_DIR) {
        path.join(ID_FILE)
    } else if path.starts_with(IMPORTED_DIR) {
        PathBuf::from(format!("{MANIFEST_FILE} [import.{}]", item_for_path(path).name()))
    } else {
        path.into()
    }
}

pub(crate) fn check_claims(claims: &Claims) -> Result<()> {
    let mut seen = BTreeMap::new();
    for (path, id) in claims {
        if let Some(id) = id
            && let Some(first) = seen.insert(id, path)
        {
            return Err(Error::DuplicateItemId { id: id.clone(), first: claim_path(first), second: claim_path(path) });
        }
    }
    Ok(())
}

/// What [`assign`] writes.
pub(crate) enum Mode<'a> {
    /// An Item ID for each included item without one; duplicates are refused.
    Missing,
    /// A new Item ID for each item that shares one, but the keeper: the
    /// claimant that holds it at its path in `baseline`. Only when none or
    /// several do, the first of them by claim path keeps it. Then one for
    /// every item without one.
    Repair { baseline: &'a Claims },
}

/// Of the items sharing an Item ID, those [`Mode::Repair`] gives a new one.
fn repaired<'a>(claims: &'a Claims, baseline: &Claims) -> BTreeSet<&'a Path> {
    let mut by_id = BTreeMap::<&ItemId, Vec<&Path>>::new();
    for (path, id) in claims {
        if let Some(id) = id {
            by_id.entry(id).or_default().push(path);
        }
    }
    let mut replaced = BTreeSet::new();
    for (id, mut paths) in by_id {
        if paths.len() < 2 {
            continue;
        }
        paths.sort_by_key(|path| claim_path(path));
        let in_baseline: Vec<_> =
            paths.iter().copied().filter(|path| baseline.get(*path).is_some_and(|b| b.as_ref() == Some(id))).collect();
        let keeper = in_baseline.first().copied().unwrap_or(paths[0]);
        replaced.extend(paths.into_iter().filter(|path| *path != keeper));
    }
    replaced
}

pub(crate) fn assign(root: &Path, include: impl Fn(&Path) -> bool, mode: Mode) -> Result<usize> {
    let manifest = manifest_on_disk(root)?;
    let claims = claims_on_disk(root, &manifest, |_| true)?;
    let replaced = match mode {
        Mode::Missing => {
            check_claims(&claims)?;
            None
        }
        Mode::Repair { baseline } => Some(repaired(&claims, baseline)),
    };
    let mut used: BTreeSet<_> = claims.values().flatten().cloned().collect();
    let mut ordered: Vec<_> = claims.iter().collect();
    ordered.sort_by_key(|(path, _)| claim_path(path));
    let mut changed = 0;
    for (path, id) in ordered {
        let replace = match &replaced {
            Some(replaced) => replaced.contains(path.as_path()) || id.is_none(),
            None => id.is_none() && include(path),
        };
        if !replace {
            continue;
        }
        let id = loop {
            let id = ItemId::generate();
            if used.insert(id.clone()) {
                break id;
            }
        };
        let dest = root.join(path);
        if path.starts_with(PERSONAL_DIR) {
            id.write_skill(&dest)?;
        } else if path.starts_with(IMPORTED_DIR) {
            let item = item_for_path(path);
            if !manifest.import.contains_key(item.name()) {
                continue;
            }
            crate::manifest::set_import_id(root, item.name(), &id)?;
        } else {
            instructions::write_id(&dest, &id)?;
        }
        changed += 1;
    }
    Ok(changed)
}

/// The files [`tree_inventory`] reads from a tree with these paths: each personal
/// skill's ID file, each rule and the manifest.
pub fn id_sources(paths: &[String]) -> Vec<&str> {
    let skill_ids: std::collections::BTreeSet<String> = tree_skills(paths)
        .filter(|(dir, _)| *dir == PERSONAL_DIR)
        .map(|(dir, name)| format!("{dir}/{name}/{ID_FILE}"))
        .collect();
    paths
        .iter()
        .map(String::as_str)
        .filter(|p| *p == MANIFEST_FILE || skill_ids.contains(*p) || tree_rule(p).is_some())
        .collect()
}

/// Item claims suitable for reporting even when some metadata is unreadable.
/// An item without an explicit Item ID has none: IDs are never derived.
#[derive(Debug, Default)]
pub struct ItemInventory {
    pub items: Vec<(ItemRef, Option<ItemId>)>,
    pub errors: Vec<String>,
    /// Where each item's Item ID is (or goes), for messages: a personal
    /// skill's sidecar, an Import's manifest table or a Rule's file.
    pub claims: BTreeMap<ItemRef, String>,
}

impl ItemInventory {
    /// What keeps this inventory from giving every item an Item ID of its
    /// own (ADR 0006); `None` if nothing does.
    pub fn id_problems(&self) -> Option<IdProblems> {
        let mut by_id = BTreeMap::<&ItemId, Vec<&ItemRef>>::new();
        let mut missing = Vec::new();
        for (item, id) in &self.items {
            match id {
                Some(id) => by_id.entry(id).or_default().push(item),
                None => missing.push(item.clone()),
            }
        }
        let shared: Vec<_> = by_id
            .into_iter()
            .filter(|(_, items)| items.len() > 1)
            .map(|(id, items)| (id.clone(), items.into_iter().cloned().collect()))
            .collect();
        let problems = IdProblems { unreadable: self.errors.clone(), missing, shared, claims: self.claims.clone() };
        (!problems.unreadable.is_empty() || !problems.missing.is_empty() || !problems.shared.is_empty())
            .then_some(problems)
    }
}

/// Items of an inventory without an Item ID of their own (see
/// [`ItemInventory::id_problems`]). Displayed, it names each one and how to
/// repair them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdProblems {
    /// Metadata that could not be read, so its items' Item IDs are unknown.
    pub unreadable: Vec<String>,
    /// Items without an Item ID.
    pub missing: Vec<ItemRef>,
    /// Item IDs more than one item claims, with those items.
    pub shared: Vec<(ItemId, Vec<ItemRef>)>,
    claims: BTreeMap<ItemRef, String>,
}

impl IdProblems {
    /// Keeps only the items without an Item ID that `keep` takes, given
    /// the item and where its claim is (a path, or the manifest's entry);
    /// `None` if no problem is left.
    pub fn retain_missing(mut self, keep: impl Fn(&ItemRef, Option<&str>) -> bool) -> Option<Self> {
        let claims = &self.claims;
        self.missing.retain(|item| keep(item, claims.get(item).map(String::as_str)));
        (!self.missing.is_empty() || !self.shared.is_empty() || !self.unreadable.is_empty()).then_some(self)
    }
}

impl std::fmt::Display for IdProblems {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = |item: &ItemRef| match self.claims.get(item) {
            Some(claim) => format!("{item} ({claim})"),
            None => item.to_string(),
        };
        f.write_str("every item needs an Item ID of its own:")?;
        for item in &self.missing {
            write!(f, "\n  {} has no Item ID", name(item))?;
        }
        for (id, items) in &self.shared {
            let items: Vec<_> = items.iter().map(name).collect();
            write!(f, "\n  {} share the Item ID `{id}`", items.join(", "))?;
        }
        for error in &self.unreadable {
            write!(f, "\n  unreadable Item ID: {error}")?;
        }
        f.write_str("\nRun `sk1ll repair-ids`, commit the repaired items and push again")
    }
}

/// Collects only the bytes needed for an Item ID claim. A Rule's body can be
/// arbitrarily large. An unfinished header at the size limit is unreadable.
pub struct ClaimSource {
    bytes: Vec<u8>,
    rule: bool,
    limit: usize,
}

impl ClaimSource {
    pub fn new(path: &str) -> Self {
        let limit = if path == MANIFEST_FILE { 64 << 20 } else { crate::content::MAX_FILE_BYTES as usize };
        Self { bytes: Vec::new(), rule: tree_rule(path).is_some(), limit }
    }

    /// Returns true when the claim can be parsed without reading more bytes.
    pub fn push(&mut self, chunk: &[u8]) -> std::result::Result<bool, &'static str> {
        self.bytes.extend_from_slice(chunk);
        if self.rule
            && let Some(end) = rule_prefix(&self.bytes)
        {
            self.bytes.truncate(end);
            if end <= self.limit {
                return Ok(true);
            }
        }
        if self.bytes.len() > self.limit {
            return Err("Item ID metadata exceeds its read limit");
        }
        Ok(false)
    }

    pub fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

fn rule_prefix(bytes: &[u8]) -> Option<usize> {
    if ![b"---\n".as_slice(), b"---\r\n"]
        .into_iter()
        .any(|header| bytes.starts_with(header) || header.starts_with(bytes))
    {
        return Some(0);
    }
    let first = bytes.iter().position(|&b| b == b'\n')?;
    if !matches!(&bytes[..first], b"---" | b"---\r") {
        return Some(0);
    }
    let mut end = first + 1;
    for line in bytes[end..].split_inclusive(|&b| b == b'\n') {
        end += line.len();
        if line.ends_with(b"\n") && str::from_utf8(line).is_ok_and(|text| text.trim() == "---") {
            return Some(end);
        }
    }
    None
}

pub fn read_claim(path: &str, mut reader: impl Read) -> std::result::Result<Vec<u8>, String> {
    let mut source = ClaimSource::new(path);
    let mut chunk = [0; 8192];
    loop {
        let n = reader.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 || source.push(&chunk[..n])? {
            return Ok(source.finish());
        }
    }
}

/// Match Item IDs in a Git tree and working-tree claims using one parser.
/// Missing claims are distinct from blobs we could not read or parse.
pub fn tree_inventory(
    paths: &[String],
    files: &BTreeMap<String, std::result::Result<Vec<u8>, String>>,
) -> ItemInventory {
    type Claim = std::result::Result<Option<ItemId>, String>;
    let text = |path: &str| -> std::result::Result<Option<&str>, String> {
        files
            .get(path)
            .map(|value| {
                let bytes = value.as_ref().map_err(Clone::clone)?;
                str::from_utf8(bytes).map_err(|e| e.to_string())
            })
            .transpose()
    };
    let manifest = text(MANIFEST_FILE)
        .and_then(|raw| raw.map(|raw| toml::from_str::<Manifest>(raw).map_err(|e| e.to_string())).transpose())
        .map(|manifest| manifest.unwrap_or_default());
    let mut inventory = ItemInventory::default();
    if let Err(error) = &manifest {
        inventory.errors.push(format!("{MANIFEST_FILE}: {error}"));
    }

    let mut skills = BTreeMap::<_, Claim>::new();
    // Personal skills take precedence over Imports of the same name.
    for (dir, name) in tree_skills(paths) {
        let id = if dir == PERSONAL_DIR {
            let path = format!("{dir}/{name}/{ID_FILE}");
            match text(&path) {
                Ok(Some(raw)) => ItemId::from_text(raw).map(Some).ok_or_else(|| format!("{path}: invalid Item ID")),
                Ok(None) => Ok(None),
                Err(error) => Err(format!("{path}: {error}")),
            }
        } else {
            manifest
                .as_ref()
                .map(|manifest| manifest.import.get(name).and_then(|import| import.id.clone()))
                .map_err(Clone::clone)
        };
        let claim = if dir == PERSONAL_DIR {
            format!("{dir}/{name}/{ID_FILE}")
        } else {
            format!("{MANIFEST_FILE} [import.{name}]")
        };
        inventory.claims.insert(ItemRef::Skill(name.into()), claim);
        skills.insert(name, id);
    }
    let rules = paths.iter().filter_map(|path| {
        let name = tree_rule(path)?;
        let id = text(path)
            .and_then(|text| match text {
                Some(raw)
                    if (raw.starts_with("---\n") || raw.starts_with("---\r\n"))
                        && crate::skill::split_frontmatter(raw).0.is_none() =>
                {
                    Err("unterminated Rule frontmatter".into())
                }
                Some(raw) => crate::instructions::explicit_id(raw, Path::new(path)).map_err(|e| e.to_string()),
                None => Ok(None),
            })
            .map_err(|error| format!("{path}: {error}"));
        Some((ItemRef::Rule(name.into()), id, path.clone()))
    });
    let rules: Vec<_> = rules.collect();
    for (item, _, path) in &rules {
        inventory.claims.insert(item.clone(), path.clone());
    }
    let rules = rules.into_iter().map(|(item, id, _)| (item, id));
    for (item, id) in skills.into_iter().map(|(name, id)| (ItemRef::Skill(name.into()), id)).chain(rules) {
        let id = id.unwrap_or_else(|error| {
            inventory.errors.push(error);
            None
        });
        inventory.items.push((item, id));
    }
    inventory
}

/// Enumerates working-tree metadata without validating the Rule bodies,
/// and retains every readable claim if another item has invalid metadata.
pub fn working_inventory(root: &Path) -> ItemInventory {
    let mut paths = Vec::new();
    let mut errors = Vec::new();
    for dir in [PERSONAL_DIR, IMPORTED_DIR, INSTRUCTIONS_DIR] {
        let entries = match std::fs::read_dir(root.join(dir)) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                errors.push(format!("{dir}: {error}"));
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    errors.push(format!("{dir}: {error}"));
                    continue;
                }
            };
            let Ok(name) = entry.file_name().into_string() else {
                errors.push(format!("{dir}: item name is not UTF-8"));
                continue;
            };
            if dir == INSTRUCTIONS_DIR {
                if tree_rule(&format!("{dir}/{name}")).is_some() && !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    paths.push(format!("{dir}/{name}"));
                }
                continue;
            }
            let skill = entry.path().join("SKILL.md");
            match std::fs::metadata(&skill) {
                Ok(meta) if meta.is_file() => paths.push(format!("{dir}/{name}/SKILL.md")),
                Ok(_) => continue,
                Err(error)
                    if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) =>
                {
                    continue;
                }
                Err(error) => {
                    errors.push(format!("{dir}/{name}/SKILL.md: {error}"));
                    continue;
                }
            }
            let sidecar = format!("{dir}/{name}/{ID_FILE}");
            if dir == PERSONAL_DIR {
                match root.join(&sidecar).symlink_metadata() {
                    Ok(_) => paths.push(sidecar),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        errors.push(format!("{sidecar}: {error}"));
                        paths.push(sidecar);
                    }
                }
            }
        }
    }
    match root.join(MANIFEST_FILE).symlink_metadata() {
        Ok(_) => paths.push(MANIFEST_FILE.into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            errors.push(format!("{MANIFEST_FILE}: {error}"));
            paths.push(MANIFEST_FILE.into());
        }
    }
    let files = id_sources(&paths)
        .into_iter()
        .map(|path| {
            use crate::config_file::{Entry, open_entry};
            let read = match open_entry(&root.join(path)) {
                Ok(Entry::File(file, _)) => read_claim(path, file),
                Ok(_) => Err("expected a regular file".into()),
                Err(e) => Err(e.to_string()),
            };
            (path.to_string(), read)
        })
        .collect();
    let mut inventory = tree_inventory(&paths, &files);
    inventory.errors.extend(errors);
    inventory
}

/// Claims in a local commit. The caller supplies its tree paths so the
/// inventory and Git change labels describe the same revision.
pub fn committed_inventory(root: &Path, commit: &str, paths: &[String]) -> ItemInventory {
    let files = id_sources(paths)
        .into_iter()
        .map(|path| {
            let claim = committed_claim(root, commit, path).map_err(|error| error.to_string());
            (path.to_string(), claim)
        })
        .collect();
    tree_inventory(paths, &files)
}

fn committed_claim(root: &Path, commit: &str, path: &str) -> std::io::Result<Vec<u8>> {
    use std::process::{Command, Stdio};
    // Streamed, so not a `Git`; its filter drivers are off all the same.
    let repo = RepoGit::open(root).map_err(std::io::Error::other)?;
    let (neutralizing, env) = repo.neutralizing();
    let mut git = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(neutralizing)
        .envs(env)
        .args(["cat-file", "blob", &format!("{commit}:{path}")])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdout = git.stdout.take().expect("piped");
    let mut source = ClaimSource::new(path);
    let mut chunk = [0; 8192];
    let result = loop {
        match stdout.read(&mut chunk) {
            Ok(0) => {
                return if git.wait()?.success() {
                    Ok(source.finish())
                } else {
                    Err(std::io::Error::other("could not read Git blob"))
                };
            }
            Ok(n) => match source.push(&chunk[..n]) {
                Ok(true) => break Ok(source.finish()),
                Ok(false) => continue,
                Err(error) => break Err(std::io::Error::other(error)),
            },
            Err(error) => break Err(error),
        }
    };
    let _ = git.kill();
    git.wait()?;
    result
}

/// `(dir, name)` of each skill in the tree, personal ones last: a skill
/// exists with its `SKILL.md`.
fn tree_skills(paths: &[String]) -> impl Iterator<Item = (&'static str, &str)> {
    [IMPORTED_DIR, PERSONAL_DIR].into_iter().flat_map(move |dir| {
        paths.iter().filter_map(move |path| {
            let name = path.strip_prefix(dir)?.strip_prefix('/')?.strip_suffix("/SKILL.md")?;
            let staging = crate::source::is_staging_path(Path::new(name));
            (!name.is_empty() && !name.contains('/') && !staging).then_some((dir, name))
        })
    })
}

/// The rule a tree path is, if any.
fn tree_rule(path: &str) -> Option<&str> {
    let name = path.strip_prefix(INSTRUCTIONS_DIR)?.strip_prefix('/')?.strip_suffix(".md")?;
    (!name.is_empty() && !name.contains('/')).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_problems_name_items_without_an_item_id_of_their_own() {
        let paths: Vec<String> = ["personal/a/SKILL.md", "personal/b/SKILL.md", "instructions/r.md", "sk1ll.toml"]
            .into_iter()
            .chain(["personal/a/.sk1ll-id", "personal/b/.sk1ll-id", "imported/web/SKILL.md"])
            .map(Into::into)
            .collect();
        let file = |text: &str| Ok(text.as_bytes().to_vec());
        let mut files = BTreeMap::from([
            ("personal/a/.sk1ll-id".to_string(), file("same\n")),
            ("personal/b/.sk1ll-id".to_string(), file("same\n")),
            ("instructions/r.md".to_string(), file("No ID.\n")),
            (MANIFEST_FILE.to_string(), file("[import.web]\nsource = 'github:o/r'\n")),
        ]);
        let problems = tree_inventory(&paths, &files).id_problems().expect("problems");
        assert_eq!(problems.missing, [ItemRef::Skill("web".into()), ItemRef::Rule("r".into())]);
        let same: ItemId = "same".parse().unwrap();
        assert_eq!(problems.shared, [(same, vec![ItemRef::Skill("a".into()), ItemRef::Skill("b".into())])]);
        let text = problems.to_string();
        assert!(text.contains("skill:web (sk1ll.toml [import.web]) has no Item ID"), "{text}");
        assert!(text.contains("rule:r (instructions/r.md) has no Item ID"), "{text}");
        assert!(text.ends_with("Run `sk1ll repair-ids`, commit the repaired items and push again"), "{text}");

        files.insert("personal/b/.sk1ll-id".into(), file("other\n"));
        files.insert("instructions/r.md".into(), file("---\nid: r\n---\nNo ID.\n"));
        files.insert(MANIFEST_FILE.into(), file("[import.web]\nsource = 'github:o/r'\nid = 'web'\n"));
        assert_eq!(tree_inventory(&paths, &files).id_problems(), None);
    }

    #[test]
    fn historical_claims_are_cached_by_tree_and_stream_past_large_rule_bodies() {
        let dir = tempfile::tempdir().unwrap();
        let repo = crate::Repo::init_committed(dir.path()).unwrap();
        repo.create_item("skill", "pdf", "").unwrap();
        let rule = repo.root().join("instructions/rule.md");
        fs::write(&rule, format!("---\nid: rule-id\n---\n{}", "x".repeat(2 * 1024 * 1024))).unwrap();
        let items = [ItemRef::Rule("rule".into()), ItemRef::Skill("pdf".into())];
        let head = repo.commit(&items, "Items", None).unwrap();
        let cache = cached_claims(repo.root(), &head).unwrap();
        assert_eq!(cache[Path::new("instructions/rule.md")].as_ref().unwrap().to_string(), "rule-id");
        assert!(cache[Path::new("personal/pdf")].is_some());
        crate::git::Git::new(repo.root())
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-m",
                "same tree",
            ])
            .run()
            .unwrap();
        assert!(std::sync::Arc::ptr_eq(&cache, &cached_claims(repo.root(), "HEAD").unwrap()));
    }

    #[test]
    fn tree_ids_match_the_working_tree() {
        let dir = tempfile::tempdir().unwrap();
        let repo = crate::Repo::init(dir.path()).unwrap();
        repo.create_item("skill", "made", "").unwrap();
        repo.create_item("rule", "made", "").unwrap();
        let write = |path: &str, text: &str| {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("personal/hand/SKILL.md", "x\n");
        write("imported/pdf/SKILL.md", "x\n");
        write("imported/hand/SKILL.md", "shadowed\n");
        write("instructions/hand.md", "x\n");
        write("sk1ll.toml", "[import.pdf]\nsource = \"github:o/r\"\nid = \"pdf-id\"\n");
        let mut paths = Vec::new();
        let mut stack = vec![dir.path().to_path_buf()];
        while let Some(d) = stack.pop() {
            for entry in std::fs::read_dir(d).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    if !path.ends_with(".git") {
                        stack.push(path);
                    }
                } else {
                    paths.push(path.strip_prefix(dir.path()).unwrap().to_string_lossy().into_owned());
                }
            }
        }
        let files = id_sources(&paths)
            .into_iter()
            .map(|p| (p.to_string(), Ok(std::fs::read(dir.path().join(p)).unwrap())))
            .collect();
        let inventory = tree_inventory(&paths, &files);
        assert!(inventory.errors.is_empty(), "{:?}", inventory.errors);
        let mut from_tree = inventory.items;
        let mut on_disk = crate::Repo::open(dir.path()).unwrap().item_ids().unwrap();
        from_tree.sort();
        on_disk.sort();
        assert_eq!(from_tree, on_disk);
        let id = |name: &str| on_disk.iter().find(|(item, _)| item.name() == name).unwrap().1.clone();
        assert_eq!(id("pdf").unwrap().to_string(), "pdf-id");
        assert!(id("made").is_some());
        // A hand-made item has no Item ID until sk1ll writes one: none is derived.
        assert_eq!(id("hand"), None);
        let unread = tree_inventory(&paths, &BTreeMap::new()).items;
        assert!(unread.iter().all(|(_, id)| id.is_none()), "{unread:?}");
    }
}
