//! A `sk1ll-daemon` process per Machine, driven through its local API.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use sk1ll_daemon::DaemonState;
use sk1ll_daemon::client::{Client, Connection, StatusView};

use crate::server::Server;
use crate::{Harness, Home, InstructionsFormat, MachineToml};

/// The harness every [`DaemonMachine`] has.
pub const HARNESS: &str = "agent";

/// One Machine run by a `sk1ll-daemon` process, everything at the default
/// places under its own [`Home`]: `machine.toml` (with one copy-mode harness
/// [`HARNESS`], its skills in `<home>/agent/skills` and its rules in
/// `<home>/agent/rules`, presets off), the repo, the daemon's state and
/// socket. The daemon logs to `<home>/daemon.log`, which is printed when a
/// test panics and copied into `$SK1LL_TEST_LOG_DIR` when that is set. The
/// daemon is killed when dropped.
pub struct DaemonMachine {
    pub id: String,
    pub home: Home,
    pub client: Client,
    bin: PathBuf,
    child: Option<Child>,
    env: Vec<(String, String)>,
}

impl std::fmt::Debug for DaemonMachine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonMachine").field("id", &self.id).field("home", &self.home).finish_non_exhaustive()
    }
}

impl DaemonMachine {
    /// Machine `id`, whose daemon is the binary `bin`
    /// (`env!("CARGO_BIN_EXE_sk1ll-daemon")`). Not started.
    pub fn new(id: &str, bin: impl Into<PathBuf>) -> Self {
        let home = Home::new();
        let h = home.path();
        let harness = Harness::copy(HARNESS, h.join("agent/skills"))
            .instructions(h.join("agent/rules"), InstructionsFormat::Rules);
        MachineToml::new().id(id).harness(harness).write(&home.config_dir().join("machine.toml"));
        let client = Client::new(home.socket());
        Self { id: id.into(), client, bin: bin.into(), child: None, env: Vec::new(), home }
    }

    /// Sets `key` in the daemon's environment, over the defaults.
    pub fn with_env(mut self, key: &str, value: &str) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// The daemon binary with this Machine's HOME, quick debounce and
    /// reconnects, `RUST_LOG=info` and the environment added.
    pub fn command(&self) -> Command {
        let mut cmd = self.home.command(&self.bin);
        cmd.env("SK1LL_DEBOUNCE", "100ms").env("SK1LL_RECONNECT_MAX", "1s").env("RUST_LOG", "info");
        cmd.envs(self.env.iter().map(|(k, v)| (k, v)));
        cmd
    }

    pub fn machine_toml(&self) -> PathBuf {
        self.home.config_dir().join("machine.toml")
    }

    /// The repo the daemon manages (the default one).
    pub fn repo(&self) -> PathBuf {
        self.home.default_repo()
    }

    /// The harness's skills directory.
    pub fn skills(&self) -> PathBuf {
        self.home.path().join("agent/skills")
    }

    /// The harness's rules directory.
    pub fn rules(&self) -> PathBuf {
        self.home.path().join("agent/rules")
    }

    /// `daemon.json`: written when the Machine joins.
    pub fn state_path(&self) -> PathBuf {
        self.home.config_dir().join("daemon.json")
    }

    /// The stored daemon state; panics if the Machine never joined.
    pub fn state(&self) -> DaemonState {
        DaemonState::load(&self.state_path()).unwrap().expect("joined")
    }

    /// The joined Machine's Assignment snapshot file.
    pub fn snapshot_path(&self) -> PathBuf {
        self.home.config_dir().join("daemon-assignment.json")
    }

    /// The CLI's cache of the Assignment.
    pub fn cache_path(&self) -> PathBuf {
        self.home.cache_dir().join("assignment.json")
    }

    /// What the daemon logged so far.
    pub fn log(&self) -> String {
        std::fs::read_to_string(self.log_path()).unwrap_or_default()
    }

    fn log_path(&self) -> PathBuf {
        self.home.path().join("daemon.log")
    }

    /// Runs `sk1ll-daemon join` against `server` with a fresh code; returns
    /// what it printed.
    pub async fn join(&self, server: &Server) -> String {
        self.join_with(server, &server.join_code().await).await
    }

    /// Runs `sk1ll-daemon join` with `code`, which must succeed.
    pub async fn join_with(&self, server: &Server, code: &str) -> String {
        let mut cmd = self.command();
        let args: [OsString; 6] =
            ["join".into(), server.url().into(), code.into(), "--id".into(), self.id.clone().into(), "--repo".into()];
        cmd.args(args).arg(self.repo());
        let out = tokio::task::spawn_blocking(move || cmd.output().unwrap()).await.unwrap();
        assert!(out.status.success(), "join: {}", String::from_utf8_lossy(&out.stderr));
        crate::text(&out)
    }

    /// Starts the daemon process, without waiting for it.
    pub fn spawn(&mut self) {
        let log = std::fs::OpenOptions::new().create(true).append(true).open(self.log_path()).unwrap();
        self.child = Some(self.command().stdout(Stdio::null()).stderr(log).spawn().unwrap());
    }

    /// Starts the daemon; returns once its local API answers.
    pub async fn start(&mut self) {
        self.spawn();
        crate::eventually(|| async { self.client.status().await.ok().map(drop) }).await;
    }

    /// Starts the daemon; returns once it is connected to its server.
    pub async fn start_connected(&mut self) {
        self.start().await;
        crate::eventually(|| async { (self.status().await.connection == Connection::Connected).then_some(()) }).await;
    }

    /// Starts a daemon that must exit with an error; returns how it exited.
    pub async fn start_fails(&mut self) -> std::process::ExitStatus {
        self.spawn();
        let mut child = self.child.take().unwrap();
        let status = crate::eventually(|| {
            let status = child.try_wait().unwrap();
            async move { status }
        })
        .await;
        assert!(!status.success(), "the daemon was to fail");
        status
    }

    /// Kills the daemon, as a crash or a reboot would.
    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// The daemon's status, which must answer.
    pub async fn status(&self) -> StatusView {
        self.client.status().await.unwrap()
    }

    /// Writes `rel` in the repo; returns its path.
    pub fn write(&self, rel: &str, text: &str) -> PathBuf {
        let path = self.repo().join(rel);
        crate::write(&path, text);
        path
    }
}

impl Drop for DaemonMachine {
    fn drop(&mut self) {
        self.stop();
        let log = self.log_path();
        if let Some(dir) = std::env::var_os("SK1LL_TEST_LOG_DIR") {
            let name = format!("{}-{}.log", std::thread::current().name().unwrap_or("test"), self.id);
            let _ = std::fs::copy(&log, Path::new(&dir).join(name.replace("::", "-")));
        }
        if std::thread::panicking() {
            eprintln!("---- daemon log of {} ----\n{}", self.id, self.log());
        }
    }
}
