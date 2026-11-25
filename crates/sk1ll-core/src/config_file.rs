//! Writing files next to their destination without trusting what else is
//! there, and rewriting a config file the user may be editing at the same
//! time (`machine.toml`) from one look at it.
//!
//! [`Staged`] is a new file in the destination's directory that only this
//! process created: its name is taken exclusively and never followed, so
//! whatever already holds a temporary name (a file, a symlink to anywhere)
//! is left alone. It is renamed into place and the directory synced. It
//! gets the mode it is given (the replaced file's, see [`existing_mode`]);
//! without one, a new file's mode is left to the umask.
//!
//! [`Snapshot::take`] reads a config file once: its bytes, which file it is
//! and its permissions. A symlinked file (from a dotfiles repo, say) is
//! followed to its target, and the link stays a link. [`Snapshot::replace`]
//! stages the new text with the old file's owner (where this process may
//! set it) and then its exact permissions (0600 for a new file), checks the
//! path still leads to the same target and that the target is still exactly
//! the file read, and renames the new text over it; a missing file is
//! published only while nothing is there. Anything else is
//! [`Error::Changed`]. A save or retargeted link that lands between that
//! last check and the rename is lost (see docs/known-limitations.md). ACLs
//! and extended attributes are not kept.
//!
//! [`read_regular`] reads a file the same way: never blocking on a FIFO,
//! and refusing anything that isn't a regular file.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::{Error, IoExt, Result};

/// Atomically replaces a private state file and syncs its parent directory.
pub fn replace_private(path: &Path, bytes: &[u8]) -> Result<()> {
    Staged::write(path, bytes, Some(0o600))?.replace()
}

/// A file staged next to `dest`; see the module docs. Removed unless published.
#[derive(Debug)]
pub(crate) struct Staged {
    path: PathBuf,
    dest: PathBuf,
    published: bool,
}

impl Staged {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Creates the staging file for `dest` (whose directory must exist):
    /// private while it is written if the caller sets `mode` on it later,
    /// else as the umask has it.
    pub(crate) fn create(dest: &Path, mode: Option<u32>) -> Result<(Self, File)> {
        let dir = dest.parent().ok_or_else(|| Error::Invalid(format!("{} has no parent", dest.display())))?;
        let name = dest
            .file_name()
            .ok_or_else(|| Error::Invalid(format!("{} has no name", dest.display())))?
            .to_string_lossy();
        for n in 0..1000 {
            let path = dir.join(if n == 0 { format!(".{name}.sk1ll-tmp") } else { format!(".{name}.sk1ll-tmp-{n}") });
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(if mode.is_some() { 0o600 } else { 0o666 });
                options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
            }
            #[cfg(not(unix))]
            let _ = mode;
            match options.open(&path) {
                Ok(file) => return Ok((Self { path, dest: dest.to_path_buf(), published: false }, file)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).at(&path),
            }
        }
        Err(Error::Invalid(format!("no free temporary name next to {}", dest.display())))
    }

    /// [`Staged::create`], with `bytes` written, then permissions exactly
    /// `mode` (if given), and synced to disk.
    pub(crate) fn write(dest: &Path, bytes: &[u8], mode: Option<u32>) -> Result<Self> {
        let (staged, mut file) = Self::create(dest, mode)?;
        file.write_all(bytes).at(&staged.path)?;
        if let Some(mode) = mode {
            set_mode(&file, mode).at(&staged.path)?;
        }
        file.sync_all().at(&staged.path)?;
        Ok(staged)
    }

    /// Renames the staged file over its destination.
    pub(crate) fn replace(mut self) -> Result<()> {
        fs::rename(&self.path, &self.dest).at(&self.dest)?;
        self.published = true;
        sync_dir(&self.dest)
    }

    /// Publishes the staged file only while nothing is at its destination
    /// (a hard link, which never replaces); `false` if something is.
    pub(crate) fn place_new(self) -> Result<bool> {
        match fs::hard_link(&self.path, &self.dest) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
            Err(e) => {
                return Err(Error::Invalid(format!(
                    "can't create {} without risking replacing a file created meanwhile (the filesystem \
                     refused a hard link: {e}); create it by hand and try again",
                    self.dest.display()
                )));
            }
        }
        // The destination holds the file now; the staging name goes away on drop.
        sync_dir(&self.dest)?;
        drop(self);
        Ok(true)
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Writes `bytes` into a new file at `path`, a staging name of the
/// caller's own: whatever is there (a crash leftover, or a symlink to
/// anywhere) is unlinked, never followed, and the file is created only if
/// nothing took the name meanwhile.
pub(crate) fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e).at(path),
        _ => {}
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = options.open(path).at(path)?;
    file.write_all(bytes).at(path)?;
    file.sync_all().at(path)
}

/// The permissions of the regular file at `path`, for its replacement to
/// keep; `None` if there is none (a new file's are the umask's).
pub(crate) fn existing_mode(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::symlink_metadata(path).ok().filter(|m| m.is_file()).map(|m| m.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

#[cfg(unix)]
fn set_mode(file: &File, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_: &File, _: u32) -> std::io::Result<()> {
    Ok(())
}

/// Makes a rename or link to `path` durable.
#[cfg(unix)]
pub(crate) fn sync_dir(path: &Path) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    File::open(dir).and_then(|d| d.sync_all()).at(dir)
}

#[cfg(not(unix))]
pub(crate) fn sync_dir(_: &Path) -> Result<()> {
    Ok(())
}

/// What is at a path, looked at without following a symlink there or
/// blocking on a FIFO.
pub(crate) enum Entry {
    Missing,
    File(File, fs::Metadata),
    /// Something that isn't a regular file: a symlink, a directory, a FIFO…
    Other(String),
}

pub(crate) fn open_entry(path: &Path) -> Result<Entry> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(
        &mut options,
        libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
    );
    let file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Entry::Missing),
        #[cfg(unix)]
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Ok(Entry::Other("symlink".into())),
        // A socket can't be opened at all.
        #[cfg(unix)]
        Err(e) if e.raw_os_error() == Some(libc::ENXIO) => return Ok(Entry::Other("socket".into())),
        Err(e) => return Err(e).at(path),
    };
    let meta = file.metadata().at(path)?;
    #[cfg(not(unix))]
    if fs::symlink_metadata(path).at(path)?.file_type().is_symlink() {
        return Ok(Entry::Other("symlink".into()));
    }
    if !meta.is_file() {
        return Ok(Entry::Other(format!("{:?}", meta.file_type())));
    }
    Ok(Entry::File(file, meta))
}

/// The bytes of the regular file at `path` (`None`: missing), read without
/// blocking on a FIFO. With `follow`, a symlink there is followed to its
/// target (a dangling one reads as missing), which must be a regular file
/// too; without, a symlink is refused like anything else that isn't one.
pub(crate) fn read_regular(path: &Path, follow: bool) -> Result<Option<Vec<u8>>> {
    use std::io::Read;
    let target = match fs::symlink_metadata(path) {
        Ok(m) if follow && m.file_type().is_symlink() => match fs::canonicalize(path) {
            Ok(target) => target,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).at(path),
        },
        _ => path.to_path_buf(),
    };
    let mut file = match open_entry(&target)? {
        Entry::Missing => return Ok(None),
        Entry::File(file, _) => file,
        Entry::Other(what) => {
            return Err(Error::Invalid(format!("{} is not a regular file ({what})", path.display())));
        }
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).at(path)?;
    Ok(Some(bytes))
}

pub(crate) fn read_text(path: &Path, follow: bool) -> Result<Option<String>> {
    read_regular(path, follow)?
        .map(|bytes| {
            String::from_utf8(bytes).map_err(|e| Error::Parse { path: path.to_path_buf(), message: e.to_string() })
        })
        .transpose()
}

/// One look at a config file; see the module docs.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// The path as given (made absolute).
    path: PathBuf,
    /// `path`, or the file its symlink pointed to, with its directory resolved.
    target: PathBuf,
    linked: bool,
    seen: Option<Seen>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    /// Device and inode.
    id: (u64, u64),
    mode: u32,
    /// User and group.
    owner: (u32, u32),
    bytes: Vec<u8>,
}

impl Snapshot {
    /// Reads `path` (a missing file reads as absent; a dangling symlink is an error).
    pub fn take(path: &Path) -> Result<Self> {
        let path = std::path::absolute(path).at(path)?;
        let (linked, target) = resolve(&path)?;
        let seen = read(&target)?;
        Ok(Self { path, target, linked, seen })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};

        let mut hash = Sha256::new();
        let target = self.target.as_os_str().as_encoded_bytes();
        hash.update((target.len() as u64).to_le_bytes());
        hash.update(target);
        hash.update([u8::from(self.linked), u8::from(self.seen.is_some())]);
        if let Some(seen) = &self.seen {
            hash.update(seen.id.0.to_le_bytes());
            hash.update(seen.id.1.to_le_bytes());
            hash.update(seen.mode.to_le_bytes());
            hash.update(seen.owner.0.to_le_bytes());
            hash.update(seen.owner.1.to_le_bytes());
            hash.update(&seen.bytes);
        }
        crate::hash::hex(&hash.finalize())
    }

    pub fn exists(&self) -> bool {
        self.seen.is_some()
    }

    /// The text read (empty for a missing file).
    pub fn text(&self) -> Result<String> {
        let bytes = self.seen.as_ref().map(|s| s.bytes.clone()).unwrap_or_default();
        String::from_utf8(bytes).map_err(|e| Error::Parse { path: self.path.clone(), message: e.to_string() })
    }

    fn changed(&self) -> Error {
        Error::Changed(format!(
            "{} changed while sk1ll was changing it; nothing was written: check it and try again",
            self.path.display()
        ))
    }

    /// Replaces the file read with `text`, unless it changed since; see the
    /// module docs.
    pub fn replace(&self, text: &str) -> Result<()> {
        // Still the same link to the same target (or still no link).
        if resolve(&self.path)? != (self.linked, self.target.clone()) {
            return Err(self.changed());
        }
        let dir =
            self.target.parent().ok_or_else(|| Error::Invalid(format!("{} has no parent", self.path.display())))?;
        if self.seen.is_none() {
            fs::create_dir_all(dir).at(dir)?;
        }
        let mode = self.seen.as_ref().map_or(0o600, |s| s.mode);
        let (staged, mut file) = Staged::create(&self.target, Some(mode))?;
        // The owner first: changing it may clear setuid and setgid bits,
        // which the exact mode then puts back.
        #[cfg(unix)]
        if let Some(seen) = &self.seen {
            match std::os::unix::fs::fchown(&file, Some(seen.owner.0), Some(seen.owner.1)) {
                // Only root, or the owner keeping its own group, may.
                Err(e) if e.raw_os_error() == Some(libc::EPERM) => {}
                result => result.at(&self.target)?,
            }
        }
        file.write_all(text.as_bytes()).at(&self.target)?;
        set_mode(&file, mode).at(&self.target)?;
        file.sync_all().at(&self.target)?;
        drop(file);
        crate::failpoint!(ConfigBeforeCheck, &self.path);
        // Still the same link to the same target, and that still exactly the file read.
        if resolve(&self.path).ok() != Some((self.linked, self.target.clone()))
            || !read(&self.target).is_ok_and(|seen| seen == self.seen)
        {
            return Err(self.changed());
        }
        crate::failpoint!(ConfigBeforePublish, &self.path);
        match &self.seen {
            Some(_) => staged.replace(),
            None if staged.place_new()? => Ok(()),
            None => Err(self.changed()),
        }
    }
}

/// Whether `path` is a symlink, and the file it means: its target, or
/// `path` itself in its resolved directory.
fn resolve(path: &Path) -> Result<(bool, PathBuf)> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => Ok((true, fs::canonicalize(path).at(path)?)),
        _ => {
            let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
                return Err(Error::Invalid(format!("{} is not a file path", path.display())));
            };
            match fs::canonicalize(dir) {
                Ok(dir) => Ok((false, dir.join(name))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((false, crate::machine::real(dir).join(name))),
                Err(e) => Err(e).at(dir),
            }
        }
    }
}

/// The regular file at `path`, never following a symlink there.
fn read(path: &Path) -> Result<Option<Seen>> {
    use std::io::Read;
    let (mut file, meta) = match open_entry(path)? {
        Entry::Missing => return Ok(None),
        Entry::File(file, meta) => (file, meta),
        Entry::Other(what) => return Err(Error::Invalid(format!("{} is not a regular file ({what})", path.display()))),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).at(path)?;
    #[cfg(unix)]
    let (id, mode, owner) = {
        use std::os::unix::fs::MetadataExt;
        ((meta.dev(), meta.ino()), meta.mode() & 0o7777, (meta.uid(), meta.gid()))
    };
    #[cfg(not(unix))]
    let (id, mode, owner) = {
        let _ = meta;
        ((0, 0), 0o600, (0, 0))
    };
    Ok(Some(Seen { id, mode, owner, bytes }))
}
