//! One name for a file, whatever name it is reached by: what sk1ll keys
//! the state it keeps about a file on (the machine lock, a pending Harness
//! rename), so that two names for one file share it.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::{Error, IoExt, Result};

/// As the kernel's limit on following symlinks.
const HOPS: usize = 40;

/// The file a write to `path` changes, as one name for all its names: every
/// symlink resolved, `path`'s own included even when it dangles (a write
/// through it creates its target), and the deepest directory that exists
/// canonical. Nothing is created.
pub fn real_path(path: &Path) -> Result<PathBuf> {
    let mut path = std::path::absolute(path).at(path)?;
    for _ in 0..HOPS {
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(Error::Invalid(format!("{} names no file", path.display())));
        };
        let here = canonical_dir(dir)?.join(name);
        match std::fs::symlink_metadata(&here) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = std::fs::read_link(&here).at(&here)?;
                path = here.parent().expect("a file in a directory").join(target);
            }
            _ => return Ok(here),
        }
    }
    Err(Error::Invalid(format!("{}: too many levels of symbolic links", path.display())))
}

/// A file name for state kept about the file at `path`, the same for all
/// its names: the SHA-256 of its [`real_path`], in hex.
pub fn key(path: &Path) -> Result<String> {
    let digest = Sha256::digest(crate::git::path_bytes(&real_path(path)?));
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

/// `dir` canonical as far as it exists, the rest as given.
fn canonical_dir(dir: &Path) -> Result<PathBuf> {
    let mut missing = Vec::new();
    let mut existing = dir;
    loop {
        match existing.canonicalize() {
            Ok(real) => return Ok(missing.iter().rev().fold(real, |p, name| p.join(name))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let (Some(parent), Some(name)) = (existing.parent(), existing.file_name()) else {
                    return Err(e).at(dir);
                };
                missing.push(name.to_os_string());
                existing = parent;
            }
            Err(e) => return Err(e).at(existing),
        }
    }
}
