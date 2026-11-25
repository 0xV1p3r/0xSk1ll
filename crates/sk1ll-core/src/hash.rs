use std::fs;
use std::io::{Read, Seek};
use std::path::Path;

use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::git::path_bytes;
use crate::sync::{Exclusion, SKILL_CONTENT_EXCLUSIONS, excluded_from_personal_skill, excluded_from_skill};
use crate::{Error, IoExt, Result};

/// Hash of the content deployable from a personal skill directory: file
/// paths and contents, without what [`excluded_from_personal_skill`] leaves
/// out: `.git`, the placement marker at the root of a harness copy, and the
/// skill's Item ID sidecar.
///
/// Paths and symlink targets are hashed as their exact bytes on Unix, where
/// `\` is an ordinary file-name character; only on Windows do `\` separators
/// in paths count as `/`. For every tree whose paths are valid UTF-8 without
/// a `\` and whose symlink targets are valid UTF-8 (all but a handful of real
/// trees), this is byte-for-byte the input of the
/// earlier algorithm (lossy UTF-8, `\` always read as `/`), so the hashes in
/// `sk1ll.lock` stay valid without a version prefix for ordinary trees.
pub fn hash_dir(dir: &Path) -> Result<String> {
    hash_dir_buffered(dir, excluded_from_personal_skill, &mut [0; BUFFER])
}

/// Hash of all vendored content, including names that have meaning elsewhere.
pub(crate) fn vendor_hash(dir: &Path) -> Result<String> {
    hash_dir_buffered(dir, |_, _| false, &mut [0; BUFFER])
}

/// Hash of a harness copy, or of an imported skill: without what
/// [`excluded_from_skill`] leaves out. A personal skill's sidecar was
/// filtered at copy time.
pub(crate) fn copy_hash(dir: &Path) -> Result<String> {
    hash_dir_buffered(dir, excluded_from_skill, &mut [0; BUFFER])
}

/// How much of a file is in memory at a time while it is hashed.
const BUFFER: usize = 64 * 1024;

fn hash_dir_buffered(dir: &Path, exclude: Exclusion, buf: &mut [u8]) -> Result<String> {
    let mut hasher = Sha256::new();
    let walker = WalkDir::new(dir)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !exclude(e.depth(), e.file_name()));
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

/// Fingerprints the entire recovery payload, including directory entries,
/// their mode bits and (on Linux) extended attributes. Skill hash
/// exclusions do not apply here.
pub(crate) fn hash_payload(path: &Path) -> Result<String> {
    Ok(Payload::of(path, None)?.hash)
}

/// A payload fingerprint and how each entry it covers looked while it was
/// taken, so a later step can tell whether any of them changed since.
#[derive(PartialEq, Eq)]
pub(crate) struct Payload {
    pub(crate) hash: String,
    /// In walk order: every directory before what it contains.
    entries: Vec<(std::path::PathBuf, bool, PayloadMetadata)>,
}

impl Payload {
    /// `exclude`: leave out what a skill copy leaves out below the root
    /// (see [`copied_exclusion`]), but not the root placement marker.
    /// Without such content, this is the same as the whole payload.
    fn of(path: &Path, exclude: Option<Exclusion>) -> Result<Self> {
        let mut hash = Sha256::new();
        let mut buffer = [0; BUFFER];
        let mut entries = Vec::new();
        let walker = WalkDir::new(path)
            .follow_root_links(false)
            .sort_by_file_name()
            .into_iter()
            .filter_entry(|e| e.depth() == 0 || exclude.is_none_or(|exclude| !exclude(e.depth(), e.file_name())));
        for entry in walker {
            let entry = entry.map_err(std::io::Error::from).at(path)?;
            let at = entry.path();
            let rel = path_bytes(at.strip_prefix(path).unwrap_or(at));
            hash.update((rel.len() as u64).to_le_bytes());
            hash.update(rel);
            let meta = fs::symlink_metadata(at).at(at)?;
            entries.push((at.to_path_buf(), meta.is_dir(), PayloadMetadata::of(at, &meta)?));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                hash.update(meta.permissions().mode().to_le_bytes());
            }
            #[cfg(not(unix))]
            hash.update([u8::from(meta.permissions().readonly())]);
            hash_xattrs(&mut hash, at).at(at)?;
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
        // A write to an early file may have happened while we read a later
        // one. Recheck every entry, including directories for new names.
        for (at, _, before) in &entries {
            let after = fs::symlink_metadata(at).at(at)?;
            if *before != PayloadMetadata::of(at, &after)? {
                return Err(Error::Changed(format!("{} changed while fingerprinting its payload", at.display())));
            }
        }
        Ok(Self { hash: format!("sha256:{}", hex(&hash.finalize())), entries })
    }
}

/// [`hash_payload`] without what skill copies leave out below the root
/// (`.git`): what a copy of `path` is the whole payload of.
pub(crate) fn hash_copied_payload(path: &Path) -> Result<String> {
    Ok(Payload::of(path, Some(copied_exclusion(false)))?.hash)
}

/// What a skill copy leaves out of a payload below its root:
/// [`SKILL_CONTENT_EXCLUSIONS`], and (`personal`) the Item ID sidecar
/// [`excluded_from_personal_skill`] leaves out.
pub(crate) fn copied_exclusion(personal: bool) -> Exclusion {
    fn copied(_: usize, name: &std::ffi::OsStr) -> bool {
        SKILL_CONTENT_EXCLUSIONS.iter().any(|x| name == *x)
    }
    fn personal_copied(depth: usize, name: &std::ffi::OsStr) -> bool {
        copied(depth, name) || name == crate::skill::ID_FILE
    }
    if personal { personal_copied } else { copied }
}

/// Runs `read` (a copy or a read of `path`) between two looks at the
/// payload, leaving out what skill copies leave out, and returns the
/// fingerprint of exactly what `read` saw: a change to any entry while it
/// ran, even one undone since, fails with [`Error::Changed`].
pub(crate) fn read_payload<T>(path: &Path, read: impl FnOnce() -> Result<T>) -> Result<(String, T)> {
    read_payload_without(path, copied_exclusion(false), read)
}

/// [`read_payload`], leaving out what `exclude` (see [`copied_exclusion`]) does.
pub(crate) fn read_payload_without<T>(
    path: &Path,
    exclude: Exclusion,
    read: impl FnOnce() -> Result<T>,
) -> Result<(String, T)> {
    let before = Payload::of(path, Some(exclude))?;
    let value = read()?;
    if Payload::of(path, Some(exclude))? != before {
        return Err(Error::Changed(format!("{} changed while it was read", path.display())));
    }
    Ok((before.hash, value))
}

/// Deletes the payload at `path` if it is still the one fingerprinted
/// `expected`, without ever deleting recursively: each file or symlink the
/// fingerprint covered is unlinked only while it is unchanged, then each
/// directory is removed only while empty. Anything that does not match, or
/// that appears meanwhile, stops the removal and stays, with whatever
/// directories hold it: `false`. A file someone renamed away is not lost and
/// does not stop it.
pub(crate) fn remove_payload(path: &Path, expected: &str) -> Result<bool> {
    let payload = match Payload::of(path, None) {
        Ok(payload) => payload,
        Err(Error::Changed(_)) => return Ok(false),
        Err(e) => return Err(e),
    };
    if payload.hash != expected {
        return Ok(false);
    }
    #[cfg(test)]
    tests::before_removal(path);
    for (at, dir, before) in payload.entries.iter().rev() {
        let removed = if *dir {
            fs::remove_dir(at)
        } else {
            match fs::symlink_metadata(at) {
                Ok(after) if PayloadMetadata::of(at, &after)? == *before => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Ok(_) => return Ok(false),
                Err(e) => return Err(e).at(at),
            }
            // A directory symlink on Windows needs remove_dir.
            fs::remove_file(at).or_else(|e| if cfg!(windows) { fs::remove_dir(at) } else { Err(e) })
        };
        match removed {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e)
                if *dir
                    && matches!(
                        e.kind(),
                        std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::AlreadyExists
                    ) =>
            {
                return Ok(false);
            }
            Err(e) => return Err(e).at(at),
        }
    }
    Ok(true)
}

/// Feeds the extended attributes of `path` (not following a symlink), sorted
/// by name, to `hasher`. ACLs are among them on Linux.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn hash_xattrs(hasher: &mut Sha256, path: &Path) -> std::io::Result<()> {
    use rustix::io::Errno;
    const MISSING: Errno = Errno::NODATA;
    // A size query, then the read; a value that grew meanwhile is asked again.
    fn read(
        size: impl Fn() -> rustix::io::Result<usize>,
        get: impl Fn(&mut [u8]) -> rustix::io::Result<usize>,
    ) -> rustix::io::Result<Vec<u8>> {
        loop {
            let mut buf = vec![0; size()?];
            if buf.is_empty() {
                return Ok(buf);
            }
            match get(&mut buf) {
                Ok(n) => {
                    buf.truncate(n);
                    return Ok(buf);
                }
                Err(Errno::RANGE) => {}
                Err(e) => return Err(e),
            }
        }
    }
    let names = match read(|| rustix::fs::llistxattr_query(path), |buf| rustix::fs::llistxattr(path, buf)) {
        Ok(names) => names,
        Err(e) if e == Errno::NOTSUP || e == Errno::OPNOTSUPP => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let mut names: Vec<&[u8]> = names.split(|b| *b == 0).filter(|n| !n.is_empty()).collect();
    names.sort_unstable();
    for name in names {
        let name_c = std::ffi::CString::new(name).map_err(std::io::Error::other)?;
        let value = match read(
            || rustix::fs::lgetxattr_query(path, name_c.as_c_str()),
            |buf| rustix::fs::lgetxattr(path, name_c.as_c_str(), buf),
        ) {
            Ok(value) => value,
            // One removed meanwhile fails the final metadata recheck instead.
            Err(e) if e == MISSING => continue,
            Err(e) => return Err(e.into()),
        };
        hasher.update(b"x");
        hasher.update((name.len() as u64).to_le_bytes());
        hasher.update(name);
        hasher.update((value.len() as u64).to_le_bytes());
        hasher.update(&value);
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn hash_xattrs(_: &mut Sha256, _: &Path) -> std::io::Result<()> {
    Ok(())
}

#[derive(PartialEq, Eq)]
struct PayloadMetadata {
    size: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    unix: (u64, u64, u32, i64, i64),
    #[cfg(windows)]
    windows: (u32, u64, i64, u32),
    #[cfg(not(any(unix, windows)))]
    created: Option<std::time::SystemTime>,
    #[cfg(not(any(unix, windows)))]
    readonly: bool,
}

impl PayloadMetadata {
    fn of(_path: &Path, meta: &fs::Metadata) -> Result<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            size: meta.len(),
            modified: meta.modified().ok(),
            #[cfg(unix)]
            unix: (meta.dev(), meta.ino(), meta.mode(), meta.ctime(), meta.ctime_nsec()),
            #[cfg(windows)]
            windows: windows_payload_metadata(_path).at(_path)?,
            #[cfg(not(any(unix, windows)))]
            created: meta.created().ok(),
            #[cfg(not(any(unix, windows)))]
            readonly: meta.permissions().readonly(),
        })
    }
}

#[cfg(windows)]
fn windows_payload_metadata(path: &Path) -> std::io::Result<(u32, u64, i64, u32)> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FileBasicInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
    };
    let file = fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let mut identity = BY_HANDLE_FILE_INFORMATION::default();
    let mut basic = FILE_BASIC_INFO::default();
    // SAFETY: the handle is open and both output structures are writable
    // for the duration of the calls.
    unsafe {
        if GetFileInformationByHandle(file.as_raw_handle(), &mut identity) == 0
            || GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileBasicInfo,
                (&mut basic as *mut FILE_BASIC_INFO).cast(),
                std::mem::size_of::<FILE_BASIC_INFO>() as u32,
            ) == 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    let inode = (u64::from(identity.nFileIndexHigh) << 32) | u64::from(identity.nFileIndexLow);
    Ok((identity.dwVolumeSerialNumber, inode, basic.ChangeTime, basic.FileAttributes))
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

/// How a tree looked to `lstat`, without reading any file: each entry's path
/// below the root, device, inode, mode and owner, and for all but
/// directories its size, mtime and ctime. Any write, rename, `chmod`,
/// `chown`, extended attribute change or `utimes` call changes an entry's
/// ctime, which nobody can set by hand, so two equal stamps of a tree taken
/// at different times mean that no entry was changed in between, unless both
/// changes fell within one tick of the filesystem's timestamps (see
/// [`HashCache`]). A directory's timestamps only say that an entry was
/// added or removed, which its entries show, or that a temporary file came
/// and went (git's lock files do, all the time), so they are left out: the
/// extended attributes of a directory are thus the one change a stamp misses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamp {
    digest: String,
    /// Latest mtime or ctime of any entry, in nanoseconds since the epoch.
    newest: i128,
}

impl Stamp {
    /// A fingerprint of the stamp, to compare or record.
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Whether every entry was last changed more than `window` before
    /// `at`: only then does an equal stamp taken later prove that nothing
    /// changed, since a write within one tick of the filesystem's clock may
    /// leave every timestamp as it was.
    pub fn settled(&self, at: std::time::SystemTime, window: std::time::Duration) -> bool {
        at.checked_sub(window)
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .is_some_and(|t| self.newest < t.as_nanos() as i128)
    }

    /// `path` and everything below it that `exclude` keeps (`follow_root`:
    /// a symlink at the root is walked into, as [`hash_dir`] does). A missing
    /// path has a stamp of its own.
    pub fn of(path: &Path, exclude: Option<Exclusion>, follow_root: bool) -> Result<Self> {
        let mut hasher = Sha256::new();
        let mut newest = i128::MIN;
        let walker = WalkDir::new(path)
            .follow_root_links(follow_root)
            .sort_by_file_name()
            .into_iter()
            .filter_entry(|e| e.depth() == 0 || exclude.is_none_or(|exclude| !exclude(e.depth(), e.file_name())));
        for entry in walker {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) if e.depth() == 0 && e.io_error().is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => {
                    return Ok(Self { digest: hash_str("missing"), newest });
                }
                Err(e) => return Err(std::io::Error::from(e)).at(path),
            };
            let meta = entry.metadata().map_err(std::io::Error::from).at(entry.path())?;
            let rel = path_bytes(entry.path().strip_prefix(path).unwrap_or(entry.path()));
            hasher.update((rel.len() as u64).to_le_bytes());
            hasher.update(&rel);
            let (stat, latest) = stat_of(&meta);
            hasher.update(stat);
            newest = newest.max(latest);
        }
        Ok(Self { digest: format!("stamp:{}", hex(&hasher.finalize())), newest })
    }

    /// [`Stamp::of`] `path` as what reads through it see it: a symlink at
    /// the root (or a chain of them) is stamped itself, and so is what it
    /// resolves to, with `exclude` applied there as below a real directory.
    /// Retargeting the link, or changing its target, changes the stamp.
    pub fn through(path: &Path, exclude: Option<Exclusion>) -> Result<Self> {
        let own = Self::of(path, exclude, false)?;
        if !fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return Ok(own);
        }
        let (target, resolved) = match fs::canonicalize(path) {
            Ok(target) => {
                let resolved = Self::of(&target, exclude, false)?;
                (path_bytes(&target), resolved)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                (Vec::new(), Self { digest: hash_str("dangling"), newest: i128::MIN })
            }
            Err(e) => return Err(e).at(path),
        };
        let mut hasher = Sha256::new();
        for part in [own.digest.as_bytes(), &target, resolved.digest.as_bytes()] {
            hasher.update((part.len() as u64).to_le_bytes());
            hasher.update(part);
        }
        Ok(Self { digest: format!("stamp:{}", hex(&hasher.finalize())), newest: own.newest.max(resolved.newest) })
    }

    /// Several stamps as one, each under its label after `header` (what
    /// else the reader of these trees depends on): equal when the header
    /// and every part are, settled when every part is.
    pub fn combine(header: &str, parts: impl IntoIterator<Item = (String, Stamp)>) -> Self {
        let mut hasher = Sha256::new();
        let mut newest = i128::MIN;
        hasher.update((header.len() as u64).to_le_bytes());
        hasher.update(header);
        for (label, stamp) in parts {
            for part in [label.as_str(), stamp.digest.as_str()] {
                hasher.update((part.len() as u64).to_le_bytes());
                hasher.update(part);
            }
            newest = newest.max(stamp.newest);
        }
        Self { digest: format!("stamp:{}", hex(&hasher.finalize())), newest }
    }
}

/// The parts of `meta` a [`Stamp`] compares, and the entry's latest mtime or
/// ctime (none for a directory).
fn stat_of(meta: &fs::Metadata) -> (Vec<u8>, i128) {
    let nanos = |t: std::io::Result<std::time::SystemTime>| {
        t.ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as i128)
    };
    let mut out = Vec::with_capacity(64);
    #[cfg(unix)]
    let changed = {
        use std::os::unix::fs::MetadataExt;
        for n in [meta.dev(), meta.ino(), u64::from(meta.mode()), u64::from(meta.uid()), u64::from(meta.gid())] {
            out.extend_from_slice(&n.to_le_bytes());
        }
        i128::from(meta.ctime()) * 1_000_000_000 + i128::from(meta.ctime_nsec())
    };
    #[cfg(not(unix))]
    let changed = {
        out.push(u8::from(meta.permissions().readonly()));
        out.push(u8::from(meta.is_dir()));
        nanos(meta.created())
    };
    if meta.is_dir() {
        return (out, i128::MIN);
    }
    let modified = nanos(meta.modified());
    for n in [i128::from(meta.len()), modified, changed] {
        out.extend_from_slice(&n.to_le_bytes());
    }
    (out, modified.max(changed))
}

/// Remembers the content hashes of trees by their [`Stamp`], so a tree that
/// is unchanged since it was last hashed is not read again.
///
/// Only planning uses it (what a sync would do, and what to show): every
/// step that replaces or deletes content, P7's [`remove_payload`] and the
/// checks `apply` makes of a destination against its plan, hashes the
/// filesystem as it is, never through this cache. A stale entry can thus at
/// worst make a plan miss a change until the tree's next change, never make
/// sync delete or overwrite something it should not.
///
/// A hash is remembered only if the tree's stamp was the same before and
/// after hashing it, and every entry's mtime and ctime are older than the
/// racy window before hashing began: a file written again within one tick of
/// the filesystem's clock (a second, on some filesystems) after it was
/// stamped could otherwise keep its ctime, size and inode with new content.
/// Such a tree is hashed again each time until it has been left alone for
/// that long.
pub struct HashCache {
    racy: std::time::Duration,
    entries: std::sync::Mutex<std::collections::HashMap<(std::path::PathBuf, Tree), Cached>>,
    hashed: std::sync::atomic::AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Tree {
    Deploy,
    Payload,
}

struct Cached {
    stamp: String,
    hash: String,
    used: bool,
}

impl Default for HashCache {
    fn default() -> Self {
        Self::new()
    }
}

impl HashCache {
    /// Two seconds: more than the coarsest timestamps of a common filesystem.
    pub const RACY_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

    pub fn new() -> Self {
        Self::with_racy_window(Self::RACY_WINDOW)
    }

    /// A cache that remembers hashes of trees changed less than `racy` ago
    /// (tests use zero, to remember trees they just wrote).
    pub fn with_racy_window(racy: std::time::Duration) -> Self {
        Self { racy, entries: Default::default(), hashed: Default::default() }
    }

    /// [`crate::sync::deploy_hash`] of the repo skill at `source`.
    pub fn deploy_hash(&self, source: &Path) -> Result<String> {
        let exclude = crate::sync::source_exclusion(source);
        self.hash(source, Tree::Deploy, || Stamp::of(source, Some(exclude), true), || crate::sync::deploy_hash(source))
    }

    /// [`hash_payload`] of `path`.
    pub(crate) fn payload_hash(&self, path: &Path) -> Result<String> {
        self.hash(path, Tree::Payload, || Stamp::of(path, None, false), || hash_payload(path))
    }

    /// Makes `path`'s payload, as it is now, hash to `hash`: a stale entry,
    /// to show what a plan with one does.
    #[cfg(test)]
    pub(crate) fn remember_payload(&self, path: &Path, hash: &str) {
        let stamp = Stamp::of(path, None, false).unwrap().digest;
        let mut entries = self.entries.lock().unwrap();
        entries.insert((path.to_path_buf(), Tree::Payload), Cached { stamp, hash: hash.into(), used: true });
    }

    /// How many trees were read and hashed, rather than found unchanged.
    pub fn hashed(&self) -> u64 {
        self.hashed.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Forgets every tree, so each is read and hashed again: a full
    /// reconcile does, to bound what a stale entry can hide.
    pub fn clear(&self) {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Forgets every tree not looked up since the last sweep.
    pub fn sweep(&self) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.retain(|_, cached| std::mem::take(&mut cached.used));
    }

    fn hash(
        &self,
        path: &Path,
        tree: Tree,
        stamp: impl Fn() -> Result<Stamp>,
        hash: impl FnOnce() -> Result<String>,
    ) -> Result<String> {
        let started = std::time::SystemTime::now();
        let before = stamp()?;
        let key = (path.to_path_buf(), tree);
        {
            let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(cached) = entries.get_mut(&key)
                && cached.stamp == before.digest
            {
                cached.used = true;
                return Ok(cached.hash.clone());
            }
        }
        let value = hash()?;
        self.hashed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let unchanged = before.settled(started, self.racy) && stamp()? == before;
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if unchanged {
            entries.insert(key, Cached { stamp: before.digest, hash: value.clone(), used: true });
        } else {
            entries.remove(&key);
        }
        Ok(value)
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
pub(crate) mod tests {
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
    pub(crate) fn with_read_hook<T>(hook: impl FnMut(&Path) + 'static, run: impl FnOnce() -> T) -> T {
        MID_READ.with_borrow_mut(|slot| *slot = Some(Box::new(hook)));
        let result = run();
        MID_READ.with_borrow_mut(|slot| *slot = None);
        result
    }

    type Removal = Box<dyn FnOnce(&Path)>;

    thread_local! {
        static BEFORE_REMOVAL: std::cell::RefCell<Option<Removal>> = const { std::cell::RefCell::new(None) };
    }

    /// Called once, after a payload removal matched the fingerprint and
    /// before it unlinks anything.
    pub(crate) fn with_removal_hook<T>(hook: impl FnOnce(&Path) + 'static, run: impl FnOnce() -> T) -> T {
        BEFORE_REMOVAL.with_borrow_mut(|slot| *slot = Some(Box::new(hook)));
        let result = run();
        BEFORE_REMOVAL.with_borrow_mut(|slot| *slot = None);
        result
    }

    pub(super) fn before_removal(path: &Path) {
        if let Some(f) = BEFORE_REMOVAL.with_borrow_mut(Option::take) {
            f(path);
        }
    }

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
        #[cfg(unix)]
        let legacy = "sha256:a21b4f51975084e6c1e3f116f62b3e64394781d07f70dd31ba60c9443335be85";
        #[cfg(not(unix))]
        let legacy = "sha256:8ed5df1f5253f60cbfa5445c2b5337744a48fd90fd08299698a2e4f2a7e59d7e";
        assert_eq!(hash_dir(dir).unwrap(), legacy);
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
            assert_eq!(
                hash_dir_buffered(dir, excluded_from_personal_skill, &mut vec![0; size]).unwrap(),
                legacy,
                "buffer of {size}"
            );
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

    /// A payload removal stops at a file whose content, mode or extended
    /// attributes changed after the fingerprint matched, and at a directory
    /// that gained an entry.
    #[test]
    fn payload_removal_keeps_what_changed_after_the_check() {
        type Edit = fn(&Path);
        let cases: [(&str, Edit); 3] = [
            ("content", |dir| fs::write(dir.join("a/f"), "edited").unwrap()),
            ("new entry", |dir| fs::write(dir.join("a/new"), "new").unwrap()),
            ("mode", |dir| {
                let path = dir.join("a/f");
                let mut permissions = fs::metadata(&path).unwrap().permissions();
                permissions.set_readonly(true);
                fs::set_permissions(path, permissions).unwrap();
            }),
        ];
        for (case, edit) in cases {
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path().join("payload");
            write(&dir.join("a/f"), "f");
            write(&dir.join("b"), "b");
            let expected = hash_payload(&dir).unwrap();
            assert!(!remove_payload(&dir, "sha256:other").unwrap(), "{case}");
            assert_eq!(hash_payload(&dir).unwrap(), expected);
            assert!(!with_removal_hook(edit, || remove_payload(&dir, &expected)).unwrap(), "{case}");
            assert!(dir.join("a").exists(), "{case}");
        }
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("payload");
        write(&dir.join("a/f"), "f");
        let expected = hash_payload(&dir).unwrap();
        assert!(remove_payload(&dir, &expected).unwrap());
        assert!(!dir.exists());
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn extended_attributes_are_part_of_the_payload() {
        use rustix::fs::{XattrFlags, lsetxattr};
        use rustix::io::Errno;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("payload");
        write(&dir.join("f"), "f");
        let before = hash_payload(&dir).unwrap();
        match lsetxattr(dir.join("f"), "user.sk1ll-test", b"1", XattrFlags::empty()) {
            // No user attributes on this filesystem.
            Err(e) if e == Errno::NOTSUP || e == Errno::OPNOTSUPP || e == Errno::PERM => return,
            result => result.unwrap(),
        }
        let tagged = hash_payload(&dir).unwrap();
        assert_ne!(tagged, before);
        let retag = |dir: &Path| lsetxattr(dir.join("f"), "user.sk1ll-test", b"2", XattrFlags::empty()).unwrap();
        assert!(!with_removal_hook(retag, || remove_payload(&dir, &tagged)).unwrap());
        assert!(dir.join("f").exists());
        assert_ne!(hash_payload(&dir).unwrap(), tagged);
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

    /// The cache sees every change `lstat` can: new content written over a
    /// file with its mtime put back (ctime), a file replaced by another with
    /// the same size and mtime (inode), a resize, a new file.
    #[test]
    fn the_cache_rehashes_a_tree_whose_content_changed_with_the_same_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("personal/pdf");
        write(&dir.join("SKILL.md"), "aaaa");
        let cache = HashCache::with_racy_window(std::time::Duration::ZERO);
        let first = cache.deploy_hash(&dir).unwrap();
        assert_eq!((cache.hashed(), cache.deploy_hash(&dir).unwrap()), (1, first.clone()));
        assert_eq!(cache.hashed(), 1, "unchanged: not read again");

        let file = dir.join("SKILL.md");
        let mtime = fs::metadata(&file).unwrap().modified().unwrap();
        let tick = || std::thread::sleep(std::time::Duration::from_millis(30));
        type Change<'a> = Box<dyn Fn() + 'a>;
        let changes: [(&str, &str, Change); 4] = [
            ("rewritten, mtime kept", "bbbb", Box::new(|| fs::write(&file, "bbbb").unwrap())),
            (
                "replaced, mtime kept",
                "cccc",
                Box::new(|| {
                    let other = tmp.path().join("other");
                    fs::write(&other, "cccc").unwrap();
                    fs::File::options().write(true).open(&other).unwrap().set_modified(mtime).unwrap();
                    fs::rename(&other, &file).unwrap();
                }),
            ),
            ("resized", "dddddd", Box::new(|| fs::write(&file, "dddddd").unwrap())),
            ("a new file", "dddddd", Box::new(|| fs::write(dir.join("new.md"), "").unwrap())),
        ];
        let mut previous = first;
        for (n, (case, content, change)) in changes.iter().enumerate() {
            tick();
            change();
            fs::File::options().write(true).open(&file).unwrap().set_modified(mtime).unwrap();
            assert_eq!(fs::metadata(&file).unwrap().modified().unwrap(), mtime);
            assert_eq!(fs::read_to_string(&file).unwrap(), *content);
            let hash = cache.deploy_hash(&dir).unwrap();
            assert_eq!(hash, crate::sync::deploy_hash(&dir).unwrap(), "{case}");
            assert_ne!(hash, previous, "{case}");
            assert_eq!(cache.hashed(), 2 + n as u64, "{case}");
            previous = hash;
        }
        // The cache forgets what no plan looked up since the last sweep.
        cache.sweep();
        cache.sweep();
        cache.deploy_hash(&dir).unwrap();
        assert_eq!(cache.hashed(), 6);
    }

    /// A tree written within the racy window is hashed every time, until it
    /// has been left alone for that long.
    #[test]
    fn a_tree_changed_within_the_racy_window_is_not_remembered() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("payload");
        write(&dir.join("f"), "f");
        let cache = HashCache::with_racy_window(std::time::Duration::from_secs(3600));
        for n in 1..=2 {
            assert_eq!(cache.payload_hash(&dir).unwrap(), hash_payload(&dir).unwrap());
            assert_eq!(cache.hashed(), n);
        }
        let settled = HashCache::with_racy_window(std::time::Duration::ZERO);
        settled.payload_hash(&dir).unwrap();
        settled.payload_hash(&dir).unwrap();
        assert_eq!(settled.hashed(), 1);
        assert_ne!(Stamp::of(&dir, None, false).unwrap(), Stamp::of(&tmp.path().join("none"), None, false).unwrap());
    }
}
