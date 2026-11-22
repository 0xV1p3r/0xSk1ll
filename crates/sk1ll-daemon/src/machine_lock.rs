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
//! It is a set of OS file locks, one beside each file that names the Machine's
//! state (`<name>.engine-lock`): the state file, `machine.toml` and the
//! Assignment file. Each can be moved on its own (`SK1LL_DAEMON_STATE`,
//! `SK1LL_MACHINE_CONFIG`, `SK1LL_ASSIGNMENT`), and two engines sharing any of
//! them would share state. Being OS locks, they are released when the holder
//! exits, even after a crash.
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
    /// The lock files for `cfg`'s Machine.
    pub fn paths(cfg: &Config) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> =
            [&cfg.state, &cfg.machine_config, &cfg.assignment].iter().map(|p| lock_path(p)).collect();
        paths.sort();
        paths.dedup();
        paths
    }

    /// Takes the lock, or returns `Ok(None)` if another process holds any
    /// part of it.
    pub fn try_acquire(cfg: &Config) -> anyhow::Result<Option<Self>> {
        let mut held = Vec::new();
        for path in Self::paths(cfg) {
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
    file.with_extension("engine-lock")
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
        assert_eq!(first.held().count(), 3);
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
}
