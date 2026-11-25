use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use sk1ll_core::failpoints::{self, Guard};

/// Variables that would point a spawned sk1ll binary away from its HOME.
const SK1LL_ENV: &[&str] = &[
    "SK1LL_REPO",
    "SK1LL_STATE_DIR",
    "SK1LL_CACHE_DIR",
    "SK1LL_DAEMON_SOCKET",
    "SK1LL_DAEMON_STATE",
    "SK1LL_MACHINE_CONFIG",
    "SK1LL_ASSIGNMENT",
];

/// An empty HOME of its own, with XDG directories inside it: what a spawned
/// binary finds as its home, config, data, state, cache and runtime
/// directories. Removed when dropped.
#[derive(Debug)]
pub struct Home {
    dir: tempfile::TempDir,
}

impl Default for Home {
    fn default() -> Self {
        Self::new()
    }
}

impl Home {
    /// A new, empty HOME in a temporary directory.
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        // The daemon's socket directory must exist and be private.
        let run = dir.path().join("run");
        std::fs::create_dir(&run).unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&run, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        Self { dir }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Points `cmd` at this HOME: `HOME` and the XDG variables, no `SK1LL_*`
    /// overrides from the test's own environment, isolated git
    /// ([`crate::git::isolate`]).
    pub fn apply<'a>(&self, cmd: &'a mut Command) -> &'a mut Command {
        let h = self.path();
        for var in SK1LL_ENV {
            cmd.env_remove(var);
        }
        cmd.env("HOME", h)
            .env("XDG_CONFIG_HOME", h.join(".config"))
            .env("XDG_DATA_HOME", h.join(".local/share"))
            .env("XDG_STATE_HOME", h.join(".local/state"))
            .env("XDG_CACHE_HOME", h.join(".cache"))
            .env("XDG_RUNTIME_DIR", h.join("run"));
        crate::git::isolate(cmd)
    }

    /// `program`, run with this HOME (see [`apply`](Self::apply)).
    pub fn command(&self, program: impl AsRef<OsStr>) -> Command {
        let mut cmd = Command::new(program);
        self.apply(&mut cmd);
        cmd
    }

    /// sk1ll's config directory as a binary run with this HOME finds it.
    pub fn config_dir(&self) -> PathBuf {
        self.path().join(".config").join("sk1ll")
    }

    /// sk1ll's data directory (where the default repo is).
    pub fn data_dir(&self) -> PathBuf {
        self.path().join(".local/share").join("sk1ll")
    }

    /// sk1ll's state directory (the recovery area is in it).
    pub fn state_dir(&self) -> PathBuf {
        self.path().join(".local/state").join("sk1ll")
    }

    /// sk1ll's cache directory.
    pub fn cache_dir(&self) -> PathBuf {
        self.path().join(".cache").join("sk1ll")
    }

    /// The daemon's default socket.
    pub fn socket(&self) -> PathBuf {
        self.path().join("run/sk1ll").join("daemon.sock")
    }

    /// sk1ll's default repo for a binary run with this HOME.
    pub fn default_repo(&self) -> PathBuf {
        self.data_dir().join("repo")
    }

    /// The state and cache directories, as core takes them.
    pub fn dirs(&self) -> sk1ll_core::paths::Dirs {
        sk1ll_core::paths::Dirs { state: self.state_dir(), cache: self.cache_dir() }
    }

    /// Isolates sk1ll code run in-process on this thread until the guard
    /// drops: core's state and cache directories are this HOME's, and the
    /// git commands core runs are isolated ([`crate::git::ENV`]). Threads
    /// the code starts itself (a daemon's engine) need
    /// [`sk1ll_core::paths::use_dirs`] instead.
    pub fn enter(&self) -> Entered {
        Entered { _dirs: failpoints::dirs(self.dirs()), _git: failpoints::git_env(crate::git::ENV) }
    }
}

/// See [`Home::enter`].
#[must_use = "the isolation ends when this is dropped"]
#[derive(Debug)]
pub struct Entered {
    _dirs: Guard,
    _git: Guard,
}
