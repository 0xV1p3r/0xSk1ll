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
//! the repo, the harness directories and sk1ll's state directory
//! ([`Config::machine_files`]): the state file, `machine.toml`, the
//! Assignment file and the files kept beside the state file (a joined
//! Machine's Assignment snapshot, a standalone one's revision and pull
//! record, the notifications shown). A pending Harness rename is kept in the
//! state directory under `machine.toml`'s real path, so `machine.toml`'s lock
//! covers it. Each lock is keyed on the file itself, not on the name it is
//! configured by: its real path (every symlink resolved, its own too, and its
//! directory canonical, see [`sk1ll_core::real_path`]), so two names for one
//! file (a symlinked `machine.toml`, two state files in one directory sharing
//! `standalone-assignment.json`) are one lock. The lock files all live in one
//! directory of sk1ll's own, `locks/` in [`Config::state_dir`], each named by
//! the hash of the real path it stands for, so none is created beside a
//! user's file (a `machine.toml` in a dotfiles repo, say), and nothing is
//! created while resolving. They are taken in the order of their names, the
//! same for every engine. Each path can be moved on its own
//! (`SK1LL_DAEMON_STATE`, `SK1LL_MACHINE_CONFIG`, `SK1LL_ASSIGNMENT`), and
//! two engines sharing any of these files would share state, so they share
//! the Machine, as long as they share the state directory too. Being OS
//! locks, they are released when the holder exits, even after a crash.
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
    /// that file's real path, in the order they are taken.
    pub fn paths(cfg: &Config) -> anyhow::Result<Vec<PathBuf>> {
        let dir = cfg.state_dir.join("locks");
        let mut paths = cfg
            .machine_files()
            .iter()
            .map(|p| Ok(dir.join(format!("{}.lock", sk1ll_core::real_path::key(p)?))))
            .collect::<anyhow::Result<Vec<_>>>()?;
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
                // What was taken is unlocked, not only closed: a child
                // another thread forked meanwhile shares its descriptions
                // until it execs, and would keep them locked.
                Err(TryLockError::WouldBlock) => {
                    drop(Self { held });
                    return Ok(None);
                }
                Err(TryLockError::Error(e)) => {
                    drop(Self { held });
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
