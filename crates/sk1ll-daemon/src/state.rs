//! The daemon's persisted state: who this machine is in the fleet and how to
//! reach the server. Holds the machine token, so it is written with mode 0600;
//! the token is never written anywhere else (git gets it from the
//! environment, see [`crate::auth`]).
//!
//! Next to it, also private: the last assignment snapshot the server sent
//! (revision and content together, so they can't disagree) and the outbox
//! of `draft_discarded` messages not yet delivered.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use sk1ll_proto::AssignmentSnapshot;

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
}

impl std::fmt::Debug for DaemonState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonState")
            .field("machine_id", &self.machine_id)
            .field("server_url", &crate::auth::redact(&self.server_url))
            .field("token", &"<redacted>")
            .field("repo", &self.repo)
            .field("revoked", &self.revoked)
            .finish()
    }
}

impl DaemonState {
    /// `None` if the machine hasn't joined.
    pub fn load(path: &Path) -> anyhow::Result<Option<Self>> {
        match fs::read_to_string(path) {
            Ok(text) => Ok(Some(serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
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

/// The persisted assignment snapshot. `Ok(None)` if there is none; an
/// unreadable one is an error, which the daemon treats like none.
pub fn load_snapshot(path: &Path) -> anyhow::Result<Option<AssignmentSnapshot>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?)),
        // A symlink to nowhere is a broken snapshot, not a missing one.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && fs::symlink_metadata(path).is_ok() => {
            Err(e).with_context(|| format!("reading {}: a symlink to a missing file", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Writes the snapshot (revision and assignment in one file) atomically.
pub fn save_snapshot(path: &Path, snap: &AssignmentSnapshot) -> anyhow::Result<()> {
    let body = serde_json::to_string_pretty(snap)?;
    write_private(path, format!("{body}\n").as_bytes())
}

/// Assignment for direct commands. Joined machines require the authoritative
/// snapshot; an absent mirror must never turn their disabled items back on.
pub fn load_assignment(cfg: &crate::Config) -> anyhow::Result<sk1ll_proto::Assignment> {
    if DaemonState::load(&cfg.state)?.is_some_and(|s| !s.revoked) {
        return load_snapshot(&cfg.snapshot_path())?.map(|s| s.assignment).context(
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
    if DaemonState::load(&cfg.state)?.is_some_and(|s| !s.revoked) {
        return Ok(load_snapshot(&cfg.snapshot_path())?.map(|s| s.assignment));
    }
    Ok(Some(sk1ll_proto::Assignment::load(&cfg.assignment)?))
}

/// Persists direct-mode restore notifications for the daemon's next connection.
/// The caller must hold the joined repo's engine lock while restoring and
/// appending, so the daemon cannot load the outbox between those operations.
pub fn queue_discards(
    cfg: &crate::Config,
    _lock: &sk1ll_core::EngineLock,
    items: Vec<sk1ll_proto::ItemRef>,
) -> anyhow::Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let state = DaemonState::load(&cfg.state)?.context("not joined to a fleet")?;
    crate::outbox::Outbox::load(cfg.outbox_path(), &state).push(items)
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
        DaemonState {
            machine_id: "ws".into(),
            server_url: "https://server.example:8443".into(),
            token: "t0k".into(),
            repo: "/r".into(),
            revoked: false,
        }
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
        assert!(load_snapshot(&path).unwrap().is_none());
        std::os::unix::fs::symlink(dir.path().join("gone.json"), &path).unwrap();
        let err = load_snapshot(&path).unwrap_err();
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
    fn snapshots_keep_revision_and_content_together() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sk1ll/daemon-assignment.json");
        assert_eq!(load_snapshot(&path).unwrap(), None);
        let mut snap = AssignmentSnapshot { revision: 7, ..Default::default() };
        snap.assignment.set_skill("h", "x", false);
        save_snapshot(&path, &snap).unwrap();
        assert_eq!(load_snapshot(&path).unwrap(), Some(snap));
        fs::write(&path, "{broken").unwrap();
        assert!(load_snapshot(&path).is_err());
    }
}
