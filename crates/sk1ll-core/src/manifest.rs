use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, Item, Table, value};

use crate::{Error, IoExt, ItemId, Result};

pub const MANIFEST_FILE: &str = "sk1ll.toml";

/// `sk1ll.toml`: what to import. Content only; harnesses are machine-local
/// (`machine.toml`) and assignment is not repo state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    #[serde(default)]
    pub import: BTreeMap<String, Import>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Import {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<ItemId>,
    /// Git URL, local path, or `github:owner/repo`.
    pub source: String,
    /// Subdirectory within the source repo containing `SKILL.md`.
    #[serde(default)]
    pub path: String,
    #[serde(rename = "ref", default = "default_ref")]
    pub git_ref: String,
}

fn default_ref() -> String {
    "HEAD".into()
}

impl Manifest {
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join(MANIFEST_FILE);
        Self::parse(&fs::read(&path).at(&path)?, &path)
    }

    pub(crate) fn parse(bytes: &[u8], path: &Path) -> Result<Self> {
        let error = |message| Error::Parse { path: path.into(), message };
        let text = std::str::from_utf8(bytes).map_err(|e| error(e.to_string()))?;
        let table: toml::Table = toml::from_str(text).map_err(|e| error(e.to_string()))?;
        reject_moved_keys(&table, path)?;
        toml::from_str(text).map_err(|e| error(e.to_string()))
    }
}

/// Manifests from before the fleet model may still declare harnesses and
/// profiles; say where they went instead of a bare "unknown field".
fn reject_moved_keys(table: &toml::Table, path: &Path) -> Result<()> {
    let mut moved = Vec::new();
    if table.contains_key("targets") {
        moved.push(
            "`[targets]` moved to machine-local harness config (`[harnesses.<name>]` in machine.toml, \
             see `sk1ll harnesses`)",
        );
    }
    if table.contains_key("profiles") {
        moved.push("`[profiles]` were replaced by assignment (see `sk1ll assign`)");
    }
    if moved.is_empty() {
        return Ok(());
    }
    Err(Error::Invalid(format!("{}: {}; delete those tables from it", path.display(), moved.join("; "))))
}

// Writes go through toml_edit so user comments and formatting survive.

pub(crate) fn write_import(root: &Path, name: &str, import: &Import) -> Result<()> {
    edit(root, |doc| {
        let imports = doc
            .entry("import")
            .or_insert_with(|| {
                let mut t = Table::new();
                t.set_implicit(true);
                Item::Table(t)
            })
            .as_table_like_mut()
            .ok_or_else(|| Error::Invalid("`import` in sk1ll.toml must be a table".into()))?;
        let mut t = Table::new();
        t["source"] = value(import.source.as_str());
        if !import.path.is_empty() {
            t["path"] = value(import.path.as_str());
        }
        t["ref"] = value(import.git_ref.as_str());
        if let Some(id) = &import.id {
            t["id"] = value(id.to_string());
        }
        imports.insert(name, Item::Table(t));
        Ok(())
    })
}

pub(crate) fn remove_import(root: &Path, name: &str) -> Result<()> {
    stage_remove_import(root, name)?.replace()
}

/// Checks and stages the manifest edit before the caller moves the Import.
pub(crate) fn stage_remove_import(root: &Path, name: &str) -> Result<crate::config_file::Staged> {
    stage_edit(root, |doc| {
        let removed = match doc.get_mut("import") {
            Some(imports) if imports.is_inline_table() => imports.as_inline_table_mut().unwrap().remove(name).is_some(),
            Some(imports) if imports.is_table() => imports.as_table_mut().unwrap().remove(name).is_some(),
            _ => false,
        };
        if removed { Ok(()) } else { Err(Error::Invalid(format!("no import named `{name}`"))) }
    })
}

pub(crate) fn set_import_id(root: &Path, name: &str, id: &ItemId) -> Result<()> {
    edit(root, |doc| {
        let import = doc
            .get_mut("import")
            .and_then(Item::as_table_like_mut)
            .and_then(|imports| imports.get_mut(name))
            .and_then(Item::as_table_like_mut)
            .ok_or_else(|| Error::Invalid(format!("no import named `{name}`")))?;
        import.insert("id", value(id.to_string()));
        Ok(())
    })
}

fn edit(root: &Path, f: impl FnOnce(&mut DocumentMut) -> Result<()>) -> Result<()> {
    stage_edit(root, f)?.replace()
}

fn stage_edit(root: &Path, f: impl FnOnce(&mut DocumentMut) -> Result<()>) -> Result<crate::config_file::Staged> {
    let path = root.join(MANIFEST_FILE);
    let text = fs::read_to_string(&path).at(&path)?;
    let mut doc: DocumentMut =
        text.parse().map_err(|e: toml_edit::TomlError| Error::Parse { path: path.clone(), message: e.to_string() })?;
    f(&mut doc)?;
    let updated = doc.to_string();
    Manifest::parse(updated.as_bytes(), &path)?;
    crate::config_file::Staged::write(&path, updated.as_bytes(), crate::config_file::existing_mode(&path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_imports_round_trip_and_default_to_the_source_root_and_head() {
        let path = Path::new(MANIFEST_FILE);
        let golden = b"[import.pdf]\nsource = 'github:o/r'\n[import.docs]\nsource = '../repo'\npath = 'skills/docs'\nref = 'main'\nid = 'docs-id'\n";
        let parsed = Manifest::parse(golden, path).unwrap();
        let pdf = &parsed.import["pdf"];
        assert_eq!((&*pdf.source, &*pdf.path, &*pdf.git_ref, &pdf.id), ("github:o/r", "", "HEAD", &None));
        let docs = &parsed.import["docs"];
        assert_eq!((&*docs.source, &*docs.path, &*docs.git_ref), ("../repo", "skills/docs", "main"));
        assert_eq!(docs.id.as_ref().unwrap().to_string(), "docs-id");
        let serialized = toml::to_string(&parsed).unwrap();
        assert_eq!(Manifest::parse(serialized.as_bytes(), path).unwrap().import, parsed.import);
    }

    #[test]
    fn old_manifest_explains_what_moved() {
        let path = Path::new(MANIFEST_FILE);
        for (legacy, expected) in [
            ("[targets.claude]\nskills = '~/.claude/skills'\n", "machine.toml"),
            ("[profiles.default]\nskills = ['*']\n", "assignment"),
        ] {
            assert!(
                matches!(Manifest::parse(legacy.as_bytes(), path), Err(Error::Invalid(msg)) if msg.contains(expected))
            );
        }
        for malformed in ["bogus = 1", "[import.pdf]\nref = 'main'", "[import.pdf]\nsource = 1"] {
            assert!(matches!(Manifest::parse(malformed.as_bytes(), path), Err(Error::Parse { .. })));
        }
    }
}
