mod sync_common;

use std::{collections::BTreeMap, fs, path::Path, time::Duration};

use sk1ll_core::failpoints::{self, Point};
use sk1ll_core::hash::{HashCache, hash_dir};
use sk1ll_core::skill::ID_FILE;
use sk1ll_core::sync::*;
use sk1ll_core::{Assignment, Error, Harness, InstructionsFormat, ItemId, LinkMode, Repo};
use sk1ll_testkit::{Home, write};

use sync_common::*;

#[test]
fn a_cached_plan_rereads_a_changed_placement() {
    let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
    let _home = tmp.enter();
    let assignment = Assignment::default();
    apply(&repo, &plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap()[0]).unwrap();
    let cache = HashCache::with_racy_window(Duration::ZERO);
    let cached =
        |assignment: &Assignment| plan_cached(&repo, &harnesses, assignment, PlanOptions::default(), &cache).unwrap();
    assert!(cached(&assignment).is_empty());
    let read = cache.hashed();
    assert_eq!(read, 2, "the source and the placement");
    assert!(cached(&assignment).is_empty());
    assert_eq!(cache.hashed(), read, "nothing changed: nothing read");

    let dest = tmp.path().join("skills/pdf");
    std::thread::sleep(Duration::from_millis(30));
    write(&dest.join("SKILL.md"), "Edited in the harness.\n");
    let actions = cached(&assignment);
    assert_eq!(cache.hashed(), read + 1);
    assert_eq!(actions.iter().map(|a| a.kind).collect::<Vec<_>>(), [ActionKind::Conflict]);
}

#[test]
fn the_harness_fingerprint_follows_what_planning_reads() {
    let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
    let _home = tmp.enter();
    let harness = &harnesses["h"];
    let before = harness_fingerprint(harness).unwrap();
    assert_eq!(harness_fingerprint(harness).unwrap(), before);
    apply(&repo, &plan(&repo, &harnesses, &Assignment::default(), PlanOptions::default()).unwrap()[0]).unwrap();
    let placed = harness_fingerprint(harness).unwrap();
    assert_ne!(placed, before);
    write(&tmp.path().join("skills/mine/SKILL.md"), "Mine.\n");
    let unmanaged = harness_fingerprint(harness).unwrap();
    assert_ne!(unmanaged, placed, "a new entry of its own");
    write(&tmp.path().join("skills/mine/SKILL.md"), "Edited, unmanaged.\n");
    assert_eq!(harness_fingerprint(harness).unwrap(), unmanaged, "not looked into");
    std::thread::sleep(Duration::from_millis(30));
    write(&tmp.path().join("skills/pdf/SKILL.md"), "Edited, managed.\n");
    assert_ne!(harness_fingerprint(harness).unwrap(), unmanaged);
}

#[cfg(unix)]
#[test]
fn the_harness_fingerprint_reads_through_links_and_covers_settings_and_recovery() {
    use std::os::unix::fs::symlink;
    let (tmp, _repo, harnesses) = skill_setup(LinkMode::Copy);
    let _home = tmp.enter();
    let dotfiles = tmp.path().join("dotfiles");
    fs::create_dir_all(dotfiles.join("skills")).unwrap();
    symlink(dotfiles.join("skills"), tmp.path().join("skills")).unwrap();
    write(&dotfiles.join("AGENTS.md"), "Mine.\n");
    let codex = tmp.path().join("codex");
    fs::create_dir_all(&codex).unwrap();
    symlink(dotfiles.join("AGENTS.md"), codex.join("AGENTS.md")).unwrap();
    let mut harness = harnesses["h"].clone();
    harness.instructions = Some(codex.join("AGENTS.md").to_string_lossy().into_owned());
    let mut last = harness_fingerprint(&harness).unwrap();
    type Change<'a> = Box<dyn Fn(&mut Harness) + 'a>;
    let changes: [(&str, Change); 5] = [
        ("an entry of the linked skills directory", Box::new(|_| write(&dotfiles.join("skills/mine/x"), "x"))),
        ("the linked instructions file's target", Box::new(|_| write(&dotfiles.join("AGENTS.md"), "Edited.\n"))),
        ("recovery beside the link", Box::new(|_| write(&codex.join(".sk1ll-tmp/displaced/a/content"), "a"))),
        ("recovery beside the target", Box::new(|_| write(&dotfiles.join(".sk1ll-tmp/displaced/b/content"), "b"))),
        ("the placement mode", Box::new(|h| h.mode = LinkMode::Symlink)),
    ];
    for (what, change) in changes {
        std::thread::sleep(Duration::from_millis(30));
        change(&mut harness);
        let now = harness_fingerprint(&harness).unwrap();
        assert_ne!(now, last, "{what}");
        assert!(!now.settled(std::time::SystemTime::now(), Duration::from_secs(3600)), "{what}: just changed");
        last = now;
    }
    assert!(last.settled(std::time::SystemTime::now(), Duration::ZERO));
    assert_eq!(harness_fingerprint(&harness).unwrap(), last);
}

#[test]
fn a_hand_edit_conflicts_even_when_it_matches_the_source() {
    let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
    let _home = tmp.enter();
    let assignment = Assignment::default();
    apply(&repo, &plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap()[0]).unwrap();
    let dest = tmp.path().join("skills/pdf/SKILL.md");
    write(&dest, "Edited in the harness.\n");
    for source in ["PDFs.\n", "Edited in the harness.\n"] {
        write(&repo.personal_dir().join("pdf/SKILL.md"), source);
        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].kind, ActionKind::Conflict);
        apply(&repo, &actions[0]).unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "Edited in the harness.\n");
    }
}

#[test]
fn an_unindexed_copy_is_unmanaged_and_never_removed() {
    let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
    let _home = tmp.enter();
    let mut assignment = Assignment::default();
    write(&repo.personal_dir().join("notes/SKILL.md"), "Notes.\n");
    for action in plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap() {
        apply(&repo, &action).unwrap();
    }
    let dir = tmp.path().join("skills");
    let mut index = read_index(&dir);
    assert!(index["entries"].as_object_mut().unwrap().remove("pdf").is_some());
    assert!(index["entries"].as_object_mut().unwrap().contains_key("notes"));
    replace_index(&dir, &index);
    assert!(unmanaged::discover(&repo, &harnesses).unwrap().is_empty());
    let blocked = unmanaged::select(&repo, &harnesses, "h", "pdf").unwrap_err();
    assert!(blocked.to_string().contains(PLACEMENT_MARKER));
    let placements = placed(&repo, "h", &harnesses["h"]).unwrap();
    assert_eq!(placements.len(), 1);
    assert_eq!(placements[0].dest, dir.join("notes"));
    let opts = PlanOptions { force: true };
    let actions = plan(&repo, &harnesses, &assignment, opts).unwrap();
    assert_eq!(actions[0].kind, ActionKind::Conflict);
    assignment.set_skill("h", &skill_id(&repo, "pdf"), false);
    assert!(plan(&repo, &harnesses, &assignment, opts).unwrap().is_empty());
    assert_eq!(fs::read_to_string(dir.join("pdf/SKILL.md")).unwrap(), "PDFs.\n");
}

#[test]
fn a_copy_replaced_between_syncs_or_after_an_interrupted_removal_stays_unmanaged() {
    for interrupted in [false, true] {
        for newcomer in ["PDFs.\n", "My skill.\n"] {
            let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
            let _home = tmp.enter();
            let mut assignment = Assignment::default();
            let initial = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
            apply(&repo, &initial[0]).unwrap();
            let dest = &initial[0].dest;
            let dir = dest.parent().unwrap();
            assignment.set_skill("h", &skill_id(&repo, "pdf"), false);
            let removal = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
            assert_eq!(removal[0].kind, ActionKind::Remove);
            if interrupted {
                interrupt(&repo, &removal[0], Point::AfterSwap);
                assert!(matches!(read_index(dir)["pending"]["pdf"]["op"].as_str(), Some("remove")));
            } else {
                fs::rename(dest, tmp.path().join("moved")).unwrap();
            }
            write(&dest.join("SKILL.md"), newcomer);
            assert!(apply(&repo, &removal[0]).is_err(), "the old action is stale");
            let off = plan(&repo, &harnesses, &assignment, PlanOptions { force: true }).unwrap();
            assert!(off.is_empty(), "{off:?}");
            assert_eq!(unmanaged::discover(&repo, &harnesses).unwrap()[0].name, "pdf");
            recover(None, &harnesses).unwrap();
            if interrupted {
                assert!(read_index(dir) == empty_index());
                assert_eq!(displaced::list().unwrap().len(), 1, "only the original was displaced");
            }
            assignment.set_skill("h", &skill_id(&repo, "pdf"), true);
            for force in [false, true] {
                let actions = plan(&repo, &harnesses, &assignment, PlanOptions { force }).unwrap();
                assert_eq!(actions.len(), 1);
                assert_eq!(actions[0].kind, ActionKind::Conflict);
                apply(&repo, &actions[0]).unwrap();
            }
            displaced::prune_with(Duration::ZERO, u64::MAX).unwrap();
            assert_eq!(fs::read_to_string(dest.join("SKILL.md")).unwrap(), newcomer);
            assert!(!dest.join(PLACEMENT_MARKER).exists());
        }
    }
}

#[test]
fn a_skill_copies_nested_state_and_detects_edits_to_it() {
    let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
    let _home = tmp.enter();
    let assignment = Assignment::default();
    let path = "x/.sk1ll-state/f";
    write(&repo.personal_dir().join("pdf").join(path), "User content.\n");
    let initial = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    apply(&repo, &initial[0]).unwrap();
    let file = initial[0].dest.join(path);
    assert_eq!(fs::read_to_string(&file).unwrap(), "User content.\n");
    write(&file, "Edited by hand.\n");
    let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].kind, ActionKind::Conflict);
}

#[test]
fn source_placement_marker_is_a_conflict_even_with_force() {
    let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
    let _home = tmp.enter();
    let marker = repo.personal_dir().join("pdf").join(PLACEMENT_MARKER);
    let assignment = Assignment::default();
    let opts = PlanOptions { force: true };
    let planned = plan(&repo, &harnesses, &assignment, opts).unwrap();
    write(&marker, "user content\n");
    let error = apply(&repo, &planned[0]).unwrap_err();
    assert!(error.to_string().contains(PLACEMENT_MARKER));
    let conflicts = plan(&repo, &harnesses, &assignment, opts).unwrap();
    assert_eq!(conflicts[0].kind, ActionKind::Conflict);
    assert!(conflicts[0].reason.as_ref().unwrap().contains(PLACEMENT_MARKER));
    assert!(!conflicts[0].dest.exists());
    assert_eq!(fs::read_to_string(marker).unwrap(), "user content\n");
}

#[test]
fn a_clone_with_the_same_marker_bytes_stays_unmanaged() {
    for interrupted in [false, true] {
        let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
        let _home = tmp.enter();
        let mut assignment = Assignment::default();
        let initial = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        if interrupted {
            interrupt(&repo, &initial[0], Point::AfterSwap);
        } else {
            apply(&repo, &initial[0]).unwrap();
        }
        let dest = &initial[0].dest;
        let original = tmp.path().join("original");
        fs::rename(dest, &original).unwrap();
        copy_test_skill(&original, dest).unwrap();
        fs::copy(original.join(PLACEMENT_MARKER), dest.join(PLACEMENT_MARKER)).unwrap();
        assert_eq!(fs::read(original.join(PLACEMENT_MARKER)).unwrap(), fs::read(dest.join(PLACEMENT_MARKER)).unwrap());
        assert_eq!(hash_dir(&original).unwrap(), hash_dir(dest).unwrap());
        let view = |assignment: &Assignment, force| plan(&repo, &harnesses, assignment, PlanOptions { force }).unwrap();
        for force in [false, true] {
            assert_eq!(view(&assignment, force)[0].kind, ActionKind::Conflict);
        }
        assignment.set_skill("h", &skill_id(&repo, "pdf"), false);
        assert!(view(&assignment, true).is_empty());
        recover(None, &harnesses).unwrap();
        assert!(view(&assignment, true).is_empty());
        let error = unmanaged::select(&repo, &harnesses, "h", "pdf").unwrap_err();
        assert!(error.to_string().contains(PLACEMENT_MARKER), "{error}");
        assert!(dest.join("SKILL.md").is_file());
    }
}

#[cfg(unix)]
#[test]
fn rule_anchor_keeps_the_old_inode_alive_after_the_live_name_is_removed() {
    let (tmp, repo, harnesses, dir) = instructions_setup(InstructionsFormat::Rules);
    let _home = tmp.enter();
    let mut assignment = Assignment::default();
    let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    let dest = &actions[0].dest;
    apply(&repo, &actions[0]).unwrap();
    let identity = inode(dest);
    let anchor = dir.join(INDEX_DIR).join("anchors").join(format!("rule-{}-{}", identity.0, identity.1));
    fs::remove_file(dest).unwrap();
    assert_eq!(inode(&anchor), identity);
    write(dest, "Use CC.\n");
    assert_ne!(inode(dest), identity);
    assert_eq!(
        plan(&repo, &harnesses, &assignment, PlanOptions { force: true }).unwrap()[0].kind,
        ActionKind::Conflict
    );
    assignment.set_rule("h", &rule_id(&repo, "git"), false);
    assert!(plan(&repo, &harnesses, &assignment, PlanOptions { force: true }).unwrap().is_empty());
    assert_eq!(fs::read_to_string(dest).unwrap(), "Use CC.\n");
}

#[test]
fn toggles_follow_item_ids_through_renames_and_recreation() {
    let tmp = Home::new();
    let _home = tmp.enter();
    let repo = Repo::init(&tmp.path().join("repo")).unwrap();
    repo.create_item("skill", "made", "").unwrap();
    repo.create_item("rule", "made", "").unwrap();
    edit_rule_body(&repo.root().join("instructions/made.md"), "Made.\n");
    write(&repo.personal_dir().join("hand/SKILL.md"), "Hand-made.\n");
    repo.assign_missing_ids().unwrap();
    let harness = Harness {
        skills: Some(tmp.path().join("skills").to_string_lossy().into_owned()),
        instructions: Some(tmp.path().join("AGENTS.md").to_string_lossy().into_owned()),
        ..Harness::default()
    };
    let harnesses = BTreeMap::from([("h".to_string(), harness)]);
    let mut assignment = Assignment::default();
    for (item, id) in Repo::open(repo.root()).unwrap().item_ids().unwrap() {
        let id = id.expect("every item has an Item ID");
        match item {
            sk1ll_core::ItemRef::Skill(_) => assignment.set_skill("h", &id, false),
            _ => assignment.set_rule("h", &id, false),
        }
    }
    let planned = |repo: &Repo| -> Vec<String> {
        let repo = Repo::open(repo.root()).unwrap();
        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        let mut names: Vec<String> = actions
            .iter()
            .flat_map(|a| match &a.item {
                Item::Skill { name, .. } => vec![name.clone()],
                Item::Instructions { rules, .. } => rules.clone(),
                Item::Rule { file, .. } => vec![file.clone()],
            })
            .collect();
        names.sort();
        names
    };
    assert!(planned(&repo).is_empty());

    repo.rename_item("skill", "made", "renamed").unwrap();
    repo.rename_item("rule", "made", "renamed").unwrap();
    repo.rename_item("skill", "hand", "handier").unwrap();
    assert!(planned(&repo).is_empty(), "renamed items keep their toggles");

    repo.delete_item("skill", "renamed").unwrap();
    repo.delete_item("rule", "renamed").unwrap();
    repo.create_item("skill", "renamed", "").unwrap();
    repo.create_item("rule", "renamed", "").unwrap();
    edit_rule_body(&repo.root().join("instructions/renamed.md"), "Made again.\n");
    write(&repo.personal_dir().join("hand/SKILL.md"), "Hand-made again.\n");
    assert_eq!(planned(&repo), ["hand", "renamed", "renamed"], "an item without an ID has the defaults");
    repo.assign_missing_ids().unwrap();
    assert_eq!(planned(&repo), ["hand", "renamed", "renamed"], "new ids start from the defaults");
}

#[test]
#[ignore = "sync planning benchmark"]
fn sync_plan_no_change() {
    let tmp = Home::new();
    let _home = tmp.enter();
    let repo = Repo::init(&tmp.path().join("repo")).unwrap();
    for i in 0..200 {
        let skill = repo.personal_dir().join(format!("skill-{i}"));
        write(&skill.join("SKILL.md"), "A skill.\n");
        write(&skill.join("references/guide.md"), "Reference material.\n");
        write(&skill.join("examples/example.md"), "An example.\n");
    }
    let harnesses = (0..4)
        .map(|i| {
            let name = format!("harness-{i}");
            let harness = Harness {
                skills: Some(tmp.path().join(&name).join("skills").to_string_lossy().into_owned()),
                mode: LinkMode::Copy,
                ..Harness::default()
            };
            (name, harness)
        })
        .collect();
    let assignment = Assignment::default();
    let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    assert_eq!(actions.len(), 800);
    for action in &actions {
        apply(&repo, action).unwrap();
    }
    let start = std::time::Instant::now();
    let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    let elapsed = start.elapsed();
    assert!(actions.is_empty(), "{actions:?}");
    println!("No-change sync plan for 200 skills across 4 copy-mode harnesses: {elapsed:?}");
}

#[test]
fn skill_id_is_ignored_in_hashes_and_copies_but_visible_through_links() {
    for mode in [LinkMode::Copy, LinkMode::Symlink] {
        let (tmp, repo, harnesses) = skill_setup(mode);
        let _home = tmp.enter();
        let src = repo.personal_dir().join("pdf");
        let hash = hash_dir(&src).unwrap();
        write(&src.join(ID_FILE), "new-id\n");
        assert_eq!(hash_dir(&src).unwrap(), hash);
        let plan_now = || plan(&repo, &harnesses, &Assignment::default(), PlanOptions::default()).unwrap();
        let actions = plan_now();
        apply(&repo, &actions[0]).unwrap();
        assert_eq!(actions[0].dest.join(ID_FILE).is_file(), mode == LinkMode::Symlink);
        write(&src.join(ID_FILE), "newer-id\n");
        let repair = plan_now().remove(0);
        assert!(matches!(repair.item, Item::Skill { metadata_only: true, .. }));
        apply(&repo, &repair).unwrap();
        assert!(plan_now().is_empty());
        assert!(displaced::list().unwrap().iter().all(|entry| entry.dest != repair.dest));
    }
}

#[test]
fn one_plan_uses_the_same_source_observation_for_every_harness() {
    let (tmp, repo, mut harnesses) = skill_setup(LinkMode::Copy);
    let _home = tmp.enter();
    let second = Harness {
        skills: Some(tmp.path().join("skills2").to_string_lossy().into_owned()),
        mode: LinkMode::Copy,
        ..Harness::default()
    };
    harnesses.insert("i".to_string(), second);
    let assignment = Assignment::default();
    for action in plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap() {
        apply(&repo, &action).unwrap();
    }
    // The source changes after the first harness was compared with it:
    // the second is judged against the same observation, not a newer one.
    let source = repo.personal_dir().join("pdf/SKILL.md");
    let mut calls = 0;
    let hook = failpoints::observe(move |at, _| {
        calls += usize::from(at == Point::PlanSource);
        if calls == 2 {
            write(&source, "Updated source.\n");
        }
    });
    let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default());
    drop(hook);
    assert!(actions.unwrap().is_empty());
    // The next plan sees the edit, for both harnesses.
    let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    assert_eq!(
        actions.iter().map(|a| (a.harness.as_str(), a.kind)).collect::<Vec<_>>(),
        [("h", ActionKind::Update), ("i", ActionKind::Update)]
    );
}

#[test]
fn reviewed_copies_reject_source_edits_before_and_during_staging() {
    for point in [Point::BeforeCopy, Point::CopyEntry] {
        let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
        let _home = tmp.enter();
        let src = repo.personal_dir().join("pdf");
        let file = src.join("SKILL.md");
        let actions = plan(&repo, &harnesses, &Assignment::default(), PlanOptions::default()).unwrap();
        let action = &actions[0];
        let digest = hash_dir(&src).unwrap();
        let error = with_hooks(
            move |at, path| {
                if at == point && path == if point == Point::BeforeCopy { &src } else { &file } {
                    write(&file, "Edited since review.\n");
                }
            },
            || apply_reviewed(&repo, action, Some(&digest)),
        )
        .unwrap_err();
        assert!(matches!(error, Error::Changed(_)), "{point:?}: {error}");
        assert!(!action.dest.exists());
        apply(&repo, action).unwrap();
        assert_copy_recorded(&action.dest);
        let mut names: Vec<_> = fs::read_dir(&action.dest).unwrap().map(|e| e.unwrap().file_name()).collect();
        names.sort();
        assert_eq!(names, [PLACEMENT_MARKER, "SKILL.md"]);
    }
}

#[test]
fn stale_skill_plans_preserve_harness_edits() {
    for remove in [false, true] {
        for force in [false, true] {
            let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
            let _home = tmp.enter();
            let mut assignment = Assignment::default();
            let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
            apply(&repo, &actions[0]).unwrap();
            write(&repo.personal_dir().join("pdf/SKILL.md"), "Updated source.\n");
            if remove {
                assignment.set_skill("h", &skill_id(&repo, "pdf"), false);
            }
            let actions = plan(&repo, &harnesses, &assignment, PlanOptions { force }).unwrap();
            assert_eq!(actions[0].kind, if remove { ActionKind::Remove } else { ActionKind::Update });
            let dest = tmp.path().join("skills/pdf");
            write(&dest.join("SKILL.md"), "Late harness edit.\n");
            let before = hash_dir(&dest).unwrap();
            let path = dest.parent().unwrap().join(INDEX_PATH);
            let index = fs::read(&path).unwrap();
            assert!(apply(&repo, &actions[0]).is_err());
            assert_eq!(hash_dir(&dest).unwrap(), before);
            assert_eq!(fs::read(path).unwrap(), index);
        }
    }
}

#[test]
fn an_emptied_block_file_goes_only_if_sync_created_it_and_nobody_touched_it() {
    for case in [
        "created",
        "preexisting",
        "whitespace",
        "outside text",
        #[cfg(unix)]
        "replaced",
        #[cfg(unix)]
        "mode",
    ] {
        let (home, repo, harnesses, dest) = instructions_setup(InstructionsFormat::Block);
        let _home = home.enter();
        let id = rule_id(&repo, "git");
        let toggle = |on| {
            let mut assignment = Assignment::default();
            assignment.set_rule("h", &id, on);
            for action in plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap() {
                apply(&repo, &action).unwrap();
            }
        };
        if case == "preexisting" {
            write(&dest, "");
        }
        toggle(true);
        let original = sk1ll_testkit::read(&dest);
        assert!(original.contains("Use CC."));
        match case {
            "whitespace" => write(&dest, &format!("\n{original}")),
            "outside text" => write(&dest, &format!("# Mine\n\n{original}")),
            #[cfg(unix)]
            "replaced" => {
                let save = home.path().join("editor");
                write(&save, &original);
                fs::rename(save, &dest).unwrap();
            }
            #[cfg(unix)]
            "mode" => {
                use std::os::unix::fs::PermissionsExt;
                let mode = dest.metadata().unwrap().permissions().mode();
                fs::set_permissions(&dest, fs::Permissions::from_mode(mode ^ 0o040)).unwrap();
            }
            _ => {}
        }
        toggle(false);
        let entries = displaced::list().unwrap();
        if case == "created" {
            assert!(!dest.exists());
            assert!(entries.len() == 1 && entries[0].dest == dest && !entries[0].changed, "{entries:?}");
        } else {
            assert_eq!(sk1ll_testkit::read(&dest).trim(), if case == "outside text" { "# Mine" } else { "" }, "{case}");
            assert!(entries.is_empty(), "{case}: {entries:?}");
            toggle(true);
            toggle(false);
            assert!(dest.exists(), "{case}: once the user's, always the user's");
        }
    }
}

#[cfg(unix)]
#[test]
fn a_created_block_file_swapped_during_its_removal_goes_back() {
    let (tmp, repo, harnesses, dest) = instructions_setup(InstructionsFormat::Block);
    let _home = tmp.enter();
    let mut assignment = Assignment::default();
    apply(&repo, &plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap()[0]).unwrap();
    let text = fs::read_to_string(&dest).unwrap();
    assignment.set_rule("h", &rule_id(&repo, "git"), false);
    let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    let swap = {
        let (dest, mine) = (dest.clone(), tmp.path().join("mine.md"));
        move |_: &Path| {
            write(&mine, &text);
            fs::rename(&mine, &dest).unwrap();
        }
    };
    assert!(with_hook(Point::BeforeSwap, swap, || apply(&repo, &actions[0])).is_err());
    assert!(dest.exists(), "put back");
    assert!(displaced::list().unwrap().is_empty(), "{:?}", displaced::list().unwrap());
}

#[test]
fn absent_rule_is_omitted_from_placements_but_removed_from_index() {
    let (tmp, repo, harnesses, dir) = instructions_setup(InstructionsFormat::Rules);
    let _home = tmp.enter();
    let mut assignment = Assignment::default();
    let initial = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    apply(&repo, &initial[0]).unwrap();
    fs::remove_file(dir.join("git.md")).unwrap();
    assert!(placed(&repo, "h", &harnesses["h"]).unwrap().is_empty());
    assignment.set_rule("h", &rule_id(&repo, "git"), false);
    let cleanup = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    assert_eq!(cleanup[0].kind, ActionKind::Remove);
    apply(&repo, &cleanup[0]).unwrap();
    assert!(read_index(&dir) == empty_index());
}

#[cfg(unix)]
#[test]
fn a_rule_file_that_isnt_a_plain_file_is_a_conflict() {
    let (tmp, repo, harnesses, dest) = instructions_setup(InstructionsFormat::Rules);
    let _home = tmp.enter();
    let assignment = Assignment::default();
    apply(&repo, &plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap()[0]).unwrap();
    let rule = dest.join("git.md");
    let text = fs::read_to_string(&rule).unwrap();
    let mine = tmp.path().join("dotfiles/git.md");
    write(&mine, &text);
    fs::remove_file(&rule).unwrap();
    std::os::unix::fs::symlink(&mine, &rule).unwrap();
    edit_rule_body(&repo.root().join("instructions/git.md"), "Use CC always.\n");
    let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    assert_eq!(actions.iter().map(|a| a.kind).collect::<Vec<_>>(), [ActionKind::Conflict], "{actions:?}");
    assert!(fs::symlink_metadata(&rule).unwrap().file_type().is_symlink());
    assert_eq!(fs::read_to_string(&mine).unwrap(), text);

    fs::remove_file(&rule).unwrap();
    let c = std::ffi::CString::new(rule.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    assert_eq!(actions.iter().map(|a| a.kind).collect::<Vec<_>>(), [ActionKind::Conflict], "{actions:?}");
    assert!(actions[0].reason.as_deref().is_some_and(|r| r.contains("not a plain file")), "{actions:?}");
}

#[cfg(unix)]
#[test]
fn stale_skill_plan_preserves_retargeted_link() {
    let (tmp, repo, harnesses) = skill_setup(LinkMode::Symlink);
    let _home = tmp.enter();
    let mut assignment = Assignment::default();
    let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    apply(&repo, &actions[0]).unwrap();
    let dir = tmp.path().join("skills");
    let index = fs::read(dir.join(INDEX_PATH)).unwrap();
    assignment.set_skill("h", &skill_id(&repo, "pdf"), false);
    let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    fs::remove_file(dir.join("pdf")).unwrap();
    // Same canonical target and ownership, but a different observed link.
    std::os::unix::fs::symlink("../repo/personal/pdf", dir.join("pdf")).unwrap();
    assert!(apply(&repo, &actions[0]).is_err());
    assert_eq!(fs::read_link(dir.join("pdf")).unwrap(), Path::new("../repo/personal/pdf"));
    assert_eq!(fs::read(dir.join(INDEX_PATH)).unwrap(), index);
}

#[cfg(unix)]
#[test]
fn unsupported_copy_entries_leave_existing_content_untouched() {
    let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
    let _home = tmp.enter();
    let assignment = Assignment::default();
    let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    apply(&repo, &actions[0]).unwrap();
    write(&repo.personal_dir().join("pdf/SKILL.md"), "Changed.\n");
    let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    let socket = std::os::unix::net::UnixListener::bind(repo.personal_dir().join("pdf/socket")).unwrap();
    let dest = tmp.path().join("skills/pdf");
    let before = hash_dir(&dest).unwrap();
    assert!(apply(&repo, &actions[0]).is_err());
    assert_eq!(hash_dir(&dest).unwrap(), before);
    assert!(!dest.join("socket").exists());
    drop(socket);
}

#[cfg(unix)]
#[test]
fn indexed_links_are_removed_when_repo_skill_is_deleted() {
    let (tmp, repo, harnesses) = skill_setup(LinkMode::Symlink);
    let _home = tmp.enter();
    let assignment = Assignment::default();
    let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    apply(&repo, &actions[0]).unwrap();
    assert!(plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap().is_empty());
    fs::remove_dir_all(repo.personal_dir().join("pdf")).unwrap();
    let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].kind, ActionKind::Remove);
    apply(&repo, &actions[0]).unwrap();
    assert!(!tmp.path().join("skills/pdf").is_symlink());
    assert!(!tmp.path().join("skills").join(INDEX_PATH).exists());
}

#[cfg(unix)]
#[test]
fn migrated_relative_links_are_removed_after_source_deletion_but_unindexed_links_remain() {
    let (tmp, repo, harnesses) = skill_setup(LinkMode::Symlink);
    let _home = tmp.enter();
    let assignment = Assignment::default();
    let dir = tmp.path().join("skills");
    fs::create_dir(&dir).unwrap();
    let target = Path::new("../repo/personal/../personal/./pdf");
    std::os::unix::fs::symlink(target, dir.join("pdf")).unwrap();
    let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].kind, ActionKind::Update);
    apply(&repo, &actions[0]).unwrap();
    assert_eq!(fs::read_link(dir.join("pdf")).unwrap(), target);
    assert!(dir.join(INDEX_PATH).is_file());
    assert!(plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap().is_empty());

    fs::remove_dir_all(repo.personal_dir().join("pdf")).unwrap();
    std::os::unix::fs::symlink(target, dir.join("manual")).unwrap();
    let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    let managed = actions.iter().find(|a| a.dest == dir.join("pdf")).unwrap();
    assert_eq!(managed.kind, ActionKind::Remove);
    let manual = actions.iter().find(|a| a.dest == dir.join("manual")).unwrap();
    assert_eq!(manual.kind, ActionKind::Conflict);
    for action in &actions {
        apply(&repo, action).unwrap();
    }
    assert!(!dir.join("pdf").is_symlink());
    assert!(!dir.join(INDEX_PATH).exists());
    assert_eq!(fs::read_link(dir.join("manual")).unwrap(), target);
}

#[cfg(unix)]
#[test]
fn copied_and_adopted_skills_preserve_symlinks() {
    let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
    let _home = tmp.enter();
    let src = repo.personal_dir().join("pdf");
    write(&src.join("data"), "data\n");
    fs::create_dir(src.join("nested")).unwrap();
    for (name, target) in [("link", "data"), ("dir-link", "nested"), ("dangling", "missing")] {
        std::os::unix::fs::symlink(target, src.join(name)).unwrap();
    }
    let assignment = Assignment::default();
    let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    apply(&repo, &actions[0]).unwrap();
    let dest = tmp.path().join("skills/pdf");
    assert_eq!(fs::read_link(dest.join("link")).unwrap(), Path::new("data"));
    assert_eq!(hash_dir(&src).unwrap(), hash_dir(&dest).unwrap());
    assert!(plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap().is_empty());
    write(&dest.join("SKILL.md"), "Adopted.\n");
    let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    adopt(&repo, &actions[0]).unwrap();
    for (name, target) in [("link", "data"), ("dir-link", "nested"), ("dangling", "missing")] {
        assert_eq!(fs::read_link(src.join(name)).unwrap(), Path::new(target));
    }
    assert_eq!(fs::read_to_string(src.join("data")).unwrap(), "data\n");
    assert!(plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn handmade_repo_links_are_unmanaged_unless_exactly_assigned() {
    let (tmp, repo, harnesses) = skill_setup(LinkMode::Symlink);
    let _home = tmp.enter();
    let dir = tmp.path().join("skills");
    fs::create_dir(&dir).unwrap();
    std::os::unix::fs::symlink(repo.personal_dir().join("pdf"), dir.join("manual")).unwrap();
    let mut assignment = Assignment::default();
    assignment.set_skill("h", &skill_id(&repo, "pdf"), false);
    let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].kind, ActionKind::Conflict);
    apply(&repo, &actions[0]).unwrap();
    assert!(dir.join("manual").is_symlink());

    // Legacy links are adopted only for the exact, currently assigned skill.
    std::os::unix::fs::symlink("../repo/personal/./pdf", dir.join("pdf")).unwrap();
    assert!(unmanaged::discover(&repo, &harnesses).unwrap().iter().any(|item| item.name == "pdf"));
    assert!(placed(&repo, "h", &harnesses["h"]).unwrap().is_empty());
    assignment.set_skill("h", &skill_id(&repo, "pdf"), true);
    let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    let action = actions.iter().find(|a| a.dest == dir.join("pdf")).unwrap();
    assert_eq!(action.kind, ActionKind::Update);
    apply(&repo, action).unwrap();
    assert!(!unmanaged::discover(&repo, &harnesses).unwrap().iter().any(|item| item.name == "pdf"));
    assert!(matches!(index_entry(&action.dest), Some(IndexEntry::Link { .. })));
    assignment.set_skill("h", &skill_id(&repo, "pdf"), false);
    let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    let action = actions.iter().find(|a| a.dest == dir.join("pdf")).unwrap();
    assert_eq!(action.kind, ActionKind::Remove);
    apply(&repo, action).unwrap();
    assert!(!dir.join("pdf").is_symlink());
    assert!(dir.join("manual").is_symlink());
}

#[cfg(unix)]
#[test]
fn links_to_another_repo_directory_conflict_even_with_force() {
    for indexed in [false, true] {
        let (tmp, repo, harnesses) = skill_setup(LinkMode::Symlink);
        let _home = tmp.enter();
        let assignment = Assignment::default();
        let dir = tmp.path().join("skills");
        if indexed {
            let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
            apply(&repo, &actions[0]).unwrap();
            fs::remove_file(dir.join("pdf")).unwrap();
        } else {
            fs::create_dir(&dir).unwrap();
        }
        std::os::unix::fs::symlink(repo.personal_dir(), dir.join("pdf")).unwrap();
        for force in [false, true] {
            let actions = plan(&repo, &harnesses, &assignment, PlanOptions { force }).unwrap();
            assert_eq!(actions[0].kind, ActionKind::Conflict);
            apply(&repo, &actions[0]).unwrap();
            assert_eq!(fs::read_link(dir.join("pdf")).unwrap(), repo.personal_dir());
        }
    }
}

#[cfg(unix)]
#[test]
fn repo_link_with_parent_escape_is_unmanaged() {
    let (tmp, repo, harnesses) = skill_setup(LinkMode::Symlink);
    let _home = tmp.enter();
    let dir = tmp.path().join("skills");
    fs::create_dir(&dir).unwrap();
    write(&tmp.path().join("outside/SKILL.md"), "Outside.\n");
    std::os::unix::fs::symlink(repo.root().join("../outside"), dir.join("pdf")).unwrap();
    let actions = plan(&repo, &harnesses, &Assignment::default(), PlanOptions::default()).unwrap();
    assert_eq!(actions[0].kind, ActionKind::Conflict);
    apply(&repo, &actions[0]).unwrap();
    assert_eq!(fs::read_to_string(dir.join("pdf/SKILL.md")).unwrap(), "Outside.\n");
}

#[test]
fn missing_ids_use_defaults_and_reconcile_without_replacing_bytes() {
    let tmp = Home::new();
    let _home = tmp.enter();
    let recovery = displaced::root().unwrap();

    let repo = Repo::init_committed(&tmp.path().join("repo")).unwrap();
    write(&repo.personal_dir().join("pdf/SKILL.md"), "PDFs.\n");
    write(&repo.root().join("instructions/git.md"), "Use CC.\n");
    let harness = Harness {
        skills: Some(tmp.path().join("skills").to_string_lossy().into_owned()),
        instructions: Some(tmp.path().join("rules").to_string_lossy().into_owned()),
        instructions_format: InstructionsFormat::Rules,
        mode: LinkMode::Copy,
    };
    let harnesses = BTreeMap::from([("h".into(), harness)]);
    // Toggles are keyed by Item ID: an item without one has none, so the
    // defaults decide, even with every known item off.
    let mut assignment = Assignment::default();
    assignment.set_skill("h", &ItemId::from_text("other-skill").unwrap(), false);
    assignment.set_rule("h", &ItemId::from_text("other-rule").unwrap(), false);
    let planned = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    assert_eq!(planned.len(), 2, "read-only planning uses enabled defaults");
    assert!(repo.item_ids().unwrap().iter().all(|(_, id)| id.is_none()));
    for action in planned {
        apply(&repo, &action).unwrap();
    }
    repo.assign_missing_ids().unwrap();
    let plan_now = || plan(&repo, &harnesses, &Assignment::default(), PlanOptions::default()).unwrap();
    let repairs = plan_now();
    assert_eq!(repairs.len(), 2);
    let receipts = recovery.join(".placements");
    fs::rename(&receipts, recovery.join("previous-receipts")).unwrap();
    write(&receipts, "state temporarily unavailable");
    assert!(apply(&repo, &repairs[0]).is_err());
    fs::remove_file(&receipts).unwrap();
    assert_eq!(plan_now().len(), 2, "an unwritten receipt stays pending");
    for action in repairs {
        assert!(matches!(
            action.item,
            Item::Skill { metadata_only: true, .. } | Item::Rule { metadata_only: true, .. }
        ));
        let before = fs::metadata(&action.dest).unwrap().modified().unwrap();
        apply(&repo, &action).unwrap();
        assert_eq!(fs::metadata(&action.dest).unwrap().modified().unwrap(), before);
    }
    assert!(plan_now().is_empty());
    assert!(displaced::list().unwrap().iter().all(|e| !e.dest.starts_with(tmp.path())));
}

#[test]
fn assignment_selects_skills_and_rules() {
    let tmp = Home::new();
    let _home = tmp.enter();
    let root = tmp.path().join("repo");
    write(&root.join("sk1ll.toml"), "");
    write(&root.join("personal/pdf/SKILL.md"), "---\nname: pdf\n---\n");
    write(&root.join("personal/git/SKILL.md"), "---\nname: git\n---\n");
    write(&root.join("instructions/10-only-other.md"), "---\nharnesses: [other]\n---\nNot here.\n");
    write(&root.join("instructions/20-legacy.md"), "---\ntargets: [h]\n---\nLegacy key.\n");
    write(&root.join("instructions/30-off.md"), "Disabled.\n");
    let repo = Repo::open(&root).unwrap();
    repo.assign_missing_ids().unwrap();

    let out = tmp.path().join("harness");
    let harness = Harness {
        skills: Some(out.join("skills").to_string_lossy().into_owned()),
        instructions: Some(out.join("AGENTS.md").to_string_lossy().into_owned()),
        ..Harness::default()
    };
    let harnesses = BTreeMap::from([("h".to_string(), harness)]);
    let mut assignment = Assignment::default();
    assignment.set_skill("h", &skill_id(&repo, "pdf"), false);
    assignment.set_rule("h", &rule_id(&repo, "30-off"), false);

    let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    let created: Vec<&str> = actions
        .iter()
        .filter(|a| a.kind == ActionKind::Create)
        .filter_map(|a| match &a.item {
            Item::Skill { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(created, ["git"]);
    let rules = actions.iter().find_map(|a| match &a.item {
        Item::Instructions { rules, .. } => Some(rules.clone()),
        _ => None,
    });
    assert_eq!(rules, Some(vec!["20-legacy".to_string()]));
    assert!(actions.iter().all(|a| a.harness == "h"));

    // Turning an applied skill off removes it.
    for a in &actions {
        apply(&repo, a).unwrap();
    }
    assignment.set_skill("h", &skill_id(&repo, "git"), false);
    let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].kind, ActionKind::Remove);
    assert_eq!(actions[0].dest, out.join("skills/git"));
}

#[cfg(unix)]
#[test]
fn every_name_of_a_file_has_one_key_and_nothing_is_created() {
    use sk1ll_core::real_path::{key, real_path};

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("dotfiles")).unwrap();
    std::fs::write(root.join("dotfiles/machine.toml"), "").unwrap();
    std::os::unix::fs::symlink(root.join("dotfiles/machine.toml"), root.join("a.toml")).unwrap();
    std::os::unix::fs::symlink("dotfiles", root.join("dots")).unwrap();
    std::os::unix::fs::symlink("dots/machine.toml", root.join("b.toml")).unwrap();
    let real = root.canonicalize().unwrap().join("dotfiles/machine.toml");
    for name in ["a.toml", "b.toml", "dots/machine.toml", "dotfiles/machine.toml"] {
        assert_eq!(real_path(&root.join(name)).unwrap(), real, "{name}");
        assert_eq!(key(&root.join(name)).unwrap(), key(&real).unwrap(), "{name}");
    }
    // A dangling link names what a write through it creates; missing
    // directories are kept as named, and none is made.
    std::os::unix::fs::symlink(root.join("new/dir/file"), root.join("dangling")).unwrap();
    assert_eq!(real_path(&root.join("dangling")).unwrap(), root.canonicalize().unwrap().join("new/dir/file"));
    assert!(!root.join("new").exists());
    assert_ne!(key(&root.join("dangling")).unwrap(), key(&real).unwrap());
    std::os::unix::fs::symlink("loop", root.join("loop")).unwrap();
    assert!(real_path(&root.join("loop")).unwrap_err().to_string().contains("too many levels"));
}
