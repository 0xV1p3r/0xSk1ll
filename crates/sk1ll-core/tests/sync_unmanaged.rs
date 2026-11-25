mod sync_common;

use std::{collections::BTreeMap, fs};

use sk1ll_core::failpoints::{self, Point};
use sk1ll_core::hash::hash_dir;
use sk1ll_core::sync::{unmanaged::*, *};
use sk1ll_core::{Assignment, Harness, LinkMode, Repo};
use sk1ll_testkit::{Home, SkillsRepo, write};

use sync_common::*;

#[test]
fn a_user_marker_is_refused_before_grouping_and_after_selection() {
    let (tmp, repo, harnesses) = setup();
    let _home = tmp.enter();
    let blocked = tmp.path().join("a/pdf");
    let clean = tmp.path().join("b/pdf");
    write(&blocked.join("SKILL.md"), "Same skill.\n");
    write(&clean.join("SKILL.md"), "Same skill.\n");
    write(&blocked.join(PLACEMENT_MARKER), "User data.\n");

    let error = prepare(&repo, &harnesses, "a", "pdf").unwrap_err();
    assert!(error.to_string().contains(PLACEMENT_MARKER));
    assert!(!repo.personal_dir().join("pdf").exists());
    let selected = select(&repo, &harnesses, "b", "pdf").unwrap();
    assert_eq!(selected.locations.len(), 1);
    assert_eq!(selected.locations[0].path, clean);

    write(&clean.join(PLACEMENT_MARKER), "Late user data.\n");
    let error = publish(&repo, &selected).unwrap_err();
    assert!(error.to_string().contains(PLACEMENT_MARKER));
    assert_eq!(fs::read_to_string(blocked.join(PLACEMENT_MARKER)).unwrap(), "User data.\n");
    assert_eq!(fs::read_to_string(clean.join(PLACEMENT_MARKER)).unwrap(), "Late user data.\n");
    assert!(!repo.personal_dir().join("pdf").exists());
    assert!(displaced::list().unwrap().is_empty());
}

fn setup() -> (Home, Repo, BTreeMap<String, Harness>) {
    let tmp = Home::new();
    let _home = tmp.enter();
    let repo = SkillsRepo::init(tmp.path().join("repo")).open();
    let harnesses = ["a", "b", "c", "d"]
        .into_iter()
        .map(|name| {
            (
                name.into(),
                Harness {
                    skills: Some(tmp.path().join(name).to_string_lossy().into_owned()),
                    mode: LinkMode::Copy,
                    ..Default::default()
                },
            )
        })
        .collect();
    (tmp, repo, harnesses)
}

#[test]
fn groups_identical_copies_and_excludes_managed_skills_and_rules() {
    let (tmp, repo, hs) = setup();
    let _home = tmp.enter();
    for name in ["a", "b"] {
        write(&tmp.path().join(name).join("pdf/SKILL.md"), "---\ndescription: PDFs\n---\none");
    }
    write(&tmp.path().join("c/pdf/SKILL.md"), "two");
    write(&tmp.path().join("a/rule.md"), "not a skill");
    write(&repo.personal_dir().join("marked/SKILL.md"), "managed");
    let only_a = BTreeMap::from([("a".into(), hs["a"].clone())]);
    let marked = plan(&repo, &only_a, &Assignment::default(), PlanOptions::default()).unwrap().remove(0);
    apply(&repo, &marked).unwrap();
    let items = discover(&repo, &hs).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].variants.len(), 2);
    let identical = items[0].variants.iter().find(|v| v.locations.len() == 2).unwrap();
    assert_eq!(identical.description.as_deref(), Some("PDFs"));
    write(&repo.personal_dir().join("pdf/SKILL.md"), "repo");
    assert!(discover(&repo, &hs).unwrap()[0].clashes_with_repo);
    assert!(prepare(&repo, &hs, "a", "pdf").unwrap_err().to_string().contains("already exists"));
}

#[test]
fn adopts_only_chosen_variant_preserves_originals_in_recovery_and_respects_defaults() {
    let (tmp, repo, hs) = setup();
    let _home = tmp.enter();
    for h in ["a", "b"] {
        write(&tmp.path().join(h).join("pdf/SKILL.md"), "same");
    }
    write(&tmp.path().join("a/pdf/.sk1ll-id"), "from-elsewhere\n");
    write(&tmp.path().join("c/pdf/SKILL.md"), "different");
    let adoption = prepare(&repo, &hs, "a", "pdf").unwrap();
    assert_eq!(adoption.locations.len(), 2);
    let id = skill_id(&repo, "pdf");
    assert!(ulid::Ulid::from_string(&id.to_string()).is_ok());
    prepare(&repo, &hs, "a", "pdf").unwrap();
    assert_eq!(skill_id(&repo, "pdf"), id);
    // The identical draft copy is a retryable Adopt, not a clash.
    assert!(!discover(&repo, &hs).unwrap()[0].clashes_with_repo);
    let mut assignment = Assignment::default();
    for h in hs.keys() {
        assignment.set_new_skills_default(h, false);
    }
    for l in &adoption.locations {
        assignment.set_skill(&l.harness, &id, true);
    }
    complete(&repo, &hs, &adoption).unwrap();
    for action in plan(&repo, &hs, &assignment, PlanOptions::default()).unwrap() {
        apply(&repo, &action).unwrap();
    }
    for h in ["a", "b"] {
        let dest = tmp.path().join(h).join("pdf");
        assert_copy_recorded(&dest);
        assert_eq!(hash_dir(&dest).unwrap(), adoption.hash);
    }
    assert_eq!(fs::read_to_string(tmp.path().join("c/pdf/SKILL.md")).unwrap(), "different");
    assert!(!tmp.path().join("d/pdf").exists());
    let recovery = displaced::list().unwrap();
    assert_eq!(recovery.len(), 2);
    for entry in recovery {
        assert_eq!(fs::read_to_string(entry.path.join("SKILL.md")).unwrap(), "same");
    }
    let remaining = discover(&repo, &hs).unwrap();
    assert_eq!(remaining[0].variants[0].locations[0].harness, "c");
    assignment.set_skill("c", &id, true);
    assert!(
        plan(&repo, &hs, &assignment, PlanOptions::default())
            .unwrap()
            .iter()
            .any(|a| a.harness == "c" && a.kind == ActionKind::Conflict)
    );
    assert!(prepare(&repo, &hs, "a", "../outside").is_err());
}

#[test]
fn only_the_variant_matching_the_personal_copy_is_resumable() {
    let (tmp, repo, hs) = setup();
    let _home = tmp.enter();
    write(&tmp.path().join("a/pdf/SKILL.md"), "same");
    write(&tmp.path().join("b/pdf/SKILL.md"), "different");
    assert!(discover(&repo, &hs).unwrap()[0].variants.iter().all(|v| !v.resumable));
    // An Adopt interrupted after publishing leaves an identical personal copy.
    prepare(&repo, &hs, "a", "pdf").unwrap();
    let skill = discover(&repo, &hs).unwrap().remove(0);
    assert!(!skill.clashes_with_repo);
    let resumable: Vec<_> =
        skill.variants.iter().filter(|v| v.resumable).flat_map(|v| &v.locations).map(|l| &l.harness).collect();
    assert_eq!(resumable, ["a"]);
    assert!(select(&repo, &hs, "a", "pdf").is_ok());
    assert!(select(&repo, &hs, "b", "pdf").unwrap_err().to_string().contains("already exists"));
    // An edited personal copy is a genuine clash.
    write(&repo.personal_dir().join("pdf/SKILL.md"), "edited");
    let skill = discover(&repo, &hs).unwrap().remove(0);
    assert!(skill.clashes_with_repo && skill.variants.iter().all(|v| !v.resumable));
}

#[test]
fn only_a_skill_added_in_the_draft_counts_as_adopted() {
    let (tmp, repo, hs) = setup();
    let _home = tmp.enter();
    write(&tmp.path().join("a/pdf/SKILL.md"), "same");
    let adoption = prepare(&repo, &hs, "a", "pdf").unwrap();
    complete(&repo, &hs, &adoption).unwrap();
    assert!(adopted(&repo, &hs, "a", "pdf").unwrap().is_some());
    repo.commit(&[sk1ll_core::ItemRef::Skill("pdf".into())], "commit pdf", Some("t <t@example.com>")).unwrap();
    assert!(adopted(&repo, &hs, "a", "pdf").unwrap().is_none());
}

#[test]
fn edits_after_prepare_remain_unmanaged() {
    let (tmp, repo, hs) = setup();
    let _home = tmp.enter();
    write(&tmp.path().join("a/pdf/SKILL.md"), "one");
    let adoption = prepare(&repo, &hs, "a", "pdf").unwrap();
    write(&tmp.path().join("a/pdf/SKILL.md"), "edited meanwhile");
    complete(&repo, &hs, &adoption).unwrap();
    assert_eq!(index_entry(&tmp.path().join("a/pdf")), None);
    assert_eq!(fs::read_to_string(tmp.path().join("a/pdf/SKILL.md")).unwrap(), "edited meanwhile");
}

#[test]
fn adopting_never_overwrites_repo_content_created_while_copying() {
    let (tmp, repo, hs) = setup();
    let _home = tmp.enter();
    write(&tmp.path().join("a/pdf/SKILL.md"), "unmanaged");
    let dest = repo.personal_dir().join("pdf");
    let _hook = failpoints::observe(move |point, at| {
        if point == Point::BeforeSwap && at == dest {
            write(&at.join("SKILL.md"), "created meanwhile");
        }
    });
    assert!(prepare(&repo, &hs, "a", "pdf").is_err());
    assert_eq!(fs::read_to_string(repo.personal_dir().join("pdf/SKILL.md")).unwrap(), "created meanwhile");
    assert_eq!(fs::read_to_string(tmp.path().join("a/pdf/SKILL.md")).unwrap(), "unmanaged");
}

#[cfg(unix)]
#[test]
fn unmanaged_directory_links_are_hashed_by_content_and_adopted_without_touching_the_target() {
    let (tmp, repo, hs) = setup();
    let _home = tmp.enter();
    write(&tmp.path().join("external/SKILL.md"), "linked");
    fs::create_dir_all(tmp.path().join("a")).unwrap();
    std::os::unix::fs::symlink(tmp.path().join("external"), tmp.path().join("a/pdf")).unwrap();
    write(&tmp.path().join("b/pdf/SKILL.md"), "linked");
    let items = discover(&repo, &hs).unwrap();
    assert_eq!(items[0].variants.len(), 1);
    let adoption = prepare(&repo, &hs, "a", "pdf").unwrap();
    complete(&repo, &hs, &adoption).unwrap();
    assert_eq!(fs::read_to_string(tmp.path().join("external/SKILL.md")).unwrap(), "linked");
    let dest = tmp.path().join("a/pdf");
    assert_copy_recorded(&dest);
    assert_eq!(hash_dir(&dest).unwrap(), adoption.hash);
    assert_eq!(displaced::list().unwrap().len(), 2);
}

#[test]
fn rollback_removes_only_the_uncommitted_copy_whose_source_still_matches() {
    use RolledBack::{Absent, Kept, Removed};

    for edit in ["absent", "committed", "id", "git", "draft", "source", "unchanged"] {
        let (home, repo, harnesses) = setup();
        let _home = home.enter();
        let source = home.path().join("a/pdf/SKILL.md");
        write(&source, "Original.\n");
        let adoption = prepare(&repo, &harnesses, "a", "pdf").unwrap();
        let id = skill_id(&repo, "pdf");
        let copy = repo.personal_dir().join("pdf");
        let (expected, content) = match edit {
            "absent" => {
                fs::remove_dir_all(&copy).unwrap();
                (Absent, None)
            }
            "committed" => {
                repo.commit(
                    &[sk1ll_core::ItemRef::Skill("pdf".into())],
                    "Commit meanwhile",
                    Some("Test <test@example.com>"),
                )
                .unwrap();
                (Kept("it was committed meanwhile"), Some("Original.\n"))
            }
            "id" => {
                write(&copy.join(".sk1ll-id"), "new-id\n");
                (Kept("its Item ID changed meanwhile"), Some("Original.\n"))
            }
            "git" => {
                write(&copy.join(".git/HEAD"), "user metadata\n");
                (Kept("it changed meanwhile"), Some("Original.\n"))
            }
            "draft" => {
                write(&copy.join("SKILL.md"), "Edited in draft.\n");
                (Kept("it changed meanwhile"), Some("Edited in draft.\n"))
            }
            "source" => {
                write(&source, "Edited in harness.\n");
                (Kept("the harness copy it came from changed meanwhile"), Some("Original.\n"))
            }
            "unchanged" => (Removed, None),
            _ => unreachable!(),
        };
        assert_eq!(rollback(&repo, &adoption, &id).unwrap(), expected, "{edit}");
        if let Some(expected) = content {
            assert_eq!(sk1ll_testkit::read(&copy.join("SKILL.md")), expected, "{edit}");
        } else {
            assert!(!copy.exists(), "{edit}");
        }
        if edit == "git" {
            assert_eq!(sk1ll_testkit::read(&copy.join(".git/HEAD")), "user metadata\n");
        }
        if edit == "unchanged" {
            let backup = displaced::list().unwrap().remove(0);
            assert!(displaced::prune_with(std::time::Duration::ZERO, 0).unwrap().is_empty());
            assert_eq!(sk1ll_testkit::read(&backup.path.join("SKILL.md")), "Original.\n");
        }
        assert_eq!(
            sk1ll_testkit::read(&source),
            if edit == "source" { "Edited in harness.\n" } else { "Original.\n" },
            "{edit}"
        );
    }
}

#[test]
fn publishing_a_selected_variant_refuses_a_late_edit_to_either_side() {
    let (home, repo, harnesses) = setup();
    let _home = home.enter();
    let source = home.path().join("a/pdf/SKILL.md");
    let draft = repo.personal_dir().join("pdf/SKILL.md");
    write(&source, "Reviewed.\n");

    let adoption = select(&repo, &harnesses, "a", "pdf").unwrap();
    write(&source, "Edited before copying.\n");
    assert!(publish(&repo, &adoption).unwrap_err().to_string().contains("source changed"));
    assert!(!draft.exists());

    write(&source, "Reviewed.\n");
    let edit = source.clone();
    let error = failpoints::scoped(
        |point, path| {
            if point == Point::CopyEntry && path == edit {
                write(&edit, "Edited while copying.\n");
            }
            failpoints::Action::Continue
        },
        || publish(&repo, &adoption),
    )
    .unwrap_err();
    assert!(error.to_string().contains("changed while being adopted"), "{error}");
    assert!(!draft.exists());
    assert_eq!(sk1ll_testkit::read(&source), "Edited while copying.\n");

    write(&draft, "Created in draft.\n");
    assert!(publish(&repo, &adoption).unwrap_err().to_string().contains("destination changed"));
    assert_eq!(sk1ll_testkit::read(&draft), "Created in draft.\n");
}
