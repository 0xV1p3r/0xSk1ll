use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::{fmt, fs};

use serde::{Deserialize, Deserializer, Serialize};

use crate::git::{Git, path_from_bytes};
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

    pub(crate) fn from_text(text: &str) -> Option<Self> {
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
    let git = || Git::new(root).env("GIT_OPTIONAL_LOCKS", "0");
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
    let git = || Git::new(root).env("GIT_OPTIONAL_LOCKS", "0");
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

pub(crate) fn assign(root: &Path, include: impl Fn(&Path) -> bool, repair: bool) -> Result<usize> {
    let manifest = manifest_on_disk(root)?;
    let claims = claims_on_disk(root, &manifest, |_| true)?;
    if !repair {
        check_claims(&claims)?;
    }
    let mut used: BTreeSet<_> = claims.values().flatten().cloned().collect();
    let mut seen = BTreeSet::new();
    let mut ordered: Vec<_> = claims.iter().collect();
    ordered.sort_by_key(|(path, _)| claim_path(path));
    let mut changed = 0;
    for (path, id) in ordered {
        let replace =
            if repair { id.as_ref().is_some_and(|id| !seen.insert(id)) } else { id.is_none() && include(path) };
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

#[cfg(test)]
mod tests {
    use super::*;

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
        Git::new(repo.root())
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
}
