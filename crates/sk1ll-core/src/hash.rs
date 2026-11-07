use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::git::path_bytes;
use crate::sync::MARKER;
use crate::{IoExt, Result};

/// Stable content hash of a skill directory (file paths + contents), ignoring
/// `.git` and sk1ll's own marker file.
///
/// Paths and symlink targets are hashed as their exact bytes on Unix, where
/// `\` is an ordinary file-name character; only on Windows do `\` separators
/// in paths count as `/`. For every tree whose paths are valid UTF-8 without
/// a `\` and whose symlink targets are valid UTF-8 (all but a handful of real
/// trees), this is byte-for-byte the input of the
/// earlier algorithm (lossy UTF-8, `\` always read as `/`), so the hashes in
/// existing `.sk1ll-managed` markers and `sk1ll.lock` stay valid without a
/// version prefix. The trees whose hash does change are exactly those the
/// old hash couldn't tell apart from another tree (`a\b` vs `a/b`), so an old
/// marker for one is ambiguous and showing it as a conflict is the safe
/// reading.
pub fn hash_dir(dir: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    let walker = WalkDir::new(dir)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || (e.file_name() != ".git" && e.file_name() != MARKER));
    for entry in walker {
        let entry = entry.map_err(std::io::Error::from).at(dir)?;
        let rel = entry.path().strip_prefix(dir).unwrap_or(entry.path());
        let rel = path_bytes(rel);
        let ft = entry.file_type();
        let (tag, bytes) = if ft.is_symlink() {
            let to = fs::read_link(entry.path()).at(entry.path())?;
            (b'l', target_bytes(&to))
        } else if ft.is_file() {
            (b'f', fs::read(entry.path()).at(entry.path())?)
        } else {
            continue;
        };
        hasher.update([tag]);
        hasher.update(&rel);
        hasher.update([0]);
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(format!("sha256:{}", hex(&hasher.finalize())))
}

/// A symlink's target as hashed: exact bytes on Unix, unchanged text elsewhere.
fn target_bytes(to: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        path_bytes(to)
    }
    #[cfg(not(unix))]
    {
        to.to_string_lossy().into_owned().into_bytes()
    }
}

pub fn hash_str(text: &str) -> String {
    format!("sha256:{}", hex(&Sha256::digest(text.as_bytes())))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The algorithm before paths were hashed byte-exactly, kept to prove that
    /// markers and lock entries it wrote still verify.
    fn legacy_hash_dir(dir: &Path) -> String {
        let mut hasher = Sha256::new();
        let walker = WalkDir::new(dir)
            .sort_by_file_name()
            .into_iter()
            .filter_entry(|e| e.depth() == 0 || (e.file_name() != ".git" && e.file_name() != MARKER));
        for entry in walker {
            let entry = entry.unwrap();
            let rel = entry.path().strip_prefix(dir).unwrap().to_string_lossy().replace('\\', "/");
            let (tag, bytes) = if entry.file_type().is_symlink() {
                (b'l', fs::read_link(entry.path()).unwrap().to_string_lossy().into_owned().into_bytes())
            } else if entry.file_type().is_file() {
                (b'f', fs::read(entry.path()).unwrap())
            } else {
                continue;
            };
            hasher.update([tag]);
            hasher.update(&rel);
            hasher.update([0]);
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(&bytes);
        }
        format!("sha256:{}", hex(&hasher.finalize()))
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn hashes_of_ordinary_trees_are_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write(&dir.join("SKILL.md"), "---\nname: pdf\n---\n");
        write(&dir.join("refs/deep/é.md"), "x\n");
        write(&dir.join(MARKER), "ignored\n");
        #[cfg(unix)]
        std::os::unix::fs::symlink("refs/deep", dir.join("link")).unwrap();
        assert_eq!(hash_dir(dir).unwrap(), legacy_hash_dir(dir));
    }

    #[cfg(unix)]
    #[test]
    fn backslash_is_part_of_a_unix_file_name() {
        let tmp = tempfile::tempdir().unwrap();
        let (flat, nested) = (tmp.path().join("flat"), tmp.path().join("nested"));
        write(&flat.join("a\\b"), "same\n");
        write(&nested.join("a/b"), "same\n");
        assert_ne!(hash_dir(&flat).unwrap(), hash_dir(&nested).unwrap());
        // Non-UTF-8 names and symlink targets aren't collapsed by lossy decoding.
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let (x, y) = (tmp.path().join("x"), tmp.path().join("y"));
        write(&x.join(OsStr::from_bytes(b"f\xfe")), "same\n");
        write(&y.join(OsStr::from_bytes(b"f\xff")), "same\n");
        assert_ne!(hash_dir(&x).unwrap(), hash_dir(&y).unwrap());
        std::os::unix::fs::symlink(OsStr::from_bytes(b"t\xfe"), x.join("l")).unwrap();
        std::os::unix::fs::symlink(OsStr::from_bytes(b"t\xff"), y.join("l")).unwrap();
        fs::remove_file(y.join(OsStr::from_bytes(b"f\xff"))).unwrap();
        write(&y.join(OsStr::from_bytes(b"f\xfe")), "same\n");
        assert_ne!(hash_dir(&x).unwrap(), hash_dir(&y).unwrap());
    }
}
