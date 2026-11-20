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

/// The files [`tree_ids`] reads from a tree with these paths: each personal
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

/// The Skills and Rules in a tree with these paths, and their Item IDs, as
/// [`crate::Repo::item_ids`] finds them in a working tree: a personal skill
/// over an Import of the same name, and a derived id for an item without
/// one. `files` holds the contents of the [`id_sources`] that were read; one
/// missing or unreadable counts as absent.
pub fn tree_ids(paths: &[String], files: &BTreeMap<String, Vec<u8>>) -> Vec<(ItemRef, ItemId)> {
    let text = |path: &str| files.get(path).map(|bytes| String::from_utf8_lossy(bytes).into_owned());
    let manifest: Manifest = text(MANIFEST_FILE).and_then(|t| toml::from_str(&t).ok()).unwrap_or_default();
    let mut skills = BTreeMap::new();
    for (dir, name) in tree_skills(paths) {
        let id = if dir == PERSONAL_DIR {
            text(&format!("{dir}/{name}/{ID_FILE}"))
                .and_then(|t| ItemId::from_text(&t))
                .unwrap_or_else(|| ItemId::derived("skill", name))
        } else if skills.contains_key(name) {
            continue;
        } else {
            manifest.import_id(name)
        };
        skills.insert(name, id);
    }
    let rules = paths.iter().filter_map(|path| {
        let name = tree_rule(path)?;
        let explicit = text(path).and_then(|t| crate::instructions::explicit_id(&t, Path::new(path)).ok().flatten());
        Some((ItemRef::Rule(name.into()), explicit.unwrap_or_else(|| ItemId::derived("rule", name))))
    });
    skills.into_iter().map(|(name, id)| (ItemRef::Skill(name.into()), id)).chain(rules).collect()
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
            .map(|p| (p.to_string(), std::fs::read(dir.path().join(p)).unwrap()))
            .collect();
        let mut from_tree = tree_ids(&paths, &files);
        let mut on_disk = crate::Repo::open(dir.path()).unwrap().item_ids().unwrap();
        from_tree.sort();
        on_disk.sort();
        assert_eq!(from_tree, on_disk);
        assert!(on_disk.iter().any(|(item, id)| *item == ItemRef::Skill("pdf".into()) && id.to_string() == "pdf-id"));
        // Without the files, everything has its derived id.
        let derived = tree_ids(&paths, &BTreeMap::new());
        assert!(derived.iter().all(|(_, id)| id.to_string().starts_with("d:")), "{derived:?}");
    }
}
