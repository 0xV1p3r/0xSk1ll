use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{IoExt, ItemId, Manifest, Result};

pub const PERSONAL_DIR: &str = "personal";
pub const IMPORTED_DIR: &str = "imported";
pub const ID_FILE: &str = ".sk1ll-id";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    Personal,
    Imported,
}

#[derive(Debug, Clone, Serialize)]
pub struct Skill {
    pub id: ItemId,
    pub name: String,
    pub origin: Origin,
    pub path: PathBuf,
    pub description: Option<String>,
    /// A personal skill with the same name as an import wins over it.
    pub shadows_import: bool,
}

/// All skills in the repo, by name. Personal skills take precedence.
pub fn discover(root: &Path) -> Result<Vec<Skill>> {
    let manifest =
        if root.join(crate::manifest::MANIFEST_FILE).is_file() { Manifest::load(root)? } else { Manifest::default() };
    discover_with(root, &manifest)
}

pub(crate) fn discover_with(root: &Path, manifest: &Manifest) -> Result<Vec<Skill>> {
    let mut skills = BTreeMap::new();
    for (origin, sub) in [(Origin::Imported, IMPORTED_DIR), (Origin::Personal, PERSONAL_DIR)] {
        let dir = root.join(sub);
        if !dir.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&dir).at(&dir)? {
            let path = entry.at(&dir)?.path();
            if path.file_name().is_some_and(|n| crate::source::is_staging_path(Path::new(n))) {
                continue;
            }
            let skill_md = path.join("SKILL.md");
            if !skill_md.is_file() {
                continue;
            }
            let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                continue;
            };
            let description = fs::read_to_string(&skill_md).ok().and_then(|t| frontmatter_field(&t, "description"));
            let shadows_import = origin == Origin::Personal && skills.contains_key(&name);
            let id = match origin {
                Origin::Personal => ItemId::read_skill(&path).unwrap_or_else(|| ItemId::derived("skill", &name)),
                Origin::Imported => manifest.import_id(&name),
            };
            skills.insert(name.clone(), Skill { id, name, origin, path, description, shadows_import });
        }
    }
    Ok(skills.into_values().collect())
}

/// Minimal YAML frontmatter lookup for a top-level scalar key, including
/// `>`/`|` block scalars. Enough for `name`/`description`.
pub fn frontmatter_field(text: &str, key: &str) -> Option<String> {
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    let body: Vec<&str> = lines.take_while(|l| l.trim() != "---").collect();
    let (i, rest) =
        body.iter().enumerate().find_map(|(i, l)| Some((i, l.strip_prefix(key)?.strip_prefix(':')?.trim())))?;
    if rest.starts_with('>') || rest.starts_with('|') {
        let block: Vec<&str> =
            body[i + 1..].iter().take_while(|l| l.starts_with([' ', '\t']) || l.is_empty()).map(|l| l.trim()).collect();
        let joined = block.join(if rest.starts_with('>') { " " } else { "\n" });
        return Some(joined.trim().to_string()).filter(|s| !s.is_empty());
    }
    let v = rest.trim_matches(|c| c == '"' || c == '\'');
    Some(v.to_string()).filter(|s| !s.is_empty())
}

/// Splits `---` frontmatter from the body. Returns `(None, text)` if absent.
pub fn split_frontmatter(text: &str) -> (Option<&str>, &str) {
    let Some(rest) = text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n")) else {
        return (None, text);
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim() == "---" {
            return (Some(&rest[..offset]), &rest[offset + line.len()..]);
        }
        offset += line.len();
    }
    (None, text)
}

#[cfg(test)]
mod tests {
    use super::{frontmatter_field, split_frontmatter};

    #[test]
    fn splits_frontmatter() {
        assert_eq!(split_frontmatter("---\na: 1\n---\nbody\n"), (Some("a: 1\n"), "body\n"));
        assert_eq!(split_frontmatter("body"), (None, "body"));
        assert_eq!(split_frontmatter("---\nunterminated"), (None, "---\nunterminated"));
    }

    #[test]
    fn parses_inline_and_folded() {
        let t = "---\nname: pdf\ndescription: \"Work with PDFs\"\n---\nbody";
        assert_eq!(frontmatter_field(t, "name").as_deref(), Some("pdf"));
        assert_eq!(frontmatter_field(t, "description").as_deref(), Some("Work with PDFs"));
        let t = "---\ndescription: >\n  line one\n  line two\nname: x\n---\n";
        assert_eq!(frontmatter_field(t, "description").as_deref(), Some("line one line two"));
        assert_eq!(frontmatter_field("no frontmatter", "name"), None);
    }
}
