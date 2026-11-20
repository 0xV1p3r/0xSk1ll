//! Pull's changes to the working tree, one path at a time, each only while
//! the path holds what pull last saw there.
//!
//! Every path is reached through pinned directory descriptors, never
//! following a symlink. What pull replaces or removes goes to the recovery
//! area first (see [`crate::sync::displaced`]), as a [`Kind::Draft`] entry
//! in the git directory, and is checked there:
//!
//! - A file is hard-linked into its entry, checked against what pull saw,
//!   and only then replaced, by a rename that swaps the new file in (where
//!   the platform can exchange two names, see [`exchange`]). If a save
//!   renamed its own file into place after the check, the exchange brings
//!   that file out again, and it goes to a recovery entry of its own. A
//!   write through a descriptor opened earlier lands in the entry, which is
//!   checked once more afterwards. Either way the result is the
//!   [`Error::Displaced`] that says where the save is.
//! - A file pull removes is moved into its entry, and put back if it isn't
//!   what pull saw.
//! - A new file is linked into place, which never replaces anything.
//!
//! Replacements are staged in the git directory, named by their place in
//! the journal, so that a later pull can tell what an interrupted one left
//! there (see [`Worktree::tidy`]).
//!
//! Only Unix has the descriptor-relative calls this needs; elsewhere
//! [`Worktree::open`] refuses, before anything changes.

#[cfg(unix)]
pub(super) use unix::Worktree;

#[cfg(not(unix))]
pub(super) use other::Worktree;

use crate::Error;
#[cfg(unix)]
use crate::sync::displaced::Kind;

/// What is at a path, read in one go.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Node {
    Missing,
    Directory,
    /// Its bytes and permission bits.
    File {
        bytes: Vec<u8>,
        mode: u32,
    },
    /// Its target.
    Link(Vec<u8>),
    /// Anything else (a FIFO, a socket, ...).
    Other,
}

/// What pull writes at a path.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Content {
    File { bytes: Vec<u8>, executable: bool },
    Link(Vec<u8>),
}

/// How a change to one path went.
pub(super) enum Outcome {
    /// It is done. `Some`: something was written to the old content (or
    /// renamed into its place) meanwhile, and is in the recovery area.
    Done(Option<Error>),
    /// The path no longer held what pull saw there (or something is in the
    /// way of a new one), so it was left as it is.
    Changed,
}

#[cfg(unix)]
mod unix {
    use std::ffi::OsStr;
    use std::fs;
    use std::io::{self, Read, Write};
    use std::os::fd::OwnedFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
    use std::path::{Path, PathBuf};

    use rustix::fs::{self as fd, AtFlags, CWD, FileType, Mode, OFlags};
    use rustix::io::Errno;

    use super::super::journal::Journal;
    use super::super::{Before, Entry, Hook};
    use super::{Content, Kind, Node, Outcome};
    use crate::placement::Directory;
    use crate::sync::displaced::{self, Displacing, PULL, leaf_fingerprint};
    use crate::{Error, IoExt, Result};

    /// The working tree, pinned at its top level.
    pub(in super::super) struct Worktree {
        dir: Directory,
        /// The top level's path, for recovery entries and messages.
        root: PathBuf,
        /// The git directory: it holds the recovery entries.
        holder: PathBuf,
        /// Where replacements are staged.
        stage: PathBuf,
    }

    impl Worktree {
        /// Pins the worktree at `root`. Its git directory `git_dir` must be
        /// on the same file system: pull links and renames between them.
        pub(in super::super) fn open(root: &Path, git_dir: &Path) -> Result<Self> {
            let root = root.canonicalize().at(root)?;
            let holder = git_dir.canonicalize().at(git_dir)?;
            if fs::metadata(&root).at(&root)?.dev() != fs::metadata(&holder).at(&holder)?.dev() {
                return Err(Error::Invalid(format!(
                    "pull needs the git directory ({}) on the same file system as the working tree ({})",
                    holder.display(),
                    root.display()
                )));
            }
            displaced::reindex(&holder)?;
            let stage = holder.join("sk1ll-pull-stage");
            Ok(Self { dir: Directory::open(&root, false).at(&root)?, root, holder, stage })
        }

        pub(in super::super) fn read(&self, path: &Path) -> Result<Node> {
            match self.dir.parent(path, false) {
                Ok((parent, name)) => read(&parent, &name).at(self.root.join(path)),
                Err(e) if missing(&e) => Ok(Node::Missing),
                Err(e) => Err(e).at(self.root.join(path)),
            }
        }

        /// Deals with what an interrupted pull left staged: a file that is
        /// also somewhere else (a replacement linked into place, or old
        /// content that is in its recovery entry too) goes, and so does
        /// one that holds the content pull stages for that path. Anything
        /// else (a directory too) is what a swap brought out of the working
        /// tree, which goes to the recovery area.
        pub(in super::super) fn tidy(
            &self,
            journal: &Journal,
            content: impl Fn(&Entry, &Path) -> Result<Content>,
        ) -> Result<()> {
            let names = match fs::read_dir(&self.stage) {
                Ok(names) => names,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e).at(&self.stage),
            };
            for name in names {
                let staged = name.at(&self.stage)?.path();
                let meta = fs::symlink_metadata(&staged).at(&staged)?;
                let change = staged
                    .file_name()
                    .and_then(|n| n.to_str()?.parse::<usize>().ok())
                    .and_then(|i| journal.changes.get(i));
                let ours = !meta.is_dir()
                    && (meta.nlink() > 1
                        || match change.and_then(|c| Some((c.new.as_ref()?, &c.path))) {
                            Some((entry, path)) => holds(&staged, &content(entry, path)?)?,
                            None => false,
                        });
                if ours {
                    fs::remove_file(&staged).at(&staged)?;
                } else {
                    let dest = change.map_or_else(|| staged.clone(), |c| self.root.join(&c.path));
                    displaced::rescue(Kind::Draft, PULL, &dest, &staged, None, &self.holder)?;
                }
            }
            Ok(())
        }

        /// Removes the file or symlink at `path`, which held `node`.
        pub(in super::super) fn remove(&self, path: &Path, node: &Node) -> Result<Outcome> {
            let dest = self.root.join(path);
            let (parent, name) = match self.dir.parent(path, false) {
                Ok(found) => found,
                Err(e) if missing(&e) => return Ok(Outcome::Done(None)),
                Err(e) => return Err(e).at(&dest),
            };
            let mut held = Displacing::begin(Kind::Draft, PULL, &dest, Some(&fingerprint(node)), &self.holder)?;
            match held.take_at(&parent, &name) {
                Ok(true) => {}
                // Gone already: nothing to remove.
                Ok(false) => {
                    held.abandon();
                    return Ok(Outcome::Done(None));
                }
                Err(e) => {
                    held.abandon();
                    return Err(e);
                }
            }
            match held.intact() {
                Ok(true) => Ok(Outcome::Done(held.finish()?)),
                verdict => match held.put_back_at(&parent, &name) {
                    // Put back, or kept as changed if something new is there.
                    Ok(()) | Err(Error::Displaced { .. }) => verdict.map(|_| Outcome::Changed),
                    Err(e) => Err(e),
                },
            }
        }

        /// Puts `content` at `path`, where `node` (nothing, or a directory
        /// that must be empty) is.
        pub(in super::super) fn create(
            &self,
            i: usize,
            path: &Path,
            content: &Content,
            node: &Node,
        ) -> Result<Outcome> {
            let dest = self.root.join(path);
            let (parent, name) = match self.dir.parent(path, true) {
                Ok(found) => found,
                // Not a directory (or a symlink to one) on the way.
                Err(e) if matches!(e.raw_os_error(), Some(libc::ENOTDIR | libc::ELOOP | libc::EEXIST)) => {
                    return Ok(Outcome::Changed);
                }
                Err(e) => return Err(e).at(&dest),
            };
            if *node == Node::Directory {
                match fd::unlinkat(&parent, &name, AtFlags::REMOVEDIR) {
                    Ok(()) | Err(Errno::NOENT) => {}
                    Err(Errno::NOTEMPTY | Errno::EXIST | Errno::NOTDIR) => return Ok(Outcome::Changed),
                    Err(e) => return Err(io::Error::from(e)).at(&dest),
                }
            }
            let created = match content {
                Content::Link(target) => fd::symlinkat(OsStr::from_bytes(target), &parent, &name),
                Content::File { .. } => {
                    let staged = self.stage(i, content, None)?;
                    let linked = fd::linkat(CWD, &staged, &parent, &name, AtFlags::empty());
                    fs::remove_file(&staged).at(&staged)?;
                    linked
                }
            };
            match created {
                Ok(()) => Ok(Outcome::Done(None)),
                Err(Errno::EXIST) => Ok(Outcome::Changed),
                Err(e) => Err(io::Error::from(e)).at(&dest),
            }
        }

        /// Replaces the file or symlink at `path`, which held `node`, with
        /// `content`. A replaced file keeps its permissions, apart from the
        /// executable bits when `content` changes them.
        pub(in super::super) fn replace(
            &self,
            i: usize,
            path: &Path,
            node: &Node,
            content: &Content,
            hook: Hook,
        ) -> Result<Outcome> {
            let dest = self.root.join(path);
            let (parent, name) = match self.dir.parent(path, false) {
                Ok(found) => found,
                Err(e) if missing(&e) => return Ok(Outcome::Changed),
                Err(e) => return Err(e).at(&dest),
            };
            let permissions = match (node, content) {
                (Node::File { mode, .. }, Content::File { executable, .. }) => Some(permissions(*mode, *executable)),
                _ => None,
            };
            let staged = self.stage(i, content, permissions)?;
            let unstage = || fs::remove_file(&staged).at(&staged);
            let mut held = match Displacing::begin(Kind::Draft, PULL, &dest, Some(&fingerprint(node)), &self.holder) {
                Ok(held) => held,
                Err(e) => {
                    unstage()?;
                    return Err(e);
                }
            };
            let verdict = match held.link_at(&parent, &name) {
                Ok(true) => held.intact(),
                Ok(false) => Ok(false),
                Err(e) => Err(e),
            };
            if !matches!(verdict, Ok(true)) {
                held.abandon();
                unstage()?;
                return verdict.map(|_| Outcome::Changed);
            }
            hook(Before::Swap)?;
            let out = match exchange(&staged, &parent, &name) {
                Ok(out) => out,
                // Deleted meanwhile, which stays so.
                Err(Errno::NOENT) => {
                    held.abandon();
                    unstage()?;
                    return Ok(Outcome::Changed);
                }
                Err(e) => {
                    held.abandon();
                    unstage()?;
                    return Err(io::Error::from(e)).at(&dest);
                }
            };
            // `staged` now holds what was at `path`: the old content, which
            // its entry holds too, unless a save renamed its file in since.
            let mut displaced = None;
            if out {
                if same_file(&staged, &held.content()) {
                    unstage()?;
                } else {
                    let entry = displaced::rescue(Kind::Draft, PULL, &dest, &staged, None, &self.holder)?;
                    displaced = Some(Error::Displaced { dest: dest.clone(), kept: entry.path, id: entry.id });
                }
            }
            Ok(Outcome::Done(held.finish()?.or(displaced)))
        }

        /// Removes the directories above `removed` that are empty now, as
        /// git does, deepest first.
        pub(in super::super) fn prune<'p>(&self, removed: impl Iterator<Item = &'p Path>) {
            let mut dirs = std::collections::BTreeSet::new();
            for path in removed {
                dirs.extend(path.ancestors().skip(1).filter(|a| !a.as_os_str().is_empty()));
            }
            for dir in dirs.into_iter().rev() {
                if let Ok((parent, name)) = self.dir.parent(dir, false) {
                    let _ = fd::unlinkat(&parent, &name, AtFlags::REMOVEDIR);
                }
            }
        }

        /// Removes the staging directory once a pull is done.
        pub(in super::super) fn clear(&self) {
            let _ = fs::remove_dir(&self.stage);
        }

        /// Writes `content` as the `i`th path's staged replacement: with
        /// exactly `permissions`, or else as git creates files (the umask
        /// applies).
        fn stage(&self, i: usize, content: &Content, permissions: Option<u32>) -> Result<PathBuf> {
            match fs::DirBuilder::new().mode(0o700).create(&self.stage) {
                Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e).at(&self.stage),
                _ => {}
            }
            let staged = self.stage.join(i.to_string());
            match content {
                Content::Link(target) => std::os::unix::fs::symlink(OsStr::from_bytes(target), &staged).at(&staged)?,
                Content::File { bytes, executable } => {
                    let mode = match (permissions, executable) {
                        (Some(_), _) => 0o600,
                        (None, true) => 0o777,
                        (None, false) => 0o666,
                    };
                    let mut options = fs::OpenOptions::new();
                    options.write(true).create_new(true).mode(mode).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
                    let mut file = options.open(&staged).at(&staged)?;
                    file.write_all(bytes).at(&staged)?;
                    if let Some(permissions) = permissions {
                        fd::fchmod(&file, Mode::from_raw_mode(permissions as fd::RawMode))
                            .map_err(io::Error::from)
                            .at(&staged)?;
                    }
                    file.sync_all().at(&staged)?;
                }
            }
            Ok(staged)
        }
    }

    /// The permission bits a file with `mode` gets when replaced, with the
    /// executable bits as `executable` says: as they were if that doesn't
    /// change, else set or cleared where the file is readable, as git does.
    fn permissions(mode: u32, executable: bool) -> u32 {
        match (mode & 0o100 != 0, executable) {
            (was, now) if was == now => mode & 0o777,
            (_, true) => (mode & 0o666) | ((mode & 0o444) >> 2),
            (_, false) => mode & 0o666,
        }
    }

    /// Moves `staged` to `name` in `parent`. Where it can, it exchanges the
    /// two names, so that whatever was at `name` then ends up at `staged`
    /// (`true`); otherwise it is replaced (`false`).
    fn exchange(staged: &Path, parent: &OwnedFd, name: &OsStr) -> std::result::Result<bool, Errno> {
        #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
        match fd::renameat_with(CWD, staged, parent, name, fd::RenameFlags::EXCHANGE) {
            Ok(()) => return Ok(true),
            // A file system without the call: plain rename.
            Err(Errno::INVAL | Errno::NOSYS | Errno::OPNOTSUPP) => {}
            Err(e) => return Err(e),
        }
        fd::renameat(CWD, staged, parent, name).map(|()| false)
    }

    /// [`displaced`]'s fingerprint of what `node` is.
    fn fingerprint(node: &Node) -> String {
        match node {
            Node::File { bytes, .. } => leaf_fingerprint(false, bytes),
            Node::Link(target) => leaf_fingerprint(true, target),
            // Never displaced: only files and symlinks are replaced or removed.
            Node::Missing | Node::Directory | Node::Other => String::new(),
        }
    }

    fn same_file(a: &Path, b: &Path) -> bool {
        match (fs::symlink_metadata(a), fs::symlink_metadata(b)) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        }
    }

    /// Whether the staged file or symlink `staged` holds `content`.
    fn holds(staged: &Path, content: &Content) -> Result<bool> {
        let meta = fs::symlink_metadata(staged).at(staged)?;
        Ok(match content {
            Content::Link(target) => {
                meta.file_type().is_symlink() && fs::read_link(staged).at(staged)?.as_os_str().as_bytes() == target
            }
            Content::File { bytes, .. } => meta.is_file() && fs::read(staged).at(staged)? == *bytes,
        })
    }

    fn missing(error: &io::Error) -> bool {
        matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP))
    }

    fn read(parent: &OwnedFd, name: &OsStr) -> io::Result<Node> {
        let stat = match fd::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(Errno::NOENT) => return Ok(Node::Missing),
            Err(error) => return Err(error.into()),
        };
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::RegularFile => {
                let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
                let file = match fd::openat(parent, name, flags, Mode::empty()) {
                    Ok(file) => file,
                    // Replaced since the stat: it isn't what pull saw anyway.
                    Err(Errno::NOENT | Errno::LOOP) => return Ok(Node::Other),
                    Err(error) => return Err(error.into()),
                };
                let stat = fd::fstat(&file)?;
                if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
                    return Ok(Node::Other);
                }
                let mut bytes = Vec::new();
                fs::File::from(file).read_to_end(&mut bytes)?;
                Ok(Node::File { bytes, mode: stat.st_mode as u32 & 0o7777 })
            }
            FileType::Directory => Ok(Node::Directory),
            FileType::Symlink => match fd::readlinkat(parent, name, Vec::new()) {
                Ok(target) => Ok(Node::Link(target.into_bytes())),
                Err(Errno::NOENT | Errno::INVAL) => Ok(Node::Other),
                Err(error) => Err(error.into()),
            },
            _ => Ok(Node::Other),
        }
    }
}

#[cfg(not(unix))]
mod other {
    use std::path::Path;

    use super::super::journal::Journal;
    use super::super::{Entry, Hook};
    use super::{Content, Node, Outcome};
    use crate::{Error, Result};

    /// Never opens: see the module docs.
    pub(in super::super) struct Worktree;

    impl Worktree {
        pub(in super::super) fn open(_: &Path, _: &Path) -> Result<Self> {
            Err(Error::Invalid(
                "pull can only change files on Unix; it fetched the new baseline but left the working tree, the \
                 index and the branch as they were"
                    .into(),
            ))
        }

        pub(in super::super) fn read(&self, _: &Path) -> Result<Node> {
            unreachable!("never opened")
        }

        pub(in super::super) fn tidy(&self, _: &Journal, _: impl Fn(&Entry, &Path) -> Result<Content>) -> Result<()> {
            unreachable!("never opened")
        }

        pub(in super::super) fn remove(&self, _: &Path, _: &Node) -> Result<Outcome> {
            unreachable!("never opened")
        }

        pub(in super::super) fn create(&self, _: usize, _: &Path, _: &Content, _: &Node) -> Result<Outcome> {
            unreachable!("never opened")
        }

        pub(in super::super) fn replace(&self, _: usize, _: &Path, _: &Node, _: &Content, _: Hook) -> Result<Outcome> {
            unreachable!("never opened")
        }

        pub(in super::super) fn prune<'p>(&self, _: impl Iterator<Item = &'p Path>) {
            unreachable!("never opened")
        }

        pub(in super::super) fn clear(&self) {
            unreachable!("never opened")
        }
    }
}
