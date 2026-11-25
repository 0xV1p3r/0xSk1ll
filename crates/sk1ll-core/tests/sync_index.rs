mod sync_common;

use std::{fs, path::Path};

use sk1ll_core::failpoints::Point;
use sk1ll_core::sync::*;
use sk1ll_core::{Assignment, InstructionsFormat, LinkMode};
use sk1ll_testkit::{Home, write};

use sync_common::*;

#[test]
fn stale_index_entries_refuse_skill_and_rule_updates() {
    for case in ["skill update", "skill removal", "rule update", "rule removal"] {
        for point in [None, Some(Point::BeforeSwap), Some(Point::AfterAside)] {
            let d = Displacement::new(case);
            let _home = d.home.enter();
            let action = d.action();
            let before = fs::read(&d.file).unwrap();
            let holder = d.holder.clone();
            let name = d.dest.file_name().unwrap().to_string_lossy().into_owned();
            let mut index = read_index(&holder);
            assert!(index["entries"].as_object_mut().unwrap().remove(&name).is_some());
            let edit = move |_: &Path| replace_index(&holder, &index);
            let result = match point {
                Some(point) => with_hook(point, edit, || apply(&d.repo, &action)),
                None => {
                    edit(&d.dest);
                    apply(&d.repo, &action)
                }
            };
            assert_changed(result, &d.dest);
            assert_eq!(fs::read(&d.file).unwrap(), before);
            assert!(index_entry(&d.dest).is_none());
            assert!(displaced::list().unwrap().is_empty());
        }
    }
}

#[test]
fn a_partially_written_index_invalidates_the_plan() {
    let d = Displacement::new("skill update");
    let _home = d.home.enter();
    let action = d.action();
    write(&d.holder.join(INDEX_PATH), "{");
    assert_changed(apply(&d.repo, &action), &d.dest);
    assert_eq!(fs::read_to_string(&d.file).unwrap(), "PDFs.\n");
    assert_eq!(fs::read_to_string(d.holder.join(INDEX_PATH)).unwrap(), "{");
}

#[test]
fn publishing_does_not_restore_an_index_entry_removed_during_the_swap() {
    let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
    let _home = tmp.enter();
    let assignment = Assignment::default();
    let initial = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    let dest = &initial[0].dest;
    let result = with_hook(
        Point::AfterSwap,
        |dest| replace_index(dest.parent().unwrap(), &empty_index()),
        || apply(&repo, &initial[0]),
    );
    assert_changed(result, dest);
    assert!(index_entry(dest).is_none());
    assert!(dest.join("SKILL.md").is_file());
}

#[cfg(unix)]
#[test]
fn recording_an_exact_link_preserves_an_intervening_index_edit() {
    let (tmp, repo, harnesses) = skill_setup(LinkMode::Symlink);
    let _home = tmp.enter();
    let dir = tmp.path().join("skills");
    fs::create_dir(&dir).unwrap();
    std::os::unix::fs::symlink(repo.personal_dir().join("pdf"), dir.join("pdf")).unwrap();
    let actions = plan(&repo, &harnesses, &Assignment::default(), PlanOptions::default()).unwrap();
    let mut changed = empty_index();
    changed["entries"]["pdf"] =
        serde_json::json!({"kind": "copy", "hash": "other", "token": "t1", "payload": "payload"});
    let edit = changed.clone();
    let result = with_hook(
        Point::BeforeSwap,
        move |dest| replace_index(dest.parent().unwrap(), &edit),
        || apply(&repo, &actions[0]),
    );
    assert_changed(result, &actions[0].dest);
    assert_eq!(read_index(&dir), changed);
}

#[test]
fn rename_never_replaces_an_occupied_slot() {
    let tmp = tempfile::tempdir().unwrap();
    for dir in [false, true] {
        let (src, backup) = (tmp.path().join(format!("src-{dir}")), tmp.path().join(format!("slot-{dir}")));
        if dir {
            std::fs::create_dir(&src).unwrap();
            std::fs::create_dir(&backup).unwrap();
        } else {
            std::fs::write(&src, "incumbent").unwrap();
            std::fs::write(&backup, "occupant").unwrap();
        }
        assert_eq!(
            sk1ll_core::placement::rename_noreplace(&src, &backup).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert!(src.exists() && backup.exists());
        if !dir {
            assert_eq!(std::fs::read_to_string(&backup).unwrap(), "occupant");
        }
        let vacant = tmp.path().join(format!("vacant-{dir}"));
        sk1ll_core::placement::rename_noreplace(&src, &vacant).unwrap();
        assert!(!src.exists() && vacant.exists());
    }
}

#[test]
fn an_index_save_preserves_an_editor_replacement() {
    for operation in ["skill update", "skill removal"] {
        let d = Displacement::new(operation);
        let _home = d.home.enter();
        let action = d.action();
        let mut edited = read_index(&d.holder);
        edited["entries"]["other"] = edited["entries"]["pdf"].clone();
        let during = edited.clone();
        let result = with_hook(
            Point::IndexBeforeMove,
            move |path| replace_index(path.parent().unwrap().parent().unwrap(), &during),
            || apply(&d.repo, &action),
        );
        assert!(result.is_err(), "{operation}");
        assert_eq!(read_index(&d.holder), edited);
        assert_eq!(fs::read_to_string(&d.file).unwrap(), "PDFs.\n");
    }
}

#[test]
fn recovery_can_read_an_index_whose_publication_was_interrupted() {
    let d = Displacement::new("skill removal");
    let _home = d.home.enter();
    let index = d.holder.join(INDEX_PATH);
    let backup = d.holder.join(".sk1ll-state/index-rollback/previous");
    fs::create_dir_all(backup.parent().unwrap()).unwrap();
    fs::rename(&index, &backup).unwrap();

    let removal = d.action();
    assert_eq!(removal.kind, ActionKind::Remove);
    assert!(backup.is_file() && !index.exists(), "planning leaves the rollback intact");
    apply(&d.repo, &removal).unwrap();
    assert_eq!(read_index(&d.holder), empty_index());
    assert!(!backup.exists() && !d.dest.exists());
}

#[test]
fn an_open_index_descriptor_keeps_late_writes_in_the_archive() {
    use std::io::Write;
    let d = Displacement::new("skill update");
    let _home = d.home.enter();
    let index = d.holder.join(INDEX_PATH);
    let mut open = fs::OpenOptions::new().append(true).open(&index).unwrap();
    apply(&d.repo, &d.action()).unwrap();
    open.write_all(b"\nlate edit").unwrap();

    let history = d.holder.join(".sk1ll-state/index-history");
    let archives: Vec<_> = fs::read_dir(&history).unwrap().map(|entry| entry.unwrap().path()).collect();
    let saved = archives
        .iter()
        .find(|dir| fs::read_to_string(dir.join("previous")).is_ok_and(|text| text.ends_with("late edit")))
        .expect("the original index has the late write");
    let untouched = archives.iter().find(|dir| *dir != saved).unwrap();
    let stray = untouched.join("stray");
    write(&stray, "User data.");
    // Age both archive records so public recovery can prune immediately.
    for archive in &archives {
        let path = archive.join("entry.json");
        let mut entry: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        entry["displaced_at"] = serde_json::json!({"secs_since_epoch": 0, "nanos_since_epoch": 0});
        write(&path, &entry.to_string());
    }
    recover(None, &d.harnesses).unwrap();
    assert!(!untouched.join("previous").exists());
    assert_eq!(sk1ll_testkit::read(&stray), "User data.");
    assert!(sk1ll_testkit::read(&saved.join("previous")).ends_with("late edit"));
    assert_copy_recorded(&d.dest);
}

#[cfg(unix)]
#[test]
fn bookkeeping_writes_never_follow_planted_temp_names() {
    use std::os::unix::fs::symlink;

    let (tmp, repo, harnesses, rules) = instructions_setup(InstructionsFormat::Rules);
    let _home = tmp.enter();
    let victim = tmp.path().join("victim");
    write(&victim, "victim\n");
    fs::create_dir_all(rules.join(INDEX_DIR)).unwrap();
    for path in [rules.join(".git.md.sk1ll-tmp"), rules.join(INDEX_DIR).join(".index.json.sk1ll-tmp")] {
        symlink(&victim, path).unwrap();
    }

    let create = plan(&repo, &harnesses, &Assignment::default(), PlanOptions::default()).unwrap().remove(0);
    apply(&repo, &create).unwrap();
    assert!(create.dest.symlink_metadata().unwrap().is_file());
    assert!(rules.join(INDEX_PATH).symlink_metadata().unwrap().is_file());
    assert_eq!(fs::read_to_string(&create.dest).unwrap(), "Use CC.\n");
    assert_eq!(fs::read_to_string(&victim).unwrap(), "victim\n");
}

#[cfg(unix)]
#[test]
fn bookkeeping_honors_umask_and_preserves_replacements() {
    use sk1ll_testkit::child;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;

    if child::dir().is_none() {
        let home = Home::new();
        let mut cmd = child::command("bookkeeping_honors_umask_and_preserves_replacements", home.path());
        home.apply(&mut cmd);
        // SAFETY: umask is async-signal-safe and changes only the child's mask.
        unsafe {
            cmd.pre_exec(|| {
                libc::umask(0o077);
                Ok(())
            });
        }
        let output = cmd.output().unwrap();
        assert!(output.status.success(), "{}", sk1ll_testkit::text(&output));
        return;
    }

    let (tmp, repo, harnesses, rules) = instructions_setup(InstructionsFormat::Rules);
    let _home = tmp.enter();
    let assignment = Assignment::default();
    let assignment_path = tmp.path().join("assignment.json");
    let sync = || {
        assignment.save(&assignment_path).unwrap();
        for action in plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap() {
            apply(&repo, &action).unwrap();
        }
    };
    sync();
    let index = rules.join(INDEX_PATH);
    for file in [&assignment_path, &index] {
        assert_eq!(fs::metadata(file).unwrap().permissions().mode() & 0o777, 0o600);
        fs::set_permissions(file, fs::Permissions::from_mode(0o640)).unwrap();
    }
    edit_rule_body(&repo.root().join("instructions/git.md"), "Updated.\n");
    sync();
    for file in [&assignment_path, &index] {
        assert_eq!(fs::metadata(file).unwrap().permissions().mode() & 0o777, 0o640);
    }
}
