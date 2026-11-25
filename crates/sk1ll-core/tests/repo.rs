//! Repo layout, discovery and item CRUD through `Repo` on real git repos.

mod common;

use common::{Scratch, commit_all};
use sk1ll_core::instructions::INSTRUCTIONS_DIR;
use sk1ll_core::manifest::MANIFEST_FILE;
use sk1ll_core::repo::RenameError;
use sk1ll_core::skill::{IMPORTED_DIR, PERSONAL_DIR};
use sk1ll_core::{Error, ItemRef, Manifest, Repo, skill};
use sk1ll_testkit::git::git;
use std::{fs, path::Path};

/// Commit and the daemon only work on the fleet's branch, so a fresh
/// repo starts on it even where git's own default is still `master`
/// (unit tests run git without the developer's config).
#[test]
fn init_starts_on_the_fleet_branch_whatever_git_defaults_to() {
    let tmp = Scratch::new();
    let root = tmp.join("repo");
    let _git = sk1ll_core::failpoints::git_env(&[
        ("GIT_CONFIG_COUNT", "1"),
        ("GIT_CONFIG_KEY_0", "init.defaultBranch"),
        ("GIT_CONFIG_VALUE_0", "unexpected"),
    ]);
    Repo::init(&root).unwrap();
    let head = sk1ll_core::git::Git::new(&root).args(["symbolic-ref", "--short", "HEAD"]).text().unwrap();
    assert_eq!(head, sk1ll_core::FLEET_BRANCH);
    commit_all(&root, "init");
    let branch = sk1ll_core::git::Git::new(&root).args(["branch", "--show-current"]).text().unwrap();
    assert_eq!(branch, sk1ll_core::FLEET_BRANCH);
}

#[test]
fn a_committed_init_starts_with_an_empty_draft_but_leaves_an_existing_repo_alone() {
    let tmp = Scratch::new();
    let root = tmp.join("repo");
    let repo = Repo::init_committed(&root).unwrap();
    assert!(repo.draft().unwrap().is_empty());
    let log = sk1ll_core::git::Git::new(&root).args(["log", "--format=%s"]).text().unwrap();
    assert_eq!(log, "Initialize skills repo");

    // An existing git repo keeps the starter files as Draft.
    let existing = tmp.join("existing");
    fs::create_dir_all(&existing).unwrap();
    git(&existing, &["init", "--quiet", "-b", sk1ll_core::FLEET_BRANCH]);
    let repo = Repo::init_committed(&existing).unwrap();
    assert!(!repo.draft().unwrap().is_empty());
    assert!(sk1ll_core::git::Git::new(&existing).args(["rev-parse", "--verify", "--quiet", "HEAD"]).text().is_err());
}

#[test]
fn init_preserves_existing_destinations_and_refuses_symlink_ancestors() {
    for scenario in ["keep", "ancestor", "manifest-link", "ignore-link"] {
        let tmp = Scratch::new();
        let root = tmp.join("repo");
        fs::create_dir(&root).unwrap();
        let outside = tmp.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("notes"), "private").unwrap();
        match scenario {
            "keep" => {
                fs::create_dir(root.join("personal")).unwrap();
                fs::write(root.join("personal/.gitkeep"), "private").unwrap();
            }
            "ancestor" => std::os::unix::fs::symlink(&outside, root.join("personal")).unwrap(),
            "manifest-link" => std::os::unix::fs::symlink(outside.join("missing"), root.join(MANIFEST_FILE)).unwrap(),
            _ => std::os::unix::fs::symlink(outside.join("missing"), root.join(".gitignore")).unwrap(),
        }
        assert!(Repo::init(&root).is_err(), "{scenario}");
        assert_eq!(fs::read(outside.join("notes")).unwrap(), b"private");
        assert!(!outside.join("missing").exists());
        assert!(!outside.join(".gitkeep").exists());
        if scenario == "keep" {
            assert_eq!(fs::read(root.join("personal/.gitkeep")).unwrap(), b"private");
        }
    }
}

#[test]
fn recovery_discovery_finds_a_deleted_manifest() {
    let tmp = Scratch::new();
    let root = tmp.join("repo");
    Repo::init(&root).unwrap();
    commit_all(&root, "init");
    fs::remove_file(root.join(MANIFEST_FILE)).unwrap();
    fs::create_dir_all(root.join("personal/pdf")).unwrap();
    assert_eq!(Repo::locate_git(&root.join("personal/pdf")), Some(root.clone()));
    assert!(Repo::open_git_only(&root).is_ok());
    assert!(Repo::open(&root).is_err());
    assert!(Repo::open_git_only(tmp.path()).is_err());
}

#[test]
fn recovery_discovery_never_picks_an_unrelated_repo() {
    let tmp = Scratch::new();
    // An unrelated project, even one with skills-like folders.
    let project = tmp.join("project");
    fs::create_dir_all(project.join("src/deep")).unwrap();
    fs::write(project.join("src/main.rs"), "fn main() {}\n").unwrap();
    git(&project, &["init", "--quiet"]);
    commit_all(&project, "code");
    fs::write(project.join("src/main.rs"), "unsaved edits\n").unwrap();
    for cwd in [project.clone(), project.join("src/deep")] {
        assert_eq!(Repo::locate_git(&cwd), Repo::default_location(), "{}", cwd.display());
    }
    for sub in [PERSONAL_DIR, IMPORTED_DIR, INSTRUCTIONS_DIR] {
        fs::create_dir_all(project.join(sub)).unwrap();
    }
    assert_eq!(Repo::locate_git(&project), Repo::default_location());
    // Not in a git repo at all.
    let plain = tmp.join("plain");
    fs::create_dir_all(&plain).unwrap();
    assert_eq!(Repo::locate_git(&plain), Repo::default_location());
    // A skills repo that was never committed can't be told apart either.
    let fresh = tmp.join("fresh");
    Repo::init(&fresh).unwrap();
    fs::remove_file(fresh.join(MANIFEST_FILE)).unwrap();
    assert_eq!(Repo::locate_git(&fresh), Repo::default_location());
}

#[test]
fn recovery_discovery_recognises_a_skills_repo_by_layout_and_history() {
    let tmp = Scratch::new();
    // The skills repo lives in a subdirectory of a larger repo, and its
    // manifest was deleted in a later commit, not just locally.
    let top = tmp.join("dotfiles");
    let root = top.join("skills");
    fs::create_dir_all(&top).unwrap();
    git(&top, &["init", "--quiet"]);
    fs::write(top.join("vimrc"), "set nu\n").unwrap();
    Repo::init(&root).unwrap();
    fs::remove_dir_all(root.join(".git")).unwrap();
    commit_all(&top, "skills");
    assert_eq!(Repo::locate_git(&top), Repo::default_location());
    fs::remove_file(root.join(MANIFEST_FILE)).unwrap();
    assert_eq!(Repo::locate_git(&root.join(PERSONAL_DIR)), Some(root.clone()));
    commit_all(&top, "drop manifest");
    assert_eq!(Repo::locate_git(&root.join(PERSONAL_DIR)), Some(root.clone()));
    assert_eq!(Repo::locate_git(&top), Repo::default_location());
}

/// Whether `dir`'s filesystem folds case (macOS's default APFS does).
fn folds_case(dir: &Path) -> bool {
    let probe = dir.join("case-probe");
    fs::write(&probe, "").unwrap();
    let folds = dir.join("CASE-PROBE").exists();
    fs::remove_file(probe).unwrap();
    folds
}

#[test]
fn creates_draft_content_and_refuses_invalid_or_taken_names() {
    let dir = Scratch::new();
    let repo = dir.init();
    repo.create_item("skill", "new-skill", "quotes: \" and\nnewlines").unwrap();
    assert!(repo.root().join("personal/new-skill/SKILL.md").exists());
    assert!(repo.create_item("skill", "new-skill", "").is_err());
    for name in ["", "..", "../escape", ".hidden", "a/b", "with space"] {
        assert!(repo.create_item("rule", name, "").is_err());
    }
    repo.create_item("rule", "style", "House style").unwrap();
    assert!(repo.root().join("instructions/style.md").exists());
    let ids = repo.item_ids().unwrap();
    assert_eq!(ids.len(), 2);
    assert!(ids.iter().all(|(_, id)| ulid::Ulid::from_string(&id.as_ref().unwrap().to_string()).is_ok()));
    assert_eq!(repo.skills().unwrap()[0].id.as_ref(), ids[0].1.as_ref());
    assert!(repo.create_item("rule", "style", "").is_err());
    assert!(repo.create_item("other", "x", "").is_err());
}

#[cfg(unix)]
#[test]
fn rename_never_writes_through_a_planted_stage_symlink() {
    let dir = Scratch::new();
    let target = dir.join("secret");
    fs::write(&target, "secret").unwrap();
    let repo = dir.init();
    repo.create_item("skill", "pdf", "PDF").unwrap();
    std::os::unix::fs::symlink(&target, repo.root().join("personal/pdf/.sk1ll-stage-rename-documents")).unwrap();
    repo.rename_item("skill", "pdf", "documents").unwrap();
    assert_eq!(fs::read_to_string(&target).unwrap(), "secret");
    let skill = repo.root().join("personal/documents/SKILL.md");
    assert!(skill.symlink_metadata().unwrap().is_file());
    assert!(fs::read_to_string(skill).unwrap().contains("name: documents"));
}

#[cfg(unix)]
#[test]
fn a_failed_rename_says_whether_the_item_moved() {
    use std::os::unix::fs::PermissionsExt;
    let dir = Scratch::new();
    let repo = dir.init();
    repo.create_item("skill", "pdf", "PDF").unwrap();
    repo.create_item("skill", "other", "").unwrap();
    assert!(matches!(repo.rename_item("skill", "pdf", "other"), Err(RenameError::NotMoved(_))));
    // The directory moves, but its frontmatter can't be rewritten.
    let skill = repo.root().join("personal/pdf");
    fs::set_permissions(&skill, fs::Permissions::from_mode(0o555)).unwrap();
    let moved = repo.rename_item("skill", "pdf", "documents");
    let documents = repo.root().join("personal/documents");
    fs::set_permissions(&documents, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(moved, Err(RenameError::Moved(_))), "{moved:?}");
    assert!(documents.is_dir() && !skill.exists());
}

#[test]
fn deletes_and_renames_only_personal_items() {
    let dir = Scratch::new();
    let repo = dir.init();
    repo.create_item("skill", "pdf", "PDF").unwrap();
    fs::write(repo.root().join("personal/pdf/notes.md"), "kept").unwrap();
    repo.create_item("rule", "style", "Style").unwrap();
    fs::create_dir_all(repo.root().join("imported/vendored")).unwrap();
    fs::write(repo.root().join("imported/vendored/SKILL.md"), "---\nname: vendored\n---\n").unwrap();

    // Imports, missing items and escapes are refused.
    for (kind, name) in
        [("skill", "vendored"), ("skill", "missing"), ("rule", "missing"), ("skill", "../imported"), ("other", "pdf")]
    {
        assert!(repo.delete_item(kind, name).is_err(), "{kind}:{name}");
        assert!(repo.rename_item(kind, name, "fresh").is_err(), "{kind}:{name}");
    }
    for to in ["vendored", "pdf", "../x", ""] {
        assert!(repo.rename_item("skill", "pdf", to).is_err(), "{to}");
    }
    repo.create_item("skill", "other", "").unwrap();
    assert!(repo.rename_item("skill", "pdf", "other").is_err());

    let before = repo.skills().unwrap().into_iter().find(|s| s.name == "pdf").unwrap();
    repo.rename_item("skill", "pdf", "documents").unwrap();
    assert!(!repo.root().join("personal/pdf").exists());
    let text = fs::read_to_string(repo.root().join("personal/documents/SKILL.md")).unwrap();
    assert_eq!(text, "---\nname: documents\ndescription: \"PDF\"\n---\n\n");
    assert_eq!(fs::read_to_string(repo.root().join("personal/documents/notes.md")).unwrap(), "kept");
    assert_eq!(repo.skills().unwrap().into_iter().find(|s| s.name == "documents").unwrap().id, before.id);
    assert!(
        fs::read_dir(repo.personal_dir().join("documents"))
            .unwrap()
            .all(|e| !sk1ll_core::source::is_staging_path(Path::new(&e.unwrap().file_name())))
    );

    // A `name:` that doesn't match the old name is left alone.
    let rule_id = repo.rules().unwrap().remove(0).id.unwrap();
    let rule = format!("---\nname: House\nid: \"{rule_id}\"\n---\nbody\n");
    fs::write(repo.root().join("instructions/style.md"), &rule).unwrap();
    repo.rename_item("rule", "style", "house-style").unwrap();
    assert_eq!(fs::read_to_string(repo.root().join("instructions/house-style.md")).unwrap(), rule);
    assert_eq!(repo.rules().unwrap()[0].id, Some(rule_id));

    repo.delete_item("skill", "documents").unwrap();
    repo.delete_item("rule", "house-style").unwrap();
    assert!(!repo.root().join("personal/documents").exists());
    assert!(!repo.root().join("instructions/house-style.md").exists());
    assert!(repo.root().join("imported/vendored/SKILL.md").exists());
}

#[test]
fn reader_leaves_item_ids_unassigned_until_they_are_persisted() {
    let tmp = Scratch::new();
    let repo = tmp.committed();
    let skill = repo.personal_dir().join("style");
    fs::create_dir(&skill).unwrap();
    fs::write(skill.join("SKILL.md"), "Skill.\n").unwrap();
    fs::write(repo.root().join("instructions/style.md"), "Rule.\n").unwrap();
    fs::write(repo.root().join(MANIFEST_FILE), "import = { vendored = { source = 'local' } }\n").unwrap();
    assert!(repo.item_ids().unwrap().iter().all(|(_, id)| id.is_none()));
    assert!(!skill.join(skill::ID_FILE).exists());
    assert_eq!(repo.assign_missing_ids().unwrap(), 3);
    let first = repo.item_ids().unwrap();
    assert_eq!(repo.assign_missing_ids().unwrap(), 0);
    assert_eq!(repo.item_ids().unwrap(), first);
    assert!(first.iter().all(|(_, id)| ulid::Ulid::from_string(&id.as_ref().unwrap().to_string()).is_ok()));
    let imported = Manifest::load(repo.root()).unwrap().import["vendored"].id.clone().unwrap();
    assert!(ulid::Ulid::from_string(&imported.to_string()).is_ok());
    repo.rename_item("skill", "style", "new-style").unwrap();
    assert_eq!(repo.skills().unwrap()[0].id, first[0].1);
    let diff = repo.diff(&ItemRef::Skill("new-style".into())).unwrap();
    assert!(diff.contains("+Skill.") && !diff.contains(sk1ll_core::skill::ID_FILE));
}

#[test]
fn copied_skill_ids_report_both_paths_and_block_creation() {
    let tmp = Scratch::new();
    let repo = tmp.init();
    repo.create_item("skill", "pdf", "PDF").unwrap();
    let copy = repo.personal_dir().join("copy");
    fs::create_dir(&copy).unwrap();
    for file in ["SKILL.md", skill::ID_FILE] {
        fs::copy(repo.personal_dir().join("pdf").join(file), copy.join(file)).unwrap();
    }
    let err = repo.item_ids().unwrap_err();
    assert!(matches!(err, Error::DuplicateItemId { .. }));
    for path in ["personal/pdf/.sk1ll-id", "personal/copy/.sk1ll-id"] {
        assert!(err.to_string().contains(path), "{err}");
    }
    assert!(repo.create_item("rule", "style", "").is_err());
    assert!(!repo.root().join("instructions/style.md").exists());
}

#[cfg(unix)]
#[test]
fn an_unassigned_skill_stays_at_its_old_name_if_the_id_cannot_be_written() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = Scratch::new();
    let repo = tmp.init();
    let source = repo.personal_dir().join("pdf");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("SKILL.md"), "PDF\n").unwrap();
    let before = repo.skills().unwrap()[0].id.clone();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o555)).unwrap();
    let result = repo.rename_item("skill", "pdf", "docs");
    let actual = if result.is_ok() { repo.personal_dir().join("docs") } else { source.clone() };
    fs::set_permissions(&actual, fs::Permissions::from_mode(0o755)).unwrap();
    // Users with DAC override can write despite the mode bits.
    if let Err(error) = result {
        assert!(matches!(error, RenameError::NotMoved(_)));
        assert!(source.is_dir());
        assert!(!repo.personal_dir().join("docs").exists());
    }
    if actual == source {
        assert_eq!(repo.skills().unwrap()[0].id, before);
    }
}

#[test]
fn an_interrupted_case_rename_reports_the_stage_path() {
    let tmp = Scratch::new();
    let repo = tmp.committed();
    repo.create_item("skill", "pdf", "").unwrap();
    let original = repo.personal_dir().join("pdf");
    let stage = original.with_file_name(".sk1ll-stage-rename-pdf");
    fs::rename(&original, &stage).unwrap();
    for error in [repo.draft().unwrap_err(), repo.skills().unwrap_err()] {
        assert!(matches!(error, Error::StrandedRename { path } if path == stage));
    }
}

#[test]
fn a_case_only_rename_keeps_the_id_and_uses_the_requested_spelling() {
    let tmp = Scratch::new();
    let repo = tmp.committed();
    repo.create_item("rule", "pdf", "PDF").unwrap();
    let id = repo.rules().unwrap()[0].id.clone();
    let old = ItemRef::Rule("pdf".into());
    let new = ItemRef::Rule("PDF".into());
    repo.commit(std::slice::from_ref(&old), "Add Rule", None).unwrap();
    repo.check_rename("rule", "pdf", "PDF").unwrap();
    repo.rename_item("rule", "pdf", "PDF").unwrap();
    let rules = repo.rules().unwrap();
    assert_eq!(rules[0].id, id);
    assert_eq!(rules[0].name, "PDF");
    assert_eq!(rules.len(), 1);
    repo.restore(&[old.clone(), new.clone()]).unwrap();
    assert_eq!(repo.rules().unwrap()[0].name, "pdf");
    repo.rename_item("rule", "pdf", "PDF").unwrap();
    repo.commit(&[old, new.clone()], "Rename Rule", None).unwrap();
    assert_eq!(sk1ll_core::content::item_ids_at(repo.root(), "HEAD").unwrap(), [(new, id)].into());
    assert!(repo.draft().unwrap().is_empty());
}

/// Restoring both sides of `pdf` -> `PDF` puts the Baseline's spelling
/// back even when the directory holds an ignored file, which keeps
/// restore from removing it. Only a case-insensitive filesystem (macOS)
/// gets there; elsewhere the two spellings are two directories.
#[test]
fn restoring_a_case_only_rename_restores_the_spelling() {
    let tmp = Scratch::new();
    if !folds_case(tmp.path()) {
        return;
    }
    let repo = tmp.committed();
    repo.create_item("skill", "pdf", "PDF").unwrap();
    let old = ItemRef::Skill("pdf".into());
    repo.commit(std::slice::from_ref(&old), "Add Skill", None).unwrap();
    fs::write(repo.root().join(".gitignore"), "*.tmp\n").unwrap();
    repo.rename_item("skill", "pdf", "PDF").unwrap();
    fs::write(repo.personal_dir().join("PDF/notes.tmp"), "ignored\n").unwrap();
    repo.restore(&[old, ItemRef::Skill("PDF".into())]).unwrap();
    let names: Vec<_> = fs::read_dir(repo.personal_dir()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(names, ["pdf"]);
    assert!(repo.personal_dir().join("pdf/notes.tmp").is_file());
}

/// Only the documented link has a bundled copy, which lands as a personal
/// skill and never replaces one.
#[test]
fn the_bundled_skill_is_added_for_its_documented_link_only_and_never_replaces() {
    use sk1ll_core::bundled::{SK1LL_SKILL_LINK, for_link, skill};
    assert_eq!(for_link(SK1LL_SKILL_LINK), Some("sk1ll"));
    assert_eq!(for_link(&format!(" {SK1LL_SKILL_LINK}/ ")), Some("sk1ll"));
    assert_eq!(for_link("https://github.com/owner/repo/tree/main/skills/sk1ll"), None);
    let tmp = Scratch::new();
    let repo = tmp.init();
    repo.add_bundled("sk1ll", "sk1ll").unwrap();
    let written = fs::read_to_string(repo.root().join("personal/sk1ll/SKILL.md")).unwrap();
    assert!(written.starts_with("---\nname: sk1ll\n"));
    assert_eq!(written, skill("sk1ll").unwrap());
    assert!(repo.add_bundled("sk1ll", "sk1ll").unwrap_err().to_string().contains("taken"));
    assert!(repo.add_bundled("pdf", "pdf").is_err());
}
