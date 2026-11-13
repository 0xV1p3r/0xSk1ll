//! All traversal uses directory descriptors. A rename can detach a pinned
//! directory, but cannot redirect subsequent access through a replacement link.
use super::*;
use rustix::fs::{self as fd, AtFlags, Dir, FileType, Mode, OFlags};
use std::ffi::OsString;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;

use crate::placement::{DIRECTORY, open_directory};

pub(super) struct Item {
    directory: OwnedFd,
    rule: Option<OsString>,
    path: PathBuf,
}

fn open_at(dir: &OwnedFd, name: &std::ffi::OsStr, flags: OFlags, path: &Path) -> Result<OwnedFd> {
    fd::openat(dir, name, flags, Mode::empty()).map_err(std::io::Error::from).at(path)
}

impl Item {
    pub(super) fn open(path: &Path, rule: bool) -> Result<Self> {
        let base = if rule { path.parent().unwrap() } else { path };
        let directory = open_directory(base, false).at(path)?;
        Ok(Self { directory, rule: rule.then(|| path.file_name().unwrap().to_owned()), path: path.into() })
    }

    pub(super) fn file(&self, relative: &Path) -> Result<fs::File> {
        if let Some(name) = &self.rule
            && relative != Path::new(name)
        {
            return Err(Error::Invalid("file path escapes the Rule".into()));
        }
        let mut directory = open_at(&self.directory, std::ffi::OsStr::new("."), DIRECTORY, &self.path)?;
        let mut components = relative.components().peekable();
        while let Some(Component::Normal(name)) = components.next() {
            if components.peek().is_none() {
                return Ok(open_at(
                    &directory,
                    name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                    &self.path,
                )?
                .into());
            }
            directory = open_at(&directory, name, DIRECTORY, &self.path)?;
        }
        Err(Error::Invalid("invalid file path".into()))
    }

    pub(super) fn tree(&self) -> Result<Vec<FileEntry>> {
        let mut result = Vec::new();
        if let Some(name) = &self.rule {
            self.entry(&self.directory, name, Path::new(name), &mut result)?;
        } else {
            self.walk(&self.directory, Path::new(""), &mut result)?;
        }
        Ok(result)
    }

    fn walk(&self, directory: &OwnedFd, relative: &Path, result: &mut Vec<FileEntry>) -> Result<()> {
        let entries = Dir::read_from(directory).map_err(std::io::Error::from).at(&self.path)?;
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(std::io::Error::from).at(&self.path)?;
            let name = std::ffi::OsStr::from_bytes(entry.file_name().to_bytes());
            if [".", "..", ".git", crate::sync::MARKER, ".sk1ll-tmp"].iter().any(|s| name == *s) {
                continue;
            }
            names.push(name.to_owned());
        }
        names.sort();
        for name in names {
            self.entry(directory, &name, &relative.join(&name), result)?;
        }
        Ok(())
    }

    fn entry(
        &self,
        directory: &OwnedFd,
        name: &std::ffi::OsStr,
        relative: &Path,
        result: &mut Vec<FileEntry>,
    ) -> Result<()> {
        let meta =
            fd::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW).map_err(std::io::Error::from).at(&self.path)?;
        let kind = FileType::from_raw_mode(meta.st_mode);
        result.push(FileEntry {
            path: relative.to_string_lossy().into_owned(),
            kind: if kind == FileType::Symlink {
                "symlink"
            } else if kind == FileType::Directory {
                "directory"
            } else {
                "file"
            }
            .into(),
            size: if kind == FileType::Directory { 0 } else { meta.st_size.max(0) as u64 },
        });
        if kind == FileType::Directory {
            let child = open_at(directory, name, DIRECTORY, &self.path)?;
            self.walk(&child, relative, result)?;
        }
        Ok(())
    }
}
