//! Ownership, pending placements, and the lock shared by directory writers.
use super::*;
use crate::config_file;

pub const INDEX_PATH: &str = ".sk1ll-state/index.json";
pub(super) const INDEX_DIR: &str = ".sk1ll-state";
pub const PLACEMENT_MARKER: &str = ".sk1ll-placement";
pub(super) const INDEX_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileId {
    pub(super) dev: u64,
    pub(super) ino: u64,
    /// Birth time where available, to distinguish files if an inode is reused.
    pub(super) born: Option<u64>,
}

impl FileId {
    pub(super) fn read(file: &fs::File, meta: &fs::Metadata, path: &Path) -> Result<Self> {
        #[cfg(unix)]
        let (dev, ino) = {
            use std::os::unix::fs::MetadataExt;
            let _ = (file, path);
            (meta.dev(), meta.ino())
        };
        #[cfg(windows)]
        let (dev, ino) = {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle};
            let mut info = BY_HANDLE_FILE_INFORMATION::default();
            // SAFETY: the file handle stays open and the struct is writable for the call.
            if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
                return Err(std::io::Error::last_os_error()).at(path);
            }
            (
                u64::from(info.dwVolumeSerialNumber),
                (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
            )
        };
        let born = meta
            .created()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|time| u64::try_from(time.as_nanos()).ok());
        Ok(Self { dev, ino, born })
    }
}

impl std::fmt::Display for FileId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{:?}", self.dev, self.ino, self.born)
    }
}

/// Ownership recorded for a harness entry in its directory's index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum IndexEntry {
    Link { hash: String, target: PathBuf },
    Copy { hash: String, token: String },
    Rule { hash: String, file: FileId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase", deny_unknown_fields)]
pub(super) enum Intent {
    Place { next: IndexEntry },
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Index {
    version: u32,
    // A pending name's entry is the previous placement. Both maps are
    // committed in the same atomic index write.
    pub(super) entries: BTreeMap<String, IndexEntry>,
    pub(super) pending: BTreeMap<String, Intent>,
}

impl Default for Index {
    fn default() -> Self {
        Self { version: INDEX_VERSION, entries: BTreeMap::new(), pending: BTreeMap::new() }
    }
}

impl Index {
    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.pending.is_empty()
    }

    pub(super) fn set(&mut self, name: &str, entry: Option<IndexEntry>, intent: Option<Intent>) {
        match entry {
            Some(entry) => {
                self.entries.insert(name.into(), entry);
            }
            None => {
                self.entries.remove(name);
            }
        }
        match intent {
            Some(intent) => {
                self.pending.insert(name.into(), intent);
            }
            None => {
                self.pending.remove(name);
            }
        }
    }

    pub(super) fn settled(&self, name: &str, dest: &Path) -> Result<Option<IndexEntry>> {
        let previous = self.entries.get(name);
        let Some(intent) = self.pending.get(name) else { return Ok(previous.cloned()) };
        if let Intent::Place { next } = intent
            && holds(dest, next)?
        {
            return Ok(Some(next.clone()));
        }
        match previous {
            Some(previous) if holds(dest, previous)? => Ok(Some(previous.clone())),
            _ => Ok(None),
        }
    }

    /// A read-only view of ownership as recovery would settle it.
    pub(super) fn owned(self, dir: &Path) -> Result<BTreeMap<String, IndexEntry>> {
        Ok(self.settle(dir)?.entries)
    }

    pub(super) fn settle(mut self, dir: &Path) -> Result<Self> {
        let names: Vec<_> = self.pending.keys().cloned().collect();
        for name in names {
            let owned = self.settled(&name, &dir.join(&name))?;
            self.set(&name, owned, None);
        }
        Ok(self)
    }
}

pub(super) fn index_path(dir: &Path) -> Result<PathBuf> {
    let state = dir.join(INDEX_DIR);
    match state.symlink_metadata() {
        Ok(meta) if !meta.is_dir() => {
            return Err(Error::Invalid(format!("{} is not a plain directory", state.display())));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).at(&state),
    }
    Ok(dir.join(INDEX_PATH))
}

pub(super) fn load_index(dir: &Path) -> Result<Index> {
    let path = index_path(dir)?;
    let Some(text) = config_file::read_text(&path, false)? else { return Ok(Index::default()) };
    let index: Index =
        serde_json::from_str(&text).map_err(|e| Error::Parse { path: path.clone(), message: e.to_string() })?;
    if index.version != INDEX_VERSION {
        return Err(Error::Parse { path, message: format!("unsupported index version {}", index.version) });
    }
    if let Some(name) =
        index.entries.keys().chain(index.pending.keys()).find(|n| Path::new(n).file_name() != Some(n.as_str().as_ref()))
    {
        return Err(Error::Parse { path, message: format!("invalid entry name {name:?}") });
    }
    Ok(index)
}

pub(super) fn index_entry(dest: &Path) -> Result<Option<IndexEntry>> {
    let dir = dest.parent().ok_or_else(|| Error::Invalid(format!("{} has no parent", dest.display())))?;
    let name = dest.file_name().unwrap_or_default().to_string_lossy();
    load_index(dir)?.settled(&name, dest)
}

fn create_state_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).at(dir)?;
    let state = dir.join(INDEX_DIR);
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    match builder.create(&state) {
        Ok(()) => config_file::sync_dir(&state),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => index_path(dir).map(|_| ()),
        Err(e) => Err(e).at(&state),
    }
}

fn write_index(dir: &Path, index: &Index, unchanged: impl FnOnce() -> Result<()>) -> Result<()> {
    let path = index_path(dir)?;
    if index.is_empty() {
        unchanged()?;
        return match fs::remove_file(&path) {
            Ok(()) => config_file::sync_dir(&path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).at(&path),
        };
    }
    create_state_dir(dir)?;
    let body =
        serde_json::to_vec_pretty(index).map_err(|e| Error::Parse { path: path.clone(), message: e.to_string() })?;
    let stage = config_file::Staged::write(&path, &body, config_file::existing_mode(&path))?;
    unchanged()?;
    stage.replace()
}

#[cfg(test)]
pub(super) fn save_index(dir: &Path, index: &Index) -> Result<()> {
    write_index(dir, index, || Ok(()))
}

/// Held across recovery, staging and publication. Sync never unlinks the
/// lock file, so independent processes always contend on the same inode.
pub(super) struct DirLock<'a> {
    pub(super) dir: &'a Path,
    file: fs::File,
}

impl<'a> DirLock<'a> {
    pub(super) fn acquire(dir: &'a Path) -> Result<Self> {
        let (path, file) = lock_file(dir)?;
        file.lock().at(&path)?;
        Ok(Self { dir, file })
    }

    pub(super) fn check_planned(&self, dest: &Path, name: &str, expected: Option<&IndexEntry>) -> Result<()> {
        match load_index(self.dir).and_then(|index| index.settled(name, dest)) {
            Ok(entry) if entry.as_ref() == expected => Ok(()),
            Ok(_) | Err(Error::Parse { .. } | Error::Invalid(_)) => Err(changed_since_plan(dest)),
            Err(e) => Err(e),
        }
    }

    pub(super) fn save(&self, before: &Index, after: &Index, dest: &Path) -> Result<()> {
        write_index(self.dir, after, || match load_index(self.dir) {
            Ok(index) if index == *before => Ok(()),
            Ok(_) | Err(Error::Parse { .. } | Error::Invalid(_)) => Err(changed_since_plan(dest)),
            Err(e) => Err(e),
        })
    }
}

impl Drop for DirLock<'_> {
    fn drop(&mut self) {
        // A child forked on another thread can still hold an inherited
        // descriptor until it reaches exec.
        let _ = self.file.unlock();
    }
}

fn lock_file(dir: &Path) -> Result<(PathBuf, fs::File)> {
    create_state_dir(dir)?;
    let path = dir.join(INDEX_DIR).join("lock");
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = options.open(&path).at(&path)?;
    if !file.metadata().at(&path)?.is_file() || path.symlink_metadata().at(&path)?.is_symlink() {
        return Err(Error::Invalid(format!("{} is not a plain file", path.display())));
    }
    Ok((path, file))
}

/// A compare-and-set for one name. Reading the latest index preserves
/// updates to other names. Every write checks both the previous placement
/// and the intent for this name.
pub(super) struct Ownership<'a> {
    lock: &'a DirLock<'a>,
    dest: &'a Path,
    name: &'a str,
    expected: Option<IndexEntry>,
    intent: RefCell<Option<Intent>>,
}

impl<'a> Ownership<'a> {
    pub(super) fn new(lock: &'a DirLock<'a>, dest: &'a Path, name: &'a str, expected: Option<&IndexEntry>) -> Self {
        Self { lock, dest, name, expected: expected.cloned(), intent: RefCell::new(None) }
    }

    fn checked_index(&self) -> Result<Index> {
        match load_index(self.lock.dir) {
            Ok(index)
                if index.entries.get(self.name) == self.expected.as_ref()
                    && index.pending.get(self.name) == self.intent.borrow().as_ref() =>
            {
                Ok(index)
            }
            Ok(_) | Err(Error::Parse { .. } | Error::Invalid(_)) => Err(changed_since_plan(self.dest)),
            Err(e) => Err(e),
        }
    }

    pub(super) fn check(&self) -> Result<()> {
        self.checked_index().map(drop)
    }

    pub(super) fn record(&self, entry: Option<IndexEntry>) -> Result<()> {
        self.change(entry, None)
    }

    pub(super) fn begin(&self, intent: Intent) -> Result<()> {
        self.change(self.expected.clone(), Some(intent.clone()))?;
        self.intent.replace(Some(intent));
        Ok(())
    }

    fn change(&self, entry: Option<IndexEntry>, intent: Option<Intent>) -> Result<()> {
        let before = self.checked_index()?;
        let mut after = before.clone();
        after.set(self.name, entry, intent);
        self.lock.save(&before, &after, self.dest)
    }

    pub(super) fn finish(&self, result: Result<Option<Error>>) -> Result<()> {
        if self.intent.borrow().is_some() {
            // Persist the destination name before clearing the intent. It
            // also settles a failed swap that left the old entry in place.
            let finish = || {
                config_file::sync_dir(self.dest)?;
                let index = self.checked_index()?;
                self.record(index.settled(self.name, self.dest)?)
            };
            let settled = finish();
            if result.is_ok() {
                settled?;
            }
        }
        result?.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independent_opens_contend_on_the_same_directory_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let first = DirLock::acquire(tmp.path()).unwrap();
        let (_, second) = lock_file(tmp.path()).unwrap();
        assert!(matches!(second.try_lock(), Err(std::fs::TryLockError::WouldBlock)));
        drop(first);
        second.try_lock().unwrap();
    }
}
