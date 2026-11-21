//! The engine lock: one process at a time may change a skills repo or the
//! harnesses it feeds.
//!
//! The daemon holds it for as long as it runs, so a direct CLI mutation finds
//! it taken and must go through the daemon instead. Without a daemon, each
//! CLI command takes it for its own duration. It is an OS file lock, so it is
//! released when the holder exits, even after a crash.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use crate::git::Git;
use crate::{Error, IoExt, Result};

const LOCK_FILE: &str = "sk1ll-engine.lock";

/// Held engine lock; dropping it releases the lock.
#[derive(Debug)]
pub struct EngineLock {
    _file: File,
    path: PathBuf,
}

impl EngineLock {
    /// Takes the lock for the repo at `root`, or returns `Ok(None)` if another
    /// process holds it.
    pub fn try_acquire(root: &Path) -> Result<Option<Self>> {
        let (file, path) = open(root)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file, path })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(e).at(&path),
        }
    }

    /// Like [`try_acquire`](Self::try_acquire), but a held lock is an error
    /// telling the user to go through the daemon.
    pub fn acquire(root: &Path) -> Result<Self> {
        Self::try_acquire(root)?.ok_or_else(|| {
            Error::Invalid(format!(
                "another sk1ll process (usually the daemon) is managing {}; run this through the daemon",
                root.display()
            ))
        })
    }

    pub(crate) fn check_for(&self, root: &Path) -> Result<()> {
        if self.path == lock_path(root)? {
            return Ok(());
        }
        Err(Error::Invalid(format!("the supplied engine lock is for a different repository than {}", root.display())))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// The lock lives in the git common dir, so every worktree of one repository
/// shares it, and it never shows up in the draft.
fn open(root: &Path) -> Result<(File, PathBuf)> {
    let path = lock_path(root)?;
    let file = OpenOptions::new().create(true).truncate(false).write(true).open(&path).at(&path)?;
    Ok((file, path))
}

fn lock_path(root: &Path) -> Result<PathBuf> {
    let common = Git::new(root).args(["rev-parse", "--path-format=absolute", "--git-common-dir"]).run()?;
    let common = crate::git::path_from_bytes(common.strip_suffix(b"\n").unwrap_or(&common));
    Ok(common.canonicalize().at(&common)?.join(LOCK_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_holder_is_refused_until_the_first_drops() {
        let dir = tempfile::tempdir().unwrap();
        Git::new(dir.path()).args(["init", "-q"]).text().unwrap();

        let first = EngineLock::acquire(dir.path()).unwrap();
        assert!(EngineLock::try_acquire(dir.path()).unwrap().is_none());
        assert!(EngineLock::acquire(dir.path()).is_err());
        drop(first);
        assert!(EngineLock::try_acquire(dir.path()).unwrap().is_some());
    }
}
