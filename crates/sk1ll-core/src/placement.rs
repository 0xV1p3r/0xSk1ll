//! Publish new content without replacing a destination or following a symlink.
//! Directory descriptors keep ancestor renames from redirecting a write.
use std::path::Path;

/// Move an entry without replacing an existing destination. This shares
/// sync's platform-specific no-replace implementation and fallback.
pub fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    crate::sync::rename_noreplace(from, to)
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
        stage.write(bytes, Mode::from_raw_mode(mode), true)?;
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
