//! Pinned, descriptor-relative publication for Pull. A prepared addition
//! consumes its staging slot, a removal fills its recovery entry, and an
//! exchange changes the staging slot's inode. A resumed Pull reads that
//! evidence before looking at the destination.

#[cfg(unix)]
pub(super) use unix::Worktree;

#[cfg(not(unix))]
pub(super) use other::Worktree;

use crate::Error;
#[cfg(unix)]
use crate::sync::displaced::Kind;

/// Where a keep's restore stages a Baseline file (see
/// [`Worktree::place_new`]), in the git directory `git_dir`: apart from
/// pull's staging, so what a stopped restore leaves never stops a pull.
pub(super) fn keep_stage(git_dir: &std::path::Path) -> std::path::PathBuf {
    git_dir.join("sk1ll-keep-stage")
}

/// The one file in [`keep_stage`].
pub(super) const KEEP_SLOT: &str = "file";

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

    use super::super::Change;
    use super::super::journal::{Inode, Operation};
    use super::{Content, Kind, Node, Outcome};
    use crate::placement::Directory;
    use crate::sync::displaced::{self, Displacing, Entry, PULL, leaf_fingerprint};
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

        /// Test the publication primitives before creating a journal.
        pub(in super::super) fn preflight(&self, changes: &[Change]) -> Result<()> {
            if changes.is_empty() {
                return Ok(());
            }
            match fs::read_dir(&self.stage) {
                Ok(mut slots) => {
                    if slots.next().is_some() {
                        return Err(Error::Invalid(format!(
                            "pull staging is left at {} without a journal",
                            self.stage.display()
                        )));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).at(&self.stage),
            }
            let holder = crate::placement::open_directory(&self.holder, false).at(&self.holder)?;
            for change in changes {
                // Find the nearest parent reached without following a symlink.
                for at in change.path.ancestors().filter(|at| !at.as_os_str().is_empty()) {
                    let (parent, name) = match self.dir.parent(at, false) {
                        Ok(found) => found,
                        Err(e) if missing(&e) => continue,
                        Err(e) => return Err(e).at(self.root.join(at)),
                    };
                    let path = self.root.join(at);
                    let same = same_mount(&holder, &parent).at(&path)?;
                    let leaf = fd::openat(&parent, &name, crate::placement::DIRECTORY, Mode::empty());
                    if !same || matches!(leaf, Ok(dir) if !same_mount(&holder, &dir).at(&path)?) {
                        return Err(Error::Invalid(format!("pull cannot cross a mount at {}", path.display())));
                    }
                    break;
                }
            }
            let probe = self.holder.join("sk1ll-pull-probe");
            let first = probe.join("first");
            let second = probe.join("second");
            let _ = fs::remove_dir_all(&probe);
            fs::DirBuilder::new().mode(0o700).create(&probe).at(&probe)?;
            let result = (|| {
                fs::write(&first, []).at(&first)?;
                fs::write(&second, []).at(&second)?;
                let dir = Directory::open(&probe, false).at(&probe)?;
                let (parent, name) = dir.parent(Path::new("second"), false).at(&probe)?;
                if changes.iter().any(|c| c.old.is_some() && c.new.is_some()) {
                    exchange(&first, &parent, &name).map_err(|e| {
                        Error::Invalid(format!("pull needs atomic file exchange on {} ({e})", self.root.display()))
                    })?;
                }
                if changes.iter().any(|c| c.new.is_some()) {
                    let (_, vacant) = dir.parent(Path::new("vacant"), false).at(&probe)?;
                    publish(&first, &parent, &vacant).map_err(|e| {
                        Error::Invalid(format!("pull needs no-replace rename on {} ({e})", self.root.display()))
                    })?;
                }
                Ok(())
            })();
            let _ = fs::remove_dir_all(&probe);
            result
        }

        pub(in super::super) fn prepare(
            &self,
            i: usize,
            path: &Path,
            old: &Node,
            content: Option<&Content>,
            keep_mode: bool,
        ) -> Result<Operation> {
            let dest = self.root.join(path);
            let existing = matches!(old, Node::File { .. } | Node::Link(_));
            let recovery = || {
                Displacing::begin(None, Kind::Draft, PULL, &dest, Some(&fingerprint(old)), None, &self.holder)
                    .map(|held| held.id().to_string())
            };
            let Some(content) = content else {
                return Ok(Operation::Remove { recovery: recovery()? });
            };
            let mode = match (old, content) {
                (Node::File { mode, .. }, Content::File { executable, .. }) => {
                    Some(if keep_mode { *mode } else { permissions(*mode, *executable) })
                }
                _ => None,
            };
            let slot = self.slot(i);
            // Before an operation is recorded, its slot only contains our
            // own unpublished file, possibly left by an interrupted write.
            remove_file(&slot)?;
            self.stage(i, content, mode)?;
            if existing {
                let staged = inode(&slot)?.ok_or_else(|| Error::Invalid("pull's staged file is missing".into()))?;
                Ok(Operation::Replace { recovery: recovery()?, staged })
            } else {
                Ok(Operation::Add)
            }
        }

        /// Returns completion even if the destination has since changed.
        pub(in super::super) fn completed(&self, i: usize, operation: &Operation) -> Result<Option<Outcome>> {
            let slot = self.slot(i);
            let completed = match operation {
                Operation::Add => {
                    return Ok(inode(&slot)?.is_none().then_some(Outcome::Done(None)));
                }
                Operation::Remove { recovery } => {
                    let held = Displacing::draft(&self.holder, recovery)?;
                    if !held.filled() {
                        return Ok(None);
                    }
                    held
                }
                Operation::Replace { recovery, staged } => {
                    let mut held = Displacing::draft(&self.holder, recovery)?;
                    if !held.filled() {
                        match inode(&slot)? {
                            Some(at) if at == *staged => return Ok(None),
                            Some(_) => held.take(&slot)?,
                            None => {
                                return Err(Error::Invalid(format!(
                                    "pull's exchange slot {} is missing",
                                    slot.display()
                                )));
                            }
                        }
                    }
                    held
                }
            };
            Ok(Some(Outcome::Done(completed.finish_committed()?)))
        }

        pub(in super::super) fn apply(
            &self,
            i: usize,
            path: &Path,
            node: &Node,
            operation: &Operation,
        ) -> Result<Outcome> {
            let dest = self.root.join(path);
            let (parent, name) = match self.dir.parent(path, matches!(operation, Operation::Add)) {
                Ok(found) => found,
                Err(e) if missing(&e) || e.kind() == io::ErrorKind::AlreadyExists => return Ok(Outcome::Changed),
                Err(e) => return Err(e).at(&dest),
            };
            let slot = self.slot(i);
            let out = match operation {
                Operation::Add => {
                    match read(&parent, &name).at(&dest)? {
                        Node::Directory if *node == Node::Directory => {
                            if !prune_empty(&parent, &name).at(&dest)? {
                                return Ok(Outcome::Changed);
                            }
                        }
                        Node::Missing => {}
                        _ => return Ok(Outcome::Changed),
                    }
                    publish(&slot, &parent, &name)
                }
                Operation::Remove { recovery } | Operation::Replace { recovery, .. } => {
                    let mut held = Displacing::draft(&self.holder, recovery)?;
                    if read(&parent, &name).at(&dest)? != *node || !held.expects(&fingerprint(node)) {
                        return Ok(Outcome::Changed);
                    }
                    match operation {
                        Operation::Remove { .. } => {
                            crate::failpoint!(PullRemove, &dest);
                            if !held.take_at(&parent, &name)? {
                                return Ok(Outcome::Changed);
                            }
                            Ok(())
                        }
                        _ => {
                            crate::failpoint!(PullSwap, &dest);
                            exchange(&slot, &parent, &name)
                        }
                    }
                }
            };
            match out {
                Ok(()) => {}
                Err(Errno::NOENT | Errno::EXIST | Errno::NOTDIR | Errno::ISDIR) => return Ok(Outcome::Changed),
                Err(e) => return Err(io::Error::from(e)).at(&dest),
            }
            crate::failpoint!(PullPublished, &dest);
            self.completed(i, operation)?
                .ok_or_else(|| Error::Invalid("pull's operation left no completion record".into()))
        }

        /// An interrupted adoption must leave both inodes reachable.
        pub(in super::super) fn adopt(&self, path: &Path, node: &Node, entry: &Entry) -> Result<()> {
            let dest = self.root.join(path);
            let missing = *node == Node::Missing;
            let (parent, name) = self.dir.parent(path, missing).at(&dest)?;
            if read(&parent, &name).at(&dest)? != *node {
                return Err(Error::Changed(format!("{} changed during recovery adoption", dest.display())));
            }
            if missing {
                publish(&entry.path, &parent, &name).map_err(io::Error::from).at(&dest)?;
                return Displacing::forget_empty(&entry.id);
            }
            let held = Displacing::begin(None, Kind::Draft, PULL, &dest, Some(&fingerprint(node)), None, &self.holder)?;
            if let Err(error) = exchange(&entry.path, &parent, &name) {
                held.abandon();
                return Err(io::Error::from(error)).at(&dest);
            }
            let mut held = held;
            // Until this move succeeds, the old destination is still at the
            // original recovery entry. Neither name is ever unlinked here.
            held.take(&entry.path)?;
            Displacing::forget_empty(&entry.id)?;
            held.finish(false)?.map_or(Ok(()), Err)
        }

        /// Writes `content` at `path` only while nothing is there, creating
        /// its parents: `false` (and nothing written) if something is, so a
        /// save made there meanwhile is never replaced.
        pub(in super::super) fn place_new(
            &self,
            path: &Path,
            content: &Content,
            staged: &mut dyn FnMut() -> Result<()>,
        ) -> Result<bool> {
            let dest = self.root.join(path);
            let (parent, name) = self.dir.parent(path, true).at(&dest)?;
            if read(&parent, &name).at(&dest)? != Node::Missing {
                return Ok(false);
            }
            let slot = super::keep_stage(&self.holder).join(super::KEEP_SLOT);
            remove_file(&slot)?;
            let slot = self.stage_as(slot, content, None)?;
            staged()?;
            let published = publish(&slot, &parent, &name);
            let _ = fs::remove_file(&slot);
            if let Some(stage) = slot.parent() {
                let _ = fs::remove_dir(stage);
            }
            match published {
                Ok(()) => Ok(true),
                Err(Errno::EXIST) => Ok(false),
                Err(e) => Err(io::Error::from(e)).at(&dest),
            }
        }

        /// Moves what is at `path`, as long as it is `node`, into a new
        /// recovery entry: a Draft deletion that keeps what it removes.
        pub(in super::super) fn take_out(&self, path: &Path, node: &Node) -> Result<()> {
            let dest = self.root.join(path);
            let (parent, name) = self.dir.parent(path, false).at(&dest)?;
            if read(&parent, &name).at(&dest)? != *node {
                return Err(Error::Changed(format!("{} changed while it was taken out", dest.display())));
            }
            let mut held =
                Displacing::begin(None, Kind::Draft, PULL, &dest, Some(&fingerprint(node)), None, &self.holder)?;
            match held.take_at(&parent, &name) {
                Ok(true) => held.finish(false)?.map_or(Ok(()), Err),
                Ok(false) => {
                    held.abandon();
                    Ok(())
                }
                Err(e) => {
                    held.abandon();
                    Err(e)
                }
            }
        }

        fn slot(&self, i: usize) -> PathBuf {
            self.stage.join(i.to_string())
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
            self.stage_as(self.slot(i), content, permissions)
        }

        fn stage_as(&self, staged: PathBuf, content: &Content, permissions: Option<u32>) -> Result<PathBuf> {
            let dir = staged.parent().unwrap_or(&self.stage);
            match fs::DirBuilder::new().mode(0o700).create(dir) {
                Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e).at(dir),
                _ => {}
            }
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

    fn exchange(staged: &Path, parent: &OwnedFd, name: &OsStr) -> std::result::Result<(), Errno> {
        if let Some(e) = crate::failpoint_error!(PullExchange, staged) {
            return Err(Errno::from_io_error(&e).unwrap_or(Errno::OPNOTSUPP));
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        return fd::renameat_with(CWD, staged, parent, name, fd::RenameFlags::EXCHANGE);
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            let _ = (staged, parent, name);
            Err(Errno::OPNOTSUPP)
        }
    }

    fn publish(staged: &Path, parent: &OwnedFd, name: &OsStr) -> std::result::Result<(), Errno> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        return fd::renameat_with(CWD, staged, parent, name, fd::RenameFlags::NOREPLACE);
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            let _ = (staged, parent, name);
            Err(Errno::OPNOTSUPP)
        }
    }

    fn fingerprint(node: &Node) -> String {
        match node {
            Node::File { bytes, mode } => leaf_fingerprint(Some(*mode), bytes),
            Node::Link(target) => leaf_fingerprint(None, target),
            Node::Missing | Node::Directory | Node::Other => String::new(),
        }
    }

    fn inode(path: &Path) -> Result<Option<Inode>> {
        match fs::symlink_metadata(path) {
            Ok(meta) => Ok(Some(Inode { device: meta.dev(), number: meta.ino() })),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).at(path),
        }
    }

    fn remove_file(path: &Path) -> Result<()> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).at(path),
        }
    }

    fn same_mount(a: &OwnedFd, b: &OwnedFd) -> io::Result<bool> {
        if fd::fstat(a)?.st_dev != fd::fstat(b)?.st_dev {
            return Ok(false);
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let mount_id = |dir: &OwnedFd| {
                let mask = fd::StatxFlags::MNT_ID;
                let stat = fd::statx(dir, "", AtFlags::EMPTY_PATH, mask).ok()?;
                (stat.stx_mask & mask.bits() != 0).then_some(stat.stx_mnt_id)
            };
            if let (Some(a), Some(b)) = (mount_id(a), mount_id(b)) {
                return Ok(a == b);
            }
        }
        Ok(true)
    }

    /// Removes empty descendants before their parent. Files, symlinks and
    /// import staging directories are obstructions, even if empty.
    fn prune_empty(parent: &OwnedFd, name: &OsStr) -> io::Result<bool> {
        use crate::placement::DIRECTORY;
        let dir = match fd::openat(parent, name, DIRECTORY, Mode::empty()) {
            Ok(dir) => dir,
            Err(Errno::NOENT) => return Ok(true),
            Err(Errno::NOTDIR | Errno::LOOP) => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        if !same_mount(parent, &dir)? {
            return Ok(false);
        }
        for entry in fd::Dir::read_from(&dir)? {
            let entry = entry?;
            let name = entry.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            if crate::source::is_staging_path(Path::new(OsStr::from_bytes(name)))
                || !prune_empty(&dir, OsStr::from_bytes(name))?
            {
                return Ok(false);
            }
        }
        match fd::unlinkat(parent, name, AtFlags::REMOVEDIR) {
            Ok(()) | Err(Errno::NOENT) => Ok(true),
            Err(Errno::NOTEMPTY | Errno::EXIST | Errno::NOTDIR) => Ok(false),
            Err(e) => Err(e.into()),
        }
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
    use super::super::{Change, journal::Operation};
    use super::{Content, Node, Outcome};
    use crate::sync::displaced::Entry;
    use crate::{Error, Result};
    use std::path::Path;

    pub(in super::super) struct Worktree;

    impl Worktree {
        pub(in super::super) fn open(_: &Path, _: &Path) -> Result<Self> {
            Err(Error::Invalid("pull can only change files on Unix".into()))
        }
        pub(in super::super) fn read(&self, _: &Path) -> Result<Node> {
            unreachable!()
        }
        pub(in super::super) fn preflight(&self, _: &[Change]) -> Result<()> {
            unreachable!()
        }
        pub(in super::super) fn prepare(
            &self,
            _: usize,
            _: &Path,
            _: &Node,
            _: Option<&Content>,
            _: bool,
        ) -> Result<Operation> {
            unreachable!()
        }
        pub(in super::super) fn completed(&self, _: usize, _: &Operation) -> Result<Option<Outcome>> {
            unreachable!()
        }
        pub(in super::super) fn apply(&self, _: usize, _: &Path, _: &Node, _: &Operation) -> Result<Outcome> {
            unreachable!()
        }
        pub(in super::super) fn adopt(&self, _: &Path, _: &Node, _: &Entry) -> Result<()> {
            unreachable!()
        }
        pub(in super::super) fn take_out(&self, _: &Path, _: &Node) -> Result<()> {
            unreachable!()
        }
        pub(in super::super) fn place_new(
            &self,
            _: &Path,
            _: &Content,
            _: &mut dyn FnMut() -> Result<()>,
        ) -> Result<bool> {
            unreachable!()
        }
        pub(in super::super) fn prune<'p>(&self, _: impl Iterator<Item = &'p Path>) {
            unreachable!()
        }
        pub(in super::super) fn clear(&self) {
            unreachable!()
        }
    }
}
