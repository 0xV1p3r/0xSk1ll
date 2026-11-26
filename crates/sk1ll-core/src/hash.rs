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
    crate::failpoint!(HashBeforeRemoval, path);
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

/// Whether `path` (not following a symlink) has extended attributes Git
/// can't restore: any but its SELinux label, which policy gives every new
/// file. ACLs are among them on Linux.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn has_local_xattrs(path: &Path) -> std::io::Result<bool> {
    use rustix::io::Errno;
    let names = loop {
        let mut buf = vec![
            0;
            match rustix::fs::llistxattr_query(path) {
                Ok(size) => size,
                Err(e) if e == Errno::NOTSUP || e == Errno::OPNOTSUPP => return Ok(false),
                Err(e) => return Err(e.into()),
            }
        ];
        match rustix::fs::llistxattr(path, &mut buf) {
            Ok(n) => {
                buf.truncate(n);
                break buf;
            }
            Err(Errno::RANGE) => {}
            Err(e) => return Err(e.into()),
        }
    };
    Ok(names.split(|b| *b == 0).any(|name| !name.is_empty() && name != b"security.selinux"))
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(crate) fn has_local_xattrs(_: &Path) -> std::io::Result<bool> {
    Ok(false)
}

/// The permission bits Git gives what it checks out under this process's
/// umask: a directory or `executable` file `0777`, another file `0666`,
/// less the umask. Git records only the executable bit, so any other mode
/// (a private `0600`, say) is one it can't give back.
#[cfg(unix)]
pub(crate) fn checkout_mode(executable: bool) -> u32 {
    (if executable { 0o777 } else { 0o666 }) & !umask()
}

/// The process's umask, read without changing it (`/proc/self/status`);
/// `022` where that can't be read.
#[cfg(unix)]
fn umask() -> u32 {
    static UMASK: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *UMASK.get_or_init(|| {
        fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| {
                let line = status.lines().find_map(|line| line.strip_prefix("Umask:"))?;
                u32::from_str_radix(line.trim(), 8).ok()
            })
            .unwrap_or(0o022)
    })
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
    crate::failpoint!(HashMidRead, path);
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
