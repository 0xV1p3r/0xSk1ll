//! The daemon's persisted state: who this machine is in the fleet and how to
//! reach the server. Holds the machine token, so it is written with mode 0600;
//! the token is never written anywhere else (git gets it from the
//! environment, see [`crate::auth`]).
//!
//! It also keeps what a restart must not forget about the server's Baseline
//! ([`PullRecord`]): the Baseline last known and whether the Machine is
//! Blocked on it. A standalone daemon has no state file and keeps that record
//! in [`standalone_record_path`].
//!
//! Next to it, also private: the last assignment snapshot the server sent
//! (revision and content together, so they can't disagree), tagged with the
//! registration it belongs to ([`DaemonState::registration_key`]): a
//! snapshot of another registration (one a rejoin replaced, half-written
//! when the process stopped) is never loaded as this one's.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use sk1ll_proto::{AssignmentSnapshot, ItemRef};

/// Where the Machine stands against the server's Baseline: what the last pull
/// (or reconcile) found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PullState {
    /// On the Baseline, or not knowingly behind it.
    #[default]
    Clear,
    /// `baseline` clashes with these Draft items, so the Machine stays on its
    /// old Baseline (Blocked).
    Blocked { baseline: String, items: Vec<ItemRef> },
    /// A pull stopped half-way and has yet to finish, so the working tree is
    /// partial. `items`: the Draft items in its way, if that is what stopped it.
    Pending { items: Vec<ItemRef> },
}

/// What the daemon remembers about the server's Baseline across restarts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRecord {
    /// The server's Baseline as last announced, fetched or pushed.
    #[serde(default)]
    pub server_baseline: Option<String>,
    #[serde(default)]
    pub pull: PullState,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonState {
    pub machine_id: String,
    /// Base URL, e.g. `https://server.example`, without a trailing slash.
    pub server_url: String,
    /// Machine token: API bearer token and git password. Shown once at join.
    pub token: String,
    /// The local clone.
    pub repo: PathBuf,
    /// The machine was removed from the fleet; the daemon no longer manages it.
    #[serde(default)]
    pub revoked: bool,
    /// As of the last time the daemon wrote it; the running daemon's
    /// lifecycle owns it.
    #[serde(default, flatten)]
    pub record: PullRecord,
}

impl std::fmt::Debug for DaemonState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonState")
            .field("machine_id", &self.machine_id)
            .field("server_url", &crate::auth::redact(&self.server_url))
            .field("token", &"<redacted>")
            .field("repo", &self.repo)
            .field("revoked", &self.revoked)
            .field("record", &self.record)
            .finish()
    }
}

impl DaemonState {
    /// A new registration: not revoked, nothing known about the Baseline yet.
    pub fn new(machine_id: String, server_url: String, token: String, repo: PathBuf) -> Self {
        Self { machine_id, server_url, token, repo, revoked: false, record: PullRecord::default() }
    }

    /// Identifies this registration locally: each join gets a new token.
    pub fn registration_key(&self) -> String {
        sk1ll_core::hash::hash_str(&format!("{}\0{}\0{}", self.server_url, self.machine_id, self.token))
    }

    pub(crate) fn rename_authority(&self) -> String {
        self.registration_key()
    }

    /// `None` if the machine hasn't joined.
    pub fn load(path: &Path) -> anyhow::Result<Option<Self>> {
        match fs::read_to_string(path) {
            Ok(text) => Ok(Some(serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => match fs::symlink_metadata(path) {
                Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => Ok(None),
                _ => Err(e).with_context(|| format!("reading {}", path.display())),
            },
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let body = serde_json::to_string_pretty(self)?;
        write_private(path, format!("{body}\n").as_bytes())
    }

    /// The repo's `origin`: `http://host/git/repo.git`, without credentials.
    pub fn repo_url(&self) -> String {
        format!("{}{}", self.server_url, sk1ll_proto::REPO_PATH)
    }

    /// `ws(s)://host/api/ws`.
    pub fn ws_url(&self) -> String {
        let url = &self.server_url;
        let ws = if let Some(rest) = url.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = url.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            url.clone()
        };
        format!("{ws}/api/ws")
    }
}

/// Where a standalone daemon keeps its [`PullRecord`]: it has no state file,
/// whose presence means joined.
pub fn standalone_record_path(state: &Path) -> PathBuf {
    state.with_file_name("standalone-state.json")
}

/// Which repo a standalone [`PullRecord`] is about: its canonical path and
/// git directory, and the git directory's inode, so a record never follows
/// another repo to the same path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoIdentity {
    pub root: PathBuf,
    pub git_dir: PathBuf,
    pub dev: u64,
    pub ino: u64,
}

impl RepoIdentity {
    /// `None` while `root` is no git repo (yet).
    pub fn of(root: &Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let root = root.canonicalize().ok()?;
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["rev-parse", "--absolute-git-dir"])
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())?;
        let text = String::from_utf8(out.stdout).ok()?;
        let git_dir = Path::new(text.trim_end_matches('\n')).canonicalize().ok()?;
        let meta = fs::metadata(&git_dir).ok()?;
        Some(Self { root, git_dir, dev: meta.dev(), ino: meta.ino() })
    }
}

#[derive(Serialize, Deserialize)]
struct StandaloneRecord {
    #[serde(default)]
    repo: Option<RepoIdentity>,
    #[serde(flatten)]
    record: PullRecord,
}

/// A standalone daemon's [`PullRecord`] for the repo at `root`; the default
/// if there is none, or if it is about another repo (or none).
pub fn load_record(path: &Path, root: &Path) -> anyhow::Result<PullRecord> {
    let stored: StandaloneRecord = match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(PullRecord::default()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let here = RepoIdentity::of(root);
    if stored.repo.is_none() || stored.repo != here {
        if stored.record != PullRecord::default() {
            tracing::info!("{} is about another repo; starting without a remembered Baseline", path.display());
        }
        return Ok(PullRecord::default());
    }
    Ok(stored.record)
}

pub fn save_record(path: &Path, root: &Path, record: &PullRecord) -> anyhow::Result<()> {
    let stored = StandaloneRecord { repo: RepoIdentity::of(root), record: record.clone() };
    let body = serde_json::to_string_pretty(&stored)?;
    write_private(path, format!("{body}\n").as_bytes())
}

/// A stored snapshot and the registration it belongs to (`None`: standalone).
#[derive(Serialize, Deserialize)]
pub(crate) struct StoredSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
    #[serde(flatten)]
    snapshot: AssignmentSnapshot,
}

impl StoredSnapshot {
    pub(crate) fn new(snapshot: &AssignmentSnapshot, owner: Option<&str>) -> Self {
        Self { owner: owner.map(str::to_string), snapshot: snapshot.clone() }
    }
}

/// The persisted assignment snapshot. `Ok(None)` if there is none, or if
/// `owner` is given and the snapshot belongs to another registration (or to
/// none); an unreadable one is an error, which the daemon treats like none.
pub fn load_snapshot(path: &Path, owner: Option<&str>) -> anyhow::Result<Option<AssignmentSnapshot>> {
    let stored: StoredSnapshot = match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
        // A symlink to nowhere is a broken snapshot, not a missing one.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && fs::symlink_metadata(path).is_ok() => {
            return Err(e).with_context(|| format!("reading {}: a symlink to a missing file", path.display()));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if owner.is_some() && stored.owner.as_deref() != owner {
        tracing::warn!("{} belongs to another registration of this Machine; ignoring it", path.display());
        return Ok(None);
    }
    Ok(Some(stored.snapshot))
}

/// Writes the snapshot (revision and assignment in one file) atomically,
/// tagged with `owner`, the registration it belongs to.
pub fn save_snapshot(path: &Path, snap: &AssignmentSnapshot, owner: Option<&str>) -> anyhow::Result<()> {
    let body = serde_json::to_string_pretty(&StoredSnapshot::new(snap, owner))?;
    write_private(path, format!("{body}\n").as_bytes())
}

/// Assignment for direct commands. Joined machines require the authoritative
/// snapshot; an absent mirror must never turn their disabled items back on.
pub fn load_assignment(cfg: &crate::Config) -> anyhow::Result<sk1ll_proto::Assignment> {
    if let Some(s) = DaemonState::load(&cfg.state)?.filter(|s| !s.revoked) {
        return load_snapshot(&cfg.snapshot_path(), Some(&s.registration_key()))?.map(|s| s.assignment).context(
            "no authoritative assignment is stored; start the daemon and connect to the server \
                      before changing harnesses",
        );
    }
    Ok(sk1ll_proto::Assignment::load(&cfg.assignment)?)
}

/// Like [`load_assignment`], except that a joined machine with no
/// authoritative snapshot yet (still waiting for the server's) is `None`.
/// Every other failure (an unreadable state, a corrupt snapshot) is an error.
pub fn load_assignment_if_present(cfg: &crate::Config) -> anyhow::Result<Option<sk1ll_proto::Assignment>> {
    if let Some(s) = DaemonState::load(&cfg.state)?.filter(|s| !s.revoked) {
        return Ok(load_snapshot(&cfg.snapshot_path(), Some(&s.registration_key()))?.map(|s| s.assignment));
    }
    Ok(Some(sk1ll_proto::Assignment::load(&cfg.assignment)?))
}

/// `url` without `user:password@`, for comparing remotes.
pub(crate) fn without_credentials(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let (host, path) = rest.split_once('/').map_or((rest, ""), |(h, p)| (h, p));
            let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
            let path = path.trim_end_matches('/');
            if path.is_empty() { format!("{scheme}://{host}") } else { format!("{scheme}://{host}/{path}") }
        }
        None => url.trim_end_matches('/').to_string(),
    }
}

/// Whether `url` carries `user[:password]@`.
pub(crate) fn has_credentials(url: &str) -> bool {
    url.split_once("://").is_some_and(|(_, rest)| rest.split('/').next().is_some_and(|host| host.contains('@')))
}

/// Writes `path` atomically with mode 0600, creating its directory (0700).
pub(crate) fn write_private(path: &Path, body: &[u8]) -> anyhow::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = path.parent().context("state path has no parent")?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    sk1ll_core::config_file::replace_private(path, body)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_write_ignores_staging_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.json");
        let target = dir.path().join("existing");
        fs::write(&target, b"keep").unwrap();
        for name in [".daemon.json.tmp", ".daemon.json.sk1ll-tmp"] {
            symlink(&target, dir.path().join(name)).unwrap();
        }

        write_private(&path, b"private").unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"keep");
        assert_eq!(fs::read(&path).unwrap(), b"private");
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    }

    fn state() -> DaemonState {
        DaemonState::new("ws".into(), "https://server.example:8443".into(), "t0k".into(), "/r".into())
    }

    #[test]
    fn urls() {
        let s = state();
        assert_eq!(s.repo_url(), "https://server.example:8443/git/repo.git");
        assert_eq!(s.ws_url(), "wss://server.example:8443/api/ws");
        assert_eq!(without_credentials("https://ws:t0k@server.example:8443/git/repo.git"), s.repo_url());
        assert_eq!(without_credentials("http://h/git/repo.git/"), "http://h/git/repo.git");
        assert!(has_credentials("https://ws:t0k@server.example/git/repo.git"));
        assert!(!has_credentials("https://server.example/git/a@b"));
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_snapshot_link_is_an_error_not_a_missing_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.json");
        assert!(load_snapshot(&path, None).unwrap().is_none());
        std::os::unix::fs::symlink(dir.path().join("gone.json"), &path).unwrap();
        let err = load_snapshot(&path, None).unwrap_err();
        assert!(format!("{err:#}").contains("symlink to a missing file"), "{err:#}");
    }

    #[test]
    fn debug_hides_the_token() {
        let text = format!("{:?}", state());
        assert!(!text.contains("t0k"), "{text}");
    }

    #[test]
    fn state_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sk1ll/daemon.json");
        let s = DaemonState { token: "secret".into(), ..state() };
        s.save(&path).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(DaemonState::load(&path).unwrap(), Some(s));
        assert_eq!(DaemonState::load(&dir.path().join("none")).unwrap(), None);
        // Older state files carried the assignment revision; it is ignored now.
        fs::write(&path, r#"{"machine_id":"m","server_url":"http://h","token":"t","repo":"/r","revision":4}"#).unwrap();
        assert!(DaemonState::load(&path).unwrap().is_some());
    }

    #[test]
    fn the_baseline_record_survives_a_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sk1ll/daemon.json");
        let record = PullRecord {
            server_baseline: Some("abc".into()),
            pull: PullState::Blocked { baseline: "abc".into(), items: vec!["skill:pdf".parse().unwrap()] },
        };
        let s = DaemonState { record: record.clone(), ..state() };
        s.save(&path).unwrap();
        assert_eq!(DaemonState::load(&path).unwrap().unwrap().record, record);
        // A state file without one knows nothing about the Baseline.
        fs::write(&path, r#"{"machine_id":"m","server_url":"http://h","token":"t","repo":"/r"}"#).unwrap();
        assert_eq!(DaemonState::load(&path).unwrap().unwrap().record, PullRecord::default());

        let standalone = standalone_record_path(&path);
        let repo = dir.path().join("repo");
        sk1ll_core::Repo::init(&repo).unwrap();
        assert_eq!(load_record(&standalone, &repo).unwrap(), PullRecord::default());
        save_record(&standalone, &repo, &record).unwrap();
        assert_eq!(load_record(&standalone, &repo).unwrap(), record);
    }

    /// A standalone record is about one repo: another repo, at another path
    /// or re-created at the same one, starts without it.
    #[test]
    fn a_standalone_record_is_not_restored_for_another_repo() {
        let dir = tempfile::tempdir().unwrap();
        let path = standalone_record_path(&dir.path().join("sk1ll/daemon.json"));
        let record = PullRecord {
            server_baseline: Some("abc".into()),
            pull: PullState::Blocked { baseline: "abc".into(), items: vec!["skill:pdf".parse().unwrap()] },
        };
        let (one, other) = (dir.path().join("one"), dir.path().join("other"));
        sk1ll_core::Repo::init(&one).unwrap();
        sk1ll_core::Repo::init(&other).unwrap();
        save_record(&path, &one, &record).unwrap();
        assert_eq!(load_record(&path, &other).unwrap(), PullRecord::default());
        assert_eq!(load_record(&path, &one).unwrap(), record);
        fs::remove_dir_all(&one).unwrap();
        assert_eq!(load_record(&path, &one).unwrap(), PullRecord::default(), "no repo there");
        fs::rename(&other, &one).unwrap();
        assert_eq!(load_record(&path, &one).unwrap(), PullRecord::default(), "another repo at the same path");
        // A record without a repo (an older one) is about none.
        fs::write(&path, r#"{"server_baseline":"abc","pull":{"state":"clear"}}"#).unwrap();
        assert_eq!(load_record(&path, &one).unwrap(), PullRecord::default());
    }

    #[test]
    fn snapshots_keep_revision_and_content_together() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sk1ll/daemon-assignment.json");
        assert_eq!(load_snapshot(&path, None).unwrap(), None);
        let mut snap = AssignmentSnapshot { revision: 7, ..Default::default() };
        snap.assignment.set_skill("h", &"x".parse().unwrap(), false);
        save_snapshot(&path, &snap, None).unwrap();
        assert_eq!(load_snapshot(&path, None).unwrap(), Some(snap.clone()));
        // Joined, a snapshot belongs to one registration.
        save_snapshot(&path, &snap, Some("old")).unwrap();
        assert_eq!(load_snapshot(&path, Some("old")).unwrap(), Some(snap.clone()));
        assert_eq!(load_snapshot(&path, Some("new")).unwrap(), None, "another registration's");
        save_snapshot(&path, &snap, None).unwrap();
        assert_eq!(load_snapshot(&path, Some("new")).unwrap(), None, "an untagged one");
        fs::write(&path, "{broken").unwrap();
        assert!(load_snapshot(&path, None).is_err());
    }
}
