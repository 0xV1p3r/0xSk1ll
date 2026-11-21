use std::fs;
use std::io::{Read, Seek};
use std::path::Path;

use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::git::path_bytes;
use crate::sync::excluded_from_skill;
use crate::{IoExt, Result};

/// Stable content hash of a skill directory (file paths + contents), without
/// what [`excluded_from_skill`] leaves out: `.git`, and the placement marker
/// at the root of a harness copy.
///
/// Paths and symlink targets are hashed as their exact bytes on Unix, where
/// `\` is an ordinary file-name character; only on Windows do `\` separators
/// in paths count as `/`. For every tree whose paths are valid UTF-8 without
/// a `\` and whose symlink targets are valid UTF-8 (all but a handful of real
/// trees), this is byte-for-byte the input of the
/// earlier algorithm (lossy UTF-8, `\` always read as `/`), so the hashes in
/// `sk1ll.lock` stay valid without a version prefix for ordinary trees.
pub fn hash_dir(dir: &Path) -> Result<String> {
    hash_dir_buffered(dir, &mut [0; BUFFER])
}

/// How much of a file is in memory at a time while it is hashed.
const BUFFER: usize = 64 * 1024;

fn hash_dir_buffered(dir: &Path, buf: &mut [u8]) -> Result<String> {
    let mut hasher = Sha256::new();
    let walker = WalkDir::new(dir)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !excluded_from_skill(e.depth(), e.file_name()));
    for entry in walker {
        let entry = entry.map_err(std::io::Error::from).at(dir)?;
        let rel = entry.path().strip_prefix(dir).unwrap_or(entry.path());
        let rel = path_bytes(rel);
        let ft = entry.file_type();
        if ft.is_symlink() {
            let to = target_bytes(&fs::read_link(entry.path()).at(entry.path())?);
            hasher.update(b"l");
            hasher.update(&rel);
            hasher.update([0]);
            hasher.update((to.len() as u64).to_le_bytes());
            hasher.update(&to);
        } else if ft.is_file() {
            hasher.update(b"f");
            hasher.update(&rel);
            hasher.update([0]);
            hash_file(&mut hasher, entry.path(), buf).at(entry.path())?;
        }
    }
    Ok(format!("sha256:{}", hex(&hasher.finalize())))
}

/// Fingerprints the entire recovery payload, including directory entries
/// and their mode bits. Skill hash exclusions do not apply here.
pub(crate) fn hash_payload(path: &Path) -> Result<String> {
    let mut hash = Sha256::new();
    let mut buffer = [0; BUFFER];
    for entry in WalkDir::new(path).follow_root_links(false).sort_by_file_name() {
        let entry = entry.map_err(std::io::Error::from).at(path)?;
        let at = entry.path();
        let rel = path_bytes(at.strip_prefix(path).unwrap_or(at));
        hash.update((rel.len() as u64).to_le_bytes());
        hash.update(rel);
        let meta = fs::symlink_metadata(at).at(at)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            hash.update(meta.permissions().mode().to_le_bytes());
        }
        #[cfg(not(unix))]
        hash.update([u8::from(meta.permissions().readonly())]);
        if meta.is_symlink() {
            hash.update(b"l");
            let target = target_bytes(&fs::read_link(at).at(at)?);
            hash.update((target.len() as u64).to_le_bytes());
            hash.update(target);
        } else if meta.is_dir() {
            hash.update(b"d");
        } else if meta.is_file() {
            hash.update(b"f");
            hash_file(&mut hash, at, &mut buffer).at(at)?;
        } else {
            return Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "unsupported recovery entry")).at(at);
        }
    }
    Ok(format!("sha256:{}", hex(&hash.finalize())))
}

/// Feeds a file's length, then its bytes, to `hasher` without holding the
/// whole file in memory.
///
/// The length goes in first, so it is taken from the open file before reading.
/// If the bytes then read don't add up to it (the file was written meanwhile,
/// or reports no size), what was fed is dropped and the file is read whole
/// instead, as it always used to be: the length hashed is then that of the
/// bytes hashed, never a mix of two states of the file.
fn hash_file(hasher: &mut Sha256, path: &Path, buf: &mut [u8]) -> std::io::Result<()> {
    let mut file = fs::File::open(path)?;
    let len = file.metadata()?.len();
    let mut streamed = hasher.clone();
    streamed.update(len.to_le_bytes());
    let mut read = 0u64;
    #[cfg(test)]
    tests::mid_read(path);
    // Reading stops past `len`: a file that grew is not read to its end twice.
    while read <= len {
        match file.read(buf) {
            Ok(0) => break,
            Ok(n) => {
                streamed.update(&buf[..n]);
                read += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    if read == len {
        *hasher = streamed;
        return Ok(());
    }
    file.rewind()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(&bytes);
    Ok(())
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
    hash_bytes(text.as_bytes())
}

pub(crate) fn hash_bytes(bytes: &[u8]) -> String {
    format!("sha256:{}", hex(&Sha256::digest(bytes)))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The algorithm before paths were hashed byte-exactly, with the names
    /// it left out at any depth, kept to prove that lock entries it wrote
    /// for ordinary trees still verify.
    fn legacy_hash_dir(dir: &Path) -> String {
        let mut hasher = Sha256::new();
        let walker = WalkDir::new(dir)
            .sort_by_file_name()
            .into_iter()
            .filter_entry(|e| e.depth() == 0 || (e.file_name() != ".git" && e.file_name() != ".sk1ll-managed"));
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

    type MidRead = Box<dyn FnMut(&Path)>;

    thread_local! {
        static MID_READ: std::cell::RefCell<Option<MidRead>> = const { std::cell::RefCell::new(None) };
    }

    /// Called once a file's length is taken and before its bytes are read, to
    /// simulate a concurrent write deterministically.
    pub(super) fn mid_read(path: &Path) {
        if let Some(mut f) = MID_READ.with_borrow_mut(Option::take) {
            f(path);
            MID_READ.with_borrow_mut(|h| *h = Some(f));
        }
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
        write(&dir.join("refs/.git/HEAD"), "ignored\n");
        #[cfg(unix)]
        std::os::unix::fs::symlink("refs/deep", dir.join("link")).unwrap();
        assert_eq!(hash_dir(dir).unwrap(), legacy_hash_dir(dir));
    }

    /// Only `.git` and the placement marker at the root are left out: sk1ll's
    /// other names below a skill's root are its own content.
    #[test]
    fn only_git_and_the_root_placement_marker_are_left_out() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write(&dir.join("SKILL.md"), "---\nname: pdf\n---\n");
        let before = hash_dir(dir).unwrap();
        write(&dir.join(crate::sync::PLACEMENT_MARKER), "token\n");
        write(&dir.join(".git/HEAD"), "ref\n");
        write(&dir.join("refs/.git/HEAD"), "ref\n");
        assert_eq!(hash_dir(dir).unwrap(), before);
        for content in [".sk1ll-state/index.json", "refs/.sk1ll-state/f", "refs/.sk1ll-placement", ".sk1ll-tmp/x"] {
            let path = dir.join(content);
            write(&path, "content\n");
            assert_ne!(hash_dir(dir).unwrap(), before, "{content}");
            fs::remove_file(&path).unwrap();
            fs::remove_dir(path.parent().unwrap()).ok();
        }
    }

    #[test]
    fn files_larger_than_the_read_buffer_hash_as_before() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let bytes: Vec<u8> = (0..3 * BUFFER + 17).map(|i| (i % 251) as u8).collect();
        fs::write(dir.join("big.bin"), &bytes).unwrap();
        fs::write(dir.join("exact.bin"), &bytes[..2 * BUFFER]).unwrap();
        fs::write(dir.join("empty"), "").unwrap();
        write(&dir.join("refs/small.md"), "x\n");
        let legacy = legacy_hash_dir(dir);
        assert_eq!(hash_dir(dir).unwrap(), legacy);
        // Whatever the buffer, down to a byte at a time.
        for size in [1, 7, BUFFER - 1, 4 * BUFFER] {
            assert_eq!(hash_dir_buffered(dir, &mut vec![0; size]).unwrap(), legacy, "buffer of {size}");
        }
    }

    #[test]
    fn a_file_resized_while_hashed_never_yields_a_mixed_hash() {
        let grown: Vec<u8> = (0..2 * BUFFER + 5).map(|i| (i % 241) as u8).collect();
        for (case, after) in [("grown", &grown[..]), ("shrunk", &grown[..3]), ("emptied", &grown[..0])] {
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path().to_path_buf();
            fs::write(dir.join("a.bin"), &grown[..BUFFER + 9]).unwrap();
            fs::write(dir.join("b.md"), "untouched\n").unwrap();
            let before = legacy_hash_dir(&dir);
            let (content, mut once) = (after.to_vec(), true);
            MID_READ.with_borrow_mut(|h| {
                *h = Some(Box::new(move |path| {
                    if std::mem::take(&mut once) {
                        assert!(path.ends_with("a.bin"));
                        fs::write(path, &content).unwrap();
                    }
                }))
            });
            let hash = hash_dir(&dir);
            MID_READ.with_borrow_mut(|h| *h = None);
            // The hash is that of the tree as rewritten, the only content
            // read; not the old length in front of the new bytes.
            assert_eq!(hash.unwrap(), legacy_hash_dir(&dir), "{case}");
            assert_ne!(legacy_hash_dir(&dir), before, "{case}");
        }
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
