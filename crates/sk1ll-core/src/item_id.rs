//! Item IDs (ADR 0006): what Assignment toggles are keyed by.

use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

use crate::instructions::INSTRUCTIONS_DIR;
use crate::manifest::{MANIFEST_FILE, Manifest};
use crate::skill::{ID_FILE, IMPORTED_DIR, PERSONAL_DIR};
use crate::{ItemRef, Result};

/// The stable identity of a personal skill, Rule or Import.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ItemId(String);

impl ItemId {
    pub(crate) fn generate() -> Self {
        Self(ulid::Ulid::generate().to_string())
    }

    /// The id of an item without an explicit one: from its kind (`skill`
    /// for a personal skill, `import` or `rule`) and name, so it is the same
    /// in every repo, and a hand-made item re-created under the same name
    /// has it again.
    pub fn derived(kind: &str, name: &str) -> Self {
        let mut hash = Sha256::new();
        hash.update(kind);
        hash.update(b"\0");
        hash.update(name);
        Self(format!("d:{:x}", hash.finalize()))
    }

    /// A nonempty, single-line id, trimmed.
    pub fn from_text(text: &str) -> Option<Self> {
        let id = text.trim();
        (!id.is_empty() && !id.contains(['\r', '\n', '\0'])).then(|| Self(id.into()))
    }

    pub(crate) fn read_skill(dir: &Path) -> Option<Self> {
        // Treat only a regular one-line file as explicit metadata.
        let crate::config_file::Entry::File(file, _) = crate::config_file::open_entry(&dir.join(ID_FILE)).ok()? else {
            return None;
        };
        let mut text = String::new();
        let limit = crate::content::MAX_FILE_BYTES;
        file.take(limit + 1).read_to_string(&mut text).ok()?;
        (text.len() as u64 <= limit).then(|| Self::from_text(&text)).flatten()
    }

    pub(crate) fn write_skill(&self, dir: &Path) -> Result<()> {
        let path = dir.join(ID_FILE);
        crate::config_file::Staged::write(&path, format!("{self}\n").as_bytes(), None)?.replace()
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
#[derive(Debug, Default)]
pub struct ItemInventory {
    pub items: Vec<(ItemRef, Option<ItemId>)>,
    pub errors: Vec<String>,
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
                Ok(None) => Ok(Some(ItemId::derived("skill", name))),
                Err(error) => Err(format!("{path}: {error}")),
            }
        } else {
            manifest.as_ref().map(|manifest| Some(manifest.import_id(name))).map_err(Clone::clone)
        };
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
                Some(raw) => crate::instructions::explicit_id(raw, Path::new(path))
                    .map(|id| id.or_else(|| Some(ItemId::derived("rule", name))))
                    .map_err(|e| e.to_string()),
                None => Ok(Some(ItemId::derived("rule", name))),
            })
            .map_err(|error| format!("{path}: {error}"));
        Some((ItemRef::Rule(name.into()), id))
    });
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
    let mut git = Command::new("git")
        .arg("-C")
        .arg(root)
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
        let mut from_tree: Vec<_> =
            tree_inventory(&paths, &files).items.into_iter().map(|(item, id)| (item, id.unwrap())).collect();
        let mut on_disk = crate::Repo::open(dir.path()).unwrap().item_ids().unwrap();
        from_tree.sort();
        on_disk.sort();
        assert_eq!(from_tree, on_disk);
        assert!(on_disk.iter().any(|(item, id)| *item == ItemRef::Skill("pdf".into()) && id.to_string() == "pdf-id"));
        // Without the files, everything has its derived id.
        let derived = tree_inventory(&paths, &BTreeMap::new()).items;
        assert!(derived.iter().all(|(_, id)| id.as_ref().unwrap().to_string().starts_with("d:")), "{derived:?}");
    }
}
