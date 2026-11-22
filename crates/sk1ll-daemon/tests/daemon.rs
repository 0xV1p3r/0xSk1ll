//! End-to-end tests: a real server (on its own runtime, so it can be stopped
//! and restarted on the same port) and one `sk1ll-daemon` process per
//! machine, each with its own HOME and XDG dirs and no global git config,
//! driven through the daemon's local API.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use sk1ll_daemon::DaemonState;
use sk1ll_daemon::client::{
    AddImportRequest, Client, CommitRequest, CommitResponse, Connection, Event, ResolveImportRequest, RestoreRequest,
    StatusView,
};
use sk1ll_daemon::state::PullState;
use sk1ll_proto::{
    Assignment, AssignmentChanges, AssignmentSnapshot, DaemonMessage, FleetView, HarnessAssignment, ItemRef, LogEntry,
    MachineStatus, ServerMessage, ToggleChange, ToggleKind,
};
use tokio_tungstenite::tungstenite::Message;

const ADMIN: &str = "admin-secret";
const HARNESS: &str = "agent";

// ---------------------------------------------------------------------------
// Server

struct Server {
    port: u16,
    data: PathBuf,
    _dir: tempfile::TempDir,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    http: reqwest::Client,
}

impl Server {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let mut server = Self { port: 0, data, _dir: dir, stop: None, thread: None, http: reqwest::Client::new() };
        server.launch().await;
        server
    }

    /// Serves on `self.port` (0: any) on a runtime of its own; dropping that
    /// runtime closes every connection, like a crashed server.
    async fn launch(&mut self) {
        let (port_tx, port_rx) = std::sync::mpsc::channel();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let (data, port) = (self.data.clone(), self.port);
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
            rt.block_on(async move {
                let socket = tokio::net::TcpSocket::new_v4().unwrap();
                socket.set_reuseaddr(true).unwrap();
                socket.bind(SocketAddr::from(([127, 0, 0, 1], port))).unwrap();
                let listener = socket.listen(128).unwrap();
                port_tx.send(listener.local_addr().unwrap().port()).unwrap();
                let mut config = sk1ll_server::Config::new(data);
                config.admin_token = Some(ADMIN.into());
                tokio::select! {
                    r = sk1ll_server::serve(config, listener) => r.unwrap(),
                    _ = stop_rx => {}
                }
            });
            rt.shutdown_timeout(Duration::from_secs(2));
        });
        self.port = port_rx.recv().unwrap();
        self.stop = Some(stop_tx);
        self.thread = Some(thread);
        eventually(|| async { self.http.get(format!("{}/healthz", self.url())).send().await.ok().map(drop) }).await;
    }

    fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    async fn join_code(&self) -> String {
        let resp = self.http.post(format!("{}/api/join-codes", self.url())).bearer_auth(ADMIN).send().await.unwrap();
        assert!(resp.status().is_success());
        resp.json::<sk1ll_proto::JoinCode>().await.unwrap().code
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> T {
        let resp = self.http.get(format!("{}/api{path}", self.url())).bearer_auth(ADMIN).send().await.unwrap();
        assert!(resp.status().is_success(), "GET {path}: {}", resp.status());
        resp.json().await.unwrap()
    }

    async fn row(&self, id: &str) -> Option<sk1ll_proto::MachineRow> {
        self.get::<FleetView>("/fleet").await.machines.into_iter().find(|m| m.id == id)
    }

    /// The bare repo's HEAD, read directly.
    fn head(&self) -> Option<String> {
        let out = Command::new("git")
            .arg("--git-dir")
            .arg(self.data.join("repo.git"))
            .args(["rev-parse", "--verify", "--quiet", "HEAD"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// Machines

const CONFIG_DIR: &str = if cfg!(target_os = "macos") { "Library/Application Support" } else { ".config" };
const DATA_DIR: &str = if cfg!(target_os = "macos") { CONFIG_DIR } else { ".local/share" };
const CACHE_DIR: &str = if cfg!(target_os = "macos") { "Library/Caches" } else { ".cache" };
const SOCKET_PATH: &str = if cfg!(target_os = "macos") {
    "Library/Application Support/sk1ll/run/daemon.sock"
} else {
    "run/sk1ll/daemon.sock"
};

struct Machine {
    id: String,
    home: tempfile::TempDir,
    child: Option<Child>,
    client: Client,
    /// Extra environment for the daemon (overrides the defaults).
    env: Vec<(String, String)>,
}

impl Machine {
    fn machine_config(home: &std::path::Path) -> std::path::PathBuf {
        home.join(CONFIG_DIR).join("sk1ll/machine.toml")
    }

    /// A machine with one harness, `agent`, under its temp HOME (copy mode,
    /// rules-format instructions). No presets exist in that HOME.
    fn new(id: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        let cfg = format!(
            "id = \"{id}\"\n[harnesses.{HARNESS}]\nskills = \"{}\"\ninstructions = \"{}\"\n\
             instructions_format = \"rules\"\nmode = \"copy\"\n",
            h.join("agent/skills").display(),
            h.join("agent/rules").display()
        );
        let config = Self::machine_config(h);
        write(config.parent().unwrap(), "machine.toml", &cfg);
        let client = Client::new(h.join(SOCKET_PATH));
        Self { id: id.into(), home, child: None, client, env: Vec::new() }
    }

    fn with_env(mut self, k: &str, v: &str) -> Self {
        self.env.push((k.into(), v.into()));
        self
    }

    fn command(&self) -> Command {
        let h = self.home.path();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sk1ll-daemon"));
        for k in ["SK1LL_DAEMON_SOCKET", "SK1LL_DAEMON_STATE", "SK1LL_MACHINE_CONFIG", "SK1LL_ASSIGNMENT", "SK1LL_REPO"]
        {
            cmd.env_remove(k);
        }
        cmd.env("HOME", h)
            .env("XDG_CONFIG_HOME", h.join(".config"))
            .env("XDG_CACHE_HOME", h.join(".cache"))
            .env("XDG_DATA_HOME", h.join(".local/share"))
            .env("XDG_STATE_HOME", h.join(".local/state"))
            .env("XDG_RUNTIME_DIR", h.join("run"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("SK1LL_DEBOUNCE", "100ms")
            .env("SK1LL_RECONNECT_MAX", "1s")
            .env("RUST_LOG", "info");
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd
    }

    fn repo(&self) -> PathBuf {
        self.home.path().join(DATA_DIR).join("sk1ll/repo")
    }

    fn skills(&self) -> PathBuf {
        self.home.path().join("agent/skills")
    }

    fn state_path(&self) -> PathBuf {
        self.home.path().join(CONFIG_DIR).join("sk1ll/daemon.json")
    }

    fn state(&self) -> DaemonState {
        DaemonState::load(&self.state_path()).unwrap().expect("joined")
    }

    fn snapshot_path(&self) -> PathBuf {
        self.home.path().join(CONFIG_DIR).join("sk1ll/daemon-assignment.json")
    }

    fn cache_path(&self) -> PathBuf {
        self.home.path().join(CACHE_DIR).join("sk1ll/assignment.json")
    }

    fn snapshot(&self) -> Option<AssignmentSnapshot> {
        let text = std::fs::read_to_string(self.snapshot_path()).ok()?;
        Some(serde_json::from_str(&text).unwrap())
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.home.path().join("daemon.log")).unwrap_or_default()
    }

    /// Joins with a fresh code; returns what the command printed.
    async fn join(&self, server: &Server) -> String {
        let code = server.join_code().await;
        let mut cmd = self.command();
        cmd.args(["join", &server.url(), &code, "--id", &self.id, "--repo"]).arg(self.repo());
        let out = tokio::task::spawn_blocking(move || cmd.output().unwrap()).await.unwrap();
        assert!(out.status.success(), "join: {}", String::from_utf8_lossy(&out.stderr));
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
    }

    fn spawn(&mut self) {
        let log =
            std::fs::OpenOptions::new().create(true).append(true).open(self.home.path().join("daemon.log")).unwrap();
        let child = self.command().stdout(Stdio::null()).stderr(log).spawn().unwrap();
        self.child = Some(child);
    }

    async fn start(&mut self) {
        self.spawn();
        eventually(|| async { self.client.status().await.ok().map(drop) }).await;
    }

    /// For real servers: API readiness alone does not mean a commit can push.
    async fn start_connected(&mut self) {
        self.start().await;
        eventually(|| async { (self.status().await.connection == Connection::Connected).then_some(()) }).await;
    }

    /// Kills the daemon (like a crash or a reboot).
    fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// Starts a daemon that is expected to exit with an error.
    async fn start_fails(&mut self) -> std::process::ExitStatus {
        self.spawn();
        let mut child = self.child.take().unwrap();
        let status = eventually(|| {
            let s = child.try_wait().unwrap();
            async move { s }
        })
        .await;
        assert!(!status.success());
        status
    }

    async fn status(&self) -> StatusView {
        self.client.status().await.unwrap()
    }

    /// Writes `rel` in the repo. A new personal skill gets the explicit
    /// Item ID [`skill_id`] names, so tests can toggle it before a refresh
    /// would write it one; IDs are never derived from names.
    fn write(&self, rel: &str, body: &str) {
        let path = self.repo().join(rel);
        write(path.parent().unwrap(), path.file_name().unwrap().to_str().unwrap(), body);
        if let Some(name) = rel.strip_prefix("personal/").and_then(|p| p.strip_suffix("/SKILL.md")) {
            let sidecar = path.parent().unwrap().join(".sk1ll-id");
            if sidecar.symlink_metadata().is_err() {
                std::fs::write(sidecar, format!("{}\n", skill_id(name))).unwrap();
            }
        }
    }

    fn harness_skill(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.skills().join(name).join("SKILL.md")).ok()
    }

    /// Commits every draft item, once the Draft shows an edit made just
    /// before: while an engine operation runs, readers answer from the
    /// Snapshot of the last reconcile.
    async fn commit_all(&self) -> CommitResponse {
        let draft = eventually(|| async { Some(self.client.draft().await.unwrap()).filter(|d| !d.is_empty()) }).await;
        let items = draft.into_iter().map(|d| d.item.to_string()).collect();
        self.client.commit(&CommitRequest { items, ..Default::default() }).await.unwrap()
    }

    async fn commit(&self, items: &[&str]) -> CommitResponse {
        let items = items.iter().map(|s| s.to_string()).collect();
        self.client.commit(&CommitRequest { items, ..Default::default() }).await.unwrap()
    }

    fn head(&self) -> String {
        git(&self.repo(), &["rev-parse", "HEAD"])
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        let log_path = self.home.path().join("daemon.log");
        if let Some(dir) = std::env::var_os("SK1LL_TEST_LOG_DIR") {
            let name = format!("{}-{}.log", std::thread::current().name().unwrap_or("test"), self.id);
            let _ = std::fs::copy(&log_path, Path::new(&dir).join(name.replace("::", "-")));
        }
        if std::thread::panicking() {
            let log = std::fs::read_to_string(&log_path).unwrap_or_default();
            eprintln!("---- daemon log of {} ----\n{log}", self.id);
        }
    }
}

fn assert_indexed_copy(dir: &Path, name: &str) {
    let bytes = std::fs::read(dir.join(sk1ll_core::sync::INDEX_PATH)).unwrap();
    let index: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let copy = dir.join(name);
    let token = std::fs::read_to_string(copy.join(sk1ll_core::sync::PLACEMENT_MARKER)).unwrap();
    let entry = &index["entries"][name];
    assert_eq!(entry["kind"], "copy");
    assert_eq!(entry["hash"], sk1ll_core::hash::hash_dir(&copy).unwrap());
    assert_eq!(entry["token"], token);
    assert!(entry["payload"].as_str().is_some_and(|hash| hash.starts_with("sha256:")));
    assert_eq!(index["pending"], serde_json::json!({}));
}

fn write(dir: &Path, file: &str, body: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(file), body).unwrap();
}

/// Runs git without the user's global/system config.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "commit.gpgsign=false"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A hand-driven server endpoint: the test decides what each connection gets.
struct FakeServer {
    listener: tokio::net::TcpListener,
}

type FakeWs = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

impl FakeServer {
    async fn new() -> Self {
        Self { listener: tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap() }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.listener.local_addr().unwrap().port())
    }

    async fn tcp(&self) -> tokio::net::TcpStream {
        tokio::time::timeout(Duration::from_secs(10), self.listener.accept()).await.expect("a connection").unwrap().0
    }

    /// Accepts a WebSocket and reads its `hello`; returns it and the `Authorization` header.
    async fn accept(&self) -> (FakeWs, Option<String>) {
        let tcp = self.tcp().await;
        let (auth_tx, auth_rx) = std::sync::mpsc::channel();
        let mut ws = tokio_tungstenite::accept_hdr_async(tcp, RecordAuth(auth_tx)).await.unwrap();
        let hello = next_message(&mut ws).await;
        assert!(matches!(hello, DaemonMessage::Hello { .. }), "{hello:?}");
        (ws, auth_rx.recv().unwrap())
    }

    /// Answers the next connection with HTTP 401, as a proxy might.
    async fn refuse_401(&self) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut tcp = self.tcp().await;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = tcp.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        tcp.write_all(b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await.unwrap();
        let _ = tcp.shutdown().await;
    }
}

async fn next_message(ws: &mut FakeWs) -> DaemonMessage {
    loop {
        match tokio::time::timeout(Duration::from_secs(10), ws.next()).await.expect("a message") {
            Some(Ok(Message::Text(t))) => return serde_json::from_str(&t).unwrap(),
            Some(Ok(_)) => {}
            other => panic!("connection ended: {other:?}"),
        }
    }
}

/// Assignment of `agent` with exactly `on` enabled among x, y and z.
fn snapshot(revision: u64, on: &[&str]) -> AssignmentSnapshot {
    let skills = ["x", "y", "z"].iter().map(|s| (skill_id(s), on.contains(s))).collect();
    let h = HarnessAssignment { new_skills_default: false, new_rules_default: false, skills, ..Default::default() };
    let assignment = Assignment { harnesses: [(HARNESS.to_string(), h)].into() };
    AssignmentSnapshot { revision, assignment, registration: None }
}

/// A machine whose repo has skills x, y, z (pushed to a local bare origin)
/// and whose server is `server_url`, with `snap` stored as its assignment.
fn solo(id: &str, server_url: &str, snap: Option<AssignmentSnapshot>) -> Machine {
    let m = Machine::new(id);
    let repo = m.repo();
    let origin = m.home.path().join("origin.git");
    sk1ll_core::Repo::init(&repo).unwrap();
    for s in ["x", "y", "z"] {
        m.write(&format!("personal/{s}/SKILL.md"), &format!("{s}\n"));
    }
    git(&repo, &["checkout", "--quiet", "-B", "main"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "--quiet", "-m", "seed"]);
    git(m.home.path(), &["init", "--quiet", "--bare", origin.to_str().unwrap()]);
    git(&repo, &["remote", "add", "origin", origin.to_str().unwrap()]);
    git(&repo, &["push", "--quiet", "-u", "origin", "main"]);
    let state = DaemonState {
        machine_id: id.into(),
        server_url: server_url.into(),
        token: "tok".into(),
        repo,
        revoked: false,
        record: Default::default(),
    };
    state.save(&m.state_path()).unwrap();
    if let Some(snap) = snap {
        sk1ll_daemon::state::save_snapshot(&m.snapshot_path(), &snap, Some(&state.registration_key())).unwrap();
    }
    m
}

/// A fixed, explicit Item ID for the skill `name` in these tests. Items are
/// never given derived IDs: a repo item that must have it is written it.
fn skill_id(name: &str) -> sk1ll_core::ItemId {
    sk1ll_core::ItemId::from_text(&format!("id-{name}")).unwrap()
}

fn toggle(name: &str, enabled: bool) -> ToggleChange {
    ToggleChange {
        harness: HARNESS.into(),
        kind: ToggleKind::Skill,
        item_id: Some(skill_id(name)),
        name: Some(name.into()),
        enabled: Some(enabled),
    }
}

async fn assign(via: &Machine, machine: &str, changes: Vec<ToggleChange>) {
    via.client.set_assignment(machine, &AssignmentChanges { changes, ..Default::default() }).await.unwrap();
}

/// Polls `f` until it returns `Some` (up to 30 s).
async fn eventually<T, F, Fut>(mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    for _ in 0..300 {
        if let Some(v) = f().await {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("condition never held");
}

/// `alpha` joins the fresh server and commits the new repo with skill `x`;
/// then `beta` joins and clones it. Returns both and the baseline.
async fn fleet_of_two(server: &Server) -> (Machine, Machine, String) {
    let mut a = Machine::new("alpha");
    a.join(server).await;
    a.start_connected().await;
    a.write("personal/x/SKILL.md", "x v1\n");
    let p = a.commit_all().await;
    assert!(p.pushed, "{p:?}");
    assert_eq!(server.head().as_deref(), Some(p.sha.as_str()));

    let mut b = Machine::new("beta");
    b.join(server).await;
    b.start_connected().await;
    // Removing a machine before the server has registered its session is an
    // offline removal: a later 401 deliberately does not revoke the daemon.
    eventually(|| async {
        let row = server.row("beta").await?;
        (row.connected && row.report?.baseline.as_deref() == Some(p.sha.as_str())).then_some(())
    })
    .await;
    (a, b, p.sha)
}

// ---------------------------------------------------------------------------
// Tests

#[tokio::test(flavor = "multi_thread")]
async fn harness_assignment_passes_through_to_the_fleet_and_reaches_sync() {
    let server = Server::start().await;
    let (_a, b, _) = fleet_of_two(&server).await;
    let on = b.client.set_harness_assignment(HARNESS, true).await.unwrap();
    assert!(on.assignment.skill_enabled(HARNESS, &skill_id("x")));
    eventually(|| async { b.harness_skill("x").is_some().then_some(()) }).await;
    let off = b.client.set_harness_assignment(HARNESS, false).await.unwrap();
    assert!(!off.assignment.skill_enabled(HARNESS, &skill_id("x")));
    assert!(off.entries.iter().all(|entry| entry.machine == "beta"));
    eventually(|| async { b.harness_skill("x").is_none().then_some(()) }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn standalone_harness_assignment_reaches_items_with_spaces() {
    let mut m = Machine::new("solo");
    sk1ll_core::Repo::init(&m.repo()).unwrap();
    m.write("personal/pdf/SKILL.md", "valid");
    m.write("personal/old name/SKILL.md", "legacy item");
    m.start().await;

    let result = m.client.set_harness_assignment(HARNESS, false).await.unwrap();
    assert!(result.errors.is_empty());
    assert!(!result.assignment.skill_enabled(HARNESS, &skill_id("pdf")));
    assert!(!result.assignment.skill_enabled(HARNESS, &skill_id("old name")));
}

#[tokio::test(flavor = "multi_thread")]
async fn join_commit_push_and_nudge_another_machine() {
    let server = Server::start().await;
    let (a, b, _) = fleet_of_two(&server).await;

    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&a.state_path()), 0o600, "the token file is private");
    assert_eq!(mode(a.client.socket()), 0o600, "the API socket is private");
    let state = b.state();
    assert_eq!((state.machine_id.as_str(), state.server_url.as_str()), ("beta", server.url().as_str()));
    assert!(b.repo().join("personal/x/SKILL.md").is_file(), "beta cloned the baseline");

    let mut events = b.client.events().await.unwrap();
    a.write("personal/y/SKILL.md", "y v1\n");
    let p = a.commit(&["y"]).await;
    assert!(p.pushed);
    assert_eq!(p.message, "commit y from alpha");
    assert_eq!(git(&a.repo(), &["log", "-1", "--format=%an <%ae>"]), "alpha <sk1ll@alpha>");

    // The server nudges beta, which pulls.
    eventually(|| async { (b.status().await.report.baseline.as_deref() == Some(p.sha.as_str())).then_some(()) }).await;
    assert!(b.repo().join("personal/y/SKILL.md").is_file());
    assert_eq!(b.harness_skill("y"), None, "a new machine starts with everything off");

    assign(&a, "beta", vec![toggle("y", true)]).await;
    eventually(|| async { (b.harness_skill("y").as_deref() == Some("y v1\n")).then_some(()) }).await;
    assert_eq!(b.harness_skill("x"), None);
    let got = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.next().await {
                Some(Ok(Event::Assignment { revision })) => return revision,
                Some(Ok(_)) => {}
                other => panic!("event stream ended: {:?}", other.map(|r| r.map_err(|e| e.to_string()))),
            }
        }
    })
    .await
    .expect("an assignment event");
    assert_eq!(got, b.status().await.assignment_revision);

    eventually(|| async { (server.row("beta").await?.status == MachineStatus::InSync).then_some(()) }).await;
    let fleet = a.client.fleet().await.unwrap();
    assert_eq!(fleet.baseline.as_deref(), Some(p.sha.as_str()));
    assert_eq!(fleet.machines.len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn join_codes_pass_their_scope_and_expiry_through() {
    let server = Server::start().await;
    let mut a = Machine::new("alpha");
    a.join(&server).await;
    a.start_connected().await;
    let code = a.client.join_code(sk1ll_proto::Scope::SelfOnly).await.unwrap();
    assert_eq!(code.scope, sk1ll_proto::Scope::SelfOnly);
    assert!(code.expires_at > 0);
    let fleet = a.client.fleet().await.unwrap();
    assert_eq!(fleet.machines[0].scope, sk1ll_proto::Scope::Fleet);
}

#[tokio::test(flavor = "multi_thread")]
async fn assignment_change_reaches_another_harness_without_git() {
    let server = Server::start().await;
    let (a, b, base) = fleet_of_two(&server).await;
    let reflog = git(&b.repo(), &["reflog"]);

    assign(&a, "beta", vec![toggle("x", true)]).await;
    eventually(|| async { (b.harness_skill("x").as_deref() == Some("x v1\n")).then_some(()) }).await;
    assign(&a, "beta", vec![toggle("x", false)]).await;
    eventually(|| async { (!b.skills().join("x").exists()).then_some(()) }).await;

    let snapshot = a.client.assignment("beta").await.unwrap();
    assert!(snapshot.revision > 0);
    assert_eq!(snapshot, server.get::<AssignmentSnapshot>("/machines/beta/assignment").await);
    assert!(!snapshot.assignment.skill_enabled(HARNESS, &skill_id("x")));
    assert_eq!(b.head(), base);
    assert_eq!(git(&b.repo(), &["reflog"]), reflog, "no git operation moved anything");
    let cached = Assignment::load(&b.home.path().join(CACHE_DIR).join("sk1ll/assignment.json")).unwrap();
    assert_eq!(cached.harness(HARNESS).skills.get(&skill_id("x")), Some(&false), "the assignment is cached");
}

#[tokio::test(flavor = "multi_thread")]
async fn offline_commits_are_queued_and_pushed_on_reconnect() {
    let mut server = Server::start().await;
    let mut a = Machine::new("alpha");
    a.join(&server).await;
    a.start_connected().await;
    a.write("personal/v/SKILL.md", "v\n");
    let first = a.commit_all().await;
    assert!(first.pushed);

    server.stop();
    eventually(|| async { (a.status().await.connection == Connection::Offline).then_some(()) }).await;
    let err = a.client.fleet().await.unwrap_err();
    assert_eq!(err.status, Some(503), "{err}");

    a.write("personal/z/SKILL.md", "z\n");
    let p = a.commit(&["z"]).await;
    assert!(!p.pushed, "nothing to push to while offline");
    let s = a.status().await;
    assert_eq!(s.report.unpushed, [ItemRef::Skill("z".into())]);
    assert_eq!(s.headline, MachineStatus::Offline);
    assert_eq!(server.head().as_deref(), Some(first.sha.as_str()));

    server.launch().await;
    eventually(|| async { (server.head().as_deref() == Some(p.sha.as_str())).then_some(()) }).await;
    eventually(|| async {
        let s = a.status().await;
        (s.connection == Connection::Connected && s.report.unpushed.is_empty()).then_some(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn blocked_pull_reports_clashes_and_keeps_files() {
    let mut server = Server::start().await;
    let (a, mut b, base) = fleet_of_two(&server).await;

    b.write("personal/x/SKILL.md", "x by beta\n");
    eventually(|| async {
        server.row("beta").await?.report?.drafts.contains(&ItemRef::Skill("x".into())).then_some(())
    })
    .await;
    a.write("personal/x/SKILL.md", "x by alpha\n");
    let p = a.commit(&["x"]).await;
    assert!(p.pushed);

    let s = eventually(|| async {
        let s = b.status().await;
        s.report.blocked.then_some(s)
    })
    .await;
    assert_eq!(s.report.clashes, [ItemRef::Skill("x".into())]);
    assert_eq!(s.report.baseline.as_deref(), Some(base.as_str()), "stays on the old baseline");
    assert_eq!(s.headline, MachineStatus::Blocked);
    assert_eq!(std::fs::read_to_string(b.repo().join("personal/x/SKILL.md")).unwrap(), "x by beta\n");
    assert_eq!(b.head(), base);
    eventually(|| async { (server.row("beta").await?.status == MachineStatus::Blocked).then_some(()) }).await;

    // The state file remembers the block, so a daemon restarted while the
    // server is away still reports it, before it could pull again.
    assert!(matches!(&b.state().record.pull, PullState::Blocked { items, .. } if items == &s.report.clashes));
    assert_eq!(b.state().record.server_baseline.as_deref(), Some(p.sha.as_str()));
    b.stop();
    server.stop();
    b.start().await;
    let s = b.status().await;
    assert_ne!(s.connection, Connection::Connected);
    assert!(s.report.blocked && s.report.behind, "{s:?}");
    assert_eq!(s.report.clashes, [ItemRef::Skill("x".into())]);
    assert_eq!(s.server_baseline.as_deref(), Some(p.sha.as_str()));
    server.launch().await;
    eventually(|| async { (b.status().await.connection == Connection::Connected).then_some(()) }).await;

    // Restoring the clashing item is a draft change: the pull is retried.
    b.client.restore(&RestoreRequest { items: vec!["x".into()], all: false }).await.unwrap();
    eventually(|| async {
        let s = b.status().await;
        (!s.report.blocked && s.report.baseline.as_deref() == Some(p.sha.as_str())).then_some(())
    })
    .await;
    assert_eq!(std::fs::read_to_string(b.repo().join("personal/x/SKILL.md")).unwrap(), "x by alpha\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn revoked_machine_stops_managing_and_keeps_files() {
    let server = Server::start().await;
    let (a, b, _) = fleet_of_two(&server).await;
    assign(&a, "beta", vec![toggle("x", true)]).await;
    eventually(|| async { b.harness_skill("x").map(drop) }).await;

    a.client.remove_machine("beta").await.unwrap();
    let s = eventually(|| async {
        let s = b.status().await;
        s.revoked.then_some(s)
    })
    .await;
    assert_eq!(s.connection, Connection::Revoked);
    assert!(b.state().revoked, "recorded locally");
    assert_eq!(b.harness_skill("x").as_deref(), Some("x v1\n"), "harness files stay");

    // Not managed any more: removing the skill from the repo leaves the harness alone.
    std::fs::remove_dir_all(b.repo().join("personal/x")).unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(b.harness_skill("x").as_deref(), Some("x v1\n"));
    let err = b.client.commit(&CommitRequest { items: vec!["x".into()], ..Default::default() }).await.unwrap_err();
    assert_eq!(err.status, Some(409), "{err}");
    assert_eq!(b.client.fleet().await.unwrap_err().status, Some(503));
    assert!(server.row("beta").await.is_none());
}

/// Records the `Authorization` header of a WebSocket handshake.
struct RecordAuth(std::sync::mpsc::Sender<Option<String>>);

impl tokio_tungstenite::tungstenite::handshake::server::Callback for RecordAuth {
    fn on_request(
        self,
        req: &tokio_tungstenite::tungstenite::handshake::server::Request,
        resp: tokio_tungstenite::tungstenite::handshake::server::Response,
    ) -> Result<
        tokio_tungstenite::tungstenite::handshake::server::Response,
        tokio_tungstenite::tungstenite::handshake::server::ErrorResponse,
    > {
        let auth = req.headers().get("authorization").map(|v| v.to_str().unwrap().to_string());
        self.0.send(auth).unwrap();
        Ok(resp)
    }
}

/// A fake server sends assignments out of order; an older revision is ignored.
#[tokio::test(flavor = "multi_thread")]
async fn stale_assignment_revision_is_ignored() {
    let server = FakeServer::new().await;
    let mut m = solo("solo", &server.url(), Some(snapshot(1, &[])));
    m.start().await;

    let (ws, auth) = server.accept().await;
    assert_eq!(auth.as_deref(), Some("Bearer tok"));
    // Keep reading what the daemon sends.
    let (mut tx, mut rx) = ws.split();
    tokio::spawn(async move { while rx.next().await.is_some() {} });
    let send = |m: ServerMessage| Message::text(serde_json::to_string(&m).unwrap());

    tx.send(send(ServerMessage::Assignment(snapshot(5, &["x"])))).await.unwrap();
    eventually(|| async { m.harness_skill("x").map(drop) }).await;

    tx.send(send(ServerMessage::Assignment(snapshot(3, &["y"])))).await.unwrap();
    // The imports event is an ordered inbox barrier. A nudge's baseline can
    // be replaced by a pull before a status poll sees it.
    let mut events = m.client.events().await.unwrap();
    tx.send(send(ServerMessage::ImportsChanged { updates: Vec::new() })).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.next().await {
                Some(Ok(Event::Imports { .. })) => break,
                Some(Ok(_)) => {}
                other => panic!("event stream ended: {other:?}"),
            }
        }
    })
    .await
    .expect("the inbox barrier");
    assert_eq!(m.status().await.assignment_revision, 5);
    assert!(m.harness_skill("x").is_some() && m.harness_skill("y").is_none());
    assert_eq!(m.snapshot().unwrap(), snapshot(5, &["x"]), "stored with its revision");

    tx.send(send(ServerMessage::Assignment(snapshot(6, &["z"])))).await.unwrap();
    eventually(|| async { (m.harness_skill("z").is_some() && m.harness_skill("x").is_none()).then_some(()) }).await;
    assert_eq!(m.status().await.assignment_revision, 6);
}

// ---------------------------------------------------------------------------
// Review findings

/// #1: the watcher is live from startup, so a repo edit reaches the harness
/// with no other trigger (no API call, no server message), even when the
/// harness's skills dir only appears later.
#[tokio::test(flavor = "multi_thread")]
async fn repo_edits_reach_the_harness_without_another_trigger() {
    let server = Server::start().await;
    let (a, b, _) = fleet_of_two(&server).await;
    assign(&a, "beta", vec![toggle("x", true)]).await;
    eventually(|| async { (b.harness_skill("x").as_deref() == Some("x v1\n")).then_some(()) }).await;

    b.write("personal/x/SKILL.md", "x edited on beta\n");
    eventually(|| async { (b.harness_skill("x").as_deref() == Some("x edited on beta\n")).then_some(()) }).await;
}

/// API readiness must include watching local edits, even if the first server
/// handshake is still waiting for a response and cannot trigger a refresh.
#[tokio::test(flavor = "multi_thread")]
async fn repo_edits_before_the_first_server_connection_reach_the_harness() {
    let server = FakeServer::new().await;
    let mut m = solo("solo", &server.url(), Some(snapshot(1, &["x"])));
    m.start().await;
    m.write("personal/x/SKILL.md", "edited before connecting\n");
    eventually(|| async { (m.harness_skill("x").as_deref() == Some("edited before connecting\n")).then_some(()) })
        .await;
    assert_ne!(m.status().await.connection, Connection::Connected);
}

/// #10: edits arriving faster than the debounce still sync within the
/// maximum debounce.
#[tokio::test(flavor = "multi_thread")]
async fn a_stream_of_edits_cannot_postpone_the_sync() {
    let server = FakeServer::new().await;
    let mut m = solo("solo", &server.url(), Some(snapshot(1, &["x"])))
        .with_env("SK1LL_DEBOUNCE", "300ms")
        .with_env("SK1LL_DEBOUNCE_MAX", "1s");
    m.start().await;
    eventually(|| async { (m.harness_skill("x").as_deref() == Some("x\n")).then_some(()) }).await;

    let started = std::time::Instant::now();
    for i in 0.. {
        m.write("personal/x/SKILL.md", &format!("x {i}\n"));
        if m.harness_skill("x").as_deref() != Some("x\n") {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(8), "never synced while edits kept coming");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// #2: a 401 (here from something in front of the server) is a recoverable
/// state: the machine keeps being managed, retries, and is never revoked.
#[tokio::test(flavor = "multi_thread")]
async fn a_401_is_not_a_revocation() {
    let server = FakeServer::new().await;
    let mut m = solo("solo", &server.url(), Some(snapshot(1, &["x"])));
    m.start().await;

    server.refuse_401().await;
    let s = eventually(|| async {
        let s = m.status().await;
        (s.connection == Connection::AuthRejected).then_some(s)
    })
    .await;
    assert!(!s.revoked && s.auth_rejected_since.is_some(), "{s:?}");
    assert!(s.last_error.as_deref().unwrap_or_default().contains("401"), "{s:?}");
    assert!(!m.state().revoked, "not stored as revoked");

    // Still managing the harnesses.
    m.write("personal/x/SKILL.md", "x while rejected\n");
    eventually(|| async { (m.harness_skill("x").as_deref() == Some("x while rejected\n")).then_some(()) }).await;

    // Retries, and recovers once the server accepts the token.
    server.refuse_401().await;
    let (_ws, _) = server.accept().await;
    let s = eventually(|| async {
        let s = m.status().await;
        (s.connection == Connection::Connected).then_some(s)
    })
    .await;
    assert_eq!(s.auth_rejected_since, None);
    assert!(!m.state().revoked);
}

/// #3: the token is never in a remote URL, `.git/config`, or any output;
/// push, clone and pull work with it supplied through the environment, and
/// a URL with credentials from an older version is cleaned at startup.
#[tokio::test(flavor = "multi_thread")]
async fn the_token_stays_out_of_git_urls_and_logs() {
    let server = Server::start().await;
    let mut a = Machine::new("alpha");
    let a_join = a.join(&server).await;
    a.start_connected().await;
    a.write("personal/x/SKILL.md", "x\n");
    assert!(a.commit_all().await.pushed, "push works");

    let mut b = Machine::new("beta");
    let b_join = b.join(&server).await;
    b.start().await;
    assert!(b.repo().join("personal/x/SKILL.md").is_file(), "clone works");
    a.write("personal/y/SKILL.md", "y\n");
    let p = a.commit(&["y"]).await;
    eventually(|| async { (b.status().await.report.baseline.as_deref() == Some(p.sha.as_str())).then_some(()) }).await;
    assert!(b.repo().join("personal/y/SKILL.md").is_file(), "pull works");

    for (m, out) in [(&a, &a_join), (&b, &b_join)] {
        let token = m.state().token;
        let remotes = git(&m.repo(), &["remote", "-v"]);
        assert!(!remotes.contains(&token) && !remotes.contains('@'), "{remotes}");
        let config = std::fs::read_to_string(m.repo().join(".git/config")).unwrap();
        assert!(!config.contains(&token), "{config}");
        assert!(!out.contains(&token), "join output: {out}");
        assert!(!m.log().contains(&token), "daemon log of {}", m.id);
    }

    // An older version put the token in origin's URL: startup removes it.
    b.stop();
    let token = b.state().token;
    let old = format!("http://beta:{token}@127.0.0.1:{}/git/repo.git", server.port);
    git(&b.repo(), &["remote", "set-url", "origin", &old]);
    b.start().await;
    assert_eq!(git(&b.repo(), &["remote", "get-url", "origin"]), format!("{}/git/repo.git", server.url()));
    eventually(|| async { (b.status().await.connection == Connection::Connected).then_some(()) }).await;
    b.write("personal/z/SKILL.md", "z\n");
    assert!(b.commit(&["z"]).await.pushed, "push works after the migration");
    assert!(!b.log().contains(&token));
}

/// #4: the assignment is stored with its revision in the state dir, so
/// losing the cache can't enable anything; without any stored snapshot the
/// harnesses wait for the server's, which is taken even at the same revision.
#[tokio::test(flavor = "multi_thread")]
async fn losing_the_assignment_cache_keeps_disabled_skills_off() {
    let mut server = Server::start().await;
    let (a, mut b, _) = fleet_of_two(&server).await;
    assert_eq!(b.harness_skill("x"), None, "a new machine starts with everything off");
    let revision = b.status().await.assignment_revision;
    assert_eq!(b.snapshot().unwrap().revision, revision);

    b.stop();
    std::fs::remove_dir_all(b.home.path().join(CACHE_DIR)).unwrap();
    b.start().await;
    eventually(|| async { (b.status().await.connection == Connection::Connected).then_some(()) }).await;
    assert_eq!(b.harness_skill("x"), None, "the disabled skill stays off");
    assert!(b.cache_path().is_file(), "the cache is mirrored again");

    b.stop();
    server.stop();
    std::fs::remove_file(b.snapshot_path()).unwrap();
    std::fs::remove_dir_all(b.home.path().join(CACHE_DIR)).unwrap();
    b.start().await;
    let s = b.status().await;
    assert!(s.awaiting_assignment, "{s:?}");
    assert_eq!(b.harness_skill("x"), None, "nothing is managed without an assignment");

    server.launch().await;
    let s = eventually(|| async {
        let s = b.status().await;
        (s.connection == Connection::Connected && !s.awaiting_assignment).then_some(s)
    })
    .await;
    assert_eq!(s.assignment_revision, revision, "the same revision is taken when none is stored");
    assert_eq!(b.harness_skill("x"), None);
    assign(&a, "beta", vec![toggle("x", true)]).await;
    eventually(|| async { b.harness_skill("x").map(drop) }).await;
}

/// #5: the daemon holds core's engine lock while it runs, and refuses to
/// start while another process holds it.
#[tokio::test(flavor = "multi_thread")]
async fn the_daemon_holds_the_engine_lock() {
    let server = Server::start().await;
    let mut a = Machine::new("alpha");
    a.join(&server).await;
    a.start().await;
    assert!(sk1ll_core::EngineLock::try_acquire(&a.repo()).unwrap().is_none(), "held by the running daemon");

    a.stop();
    let held = sk1ll_core::EngineLock::acquire(&a.repo()).unwrap();
    a.start_fails().await;
    assert!(a.log().contains("another sk1ll process"), "{}", a.log());
    assert!(!a.client.socket().exists(), "its socket is gone");

    drop(held);
    a.start().await;
    assert!(sk1ll_core::EngineLock::try_acquire(&a.repo()).unwrap().is_none());
}

/// #7: on reconnecting, the first status reflects the local state now, not
/// a report cached from before the disconnect.
#[tokio::test(flavor = "multi_thread")]
async fn reconnecting_reports_fresh_local_state_first() {
    let server = FakeServer::new().await;
    // Neither the watcher nor the reconciliation refreshes during the test.
    let mut m = solo("solo", &server.url(), Some(snapshot(1, &[])))
        .with_env("SK1LL_DEBOUNCE", "60s")
        .with_env("SK1LL_DEBOUNCE_MAX", "60s")
        .with_env("SK1LL_RECONCILE_INTERVAL", "1h");
    m.start().await;
    let (mut ws, _) = server.accept().await;
    let DaemonMessage::Status(first) = next_message(&mut ws).await else { panic!("expected a status") };
    assert!(first.drafts.is_empty(), "{first:?}");
    // Let the catch-up pull after connecting finish.
    tokio::time::sleep(Duration::from_millis(500)).await;
    drop(ws);

    m.write("personal/new/SKILL.md", "new\n");
    let (mut ws, _) = server.accept().await;
    let DaemonMessage::Status(report) = next_message(&mut ws).await else { panic!("status comes first") };
    assert_eq!(report.drafts, [ItemRef::Skill("new".into())]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnecting_does_not_send_status_until_refresh_succeeds() {
    let server = FakeServer::new().await;
    let mut m = solo("solo", &server.url(), Some(snapshot(1, &[]))).with_env("SK1LL_RECONCILE_INTERVAL", "1h");
    m.write("personal/w/SKILL.md", "draft");
    let config = Machine::machine_config(m.home.path());
    let good = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, "[broken").unwrap();
    m.start().await;
    let (mut ws, _) = server.accept().await;
    if let Ok(Some(Ok(Message::Text(text)))) = tokio::time::timeout(Duration::from_millis(700), ws.next()).await {
        panic!("a failed refresh sent status: {text}");
    }
    let status = m.status().await;
    assert!(status.last_error.as_deref().unwrap_or_default().contains("machine.toml"), "{status:?}");

    std::fs::write(config, good).unwrap();
    let (mut ws, _) = server.accept().await;
    let DaemonMessage::Status(report) = next_message(&mut ws).await else { panic!("expected status") };
    assert_eq!(report.seq, 1);
    assert!(report.drafts.contains(&ItemRef::Skill("w".into())));
    assert!(report.items.iter().any(|item| item.id == skill_id("w")));
}

/// Connecting costs no reconcile when nothing changed since the one at
/// startup: not the one before the first status, not the catch-up pull that
/// moved nothing, not the server's nudge for the baseline in use.
#[tokio::test(flavor = "multi_thread")]
async fn connecting_costs_no_reconcile_when_nothing_changed() {
    let server = FakeServer::new().await;
    // Neither the watcher nor the reconciliation refreshes during the test.
    let mut m = solo("solo", &server.url(), Some(snapshot(1, &[])))
        .with_env("SK1LL_DEBOUNCE", "60s")
        .with_env("SK1LL_DEBOUNCE_MAX", "60s")
        .with_env("SK1LL_RECONCILE_INTERVAL", "1h")
        .with_env("RUST_LOG", "info,sk1ll_daemon=debug");
    let baseline = git(&m.repo(), &["rev-parse", "HEAD"]);
    let refreshes = |m: &Machine| m.log().matches("reconciling: ").count();
    // Inputs written moments ago are reconciled every time (the racy window).
    tokio::time::sleep(sk1ll_core::hash::HashCache::RACY_WINDOW).await;
    m.start().await;
    for connection in 0..2 {
        let (mut ws, _) = server.accept().await;
        let DaemonMessage::Status(first) = next_message(&mut ws).await else { panic!("expected a status") };
        assert_eq!(first.baseline.as_deref(), Some(baseline.as_str()));
        let before = refreshes(&m);
        let nudge = ServerMessage::Nudge { baseline: baseline.clone() };
        ws.send(Message::text(serde_json::to_string(&nudge).unwrap())).await.unwrap();
        eventually(|| async {
            let s = m.status().await;
            (s.connection == Connection::Connected && s.server_baseline.as_deref() == Some(baseline.as_str()))
                .then_some(())
        })
        .await;
        // Let the catch-up pull and the nudge finish.
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(refreshes(&m), before, "connection {connection}: nothing changed after the first status");
        assert_eq!(m.status().await.headline, MachineStatus::InSync);
        drop(ws);
        eventually(|| async { (m.status().await.connection == Connection::Offline).then_some(()) }).await;
        assert_eq!(refreshes(&m), 1, "connection {connection}: only the one at startup");
    }
}

/// #9: `join` while the daemon runs goes through it; it comes back
/// connected and managing with the new credentials.
#[tokio::test(flavor = "multi_thread")]
async fn joining_again_while_the_daemon_runs_resumes_management() {
    let server = Server::start().await;
    let (a, b, _) = fleet_of_two(&server).await;
    let old_token = b.state().token;
    a.client.remove_machine("beta").await.unwrap();
    eventually(|| async { b.status().await.revoked.then_some(()) }).await;

    let out = b.join(&server).await;
    assert!(out.contains("restarts"), "went through the daemon: {out}");
    let s = eventually(|| async {
        let s = b.client.status().await.ok()?;
        (s.connection == Connection::Connected).then_some(s)
    })
    .await;
    assert!(!s.revoked);
    let state = b.state();
    assert!(!state.revoked && state.token != old_token);
    eventually(|| async { server.row("beta").await.filter(|r| r.connected).map(drop) }).await;

    assign(&a, "beta", vec![toggle("x", true)]).await;
    eventually(|| async { b.harness_skill("x").map(drop) }).await;
    b.write("personal/q/SKILL.md", "q\n");
    assert!(b.commit(&["q"]).await.pushed, "git works with the new token");
    assert!(!b.log().contains(&state.token));
}

#[tokio::test(flavor = "multi_thread")]
async fn replacing_a_watched_skills_directory_rearms_the_watch() {
    let server = FakeServer::new().await;
    let mut m = solo("solo", &server.url(), Some(snapshot(1, &["x"]))).with_env("SK1LL_RECONCILE_INTERVAL", "1h");
    m.start().await;
    eventually(|| async { (m.harness_skill("x").as_deref() == Some("x\n")).then_some(()) }).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    std::fs::rename(m.skills(), m.skills().with_file_name("old-skills")).unwrap();
    std::fs::create_dir(m.skills()).unwrap();
    eventually(|| async { (m.harness_skill("x").as_deref() == Some("x\n")).then_some(()) }).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    std::fs::write(m.skills().join("x/SKILL.md"), "edited in the replacement directory\n").unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        eventually(|| async {
            m.status().await.report.conflicts.iter().any(|c| c.item == ItemRef::Skill("x".into())).then_some(())
        }),
    )
    .await
    .expect("edits in the new inode must be watched without waiting for reconciliation");
}

// ---------------------------------------------------------------------------
// Adopting an existing clone, and git deadlines

/// Runs `sk1ll-daemon join` with `code`; returns whether it succeeded and its stderr.
async fn try_join(m: &Machine, server: &Server, code: &str) -> (bool, String) {
    let mut cmd = m.command();
    cmd.args(["join", &server.url(), code, "--id", &m.id, "--repo"]).arg(m.repo());
    let out = tokio::task::spawn_blocking(move || cmd.output().unwrap()).await.unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned())
}

#[tokio::test(flavor = "multi_thread")]
async fn joining_with_an_independent_clone_is_refused() {
    let server = Server::start().await;
    let m = Machine::new("gamma");
    let repo = m.repo();
    // `git init` on `master` with the server as origin: not a clone of the server.
    git(m.home.path(), &["init", "--quiet", "-b", "master", repo.to_str().unwrap()]);
    m.write("sk1ll.toml", "# gamma's own\n");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "--quiet", "-m", "gamma root"]);
    git(&repo, &["remote", "add", "origin", &format!("{}{}", server.url(), sk1ll_proto::REPO_PATH)]);
    let baseline = server.head();

    // The wrong branch is refused before the join code is spent.
    let code = server.join_code().await;
    let (ok, err) = try_join(&m, &server, &code).await;
    assert!(!ok && err.contains("branch `master`") && err.contains("`main`"), "{err}");
    assert!(server.row("gamma").await.is_none());

    // On `main` but with a history of its own: refused once the server can be fetched.
    git(&repo, &["branch", "-m", "master", "main"]);
    let (ok, err) = try_join(&m, &server, &code).await;
    assert!(!ok && err.contains("shares no history"), "{err}");
    assert_eq!(server.head(), baseline, "nothing was pushed");
    let bare = server.data.join("repo.git");
    let branches = git(&bare, &["for-each-ref", "--format=%(refname)", "refs/heads/"]);
    assert_eq!(branches, "refs/heads/main");
}

/// Processes whose command line mentions `needle`.
#[cfg(target_os = "linux")]
fn processes_mentioning(needle: &str) -> Vec<String> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        if let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) {
            let cmdline = String::from_utf8_lossy(&cmdline).replace('\0', " ");
            if cmdline.contains(needle) {
                found.push(format!("{}: {cmdline}", entry.file_name().to_string_lossy()));
            }
        }
    }
    found
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn the_startup_clone_gives_up_at_the_git_deadline() {
    // A "server" that accepts connections and then never says a word.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for s in listener.incoming() {
            held.push(s);
        }
    });
    let mut m =
        Machine::new("slow").with_env("SK1LL_GIT_TIMEOUT", "100ms").with_env("SK1LL_GIT_LOCAL_TIMEOUT", "100ms");
    let server_url = format!("http://127.0.0.1:{port}");
    DaemonState {
        machine_id: "slow".into(),
        server_url,
        token: "tok".into(),
        repo: m.repo(),
        revoked: false,
        record: Default::default(),
    }
    .save(&m.state_path())
    .unwrap();

    // The local API only answers once startup (and so the clone) is done.
    let start = std::time::Instant::now();
    m.spawn();
    let status = loop {
        if let Ok(s) = m.client.status().await {
            break s;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "startup never finished");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let elapsed = start.elapsed();
    assert!(elapsed < Duration::from_secs(5), "the clone held startup for {elapsed:?}");
    assert!(status.last_error.is_some());
    let log = m.log();
    assert!(log.contains("setting up") && log.contains("timed out after 100ms"), "{log}");
    assert!(!m.repo().join(".git").join("HEAD").exists(), "no half-made clone is left behind");
    // git and its remote helper are gone, not left hanging on the connection.
    let needle = format!("127.0.0.1:{port}/");
    let gone = std::time::Instant::now() + Duration::from_secs(5);
    while !processes_mentioning(&needle).is_empty() && std::time::Instant::now() < gone {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(processes_mentioning(&needle), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn displaced_conflicts_report_to_server_and_resolve() {
    let server = FakeServer::new().await;
    let mut m = solo("solo", &server.url(), Some(snapshot(1, &["x"])));
    let state = m.home.path().join(if cfg!(target_os = "macos") { DATA_DIR } else { "state" });
    m = m.with_env("XDG_STATE_HOME", state.to_str().unwrap());
    m.start().await;
    let (mut ws, _) = server.accept().await;
    let DaemonMessage::Status(_) = next_message(&mut ws).await else { panic!("expected status") };
    eventually(|| async { m.harness_skill("x").map(drop) }).await;
    let mut open = std::fs::OpenOptions::new().append(true).open(m.skills().join("x/SKILL.md")).unwrap();
    m.write("personal/x/SKILL.md", "draft v2\n");
    let entry =
        eventually(|| async { m.client.recovered().await.ok()?.into_iter().find(|entry| entry.name == "x") }).await;
    use std::io::Write;
    open.write_all(b"late harness edit\n").unwrap();
    drop(open);
    let id = entry.id.unwrap();
    let index = state.join("sk1ll/displaced/.index").join(&id);
    std::fs::remove_file(index).unwrap();
    m.client.sync(&Default::default()).await.unwrap();
    eventually(|| async {
        m.status()
            .await
            .report
            .conflicts
            .iter()
            .any(|c| c.harness == HARNESS && c.item == ItemRef::Skill("x".into()))
            .then_some(())
    })
    .await;
    let target = m.client.adopt_displaced(&id).await.unwrap();
    assert_eq!(target, m.repo().join("personal/x"));
    assert_eq!(std::fs::read_to_string(target.join("SKILL.md")).unwrap(), "x\nlate harness edit\n");
    assert!(m.client.conflicts().await.unwrap().is_empty());
    assert!(m.status().await.report.conflicts.is_empty());
}

/// A pasted link is resolved without touching the repo, then one of its
/// skills is added into the draft.
#[tokio::test(flavor = "multi_thread")]
async fn imports_resolve_links_and_add_one_skill() {
    let server = FakeServer::new().await;
    let mut m = solo("solo", &server.url(), Some(snapshot(1, &["x"])));
    let source = m.home.path().join("source");
    write(&source.join("skills/pdf"), "SKILL.md", "---\ndescription: Work with PDFs\n---\n");
    write(&source.join("skills/x"), "SKILL.md", "---\ndescription: Clashes with personal x\n---\n");
    git(&source, &["init", "--quiet", "-b", "main"]);
    git(&source, &["add", "-A"]);
    git(&source, &["commit", "--quiet", "-m", "skills"]);
    let link = source.to_string_lossy().into_owned();
    m.start().await;

    let req = ResolveImportRequest { link: link.clone(), path: String::new(), git_ref: String::new() };
    let r = m.client.resolve_import(&req).await.unwrap();
    assert_eq!((r.source.as_str(), r.git_ref.as_str()), (link.as_str(), "HEAD"));
    let found: Vec<_> = r.candidates.iter().map(|c| (c.path.as_str(), c.default_name.as_str(), c.taken)).collect();
    assert_eq!(found, [("skills/pdf", "pdf", false), ("skills/x", "x", true)]);
    assert_eq!(r.candidates[0].description.as_deref(), Some("Work with PDFs"));
    assert!(m.client.draft().await.unwrap().is_empty(), "resolving leaves the repo alone");

    let add = |path: Option<&str>| AddImportRequest {
        name: String::new(),
        source: link.clone(),
        path: path.map(Into::into),
        git_ref: None,
        commit: None,
    };
    let err = m.client.add_import(&add(None)).await.unwrap_err();
    assert_eq!(err.status, Some(400));
    assert!(err.message.contains("skills/pdf, skills/x"), "{}", err.message);
    // Upstream moves on after the preview; the previewed commit is what's added.
    write(&source.join("skills/pdf"), "SKILL.md", "---\ndescription: v2\n---\n");
    git(&source, &["commit", "--quiet", "-am", "v2"]);
    let added = m
        .client
        .add_import(&AddImportRequest { commit: Some(r.commit.clone()), ..add(Some("skills/pdf")) })
        .await
        .unwrap();
    assert_eq!((added.name.as_str(), added.new_commit.as_str()), ("pdf", r.commit.as_str()));
    let vendored = std::fs::read_to_string(m.repo().join("imported/pdf/SKILL.md")).unwrap();
    assert!(vendored.contains("Work with PDFs"), "{vendored}");
    let draft = eventually(|| async {
        let d = m.client.draft().await.unwrap();
        (!d.is_empty()).then_some(d)
    })
    .await;
    assert!(draft.iter().any(|d| d.item == ItemRef::Skill("pdf".into())), "{draft:?}");

    let selected_ref = m.client.resolve_import(&ResolveImportRequest { git_ref: "main".into(), ..req }).await.unwrap();
    assert_eq!(selected_ref.git_ref, "main");
    let local = m.repo().join("imported/pdf/SKILL.md");
    std::fs::write(&local, "my edits").unwrap();
    let preview = m.client.import_changes("pdf", Some(&selected_ref.commit)).await.unwrap();
    assert_eq!(preview.locked, r.commit);
    assert_eq!(preview.commits[0].subject, "v2");
    assert_eq!(preview.files, ["skills/pdf/SKILL.md"]);
    let error = m.client.update_import("pdf").await.unwrap_err();
    assert_eq!(error.status, Some(400));
    assert!(error.message.contains("locally modified imports: pdf"), "{}", error.message);
    assert!(error.message.contains("fork") && error.message.contains("overwrite"), "{}", error.message);
    assert_eq!(std::fs::read_to_string(&local).unwrap(), "my edits");
    write(&source.join("skills/pdf"), "SKILL.md", "---\ndescription: v3\n---\n");
    git(&source, &["commit", "--quiet", "-am", "v3"]);
    let rejected = m.client.update_reviewed_import("pdf", true, Some(&preview.upstream)).await.unwrap_err();
    assert_eq!(rejected.status, Some(409));
    assert_eq!(std::fs::read_to_string(&local).unwrap(), "my edits");
    let fresh = m.client.import_changes("pdf", None).await.unwrap();
    let updated = m.client.update_reviewed_import("pdf", true, Some(&fresh.upstream)).await.unwrap();
    assert_eq!(updated.new_commit, fresh.upstream);
    assert!(std::fs::read_to_string(&local).unwrap().contains("v3"));
}

/// A GitHub folder link with an explicit `ref` and an explicit root `path`
/// (`""`, as the GUI sends it): both override the link. GitHub is a local
/// repo here (`url.<dir>.insteadOf`).
#[tokio::test(flavor = "multi_thread")]
async fn an_explicit_root_path_overrides_a_folder_link() {
    let server = FakeServer::new().await;
    let gh = tempfile::tempdir().unwrap();
    let source = gh.path().join("o/r.git");
    write(&source, "SKILL.md", "---\ndescription: The root skill\n---\n");
    write(&source.join("sub/pdf"), "SKILL.md", "---\ndescription: PDFs\n---\n");
    git(&source, &["init", "--quiet", "-b", "main"]);
    git(&source, &["add", "-A"]);
    git(&source, &["commit", "--quiet", "-m", "skills"]);
    let base = format!("file://{}/", gh.path().display());
    let mut m = solo("solo", &server.url(), Some(snapshot(1, &["x"])))
        .with_env("GIT_CONFIG_COUNT", "1")
        .with_env("GIT_CONFIG_KEY_0", &format!("url.{base}.insteadOf"))
        .with_env("GIT_CONFIG_VALUE_0", "https://github.com/");
    m.start().await;

    let link = "https://github.com/o/r/tree/main/sub/pdf";
    let r = m
        .client
        .resolve_import(&ResolveImportRequest { link: link.into(), path: String::new(), git_ref: String::new() })
        .await
        .unwrap();
    assert_eq!((r.source.as_str(), r.git_ref.as_str()), ("github:o/r", "main"));
    assert_eq!(r.candidates.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(), ["sub/pdf"]);

    let req: AddImportRequest = serde_json::from_value(serde_json::json!({
        "name": "", "source": link, "path": "", "ref": "HEAD",
    }))
    .unwrap();
    let added = m.client.add_import(&req).await.unwrap();
    assert_eq!((added.name.as_str(), added.new_commit.as_str()), ("r", r.commit.as_str()));
    let vendored = std::fs::read_to_string(m.repo().join("imported/r/SKILL.md")).unwrap();
    assert!(vendored.contains("The root skill"), "{vendored}");
    let manifest = std::fs::read_to_string(m.repo().join("sk1ll.toml")).unwrap();
    assert!(manifest.contains("[import.r]\nsource = \"github:o/r\"\nref = \"HEAD\""), "{manifest}");
}

/// A resolve stuck fetching (an ssh that never answers) holds up no other
/// call, including ones that need the engine mutex, and ends at the git
/// deadline.
#[tokio::test(flavor = "multi_thread")]
async fn a_hanging_fetch_blocks_no_other_call() {
    let server = FakeServer::new().await;
    let probe = tempfile::tempdir().unwrap();
    let pidfile = probe.path().join("ssh.pid");
    let ssh = format!("sleep 30 & echo $! > '{}'; wait; :", pidfile.display());
    let mut m = solo("solo", &server.url(), Some(snapshot(1, &["x"])))
        .with_env("GIT_SSH_COMMAND", &ssh)
        .with_env("GIT_SSH_VARIANT", "simple")
        .with_env("SK1LL_GIT_TIMEOUT", "4s");
    m.start().await;

    let client = m.client.clone();
    let req = ResolveImportRequest {
        link: "ssh://sk1ll.invalid/slow.git".into(),
        path: String::new(),
        git_ref: String::new(),
    };
    let stuck = tokio::spawn(async move { client.resolve_import(&req).await });
    eventually(|| async { pidfile.exists().then_some(()) }).await;

    let started = std::time::Instant::now();
    m.client.status().await.unwrap();
    m.client.items().await.unwrap();
    m.client.draft().await.unwrap();
    m.client.sync(&Default::default()).await.unwrap();
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(2), "other calls waited {elapsed:?} for the fetch");
    assert!(!stuck.is_finished(), "the fetch was still hanging");

    let err = stuck.await.unwrap().unwrap_err();
    assert!(err.message.contains("timed out"), "{}", err.message);
}

/// A pull's fetch waits for the server without the engine mutex, so the
/// read-only endpoints (which never take it) and engine operations alike keep
/// answering.
#[tokio::test(flavor = "multi_thread")]
async fn reads_and_syncs_answer_while_a_pull_fetches() {
    use sk1ll_daemon::client::{ConflictDiffRequest, ContentQuery, FileQuery};
    // Never accepted: the daemon stays offline and pulls only when asked.
    let server = FakeServer::new().await;
    let probe = tempfile::tempdir().unwrap();
    let pidfile = probe.path().join("ssh.pid");
    let ssh = format!("sleep 30 & echo $! > '{}'; wait; :", pidfile.display());
    let mut m = solo("solo", &server.url(), Some(snapshot(1, &["x"])))
        .with_env("GIT_SSH_COMMAND", &ssh)
        .with_env("GIT_SSH_VARIANT", "simple")
        .with_env("SK1LL_GIT_TIMEOUT", "8s")
        .with_env("SK1LL_RECONCILE_INTERVAL", "1h");
    m.start().await;
    eventually(|| async { m.harness_skill("x").map(drop) }).await;
    // A draft change, and a conflict: a hand edit of a synced skill.
    m.write("personal/x/SKILL.md", "x edited\n");
    eventually(|| async { (m.harness_skill("x")? == "x edited\n").then_some(()) }).await;
    write(&m.skills().join("x"), "SKILL.md", "edited in the harness\n");
    eventually(|| async { (!m.status().await.report.conflicts.is_empty()).then_some(()) }).await;

    git(&m.repo(), &["remote", "set-url", "origin", "ssh://sk1ll.invalid/slow.git"]);
    let client = m.client.clone();
    let stuck = tokio::spawn(async move { client.pull().await });
    eventually(|| async { pidfile.exists().then_some(()) }).await;

    let query = ContentQuery { item: "skill:x".into(), hash: None, harness: None };
    let started = std::time::Instant::now();
    let draft = m.client.draft().await.unwrap();
    assert_eq!(draft.iter().map(|d| d.item.to_string()).collect::<Vec<_>>(), ["skill:x"]);
    assert!(m.client.diff("skill:x").await.unwrap().diff.contains("+x edited"));
    assert!(m.client.commit_message(&["skill:x".into()]).await.unwrap().message.contains('x'));
    assert!(m.client.skills().await.unwrap().iter().any(|e| e.name == "x"));
    assert!(m.client.skill_tree(&query).await.unwrap().iter().any(|f| f.path == "SKILL.md"));
    let query = FileQuery { content: query, path: "SKILL.md".into() };
    assert_eq!(m.client.skill_file(&query).await.unwrap().text.as_deref(), Some("x edited\n"));
    let conflicts = m.client.conflicts().await.unwrap();
    assert_eq!(conflicts.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["x"]);
    m.client.recovered().await.unwrap();
    let request = ConflictDiffRequest { harness: Some(HARNESS.into()), name: Some("x".into()), id: None };
    assert!(m.client.conflict_diff(&request).await.unwrap().diff.contains("edited in the harness"));
    assert_eq!(m.client.harnesses().await.unwrap().len(), 1);
    assert!(!m.client.items().await.unwrap().is_empty());
    m.client.status().await.unwrap();
    m.client.sync(&Default::default()).await.unwrap();
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(3), "the calls waited {elapsed:?} for the pull");
    assert!(!stuck.is_finished(), "the pull was still fetching");

    let err = stuck.await.unwrap().unwrap_err();
    assert!(err.message.contains("timed out"), "{}", err.message);
}

#[tokio::test(flavor = "multi_thread")]
async fn skills_api_browses_content_and_adopts_only_while_connected() {
    use sk1ll_daemon::client::{ConflictTarget, ContentQuery, ContentType, FileQuery};
    let mut server = Server::start().await;
    let mut m = Machine::new("skills");
    // Identical copies in two Harnesses, a differing third, and an empty
    // fourth whose new-skills default remains off.
    let config = Machine::machine_config(m.home.path());
    let mut text = std::fs::read_to_string(&config).unwrap();
    for h in ["second", "different", "empty"] {
        text.push_str(&format!(
            "\n[harnesses.{h}]\nskills = {:?}\nmode = \"copy\"\n",
            m.home.path().join(h).to_str().unwrap()
        ));
    }
    std::fs::write(config, text).unwrap();
    m.join(&server).await;
    m.start_connected().await;
    m.write("personal/baseline/SKILL.md", "---\ndescription: baseline\n---\nold");
    m.write("instructions/git.md", "---\ndescription: Git\n---\nrule baseline");
    assert!(m.commit_all().await.pushed);
    m.write("personal/baseline/SKILL.md", "---\ndescription: draft\n---\nnew");
    std::fs::remove_file(m.repo().join("instructions/git.md")).unwrap();
    let items = m.client.skills().await.unwrap();
    let baseline = items.iter().find(|s| s.name == "baseline").unwrap();
    assert!(baseline.in_baseline);
    assert_eq!(baseline.description.as_deref(), Some("draft"));
    assert_eq!(baseline.draft, Some(sk1ll_core::Change::Modified));
    let rule = items.iter().find(|s| s.name == "git").unwrap();
    assert_eq!(rule.kind, ContentType::Rule);
    assert_eq!(rule.draft, Some(sk1ll_core::Change::Deleted));
    let q = |item: &str| ContentQuery { item: item.into(), hash: None, harness: None };
    let file = |item: &str, path: &str| FileQuery { content: q(item), path: path.into() };
    assert!(m.client.skill_file(&file("skill:baseline", "SKILL.md")).await.unwrap().text.unwrap().ends_with("new"));
    assert!(m.client.skill_file(&file("rule:git", "git.md")).await.unwrap().text.unwrap().ends_with("rule baseline"));
    assert_eq!(m.client.skill_tree(&q("rule:git")).await.unwrap()[0].path, "git.md");
    assert!(m.client.skill_file(&file("skill:baseline", "../sk1ll.toml")).await.is_err());
    std::fs::write(m.repo().join("personal/baseline/binary"), b"\x00\xff").unwrap();
    assert!(m.client.skill_file(&file("skill:baseline", "binary")).await.unwrap().binary);
    for path in [m.skills(), m.home.path().join("second")] {
        write(&path.join("pdf"), "SKILL.md", "---\ndescription: PDFs\n---\nsame");
    }
    write(&m.home.path().join("different/pdf"), "SKILL.md", "different");
    let skills = m.client.skills().await.unwrap();
    let unmanaged = skills.iter().find(|s| s.kind == ContentType::Unmanaged && s.name == "pdf").unwrap();
    assert_eq!(unmanaged.variants.len(), 2);
    assert!(!unmanaged.clashes_with_repo);
    let variant = unmanaged.variants.iter().find(|v| v.locations.len() == 2).unwrap();
    let selected = ContentQuery { hash: Some(variant.hash.clone()), ..q("unmanaged:pdf") };
    assert_eq!(m.client.skill_tree(&selected).await.unwrap()[0].path, "SKILL.md");
    assert!(
        m.client
            .skill_file(&FileQuery { content: selected, path: "SKILL.md".into() })
            .await
            .unwrap()
            .text
            .unwrap()
            .ends_with("same")
    );
    assert!(m.client.skill_tree(&q("unmanaged:pdf")).await.is_err());
    server.stop();
    eventually(|| async { (m.status().await.connection == Connection::Offline).then_some(()) }).await;
    let target = ConflictTarget { harness: HARNESS.into(), name: "pdf".into() };
    for result in [m.client.adopt_unmanaged(&target).await, m.client.adopt(&target).await] {
        assert!(result.unwrap_err().message.contains("Adopt needs a connection to the Server"));
    }
    assert!(!m.repo().join("personal/pdf").exists());
    assert!(!m.skills().join(sk1ll_core::sync::INDEX_PATH).exists());
    assert!(m.harness_skill("pdf").unwrap().ends_with("same"));
    server.launch().await;
    eventually(|| async { (m.status().await.connection == Connection::Connected).then_some(()) }).await;
    m.client.adopt_unmanaged(&target).await.unwrap();
    assert!(m.repo().join("personal/pdf/SKILL.md").is_file());
    assert_indexed_copy(&m.skills(), "pdf");
    assert_indexed_copy(&m.home.path().join("second"), "pdf");
    // The entire operation may have completed before the local response was
    // lost. Repeating it must still succeed and keep its explicit Assignment.
    m.client.adopt_unmanaged(&target).await.unwrap();

    let snap = m.snapshot().unwrap();
    let pdf = sk1ll_core::Repo::open(m.repo()).unwrap().skills().unwrap().into_iter().find(|s| s.name == "pdf");
    let pdf = pdf.unwrap().id.unwrap();
    assert!(!snap.assignment.skill_enabled("different", &pdf));
    assert!(!snap.assignment.skill_enabled("empty", &pdf));
    assert!(!m.home.path().join("empty/pdf").exists());
    assert_eq!(std::fs::read_to_string(m.home.path().join("different/pdf/SKILL.md")).unwrap(), "different");
    assert!(m.skills().join(".sk1ll-tmp/displaced").is_dir());
    let recovered = m.client.recovered().await.unwrap();
    let originals = recovered
        .iter()
        .filter(|entry| {
            entry.name == "pdf"
                && entry.path.as_ref().is_some_and(|path| {
                    std::fs::read_to_string(path.join("SKILL.md")).is_ok_and(|text| text.ends_with("same"))
                })
        })
        .count();
    assert_eq!(originals, 2);

    let skills = m.client.skills().await.unwrap();
    let adopted = skills.iter().find(|s| s.name == "pdf" && s.kind == ContentType::Personal).unwrap();
    assert_eq!(adopted.enabled_harnesses, [HARNESS, "second"]);
    assert_eq!(adopted.draft, Some(sk1ll_core::Change::Added));
    let unmanaged = skills.iter().find(|s| s.kind == ContentType::Unmanaged && s.name == "pdf").unwrap();
    assert!(unmanaged.clashes_with_repo);
    assert!(
        m.client
            .adopt_unmanaged(&ConflictTarget { harness: "different".into(), name: "pdf".into() })
            .await
            .unwrap_err()
            .message
            .contains("already exists")
    );
    let log: Vec<LogEntry> = server.get("/log").await;
    assert!(
        log.iter().any(|e| e.kind == ToggleKind::Skill && e.name.as_deref() == Some("pdf") && e.after == Some(true))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unmanaged_adoption_retries_after_crash_between_publication_and_assignment() {
    use sk1ll_daemon::client::ConflictTarget;
    let server = Server::start().await;
    let mut m = Machine::new("adopt-retry");
    m.join(&server).await;
    m.start_connected().await;
    m.stop();
    write(&m.skills().join("pdf"), "SKILL.md", "original user content");
    let repo = sk1ll_core::Repo::open(m.repo()).unwrap();
    let hs = sk1ll_core::MachineConfig::load(&Machine::machine_config(m.home.path())).unwrap().harnesses();
    // Simulate a process dying just after atomic draft publication. There is
    // no intent or startup reconciliation; the original is still unmanaged.
    sk1ll_core::sync::unmanaged::prepare(&repo, &hs, HARNESS, "pdf").unwrap();
    m.start_connected().await;
    assert!(!m.skills().join(sk1ll_core::sync::INDEX_PATH).exists());
    let target = ConflictTarget { harness: HARNESS.into(), name: "pdf".into() };
    m.client.adopt(&target).await.unwrap();
    let snap: AssignmentSnapshot = server.get(&format!("/machines/{}/assignment", m.id)).await;
    let pdf = sk1ll_core::Repo::open(m.repo()).unwrap().skills().unwrap().into_iter().find(|s| s.name == "pdf");
    let pdf = pdf.unwrap().id.unwrap();
    assert!(snap.assignment.skill_enabled(HARNESS, &pdf));
    assert_indexed_copy(&m.skills(), "pdf");
    assert_eq!(m.harness_skill("pdf").unwrap(), "original user content");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gui_creation_import_inventory_check_and_conflict_diff() {
    use sk1ll_daemon::client::{ConflictDiffRequest, CreateItemRequest};
    let mut snap = snapshot(1, &["x"]);
    snap.assignment.set_rule(HARNESS, &"diff-rule-id".parse().unwrap(), true);
    // The Import added below gets its Item ID then: the default enables it.
    snap.assignment.harnesses.get_mut(HARNESS).unwrap().new_skills_default = true;
    let mut m = solo("gui", "http://127.0.0.1:1", Some(snap));
    m.write(
        "instructions/diff-rule.md",
        &format!("---\nid: diff-rule-id\nharnesses: [{HARNESS}]\npaths: [src/**]\n---\nDraft rule.\n"),
    );
    m.start().await;
    m.client
        .create_item(&CreateItemRequest {
            kind: "rule".into(),
            name: "style".into(),
            description: Some("House style".into()),
            bundled: None,
        })
        .await
        .unwrap();
    assert!(m.repo().join("instructions/style.md").exists());
    assert!(
        m.client
            .create_item(&CreateItemRequest {
                kind: "rule".into(),
                name: "style".into(),
                description: None,
                bundled: None
            })
            .await
            .is_err()
    );
    let upstream = tempfile::tempdir().unwrap();
    git(upstream.path(), &["init", "-q", "-b", "main"]);
    write(upstream.path(), "SKILL.md", "upstream v1\n");
    write(upstream.path(), ".sk1ll-id", "vendored content\n");
    std::os::unix::fs::symlink("SKILL.md", upstream.path().join("alias")).unwrap();
    git(upstream.path(), &["add", "."]);
    git(upstream.path(), &["commit", "-qm", "first"]);
    m.client
        .add_import(&AddImportRequest {
            name: "external".into(),
            source: upstream.path().display().to_string(),
            path: Some("".into()),
            git_ref: Some("main".into()),
            commit: None,
        })
        .await
        .unwrap();
    let inventory = m.client.imports().await.unwrap();
    assert_eq!(inventory.imports.len(), 1);
    assert_eq!(inventory.imports[0].name, "external");
    assert!(inventory.imports[0].locked.is_some());
    write(upstream.path(), "SKILL.md", "upstream v2\n");
    git(upstream.path(), &["commit", "-qam", "second"]);
    let checked = m.client.check_imports().await.unwrap();
    assert!(checked.errors.is_empty(), "{:?}", checked.errors);
    assert_eq!(checked.updates.len(), 1);
    assert_eq!(m.client.imports().await.unwrap().updates, checked.updates);
    eventually(|| async { m.harness_skill("x").filter(|s| s == "x\n") }).await;
    write(&m.skills().join("x"), "SKILL.md", "Harness edit\n");
    eventually(|| async { m.client.conflicts().await.ok().filter(|v| v.iter().any(|v| v.name == "x")) }).await;
    let edited = std::fs::metadata(m.skills().join("x/SKILL.md"))
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let conflict = m.client.conflicts().await.unwrap().into_iter().find(|v| v.name == "x").unwrap();
    assert!(conflict.detected_at.is_some_and(|at| at >= edited), "{conflict:?}");
    let diff = m
        .client
        .conflict_diff(&ConflictDiffRequest { harness: Some(HARNESS.into()), name: Some("x".into()), id: None })
        .await
        .unwrap();
    assert!(diff.diff.contains("-Harness edit"), "{}", diff.diff);
    assert!(diff.diff.contains("+x"), "{}", diff.diff);
    assert!(!diff.diff.contains(sk1ll_core::sync::PLACEMENT_MARKER), "{}", diff.diff);
    assert!(!diff.diff.contains(".sk1ll-id"), "{}", diff.diff);

    let rule_dir = m.home.path().join("agent/rules");
    let rule = rule_dir.join("diff-rule.md");
    eventually(|| async { std::fs::read_to_string(&rule).ok() }).await;
    write(&rule_dir, "diff-rule.md", "---\npaths: [src/**]\n---\n\nHarness rule.\n");
    eventually(|| async { m.client.conflicts().await.ok().filter(|v| v.iter().any(|v| v.name == "diff-rule")) }).await;
    let rule_diff = m
        .client
        .conflict_diff(&ConflictDiffRequest { harness: Some(HARNESS.into()), name: Some("diff-rule".into()), id: None })
        .await
        .unwrap()
        .diff;
    assert!(rule_diff.contains("-Harness rule.") && rule_diff.contains("+Draft rule."), "{rule_diff}");
    assert!(!rule_diff.contains("diff-rule-id") && !rule_diff.contains("harnesses:"), "{rule_diff}");
    eventually(|| async { m.harness_skill("external") }).await;
    let external = m.skills().join("external");
    write(&external, ".sk1ll-id", "changed sidecar\n");
    std::fs::remove_file(external.join("alias")).unwrap();
    std::os::unix::fs::symlink(".sk1ll-id", external.join("alias")).unwrap();
    let diff = m
        .client
        .conflict_diff(&ConflictDiffRequest { harness: Some(HARNESS.into()), name: Some("external".into()), id: None })
        .await
        .unwrap();
    assert!(diff.diff.contains("-changed sidecar") && diff.diff.contains("+vendored content"), "{}", diff.diff);
    assert!(diff.diff.contains("120000") && diff.diff.contains("alias"), "{}", diff.diff);
    assert_eq!(diff.target, Some(m.repo().join("imported/external")));
    m.client.remove_import("external").await.unwrap();
    assert!(m.client.imports().await.unwrap().imports.is_empty());
    assert!(!m.repo().join("imported/external").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_repairs_duplicate_ids_and_persists_handmade_ids_in_the_draft() {
    let mut m = solo("repair", "http://127.0.0.1:1", Some(snapshot(1, &["x"])));
    let source = m.repo().join("personal/x");
    let copy = m.repo().join("personal/copy");
    std::fs::create_dir(&copy).unwrap();
    for file in ["SKILL.md", ".sk1ll-id"] {
        std::fs::copy(source.join(file), copy.join(file)).unwrap();
    }
    m.write("instructions/handmade.md", "Plain rule.\n");
    m.start().await;
    let error = m.client.items().await.unwrap_err();
    assert!(error.message.contains("personal/copy/.sk1ll-id") && error.message.contains("personal/x/.sk1ll-id"));
    assert_eq!(m.client.repair_ids().await.unwrap(), 1);
    let items = m.client.items().await.unwrap();
    let get = |item| items.iter().find(|entry| entry.item == item).unwrap();
    let copy = get(ItemRef::Skill("copy".into()));
    let source = get(ItemRef::Skill("x".into()));
    assert!(copy.id.is_some() && source.id.is_some() && copy.id != source.id);
    assert!(get(ItemRef::Rule("handmade".into())).id.is_some());
    assert_eq!(m.client.repair_ids().await.unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn block_conflict_diff_preserves_surrounding_notes() {
    use sk1ll_daemon::client::ConflictDiffRequest;
    let mut snap = snapshot(1, &[]);
    snap.assignment.harnesses.get_mut(HARNESS).unwrap().new_rules_default = true;
    let mut m = solo("block-diff", "http://127.0.0.1:1", Some(snap));
    let config = Machine::machine_config(m.home.path());
    let cfg = std::fs::read_to_string(&config)
        .unwrap()
        .replace("instructions_format = \"rules\"", "instructions_format = \"block\"");
    std::fs::write(config, cfg).unwrap();
    m.write("instructions/style.md", "---\ndescription: Style\n---\nExpected rule\n");
    git(&m.repo(), &["add", "."]);
    git(&m.repo(), &["commit", "-qm", "rule"]);
    m.start().await;
    let path = m.home.path().join("agent/rules");
    let original =
        eventually(|| async { std::fs::read_to_string(&path).ok().filter(|s| s.contains("Expected rule")) }).await;
    std::fs::write(
        &path,
        format!("My private notes\n{}\nMore notes\n", original.replace("Expected rule", "Edited rule")),
    )
    .unwrap();
    eventually(|| async { m.client.conflicts().await.ok().filter(|v| v.iter().any(|v| v.name == "instructions")) })
        .await;
    let diff = m
        .client
        .conflict_diff(&ConflictDiffRequest {
            harness: Some(HARNESS.into()),
            name: Some("instructions".into()),
            id: None,
        })
        .await
        .unwrap();
    assert!(diff.diff.contains("-Edited rule"), "{}", diff.diff);
    assert!(diff.diff.contains("+Expected rule"), "{}", diff.diff);
    assert!(!diff.diff.contains("private notes"), "{}", diff.diff);
    assert!(!diff.diff.contains("More notes"), "{}", diff.diff);
}

// Standalone tests use the local API and git only; no fleet server is needed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_starts_without_a_repo_and_init_creates_it() {
    use sk1ll_daemon::client::DaemonMode;
    let mut m = Machine::new("solo");
    m.start().await;
    let status = m.status().await;
    assert_eq!(status.mode, DaemonMode::Standalone);
    assert_eq!(status.connection, Connection::Standalone);
    assert_eq!(status.server_url, None);
    assert_eq!(status.server_baseline, None);
    assert!(!status.repo_ready);
    assert!(!status.has_origin);
    assert!(!status.awaiting_assignment);
    assert_eq!(status.headline, MachineStatus::InSync);
    assert_eq!(status.machine_id, "solo");
    assert_eq!(status.repo, m.repo());
    let error = m.client.skills().await.unwrap_err();
    assert_eq!(error.status, Some(409));
    assert!(error.message.contains("no skills repo"));
    let copy = sk1ll_proto::MachineAssignmentCopy {
        source: "other".into(),
        harnesses: None,
        mode: sk1ll_proto::CopyMode::Replace,
        expected: None,
        source_expected: None,
    };
    for error in [
        m.client.copy_machine_assignment("solo", &copy).await.unwrap_err(),
        m.client.preview_assignment_copy("solo", &copy).await.unwrap_err(),
        m.client.fleet().await.unwrap_err(),
        m.client.log(&Default::default()).await.unwrap_err(),
        m.client.log_facets().await.unwrap_err(),
        m.client.undo(1).await.unwrap_err(),
        m.client.join_code(Default::default()).await.unwrap_err(),
        m.client.remove_machine("solo").await.unwrap_err(),
        m.client.assignment("other").await.unwrap_err(),
    ] {
        assert_eq!(error.status, Some(409));
        assert_eq!(error.message, "This Machine is standalone; join a Fleet to use this");
    }
    m.client.init().await.unwrap();
    assert!(m.repo().join("sk1ll.toml").is_file());
    assert!(m.status().await.repo_ready);
    assert!(!m.state_path().exists());
    assert!(sk1ll_core::EngineLock::try_acquire(&m.repo()).unwrap().is_none());
    assert_eq!(m.client.init().await.unwrap_err().status, Some(409));
    let error = m.client.push().await.unwrap_err();
    assert_eq!(error.status, Some(409));
    assert_eq!(error.message, "this repo has no origin remote");
    let error = m.client.pull().await.unwrap_err();
    assert_eq!(error.status, Some(409));
    assert_eq!(error.message, "this repo has no origin remote");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_syncs_and_keeps_assignment_local_across_restarts() {
    let mut m = Machine::new("solo");
    sk1ll_core::Repo::init(&m.repo()).unwrap();
    m.write("personal/pdf/SKILL.md", "original");
    m.start().await;
    eventually(|| async { (m.harness_skill("pdf").as_deref() == Some("original")).then_some(()) }).await;
    m.write("personal/pdf/SKILL.md", "edited");
    eventually(|| async { (m.harness_skill("pdf").as_deref() == Some("edited")).then_some(()) }).await;
    let original = m.client.assignment("solo").await.unwrap();
    let changes = AssignmentChanges {
        changes: vec![ToggleChange {
            harness: HARNESS.into(),
            kind: ToggleKind::Skill,
            item_id: Some(skill_id("pdf")),
            name: Some("pdf".into()),
            enabled: Some(false),
        }],
        ..Default::default()
    };
    let update = m.client.set_assignment("solo", &changes).await.unwrap();
    assert!(update.entries.is_empty());
    assert!(update.revision > original.revision);
    assert!(!Assignment::load(&m.cache_path()).unwrap().skill_enabled(HARNESS, &skill_id("pdf")));
    assert!(m.harness_skill("pdf").is_none());
    assert!(!m.snapshot_path().exists(), "local snapshots must not become fleet snapshots");
    let revision = update.revision;
    m.stop();
    m.start().await;
    assert_eq!(m.client.assignment("solo").await.unwrap().revision, revision);
    assert!(m.harness_skill("pdf").is_none());
    // A direct CLI assign atomically replaces this same file.
    let mut assignment = Assignment::load(&m.cache_path()).unwrap();
    assignment.set_skill(HARNESS, &skill_id("pdf"), true);
    assignment.save(&m.cache_path()).unwrap();
    eventually(|| async { (m.harness_skill("pdf").as_deref() == Some("edited")).then_some(()) }).await;
    assert!(m.status().await.assignment_revision > revision);
    // Defaults preserve current items, while later skills follow the new default.
    m.client
        .set_assignment(
            "solo",
            &AssignmentChanges {
                changes: vec![ToggleChange {
                    harness: HARNESS.into(),
                    kind: ToggleKind::NewSkillsDefault,
                    item_id: None,
                    name: None,
                    enabled: Some(false),
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    m.write("personal/new/SKILL.md", "new skill");
    m.client.sync(&Default::default()).await.unwrap();
    assert!(m.harness_skill("pdf").is_some());
    assert!(m.harness_skill("new").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_adopts_unmanaged_skills_with_local_assignment() {
    let mut m = Machine::new("solo");
    sk1ll_core::Repo::init(&m.repo()).unwrap();
    let mut assignment = Assignment::default();
    assignment.set_new_skills_default(HARNESS, false);
    assignment.save(&m.cache_path()).unwrap();
    write(&m.skills().join("pdf"), "SKILL.md", "unmanaged");
    m.start().await;
    m.client
        .adopt_unmanaged(&sk1ll_daemon::client::ConflictTarget { harness: HARNESS.into(), name: "pdf".into() })
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(m.repo().join("personal/pdf/SKILL.md")).unwrap(), "unmanaged");
    assert_eq!(m.harness_skill("pdf").as_deref(), Some("unmanaged"));
    assert_indexed_copy(&m.skills(), "pdf");
    let pdf = sk1ll_core::Repo::open(m.repo()).unwrap().skills().unwrap()[0].id.clone().unwrap();
    assert!(Assignment::load(&m.cache_path()).unwrap().skill_enabled(HARNESS, &pdf));
    assert!(m.client.draft().await.unwrap().iter().any(|d| d.item == ItemRef::Skill("pdf".into())));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_commits_locally_and_pushes_and_pulls_its_own_origin() {
    let mut m = Machine::new("solo");
    sk1ll_core::Repo::init(&m.repo()).unwrap();
    let bare = m.home.path().join("origin.git");
    git(m.home.path(), &["init", "--quiet", "--bare", "-b", "main", bare.to_str().unwrap()]);
    git(&m.repo(), &["remote", "add", "origin", bare.to_str().unwrap()]);
    m.write("personal/pdf/SKILL.md", "initial");
    m.start().await;
    assert!(m.status().await.has_origin);
    let commit = m.commit_all().await;
    assert!(!commit.pushed);
    assert_eq!(commit.push_error, None);
    assert!(git(&bare, &["for-each-ref"]).is_empty());
    assert!(m.client.push().await.unwrap().pushed);
    assert_eq!(git(&bare, &["rev-parse", "HEAD"]), commit.sha);
    let peer = m.home.path().join("peer");
    git(m.home.path(), &["clone", "--quiet", bare.to_str().unwrap(), peer.to_str().unwrap()]);
    write(&peer.join("personal/pdf"), "SKILL.md", "remote edit");
    git(&peer, &["add", "-A"]);
    git(&peer, &["commit", "--quiet", "-m", "remote edit"]);
    git(&peer, &["push", "--quiet", "origin", "main"]);
    assert!(matches!(m.client.pull().await.unwrap(), sk1ll_daemon::client::PullResult::Updated { .. }));
    assert_eq!(m.harness_skill("pdf").as_deref(), Some("remote edit"));
    m.write("personal/pdf/SKILL.md", "local draft");
    write(&peer.join("personal/pdf"), "SKILL.md", "another remote edit");
    git(&peer, &["add", "-A"]);
    git(&peer, &["commit", "--quiet", "-m", "another remote edit"]);
    git(&peer, &["push", "--quiet", "origin", "main"]);
    assert!(matches!(m.client.pull().await.unwrap(), sk1ll_daemon::client::PullResult::Blocked { .. }));
    assert_eq!(m.status().await.headline, MachineStatus::Blocked);
    assert_eq!(std::fs::read_to_string(m.repo().join("personal/pdf/SKILL.md")).unwrap(), "local draft");
}

/// A pull that stopped half-way (here: at a directory it can't write to)
/// is finished by the daemon's next refresh, without another pull.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refresh_finishes_a_pull_that_stopped_half_way() {
    use sk1ll_daemon::client::{PullResult, RenameItemRequest, SyncRequest};
    use std::os::unix::fs::PermissionsExt;
    let mut m = Machine::new("solo");
    sk1ll_core::Repo::init(&m.repo()).unwrap();
    let bare = m.home.path().join("origin.git");
    git(m.home.path(), &["init", "--quiet", "--bare", "-b", "main", bare.to_str().unwrap()]);
    git(&m.repo(), &["remote", "add", "origin", bare.to_str().unwrap()]);
    m.write("personal/pdf/SKILL.md", "initial");
    m.start().await;
    m.commit_all().await;
    assert!(m.client.push().await.unwrap().pushed);
    let peer = m.home.path().join("peer");
    git(m.home.path(), &["clone", "--quiet", bare.to_str().unwrap(), peer.to_str().unwrap()]);
    write(&peer.join("instructions"), "git.md", "Use Conventional Commits.");
    write(&peer.join("personal/pdf"), "SKILL.md", "remote edit");
    git(&peer, &["add", "-A"]);
    git(&peer, &["commit", "--quiet", "-m", "remote edit"]);
    git(&peer, &["push", "--quiet", "origin", "main"]);
    let remote = git(&peer, &["rev-parse", "HEAD"]);

    let pdf = m.repo().join("personal/pdf");
    let mode = |mode| std::fs::set_permissions(&pdf, std::fs::Permissions::from_mode(mode)).unwrap();
    mode(0o555);
    if std::fs::write(pdf.join("probe"), "").is_ok() {
        return; // root writes into read-only directories
    }
    m.client.pull().await.unwrap_err();
    let journal = m.repo().join(".git/sk1ll-pull.json");
    assert!(journal.exists());
    let rule = std::fs::read_to_string(m.repo().join("instructions/git.md")).unwrap();
    assert_eq!(rule, "Use Conventional Commits.", "rolled forward, never back");
    let report = m.status().await.report;
    assert_eq!(report.pull_pending, Some(vec![]));
    assert!(report.drafts.is_empty(), "the partial checkout is not a Draft");
    assert_eq!(m.harness_skill("pdf").as_deref(), Some("initial"));
    let error = m.client.sync(&SyncRequest { force: true, ..Default::default() }).await.unwrap_err();
    assert!(error.to_string().contains("pull pending"), "{error}");
    // Nor does anything write Item IDs into the partial tree meanwhile.
    let error = m.client.repair_ids().await.unwrap_err();
    assert!(error.to_string().contains("pull stopped half-way"), "{error}");
    let rename = RenameItemRequest { kind: "skill".into(), name: "pdf".into(), to: "new-pdf".into() };
    let error = m.client.rename_item(&rename).await.unwrap_err();
    assert!(error.to_string().contains("pull stopped half-way"), "{error}");
    assert!(pdf.join("SKILL.md").is_file() && !m.repo().join("personal/new-pdf").exists());

    // An editor can still write to a file in the read-only directory.
    m.write("personal/pdf/SKILL.md", "saved while pull was pending");
    let pdf_item = ItemRef::Skill("pdf".into());
    eventually(|| async {
        let report = m.status().await.report;
        (report.pull_pending.as_deref() == Some(std::slice::from_ref(&pdf_item))).then_some(())
    })
    .await;
    assert_eq!(m.harness_skill("pdf").as_deref(), Some("initial"));
    assert!(m.status().await.report.drafts.is_empty());
    assert!(matches!(m.client.pull().await.unwrap(), PullResult::Blocked { pending: true, .. }));
    m.write("personal/pdf/SKILL.md", "initial");
    mode(0o755);

    // Any refresh finishes it; this one follows a draft edit.
    m.write("notes.txt", "a draft");
    eventually(|| async { (m.head() == remote && !journal.exists()).then_some(()) }).await;
    eventually(|| async { (m.harness_skill("pdf").as_deref() == Some("remote edit")).then_some(()) }).await;
    assert_eq!(git(&m.repo(), &["symbolic-ref", "HEAD"]), "refs/heads/main");
    // The peer's Rule came without an Item ID: once the pull is done, the
    // refresh writes one, as a Draft change.
    assert_eq!(git(&m.repo(), &["status", "--porcelain"]), "M instructions/git.md\n?? notes.txt");
    let rule = std::fs::read_to_string(m.repo().join("instructions/git.md")).unwrap();
    assert!(rule.contains("id: ") && rule.ends_with("Use Conventional Commits."), "{rule}");
    eventually(|| async {
        let status = m.status().await;
        (status.report.pull_pending.is_none() && status.headline != MachineStatus::Blocked).then_some(())
    })
    .await;
    assert!(m.log().contains("finished a pull that had stopped half-way"), "{}", m.log());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_chooses_harnesses_only_before_the_repo_exists() {
    let mut m = Machine::new("solo");
    for d in [".claude", ".kiro", ".cursor"] {
        std::fs::create_dir_all(m.home.path().join(d)).unwrap();
    }
    m.start().await;
    let enabled = |choices: &[sk1ll_daemon::client::HarnessChoice], name: &str| {
        choices.iter().find(|c| c.name == name).map(|c| (c.installed, c.enabled))
    };
    let path = Machine::machine_config(m.home.path());
    let before = std::fs::read_to_string(&path).unwrap();
    let choices = m.client.setup_harnesses().await.unwrap();
    assert_eq!(enabled(&choices, "claude"), Some((true, true)));
    // Opt-in presets found here are offered on; listing them writes nothing.
    assert_eq!(enabled(&choices, "kiro"), Some((true, true)));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    m.client.set_setup_harness("cursor", false).await.unwrap();
    assert_eq!(enabled(&choices, HARNESS), Some((true, true)));
    assert_eq!(enabled(&choices, "codex"), Some((false, false)));
    let choices = m.client.set_setup_harness("claude", false).await.unwrap();
    assert_eq!(enabled(&choices, "claude"), Some((true, false)));
    assert!(m.client.harnesses().await.unwrap().iter().all(|h| h.name != "claude"));
    let config = std::fs::read_to_string(&path).unwrap();
    assert!(config.contains("instructions_format = \"rules\""), "{config}");
    assert_eq!(m.client.set_setup_harness("nope", false).await.unwrap_err().status, Some(404));

    m.client.init().await.unwrap();
    // Setup ended: the opt-in presets it kept on are enabled in machine.toml.
    let config = std::fs::read_to_string(&path).unwrap();
    assert!(config.contains("[harnesses.kiro]\nenabled = true\n"), "{config}");
    assert!(config.contains("[harnesses.cursor]\nenabled = false\n"), "{config}");
    let error = m.client.set_setup_harness("claude", true).await.unwrap_err();
    assert_eq!(error.status, Some(409));
    assert!(error.message.contains("first-run setup"), "{}", error.message);
    assert_eq!(enabled(&m.client.setup_harnesses().await.unwrap(), "claude"), Some((true, false)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn harnesses_change_after_setup_with_a_preview_of_what_stays() {
    use sk1ll_daemon::client::{HarnessChange, HarnessChangeRequest};
    let mut m = Machine::new("solo");
    sk1ll_core::Repo::init(&m.repo()).unwrap();
    m.write("personal/pdf/SKILL.md", "original");
    write(&m.skills().join("mine"), "SKILL.md", "unmanaged");
    m.start().await;
    eventually(|| async { (m.harness_skill("pdf").as_deref() == Some("original")).then_some(()) }).await;
    let settings = m.client.harness_settings().await.unwrap();
    assert_eq!(settings.path, Machine::machine_config(m.home.path()));
    assert!(settings.harnesses.iter().any(|c| c.name == HARNESS && c.configured && !c.preset));

    let config_text = std::fs::read_to_string(&settings.path).unwrap();
    std::fs::write(&settings.path, "broken = [").unwrap();
    assert!(m.client.harness_settings().await.is_err());
    assert_eq!(m.client.machine_config_path().await.unwrap(), settings.path);
    std::fs::write(&settings.path, config_text).unwrap();

    // Adding a custom harness previews what it receives, then syncs it.
    let extra = m.home.path().join("extra/skills");
    let harness = sk1ll_core::Harness {
        skills: Some(extra.to_string_lossy().into_owned()),
        mode: sk1ll_core::LinkMode::Copy,
        ..Default::default()
    };
    let mut stream = tokio::net::UnixStream::connect(m.client.socket()).await.unwrap();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let body = r#"{"change":{"op":"disable","name":"agent"}}"#;
    let request = format!(
        "POST /harnesses/apply HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{}: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        sk1ll_daemon::client::API_HEADER,
        sk1ll_daemon::client::API_VERSION,
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    assert!(response.contains("this client is older than the daemon; update it"), "{response}");

    let mut add =
        HarnessChangeRequest { fingerprint: None, change: HarnessChange::Set { name: "extra".into(), harness } };
    let preview = m.client.preview_harness(&add).await.unwrap();
    assert!(preview.before.is_none() && preview.placed.is_empty());
    assert_eq!(
        preview.sync.iter().map(|p| (p.kind.as_str(), p.name.as_str())).collect::<Vec<_>>(),
        [("create", "pdf")]
    );
    assert!(!extra.exists(), "a preview changes nothing");
    let original = std::fs::read_to_string(&settings.path).unwrap();
    let edited = format!(
        "{original}
# External edit
"
    );
    std::fs::write(&settings.path, &edited).unwrap();
    add.fingerprint = Some(preview.fingerprint);
    let error = m.client.apply_harness(&add).await.unwrap_err();
    assert_eq!(error.status, Some(409));
    assert!(error.message.contains("machine.toml changed since the preview"));
    assert_eq!(std::fs::read_to_string(&settings.path).unwrap(), edited);
    add.fingerprint = Some(m.client.preview_harness(&add).await.unwrap().fingerprint);
    let applied = m.client.apply_harness(&add).await.unwrap();
    assert!(applied.errors.is_empty(), "{:?}", applied.errors);
    assert!(extra.join("pdf/SKILL.md").is_file());

    // Disabling shows what sk1ll placed, and leaves it.
    let mut disable =
        HarnessChangeRequest { fingerprint: None, change: HarnessChange::Disable { name: "extra".into() } };
    let preview = m.client.preview_harness(&disable).await.unwrap();
    assert!(preview.after.is_none());
    assert_eq!(
        preview.placed.iter().map(|p| (p.kind.as_str(), p.name.as_str())).collect::<Vec<_>>(),
        [("remove", "pdf")]
    );
    disable.fingerprint = Some(preview.fingerprint);
    let applied = m.client.apply_harness(&disable).await.unwrap();
    assert!(applied.errors.is_empty(), "{:?}", applied.errors);
    assert!(extra.join("pdf/SKILL.md").is_file(), "left in place");
    assert!(m.status().await.report.harnesses.iter().all(|h| h != "extra"));

    // Turning a harness's items off first lets sync take them out; then disabling leaves nothing.
    let result = m.client.set_harness_assignment(HARNESS, false).await.unwrap();
    assert!(!result.assignment.skill_enabled(HARNESS, &skill_id("pdf")));
    eventually(|| async { m.harness_skill("pdf").is_none().then_some(()) }).await;
    let mut disable =
        HarnessChangeRequest { fingerprint: None, change: HarnessChange::Disable { name: HARNESS.into() } };
    assert!(m.client.preview_harness(&disable).await.unwrap().placed.is_empty());
    disable.fingerprint = Some(m.client.preview_harness(&disable).await.unwrap().fingerprint);
    m.client.apply_harness(&disable).await.unwrap();
    assert_eq!(m.harness_skill("mine").as_deref(), Some("unmanaged"));
    assert!(m.client.harnesses().await.unwrap().iter().all(|h| h.name != HARNESS));

    // What the sync after a change fails to place is in the result, not just the log.
    // A skills directory sync can read but not write into.
    std::fs::create_dir_all(m.home.path().join("blocked/skills")).unwrap();
    std::fs::set_permissions(m.home.path().join("blocked/skills"), std::os::unix::fs::PermissionsExt::from_mode(0o555))
        .unwrap();
    let blocked = sk1ll_core::Harness {
        skills: Some(m.home.path().join("blocked/skills").to_string_lossy().into_owned()),
        mode: sk1ll_core::LinkMode::Copy,
        ..Default::default()
    };
    let mut add_blocked = HarnessChangeRequest {
        fingerprint: None,
        change: HarnessChange::Set { name: "blocked".into(), harness: blocked },
    };
    add_blocked.fingerprint = Some(m.client.preview_harness(&add_blocked).await.unwrap().fingerprint);
    let applied = m.client.apply_harness(&add_blocked).await.unwrap();
    assert!(applied.errors.iter().any(|e| e.contains("blocked/skills")), "{:?}", applied.errors);
    assert!(applied.harnesses.iter().any(|c| c.name == "blocked" && c.enabled), "machine.toml changed anyway");
    let mut drop_blocked =
        HarnessChangeRequest { fingerprint: None, change: HarnessChange::Remove { name: "blocked".into() } };
    drop_blocked.fingerprint = Some(m.client.preview_harness(&drop_blocked).await.unwrap().fingerprint);
    m.client.apply_harness(&drop_blocked).await.unwrap();
    std::fs::set_permissions(m.home.path().join("blocked/skills"), std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .unwrap();

    let mut bad = HarnessChangeRequest {
        fingerprint: None,
        change: HarnessChange::Set { name: "Bad Name".into(), harness: Default::default() },
    };
    assert_eq!(m.client.preview_harness(&bad).await.unwrap_err().status, Some(400));
    // Without a reviewed preview the client refuses before asking the daemon.
    let unreviewed = m.client.apply_harness(&bad).await.unwrap_err();
    assert_eq!(unreviewed.status, None);
    assert!(unreviewed.message.contains("reviewed preview is required"), "{}", unreviewed.message);
    // A preview of a bad name fails, so any fingerprint sent with it is stale.
    bad.fingerprint = Some("stale".into());
    assert_eq!(m.client.apply_harness(&bad).await.unwrap_err().status, Some(409));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_uses_the_repo_environment_override() {
    let mut m = Machine::new("solo");
    let custom = m.home.path().join("custom-skills");
    m = m.with_env("SK1LL_REPO", custom.to_str().unwrap());
    m.start().await;
    assert_eq!(m.status().await.repo, custom);
    m.client.init().await.unwrap();
    assert!(custom.join("sk1ll.toml").is_file());
    assert!(!m.repo().exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_init_refuses_existing_user_content() {
    let mut m = Machine::new("solo");
    m.write("notes.txt", "keep this");
    m.start().await;
    assert_eq!(m.client.init().await.unwrap_err().status, Some(409));
    assert_eq!(std::fs::read_to_string(m.repo().join("notes.txt")).unwrap(), "keep this");
    assert!(!m.repo().join(".git").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_join_refuses_a_draft_before_spending_the_code() {
    let mut m = Machine::new("solo");
    sk1ll_core::Repo::init(&m.repo()).unwrap();
    m.write("personal/pdf/SKILL.md", "user draft");
    m.start().await;
    let error = m
        .client
        .join(&sk1ll_daemon::client::JoinApiRequest {
            server_url: "http://127.0.0.1:1".into(),
            code: "unused".into(),
            admin_token: None,
            machine_id: None,
            copy_from: None,
            repo: None,
        })
        .await
        .unwrap_err();
    assert_eq!(error.status, Some(409));
    assert!(error.message.contains("draft or local commits"), "{}", error.message);
    assert!(!m.state_path().exists());
    assert_eq!(std::fs::read_to_string(m.repo().join("personal/pdf/SKILL.md")).unwrap(), "user draft");
    m.commit_all().await;
    assert!(m.client.draft().await.unwrap().is_empty());
    let error = m
        .client
        .join(&sk1ll_daemon::client::JoinApiRequest {
            server_url: "http://127.0.0.1:1".into(),
            code: "unused".into(),
            admin_token: None,
            machine_id: None,
            copy_from: None,
            repo: None,
        })
        .await
        .unwrap_err();
    assert_eq!(error.status, Some(409));
    assert!(error.message.contains("draft or local commits"), "{}", error.message);
    assert!(!m.state_path().exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_joins_a_new_fleet_with_the_admin_token() {
    use sk1ll_daemon::client::{DaemonMode, JoinApiRequest};
    let server = Server::start().await;
    let mut m = Machine::new("first");
    m.start().await;
    let request = |admin_token: &str| JoinApiRequest {
        server_url: server.url(),
        code: String::new(),
        admin_token: Some(admin_token.into()),
        machine_id: None,
        copy_from: None,
        repo: None,
    };
    let error = m.client.join(&request("wrong")).await.unwrap_err();
    assert_eq!(error.status, Some(409));
    assert!(error.message.contains("rejected the admin token"), "{}", error.message);
    assert!(!m.state_path().exists());
    let joined = m.client.join(&request(ADMIN)).await.unwrap();
    assert_eq!(joined.machine_id, "first");
    eventually(|| async {
        m.client
            .status()
            .await
            .ok()
            .filter(|s| s.mode == DaemonMode::Joined && s.connection == Connection::Connected)
            .map(drop)
    })
    .await;
    assert!(server.row("first").await.is_some());
    assert!(!std::fs::read_to_string(m.state_path()).unwrap().contains(ADMIN));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_joins_and_restarts_from_an_untouched_init() {
    use sk1ll_daemon::client::DaemonMode;
    let server = Server::start().await;
    let mut m = Machine::new("solo");
    m.start().await;
    m.client.init().await.unwrap();
    let old_manifest = std::fs::read(m.repo().join("sk1ll.toml")).unwrap();
    m.join(&server).await;
    eventually(|| async {
        m.client
            .status()
            .await
            .ok()
            .filter(|s| s.mode == DaemonMode::Joined && s.connection == Connection::Connected)
            .map(drop)
    })
    .await;
    assert!(m.state_path().exists());
    let backup = std::fs::read_dir(m.repo().parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.file_name().unwrap().to_string_lossy().starts_with("sk1ll-before-join-"))
        .unwrap();
    assert_eq!(std::fs::read(backup.join("repo/sk1ll.toml")).unwrap(), old_manifest);
    assert_eq!(m.status().await.server_url, Some(server.url()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_first_pull_blocks_ignored_ancestor_without_changing_head_or_index() {
    use sk1ll_daemon::client::PullResult;
    let mut m = Machine::new("solo");
    m.start().await;
    m.client.init().await.unwrap();
    let peer = m.home.path().join("peer");
    sk1ll_core::Repo::init(&peer).unwrap();
    write(&peer.join("personal/pdf"), "SKILL.md", "remote skill");
    git(&peer, &["add", "-A"]);
    git(&peer, &["commit", "--quiet", "-m", "baseline"]);
    git(&m.repo(), &["remote", "add", "origin", peer.to_str().unwrap()]);
    m.write("personal/pdf", "private notes");
    write(&m.repo().join(".git"), "info/exclude", "personal/pdf\n");
    let head = std::fs::read(m.repo().join(".git/HEAD")).unwrap();
    let index = std::fs::read(m.repo().join(".git/index")).ok();
    let result = m.client.pull().await.unwrap();
    assert!(
        matches!(result, PullResult::Blocked { ref items, .. } if items.contains(&ItemRef::Skill("pdf".into()))),
        "{result:?}"
    );
    assert_eq!(std::fs::read_to_string(m.repo().join("personal/pdf")).unwrap(), "private notes");
    assert_eq!(std::fs::read(m.repo().join(".git/HEAD")).unwrap(), head);
    assert_eq!(std::fs::read(m.repo().join(".git/index")).ok(), index);
    assert_eq!(m.status().await.headline, MachineStatus::Blocked);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_join_revalidates_after_registration_and_keeps_local_authority() {
    use axum::{Json, Router, routing::post};
    use sk1ll_daemon::client::{DaemonMode, JoinApiRequest};
    let mut m = Machine::new("solo");
    m.start().await;
    m.client.init().await.unwrap();
    m.client
        .set_assignment(
            "solo",
            &AssignmentChanges {
                changes: vec![ToggleChange {
                    harness: HARNESS.into(),
                    kind: ToggleKind::NewSkillsDefault,
                    item_id: None,
                    name: None,
                    enabled: Some(false),
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let assignment = std::fs::read(m.cache_path()).unwrap();
    let snapshot = std::fs::read(m.snapshot_path()).ok();
    let entered = std::sync::Arc::new(tokio::sync::Notify::new());
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let app = Router::new().route(
        "/api/join",
        post({
            let entered = entered.clone();
            let release = release.clone();
            move || {
                let entered = entered.clone();
                let release = release.clone();
                async move {
                    entered.notify_one();
                    release.notified().await;
                    Json(sk1ll_proto::JoinResponse {
                        machine_id: "fleet-solo".into(),
                        token: "pending-secret".into(),
                        repo_path: sk1ll_proto::REPO_PATH.into(),
                        assignment: Assignment::default(),
                        revision: 1,
                    })
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = Client::new(m.home.path().join(SOCKET_PATH));
    let joining = tokio::spawn(async move {
        client
            .join(&JoinApiRequest {
                server_url: url,
                code: "once".into(),
                admin_token: None,
                machine_id: None,
                copy_from: None,
                repo: None,
            })
            .await
    });
    entered.notified().await;
    // Registration must not hold the engine lock over the network request.
    tokio::time::timeout(
        Duration::from_secs(2),
        m.client.set_assignment("solo", &AssignmentChanges { changes: vec![], ..Default::default() }),
    )
    .await
    .expect("registration held the engine lock")
    .unwrap();
    m.write("personal/draft/SKILL.md", "saved during registration");
    release.notify_one();
    let error = joining.await.unwrap().unwrap_err();
    assert_eq!(error.status, Some(409), "{}", error.message);
    assert!(error.message.contains("draft"), "{}", error.message);
    assert!(!m.state_path().exists());
    let pending: Vec<_> = std::fs::read_dir(m.state_path().parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.file_name().unwrap().to_string_lossy().starts_with("daemon-pending-join-"))
        .collect();
    assert_eq!(pending.len(), 1);
    let credentials: serde_json::Value = serde_json::from_slice(&std::fs::read(&pending[0]).unwrap()).unwrap();
    assert_eq!(credentials["state"]["token"], "pending-secret");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&pending[0]).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(!m.log().contains("pending-secret"));
    assert_eq!(std::fs::read(m.cache_path()).unwrap(), assignment);
    assert_eq!(std::fs::read(m.snapshot_path()).ok(), snapshot);
    // A restart is delayed by 300ms. Check after it would have run, then prove
    // that local writes are still accepted by the unfrozen standalone engine.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(m.status().await.mode, DaemonMode::Standalone);
    m.client
        .set_assignment(
            "solo",
            &AssignmentChanges {
                changes: vec![ToggleChange {
                    harness: HARNESS.into(),
                    kind: ToggleKind::Skill,
                    item_id: Some(skill_id("draft")),
                    name: Some("draft".into()),
                    enabled: Some(true),
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(m.repo().join("personal/draft/SKILL.md")).unwrap(), "saved during registration");
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_join_snapshot_failure_rolls_back_and_retries_pending_registration() {
    use sk1ll_daemon::client::{DaemonMode, JoinApiRequest};
    let server = Server::start().await;
    let mut m = Machine::new("solo");
    m.start().await;
    m.client.init().await.unwrap();
    m.client
        .set_assignment(
            "solo",
            &AssignmentChanges {
                changes: vec![ToggleChange {
                    harness: HARNESS.into(),
                    kind: ToggleKind::NewSkillsDefault,
                    item_id: None,
                    name: None,
                    enabled: Some(false),
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let manifest = std::fs::read(m.repo().join("sk1ll.toml")).unwrap();
    let assignment = std::fs::read(m.cache_path()).unwrap();
    std::fs::create_dir(m.snapshot_path()).unwrap();
    let req = JoinApiRequest {
        server_url: server.url(),
        code: server.join_code().await,
        admin_token: None,
        machine_id: Some("solo".into()),
        copy_from: None,
        repo: None,
    };
    let error = m.client.join(&req).await.unwrap_err();
    assert!(!m.state_path().exists(), "partial activation: {}", error.message);
    assert_eq!(std::fs::read(m.repo().join("sk1ll.toml")).unwrap(), manifest);
    assert_eq!(std::fs::read(m.cache_path()).unwrap(), assignment);
    assert!(m.snapshot_path().is_dir());
    assert_eq!(m.status().await.mode, DaemonMode::Standalone);
    assert!(sk1ll_core::EngineLock::try_acquire(&m.repo()).unwrap().is_none());
    m.client.set_assignment("solo", &AssignmentChanges { changes: vec![], ..Default::default() }).await.unwrap();
    assert!(error.message.contains("pending"), "{}", error.message);
    std::fs::remove_dir(m.snapshot_path()).unwrap();
    // The code was consumed. Repeating the request must reuse the pending token.
    m.client.join(&req).await.unwrap();
    eventually(|| async { m.client.status().await.ok().filter(|s| s.mode == DaemonMode::Joined).map(drop) }).await;
    assert!(sk1ll_core::EngineLock::try_acquire(&m.repo()).unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn clients_of_another_local_api_version_are_told_to_restart_the_daemon() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut m = solo("solo", "http://127.0.0.1:1", None);
    m.start().await;
    assert_eq!(m.status().await.version, sk1ll_core::VERSION);
    let request = |version: &str| {
        format!("GET /status HTTP/1.1\r\nHost: localhost\r\nsk1ll-api: {version}\r\nConnection: close\r\n\r\n")
    };
    let version = sk1ll_daemon::client::API_VERSION;
    for (version, expected) in [(version, "HTTP/1.1 200"), (version + 1, "HTTP/1.1 426")] {
        let mut stream = tokio::net::UnixStream::connect(m.client.socket()).await.unwrap();
        stream.write_all(request(&version.to_string()).as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with(expected), "{version}: {response}");
        if expected == "HTTP/1.1 426" {
            assert!(response.contains("restart the daemon"), "{response}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bundled_skill_is_created_as_a_personal_draft_skill() {
    use sk1ll_daemon::client::CreateItemRequest;
    let mut m = solo("solo", "http://127.0.0.1:1", None);
    m.start().await;
    let bundled = |kind: &str, name: &str, bundled: &str| CreateItemRequest {
        kind: kind.into(),
        name: name.into(),
        description: None,
        bundled: Some(bundled.into()),
    };
    m.client.create_item(&bundled("skill", "sk1ll", "sk1ll")).await.unwrap();
    let written = std::fs::read_to_string(m.repo().join("personal/sk1ll/SKILL.md")).unwrap();
    assert_eq!(Some(written.as_str()), sk1ll_core::bundled::skill("sk1ll"));
    for (kind, name, from) in [("skill", "other", "pdf"), ("rule", "rule", "sk1ll"), ("skill", "sk1ll", "sk1ll")] {
        let err = m.client.create_item(&bundled(kind, name, from)).await.unwrap_err();
        assert_eq!(err.status, Some(400), "{kind} {name} from {from}: {err:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn skill_file_writes_require_current_personal_or_rule_text() {
    use sk1ll_daemon::client::{ContentQuery, FileQuery, WriteFileRequest};
    use std::os::unix::fs::{PermissionsExt, symlink};

    let server = Server::start().await;
    let mut m = Machine::new("file-writes");
    m.join(&server).await;
    m.start_connected().await;
    let skill = "personal/writing/SKILL.md";
    m.write(skill, "# Original");
    m.write("instructions/style.md", "# Style");
    m.write("imported/other/SKILL.md", "# Import");
    std::fs::set_permissions(m.repo().join(skill), std::fs::Permissions::from_mode(0o640)).unwrap();

    let file = |item: &str, path: &str| FileQuery {
        content: ContentQuery { item: item.into(), hash: None, harness: None },
        path: path.into(),
    };
    let edit = file("skill:writing", "SKILL.md");
    let expected = m.client.skill_file(&edit).await.unwrap().fingerprint.unwrap();
    let mut request = WriteFileRequest { file: edit, content: "# Saved".into(), expected };
    m.client.write_skill_file(&request).await.unwrap();
    assert_eq!(std::fs::read_to_string(m.repo().join(skill)).unwrap(), "# Saved");
    assert_eq!(std::fs::metadata(m.repo().join(skill)).unwrap().permissions().mode() & 0o777, 0o640);
    assert!(m.client.draft().await.unwrap().iter().any(|item| item.item.to_string() == "skill:writing"));

    request.expected = m.client.skill_file(&request.file).await.unwrap().fingerprint.unwrap();
    m.write(skill, "# External edit");
    assert_eq!(m.client.write_skill_file(&request).await.unwrap_err().status, Some(409));
    assert_eq!(std::fs::read_to_string(m.repo().join(skill)).unwrap(), "# External edit");

    request.content = "x".repeat(1024 * 1024 + 1);
    let error = m.client.write_skill_file(&request).await.unwrap_err();
    assert_eq!(error.status, Some(400));
    assert!(error.message.contains("1 MiB"));
    request.content = "# Saved".into();

    for (item, path, message) in [
        ("skill:other", "SKILL.md", "Fork"),
        ("unmanaged:other", "SKILL.md", "Adopt"),
        ("skill:writing", "../SKILL.md", "stay inside"),
        ("rule:style", "other.md", "escapes the Rule"),
    ] {
        request.file = file(item, path);
        let error = m.client.write_skill_file(&request).await.unwrap_err();
        assert_eq!(error.status, Some(400), "{error}");
        assert!(error.message.contains(message), "{error}");
    }
    symlink(m.repo().join("instructions"), m.repo().join("personal/writing/link")).unwrap();
    request.file = file("skill:writing", "link/style.md");
    assert!(m.client.write_skill_file(&request).await.is_err());

    request.file = file("rule:style", "style.md");
    request.expected = m.client.skill_file(&request.file).await.unwrap().fingerprint.unwrap();
    request.content = "---\nharnesses: [false]\n---\n# Invalid".into();
    let error = m.client.write_skill_file(&request).await.unwrap_err();
    assert_eq!(error.status, Some(400));
    assert!(error.message.contains("Invalid Rule frontmatter"));
    request.content = "---\nharnesses: [claude]\n---\n# Valid".into();
    m.client.write_skill_file(&request).await.unwrap();
    let saved = m.client.skill_file(&request.file).await.unwrap().text.unwrap();
    assert!(saved.contains("id:") && saved.contains("harnesses: [claude]") && saved.ends_with("# Valid"), "{saved}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn joined_daemon_proxies_machine_assignment_copy() {
    use sk1ll_proto::{CopyMode, MachineAssignmentCopy};
    let server = Server::start().await;
    let mut target = Machine::new("target");
    target.join(&server).await;
    target.start_connected().await;
    let source = Machine::new("source");
    source.join(&server).await;
    server
        .http
        .put(format!("{}/api/machines/source/assignment", server.url()))
        .bearer_auth(ADMIN)
        .json(&serde_json::json!({"changes":[{"harness":HARNESS,"kind":"new_rules_default","enabled":true}]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    let mut copy = MachineAssignmentCopy {
        source: "source".into(),
        harnesses: None,
        mode: CopyMode::Replace,
        expected: None,
        source_expected: None,
    };
    let preview = target.client.preview_assignment_copy("target", &copy).await.unwrap();
    assert_eq!(preview.count, 1);
    copy.expected = Some(preview.target);
    copy.source_expected = Some(preview.source);
    let update = target.client.copy_machine_assignment("target", &copy).await.unwrap();
    assert_eq!(update.entries.len(), 1);
    assert!(update.assignment.harness(HARNESS).new_rules_default);
    assert_eq!(target.client.copy_machine_assignment("target", &copy).await.unwrap_err().status, Some(409));
    eventually(|| async { (target.status().await.assignment_revision == update.revision).then_some(()) }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reviewed_sync_replans_after_recovering_an_interrupted_import() {
    let mut m = Machine::new("preview").with_env("SK1LL_DEBOUNCE", "60s");
    sk1ll_core::Repo::init(&m.repo()).unwrap();
    m.write("imported/pdf/SKILL.md", "pdf");
    m.start().await;
    eventually(|| async { (m.harness_skill("pdf").as_deref() == Some("pdf")).then_some(()) }).await;
    let imported = m.repo().join("imported");
    std::fs::rename(imported.join("pdf"), imported.join(".sk1ll-stage-old-pdf")).unwrap();

    let preview = m.client.preview_sync().await.unwrap();
    assert!(preview.actions.unwrap().iter().any(|action| action.kind == "remove" && action.name == "pdf"));
    let fresh = m.client.apply_sync_preview(preview.fingerprint.unwrap()).await.unwrap();

    assert!(fresh.fingerprint.is_some(), "recovery changes the plan and requires confirmation");
    assert!(fresh.actions.unwrap().is_empty());
    assert!(imported.join("pdf/SKILL.md").is_file());
    assert_eq!(m.harness_skill("pdf").as_deref(), Some("pdf"));
}

#[tokio::test(flavor = "multi_thread")]
async fn render_and_dry_run_use_the_current_draft_without_writing() {
    use sk1ll_daemon::client::{API_VERSION, SyncResult};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut m = Machine::new("preview").with_env("SK1LL_DEBOUNCE", "60s");
    let repo = sk1ll_core::Repo::init(&m.repo()).unwrap();
    m.start().await;
    let draft = "---\nharnesses: [agent]\n---\n\nKeep functions short.\n";
    m.write("instructions/style.md", draft);
    m.write("instructions/other.md", "---\nharnesses: [elsewhere]\n---\n\nSkip this.\n");
    let path = m.home.path().join("agent/rules/style.md");
    let config = sk1ll_core::MachineConfig::load(&Machine::machine_config(m.home.path())).unwrap();
    let assignment = Assignment::load(&m.cache_path()).unwrap();
    let cli_files = sk1ll_core::sync::render_instructions(&repo, &config.harnesses(), &assignment, HARNESS).unwrap();

    let files = m.client.render_instructions(HARNESS).await.unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, path);
    assert_eq!(files[0].path, cli_files[0].path);
    assert_eq!(files[0].content, cli_files[0].content);
    assert_eq!(files[0].content, "Keep functions short.\n");
    assert!(files[0].differs && files[0].diff.contains("+Keep functions short."));

    let body = r#"{"dry_run":true}"#;
    let request = format!(
        "POST /sync HTTP/1.1\r\nHost: localhost\r\nsk1ll-api: {API_VERSION}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = tokio::net::UnixStream::connect(m.client.socket()).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let plan: SyncResult = serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(plan, m.client.preview_sync().await.unwrap());
    let actions = plan.actions.unwrap();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].kind, "create");
    assert_eq!(actions[0].dest, path);
    assert!(!path.parent().unwrap().exists());
    assert_eq!(std::fs::read_to_string(m.repo().join("instructions/style.md")).unwrap(), draft);

    m.write("instructions/style.md", &draft.replace("short", "readable"));
    let reviewed = plan.fingerprint.unwrap();
    let refused = m
        .client
        .sync(&sk1ll_daemon::client::SyncRequest { fingerprint: Some(reviewed.clone()), ..Default::default() })
        .await
        .unwrap_err();
    assert_eq!(refused.status, Some(409));
    let fresh = m.client.apply_sync_preview(reviewed).await.unwrap();
    assert!(fresh.actions.is_some());
    assert!(!path.exists());
    let applied = m.client.apply_sync_preview(fresh.fingerprint.unwrap()).await.unwrap();
    assert!(applied.errors.is_empty());
    assert!(applied.fingerprint.is_none());
    assert!(std::fs::read_to_string(&path).unwrap().contains("readable"));
    let files = m.client.render_instructions(HARNESS).await.unwrap();
    assert!(!files[0].differs && files[0].diff.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_item_rename_keeps_both_machines_assignments() {
    use sk1ll_daemon::client::RenameItemRequest;

    let mut server = Server::start().await;
    let (a, b, _) = fleet_of_two(&server).await;
    assign(&a, "alpha", vec![toggle("x", true)]).await;
    assign(&b, "beta", vec![toggle("x", true)]).await;
    let before_a = a.client.assignment("alpha").await.unwrap();
    let before_b = b.client.assignment("beta").await.unwrap();
    server.stop();
    eventually(|| async { (a.status().await.connection == Connection::Offline).then_some(()) }).await;
    let renamed = a
        .client
        .rename_item(&RenameItemRequest { kind: "skill".into(), name: "x".into(), to: "new-x".into() })
        .await
        .unwrap();
    assert_eq!(renamed.item, ItemRef::Skill("new-x".into()));
    assert_eq!(
        a.harness_skill("new-x").as_deref(),
        Some(
            "x v1
"
        )
    );
    server.launch().await;
    eventually(|| async { (a.status().await.connection == Connection::Connected).then_some(()) }).await;
    assert!(a.commit_all().await.pushed);
    eventually(|| async { b.harness_skill("new-x").is_some().then_some(()) }).await;
    assert_eq!(a.client.assignment("alpha").await.unwrap(), before_a);
    assert_eq!(b.client.assignment("beta").await.unwrap(), before_b);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_id_is_inert_and_harness_rename_moves_explicit_toggles() {
    let server = Server::start().await;
    let (a, _b, _) = fleet_of_two(&server).await;
    let mut unknown = toggle("x", true);
    unknown.item_id = Some("absent-id".parse().unwrap());
    let update =
        a.client.set_assignment("alpha", &AssignmentChanges { changes: vec![unknown], expected: None }).await.unwrap();
    assert_eq!(update.entries[0].item_id.as_ref().unwrap().to_string(), "absent-id");
    assert!(a.harness_skill("x").is_none());
    assign(&a, "alpha", vec![toggle("x", true)]).await;

    let update = a
        .client
        .rename_harness(&sk1ll_daemon::client::HarnessRenameRequest {
            from: HARNESS.into(),
            to: "new-agent".into(),
            replace: false,
        })
        .await
        .unwrap();
    let update = update.update;
    assert!(!update.assignment.harnesses.contains_key(HARNESS));
    assert!(update.assignment.skill_enabled("new-agent", &skill_id("x")));
    assert!(!update.assignment.harness("new-agent").new_skills_default);
    assert!(update.assignment.harness("new-agent").skills.contains_key(&"absent-id".parse().unwrap()));
    assert_eq!(
        a.harness_skill("x").as_deref(),
        Some(
            "x v1
"
        )
    );
    assert!(a.client.harnesses().await.unwrap().iter().any(|h| h.name == "new-agent"));
    assert_eq!(a.client.assignment("alpha").await.unwrap().revision, update.revision);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_fleet_assignment_sends_status_and_caches_the_result() {
    let server = Server::start().await;
    let (mut a, _b, _) = fleet_of_two(&server).await;
    a.stop();
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let state = a.state();
    let mut request = state.ws_url().into_client_request().unwrap();
    request.headers_mut().insert("authorization", format!("Bearer {}", state.token).parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let item = ItemRef::Skill("x".into());
    let saved = sk1ll_proto::MachineReport {
        seq: 9,
        baseline: Some("blocked-baseline".into()),
        blocked: true,
        behind: true,
        clashes: vec![item.clone()],
        drafts: vec![item.clone()],
        harnesses: vec![HARNESS.into()],
        conflicts: vec![sk1ll_proto::ConflictRef { item, harness: HARNESS.into() }],
        inventory_complete: true,
        ..Default::default()
    };
    ws.send(Message::text(serde_json::to_string(&DaemonMessage::Status(saved.clone())).unwrap())).await.unwrap();
    eventually(|| async { (server.row("alpha").await?.report? == saved).then_some(()) }).await;
    drop(ws);

    let cfg = sk1ll_daemon::Config {
        state: a.state_path(),
        machine_config: Machine::machine_config(a.home.path()),
        ..sk1ll_daemon::Config::new().unwrap()
    };
    let lock = sk1ll_core::EngineLock::acquire(&a.repo()).unwrap();
    let update = sk1ll_daemon::write_assignment_direct(
        &cfg,
        &lock,
        sk1ll_proto::AssignmentWrite::Changes(AssignmentChanges { changes: vec![toggle("x", true)], expected: None }),
    )
    .await
    .unwrap();
    let report = server.row("alpha").await.unwrap().report.unwrap();
    assert!(report.items.iter().any(|item| item.id == skill_id("x")));
    assert_eq!(sk1ll_proto::MachineReport { items: vec![], ..report }, saved);
    let snapshot = a.snapshot().unwrap();
    assert_eq!(snapshot.revision, update.revision);
    assert!(snapshot.assignment.skill_enabled(HARNESS, &skill_id("x")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recreated_personal_skill_gets_a_new_assignment_identity() {
    use sk1ll_daemon::client::{CreateItemRequest, DeleteItemRequest};
    let mut m = Machine::new("solo");
    sk1ll_core::Repo::init(&m.repo()).unwrap();
    m.start().await;
    let create = CreateItemRequest { kind: "skill".into(), name: "pdf".into(), description: None, bundled: None };
    m.client.create_item(&create).await.unwrap();
    let id = || sk1ll_core::Repo::open(m.repo()).unwrap().skills().unwrap()[0].id.clone().unwrap();
    let old = id();
    let mut off = toggle("pdf", false);
    off.item_id = Some(old.clone());
    assign(&m, "solo", vec![off]).await;
    assert!(m.harness_skill("pdf").is_none());
    m.client.delete_item(&DeleteItemRequest { kind: "skill".into(), name: "pdf".into() }).await.unwrap();
    m.client.create_item(&create).await.unwrap();
    assert_ne!(id(), old);
    assert!(m.harness_skill("pdf").is_some());
    assert!(!m.client.assignment("solo").await.unwrap().assignment.skill_enabled(HARNESS, &old));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnect_replays_harness_rename_before_first_status() {
    use sk1ll_core::harness_rename::{RenameIntent, RenameRequest};
    use sk1ll_proto::{AssignmentOutcome, AssignmentUpdate, AssignmentWrite};

    let server = FakeServer::new().await;
    let mut snap = snapshot(1, &[]);
    let id: sk1ll_core::ItemId = "persisted-x".parse().unwrap();
    snap.assignment.set_skill(HARNESS, &id, true);
    let mut m = solo("solo", &server.url(), Some(snap.clone()));
    m.write("personal/x/.sk1ll-id", "persisted-x");
    let cfg = Machine::machine_config(m.home.path());
    let authority = sk1ll_core::hash::hash_str(&format!("{}\0solo\0tok", server.url()));
    let intent = RenameIntent::prepare(
        &cfg,
        &m.repo(),
        RenameRequest { from: HARNESS.into(), to: "renamed".into(), replace: false },
        &snap.assignment,
        Some(&authority),
    )
    .unwrap();
    let mut after = snap.assignment;
    after.rename_harness(HARNESS, "renamed");

    m.start().await;
    let (mut socket, _) = server.accept().await;
    let DaemonMessage::SetAssignment { write: AssignmentWrite::RenameHarness(first), .. } =
        next_message(&mut socket).await
    else {
        panic!("send the rename before status")
    };
    assert_eq!(first.operation_id, intent.operation_id);
    assert!(sk1ll_core::MachineConfig::load(&cfg).unwrap().harnesses().contains_key("renamed"));
    drop(socket);

    let (mut socket, _) = server.accept().await;
    let DaemonMessage::SetAssignment { req, write: AssignmentWrite::RenameHarness(replayed) } =
        next_message(&mut socket).await
    else {
        panic!("replay the rename before status")
    };
    assert_eq!(replayed, first);
    let reply = ServerMessage::AssignmentResult {
        req,
        seq: 0,
        result: AssignmentOutcome::Applied(AssignmentUpdate {
            assignment: after,
            revision: 2,
            entries: vec![],
            errors: vec![],
        }),
    };
    socket.send(Message::text(serde_json::to_string(&reply).unwrap())).await.unwrap();
    let DaemonMessage::Status(report) = next_message(&mut socket).await else { panic!("expected status after replay") };
    assert_eq!(report.harnesses, ["renamed"]);
    assert!(!RenameIntent::exists(&cfg).unwrap());
    assert!(m.snapshot().unwrap().assignment.skill_enabled("renamed", &id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_rule_reports_partial_inventory_and_still_pulls() {
    let server = Server::start().await;
    let (mut a, b, _) = fleet_of_two(&server).await;
    a.stop();
    a.write("instructions/unreadable.md", "---\nid: [bad]\n---\nrule\n");
    a.write("personal/ok/SKILL.md", "valid skill");
    a.write("personal/ok/.sk1ll-id", "ok-id");
    a.start_connected().await;
    let first = eventually(|| async {
        let report = server.row("alpha").await?.report?;
        report.inventory_error.as_ref()?.contains("unreadable.md").then_some(report)
    })
    .await;
    assert!(!first.inventory_complete);
    assert!(first.items.iter().any(|item| item.id.to_string() == "ok-id"));
    let response = server
        .http
        .post(format!("{}/api/assignment/orphans/prune", server.url()))
        .bearer_auth(ADMIN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    b.write("notes", "a change to pull");
    let pushed = b.commit_all().await;
    assert!(pushed.pushed);
    eventually(|| async {
        let status = a.status().await;
        (status.connection == Connection::Connected && status.report.baseline.as_deref() == Some(&pushed.sha))
            .then_some(())
    })
    .await;
    assert_eq!(std::fs::read_to_string(a.repo().join("notes")).unwrap(), "a change to pull");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_rename_protects_destination_even_with_an_old_cache() {
    use sk1ll_core::harness_rename::RenameIntent;
    use sk1ll_daemon::client::HarnessRenameRequest;

    let server = Server::start().await;
    let (mut a, _b, _) = fleet_of_two(&server).await;
    a.stop();
    assert!(!a.snapshot().unwrap().assignment.harnesses.contains_key("already-stored"));
    let saved = server
        .http
        .put(format!("{}/api/machines/alpha/assignment", server.url()))
        .bearer_auth(ADMIN)
        .json(&serde_json::json!({"changes":[{
            "harness":"already-stored","kind":"skill","item_id":"absent","enabled":true
        }]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<sk1ll_proto::AssignmentUpdate>()
        .await
        .unwrap();
    let cfg = sk1ll_daemon::Config {
        state: a.state_path(),
        machine_config: Machine::machine_config(a.home.path()),
        ..sk1ll_daemon::Config::new().unwrap()
    };
    let before = std::fs::read(&cfg.machine_config).unwrap();
    let lock = sk1ll_core::EngineLock::acquire(&a.repo()).unwrap();
    let error = sk1ll_daemon::rename_harness_direct(
        &cfg,
        &a.state(),
        &lock,
        HarnessRenameRequest { from: HARNESS.into(), to: "already-stored".into(), replace: false },
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("already has an Assignment"), "{error:#}");
    assert_eq!(std::fs::read(&cfg.machine_config).unwrap(), before);
    assert!(!RenameIntent::exists(&cfg.machine_config).unwrap());
    let snapshot: AssignmentSnapshot = server.get("/machines/alpha/assignment").await;
    assert_eq!(snapshot.revision, saved.revision);
    assert_eq!(snapshot.assignment, saved.assignment);
}
