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
        let (directory, name) = self.parent(relative)?;
        self.open_file(&directory, &name)
    }

    fn open_file(&self, directory: &OwnedFd, name: &std::ffi::OsStr) -> Result<fs::File> {
        Ok(open_at(
            directory,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            &self.path,
        )?
        .into())
    }

    pub(super) fn replace(&self, relative: &Path, request: &WriteFileRequest) -> Result<bool> {
        use std::os::unix::fs::MetadataExt;

        let (directory, name) = self.parent(relative)?;
        let original = match self.open_file(&directory, &name) {
            Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            opened => opened?,
        };
        let meta = original.metadata().at(&self.path)?;
        if !meta.is_file() {
            return Err(Error::Invalid("path is not a regular file".into()));
        }
        let matches = |file: fs::File| -> Result<bool> {
            let mut bytes = Vec::new();
            file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes).at(&self.path)?;
            Ok(bytes.len() as u64 <= MAX_FILE_BYTES
                && std::str::from_utf8(&bytes)
                    .is_ok_and(|text| !text.contains('\0') && crate::hash::hash_str(text) == request.expected))
        };
        if !matches(original)? {
            return Ok(false);
        }
        if self.rule.is_some() {
            crate::instructions::validate_edit(&request.content, &self.path)
                .map_err(|e| Error::Invalid(format!("Invalid Rule frontmatter: {e}")))?;
        }
        crate::placement::replace_file(&directory, &name, request.content.as_bytes(), meta.mode(), || {
            let Ok(current) = self.open_file(&directory, &name) else {
                return Ok(false);
            };
            let now = current.metadata().at(&self.path)?;
            Ok(now.is_file()
                && now.dev() == meta.dev()
                && now.ino() == meta.ino()
                && now.mode() == meta.mode()
                && matches(current)?)
        })
        .at(&self.path)?
    }

    fn parent(&self, relative: &Path) -> Result<(OwnedFd, OsString)> {
        if let Some(name) = &self.rule
            && relative != Path::new(name)
        {
            return Err(Error::Invalid("file path escapes the Rule".into()));
        }
        let mut directory = open_at(&self.directory, std::ffi::OsStr::new("."), DIRECTORY, &self.path)?;
        let mut components = relative.components().peekable();
        while let Some(Component::Normal(name)) = components.next() {
            if components.peek().is_none() {
                return Ok((directory, name.to_owned()));
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
            if name == "." || name == ".." || hidden(name) {
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
