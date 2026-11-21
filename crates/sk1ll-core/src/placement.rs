//! Publish new content without replacing a destination or following a symlink.
//! Directory descriptors keep ancestor renames from redirecting a write.
use std::path::Path;

/// Move an entry without replacing an existing destination. This shares
/// sync's platform-specific no-replace implementation and fallback.
pub fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    crate::sync::rename_noreplace(from, to)
}

pub(crate) fn is_placeholder(path: &Path, backup: &Path) -> bool {
    std::fs::read_link(path).is_ok_and(|target| target == backup.with_file_name("exchange-placeholder"))
}

pub(crate) fn clear_placeholder(src: &Path, backup: &Path) -> std::io::Result<()> {
    if !is_placeholder(src, backup) {
        return Ok(());
    }
    let aside = backup.with_file_name("exchange-aside");
    match aside.symlink_metadata() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::rename(src, &aside)?,
        Err(e) => return Err(e),
        Ok(_) => return Err(std::io::ErrorKind::AlreadyExists.into()),
    }
    if is_placeholder(&aside, backup) {
        std::fs::remove_file(aside)
    } else {
        rename_noreplace(&aside, src)?;
        Err(std::io::ErrorKind::AlreadyExists.into())
    }
}

/// Detach an incumbent before validation. Both recovery names are private
/// to the caller. If an editor replaces the exchange placeholder, restore
/// the editor's file without replacement or keep it in the second slot.
pub(crate) fn detach(src: &Path, backup: &Path) -> std::io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    {
        let sentinel = backup.with_file_name("exchange-placeholder");
        let aside = backup.with_file_name("exchange-aside");
        std::os::unix::fs::symlink(&sentinel, backup)?;
        match exchange(src, backup) {
            Ok(()) => {
                match std::fs::rename(src, &aside) {
                    Ok(()) => {
                        if std::fs::read_link(&aside).is_ok_and(|target| target == sentinel) {
                            std::fs::remove_file(aside)?;
                        } else {
                            rename_noreplace(&aside, src)?;
                            return Err(std::io::ErrorKind::AlreadyExists.into());
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
                return Ok(());
            }
            Err(e) => {
                std::fs::remove_file(backup)?;
                if !matches!(e.raw_os_error(), Some(libc::EINVAL | libc::ENOSYS | libc::ENOTSUP)) {
                    return Err(e);
                }
            }
        }
    }
    // The backup name is vacant and private. A rename moves the object
    // that is there at the syscall, never unlinking a later replacement.
    std::fs::rename(src, backup)
}

#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
fn exchange(a: &Path, b: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let a = std::ffi::CString::new(a.as_os_str().as_bytes())?;
    let b = std::ffi::CString::new(b.as_os_str().as_bytes())?;
    // SAFETY: the paths are NUL-terminated and stay alive for the syscall.
    #[cfg(target_os = "macos")]
    let result = unsafe { libc::renamex_np(a.as_ptr(), b.as_ptr(), libc::RENAME_SWAP) };
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD as libc::c_long,
            a.as_ptr(),
            libc::AT_FDCWD as libc::c_long,
            b.as_ptr(),
            libc::RENAME_EXCHANGE as libc::c_long,
        )
    };
    if result == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

/// Move the incumbent, validate the object that actually moved, then make
/// the replacement visible only at a vacant name. The caller owns recovery
/// of the backup on success or failure.
pub(crate) fn swap_managed<E>(
    stage: Option<&Path>,
    dest: &Path,
    backup: Option<&Path>,
    before_publish: impl FnOnce() -> Result<(), E>,
) -> std::io::Result<Result<(), E>> {
    if let Some(backup) = backup {
        detach(dest, backup)?;
    }
    if let Err(error) = before_publish() {
        return Ok(Err(error));
    }
    if let Some(stage) = stage {
        rename_noreplace(stage, dest)?;
    }
    Ok(Ok(()))
}

#[cfg(unix)]
mod unix {
    use rustix::fs::{self as fd, AtFlags, Mode, OFlags};
    use std::ffi::{OsStr, OsString};
    use std::fs::File;
    use std::io::{self, Write};
    use std::os::fd::OwnedFd;
    use std::path::{Component, Path};
    use std::sync::atomic::{AtomicU64, Ordering};

    pub(crate) const DIRECTORY: OFlags =
        OFlags::RDONLY.union(OFlags::DIRECTORY).union(OFlags::NOFOLLOW).union(OFlags::CLOEXEC);

    pub(crate) fn open_directory(path: &Path, create: bool) -> io::Result<OwnedFd> {
        let absolute = std::path::absolute(path)?;
        let mut directory = fd::open("/", DIRECTORY, Mode::empty())?;
        for component in absolute.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => directory = child(&directory, name, create)?,
                _ => return Err(io::Error::other("directory path must be normalized")),
            }
        }
        Ok(directory)
    }

    fn child(parent: &OwnedFd, name: &OsStr, create: bool) -> io::Result<OwnedFd> {
        if create {
            match fd::mkdirat(parent, name, Mode::from_raw_mode(0o755)) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(fd::openat(parent, name, DIRECTORY, Mode::empty())?)
    }

    /// A pinned root for no-follow traversal and no-replace publication.
    pub struct Directory(OwnedFd);
    impl Directory {
        pub fn open(path: &Path, create: bool) -> io::Result<Self> {
            Ok(Self(open_directory(path, create)?))
        }

        fn parent(&self, path: &Path, create: bool) -> io::Result<(OwnedFd, OsString)> {
            let parts: Vec<_> = path.components().collect();
            if parts.is_empty() || parts.iter().any(|p| !matches!(p, Component::Normal(_))) {
                return Err(io::Error::other("file path must be relative and normalized"));
            }
            let mut directory = fd::openat(&self.0, ".", DIRECTORY, Mode::empty())?;
            for part in &parts[..parts.len() - 1] {
                directory = child(&directory, part.as_os_str(), create)?;
            }
            Ok((directory, parts.last().unwrap().as_os_str().to_owned()))
        }

        /// Checks all existing ancestors and the leaf without following links.
        pub fn check_missing(&self, path: &Path) -> io::Result<()> {
            let (parent, name) = match self.parent(path, false) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
                result => result?,
            };
            match fd::statat(&parent, &name, AtFlags::SYMLINK_NOFOLLOW) {
                Err(rustix::io::Errno::NOENT) => Ok(()),
                Ok(_) => Err(io::ErrorKind::AlreadyExists.into()),
                Err(e) => Err(e.into()),
            }
        }

        /// Writes privately, then links the complete file into place. linkat
        /// refuses every existing leaf, including dangling links and directories.
        pub fn create_file(&self, path: &Path, bytes: &[u8], executable: bool) -> io::Result<()> {
            let (parent, name) = self.parent(path, true)?;
            let mut stage = Stage::new(&parent)?;
            stage.write(bytes, Mode::from_raw_mode(if executable { 0o755 } else { 0o644 }), false)?;
            fd::linkat(&stage.directory, "file", &parent, &name, AtFlags::empty())?;
            Ok(())
        }

        pub fn create_symlink(&self, path: &Path, target: &OsStr) -> io::Result<()> {
            let (parent, name) = self.parent(path, true)?;
            Ok(fd::symlinkat(target, &parent, &name)?)
        }
    }

    pub(crate) fn replace_file<E>(
        parent: &OwnedFd,
        name: &OsStr,
        bytes: &[u8],
        mode: u32,
        unchanged: impl FnOnce() -> Result<bool, E>,
    ) -> io::Result<Result<bool, E>> {
        let mut stage = Stage::new(parent)?;
        stage.write(bytes, Mode::from_raw_mode((mode & 0o7777) as fd::RawMode), true)?;
        match unchanged() {
            Ok(true) => {
                fd::renameat(&stage.directory, "file", parent, name)?;
                Ok(Ok(true))
            }
            result => Ok(result),
        }
    }

    impl Stage<'_> {
        fn write(&mut self, bytes: &[u8], mode: Mode, preserve_mode: bool) -> io::Result<()> {
            let file = fd::openat(
                &self.directory,
                "file",
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                if preserve_mode { Mode::RUSR | Mode::WUSR } else { mode },
            )?;
            self.file_created = true;
            let mut file = File::from(file);
            file.write_all(bytes)?;
            if preserve_mode {
                fd::fchmod(&file, mode)?;
            }
            file.sync_all()
        }
    }

    struct Stage<'a> {
        parent: &'a OwnedFd,
        directory: OwnedFd,
        name: String,
        file_created: bool,
    }
    impl<'a> Stage<'a> {
        fn new(parent: &'a OwnedFd) -> io::Result<Self> {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            loop {
                let name =
                    format!(".sk1ll-stage-publish-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
                match fd::mkdirat(parent, name.as_str(), Mode::from_raw_mode(0o700)) {
                    Err(rustix::io::Errno::EXIST) => continue,
                    result => result?,
                }
                let directory = fd::openat(parent, name.as_str(), DIRECTORY, Mode::empty())?;
                return Ok(Self { parent, directory, name, file_created: false });
            }
        }
    }
    impl Drop for Stage<'_> {
        fn drop(&mut self) {
            if self.file_created {
                let _ = fd::unlinkat(&self.directory, "file", AtFlags::empty());
            }
            let _ = fd::unlinkat(self.parent, self.name.as_str(), AtFlags::REMOVEDIR);
        }
    }
}
#[cfg(unix)]
pub use unix::Directory;
#[cfg(unix)]
pub(crate) use unix::{DIRECTORY, open_directory, replace_file};

// New writes fail closed where descriptor-relative traversal isn't available.
#[cfg(not(unix))]
pub struct Directory;
#[cfg(not(unix))]
impl Directory {
    pub fn open(_: &Path, _: bool) -> std::io::Result<Self> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
    pub fn check_missing(&self, _: &Path) -> std::io::Result<()> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
    pub fn create_file(&self, _: &Path, _: &[u8], _: bool) -> std::io::Result<()> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
}
