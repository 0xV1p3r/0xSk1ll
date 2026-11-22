//! Where sk1ll keeps its own files outside the repo and the harnesses: its
//! state directory (the recovery area's index and placement records, the
//! machine lock, a pending Harness rename) and its cache directory (import
//! sources, the Assignment). Each is what the process was configured with
//! ([`use_dirs`], which the daemon and the CLI call with their `Config`),
//! else `SK1LL_STATE_DIR` / `SK1LL_CACHE_DIR`, else the platform's default.
//! An empty variable counts as unset. Tests point them at temporary
//! directories, so no test reaches the real home directory.

use std::path::PathBuf;
use std::sync::RwLock;

use crate::{Error, Result};

/// The directories [`use_dirs`] set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirs {
    pub state: PathBuf,
    pub cache: PathBuf,
}

static DIRS: RwLock<Option<Dirs>> = RwLock::new(None);

/// Makes `dirs` this process's state and cache directories.
pub fn use_dirs(dirs: Dirs) {
    *DIRS.write().unwrap_or_else(|e| e.into_inner()) = Some(dirs);
}

fn configured() -> Option<Dirs> {
    let dirs = DIRS.read().unwrap_or_else(|e| e.into_inner()).clone();
    #[cfg(test)]
    return dirs.or_else(|| Some(test_dirs()));
    #[cfg(not(test))]
    dirs
}

/// Core's own tests never reach the real home directory: unless a test
/// says otherwise, both directories are in one temporary directory.
#[cfg(test)]
fn test_dirs() -> Dirs {
    static ROOT: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    let root = ROOT.get_or_init(|| tempfile::tempdir().expect("a temporary directory")).path();
    Dirs { state: root.join("state"), cache: root.join("cache") }
}

fn var(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// sk1ll's state directory: `$XDG_STATE_HOME/sk1ll` by default (the data
/// directory's `sk1ll` where there is no state directory, as on macOS).
pub fn state_dir() -> Result<PathBuf> {
    if let Some(dirs) = configured() {
        return Ok(dirs.state);
    }
    var("SK1LL_STATE_DIR")
        .or_else(|| dirs::state_dir().or_else(dirs::data_dir).map(|d| d.join("sk1ll")))
        .ok_or_else(|| Error::Invalid("cannot determine the state directory".into()))
}

/// sk1ll's cache directory: `$XDG_CACHE_HOME/sk1ll` by default.
pub fn cache_dir() -> Result<PathBuf> {
    if let Some(dirs) = configured() {
        return Ok(dirs.cache);
    }
    var("SK1LL_CACHE_DIR")
        .or_else(|| dirs::cache_dir().map(|d| d.join("sk1ll")))
        .ok_or_else(|| Error::Invalid("cannot determine the cache directory".into()))
}
