mod sync_common;

use std::{collections::BTreeMap, fs, time::Duration};

use sk1ll_core::failpoints::Point;
use sk1ll_core::skill::ID_FILE;
use sk1ll_core::sync::*;
use sk1ll_core::{Assignment, Error, Harness, InstructionsFormat, ItemId, LinkMode, instructions};
use sk1ll_testkit::{Home, SkillsRepo, write};

use sync_common::*;

#[test]
fn adopt_turns_harness_edits_into_draft() {
    use sk1ll_core::draft::{Change, ItemRef};

    let tmp = Home::new();
    let _home = tmp.enter();
    let root = tmp.path().join("repo");
    let fixture = SkillsRepo::init(&root);
    write(&root.join("personal/pdf/SKILL.md"), "---\nname: pdf\n---\n");
    write(&root.join("personal/pdf/ref.md"), "ref\n");
    let id = ItemId::from_text("pdf-id").unwrap();
    write(&root.join("personal/pdf").join(ID_FILE), "pdf-id\n");
    write(&root.join("instructions/git.md"), "---\nharnesses: [h, b]\nid: git-rule\ndescription: Git\n---\nUse CC.\n");
    fixture.commit_all("Seed skill and rule");
    let repo = fixture.open();

    let out = tmp.path().join("harness");
    let h = Harness {
        skills: Some(out.join("skills").to_string_lossy().into_owned()),
        instructions: Some(out.join("rules").to_string_lossy().into_owned()),
        instructions_format: InstructionsFormat::Rules,
        mode: LinkMode::Copy,
        ..Harness::default()
    };
    let b = Harness { instructions: Some(out.join("AGENTS.md").to_string_lossy().into_owned()), ..Harness::default() };
    let harnesses = BTreeMap::from([("h".to_string(), h), ("b".to_string(), b)]);
    let assignment = Assignment::default();
    let plan_now = |force| plan(&repo, &harnesses, &assignment, PlanOptions { force }).unwrap();
    for a in plan_now(false) {
        apply(&repo, &a).unwrap();
    }
    assert!(plan_now(false).is_empty());
    for path in ["rules/git.md", "AGENTS.md"] {
        assert!(!fs::read_to_string(out.join(path)).unwrap().contains("id:"));
    }

    write(&out.join("skills/pdf/SKILL.md"), "---\nname: pdf\n---\nEdited.\n");
    write(&out.join("skills/pdf/extra.md"), "extra\n");
    write(&out.join("rules/git.md"), "---\ndescription: Git\n---\n\nUse CC always.\n");
    let agents = fs::read_to_string(out.join("AGENTS.md")).unwrap();
    write(&out.join("AGENTS.md"), &agents.replace("Use CC.", "Use CC!"));

    let conflicts: Vec<Action> = plan_now(false).into_iter().filter(|a| a.kind == ActionKind::Conflict).collect();
    assert_eq!(conflicts.len(), 3, "{conflicts:?}");
    let forced = plan_now(true);
    assert!(forced.iter().all(|a| a.kind == ActionKind::Update), "{forced:?}");

    for a in &conflicts {
        match &a.item {
            Item::Instructions { .. } => {
                assert!(matches!(adopt(&repo, a), Err(Error::Invalid(_))))
            }
            _ => adopt(&repo, a).unwrap(),
        }
    }
    assert!(matches!(adopt(&repo, &forced[0]), Err(Error::Invalid(_))));
    assert_eq!(fs::read_to_string(root.join("personal/pdf/SKILL.md")).unwrap(), "---\nname: pdf\n---\nEdited.\n");
    assert_eq!(skill_id(&repo, "pdf"), id);
    assert!(root.join("personal/pdf/extra.md").is_file() && !root.join("personal/pdf").join(INDEX_DIR).exists());
    assert_eq!(
        fs::read_to_string(root.join("instructions/git.md")).unwrap(),
        "---\nharnesses: [h, b]\nid: git-rule\ndescription: Git\n---\nUse CC always.\n"
    );
    let draft: Vec<(ItemRef, Change)> = repo.draft().unwrap().into_iter().map(|d| (d.item, d.change)).collect();
    assert_eq!(
        draft,
        [(ItemRef::Skill("pdf".into()), Change::Modified), (ItemRef::Rule("git".into()), Change::Modified)]
    );

    // Only the block harness is still out of line (and its edit conflicts).
    let rest = plan_now(false);
    assert!(rest.iter().all(|a| a.harness == "b" && a.kind == ActionKind::Conflict), "{rest:?}");
}

#[test]
fn rule_adopt_keeps_the_repo_id_and_routing_with_flow_frontmatter() {
    let (tmp, repo, mut harnesses) = skill_setup(LinkMode::Copy);
    let _home = tmp.enter();
    let rules = tmp.path().join("rules");
    let config = harnesses.get_mut("h").unwrap();
    config.instructions = Some(rules.display().to_string());
    config.instructions_format = InstructionsFormat::Rules;
    let source = repo.root().join("instructions/git.md");
    write(&source, "---\nid: repo-id\nharnesses: [h]\n---\nOriginal.\n");
    let plan_now = || plan(&repo, &harnesses, &Assignment::default(), PlanOptions::default()).unwrap();
    for action in plan_now() {
        apply(&repo, &action).unwrap();
    }
    let dest = rules.join("git.md");
    let conflict = || plan_now().into_iter().find(|action| action.dest == dest).unwrap();
    let invalid = "---\nid: 42\n---\nInvalid edit.\n";
    write(&dest, invalid);
    assert!(adopt(&repo, &conflict()).is_err());
    assert!(fs::read_to_string(&source).unwrap().contains("Original."));
    assert_eq!(fs::read_to_string(&dest).unwrap(), invalid);
    assert_eq!(conflict().kind, ActionKind::Conflict);

    write(&dest, "---\n{paths: [src/**], id: other}\n---\nHarness edit.\n");
    adopt(&repo, &conflict()).unwrap();
    let rule = repo.rules().unwrap().remove(0);
    assert_eq!(rule.id.as_ref().unwrap().to_string(), "repo-id");
    assert_eq!(rule.harnesses.as_deref().unwrap(), ["h"]);
    let rendered = instructions::render_rule(&rule).1;
    assert!(rendered.contains("src/**") && rendered.contains("Harness edit."));
    assert!(!rendered.contains("other") && !rendered.contains("harnesses"));
}

#[test]
fn adopting_a_filtered_import_copy_keeps_upstream_only_files() {
    let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
    let _home = tmp.enter();
    let src = repo.imported_dir().join("pdf");
    fs::create_dir_all(src.parent().unwrap()).unwrap();
    fs::rename(repo.personal_dir().join("pdf"), &src).unwrap();
    write(&src.join(ID_FILE), "vendored\n");
    write(&src.join("refs/.git/HEAD"), "upstream\n");
    write(&repo.root().join("sk1ll.toml"), "[import.pdf]\nsource = 'local'\n");
    repo.assign_missing_ids().unwrap();
    let plan_now = || plan(&repo, &harnesses, &Assignment::default(), PlanOptions::default()).unwrap();
    for action in plan_now() {
        apply(&repo, &action).unwrap();
    }
    let copy = tmp.path().join("skills/pdf");
    assert_eq!(fs::read_to_string(copy.join(ID_FILE)).unwrap(), "vendored\n");
    assert!(!copy.join("refs/.git/HEAD").exists());
    write(&copy.join("SKILL.md"), "Harness edit.\n");
    fs::remove_file(copy.join(ID_FILE)).unwrap();
    adopt(&repo, &plan_now()[0]).unwrap();
    assert_eq!(fs::read_to_string(src.join("SKILL.md")).unwrap(), "Harness edit.\n");
    assert!(!src.join(ID_FILE).exists());
    assert_eq!(fs::read_to_string(src.join("refs/.git/HEAD")).unwrap(), "upstream\n");
}

#[cfg(unix)]
#[test]
fn adopting_refuses_symlinked_repo_content_parents() {
    for origin in ["imported", "instructions"] {
        let (tmp, repo, mut harnesses) = skill_setup(LinkMode::Copy);
        let _home = tmp.enter();
        let external = tmp.path().join("external");
        let source = if origin == "imported" {
            write(&external.join("vendored/SKILL.md"), "original\n");
            external.join("vendored/SKILL.md")
        } else {
            write(&external.join("git.md"), "original\n");
            let h = harnesses.get_mut("h").unwrap();
            h.instructions = Some(tmp.path().join("rules").to_string_lossy().into_owned());
            h.instructions_format = InstructionsFormat::Rules;
            external.join("git.md")
        };
        let parent = repo.root().join(origin);
        fs::remove_file(parent.join(".gitkeep")).unwrap();
        fs::remove_dir(&parent).unwrap();
        std::os::unix::fs::symlink(&external, parent).unwrap();
        let dest = if origin == "imported" {
            tmp.path().join("skills/vendored/SKILL.md")
        } else {
            tmp.path().join("rules/git.md")
        };
        let plan_now = || plan(&repo, &harnesses, &Assignment::default(), PlanOptions::default()).unwrap();
        for action in plan_now() {
            apply(&repo, &action).unwrap();
        }
        write(&dest, "harness edit\n");
        let conflict = plan_now().into_iter().find(|action| action.kind == ActionKind::Conflict).unwrap();
        assert!(adopt(&repo, &conflict).is_err(), "{origin}");
        assert_eq!(fs::read_to_string(source).unwrap(), "original\n");
    }
}

#[test]
fn adopt_finds_a_renamed_target_by_its_item_id() {
    let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
    let _home = tmp.enter();
    let assignment = Assignment::default();
    apply(&repo, &plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap()[0]).unwrap();
    let personal = repo.personal_dir();
    fs::rename(personal.join("pdf"), personal.join("docs")).unwrap();
    write(&tmp.path().join("skills/pdf/SKILL.md"), "Edited in the harness.\n");
    let conflict = || {
        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        actions.into_iter().find(|a| a.kind == ActionKind::Conflict && a.dest.ends_with("pdf")).unwrap()
    };
    let pending = conflict();

    // Another item of the old name, and later a copy that shares the ID.
    write(&personal.join("pdf/SKILL.md"), "Another pdf.\n");
    repo.assign_missing_ids().unwrap();
    fs::rename(personal.join("docs"), tmp.path().join("docs-aside")).unwrap();
    let error = adopt(&repo, &pending).unwrap_err().to_string();
    assert!(error.contains("no skill in the repo has now") && error.contains("another Item ID"), "{error}");
    fs::rename(tmp.path().join("docs-aside"), personal.join("docs")).unwrap();
    fs::create_dir(personal.join("copy")).unwrap();
    fs::copy(personal.join("docs/SKILL.md"), personal.join("copy/SKILL.md")).unwrap();
    fs::copy(personal.join("docs/.sk1ll-id"), personal.join("copy/.sk1ll-id")).unwrap();
    let error = adopt(&repo, &pending).unwrap_err().to_string();
    assert!(error.contains("personal/copy") && error.contains("personal/docs"), "{error}");
    fs::remove_dir_all(personal.join("copy")).unwrap();

    adopt(&repo, &conflict()).unwrap();
    assert_eq!(fs::read_to_string(personal.join("docs/SKILL.md")).unwrap(), "Edited in the harness.\n");
    assert_eq!(fs::read_to_string(personal.join("pdf/SKILL.md")).unwrap(), "Another pdf.\n");
}

#[test]
fn adopt_owns_only_what_it_copied_and_keeps_what_it_left_out() {
    let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
    let _home = tmp.enter();
    let (dest, conflict) = adoptable(&tmp, &repo, &harnesses);
    write(&dest.join(".git/HEAD"), "harness git\n");
    let source = repo.personal_dir().join("pdf");
    write(&source.join(".git/HEAD"), "repo git\n");

    let at = dest.clone();
    let during =
        with_hook(Point::CopyEntry, move |_| write(&at.join("late.md"), "Late.\n"), || adopt(&repo, &conflict));
    assert!(matches!(during, Err(Error::Changed(_))), "{during:?}");
    assert_eq!(fs::read_to_string(source.join("SKILL.md")).unwrap(), "PDFs.\n");
    assert!(displaced::list().unwrap().is_empty());
    fs::remove_file(dest.join("late.md")).unwrap();

    adopt(&repo, &conflict).unwrap();
    assert_eq!(fs::read_to_string(source.join("SKILL.md")).unwrap(), "Edited in the harness.\n");
    assert!(!source.join(".git").exists());
    assert_eq!(fs::read_to_string(dest.join(".git/HEAD")).unwrap(), "harness git\n");
    let repo_entry = displaced::list().unwrap().remove(0);
    assert!(repo_entry.changed && repo_entry.harness == "repo", "{repo_entry:?}");
    assert_eq!(fs::read_to_string(repo_entry.path.join(".git/HEAD")).unwrap(), "repo git\n");

    let mut assignment = Assignment::default();
    let normal = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    assert_eq!(normal[0].kind, ActionKind::Conflict, "{normal:?}");
    assignment.set_skill("h", &skill_id(&repo, "pdf"), false);
    let removal = plan(&repo, &harnesses, &assignment, PlanOptions { force: true }).unwrap();
    assert_eq!(removal[0].kind, ActionKind::Remove);
    apply(&repo, &removal[0]).unwrap();
    assert!(fs::symlink_metadata(&dest).is_err());
    let entry = displaced::list().unwrap().into_iter().find(|e| e.dest == dest).unwrap();
    assert!(entry.changed);
    assert!(displaced::prune_with(Duration::ZERO).unwrap().is_empty());
    assert_eq!(fs::read_to_string(entry.path.join(".git/HEAD")).unwrap(), "harness git\n");
    assert_eq!(fs::read_to_string(repo_entry.path.join(".git/HEAD")).unwrap(), "repo git\n");
}

/// Adopt keeps the repo version it replaces as changed unless that is
/// exactly the committed version: a Draft edit or deletion there may be in
/// no other copy. A name ending in a CR is compared as it is.
#[test]
fn adopt_keeps_a_replaced_draft_edit_and_lets_a_committed_version_go() {
    for draft in ["none", "edit", "deletion"] {
        let (tmp, repo, mut harnesses) = skill_setup(LinkMode::Copy);
        let _home = tmp.enter();
        let config = harnesses.get_mut("h").unwrap();
        config.instructions = Some(tmp.path().join("rules").display().to_string());
        config.instructions_format = InstructionsFormat::Rules;
        let rule = repo.root().join("instructions/git.md");
        write(&rule, "---\nid: git-id\nharnesses: [h]\n---\nCommitted.\n");
        write(&repo.personal_dir().join("pdf/ref.md"), "Reference.\n");
        write(&repo.personal_dir().join("pdf/notes\r"), "Notes.\n");
        sk1ll_testkit::git::git(repo.root(), &["add", "."]);
        sk1ll_testkit::git::git(repo.root(), &["commit", "-qm", "rule and references"]);
        let plan_now = || plan(&repo, &harnesses, &Assignment::default(), PlanOptions::default()).unwrap();
        for action in plan_now() {
            apply(&repo, &action).unwrap();
        }
        write(&tmp.path().join("skills/pdf/SKILL.md"), "Harness edit.\n");
        edit_rule_body(&tmp.path().join("rules/git.md"), "Harness edit.\n");
        match draft {
            "edit" => {
                write(&repo.personal_dir().join("pdf/SKILL.md"), "Draft edit.\n");
                edit_rule_body(&rule, "Draft edit.\n");
            }
            "deletion" => fs::remove_file(repo.personal_dir().join("pdf/ref.md")).unwrap(),
            _ => {}
        }
        let conflicts: Vec<_> = plan_now().into_iter().filter(|a| a.kind == ActionKind::Conflict).collect();
        assert_eq!(conflicts.len(), 2, "{conflicts:?}");
        for conflict in &conflicts {
            adopt(&repo, conflict).unwrap();
        }
        let replaced: Vec<_> = displaced::list().unwrap().into_iter().filter(|e| e.harness == "repo").collect();
        assert_eq!(replaced.len(), 2, "{replaced:?}");
        for entry in &replaced {
            let changed = match draft {
                "none" => false,
                "edit" => true,
                _ => entry.kind == displaced::Kind::Skill,
            };
            assert_eq!(entry.changed, changed, "{draft}: {entry:?}");
        }
        displaced::prune_with(Duration::ZERO).unwrap();
        let left = displaced::list().unwrap().into_iter().filter(|e| e.harness == "repo").count();
        assert_eq!(left, replaced.iter().filter(|e| e.changed).count(), "{draft}");
    }
}

/// A skills repo in a subdirectory of its Git worktree: what Adopt replaces
/// is compared with that subdirectory's committed version, not with a path
/// of the same name at the worktree's top.
#[test]
fn adopt_compares_a_nested_skills_repo_with_its_own_committed_version() {
    for draft in [false, true] {
        let tmp = Home::new();
        let _home = tmp.enter();
        let outer = tmp.path().join("project");
        let git = |args: &[&str]| sk1ll_testkit::git::git(&outer, args);
        fs::create_dir_all(&outer).unwrap();
        git(&["init", "-q", "-b", "main"]);
        let root = outer.join("skills");
        write(&root.join("sk1ll.toml"), "");
        write(&root.join("personal/pdf/SKILL.md"), "Committed.\n");
        write(&root.join("personal/pdf").join(ID_FILE), "pdf-id\n");
        // A path of the same name at the top, holding what the Draft will.
        write(&outer.join("personal/pdf/SKILL.md"), "Draft edit.\n");
        write(&outer.join("personal/pdf").join(ID_FILE), "pdf-id\n");
        git(&["add", "."]);
        git(&["commit", "-qm", "seed"]);
        let repo = sk1ll_core::Repo::open(&root).unwrap();
        let harness = Harness {
            skills: Some(tmp.path().join("skills").display().to_string()),
            mode: LinkMode::Copy,
            ..Harness::default()
        };
        let harnesses = BTreeMap::from([("h".to_string(), harness)]);
        let plan_now = || plan(&repo, &harnesses, &Assignment::default(), PlanOptions::default()).unwrap();
        for action in plan_now() {
            apply(&repo, &action).unwrap();
        }
        write(&tmp.path().join("skills/pdf/SKILL.md"), "Harness edit.\n");
        if draft {
            write(&root.join("personal/pdf/SKILL.md"), "Draft edit.\n");
        }
        let conflict = plan_now().into_iter().find(|a| a.kind == ActionKind::Conflict).unwrap();
        adopt(&repo, &conflict).unwrap();
        let replaced = displaced::list().unwrap().into_iter().find(|e| e.harness == "repo").unwrap();
        assert_eq!(replaced.changed, draft, "{replaced:?}");
        displaced::prune_with(Duration::ZERO).unwrap();
        assert_eq!(displaced::get(&replaced.id).is_ok(), draft);
    }
}
