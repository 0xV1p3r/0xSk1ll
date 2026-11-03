//! Project-agnostic agent instructions ("for X do Y"), kept as fragments in
//! `instructions/*.md` and rendered into each harness's global instructions
//! file (e.g. `~/.claude/CLAUDE.md`, `~/.codex/AGENTS.md`).
//!
//! Two delivery formats:
//! - `block`: 0xSk1ll owns only a marked block in a single file; everything
//!   outside it is left alone. The begin marker records a hash of the block.
//! - `rules`: one file per fragment in a directory (e.g. `~/.claude/rules`).
//!   Files are copied, not symlinked, and their hashes are kept in a hidden
//!   index next to them.
//!
//! Both record hashes so edits made directly in the harness are detected
//! instead of silently overwritten.

use std::collections::BTreeMap;

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::hash::hash_str;
use crate::skill::{frontmatter_field, split_frontmatter};
use crate::sync::MARKER;
use crate::{Error, IoExt, Result};

pub const INSTRUCTIONS_DIR: &str = "instructions";

const BEGIN: &str = "<!-- 0xSk1ll:begin";
const END: &str = "<!-- 0xSk1ll:end -->";
const NOTE: &str =
    "<!-- Managed by 0xSk1ll: edit instructions/ in your skills repo, then run `sk1ll sync`. -->";

/// One `instructions/<name>.md` file. Fragments are rendered in file-name
/// order, so numeric prefixes (`10-git.md`) control ordering.
#[derive(Debug, Clone, Serialize)]
pub struct Fragment {
    pub name: String,
    pub path: PathBuf,
    pub description: Option<String>,
    /// From frontmatter `targets: [claude, codex]`; `None` means all targets.
    pub targets: Option<Vec<String>>,
    #[serde(skip)]
    pub body: String,
    #[serde(skip)]
    raw: String,
}

impl Fragment {
    pub fn applies_to(&self, target: &str) -> bool {
        self.targets.as_ref().is_none_or(|t| t.iter().any(|t| t == target))
    }
}

pub fn discover(root: &Path) -> Result<Vec<Fragment>> {
    let dir = root.join(INSTRUCTIONS_DIR);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(&dir).at(&dir)? {
        let path = entry.at(&dir)?.path();
        if path.is_file() && path.extension().is_some_and(|e| e == "md") {
            paths.push(path);
        }
    }
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let text = fs::read_to_string(&path).at(&path)?;
            let name = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
            let targets = frontmatter_field(&text, "targets").map(|v| {
                v.trim_matches(['[', ']'])
                    .split(',')
                    .map(|s| s.trim().trim_matches(['"', '\'']).to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            });
            Ok(Fragment {
                name,
                description: frontmatter_field(&text, "description"),
                targets,
                body: split_frontmatter(&text).1.trim().to_string(),
                raw: text,
                path,
            })
        })
        .collect()
}

/// Block content for the given fragments, or `None` if there is nothing to write.
pub fn render(fragments: &[&Fragment]) -> Option<String> {
    let bodies: Vec<&str> = fragments.iter().map(|f| f.body.as_str()).filter(|b| !b.is_empty()).collect();
    (!bodies.is_empty()).then(|| format!("{NOTE}\n\n{}", bodies.join("\n\n")))
}

/// File name and content of a fragment in the `rules` format. Frontmatter is
/// kept (e.g. Claude's `paths:` for path-scoped rules) minus 0xSk1ll's own
/// `targets` key.
pub fn render_rule(fragment: &Fragment) -> (String, String) {
    let file_name = format!("{}.md", fragment.name);
    let (frontmatter, body) = split_frontmatter(&fragment.raw);
    let mut skipping = false;
    let kept: Vec<&str> = frontmatter
        .unwrap_or_default()
        .lines()
        .filter(|line| {
            if !line.starts_with([' ', '\t', '-']) {
                skipping = line.starts_with("targets:");
            }
            !skipping
        })
        .collect();
    let body = body.trim();
    let content = if kept.iter().all(|l| l.trim().is_empty()) {
        format!("{body}\n")
    } else {
        format!("---\n{}\n---\n\n{body}\n", kept.join("\n"))
    };
    (file_name, content)
}

/// Hashes of the rule files 0xSk1ll wrote, stored as `<dir>/.sk1ll-managed`.
pub(crate) type RulesIndex = BTreeMap<String, String>;

pub(crate) fn load_rules_index(dir: &Path) -> Result<RulesIndex> {
    let path = dir.join(MARKER);
    match read_optional(&path)? {
        Some(text) => toml::from_str(&text).map_err(|e| Error::Parse { path, message: e.to_string() }),
        None => Ok(RulesIndex::new()),
    }
}

pub(crate) fn save_rules_index(dir: &Path, index: &RulesIndex) -> Result<()> {
    let path = dir.join(MARKER);
    if index.is_empty() {
        return match fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e).at(&path),
            _ => Ok(()),
        };
    }
    let body = toml::to_string(index).map_err(|e| Error::Parse { path: path.clone(), message: e.to_string() })?;
    write_file(&path, &format!("# Rule files managed by 0xSk1ll. Do not edit.\n{body}"))
}

/// The managed block within a harness instructions file.
#[derive(Debug)]
pub(crate) struct Block {
    start: usize,
    end: usize,
    recorded_hash: String,
    pub(crate) inner: String,
}

impl Block {
    /// The content no longer matches what 0xSk1ll last wrote.
    pub(crate) fn edited(&self) -> bool {
        hash_str(&self.inner) != self.recorded_hash
    }
}

pub(crate) fn find_block(text: &str, path: &Path) -> Result<Option<Block>> {
    let Some(start) = find_at_line_start(text, BEGIN) else {
        return Ok(None);
    };
    let header_end = text[start..].find('\n').map_or(text.len(), |i| start + i + 1);
    let recorded_hash = text[start + BEGIN.len()..header_end].trim().trim_end_matches("-->").trim().to_string();
    let Some(end_rel) = find_at_line_start(&text[header_end..], END) else {
        return Err(Error::Invalid(format!("{}: unterminated 0xSk1ll block (missing `{END}`)", path.display())));
    };
    let end_marker = header_end + end_rel;
    let raw = &text[header_end..end_marker];
    let inner = raw.strip_suffix('\n').unwrap_or(raw).trim_end_matches('\r').to_string();
    let mut end = end_marker + END.len();
    if text[end..].starts_with('\n') {
        end += 1;
    }
    Ok(Some(Block { start, end, recorded_hash, inner }))
}

/// Returns `text` with the block replaced by `content` (appended if there was
/// no block) or removed (`content == None`).
pub(crate) fn splice(text: &str, block: Option<&Block>, content: Option<&str>) -> String {
    let (before, after) = match block {
        Some(b) => (&text[..b.start], &text[b.end..]),
        None => (text, ""),
    };
    let mut out = String::new();
    match content {
        Some(content) => {
            if block.is_some() {
                out.push_str(before);
            } else if !before.trim().is_empty() {
                out.push_str(before.trim_end());
                out.push_str("\n\n");
            }
            out.push_str(&format!("{BEGIN} {} -->\n{content}\n{END}\n", hash_str(content)));
            out.push_str(after);
        }
        None => {
            let before = before.trim_end();
            let after = after.trim_start_matches(['\n', '\r']);
            out.push_str(before);
            if !before.is_empty() && !after.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(after);
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    out
}

pub(crate) fn read_optional(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).at(path),
    }
}

/// Atomic write that goes through symlinks, since global instruction files are
/// often linked from a dotfiles repo.
pub(crate) fn write_file(path: &Path, text: &str) -> Result<()> {
    let real = match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => fs::canonicalize(path).at(path)?,
        _ => path.to_path_buf(),
    };
    if let Some(parent) = real.parent() {
        fs::create_dir_all(parent).at(parent)?;
    }
    let file_name = real.file_name().unwrap_or_default().to_string_lossy();
    let tmp = real.with_file_name(format!(".{file_name}.sk1ll-tmp"));
    fs::write(&tmp, text).at(&tmp)?;
    fs::rename(&tmp, &real).at(&real)
}

fn find_at_line_start(text: &str, pat: &str) -> Option<usize> {
    text.match_indices(pat).map(|(i, _)| i).find(|&i| i == 0 || text.as_bytes()[i - 1] == b'\n')
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn block(text: &str) -> Option<Block> {
        find_block(text, Path::new("test.md")).unwrap()
    }

    #[test]
    fn appends_replaces_and_removes_block() {
        let user = "# Mine\n\nKeep this.\n";
        let v1 = splice(user, None, Some("a"));
        assert!(v1.starts_with("# Mine\n\nKeep this.\n\n<!-- 0xSk1ll:begin sha256:"));
        let b = block(&v1).unwrap();
        assert_eq!(b.inner, "a");
        assert!(!b.edited());

        let with_tail = format!("{v1}\n## After\n");
        let v2 = splice(&with_tail, block(&with_tail).as_ref(), Some("b\nc"));
        assert_eq!(block(&v2).unwrap().inner, "b\nc");
        assert!(v2.starts_with(user.trim_end()) && v2.ends_with("\n## After\n"));

        let v3 = splice(&v2, block(&v2).as_ref(), None);
        assert_eq!(v3, "# Mine\n\nKeep this.\n\n## After\n");
        assert_eq!(splice(&v1, block(&v1).as_ref(), None), user);
    }

    #[test]
    fn rule_keeps_frontmatter_except_targets() {
        let frag = |raw: &str| Fragment {
            name: "r".into(),
            path: "r.md".into(),
            description: None,
            targets: None,
            body: split_frontmatter(raw).1.trim().into(),
            raw: raw.into(),
        };
        let (name, text) = render_rule(&frag("---\ntargets:\n  - claude\npaths:\n  - \"src/**\"\n---\nDo X.\n"));
        assert_eq!(name, "r.md");
        assert_eq!(text, "---\npaths:\n  - \"src/**\"\n---\n\nDo X.\n");
        assert_eq!(render_rule(&frag("---\ntargets: [claude]\n---\nDo Y.")).1, "Do Y.\n");
        assert_eq!(render_rule(&frag("Plain.")).1, "Plain.\n");
    }

    #[test]
    fn detects_hand_edits_and_bad_markers() {
        let text = splice("", None, Some("rule"));
        assert_eq!(text.lines().count(), 3);
        let edited = text.replace("rule", "rule!");
        assert!(block(&edited).unwrap().edited());
        assert!(find_block("<!-- 0xSk1ll:begin x -->\nno end", Path::new("f")).is_err());
        assert!(block("text mentioning <!-- 0xSk1ll:begin inline").is_none());
    }
}
