use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::{Before, Change, Entry, ItemRef, Pull, Switched, git_error, has_errors, refusal, rev};
use crate::git::{path_bytes, path_from_bytes};
use crate::{Error, IoExt, Result};

type Index = BTreeMap<PathBuf, Vec<Vec<u8>>>;

impl Pull<'_> {
    pub(super) fn switch(
        &self,
        branch: &str,
        old: &str,
        target: &str,
        changes: &[Change],
        hook: &mut dyn FnMut(Before),
        check: impl FnOnce() -> Result<BTreeSet<ItemRef>>,
    ) -> Result<Switched> {
        let branch_ref = format!("refs/heads/{branch}");
        let branch = branch_ref.as_str();
        let disk = Disk::open(&self.root)?;
        let before: Vec<_> = changes.iter().map(|change| disk.read(&change.path)).collect::<Result<_>>()?;
        let index = self.index()?;
        hook(Before::Checks);
        let blocked = check()?;
        if !blocked.is_empty() {
            return Ok(Switched::Refused(blocked));
        }
        hook(Before::Checkout);
        let staging_obstructions: BTreeSet<_> =
            changes.iter().filter(|change| self.reaches_stage(change)).map(|change| self.item(&change.path)).collect();
        if !staging_obstructions.is_empty() {
            return Ok(Switched::Refused(staging_obstructions));
        }
        let checkout = self.checkout(target);
        if let Err(error) = checkout {
            hook(Before::CheckoutFailed);
            let mut problems = match self.index() {
                Ok(current) => {
                    let stderr = match &error {
                        Error::Git { stderr, .. } => stderr.as_bytes(),
                        _ => b"",
                    };
                    let paths = refusal(stderr);
                    let no_updates = self.attached_to(branch).unwrap_or(false)
                        && rev(self.git(), "HEAD").ok().flatten().as_deref() == Some(old)
                        && current == index
                        && (paths.is_some()
                            || changes
                                .iter()
                                .zip(&before)
                                .all(|(change, before)| disk.read(&change.path).is_ok_and(|now| now == *before)));
                    if no_updates {
                        let mut items: BTreeSet<_> = paths.unwrap_or_default().iter().map(|p| self.item(p)).collect();
                        if items.is_empty() {
                            items = self.blocking(&[], &[], changes, &self.tracked(old)?)?;
                        }
                        if items.is_empty() {
                            items.extend(changes.iter().map(|change| self.item(&change.path)));
                        }
                        if !items.is_empty() {
                            return Ok(Switched::Refused(items));
                        }
                        return Err(Error::Invalid(format!("couldn't switch to the new commit during pull ({error})")));
                    }
                    self.recover_paths(&disk, changes, &before, &index, &current)
                }
                Err(index) => vec![format!("couldn't inspect the index: {index}")],
            };
            self.reattach(branch, old, target, &mut problems);
            return Err(recovery_error(error, problems));
        }

        let mut advanced = false;
        let publication = (|| {
            hook(Before::UpdateRef);
            if self.symbolic_head()?.is_some() || rev(self.git(), "HEAD")?.as_deref() != Some(target) {
                return Err(Error::Invalid("detached HEAD changed during pull".into()));
            }
            self.update_branch(branch, target, old, "sk1ll: pull")?;
            advanced = true;
            hook(Before::Attach);
            self.attach(branch)?;
            let actual = rev(self.git(), "HEAD")?;
            if actual.as_deref() != Some(target) || !self.attached_to(branch)? {
                return Err(Error::Invalid(format!("HEAD changed while publishing pull (now {actual:?})")));
            }
            actual.ok_or_else(|| Error::Invalid("HEAD is missing after pull".into()))
        })();
        match publication {
            Ok(actual) => Ok(Switched::To(actual)),
            Err(error) => {
                let mut problems = Vec::new();
                // A completed checkout put the target in HEAD and the index.
                // Use the same two-tree checkout to go back, with its checks.
                let rollback = rev(self.git(), "HEAD").and_then(|head| {
                    if head.as_deref() == Some(target) {
                        self.checkout(old)
                    } else {
                        Err(Error::Invalid("HEAD changed before pull could roll back".into()))
                    }
                });
                if let Err(rollback) = rollback {
                    match self.index() {
                        Ok(current) => problems.extend(self.recover_paths(&disk, changes, &before, &index, &current)),
                        Err(index) => problems.push(format!("can't inspect the index after {rollback}: {index}")),
                    }
                }
                if advanced && let Err(undo) = self.update_branch(branch, old, target, "sk1ll: undo failed pull") {
                    problems.push(format!("couldn't undo the branch update: {undo}"));
                }
                self.reattach(branch, old, target, &mut problems);
                Err(recovery_error(error, problems))
            }
        }
    }

    fn checkout(&self, target: &str) -> Result<()> {
        let out = self
            .git()
            .args([
                "checkout",
                "--quiet",
                "--no-overwrite-ignore",
                "--no-recurse-submodules",
                "--no-merge",
                "--detach",
                target,
            ])
            .output()?;
        // Some Git versions report a worktree write error on stderr after
        // writing the new index and HEAD, but exit successfully.
        if !out.status.success() || has_errors(&out.stderr) {
            return Err(git_error(&format!("checkout --detach {target}"), &out.stderr));
        }
        Ok(())
    }

    fn update_branch(&self, branch: &str, to: &str, expected: &str, message: &str) -> Result<()> {
        let args = ["update-ref", "--no-deref", "-m", message, branch, to, expected];
        let out = self.git().args(args).output()?;
        if out.status.success() {
            return Ok(());
        }
        let mut error = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if let Ok(Some(actual)) = rev(self.git(), branch)
            && actual != expected
        {
            error.push_str(&format!("; another Git command moved {branch} to {actual}"));
        }
        Err(Error::Git { args: args.join(" "), stderr: error })
    }

    fn symbolic_head(&self) -> Result<Option<String>> {
        let out = self.git().args(["symbolic-ref", "--quiet", "HEAD"]).output()?;
        match out.status.code() {
            Some(0) => Ok(Some(String::from_utf8_lossy(&out.stdout).trim().into())),
            Some(1) => Ok(None),
            _ => Err(git_error("symbolic-ref --quiet HEAD", &out.stderr)),
        }
    }

    fn attached_to(&self, branch: &str) -> Result<bool> {
        Ok(self.symbolic_head()?.as_deref() == Some(branch))
    }

    fn attach(&self, branch: &str) -> Result<()> {
        self.git().args(["symbolic-ref", "-m", "sk1ll: attach after pull", "HEAD", branch]).run()?;
        Ok(())
    }

    fn reattach(&self, branch: &str, old: &str, target: &str, problems: &mut Vec<String>) {
        let reattach = (|| {
            if self.attached_to(branch)? {
                return Ok(());
            }
            let head = rev(self.git(), "HEAD")?;
            if self.symbolic_head()?.is_some() || !matches!(head.as_deref(), Some(h) if h == old || h == target) {
                return Err(Error::Invalid("HEAD moved independently".into()));
            }
            self.attach(branch)
        })();
        if let Err(error) = reattach {
            problems.push(format!("couldn't reattach HEAD to {branch}: {error}"));
        }
    }

    fn index(&self) -> Result<Index> {
        let out = self.git().args(["ls-files", "--stage", "-z"]).run()?;
        let mut index = Index::new();
        for entry in out.split(|b| *b == 0).filter(|entry| !entry.is_empty()) {
            let Some(tab) = entry.iter().position(|b| *b == b'\t') else {
                return Err(Error::Invalid("invalid index entry during pull".into()));
            };
            if entry.starts_with(b"160000 ") {
                return Err(super::unsupported_submodules());
            }
            index.entry(path_from_bytes(&entry[tab + 1..])).or_default().push(entry[..tab].to_vec());
        }
        Ok(index)
    }

    fn recover_paths(
        &self,
        disk: &Disk,
        changes: &[Change],
        previous_files: &[File],
        index: &Index,
        current_index: &Index,
    ) -> Vec<String> {
        let mut problems = Vec::new();
        let mut reset_index = Vec::new();
        // Remove added leaf entries first, so file/directory swaps can be
        // undone without recursively deleting anything in the worktree.
        let mut ordering: Vec<_> = changes.iter().zip(previous_files).collect();
        ordering.sort_by_key(|(_, before)| before.is_leaf());
        for (change, before) in ordering {
            let path = &change.path;
            let recover = (|| {
                let current = current_index.get(path);
                let previous = index.get(path);
                if current != previous && !matches_index(current, change.new.as_ref()) {
                    return Err(Error::Changed("the index entry was edited".into()));
                }
                if current != previous {
                    reset_index.push((change, previous));
                }

                let now = disk.read(path)?;
                if &now == before || (!now.is_leaf() && !before.is_leaf()) {
                    return Ok(());
                }
                let contents = match &change.new {
                    Some(entry) => self.git().args(["cat-file", "blob", &entry.oid]).run()?,
                    None => Vec::new(),
                };
                // A directory where a removed file used to be can be removed
                // only if it is empty when the previous file is put back.
                if !now.matches(change.new.as_ref(), &contents) {
                    return Err(Error::Changed("the file differs from both checkout versions".into()));
                }
                disk.restore(path, &now, before)?;
                if !before.is_leaf()
                    && now.is_leaf()
                    && let Some(ancestor) = path.ancestors().skip(1).find(|parent| {
                        changes.iter().zip(previous_files).any(|(change, old)| change.path == *parent && old.is_leaf())
                    })
                {
                    disk.prune(path, ancestor);
                }
                Ok(())
            })();
            if let Err(error) = recover {
                problems.push(format!("{} ({error})", path.display()));
            }
        }
        if !reset_index.is_empty() {
            let update = self.restore_index(&reset_index, current_index);
            if let Err(error) = update {
                problems.push(format!("couldn't restore the index: {error}"));
            }
        }
        problems
    }

    fn restore_index(&self, reset: &[(&Change, Option<&Vec<Vec<u8>>>)], expected: &Index) -> Result<()> {
        let raw = self.git().args(["rev-parse", "--path-format=absolute", "--git-path", "index"]).run()?;
        let index = path_from_bytes(raw.strip_suffix(b"\n").unwrap_or(&raw));
        let mut lock_name = index.as_os_str().to_os_string();
        lock_name.push(".lock");
        let lock_name = PathBuf::from(lock_name);
        let mut lock = fs::OpenOptions::new().write(true).create_new(true).open(&lock_name).at(&lock_name)?;
        let mut guard = IndexLock(Some(&lock_name));
        // Check under the same lock Git uses, so recovery can't overwrite
        // something another Git process just staged.
        if &self.index()? != expected {
            return Err(Error::Changed("the index changed during pull recovery".into()));
        }
        let temporary = self.hooks.join("restore.index");
        let _temporary = IndexLock(Some(&temporary));
        fs::copy(&index, &temporary).at(&index)?;
        let mut input = Vec::new();
        for (change, _) in reset {
            let oid_len = change.old.as_ref().or(change.new.as_ref()).unwrap().oid.len();
            input.extend_from_slice(format!("0 {}\t", "0".repeat(oid_len)).as_bytes());
            input.extend_from_slice(&path_bytes(&change.path));
            input.push(0);
        }
        for (change, entries) in reset {
            for entry in entries.iter().flat_map(|entries| entries.iter()) {
                input.extend_from_slice(entry);
                input.push(b'\t');
                input.extend_from_slice(&path_bytes(&change.path));
                input.push(0);
            }
        }
        self.git()
            .env("GIT_INDEX_FILE", &temporary)
            .args(["-c", "core.splitIndex=false", "update-index", "-z", "--index-info"])
            .stdin(input)
            .run()?;
        io::copy(&mut fs::File::open(&temporary).at(&temporary)?, &mut lock).at(&lock_name)?;
        lock.sync_all().at(&lock_name)?;
        drop(lock);
        fs::rename(&lock_name, &index).at(&index)?;
        guard.0 = None;
        Ok(())
    }
}

struct IndexLock<'a>(Option<&'a Path>);

impl Drop for IndexLock<'_> {
    fn drop(&mut self) {
        if let Some(path) = self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

fn matches_index(entries: Option<&Vec<Vec<u8>>>, expected: Option<&Entry>) -> bool {
    match (entries, expected) {
        (None, None) => true,
        (Some(entries), Some(entry)) if entries.len() == 1 => {
            entries[0] == format!("{} {} 0", entry.mode, entry.oid).as_bytes()
        }
        _ => false,
    }
}

fn recovery_error(cause: Error, problems: Vec<String>) -> Error {
    let recovery = if problems.is_empty() {
        "pull put back what it had changed".to_string()
    } else {
        format!("pull recovery left these changes in place: {}", problems.join(", "))
    };
    match cause {
        Error::Git { args, stderr } => Error::Git { args, stderr: format!("{stderr}; {recovery}") },
        cause => Error::Invalid(format!("couldn't finish pull ({cause}); {recovery}")),
    }
}

#[derive(PartialEq, Eq)]
enum File {
    Missing,
    Directory,
    Regular { bytes: Vec<u8>, mode: u32 },
    Link(PathBuf),
    Other,
}

impl File {
    fn is_leaf(&self) -> bool {
        matches!(self, Self::Regular { .. } | Self::Link(_) | Self::Other)
    }

    fn matches(&self, entry: Option<&Entry>, contents: &[u8]) -> bool {
        match (self, entry.map(|entry| entry.mode.as_str())) {
            (Self::Missing | Self::Directory, None) => true,
            (Self::Link(target), Some("120000")) => path_bytes(target) == contents,
            (Self::Regular { bytes, mode }, Some("100644")) => *mode & 0o100 == 0 && bytes == contents,
            (Self::Regular { bytes, mode }, Some("100755")) => *mode & 0o100 != 0 && bytes == contents,
            _ => false,
        }
    }
}

#[cfg(unix)]
use unix::Disk;

#[cfg(unix)]
mod unix {
    use std::ffi::OsStr;
    use std::io::{self, Read};
    use std::os::fd::OwnedFd;

    use rustix::fs::{self as fd, AtFlags, FileType, Mode, OFlags};
    use rustix::io::Errno;

    use super::{Error, File, Path, Result};
    use crate::IoExt;
    use crate::placement::{Directory, replace_file};

    pub(super) struct Disk(Directory);

    impl Disk {
        pub(super) fn open(root: &Path) -> Result<Self> {
            Ok(Self(Directory::open(root, false).at(root)?))
        }

        pub(super) fn read(&self, path: &Path) -> Result<File> {
            match self.0.parent(path, false) {
                Ok((parent, name)) => read(&parent, &name).at(path),
                Err(error) if missing(&error) => Ok(File::Missing),
                Err(error) => Err(error).at(path),
            }
        }

        pub(super) fn restore(&self, path: &Path, expected: &File, old: &File) -> Result<()> {
            let (parent, name) = self.0.parent(path, old.is_leaf()).at(path)?;
            let unchanged = || {
                if read(&parent, &name).at(path)? == *expected {
                    Ok(true)
                } else {
                    Err(Error::Changed("the file changed during pull recovery".into()))
                }
            };
            if let (File::Regular { bytes, mode }, File::Regular { .. } | File::Link(_)) = (old, expected) {
                replace_file(&parent, &name, bytes, *mode, unchanged).at(path)??;
                return Ok(());
            }
            unchanged()?;
            match expected {
                File::Missing => {}
                File::Directory => {
                    fd::unlinkat(&parent, &name, AtFlags::REMOVEDIR).map_err(io::Error::from).at(path)?
                }
                _ => fd::unlinkat(&parent, &name, AtFlags::empty()).map_err(io::Error::from).at(path)?,
            }
            match old {
                File::Regular { bytes, mode } => self.0.create_file(path, bytes, *mode & 0o100 != 0).at(path)?,
                File::Link(target) => self.0.create_symlink(path, target.as_os_str()).at(path)?,
                File::Directory => {
                    // A nonempty directory is rebuilt by restoring its
                    // tracked children. An empty one may have been present.
                    match fd::mkdirat(&parent, &name, Mode::from_raw_mode(0o755)) {
                        Ok(()) | Err(Errno::EXIST) => {}
                        Err(error) => return Err(io::Error::from(error)).at(path),
                    }
                }
                File::Missing => {}
                File::Other => return Err(Error::Invalid("can't restore a special file".into())),
            }
            Ok(())
        }

        pub(super) fn prune(&self, path: &Path, ancestor: &Path) {
            // These directories took the place of an old file. Remove only
            // empty ones, and leave other directories alone.
            for dir in path.ancestors().skip(1).take_while(|dir| *dir != ancestor) {
                let Ok((parent, name)) = self.0.parent(dir, false) else { break };
                if fd::unlinkat(&parent, &name, AtFlags::REMOVEDIR).is_err() {
                    break;
                }
            }
        }
    }

    fn missing(error: &io::Error) -> bool {
        matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP))
    }

    fn read(parent: &OwnedFd, name: &OsStr) -> io::Result<File> {
        let stat = match fd::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(Errno::NOENT) => return Ok(File::Missing),
            Err(error) => return Err(error.into()),
        };
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::RegularFile => {
                let file = fd::openat(
                    parent,
                    name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                    Mode::empty(),
                )?;
                let stat = fd::fstat(&file)?;
                if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
                    return Ok(File::Other);
                }
                let mut bytes = Vec::new();
                std::fs::File::from(file).read_to_end(&mut bytes)?;
                Ok(File::Regular { bytes, mode: stat.st_mode as u32 & 0o7777 })
            }
            FileType::Directory => Ok(File::Directory),
            FileType::Symlink => {
                Ok(File::Link(super::path_from_bytes(fd::readlinkat(parent, name, Vec::new())?.as_bytes())))
            }
            _ => Ok(File::Other),
        }
    }
}

#[cfg(not(unix))]
struct Disk;

#[cfg(not(unix))]
impl Disk {
    fn open(_: &Path) -> Result<Self> {
        Err(Error::Invalid("pull needs descriptor-relative checkout recovery".into()))
    }

    fn read(&self, _: &Path) -> Result<File> {
        unreachable!()
    }

    fn restore(&self, _: &Path, _: &File, _: &File) -> Result<()> {
        unreachable!()
    }

    fn prune(&self, _: &Path, _: &Path) {
        unreachable!()
    }
}
