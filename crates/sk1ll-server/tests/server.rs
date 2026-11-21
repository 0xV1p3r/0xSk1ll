//! End-to-end tests: a real server on an ephemeral port with a temp data dir,
//! driven over HTTP, WebSocket and git smart HTTP.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{Value, json};
use sk1ll_proto::{
    AssignmentSnapshot, AssignmentUpdate, DaemonMessage, FleetView, ImportsView, ItemId, ItemRef, JoinCode,
    JoinResponse, LogCause, LogEntry, MachineReport, MachineRow, MachineStatus, ReportedItem, Scope, ServerMessage,
    ToggleKind,
};
use sk1ll_server::Config;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

const ADMIN: &str = "admin-secret";

fn id(s: &str) -> ItemId {
    s.parse().unwrap()
}

struct TestServer {
    base: String,
    port: u16,
    http: reqwest::Client,
    _dir: tempfile::TempDir,
}

async fn start() -> TestServer {
    start_with(|_| {}, false).await
}

/// A server with `tune` applied to its config. With `small_buffers` its sockets
/// get tiny kernel send buffers, so a daemon that stops reading stalls the
/// server's sends after a few kilobytes.
async fn start_with(tune: impl FnOnce(&mut Config), small_buffers: bool) -> TestServer {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::new(dir.path().join("data"));
    config.admin_token = Some(ADMIN.into());
    tune(&mut config);
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    if small_buffers {
        // Inherited by accepted sockets.
        socket.set_send_buffer_size(4096).unwrap();
    }
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener = socket.listen(128).unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { sk1ll_server::serve(config, listener).await.unwrap() });
    let server = TestServer { base: format!("http://127.0.0.1:{port}"), port, http: reqwest::Client::new(), _dir: dir };
    for _ in 0..100 {
        if server.http.get(format!("{}/healthz", server.base)).send().await.is_ok() {
            return server;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("server didn't start");
}

impl TestServer {
    fn url(&self, path: &str) -> String {
        format!("{}/api{path}", self.base)
    }

    async fn get(&self, token: &str, path: &str) -> reqwest::Response {
        self.http.get(self.url(path)).bearer_auth(token).send().await.unwrap()
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, token: &str, path: &str) -> T {
        let resp = self.get(token, path).await;
        assert!(resp.status().is_success(), "GET {path}: {}", resp.status());
        resp.json().await.unwrap()
    }

    async fn join_code(&self, token: &str) -> String {
        let resp = self.http.post(self.url("/join-codes")).bearer_auth(token).send().await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        resp.json::<JoinCode>().await.unwrap().code
    }

    /// `POST /join-codes` with `body` as raw JSON (`None`: no body).
    async fn create_code(&self, token: &str, body: Option<&str>) -> reqwest::Response {
        let req = self.http.post(self.url("/join-codes")).bearer_auth(token);
        let req = match body {
            Some(b) => req.header("content-type", "application/json").body(b.to_string()),
            None => req,
        };
        req.send().await.unwrap()
    }

    async fn scoped_code(&self, token: &str, scope: &str) -> JoinCode {
        let resp = self.create_code(token, Some(&json!({ "scope": scope }).to_string())).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        resp.json().await.unwrap()
    }

    /// Joins `id` with a code of `scope` and returns its token.
    async fn join_scoped(&self, id: &str, harnesses: &[&str], scope: &str) -> String {
        let code = self.scoped_code(ADMIN, scope).await.code;
        let resp = self.try_join(&code, id, harnesses).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        resp.json::<JoinResponse>().await.unwrap().token
    }

    async fn try_join(&self, code: &str, id: &str, harnesses: &[&str]) -> reqwest::Response {
        let body = json!({"code": code, "machine_id": id, "harnesses": harnesses});
        self.http.post(self.url("/join")).json(&body).send().await.unwrap()
    }

    /// Joins `id` and returns its token.
    async fn join(&self, id: &str, harnesses: &[&str]) -> String {
        let code = self.join_code(ADMIN).await;
        let resp = self.try_join(&code, id, harnesses).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        resp.json::<JoinResponse>().await.unwrap().token
    }

    async fn try_put_assignment(&self, token: &str, machine: &str, changes: Value) -> reqwest::Response {
        self.http
            .put(self.url(&format!("/machines/{machine}/assignment")))
            .bearer_auth(token)
            .json(&json!({ "changes": changes }))
            .send()
            .await
            .unwrap()
    }

    async fn put_assignment(&self, token: &str, machine: &str, changes: Value) -> AssignmentUpdate {
        let resp = self.try_put_assignment(token, machine, changes).await;
        assert_eq!(resp.status(), StatusCode::OK);
        resp.json().await.unwrap()
    }

    async fn machine_row(&self, id: &str) -> Option<MachineRow> {
        let fleet: FleetView = self.get_json(ADMIN, "/fleet").await;
        fleet.machines.into_iter().find(|m| m.id == id)
    }

    /// Waits until `id`'s stored report is `report`.
    async fn reported(&self, id: &str, report: &MachineReport) {
        eventually(|| async { (self.machine_row(id).await?.report.as_ref() == Some(report)).then_some(()) }).await;
    }

    fn repo_url(&self, id: &str, token: &str) -> String {
        format!("http://{id}:{token}@127.0.0.1:{}/git/repo.git", self.port)
    }
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn ws_connect(server: &TestServer, token: &str) -> Ws {
    let url = format!("ws://127.0.0.1:{}/api/ws", server.port);
    let mut url = url.into_client_request().unwrap();
    url.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
    tokio_tungstenite::connect_async(url).await.unwrap().0
}

/// Connects with a tiny receive buffer, so not reading soon stalls the server.
async fn ws_connect_small(server: &TestServer, token: &str) -> Ws {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(4096).unwrap();
    let tcp = socket.connect(format!("127.0.0.1:{}", server.port).parse().unwrap()).await.unwrap();
    let url = format!("ws://127.0.0.1:{}/api/ws", server.port);
    let mut url = url.into_client_request().unwrap();
    url.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
    let stream = tokio_tungstenite::MaybeTlsStream::Plain(tcp);
    tokio_tungstenite::client_async(url, stream).await.unwrap().0
}

/// The next assignment, skipping other messages.
async fn ws_assignment(ws: &mut Ws) -> AssignmentSnapshot {
    loop {
        if let ServerMessage::Assignment(a) = ws_next(ws).await {
            return a;
        }
    }
}

async fn ws_next(ws: &mut Ws) -> ServerMessage {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), ws.next()).await.expect("ws timeout");
        match msg.expect("ws closed").unwrap() {
            Message::Text(t) => return serde_json::from_str(&t).unwrap(),
            Message::Close(_) => panic!("ws closed"),
            _ => {}
        }
    }
}

async fn ws_send(ws: &mut Ws, msg: &DaemonMessage) {
    ws.send(Message::text(serde_json::to_string(msg).unwrap())).await.unwrap();
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

async fn git_async(dir: PathBuf, args: Vec<String>) -> String {
    tokio::task::spawn_blocking(move || git(&dir, &args.iter().map(String::as_str).collect::<Vec<_>>())).await.unwrap()
}

/// A fresh clone of the server repo in a temp dir.
async fn clone(server: &TestServer, id: &str, token: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("repo").to_string_lossy().into_owned();
    git_async(dir.path().into(), vec!["clone".into(), "--quiet".into(), server.repo_url(id, token), dest]).await;
    git_async(dir.path().join("repo"), vec!["checkout".into(), "--quiet".into(), "-B".into(), "main".into()]).await;
    dir
}

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// Commits and pushes, then waits for the server to publish the baseline and
/// its item catalogue before subsequent API calls use them. Returns HEAD.
async fn commit_push(server: &TestServer, repo: &Path, msg: &str) -> String {
    let r = repo.to_path_buf();
    git_async(r.clone(), vec!["add".into(), "-A".into()]).await;
    git_async(r.clone(), vec!["commit".into(), "--quiet".into(), "-m".into(), msg.into()]).await;
    git_async(r.clone(), vec!["push".into(), "--quiet".into(), "origin".into(), "HEAD:main".into()]).await;
    let head = git_async(r, vec!["rev-parse".into(), "HEAD".into()]).await;
    eventually(|| async {
        let fleet: FleetView = server.get_json(ADMIN, "/fleet").await;
        (fleet.baseline.as_deref() == Some(head.as_str())).then_some(())
    })
    .await;
    head
}

/// A fresh clone with `files` pushed as the baseline; returns it and HEAD.
async fn seed(server: &TestServer, id: &str, token: &str, files: &[&str]) -> (tempfile::TempDir, String) {
    let dir = clone(server, id, token).await;
    let repo = dir.path().join("repo");
    for f in files {
        write(&repo, f, "x\n");
    }
    let head = commit_push(server, &repo, "seed").await;
    (dir, head)
}

/// Long draft skill names, so that an assignment enabling them all is big.
fn fat_names(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("s{i:04}-{}", "x".repeat(100))).collect()
}

/// Makes `machine`'s assignment a few hundred KB: reports many draft skills
/// over `ws` and enables them all. A daemon that doesn't read stalls on it.
async fn fatten(server: &TestServer, ws: &mut Ws, machine: &str) {
    let names = fat_names(2000);
    let report = MachineReport {
        seq: 1,
        drafts: names.iter().map(|n| ItemRef::Skill(n.clone())).collect(),
        harnesses: vec!["claude".into()],
        ..Default::default()
    };
    ws_send(ws, &DaemonMessage::Status(report.clone())).await;
    server.reported(machine, &report).await;
    let changes: Vec<Value> =
        names.iter().map(|n| json!({"harness": "claude", "kind": "skill", "item_id": n, "enabled": true})).collect();
    server.put_assignment(ADMIN, machine, Value::Array(changes)).await;
}

/// Polls `f` until it returns `Some`.
async fn eventually<T, F, Fut>(mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    for _ in 0..200 {
        if let Some(v) = f().await {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("condition never held");
}

#[tokio::test(flavor = "multi_thread")]
async fn fresh_server_has_one_root_commit_with_standard_skeleton() {
    let server = start().await;
    let bare = server._dir.path().join("data/repo.git");
    let head = git(&bare, &["rev-parse", "refs/heads/main"]);
    assert_eq!(git(&bare, &["rev-list", "--count", "HEAD"]), "1");
    assert_eq!(git(&bare, &["rev-list", "--parents", "HEAD"]), head, "the initial commit must have no parents");
    assert_eq!(
        git(&bare, &["ls-tree", "-r", "--name-only", "HEAD"]),
        ".gitignore\nimported/.gitkeep\ninstructions/.gitkeep\npersonal/.gitkeep\nsk1ll.toml"
    );
    assert_eq!(
        git(&bare, &["show", "-s", "--format=%an <%ae>%n%cn <%ce>", "HEAD"]),
        "server <sk1ll@server>\nserver <sk1ll@server>"
    );
    let fleet: FleetView = server.get_json(ADMIN, "/fleet").await;
    assert_eq!(fleet.baseline.as_deref(), Some(head.as_str()));
    let imports: ImportsView = eventually(|| async {
        let view: ImportsView = server.get_json(ADMIN, "/imports").await;
        view.checked_at.map(|_| view)
    })
    .await;
    assert!(imports.errors.is_empty() && imports.updates.is_empty());
    let health = server.http.get(format!("{}/healthz", server.base)).send().await.unwrap();
    assert_eq!(health.status(), StatusCode::OK);
    let health = health.json::<Value>().await.unwrap();
    assert_eq!((&health["status"], &health["push_degraded"]), (&json!("ok"), &json!(false)));
    assert_eq!(health["protocol"], json!(sk1ll_proto::PROTOCOL_VERSION));
}

#[tokio::test]
async fn requests_from_another_protocol_version_are_refused() {
    let server = start().await;
    let header = sk1ll_proto::PROTOCOL_HEADER;
    let fleet = |version: Option<&str>| {
        let req = server.http.get(server.url("/fleet")).bearer_auth(ADMIN);
        let req = if let Some(v) = version { req.header(header, v) } else { req };
        async move { req.send().await.unwrap() }
    };
    for version in [None, Some("1")] {
        let resp = fleet(version).await;
        assert_eq!(resp.status(), StatusCode::OK, "{version:?}");
        assert_eq!(resp.headers()[header], sk1ll_proto::PROTOCOL_VERSION.to_string());
    }
    let refused = fleet(Some("2")).await;
    assert_eq!(refused.status(), StatusCode::UPGRADE_REQUIRED);
    assert_eq!(refused.headers()[header], "1");
    let body: Value = refused.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("speaks version 1, this machine speaks 2"), "{body}");

    let mut ws = format!("ws://127.0.0.1:{}/api/ws", server.port).into_client_request().unwrap();
    ws.headers_mut().insert("Authorization", format!("Bearer {ADMIN}").parse().unwrap());
    ws.headers_mut().insert(header, "2".parse().unwrap());
    match tokio_tungstenite::connect_async(ws).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status(), StatusCode::UPGRADE_REQUIRED.as_u16());
        }
        other => panic!("expected a refused upgrade, got {:?}", other.map(|_| ())),
    }
}

#[tokio::test]
async fn restarting_server_preserves_initial_commit() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config::new(dir.path().join("data"));
    let server = sk1ll_server::Server::open(config.clone()).await.unwrap();
    let initial = git(&config.repo_dir(), &["rev-parse", "HEAD"]);
    drop(server);
    let _restarted = sk1ll_server::Server::open(config.clone()).await.unwrap();
    assert_eq!(git(&config.repo_dir(), &["rev-parse", "HEAD"]), initial);
    assert_eq!(git(&config.repo_dir(), &["rev-list", "--all", "--count"]), "1");
}

#[tokio::test]
async fn existing_empty_server_gets_initial_commit() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config::new(dir.path().join("data"));
    std::fs::create_dir_all(&config.data_dir).unwrap();
    git(&config.data_dir, &["init", "--quiet", "--bare", "--initial-branch=legacy", "repo.git"]);
    git(&config.repo_dir(), &["config", "http.receivepack", "true"]);
    let _server = sk1ll_server::Server::open(config.clone()).await.unwrap();
    assert_eq!(git(&config.repo_dir(), &["rev-list", "--all", "--count"]), "1");
    git(&config.repo_dir(), &["cat-file", "-e", "main:sk1ll.toml"]);
    assert_eq!(git(&config.repo_dir(), &["symbolic-ref", "HEAD"]), "refs/heads/main");
}

/// Review finding: the branch that takes pushes is decided after bootstrap,
/// so an empty repo whose unborn HEAD named another branch accepts pushes to
/// the `main` that bootstrap created.
#[tokio::test(flavor = "multi_thread")]
async fn existing_empty_repo_on_another_branch_takes_pushes_to_main() {
    let server = start_with(
        |c| {
            std::fs::create_dir_all(&c.data_dir).unwrap();
            git(&c.data_dir, &["init", "--quiet", "--bare", "--initial-branch=legacy", "repo.git"]);
        },
        false,
    )
    .await;
    let token = server.join("writer", &[]).await;
    let (_dir, head) = seed(&server, "writer", &token, &["personal/pdf/SKILL.md"]).await;
    let bare = server._dir.path().join("data/repo.git");
    assert_eq!(git(&bare, &["rev-parse", "refs/heads/main"]), head);
}

/// Review finding: the server refuses to start without a baseline branch,
/// rather than serving one branch and taking pushes to another.
#[tokio::test]
async fn startup_refuses_a_head_that_names_no_branch() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config::new(dir.path().join("data"));
    drop(sk1ll_server::Server::open(config.clone()).await.unwrap());
    let repo = config.repo_dir();
    let main = git(&repo, &["rev-parse", "main"]);
    git(&repo, &["update-ref", "--no-deref", "HEAD", &main]);
    let err = sk1ll_server::Server::open(config.clone()).await.err().expect("a detached HEAD must refuse startup");
    assert!(format!("{err:#}").contains("HEAD is detached"), "{err:#}");
    assert!(format!("{err:#}").contains("symbolic-ref HEAD refs/heads/"), "{err:#}");
    git(&repo, &["tag", "v1", &main]);
    git(&repo, &["symbolic-ref", "HEAD", "refs/tags/v1"]);
    let err = sk1ll_server::Server::open(config.clone()).await.err().expect("a tag HEAD must refuse startup");
    assert!(format!("{err:#}").contains("not a branch under refs/heads/"), "{err:#}");
    git(&repo, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    sk1ll_server::Server::open(config).await.unwrap();
}

#[tokio::test]
async fn bootstrap_ignores_inherited_git_configuration_and_identity() {
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("gitconfig");
    std::fs::write(&global, "[commit]\n\tgpgsign = true\n[init]\n\ttemplateDir = /missing-server-test-template\n")
        .unwrap();
    let data = dir.path().join("data");
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_sk1ll-server"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("SK1LL_SERVER_DATA", &data)
        .env("SK1LL_SERVER_BIND", "127.0.0.1:0")
        .env("GIT_CONFIG_GLOBAL", global)
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "commit.gpgsign")
        .env("GIT_CONFIG_VALUE_0", "true")
        .env("GIT_AUTHOR_NAME", "Wrong author")
        .env("GIT_AUTHOR_EMAIL", "wrong@example.com")
        .env("GIT_COMMITTER_NAME", "Wrong committer")
        .env("GIT_COMMITTER_EMAIL", "wrong@example.com")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    eventually(|| async { data.join("repo.git/refs/heads/main").is_file().then_some(()) }).await;
    child.kill().await.unwrap();
    let bare = data.join("repo.git");
    assert_eq!(
        git(&bare, &["show", "-s", "--format=%an <%ae>%n%cn <%ce>", "HEAD"]),
        "server <sk1ll@server>\nserver <sk1ll@server>"
    );
    git(&bare, &["cat-file", "-e", "HEAD:sk1ll.toml"]);
}

#[tokio::test]
async fn nonempty_server_without_main_is_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config::new(dir.path().join("data"));
    drop(sk1ll_server::Server::open(config.clone()).await.unwrap());
    git(&config.repo_dir(), &["branch", "-m", "main", "custom"]);
    let before = git(&config.repo_dir(), &["show-ref", "--head"]);
    let _server = sk1ll_server::Server::open(config.clone()).await.unwrap();
    assert_eq!(git(&config.repo_dir(), &["show-ref", "--head"]), before);
    assert_eq!(git(&config.repo_dir(), &["symbolic-ref", "HEAD"]), "refs/heads/custom");
}

#[tokio::test(flavor = "multi_thread")]
async fn offline_clones_share_a_root_and_can_rebase_then_push() {
    let server = start().await;
    let a = server.join("a", &[]).await;
    let b = server.join("b", &[]).await;
    let a_dir = clone(&server, "a", &a).await;
    let b_dir = clone(&server, "b", &b).await;
    let a_repo = a_dir.path().join("repo");
    let b_repo = b_dir.path().join("repo");
    let root = git(&a_repo, &["rev-parse", "HEAD"]);
    assert_eq!(git(&b_repo, &["rev-parse", "HEAD"]), root);
    // Both machines commit before either pushes, using local identities.
    for (repo, name) in [(&a_repo, "a"), (&b_repo, "b")] {
        git(repo, &["config", "user.name", name]);
        git(repo, &["config", "user.email", &format!("{name}@example.com")]);
        write(repo, &format!("personal/{name}/SKILL.md"), name);
        git(repo, &["add", "-A"]);
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["commit", "--quiet", "-m", name])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(git(repo, &["show", "-s", "--format=%an <%ae>", "HEAD"]), format!("{name} <{name}@example.com>"));
    }
    git_async(a_repo, vec!["push".into(), "--quiet".into(), "origin".into(), "HEAD:main".into()]).await;
    git_async(b_repo.clone(), vec!["fetch".into(), "--quiet".into(), "origin".into()]).await;
    git(&b_repo, &["rebase", "origin/main"]);
    git_async(b_repo.clone(), vec!["push".into(), "--quiet".into(), "origin".into(), "HEAD:main".into()]).await;
    let bare = server._dir.path().join("data/repo.git");
    assert_eq!(git(&bare, &["rev-parse", "HEAD"]), git(&b_repo, &["rev-parse", "HEAD"]));
    assert_eq!(git(&bare, &["rev-list", "--max-parents=0", "HEAD"]), root);
    assert_eq!(git(&bare, &["rev-list", "--count", "HEAD"]), "3");
    for name in ["a", "b"] {
        git(&bare, &["cat-file", "-e", &format!("HEAD:personal/{name}/SKILL.md")]);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn join_codes_are_single_use() {
    let server = start().await;
    assert_eq!(server.http.post(server.url("/join-codes")).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
    assert_eq!(server.get("wrong", "/fleet").await.status(), StatusCode::UNAUTHORIZED);

    let code = server.join_code(ADMIN).await;
    let resp = server.try_join(&code.to_lowercase(), "laptop", &["claude", "codex"]).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let joined: JoinResponse = resp.json().await.unwrap();
    assert_eq!(joined.repo_path, "/git/repo.git");
    let claude = joined.assignment.harness("claude");
    assert!(!claude.new_skills_default && !claude.new_rules_default);

    assert_eq!(server.try_join(&code, "desktop", &[]).await.status(), StatusCode::FORBIDDEN);
    assert_eq!(server.try_join("NOPE0-NOPE0", "desktop", &[]).await.status(), StatusCode::FORBIDDEN);

    // A joined machine can invite the next one; a taken id doesn't burn the code.
    let code = server.join_code(&joined.token).await;
    assert_eq!(server.try_join(&code, "laptop", &[]).await.status(), StatusCode::CONFLICT);
    assert_eq!(server.try_join(&code, "desktop", &[]).await.status(), StatusCode::CREATED);

    let fleet: FleetView = server.get_json(&joined.token, "/fleet").await;
    let ids: Vec<_> = fleet.machines.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["desktop", "laptop"]);
    assert!(fleet.machines.iter().all(|m| m.status == MachineStatus::Offline && !m.connected));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_expected_revision_and_registration_guard_a_write() {
    let server = start().await;
    let token = server.join("ws", &["claude"]).await;
    seed(&server, "ws", &token, &["personal/pdf/SKILL.md"]).await;
    let put = |body: Value| {
        let request = server.http.put(server.url("/machines/ws/assignment")).bearer_auth(ADMIN).json(&body);
        async move { request.send().await.unwrap() }
    };
    let snapshot: AssignmentSnapshot = server.get_json(ADMIN, "/machines/ws/assignment").await;
    let registration = snapshot.registration.clone().expect("a registration");
    assert_eq!(server.machine_row("ws").await.unwrap().registration, registration);
    let change = json!([{"harness": "claude", "kind": "skill", "item_id": "pdf", "enabled": false}]);
    let expected = |revision: u64, registration: &str| json!({"changes": change, "expected": {"revision": revision, "registration": registration}});
    let resp = put(expected(snapshot.revision, &registration)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let update: AssignmentUpdate = resp.json().await.unwrap();
    // The write moved the revision on: the same precondition no longer holds.
    let resp = put(expected(snapshot.revision, &registration)).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert!(resp.text().await.unwrap().contains("changed since revision"));

    // A new join of the same id is another registration, even within the second.
    let resp = server.http.delete(server.url("/machines/ws")).bearer_auth(ADMIN).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    server.join("ws", &["claude"]).await;
    let again: AssignmentSnapshot = server.get_json(ADMIN, "/machines/ws/assignment").await;
    assert_ne!(again.registration.as_ref(), Some(&registration));
    let resp = put(expected(again.revision, &registration)).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert!(resp.text().await.unwrap().contains("joined the fleet again"));
    let resp = put(expected(again.revision, again.registration.as_deref().unwrap())).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(update.revision < again.revision);
}

#[tokio::test(flavor = "multi_thread")]
async fn assignment_changes_are_logged_and_undoable() {
    let server = start().await;
    let token = server.join("ws", &["claude"]).await;
    seed(&server, "ws", &token, &["personal/pdf/SKILL.md"]).await;

    let update = server
        .put_assignment(
            &token,
            "ws",
            json!([
                {"harness": "claude", "kind": "skill", "item_id": "pdf", "enabled": true},
                {"harness": "claude", "kind": "new_rules_default", "enabled": true},
                {"harness": "claude", "kind": "new_rules_default", "enabled": true},
            ]),
        )
        .await;
    assert!(update.assignment.skill_enabled("claude", &id("pdf")));
    assert_eq!(update.entries.len(), 2, "the repeated change is a no-op");
    let e = &update.entries[0];
    assert_eq!((e.author.as_str(), e.machine.as_str(), e.kind), ("ws", "ws", ToggleKind::Skill));
    assert_eq!((e.before, e.after, &e.cause), (None, Some(true), &LogCause::Change));

    let bad = server
        .http
        .put(server.url("/machines/ws/assignment"))
        .bearer_auth(&token)
        .json(&json!({"changes": [{"harness": "claude", "kind": "skill", "enabled": true}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);

    let log: Vec<LogEntry> = server.get_json(ADMIN, "/log?machine=ws").await;
    assert_eq!(log.len(), 2);
    assert_eq!(log[1], *e, "newest first");

    let resp = server.http.post(server.url(&format!("/log/{}/undo", e.id))).bearer_auth(ADMIN).send().await.unwrap();
    let undo: AssignmentUpdate = resp.json().await.unwrap();
    let u = &undo.entries[0];
    assert_eq!((u.author.as_str(), u.before, u.after), ("admin", Some(true), None));
    assert_eq!(u.cause, LogCause::Undo { entry: e.id });
    let a = server.get_json::<AssignmentSnapshot>(&token, "/machines/ws/assignment").await.assignment;
    assert!(!a.harness("claude").skills.contains_key(&id("pdf")));
    assert!(a.harness("claude").new_rules_default);

    let again = server.http.post(server.url(&format!("/log/{}/undo", e.id))).bearer_auth(ADMIN).send().await.unwrap();
    assert!(again.json::<AssignmentUpdate>().await.unwrap().entries.is_empty());
    assert_eq!(server.get(&token, "/machines/nope/assignment").await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn log_filters_and_facets_cover_the_whole_log() {
    let server = start().await;
    let token = server.join("ws", &["claude", "codex"]).await;
    seed(&server, "ws", &token, &["personal/pdf/SKILL.md"]).await;
    let pdf = server
        .put_assignment(&token, "ws", json!([{"harness": "codex", "kind": "skill", "item_id": sk1ll_proto::ItemId::derived("skill", "pdf"), "enabled": true}]))
        .await
        .entries[0]
        .clone();
    let changed = server
        .put_assignment(ADMIN, "ws", json!([{"harness": "claude", "kind": "new_rules_default", "enabled": true}]))
        .await;
    let newest = changed.entries.last().unwrap().id;

    // The match is older than a one-entry page of everything.
    let page: Vec<LogEntry> = server.get_json(ADMIN, "/log?limit=1").await;
    assert_eq!(page[0].id, newest);
    let found: Vec<LogEntry> = server.get_json(ADMIN, "/log?item=Skill%3APD&limit=1").await;
    assert_eq!(found, std::slice::from_ref(&pdf));
    let found: Vec<LogEntry> = server.get_json(ADMIN, &format!("/log?harness=codex&author=ws&before={newest}")).await;
    assert_eq!(found, std::slice::from_ref(&pdf));
    let none: Vec<LogEntry> = server.get_json(ADMIN, &format!("/log?item=pdf&before={}", pdf.id)).await;
    assert!(none.is_empty());
    let admin: Vec<LogEntry> = server.get_json(ADMIN, "/log?author=admin&machine=ws").await;
    assert!(!admin.is_empty() && admin.iter().all(|e| e.author == "admin" && e.harness == "claude"));

    let facets: Value = server.get_json(&token, "/log/facets").await;
    assert_eq!(facets["machines"], json!(["ws"]));
    assert_eq!(facets["harnesses"], json!(["claude", "codex"]));
    let authors = facets["authors"].as_array().unwrap();
    assert!(authors.contains(&json!("ws")) && authors.contains(&json!("admin")), "{authors:?}");
    assert_eq!(server.http.get(server.url("/log/facets")).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread")]
async fn websocket_gets_assignment_nudge_and_status() {
    let server = start().await;
    let token = server.join("ws", &["claude"]).await;
    let mut ws = ws_connect(&server, &token).await;
    let ServerMessage::Assignment(a) = ws_next(&mut ws).await else { panic!("expected assignment first") };
    assert!(!a.assignment.harness("claude").new_skills_default);
    assert!(a.revision > 0);
    let fleet: FleetView = server.get_json(&token, "/fleet").await;
    assert_eq!(ws_next(&mut ws).await, ServerMessage::Nudge { baseline: fleet.baseline.unwrap() });

    ws_send(&mut ws, &DaemonMessage::Hello { machine_id: "ws".into() }).await;
    let report = MachineReport { seq: 1, harnesses: vec!["claude".into(), "codex".into()], ..Default::default() };
    ws_send(&mut ws, &DaemonMessage::Status(report)).await;
    // `codex` was new, so the server adds it all off and pushes the assignment.
    let ServerMessage::Assignment(b) = ws_next(&mut ws).await else { panic!("expected assignment") };
    let codex = b.assignment.harness("codex");
    assert!(!codex.new_skills_default && !codex.new_rules_default);
    assert!(b.revision > a.revision);

    let dir = clone(&server, "ws", &token).await;
    let repo = dir.path().join("repo");
    write(&repo, "personal/pdf/SKILL.md", "---\nname: pdf\n---\n");
    let head = commit_push(&server, &repo, "add pdf").await;
    assert_eq!(ws_next(&mut ws).await, ServerMessage::Nudge { baseline: head.clone() });

    server
        .put_assignment(
            &token,
            "ws",
            json!([{"harness": "claude", "kind": "skill", "item_id": "pdf", "enabled": true}]),
        )
        .await;
    let ServerMessage::Assignment(a) = ws_next(&mut ws).await else { panic!("expected assignment") };
    assert!(a.assignment.skill_enabled("claude", &id("pdf")));

    let fleet: FleetView = server.get_json(&token, "/fleet").await;
    assert_eq!(fleet.baseline.as_deref(), Some(head.as_str()));
    assert!(fleet.machines[0].connected);
    assert_eq!(fleet.machines[0].status, MachineStatus::Behind);

    let report = MachineReport {
        seq: 2,
        baseline: Some(head.clone()),
        drafts: vec![ItemRef::Skill("new".into())],
        harnesses: vec!["claude".into(), "codex".into()],
        ..Default::default()
    };
    ws_send(&mut ws, &DaemonMessage::Status(report)).await;
    let status = eventually(|| async {
        let fleet: FleetView = server.get_json(&token, "/fleet").await;
        Some(fleet.machines[0].status).filter(|s| *s != MachineStatus::Behind)
    })
    .await;
    assert_eq!(status, MachineStatus::Drafting);

    // A reconnecting daemon learns the current baseline right away.
    drop(ws);
    let mut ws = ws_connect(&server, &token).await;
    assert!(matches!(ws_next(&mut ws).await, ServerMessage::Assignment(_)));
    assert_eq!(ws_next(&mut ws).await, ServerMessage::Nudge { baseline: head });
}

#[tokio::test(flavor = "multi_thread")]
async fn removing_a_machine_revokes_it() {
    let server = start().await;
    let a_token = server.join("a", &["claude"]).await;
    let b_token = server.join("b", &["claude"]).await;
    seed(&server, "b", &b_token, &["personal/pdf/SKILL.md"]).await;
    server
        .put_assignment(
            &a_token,
            "a",
            json!([{"harness": "claude", "kind": "skill", "item_id": "pdf", "enabled": true}]),
        )
        .await;
    let mut ws = ws_connect(&server, &a_token).await;
    assert!(matches!(ws_next(&mut ws).await, ServerMessage::Assignment(_)));
    assert!(matches!(ws_next(&mut ws).await, ServerMessage::Nudge { .. }));

    let resp = server.http.delete(server.url("/machines/a")).bearer_auth(&b_token).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(ws_next(&mut ws).await, ServerMessage::Revoked);

    assert_eq!(server.get(&a_token, "/fleet").await.status(), StatusCode::UNAUTHORIZED);
    let git_resp = server.http.get(format!("{}/git/repo.git/info/refs?service=git-upload-pack", server.base));
    let git_resp = git_resp.basic_auth("a", Some(&a_token)).send().await.unwrap();
    assert_eq!(git_resp.status(), StatusCode::UNAUTHORIZED);
    let ok = server.http.get(format!("{}/git/repo.git/info/refs?service=git-upload-pack", server.base));
    assert_eq!(ok.basic_auth("b", Some(&b_token)).send().await.unwrap().status(), StatusCode::OK);

    let log: Vec<LogEntry> = server.get_json(&b_token, "/log?machine=a").await;
    let removed: Vec<_> = log.iter().filter(|e| e.cause == LogCause::MachineRemoved).collect();
    assert_eq!(removed.len(), 3, "two defaults and one toggle");
    assert!(removed.iter().all(|e| e.author == "b" && e.after.is_none()));
    let undo = server.http.post(server.url(&format!("/log/{}/undo", removed[0].id))).bearer_auth(&b_token);
    assert_eq!(undo.send().await.unwrap().status(), StatusCode::CONFLICT);

    let fleet: FleetView = server.get_json(&b_token, "/fleet").await;
    assert_eq!(fleet.machines.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn imports_with_newer_upstream_are_reported() {
    // The upstream is a local repo; only tests allow the `file` transport.
    let server = start_with(|c| c.import_protocols.push_str(":file"), false).await;
    let token = server.join("ws", &[]).await;
    let (locked, latest, _upstream) = push_moved_import(&server, "ws", &token).await;

    let view = eventually(|| async {
        let v: ImportsView = server.get_json(&token, "/imports").await;
        Some(v).filter(|v| !v.updates.is_empty())
    })
    .await;
    assert_eq!(view.updates.len(), 1);
    let u = &view.updates[0];
    assert_eq!((u.name.as_str(), u.locked.as_str(), u.upstream.as_str()), ("pdf", locked.as_str(), latest.as_str()));
    assert!(view.errors.is_empty(), "{:?}", view.errors);
    assert!(view.skipped.is_empty(), "{:?}", view.skipped);
}

/// Review finding: a replaced socket whose writer is stuck must not survive
/// removing the machine and keep reporting for its re-joined successor.
/// Pushes a `pdf` import of a local upstream whose `main` moved past the
/// locked commit; returns (locked, latest).
async fn push_moved_import(server: &TestServer, id: &str, token: &str) -> (String, String, tempfile::TempDir) {
    let up = tempfile::tempdir().unwrap();
    let upstream = up.path().to_path_buf();
    git(&upstream, &["init", "--quiet", "--initial-branch=main"]);
    write(&upstream, "SKILL.md", "v1\n");
    git(&upstream, &["add", "-A"]);
    git(&upstream, &["commit", "--quiet", "-m", "v1"]);
    let locked = git(&upstream, &["rev-parse", "HEAD"]);
    write(&upstream, "SKILL.md", "v2\n");
    git(&upstream, &["commit", "--quiet", "-am", "v2"]);
    let latest = git(&upstream, &["rev-parse", "HEAD"]);

    let dir = clone(server, id, token).await;
    let repo = dir.path().join("repo");
    let source = format!("file://{}", upstream.display());
    write(&repo, "sk1ll.toml", &format!("[import.pdf]\nsource = \"{source}\"\nref = \"main\"\n"));
    write(
        &repo,
        "sk1ll.lock",
        &format!(
            "[skills.pdf]\nsource = \"{source}\"\npath = \"\"\nref = \"main\"\ncommit = \"{locked}\"\nhash = \"x\"\n"
        ),
    );
    write(&repo, "imported/pdf/SKILL.md", "v1\n");
    commit_push(server, &repo, "import pdf").await;
    (locked, latest, up)
}

#[tokio::test(flavor = "multi_thread")]
async fn import_checks_can_be_turned_off_or_limited_to_hosts() {
    for (tune, want) in [
        (
            Box::new(|c: &mut Config| c.import_checks = false) as Box<dyn FnOnce(&mut Config) + Send>,
            "SK1LL_SERVER_IMPORT_CHECKS=off",
        ),
        (Box::new(|c: &mut Config| c.import_hosts = Some(vec!["github.com".into()])), "can't tell this source's host"),
    ] {
        let server = start_with(
            |c| {
                c.import_protocols.push_str(":file");
                tune(c);
            },
            false,
        )
        .await;
        let token = server.join("ws", &[]).await;
        let _upstream = push_moved_import(&server, "ws", &token).await;
        let view = eventually(|| async {
            let v: ImportsView = server.get_json(&token, "/imports").await;
            Some(v).filter(|v| !v.skipped.is_empty())
        })
        .await;
        assert_eq!(view.skipped.len(), 1);
        assert_eq!(view.skipped[0].name, "pdf");
        assert!(view.skipped[0].reason.contains(want), "{}", view.skipped[0].reason);
        assert!(view.updates.is_empty() && view.errors.is_empty(), "{view:?}");
    }
}

/// Review finding: git decodes ssh URLs before it finds the host, so a
/// percent-encoded `/` in the userinfo must not let a source past the
/// allowlist. A recording `ssh` shows which hosts the check contacts.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn encoded_userinfo_cannot_reach_an_unlisted_host() {
    use std::os::unix::fs::PermissionsExt;
    let stub = tempfile::tempdir().unwrap();
    let log = stub.path().join("ssh.log");
    let ssh = stub.path().join("ssh");
    std::fs::write(&ssh, format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 1\n", log.display())).unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let server = start_with(
        |c| {
            c.import_hosts = Some(vec!["github.com".into()]);
            c.import_ssh = ssh.display().to_string();
        },
        false,
    )
    .await;
    let token = server.join("ws", &[]).await;
    let dir = clone(&server, "ws", &token).await;
    let repo = dir.path().join("repo");
    let (crafted, plain) = ("ssh://evil.example%2F@github.com/o/r", "ssh://git@github.com/o/r");
    let (mut manifest, mut lock) = (String::new(), String::new());
    for (name, source) in [("crafted", crafted), ("plain", plain)] {
        manifest.push_str(&format!("[import.{name}]\nsource = \"{source}\"\nref = \"main\"\n"));
        lock.push_str(&format!(
            "[skills.{name}]\nsource = \"{source}\"\npath = \"\"\nref = \"main\"\ncommit = \"{}\"\nhash = \"x\"\n",
            "a".repeat(40)
        ));
        write(&repo, &format!("imported/{name}/SKILL.md"), "x\n");
    }
    write(&repo, "sk1ll.toml", &manifest);
    write(&repo, "sk1ll.lock", &lock);
    commit_push(&server, &repo, "imports").await;
    let view = eventually(|| async {
        let v: ImportsView = server.get_json(&token, "/imports").await;
        Some(v).filter(|v| !v.skipped.is_empty() && !v.errors.is_empty())
    })
    .await;
    assert_eq!(view.skipped.len(), 1, "{view:?}");
    assert_eq!(view.skipped[0].name, "crafted");
    assert!(view.skipped[0].reason.contains("percent escape"), "{}", view.skipped[0].reason);
    assert_eq!(view.errors.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["plain"]);
    let seen = std::fs::read_to_string(&log).unwrap();
    assert!(seen.lines().all(|l| l.contains("git@github.com") && !l.contains("evil")), "{seen}");
}

/// Review finding: daemons must hear about newly skipped imports even when
/// the updates didn't change, or the GUI never shows them.
#[tokio::test(flavor = "multi_thread")]
async fn newly_skipped_imports_are_announced() {
    let server = start_with(|c| c.import_checks = false, false).await;
    let token = server.join("ws", &[]).await;
    let mut ws = ws_connect(&server, &token).await;
    let _upstream = push_moved_import(&server, "ws", &token).await;
    loop {
        if let ServerMessage::ImportsChanged { updates } = ws_next(&mut ws).await {
            assert!(updates.is_empty(), "{updates:?}");
            break;
        }
    }
    let view: ImportsView = server.get_json(&token, "/imports").await;
    assert_eq!(view.skipped.len(), 1, "{view:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn join_codes_carry_their_scope_and_expiry() {
    let server = start_with(|c| c.join_code_ttl = Duration::from_secs(3600), false).await;
    let before = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let code = server.scoped_code(ADMIN, "self").await;
    assert_eq!(code.scope, Scope::SelfOnly);
    let expires = code.expires_at;
    assert!((before + 3600..=before + 3605).contains(&expires), "{expires} vs {before}");
    // No body, `null` and `{}` all ask for a fleet code.
    for body in [None, Some("null"), Some("{}"), Some(" ")] {
        let resp = server.create_code(ADMIN, body).await;
        assert_eq!(resp.status(), StatusCode::CREATED, "{body:?}");
        assert_eq!(resp.json::<JoinCode>().await.unwrap().scope, Scope::Fleet, "{body:?}");
    }
    for body in [r#"{"scope":"admin"}"#, "[", r#""self""#] {
        assert_eq!(server.create_code(ADMIN, Some(body)).await.status(), StatusCode::BAD_REQUEST, "{body}");
    }
    // A fleet machine may mint either scope.
    let fleet = server.join("f", &[]).await;
    assert_eq!(server.scoped_code(&fleet, "self").await.scope, Scope::SelfOnly);
    assert_eq!(server.scoped_code(&fleet, "fleet").await.scope, Scope::Fleet);
}

#[tokio::test(flavor = "multi_thread")]
async fn self_scoped_machines_manage_only_themselves() {
    let server = start().await;
    let me = server.join_scoped("me", &["claude"], "self").await;
    let other = server.join_scoped("other", &["claude"], "fleet").await;
    let fleet: FleetView = server.get_json(&me, "/fleet").await;
    let scopes: Vec<_> = fleet.machines.iter().map(|m| (m.id.as_str(), m.scope)).collect();
    assert_eq!(scopes, [("me", Scope::SelfOnly), ("other", Scope::Fleet)]);

    // Reading is open to every machine.
    for path in ["/fleet", "/machines/other/assignment", "/log", "/log/facets", "/imports"] {
        assert_eq!(server.get(&me, path).await.status(), StatusCode::OK, "{path}");
    }
    let forbidden = |resp: reqwest::Response| async move {
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body: Value = resp.json().await.unwrap();
        assert!(body["error"].as_str().unwrap().contains("self-only join code"), "{body}");
    };

    // No join codes of any scope, so it can't mint a fleet one.
    for body in [None, Some(r#"{"scope":"fleet"}"#), Some(r#"{"scope":"self"}"#)] {
        forbidden(server.create_code(&me, body).await).await;
    }

    // Its own assignment, but no one else's, existing or not.
    let toggle = json!([{"harness": "claude", "kind": "new_skills_default", "enabled": true}]);
    let mine = server.put_assignment(&me, "me", toggle.clone()).await;
    forbidden(server.try_put_assignment(&me, "other", toggle.clone()).await).await;
    forbidden(server.try_put_assignment(&me, "ghost", toggle.clone()).await).await;
    // Copying another Machine's Assignment can only target this Machine.
    let copy = |id: &str| {
        let req = server
            .http
            .post(server.url(&format!("/machines/{id}/assignment/copy-from")))
            .bearer_auth(&me)
            .json(&json!({"source": "other", "mode": "merge"}));
        async move { req.send().await.unwrap() }
    };
    assert_eq!(copy("me").await.status(), StatusCode::OK);
    forbidden(copy("other").await).await;
    forbidden(copy("ghost").await).await;
    let theirs = server.put_assignment(&other, "other", toggle.clone()).await;
    // A fleet machine still manages it.
    let by_other = server
        .put_assignment(&other, "me", json!([{"harness": "claude", "kind": "new_rules_default", "enabled": true}]))
        .await;

    // Undo: entries for itself (whoever wrote them), not for others.
    let undo = |token: String, id: i64| {
        let req = server.http.post(server.url(&format!("/log/{id}/undo"))).bearer_auth(token);
        async move { req.send().await.unwrap() }
    };
    assert_eq!(undo(me.clone(), mine.entries[0].id).await.status(), StatusCode::OK);
    assert_eq!(undo(me.clone(), by_other.entries[0].id).await.status(), StatusCode::OK);
    forbidden(undo(me.clone(), theirs.entries[0].id).await).await;
    assert_eq!(undo(me.clone(), 9999).await.status(), StatusCode::NOT_FOUND);

    // Removing others is refused; removing itself is allowed.
    let remove =
        |token: &str, id: &str| server.http.delete(server.url(&format!("/machines/{id}"))).bearer_auth(token).send();
    forbidden(remove(&me, "other").await.unwrap()).await;
    forbidden(remove(&me, "ghost").await.unwrap()).await;
    assert!(server.machine_row("other").await.is_some());
    assert_eq!(remove(&me, "me").await.unwrap().status(), StatusCode::NO_CONTENT);
    assert_eq!(server.get(&me, "/fleet").await.status(), StatusCode::UNAUTHORIZED);

    // Rejoining takes the new code's scope.
    let me = server.join_scoped("me", &["claude"], "fleet").await;
    assert_eq!(server.machine_row("me").await.unwrap().scope, Scope::Fleet);
    assert_eq!(server.create_code(&me, None).await.status(), StatusCode::CREATED);
    assert_eq!(remove(&me, "other").await.unwrap().status(), StatusCode::NO_CONTENT);
    server.join_scoped("other", &[], "self").await;
    assert_eq!(server.machine_row("other").await.unwrap().scope, Scope::SelfOnly);
}

#[tokio::test(flavor = "multi_thread")]
async fn replaced_socket_cannot_report_after_revocation() {
    let server = start_with(|c| c.send_timeout = Duration::from_secs(120), true).await;
    let old_token = server.join("ws", &["claude"]).await;
    let mut old = ws_connect_small(&server, &old_token).await;
    // From here on `old` doesn't read, so the server's send of the big
    // assignment to it stalls.
    fatten(&server, &mut old, "ws").await;

    let mut replacement = ws_connect(&server, &old_token).await;
    ws_assignment(&mut replacement).await;
    let resp = server.http.delete(server.url("/machines/ws")).bearer_auth(ADMIN).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    while ws_next(&mut replacement).await != ServerMessage::Revoked {}

    let token = server.join("ws", &["claude"]).await;
    let mut fresh = ws_connect(&server, &token).await;
    let good =
        MachineReport { seq: 1, baseline: Some("good".into()), harnesses: vec!["claude".into()], ..Default::default() };
    ws_send(&mut fresh, &DaemonMessage::Status(good.clone())).await;
    server.reported("ws", &good).await;

    let evil =
        MachineReport { seq: 2, baseline: Some("evil".into()), harnesses: vec!["evil".into()], ..Default::default() };
    let _ = old.send(Message::text(serde_json::to_string(&DaemonMessage::Status(evil)).unwrap())).await;
    // The server let go of the old socket: it ends once what's buffered is read.
    let drained = tokio::time::timeout(Duration::from_secs(20), async { while let Some(Ok(_)) = old.next().await {} });
    drained.await.expect("the old socket is still open");

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(server.machine_row("ws").await.unwrap().report, Some(good));
    let a = server.get_json::<AssignmentSnapshot>(&token, "/machines/ws/assignment").await.assignment;
    assert!(!a.harnesses.contains_key("evil"));
}

/// Review finding: an unread socket queued a full snapshot per change.
#[tokio::test(flavor = "multi_thread")]
async fn stalled_daemon_gets_only_the_latest_assignment() {
    let server = start_with(|c| c.send_timeout = Duration::from_secs(120), true).await;
    let token = server.join("ws", &["claude"]).await;
    let mut ws = ws_connect_small(&server, &token).await;
    fatten(&server, &mut ws, "ws").await;
    let name = &fat_names(1)[0];
    let mut last = 0;
    for i in 0..30 {
        let change = json!([{"harness": "claude", "kind": "skill", "item_id": name, "enabled": i % 2 == 1}]);
        last = server.put_assignment(&token, "ws", change).await.revision;
    }

    let mut received = Vec::new();
    loop {
        let a = ws_assignment(&mut ws).await;
        received.push(a.revision);
        if a.revision == last {
            assert!(a.assignment.skill_enabled("claude", &id(name)));
            break;
        }
    }
    assert!(received.windows(2).all(|w| w[0] < w[1]), "{received:?}");
    // The initial one, the stalled one, maybe one in flight, and the latest.
    assert!(received.len() <= 4, "superseded snapshots were queued: {received:?}");
}

/// Review finding: a consumer that never reads must be disconnected.
#[tokio::test(flavor = "multi_thread")]
async fn daemon_that_misses_the_send_deadline_is_disconnected() {
    let server = start_with(|c| c.send_timeout = Duration::from_millis(300), true).await;
    let token = server.join("ws", &["claude"]).await;
    let mut ws = ws_connect_small(&server, &token).await;
    fatten(&server, &mut ws, "ws").await;
    eventually(|| async { (!server.machine_row("ws").await?.connected).then_some(()) }).await;
}

/// Review finding: one item ref proto didn't know dropped the whole report.
#[tokio::test(flavor = "multi_thread")]
async fn reports_keep_every_kind_of_draft_item() {
    let server = start().await;
    let token = server.join("ws", &["claude"]).await;
    let mut ws = ws_connect(&server, &token).await;
    let fleet: FleetView = server.get_json(&token, "/fleet").await;
    let text = json!({
        "type": "status",
        "seq": 1,
        "baseline": fleet.baseline,
        "drafts": ["skill:pdf", "manifest", "other:notes.txt", "bogus:x"],
        "harnesses": ["claude"],
    });
    ws.send(Message::text(text.to_string())).await.unwrap();
    let expected = MachineReport {
        seq: 1,
        baseline: fleet.baseline,
        drafts: vec![ItemRef::Skill("pdf".into()), ItemRef::Manifest, ItemRef::Other("notes.txt".into())],
        harnesses: vec!["claude".into()],
        ..Default::default()
    };
    server.reported("ws", &expected).await;
    assert_eq!(server.machine_row("ws").await.unwrap().status, MachineStatus::Drafting);
}

/// Review finding: pushes in quick succession collapsed into fewer nudges.
/// A push that deletes an item changes no assignment (ADR 0006).
#[tokio::test(flavor = "multi_thread")]
async fn each_push_is_announced_and_changes_no_assignment() {
    let server = start().await;
    let token = server.join("ws", &["claude"]).await;
    let (dir, c0) = seed(&server, "ws", &token, &["personal/pdf/SKILL.md"]).await;
    let repo = dir.path().join("repo");
    server
        .put_assignment(
            &token,
            "ws",
            json!([{"harness": "claude", "kind": "skill", "item_id": "pdf", "enabled": true}]),
        )
        .await;
    let mut ws = ws_connect(&server, &token).await;
    ws_assignment(&mut ws).await;
    assert_eq!(ws_next(&mut ws).await, ServerMessage::Nudge { baseline: c0 });

    write(&repo, "personal/a/SKILL.md", "a\n");
    let c1 = commit_push(&server, &repo, "add a").await;
    std::fs::remove_dir_all(repo.join("personal/pdf")).unwrap();
    let c2 = commit_push(&server, &repo, "drop pdf").await;
    write(&repo, "personal/b/SKILL.md", "b\n");
    let c3 = commit_push(&server, &repo, "add b").await;

    let mut nudges = Vec::new();
    while nudges.last() != Some(&c3) {
        if let ServerMessage::Nudge { baseline } = ws_next(&mut ws).await {
            nudges.push(baseline);
        }
    }
    assert_eq!(nudges, [c1, c2, c3]);
    let a = server.get_json::<AssignmentSnapshot>(&token, "/machines/ws/assignment").await.assignment;
    assert!(a.harness("claude").skills.contains_key(&id("pdf")));
    let log: Vec<LogEntry> = server.get_json(&token, "/log?machine=ws").await;
    assert_eq!(log.len(), 1, "{log:?}");
}

/// Review finding: snapshots could reach the daemon out of commit order.
#[tokio::test(flavor = "multi_thread")]
async fn assignment_revisions_arrive_in_order() {
    let server = start().await;
    let token = server.join("ws", &["claude"]).await;
    let files: Vec<String> = (0..20).map(|i| format!("personal/k{i:02}/SKILL.md")).collect();
    seed(&server, "ws", &token, &files.iter().map(String::as_str).collect::<Vec<_>>()).await;
    let mut ws = ws_connect(&server, &token).await;
    let first = ws_assignment(&mut ws).await.revision;

    let puts = (0..20).map(|i| {
        let change = json!([{"harness": "claude", "kind": "skill", "item_id": format!("k{i:02}"), "enabled": true}]);
        server.put_assignment(&token, "ws", change)
    });
    let last = futures_util::future::join_all(puts).await.iter().map(|u| u.revision).max().unwrap();

    let mut revisions = vec![first];
    let latest = loop {
        let a = ws_assignment(&mut ws).await;
        revisions.push(a.revision);
        if a.revision == last {
            break a;
        }
    };
    assert!(revisions.windows(2).all(|w| w[0] < w[1]), "{revisions:?}");
    let stored: AssignmentSnapshot = server.get_json(&token, "/machines/ws/assignment").await;
    assert_eq!(stored.revision, last);
    assert_eq!(latest, AssignmentSnapshot { registration: None, ..stored.clone() });
    assert_eq!(stored.assignment.harness("claude").skills.len(), 20);
    let log_before: Vec<LogEntry> = server.get_json(&token, "/log?machine=ws").await;
    let noop = server.put_assignment(&token, "ws", json!([])).await;
    assert_eq!(noop.revision, stored.revision);
    assert_eq!(noop.assignment, stored.assignment);
    assert!(noop.entries.is_empty());
    let log_after: Vec<LogEntry> = server.get_json(&token, "/log?machine=ws").await;
    assert_eq!(log_before, log_after);
}

/// Review finding: a silent connection kept a machine online forever.
#[tokio::test(flavor = "multi_thread")]
async fn silent_connections_go_offline() {
    let server = start_with(
        |c| {
            c.ping_every = Duration::from_millis(200);
            c.offline_after = Duration::from_secs(1);
        },
        false,
    )
    .await;
    let silent_token = server.join("silent", &[]).await;
    let live_token = server.join("live", &[]).await;
    // Never read, so it never answers pings.
    let mut silent = ws_connect(&server, &silent_token).await;
    let report = MachineReport { seq: 1, ..Default::default() };
    ws_send(&mut silent, &DaemonMessage::Status(report.clone())).await;
    server.reported("silent", &report).await;
    let mut live = ws_connect(&server, &live_token).await;
    tokio::spawn(async move { while let Some(Ok(_)) = live.next().await {} });

    eventually(|| async { server.machine_row("silent").await?.connected.then_some(()) }).await;
    let row = eventually(|| async {
        let row = server.machine_row("silent").await?;
        (!row.connected && row.status == MachineStatus::Offline).then_some(row)
    })
    .await;
    assert!(row.last_seen.is_some());

    let live = server.machine_row("live").await.unwrap();
    assert!(live.connected);
    assert_ne!(live.status, MachineStatus::Offline);
}

/// Review follow-up: changing a default must not flip existing items.
#[tokio::test(flavor = "multi_thread")]
async fn changing_a_default_keeps_existing_items() {
    let server = start().await;
    let token = server.join("ws", &["claude"]).await;
    let (dir, _) =
        seed(&server, "ws", &token, &["personal/a/SKILL.md", "personal/b/SKILL.md", "instructions/f.md"]).await;
    server
        .put_assignment(&token, "ws", json!([{"harness": "claude", "kind": "skill", "item_id": sk1ll_proto::ItemId::derived("skill", "a"), "enabled": true}]))
        .await;

    let update = server
        .put_assignment(&token, "ws", json!([{"harness": "claude", "kind": "new_skills_default", "enabled": true}]))
        .await;
    let kinds: Vec<_> =
        update.entries.iter().map(|e| (e.kind, e.name.as_deref(), e.before, e.after, &e.cause)).collect();
    assert_eq!(
        kinds,
        [
            (ToggleKind::Skill, Some("b"), None, Some(false), &LogCause::DefaultMaterialized),
            (ToggleKind::NewSkillsDefault, None, Some(false), Some(true), &LogCause::Change),
        ]
    );
    let claude = update.assignment.harness("claude");
    assert!(
        claude.skill_enabled(&sk1ll_proto::ItemId::derived("skill", "a"))
            && !claude.skill_enabled(&sk1ll_proto::ItemId::derived("skill", "b"))
    );
    assert!(claude.rules.is_empty() && !claude.rule_enabled(&sk1ll_proto::ItemId::derived("rule", "f")));

    // Items that appear later follow the new default.
    let repo = dir.path().join("repo");
    write(&repo, "personal/c/SKILL.md", "c\n");
    commit_push(&server, &repo, "add c").await;
    let a = server.get_json::<AssignmentSnapshot>(&token, "/machines/ws/assignment").await.assignment;
    assert!(
        a.skill_enabled("claude", &sk1ll_proto::ItemId::derived("skill", "c"))
            && !a.skill_enabled("claude", &sk1ll_proto::ItemId::derived("skill", "b"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stalled_upload_does_not_block_the_next_push() {
    use base64::Engine;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let server = start_with(|c| c.push_idle = Duration::from_secs(1), false).await;
    let token = server.join("ws", &[]).await;
    let (dir, _) = seed(&server, "ws", &token, &["personal/pdf/SKILL.md"]).await;
    let repo = dir.path().join("repo");
    let mut stalled = tokio::net::TcpStream::connect(("127.0.0.1", server.port)).await.unwrap();
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("ws:{token}"));
    stalled.write_all(format!("POST /git/repo.git/git-receive-pack HTTP/1.1\r\nHost: localhost\r\nAuthorization: Basic {auth}\r\nContent-Type: application/x-git-receive-pack-request\r\nContent-Length: 100\r\nExpect: 100-continue\r\n\r\n").as_bytes()).await.unwrap();
    // The body is first polled after acquiring the push lock. Hyper sends
    // 100 Continue then, so wait for it before queuing the legitimate push.
    let mut reader = BufReader::new(&mut stalled);
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line)).await.unwrap().unwrap();
    assert!(line.starts_with("HTTP/1.1 100"), "{line}");
    write(&repo, "personal/pdf/SKILL.md", "updated\n");
    git(&repo, &["commit", "--quiet", "-am", "update"]);
    let started = std::time::Instant::now();
    let push = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["push", "--quiet", "origin", "HEAD:main"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .kill_on_drop(true)
        .output();
    let out =
        tokio::time::timeout(Duration::from_secs(4), push).await.expect("a stalled upload held the push lock").unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(started.elapsed() < Duration::from_secs(4));
    let head = git(&repo, &["rev-parse", "HEAD"]);
    eventually(|| async {
        let fleet: FleetView = server.get_json(&token, "/fleet").await;
        (fleet.baseline.as_deref() == Some(head.as_str())).then_some(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_disconnect_does_not_refresh_last_seen() {
    let server = start_with(
        |c| {
            c.ping_every = Duration::from_millis(200);
            c.offline_after = Duration::from_secs(2);
        },
        false,
    )
    .await;
    let token = server.join("silent", &[]).await;
    let mut silent = ws_connect(&server, &token).await;
    let report = MachineReport { seq: 1, ..Default::default() };
    ws_send(&mut silent, &DaemonMessage::Status(report.clone())).await;
    server.reported("silent", &report).await;
    let last_frame = server.machine_row("silent").await.unwrap().last_seen;
    // Do not read again, so no pong is sent.
    let disconnected = eventually(|| async {
        let row = server.machine_row("silent").await?;
        (!row.connected).then_some(row)
    })
    .await;
    assert_eq!(disconnected.last_seen, last_frame, "closing the socket is not an inbound frame");
    eventually(|| async { (server.machine_row("silent").await?.status == MachineStatus::Offline).then_some(()) }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn websocket_upgrade_alone_does_not_refresh_last_seen() {
    let server = start().await;
    let token = server.join("silent", &[]).await;
    let mut ws = ws_connect(&server, &token).await;
    ws_assignment(&mut ws).await;
    let row = server.machine_row("silent").await.unwrap();
    assert!(row.connected);
    assert_eq!(row.last_seen, None);
    assert_eq!(row.status, MachineStatus::Offline);
}

/// A timed-out receive-pack may already have updated HEAD. Its cleanup must
/// still sweep and announce that baseline before admitting another push.
#[cfg(unix)]
async fn push_timeout_runs_follow_up(hook_body: &str, idle: Duration, max: Duration) {
    use base64::Engine;
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::AsyncWriteExt;

    let server = start_with(
        |c| {
            c.push_idle = idle;
            c.push_max = max;
        },
        true,
    )
    .await;
    let token = server.join("ws", &["claude"]).await;
    let (dir, first) = seed(&server, "ws", &token, &["personal/keep/SKILL.md"]).await;
    let repo = dir.path().join("repo");
    write(&repo, "personal/removed/SKILL.md", "removed\n");
    let second = commit_push(&server, &repo, "add removed").await;
    server
        .put_assignment(
            &token,
            "ws",
            json!([{"harness": "claude", "kind": "skill", "item_id": "removed", "enabled": true}]),
        )
        .await;
    let mut ws = ws_connect(&server, &token).await;
    ws_assignment(&mut ws).await;
    assert_eq!(ws_next(&mut ws).await, ServerMessage::Nudge { baseline: second.clone() });

    let bare = server._dir.path().join("data/repo.git");
    // The rewind below stands in for any push; let it past the server's
    // history protection, which is covered by its own test.
    git(&bare, &["config", "receive.denyNonFastForwards", "false"]);
    let hook = bare.join("hooks/post-receive");
    std::fs::write(&hook, format!("#!/bin/sh\ntouch timeout-hook-started\n{hook_body}\n")).unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    // Move HEAD back to an existing commit with an empty packfile. The
    // post-receive hook runs only after the ref has been updated.
    let command = format!("{second} {first} refs/heads/main\0report-status side-band-64k\n");
    let mut body = format!("{:04x}{command}0000", command.len() + 4).into_bytes();
    let pack = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["pack-objects", "--stdout"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(pack.status.success());
    body.extend(pack.stdout);
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(1024).unwrap();
    let mut stalled = socket.connect((std::net::Ipv4Addr::LOCALHOST, server.port).into()).await.unwrap();
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("ws:{token}"));
    let request = format!(
        "POST /git/repo.git/git-receive-pack HTTP/1.1\r\nHost: localhost\r\nAuthorization: Basic {auth}\r\nContent-Type: application/x-git-receive-pack-request\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    stalled.write_all(request.as_bytes()).await.unwrap();
    stalled.write_all(&body).await.unwrap();
    eventually(|| async { bare.join("timeout-hook-started").exists().then_some(()) }).await;
    std::fs::remove_file(&hook).unwrap();

    // The hook either stalls the backend or floods an unread response. In
    // both cases the configured timeout must run follow-up under the lock.
    let nudge = tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            if let ServerMessage::Nudge { baseline } = ws_next(&mut ws).await
                && baseline == first
            {
                break baseline;
            }
        }
    })
    .await
    .expect("timed-out push did not run its follow-up");
    assert_eq!(nudge, first);
    let a = server.get_json::<AssignmentSnapshot>(&token, "/machines/ws/assignment").await.assignment;
    assert_eq!(a.harness("claude").skills.get(&id("removed")), Some(&true));

    let push = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["push", "--quiet", "origin", "HEAD:main"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(Duration::from_secs(4), push).await.expect("push lock was not released").unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn backend_deadline_runs_follow_up_and_releases_push_lock() {
    push_timeout_runs_follow_up("exec sleep 5", Duration::from_secs(10), Duration::from_secs(1)).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn backend_deadline_kills_pre_receive_descendants() {
    use std::os::unix::fs::PermissionsExt;

    let server = start_with(|c| c.push_max = Duration::from_secs(1), false).await;
    let token = server.join("ws", &["claude"]).await;
    let (dir, baseline) = seed(&server, "ws", &token, &["README.md"]).await;
    let repo = dir.path().join("repo");
    write(&repo, "personal/wip/SKILL.md", "committed\n");
    git_async(repo.clone(), vec!["add".into(), "-A".into()]).await;
    git_async(repo.clone(), vec!["commit".into(), "--quiet".into(), "-m".into(), "commit".into()]).await;
    let bare = server._dir.path().join("data/repo.git");
    let hook = bare.join("hooks/pre-receive");
    std::fs::write(&hook, "#!/bin/sh\ntouch pre-receive-started\nsleep 3\ntouch pre-receive-finished\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut push = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["push", "--quiet", "origin", "HEAD:main"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    eventually(|| async { bare.join("pre-receive-started").exists().then_some(()) }).await;
    let out = tokio::time::timeout(Duration::from_secs(5), push.wait()).await.expect("push did not time out").unwrap();
    assert!(!out.success(), "a push past its deadline must fail");
    // Wait past the hook's sleep: killing only http-backend lets receive-pack
    // update HEAD after the server has already checked it during follow-up.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(git_async(bare.clone(), vec!["rev-parse".into(), "HEAD".into()]).await, baseline);
    assert!(!bare.join("pre-receive-finished").exists(), "the timed-out hook survived");
    let fleet: FleetView = server.get_json(&token, "/fleet").await;
    assert_eq!(fleet.baseline.as_deref(), Some(baseline.as_str()));

    // The next push must acquire the lock after the entire group is gone.
    std::fs::remove_file(&hook).unwrap();
    tokio::time::timeout(
        Duration::from_secs(4),
        git_async(repo.clone(), vec!["push".into(), "--quiet".into(), "origin".into(), "HEAD:main".into()]),
    )
    .await
    .expect("push lock was not released");
    let committed = git_async(repo, vec!["rev-parse".into(), "HEAD".into()]).await;
    eventually(|| async {
        let fleet: FleetView = server.get_json(&token, "/fleet").await;
        (fleet.baseline.as_deref() == Some(committed.as_str())).then_some(())
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn stalled_response_runs_follow_up_and_releases_push_lock() {
    push_timeout_runs_follow_up("head -c 4194304 /dev/zero", Duration::from_secs(1), Duration::from_secs(10)).await;
}

/// Default changes and their undo preserve the destination's reported local content.
#[tokio::test(flavor = "multi_thread")]
async fn defaults_and_undo_materialize_destination_drafts_and_unpushed() {
    let server = start().await;
    let alpha = server.join("alpha", &["claude"]).await;
    let beta = server.join("beta", &["claude"]).await;
    let mut ws = ws_connect(&server, &alpha).await;
    let mut other_ws = ws_connect(&server, &beta).await;
    let mut report = MachineReport {
        seq: 1,
        inventory_complete: true,
        harnesses: vec!["claude".into()],
        drafts: vec![ItemRef::Skill("draft".into()), ItemRef::Rule("draft".into())],
        unpushed: vec![
            ItemRef::Skill("unpushed".into()),
            ItemRef::Rule("unpushed".into()),
            ItemRef::Skill("draft".into()),
            ItemRef::Rule("draft".into()),
            ItemRef::Skill("explicit".into()),
            ItemRef::Rule("explicit".into()),
        ],
        ..Default::default()
    };
    report.items = report
        .drafts
        .iter()
        .chain(&report.unpushed)
        .cloned()
        .map(|item| ReportedItem { id: id(item.name()), item })
        .collect();
    ws_send(&mut ws, &DaemonMessage::Status(report.clone())).await;
    server.reported("alpha", &report).await;
    let other = MachineReport { seq: 1, drafts: vec![ItemRef::Skill("other".into())], ..Default::default() };
    ws_send(&mut other_ws, &DaemonMessage::Status(other.clone())).await;
    server.reported("beta", &other).await;
    for (kind, default, item_kind) in
        [("skill", "new_skills_default", ToggleKind::Skill), ("rule", "new_rules_default", ToggleKind::Rule)]
    {
        server
            .put_assignment(
                &beta,
                "alpha",
                json!([
                    {"harness": "claude", "kind": kind, "item_id": "explicit", "enabled": true}
                ]),
            )
            .await;
        let update = server
            .put_assignment(
                &beta,
                "alpha",
                json!([
                    {"harness": "claude", "kind": default, "enabled": true}
                ]),
            )
            .await;
        let read: AssignmentSnapshot = server.get_json(&beta, "/machines/alpha/assignment").await;
        assert_eq!(read.assignment, update.assignment);
        assert_eq!(read.revision, update.revision);
        let materialized: Vec<_> = update.entries.iter().filter(|e| e.cause == LogCause::DefaultMaterialized).collect();
        assert_eq!(materialized.len(), 2, "deduplicate drafts and preserve explicit values");
        for entry in materialized {
            assert_eq!(entry.machine, "alpha");
            assert_eq!(entry.kind, item_kind);
            assert!(matches!(entry.name.as_deref(), Some("draft" | "unpushed")));
            assert_eq!((entry.before, entry.after), (None, Some(false)));
        }
        let h = update.assignment.harness("claude");
        let toggles = if kind == "skill" { &h.skills } else { &h.rules };
        assert_eq!(toggles.get(&id("explicit")), Some(&true));
        assert!(!toggles.contains_key(&id("other")));
        assert!(!toggles.contains_key(&id("future")));
        assert!(update.assignment.harness("codex").skills.is_empty());

        // A new local item inherits true. Undo must preserve that true value too.
        let future = if kind == "skill" { ItemRef::Skill("future".into()) } else { ItemRef::Rule("future".into()) };
        report.items.push(ReportedItem { item: future.clone(), id: id("future") });
        report.drafts.push(future);
        report.seq += 1;
        ws_send(&mut ws, &DaemonMessage::Status(report.clone())).await;
        server.reported("alpha", &report).await;
        let entry = update.entries.last().unwrap();
        let response =
            server.http.post(server.url(&format!("/log/{}/undo", entry.id))).bearer_auth(&beta).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let undone: AssignmentUpdate = response.json().await.unwrap();
        assert_eq!(undone.entries.len(), 2);
        let preserved = &undone.entries[0];
        assert_eq!(preserved.cause, LogCause::DefaultMaterialized);
        assert_eq!(preserved.kind, item_kind);
        assert_eq!(preserved.name.as_deref(), Some("future"));
        assert_eq!((preserved.before, preserved.after), (None, Some(true)));
        let h = undone.assignment.harness("claude");
        let toggles = if kind == "skill" { &h.skills } else { &h.rules };
        assert_eq!(toggles.get(&id("draft")), Some(&false));
        assert_eq!(toggles.get(&id("unpushed")), Some(&false));
        assert_eq!(toggles.get(&id("future")), Some(&true));
        assert_eq!(undone.entries[1].cause, LogCause::Undo { entry: entry.id });
    }
}

/// Sets the creation time of join code `code` in the server's database to
/// `seconds` ago, as the server hashes codes: normalized, then SHA-256.
fn backdate_code(server: &TestServer, code: &str, seconds: i64) {
    use sha2::Digest;
    let normalized: String = code.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_uppercase()).collect();
    let hash: String = sha2::Sha256::digest(normalized.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    let conn = rusqlite::Connection::open(server._dir.path().join("data/server.db")).unwrap();
    conn.busy_timeout(Duration::from_secs(5)).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let changed =
        conn.execute("UPDATE join_codes SET created_at = ?2 WHERE code_hash = ?1", (hash, now - seconds)).unwrap();
    assert_eq!(changed, 1, "no such code");
}

/// A code is invalid from its `expires_at` on, and then looks just like a
/// code that never existed.
#[tokio::test]
async fn expired_join_code_is_invalid() {
    const TTL: i64 = 3600;
    let server = start_with(|c| c.join_code_ttl = Duration::from_secs(TTL as u64), false).await;
    let expired = server.join_code(ADMIN).await;
    backdate_code(&server, &expired, TTL + 60);
    let boundary = server.join_code(ADMIN).await;
    backdate_code(&server, &boundary, TTL);
    let almost = server.join_code(ADMIN).await;
    backdate_code(&server, &almost, TTL - 60);

    let invalid = server.try_join("invalid", "invalid", &[]).await;
    let (status, text) = (invalid.status(), invalid.text().await.unwrap());
    assert_eq!(status, StatusCode::FORBIDDEN);
    for (code, id) in [(&expired, "expired"), (&boundary, "boundary")] {
        let resp = server.try_join(code, id, &[]).await;
        assert_eq!(resp.status(), status, "{id}");
        assert_eq!(resp.text().await.unwrap(), text, "{id}");
    }
    assert_eq!(server.try_join(&almost, "almost", &[]).await.status(), StatusCode::CREATED);
    server.join("fresh", &[]).await;
}

/// Review finding: a replacement ref changes the tree git reads for a commit
/// without moving HEAD, so only the baseline branch may be pushed to.
#[tokio::test(flavor = "multi_thread")]
async fn pushes_to_refs_outside_the_baseline_branch_are_refused() {
    let server = start().await;
    let token = server.join("writer", &[]).await;
    let (dir, head) = seed(&server, "writer", &token, &["personal/pdf/SKILL.md"]).await;
    let repo = dir.path().join("repo");
    git(&repo, &["rm", "--quiet", "-r", "personal/pdf"]);
    git(&repo, &["commit", "--quiet", "-m", "without pdf"]);
    let without = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["replace", &head, &without]);
    git(&repo, &["tag", "v1"]);
    let refspecs = [
        format!("refs/replace/{head}:refs/replace/{head}"),
        "v1".into(),
        "HEAD:refs/heads/side".into(),
        "HEAD:refs/heads/main/x".into(),
    ];
    for refspec in refspecs {
        let out = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["push", "origin", &refspec])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        assert!(!out.status.success(), "pushing {refspec} must fail");
        // Hiding refs matches by prefix, so the hook is what refuses this one.
        if refspec.ends_with("main/x") {
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(stderr.contains("only refs/heads/main takes pushes, not refs/heads/main/x"), "{stderr}");
        }
    }
    let bare = server._dir.path().join("data/repo.git");
    assert_eq!(git(&bare, &["for-each-ref", "--format=%(refname)"]), "refs/heads/main");
    assert_eq!(git(&bare, &["rev-parse", "refs/heads/main"]), head);
}

#[tokio::test(flavor = "multi_thread")]
async fn main_rejects_rewrites_and_deletion() {
    let server = start().await;
    let token = server.join("writer", &[]).await;
    let dir = clone(&server, "writer", &token).await;
    let repo = dir.path().join("repo");
    git(&repo, &["commit", "--allow-empty", "-m", "fast forward"]);
    git(&repo, &["push", "origin", "HEAD:main"]);
    git(&repo, &["reset", "--hard", "HEAD~1"]);
    for args in [["push", "--force", "origin", "HEAD:main"], ["push", "--delete", "origin", "main"]] {
        let out = Command::new("git").arg("-C").arg(&repo).args(args).env("GIT_TERMINAL_PROMPT", "0").output().unwrap();
        assert!(!out.status.success());
    }
}

#[tokio::test]
async fn copy_machine_assignment_previews_merges_replaces_and_logs() {
    let server = start().await;
    let source = server.join_scoped("source", &["claude"], "self").await;
    let target = server.join_scoped("target", &["claude", "codex"], "self").await;
    server
        .put_assignment(
            ADMIN,
            "source",
            json!([
                {"harness":"claude","kind":"skill","item_id":"private","name":"private","enabled":false},
                {"harness":"claude","kind":"rule","item_id":"shared","enabled":false},
                {"harness":"claude","kind":"new_skills_default","enabled":true}
            ]),
        )
        .await;
    server
        .put_assignment(
            ADMIN,
            "target",
            json!([
                {"harness":"claude","kind":"rule","item_id":"shared","enabled":false},
                {"harness":"codex","kind":"skill","item_id":"old","enabled":false}
            ]),
        )
        .await;
    let path = "/machines/target/assignment/copy-from";
    let post =
        |token: &str, path: &str, body: Value| server.http.post(server.url(path)).bearer_auth(token).json(&body).send();
    let merge = json!({"source":"source","mode":"merge"});
    assert_eq!(post(&source, path, merge.clone()).await.unwrap().status(), StatusCode::FORBIDDEN);
    assert_eq!(
        post(&target, path, json!({"source":"missing","mode":"merge"})).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        post(&target, path, json!({"source":"source","mode":"merge","expected":{"revision":0}}))
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    let update: AssignmentUpdate =
        post(&target, path, merge).await.unwrap().error_for_status().unwrap().json().await.unwrap();
    assert_eq!(update.entries.len(), 1);
    assert_eq!(update.entries[0].name.as_deref(), Some("private"));
    assert!(!update.assignment.harness("claude").new_skills_default);

    let mut replace = json!({"source":"source","mode":"replace","harnesses":[{"from":"claude","to":"codex"}]});
    let preview: Value = post(&target, &format!("{path}/preview"), replace.clone())
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(preview["count"], 4);
    assert_eq!(
        server.get_json::<AssignmentSnapshot>(ADMIN, "/machines/target/assignment").await.revision,
        update.revision
    );
    replace["expected"] = preview["target"].clone();
    replace["source_expected"] = preview["source"].clone();
    let copied: AssignmentUpdate =
        post(&target, path, replace.clone()).await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let src: AssignmentSnapshot = server.get_json(ADMIN, "/machines/source/assignment").await;
    assert_eq!(copied.assignment.harness("codex"), src.assignment.harness("claude"));
    assert_eq!(copied.assignment.harness("claude"), update.assignment.harness("claude"));
    assert_eq!(copied.entries.len(), 4);
    assert_eq!(post(&target, path, replace).await.unwrap().status(), StatusCode::CONFLICT);
    for entry in copied.entries.iter().rev() {
        let restored: AssignmentUpdate = post(&target, &format!("/log/{}/undo", entry.id), json!(null))
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(restored.entries[0].cause, LogCause::Undo { entry: entry.id });
    }
    let restored: AssignmentSnapshot = server.get_json(ADMIN, "/machines/target/assignment").await;
    assert_eq!(restored.assignment, update.assignment);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_rules_keep_their_claim_and_incomplete_baselines_refuse_prune() {
    let server = start().await;
    let token = server.join("alpha", &["claude"]).await;
    let dir = clone(&server, "alpha", &token).await;
    let repo = dir.path().join("repo");
    let rule = format!("---\nid: retained-rule\n---\n{}", "body\n".repeat(250_000));
    write(&repo, "instructions/large.md", &rule);
    // The manifest used to share the Rule body cap.
    let manifest =
        format!("{}\n[import.pdf]\nsource = 'github:o/r'\nid = 'retained-import'\n", "# description\n".repeat(100_000));
    write(&repo, "sk1ll.toml", &manifest);
    write(&repo, "imported/pdf/SKILL.md", "imported content");
    commit_push(&server, &repo, "large rule").await;
    server
        .put_assignment(
            &token,
            "alpha",
            json!([{
                "harness": "claude", "kind": "rule", "item_id": "retained-rule", "enabled": true
            }]),
        )
        .await;
    server
        .put_assignment(
            &token,
            "alpha",
            json!([{
                "harness": "claude", "kind": "skill", "item_id": "retained-import", "enabled": true
            }]),
        )
        .await;
    let view: sk1ll_proto::OrphansView = server.get_json(&token, "/assignment/orphans").await;
    assert!(view.orphans.is_empty());
    let before: AssignmentSnapshot = server.get_json(&token, "/machines/alpha/assignment").await;

    write(&repo, "instructions/broken.md", "---\nid: [invalid]\n---\n");
    commit_push(&server, &repo, "unreadable claim").await;
    for path in ["/assignment/orphans", "/fleet"] {
        let status = server.get(&token, path).await.status();
        assert_eq!(status, if path == "/fleet" { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE });
    }
    // Boot the Server over this same Baseline. Inventory errors must not prevent startup.
    sk1ll_server::Server::open(sk1ll_server::Config::new(server._dir.path().join("data"))).await.unwrap();
    let prune = server.http.post(server.url("/assignment/orphans/prune")).bearer_auth(&token).send().await.unwrap();
    assert_eq!(prune.status(), StatusCode::SERVICE_UNAVAILABLE);
    let default = server
        .try_put_assignment(
            &token,
            "alpha",
            json!([
                {"harness":"claude","kind":"new_rules_default","enabled":true}
            ]),
        )
        .await;
    assert_eq!(default.status(), StatusCode::SERVICE_UNAVAILABLE);
    let after: AssignmentSnapshot = server.get_json(&token, "/machines/alpha/assignment").await;
    assert_eq!(after, before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn materializing_defaults_and_undo_read_the_current_baseline() {
    let server = start().await;
    let token = server.join("alpha", &["claude"]).await;
    let dir = clone(&server, "alpha", &token).await;
    let work = dir.path().join("repo");
    let bare = server._dir.path().join("data/repo.git");
    // Simulate the gap before post-push follow-up publishes the new HEAD.
    let push_unannounced = |name: &str, id: &str| {
        write(&work, &format!("instructions/{name}.md"), &format!("---\nid: {id}\n---\nbody"));
        git(&work, &["add", "--all"]);
        git(&work, &["commit", "--quiet", "-m", name]);
        git(&work, &["push", "--quiet", bare.to_str().unwrap(), "HEAD:main"]);
    };
    push_unannounced("new", "first-id");
    let update = server
        .put_assignment(
            &token,
            "alpha",
            json!([{
                "harness": "claude", "kind": "new_rules_default", "enabled": true
            }]),
        )
        .await;
    assert_eq!(update.assignment.harness("claude").rules.get(&id("first-id")), Some(&false));
    push_unannounced("later", "second-id");
    let undo_id = update.entries.last().unwrap().id;
    let undo: AssignmentUpdate = server
        .http
        .post(server.url(&format!("/log/{undo_id}/undo")))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(undo.assignment.harness("claude").rules.get(&id("second-id")), Some(&true));
    assert!(!undo.assignment.harness("claude").new_rules_default);
}
