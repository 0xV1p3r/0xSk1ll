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

impl Import {
    pub fn item_id(&self, name: &str) -> ItemId {
        self.id.clone().unwrap_or_else(|| ItemId::derived("import", name))
    }
}

fn default_ref() -> String {
    "HEAD".into()
}

impl Manifest {
    pub(crate) fn import_id(&self, name: &str) -> ItemId {
        self.import.get(name).map_or_else(|| ItemId::derived("import", name), |import| import.item_id(name))
    }

    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join(MANIFEST_FILE);
        let text = fs::read_to_string(&path).at(&path)?;
        let table: toml::Table =
            toml::from_str(&text).map_err(|e| Error::Parse { path: path.clone(), message: e.to_string() })?;
        reject_moved_keys(&table, &path)?;
        toml::from_str(&text).map_err(|e| Error::Parse { path, message: e.to_string() })
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
            .as_table_mut()
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
    edit(root, |doc| {
        if let Some(t) = doc.get_mut("import").and_then(Item::as_table_mut) {
            t.remove(name);
        }
        Ok(())
    })
}

fn edit(root: &Path, f: impl FnOnce(&mut DocumentMut) -> Result<()>) -> Result<()> {
    let path = root.join(MANIFEST_FILE);
    let text = fs::read_to_string(&path).at(&path)?;
    let mut doc: DocumentMut =
        text.parse().map_err(|e: toml_edit::TomlError| Error::Parse { path: path.clone(), message: e.to_string() })?;
    f(&mut doc)?;
    fs::write(&path, doc.to_string()).at(&path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_manifest_explains_what_moved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MANIFEST_FILE);
        fs::write(&path, "[targets.claude]\nskills = \"~/.claude/skills\"\n[profiles.default]\nskills = [\"*\"]\n")
            .unwrap();
        let Err(Error::Invalid(msg)) = Manifest::load(dir.path()) else { panic!("expected Error::Invalid") };
        assert!(msg.contains("machine.toml") && msg.contains("assignment"), "{msg}");

        fs::write(&path, "[import.pdf]\nsource = \"github:o/r\"\n").unwrap();
        assert_eq!(Manifest::load(dir.path()).unwrap().import["pdf"].source, "github:o/r");
        fs::write(&path, "bogus = 1\n").unwrap();
        assert!(matches!(Manifest::load(dir.path()), Err(Error::Parse { .. })));
    }
}
