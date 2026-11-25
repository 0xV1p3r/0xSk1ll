//! Ownership, pending placements, and the lock shared by directory writers.
use super::*;
use crate::config_file;

pub const INDEX_PATH: &str = ".sk1ll-state/index.json";
pub(super) const INDEX_DIR: &str = ".sk1ll-state";
pub const PLACEMENT_MARKER: &str = ".sk1ll-placement";
pub(super) const INDEX_VERSION: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileId {
    pub(super) dev: u64,
    pub(super) ino: u64,
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
        Ok(Self { dev, ino })
    }
}

impl std::fmt::Display for FileId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.dev, self.ino)
    }
}

/// Ownership recorded for a harness entry in its directory's index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum IndexEntry {
    Link { hash: String, target: PathBuf, payload: String },
    Copy { hash: String, token: String, payload: String },
    Rule { hash: String, file: FileId, payload: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase", deny_unknown_fields)]
pub(super) enum Intent {
    Place { next: IndexEntry, recovery: Option<String> },
    Remove { recovery: Option<String> },
}

impl IndexEntry {
    pub(super) fn payload(&self) -> &str {
        match self {
            Self::Link { payload, .. } | Self::Copy { payload, .. } | Self::Rule { payload, .. } => payload,
        }
    }

    fn anchor_name(&self) -> Option<String> {
        match self {
            Self::Copy { token, .. } if token.len() == 32 && token.bytes().all(|b| b.is_ascii_hexdigit()) => {
                Some(format!("copy-{token}"))
            }
            Self::Rule { file, .. } => Some(format!("rule-{}-{}", file.dev, file.ino)),
            _ => None,
        }
    }
}

impl Intent {
    pub(super) fn with_recovery(mut self, id: Option<String>) -> Self {
        match &mut self {
            Self::Place { recovery, .. } | Self::Remove { recovery } => *recovery = id,
        }
        self
    }

    pub(super) fn recovery(&self) -> Option<&str> {
        match self {
            Self::Place { recovery, .. } | Self::Remove { recovery } => recovery.as_deref(),
        }
    }

    pub(super) fn keeps_backup(&self, owned: bool, dest: &Path) -> bool {
        !owned && (matches!(self, Self::Place { .. }) || dest.symlink_metadata().is_ok())
    }
}

pub(super) fn create_anchor(dir: &Path, entry: &IndexEntry, file: &Path) -> Result<()> {
    let anchors = dir.join(INDEX_DIR).join("anchors");
    mkdir_private(&anchors)?;
    let name = entry.anchor_name().ok_or_else(|| Error::Invalid("missing placement anchor name".into()))?;
    let anchor = anchors.join(name);
    match fs::hard_link(file, &anchor) {
        Ok(()) => config_file::sync_dir(&anchor),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && same_anchor(dir, entry, file)? => Ok(()),
        Err(e) => Err(Error::Invalid(format!("{}: could not make a hard-link placement anchor: {e}", file.display()))),
    }
}

pub(super) fn same_anchor(dir: &Path, entry: &IndexEntry, file: &Path) -> Result<bool> {
    let Some(name) = entry.anchor_name() else { return Ok(false) };
    let state = dir.join(INDEX_DIR);
    let anchors = state.join("anchors");
    for path in [&state, &anchors] {
        match path.symlink_metadata() {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Ok(false),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e).at(path),
        }
    }
    let id = rule_identity(file)?;
    Ok(id.is_some() && id == rule_identity(&anchors.join(name))?)
}

pub(super) fn retire_anchors(lock: &DirLock) -> Result<()> {
    let index = load_index(lock.dir)?;
    let anchors = lock.dir.join(INDEX_DIR).join("anchors");
    if !anchors.symlink_metadata().is_ok_and(|meta| meta.is_dir()) {
        return Ok(());
    }
    let mut live: BTreeSet<_> = index.entries.values().filter_map(IndexEntry::anchor_name).collect();
    live.extend(index.pending.values().filter_map(|op| match op {
        Intent::Place { next, .. } => next.anchor_name(),
        Intent::Remove { .. } => None,
    }));
    for name in read_dir_names(&anchors)? {
        if (name.starts_with("copy-") || name.starts_with("rule-")) && !live.contains(&name) {
            fs::remove_file(anchors.join(name)).at(&anchors)?;
        }
    }
    config_file::sync_dir(&anchors.join("anchor-publication"))
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
        if let Intent::Place { next, .. } = intent
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
    let backup = index_backup(dir);
    let source = if missing(&path)?
        && backup.parent().is_some_and(|p| p.symlink_metadata().is_ok_and(|m| m.is_dir()))
        && backup.symlink_metadata().is_ok_and(|m| m.is_file())
    {
        &backup
    } else {
        &path
    };
    let Some(text) = config_file::read_text(source, false)? else { return Ok(Index::default()) };
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
    mkdir_private(&dir.join(INDEX_DIR))
}

pub(super) fn mkdir_private(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    match builder.create(path) {
        Ok(()) => config_file::sync_dir(path),
        Err(e)
            if e.kind() == std::io::ErrorKind::AlreadyExists && path.symlink_metadata().is_ok_and(|m| m.is_dir()) =>
        {
            Ok(())
        }
        Err(e) => Err(e).at(path),
    }
}

fn index_backup(dir: &Path) -> PathBuf {
    dir.join(INDEX_DIR).join("index-rollback").join("previous")
}

fn missing(path: &Path) -> Result<bool> {
    match path.symlink_metadata() {
        Ok(_) => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e).at(path),
    }
}

const HISTORY: &str = "index-history";
const BACKUP_META: &str = "entry.json";
const BACKUP_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct IndexBackup {
    version: u32,
    payload: String,
    displaced_at: std::time::SystemTime,
}

fn archive_index(dir: &Path, rollback: &Path) -> Result<()> {
    let history = dir.join(INDEX_DIR).join(HISTORY);
    mkdir_private(&history)?;
    let dest = history.join(new_token()?);
    crate::placement::rename_noreplace(rollback, &dest).at(&dest)?;
    config_file::sync_dir(&dest)?;
    config_file::sync_dir(rollback)
}

pub(super) fn prune_indexes(lock: &DirLock, grace: std::time::Duration) -> Result<()> {
    let history = lock.dir.join(INDEX_DIR).join(HISTORY);
    match history.symlink_metadata() {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Err(Error::Invalid(format!("{} is not a plain recovery directory", history.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).at(&history),
    }
    for name in read_dir_names(&history)? {
        let archived = history.join(name);
        if !archived.symlink_metadata().is_ok_and(|meta| meta.is_dir()) {
            continue;
        }
        let Some(text) = config_file::read_text(&archived.join(BACKUP_META), false)? else { continue };
        let Ok(meta) = serde_json::from_str::<IndexBackup>(&text) else { continue };
        // Never recursive: a changed or extra entry keeps the archive.
        if meta.version == BACKUP_VERSION
            && meta.displaced_at.elapsed().is_ok_and(|age| age >= grace)
            && crate::hash::remove_payload(&archived.join("previous"), &meta.payload)?
        {
            displaced::remove_if_there(&archived.join(BACKUP_META))?;
            match fs::remove_dir(&archived) {
                Err(e) if e.kind() != std::io::ErrorKind::DirectoryNotEmpty => return Err(e).at(&archived),
                _ => {}
            }
        }
    }
    Ok(())
}

fn write_index(dir: &Path, index: &Index, unchanged: impl FnOnce() -> Result<()>) -> Result<()> {
    let path = index_path(dir)?;
    create_state_dir(dir)?;
    let backup = index_backup(dir);
    let rollback = backup.parent().expect("backup has a parent");
    mkdir_private(rollback)?;
    if !missing(&backup)? {
        if missing(&path)? {
            crate::placement::rename_noreplace(&backup, &path).at(&path)?;
            config_file::sync_dir(&path)?;
            config_file::sync_dir(&backup)?;
        } else {
            archive_index(dir, rollback)?;
            mkdir_private(rollback)?;
        }
    }
    let scratch = Scratch::open(dir)?;
    let stage = if index.is_empty() {
        None
    } else {
        let body = serde_json::to_vec_pretty(index)
            .map_err(|e| Error::Parse { path: path.clone(), message: e.to_string() })?;
        Some(config_file::Staged::write(&scratch.path().join("index.json"), &body, config_file::existing_mode(&path))?)
    };
    let expected =
        if missing(&path)? { None } else { Some((crate::hash::hash_payload(&path)?, rule_identity(&path)?)) };
    unchanged()?;
    if let Some((payload, _)) = &expected {
        let meta = IndexBackup {
            version: BACKUP_VERSION,
            payload: payload.clone(),
            displaced_at: std::time::SystemTime::now(),
        };
        let metadata = serde_json::to_vec(&meta).map_err(|e| Error::Invalid(e.to_string()))?;
        config_file::replace_private(&rollback.join(BACKUP_META), &metadata)?;
    }
    crate::failpoint!(IndexBeforeMove, &path);
    let swap = crate::placement::swap_managed(
        stage.as_ref().map(config_file::Staged::path),
        &path,
        expected.as_ref().map(|_| backup.as_path()),
        || {
            if let Some(expected) = &expected {
                config_file::sync_dir(&backup)?;
                config_file::sync_dir(rollback)?;
                config_file::sync_dir(&path)?;
                if (crate::hash::hash_payload(&backup)?, rule_identity(&backup)?) != *expected {
                    return Err(changed_since_plan(&path));
                }
            }
            if stage.is_none() && !missing(&path)? {
                return Err(changed_since_plan(&path));
            }
            Ok(())
        },
    );
    let result = match swap {
        Ok(result) => result,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(changed_since_plan(&path)),
        Err(e) => Err(e).at(&path),
    };
    if result.is_err() {
        if !missing(&backup)? {
            // A competing publication leaves both versions available.
            if crate::placement::rename_noreplace(&backup, &path).is_ok() {
                config_file::sync_dir(&path)?;
                config_file::sync_dir(&backup)?;
            } else {
                displaced::remove_if_there(&rollback.join(BACKUP_META))?;
                config_file::sync_dir(&backup)?;
            }
        }
        return result;
    }
    config_file::sync_dir(&path)?;
    if expected.is_some() {
        archive_index(dir, rollback)?;
    }
    Ok(())
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

    pub(super) fn placed_payload(&self) -> Result<Option<&str>> {
        match &self.expected {
            Some(entry) if holds(self.dest, entry)? => Ok(Some(entry.payload())),
            _ => Ok(None),
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
                let owned = index.settled(self.name, self.dest)?;
                if let Some(intent) = self.intent.borrow().as_ref()
                    && let Some(id) = intent.recovery()
                {
                    displaced::settle(id, self.dest, intent.keeps_backup(owned.is_some(), self.dest))?;
                }
                self.record(owned)
            };
            let settled = finish();
            if result.is_ok() {
                settled?;
            }
        }
        result?.map_or(Ok(()), Err)
    }
}

/// Private staging directory. On Linux the descriptor path keeps writes
/// and cleanup attached to the directory they opened.
pub(super) struct Scratch {
    path: PathBuf,
    #[cfg(unix)]
    _directory: std::os::fd::OwnedFd,
}

impl Scratch {
    pub(super) fn open(dir: &Path) -> Result<Self> {
        let dir = dir.canonicalize().at(dir)?;
        let state = dir.join(INDEX_DIR);
        index_path(&dir)?;
        let path = state.join("staging");
        mkdir_private(&path)?;
        Self::pin(&path)
    }

    pub(super) fn pin(path: &Path) -> Result<Self> {
        #[cfg(unix)]
        let directory = crate::placement::open_directory(path, false).at(path)?;
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let path = path.to_path_buf();
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let path = {
            use std::os::fd::AsRawFd;
            PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()))
        };
        Ok(Self {
            path,
            #[cfg(unix)]
            _directory: directory,
        })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failpoints::Point;

    #[test]
    fn independent_opens_contend_on_the_same_directory_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let first = DirLock::acquire(tmp.path()).unwrap();
        let (_, second) = lock_file(tmp.path()).unwrap();
        assert!(matches!(second.try_lock(), Err(std::fs::TryLockError::WouldBlock)));
        drop(first);
        second.try_lock().unwrap();
    }

    #[test]
    fn an_index_edit_after_validation_survives_replacement_and_removal() {
        for remove in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path();
            let lock = DirLock::acquire(dir).unwrap();
            let path = dir.join(INDEX_PATH);
            let link = |target: &str| IndexEntry::Link {
                hash: target.into(),
                target: PathBuf::from(target),
                payload: "fixture".into(),
            };
            let mut before = Index::default();
            before.set("skill", Some(link("before")), None);
            save_index(dir, &before).unwrap();
            let mut after = Index::default();
            if !remove {
                after.set("skill", Some(link("next")), None);
            }
            let mut edit = before.clone();
            edit.set("another", Some(link("editor")), None);
            let expected = edit.clone();
            let replaced = super::super::tests::with_hook(
                Point::IndexBeforeMove,
                move |path| {
                    let staged = path.with_extension("editor");
                    fs::write(&staged, serde_json::to_vec(&edit).unwrap()).unwrap();
                    fs::rename(staged, path).unwrap();
                },
                || lock.save(&before, &after, &path),
            );
            assert_eq!(replaced.unwrap_err().to_string(), changed_since_plan(&path).to_string());
            assert_eq!(load_index(dir).unwrap(), expected);
        }
    }

    #[test]
    fn an_interrupted_index_publication_keeps_the_previous_index_readable() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let lock = DirLock::acquire(dir).unwrap();
        let path = dir.join(INDEX_PATH);
        let backup = index_backup(dir);
        let mut before = Index::default();
        before.set(
            "skill",
            Some(IndexEntry::Link { hash: "link".into(), target: "/repo/skill".into(), payload: "fixture".into() }),
            None,
        );
        save_index(dir, &before).unwrap();
        crate::placement::detach(&path, &backup).unwrap();
        assert_eq!(load_index(dir).unwrap(), before);
        lock.save(&before, &Index::default(), &path).unwrap();
        assert!(load_index(dir).unwrap().is_empty());
        assert!(!backup.exists());
    }

    #[test]
    fn a_write_through_an_open_index_descriptor_keeps_its_archived_version() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let lock = DirLock::acquire(dir).unwrap();
        let path = dir.join(INDEX_PATH);
        let mut before = Index::default();
        let link =
            |target: &str| IndexEntry::Link { hash: target.into(), target: target.into(), payload: "fixture".into() };
        before.set("skill", Some(link("first")), None);
        save_index(dir, &before).unwrap();
        let mut open = fs::OpenOptions::new().append(true).open(&path).unwrap();
        let mut after = before.clone();
        after.set("skill", Some(link("second")), None);
        lock.save(&before, &after, &path).unwrap();
        open.write_all(b"\nlate edit").unwrap();
        let history = dir.join(INDEX_DIR).join(HISTORY);
        let saved = history.join(read_dir_names(&history).unwrap().remove(0)).join("previous");
        assert!(fs::read_to_string(&saved).unwrap().ends_with("late edit"));
        lock.save(&after, &Index::default(), &path).unwrap();
        assert_eq!(read_dir_names(&history).unwrap().len(), 2);
        // An archive holding anything else is never deleted recursively.
        let other =
            read_dir_names(&history).unwrap().into_iter().find(|n| !saved.starts_with(history.join(n))).unwrap();
        let stray = history.join(&other).join("stray");
        fs::write(&stray, "not sk1ll's").unwrap();
        prune_indexes(&lock, std::time::Duration::ZERO).unwrap();
        assert_eq!(fs::read_to_string(&stray).unwrap(), "not sk1ll's");
        fs::remove_file(&stray).unwrap();
        fs::remove_dir(history.join(other)).unwrap();
        assert_eq!(read_dir_names(&history).unwrap().len(), 1);
        assert!(fs::read_to_string(&saved).unwrap().ends_with("late edit"));
        assert!(load_index(dir).unwrap().is_empty());
    }
}
