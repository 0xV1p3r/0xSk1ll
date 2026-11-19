//! Project-agnostic agent instructions ("for X do Y"), kept as rules in
//! `instructions/*.md` and rendered into each harness's global instructions
//! file (e.g. `~/.claude/CLAUDE.md`, `~/.codex/AGENTS.md`).
//!
//! Two delivery formats:
//! - `block`: 0xSk1ll owns only a marked block in a single file; everything
//!   outside it is left alone. The begin marker records a hash of the block.
//! - `rules`: one file per rule in a directory (e.g. `~/.claude/rules`).
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
/// Rule frontmatter keys restricting which harnesses get it; `targets` is
/// the deprecated spelling.
const FRONTMATTER_KEYS: [&str; 2] = ["harnesses", "targets"];
const NOTE: &str = "<!-- Managed by 0xSk1ll: edit instructions/ in your skills repo, then run `sk1ll sync`. -->";

/// One `instructions/<name>.md` file. Rules are rendered in file-name
/// order, so numeric prefixes (`10-git.md`) control ordering.
#[derive(Debug, Clone, Serialize)]
pub struct Rule {
    pub name: String,
    pub path: PathBuf,
    pub description: Option<String>,
    /// From frontmatter `harnesses: [claude, codex]` (or the deprecated
    /// `targets:`); `None` means all harnesses.
    pub harnesses: Option<Vec<String>>,
    #[serde(skip)]
    pub body: String,
    #[serde(skip)]
    rule_frontmatter: String,
    #[serde(skip)]
    routing_source: String,
}

impl Rule {
    pub fn applies_to(&self, harness: &str) -> bool {
        self.harnesses.as_ref().is_none_or(|h| h.iter().any(|h| h == harness))
    }
}

pub fn discover(root: &Path) -> Result<Vec<Rule>> {
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
            let text =
                read_optional(&path)?.ok_or_else(|| Error::Invalid(format!("{} no longer exists", path.display())))?;
            let name = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
            let harnesses = parse_harnesses(&text, &path)?;
            // Prepare this while errors can still be returned, before sync writes
            // any rules. Rendering only uses these validated source slices.
            let (rule_frontmatter, routing_source) =
                split_harnesses_key(split_frontmatter(&text).0.unwrap_or_default(), &path)?;
            Ok(Rule {
                name,
                description: frontmatter_field(&text, "description"),
                harnesses,
                body: split_frontmatter(&text).1.trim().to_string(),
                rule_frontmatter,
                routing_source,
                path,
            })
        })
        .collect()
}

pub(crate) fn validate_edit(text: &str, path: &Path) -> Result<()> {
    parse_harnesses(text, path)?;
    split_harnesses_key(split_frontmatter(text).0.unwrap_or_default(), path)?;
    Ok(())
}

/// Parses only sk1ll's routing keys; other frontmatter belongs to the harness.
fn parse_harnesses(text: &str, path: &Path) -> Result<Option<Vec<String>>> {
    let (frontmatter, _) = split_frontmatter(text);
    let invalid = || Error::Parse {
        path: path.to_path_buf(),
        message: "harnesses/targets must be a string or a sequence of strings".into(),
    };
    let Some(frontmatter) = frontmatter else {
        if text.starts_with("---\n") || text.starts_with("---\r\n") {
            return Err(invalid());
        }
        return Ok(None);
    };
    let mapping = frontmatter_mapping(frontmatter, path)?;
    let mut scopes = BTreeMap::new();
    for key in FRONTMATTER_KEYS {
        let Some(value) = mapping.get(serde_yaml_ng::Value::String(key.into())) else {
            continue;
        };
        let names = match value {
            serde_yaml_ng::Value::String(name) => vec![name.clone()],
            serde_yaml_ng::Value::Sequence(names) => {
                names.iter().map(|name| name.as_str().map(str::to_owned).ok_or_else(invalid)).collect::<Result<_>>()?
            }
            _ => return Err(invalid()),
        };
        scopes.insert(key, names);
    }
    Ok(scopes.remove("harnesses").or_else(|| scopes.remove("targets")))
}

/// Block content for the given rules, or `None` if there is nothing to write.
pub fn render(rules: &[&Rule]) -> Option<String> {
    let bodies: Vec<&str> = rules.iter().map(|f| f.body.as_str()).filter(|b| !b.is_empty()).collect();
    (!bodies.is_empty()).then(|| format!("{NOTE}\n\n{}", bodies.join("\n\n")))
}

/// File name and content of a rule in the `rules` format. Frontmatter is
/// kept (e.g. Claude's `paths:` for path-scoped rules) minus 0xSk1ll's own
/// `harnesses`/`targets` key.
pub fn render_rule(rule: &Rule) -> (String, String) {
    let file_name = format!("{}.md", rule.name);
    let kept = &rule.rule_frontmatter;
    let body = rule.body.trim();
    let content = if kept.trim().is_empty() { format!("{body}\n") } else { format!("---\n{kept}---\n\n{body}\n") };
    (file_name, content)
}

fn frontmatter_mapping(text: &str, path: &Path) -> Result<serde_yaml_ng::Mapping> {
    serde_yaml_ng::from_str::<Option<serde_yaml_ng::Mapping>>(text)
        .map(Option::unwrap_or_default)
        .map_err(|e| Error::Parse { path: path.to_path_buf(), message: e.to_string() })
}

/// Split source ranges, never reserialize unrelated frontmatter. Only implicit
/// block-mapping keys on one line are supported when routing needs removal.
///
/// A possible boundary is accepted only if the YAML parser says the entire
/// preceding prefix is a complete mapping. Thus apparent keys inside quoted
/// scalars or flow collections cannot become boundaries. Indentation excludes
/// nested mappings and block scalar content. Parsing prefixes also handles
/// escaped quotes, multiline flow lists and indentless block sequences without
/// maintaining a second, incomplete YAML lexer.
fn split_harnesses_key(frontmatter: &str, path: &Path) -> Result<(String, String)> {
    let mapping = frontmatter_mapping(frontmatter, path)?;
    let mut expected = mapping.clone();
    let mut routing = serde_yaml_ng::Mapping::new();
    for key in FRONTMATTER_KEYS {
        let key = serde_yaml_ng::Value::String(key.into());
        if let Some(value) = expected.remove(&key) {
            routing.insert(key, value);
        }
    }
    if routing.is_empty() {
        return Ok((frontmatter.to_string(), String::new()));
    }
    let unsafe_extent = || Error::Parse {
        path: path.to_path_buf(),
        message: "cannot safely strip harnesses/targets source ranges; use single-line keys in a block mapping".into(),
    };
    let mut entries = Vec::new();
    let mut offset = 0;
    let mut root_indent = None;
    for line in frontmatter.split_inclusive('\n') {
        let content = line.trim_end_matches(['\r', '\n']);
        let indent = content.len() - content.trim_start_matches(' ').len();
        let content = &content[indent..];
        if !content.is_empty() && !content.starts_with('#') {
            let root_indent = *root_indent.get_or_insert(indent);
            if indent == root_indent
                && let Some(key) = mapping_key(content)
                && frontmatter_mapping(&frontmatter[..offset], path).is_ok()
            {
                entries.push((offset, key));
            }
        }
        offset += line.len();
    }
    // Reject explicit/complex keys, flow mappings, directives and any other
    // syntax for which we could not identify every top-level entry exactly.
    if entries.len() != mapping.len() || entries.iter().any(|(_, key)| !mapping.contains_key(key)) {
        return Err(unsafe_extent());
    }
    let mut kept = String::with_capacity(frontmatter.len());
    let mut removed = String::new();
    let mut cursor = 0;
    for (index, (start, key)) in entries.iter().enumerate() {
        if !routing.contains_key(key) {
            continue;
        }
        let next = entries.get(index + 1).map_or(frontmatter.len(), |(start, _)| *start);
        let mut end = next;
        // Standalone comments and blank lines following an entry belong to
        // the surrounding document. Inline and internal comments stay with it.
        let through_entry = frontmatter_mapping(&frontmatter[..next], path)?;
        for line in frontmatter[*start..next].split_inclusive('\n').rev() {
            let content = line.trim_end_matches(['\r', '\n']);
            if (content.trim().is_empty() || content.trim_start().starts_with('#'))
                && frontmatter_mapping(&frontmatter[..end - line.len()], path).ok().as_ref() == Some(&through_entry)
            {
                end -= line.len();
            } else {
                break;
            }
        }
        kept.push_str(&frontmatter[cursor..*start]);
        removed.push_str(&frontmatter[*start..end]);
        cursor = end;
    }
    kept.push_str(&frontmatter[cursor..]);
    // Removing an anchor, for example, could invalidate aliases elsewhere.
    // Check both YAML validity and the values of every remaining entry.
    if frontmatter_mapping(&kept, path)? != expected {
        return Err(unsafe_extent());
    }
    // Aliases to retained metadata need a standalone representation when
    // routing is later restored, potentially ahead of their anchor definitions.
    if frontmatter_mapping(&removed, path).ok().as_ref() != Some(&routing) {
        removed = serde_yaml_ng::to_string(&routing)
            .map_err(|e| Error::Parse { path: path.to_path_buf(), message: e.to_string() })?;
    }
    Ok((kept, removed))
}

/// Decode a simple, implicit YAML mapping key. Parsing the candidate rather
/// than trimming quotes also handles escapes in quoted routing key names.
fn mapping_key(line: &str) -> Option<serde_yaml_ng::Value> {
    if line.starts_with(['?', ':', '{', '[', '&', '!', '*', '%']) {
        return None;
    }
    line.match_indices(':').find_map(|(offset, _)| {
        if line.as_bytes().get(offset + 1).is_some_and(|b| !b.is_ascii_whitespace()) {
            return None;
        }
        let key = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&line[..offset]).ok()?;
        (!matches!(key, serde_yaml_ng::Value::Mapping(_) | serde_yaml_ng::Value::Sequence(_))).then_some(key)
    })
}

/// A rule file's `text` as rule source: puts back the `harnesses:` key
/// that [`render_rule`] stripped from `rule`, unless `text` has its own.
pub(crate) fn restore_harnesses(rule: &Rule, text: &str) -> String {
    let key = &rule.routing_source;
    let (frontmatter, body) = split_frontmatter(text);
    if key.is_empty() {
        return text.to_string();
    }
    if let Some(f) = frontmatter {
        let Ok(mapping) = frontmatter_mapping(f, &rule.path) else {
            return text.to_string();
        };
        if FRONTMATTER_KEYS.iter().any(|key| mapping.contains_key(serde_yaml_ng::Value::String((*key).into()))) {
            return text.to_string();
        }
    }
    let restored = match frontmatter {
        Some(f) => format!("---\n{key}{f}---\n{body}"),
        None => format!("---\n{key}---\n{text}"),
    };
    // Adoption's existing infallible API must not manufacture invalid YAML
    // when the edited rule uses a different mapping style or invalid input.
    if frontmatter_mapping(split_frontmatter(&restored).0.unwrap_or_default(), &rule.path).is_err() {
        return text.to_string();
    }
    restored
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

/// Extracts managed content for conflict previews, excluding surrounding user notes.
pub fn managed_content(text: &str, path: &Path) -> Result<String> {
    Ok(find_block(text, path)?.map(|block| block.inner).unwrap_or_default())
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
    crate::config_file::read_text(path, true)
}

/// Atomic write that goes through symlinks, since global instruction files are
/// often linked from a dotfiles repo. The new text is staged in a file only
/// this write created (see [`crate::config_file::Staged`]), so nothing that
/// holds the temporary name is followed, truncated or published.
pub(crate) fn write_file(path: &Path, text: &str) -> Result<()> {
    let real = match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => fs::canonicalize(path).at(path)?,
        _ => path.to_path_buf(),
    };
    if let Some(parent) = real.parent() {
        fs::create_dir_all(parent).at(parent)?;
    }
    crate::config_file::Staged::write(&real, text.as_bytes(), crate::config_file::existing_mode(&real))?.replace()
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

    fn rule(raw: &str) -> Rule {
        let path = Path::new("r.md");
        let (rule_frontmatter, routing_source) =
            split_harnesses_key(split_frontmatter(raw).0.unwrap_or_default(), path).unwrap();
        Rule {
            name: "r".into(),
            path: path.into(),
            description: None,
            harnesses: parse_harnesses(raw, path).unwrap(),
            body: split_frontmatter(raw).1.trim().into(),
            rule_frontmatter,
            routing_source,
        }
    }

    fn assert_rule_metadata_and_routing(raw: &str, metadata: &str) {
        let rule = rule(raw);
        let rendered = render_rule(&rule).1;
        let kept = split_frontmatter(&rendered).0.unwrap();
        assert_eq!(kept.as_bytes(), metadata.as_bytes());
        let actual: serde_yaml_ng::Value = serde_yaml_ng::from_str(kept).unwrap();
        let expected: serde_yaml_ng::Value = serde_yaml_ng::from_str(metadata).unwrap();
        assert_eq!(actual, expected);

        let restored = restore_harnesses(&rule, &rendered);
        let original = frontmatter_mapping(split_frontmatter(raw).0.unwrap(), &rule.path).unwrap();
        let restored_mapping = frontmatter_mapping(split_frontmatter(&restored).0.unwrap(), &rule.path).unwrap();
        assert_eq!(restored_mapping, original);
        assert_eq!(parse_harnesses(&restored, &rule.path).unwrap(), rule.harnesses);
        assert!(restored.contains(metadata));
    }

    #[test]
    fn rule_strips_multiline_flow_routing_without_changing_metadata() {
        let metadata = "description: Keep café exactly. # comment\npaths: ['src/**']\n";
        for key in FRONTMATTER_KEYS {
            let routing = format!("{key}: [claude,\n  codex] # routing\n");
            for source in [format!("{routing}{metadata}"), format!("{metadata}{routing}")] {
                assert_rule_metadata_and_routing(&format!("---\n{source}---\nBody.\n"), metadata);
            }
        }
    }

    #[test]
    fn rule_preserves_unindented_quoted_scalar_continuations() {
        for metadata in [
            "description: \"first line\nharnesses: are tools\nend\"\n",
            "description: 'first line\ntargets: aren''t routing\nend'\n",
            "metadata: {description: \"first line\nharnesses: are tools\nend\"}\n",
        ] {
            for key in FRONTMATTER_KEYS {
                let routing = format!("{key}: [claude]\n");
                for source in [format!("{routing}{metadata}"), format!("{metadata}{routing}")] {
                    assert_rule_metadata_and_routing(&format!("---\n{source}---\nBody.\n"), metadata);
                }
            }
            assert_rule_metadata_and_routing(&format!("---\n{metadata}---\nBody.\n"), metadata);
        }
    }

    #[test]
    fn rule_preserves_comments_and_crlf_around_routing_entries() {
        for newline in ["\n", "\r\n"] {
            let metadata = "# header\npaths: ['src/**'] # paths\ndescription: |\n  harnesses: documentation\n  targets: examples\n\n# footer\n"
                .replace('\n', newline);
            for key in FRONTMATTER_KEYS {
                for value in [
                    "[claude, codex] # routing\n",
                    "\n- claude # first\n- codex # last\n",
                    "# routing\n  - claude\n\n# internal\n  - codex # last\n",
                ] {
                    let routing = format!("{key}: {value}").replace('\n', newline);
                    for source in [format!("{routing}{metadata}"), format!("{metadata}{routing}")] {
                        let raw = format!("---{newline}{source}---{newline}Body.{newline}");
                        assert_rule_metadata_and_routing(&raw, &metadata);
                    }
                }
            }
        }
    }

    #[test]
    fn rule_omits_frontmatter_when_routing_is_only_key() {
        for newline in ["\n", "\r\n"] {
            for key in FRONTMATTER_KEYS {
                for value in ["[claude] # scope\n", "\n- claude # scope\n", "[claude,\n  codex]\n"] {
                    let raw = format!("---\n{key}: {value}---\nBody.\n").replace('\n', newline);
                    let rule = rule(&raw);
                    let rendered = render_rule(&rule).1;
                    assert_eq!(rendered, "Body.\n");
                    assert_eq!(
                        parse_harnesses(&restore_harnesses(&rule, &rendered), &rule.path).unwrap(),
                        rule.harnesses
                    );
                }
            }
        }
    }

    #[test]
    fn rule_restores_equivalent_routing_values() {
        for source in [
            "description: &name claude\nharnesses: *name\n",
            "description: Keep\n\"harn\\u0065sses\": [claude]\n'targets': [codex]\n",
            "description: Keep\nharnesses: |+\n  claude\n\n\n",
            "description: Keep\nharnesses: &tools [claude]\n",
        ] {
            let metadata = if source.starts_with("description: &") {
                "description: &name claude\n"
            } else {
                "description: Keep\n"
            };
            assert_rule_metadata_and_routing(&format!("---\n{source}---\nBody.\n"), metadata);
        }
    }

    #[test]
    fn rule_preserves_comments_after_routing_and_inside_block_scalars() {
        for (source, metadata) in [
            (
                "harnesses: [claude]\n  # keep this indented comment\n# keep this comment too\n",
                "  # keep this indented comment\n# keep this comment too\n",
            ),
            ("# header\nharnesses: [claude]\n# footer\n", "# header\n# footer\n"),
            ("harnesses: |\n  claude\n  # part of the routing scalar\n# footer\n", "# footer\n"),
            (
                "description: |\n  # keep this scalar content\n\nharnesses: [claude]\n",
                "description: |\n  # keep this scalar content\n\n",
            ),
        ] {
            assert_rule_metadata_and_routing(&format!("---\n{source}---\nBody.\n"), metadata);
        }
    }

    #[test]
    fn unsafe_routing_source_ranges_are_discovery_errors() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(INSTRUCTIONS_DIR)).unwrap();
        let path = tmp.path().join("instructions/scoped.md");
        for source in [
            "{harnesses: [claude], description: Keep}\n",
            "? harnesses\n: [claude]\ndescription: Keep\n",
            "harnesses: &tools [claude]\nmetadata: *tools\n",
        ] {
            let raw = format!("---\n{source}---\nBody.\n");
            assert!(parse_harnesses(&raw, &path).is_ok(), "invalid fixture: {raw}");
            fs::write(&path, raw).unwrap();
            assert!(discover(tmp.path()).is_err(), "accepted unsafe removal: {source}");
        }
    }

    #[test]
    fn multiline_harnesses_and_targets_restrict_rules() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(INSTRUCTIONS_DIR)).unwrap();
        let path = tmp.path().join("instructions/scoped.md");
        for key in FRONTMATTER_KEYS {
            for value in [
                "\n  - only\n  - 'second'",
                "\n- only\n- second",
                "[only, second] # scope",
                "\n  - only # first\n\n# scope\n  - second",
            ] {
                fs::write(&path, format!("---\n{key}: {value}\n---\nScoped.\n")).unwrap();
                let rules = discover(tmp.path()).unwrap();
                assert!(rules[0].applies_to("only"));
                assert!(rules[0].applies_to("second"));
                assert!(!rules[0].applies_to("other"));
                assert_eq!(render_rule(&rules[0]).1, "Scoped.\n");
            }
        }
    }

    #[test]
    fn routing_ignores_nested_and_embedded_keys_and_preserves_rule_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(INSTRUCTIONS_DIR)).unwrap();
        let path = tmp.path().join("instructions/scoped.md");
        for metadata in [
            "description: |\n  harnesses: are agent tools\n  targets: are deprecated\n",
            "description: >\n  harnesses: are agent tools\n  targets: are deprecated\n",
            "description: \"Some tools\n  harnesses: are agent tools\n  targets: are deprecated\"\n",
            "metadata:\n  harnesses: {arbitrary: true}\n  targets: 42\n",
        ] {
            for key in FRONTMATTER_KEYS {
                for routing in [String::new(), format!("{key}: [only]\n")] {
                    for frontmatter in [format!("{routing}{metadata}"), format!("{metadata}{routing}")] {
                        fs::write(&path, format!("---\n{frontmatter}---\nScoped.\n")).unwrap();
                        let rules = discover(tmp.path()).unwrap();
                        let rule = &rules[0];
                        assert!(rule.applies_to("only"));
                        assert_eq!(rule.applies_to("other"), routing.is_empty());
                        let rendered = render_rule(rule).1;
                        assert_eq!(rendered, format!("---\n{metadata}---\n\nScoped.\n"));
                        let restored = restore_harnesses(rule, &rendered);
                        fs::write(&path, &restored).unwrap();
                        assert_eq!(discover(tmp.path()).unwrap()[0].harnesses, rule.harnesses);
                        assert!(restored.contains(metadata));
                    }
                }
            }
        }
    }

    #[test]
    fn routing_accepts_yaml_strings_and_validates_both_top_level_keys() {
        let path = Path::new("scoped.md");
        for key in FRONTMATTER_KEYS {
            for value in ["only", "'only'", "\"only\"", "\n  only", "[only]", "|\n  only"] {
                let text = format!("---\n{key}: {value}\n---\nScoped.\n");
                let expected = if value.starts_with('|') { "only\n" } else { "only" };
                assert_eq!(parse_harnesses(&text, path).unwrap(), Some(vec![expected.into()]));
            }
        }
        for text in [
            "harnesses: [only]\ntargets: false",
            "harnesses: false\ntargets: [only]",
            "harnesses: [only, 42]",
            "targets: [only, null]",
            "harnesses: 42",
            "targets: true",
        ] {
            assert!(parse_harnesses(&format!("---\n{text}\n---\n"), path).is_err(), "accepted {text}");
        }
        assert_eq!(
            parse_harnesses("---\nharnesses: [only]\ntargets: [legacy]\n---\n", path).unwrap(),
            Some(vec!["only".into()])
        );
    }

    #[test]
    fn invalid_harness_restrictions_are_errors() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(INSTRUCTIONS_DIR)).unwrap();
        let path = tmp.path().join("instructions/scoped.md");
        for key in FRONTMATTER_KEYS {
            for value in [
                "",
                "[only",
                "[only,,other]",
                "\n  - [nested]",
                "null",
                "{only: true}",
                "[\"unterminated]",
                "\n  - only\n    nested: bad",
            ] {
                fs::write(&path, format!("---\n{key}: {value}\n---\nScoped.\n")).unwrap();
                assert!(discover(tmp.path()).is_err(), "accepted {key}: {value}");
            }
        }
        fs::write(&path, "---\nharnesses:\n  - only\n").unwrap();
        assert!(discover(tmp.path()).is_err());
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
    fn rule_keeps_frontmatter_except_harnesses() {
        let frag = rule;
        let (name, text) = render_rule(&frag("---\ntargets:\n  - claude\npaths:\n  - \"src/**\"\n---\nDo X.\n"));
        assert_eq!(name, "r.md");
        assert_eq!(text, "---\npaths:\n  - \"src/**\"\n---\n\nDo X.\n");
        assert_eq!(render_rule(&frag("---\ntargets: [claude]\n---\nDo Y.")).1, "Do Y.\n");
        assert_eq!(
            render_rule(&frag("---\nharnesses: [claude]\ndescription: d\n---\nDo Z.")).1,
            "---\ndescription: d\n---\n\nDo Z.\n"
        );
        assert_eq!(render_rule(&frag("Plain.")).1, "Plain.\n");

        let f = frag("---\nharnesses:\n  - claude\npaths: [x]\n---\nDo X.\n");
        let edited = "---\npaths: [x]\n---\n\nDo X better.\n";
        assert_eq!(restore_harnesses(&f, edited), "---\nharnesses:\n  - claude\npaths: [x]\n---\n\nDo X better.\n");
        assert_eq!(restore_harnesses(&f, "Plain.\n"), "---\nharnesses:\n  - claude\n---\nPlain.\n");
        assert_eq!(restore_harnesses(&frag("Plain."), edited), edited);
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

    #[cfg(unix)]
    #[test]
    fn bookkeeping_writes_never_follow_a_planted_temp_name() {
        let tmp = tempfile::tempdir().unwrap();
        let victim = tmp.path().join("victim");
        fs::write(&victim, "victim\n").unwrap();
        let planted = tmp.path().join(format!(".{MARKER}.sk1ll-tmp"));
        std::os::unix::fs::symlink(&victim, &planted).unwrap();
        let index = RulesIndex::from([("git.md".to_string(), "sha256:1".to_string())]);
        save_rules_index(tmp.path(), &index).unwrap();
        assert_eq!(load_rules_index(tmp.path()).unwrap(), index);
        assert!(fs::symlink_metadata(tmp.path().join(MARKER)).unwrap().file_type().is_file(), "never a symlink");
        assert_eq!(fs::read_to_string(&victim).unwrap(), "victim\n");
        assert!(fs::symlink_metadata(&planted).unwrap().file_type().is_symlink());
    }
}
