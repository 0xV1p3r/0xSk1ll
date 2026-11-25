//! Contracts of user-facing formats: rule and skill front matter, the
//! instructions block markers, Item refs and IDs (and the `.sk1ll-id`
//! sidecar), the Assignment file, the manifest and lockfile, and what
//! redaction hides. Each is pinned by golden examples and the malformed
//! input it rejects.

use sk1ll_core::assignment::ASSIGNMENT_FILE;
use sk1ll_core::git::plumbing::{has_credentials, without_credentials};
use sk1ll_core::instructions::{Rule, discover, managed_content, render_rule};
use sk1ll_core::redact::redact;
use sk1ll_core::skill::{frontmatter_field, split_frontmatter};
use sk1ll_core::{Assignment, ItemId, ItemRef, Lockfile, Manifest};
use sk1ll_testkit::write;
use std::{fs, path::Path};

fn discovered(raw: &str) -> sk1ll_core::Result<Rule> {
    let tmp = tempfile::tempdir().unwrap();
    write(&tmp.path().join("instructions/r.md"), raw);
    discover(tmp.path()).map(|mut rules| rules.remove(0))
}

fn rule(raw: &str) -> Rule {
    discovered(raw).unwrap_or_else(|e| panic!("{raw:?}: {e}"))
}

fn id(s: &str) -> ItemId {
    ItemId::from_text(s).unwrap()
}

fn item(s: &str) -> ItemRef {
    s.parse().unwrap()
}

// Rule front matter: `harnesses` (or the older `targets`) routes a rule;
// every other key, comment and byte of it reaches the harness unchanged.

/// Metadata a routing key may sit before or after, including text that
/// only looks like routing (in scalars, nested maps and comments).
const METADATA: &[&str] = &[
    "",
    "description: Keep café exactly. # comment\npaths: ['src/**']\n",
    "description: \"first line\nharnesses: are tools\nend\"\n",
    "description: 'first line\ntargets: aren''t routing\nend'\n",
    "metadata: {description: \"first line\nharnesses: are tools\nend\"}\n",
    "# header\npaths: ['src/**'] # paths\ndescription: |\n  harnesses: documentation\n  targets: examples\n\n# footer\n",
    "description: >\n  harnesses: are agent tools\n  targets: are deprecated\n",
    "description: \"Some tools\n  harnesses: are agent tools\n  targets: are deprecated\"\n",
    "metadata:\n  harnesses: {arbitrary: true}\n  targets: 42\n",
];

/// Spellings of the list `[claude, codex]`.
const ROUTING: &[&str] = &[
    "[claude, codex] # routing\n",
    "[claude,\n  codex] # routing\n",
    "\n- claude # first\n- codex # last\n",
    "\n  - claude\n  - 'codex'\n",
    "# routing\n  - claude\n\n# internal\n  - codex # last\n",
];

#[test]
fn routing_is_removed_from_a_rendered_rule_and_every_other_byte_of_its_front_matter_kept() {
    for newline in ["\n", "\r\n"] {
        for metadata in METADATA {
            let metadata = metadata.replace('\n', newline);
            let routings = ["harnesses", "targets"].iter().flat_map(|key| ROUTING.iter().map(move |v| (key, v)));
            for routing in routings.map(Some).chain([None]) {
                let sources = match routing {
                    None => vec![metadata.clone()],
                    Some((key, value)) => {
                        let routing = format!("{key}: {value}").replace('\n', newline);
                        vec![format!("{routing}{metadata}"), format!("{metadata}{routing}")]
                    }
                };
                for source in sources {
                    if source.is_empty() {
                        continue;
                    }
                    let raw = format!("---{newline}{source}---{newline}Body.{newline}");
                    let rule = rule(&raw);
                    let (name, rendered) = render_rule(&rule);
                    assert_eq!(name, "r.md");
                    assert!(rule.applies_to("claude") && rule.applies_to("codex"), "{raw:?}");
                    assert_eq!(rule.applies_to("other"), routing.is_none(), "{raw:?}");
                    if metadata.is_empty() {
                        assert_eq!(rendered, "Body.\n", "{raw:?}");
                    } else {
                        assert_eq!(split_frontmatter(&rendered).0, Some(metadata.as_str()), "{raw:?}");
                    }
                }
            }
        }
    }
    assert_eq!(render_rule(&rule("Plain.")).1, "Plain.\n");
    assert_eq!(
        render_rule(&rule("---\ntargets:\n  - claude\npaths:\n  - \"src/**\"\n---\nDo X.\n")).1,
        "---\npaths:\n  - \"src/**\"\n---\n\nDo X.\n"
    );
}

/// Routing spelled through anchors, escapes or block scalars, and
/// comments around it: what is left is exactly the rest.
#[test]
fn routing_written_any_yaml_way_leaves_exactly_the_rest_of_the_front_matter() {
    for (source, metadata) in [
        ("description: &name claude\nharnesses: *name\n", "description: &name claude\n"),
        ("description: Keep\n\"harn\\u0065sses\": [claude]\n'targets': [codex]\n", "description: Keep\n"),
        ("description: Keep\nharnesses: |+\n  claude\n\n\n", "description: Keep\n"),
        ("description: Keep\nharnesses: &tools [claude]\n", "description: Keep\n"),
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
        let rendered = render_rule(&rule(&format!("---\n{source}---\nBody.\n"))).1;
        assert_eq!(split_frontmatter(&rendered).0, Some(metadata), "{source:?}");
    }
}

#[test]
fn a_routing_value_names_one_harness_or_a_list_and_harnesses_wins_over_targets() {
    for key in ["harnesses", "targets"] {
        for value in ["only", "'only'", "\"only\"", "\n  only", "[only]", "|\n  only"] {
            let expected = if value.starts_with('|') { "only\n" } else { "only" };
            assert_eq!(rule(&format!("---\n{key}: {value}\n---\n")).harnesses, Some(vec![expected.into()]));
        }
    }
    assert_eq!(rule("---\nharnesses: [only]\ntargets: [legacy]\n---\n").harnesses, Some(vec!["only".into()]));
}

#[test]
fn malformed_routing_or_front_matter_it_cannot_remove_safely_is_a_discovery_error() {
    let mut sources: Vec<String> = [
        // Routing that can't be cut out without changing the rest.
        "{harnesses: [claude], description: Keep}",
        "? harnesses\n: [claude]\ndescription: Keep",
        "harnesses: &tools [claude]\nmetadata: *tools",
        // Both keys are validated.
        "harnesses: [only]\ntargets: false",
        "harnesses: false\ntargets: [only]",
        "harnesses: [only, 42]",
        "targets: [only, null]",
        "harnesses: 42",
        "targets: true",
    ]
    .map(String::from)
    .into();
    for key in ["harnesses", "targets"] {
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
            sources.push(format!("{key}: {value}"));
        }
    }
    for source in sources {
        assert!(discovered(&format!("---\n{source}\n---\nBody.\n")).is_err(), "accepted {source:?}");
    }
    assert!(discovered("---\nharnesses:\n  - only\n").is_err(), "unterminated front matter");
}

#[test]
fn skill_front_matter_splits_off_and_reads_inline_and_folded_fields() {
    assert_eq!(split_frontmatter("---\na: 1\n---\nbody\n"), (Some("a: 1\n"), "body\n"));
    assert_eq!(split_frontmatter("body"), (None, "body"));
    assert_eq!(split_frontmatter("---\nunterminated"), (None, "---\nunterminated"));
    let t = "---\nname: pdf\ndescription: \"Work with PDFs\"\n---\nbody";
    assert_eq!(frontmatter_field(t, "name").as_deref(), Some("pdf"));
    assert_eq!(frontmatter_field(t, "description").as_deref(), Some("Work with PDFs"));
    let t = "---\ndescription: >\n  line one\n  line two\nname: x\n---\n";
    assert_eq!(frontmatter_field(t, "description").as_deref(), Some("line one line two"));
    assert_eq!(frontmatter_field("no frontmatter", "name"), None);
}

#[test]
fn instructions_markers_bound_the_managed_text_and_report_a_missing_end() {
    let path = Path::new("AGENTS.md");
    let begin = "<!-- 0xSk1ll:begin sha256:ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb -->";
    for (text, want) in [
        (format!("# Mine\n\n{begin}\na\n<!-- 0xSk1ll:end -->\n\n# After\n"), "a"),
        (format!("{begin}\r\na\r\n<!-- 0xSk1ll:end -->\r\n"), "a"),
        (format!("inline {begin}\na\n<!-- 0xSk1ll:end -->\n"), ""),
    ] {
        assert_eq!(managed_content(&text, path).unwrap(), want);
    }
    let error = managed_content(&format!("{begin}\nmissing end\n"), path).unwrap_err().to_string();
    assert!(error.contains("unterminated 0xSk1ll block"), "{error}");
}

#[test]
fn secrets_are_redacted_from_messages_and_stripped_from_urls() {
    assert_eq!(
        redact("`git clone http://ws:tok@h:1/git/repo.git x` failed: 'https://a@b/c'"),
        "`git clone http://***@h:1/git/repo.git x` failed: 'https://***@b/c'"
    );
    assert_eq!(redact("http://h/git/repo.git"), "http://h/git/repo.git");
    assert_eq!(redact("Authorization: Basic d3M6dG9r here"), "Authorization: Basic *** here");
    assert_eq!(redact("Bearer abc"), "Bearer ***");
    assert_eq!(
        without_credentials("https://ws:t0k@server.example:8443/git/repo.git"),
        "https://server.example:8443/git/repo.git"
    );
    assert_eq!(without_credentials("http://h/git/repo.git/"), "http://h/git/repo.git");
    assert!(has_credentials("https://ws:t0k@server.example/git/repo.git"));
    assert!(!has_credentials("https://server.example/git/a@b"));
}

#[test]
fn the_assignment_file_round_trips_and_a_missing_one_is_the_default() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested").join(ASSIGNMENT_FILE);
    assert_eq!(Assignment::load(&path).unwrap(), Assignment::default());

    let mut a = Assignment::default();
    a.set_skill("claude", &id("01J0000000000000000000PDF0"), false);
    a.set_new_rules_default("claude", false);
    a.save(&path).unwrap();
    assert_eq!(Assignment::load(&path).unwrap(), a);

    fs::write(&path, r#"{"harnesses":{"claude":{"skills":{"x":false}}}}"#).unwrap();
    let partial = Assignment::load(&path).unwrap();
    assert!(!partial.skill_enabled("claude", &id("x")));
    assert!(partial.harness("claude").new_skills_default);
    fs::write(&path, r#"{"harnesses":{"claude":{"skills":{"":false}}}}"#).unwrap();
    assert!(Assignment::load(&path).is_err(), "an empty id is refused");
}

#[test]
fn item_refs_round_trip_and_name_the_item_a_path_belongs_to() {
    for s in ["skill:pdf", "rule:10-git", "manifest", "other:notes/a.txt"] {
        assert_eq!(item(s).to_string(), s);
        assert_eq!(serde_json::to_string(&item(s)).unwrap(), format!("\"{s}\""));
    }
    assert!("bogus:x".parse::<ItemRef>().is_err() && "skill:".parse::<ItemRef>().is_err());
    for (path, item) in [
        ("imported/web/sub/x.md", "skill:web"),
        ("personal/.gitkeep", "other:personal/.gitkeep"),
        ("instructions/git.md", "rule:git"),
        ("instructions/.gitkeep", "other:instructions/.gitkeep"),
        ("sk1ll.lock", "manifest"),
    ] {
        assert_eq!(ItemRef::for_path(Path::new(path)).to_string(), item);
    }
    assert_eq!(
        sk1ll_core::default_commit_message(&[item("skill:pdf"), item("rule:git-flow")], "laptop"),
        "commit pdf, git-flow from laptop"
    );
}

/// Byte strings that are hard to name: quotes, backslashes, escapes
/// spelled out literally, control characters and invalid UTF-8.
#[cfg(unix)]
fn tricky_names() -> Vec<Vec<u8>> {
    let atoms: [&[u8]; 11] =
        [b"\"", b"\\", b"\\377", b"\\\"", b"\n", b"\t", b"\xff", b"\xc3", "é".as_bytes(), b"a", b"/"];
    let mut names: Vec<Vec<u8>> = vec![b"x".to_vec()];
    for a in atoms {
        for b in atoms {
            for c in atoms {
                names.push([a, b, c].concat());
                names.push([a, b, c, b".txt"].concat());
            }
        }
    }
    names.extend([b"bad\xff.txt".to_vec(), b"\"bad\\377.txt\"".to_vec(), b"\"\"".to_vec()]);
    // Only well-formed relative paths: no empty, `.` or `..` components.
    names.retain(|n| n.split(|b| *b == b'/').all(|c| !c.is_empty() && c != b"." && c != b".."));
    names.sort();
    names.dedup();
    names
}

#[cfg(unix)]
#[test]
fn other_item_names_are_injective_and_round_trip() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let names = tricky_names();
    assert!(names.len() > 1000);
    let mut seen = std::collections::BTreeMap::new();
    for raw in &names {
        let path = Path::new(OsStr::from_bytes(raw));
        let it = ItemRef::for_path(path);
        let ItemRef::Other(_) = &it else { continue };
        assert_eq!(it.to_string().parse::<ItemRef>().unwrap(), it, "{raw:?}");
        assert_eq!(it.other_path().as_deref(), Some(path), "{it}");
        if let Some(prev) = seen.insert(it.clone(), raw.clone()) {
            panic!("{prev:?} and {raw:?} are both {it}");
        }
    }
    for (raw, name) in [
        (b"bad\xff.txt".as_slice(), r#"other:"bad\377.txt""#),
        (br#""bad\377.txt""#, r#"other:"\"bad\\377.txt\"""#),
        (br"notes\b", r"other:notes\b"),
        (b"a\nb", r#"other:"a\nb""#),
    ] {
        assert_eq!(ItemRef::for_path(Path::new(OsStr::from_bytes(raw))).to_string(), name);
    }
    // Malformed quoted names name no path.
    for bad in [r#"other:""#, r#"other:"a"#, r#"other:"\q""#, r#"other:"\777""#, r#"other:"a"b""#] {
        assert_eq!(item(bad).other_path(), None, "{bad}");
    }
}

#[test]
fn item_ids_and_sidecars_accept_one_nonempty_line() {
    let tmp = tempfile::tempdir().unwrap();
    let skill = tmp.path().join("personal/pdf");
    write(&skill.join("SKILL.md"), "PDFs.\n");
    let sidecar = skill.join(".sk1ll-id");
    for line in ["pdf-id", " pdf-id \n", "\tpdf-id\r\n"] {
        write(&sidecar, line);
        assert_eq!(ItemId::from_text(line), Some(id("pdf-id")));
        assert_eq!(sk1ll_core::skill::discover(tmp.path()).unwrap()[0].id, Some(id("pdf-id")));
        assert_eq!(serde_json::to_string(&id("pdf-id")).unwrap(), r#""pdf-id""#);
    }
    for bytes in [b"".as_slice(), b" \r\n", b"one\ntwo", b"one\0two", b"one\n\n", b"\xff"] {
        fs::write(&sidecar, bytes).unwrap();
        assert!(sk1ll_core::skill::discover(tmp.path()).unwrap_err().to_string().contains(".sk1ll-id"));
    }
}

#[test]
fn the_manifest_and_lockfile_load_their_documented_tables() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    assert!(Lockfile::load(root).unwrap().skills.is_empty());
    write(&root.join("sk1ll.toml"), "[import.pdf]\nsource = 'github:owner/repo'\nid = 'pdf-id'\n");
    write(
        &root.join("sk1ll.lock"),
        concat!(
            "[skills.pdf]\nsource = 'github:owner/repo'\n",
            "ref = 'main'\ncommit = '0123456789abcdef0123456789abcdef01234567'\n",
            "hash = 'sha256:seed'\n",
        ),
    );
    let imports = Manifest::load(root).unwrap();
    let pdf = &imports.import["pdf"];
    assert_eq!((pdf.source.as_str(), pdf.git_ref.as_str(), pdf.path.as_str()), ("github:owner/repo", "HEAD", ""));
    assert_eq!(pdf.id.as_ref(), Some(&id("pdf-id")));
    let lock = Lockfile::load(root).unwrap();
    let pdf = &lock.skills["pdf"];
    assert_eq!(
        (pdf.source.as_str(), pdf.git_ref.as_str(), pdf.path.as_str(), pdf.commit.as_str(), pdf.hash.as_str()),
        ("github:owner/repo", "main", "", "0123456789abcdef0123456789abcdef01234567", "sha256:seed"),
    );
    lock.save(root).unwrap();
    let saved = sk1ll_testkit::read(&root.join("sk1ll.lock"));
    assert!(saved.contains("[skills.pdf]\n") && saved.contains("ref = \"main\""));
    assert_eq!(Lockfile::load(root).unwrap().skills, lock.skills);
    for invalid in ["[skills", "[skills.pdf]\nref = 'main'\n", "skills = []\n"] {
        write(&root.join("sk1ll.lock"), invalid);
        assert!(
            matches!(Lockfile::load(root), Err(sk1ll_core::Error::Parse { path, .. }) if path == root.join("sk1ll.lock"))
        );
    }
}
