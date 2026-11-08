//! End-to-end tests: a real hub (on its own runtime, so it can be stopped
//! and restarted on the same port) and one `sk1ll-daemon` process per
//! machine, each with its own HOME and XDG dirs and no global git config,
//! driven through the daemon's local API.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use sk1ll_daemon::DaemonState;
use sk1ll_daemon::client::{
    AddImportRequest, Client, Connection, Event, PromoteRequest, PromoteResponse, ResetRequest, ResolveImportRequest,
    StatusView,
};
use sk1ll_proto::{
    Assignment, AssignmentChanges, AssignmentSnapshot, DaemonMessage, FleetView, HarnessAssignment, HubMessage,
    ItemRef, LogCause, LogEntry, MachineStatus, ToggleChange, ToggleKind,
};
use tokio_tungstenite::tungstenite::Message;

const ADMIN: &str = "admin-secret";
const HARNESS: &str = "agent";

// ---------------------------------------------------------------------------
// Hub

struct Hub {
    port: u16,
    data: PathBuf,
    _dir: tempfile::TempDir,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    http: reqwest::Client,
}

impl Hub {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let mut hub = Self { port: 0, data, _dir: dir, stop: None, thread: None, http: reqwest::Client::new() };
        hub.launch().await;
        hub
    }

    /// Serves on `self.port` (0: any) on a runtime of its own; dropping that
    /// runtime closes every connection, like a crashed hub.
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
                let mut config = sk1ll_hub::Config::new(data);
                config.admin_token = Some(ADMIN.into());
                tokio::select! {
                    r = sk1ll_hub::serve(config, listener) => r.unwrap(),
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

impl Drop for Hub {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// Machines

struct Machine {
    id: String,
    home: tempfile::TempDir,
    child: Option<Child>,
    client: Client,
    /// Extra environment for the daemon (overrides the defaults).
    env: Vec<(String, String)>,
}

impl Machine {
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
        write(&h.join(".config/sk1ll"), "machine.toml", &cfg);
        let client = Client::new(h.join("run/sk1ll/daemon.sock"));
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
        self.home.path().join("skills")
    }

    fn skills(&self) -> PathBuf {
        self.home.path().join("agent/skills")
    }

    fn state_path(&self) -> PathBuf {
        self.home.path().join(".config/sk1ll/daemon.json")
    }

    fn state(&self) -> DaemonState {
        DaemonState::load(&self.state_path()).unwrap().expect("joined")
    }

    fn snapshot_path(&self) -> PathBuf {
        self.home.path().join(".config/sk1ll/daemon-assignment.json")
    }

    fn outbox_path(&self) -> PathBuf {
        self.home.path().join(".config/sk1ll/daemon-outbox.json")
    }

    fn cache_path(&self) -> PathBuf {
        self.home.path().join(".cache/sk1ll/assignment.json")
    }

    fn snapshot(&self) -> Option<AssignmentSnapshot> {
        let text = std::fs::read_to_string(self.snapshot_path()).ok()?;
        Some(serde_json::from_str(&text).unwrap())
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.home.path().join("daemon.log")).unwrap_or_default()
    }

    /// Joins with a fresh code; returns what the command printed.
    async fn join(&self, hub: &Hub) -> String {
        let code = hub.join_code().await;
        let mut cmd = self.command();
        cmd.args(["join", &hub.url(), &code, "--id", &self.id, "--repo"]).arg(self.repo());
        let out = tokio::task::spawn_blocking(move || cmd.output().unwrap()).await.unwrap();
        assert!(out.status.success(), "join: {}", String::from_utf8_lossy(&out.stderr));
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
    }

    fn spawn(&mut self) {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.home.path().join("daemon.log"))
            .unwrap();
        let child = self.command().stdout(Stdio::null()).stderr(log).spawn().unwrap();
        self.child = Some(child);
    }

    async fn start(&mut self) {
        self.spawn();
        eventually(|| async { self.client.status().await.ok().map(drop) }).await;
    }

    /// For real hubs: API readiness alone does not mean a promote can push.
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

    fn write(&self, rel: &str, body: &str) {
        let path = self.repo().join(rel);
        write(path.parent().unwrap(), path.file_name().unwrap().to_str().unwrap(), body);
    }

    fn harness_skill(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.skills().join(name).join("SKILL.md")).ok()
    }

    /// Promotes every draft item.
    async fn promote_all(&self) -> PromoteResponse {
        let items = self.client.draft().await.unwrap().into_iter().map(|d| d.item.to_string()).collect();
        self.client.promote(&PromoteRequest { items, ..Default::default() }).await.unwrap()
    }

    async fn promote(&self, items: &[&str]) -> PromoteResponse {
        let items = items.iter().map(|s| s.to_string()).collect();
        self.client.promote(&PromoteRequest { items, ..Default::default() }).await.unwrap()
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

/// A hand-driven hub endpoint: the test decides what each connection gets.
struct FakeHub {
    listener: tokio::net::TcpListener,
}

type FakeWs = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

impl FakeHub {
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
    let skills: BTreeMap<String, bool> = ["x", "y", "z"].iter().map(|s| (s.to_string(), on.contains(s))).collect();
    let h = HarnessAssignment { new_skills_default: false, new_fragments_default: false, skills, ..Default::default() };
    let assignment = Assignment { harnesses: [(HARNESS.to_string(), h)].into() };
    AssignmentSnapshot { revision, assignment }
}

/// A machine whose repo has skills x, y, z (pushed to a local bare origin)
/// and whose hub is `hub_url`, with `snap` stored as its assignment.
fn solo(id: &str, hub_url: &str, snap: Option<AssignmentSnapshot>) -> Machine {
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
    DaemonState { machine_id: id.into(), hub_url: hub_url.into(), token: "tok".into(), repo, revoked: false }
        .save(&m.state_path())
        .unwrap();
    if let Some(snap) = snap {
        write(m.snapshot_path().parent().unwrap(), "daemon-assignment.json", &serde_json::to_string(&snap).unwrap());
    }
    m
}

fn toggle(name: &str, enabled: bool) -> ToggleChange {
    ToggleChange { harness: HARNESS.into(), kind: ToggleKind::Skill, name: Some(name.into()), enabled: Some(enabled) }
}

async fn assign(via: &Machine, machine: &str, changes: Vec<ToggleChange>) {
    via.client.set_assignment(machine, &AssignmentChanges { changes }).await.unwrap();
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

/// `alpha` joins the fresh hub and promotes the new repo with skill `x`;
/// then `beta` joins and clones it. Returns both and the baseline.
async fn fleet_of_two(hub: &Hub) -> (Machine, Machine, String) {
    let mut a = Machine::new("alpha");
    a.join(hub).await;
    a.start_connected().await;
    a.write("personal/x/SKILL.md", "x v1\n");
    let p = a.promote_all().await;
    assert!(p.pushed, "{p:?}");
    assert_eq!(hub.head().as_deref(), Some(p.sha.as_str()));

    let mut b = Machine::new("beta");
    b.join(hub).await;
    b.start_connected().await;
    // Removing a machine before the hub has registered its session is an
    // offline removal: a later 401 deliberately does not revoke the daemon.
    eventually(|| async {
        let row = hub.row("beta").await?;
        (row.connected && row.report?.baseline.as_deref() == Some(p.sha.as_str())).then_some(())
    }).await;
    (a, b, p.sha)
}

// ---------------------------------------------------------------------------
// Tests

#[tokio::test(flavor = "multi_thread")]
async fn join_promote_push_and_nudge_another_machine() {
    let hub = Hub::start().await;
    let (a, b, _) = fleet_of_two(&hub).await;

    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&a.state_path()), 0o600, "the token file is private");
    assert_eq!(mode(a.client.socket()), 0o600, "the API socket is private");
    let state = b.state();
    assert_eq!((state.machine_id.as_str(), state.hub_url.as_str()), ("beta", hub.url().as_str()));
    assert!(b.repo().join("personal/x/SKILL.md").is_file(), "beta cloned the baseline");

    let mut events = b.client.events().await.unwrap();
    a.write("personal/y/SKILL.md", "y v1\n");
    let p = a.promote(&["y"]).await;
    assert!(p.pushed);
    assert_eq!(p.message, "promote y from alpha");
    assert_eq!(git(&a.repo(), &["log", "-1", "--format=%an <%ae>"]), "alpha <sk1ll@alpha>");

    // The hub nudges beta, which pulls.
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

    eventually(|| async { (hub.row("beta").await?.status == MachineStatus::InSync).then_some(()) }).await;
    let fleet = a.client.fleet().await.unwrap();
    assert_eq!(fleet.baseline.as_deref(), Some(p.sha.as_str()));
    assert_eq!(fleet.machines.len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn assignment_change_reaches_another_harness_without_git() {
    let hub = Hub::start().await;
    let (a, b, base) = fleet_of_two(&hub).await;
    let reflog = git(&b.repo(), &["reflog"]);

    assign(&a, "beta", vec![toggle("x", true)]).await;
    eventually(|| async { (b.harness_skill("x").as_deref() == Some("x v1\n")).then_some(()) }).await;
    assign(&a, "beta", vec![toggle("x", false)]).await;
    eventually(|| async { (!b.skills().join("x").exists()).then_some(()) }).await;

    let snapshot = a.client.assignment("beta").await.unwrap();
    assert!(snapshot.revision > 0);
    assert_eq!(snapshot, hub.get::<AssignmentSnapshot>("/machines/beta/assignment").await);
    assert!(!snapshot.assignment.skill_enabled(HARNESS, "x"));
    assert_eq!(b.head(), base);
    assert_eq!(git(&b.repo(), &["reflog"]), reflog, "no git operation moved anything");
    let cached = Assignment::load(&b.home.path().join(".cache/sk1ll/assignment.json")).unwrap();
    assert_eq!(cached.harness(HARNESS).skills.get("x"), Some(&false), "the assignment is cached");
}

#[tokio::test(flavor = "multi_thread")]
async fn offline_promotes_are_queued_and_pushed_on_reconnect() {
    let mut hub = Hub::start().await;
    let mut a = Machine::new("alpha");
    a.join(&hub).await;
    a.start_connected().await;
    a.write("personal/v/SKILL.md", "v\n");
    let first = a.promote_all().await;
    assert!(first.pushed);

    hub.stop();
    eventually(|| async { (a.status().await.connection == Connection::Offline).then_some(()) }).await;
    let err = a.client.fleet().await.unwrap_err();
    assert_eq!(err.status, Some(503), "{err}");

    a.write("personal/z/SKILL.md", "z\n");
    let p = a.promote(&["z"]).await;
    assert!(!p.pushed, "nothing to push to while offline");
    let s = a.status().await;
    assert_eq!(s.report.unpushed, [ItemRef::Skill("z".into())]);
    assert_eq!(s.headline, MachineStatus::Offline);
    assert_eq!(hub.head().as_deref(), Some(first.sha.as_str()));

    hub.launch().await;
    eventually(|| async { (hub.head().as_deref() == Some(p.sha.as_str())).then_some(()) }).await;
    eventually(|| async {
        let s = a.status().await;
        (s.connection == Connection::Connected && s.report.unpushed.is_empty()).then_some(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn blocked_pull_reports_clashes_and_keeps_files() {
    let hub = Hub::start().await;
    let (a, b, base) = fleet_of_two(&hub).await;

    b.write("personal/x/SKILL.md", "x by beta\n");
    eventually(|| async { hub.row("beta").await?.report?.drafts.contains(&ItemRef::Skill("x".into())).then_some(()) })
        .await;
    a.write("personal/x/SKILL.md", "x by alpha\n");
    let p = a.promote(&["x"]).await;
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
    eventually(|| async { (hub.row("beta").await?.status == MachineStatus::Blocked).then_some(()) }).await;

    // Resetting the clashing item is a draft change: the pull is retried.
    b.client.reset(&ResetRequest { items: vec!["x".into()], all: false }).await.unwrap();
    eventually(|| async {
        let s = b.status().await;
        (!s.report.blocked && s.report.baseline.as_deref() == Some(p.sha.as_str())).then_some(())
    })
    .await;
    assert_eq!(std::fs::read_to_string(b.repo().join("personal/x/SKILL.md")).unwrap(), "x by alpha\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn reset_sends_draft_discarded() {
    let hub = Hub::start().await;
    let (a, b, _) = fleet_of_two(&hub).await;

    b.write("personal/w/SKILL.md", "work in progress\n");
    eventually(|| async { hub.row("beta").await?.report?.drafts.contains(&ItemRef::Skill("w".into())).then_some(()) })
        .await;
    // A draft-only skill can be assigned on its own machine, and is live there.
    assign(&a, "beta", vec![toggle("w", true)]).await;
    eventually(|| async { b.harness_skill("w").map(drop) }).await;

    let reset = b.client.reset(&ResetRequest { items: vec!["w".into()], all: false }).await.unwrap();
    assert_eq!(reset.discarded, [ItemRef::Skill("w".into())]);
    assert!(!b.repo().join("personal/w").exists());
    eventually(|| async { (!b.skills().join("w").exists()).then_some(()) }).await;

    // The hub dropped the toggle because of `draft_discarded`, and logged it.
    let entry = eventually(|| async {
        let log: Vec<LogEntry> = hub.get("/log?machine=beta").await;
        log.into_iter().find(|e| e.cause == LogCause::ItemRemoved && e.name.as_deref() == Some("w"))
    })
    .await;
    assert_eq!((entry.before, entry.after), (Some(true), None));
    let assignment = hub.get::<AssignmentSnapshot>("/machines/beta/assignment").await.assignment;
    assert!(!assignment.harness(HARNESS).skills.contains_key("w"));
}

#[tokio::test(flavor = "multi_thread")]
async fn revoked_machine_stops_managing_and_keeps_files() {
    let hub = Hub::start().await;
    let (a, b, _) = fleet_of_two(&hub).await;
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
    let err = b.client.promote(&PromoteRequest { items: vec!["x".into()], ..Default::default() }).await.unwrap_err();
    assert_eq!(err.status, Some(409), "{err}");
    assert_eq!(b.client.fleet().await.unwrap_err().status, Some(503));
    assert!(hub.row("beta").await.is_none());
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

/// A fake hub sends assignments out of order; an older revision is ignored.
#[tokio::test(flavor = "multi_thread")]
async fn stale_assignment_revision_is_ignored() {
    let hub = FakeHub::new().await;
    let mut m = solo("solo", &hub.url(), Some(snapshot(1, &[])));
    m.start().await;

    let (ws, auth) = hub.accept().await;
    assert_eq!(auth.as_deref(), Some("Bearer tok"));
    // Keep reading what the daemon sends.
    let (mut tx, mut rx) = ws.split();
    tokio::spawn(async move { while rx.next().await.is_some() {} });
    let send = |m: HubMessage| Message::text(serde_json::to_string(&m).unwrap());

    tx.send(send(HubMessage::Assignment(snapshot(5, &["x"])))).await.unwrap();
    eventually(|| async { m.harness_skill("x").map(drop) }).await;

    tx.send(send(HubMessage::Assignment(snapshot(3, &["y"])))).await.unwrap();
    // The imports event is an ordered inbox barrier. A nudge's baseline can
    // be replaced by a pull before a status poll sees it.
    let mut events = m.client.events().await.unwrap();
    tx.send(send(HubMessage::ImportsChanged { updates: Vec::new() })).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.next().await {
                Some(Ok(Event::Imports { .. })) => break,
                Some(Ok(_)) => {}
                other => panic!("event stream ended: {other:?}"),
            }
        }
    }).await.expect("the inbox barrier");
    assert_eq!(m.status().await.assignment_revision, 5);
    assert!(m.harness_skill("x").is_some() && m.harness_skill("y").is_none());
    assert_eq!(m.snapshot().unwrap(), snapshot(5, &["x"]), "stored with its revision");

    tx.send(send(HubMessage::Assignment(snapshot(6, &["z"])))).await.unwrap();
    eventually(|| async { (m.harness_skill("z").is_some() && m.harness_skill("x").is_none()).then_some(()) }).await;
    assert_eq!(m.status().await.assignment_revision, 6);
}

// ---------------------------------------------------------------------------
// Review findings

/// #1: the watcher is live from startup, so a repo edit reaches the harness
/// with no other trigger (no API call, no hub message), even when the
/// harness's skills dir only appears later.
#[tokio::test(flavor = "multi_thread")]
async fn repo_edits_reach_the_harness_without_another_trigger() {
    let hub = Hub::start().await;
    let (a, b, _) = fleet_of_two(&hub).await;
    assign(&a, "beta", vec![toggle("x", true)]).await;
    eventually(|| async { (b.harness_skill("x").as_deref() == Some("x v1\n")).then_some(()) }).await;

    b.write("personal/x/SKILL.md", "x edited on beta\n");
    eventually(|| async { (b.harness_skill("x").as_deref() == Some("x edited on beta\n")).then_some(()) }).await;
}

/// API readiness must include watching local edits, even if the first hub
/// handshake is still waiting for a response and cannot trigger a refresh.
#[tokio::test(flavor = "multi_thread")]
async fn repo_edits_before_the_first_hub_connection_reach_the_harness() {
    let hub = FakeHub::new().await;
    let mut m = solo("solo", &hub.url(), Some(snapshot(1, &["x"])));
    m.start().await;
    m.write("personal/x/SKILL.md", "edited before connecting\n");
    eventually(|| async {
        (m.harness_skill("x").as_deref() == Some("edited before connecting\n")).then_some(())
    }).await;
    assert_ne!(m.status().await.connection, Connection::Connected);
}

/// #10: edits arriving faster than the debounce still sync within the
/// maximum debounce.
#[tokio::test(flavor = "multi_thread")]
async fn a_stream_of_edits_cannot_postpone_the_sync() {
    let hub = FakeHub::new().await;
    let mut m = solo("solo", &hub.url(), Some(snapshot(1, &["x"])))
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

/// #2: a 401 (here from something in front of the hub) is a recoverable
/// state: the machine keeps being managed, retries, and is never revoked.
#[tokio::test(flavor = "multi_thread")]
async fn a_401_is_not_a_revocation() {
    let hub = FakeHub::new().await;
    let mut m = solo("solo", &hub.url(), Some(snapshot(1, &["x"])));
    m.start().await;

    hub.refuse_401().await;
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

    // Retries, and recovers once the hub accepts the token.
    hub.refuse_401().await;
    let (_ws, _) = hub.accept().await;
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
    let hub = Hub::start().await;
    let mut a = Machine::new("alpha");
    let a_join = a.join(&hub).await;
    a.start_connected().await;
    a.write("personal/x/SKILL.md", "x\n");
    assert!(a.promote_all().await.pushed, "push works");

    let mut b = Machine::new("beta");
    let b_join = b.join(&hub).await;
    b.start().await;
    assert!(b.repo().join("personal/x/SKILL.md").is_file(), "clone works");
    a.write("personal/y/SKILL.md", "y\n");
    let p = a.promote(&["y"]).await;
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
    let old = format!("http://beta:{token}@127.0.0.1:{}/git/repo.git", hub.port);
    git(&b.repo(), &["remote", "set-url", "origin", &old]);
    b.start().await;
    assert_eq!(git(&b.repo(), &["remote", "get-url", "origin"]), format!("{}/git/repo.git", hub.url()));
    eventually(|| async { (b.status().await.connection == Connection::Connected).then_some(()) }).await;
    b.write("personal/z/SKILL.md", "z\n");
    assert!(b.promote(&["z"]).await.pushed, "push works after the migration");
    assert!(!b.log().contains(&token));
}

/// #4: the assignment is stored with its revision in the state dir, so
/// losing the cache can't enable anything; without any stored snapshot the
/// harnesses wait for the hub's, which is taken even at the same revision.
#[tokio::test(flavor = "multi_thread")]
async fn losing_the_assignment_cache_keeps_disabled_skills_off() {
    let mut hub = Hub::start().await;
    let (a, mut b, _) = fleet_of_two(&hub).await;
    assert_eq!(b.harness_skill("x"), None, "a new machine starts with everything off");
    let revision = b.status().await.assignment_revision;
    assert_eq!(b.snapshot().unwrap().revision, revision);

    b.stop();
    std::fs::remove_dir_all(b.home.path().join(".cache")).unwrap();
    b.start().await;
    eventually(|| async { (b.status().await.connection == Connection::Connected).then_some(()) }).await;
    assert_eq!(b.harness_skill("x"), None, "the disabled skill stays off");
    assert!(b.cache_path().is_file(), "the cache is mirrored again");

    b.stop();
    hub.stop();
    std::fs::remove_file(b.snapshot_path()).unwrap();
    std::fs::remove_dir_all(b.home.path().join(".cache")).unwrap();
    b.start().await;
    let s = b.status().await;
    assert!(s.awaiting_assignment, "{s:?}");
    assert_eq!(b.harness_skill("x"), None, "nothing is managed without an assignment");

    hub.launch().await;
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
    let hub = Hub::start().await;
    let mut a = Machine::new("alpha");
    a.join(&hub).await;
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
    let hub = FakeHub::new().await;
    // Neither the watcher nor the reconciliation refreshes during the test.
    let mut m = solo("solo", &hub.url(), Some(snapshot(1, &[])))
        .with_env("SK1LL_DEBOUNCE", "60s")
        .with_env("SK1LL_DEBOUNCE_MAX", "60s")
        .with_env("SK1LL_RECONCILE_INTERVAL", "1h");
    m.start().await;
    let (mut ws, _) = hub.accept().await;
    let DaemonMessage::Status(first) = next_message(&mut ws).await else { panic!("expected a status") };
    assert!(first.drafts.is_empty(), "{first:?}");
    // Let the catch-up pull after connecting finish.
    tokio::time::sleep(Duration::from_millis(500)).await;
    drop(ws);

    m.write("personal/new/SKILL.md", "new\n");
    let (mut ws, _) = hub.accept().await;
    let DaemonMessage::Status(report) = next_message(&mut ws).await else { panic!("status comes first") };
    assert_eq!(report.drafts, [ItemRef::Skill("new".into())]);
}

/// #8: a Reset made while offline is reported to the hub even if the daemon
/// restarts before it reconnects.
#[tokio::test(flavor = "multi_thread")]
async fn discarded_drafts_are_reported_after_a_restart() {
    let mut hub = Hub::start().await;
    let (a, mut b, _) = fleet_of_two(&hub).await;
    b.write("personal/w/SKILL.md", "work in progress\n");
    eventually(|| async { hub.row("beta").await?.report?.drafts.contains(&ItemRef::Skill("w".into())).then_some(()) })
        .await;
    assign(&a, "beta", vec![toggle("w", true)]).await;
    eventually(|| async { b.harness_skill("w").map(drop) }).await;

    hub.stop();
    eventually(|| async { (b.status().await.connection == Connection::Offline).then_some(()) }).await;
    let reset = b.client.reset(&ResetRequest { items: vec!["w".into()], all: false }).await.unwrap();
    assert_eq!(reset.discarded, [ItemRef::Skill("w".into())]);
    assert_eq!(b.status().await.pending_discards, 1);
    assert!(b.outbox_path().is_file(), "queued durably");
    b.stop();

    hub.launch().await;
    b.start().await;
    eventually(|| async {
        let log: Vec<LogEntry> = hub.get("/log?machine=beta").await;
        log.into_iter().find(|e| e.cause == LogCause::ItemRemoved && e.name.as_deref() == Some("w"))
    })
    .await;
    eventually(|| async { (!b.outbox_path().exists()).then_some(()) }).await;
    assert_eq!(b.status().await.pending_discards, 0);
    let assignment = hub.get::<AssignmentSnapshot>("/machines/beta/assignment").await.assignment;
    assert!(!assignment.harness(HARNESS).skills.contains_key("w"));
}

/// #9: `join` while the daemon runs goes through it; it comes back
/// connected and managing with the new credentials.
#[tokio::test(flavor = "multi_thread")]
async fn joining_again_while_the_daemon_runs_resumes_management() {
    let hub = Hub::start().await;
    let (a, b, _) = fleet_of_two(&hub).await;
    let old_token = b.state().token;
    a.client.remove_machine("beta").await.unwrap();
    eventually(|| async { b.status().await.revoked.then_some(()) }).await;

    let out = b.join(&hub).await;
    assert!(out.contains("restarts"), "went through the daemon: {out}");
    let s = eventually(|| async {
        let s = b.client.status().await.ok()?;
        (s.connection == Connection::Connected).then_some(s)
    })
    .await;
    assert!(!s.revoked);
    let state = b.state();
    assert!(!state.revoked && state.token != old_token);
    eventually(|| async { hub.row("beta").await.filter(|r| r.connected).map(drop) }).await;

    assign(&a, "beta", vec![toggle("x", true)]).await;
    eventually(|| async { b.harness_skill("x").map(drop) }).await;
    b.write("personal/q/SKILL.md", "q\n");
    assert!(b.promote(&["q"]).await.pushed, "git works with the new token");
    assert!(!b.log().contains(&state.token));
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_refresh_sends_no_status_or_discards() {
    let hub = FakeHub::new().await;
    let mut m = solo("solo", &hub.url(), Some(snapshot(1, &[])))
        .with_env("SK1LL_RECONCILE_INTERVAL", "1h");
    write(m.outbox_path().parent().unwrap(), "daemon-outbox.json", r#"[{"id":1,"items":["skill:w"]}]"#);
    m.write("personal/w/SKILL.md", "recreated\n");
    let config = m.home.path().join(".config/sk1ll/machine.toml");
    let good = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, "[broken").unwrap();
    m.start().await;
    let (mut ws, _) = hub.accept().await;
    if let Ok(Some(Ok(Message::Text(text)))) = tokio::time::timeout(Duration::from_millis(700), ws.next()).await {
        panic!("a failed local refresh must not report or replay discards: {text}");
    }
    let status = m.status().await;
    assert_eq!(status.pending_discards, 1);
    assert!(status.last_error.as_deref().unwrap_or_default().contains("machine.toml"), "{status:?}");

    std::fs::write(config, good).unwrap();
    let (mut ws, _) = hub.accept().await;
    let DaemonMessage::Status(report) = next_message(&mut ws).await else { panic!("fresh status comes first") };
    assert!(report.drafts.contains(&ItemRef::Skill("w".into())), "{report:?}");
    eventually(|| async { (m.status().await.pending_discards == 0).then_some(()) }).await;
    while let Ok(Some(Ok(Message::Text(text)))) = tokio::time::timeout(Duration::from_millis(300), ws.next()).await {
        assert!(!matches!(serde_json::from_str::<DaemonMessage>(&text).unwrap(), DaemonMessage::DraftDiscarded { .. }));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn discard_replay_skips_recreated_drafts_and_unpushed_items() {
    let hub = FakeHub::new().await;
    let mut m = solo("solo", &hub.url(), Some(snapshot(1, &[])));
    m.write("personal/v/SKILL.md", "promoted again\n");
    git(&m.repo(), &["add", "personal/v"]);
    git(&m.repo(), &["commit", "-qm", "promote v again"]);
    m.write("personal/w/SKILL.md", "recreated draft\n");
    write(m.outbox_path().parent().unwrap(), "daemon-outbox.json",
        r#"[{"id":1,"items":["skill:w","skill:v","skill:gone"]}]"#);
    m.start().await;
    let (mut ws, _) = hub.accept().await;
    let DaemonMessage::Status(report) = next_message(&mut ws).await else { panic!("fresh status comes first") };
    assert!(report.drafts.contains(&ItemRef::Skill("w".into())));
    assert!(report.unpushed.contains(&ItemRef::Skill("v".into())));
    let DaemonMessage::DraftDiscarded { items } = next_message(&mut ws).await else { panic!("expected discards") };
    assert_eq!(items, [ItemRef::Skill("gone".into())]);
    eventually(|| async { (m.status().await.pending_discards == 0).then_some(()) }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn replacing_a_watched_skills_directory_rearms_the_watch() {
    let hub = FakeHub::new().await;
    let mut m = solo("solo", &hub.url(), Some(snapshot(1, &["x"])))
        .with_env("SK1LL_RECONCILE_INTERVAL", "1h");
    m.start().await;
    eventually(|| async { (m.harness_skill("x").as_deref() == Some("x\n")).then_some(()) }).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    std::fs::rename(m.skills(), m.skills().with_file_name("old-skills")).unwrap();
    std::fs::create_dir(m.skills()).unwrap();
    eventually(|| async { (m.harness_skill("x").as_deref() == Some("x\n")).then_some(()) }).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    std::fs::write(m.skills().join("x/SKILL.md"), "edited in the replacement directory\n").unwrap();
    tokio::time::timeout(Duration::from_secs(5), eventually(|| async {
        m.status().await.report.conflicts.iter().any(|c| c.item == ItemRef::Skill("x".into())).then_some(())
    })).await.expect("edits in the new inode must be watched without waiting for reconciliation");
}

// ---------------------------------------------------------------------------
// Adopting an existing clone, and git deadlines

/// Runs `sk1ll-daemon join` with `code`; returns whether it succeeded and its stderr.
async fn try_join(m: &Machine, hub: &Hub, code: &str) -> (bool, String) {
    let mut cmd = m.command();
    cmd.args(["join", &hub.url(), code, "--id", &m.id, "--repo"]).arg(m.repo());
    let out = tokio::task::spawn_blocking(move || cmd.output().unwrap()).await.unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned())
}

#[tokio::test(flavor = "multi_thread")]
async fn joining_with_an_independent_clone_is_refused() {
    let hub = Hub::start().await;
    let m = Machine::new("gamma");
    let repo = m.repo();
    // `git init` on `master` with the hub as origin: not a clone of the hub.
    git(m.home.path(), &["init", "--quiet", "-b", "master", repo.to_str().unwrap()]);
    m.write("sk1ll.toml", "# gamma's own\n");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "--quiet", "-m", "gamma root"]);
    git(&repo, &["remote", "add", "origin", &format!("{}{}", hub.url(), sk1ll_proto::REPO_PATH)]);
    let baseline = hub.head();

    // The wrong branch is refused before the join code is spent.
    let code = hub.join_code().await;
    let (ok, err) = try_join(&m, &hub, &code).await;
    assert!(!ok && err.contains("branch `master`") && err.contains("`main`"), "{err}");
    assert!(hub.row("gamma").await.is_none());

    // On `main` but with a history of its own: refused once the hub can be fetched.
    git(&repo, &["branch", "-m", "master", "main"]);
    let (ok, err) = try_join(&m, &hub, &code).await;
    assert!(!ok && err.contains("shares no history"), "{err}");
    assert_eq!(hub.head(), baseline, "nothing was pushed");
    let bare = hub.data.join("repo.git");
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
    // A "hub" that accepts connections and then never says a word.
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
    let hub_url = format!("http://127.0.0.1:{port}");
    DaemonState { machine_id: "slow".into(), hub_url, token: "tok".into(), repo: m.repo(), revoked: false }
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
async fn displaced_conflicts_report_to_hub_and_resolve() {
    let hub = FakeHub::new().await;
    let mut m = solo("solo", &hub.url(), Some(snapshot(1, &["x"])));
    let state = m.home.path().join("state");
    m = m.with_env("XDG_STATE_HOME", state.to_str().unwrap());
    // Two entries kept in the state directory by an older version, and one in
    // the skills dir's own recovery area that the state directory's index
    // doesn't know (it was lost): the daemon's recovery lists it again.
    let recovery = state.join("sk1ll/displaced");
    let area = m.skills().join(".sk1ll-tmp/displaced");
    for (id, dir) in [("adopt-me", &recovery), ("discard-me", &recovery), ("found-again", &area)] {
        let dir = dir.join(id);
        write(&dir.join("content"), "SKILL.md", "recovered edit\n");
        write(&dir, "entry.json", &serde_json::json!({
            "harness": HARNESS, "kind": "skill", "dest": m.skills().join("x"),
            "expected": "", "displaced_at": 0
        }).to_string());
    }
    m.start().await;
    let (mut ws, _) = hub.accept().await;
    let DaemonMessage::Status(report) = next_message(&mut ws).await else { panic!("expected status") };
    assert!(report.conflicts.iter().any(|c| c.harness == HARNESS && c.item == ItemRef::Skill("x".into())));
    let entries = m.client.conflicts().await.unwrap();
    assert_eq!(entries.iter().filter(|e| e.kind.as_deref() == Some("displaced")).count(), 3);
    assert!(entries.iter().all(|e| e.path.is_some() && e.displaced_at == Some(0)));
    m.client.adopt_displaced("adopt-me").await.unwrap();
    assert_eq!(std::fs::read_to_string(m.repo().join("personal/x/SKILL.md")).unwrap(), "recovered edit\n");
    assert_eq!(m.client.conflicts().await.unwrap().len(), 2);
    m.client.discard_displaced("found-again").await.unwrap();
    m.client.discard_displaced("discard-me").await.unwrap();
    assert!(m.client.conflicts().await.unwrap().is_empty());
    assert!(m.status().await.report.conflicts.is_empty());
}

/// A pasted link is resolved without touching the repo, then one of its
/// skills is added into the draft.
#[tokio::test(flavor = "multi_thread")]
async fn imports_resolve_links_and_add_one_skill() {
    let hub = FakeHub::new().await;
    let mut m = solo("solo", &hub.url(), Some(snapshot(1, &["x"])));
    let source = m.home.path().join("source");
    write(&source.join("skills/pdf"), "SKILL.md", "---\ndescription: Work with PDFs\n---\n");
    write(&source.join("skills/x"), "SKILL.md", "---\ndescription: Clashes with personal x\n---\n");
    git(&source, &["init", "--quiet", "-b", "main"]);
    git(&source, &["add", "-A"]);
    git(&source, &["commit", "--quiet", "-m", "skills"]);
    let link = source.to_string_lossy().into_owned();
    m.start().await;

    let req = ResolveImportRequest { link: link.clone(), path: String::new() };
    let r = m.client.resolve_import(&req).await.unwrap();
    assert_eq!((r.source.as_str(), r.git_ref.as_str()), (link.as_str(), "HEAD"));
    let found: Vec<_> = r.candidates.iter().map(|c| (c.path.as_str(), c.default_name.as_str(), c.taken)).collect();
    assert_eq!(found, [("skills/pdf", "pdf", false), ("skills/x", "x", true)]);
    assert_eq!(r.candidates[0].description.as_deref(), Some("Work with PDFs"));
    assert!(m.client.draft().await.unwrap().is_empty(), "resolving leaves the repo alone");

    let add = |path: Option<&str>| AddImportRequest {
        name: String::new(), source: link.clone(), path: path.map(Into::into), git_ref: None, commit: None,
    };
    let err = m.client.add_import(&add(None)).await.unwrap_err();
    assert_eq!(err.status, Some(400));
    assert!(err.message.contains("skills/pdf, skills/x"), "{}", err.message);
    // Upstream moves on after the preview; the previewed commit is what's added.
    write(&source.join("skills/pdf"), "SKILL.md", "---\ndescription: v2\n---\n");
    git(&source, &["commit", "--quiet", "-am", "v2"]);
    let added = m.client.add_import(&AddImportRequest { commit: Some(r.commit.clone()), ..add(Some("skills/pdf")) })
        .await.unwrap();
    assert_eq!((added.name.as_str(), added.new_commit.as_str()), ("pdf", r.commit.as_str()));
    let vendored = std::fs::read_to_string(m.repo().join("imported/pdf/SKILL.md")).unwrap();
    assert!(vendored.contains("Work with PDFs"), "{vendored}");
    let draft = eventually(|| async {
        let d = m.client.draft().await.unwrap();
        (!d.is_empty()).then_some(d)
    }).await;
    assert!(draft.iter().any(|d| d.item == ItemRef::Skill("pdf".into())), "{draft:?}");
}

/// A GitHub folder link with an explicit `ref` and an explicit root `path`
/// (`""`, as the GUI sends it): both override the link. GitHub is a local
/// repo here (`url.<dir>.insteadOf`).
#[tokio::test(flavor = "multi_thread")]
async fn an_explicit_root_path_overrides_a_folder_link() {
    let hub = FakeHub::new().await;
    let gh = tempfile::tempdir().unwrap();
    let source = gh.path().join("o/r.git");
    write(&source, "SKILL.md", "---\ndescription: The root skill\n---\n");
    write(&source.join("sub/pdf"), "SKILL.md", "---\ndescription: PDFs\n---\n");
    git(&source, &["init", "--quiet", "-b", "main"]);
    git(&source, &["add", "-A"]);
    git(&source, &["commit", "--quiet", "-m", "skills"]);
    let base = format!("file://{}/", gh.path().display());
    let mut m = solo("solo", &hub.url(), Some(snapshot(1, &["x"])))
        .with_env("GIT_CONFIG_COUNT", "1")
        .with_env("GIT_CONFIG_KEY_0", &format!("url.{base}.insteadOf"))
        .with_env("GIT_CONFIG_VALUE_0", "https://github.com/");
    m.start().await;

    let link = "https://github.com/o/r/tree/main/sub/pdf";
    let r = m.client.resolve_import(&ResolveImportRequest { link: link.into(), path: String::new() }).await.unwrap();
    assert_eq!((r.source.as_str(), r.git_ref.as_str()), ("github:o/r", "main"));
    assert_eq!(r.candidates.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(), ["sub/pdf"]);

    let req: AddImportRequest = serde_json::from_value(serde_json::json!({
        "name": "", "source": link, "path": "", "ref": "HEAD",
    })).unwrap();
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
    let hub = FakeHub::new().await;
    let probe = tempfile::tempdir().unwrap();
    let pidfile = probe.path().join("ssh.pid");
    let ssh = format!("sleep 30 & echo $! > '{}'; wait; :", pidfile.display());
    let mut m = solo("solo", &hub.url(), Some(snapshot(1, &["x"])))
        .with_env("GIT_SSH_COMMAND", &ssh)
        .with_env("GIT_SSH_VARIANT", "simple")
        .with_env("SK1LL_GIT_TIMEOUT", "4s");
    m.start().await;

    let client = m.client.clone();
    let req = ResolveImportRequest { link: "ssh://sk1ll.invalid/slow.git".into(), path: String::new() };
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

#[tokio::test(flavor = "multi_thread")]
async fn skills_api_browses_content_and_adopts_only_while_connected() {
    use sk1ll_daemon::client::{ConflictTarget, ContentQuery, ContentType, FileQuery};
    let mut hub = Hub::start().await;
    let mut m = Machine::new("skills");
    // Identical copies in two Harnesses, a differing third, and an empty
    // fourth whose new-skills default remains off.
    let config = m.home.path().join(".config/sk1ll/machine.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    for h in ["second", "different", "empty"] {
        text.push_str(&format!(
            "\n[harnesses.{h}]\nskills = {:?}\nmode = \"copy\"\n",
            m.home.path().join(h).to_str().unwrap()
        ));
    }
    std::fs::write(config, text).unwrap();
    m.join(&hub).await;
    m.start_connected().await;
    m.write(
        "personal/baseline/SKILL.md",
        "---\ndescription: baseline\n---\nold",
    );
    m.write(
        "instructions/git.md",
        "---\ndescription: Git\n---\nfragment baseline",
    );
    assert!(m.promote_all().await.pushed);
    m.write(
        "personal/baseline/SKILL.md",
        "---\ndescription: draft\n---\nnew",
    );
    std::fs::remove_file(m.repo().join("instructions/git.md")).unwrap();
    let items = m.client.skills().await.unwrap();
    let baseline = items.iter().find(|s| s.name == "baseline").unwrap();
    assert!(baseline.in_baseline);
    assert_eq!(baseline.description.as_deref(), Some("draft"));
    assert_eq!(baseline.draft, Some(sk1ll_core::Change::Modified));
    let fragment = items.iter().find(|s| s.name == "git").unwrap();
    assert_eq!(fragment.kind, ContentType::Fragment);
    assert_eq!(fragment.draft, Some(sk1ll_core::Change::Deleted));
    let q = |item: &str| ContentQuery {
        item: item.into(),
        hash: None,
        harness: None,
    };
    let file = |item: &str, path: &str| FileQuery {
        content: q(item),
        path: path.into(),
    };
    assert!(
        m.client
            .skill_file(&file("skill:baseline", "SKILL.md"))
            .await
            .unwrap()
            .text
            .unwrap()
            .ends_with("new")
    );
    assert!(
        m.client
            .skill_file(&file("fragment:git", "git.md"))
            .await
            .unwrap()
            .text
            .unwrap()
            .ends_with("fragment baseline")
    );
    assert_eq!(
        m.client.skill_tree(&q("fragment:git")).await.unwrap()[0].path,
        "git.md"
    );
    assert!(
        m.client
            .skill_file(&file("skill:baseline", "../sk1ll.toml"))
            .await
            .is_err()
    );
    std::fs::write(m.repo().join("personal/baseline/binary"), b"\x00\xff").unwrap();
    assert!(
        m.client
            .skill_file(&file("skill:baseline", "binary"))
            .await
            .unwrap()
            .binary
    );
    for path in [m.skills(), m.home.path().join("second")] {
        write(
            &path.join("pdf"),
            "SKILL.md",
            "---\ndescription: PDFs\n---\nsame",
        );
    }
    write(
        &m.home.path().join("different/pdf"),
        "SKILL.md",
        "different",
    );
    let skills = m.client.skills().await.unwrap();
    let unmanaged = skills
        .iter()
        .find(|s| s.kind == ContentType::Unmanaged && s.name == "pdf")
        .unwrap();
    assert_eq!(unmanaged.variants.len(), 2);
    assert!(!unmanaged.clashes_with_repo);
    let variant = unmanaged
        .variants
        .iter()
        .find(|v| v.locations.len() == 2)
        .unwrap();
    let selected = ContentQuery {
        hash: Some(variant.hash.clone()),
        ..q("unmanaged:pdf")
    };
    assert_eq!(
        m.client.skill_tree(&selected).await.unwrap()[0].path,
        "SKILL.md"
    );
    assert!(
        m.client
            .skill_file(&FileQuery {
                content: selected,
                path: "SKILL.md".into()
            })
            .await
            .unwrap()
            .text
            .unwrap()
            .ends_with("same")
    );
    assert!(m.client.skill_tree(&q("unmanaged:pdf")).await.is_err());
    hub.stop();
    eventually(|| async { (m.status().await.connection == Connection::Offline).then_some(()) })
        .await;
    let target = ConflictTarget { harness: HARNESS.into(), name: "pdf".into() };
    for result in [m.client.adopt_unmanaged(&target).await, m.client.adopt(&target).await] {
        assert!(result.unwrap_err().message.contains("Adopt needs a connection to the Hub"));
    }
    assert!(!m.repo().join("personal/pdf").exists());
    assert!(!m.outbox_path().exists());
    assert!(!m.skills().join("pdf/.sk1ll-managed").exists());
    assert!(m.harness_skill("pdf").unwrap().ends_with("same"));
    hub.launch().await;
    eventually(|| async { (m.status().await.connection == Connection::Connected).then_some(()) }).await;
    m.client.adopt_unmanaged(&target).await.unwrap();
    assert!(m.repo().join("personal/pdf/SKILL.md").is_file());
    assert!(!m.outbox_path().exists());
    assert!(m.skills().join("pdf/.sk1ll-managed").is_file());
    assert!(m.home.path().join("second/pdf/.sk1ll-managed").is_file());
    // The entire operation may have completed before the local response was
    // lost. Repeating it must still succeed and keep its explicit Assignment.
    m.client.adopt_unmanaged(&target).await.unwrap();

    let snap = m.snapshot().unwrap();
    assert!(!snap.assignment.skill_enabled("different", "pdf"));
    assert!(!snap.assignment.skill_enabled("empty", "pdf"));
    assert!(!m.home.path().join("empty/pdf").exists());
    assert_eq!(
        std::fs::read_to_string(m.home.path().join("different/pdf/SKILL.md")).unwrap(),
        "different"
    );
    assert!(m.skills().join(".sk1ll-tmp/displaced").is_dir());
    let recovered = m.client.recovered().await.unwrap();
    let originals = recovered.iter().filter(|entry| entry.name == "pdf" && entry.path.as_ref().is_some_and(|path| {
        std::fs::read_to_string(path.join("SKILL.md")).is_ok_and(|text| text.ends_with("same"))
    })).count();
    assert_eq!(originals, 2);

    let skills = m.client.skills().await.unwrap();
    let adopted = skills
        .iter()
        .find(|s| s.name == "pdf" && s.kind == ContentType::Personal)
        .unwrap();
    assert_eq!(adopted.enabled_harnesses, [HARNESS, "second"]);
    assert_eq!(adopted.draft, Some(sk1ll_core::Change::Added));
    let unmanaged = skills
        .iter()
        .find(|s| s.kind == ContentType::Unmanaged && s.name == "pdf")
        .unwrap();
    assert!(unmanaged.clashes_with_repo);
    assert!(
        m.client
            .adopt_unmanaged(&ConflictTarget {
                harness: "different".into(),
                name: "pdf".into()
            })
            .await
            .unwrap_err()
            .message
            .contains("already exists")
    );
    let log: Vec<LogEntry> = hub.get("/log").await;
    assert!(log.iter().any(|e| e.kind == ToggleKind::Skill
        && e.name.as_deref() == Some("pdf")
        && e.after == Some(true)));
}

#[tokio::test(flavor = "multi_thread")]
async fn unmanaged_adoption_retries_after_crash_between_publication_and_assignment() {
    use sk1ll_daemon::client::ConflictTarget;
    let hub = Hub::start().await;
    let mut m = Machine::new("adopt-retry");
    m.join(&hub).await;
    m.start_connected().await;
    m.stop();
    write(&m.skills().join("pdf"), "SKILL.md", "original user content");
    let repo = sk1ll_core::Repo::open(m.repo()).unwrap();
    let hs = sk1ll_core::MachineConfig::load(&m.home.path().join(".config/sk1ll/machine.toml")).unwrap().harnesses();
    // Simulate a process dying just after atomic draft publication. There is
    // no intent or startup reconciliation; the original is still unmanaged.
    sk1ll_core::sync::unmanaged::prepare(&repo, &hs, HARNESS, "pdf").unwrap();
    m.start_connected().await;
    assert!(!m.skills().join("pdf/.sk1ll-managed").exists());
    let target = ConflictTarget { harness: HARNESS.into(), name: "pdf".into() };
    m.client.adopt(&target).await.unwrap();
    let snap: AssignmentSnapshot = hub.get(&format!("/machines/{}/assignment", m.id)).await;
    assert!(snap.assignment.skill_enabled(HARNESS, "pdf"));
    assert!(m.skills().join("pdf/.sk1ll-managed").is_file());
    assert_eq!(m.harness_skill("pdf").unwrap(), "original user content");
}
