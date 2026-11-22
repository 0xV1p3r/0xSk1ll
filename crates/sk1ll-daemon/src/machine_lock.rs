//! The machine lock: one engine at a time per Machine ([ADR 0008]).
//!
//! What an engine changes is mostly the Machine's, not a repo's:
//! `machine.toml` and a pending Harness rename, the Assignment (the
//! standalone file and its revision, or a joined Machine's snapshot and its
//! mirror), the state file and the harness directories `machine.toml` names.
//! So whatever repo it manages, an engine holds this lock before it reads any
//! of that and until it ends: the daemon for its whole life, a command's
//! engine in the CLI for the command. A second engine on the same Machine, for
//! the same repo or another, is refused, and the daemon waits for a command's
//! engine to end before it starts.
//!
//! It is a set of OS file locks, one for each file an engine writes outside
//! the repo and the harness directories ([`Config::machine_files`]): the
//! state file, `machine.toml`, the Assignment file and the files kept beside
//! the state file (a joined Machine's Assignment snapshot, a standalone
//! one's revision and pull record, the notifications shown) and beside
//! `machine.toml` (a pending Harness rename). Each lock is keyed on the file
//! itself, not on the name it is configured by: it sits beside the file's
//! real path (`<file>.engine-lock`, every symlink resolved, its own too, and
//! its directory canonical), so two names for one file (a symlinked
//! `machine.toml`, two state files in one directory sharing
//! `standalone-assignment.json`) are one lock. Each path can be moved on its
//! own (`SK1LL_DAEMON_STATE`, `SK1LL_MACHINE_CONFIG`, `SK1LL_ASSIGNMENT`), and
//! two engines sharing any of these files would share state, so they share
//! the Machine. Being OS locks, they are released when the holder exits,
//! even after a crash.
//!
//! Core's engine lock on the repo ([`sk1ll_core::EngineLock`]) stays: it is
//! the token core's repo operations (a pull, an Adopt, a harness change)
//! require, and it keeps two Machines' engines (separate config dirs over
//! one working tree) apart.
//!
//! [ADR 0008]: ../../../docs/adr/0008-cli-is-a-client-of-the-engine.md

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};

use crate::Config;

/// The held machine lock; dropping it releases it.
#[derive(Debug)]
pub struct MachineLock {
    held: Vec<(File, PathBuf)>,
}

impl Drop for MachineLock {
    fn drop(&mut self) {
        // As for core's engine lock: a child forked on another thread shares
        // the open description until it execs, so unlock for all of them.
        for (file, _) in &self.held {
            let _ = file.unlock();
        }
    }
}

impl MachineLock {
    /// The lock files for `cfg`'s Machine, one per file it writes, keyed on
    /// that file's real path.
    pub fn paths(cfg: &Config) -> anyhow::Result<Vec<PathBuf>> {
        let mut paths =
            cfg.machine_files().iter().map(|p| Ok(lock_path(&real_path(p)?))).collect::<anyhow::Result<Vec<_>>>()?;
        paths.sort();
        paths.dedup();
        Ok(paths)
    }

    /// Takes the lock, or returns `Ok(None)` if another process holds any
    /// part of it.
    pub fn try_acquire(cfg: &Config) -> anyhow::Result<Option<Self>> {
        let mut held = Vec::new();
        for path in Self::paths(cfg)? {
            let file = open(&path)?;
            match file.try_lock() {
                Ok(()) => held.push((file, path)),
                // What was taken is released with `held`.
                Err(TryLockError::WouldBlock) => return Ok(None),
                Err(TryLockError::Error(e)) => {
                    return Err(e).with_context(|| format!("locking {}", path.display()));
                }
            }
        }
        Ok(Some(Self { held }))
    }

    /// Like [`try_acquire`](Self::try_acquire), but a held lock is an error
    /// saying why.
    pub fn acquire(cfg: &Config) -> anyhow::Result<Self> {
        match Self::try_acquire(cfg)? {
            Some(lock) => Ok(lock),
            None => bail!("{}", held_message(cfg)),
        }
    }

    /// For the daemon: waits up to `patience` for a command's engine to end.
    pub async fn wait(cfg: &Config, patience: Duration) -> anyhow::Result<Self> {
        let until = Instant::now() + patience;
        let mut said = false;
        loop {
            if let Some(lock) = Self::try_acquire(cfg)? {
                return Ok(lock);
            }
            if Instant::now() >= until {
                bail!("{}", held_message(cfg));
            }
            if !said {
                tracing::info!("waiting for another sk1ll process to finish with this Machine");
                said = true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// The files it holds locked.
    pub fn held(&self) -> impl Iterator<Item = &Path> {
        self.held.iter().map(|(_, path)| path.as_path())
    }
}

/// Why the lock can't be had now.
pub(crate) fn held_message(cfg: &Config) -> String {
    format!(
        "another sk1ll process (the daemon, or a command running without it) is managing this Machine ({}); \
         run this through the daemon, or once that command has finished",
        cfg.machine_config.display()
    )
}

fn lock_path(file: &Path) -> PathBuf {
    let mut name = file.file_name().map(std::ffi::OsStr::to_os_string).unwrap_or_default();
    name.push(".engine-lock");
    file.with_file_name(name)
}

/// The file a write to `path` changes, as one name for all its names: every
/// symlink resolved, `path`'s own included even when it dangles (a write
/// through it creates its target), and the deepest directory that exists
/// canonical. Nothing is created.
fn real_path(path: &Path) -> anyhow::Result<PathBuf> {
    /// As the kernel's limit on following symlinks.
    const HOPS: usize = 40;
    let mut path = std::path::absolute(path).with_context(|| format!("resolving {}", path.display()))?;
    for _ in 0..HOPS {
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
            bail!("{} names no file", path.display());
        };
        let here = canonical_dir(dir)?.join(name);
        match std::fs::symlink_metadata(&here) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = std::fs::read_link(&here).with_context(|| format!("reading {}", here.display()))?;
                path = here.parent().expect("a file in a directory").join(target);
            }
            _ => return Ok(here),
        }
    }
    bail!("{}: too many levels of symbolic links", path.display())
}

/// `dir` canonical as far as it exists, the rest as given.
fn canonical_dir(dir: &Path) -> anyhow::Result<PathBuf> {
    let mut missing = Vec::new();
    let mut existing = dir;
    loop {
        match existing.canonicalize() {
            Ok(real) => return Ok(missing.iter().rev().fold(real, |p, name| p.join(name))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let (Some(parent), Some(name)) = (existing.parent(), existing.file_name()) else {
                    return Err(e).with_context(|| format!("resolving {}", dir.display()));
                };
                missing.push(name.to_os_string());
                existing = parent;
            }
            Err(e) => return Err(e).with_context(|| format!("resolving {}", existing.display())),
        }
    }
}

fn open(path: &Path) -> anyhow::Result<File> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
    }
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(dir: &Path, repo: &str) -> Config {
        Config {
            repo: Some(dir.join(repo)),
            state: dir.join("config/daemon.json"),
            machine_config: dir.join("config/machine.toml"),
            assignment: dir.join("cache/assignment.json"),
            socket: dir.join("run/daemon.sock"),
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
    }
}
