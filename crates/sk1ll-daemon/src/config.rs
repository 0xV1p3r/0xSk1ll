use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
#[cfg(not(test))]
use sk1ll_core::{Assignment, MachineConfig};

/// How often a full reconcile runs by default (see [`Config::full_reconcile_every`]).
pub const FULL_RECONCILE_EVERY: Duration = Duration::from_secs(600);

/// Daemon settings: where its files live and how often it does things.
#[derive(Debug, Clone)]
pub struct Config {
    /// Standalone repo override (SK1LL_REPO); otherwise locate the nearest repo or ~/.local/share/sk1ll/repo.
    pub repo: Option<PathBuf>,
    /// Machine id, server URL, token and repo path (`SK1LL_DAEMON_STATE`,
    /// default `~/.config/sk1ll/daemon.json`). Mode 0600.
    pub state: PathBuf,
    /// Harness config (`SK1LL_MACHINE_CONFIG`, default `~/.config/sk1ll/machine.toml`).
    pub machine_config: PathBuf,
    /// Assignment cache (`SK1LL_ASSIGNMENT`, default `~/.cache/sk1ll/assignment.json`).
    /// Authoritative in standalone; a mirror of the server snapshot when joined.
    pub assignment: PathBuf,
    /// sk1ll's own state directory (`SK1LL_STATE_DIR`, default
    /// `$XDG_STATE_HOME/sk1ll`, else the platform's data directory's
    /// `sk1ll`): the machine lock's files (`locks/`) and a pending Harness
    /// rename (`harness-rename/`), each named by the hash of the real path of
    /// the file it is about, so none of them lands beside a user's file.
    pub state_dir: PathBuf,
    /// sk1ll's own cache directory (`SK1LL_CACHE_DIR`, default
    /// `$XDG_CACHE_HOME/sk1ll`): import sources are fetched into `git/`.
    pub cache_dir: PathBuf,
    /// Local API socket (`SK1LL_DAEMON_SOCKET`, default
    /// `$XDG_RUNTIME_DIR/sk1ll/daemon.sock`, else `run/daemon.sock` in the
    /// config dir, as on macOS). Its directory must be ours and mode 0700.
    pub socket: PathBuf,
    /// Fallback pull while connected (`SK1LL_PULL_INTERVAL`, default `5m`).
    pub pull_every: Duration,
    /// Quiet time before reacting to file changes (`SK1LL_DEBOUNCE`, default `300ms`).
    pub debounce: Duration,
    /// Longest a stream of file changes can postpone a sync
    /// (`SK1LL_DEBOUNCE_MAX`, default `2s`, at least `debounce`).
    pub debounce_max: Duration,
    /// Local re-sync of the harnesses, whether or not the server is reachable
    /// (`SK1LL_RECONCILE_INTERVAL`, default `60s`).
    pub reconcile_every: Duration,
    /// A reconcile that plans and rehashes everything, whatever the input
    /// fingerprint and the hash cache say, at most this long after the last
    /// one (`SK1LL_FULL_RECONCILE_INTERVAL`, default `10m`): what metadata
    /// misses is noticed by then, give or take one `reconcile_every`.
    pub full_reconcile_every: Duration,
    /// After the server has rejected the token this long, `/status` says the
    /// machine was probably removed (`SK1LL_AUTH_WARN_AFTER`, default `24h`).
    /// It is never revoked automatically.
    pub auth_warn_after: Duration,
    /// First reconnect delay; doubles up to `reconnect_max` (jittered).
    pub reconnect_min: Duration,
    /// `SK1LL_RECONNECT_MAX`, default `60s`.
    pub reconnect_max: Duration,
}

impl Config {
    /// Defaults from the platform's config, cache and runtime dirs. In this
    /// crate's own tests, from a temporary directory instead, which core's
    /// state and cache directories follow too: no test reaches the real
    /// home directory.
    pub fn new() -> anyhow::Result<Self> {
        #[cfg(test)]
        {
            let home = test_home();
            Ok(Self::with_paths(
                home.join("config/daemon.json"),
                home.join("config/machine.toml"),
                home.join("cache/assignment.json"),
                home.join("state"),
                home.join("cache"),
                home.join("run/daemon.sock"),
            ))
        }
        #[cfg(not(test))]
        Ok(Self::with_paths(
            config_dir()?.join("daemon.json"),
            MachineConfig::default_path()?,
            Assignment::default_path()?,
            sk1ll_core::paths::state_dir()?,
            sk1ll_core::paths::cache_dir()?,
            Self::default_socket()?,
        ))
    }

    fn with_paths(
        state: PathBuf,
        machine_config: PathBuf,
        assignment: PathBuf,
        state_dir: PathBuf,
        cache_dir: PathBuf,
        socket: PathBuf,
    ) -> Self {
        Self {
            repo: None,
            state,
            machine_config,
            assignment,
            state_dir,
            cache_dir,
            socket,
            pull_every: Duration::from_secs(300),
            debounce: Duration::from_millis(300),
            debounce_max: Duration::from_secs(2),
            reconcile_every: Duration::from_secs(60),
            full_reconcile_every: FULL_RECONCILE_EVERY,
            auth_warn_after: Duration::from_secs(24 * 3600),
            reconnect_min: Duration::from_millis(500),
            reconnect_max: Duration::from_secs(60),
        }
    }

    /// [`Config::new`] adjusted by `SK1LL_*` environment variables.
    pub fn from_env() -> anyhow::Result<Self> {
        let mut cfg = Self::new()?;
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        if let Some(v) = var("SK1LL_REPO") {
            cfg.repo = Some(v.into());
        }
        if let Some(v) = var("SK1LL_DAEMON_STATE") {
            cfg.state = v.into();
        }
        if let Some(v) = var("SK1LL_MACHINE_CONFIG") {
            cfg.machine_config = v.into();
        }
        if let Some(v) = var("SK1LL_ASSIGNMENT") {
            cfg.assignment = v.into();
        }
        if let Some(v) = var("SK1LL_STATE_DIR") {
            cfg.state_dir = v.into();
        }
        if let Some(v) = var("SK1LL_CACHE_DIR") {
            cfg.cache_dir = v.into();
        }
        if let Some(v) = var("SK1LL_DAEMON_SOCKET") {
            cfg.socket = v.into();
        }
        let dur = |k: &str, d: &mut Duration| -> anyhow::Result<()> {
            if let Some(v) = var(k) {
                *d = parse_duration(&v).with_context(|| format!("{k}: `{v}`"))?;
            }
            Ok(())
        };
        dur("SK1LL_PULL_INTERVAL", &mut cfg.pull_every)?;
        dur("SK1LL_DEBOUNCE", &mut cfg.debounce)?;
        dur("SK1LL_DEBOUNCE_MAX", &mut cfg.debounce_max)?;
        dur("SK1LL_RECONCILE_INTERVAL", &mut cfg.reconcile_every)?;
        dur("SK1LL_FULL_RECONCILE_INTERVAL", &mut cfg.full_reconcile_every)?;
        dur("SK1LL_AUTH_WARN_AFTER", &mut cfg.auth_warn_after)?;
        dur("SK1LL_RECONNECT_MAX", &mut cfg.reconnect_max)?;
        cfg.reconnect_min = cfg.reconnect_min.min(cfg.reconnect_max);
        cfg.debounce_max = cfg.debounce_max.max(cfg.debounce);
        Ok(cfg)
    }

    /// The assignment snapshot the daemon applies (revision and content in
    /// one file), next to the state file.
    pub fn snapshot_path(&self) -> PathBuf {
        self.state.with_file_name("daemon-assignment.json")
    }

    /// A standalone Machine's Assignment revision, next to the state file.
    pub fn standalone_assignment_path(&self) -> PathBuf {
        self.state.with_file_name("standalone-assignment.json")
    }

    /// The notifications already shown, next to the state file.
    pub fn notifications_path(&self) -> PathBuf {
        self.state.with_extension("notifications.json")
    }

    /// Every file an engine for this Machine may write, outside the repo,
    /// the harness directories (which have locks of their own) and
    /// [`Config::state_dir`]: what the machine lock covers
    /// ([`crate::MachineLock`]). A pending Harness rename is kept in the
    /// state directory under `machine.toml`'s real path, so `machine.toml`'s
    /// lock covers it.
    pub fn machine_files(&self) -> Vec<PathBuf> {
        vec![
            self.state.clone(),
            self.machine_config.clone(),
            self.assignment.clone(),
            self.snapshot_path(),
            self.standalone_assignment_path(),
            crate::state::standalone_record_path(&self.state),
            self.notifications_path(),
        ]
    }

    /// `SK1LL_DAEMON_SOCKET`, else `$XDG_RUNTIME_DIR/sk1ll/daemon.sock`, else
    /// `run/daemon.sock` in the config dir (a directory of its own, since the
    /// socket's directory must be private). macOS has no runtime dir, so it is
    /// `~/Library/Application Support/sk1ll/run/daemon.sock` there.
    pub fn default_socket() -> anyhow::Result<PathBuf> {
        if let Some(v) = std::env::var_os("SK1LL_DAEMON_SOCKET").filter(|v| !v.is_empty()) {
            return Ok(v.into());
        }
        match dirs::runtime_dir() {
            Some(d) => Ok(d.join("sk1ll").join("daemon.sock")),
            None => Ok(config_dir()?.join("run").join("daemon.sock")),
        }
    }
}

/// The temporary home of this crate's tests (see [`Config::new`]).
#[cfg(test)]
pub(crate) fn test_home() -> &'static std::path::Path {
    static HOME: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        let home = tempfile::tempdir().expect("a temporary directory");
        let dirs = sk1ll_core::paths::Dirs { state: home.path().join("state"), cache: home.path().join("cache") };
        sk1ll_core::paths::use_dirs(dirs);
        home
    })
    .path()
}

fn config_dir() -> anyhow::Result<PathBuf> {
    dirs::config_dir().map(|d| d.join("sk1ll")).context("cannot determine the config directory")
}

/// `500ms`, `90`, `90s`, `2m`, `1h`; a bare number is seconds.
pub fn parse_duration(s: &str) -> anyhow::Result<Duration> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: u64 = num.parse().context("expected a number with an optional ms/s/m/h unit")?;
    let d = match unit.trim() {
        "ms" => Duration::from_millis(n),
        "" | "s" => Duration::from_secs(n),
        "m" => Duration::from_secs(n * 60),
        "h" => Duration::from_secs(n * 3600),
        u => bail!("unknown unit `{u}`; use ms, s, m or h"),
    };
    if d.is_zero() {
        bail!("must be greater than zero");
    }
    Ok(d)
}

/// Selects the same standalone repo for the CLI and daemon. An explicit path
/// wins, then SK1LL_REPO, then the nearest skills repo or ~/.local/share/sk1ll/repo.
pub fn locate_repo(explicit: Option<&std::path::Path>, git_only: bool) -> anyhow::Result<PathBuf> {
    let env = std::env::var_os("SK1LL_REPO").filter(|v| !v.is_empty()).map(PathBuf::from);
    let path = match explicit.map(std::path::Path::to_path_buf).or(env) {
        Some(path) => path,
        None => {
            let cwd = std::env::current_dir()?;
            if git_only { sk1ll_core::Repo::locate_git(&cwd) } else { sk1ll_core::Repo::locate(&cwd) }
                .context("cannot determine repo location")?
        }
    };
    Ok(std::path::absolute(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_default_socket_fits_sun_path() {
        // macOS's sun_path holds 104 bytes with the NUL; a 32-character
        // account name leaves room to spare.
        let home = format!("/Users/{}", "u".repeat(32));
        let socket = std::path::Path::new(&home).join("Library/Application Support/sk1ll/run/daemon.sock");
        assert!(socket.as_os_str().len() < 104, "{}", socket.display());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert!(parse_duration("0s").is_err());
        assert!(parse_duration("3w").is_err());
    }
}
