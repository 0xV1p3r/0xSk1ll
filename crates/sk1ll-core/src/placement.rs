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

        pub(crate) fn parent(&self, path: &Path, create: bool) -> io::Result<(OwnedFd, OsString)> {
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

        pub(crate) fn new_directory(&self, name: &OsStr) -> io::Result<Self> {
            fd::mkdirat(&self.0, name, Mode::from_raw_mode(0o700))?;
            Ok(Self(child(&self.0, name, false)?))
        }

        pub(crate) fn create_directory(&self, path: &Path) -> io::Result<()> {
            let (parent, name) = self.parent(path, true)?;
            child(&parent, &name, true).map(drop)
        }

        pub(crate) fn copy_file(&self, from: &Path, to: &Path) -> io::Result<()> {
            use std::os::unix::fs::PermissionsExt;
            let (parent, name) = self.parent(to, true)?;
            let mut src = File::open(from)?;
            let mode = src.metadata()?.permissions().mode();
            let mut dest = File::from(fd::openat(
                &parent,
                &name,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(mode & 0o777),
            )?);
            io::copy(&mut src, &mut dest)?;
            dest.set_permissions(std::fs::Permissions::from_mode(mode))?;
            Ok(())
        }

        pub(crate) fn is_directory(&self, name: &OsStr) -> bool {
            child(&self.0, name, false).is_ok()
        }

        pub(crate) fn still_at(&self, path: &Path) -> io::Result<bool> {
            let visible = open_directory(path, false)?;
            let a = fd::fstat(&self.0)?;
            let b = fd::fstat(&visible)?;
            Ok(a.st_dev == b.st_dev && a.st_ino == b.st_ino)
        }

        pub(crate) fn rename_child(&self, from: &OsStr, to: &OsStr) -> io::Result<()> {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            match fd::renameat_with(&self.0, from, &self.0, to, fd::RenameFlags::NOREPLACE) {
                Ok(()) => return Ok(()),
                Err(rustix::io::Errno::INVAL | rustix::io::Errno::NOSYS | rustix::io::Errno::OPNOTSUPP) => {}
                Err(e) => return Err(e.into()),
            }
            #[cfg(target_os = "macos")]
            {
                use std::os::fd::AsRawFd;
                use std::os::unix::ffi::OsStrExt;
                let from = std::ffi::CString::new(from.as_bytes())?;
                let to = std::ffi::CString::new(to.as_bytes())?;
                // Both names are relative to the pinned directory and NUL-terminated.
                if unsafe {
                    libc::renameatx_np(
                        self.0.as_raw_fd(),
                        from.as_ptr(),
                        self.0.as_raw_fd(),
                        to.as_ptr(),
                        libc::RENAME_EXCL,
                    )
                } == 0
                {
                    return Ok(());
                }
                let e = io::Error::last_os_error();
                if !matches!(e.raw_os_error(), Some(libc::EINVAL | libc::ENOTSUP)) {
                    return Err(e);
                }
            }
            self.check_missing(Path::new(to))?;
            Ok(fd::renameat(&self.0, from, &self.0, to)?)
        }

        pub(crate) fn remove_tree(&self, name: &OsStr) -> io::Result<()> {
            remove_tree(&self.0, name)
        }

        pub(crate) fn replace_text(&self, name: &OsStr, text: &str) -> io::Result<()> {
            let meta = fd::statat(&self.0, name, AtFlags::SYMLINK_NOFOLLOW)?;
            if fd::FileType::from_raw_mode(meta.st_mode) != fd::FileType::RegularFile {
                return Err(io::Error::other("not a regular file"));
            }
            replace_file(&self.0, name, text.as_bytes(), meta.st_mode as u32, || Ok::<_, io::Error>(true))??;
            Ok(())
        }

        pub fn create_symlink(&self, path: &Path, target: &OsStr) -> io::Result<()> {
            let (parent, name) = self.parent(path, true)?;
            Ok(fd::symlinkat(target, &parent, &name)?)
        }
    }

    fn remove_tree(parent: &OwnedFd, name: &OsStr) -> io::Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let dir = child(parent, name, false)?;
        for entry in fd::Dir::read_from(&dir)? {
            let entry = entry?;
            let name = OsStr::from_bytes(entry.file_name().to_bytes());
            if matches!(name.to_str(), Some("." | "..")) {
                continue;
            }
            let meta = fd::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
            if fd::FileType::from_raw_mode(meta.st_mode) == fd::FileType::Directory {
                remove_tree(&dir, name)?;
            } else {
                fd::unlinkat(&dir, name, AtFlags::empty())?;
            }
        }
        Ok(fd::unlinkat(parent, name, AtFlags::REMOVEDIR)?)
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
