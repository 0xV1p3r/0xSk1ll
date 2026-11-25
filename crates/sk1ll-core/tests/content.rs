//! Browsing and editing item content (`sk1ll_core::content`) on real repos.

mod common;

use common::Scratch;
use sk1ll_core::content::*;
use sk1ll_core::sync::unmanaged;
use sk1ll_core::{Assignment, Change, Error, Harness, ItemId, ItemRef, Repo, Result};
use sk1ll_testkit::git::git;
use std::{collections::BTreeMap, fs, path::Path};

fn write(path: &Path, bytes: impl AsRef<[u8]>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}
fn query(item: &str) -> ContentQuery {
    ContentQuery { item: item.into(), hash: None, harness: None }
}
fn read(repo: &Repo, item: &str, path: &str) -> Result<FileContent> {
    file(repo, &BTreeMap::new(), &FileQuery { content: query(item), path: path.into() })
}
fn commit(repo: &Repo) {
    git(repo.root(), &["add", "-A"]);
    git(repo.root(), &["commit", "-qm", "baseline"]);
}

#[test]
fn listing_and_browsing_use_draft_and_fall_back_to_head_for_deleted_items() {
    let tmp = Scratch::new();
    let repo = tmp.init();
    write(&repo.personal_dir().join("pdf/SKILL.md"), "---\ndescription: baseline\n---\nold");
    let skill_id: ItemId = "pdf-id".parse().unwrap();
    write(&repo.personal_dir().join("pdf/.sk1ll-id"), "pdf-id\n");
    write(&repo.imported_dir().join("remote/SKILL.md"), "import");
    write(&repo.root().join("instructions/git.md"), "---\ndescription: Git conventions\n---\nold rule");
    commit(&repo);
    write(&repo.personal_dir().join("pdf/SKILL.md"), "---\ndescription: draft\n---\nnew");
    write(&repo.personal_dir().join("new/SKILL.md"), "added");
    fs::remove_file(repo.root().join("instructions/git.md")).unwrap();
    let items = list(&repo, &BTreeMap::new(), &Assignment::default(), Some("HEAD")).unwrap();
    let pdf = items.iter().find(|s| s.name == "pdf").unwrap();
    assert_eq!(pdf.description.as_deref(), Some("draft"));
    assert_eq!(pdf.draft, Some(Change::Modified));
    assert!(pdf.in_baseline);
    assert_eq!(pdf.id.as_ref(), Some(&skill_id));
    let files = tree(&repo, &BTreeMap::new(), &query("skill:pdf")).unwrap();
    assert_eq!(files.iter().map(|file| file.path.as_str()).collect::<Vec<_>>(), ["SKILL.md"]);
    let new = items.iter().find(|s| s.name == "new").unwrap();
    assert_eq!(new.draft, Some(Change::Added));
    assert!(!new.in_baseline);
    let rule = items.iter().find(|s| s.name == "git").unwrap();
    assert_eq!(rule.draft, Some(Change::Deleted));
    assert_eq!(rule.description.as_deref(), Some("Git conventions"));
    assert!(read(&repo, "skill:pdf", "SKILL.md").unwrap().text.unwrap().ends_with("new"));
    assert!(read(&repo, "rule:git", "git.md").unwrap().text.unwrap().ends_with("old rule"));
    fs::remove_dir_all(repo.personal_dir().join("pdf")).unwrap();
    assert!(read(&repo, "skill:pdf", "SKILL.md").unwrap().text.unwrap().ends_with("old"));
    let tree = tree(&repo, &BTreeMap::new(), &query("skill:pdf")).unwrap();
    assert_eq!(tree.iter().map(|file| file.path.as_str()).collect::<Vec<_>>(), ["SKILL.md"]);
    let items = list(&repo, &BTreeMap::new(), &Assignment::default(), Some("HEAD")).unwrap();
    assert_eq!(items.iter().find(|s| s.name == "pdf").unwrap().id.as_ref(), Some(&skill_id));
    assert!(editable_path(&repo, &BTreeMap::new(), "remote").unwrap_err().to_string().contains("sk1ll fork remote"));
}

#[cfg(unix)]
#[test]
fn a_personal_sidecar_is_reserved_and_malformed_metadata_is_an_error() {
    let tmp = Scratch::new();
    let repo = tmp.init();
    repo.create_item("skill", "pdf", "PDF").unwrap();
    let path = repo.personal_dir().join("pdf/.sk1ll-id");
    let before = fs::read_to_string(&path).unwrap();
    let file = FileQuery { content: query("skill:pdf"), path: ".sk1ll-id".into() };
    assert!(read(&repo, "skill:pdf", ".sk1ll-id").unwrap_err().to_string().contains("reserved"));
    assert!(
        write_file(
            &repo,
            &WriteFileRequest { file, expected: sk1ll_core::hash::hash_str(&before), content: "other".into() }
        )
        .unwrap_err()
        .to_string()
        .contains("reserved")
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), before);
    fs::write(&path, "one\ntwo\n").unwrap();
    assert!(matches!(item_ids_on_disk(repo.root()), Err(Error::Parse { .. })));
    fs::remove_file(&path).unwrap();
    assert_eq!(item_ids_on_disk(repo.root()).unwrap()[&ItemRef::Skill("pdf".into())], None);

    std::os::unix::fs::symlink("plausible-id", &path).unwrap();
    commit(&repo);
    fs::remove_dir_all(repo.personal_dir().join("pdf")).unwrap();
    assert!(matches!(item_ids_at(repo.root(), "HEAD"), Err(Error::Parse { .. })));
    assert!(read(&repo, "skill:pdf", ".sk1ll-id").unwrap_err().to_string().contains("reserved"));
}

#[cfg(unix)]
#[test]
fn rule_symlinks_cannot_carry_ids_on_disk_or_in_git() {
    let tmp = Scratch::new();
    let repo = tmp.init();
    repo.create_item("rule", "style", "").unwrap();
    let path = repo.root().join("instructions/linked.md");
    std::os::unix::fs::symlink("style.md", &path).unwrap();
    assert!(item_ids_on_disk(repo.root()).unwrap_err().to_string().contains("linked.md"));
    commit(&repo);
    assert!(item_ids_at(repo.root(), "HEAD").unwrap_err().to_string().contains("linked.md"));
}

#[test]
fn files_reject_escape_flag_binary_and_bound_memory() {
    let tmp = Scratch::new();
    let repo = tmp.init();
    write(&repo.personal_dir().join("pdf/SKILL.md"), "skill");
    write(&repo.personal_dir().join("pdf/binary"), b"\x00\xff");
    write(&repo.personal_dir().join("pdf/huge"), vec![b'x'; MAX_FILE_BYTES as usize + 1]);
    for path in ["../sk1ll.toml", "/etc/passwd", "a/../../sk1ll.toml", ""] {
        assert!(read(&repo, "skill:pdf", path).is_err(), "{path}");
    }
    let binary = read(&repo, "skill:pdf", "binary").unwrap();
    assert!(binary.binary && binary.text.is_none());
    let huge = read(&repo, "skill:pdf", "huge").unwrap();
    assert!(huge.too_large && huge.text.is_none());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(repo.root().join("sk1ll.toml"), repo.personal_dir().join("pdf/escape")).unwrap();
        assert!(read(&repo, "skill:pdf", "escape").is_err());
    }
    commit(&repo);
    fs::remove_dir_all(repo.personal_dir().join("pdf")).unwrap();
    assert!(read(&repo, "skill:pdf", "binary").unwrap().binary);
    assert!(read(&repo, "skill:pdf", "huge").unwrap().too_large);
    #[cfg(unix)]
    assert!(read(&repo, "skill:pdf", "escape").is_err());
}

#[test]
fn unmanaged_browsing_requires_variant_when_content_differs() {
    let tmp = Scratch::new();
    let repo = tmp.init();
    let hs: BTreeMap<_, _> = ["a", "b"]
        .into_iter()
        .map(|h| {
            write(&tmp.join(h).join("pdf/SKILL.md"), h);
            (h.into(), Harness { skills: Some(tmp.join(h).to_string_lossy().into_owned()), ..Default::default() })
        })
        .collect();
    assert!(tree(&repo, &hs, &query("unmanaged:pdf")).is_err());
    let hash = unmanaged::discover(&repo, &hs).unwrap()[0].variants[0].hash.clone();
    let q = ContentQuery { hash: Some(hash), ..query("unmanaged:pdf") };
    assert_eq!(tree(&repo, &hs, &q).unwrap()[0].path, "SKILL.md");
    assert!(file(&repo, &hs, &FileQuery { content: q, path: "SKILL.md".into() }).unwrap().text.is_some());
    assert!(editable_path(&repo, &hs, "pdf").unwrap_err().to_string().contains("sk1ll adopt"));
}

#[test]
fn removing_only_skill_md_still_serves_the_remaining_draft_files() {
    let tmp = Scratch::new();
    let repo = tmp.init();
    write(&repo.personal_dir().join("pdf/SKILL.md"), "baseline");
    write(&repo.personal_dir().join("pdf/data.txt"), "baseline data");
    commit(&repo);
    fs::remove_file(repo.personal_dir().join("pdf/SKILL.md")).unwrap();
    write(&repo.personal_dir().join("pdf/data.txt"), "draft data");
    assert_eq!(read(&repo, "skill:pdf", "data.txt").unwrap().text.as_deref(), Some("draft data"));
    assert!(read(&repo, "skill:pdf", "SKILL.md").is_err());
}

/// sk1ll's names inside a skill are its content, browsed and edited like
/// any other; only `.git`, the scratch directory and a copy's placement
/// marker at the root are left out.
#[test]
fn sk1ll_names_below_a_skills_root_are_browsed_and_edited() {
    let tmp = Scratch::new();
    let repo = tmp.init();
    let root = repo.personal_dir().join("pdf");
    write(&root.join("SKILL.md"), "skill");
    write(&root.join(sk1ll_core::sync::PLACEMENT_MARKER), "token");
    write(&root.join("x/.sk1ll-state/f"), "state");
    write(&root.join("x/.sk1ll-placement"), "not a marker");
    write(&root.join(".git/HEAD"), "ref");
    write(&root.join("x/.sk1ll-tmp/scratch"), "scratch");
    let paths: Vec<_> =
        tree(&repo, &BTreeMap::new(), &query("skill:pdf")).unwrap().into_iter().map(|e| e.path).collect();
    assert_eq!(paths, ["SKILL.md", "x", "x/.sk1ll-placement", "x/.sk1ll-state", "x/.sk1ll-state/f"]);
    assert_eq!(read(&repo, "skill:pdf", "x/.sk1ll-state/f").unwrap().text.as_deref(), Some("state"));
    #[cfg(unix)]
    {
        let edit = |path: &str| {
            let expected = read(&repo, "skill:pdf", path).unwrap().fingerprint.unwrap();
            let file = FileQuery { content: query("skill:pdf"), path: path.into() };
            write_file(&repo, &WriteFileRequest { file, content: "edited".into(), expected })
        };
        assert!(edit("x/.sk1ll-state/f").unwrap());
        assert_eq!(fs::read_to_string(root.join("x/.sk1ll-state/f")).unwrap(), "edited");
        assert!(edit(sk1ll_core::sync::PLACEMENT_MARKER).unwrap_err().to_string().contains("reserved"));
    }
}

#[cfg(unix)]
#[test]
fn ancestor_swap_cannot_redirect_file_or_tree() {
    for listing in [false, true] {
        let tmp = Scratch::new();
        let repo = tmp.init();
        let root = repo.personal_dir().join("pdf");
        write(&root.join("SKILL.md"), "safe");
        write(&root.join("docs/data"), "safe");
        let outside = tmp.join("outside");
        write(&outside.join("SKILL.md"), "secret");
        write(&outside.join("docs/data"), "secret");
        write(&outside.join("secret"), "secret");
        let swap = root.clone();
        let _access = sk1ll_core::failpoints::once(sk1ll_core::failpoints::Point::ContentAccess, move |_| {
            fs::rename(&swap, swap.with_file_name("old-pdf")).unwrap();
            std::os::unix::fs::symlink(outside, swap).unwrap();
        });
        if listing {
            let result = tree(&repo, &BTreeMap::new(), &query("skill:pdf"));
            if let Ok(entries) = result {
                assert!(!entries.iter().any(|e| e.path == "secret"));
            }
        } else {
            let result = read(&repo, "skill:pdf", "docs/data");
            if let Ok(file) = result {
                assert_eq!(file.text.as_deref(), Some("safe"));
            }
        }
    }
}
