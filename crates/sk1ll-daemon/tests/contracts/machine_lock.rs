use std::path::{Path, PathBuf};

use sk1ll_daemon::{Config, MachineLock};

fn cfg(dir: &Path, repo: &str) -> Config {
    Config {
        repo: Some(dir.join(repo)),
        state: dir.join("config/daemon.json"),
        machine_config: dir.join("config/machine.toml"),
        assignment: dir.join("cache/assignment.json"),
        socket: dir.join("run/daemon.sock"),
        state_dir: dir.join("state"),
        cache_dir: dir.join("cache"),
        ..Config::new().unwrap()
    }
}

#[test]
fn one_holder_per_machine_whatever_the_repo() {
    let dir = tempfile::tempdir().unwrap();
    let first = MachineLock::acquire(&cfg(dir.path(), "a")).unwrap();
    assert_eq!(first.held().count(), cfg(dir.path(), "a").machine_files().len());
    // Another repo on the same Machine is the same Machine.
    assert!(MachineLock::try_acquire(&cfg(dir.path(), "b")).unwrap().is_none());
    let err = MachineLock::acquire(&cfg(dir.path(), "b")).unwrap_err().to_string();
    assert!(err.contains("another sk1ll process"), "{err}");
    // Sharing any one of its files is sharing the Machine.
    let shares_assignment = Config { state: dir.path().join("other/daemon.json"), ..cfg(dir.path(), "b") };
    assert!(MachineLock::try_acquire(&shares_assignment).unwrap().is_none());
    drop(first);
    assert!(MachineLock::try_acquire(&cfg(dir.path(), "b")).unwrap().is_some());
}

#[test]
fn a_refused_attempt_keeps_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let machine = cfg(dir.path(), "a");
    // Holds only the Assignment's lock file.
    let partial = Config {
        state: dir.path().join("x/daemon.json"),
        machine_config: dir.path().join("x/m.toml"),
        ..machine.clone()
    };
    let held = MachineLock::acquire(&partial).unwrap();
    assert!(MachineLock::try_acquire(&machine).unwrap().is_none());
    drop(held);
    // Nothing of the refused attempt stayed locked.
    let state_only = Config { assignment: dir.path().join("y/assignment.json"), ..machine.clone() };
    assert!(MachineLock::try_acquire(&state_only).unwrap().is_some());
}

/// Two names for one file are one lock: symlinked `machine.toml` names,
/// and state files that share the files kept beside them.
#[test]
fn a_lock_is_keyed_on_the_file_not_its_name() {
    let dir = tempfile::tempdir().unwrap();
    let base = |n: &str| Config {
        state: dir.path().join(n).join("daemon.json"),
        machine_config: dir.path().join(n).join("machine.toml"),
        assignment: dir.path().join(n).join("assignment.json"),
        ..cfg(dir.path(), "a")
    };
    std::fs::create_dir_all(dir.path().join("dotfiles")).unwrap();
    let shared = dir.path().join("dotfiles/machine.toml");
    std::fs::write(&shared, "").unwrap();
    for n in ["x", "y"] {
        std::fs::create_dir_all(dir.path().join(n)).unwrap();
    }
    let x = Config { machine_config: dir.path().join("x/a.toml"), ..base("x") };
    let y = Config { machine_config: dir.path().join("y/b.toml"), ..base("y") };
    std::os::unix::fs::symlink(&shared, &x.machine_config).unwrap();
    // A relative link, through a link to the directory.
    std::os::unix::fs::symlink(dir.path().join("dotfiles"), dir.path().join("y/dots")).unwrap();
    std::os::unix::fs::symlink("dots/machine.toml", &y.machine_config).unwrap();
    let held = MachineLock::acquire(&x).unwrap();
    assert!(MachineLock::try_acquire(&y).unwrap().is_none(), "one machine.toml, two names");
    drop(held);
    // A dangling link names the file a write through it creates.
    let dangling = Config { assignment: dir.path().join("x/link.json"), ..base("z") };
    std::os::unix::fs::symlink(dir.path().join("y/assignment.json"), &dangling.assignment).unwrap();
    let held = MachineLock::acquire(&base("y")).unwrap();
    assert!(MachineLock::try_acquire(&dangling).unwrap().is_none(), "a dangling link to y's Assignment");
    drop(held);
    // Two state files in one directory share what is kept beside them.
    let first = Config { state: dir.path().join("s/first.json"), ..base("p") };
    let second = Config { state: dir.path().join("s/second.json"), ..base("q") };
    let held = MachineLock::acquire(&first).unwrap();
    assert!(MachineLock::try_acquire(&second).unwrap().is_none(), "standalone-assignment.json is shared");
    drop(held);
    // Nothing shared: two Machines.
    let _p = MachineLock::acquire(&base("p")).unwrap();
    assert!(MachineLock::try_acquire(&base("q")).unwrap().is_some());
    // No lock file beside any of the files, the one in the dotfiles
    // directory included: they are all in the state directory.
    assert_eq!(std::fs::read_dir(dir.path().join("dotfiles")).unwrap().count(), 1);
}

/// Every lock file is in `locks/` of sk1ll's state directory, named by
/// a hash, and taken in one order; none sits beside the files.
#[test]
fn the_lock_files_are_in_one_directory_of_sk1lls_own() {
    let dir = tempfile::tempdir().unwrap();
    let machine = cfg(dir.path(), "a");
    let lock = MachineLock::acquire(&machine).unwrap();
    let held: Vec<PathBuf> = lock.held().map(Path::to_path_buf).collect();
    let locks = dir.path().join("state/locks");
    assert_eq!(held.len(), machine.machine_files().len());
    assert!(held.is_sorted(), "taken in the order of their names: {held:?}");
    for path in &held {
        assert_eq!(path.parent(), Some(locks.as_path()));
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(name.len() == 64 + ".lock".len() && name.ends_with(".lock"), "{name}");
    }
    assert_eq!(MachineLock::paths(&machine).unwrap(), held);
    // Nothing beside the Machine's files: only the lock directory was made.
    let mut made: Vec<_> = walk(dir.path());
    made.sort();
    let mut expected = vec![dir.path().join("state"), locks.clone()];
    expected.extend(held.iter().cloned());
    expected.sort();
    assert_eq!(made, expected);
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(walk(&path));
        }
        found.push(path);
    }
    found
}
