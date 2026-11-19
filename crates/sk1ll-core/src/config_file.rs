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
fn sync_dir(path: &Path) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    File::open(dir).and_then(|d| d.sync_all()).at(dir)
}

#[cfg(not(unix))]
fn sync_dir(_: &Path) -> Result<()> {
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
        format!("{:x}", hash.finalize())
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
        #[cfg(test)]
        hook(Point::BeforeCheck);
        // Still the same link to the same target, and that still exactly the file read.
        if resolve(&self.path).ok() != Some((self.linked, self.target.clone()))
            || !read(&self.target).is_ok_and(|seen| seen == self.seen)
        {
            return Err(self.changed());
        }
        #[cfg(test)]
        hook(Point::BeforePublish);
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

/// Where a test runs its hook in [`Snapshot::replace`].
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Point {
    /// The new text is staged; the last check is next.
    BeforeCheck,
    /// Right before the new text is renamed in, after the last check.
    BeforePublish,
}

#[cfg(test)]
type HookFn = Box<dyn Fn(Point)>;

#[cfg(test)]
thread_local! {
    static HOOK: std::cell::RefCell<Option<HookFn>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn hook(point: Point) {
    HOOK.with(|h| {
        if let Some(h) = h.borrow().as_ref() {
            h(point);
        }
    });
}

/// Runs `f` with `hook` set at `point`.
#[cfg(test)]
fn with_hook<T>(point: Point, hook: impl Fn() + 'static, f: impl FnOnce() -> T) -> T {
    HOOK.with(|h| {
        *h.borrow_mut() = Some(Box::new(move |p| {
            if p == point {
                hook()
            }
        }))
    });
    let out = f();
    HOOK.with(|h| *h.borrow_mut() = None);
    out
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> =
            fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        names
    }

    #[test]
    fn creates_config_under_a_missing_directory_beneath_a_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let path = link.join("new/machine.toml");
        Snapshot::take(&path).unwrap().replace("id = 'box'").unwrap();
        assert_eq!(fs::read_to_string(target.join("new/machine.toml")).unwrap(), "id = 'box'");
    }

    #[test]
    fn retargeting_during_staging_aborts_publish() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        let path = tmp.path().join("machine.toml");
        fs::write(&first, "first").unwrap();
        fs::write(&second, "second").unwrap();
        symlink(&first, &path).unwrap();
        let snap = Snapshot::take(&path).unwrap();
        let link = path.clone();
        let newer = second.clone();
        let result = with_hook(
            Point::BeforeCheck,
            move || {
                fs::remove_file(&link).unwrap();
                symlink(&newer, &link).unwrap();
            },
            || snap.replace("ours"),
        );
        assert!(matches!(result, Err(Error::Changed(_))));
        assert_eq!(fs::read_to_string(first).unwrap(), "first");
        assert_eq!(fs::read_to_string(path).unwrap(), "second");
    }

    #[test]
    fn replaces_keeping_mode_and_never_using_an_occupied_temp_name() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("machine.toml");
        fs::write(&path, "old\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o6640)).unwrap();
        // Whatever holds the temporary names stays as it is: a symlink to
        // another file is never followed, a file never truncated.
        let victim = tmp.path().join("victim");
        fs::write(&victim, "victim\n").unwrap();
        std::os::unix::fs::symlink(&victim, tmp.path().join(".machine.toml.sk1ll-tmp")).unwrap();
        fs::write(tmp.path().join(".machine.toml.sk1ll-tmp-1"), "someone's\n").unwrap();

        Snapshot::take(&path).unwrap().replace("new\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o7777, 0o6640);
        assert_eq!(fs::read_to_string(&victim).unwrap(), "victim\n");
        assert!(fs::symlink_metadata(tmp.path().join(".machine.toml.sk1ll-tmp")).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(tmp.path().join(".machine.toml.sk1ll-tmp-1")).unwrap(), "someone's\n");
        assert!(!tmp.path().join(".machine.toml.sk1ll-tmp-2").exists(), "the temporary file is gone");

        // A new file is private.
        let new = tmp.path().join("sub/new.toml");
        Snapshot::take(&new).unwrap().replace("x = 1\n").unwrap();
        assert_eq!(fs::metadata(&new).unwrap().permissions().mode() & 0o7777, 0o600);
        assert_eq!(names(&tmp.path().join("sub")), ["new.toml"]);
    }

    #[test]
    fn a_symlinked_file_stays_a_link_and_a_retargeted_link_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let dotfiles = tmp.path().join("dotfiles");
        fs::create_dir_all(&dotfiles).unwrap();
        fs::write(dotfiles.join("machine.toml"), "old\n").unwrap();
        fs::write(dotfiles.join("other.toml"), "other\n").unwrap();
        let path = tmp.path().join("machine.toml");
        std::os::unix::fs::symlink(dotfiles.join("machine.toml"), &path).unwrap();

        Snapshot::take(&path).unwrap().replace("new\n").unwrap();
        assert!(fs::symlink_metadata(&path).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(dotfiles.join("machine.toml")).unwrap(), "new\n");

        let snap = Snapshot::take(&path).unwrap();
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(dotfiles.join("other.toml"), &path).unwrap();
        assert!(matches!(snap.replace("derived from machine.toml\n"), Err(Error::Changed(_))));
        assert_eq!(fs::read_to_string(dotfiles.join("other.toml")).unwrap(), "other\n");
        assert_eq!(fs::read_to_string(dotfiles.join("machine.toml")).unwrap(), "new\n");

        // A dangling link is no missing file.
        let dangling = tmp.path().join("dangling.toml");
        std::os::unix::fs::symlink(tmp.path().join("nowhere"), &dangling).unwrap();
        assert!(Snapshot::take(&dangling).is_err());
    }

    #[test]
    fn a_change_since_the_look_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("machine.toml");
        fs::write(&path, "old\n").unwrap();

        // Saved after the look, before the write: in place, or by an editor's rename.
        for in_place in [true, false] {
            fs::write(&path, "old\n").unwrap();
            let snap = Snapshot::take(&path).unwrap();
            if in_place {
                fs::write(&path, "saved\n").unwrap();
            } else {
                fs::write(tmp.path().join("editor.swp"), "old\n").unwrap();
                fs::rename(tmp.path().join("editor.swp"), &path).unwrap();
            }
            assert!(matches!(snap.replace("new\n"), Err(Error::Changed(_))), "in place: {in_place}");
            assert_ne!(fs::read_to_string(&path).unwrap(), "new\n");
        }

        // Its permissions changed: never silently put back.
        let snap = Snapshot::take(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(snap.replace("new\n"), Err(Error::Changed(_))));
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o7777, 0o600);
        assert_eq!(names(tmp.path()), ["machine.toml"], "no temporary file is left behind");

        // Created meanwhile where there was none, even after the last check.
        let missing = tmp.path().join("new.toml");
        let snap = Snapshot::take(&missing).unwrap();
        let m = missing.clone();
        let result =
            with_hook(Point::BeforePublish, move || fs::write(&m, "theirs\n").unwrap(), || snap.replace("ours\n"));
        assert!(matches!(result, Err(Error::Changed(_))), "{result:?}");
        assert_eq!(fs::read_to_string(&missing).unwrap(), "theirs\n");
        assert_eq!(names(tmp.path()), ["machine.toml", "new.toml"]);
    }

    #[test]
    fn a_fifo_is_refused_without_blocking() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("machine.toml");
        let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let err = Snapshot::take(&path).unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{err}");

        // Nor does one placed after the look stall the write.
        let other = tmp.path().join("other.toml");
        fs::write(&other, "old\n").unwrap();
        let snap = Snapshot::take(&other).unwrap();
        fs::remove_file(&other).unwrap();
        let c = std::ffi::CString::new(other.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert!(matches!(snap.replace("new\n"), Err(Error::Changed(_))));
    }

    #[test]
    fn staging_never_follows_or_publishes_a_planted_temp_name() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join(".sk1ll-links.toml");
        let victim = tmp.path().join("victim");
        fs::write(&victim, "victim\n").unwrap();
        std::os::unix::fs::symlink(&victim, tmp.path().join(".sk1ll-links.toml.sk1ll-tmp")).unwrap();
        Staged::write(&dest, b"index\n", Some(0o644)).unwrap().replace().unwrap();
        assert!(fs::symlink_metadata(&dest).unwrap().file_type().is_file());
        assert_eq!(fs::read_to_string(&dest).unwrap(), "index\n");
        assert_eq!(fs::read_to_string(&victim).unwrap(), "victim\n");

        // Never placed over something that is there.
        assert!(!Staged::write(&dest, b"other\n", None).unwrap().place_new().unwrap());
        assert_eq!(fs::read_to_string(&dest).unwrap(), "index\n");
        assert_eq!(names(tmp.path()), [".sk1ll-links.toml", ".sk1ll-links.toml.sk1ll-tmp", "victim"]);
    }
}
