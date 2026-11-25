//! Writing user config files (`config_file::Snapshot`): never through a
//! changed file, never half-written.
#![cfg(unix)]

use sk1ll_core::{
    Error,
    config_file::Snapshot,
    failpoints::{self, Point},
};
use std::{fs, os::unix::fs::PermissionsExt, path::Path};

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    names.sort();
    names
}

#[test]
fn creates_config_under_a_missing_directory_beneath_a_symlink() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("target");
    fs::create_dir(&target).unwrap();
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let path = link.join("new/machine.toml");
    Snapshot::take(&path).unwrap().replace("id = 'box'").unwrap();
    assert_eq!(fs::read_to_string(target.join("new/machine.toml")).unwrap(), "id = 'box'");
}

#[test]
fn retargeting_during_staging_aborts_publish() {
    use std::os::unix::fs::symlink;
    let tmp = tempfile::tempdir().unwrap();
    let first = tmp.path().join("first");
    let second = tmp.path().join("second");
    let path = tmp.path().join("machine.toml");
    fs::write(&first, "first").unwrap();
    fs::write(&second, "second").unwrap();
    symlink(&first, &path).unwrap();
    let snap = Snapshot::take(&path).unwrap();
    let (link, newer) = (path.clone(), second.clone());
    let _retarget = failpoints::once(Point::ConfigBeforeCheck, move |_| {
        fs::remove_file(&link).unwrap();
        symlink(&newer, &link).unwrap();
    });
    assert!(matches!(snap.replace("ours"), Err(Error::Changed(_))));
    assert_eq!(fs::read_to_string(first).unwrap(), "first");
    assert_eq!(fs::read_to_string(path).unwrap(), "second");
}

#[test]
fn replaces_keeping_mode_and_never_using_an_occupied_temp_name() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("machine.toml");
    fs::write(&path, "old\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o6640)).unwrap();
    // Whatever holds the temporary names stays as it is: a symlink to
    // another file is never followed, a file never truncated.
    let victim = tmp.path().join("victim");
    fs::write(&victim, "victim\n").unwrap();
    std::os::unix::fs::symlink(&victim, tmp.path().join(".machine.toml.sk1ll-tmp")).unwrap();
    fs::write(tmp.path().join(".machine.toml.sk1ll-tmp-1"), "someone's\n").unwrap();

    Snapshot::take(&path).unwrap().replace("new\n").unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o7777, 0o6640);
    assert_eq!(fs::read_to_string(&victim).unwrap(), "victim\n");
    assert!(fs::symlink_metadata(tmp.path().join(".machine.toml.sk1ll-tmp")).unwrap().file_type().is_symlink());
    assert_eq!(fs::read_to_string(tmp.path().join(".machine.toml.sk1ll-tmp-1")).unwrap(), "someone's\n");
    assert!(!tmp.path().join(".machine.toml.sk1ll-tmp-2").exists(), "the temporary file is gone");

    // A new file is private.
    let new = tmp.path().join("sub/new.toml");
    Snapshot::take(&new).unwrap().replace("x = 1\n").unwrap();
    assert_eq!(fs::metadata(&new).unwrap().permissions().mode() & 0o7777, 0o600);
    assert_eq!(names(&tmp.path().join("sub")), ["new.toml"]);
}

#[test]
fn a_symlinked_file_stays_a_link_and_a_retargeted_link_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let dotfiles = tmp.path().join("dotfiles");
    fs::create_dir_all(&dotfiles).unwrap();
    fs::write(dotfiles.join("machine.toml"), "old\n").unwrap();
    fs::write(dotfiles.join("other.toml"), "other\n").unwrap();
    let path = tmp.path().join("machine.toml");
    std::os::unix::fs::symlink(dotfiles.join("machine.toml"), &path).unwrap();

    Snapshot::take(&path).unwrap().replace("new\n").unwrap();
    assert!(fs::symlink_metadata(&path).unwrap().file_type().is_symlink());
    assert_eq!(fs::read_to_string(dotfiles.join("machine.toml")).unwrap(), "new\n");

    let snap = Snapshot::take(&path).unwrap();
    fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(dotfiles.join("other.toml"), &path).unwrap();
    assert!(matches!(snap.replace("derived from machine.toml\n"), Err(Error::Changed(_))));
    assert_eq!(fs::read_to_string(dotfiles.join("other.toml")).unwrap(), "other\n");
    assert_eq!(fs::read_to_string(dotfiles.join("machine.toml")).unwrap(), "new\n");

    // A dangling link is no missing file.
    let dangling = tmp.path().join("dangling.toml");
    std::os::unix::fs::symlink(tmp.path().join("nowhere"), &dangling).unwrap();
    assert!(Snapshot::take(&dangling).is_err());
}

#[test]
fn a_change_since_the_look_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("machine.toml");
    fs::write(&path, "old\n").unwrap();

    // Saved after the look, before the write: in place, or by an editor's rename.
    for in_place in [true, false] {
        fs::write(&path, "old\n").unwrap();
        let snap = Snapshot::take(&path).unwrap();
        if in_place {
            fs::write(&path, "saved\n").unwrap();
        } else {
            fs::write(tmp.path().join("editor.swp"), "old\n").unwrap();
            fs::rename(tmp.path().join("editor.swp"), &path).unwrap();
        }
        assert!(matches!(snap.replace("new\n"), Err(Error::Changed(_))), "in place: {in_place}");
        assert_ne!(fs::read_to_string(&path).unwrap(), "new\n");
    }

    // Its permissions changed: never silently put back.
    let snap = Snapshot::take(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(matches!(snap.replace("new\n"), Err(Error::Changed(_))));
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o7777, 0o600);
    assert_eq!(names(tmp.path()), ["machine.toml"], "no temporary file is left behind");

    // Created meanwhile where there was none, even after the last check.
    let missing = tmp.path().join("new.toml");
    let snap = Snapshot::take(&missing).unwrap();
    let m = missing.clone();
    let _create = failpoints::once(Point::ConfigBeforePublish, move |_| fs::write(&m, "theirs\n").unwrap());
    let result = snap.replace("ours\n");
    assert!(matches!(result, Err(Error::Changed(_))), "{result:?}");
    assert_eq!(fs::read_to_string(&missing).unwrap(), "theirs\n");
    assert_eq!(names(tmp.path()), ["machine.toml", "new.toml"]);
}

#[test]
fn a_fifo_is_refused_without_blocking() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("machine.toml");
    let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let err = Snapshot::take(&path).unwrap_err();
    assert!(err.to_string().contains("not a regular file"), "{err}");

    // Nor does one placed after the look stall the write.
    let other = tmp.path().join("other.toml");
    fs::write(&other, "old\n").unwrap();
    let snap = Snapshot::take(&other).unwrap();
    fs::remove_file(&other).unwrap();
    let c = std::ffi::CString::new(other.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    assert!(matches!(snap.replace("new\n"), Err(Error::Changed(_))));
}

#[test]
fn every_config_write_checkpoint_leaves_a_readable_file_that_can_be_retried() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let old = "# mine\nid = 'old'\n";
    let new = "# mine\nid = 'new'\n";
    for exists in [false, true] {
        let make = || {
            let case = tempfile::tempdir_in(root).unwrap();
            let path = case.path().join("machine.toml");
            if exists {
                sk1ll_testkit::write(&path, old);
            }
            (case, path)
        };
        let (_probe, path) = make();
        let hits = {
            let record = failpoints::record();
            Snapshot::take(&path).unwrap().replace(new).unwrap();
            record.hits()
        };
        assert_eq!(
            hits.iter().map(|hit| hit.point).collect::<Vec<_>>(),
            [Point::ConfigBeforeCheck, Point::ConfigBeforePublish]
        );
        for (n, expected) in hits.iter().enumerate() {
            let (dir, path) = make();
            let crash = {
                let _at = failpoints::crash_at(n);
                failpoints::catch(|| Snapshot::take(&path).unwrap().replace(new)).unwrap_err()
            };
            assert_eq!(crash.point, expected.point);
            let reopened = Snapshot::take(&path).unwrap();
            assert_eq!(reopened.exists(), exists);
            assert_eq!(reopened.text().unwrap(), if exists { old } else { "" });
            reopened.replace(new).unwrap();
            assert_eq!(sk1ll_core::MachineConfig::load(&path).unwrap().id, "new");
            assert_eq!(names(dir.path()), ["machine.toml"]);
        }
    }
}
