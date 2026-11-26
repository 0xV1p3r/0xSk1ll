mod sync_common;

use std::{fs, path::Path, time::Duration};

use sk1ll_core::failpoints::{self, Point};
use sk1ll_core::skill::ID_FILE;
use sk1ll_core::sync::*;
use sk1ll_core::{Assignment, Error, InstructionsFormat, LinkMode, Repo};
use sk1ll_testkit::write;
use walkdir::WalkDir;

use sync_common::*;

#[test]
fn the_recovery_stamp_covers_payloads_beside_instructions() {
    let d = Displacement::new("block update");
    let _home = d.home.enter();
    // Only a block edited by hand leaves an entry when it is rewritten (forced).
    let text = fs::read_to_string(&d.dest).unwrap();
    write(&d.dest, &text.replace("Use CC.", "Use CC by hand."));
    let forced = plan(&d.repo, &d.harnesses, &d.assignment, PlanOptions { force: true }).unwrap();
    apply(&d.repo, &forced[0]).unwrap();
    let entry = d.entry();
    assert!(entry.path.starts_with(d.area()));
    let before = displaced::stamp().unwrap();
    assert_eq!(displaced::stamp().unwrap(), before);
    std::thread::sleep(Duration::from_millis(30));
    fs::write(d.kept_file(&entry), "Written to the recovery copy.\n").unwrap();
    assert_ne!(displaced::stamp().unwrap(), before);
}

#[test]
fn pruning_rechecks_queued_entries_and_includes_git_and_modes() {
    use std::io::Write;
    for case in ["queued write", "git metadata", "empty directory", "mode"] {
        let d = Displacement::new("skill update");
        let _home = d.home.enter();
        let mut open = fs::OpenOptions::new().append(true).open(&d.file).unwrap();
        apply(&d.repo, &d.action()).unwrap();
        let entry = d.entry();
        assert!(!entry.changed);
        match case {
            "queued write" => {
                let removed = with_hook(
                    Point::BeforePrune,
                    move |_| open.write_all(b"Late edit.\n").unwrap(),
                    || displaced::prune_with(Duration::ZERO),
                )
                .unwrap();
                assert!(removed.is_empty());
                assert!(fs::read_to_string(entry.path.join("SKILL.md")).unwrap().ends_with("Late edit.\n"));
            }
            "git metadata" => write(&entry.path.join(".git/notes"), "Git metadata"),
            "empty directory" => fs::create_dir(entry.path.join("empty")).unwrap(),
            "mode" => {
                let path = entry.path.join("SKILL.md");
                let mut permissions = path.metadata().unwrap().permissions();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    permissions.set_mode(permissions.mode() ^ 0o100);
                }
                #[cfg(not(unix))]
                permissions.set_readonly(true);
                fs::set_permissions(path, permissions).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(displaced::get(&entry.id).unwrap().changed, "{case}");
        assert!(displaced::prune_with(Duration::ZERO).unwrap().is_empty(), "{case}");
    }
}

#[cfg(unix)]
#[test]
fn edits_since_placement_conflict_and_forced_swaps_keep_the_backup() {
    use std::os::unix::fs::PermissionsExt;
    for case in ["git init", "mode", "empty directory"] {
        let d = Displacement::new("skill update");
        let _home = d.home.enter();
        match case {
            "git init" => {
                sk1ll_testkit::git::git(&d.dest, &["init", "-q"]);
            }
            "mode" => {
                let mode = fs::metadata(&d.file).unwrap().permissions().mode();
                fs::set_permissions(&d.file, fs::Permissions::from_mode(mode ^ 0o100)).unwrap();
            }
            "empty directory" => fs::create_dir(d.dest.join("empty")).unwrap(),
            _ => unreachable!(),
        }
        let normal = plan(&d.repo, &d.harnesses, &d.assignment, PlanOptions::default()).unwrap();
        assert_eq!(normal[0].kind, ActionKind::Conflict, "{case}");
        let forced = plan(&d.repo, &d.harnesses, &d.assignment, PlanOptions { force: true }).unwrap();
        apply(&d.repo, &forced[0]).unwrap();
        let entry = d.entry();
        assert!(entry.changed, "{case}");
        match case {
            "git init" => assert!(entry.path.join(".git/HEAD").is_file()),
            "mode" => assert_ne!(entry.path.join("SKILL.md").metadata().unwrap().permissions().mode() & 0o100, 0),
            "empty directory" => assert!(entry.path.join("empty").is_dir()),
            _ => unreachable!(),
        }
        assert!(displaced::prune_with(Duration::ZERO).unwrap().is_empty(), "{case}");
    }
}

#[test]
fn a_write_to_an_early_file_during_the_final_prune_scan_keeps_the_entry() {
    use std::io::Write;
    let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
    let _home = tmp.enter();
    let source = repo.personal_dir().join("pdf");
    write(&source.join("a-first"), "old");
    fs::write(source.join("z-large"), vec![b'x'; 1 << 20]).unwrap();
    let assignment = Assignment::default();
    let actions = || plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    apply(&repo, &actions()[0]).unwrap();
    let dest = tmp.path().join("skills/pdf");
    let open = fs::OpenOptions::new().write(true).open(dest.join("a-first")).unwrap();
    write(&source.join("SKILL.md"), "Updated.\n");
    apply(&repo, &actions()[0]).unwrap();
    let entry = displaced::list().unwrap().remove(0);
    assert!(!entry.changed);
    let later = entry.path.join("z-large");
    let checking = std::rc::Rc::new(std::cell::Cell::new(false));
    let at_deletion_check = checking.clone();
    let mut open = Some(open);
    let removed = with_read_hook(
        move |path| {
            if checking.get()
                && path == later
                && let Some(mut file) = open.take()
            {
                file.write_all(b"new").unwrap();
            }
        },
        || {
            with_hook(
                Point::BeforePrune,
                move |_| at_deletion_check.set(true),
                || displaced::prune_with(Duration::ZERO),
            )
        },
    )
    .unwrap();
    assert!(removed.is_empty());
    assert_eq!(fs::read_to_string(entry.path.join("a-first")).unwrap(), "new");
    assert!(displaced::get(&entry.id).unwrap().changed);
}

#[cfg(unix)]
#[test]
fn recovery_refuses_a_symlinked_scratch_root() {
    let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
    let _home = tmp.enter();
    let assignment = Assignment::default();
    let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    apply(&repo, &actions[0]).unwrap();
    let dir = actions[0].dest.parent().unwrap();
    let outside = tmp.path().join("outside");
    let victim = outside.join("stage-pdf/data");
    write(&victim, "Keep this.\n");
    for scratch in [dir.join(TMP_DIR), dir.join(INDEX_DIR).join("staging")] {
        if scratch.is_dir() {
            fs::remove_dir(&scratch).unwrap();
        }
        std::os::unix::fs::symlink(&outside, &scratch).unwrap();
        assert!(recover(None, &harnesses).is_err());
        assert_eq!(fs::read_to_string(&victim).unwrap(), "Keep this.\n");
        fs::remove_file(scratch).unwrap();
    }
}

#[test]
fn a_forced_swap_interrupted_after_detach_preserves_the_edited_backup() {
    let d = Displacement::new("skill update");
    let _home = d.home.enter();
    write(&d.file, "Hand-edited original.\n");
    let actions = plan(&d.repo, &d.harnesses, &d.assignment, PlanOptions { force: true }).unwrap();
    let stopped = std::panic::catch_unwind(|| {
        with_hook(
            Point::AfterAside,
            |dest| {
                write(&dest.join("SKILL.md"), "Newcomer.\n");
                std::panic::panic_any("interrupted sync");
            },
            || apply(&d.repo, &actions[0]),
        )
    })
    .expect_err("interrupted before put_back");
    assert_eq!(stopped.downcast_ref::<&str>(), Some(&"interrupted sync"));
    let entry = d.entry();
    assert!(displaced::prune_with(Duration::ZERO).unwrap().is_empty());
    recover(None, &d.harnesses).unwrap();
    assert!(displaced::get(&entry.id).unwrap().changed);
    assert!(displaced::prune_with(Duration::ZERO).unwrap().is_empty());
    assert_eq!(fs::read_to_string(entry.path.join("SKILL.md")).unwrap(), "Hand-edited original.\n");
    assert_eq!(fs::read_to_string(&d.file).unwrap(), "Newcomer.\n");
}

#[test]
fn writers_from_independent_repos_serialize_in_one_harness_directory() {
    use std::sync::mpsc::{self, RecvTimeoutError};
    let (tmp, first, harnesses) = skill_setup(LinkMode::Copy);
    let _home = tmp.enter();
    let other = tmp.path().join("other-repo");
    write(&other.join("sk1ll.toml"), "");
    write(&other.join("personal/notes/SKILL.md"), "Notes.\n");
    let second = Repo::open(&other).unwrap();
    let assignment = Assignment::default();
    let place = |repo| plan(repo, &harnesses, &assignment, PlanOptions::default()).unwrap().remove(0);
    let one = place(&first);
    let two = place(&second);
    std::thread::scope(|threads| {
        let (staged, ready) = mpsc::channel();
        let (resume, wait) = mpsc::channel();
        let first = threads.spawn(|| {
            let _home = tmp.enter();
            with_hook(
                Point::BeforeSwap,
                move |_| {
                    staged.send(()).unwrap();
                    wait.recv().unwrap();
                },
                || apply(&first, &one),
            )
        });
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        let (started, started_rx) = mpsc::channel();
        let (finished, result) = mpsc::channel();
        let two = &two;
        let home = &tmp;
        let second = threads.spawn(move || {
            let _home = home.enter();
            started.send(()).unwrap();
            finished.send(apply(&second, two)).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let early = result.recv_timeout(Duration::from_millis(100));
        let stage_intact = tmp.path().join("skills/.sk1ll-state/staging/stage-pdf/SKILL.md").is_file();
        resume.send(()).unwrap();
        first.join().unwrap().unwrap();
        second.join().unwrap();
        assert!(matches!(early, Err(RecvTimeoutError::Timeout)), "{early:?}");
        assert!(stage_intact, "the second writer left the first writer's stage alone");
        result.recv().unwrap().unwrap();
    });
    assert_copy_recorded(&one.dest);
    assert_copy_recorded(&two.dest);
}

#[test]
fn rewriting_an_owned_block_leaves_no_recovery_entry() {
    let (tmp, repo, harnesses, dest) = instructions_setup(InstructionsFormat::Block);
    let _home = tmp.enter();
    let mut assignment = Assignment::default();
    let sync = |assignment: &Assignment, force| {
        for action in plan(&repo, &harnesses, assignment, PlanOptions { force }).unwrap() {
            apply(&repo, &action).unwrap();
        }
    };
    sync(&assignment, false);
    assignment.set_rule("h", &rule_id(&repo, "git"), false);
    sync(&assignment, false);
    assert!(!dest.exists(), "sync created it, and its block was all of it");
    let entries = displaced::list().unwrap();
    assert!(entries.len() == 1 && !entries[0].changed, "taken out into recovery: {entries:?}");
    assert_eq!(displaced::prune_with(Duration::ZERO).unwrap().len(), 1);
    assignment.set_rule("h", &rule_id(&repo, "git"), true);
    sync(&assignment, false);
    write(&dest, &format!("# Mine\n\n{}", fs::read_to_string(&dest).unwrap()));
    edit_rule_body(&repo.root().join("instructions/git.md"), "Use CC always.\n");
    sync(&assignment, false);
    let text = fs::read_to_string(&dest).unwrap();
    assert!(text.starts_with("# Mine\n") && text.contains("Use CC always."), "{text}");
    assert!(displaced::list().unwrap().is_empty(), "{:?}", displaced::list().unwrap());

    let edited = text.replace("Use CC always.", "Use CC by hand.");
    write(&dest, &edited);
    assert_eq!(plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap()[0].kind, ActionKind::Conflict);
    sync(&assignment, true);
    let entries = displaced::list().unwrap();
    assert!(entries.len() == 1 && entries[0].changed, "{entries:?}");
    assert_eq!(fs::read_to_string(&entries[0].path).unwrap(), edited);
    assert!(displaced::prune_with(Duration::ZERO).unwrap().is_empty());
}

#[test]
fn recovery_follows_a_rename_and_refuses_a_foreign_repo_or_reused_id() {
    let (tmp, _repo, harnesses) = skill_setup(LinkMode::Copy);
    let _home = tmp.enter();
    let repo = Repo::init_committed(&tmp.path().join("tracked")).unwrap();
    write(&repo.personal_dir().join("pdf/SKILL.md"), "PDFs.\n");
    let pdf = sk1ll_core::ItemRef::Skill("pdf".into());
    let docs = sk1ll_core::ItemRef::Skill("docs".into());
    repo.commit(std::slice::from_ref(&pdf), "PDF", None).unwrap();
    let id = repo.skills().unwrap()[0].id.clone().unwrap();
    let plan_now = || plan(&repo, &harnesses, &Assignment::default(), PlanOptions::default()).unwrap();
    apply(&repo, &plan_now()[0]).unwrap();
    let mut open = fs::OpenOptions::new().append(true).open(tmp.path().join("skills/pdf/SKILL.md")).unwrap();
    repo.rename_item("skill", "pdf", "docs").unwrap();
    for action in plan_now() {
        apply(&repo, &action).unwrap();
    }
    use std::io::Write;
    open.write_all(b"late edit\n").unwrap();
    let entry =
        displaced::changed().unwrap().into_iter().find(|entry| entry.dest == tmp.path().join("skills/pdf")).unwrap();
    let foreign = Repo::init_committed(&tmp.path().join("foreign")).unwrap();
    assert!(displaced::target(&foreign, &entry).err().unwrap().to_string().contains("another repo"));
    assert_eq!(displaced::target(&repo, &entry).unwrap().path, repo.personal_dir().join("docs"));

    repo.commit(&[pdf, docs.clone()], "Rename", None).unwrap();
    repo.delete_item("skill", "docs").unwrap();
    repo.commit(&[docs], "Delete", None).unwrap();
    let replacement = repo.personal_dir().join("replacement");
    write(&replacement.join("SKILL.md"), "Different item.\n");
    write(&replacement.join(ID_FILE), &format!("{id}\n"));
    assert!(adopt_test_entry(&repo, &entry.id).unwrap_err().to_string().contains("Pick a target"));
    assert_eq!(fs::read_to_string(replacement.join("SKILL.md")).unwrap(), "Different item.\n");
    assert!(displaced::get(&entry.id).is_ok());
}

#[test]
fn recovery_recreates_a_deleted_rule_with_its_placed_id() {
    let (tmp, repo, mut harnesses) = skill_setup(LinkMode::Copy);
    let _home = tmp.enter();
    let rules = tmp.path().join("rules");
    let config = harnesses.get_mut("h").unwrap();
    config.instructions = Some(rules.display().to_string());
    config.instructions_format = InstructionsFormat::Rules;
    let source = repo.root().join("instructions/git.md");
    write(&source, "---\nid: original-rule\n---\nOriginal.\n");
    let plan_now = |force| plan(&repo, &harnesses, &Assignment::default(), PlanOptions { force }).unwrap();
    for action in plan_now(false) {
        apply(&repo, &action).unwrap();
    }
    let dest = rules.join("git.md");
    assert_eq!(fs::read_to_string(&dest).unwrap(), "Original.\n");
    write(&dest, "---\nid: harness-edit\n---\nEdited.\n");
    repo.delete_item("rule", "git").unwrap();
    for action in plan_now(true) {
        apply(&repo, &action).unwrap();
    }
    let entry = displaced::list().unwrap().into_iter().find(|entry| entry.dest == dest).unwrap();
    assert_eq!(entry.item_id.unwrap().to_string(), "original-rule");
    adopt_test_entry(&repo, &entry.id).unwrap();
    let restored = repo.rules().unwrap().remove(0);
    assert_eq!(restored.id.unwrap().to_string(), "original-rule");
    assert_eq!(restored.body, "Edited.");
}

#[test]
fn displaced_content_is_pruned_after_the_grace_period_unless_it_changed() {
    let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
    let _home = tmp.enter();
    let assignment = Assignment::default();
    apply(&repo, &plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap()[0]).unwrap();
    assert!(displaced::list().unwrap().is_empty(), "a create displaces nothing");
    for version in ["v2\n", "v3\n"] {
        write(&repo.personal_dir().join("pdf/SKILL.md"), version);
        for a in plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap() {
            apply(&repo, &a).unwrap();
        }
    }
    let entries = displaced::list().unwrap();
    assert_eq!(entries.len(), 2, "{entries:?}");
    assert!(entries.iter().all(|e| !e.changed && e.harness == "h" && e.dest == tmp.path().join("skills/pdf")));
    assert!(displaced::changed().unwrap().is_empty());
    // Too young to prune: a writer may still hold one open.
    assert!(displaced::prune().unwrap().is_empty());

    // A late write to the older entry keeps it, however old.
    write(&entries[0].path.join("SKILL.md"), "Late.\n");
    assert_eq!(displaced::prune_with(Duration::ZERO).unwrap(), [entries[1].id.clone()]);
    let left = displaced::list().unwrap();
    assert_eq!(left.len(), 1);
    assert!(left[0].changed && left[0].id == entries[0].id);
    displaced::discard(&left[0].id).unwrap();
    assert!(displaced::list().unwrap().is_empty());
    assert!(displaced::discard("../escape").is_err());
}

#[test]
fn crash_leftovers_are_ignored_by_planning_and_recovered() {
    for explicit in [true, false] {
        let (tmp, repo, mut harnesses) = skill_setup(LinkMode::Copy);
        let _home = tmp.enter();
        let rules = tmp.path().join("rules");
        harnesses.get_mut("h").unwrap().instructions = Some(rules.to_string_lossy().into_owned());
        harnesses.get_mut("h").unwrap().instructions_format = InstructionsFormat::Rules;
        write(&repo.root().join("instructions/git.md"), "Use CC.\n");
        let assignment = Assignment::default();
        for a in plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap() {
            apply(&repo, &a).unwrap();
        }
        write(&repo.personal_dir().join("pdf/SKILL.md"), "Updated source.\n");

        // Killed between renames, by an older version (names next to the
        // skills) and by this one (in the scratch dir): the destination
        // was moved aside and the replacement never went in.
        let skills = tmp.path().join("skills");
        fs::rename(skills.join("pdf"), skills.join(".sk1ll-old-pdf")).unwrap();
        copy_test_skill(&repo.personal_dir().join("pdf"), &skills.join(".sk1ll-stage-pdf")).unwrap();
        copy_test_skill(&repo.personal_dir().join("pdf"), &skills.join(".sk1ll-tmp/stage-pdf")).unwrap();
        copy_test_skill(&skills.join(".sk1ll-old-pdf"), &skills.join(".sk1ll-tmp/old-gone")).unwrap();
        let mut index = read_index(&skills);
        index["entries"]["gone"] = index["entries"]["pdf"].clone();
        replace_index(&skills, &index);
        write(&skills.join(".sk1ll-tmp/old-gone/SKILL.md"), "Edited before the crash.\n");
        fs::rename(rules.join("git.md"), rules.join(".sk1ll-old-git.md")).unwrap();
        write(&rules.join(".git.md.sk1ll-tmp"), "staged\n");
        let index_stage = skills.join(".sk1ll-state/staging/.index.json.sk1ll-tmp-1");
        write(&index_stage, "interrupted index write\n");

        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        let summary: Vec<_> = actions.iter().map(|a| (a.kind, a.dest.clone())).collect();
        assert_eq!(
            summary,
            [
                (ActionKind::Create, skills.join("pdf")),
                (ActionKind::Remove, skills.join("gone")),
                (ActionKind::Create, rules.join("git.md")),
            ],
            "explicit={explicit}"
        );
        if explicit {
            let lock = sk1ll_core::EngineLock::acquire(repo.root()).unwrap();
            let rescued = recover(Some(&lock), &harnesses).unwrap();
            assert_eq!(rescued.len(), 6, "{rescued:?}");
            assert!(rescued.iter().all(|entry| entry.changed), "{rescued:?}");
        }
        for a in &actions {
            apply(&repo, a).unwrap();
        }
        assert!(plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap().is_empty());
        assert_eq!(fs::read_to_string(skills.join("pdf/SKILL.md")).unwrap(), "Updated source.\n");
        let mut names: Vec<_> = fs::read_dir(&skills).unwrap().map(|e| e.unwrap().file_name()).collect();
        names.sort();
        assert_eq!(names, [INDEX_DIR, TMP_DIR, "pdf"], "explicit={explicit}");
        assert!(sk1ll_leftovers(&skills).is_empty(), "only recovery entries are kept");
        assert!(sk1ll_leftovers(&rules).is_empty());
        assert!(!index_stage.exists());
        let changed = displaced::changed().unwrap();
        assert_eq!(changed.len(), 6, "{changed:?}");
        let gone = changed.iter().find(|entry| entry.dest == skills.join("gone")).unwrap();
        assert_eq!(fs::read_to_string(gone.path.join("SKILL.md")).unwrap(), "Edited before the crash.\n");
        assert!(
            changed
                .iter()
                .any(|entry| entry.dest == rules.join("git.md")
                    && fs::read_to_string(&entry.path).unwrap() == "staged\n")
        );
        assert!(displaced::prune_with(Duration::ZERO).unwrap().is_empty());
    }
}

#[cfg(unix)]
#[test]
fn late_writes_after_sync_reach_the_recovery_copy() {
    use std::io::Write;
    for case in ["skill update", "skill removal", "rule update", "rule removal"] {
        let d = Displacement::new(case);
        let _home = d.home.enter();
        let mut open = fs::OpenOptions::new().append(true).open(&d.file).unwrap();
        apply(&d.repo, &d.action()).unwrap();
        open.write_all(b"Written after sync returned.\n").unwrap();
        let entry = d.entry();
        let kept = fs::read_to_string(d.kept_file(&entry)).unwrap();
        assert!(kept.ends_with("Written after sync returned.\n"), "{case}: {kept}");
        assert!(entry.changed && entry.path.starts_with(d.area()), "{case}: {entry:?}");
    }
}

#[cfg(unix)]
#[test]
fn displaced_content_never_goes_back_over_what_appeared_meanwhile() {
    use std::io::Write;
    for case in ["rule removal", "skill removal", "link removal", "skill update"] {
        let d = Displacement::new(case);
        let _home = d.home.enter();
        let action = d.action();
        // A removal is called off by an edit made right before the content
        // is moved into recovery; the update fails on the newcomer itself.
        let mut open = fs::OpenOptions::new().append(true).open(&d.file).ok();
        let hooks = move |point: Point, dest: &Path| match point {
            Point::BeforeSwap if case.starts_with("link") => {
                fs::remove_file(dest).unwrap();
                std::os::unix::fs::symlink("/elsewhere", dest).unwrap();
            }
            Point::BeforeSwap if case.ends_with("removal") => open.take().unwrap().write_all(b"Late edit.\n").unwrap(),
            Point::AfterAside => write(dest, "Newcomer.\n"),
            _ => {}
        };
        let entry = reported(with_hooks(hooks, || apply(&d.repo, &action)));
        assert_eq!(fs::read_to_string(&d.dest).unwrap(), "Newcomer.\n", "{case}");
        assert!(entry.path.starts_with(d.area()), "{case}");
        match case {
            "rule removal" => assert_eq!(fs::read_to_string(&entry.path).unwrap(), "Use CC.\nLate edit.\n"),
            "link removal" => assert_eq!(fs::read_link(&entry.path).unwrap(), Path::new("/elsewhere")),
            "skill removal" => assert_eq!(fs::read_to_string(d.kept_file(&entry)).unwrap(), "PDFs.\nLate edit.\n"),
            _ => assert_eq!(fs::read_to_string(d.kept_file(&entry)).unwrap(), "PDFs.\n", "intact, yet kept"),
        }
        // Kept for good, however old.
        assert!(!displaced::prune_with(Duration::ZERO).unwrap().contains(&entry.id), "{case}");
        assert!(displaced::get(&entry.id).unwrap().changed, "{case}");
        assert!(!d.holder.join(TMP_DIR).join("stage-pdf").exists(), "{case}");
    }
}

#[cfg(unix)]
#[test]
fn adopting_a_block_entry_is_refused_while_the_managed_block_is_edited_by_hand() {
    use std::io::Write;
    let d = Displacement::new("block update");
    let _home = d.home.enter();
    let action = d.action();
    let mut open = fs::OpenOptions::new().append(true).open(&d.dest).unwrap();
    let late = move |_: &Path| open.write_all(b"Outside note.\n").unwrap();
    let entry = reported(with_hook(Point::AfterSwap, late, || apply(&d.repo, &action)));
    let synced = fs::read_to_string(&d.dest).unwrap();
    let edited = synced.replace("Use CC always.", "Use CC always, edited by hand.");
    assert_ne!(edited, synced);
    write(&d.dest, &edited);
    let conflict = |d: &Displacement| {
        let actions = plan(&d.repo, &d.harnesses, &d.assignment, PlanOptions::default()).unwrap();
        actions.len() == 1 && actions[0].kind == ActionKind::Conflict
    };
    assert!(conflict(&d));

    let err = adopt_test_entry(&d.repo, &entry.id).unwrap_err();
    assert!(matches!(&err, Error::Invalid(m) if m.contains("edited by hand")), "{err}");
    assert_eq!(fs::read_to_string(&d.dest).unwrap(), edited);
    assert!(displaced::get(&entry.id).unwrap().changed);
    assert!(conflict(&d), "the hand edit is still a conflict");
}

#[test]
fn displaced_content_stays_for_the_grace_period_however_much_there_is() {
    let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
    let _home = tmp.enter();
    let assignment = Assignment::default();
    write(&repo.personal_dir().join("pdf/asset.bin"), &"x".repeat(64 << 10));
    let sync = || {
        for a in plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap() {
            apply(&repo, &a).unwrap();
        }
    };
    sync();
    for version in ["v2\n", "v3\n", "v4\n"] {
        write(&repo.personal_dir().join("pdf/SKILL.md"), version);
        sync();
    }
    let entries = displaced::list().unwrap();
    assert_eq!(entries.len(), 3, "{entries:?}");
    write(&entries[0].path.join("SKILL.md"), "Late.\n");
    // Young: nothing goes, whatever they take, as a writer may hold one open.
    assert!(displaced::prune().unwrap().is_empty());
    assert_eq!(displaced::list().unwrap().len(), 3);
    // Old enough: the unchanged ones go; the changed one stays.
    let mut pruned = displaced::prune_with(Duration::ZERO).unwrap();
    pruned.sort();
    assert_eq!(pruned, [entries[1].id.clone(), entries[2].id.clone()]);
    let left = displaced::list().unwrap();
    assert_eq!(left.len(), 1);
    assert!(left[0].changed && left[0].id == entries[0].id);
}

#[cfg(unix)]
#[test]
fn displacing_fails_closed_if_the_move_crosses_filesystems() {
    use std::os::unix::fs::MetadataExt;
    for case in ["skill update", "skill removal", "rule update", "rule removal", "block update"] {
        let d = Displacement::new(case);
        let _home = d.home.enter();
        let action = d.action();
        let (before, inode) = (fs::read(&d.file).unwrap(), fs::metadata(&d.file).unwrap().ino());
        // Every move or link into an entry crosses file systems, which
        // in-directory ones never should.
        let _exdev = failpoints::install(|point, _| match point {
            Point::RecoveryRename | Point::RecoveryLink => failpoints::Action::Fail(std::io::ErrorKind::CrossesDevices),
            _ => failpoints::Action::Continue,
        });
        let result = apply(&d.repo, &action);
        assert!(result.is_err(), "{case}: {result:?}");
        assert_eq!(fs::read(&d.file).unwrap(), before, "{case}");
        assert_eq!(fs::metadata(&d.file).unwrap().ino(), inode, "{case}: the original itself, in place");
        assert!(displaced::list().unwrap().is_empty(), "{case}");
        assert!(sk1ll_leftovers(&d.holder).is_empty(), "{case}: {:?}", sk1ll_leftovers(&d.holder));
        assert!(!d.holder.join(TMP_DIR).exists(), "{case}: tidied away");
    }
}

#[test]
fn recovery_entries_are_found_wherever_they_are() {
    let d = Displacement::new("rule update");
    let _home = d.home.enter();
    let state = displaced::root().unwrap();

    apply(&d.repo, &d.action()).unwrap();
    let entry = d.entry();
    assert!(entry.path.starts_with(d.area()) && !entry.changed);
    // A harness that loads every `.md` below its rules dir finds none here.
    let md: Vec<_> = WalkDir::new(d.holder.join(TMP_DIR))
        .into_iter()
        .map(|e| e.unwrap().into_path())
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    assert!(md.is_empty(), "{md:?}");
    assert_eq!(fs::read_to_string(d.area().join(".gitignore")).unwrap(), "*\n");
    // The rules dir's own planning ignores it.
    assert!(plan(&d.repo, &d.harnesses, &d.assignment, PlanOptions::default()).unwrap().is_empty());

    // An entry an older version kept in the state directory itself.
    let legacy = state.join("0000000000001-h-old.md");
    write(&legacy.join("content"), "Old rule.\n");
    let meta = format!(
        r#"{{"version":3,"harness":"h","kind":"rule","dest":{:?},"expected":"","displaced_at":0}}"#,
        d.holder.join("old.md")
    );
    write(&legacy.join("entry.json"), &meta);
    let ids = |list: Vec<displaced::Entry>| list.into_iter().map(|e| e.id).collect::<Vec<_>>();
    assert_eq!(ids(displaced::list().unwrap()), ["0000000000001-h-old.md", entry.id.as_str()]);

    // The index is lost: recovery lists the harness's entries again.
    fs::remove_dir_all(state.join(".index")).unwrap();
    assert_eq!(ids(displaced::list().unwrap()), ["0000000000001-h-old.md"]);

    let lock = sk1ll_core::EngineLock::acquire(d.repo.root()).unwrap();
    recover(Some(&lock), &d.harnesses).unwrap();
    assert_eq!(displaced::list().unwrap(), [displaced::get("0000000000001-h-old.md").unwrap(), entry.clone()]);

    // A record pointing anywhere else is never followed.
    let victim = d.home.path().join("victim");
    write(&victim.join("content"), "Not sk1ll's.\n");
    write(&victim.join("entry.json"), &meta);
    write(&state.join(".index/victim"), &serde_json::to_string(&victim).unwrap());
    assert!(displaced::get("victim").is_err() && displaced::discard("victim").is_err());
    assert!(victim.join("content").exists());

    displaced::discard(&entry.id).unwrap();
    displaced::discard("0000000000001-h-old.md").unwrap();
    assert!(!d.holder.join(TMP_DIR).exists(), "the empty area is tidied away");
    assert!(!state.join(".index").join(&entry.id).exists());
}

#[test]
fn recover_adopt_keeps_an_entry_whose_copy_left_something_out() {
    let d = Displacement::new("skill update");
    let _home = d.home.enter();
    write(&d.dest.join(".git/HEAD"), "git\n");
    let forced = plan(&d.repo, &d.harnesses, &d.assignment, PlanOptions { force: true }).unwrap();
    apply(&d.repo, &forced[0]).unwrap();
    let entry = d.entry();
    assert!(entry.changed);
    adopt_test_entry(&d.repo, &entry.id).unwrap();
    assert_eq!(fs::read_to_string(d.repo.personal_dir().join("pdf/SKILL.md")).unwrap(), "PDFs.\n");
    let kept = displaced::get(&entry.id).unwrap();
    assert!(kept.changed);
    assert_eq!(fs::read_to_string(kept.path.join(".git/HEAD")).unwrap(), "git\n");
    assert_eq!(fs::read_to_string(kept.path.join("SKILL.md")).unwrap(), "PDFs.\n");
    assert!(!displaced::prune_with(Duration::ZERO).unwrap().contains(&entry.id));
    assert!(kept.path.join(".git/HEAD").exists());
}

#[test]
fn pruning_never_deletes_recursively() {
    for case in ["new file", "new directory", "edited file", "renamed away"] {
        let d = Displacement::new("skill update");
        let _home = d.home.enter();
        apply(&d.repo, &d.action()).unwrap();
        let entry = d.entry();
        assert!(!entry.changed);
        let content = entry.path.clone();
        let outside = d.home.path().join("moved.md");
        let target = outside.clone();
        let pruned = with_hook(
            Point::HashBeforeRemoval,
            move |path| match case {
                "new file" => write(&path.join("new.md"), "New.\n"),
                "new directory" => write(&path.join("new/deeper.md"), "New.\n"),
                "edited file" => write(&path.join("SKILL.md"), "Edited.\n"),
                "renamed away" => fs::rename(path.join("SKILL.md"), &target).unwrap(),
                _ => unreachable!(),
            },
            || displaced::prune_with(Duration::ZERO),
        )
        .unwrap();
        match case {
            "renamed away" => {
                assert_eq!(pruned, std::slice::from_ref(&entry.id), "{case}");
                assert!(displaced::list().unwrap().is_empty(), "{case}");
                assert_eq!(fs::read_to_string(&outside).unwrap(), "PDFs.\n");
            }
            _ => {
                assert!(!pruned.contains(&entry.id), "{case}");
                assert!(displaced::get(&entry.id).unwrap().changed, "{case}");
                let kept = match case {
                    "new file" => ("new.md", "New.\n"),
                    "new directory" => ("new/deeper.md", "New.\n"),
                    _ => ("SKILL.md", "Edited.\n"),
                };
                assert_eq!(fs::read_to_string(content.join(kept.0)).unwrap(), kept.1, "{case}");
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn a_forced_update_of_an_edited_link_keeps_a_backup() {
    let (tmp, repo, harnesses) = skill_setup(LinkMode::Symlink);
    let _home = tmp.enter();
    let assignment = Assignment::default();
    apply(&repo, &plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap()[0]).unwrap();
    let dest = tmp.path().join("skills/pdf");
    let placed = fs::read_link(&dest).unwrap();
    fs::remove_file(&dest).unwrap();
    std::os::unix::fs::symlink("../repo/personal/pdf", &dest).unwrap();
    let normal = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    assert_eq!(normal[0].kind, ActionKind::Conflict, "{normal:?}");
    let forced = plan(&repo, &harnesses, &assignment, PlanOptions { force: true }).unwrap();
    assert_eq!(forced[0].kind, ActionKind::Update);
    apply(&repo, &forced[0]).unwrap();
    assert_eq!(fs::read_link(&dest).unwrap(), placed);
    let entry = displaced::list().unwrap().remove(0);
    assert!(entry.changed && entry.dest == dest, "{entry:?}");
    assert_eq!(fs::read_link(&entry.path).unwrap(), Path::new("../repo/personal/pdf"));
    assert!(plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn a_late_instruction_edit_can_be_adopted_from_recovery() {
    use std::io::Write;

    for (format, linked) in
        [(InstructionsFormat::Rules, false), (InstructionsFormat::Block, false), (InstructionsFormat::Block, true)]
    {
        let (home, repo, harnesses, path) = instructions_setup(format);
        let _home = home.enter();
        let file = if format == InstructionsFormat::Rules { path.join("git.md") } else { path.clone() };
        if linked {
            let target = home.path().join("dotfiles/AGENTS.md");
            write(&target, "# Dotfiles\n");
            std::os::unix::fs::symlink(target, &file).unwrap();
        }
        let assignment = Assignment::default();
        let actions = || plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        apply(&repo, &actions()[0]).unwrap();
        let rule = repo.root().join("instructions/git.md");
        edit_rule_body(&rule, "Updated rule.\n");

        let actual = file.canonicalize().unwrap();
        let mut open = fs::OpenOptions::new().append(true).open(&file).unwrap();
        let entry = reported(with_hook(
            Point::AfterSwap,
            move |_| open.write_all(b"Late note.\n").unwrap(),
            || apply(&repo, &actions()[0]),
        ));
        assert_eq!(entry.dest, file);
        assert!(entry.path.starts_with(displaced::area(actual.parent().unwrap())));
        let kept = sk1ll_testkit::read(&entry.path);
        assert!(kept.contains("Use CC.") && kept.ends_with("Late note.\n"));
        assert!(sk1ll_testkit::read(&file).contains("Updated rule."));
        assert!(displaced::prune_with(Duration::ZERO).unwrap().is_empty());

        adopt_test_entry(&repo, &entry.id).unwrap();
        assert!(displaced::find(&entry.id).unwrap().is_none());
        let adopted = if format == InstructionsFormat::Rules { rule } else { file };
        assert!(sk1ll_testkit::read(&adopted).ends_with("Late note.\n"));
        if linked {
            assert!(adopted.is_symlink());
            assert!(sk1ll_testkit::read(&adopted).starts_with("# Dotfiles\n"));
        }
    }
}

#[test]
fn adopting_recovered_skill_bytes_keeps_harness_only_sidecars() {
    use std::io::Write;

    for remove in [false, true] {
        let d = Displacement::new("skill update");
        let _home = d.home.enter();
        let id = skill_id(&d.repo, "pdf");
        write(&d.dest.join(ID_FILE), "harness-sidecar\n");
        if remove {
            d.repo.delete_item("skill", "pdf").unwrap();
        }
        let forced = plan(&d.repo, &d.harnesses, &d.assignment, PlanOptions { force: true }).unwrap();
        let mut open = fs::OpenOptions::new().append(true).open(&d.file).unwrap();
        let entry = reported(with_hook(
            Point::AfterAside,
            move |_| open.write_all(b"Late note.\n").unwrap(),
            || apply(&d.repo, &forced[0]),
        ));
        assert_eq!(entry.item_id.as_ref(), Some(&id));
        let name = if remove {
            "pdf"
        } else {
            d.repo.rename_item("skill", "pdf", "docs").unwrap();
            "docs"
        };
        let target = adopt_test_entry(&d.repo, &entry.id).unwrap();
        assert_eq!(target, d.repo.personal_dir().join(name));
        assert_eq!(skill_id(&d.repo, name), id);
        assert_eq!(sk1ll_testkit::read(&target.join("SKILL.md")), "PDFs.\nLate note.\n");
        let kept = displaced::get(&entry.id).unwrap();
        assert!(kept.changed);
        assert_eq!(sk1ll_testkit::read(&kept.path.join(ID_FILE)), "harness-sidecar\n");
    }
}

/// Recover Adopt of an unchanged copy past its grace period: the prune its
/// own swap runs leaves its input alone, and the Draft version it replaces,
/// an edit no other copy holds, is kept, changed, through any prune.
#[test]
fn recover_adopt_keeps_its_input_and_the_draft_edit_it_replaces() {
    let d = Displacement::new("skill update");
    let _home = d.home.enter();
    apply(&d.repo, &d.action()).unwrap();
    let entry = d.entry();
    assert!(!entry.changed);
    let original = fs::read_to_string(entry.path.join("SKILL.md")).unwrap();
    let meta = entry.path.with_file_name("entry.json");
    let mut json: serde_json::Value = serde_json::from_str(&fs::read_to_string(&meta).unwrap()).unwrap();
    json["displaced_at"] = 0.into();
    fs::write(&meta, json.to_string()).unwrap();
    let to = adopt_test_entry(&d.repo, &entry.id).unwrap();
    assert_eq!(fs::read_to_string(to.join("SKILL.md")).unwrap(), original);
    assert!(displaced::get(&entry.id).is_err());
    let draft = displaced::list().unwrap().into_iter().find(|e| e.harness == "repo").unwrap();
    assert!(draft.changed, "{draft:?}");
    assert!(displaced::prune_with(Duration::ZERO).unwrap().is_empty());
    assert!(fs::read_to_string(draft.path.join("SKILL.md")).unwrap().contains("Updated source."));
    // It goes back into the skill it was, by that skill's Item ID.
    assert_eq!(draft.item_id, Some(skill_id(&d.repo, "pdf")));
    assert!(displaced::adoptable(&d.repo, &draft));
    assert_eq!(adopt_test_entry(&d.repo, &draft.id).unwrap(), to);
    assert!(fs::read_to_string(to.join("SKILL.md")).unwrap().contains("Updated source."));
}

/// Adopt holds its entry for as long as its process runs: a hold a crashed
/// Adopt left keeps nothing from being pruned, adopted or discarded.
#[cfg(unix)]
#[test]
fn a_hold_a_crashed_adopt_left_lapses() {
    let d = Displacement::new("skill update");
    let _home = d.home.enter();
    apply(&d.repo, &d.action()).unwrap();
    let entry = d.entry();
    let meta = entry.path.with_file_name("entry.json");
    let hold = |pid: u32| {
        let mut json: serde_json::Value = serde_json::from_str(&fs::read_to_string(&meta).unwrap()).unwrap();
        json["held_by"] = pid.into();
        fs::write(&meta, json.to_string()).unwrap();
    };
    hold(std::process::id());
    assert!(displaced::prune_with(Duration::ZERO).unwrap().is_empty(), "held by a running process");
    let mut gone = std::process::Command::new("true").spawn().unwrap();
    let pid = gone.id();
    gone.wait().unwrap();
    hold(pid);
    assert_eq!(displaced::prune_with(Duration::ZERO).unwrap(), std::slice::from_ref(&entry.id));
}

/// A rule Adopt can't read as UTF-8 is refused, and stays as it was: its
/// bytes would not survive as text.
#[test]
fn recover_adopt_refuses_a_rule_that_isnt_utf8() {
    let d = Displacement::new("rule update");
    let _home = d.home.enter();
    let bytes = b"---\ndescription: Git\n---\nCaf\xe9 au lait.\n";
    fs::write(&d.dest, bytes).unwrap();
    let source = d.repo.root().join("instructions/git.md");
    for action in plan(&d.repo, &d.harnesses, &d.assignment, PlanOptions { force: true }).unwrap() {
        apply(&d.repo, &action).unwrap();
    }
    let entry = d.entry();
    let before = fs::read(&source).unwrap();
    let error = adopt_test_entry(&d.repo, &entry.id).unwrap_err();
    assert!(error.to_string().contains("isn't UTF-8"), "{error}");
    assert_eq!(fs::read(&entry.path).unwrap(), bytes);
    assert!(displaced::get(&entry.id).unwrap().changed);
    assert_eq!(fs::read(&source).unwrap(), before);
}

/// A forced sync over a hand edit keeps the edit, changed and marked
/// overwritten: the user chose that, so it needs no one. A write to the
/// kept copy since makes it a change like any other.
#[test]
fn a_forced_overwrite_keeps_the_edit_marked_overwritten() {
    for case in ["skill update", "rule update", "block update"] {
        let d = Displacement::new(case);
        let _home = d.home.enter();
        let text = fs::read_to_string(&d.file).unwrap();
        let edited = if text.contains("Use CC.") {
            text.replace("Use CC.", "Use CC by hand.")
        } else {
            format!("{text}Hand edit.\n")
        };
        fs::write(&d.file, &edited).unwrap();
        let normal = plan(&d.repo, &d.harnesses, &d.assignment, PlanOptions::default()).unwrap();
        assert!(normal.iter().any(|a| a.kind == ActionKind::Conflict), "{case}: {normal:?}");
        for action in plan(&d.repo, &d.harnesses, &d.assignment, PlanOptions { force: true }).unwrap() {
            apply(&d.repo, &action).unwrap();
        }
        let entry = d.entry();
        assert!(entry.changed && entry.overwritten, "{case}: {entry:?}");
        assert!(displaced::prune_with(Duration::ZERO).unwrap().is_empty(), "{case}");
        let kept = d.kept_file(&entry);
        fs::write(&kept, format!("{edited}Written later.\n")).unwrap();
        let entry = d.entry();
        assert!(entry.changed && !entry.overwritten, "{case}: {entry:?}");
    }
}
