//! The Draft through `Repo`: what it lists, its diffs, committing and
//! restoring items, and pushing, on real clones with real git.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::*;
use sk1ll_core::draft::{Change, ItemRef};
use sk1ll_core::{Error, Manifest, Repo};

fn ids_at_head(repo: &Repo) -> std::collections::BTreeMap<ItemRef, Option<sk1ll_core::ItemId>> {
    sk1ll_core::content::item_ids_at(repo.root(), "HEAD").unwrap()
}

#[test]
fn the_draft_lists_each_changed_item_once_with_its_paths_and_diff() {
    let f = Fleet::new();
    let r = f.a();
    assert!(r.draft().unwrap().is_empty());
    write(&f.a.join("personal/pdf/SKILL.md"), "changed\n");
    write(&f.a.join("personal/new/SKILL.md"), "new\n");
    write(&f.a.join("personal/new/deep/x.md"), "x\n");
    fs::remove_dir_all(f.a.join("imported/web")).unwrap();
    write(&f.a.join("instructions/git.md"), "changed\n");
    git(&f.a, &["mv", "instructions/style.md", "instructions/tone.md"]);
    write(&f.a.join("sk1ll.toml"), "# edited\n");
    write(&f.a.join("notes.txt"), "n\n");

    use Change::*;
    let expect = [
        ("skill:new", Added),
        ("skill:pdf", Modified),
        ("skill:web", Deleted),
        ("rule:git", Modified),
        ("rule:style", Deleted),
        ("rule:tone", Added),
        ("manifest", Modified),
        ("other:notes.txt", Added),
    ];
    assert_eq!(items(&r), expect.map(|(i, c)| (i.to_string(), c)));
    let draft = r.draft().unwrap();
    let new = draft.iter().find(|d| d.item == item("skill:new")).unwrap();
    assert_eq!(new.paths, [PathBuf::from("personal/new/SKILL.md"), PathBuf::from("personal/new/deep/x.md")]);

    let diff = r.diff(&item("skill:new")).unwrap();
    assert!(diff.contains("+++ b/personal/new/SKILL.md") && diff.contains("+new"), "{diff}");
    let diff = r.diff(&item("skill:pdf")).unwrap();
    assert!(diff.contains("-PDFs.") && diff.contains("+changed"), "{diff}");
    assert!(r.diff(&item("rule:tone")).unwrap().contains("+Be terse."));
    assert!(matches!(r.diff(&item("skill:nope")), Err(Error::Invalid(_))));
    assert_eq!(ItemRef::resolve("pdf", &draft).unwrap(), item("skill:pdf"));
    assert!(ItemRef::resolve("nope", &draft).is_err());
}

#[test]
fn a_partial_commit_records_only_the_chosen_items_and_leaves_the_rest_in_the_draft() {
    let f = Fleet::new();
    let r = f.b(); // b has no git identity
    assert!(!r.has_git_identity().unwrap());
    write(&f.b.join("personal/pdf/SKILL.md"), "committed\n");
    fs::remove_dir_all(f.b.join("imported/web")).unwrap();
    write(&f.b.join("instructions/git.md"), "draft only\n");
    write(&f.b.join("notes.txt"), "staged\n");
    git(&f.b, &["add", "notes.txt"]);

    assert!(matches!(r.commit(&[item("skill:nope")], "m", None), Err(Error::Invalid(_))));
    let sha = r.commit(&[item("skill:pdf"), item("skill:web")], "commit pdf, web from b", None).unwrap();
    assert_eq!(head(&f.b), sha);
    let files = git(&f.b, &["show", "--name-only", "--format=", "HEAD"]);
    assert_eq!(
        files.lines().collect::<Vec<_>>(),
        ["imported/web/SKILL.md", "personal/pdf/.sk1ll-id", "personal/pdf/SKILL.md"]
    );
    assert_eq!(git(&f.b, &["log", "-1", "--format=%an|%s"]), format!("{}|commit pdf, web from b", hostname()));
    assert_eq!(items(&r), [("rule:git".into(), Change::Modified), ("other:notes.txt".into(), Change::Added)]);
    assert_eq!(git(&f.b, &["diff", "--cached", "--name-only"]), "notes.txt", "the user's staging stays");
    assert_eq!(read(&f.b.join("instructions/git.md")), "draft only\n");

    let sha = r.commit(&[item("rule:git")], "git", Some("laptop <sk1ll@laptop>")).unwrap();
    assert_eq!(git(&f.b, &["log", "-1", "--format=%an <%ae>"]), "laptop <sk1ll@laptop>");
    r.push().unwrap();
    assert_eq!(git(&f.b, &["rev-parse", "origin/main"]), sha);
    assert_eq!(items(&r), [("other:notes.txt".into(), Change::Added)]);
}

#[test]
fn restoring_items_returns_them_and_only_them_to_the_baseline() {
    let f = Fleet::new();
    let r = f.a();
    let before = snapshot(&f.a);
    write(&f.a.join("personal/pdf/SKILL.md"), "changed\n");
    write(&f.a.join("personal/new/deep/SKILL.md"), "new\n");
    write(&f.a.join("instructions/new.md"), "staged new\n");
    git(&f.a, &["add", "instructions/new.md"]);
    fs::remove_dir_all(f.a.join("imported/web")).unwrap();
    write(&f.a.join("notes.txt"), "n\n");

    r.restore(&[item("skill:pdf"), item("skill:new"), item("rule:new"), item("skill:web")]).unwrap();
    assert_eq!(items(&r), [("other:notes.txt".into(), Change::Added)]);
    assert!(!f.a.join("personal/new").exists());
    assert!(f.a.join("personal").is_dir());
    r.restore_all().unwrap();
    assert!(r.draft().unwrap().is_empty());
    assert_eq!(snapshot(&f.a), before);
}

#[test]
fn restore_and_commit_refuse_to_split_a_path_that_changed_between_file_and_directory() {
    let f = Fleet::new();
    let r = f.a();
    write(&f.a.join("notes"), "baseline file\n");
    write(&f.a.join("docs/private"), "baseline\n");
    commit_all(&f.a, "notes and docs");
    let before_head = head(&f.a);
    fs::remove_file(f.a.join("notes")).unwrap();
    write(&f.a.join("notes/private.txt"), "secret\n");
    fs::remove_dir_all(f.a.join("docs")).unwrap();
    write(&f.a.join("docs"), "now a file\n");
    let names: Vec<String> = items(&r).into_iter().map(|(i, _)| i).collect();
    assert_eq!(names, ["other:docs", "other:docs/private", "other:notes", "other:notes/private.txt"]);

    let before = snapshot(&f.a);
    assert!(invalid(r.restore(&[item("other:notes")])).contains("other:notes/private.txt"));
    assert!(invalid(r.restore(&[item("other:notes/private.txt")])).contains("other:notes"));
    assert!(invalid(r.commit(&[item("other:docs")], "m", None)).contains("other:docs/private"));
    assert!(invalid(r.commit(&[item("other:docs/private")], "m", None)).contains("other:docs"));
    assert_eq!(snapshot(&f.a), before);
    assert_eq!(head(&f.a), before_head);

    r.restore(&[item("other:notes"), item("other:notes/private.txt")]).unwrap();
    assert_eq!(read(&f.a.join("notes")), "baseline file\n");
    r.commit(&[item("other:docs"), item("other:docs/private")], "docs", None).unwrap();
    assert_eq!(git(&f.a, &["show", "HEAD:docs"]), "now a file");
    assert!(r.draft().unwrap().is_empty());
}

#[test]
fn restore_never_deletes_ignored_content_and_commit_never_touches_it() {
    let f = Fleet::new();
    let r = f.a();
    write(&f.a.join("notes"), "baseline file\n");
    write(&f.a.join("docs/private"), "baseline\n");
    commit_all(&f.a, "notes and docs");
    write(&f.a.join(".git/info/exclude"), "notes/private.txt\n/docs\n");
    // Ignored content below a selected path (the file became a directory)
    // and above one (the directory became a file).
    fs::remove_file(f.a.join("notes")).unwrap();
    write(&f.a.join("notes/private.txt"), "secret\n");
    fs::remove_dir_all(f.a.join("docs")).unwrap();
    write(&f.a.join("docs"), "ignored file\n");
    let names: Vec<String> = items(&r).into_iter().map(|(i, _)| i).collect();
    assert_eq!(names, ["other:docs/private", "other:notes"]);

    let before = snapshot(&f.a);
    assert!(invalid(r.restore(&[item("other:notes")])).contains("`notes/private.txt`"));
    assert!(invalid(r.restore(&[item("other:docs/private")])).contains("`docs`"));
    assert!(invalid(r.restore_all()).contains("ignored"));
    assert_eq!(snapshot(&f.a), before);

    r.commit(&[item("other:notes")], "drop notes", None).unwrap();
    assert_eq!(read(&f.a.join("notes/private.txt")), "secret\n");
    fs::remove_file(f.a.join("docs")).unwrap();
    r.restore(&[item("other:docs/private")]).unwrap();
    assert_eq!(read(&f.a.join("docs/private")), "baseline\n");
}

#[test]
fn commit_and_push_refuse_a_detached_head_and_change_nothing() {
    let f = Fleet::new();
    let r = f.a();
    git(&f.a, &["checkout", "--quiet", "--detach"]);
    write(&f.a.join("personal/pdf/SKILL.md"), "changed\n");
    let before = snapshot(&f.a);
    assert!(invalid(r.commit(&[item("skill:pdf")], "m", None)).contains("detached"));
    assert!(invalid(r.push()).contains("detached"));
    assert_eq!(snapshot(&f.a), before);
}

#[test]
fn both_sides_of_an_item_id_rename_go_together_or_not_at_all() {
    let scratch = Scratch::new();
    let repo = scratch.committed();
    let (old, new) = (item("skill:pdf"), item("skill:docs"));
    repo.create_item("skill", "pdf", "PDF").unwrap();
    let baseline = repo.commit(std::slice::from_ref(&old), "Add PDF", None).unwrap();
    let id = repo.skills().unwrap()[0].id.clone();
    repo.rename_item("skill", "pdf", "docs").unwrap();
    for side in [&old, &new] {
        let only = std::slice::from_ref(side);
        for err in [repo.commit(only, "Rename side", None).unwrap_err(), repo.restore(only).unwrap_err()] {
            let message = err.to_string();
            for expected in ["both sides", "personal/pdf/.sk1ll-id", "personal/docs/.sk1ll-id"] {
                assert!(message.contains(expected), "{message}");
            }
        }
    }
    assert_eq!(head(repo.root()), baseline);
    assert_eq!(repo.skills().unwrap()[0].name, "docs");
    repo.restore(&[old.clone(), new.clone()]).unwrap();
    assert_eq!(repo.skills().unwrap()[0].name, "pdf");
    repo.rename_item("skill", "pdf", "docs").unwrap();
    repo.commit(&[old, new.clone()], "Rename PDF", None).unwrap();
    assert_eq!(ids_at_head(&repo)[&new], id);
    assert!(repo.draft().unwrap().is_empty());
}

#[test]
fn a_handmade_import_is_committed_with_the_manifest_that_holds_its_new_item_id() {
    let scratch = Scratch::new();
    let repo = scratch.committed();
    write(&repo.root().join("sk1ll.toml"), "import = { pdf = { source = 'upstream' } }\n");
    write(&repo.imported_dir().join("pdf/SKILL.md"), "PDFs.\n");
    let pdf = item("skill:pdf");
    let error = repo.commit(std::slice::from_ref(&pdf), "Import without manifest", None).unwrap_err();
    assert!(error.to_string().contains("Select `manifest` too"), "{error}");
    let id = Manifest::load(repo.root()).unwrap().import["pdf"].id.clone().unwrap();
    repo.commit(&[pdf.clone(), ItemRef::Manifest], "Import with manifest", None).unwrap();
    assert_eq!(ids_at_head(&repo)[&pdf], Some(id));
}

#[test]
fn committing_a_skill_tracks_its_item_id_sidecar_even_when_gitignore_hides_it() {
    let scratch = Scratch::new();
    let repo = scratch.committed();
    write(&repo.root().join(".gitignore"), ".sk1ll-*\n");
    repo.create_item("skill", "pdf", "PDF").unwrap();
    let sidecar = "personal/pdf/.sk1ll-id";
    assert_eq!(git(repo.root(), &["check-ignore", sidecar]), sidecar);
    let id = repo.skills().unwrap()[0].id.as_ref().unwrap().to_string();
    repo.commit(&[item("skill:pdf")], "Add PDF", None).unwrap();
    assert_eq!(git(repo.root(), &["show", &format!("HEAD:{sidecar}")]), id);
    assert!(repo.draft().unwrap().iter().all(|change| matches!(change.item, ItemRef::Other(_))));
}

#[test]
fn restore_keeps_a_manifest_that_no_baseline_has() {
    let scratch = Scratch::new();
    let r = Repo::init(&scratch.join("repo")).unwrap();
    let root = r.root().to_path_buf();
    r.restore_all().unwrap();
    assert!(root.join("sk1ll.toml").is_file());
    assert_eq!(items(&r), [("manifest".into(), Change::Added)]);
    assert!(invalid(r.restore(&[ItemRef::Manifest])).contains("commit it first"));

    // A baseline without the manifest is no better.
    write(&root.join("notes.txt"), "n\n");
    git(&root, &["add", "notes.txt"]);
    git(&root, &["commit", "--quiet", "-m", "notes"]);
    write(&root.join("instructions/x.md"), "x\n");
    r.restore_all().unwrap();
    assert!(root.join("sk1ll.toml").is_file());
    assert_eq!(items(&r), [("manifest".into(), Change::Added)]);
    write(&root.join("instructions/x.md"), "x\n");
    assert!(invalid(r.restore(&[ItemRef::Manifest, item("rule:x")])).contains("commit it first"));
    assert!(root.join("instructions/x.md").is_file());
}

/// Metadata HEAD can't read doesn't block its own repair: a commit that
/// replaces it goes through, its tree checked strictly; one that leaves it
/// is refused, and so is a commit whose own metadata is unreadable.
#[test]
fn a_commit_may_repair_metadata_head_cannot_read_but_never_commit_unreadable_metadata() {
    let scratch = Scratch::new();
    let repo = scratch.committed();
    let root = repo.root().to_path_buf();
    repo.create_item("rule", "style", "Style").unwrap();
    repo.commit(&[item("rule:style")], "style", None).unwrap();
    let manifest = read(&root.join("sk1ll.toml"));
    let rule = read(&root.join("instructions/style.md"));
    write(&root.join("sk1ll.toml"), "[import.web\n");
    write(&root.join("instructions/style.md"), &rule.replacen("id:", "id: [", 1));
    commit_all(&root, "broken by hand");

    write(&root.join("sk1ll.toml"), &manifest);
    write(&root.join("instructions/style.md"), &rule);
    let error = repo.commit(&[item("manifest")], "fix the manifest", None).unwrap_err().to_string();
    assert!(error.contains("instructions/style.md"), "{error}");
    repo.create_item("rule", "git", "Git").unwrap();
    let error = repo.commit(&[item("rule:git")], "git", None).unwrap_err().to_string();
    assert!(error.contains("sk1ll.toml"), "{error}");
    repo.commit(&[item("manifest"), item("rule:style"), item("rule:git")], "fix both", None).unwrap();
    assert!(repo.draft().unwrap().is_empty());
    assert!(sk1ll_core::content::item_ids_at(&root, "HEAD").is_ok());

    write(&root.join("sk1ll.toml"), "[import.web\n");
    assert!(repo.commit(&[item("manifest")], "broken again", None).is_err());
}

#[cfg(unix)]
mod byte_paths {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    use super::*;

    #[test]
    fn paths_that_are_not_utf_8_or_hold_backslashes_are_listed_and_restored_byte_exactly() {
        let f = Fleet::new();
        let r = f.a();
        write(&f.a.join("notes/b"), "tracked\n");
        commit_all(&f.a, "notes/b");
        write(&f.a.join("notes\\b"), "untracked\n");
        let bad = OsStr::from_bytes(b"bad\xff.txt");
        fs::write(f.a.join(bad), "x\n").unwrap();

        assert_eq!(
            items(&r),
            [("other:\"bad\\377.txt\"".into(), Change::Added), ("other:notes\\b".into(), Change::Added)]
        );
        let draft = r.draft().unwrap();
        assert_eq!(draft[0].paths, [PathBuf::from(bad)]);
        assert_eq!(ItemRef::resolve("\"bad\\377.txt\"", &draft).unwrap(), draft[0].item);

        r.restore(&[item("other:notes\\b")]).unwrap();
        assert_eq!(git(&f.a, &["ls-files", "notes"]), "notes/b");
        assert_eq!(read(&f.a.join("notes/b")), "tracked\n");
        assert!(!f.a.join("notes\\b").exists());
        r.restore(&[draft[0].item.clone()]).unwrap();
        assert!(!f.a.join(bad).exists());
        assert!(r.draft().unwrap().is_empty());
    }

    #[test]
    fn a_quoted_name_and_the_literal_name_it_spells_are_two_items() {
        let f = Fleet::new();
        let r = f.a();
        let bad = OsStr::from_bytes(b"bad\xff.txt");
        let literal = r#""bad\377.txt""#;
        fs::write(f.a.join(bad), "x\n").unwrap();
        fs::write(f.a.join(literal), "y\n").unwrap();
        let draft = r.draft().unwrap();
        assert_eq!(draft.len(), 2);
        let of = |p: &Path| draft.iter().find(|d| d.paths == [p.to_path_buf()]).unwrap().item.clone();
        let (bad_item, literal_item) = (of(Path::new(bad)), of(Path::new(literal)));
        assert_ne!(bad_item, literal_item);

        r.restore(&[bad_item]).unwrap();
        assert!(!f.a.join(bad).exists());
        assert_eq!(read(&f.a.join(literal)), "y\n");
        r.restore(&[ItemRef::resolve(&literal_item.to_string(), &r.draft().unwrap()).unwrap()]).unwrap();
        assert!(r.draft().unwrap().is_empty());
    }
}

/// No git command on the skills repo runs a filter driver: not the
/// Draft's `status`, a diff, a commit's `add` or a restore's checkout,
/// even for a driver a conditional include (`includeIf.gitdir`) names.
#[test]
fn draft_commands_run_no_filter_driver() {
    let tmp = Scratch::new();
    let repo = tmp.committed();
    let root = repo.root().to_path_buf();
    let marker = tmp.join("ran");
    let cmd = format!("touch '{}'; cat", marker.display());
    let filter = tmp.join("filter.gitconfig");
    write(&filter, &format!("[filter \"x\"]\n\tclean = {cmd}\n\tsmudge = {cmd}\n\trequired = true\n"));
    git(&root, &["config", &format!("includeIf.gitdir:{}/.path", root.display()), filter.to_str().unwrap()]);
    write(&root.join(".git/info/attributes"), "*.md filter=x\n");
    repo.create_item("skill", "pdf", "PDF").unwrap();
    let pdf = item("skill:pdf");
    repo.commit(std::slice::from_ref(&pdf), "Add PDF", None).unwrap();
    // Later than the index: status must look at the content.
    std::thread::sleep(std::time::Duration::from_millis(20));
    write(&root.join("personal/pdf/SKILL.md"), "Edited.\n");
    assert_eq!(items(&repo), [("skill:pdf".to_string(), Change::Modified)]);
    assert!(repo.diff(&pdf).unwrap().contains("Edited."));
    repo.restore(std::slice::from_ref(&pdf)).unwrap();
    assert!(repo.draft().unwrap().is_empty());
    assert!(!marker.exists(), "a filter driver ran");
    // The include does name the driver where git runs.
    write(&root.join("personal/pdf/SKILL.md"), "Edited again.\n");
    let _ = sk1ll_testkit::git::output(&root, &["add", "--all"]);
    assert!(marker.exists(), "the test's driver is not configured as git reads it");
}
