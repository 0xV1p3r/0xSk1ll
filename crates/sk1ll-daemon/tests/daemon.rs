//! End-to-end tests: a real server (on its own runtime, so it can be stopped
//! and restarted on the same port) and one `sk1ll-daemon` process per
//! machine, each with its own HOME and XDG dirs and no global git config,
//! driven through the daemon's local API.

use std::path::Path;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use sk1ll_daemon::DaemonState;
use sk1ll_daemon::client::{
    AddImportRequest, Client, CommitRequest, CommitResponse, Connection, Event, PullKeepRequest, PullResult,
    ResolveImportRequest, RestoreRequest,
};
use sk1ll_daemon::state::PullState;
use sk1ll_proto::{
    Assignment, AssignmentChanges, AssignmentSnapshot, DaemonMessage, HarnessAssignment, ItemRef, LogEntry,
    MachineStatus, ServerMessage, ToggleChange, ToggleKind,
};
use tokio_tungstenite::tungstenite::Message;

use sk1ll_testkit::daemon::{DaemonMachine, HARNESS};
use sk1ll_testkit::eventually;
use sk1ll_testkit::git::git;
use sk1ll_testkit::server::{ADMIN, Server};

fn machine(id: &str) -> DaemonMachine {
    DaemonMachine::new(id, env!("CARGO_BIN_EXE_sk1ll-daemon"))
        .with_env("TOKIO_WORKER_THREADS", "2")
        .with_env("SK1LL_NOTIFICATIONS", "0")
}

trait MachineExt {
    fn snapshot(&self) -> Option<AssignmentSnapshot>;
    fn write_repo(&self, rel: &str, body: &str);
    fn harness_skill(&self, name: &str) -> Option<String>;
    async fn commit_all(&self) -> CommitResponse;
    async fn commit(&self, items: &[&str]) -> CommitResponse;
    fn head(&self) -> String;
}

impl MachineExt for DaemonMachine {
    fn snapshot(&self) -> Option<AssignmentSnapshot> {
        let text = std::fs::read_to_string(self.snapshot_path()).ok()?;
        Some(serde_json::from_str(&text).unwrap())
    }

    /// Writes `rel` in the repo. A new personal skill gets the explicit
    /// Item ID [`skill_id`] names, so tests can toggle it before a refresh
    /// would write it one; IDs are never derived from names.
    fn write_repo(&self, rel: &str, body: &str) {
        let path = self.write(rel, body);
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
    sk1ll_testkit::write(&dir.join(file), body);
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
        self.refuse("HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
    }

    /// Answers the next connection's request with `response`.
    async fn refuse(&self, response: &str) {
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
        tcp.write_all(response.as_bytes()).await.unwrap();
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
    let h = HarnessAssignment { skills, ..HarnessAssignment::all_off() };
    let assignment = Assignment { harnesses: [(HARNESS.to_string(), h)].into(), ..Assignment::fleet() };
    AssignmentSnapshot { revision, assignment, registration: None }
}

/// A machine whose repo has skills x, y, z (pushed to a local bare origin)
/// and whose server is `server_url`, with `snap` stored as its assignment.
fn solo(id: &str, server_url: &str, snap: Option<AssignmentSnapshot>) -> DaemonMachine {
    let m = machine(id);
    let repo = m.repo();
    let origin = m.home.path().join("origin.git");
    sk1ll_core::Repo::init(&repo).unwrap();
    for s in ["x", "y", "z"] {
        m.write_repo(&format!("personal/{s}/SKILL.md"), &format!("{s}\n"));
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

async fn assign(via: &DaemonMachine, machine: &str, changes: Vec<ToggleChange>) {
    via.client.set_assignment(machine, &AssignmentChanges { changes, ..Default::default() }).await.unwrap();
}

/// `alpha` joins the fresh server and commits the new repo with skill `x`;
/// then `beta` joins and clones it. Returns both and the baseline.
async fn fleet_of_two(server: &Server) -> (DaemonMachine, DaemonMachine, String) {
    let mut a = machine("alpha");
    a.join(server).await;
    a.start_connected().await;
    a.write_repo("personal/x/SKILL.md", "x v1\n");
    let p = a.commit_all().await;
    assert!(p.pushed, "{p:?}");
    assert_eq!(server.head().as_deref(), Some(p.sha.as_str()));

    let mut b = machine("beta");
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

/// The per-Machine harness route: this Machine's own is written here (and
/// syncs), another's at the server.
#[tokio::test]
async fn machine_harness_assignment_writes_this_or_another_machine() {
    let server = Server::start().await;
    let (a, b, _) = fleet_of_two(&server).await;
    let other = a.client.set_machine_harness_assignment("beta", HARNESS, true).await.unwrap();
    assert!(other.assignment.skill_enabled(HARNESS, &skill_id("x")));
    assert!(other.entries.iter().all(|entry| entry.machine == "beta" && entry.author == "alpha"));
    eventually(|| async { b.harness_skill("x").is_some().then_some(()) }).await;
    let own = b.client.set_machine_harness_assignment("beta", HARNESS, false).await.unwrap();
    assert!(own.entries.iter().all(|entry| entry.machine == "beta" && entry.author == "beta"));
    eventually(|| async { b.harness_skill("x").is_none().then_some(()) }).await;
    let on = b.client.set_harness_assignment(HARNESS, true).await.unwrap();
    assert!(on.assignment.skill_enabled(HARNESS, &skill_id("x")));
    eventually(|| async { b.harness_skill("x").is_some().then_some(()) }).await;
    let off = b.client.set_harness_assignment(HARNESS, false).await.unwrap();
    assert!(!off.assignment.skill_enabled(HARNESS, &skill_id("x")));
    assert!(off.entries.iter().all(|entry| entry.machine == "beta"));
    eventually(|| async { b.harness_skill("x").is_none().then_some(()) }).await;
    let mut own = machine("gamma");
    own.join_with(&server, &server.join_code_for(sk1ll_proto::Scope::SelfOnly).await.code).await;
    own.start_connected().await;
    eventually(|| async { (server.row("gamma").await?.scope == sk1ll_proto::Scope::SelfOnly).then_some(()) }).await;
    let before = a.client.assignment("alpha").await.unwrap();
    let refused = own.client.set_machine_harness_assignment("alpha", HARNESS, true).await.unwrap_err();
    assert_eq!(refused.status, Some(403), "{refused:?}");
    assert_eq!(a.client.assignment("alpha").await.unwrap(), before, "unchanged");
    let mine = own.client.set_machine_harness_assignment("gamma", HARNESS, true).await.unwrap();
    assert!(!mine.entries.is_empty(), "{mine:?}");
    assert!(mine.entries.iter().all(|entry| entry.machine == "gamma" && entry.author == "gamma"), "{mine:?}");
    assert!(mine.assignment.skill_enabled(HARNESS, &skill_id("x")), "the change is there: {mine:?}");
}

#[tokio::test]
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
    a.write_repo("personal/y/SKILL.md", "y v1\n");
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

#[tokio::test]
async fn join_codes_pass_their_scope_and_expiry_through() {
    let server = Server::start().await;
    let mut a = machine("alpha");
    a.join(&server).await;
    a.start_connected().await;
    let code = a.client.join_code(sk1ll_proto::Scope::SelfOnly).await.unwrap();
    assert_eq!(code.scope, sk1ll_proto::Scope::SelfOnly);
    assert!(code.expires_at > 0);
    let fleet = a.client.fleet().await.unwrap();
    assert_eq!(fleet.machines[0].scope, sk1ll_proto::Scope::Fleet);
}

#[tokio::test]
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
    let cached = Assignment::load(&b.cache_path()).unwrap();
    assert_eq!(cached.harness(HARNESS).skills.get(&skill_id("x")), Some(&false), "the assignment is cached");
}

#[tokio::test]
async fn offline_commits_are_queued_and_pushed_on_reconnect() {
    let mut server = Server::start().await;
    let (a, mut b, baseline) = fleet_of_two(&server).await;
    assert!(a.status().await.draft_commit_complete);
    assert!(!b.status().await.draft_commit_complete, "cloning another Machine's commit doesn't complete the step");

    server.stop();
    eventually(|| async { (b.status().await.connection == Connection::Offline).then_some(()) }).await;
    let err = b.client.fleet().await.unwrap_err();
    assert_eq!(err.status, Some(503), "{err}");

    b.write_repo("personal/z/SKILL.md", "z\n");
    let p = b.commit(&["z"]).await;
    assert!(!p.pushed, "nothing to push to while offline");
    let s = b.status().await;
    assert!(!s.draft_commit_complete);
    assert_eq!(s.report.unpushed, [ItemRef::Skill("z".into())]);
    assert_eq!(s.headline, MachineStatus::Offline);
    assert_eq!(server.head().as_deref(), Some(baseline.as_str()));

    server.restart().await;
    eventually(|| async { (server.head().as_deref() == Some(p.sha.as_str())).then_some(()) }).await;
    eventually(|| async {
        let s = b.status().await;
        (s.connection == Connection::Connected && s.report.unpushed.is_empty() && s.draft_commit_complete).then_some(())
    })
    .await;
    b.stop();
    b.start_connected().await;
    assert!(b.status().await.draft_commit_complete);
}

#[tokio::test]
async fn blocked_pull_reports_clashes_and_keeps_files() {
    let mut server = Server::start().await;
    let (a, mut b, base) = fleet_of_two(&server).await;

    b.write_repo("personal/x/SKILL.md", "x by beta\n");
    eventually(|| async {
        server.row("beta").await?.report?.drafts.contains(&ItemRef::Skill("x".into())).then_some(())
    })
    .await;
    a.write_repo("personal/x/SKILL.md", "x by alpha\n");
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
    server.restart().await;
    eventually(|| async { (b.status().await.connection == Connection::Connected).then_some(()) }).await;

    // Restoring the clashing item is a draft change: the pull is retried.
    let kept = b.client.restore(&RestoreRequest { items: vec!["x".into()], all: false }).await.unwrap().kept;
    eventually(|| async {
        let s = b.status().await;
        (!s.report.blocked && s.report.baseline.as_deref() == Some(p.sha.as_str())).then_some(())
    })
    .await;
    assert_eq!(std::fs::read_to_string(b.repo().join("personal/x/SKILL.md")).unwrap(), "x by alpha\n");
    let id = kept.into_iter().next().expect("a recovery entry");
    let report = serde_json::json!([{ "item": "skill:x", "entry": id, "error": "the keep stopped after its pull" }]);
    std::fs::write(b.repo().join(".git/sk1ll-keep-report.json"), report.to_string()).unwrap();
    // A fresh read waits for, and runs, a reconcile, which reads the report.
    let listed = || async {
        b.client.recovered_fresh().await.unwrap();
        b.client.status().await.unwrap().keep_report.iter().any(|u| u.entry.as_ref() == Some(&id))
    };
    eventually(|| async { listed().await.then_some(()) }).await;
    assert!(listed().await, "still there after another reconcile");
    b.client.discard_displaced(&id).await.unwrap();
    eventually(|| async { (!listed().await).then_some(()) }).await;
    assert!(!b.repo().join(".git/sk1ll-keep-report.json").exists());
}

/// `POST /pull/keep` takes the new Baseline and puts the Draft's version
/// back on top: the Machine is no longer Blocked and its version is Draft.
/// Here, not among the library's tests: its recovery entries are in this
/// daemon's own state directory, which no other test's reconcile lists.
#[tokio::test]
async fn keep_mine_clears_blocked_with_the_draft_on_top() {
    let server = Server::start().await;
    let (a, b, _) = fleet_of_two(&server).await;
    b.write_repo("personal/x/SKILL.md", "x by beta\n");
    eventually(|| async {
        server.row("beta").await?.report?.drafts.contains(&ItemRef::Skill("x".into())).then_some(())
    })
    .await;
    a.write_repo("personal/x/SKILL.md", "x by alpha\n");
    let p = a.commit(&["x"]).await;
    assert!(p.pushed);
    eventually(|| async { b.status().await.report.blocked.then_some(()) }).await;

    let request = |items: &[&str], all| PullKeepRequest { items: items.iter().map(|i| i.to_string()).collect(), all };
    let refused = b.client.pull_keep(&request(&["rule:none"], false)).await.unwrap_err();
    assert!(refused.to_string().contains("is neither in the draft nor in an unpushed commit"), "{refused}");
    let kept = b.client.pull_keep(&request(&["x"], false)).await.unwrap();
    assert!(matches!(kept.pull, PullResult::Updated { .. }), "{kept:?}");
    assert_eq!(kept.kept, [ItemRef::Skill("x".into())]);
    assert_eq!(std::fs::read_to_string(b.repo().join("personal/x/SKILL.md")).unwrap(), "x by beta\n");
    assert_eq!(b.head(), p.sha, "on the new baseline");
    let s = b.status().await;
    assert!(!s.report.blocked, "{s:?}");
    assert!(s.report.drafts.contains(&ItemRef::Skill("x".into())), "{s:?}");
}

#[tokio::test]
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
#[tokio::test]
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

/// API readiness must include watching local edits, even if the first server
/// handshake is still waiting for a response and cannot trigger a refresh.
#[tokio::test]
async fn repo_edits_reach_the_harness_before_and_after_the_first_connection() {
    let server = FakeServer::new().await;
    let mut m = solo("solo", &server.url(), Some(snapshot(1, &["x"])));
    m.start().await;
    m.write_repo("personal/x/SKILL.md", "edited before connecting\n");
    eventually(|| async { (m.harness_skill("x").as_deref() == Some("edited before connecting\n")).then_some(()) })
        .await;
    assert_ne!(m.status().await.connection, Connection::Connected);
    let (_socket, _) = server.accept().await;
    eventually(|| async { (m.status().await.connection == Connection::Connected).then_some(()) }).await;
    m.write_repo("personal/x/SKILL.md", "edited after connecting\n");
    eventually(|| async { (m.harness_skill("x").as_deref() == Some("edited after connecting\n")).then_some(()) }).await;
}

/// #10: edits arriving faster than the debounce still sync within the
/// maximum debounce.
#[tokio::test]
async fn a_stream_of_edits_cannot_postpone_the_sync() {
    let server = FakeServer::new().await;
    let mut m = solo("solo", &server.url(), Some(snapshot(1, &["x"])))
        .with_env("SK1LL_DEBOUNCE", "300ms")
        .with_env("SK1LL_DEBOUNCE_MAX", "1s");
    m.start().await;
    eventually(|| async { (m.harness_skill("x").as_deref() == Some("x\n")).then_some(()) }).await;

    let started = std::time::Instant::now();
    for i in 0.. {
        m.write_repo("personal/x/SKILL.md", &format!("x {i}\n"));
        if m.harness_skill("x").as_deref() != Some("x\n") {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(8), "never synced while edits kept coming");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// #2: a 401 (here from something in front of the server) is a recoverable
/// state: the machine keeps being managed, retries, and is never revoked.
#[tokio::test]
async fn a_401_is_not_a_revocation() {
    let server = FakeServer::new().await;
    let mut m = solo("solo", &server.url(), Some(snapshot(1, &["x"])));
    m.start().await;

    // Every status a client is told about shows the rejection whole: the
    // state, since when, and why, published together.
    let mut events = m.client.events().await.unwrap();
    server.refuse_401().await;
    let s = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match events.next().await {
                Some(Ok(Event::Status)) => {
                    let s = m.status().await;
                    if s.connection == Connection::AuthRejected {
                        assert!(s.auth_rejected_since.is_some(), "{s:?}");
                        assert!(s.last_error.as_deref().is_some_and(|error| error.contains("401")), "{s:?}");
                        break s;
                    }
                }
                Some(Ok(_)) => {}
                other => panic!("event stream ended: {other:?}"),
            }
        }
    })
    .await
    .expect("no status showed the rejection");
    assert!(!s.revoked, "{s:?}");
    assert!(!m.state().revoked, "not stored as revoked");

    // Still managing the harnesses.
    m.write_repo("personal/x/SKILL.md", "x while rejected\n");
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

/// A server of another protocol version refuses the connection (426) and
/// names its release: the status says so and which side to upgrade, the
/// machine keeps being managed, and a later accepted connection clears it.
#[tokio::test]
async fn a_server_of_another_protocol_is_incompatible_and_named() {
    let server = FakeServer::new().await;
    let mut m = solo("solo", &server.url(), Some(snapshot(1, &["x"])));
    m.start().await;

    let newer = sk1ll_proto::PROTOCOL_VERSION + 1;
    let body = r#"{"error":"protocol mismatch"}"#;
    server
        .refuse(&format!(
            "HTTP/1.1 426 Upgrade Required\r\nsk1ll-protocol: {newer}\r\nsk1ll-version: 9.9.9\r\n\
             content-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        ))
        .await;
    let s = eventually(|| async {
        let s = m.status().await;
        (s.connection == Connection::Incompatible).then_some(s)
    })
    .await;
    let named = sk1ll_daemon::client::ServerVersion { version: Some("9.9.9".into()), protocol: Some(newer) };
    assert_eq!(s.server.as_ref(), Some(&named), "{s:?}");
    assert_eq!(s.protocol, sk1ll_proto::PROTOCOL_VERSION);
    assert_eq!(s.headline, sk1ll_proto::MachineStatus::Offline);
    let error = s.last_error.unwrap_or_default();
    assert!(error.contains("sk1ll 9.9.9") && error.contains("upgrade sk1ll on this Machine"), "{error}");

    server.refuse_401().await;
    let (_ws, _) = server.accept().await;
    eventually(|| async { (m.status().await.connection == Connection::Connected).then_some(()) }).await;
}

/// #3: the token is never in a remote URL, `.git/config`, or any output;
/// push, clone and pull work with it supplied through the environment, and
/// a URL with credentials from an older version is cleaned at startup.
#[tokio::test]
async fn the_token_stays_out_of_git_urls_and_logs() {
    let server = Server::start().await;
    let mut a = machine("alpha");
    let a_join = a.join(&server).await;
    a.start_connected().await;
    a.write_repo("personal/x/SKILL.md", "x\n");
    assert!(a.commit_all().await.pushed, "push works");

    let mut b = machine("beta");
    let b_join = b.join(&server).await;
    b.start().await;
    assert!(b.repo().join("personal/x/SKILL.md").is_file(), "clone works");
    a.write_repo("personal/y/SKILL.md", "y\n");
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
    let old = format!("http://beta:{token}@127.0.0.1:{}/git/repo.git", server.port());
    git(&b.repo(), &["remote", "set-url", "origin", &old]);
    b.start().await;
    assert_eq!(git(&b.repo(), &["remote", "get-url", "origin"]), format!("{}/git/repo.git", server.url()));
    eventually(|| async { (b.status().await.connection == Connection::Connected).then_some(()) }).await;
    b.write_repo("personal/z/SKILL.md", "z\n");
    assert!(b.commit(&["z"]).await.pushed, "push works after the migration");
    assert!(!b.log().contains(&token));
}

/// #4: the assignment is stored with its revision in the state dir, so
/// losing the cache can't enable anything; without any stored snapshot the
/// harnesses wait for the server's, which is taken even at the same revision.
#[tokio::test]
async fn losing_the_assignment_cache_keeps_disabled_skills_off() {
    let mut server = Server::start().await;
    let (a, mut b, _) = fleet_of_two(&server).await;
    assert_eq!(b.harness_skill("x"), None, "a new machine starts with everything off");
    let revision = b.status().await.assignment_revision;
    assert_eq!(b.snapshot().unwrap().revision, revision);

    b.stop();
    std::fs::remove_dir_all(b.home.cache_dir()).unwrap();
    b.start().await;
    eventually(|| async { (b.status().await.connection == Connection::Connected).then_some(()) }).await;
    assert_eq!(b.harness_skill("x"), None, "the disabled skill stays off");
    assert!(b.cache_path().is_file(), "the cache is mirrored again");

    b.stop();
    server.stop();
    std::fs::remove_file(b.snapshot_path()).unwrap();
    std::fs::remove_dir_all(b.home.cache_dir()).unwrap();
    b.start().await;
    let s = b.status().await;
    assert!(s.awaiting_assignment, "{s:?}");
    assert_eq!(b.harness_skill("x"), None, "nothing is managed without an assignment");

    server.restart().await;
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
#[tokio::test]
async fn the_daemon_holds_the_engine_lock() {
    let server = Server::start().await;
    let mut a = machine("alpha");
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
#[tokio::test]
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

    m.write_repo("personal/new/SKILL.md", "new\n");
    let (mut ws, _) = server.accept().await;
    let DaemonMessage::Status(report) = next_message(&mut ws).await else { panic!("status comes first") };
    assert_eq!(report.drafts, [ItemRef::Skill("new".into())]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnecting_does_not_send_status_until_refresh_succeeds() {
    let server = FakeServer::new().await;
    let mut m = solo("solo", &server.url(), Some(snapshot(1, &[]))).with_env("SK1LL_RECONCILE_INTERVAL", "1h");
    m.write_repo("personal/w/SKILL.md", "draft");
    let config = m.machine_toml();
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
#[tokio::test]
async fn connecting_costs_no_reconcile_when_nothing_changed() {
    let server = FakeServer::new().await;
    // Neither the watcher nor the reconciliation refreshes during the test.
    let mut m = solo("solo", &server.url(), Some(snapshot(1, &[])))
        .with_env("SK1LL_DEBOUNCE", "60s")
        .with_env("SK1LL_DEBOUNCE_MAX", "60s")
        .with_env("SK1LL_RECONCILE_INTERVAL", "1h")
        .with_env("RUST_LOG", "info,sk1ll_daemon=debug");
    let baseline = git(&m.repo(), &["rev-parse", "HEAD"]);
    let refreshes = |m: &DaemonMachine| m.log().matches("reconciling: ").count();
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
#[tokio::test]
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
    b.write_repo("personal/q/SKILL.md", "q\n");
    assert!(b.commit(&["q"]).await.pushed, "git works with the new token");
    assert!(!b.log().contains(&state.token));
}

#[tokio::test]
async fn replacing_a_watched_skills_directory_rearms_the_watch() {
    const WAKE: &str = "file changes: waking a reconcile";
    let server = FakeServer::new().await;
    // No periodic pull or reconcile within the test, wakes that fire fast,
    // and a log line for each wake file events cause.
    let mut m = solo("solo", &server.url(), Some(snapshot(1, &["x"])))
        .with_env("SK1LL_RECONCILE_INTERVAL", "1h")
        .with_env("SK1LL_FULL_RECONCILE_INTERVAL", "1h")
        .with_env("SK1LL_PULL_INTERVAL", "1h")
        .with_env("SK1LL_DEBOUNCE", "100ms")
        .with_env("SK1LL_DEBOUNCE_MAX", "300ms")
        .with_env("RUST_LOG", "info,sk1ll_daemon=debug");
    m.start().await;
    // The initial watches are in place before the local API answers.
    eventually(|| async { (m.harness_skill("x").as_deref() == Some("x\n")).then_some(()) }).await;
    std::fs::rename(m.skills(), m.skills().with_file_name("old-skills")).unwrap();
    std::fs::create_dir(m.skills()).unwrap();
    // Placed again: the daemon saw the replacement, with no reconcile due for an hour.
    eventually(|| async { (m.harness_skill("x").as_deref() == Some("x\n")).then_some(()) }).await;
    // Idle: no file-event wake in a whole cycle of waiting out the debounce
    // maximum and a fresh read, which waits for the reconcile running.
    let wakes = || m.log().matches(WAKE).count();
    let mut before = wakes();
    loop {
        tokio::time::sleep(Duration::from_millis(600)).await;
        m.client.recovered_fresh().await.unwrap();
        match wakes() {
            now if now == before => break,
            now => before = now,
        }
    }
    // One edit, no retry: the conflict it causes must come with a wake the watch caused.
    std::fs::write(m.skills().join("x/SKILL.md"), "edited in the replacement directory\n").unwrap();
    eventually(|| async {
        m.status().await.report.conflicts.iter().any(|c| c.item == ItemRef::Skill("x".into())).then_some(())
    })
    .await;
    assert!(wakes() > before, "the edit's wake came from the watch on the new directory");
}

// ---------------------------------------------------------------------------
// Adopting an existing clone, and git deadlines

/// Runs `sk1ll-daemon join` with `code`; returns whether it succeeded and its stderr.
async fn try_join(m: &DaemonMachine, server: &Server, code: &str) -> (bool, String) {
    let mut cmd = m.command();
    cmd.args(["join", &server.url(), code, "--id", &m.id, "--repo"]).arg(m.repo());
    let out = tokio::task::spawn_blocking(move || cmd.output().unwrap()).await.unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned())
}

#[tokio::test]
async fn joining_with_an_independent_clone_is_refused() {
    let server = Server::start().await;
    let m = machine("gamma");
    let repo = m.repo();
    // `git init` on `master` with the server as origin: not a clone of the server.
    git(m.home.path(), &["init", "--quiet", "-b", "master", repo.to_str().unwrap()]);
    m.write_repo("sk1ll.toml", "# gamma's own\n");
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
    let bare = server.data().join("repo.git");
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
#[tokio::test]
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
    let mut m = machine("slow").with_env("SK1LL_GIT_TIMEOUT", "100ms").with_env("SK1LL_GIT_LOCAL_TIMEOUT", "100ms");
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

#[tokio::test]
async fn displaced_conflicts_report_to_server_and_resolve() {
    let server = FakeServer::new().await;
    let mut m = solo("solo", &server.url(), Some(snapshot(1, &["x"])));
    let state = m.home.path().join("state");
    m = m.with_env("XDG_STATE_HOME", state.to_str().unwrap());
    m.start().await;
    let (mut ws, _) = server.accept().await;
    let DaemonMessage::Status(_) = next_message(&mut ws).await else { panic!("expected status") };
    eventually(|| async { m.harness_skill("x").map(drop) }).await;
    let mut open = std::fs::OpenOptions::new().append(true).open(m.skills().join("x/SKILL.md")).unwrap();
    m.write_repo("personal/x/SKILL.md", "draft v2\n");
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
#[tokio::test]
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
#[tokio::test]
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
#[tokio::test]
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
#[tokio::test]
async fn reads_and_syncs_answer_while_a_pull_fetches() {
    use sk1ll_daemon::client::{ConflictDiffRequest, ContentQuery, FileQuery};
    // Never accepted: the daemon stays offline and pulls only when asked.
    let server = FakeServer::new().await;
    let probe = tempfile::tempdir().unwrap();
    let pidfile = probe.path().join("ssh.pid");
    let release = probe.path().join("release");
    let ssh = format!(
        "echo ready > '{}'; for i in $(seq 1 250); do test -f '{}' && exit 7; sleep 0.04; done; exit 8",
        pidfile.display(),
        release.display()
    );
    let mut m = solo("solo", &server.url(), Some(snapshot(1, &["x"])))
        .with_env("GIT_SSH_COMMAND", &ssh)
        .with_env("GIT_SSH_VARIANT", "simple")
        .with_env("SK1LL_GIT_TIMEOUT", "8s")
        .with_env("SK1LL_RECONCILE_INTERVAL", "1h");
    m.start().await;
    eventually(|| async { m.harness_skill("x").map(drop) }).await;
    // A draft change, and a conflict: a hand edit of a synced skill.
    m.write_repo("personal/x/SKILL.md", "x edited\n");
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
    // The fetch holds the pull for 8 s (the git timeout): well under that
    // is not waiting for it, with room for a loaded machine.
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(6), "the calls waited {elapsed:?} for the pull");
    assert!(!stuck.is_finished(), "the pull was still fetching");

    sk1ll_testkit::write(&release, "answered");
    let err = stuck.await.unwrap().unwrap_err();
    assert!(err.message.contains("Could not read from remote repository"), "{}", err.message);
}

#[tokio::test]
async fn skills_api_browses_content_and_adopts_only_while_connected() {
    use sk1ll_daemon::client::{ConflictTarget, ContentQuery, ContentType, FileQuery};
    let mut server = Server::start().await;
    let mut m = machine("skills");
    // Identical copies in two Harnesses, a differing third, and an empty
    // fourth whose new-skills default remains off.
    let config = m.machine_toml();
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
    m.write_repo("personal/baseline/SKILL.md", "---\ndescription: baseline\n---\nold");
    m.write_repo("instructions/git.md", "---\ndescription: Git\n---\nrule baseline");
    assert!(m.commit_all().await.pushed);
    m.write_repo("personal/baseline/SKILL.md", "---\ndescription: draft\n---\nnew");
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
    server.restart().await;
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
    let originals: Vec<_> = recovered
        .iter()
        .filter(|entry| {
            entry.name == "pdf"
                && entry.path.as_ref().is_some_and(|path| {
                    std::fs::read_to_string(path.join("SKILL.md")).is_ok_and(|text| text.ends_with("same"))
                })
        })
        .collect();
    assert_eq!(originals.len(), 2);
    // Each original maps to the skill it was adopted as: its diff and its
    // Adopt both go there.
    let personal = m.repo().canonicalize().unwrap().join("personal/pdf");
    for entry in &originals {
        assert!(entry.adoptable && !entry.changed, "{entry:?}");
        let request = sk1ll_daemon::client::ConflictDiffRequest { id: entry.id.clone(), harness: None, name: None };
        let diff = m.client.conflict_diff(&request).await.unwrap();
        assert_eq!(diff.target.as_ref().and_then(|t| t.canonicalize().ok()), Some(personal.clone()));
    }
    // Routine copies need no one: the report (Fleet's count) has no conflict.
    assert!(m.status().await.report.conflicts.is_empty());
    let adopted_to = m.client.adopt_displaced(originals[0].id.as_deref().unwrap()).await.unwrap();
    assert_eq!(adopted_to.canonicalize().unwrap(), personal);

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

#[tokio::test]
async fn unmanaged_adoption_retries_after_crash_between_publication_and_assignment() {
    use sk1ll_daemon::client::ConflictTarget;
    let server = Server::start().await;
    let mut m = machine("adopt-retry");
    m.join(&server).await;
    m.start_connected().await;
    m.stop();
    write(&m.skills().join("pdf"), "SKILL.md", "original user content");
    let repo = sk1ll_core::Repo::open(m.repo()).unwrap();
    let hs = sk1ll_core::MachineConfig::load(&m.machine_toml()).unwrap().harnesses();
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
    m.write_repo(
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
    m.write_repo("instructions/handmade.md", "Plain rule.\n");
    m.start().await;
    let error = m.client.items().await.unwrap_err();
    assert!(error.message.contains("personal/copy/.sk1ll-id") && error.message.contains("personal/x/.sk1ll-id"));
    // The copy, and the hand-made rule without one.
    assert_eq!(m.client.repair_ids().await.unwrap(), 2);
    let items = m.client.items().await.unwrap();
    let get = |item| items.iter().find(|entry| entry.item == item).unwrap();
    let copy = get(ItemRef::Skill("copy".into()));
    let source = get(ItemRef::Skill("x".into()));
    assert!(copy.id.is_some() && source.id.is_some() && copy.id != source.id);
    assert!(get(ItemRef::Rule("handmade".into())).id.is_some());
    assert_eq!(m.client.repair_ids().await.unwrap(), 0);
}

#[tokio::test]
async fn block_conflict_diff_preserves_surrounding_notes() {
    use sk1ll_daemon::client::ConflictDiffRequest;
    let mut snap = snapshot(1, &[]);
    snap.assignment.harnesses.get_mut(HARNESS).unwrap().new_rules_default = true;
    let mut m = solo("block-diff", "http://127.0.0.1:1", Some(snap));
    let config = m.machine_toml();
    let cfg = std::fs::read_to_string(&config)
        .unwrap()
        .replace("instructions_format = \"rules\"", "instructions_format = \"block\"");
    std::fs::write(config, cfg).unwrap();
    m.write_repo("instructions/style.md", "---\ndescription: Style\n---\nExpected rule\n");
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

    // Overwrite resolves the conflict: the edit is only recovered content
    // now, which still counts as needing the user.
    let target = sk1ll_daemon::client::ConflictTarget { harness: HARNESS.into(), name: "instructions".into() };
    m.client.overwrite(&target).await.unwrap();
    assert_eq!(m.client.conflicts_fresh().await.unwrap(), []);
    let kept: Vec<_> = m.client.recovered().await.unwrap().into_iter().filter(|e| e.changed).collect();
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert!(std::fs::read_to_string(kept[0].path.as_ref().unwrap()).unwrap().contains("Edited rule"));
    assert_eq!(m.status().await.report.conflicts.len(), 1);
    assert!(m.client.overwrite(&target).await.unwrap_err().message.contains("no conflict"));
}

// Standalone tests use the local API and git only; no fleet server is needed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_starts_without_a_repo_and_init_creates_it() {
    use sk1ll_daemon::client::DaemonMode;
    let mut m = machine("solo");
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
    m.write("notes.txt", "keep this");
    assert_eq!(m.client.init().await.unwrap_err().status, Some(409));
    assert_eq!(std::fs::read_to_string(m.repo().join("notes.txt")).unwrap(), "keep this");
    assert!(!m.repo().join(".git").exists());
    std::fs::remove_file(m.repo().join("notes.txt")).unwrap();
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
    let mut m = machine("solo");
    sk1ll_core::Repo::init(&m.repo()).unwrap();
    m.write_repo("personal/pdf/SKILL.md", "original");
    m.start().await;
    eventually(|| async { (m.harness_skill("pdf").as_deref() == Some("original")).then_some(()) }).await;
    m.write_repo("personal/pdf/SKILL.md", "edited");
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
    m.write_repo("personal/new/SKILL.md", "new skill");
    m.client.sync(&Default::default()).await.unwrap();
    assert!(m.harness_skill("pdf").is_some());
    assert!(m.harness_skill("new").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_commits_locally_and_pushes_and_pulls_its_own_origin() {
    let mut m = machine("solo");
    sk1ll_core::Repo::init(&m.repo()).unwrap();
    let bare = m.home.path().join("origin.git");
    git(m.home.path(), &["init", "--quiet", "--bare", "-b", "main", bare.to_str().unwrap()]);
    git(&m.repo(), &["remote", "add", "origin", bare.to_str().unwrap()]);
    m.write_repo("personal/pdf/SKILL.md", "initial");
    m.start().await;
    assert!(m.status().await.has_origin);
    assert!(!m.status().await.draft_commit_complete);
    let commit = m.commit_all().await;
    assert!(!commit.pushed);
    assert!(m.status().await.draft_commit_complete);
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
    m.write_repo("personal/queued/SKILL.md", "local commit");
    let local = m.commit(&["queued"]).await;
    write(&peer, "remote-notes", "a concurrent change");
    git(&peer, &["add", "remote-notes"]);
    git(&peer, &["commit", "-qm", "peer won the race"]);
    git(&peer, &["push", "--quiet", "origin", "main"]);
    let peer_tip = git(&peer, &["rev-parse", "HEAD"]);
    assert!(m.client.push().await.unwrap().pushed);
    assert!(m.log().contains("push rejected"));
    assert_ne!(m.head(), local.sha, "the rebased commit has a new parent");
    assert_eq!(git(&m.repo(), &["rev-parse", "HEAD^"]), peer_tip);
    assert_eq!(git(&bare, &["rev-parse", "HEAD"]), m.head());
    git(&peer, &["pull", "--ff-only"]);
    m.write_repo("personal/pdf/SKILL.md", "local draft");
    write(&peer.join("personal/pdf"), "SKILL.md", "another remote edit");
    git(&peer, &["add", "-A"]);
    git(&peer, &["commit", "--quiet", "-m", "another remote edit"]);
    git(&peer, &["push", "--quiet", "origin", "main"]);
    assert!(matches!(m.client.pull().await.unwrap(), sk1ll_daemon::client::PullResult::Blocked { .. }));
    assert_eq!(m.status().await.headline, MachineStatus::Blocked);
    assert_eq!(std::fs::read_to_string(m.repo().join("personal/pdf/SKILL.md")).unwrap(), "local draft");
}

/// A Baseline that writes a file through a filter driver (Git LFS) leaves
/// the Machine Blocked on it, changes nothing and runs no driver, and the
/// error says why.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pull_through_a_filter_driver_stays_blocked_and_says_why() {
    let mut m = machine("solo");
    sk1ll_core::Repo::init(&m.repo()).unwrap();
    let bare = m.home.path().join("origin.git");
    git(m.home.path(), &["init", "--quiet", "--bare", "-b", "main", bare.to_str().unwrap()]);
    git(&m.repo(), &["remote", "add", "origin", bare.to_str().unwrap()]);
    let ran = m.home.path().join("driver-ran");
    for key in ["clean", "smudge"] {
        git(&m.repo(), &["config", &format!("filter.lfs.{key}"), &format!("touch {}; cat", ran.display())]);
    }
    m.write_repo("personal/pdf/SKILL.md", "initial");
    m.start().await;
    m.commit_all().await;
    assert!(m.client.push().await.unwrap().pushed);
    let peer = m.home.path().join("peer");
    git(m.home.path(), &["clone", "--quiet", bare.to_str().unwrap(), peer.to_str().unwrap()]);
    write(&peer, ".gitattributes", "*.md filter=lfs\n");
    write(&peer.join("personal/pdf"), "SKILL.md", "pdf v2");
    git(&peer, &["-c", "filter.lfs.clean=cat", "add", "-A"]);
    git(&peer, &["commit", "--quiet", "-m", "v2"]);
    git(&peer, &["push", "--quiet", "origin", "main"]);
    let before = m.head();

    let error = m.client.pull().await.unwrap_err().to_string();
    assert!(error.contains("filter driver") && error.contains("personal/pdf/SKILL.md"), "{error}");
    let status = m.status().await;
    assert_eq!(status.headline, MachineStatus::Blocked);
    assert!(status.last_error.as_deref().unwrap_or_default().contains("filter="), "{status:?}");
    assert_eq!(m.head(), before);
    assert!(!m.repo().join(".git/sk1ll-pull.json").exists());
    assert_eq!(std::fs::read_to_string(m.repo().join("personal/pdf/SKILL.md")).unwrap(), "initial");
    assert!(!ran.exists(), "a filter driver ran");
}

/// sk1ll refuses to commit a tree the server would refuse; one committed
/// by hand is refused by the server for good: the push isn't retried after
/// a pull, the error says so, and status and the fleet show it until a fix
/// is committed on top.
#[tokio::test]
async fn a_push_the_server_refuses_for_good_is_reported_until_fixed() {
    let server = Server::start().await;
    let (a, _b, _) = fleet_of_two(&server).await;
    a.write_repo(".gitattributes", "*.bin filter=lfs\n");
    a.write_repo("personal/x/data.bin", "data\n");
    let request = CommitRequest { items: vec!["x".into(), "other:.gitattributes".into()], ..Default::default() };
    let error = a.client.commit(&request).await.unwrap_err().to_string();
    assert!(error.contains("filter driver") && error.contains("personal/x/data.bin"), "{error}");
    let before = server.head();

    git(&a.repo(), &["add", "-A"]);
    git(&a.repo(), &["commit", "--quiet", "-m", "by hand"]);
    // As the server binary's check does (this embedded one runs none).
    let hook = server.data().join("repo.git/hooks/pre-receive");
    let refuse = "#!/bin/sh\nread -r old new ref\nif git cat-file -e \"$new:.gitattributes\" 2>/dev/null; then\n  \
                  echo \"sk1ll-server: refusing $new: the skills repo uses a git filter driver\" >&2\n  exit 1\nfi\n";
    std::fs::write(&hook, refuse).unwrap();
    let error = a.client.push().await.unwrap_err().to_string();
    assert!(error.contains("refused the push for good") && error.contains("filter driver"), "{error}");
    assert_eq!(server.head(), before);
    let status = a.status().await;
    assert!(status.report.push_refused.as_deref().unwrap_or_default().contains("filter driver"), "{status:?}");
    eventually(|| async { server.row("alpha").await?.report?.push_refused.map(drop) }).await;

    std::fs::remove_file(a.repo().join(".gitattributes")).unwrap();
    let fixed = a.commit(&["other:.gitattributes"]).await;
    assert!(fixed.pushed, "{fixed:?}");
    assert!(a.status().await.report.push_refused.is_none());
    eventually(|| async { server.row("alpha").await?.report.filter(|r| r.push_refused.is_none()).map(drop) }).await;
}

/// A pull that stopped half-way (here: at a directory it can't write to)
/// is finished by the daemon's next refresh, without another pull.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refresh_finishes_a_pull_that_stopped_half_way() {
    use sk1ll_daemon::client::{PullResult, RenameItemRequest, SyncRequest};
    use std::os::unix::fs::PermissionsExt;
    let mut m = machine("solo");
    sk1ll_core::Repo::init(&m.repo()).unwrap();
    let bare = m.home.path().join("origin.git");
    git(m.home.path(), &["init", "--quiet", "--bare", "-b", "main", bare.to_str().unwrap()]);
    git(&m.repo(), &["remote", "add", "origin", bare.to_str().unwrap()]);
    m.write_repo("personal/pdf/SKILL.md", "initial");
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
    m.write_repo("personal/pdf/SKILL.md", "saved while pull was pending");
    let pdf_item = ItemRef::Skill("pdf".into());
    eventually(|| async {
        let report = m.status().await.report;
        (report.pull_pending.as_deref() == Some(std::slice::from_ref(&pdf_item))).then_some(())
    })
    .await;
    assert_eq!(m.harness_skill("pdf").as_deref(), Some("initial"));
    assert!(m.status().await.report.drafts.is_empty());
    assert!(matches!(m.client.pull().await.unwrap(), PullResult::Blocked { pending: true, .. }));
    m.write_repo("personal/pdf/SKILL.md", "initial");
    mode(0o755);

    // Any refresh finishes it; this one follows a draft edit.
    m.write_repo("notes.txt", "a draft");
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
    let mut m = machine("solo");
    for d in [".claude", ".kiro", ".cursor"] {
        std::fs::create_dir_all(m.home.path().join(d)).unwrap();
    }
    // This test asks which presets setup detects in an otherwise unconfigured HOME.
    let config = format!(
        "id = \"solo\"\n{}",
        sk1ll_testkit::Harness::copy(HARNESS, m.skills())
            .instructions(m.rules(), sk1ll_testkit::InstructionsFormat::Rules)
            .toml()
    );
    sk1ll_testkit::write(&m.machine_toml(), &config);
    m.start().await;
    let enabled = |choices: &[sk1ll_daemon::client::HarnessChoice], name: &str| {
        choices.iter().find(|c| c.name == name).map(|c| (c.installed, c.enabled))
    };
    let path = m.machine_toml();
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

/// A harness enabled on a joined Machine starts as the Server will assign
/// it, all off: neither its preview nor the sync before the Server's
/// column arrives places anything that column would take out again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_harness_enabled_in_a_fleet_starts_all_off() {
    use sk1ll_daemon::client::{HarnessChange, HarnessChangeRequest};
    let server = Server::start().await;
    let (a, _b, _) = fleet_of_two(&server).await;
    let extra = a.home.path().join("extra/skills");
    let harness = sk1ll_core::Harness {
        skills: Some(extra.to_string_lossy().into_owned()),
        mode: sk1ll_core::LinkMode::Copy,
        ..Default::default()
    };
    let mut add =
        HarnessChangeRequest { fingerprint: None, change: HarnessChange::Set { name: "extra".into(), harness } };
    let preview = a.client.preview_harness(&add).await.unwrap();
    assert!(preview.sync.is_empty(), "{:?}", preview.sync);
    add.fingerprint = Some(preview.fingerprint);
    let applied = a.client.apply_harness(&add).await.unwrap();
    assert!(applied.errors.is_empty(), "{:?}", applied.errors);
    eventually(|| async { a.client.assignment("alpha").await.ok()?.assignment.harnesses.get("extra").cloned() }).await;
    a.client.sync(&Default::default()).await.unwrap();
    assert!(!extra.join("x").exists(), "nothing placed");
    assert!(!extra.join(".sk1ll-tmp/displaced").exists(), "nothing taken out into recovery");

    // Turning everything on (the setup wizard's choice) still places it all.
    a.client.set_harness_assignment("extra", true).await.unwrap();
    eventually(|| async { extra.join("x/SKILL.md").is_file().then_some(()) }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn harnesses_change_after_setup_with_a_preview_of_what_stays() {
    use sk1ll_daemon::client::{HarnessChange, HarnessChangeRequest};
    let mut m = machine("solo");
    sk1ll_core::Repo::init(&m.repo()).unwrap();
    m.write_repo("personal/pdf/SKILL.md", "original");
    write(&m.skills().join("mine"), "SKILL.md", "unmanaged");
    m.start().await;
    eventually(|| async { (m.harness_skill("pdf").as_deref() == Some("original")).then_some(()) }).await;
    let settings = m.client.harness_settings().await.unwrap();
    assert_eq!(settings.path, m.machine_toml());
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
    let mut m = machine("solo");
    let custom = m.home.path().join("custom-skills");
    m = m.with_env("SK1LL_REPO", custom.to_str().unwrap());
    m.start().await;
    assert_eq!(m.status().await.repo, custom);
    m.client.init().await.unwrap();
    assert!(custom.join("sk1ll.toml").is_file());
    assert!(!m.repo().exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_join_refuses_a_draft_before_spending_the_code() {
    let mut m = machine("solo");
    sk1ll_core::Repo::init(&m.repo()).unwrap();
    m.write_repo("personal/pdf/SKILL.md", "user draft");
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
    let mut m = machine("first");
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
    let mut m = machine("solo");
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
    let mut m = machine("solo");
    m.start().await;
    m.client.init().await.unwrap();
    let peer = m.home.path().join("peer");
    sk1ll_core::Repo::init(&peer).unwrap();
    write(&peer.join("personal/pdf"), "SKILL.md", "remote skill");
    git(&peer, &["add", "-A"]);
    git(&peer, &["commit", "--quiet", "-m", "baseline"]);
    git(&m.repo(), &["remote", "add", "origin", peer.to_str().unwrap()]);
    m.write_repo("personal/pdf", "private notes");
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
    let mut m = machine("solo");
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
    let client = Client::new(m.home.socket());
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
    m.write_repo("personal/draft/SKILL.md", "saved during registration");
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
    let mut m = machine("solo");
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

#[tokio::test]
async fn the_local_api_routes_requests_and_rejects_incompatible_clients() {
    use sk1ll_daemon::client::API_VERSION;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut m = machine("api");
    m.start().await;
    assert_eq!(m.status().await.version, sk1ll_core::VERSION);
    let valid = format!("sk1ll-api: {API_VERSION}\r\n");
    let future = format!("sk1ll-api: {}\r\n", API_VERSION + 1);
    for (route, header, status, message) in [
        ("GET /status", valid.as_str(), 200, Some("machine_id")),
        ("GET /status", "", 200, Some("machine_id")),
        ("GET /status", future.as_str(), 426, Some("restart the daemon")),
        ("GET /status", "sk1ll-api: invalid\r\n", 426, Some("speaks invalid")),
        ("POST /status", valid.as_str(), 405, None),
        ("GET /skills", valid.as_str(), 409, Some("no skills repo")),
    ] {
        let request = format!("{route} HTTP/1.1\r\nHost: localhost\r\n{header}Connection: close\r\n\r\n");
        let mut stream = tokio::net::UnixStream::connect(m.client.socket()).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with(&format!("HTTP/1.1 {status}")), "{route}: {response}");
        if let Some(expected) = message {
            let (_, body) = response.split_once("\r\n\r\n").unwrap();
            let body: serde_json::Value = serde_json::from_str(body).unwrap();
            if status == 200 {
                assert_eq!(body[expected], "api");
            } else {
                assert!(body["error"].as_str().unwrap().contains(expected), "{route}: {body}");
            }
        }
    }
    m.client.init().await.unwrap();
    assert_eq!(m.client.editable("missing skill").await.unwrap_err().status, Some(400));
}

#[tokio::test]
async fn the_socket_requires_a_private_directory_and_refuses_overlong_paths() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let m = machine("socket");
    let public = m.home.path().join("public");
    std::fs::create_dir(&public).unwrap();
    std::fs::set_permissions(&public, std::fs::Permissions::from_mode(0o755)).unwrap();
    let link = m.home.path().join("link");
    symlink(&public, &link).unwrap();
    let deep = m.home.path().join("d".repeat(120));
    for (parent, message) in [(&public, "mode is 755"), (&link, "not a symlink"), (&deep, "too long")] {
        let socket = parent.join("daemon.sock");
        let output = m.command().env("SK1LL_DAEMON_SOCKET", &socket).output().unwrap();
        assert!(!output.status.success());
        assert!(sk1ll_testkit::text(&output).contains(message), "{}", sk1ll_testkit::text(&output));
        assert!(!socket.exists());
    }
    assert!(!deep.exists());

    let fresh = m.home.path().join("fresh");
    let socket = fresh.join("daemon.sock");
    let mut m = m.with_env("SK1LL_DAEMON_SOCKET", socket.to_str().unwrap());
    m.client = Client::new(&socket);
    m.start().await;
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&fresh), 0o700);
    assert_eq!(mode(&socket), 0o600);
    assert_eq!(m.status().await.machine_id, "socket");
    let other = machine("other");
    let output = other.command().env("SK1LL_DAEMON_SOCKET", &socket).output().unwrap();
    assert!(!output.status.success());
    assert!(sk1ll_testkit::text(&output).contains("already listening"));
}

#[tokio::test]
async fn skill_file_writes_require_current_personal_or_rule_text() {
    use sk1ll_daemon::client::{ContentQuery, FileQuery, WriteFileRequest};
    use std::os::unix::fs::{PermissionsExt, symlink};

    let server = Server::start().await;
    let mut m = machine("file-writes");
    m.join(&server).await;
    m.start_connected().await;
    let skill = "personal/writing/SKILL.md";
    m.write_repo(skill, "# Original");
    m.write_repo("instructions/style.md", "# Style");
    m.write_repo("imported/other/SKILL.md", "# Import");
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
    m.write_repo(skill, "# External edit");
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
    let mut target = machine("target");
    target.join(&server).await;
    target.start_connected().await;
    let source = machine("source");
    source.join(&server).await;
    server
        .http()
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

#[tokio::test]
async fn render_and_dry_run_use_the_current_draft_without_writing() {
    use sk1ll_daemon::client::{API_VERSION, SyncResult};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut m = machine("preview").with_env("SK1LL_DEBOUNCE", "60s");
    let repo = sk1ll_core::Repo::init(&m.repo()).unwrap();
    m.start().await;
    let draft = "---\nharnesses: [agent]\n---\n\nKeep functions short.\n";
    m.write_repo("instructions/style.md", draft);
    m.write_repo("instructions/other.md", "---\nharnesses: [elsewhere]\n---\n\nSkip this.\n");
    let path = m.home.path().join("agent/rules/style.md");
    let config = sk1ll_core::MachineConfig::load(&m.machine_toml()).unwrap();
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

    m.write_repo("instructions/style.md", &draft.replace("short", "readable"));
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
    server.restart().await;
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
async fn reconnect_replays_harness_rename_before_first_status() {
    use sk1ll_core::harness_rename::{RenameIntent, RenameRequest};
    use sk1ll_proto::{AssignmentOutcome, AssignmentUpdate, AssignmentWrite};

    let server = FakeServer::new().await;
    let mut snap = snapshot(1, &[]);
    let id: sk1ll_core::ItemId = "persisted-x".parse().unwrap();
    snap.assignment.set_skill(HARNESS, &id, true);
    let mut m = solo("solo", &server.url(), Some(snap.clone()));
    m.write_repo("personal/x/.sk1ll-id", "persisted-x");
    let cfg = m.machine_toml();
    // sk1ll's state directory, as the daemon finds it from `XDG_STATE_HOME`.
    let state = m.home.state_dir();
    let authority = sk1ll_core::hash::hash_str(&format!("{}\0solo\0tok", server.url()));
    let intent = RenameIntent::prepare(
        &state,
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
    assert!(!RenameIntent::exists(&state, &cfg).unwrap());
    assert!(m.snapshot().unwrap().assignment.skill_enabled("renamed", &id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_rule_reports_partial_inventory_and_still_pulls() {
    let server = Server::start().await;
    let (mut a, b, _) = fleet_of_two(&server).await;
    a.stop();
    a.write_repo("instructions/unreadable.md", "---\nid: [bad]\n---\nrule\n");
    a.write_repo("personal/ok/SKILL.md", "valid skill");
    a.write_repo("personal/ok/.sk1ll-id", "ok-id");
    a.start_connected().await;
    let first = eventually(|| async {
        let report = server.row("alpha").await?.report?;
        report.inventory_error.as_ref()?.contains("unreadable.md").then_some(report)
    })
    .await;
    assert!(!first.inventory_complete);
    assert!(first.items.iter().any(|item| item.id.to_string() == "ok-id"));
    let response = server
        .http()
        .post(format!("{}/api/assignment/orphans/prune", server.url()))
        .bearer_auth(ADMIN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    b.write_repo("notes", "a change to pull");
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

async fn send(ws: &mut FakeWs, message: ServerMessage) {
    ws.send(Message::text(serde_json::to_string(&message).unwrap())).await.unwrap();
}

async fn assignment_request(ws: &mut FakeWs, seq: &mut u64) -> (u64, sk1ll_proto::AssignmentWrite) {
    loop {
        match next_message(ws).await {
            DaemonMessage::Status(report) => *seq = report.seq,
            DaemonMessage::SetAssignment { req, write } => return (req, write),
            other => panic!("expected an assignment write: {other:?}"),
        }
    }
}

#[tokio::test]
async fn a_slow_assignment_reply_leaves_the_engine_free_and_cannot_replace_a_newer_revision() {
    use sk1ll_proto::{AssignmentOutcome, AssignmentUpdate, AssignmentWrite};
    let server = FakeServer::new().await;
    let mut m = solo("solo", &server.url(), Some(snapshot(1, &[])));
    m.start().await;
    let (mut ws, _) = server.accept().await;
    eventually(|| async { (m.status().await.connection == Connection::Connected).then_some(()) }).await;
    let toggle = || {
        let c = m.client.clone();
        tokio::spawn(async move { c.set_harness_assignment(HARNESS, true).await })
    };
    let write = toggle();
    let mut seq = 0;
    let (req, action) = assignment_request(&mut ws, &mut seq).await;
    assert!(matches!(action, AssignmentWrite::Harness { enabled: true, .. }));
    tokio::time::timeout(Duration::from_secs(5), async {
        m.client.draft().await.unwrap();
        m.client.items_fresh().await.unwrap();
    })
    .await
    .expect("the server's pending confirmation held the engine lock");
    assert!(!write.is_finished());
    send(&mut ws, ServerMessage::Assignment(snapshot(3, &["y"]))).await;
    eventually(|| async { (m.status().await.assignment_revision == 3).then_some(()) }).await;
    let stale = AssignmentUpdate {
        revision: 2,
        assignment: snapshot(2, &["x"]).assignment,
        entries: Vec::new(),
        errors: Vec::new(),
    };
    send(&mut ws, ServerMessage::AssignmentResult { req, seq, result: AssignmentOutcome::Applied(stale) }).await;
    let reply = write.await.unwrap().unwrap();
    assert_eq!(reply.revision, 3);
    assert_eq!(reply.assignment, snapshot(3, &["y"]).assignment);
    assert_eq!(m.snapshot(), Some(snapshot(3, &["y"])));
    assert!(m.harness_skill("y").is_some());
    assert!(m.harness_skill("x").is_none());

    let write = toggle();
    let (req, _) = assignment_request(&mut ws, &mut seq).await;
    send(&mut ws, ServerMessage::Revoked).await;
    let late = AssignmentUpdate {
        revision: 4,
        assignment: snapshot(4, &["z"]).assignment,
        entries: Vec::new(),
        errors: Vec::new(),
    };
    // Revocation closes the session. A response still in flight must have no effect.
    let frame = ServerMessage::AssignmentResult { req, seq, result: AssignmentOutcome::Applied(late) };
    let _ = ws.send(Message::text(serde_json::to_string(&frame).unwrap())).await;
    assert!(write.await.unwrap().is_err());
    eventually(|| async { m.status().await.revoked.then_some(()) }).await;
    assert_eq!(m.snapshot(), Some(snapshot(3, &["y"])));
    assert!(m.harness_skill("z").is_none());
    m.stop();
    m.start().await;
    assert_eq!(m.status().await.connection, Connection::Revoked);
}

#[tokio::test]
async fn adoption_preserves_every_copy_it_no_longer_owns_when_confirmation_arrives() {
    use sk1ll_daemon::client::ConflictTarget;
    use sk1ll_proto::{AssignmentOutcome, AssignmentUpdate, AssignmentWrite};
    let server = FakeServer::new().await;
    let mut m = solo("solo", &server.url(), Some(snapshot(1, &[])))
        .with_env("SK1LL_DEBOUNCE", "1h")
        .with_env("SK1LL_RECONCILE_INTERVAL", "1h");
    m.start().await;
    let (mut ws, _) = server.accept().await;
    eventually(|| async { (m.status().await.connection == Connection::Connected).then_some(()) }).await;
    let mut seq = 0;
    for name in ["unchanged", "repo-edited", "source-edited", "committed", "reidentified", "pending-pull"] {
        let source = m.skills().join(name).join("SKILL.md");
        let draft = m.repo().join("personal").join(name);
        sk1ll_testkit::write(&source, "unmanaged user content");
        let start_adopt = || {
            let client = m.client.clone();
            tokio::spawn(async move {
                client.adopt_unmanaged(&ConflictTarget { harness: HARNESS.into(), name: name.into() }).await
            })
        };
        let adopting = start_adopt();
        let (req, action) = assignment_request(&mut ws, &mut seq).await;
        let AssignmentWrite::Changes(body) = action else { panic!("expected an adoption toggle") };
        assert_eq!(body.changes.len(), 1);
        let change = &body.changes[0];
        assert_eq!(change.name.as_deref(), Some(name));
        assert_eq!(change.enabled, Some(true));
        match name {
            "repo-edited" => sk1ll_testkit::write(&draft.join("SKILL.md"), "later repo edit"),
            "source-edited" => sk1ll_testkit::write(&source, "later harness edit"),
            "committed" => {
                git(&m.repo(), &["add", &format!("personal/{name}")]);
                git(&m.repo(), &["commit", "-qm", "User committed the adopted skill"]);
            }
            "reidentified" => sk1ll_testkit::write(&draft.join(".sk1ll-id"), "later-id\n"),
            "pending-pull" => sk1ll_testkit::write(&m.repo().join(".git/sk1ll-pull.json"), "{}"),
            _ => {}
        };
        let outcome = if name == "pending-pull" {
            let mut assignment = snapshot(3, &[]).assignment;
            assignment.set_skill(HARNESS, change.item_id.as_ref().unwrap(), true);
            AssignmentOutcome::Applied(AssignmentUpdate {
                assignment,
                revision: 3,
                entries: Vec::new(),
                errors: Vec::new(),
            })
        } else {
            AssignmentOutcome::Refused { status: 422, error: "Server refused".into() }
        };
        send(&mut ws, ServerMessage::AssignmentResult { req, seq, result: outcome }).await;
        let error = adopting.await.unwrap().unwrap_err();
        if name == "pending-pull" {
            assert!(error.message.contains("pull") && error.message.contains("kept"), "{error}");
        } else {
            assert!(error.message.contains("Server refused"), "{name}: {error}");
        }
        assert_eq!(
            sk1ll_testkit::read(&source),
            if name == "source-edited" { "later harness edit" } else { "unmanaged user content" },
            "{name}"
        );
        assert!(!source.with_file_name(sk1ll_core::sync::PLACEMENT_MARKER).exists(), "{name}");
        if name == "unchanged" {
            assert!(!draft.exists());
            let retry = start_adopt();
            let (req, action) = assignment_request(&mut ws, &mut seq).await;
            let AssignmentWrite::Changes(body) = action else { panic!("expected an adoption toggle") };
            let mut assignment = snapshot(2, &[]).assignment;
            assignment.set_skill(HARNESS, body.changes[0].item_id.as_ref().unwrap(), true);
            let update = AssignmentUpdate { assignment, revision: 2, entries: Vec::new(), errors: Vec::new() };
            send(&mut ws, ServerMessage::AssignmentResult { req, seq, result: AssignmentOutcome::Applied(update) })
                .await;
            retry.await.unwrap().unwrap();
            assert_indexed_copy(&m.skills(), name);
        } else {
            assert_eq!(
                sk1ll_testkit::read(&draft.join("SKILL.md")),
                if name == "repo-edited" { "later repo edit" } else { "unmanaged user content" },
                "{name}"
            );
            if name == "reidentified" {
                assert_eq!(sk1ll_testkit::read(&draft.join(".sk1ll-id")), "later-id\n");
            }
        }
    }
}

#[tokio::test]
async fn a_disappearing_repo_clears_the_daemons_published_inventory() {
    use sk1ll_testkit::{SkillsRepo, read, write};

    let mut m = machine("disappearing").with_env("SK1LL_RECONCILE_INTERVAL", "150ms");
    SkillsRepo::init(m.repo());
    m.write_repo("personal/pdf/SKILL.md", "PDFs");
    let separate = m.home.path().join("separate.git");
    let pointer = m.repo().join(".git");
    std::fs::rename(&pointer, &separate).unwrap();
    let gitdir = format!("gitdir: {}\n", separate.display());
    write(&pointer, &gitdir);
    m.start().await;
    let inventory_returns = || async {
        let view = m.status().await;
        (view.repo_ready && view.report.items.iter().any(|item| item.item.to_string() == "skill:pdf")).then_some(())
    };
    eventually(inventory_returns).await;
    let moved = m.home.path().join("moved");
    for missing in ["work tree", "git directory", "invalid pointer"] {
        match missing {
            "work tree" => std::fs::rename(m.repo(), &moved).unwrap(),
            "git directory" => std::fs::rename(&separate, &moved).unwrap(),
            _ => write(&pointer, "this is not a git pointer"),
        }
        eventually(|| async { (!m.status().await.repo_ready).then_some(()) }).await;
        assert!(m.status().await.report.items.is_empty(), "{missing}");
        assert!(m.client.items().await.is_err(), "{missing}");
        assert!(m.client.draft().await.is_err(), "{missing}");
        assert!(m.client.conflicts().await.is_err(), "{missing}");
        assert!(m.client.recovered().await.is_err(), "{missing}");
        match missing {
            "work tree" => std::fs::rename(&moved, m.repo()).unwrap(),
            "git directory" => std::fs::rename(&moved, &separate).unwrap(),
            _ => write(&pointer, &gitdir),
        }
        eventually(inventory_returns).await;
    }

    let config = m.machine_toml();
    let original = read(&config);
    write(&config, "invalid = [");
    for _ in 0..2 {
        assert!(m.client.items_fresh().await.is_err());
    }
    write(&config, &original);
    assert!(m.client.items_fresh().await.unwrap().iter().any(|item| item.item.to_string() == "skill:pdf"));
    assert_eq!(m.harness_skill("pdf").as_deref(), Some("PDFs"));
}

#[tokio::test]
async fn recovering_a_connection_during_registration_keeps_the_old_registration_active() {
    use axum::{Json, Router, routing::post};
    use sk1ll_daemon::client::JoinApiRequest;
    use std::sync::Arc;
    use tokio::sync::Notify;

    let server = FakeServer::new().await;
    let mut m = solo("solo", &server.url(), Some(snapshot(1, &["x"])));
    let registered = Arc::new(Notify::new());
    let finish = Arc::new(Notify::new());
    let app = Router::new().route(
        "/api/join",
        post({
            let registered = registered.clone();
            let finish = finish.clone();
            move || {
                let registered = registered.clone();
                let finish = finish.clone();
                async move {
                    registered.notify_one();
                    finish.notified().await;
                    Json(sk1ll_proto::JoinResponse {
                        machine_id: "candidate".into(),
                        token: "candidate-token".into(),
                        repo_path: sk1ll_proto::REPO_PATH.into(),
                        assignment: snapshot(9, &["z"]).assignment,
                        revision: 9,
                    })
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let new_origin = format!("{url}{}", sk1ll_proto::REPO_PATH);
    git(&m.repo(), &["remote", "set-url", "origin", &new_origin]);
    let registration = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    m.start().await;
    server.refuse_401().await;
    eventually(|| async { (m.status().await.connection == Connection::AuthRejected).then_some(()) }).await;
    let old = m.state();
    let before = std::fs::read(m.snapshot_path()).unwrap();
    let joining = tokio::spawn({
        let c = m.client.clone();
        async move {
            c.join(&JoinApiRequest {
                server_url: url,
                code: "once".into(),
                admin_token: None,
                machine_id: None,
                copy_from: None,
                repo: None,
            })
            .await
        }
    });
    tokio::time::timeout(Duration::from_secs(5), registered.notified()).await.expect("join registration did not start");
    let (_ws, _) = server.accept().await;
    eventually(|| async { (m.status().await.connection == Connection::Connected).then_some(()) }).await;
    finish.notify_one();
    let refused = joining.await.unwrap().unwrap_err();
    assert_eq!(refused.status, Some(409));
    assert!(refused.message.contains("works again"), "{refused}");
    assert_eq!(m.state().registration_key(), old.registration_key());
    assert_eq!(std::fs::read(m.snapshot_path()).unwrap(), before);
    assert!(m.harness_skill("x").is_some());
    assert!(m.harness_skill("z").is_none());
    registration.abort();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn desktop_notifications_deliver_conflicts_and_remember_retries_across_restarts() {
    use sk1ll_testkit::{SkillsRepo, read, write};
    use std::os::unix::fs::PermissionsExt;

    let mut m = machine("alerts");
    let root = m.home.path();
    let command = root.join("bin/notify-send");
    let output = root.join("delivered");
    let failed = root.join("failed");
    write(
        &command,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$SK1LL_TEST_NOTIFY_OUTPUT\"\ntest ! -f \"$SK1LL_TEST_NOTIFY_FAIL\"\n",
    );
    std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o755)).unwrap();
    write(&failed, "fail delivery");
    let command_path = format!("{}:{}", command.parent().unwrap().display(), std::env::var("PATH").unwrap());
    m = m
        .with_env("SK1LL_NOTIFICATIONS", "1")
        .with_env("PATH", &command_path)
        .with_env("SK1LL_TEST_NOTIFY_OUTPUT", output.to_str().unwrap())
        .with_env("SK1LL_TEST_NOTIFY_FAIL", failed.to_str().unwrap());
    let history = m.state_path().with_extension("notifications.json");
    let current =
        || std::fs::read_to_string(&history).ok().and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    SkillsRepo::init(m.repo());
    m.write_repo("personal/pdf/SKILL.md", "from the draft");
    m.start().await;
    eventually(|| async { m.harness_skill("pdf") }).await;
    write(&m.skills().join("pdf/SKILL.md"), "user edit");
    let mut retries = eventually(|| async {
        let recorded = current()?;
        (recorded["Conflict"]["failures"] == 1).then_some(recorded)
    })
    .await;
    assert_eq!(retries["Conflict"]["pending"], true);
    assert!(retries["Conflict"]["retry_at"].is_number());
    assert!(retries["Conflict"]["sent_at"].is_null());
    assert_eq!(std::fs::metadata(&history).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(
        read(&output),
        "--app-name=0xSk1ll\n--\nNew harness Conflicts\nskill:pdf in agent. Open 0xSk1ll to resolve.\n"
    );

    m.stop();
    std::fs::remove_file(failed).unwrap();
    // Resume from the stored schedule with the retry already due.
    retries["Conflict"]["retry_at"] = serde_json::json!(0);
    write(&history, &retries.to_string());
    m.start().await;
    let sent = eventually(|| async {
        let recorded = current()?;
        recorded["Conflict"]["sent_at"].is_number().then_some(recorded)
    })
    .await;
    assert_eq!(sent["Conflict"]["failures"], 0);
    assert_eq!(sent["Conflict"]["pending"], false);
    assert!(sent["Conflict"]["retry_at"].is_null());
    assert_eq!(read(&output).matches("New harness Conflicts").count(), 2);

    m.stop();
    m.start().await;
    m.write_repo("personal/second/SKILL.md", "second skill");
    eventually(|| async { m.harness_skill("second") }).await;
    write(&m.skills().join("second/SKILL.md"), "second user edit");
    eventually(|| async {
        let recorded = current()?;
        (recorded["Conflict"]["active"].as_array()?.len() == 2).then_some(())
    })
    .await;
    let output = read(&output);
    assert_eq!(output.matches("New harness Conflicts").count(), 3, "{output}");
    assert!(output.contains("skill:pdf in agent, skill:second in agent"));
}
