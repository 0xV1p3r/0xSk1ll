use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::{fmt, fs};

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

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

    pub(crate) fn derived(kind: &str, name: &str) -> Self {
        Self::derive(kind, name, None)
    }

    fn derive(kind: &str, name: &str, shadowed: Option<usize>) -> Self {
        let mut hash = Sha256::new();
        hash.update(kind);
        hash.update(b"\0");
        hash.update(name);
        if let Some(attempt) = shadowed {
            hash.update(b"\0shadowed\0");
            hash.update(attempt.to_string());
        }
        Self(format!("d:{:x}", hash.finalize()))
    }

    pub(crate) fn from_text(text: &str) -> Option<Self> {
        let line = text.strip_suffix("\r\n").or_else(|| text.strip_suffix('\n')).unwrap_or(text);
        let id = line.trim();
        (!id.is_empty() && !line.contains(['\r', '\n', '\0'])).then(|| Self(id.into()))
    }

    pub(crate) fn read_skill(dir: &Path) -> Result<Option<Self>> {
        use crate::config_file::{Entry, open_entry};
        let path = dir.join(ID_FILE);
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
pub(crate) type Resolved = BTreeMap<PathBuf, ItemId>;

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
            if !include(relative) || crate::source::is_staging_path(relative) {
                continue;
            }
            if dir == INSTRUCTIONS_DIR {
                if path.extension().is_some_and(|ext| ext == "md")
                    && (ty.is_file() || ty.is_symlink() && path.is_file())
                {
                    let source = if ty.is_symlink() { path.canonicalize().at(&path)? } else { path.clone() };
                    let file = crate::config_file::open_entry(&source)?;
                    let crate::config_file::Entry::File(file, _) = file else {
                        return Err(Error::Invalid(format!("{} is not a regular Rule file", path.display())));
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
    let git = || Git::new(root).env("GIT_OPTIONAL_LOCKS", "0");
    let tree = git().args(["ls-tree", "-r", "-t", "-z", rev]).run()?;
    let entries: BTreeMap<_, _> = tree
        .split(|&b| b == 0)
        .filter_map(|row| {
            let tab = row.iter().position(|&b| b == b'\t')?;
            let mut meta = std::str::from_utf8(&row[..tab]).ok()?.split_whitespace();
            let mode = meta.next()?;
            let oid = meta.nth(1)?;
            Some((path_from_bytes(&row[tab + 1..]), (mode, oid)))
        })
        .collect();
    let blob = |oid| git().args(["cat-file", "blob", oid]).run();
    let mut claims = Claims::new();
    if manifest && let Some(&(mode, oid)) = entries.get(Path::new(MANIFEST_FILE)) {
        let path = Path::new(MANIFEST_FILE);
        if !regular(mode) {
            return Err(Error::Invalid(format!("{} is not a regular file", path.display())));
        }
        claims = import_claims(&Manifest::parse(&blob(oid)?, path)?)?;
    }
    for (path, &(mode, oid)) in &entries {
        let Some(parent) = path.parent() else { continue };
        if path.ends_with("SKILL.md")
            && (parent.parent() == Some(Path::new(PERSONAL_DIR)) || parent.parent() == Some(Path::new(IMPORTED_DIR)))
        {
            if !include(parent) || mode == "040000" || crate::source::is_staging_path(parent) {
                continue;
            }
            if parent.starts_with(IMPORTED_DIR) {
                claims.entry(parent.into()).or_default();
            } else {
                let sidecar = parent.join(ID_FILE);
                let id = match entries.get(&sidecar) {
                    Some(&(mode, oid)) if regular(mode) => Some(ItemId::parse_sidecar(&blob(oid)?, &sidecar)?),
                    Some(_) => return Err(invalid_sidecar(&sidecar)),
                    None => None,
                };
                claims.insert(parent.into(), id);
            }
        } else if parent == Path::new(INSTRUCTIONS_DIR)
            && path.extension().is_some_and(|ext| ext == "md")
            && include(path)
        {
            if regular(mode) {
                claims.insert(path.clone(), instructions::read_explicit_id(blob(oid)?.as_slice(), path)?);
            } else if mode == "120000" {
                claims.insert(path.clone(), None);
            }
        }
    }
    Ok(claims)
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

fn claim_path(path: &Path) -> PathBuf {
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

pub(crate) fn resolve(claims: Claims) -> Result<Resolved> {
    check_claims(&claims)?;
    let mut used: BTreeSet<_> = claims.values().flatten().cloned().collect();
    Ok(claims
        .into_iter()
        .map(|(path, explicit)| {
            let id = explicit.unwrap_or_else(|| {
                let item = item_for_path(&path);
                let kind = if path.starts_with(PERSONAL_DIR) {
                    "skill"
                } else if path.starts_with(IMPORTED_DIR) {
                    "import"
                } else {
                    "rule"
                };
                let mut id = ItemId::derived(kind, item.name());
                let mut attempt = 0;
                while !used.insert(id.clone()) {
                    id = ItemId::derive(kind, item.name(), Some(attempt));
                    attempt += 1;
                }
                id
            });
            (path, id)
        })
        .collect())
}
