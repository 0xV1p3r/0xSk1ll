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
    JoinResponse, LogCause, LogEntry, MachineReport, MachineStatus, ReportedItem, Scope, ServerMessage, ToggleKind,
};
use sk1ll_server::Config;
use sk1ll_testkit::{
    eventually,
    git::{self, git},
    server::{ADMIN, Server},
    write,
};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

/// Reported harnesses of these names, each the preset of its name.
fn harnesses(names: &[&str]) -> Vec<sk1ll_proto::ReportedHarness> {
    names.iter().map(|n| sk1ll_proto::ReportedHarness { name: n.to_string(), kind: Some(n.to_string()) }).collect()
}

fn id(s: &str) -> ItemId {
    s.parse().unwrap()
}

/// A testkit [`Server`] that checks pushes with this crate's binary, with
/// this file's API helpers.
struct TestServer(Server);

impl std::ops::Deref for TestServer {
    type Target = Server;
    fn deref(&self) -> &Server {
        &self.0
    }
}

impl std::ops::DerefMut for TestServer {
    fn deref_mut(&mut self) -> &mut Server {
        &mut self.0
    }
}

fn builder() -> sk1ll_testkit::server::Builder {
    Server::builder().config(|config| config.push_check = Some(env!("CARGO_BIN_EXE_sk1ll-server").into()))
}

async fn start() -> TestServer {
    start_with(|_| {}).await
}

async fn start_with(tune: impl Fn(&mut Config) + Send + Sync + 'static) -> TestServer {
    TestServer(builder().config(tune).start().await)
}

/// A server whose sends to a client that doesn't read soon fill its tiny
/// send buffer.
async fn small_send_buffer(tune: impl Fn(&mut Config) + Send + Sync + 'static) -> TestServer {
    TestServer(builder().socket(|s| s.set_send_buffer_size(4096).unwrap()).config(tune).start().await)
}

/// A server on the test's own runtime and clock.
async fn on_test_runtime() -> TestServer {
    TestServer(builder().on_caller_runtime().start().await)
}

impl TestServer {
    fn api(&self, path: &str) -> String {
        format!("{}/api{path}", self.url())
    }

    /// A request to `/api<path>` as `token`.
    fn request(&self, method: reqwest::Method, token: &str, path: &str) -> reqwest::RequestBuilder {
        self.http().request(method, self.api(path)).bearer_auth(token)
    }

    async fn get_as(&self, token: &str, path: &str) -> reqwest::Response {
        self.request(reqwest::Method::GET, token, path).send().await.unwrap()
    }

    /// `DELETE /machines/<machine>` as `token`.
    async fn remove(&self, token: &str, machine: &str) -> reqwest::Response {
        self.request(reqwest::Method::DELETE, token, &format!("/machines/{machine}")).send().await.unwrap()
    }

    /// Undoes log entry `entry` as `token`.
    async fn undo(&self, token: &str, entry: impl std::fmt::Display) -> reqwest::Response {
        self.request(reqwest::Method::POST, token, &format!("/log/{entry}/undo")).send().await.unwrap()
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, token: &str, path: &str) -> T {
        let resp = self.get_as(token, path).await;
        assert!(resp.status().is_success(), "GET {path}: {}", resp.status());
        resp.json().await.unwrap()
    }

    /// `POST /join-codes` with `body` as raw JSON (`None`: no body).
    async fn create_code(&self, token: &str, body: Option<&str>) -> reqwest::Response {
        let req = self.http().post(self.api("/join-codes")).bearer_auth(token);
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
    async fn join_scoped(&self, id: &str, harnesses: &[&str], scope: Scope) -> String {
        let code = self.join_code_for(scope).await.code;
        let resp = self.try_join(&code, id, harnesses).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        resp.json::<JoinResponse>().await.unwrap().token
    }

    async fn try_join(&self, code: &str, id: &str, harnesses: &[&str]) -> reqwest::Response {
        let body = json!({"code": code, "machine_id": id, "harnesses": self::harnesses(harnesses)});
        self.http().post(self.api("/join")).json(&body).send().await.unwrap()
    }

    /// Joins `id` and returns its token.
    async fn join(&self, id: &str, harnesses: &[&str]) -> String {
        self.join_scoped(id, harnesses, Scope::Fleet).await
    }

    async fn try_put_assignment(&self, token: &str, machine: &str, changes: Value) -> reqwest::Response {
        self.http()
            .put(self.api(&format!("/machines/{machine}/assignment")))
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

    /// Waits until `id`'s stored report is `report`.
    async fn reported(&self, id: &str, report: &MachineReport) {
        eventually(|| async { (self.row(id).await?.report.as_ref() == Some(report)).then_some(()) }).await;
    }

    fn repo_url(&self, id: &str, token: &str) -> String {
        format!("http://{id}:{token}@127.0.0.1:{}/git/repo.git", self.port())
    }
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn ws_connect(server: &TestServer, token: &str) -> Ws {
    let url = format!("ws://127.0.0.1:{}/api/ws", server.port());
    let mut url = url.into_client_request().unwrap();
    url.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
    tokio_tungstenite::connect_async(url).await.unwrap().0
}

/// Connects with a tiny receive buffer, so not reading soon stalls the server.
async fn ws_connect_small(server: &TestServer, token: &str) -> Ws {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(4096).unwrap();
    let tcp = socket.connect(format!("127.0.0.1:{}", server.port()).parse().unwrap()).await.unwrap();
    let url = format!("ws://127.0.0.1:{}/api/ws", server.port());
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

/// [`git`] off the async workers, for commands that talk to a server on
/// the test's own runtime.
async fn git_async(dir: &Path, args: &[&str]) -> String {
    let (dir, args): (PathBuf, Vec<String>) = (dir.into(), args.iter().map(|a| a.to_string()).collect());
    tokio::task::spawn_blocking(move || git(&dir, &args.iter().map(String::as_str).collect::<Vec<_>>())).await.unwrap()
}

/// A fresh clone of the server repo in a temp dir.
async fn clone(server: &TestServer, id: &str, token: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("repo").to_string_lossy().into_owned();
    git_async(dir.path(), &["clone", "--quiet", &server.repo_url(id, token), &dest]).await;
    git(&dir.path().join("repo"), &["checkout", "--quiet", "-B", "main"]);
    dir
}

/// Commits and pushes, then waits for the server to publish the baseline and
/// its item catalogue before subsequent API calls use them. Returns HEAD.
async fn commit_push(server: &TestServer, repo: &Path, msg: &str) -> String {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", msg]);
    git_async(repo, &["push", "--quiet", "origin", "HEAD:main"]).await;
    let head = git(repo, &["rev-parse", "HEAD"]);
    eventually(|| async {
        let fleet: FleetView = server.get("/fleet").await;
        (fleet.baseline.as_deref() == Some(head.as_str())).then_some(())
    })
    .await;
    head
}

/// `git push origin <refspec>` in `repo`, which may fail.
fn push_output(repo: &Path, refspec: &str) -> std::process::Output {
    git::output(repo, &["push", "origin", refspec])
}

/// `git push --quiet origin HEAD:main` in `repo` as a child to await, killed
/// if the test drops it.
fn push_main(repo: &Path) -> tokio::process::Command {
    let mut push = tokio::process::Command::from(git::command(repo));
    push.args(["push", "--quiet", "origin", "HEAD:main"]).kill_on_drop(true);
    push
}

/// A fresh clone with `files` pushed as the baseline; returns it and HEAD.
async fn seed(server: &TestServer, id: &str, token: &str, files: &[&str]) -> (tempfile::TempDir, String) {
    let dir = clone(server, id, token).await;
    let repo = dir.path().join("repo");
    for f in files {
        write_item(&repo, f, "x\n");
    }
    let head = commit_push(server, &repo, "seed").await;
    (dir, head)
}

/// Writes `path` and, for a personal skill or a Rule, an explicit Item ID:
/// `id(name)` for a skill, `id("rule-<name>")` for a Rule. Items are never
/// given derived IDs.
fn write_item(repo: &Path, path: &str, text: &str) {
    if let Some(name) = path.strip_prefix("personal/").and_then(|p| p.strip_suffix("/SKILL.md")) {
        write(&repo.join(path), text);
        write(&repo.join(format!("personal/{name}/.sk1ll-id")), &format!("{name}\n"));
    } else if let Some(name) = path.strip_prefix("instructions/").and_then(|p| p.strip_suffix(".md")) {
        write(&repo.join(path), &format!("---\nid: rule-{name}\n---\n{text}"));
    } else {
        write(&repo.join(path), text);
    }
}

/// A change of `claude`'s toggle for skill `id`, named after it.
fn skill_toggle(id: &str, enabled: bool) -> Value {
    json!({"harness":"claude", "kind":"skill", "item_id":id, "name":id, "enabled":enabled})
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
        harnesses: harnesses(&["claude"]),
        ..Default::default()
    };
    ws_send(ws, &DaemonMessage::Status(report.clone())).await;
    server.reported(machine, &report).await;
    let changes: Vec<Value> =
        names.iter().map(|n| json!({"harness": "claude", "kind": "skill", "item_id": n, "enabled": true})).collect();
    server.put_assignment(ADMIN, machine, Value::Array(changes)).await;
}

#[tokio::test]
async fn fresh_server_has_one_root_commit_with_standard_skeleton() {
    let server = start().await;
    let bare = server.data().join("repo.git");
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
    let fleet: FleetView = server.get("/fleet").await;
    assert_eq!(fleet.baseline.as_deref(), Some(head.as_str()));
    let imports: ImportsView = eventually(|| async {
        let view: ImportsView = server.get("/imports").await;
        view.checked_at.map(|_| view)
    })
    .await;
    assert!(imports.errors.is_empty() && imports.updates.is_empty());
    let health = server.http().get(format!("{}/healthz", server.url())).send().await.unwrap();
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
        let req = server.http().get(server.api("/fleet")).bearer_auth(ADMIN);
        let req = if let Some(v) = version { req.header(header, v) } else { req };
        async move { req.send().await.unwrap() }
    };
    for version in [None, Some("1")] {
        let resp = fleet(version).await;
        assert_eq!(resp.status(), StatusCode::OK, "{version:?}");
        assert_eq!(resp.headers()[header], sk1ll_proto::PROTOCOL_VERSION.to_string());
        assert_eq!(resp.headers()[sk1ll_proto::VERSION_HEADER], env!("CARGO_PKG_VERSION"));
    }
    let refused = fleet(Some("2")).await;
    assert_eq!(refused.status(), StatusCode::UPGRADE_REQUIRED);
    assert_eq!(refused.headers()[header], "1");
    // A refusal names the server's release too, so the daemon can say which side is older.
    assert_eq!(refused.headers()[sk1ll_proto::VERSION_HEADER], env!("CARGO_PKG_VERSION"));
    let body: Value = refused.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("speaks version 1, this machine speaks 2"), "{body}");

    let mut ws = format!("ws://127.0.0.1:{}/api/ws", server.port()).into_client_request().unwrap();
    ws.headers_mut().insert("Authorization", format!("Bearer {ADMIN}").parse().unwrap());
    ws.headers_mut().insert(header, "2".parse().unwrap());
    match tokio_tungstenite::connect_async(ws).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status(), StatusCode::UPGRADE_REQUIRED.as_u16());
            assert_eq!(resp.headers()[sk1ll_proto::VERSION_HEADER], env!("CARGO_PKG_VERSION"));
        }
        other => panic!("expected a refused upgrade, got {:?}", other.map(|_| ())),
    }
}

/// Review finding: the branch that takes pushes is decided after bootstrap,
/// so an empty repo whose unborn HEAD named another branch accepts pushes to
/// the `main` that bootstrap created.
#[tokio::test]
async fn existing_empty_repo_on_another_branch_takes_pushes_to_main() {
    let server = start_with(|c| {
        std::fs::create_dir_all(&c.data_dir).unwrap();
        git(&c.data_dir, &["init", "--quiet", "--bare", "--initial-branch=legacy", "repo.git"]);
    })
    .await;
    let bare = server.data().join("repo.git");
    assert_eq!(git(&bare, &["rev-list", "--all", "--count"]), "1");
    git(&bare, &["cat-file", "-e", "main:sk1ll.toml"]);
    assert_eq!(git(&bare, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    let token = server.join("writer", &[]).await;
    let (_dir, head) = seed(&server, "writer", &token, &["personal/pdf/SKILL.md"]).await;
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
        .env("HOME", dir.path())
        .env("TMPDIR", dir.path())
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

/// Simulation: machines that committed offline push at once. One push lands
/// per round and the rest are refused, so no commit is lost; each loser
/// rebases and pushes again until every commit is on the shared root.
#[tokio::test]
async fn competing_pushes_land_one_at_a_time_and_losers_rebase_onto_the_shared_root() {
    const NAMES: [&str; 3] = ["a", "b", "c"];
    let server = start().await;
    let mut clones = Vec::new();
    for name in NAMES {
        let token = server.join(name, &[]).await;
        clones.push((name, clone(&server, name, &token).await));
    }
    let root = git(&clones[0].1.path().join("repo"), &["rev-parse", "HEAD"]);
    // Every machine commits before any pushes, with its own identity.
    for (name, dir) in &clones {
        let repo = dir.path().join("repo");
        assert_eq!(git(&repo, &["rev-parse", "HEAD"]), root);
        git(&repo, &["config", "user.name", name]);
        git(&repo, &["config", "user.email", &format!("{name}@example.com")]);
        write_item(&repo, &format!("personal/{name}/SKILL.md"), name);
        git(&repo, &["add", "-A"]);
        let out = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["commit", "--quiet", "-m", name])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    let bare = server.data().join("repo.git");
    let mut waiting: Vec<PathBuf> = clones.iter().map(|(_, dir)| dir.path().join("repo")).collect();
    while !waiting.is_empty() {
        let pushes = waiting.iter().map(|repo| {
            let repo = repo.clone();
            // Forced, so only the server's history protection refuses it.
            tokio::task::spawn_blocking(move || push_output(&repo, "+HEAD:main"))
        });
        let results = futures_util::future::join_all(pushes).await;
        let landed: Vec<_> = results.iter().map(|r| r.as_ref().unwrap().status.success()).collect();
        assert_eq!(landed.iter().filter(|l| **l).count(), 1, "exactly one competing push lands: {results:?}");
        let winner = &waiting[landed.iter().position(|l| *l).unwrap()];
        assert_eq!(git(&bare, &["rev-parse", "HEAD"]), git(winner, &["rev-parse", "HEAD"]));
        waiting = waiting.into_iter().zip(landed).filter(|(_, l)| !l).map(|(repo, _)| repo).collect();
        for repo in &waiting {
            git_async(repo, &["fetch", "--quiet", "origin"]).await;
            git(repo, &["rebase", "--quiet", "origin/main"]);
        }
    }
    assert_eq!(git(&bare, &["rev-list", "--max-parents=0", "HEAD"]), root);
    assert_eq!(git(&bare, &["rev-list", "--count", "HEAD"]), (NAMES.len() + 1).to_string());
    let authors = git(&bare, &["log", "--format=%an <%ae>", "HEAD~3..HEAD"]);
    for name in NAMES {
        git(&bare, &["cat-file", "-e", &format!("HEAD:personal/{name}/SKILL.md")]);
        assert!(authors.contains(&format!("{name} <{name}@example.com>")), "{authors}");
    }
}

#[tokio::test]
async fn join_codes_are_single_use() {
    let server = start().await;
    assert_eq!(server.http().post(server.api("/join-codes")).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
    assert_eq!(server.get_as("wrong", "/fleet").await.status(), StatusCode::UNAUTHORIZED);

    let code = server.join_code().await;
    let (left, right) = code.split_once('-').unwrap();
    assert_eq!((left.len(), right.len()), (5, 5));
    assert!(left.bytes().chain(right.bytes()).all(|b| b"ABCDEFGHJKMNPQRSTUVWXYZ23456789".contains(&b)));
    let normalized = code.to_lowercase().replace('-', " ");
    let resp = server.try_join(&normalized, "laptop", &["claude", "codex"]).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let joined: JoinResponse = resp.json().await.unwrap();
    assert_eq!(joined.repo_path, "/git/repo.git");
    let claude = joined.assignment.harness("claude");
    assert!(!claude.new_skills_default && !claude.new_rules_default);

    assert_eq!(server.try_join(&code, "desktop", &[]).await.status(), StatusCode::FORBIDDEN);
    assert_eq!(server.try_join("NOPE0-NOPE0", "desktop", &[]).await.status(), StatusCode::FORBIDDEN);

    // A joined machine can invite the next one; a taken id doesn't burn the code.
    let resp = server.create_code(&joined.token, None).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let code = resp.json::<JoinCode>().await.unwrap().code;
    assert_eq!(server.try_join(&code, "laptop", &[]).await.status(), StatusCode::CONFLICT);
    assert_eq!(server.try_join(&code, "desktop", &[]).await.status(), StatusCode::CREATED);

    let fleet: FleetView = server.get_json(&joined.token, "/fleet").await;
    let ids: Vec<_> = fleet.machines.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["desktop", "laptop"]);
    assert!(fleet.machines.iter().all(|m| m.status == MachineStatus::Offline && !m.connected));
}

#[tokio::test]
async fn an_expected_revision_and_registration_guard_a_write() {
    let server = start().await;
    let token = server.join("ws", &["claude"]).await;
    seed(&server, "ws", &token, &["personal/pdf/SKILL.md"]).await;
    let put = |body: Value| {
        let request = server.http().put(server.api("/machines/ws/assignment")).bearer_auth(ADMIN).json(&body);
        async move { request.send().await.unwrap() }
    };
    let snapshot: AssignmentSnapshot = server.get("/machines/ws/assignment").await;
    let registration = snapshot.registration.clone().expect("a registration");
    assert_eq!(server.row("ws").await.unwrap().registration, registration);
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
    assert_eq!(server.remove(ADMIN, "ws").await.status(), StatusCode::NO_CONTENT);
    server.join("ws", &["claude"]).await;
    let again: AssignmentSnapshot = server.get("/machines/ws/assignment").await;
    assert_ne!(again.registration.as_ref(), Some(&registration));
    assert_eq!(again.registration.as_ref().unwrap().len(), 32);
    assert!(again.revision > update.revision);
    let resp = put(expected(again.revision, &registration)).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert!(resp.text().await.unwrap().contains("joined the fleet again"));
    let resp = put(expected(again.revision, again.registration.as_deref().unwrap())).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(update.revision < again.revision);
}

#[tokio::test]
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
        .http()
        .put(server.api("/machines/ws/assignment"))
        .bearer_auth(&token)
        .json(&json!({"changes": [{"harness": "claude", "kind": "skill", "enabled": true}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);

    let log: Vec<LogEntry> = server.get("/log?machine=ws").await;
    assert_eq!(log.len(), 2);
    assert_eq!(log[1], *e, "newest first");

    let resp = server.undo(ADMIN, e.id).await;
    let undo: AssignmentUpdate = resp.json().await.unwrap();
    let u = &undo.entries[0];
    assert_eq!((u.author.as_str(), u.before, u.after), ("admin", Some(true), None));
    assert_eq!(u.cause, LogCause::Undo { entry: e.id });
    let a = server.get_json::<AssignmentSnapshot>(&token, "/machines/ws/assignment").await.assignment;
    assert!(!a.harness("claude").skills.contains_key(&id("pdf")));
    assert!(a.harness("claude").new_rules_default);

    let again = server.undo(ADMIN, e.id).await;
    assert!(again.json::<AssignmentUpdate>().await.unwrap().entries.is_empty());
    assert_eq!(server.get_as(&token, "/machines/nope/assignment").await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn log_filters_and_facets_cover_the_whole_log() {
    let server = start().await;
    let token = server.join("ws", &["claude", "codex"]).await;
    seed(&server, "ws", &token, &["personal/pdf/SKILL.md"]).await;
    let pdf = server
        .put_assignment(
            &token,
            "ws",
            json!([{"harness": "codex", "kind": "skill", "item_id": id("pdf"), "enabled": true}]),
        )
        .await
        .entries[0]
        .clone();
    let changed = server
        .put_assignment(ADMIN, "ws", json!([{"harness": "claude", "kind": "new_rules_default", "enabled": true}]))
        .await;
    let newest = changed.entries.last().unwrap().id;

    // The match is older than a one-entry page of everything.
    let page: Vec<LogEntry> = server.get("/log?limit=1").await;
    assert_eq!(page[0].id, newest);
    let found: Vec<LogEntry> = server.get("/log?item=Skill%3APD&limit=1").await;
    assert_eq!(found, std::slice::from_ref(&pdf));
    let found: Vec<LogEntry> = server.get(&format!("/log?harness=codex&author=ws&before={newest}")).await;
    assert_eq!(found, std::slice::from_ref(&pdf));
    let none: Vec<LogEntry> = server.get(&format!("/log?item=pdf&before={}", pdf.id)).await;
    assert!(none.is_empty());
    let admin: Vec<LogEntry> = server.get("/log?author=admin&machine=ws").await;
    assert!(!admin.is_empty() && admin.iter().all(|e| e.author == "admin" && e.harness == "claude"));
    // `item` matches an Item ID or a kind too, ignoring case, and is no pattern.
    let tools = server
        .put_assignment(
            ADMIN,
            "ws",
            json!([{"harness": "codex", "kind": "skill", "item_id": "id-tools", "name": "tools", "enabled": true}]),
        )
        .await
        .entries[0]
        .clone();
    let found: Vec<LogEntry> = server.get("/log?item=ID-Tools").await;
    assert_eq!(found, std::slice::from_ref(&tools));
    let found: Vec<LogEntry> = server.get("/log?item=new_rules").await;
    assert_eq!(found, std::slice::from_ref(changed.entries.last().unwrap()));
    assert!(server.get::<Vec<LogEntry>>("/log?item=%25").await.is_empty());
    let all: Vec<LogEntry> = server.get("/log").await;
    assert_eq!(server.get::<Vec<LogEntry>>("/log?machine=&harness=&item=").await, all);

    let facets: Value = server.get_json(&token, "/log/facets").await;
    assert_eq!(facets["machines"], json!(["ws"]));
    assert_eq!(facets["harnesses"], json!(["claude", "codex"]));
    let authors = facets["authors"].as_array().unwrap();
    assert!(authors.contains(&json!("ws")) && authors.contains(&json!("admin")), "{authors:?}");
    assert_eq!(server.http().get(server.api("/log/facets")).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
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
    let report = MachineReport { seq: 1, harnesses: harnesses(&["claude", "codex"]), ..Default::default() };
    ws_send(&mut ws, &DaemonMessage::Status(report)).await;
    // `codex` was new, so the server adds it all off and pushes the assignment.
    let ServerMessage::Assignment(b) = ws_next(&mut ws).await else { panic!("expected assignment") };
    let codex = b.assignment.harness("codex");
    assert!(b.assignment.harnesses.contains_key("codex"), "it has an entry of its own");
    assert!(!codex.new_skills_default && !codex.new_rules_default);
    assert!(b.revision > a.revision);
    // Registering it is logged, but there is nothing earlier to restore.
    let log: Vec<LogEntry> = server.get("/log?machine=ws").await;
    let added: Vec<_> = log.iter().filter(|e| e.cause == LogCause::HarnessAdded).collect();
    assert_eq!(added.len(), 2, "{log:?}");
    assert!(added.iter().all(|e| e.harness == "codex" && e.before.is_none() && e.after == Some(false)));
    assert_eq!(server.undo(ADMIN, added[0].id).await.status(), StatusCode::CONFLICT);

    let dir = clone(&server, "ws", &token).await;
    let repo = dir.path().join("repo");
    write_item(&repo, "personal/pdf/SKILL.md", "---\nname: pdf\n---\n");
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
        harnesses: harnesses(&["claude", "codex"]),
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

#[tokio::test]
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

    assert_eq!(server.remove(&b_token, "a").await.status(), StatusCode::NO_CONTENT);
    assert_eq!(ws_next(&mut ws).await, ServerMessage::Revoked);

    assert_eq!(server.get_as(&a_token, "/fleet").await.status(), StatusCode::UNAUTHORIZED);
    let git_resp = server.http().get(format!("{}/git/repo.git/info/refs?service=git-upload-pack", server.url()));
    let git_resp = git_resp.basic_auth("a", Some(&a_token)).send().await.unwrap();
    assert_eq!(git_resp.status(), StatusCode::UNAUTHORIZED);
    let ok = server.http().get(format!("{}/git/repo.git/info/refs?service=git-upload-pack", server.url()));
    assert_eq!(ok.basic_auth("b", Some(&b_token)).send().await.unwrap().status(), StatusCode::OK);

    let log: Vec<LogEntry> = server.get_json(&b_token, "/log?machine=a").await;
    let removed: Vec<_> = log.iter().filter(|e| e.cause == LogCause::MachineRemoved).collect();
    assert_eq!(removed.len(), 3, "two defaults and one toggle");
    assert!(removed.iter().all(|e| e.author == "b" && e.after.is_none()));
    assert_eq!(server.undo(&b_token, removed[0].id).await.status(), StatusCode::CONFLICT);

    let fleet: FleetView = server.get_json(&b_token, "/fleet").await;
    assert_eq!(fleet.machines.len(), 1);
}

#[tokio::test]
async fn import_updates_resolve_branches_and_tags_to_upstream_commits() {
    let server = start_with(|c| c.import_protocols.push_str(":file")).await;
    let token = server.join("ws", &[]).await;
    let refs = [("branch", "main"), ("tag", "v1"), ("full-tag", "refs/tags/v1"), ("light", "light"), ("head", "HEAD")];
    let pushed: Vec<_> = refs.iter().copied().chain([("missing", "nope")]).collect();
    let (locked, latest, _upstream) = push_moved_imports(&server, "ws", &token, &pushed).await;
    let view = eventually(|| async {
        let v: ImportsView = server.get_json(&token, "/imports").await;
        Some(v).filter(|v| !v.updates.is_empty() || !v.errors.is_empty())
    })
    .await;
    assert_eq!(view.updates.len(), refs.len(), "{view:?}");
    for (name, git_ref) in refs {
        let update = view.updates.iter().find(|update| update.name == name).unwrap();
        assert_eq!((update.git_ref.as_str(), &update.locked, &update.upstream), (git_ref, &locked, &latest));
    }
    let errors: Vec<_> = view.errors.iter().map(|e| (e.name.as_str(), e.message.as_str())).collect();
    assert_eq!(errors, [("missing", "ref `nope` not found")]);
    assert!(view.skipped.is_empty(), "{view:?}");
}

fn write_imports(repo: &Path, imports: &[(&str, &str, &str)], locked: &str) {
    let (mut manifest, mut lock) = (String::new(), String::new());
    for &(name, source, git_ref) in imports {
        manifest.push_str(&format!("[import.{name}]\nsource = {source:?}\nref = {git_ref:?}\nid = {name:?}\n"));
        lock.push_str(&format!(
            "[skills.{name}]\nsource = {source:?}\npath = \"\"\nref = {git_ref:?}\ncommit = \"{locked}\"\nhash = \"x\"\n"
        ));
        write(&repo.join(format!("imported/{name}/SKILL.md")), "imported content\n");
    }
    write(&repo.join("sk1ll.toml"), &manifest);
    write(&repo.join("sk1ll.lock"), &lock);
}

async fn push_moved_imports(
    server: &TestServer,
    id: &str,
    token: &str,
    refs: &[(&str, &str)],
) -> (String, String, tempfile::TempDir) {
    let upstream = tempfile::tempdir().unwrap();
    git(upstream.path(), &["init", "--quiet"]);
    write(&upstream.path().join("SKILL.md"), "v1\n");
    git(upstream.path(), &["add", "-A"]);
    git(upstream.path(), &["commit", "--quiet", "-m", "v1"]);
    let locked = git(upstream.path(), &["rev-parse", "HEAD"]);
    write(&upstream.path().join("SKILL.md"), "v2\n");
    git(upstream.path(), &["commit", "--quiet", "-am", "v2"]);
    let latest = git(upstream.path(), &["rev-parse", "HEAD"]);
    git(upstream.path(), &["tag", "-am", "annotated", "v1"]);
    git(upstream.path(), &["tag", "light"]);

    let dir = clone(server, id, token).await;
    let repo = dir.path().join("repo");
    let source = format!("file://{}", upstream.path().display());
    let imports: Vec<_> = refs.iter().map(|&(name, git_ref)| (name, source.as_str(), git_ref)).collect();
    write_imports(&repo, &imports, &locked);
    commit_push(server, &repo, "add imports").await;
    (locked, latest, upstream)
}

#[tokio::test]
async fn import_checks_can_be_turned_off_or_limited_to_hosts() {
    for (tune, want) in [
        ((|c: &mut Config| c.import_checks = false) as fn(&mut Config), "SK1LL_SERVER_IMPORT_CHECKS=off"),
        (|c: &mut Config| c.import_hosts = Some(vec!["github.com".into()]), "can't tell this source's host"),
    ] {
        let server = start_with(move |c| {
            c.import_protocols.push_str(":file");
            tune(c);
        })
        .await;
        let token = server.join("ws", &[]).await;
        let _upstream = push_moved_imports(&server, "ws", &token, &[("pdf", "main")]).await;
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
#[tokio::test]
async fn an_import_allowlist_skips_ambiguous_and_unlisted_hosts() {
    use std::os::unix::fs::PermissionsExt;
    let stub = tempfile::tempdir().unwrap();
    let log = stub.path().join("ssh.log");
    let ssh = stub.path().join("ssh");
    std::fs::write(&ssh, format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 1\n", log.display())).unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let server = start_with(move |c| {
        c.import_hosts = Some(vec!["github.com".into()]);
        c.import_ssh = ssh.display().to_string();
    })
    .await;
    let token = server.join("ws", &[]).await;
    let dir = clone(&server, "ws", &token).await;
    let repo = dir.path().join("repo");
    let candidates = [
        ("encoded-slash", "ssh://evil.example%2F@github.com/o/r", "percent escape"),
        ("encoded-at", "ssh://git%40example@github.com/o/r", "percent escape"),
        ("encoded-host", "ssh://git%68ub.com/o/r", "percent escape"),
        ("bad-port", "ssh://git@github.com:99999999/o/r", "can't tell"),
        ("multiple-at", "ssh://git@github.com@evil.example/o/r", "can't tell"),
        ("missing-host", "ssh://git@/r", "can't tell"),
        ("unlisted", "git@other.example:o/r", "`other.example` is not in"),
    ];
    let mut imports: Vec<_> = candidates.iter().map(|&(name, source, _)| (name, source, "main")).collect();
    imports.extend([("plain", "ssh://git@GitHub.com:22/o/r", "main"), ("scp", "git@github.com:o/r", "main")]);
    write_imports(&repo, &imports, &"a".repeat(40));
    commit_push(&server, &repo, "imports").await;
    let view = eventually(|| async {
        let v: ImportsView = server.get_json(&token, "/imports").await;
        Some(v).filter(|v| !v.skipped.is_empty() && !v.errors.is_empty())
    })
    .await;
    assert_eq!(view.skipped.len(), candidates.len(), "{view:?}");
    for (name, _, reason) in candidates {
        let skipped = view.skipped.iter().find(|s| s.name == name).unwrap();
        assert!(skipped.reason.contains(reason), "{skipped:?}");
    }
    assert_eq!(view.errors.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["plain", "scp"]);
    let seen = std::fs::read_to_string(&log).unwrap();
    assert!(
        seen.lines()
            .all(|l| { l.starts_with("-F /dev/null -o BatchMode=yes") && l.to_lowercase().contains("git@github.com") }),
        "{seen}"
    );
}

#[tokio::test]
async fn imports_cannot_use_local_or_external_command_transports_by_default() {
    let server = start().await;
    let token = server.join("writer", &[]).await;
    let upstream = tempfile::tempdir().unwrap();
    git::init_bare(upstream.path());
    let path = upstream.path().to_str().unwrap();
    let marker = upstream.path().join("executed");
    let external = format!("ext::sh -c touch% {}", marker.display());
    let dir = clone(&server, "writer", &token).await;
    let repo = dir.path().join("repo");
    write_imports(
        &repo,
        &[("local", path, "main"), ("file", &format!("file://{path}"), "main"), ("external", &external, "main")],
        &"a".repeat(40),
    );
    commit_push(&server, &repo, "add imports").await;
    let view = eventually(|| async {
        let v: ImportsView = server.get("/imports").await;
        Some(v).filter(|v| !v.errors.is_empty())
    })
    .await;
    assert!(view.updates.is_empty() && view.skipped.is_empty(), "{view:?}");
    assert_eq!(view.errors.len(), 3);
    for error in view.errors {
        let protocol = if error.name == "external" { "ext" } else { "file" };
        assert!(error.message.contains(&format!("transport '{protocol}' not allowed")), "{error:?}");
    }
    assert!(!marker.exists(), "the ext:: command ran");
}

/// The allowlist compares the host git would contact, so the skip reason
/// names that host, or says it can't tell. No source here gets a check: the
/// only listed host is none of them.
#[tokio::test]
async fn the_import_allowlist_parses_each_sources_host_strictly() {
    let server = start_with(|c| c.import_hosts = Some(vec!["allowed.invalid".into()])).await;
    let token = server.join("writer", &[]).await;
    let parsed = [
        ("https://github.com/o/r.git", "github.com"),
        ("HTTPS://GitHub.COM/o/r", "github.com"),
        ("https://user:pw@gitlab.com:8443/o/r", "gitlab.com"),
        ("http://gitlab.com", "gitlab.com"),
        ("ssh://git@github.com:22/o/r.git", "github.com"),
        ("git+ssh://github.com/o/r", "github.com"),
        ("ssh://deploy.bot_1-x@github.com/o/r", "github.com"),
        ("git://example.org/r", "example.org"),
        ("git@github.com:o/r.git", "github.com"),
        ("github.com:o/r", "github.com"),
        // scp-like has no port: this is path `22:o/r`.
        ("git@github.com:22:o/r", "github.com"),
        ("git@[::1]:o/r", "[::1]"),
        ("https://[2001:db8::1]:443/r", "[2001:db8::1]"),
        ("https://xn--bcher-kva.example/r", "xn--bcher-kva.example"),
        ("https://10.0.0.1/r", "10.0.0.1"),
    ];
    let ambiguous = [
        "file:///srv/repo",
        "/srv/repo",
        "./repo",
        "ext::sh -c touch% /tmp/x",
        "ext::git-remote-x",
        "fd::3",
        "https://a@github.com@evil.example/r",
        "https://github.com\\@evil.example/r",
        "https://github.com?@evil.example/r",
        "https://github.com#@evil.example/r",
        "https://bücher.example/r",
        "https://git%68ub.com/r",
        "https:///r",
        "ssh://git@/r",
        "https://github.com:/r",
        "https://github.com:99999999/r",
        "https://github.com:x/r",
        "[github.com:22]:o/r",
        "https://0x7f.1/r",
        "https://[::0001]/r",
        "ssh://-oProxyCommand=x/r",
        "https://github.com /r",
        // git decodes ssh URLs before finding the host: this is `evil.example`.
        "ssh://evil.example%2F@github.com/r",
        "ssh://evil.example%2f@github.com:22/r",
        "git+ssh://a%40evil.example@github.com/r",
        "https://evil.example%2F@github.com/r",
        "https://u:p%40x@github.com/r",
        "evil.example%2F@github.com:o/r",
        "ssh://git:pw@github.com/r",
        "ssh://-l@github.com/r",
        "ssh://@github.com/r",
        "g=t@github.com:o/r",
        "git$(x)@github.com:o/r",
    ];
    let names: Vec<String> = (0..parsed.len() + ambiguous.len()).map(|i| format!("i{i:02}")).collect();
    let sources = parsed.iter().map(|(source, _)| *source).chain(ambiguous);
    let imports: Vec<_> = names.iter().zip(sources).map(|(name, source)| (name.as_str(), source, "main")).collect();
    let dir = clone(&server, "writer", &token).await;
    let repo = dir.path().join("repo");
    write_imports(&repo, &imports, &"a".repeat(40));
    commit_push(&server, &repo, "imports").await;
    let view = eventually(|| async {
        let v: ImportsView = server.get("/imports").await;
        Some(v).filter(|v| v.skipped.len() == imports.len())
    })
    .await;
    assert!(view.errors.is_empty() && view.updates.is_empty(), "{view:?}");
    for (i, &(_, source, _)) in imports.iter().enumerate() {
        let reason = &view.skipped.iter().find(|s| s.name == names[i]).unwrap().reason;
        match parsed.get(i) {
            Some((_, host)) => assert!(reason.contains(&format!("host `{host}` is not in")), "{source}: {reason}"),
            None => assert!(reason.contains("can't tell this source's host"), "{source}: {reason}"),
        }
    }
}

/// An `http` server that redirects every request to `target`, and one that
/// counts the requests it gets; returns (redirecting URL, target's count).
async fn redirect_to_counter() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let hits = Arc::new(AtomicUsize::new(0));
    let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_port = target.local_addr().unwrap().port();
    let redirect = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let redirect_port = redirect.local_addr().unwrap().port();
    let serve = |listener: tokio::net::TcpListener, response: String, hits: Option<Arc<AtomicUsize>>| async move {
        loop {
            let (mut tcp, _) = listener.accept().await.unwrap();
            if let Some(hits) = &hits {
                hits.fetch_add(1, Ordering::SeqCst);
            }
            let mut request = [0; 4096];
            let _ = tcp.read(&mut request).await;
            let _ = tcp.write_all(response.as_bytes()).await;
        }
    };
    let location = format!("http://127.0.0.1:{target_port}/o/r.git/info/refs?service=git-upload-pack");
    let moved = format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let missing = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string();
    tokio::spawn(serve(redirect, moved, None));
    tokio::spawn(serve(target, missing, Some(hits.clone())));
    (format!("http://127.0.0.1:{redirect_port}/o/r.git"), hits)
}

/// Review finding: with an allowlist, a listed host must not redirect the
/// check to another one; without one, the server's own git config decides.
#[tokio::test]
async fn allowlisted_import_checks_do_not_follow_redirects() {
    for allowlist in [false, true] {
        let server = start_with(move |c| {
            if allowlist {
                c.import_hosts = Some(vec!["127.0.0.1".into()]);
            }
        })
        .await;
        let (source, hits) = redirect_to_counter().await;
        let token = server.join("writer", &[]).await;
        let dir = clone(&server, "writer", &token).await;
        let repo = dir.path().join("repo");
        write_imports(&repo, &[("moved", &source, "main")], &"a".repeat(40));
        commit_push(&server, &repo, "import").await;
        eventually(|| async {
            let v: ImportsView = server.get("/imports").await;
            (!v.errors.is_empty()).then_some(())
        })
        .await;
        let followed = hits.load(std::sync::atomic::Ordering::SeqCst) > 0;
        assert_eq!(followed, !allowlist, "allowlist {allowlist}: redirect followed {followed}");
    }
}

/// Review finding: daemons must hear about newly skipped imports even when
/// the updates didn't change, or the GUI never shows them.
#[tokio::test]
async fn newly_skipped_imports_are_announced() {
    let server = start_with(|c| c.import_checks = false).await;
    let token = server.join("ws", &[]).await;
    let mut ws = ws_connect(&server, &token).await;
    let _upstream = push_moved_imports(&server, "ws", &token, &[("pdf", "main")]).await;
    loop {
        if let ServerMessage::ImportsChanged { updates } = ws_next(&mut ws).await {
            assert!(updates.is_empty(), "{updates:?}");
            break;
        }
    }
    let view: ImportsView = server.get_json(&token, "/imports").await;
    assert_eq!(view.skipped.len(), 1, "{view:?}");
}

#[tokio::test]
async fn join_codes_carry_their_scope_and_expiry() {
    let server = start_with(|c| c.join_code_ttl = Duration::from_secs(3600)).await;
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

#[tokio::test]
async fn self_scoped_machines_manage_only_themselves() {
    let server = start().await;
    let me = server.join_scoped("me", &["claude"], Scope::SelfOnly).await;
    let other = server.join_scoped("other", &["claude"], Scope::Fleet).await;
    let fleet: FleetView = server.get_json(&me, "/fleet").await;
    let scopes: Vec<_> = fleet.machines.iter().map(|m| (m.id.as_str(), m.scope)).collect();
    assert_eq!(scopes, [("me", Scope::SelfOnly), ("other", Scope::Fleet)]);

    // Reading is open to every machine.
    for path in ["/fleet", "/machines/other/assignment", "/log", "/log/facets", "/imports"] {
        assert_eq!(server.get_as(&me, path).await.status(), StatusCode::OK, "{path}");
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
            .http()
            .post(server.api(&format!("/machines/{id}/assignment/copy-from")))
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
    assert_eq!(server.undo(&me, mine.entries[0].id).await.status(), StatusCode::OK);
    assert_eq!(server.undo(&me, by_other.entries[0].id).await.status(), StatusCode::OK);
    forbidden(server.undo(&me, theirs.entries[0].id).await).await;
    assert_eq!(server.undo(&me, 9999).await.status(), StatusCode::NOT_FOUND);

    // Removing others is refused; removing itself is allowed.
    forbidden(server.remove(&me, "other").await).await;
    forbidden(server.remove(&me, "ghost").await).await;
    assert!(server.row("other").await.is_some());
    assert_eq!(server.remove(&me, "me").await.status(), StatusCode::NO_CONTENT);
    assert_eq!(server.get_as(&me, "/fleet").await.status(), StatusCode::UNAUTHORIZED);

    // Rejoining takes the new code's scope.
    let me = server.join_scoped("me", &["claude"], Scope::Fleet).await;
    assert_eq!(server.row("me").await.unwrap().scope, Scope::Fleet);
    assert_eq!(server.create_code(&me, None).await.status(), StatusCode::CREATED);
    assert_eq!(server.remove(&me, "other").await.status(), StatusCode::NO_CONTENT);
    server.join_scoped("other", &[], Scope::SelfOnly).await;
    assert_eq!(server.row("other").await.unwrap().scope, Scope::SelfOnly);
}

#[tokio::test]
async fn replaced_socket_cannot_report_after_revocation() {
    let server = small_send_buffer(|c| c.send_timeout = Duration::from_secs(120)).await;
    let old_token = server.join("ws", &["claude"]).await;
    let mut old = ws_connect_small(&server, &old_token).await;
    // From here on `old` doesn't read, so the server's send of the big
    // assignment to it stalls.
    fatten(&server, &mut old, "ws").await;

    let mut replacement = ws_connect(&server, &old_token).await;
    ws_assignment(&mut replacement).await;
    assert_eq!(server.remove(ADMIN, "ws").await.status(), StatusCode::NO_CONTENT);
    while ws_next(&mut replacement).await != ServerMessage::Revoked {}

    let token = server.join("ws", &["claude"]).await;
    let mut fresh = ws_connect(&server, &token).await;
    let good = MachineReport {
        seq: 1,
        baseline: Some("good".into()),
        harnesses: harnesses(&["claude"]),
        ..Default::default()
    };
    ws_send(&mut fresh, &DaemonMessage::Status(good.clone())).await;
    server.reported("ws", &good).await;

    let evil =
        MachineReport { seq: 2, baseline: Some("evil".into()), harnesses: harnesses(&["evil"]), ..Default::default() };
    let _ = old.send(Message::text(serde_json::to_string(&DaemonMessage::Status(evil)).unwrap())).await;
    // The server let go of the old socket: it ends once what's buffered is read.
    let drained = tokio::time::timeout(Duration::from_secs(20), async { while let Some(Ok(_)) = old.next().await {} });
    drained.await.expect("the old socket is still open");

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(server.row("ws").await.unwrap().report, Some(good));
    let a = server.get_json::<AssignmentSnapshot>(&token, "/machines/ws/assignment").await.assignment;
    assert!(!a.harnesses.contains_key("evil"));
}

/// Review finding: an unread socket queued a full snapshot per change.
#[tokio::test]
async fn stalled_daemon_gets_only_the_latest_assignment() {
    let server = small_send_buffer(|c| c.send_timeout = Duration::from_secs(120)).await;
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
#[tokio::test]
async fn daemon_that_misses_the_send_deadline_is_disconnected() {
    let server = small_send_buffer(|c| c.send_timeout = Duration::from_millis(300)).await;
    let token = server.join("ws", &["claude"]).await;
    let mut ws = ws_connect_small(&server, &token).await;
    fatten(&server, &mut ws, "ws").await;
    eventually(|| async { (!server.row("ws").await?.connected).then_some(()) }).await;
}

/// Review finding: a removal while a send to the machine is under way must
/// still tell it it was revoked, last, before the socket closes.
#[tokio::test]
async fn a_daemon_removed_mid_send_still_hears_it_was_revoked() {
    let server = small_send_buffer(|c| c.send_timeout = Duration::from_secs(120)).await;
    let token = server.join("ws", &["claude"]).await;
    let mut ws = ws_connect_small(&server, &token).await;
    // From here on the server's send of the big assignment stalls.
    fatten(&server, &mut ws, "ws").await;
    assert_eq!(server.remove(ADMIN, "ws").await.status(), StatusCode::NO_CONTENT);
    let mut last = None;
    let drained = tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(Ok(msg)) = ws.next().await {
            match msg {
                Message::Text(text) => last = Some(serde_json::from_str::<ServerMessage>(&text).unwrap()),
                Message::Close(_) => break,
                _ => {}
            }
        }
    });
    drained.await.expect("the revoked socket stayed open");
    assert_eq!(last, Some(ServerMessage::Revoked));
}

/// Review finding: a write a session sent before another session replaced
/// it, still waiting for the push lock, must not land once it gets the lock.
#[tokio::test]
async fn a_replaced_sessions_pending_write_is_not_applied() {
    use sk1ll_proto::{AssignmentChanges, AssignmentWrite, ToggleChange};

    let server = start().await;
    let token = server.join("writer", &["claude"]).await;
    let mut first = ws_connect(&server, &token).await;
    ws_assignment(&mut first).await;
    assert!(matches!(ws_next(&mut first).await, ServerMessage::Nudge { .. }));
    let report =
        MachineReport { seq: 1, harnesses: harnesses(&["claude"]), inventory_complete: true, ..Default::default() };
    ws_send(&mut first, &DaemonMessage::Status(report.clone())).await;
    server.reported("writer", &report).await;
    // Changing a default reads the baseline under the push lock.
    let held = hold_receive_pack(&server, &token).await;
    let write = AssignmentWrite::Changes(AssignmentChanges {
        changes: vec![ToggleChange {
            harness: "claude".into(),
            kind: ToggleKind::NewRulesDefault,
            item_id: None,
            name: None,
            enabled: Some(true),
        }],
        expected: None,
    });
    ws_send(&mut first, &DaemonMessage::SetAssignment { req: 1, write }).await;
    let early = tokio::time::timeout(Duration::from_millis(300), first.next()).await;
    assert!(early.is_err(), "the write did not wait: {early:?}");
    let mut next = ws_connect(&server, &token).await;
    ws_assignment(&mut next).await;
    drop(held);
    let drained = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(Ok(msg)) = first.next().await {
            if let Message::Text(text) = msg {
                let msg = serde_json::from_str::<ServerMessage>(&text).unwrap();
                assert!(!matches!(msg, ServerMessage::AssignmentResult { .. }), "{msg:?}");
            }
        }
    });
    drained.await.expect("the replaced socket stayed open");
    let a: AssignmentSnapshot = server.get("/machines/writer/assignment").await;
    assert!(!a.assignment.harness("claude").new_rules_default);
    assert!(server.get::<Vec<LogEntry>>("/log?machine=writer").await.is_empty());
}

/// Review finding: one item ref proto didn't know dropped the whole report.
#[tokio::test]
async fn valid_report_entries_survive_malformed_neighbors() {
    let server = start().await;
    let token = server.join("ws", &["claude"]).await;
    let mut ws = ws_connect(&server, &token).await;
    let fleet: FleetView = server.get_json(&token, "/fleet").await;
    let text = json!({
        "type": "status",
        "seq": 1,
        "baseline": fleet.baseline,
        "drafts": ["skill:pdf", "manifest", "other:notes.txt", "bogus:x", 3],
        "unpushed": ["skill:wip", "rule:git", 3, "manifest", "other:notes.txt"],
        "conflicts": [{"item":"rule:git", "harness":"codex"}, {"item":"bad"}],
        "harnesses": [{"name": "claude", "kind": "claude"}],
    });
    ws.send(Message::text(text.to_string())).await.unwrap();
    let expected = MachineReport {
        seq: 1,
        baseline: fleet.baseline,
        drafts: vec![ItemRef::Skill("pdf".into()), ItemRef::Manifest, ItemRef::Other("notes.txt".into())],
        unpushed: vec![
            ItemRef::Skill("wip".into()),
            ItemRef::Rule("git".into()),
            ItemRef::Manifest,
            ItemRef::Other("notes.txt".into()),
        ],
        conflicts: vec![sk1ll_proto::ConflictRef { item: ItemRef::Rule("git".into()), harness: "codex".into() }],
        harnesses: harnesses(&["claude"]),
        ..Default::default()
    };
    server.reported("ws", &expected).await;
    assert_eq!(server.row("ws").await.unwrap().status, MachineStatus::Conflict);
}

/// Review finding: pushes in quick succession collapsed into fewer nudges.
/// A push that deletes an item changes no assignment (ADR 0006).
#[tokio::test]
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

    write_item(&repo, "personal/a/SKILL.md", "a\n");
    let c1 = commit_push(&server, &repo, "add a").await;
    std::fs::remove_dir_all(repo.join("personal/pdf")).unwrap();
    let c2 = commit_push(&server, &repo, "drop pdf").await;
    write_item(&repo, "personal/b/SKILL.md", "b\n");
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
#[tokio::test]
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
#[tokio::test]
async fn silent_connections_go_offline() {
    let server = start_with(|c| {
        c.ping_every = Duration::from_millis(200);
        c.offline_after = Duration::from_secs(1);
    })
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

    eventually(|| async { server.row("silent").await?.connected.then_some(()) }).await;
    let row = eventually(|| async {
        let row = server.row("silent").await?;
        (!row.connected && row.status == MachineStatus::Offline).then_some(row)
    })
    .await;
    assert!(row.last_seen.is_some());

    let live = server.row("live").await.unwrap();
    assert!(live.connected);
    assert_ne!(live.status, MachineStatus::Offline);
}

/// Review follow-up: changing a default must not flip existing items.
/// A harness the Server has no entry for yet has the defaults of a new
/// harness, both off: a change to it is logged against them, so it can be
/// undone, and one that sets what it already has changes nothing.
#[tokio::test]
async fn a_harness_without_an_entry_changes_from_all_off() {
    let server = start().await;
    let token = server.join("ws", &["claude"]).await;
    seed(&server, "ws", &token, &["personal/a/SKILL.md"]).await;
    let set = |enabled: bool| json!([{"harness": "codex", "kind": "new_skills_default", "enabled": enabled}]);

    let update = server.put_assignment(&token, "ws", set(false)).await;
    assert!(update.entries.is_empty(), "{:?}", update.entries);
    assert!(!update.assignment.harnesses.contains_key("codex"), "no toggles restating the default");

    let update = server.put_assignment(&token, "ws", set(true)).await;
    let kinds: Vec<_> = update.entries.iter().map(|e| (e.kind, e.before, e.after)).collect();
    assert_eq!(
        kinds,
        [(ToggleKind::Skill, None, Some(false)), (ToggleKind::NewSkillsDefault, Some(false), Some(true))]
    );
    let resp = server.undo(ADMIN, update.entries[1].id).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let a = server.get_json::<AssignmentSnapshot>(&token, "/machines/ws/assignment").await.assignment;
    assert!(!a.harness("codex").new_skills_default);
}

#[tokio::test]
async fn changing_a_default_keeps_existing_items() {
    let server = start().await;
    let token = server.join("ws", &["claude"]).await;
    let (dir, _) =
        seed(&server, "ws", &token, &["personal/a/SKILL.md", "personal/b/SKILL.md", "instructions/f.md"]).await;
    server
        .put_assignment(
            &token,
            "ws",
            json!([{"harness": "claude", "kind": "skill", "item_id": id("a"), "enabled": true}]),
        )
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
    assert!(claude.skill_enabled(&id("a")) && !claude.skill_enabled(&id("b")));
    assert!(claude.rules.is_empty() && !claude.rule_enabled(&id("rule-f")));

    // Items that appear later follow the new default.
    let repo = dir.path().join("repo");
    write_item(&repo, "personal/c/SKILL.md", "c\n");
    commit_push(&server, &repo, "add c").await;
    let a = server.get_json::<AssignmentSnapshot>(&token, "/machines/ws/assignment").await.assignment;
    assert!(a.skill_enabled("claude", &id("c")) && !a.skill_enabled("claude", &id("b")));
}

#[tokio::test]
async fn stalled_upload_does_not_block_the_next_push() {
    let server = start_with(|c| c.push_idle = Duration::from_secs(1)).await;
    let token = server.join("ws", &[]).await;
    let (dir, _) = seed(&server, "ws", &token, &["personal/pdf/SKILL.md"]).await;
    let repo = dir.path().join("repo");
    let _stalled = hold_receive_pack(&server, &token).await;
    write(&repo.join("personal/pdf/SKILL.md"), "updated\n");
    git(&repo, &["commit", "--quiet", "-am", "update"]);
    let started = std::time::Instant::now();
    let push = push_main(&repo).output();
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

#[tokio::test]
async fn idle_disconnect_does_not_refresh_last_seen() {
    let server = start_with(|c| {
        c.ping_every = Duration::from_millis(200);
        c.offline_after = Duration::from_secs(2);
    })
    .await;
    let token = server.join("silent", &[]).await;
    let mut silent = ws_connect(&server, &token).await;
    let report = MachineReport { seq: 1, ..Default::default() };
    ws_send(&mut silent, &DaemonMessage::Status(report.clone())).await;
    server.reported("silent", &report).await;
    let last_frame = server.row("silent").await.unwrap().last_seen;
    // Do not read again, so no pong is sent.
    let disconnected = eventually(|| async {
        let row = server.row("silent").await?;
        (!row.connected).then_some(row)
    })
    .await;
    assert_eq!(disconnected.last_seen, last_frame, "closing the socket is not an inbound frame");
    eventually(|| async { (server.row("silent").await?.status == MachineStatus::Offline).then_some(()) }).await;
}

#[tokio::test]
async fn websocket_upgrade_alone_does_not_refresh_last_seen() {
    let server = start().await;
    let token = server.join("silent", &[]).await;
    let mut ws = ws_connect(&server, &token).await;
    ws_assignment(&mut ws).await;
    let row = server.row("silent").await.unwrap();
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

    let server = start_with(move |c| {
        c.push_idle = idle;
        c.push_max = max;
    })
    .await;
    let token = server.join("ws", &["claude"]).await;
    let (dir, first) = seed(&server, "ws", &token, &["personal/keep/SKILL.md"]).await;
    let repo = dir.path().join("repo");
    write_item(&repo, "personal/removed/SKILL.md", "removed\n");
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

    let bare = server.data().join("repo.git");
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
    let mut stalled = socket.connect((std::net::Ipv4Addr::LOCALHOST, server.port()).into()).await.unwrap();
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

    let push = push_main(&repo).output();
    let out = tokio::time::timeout(Duration::from_secs(4), push).await.expect("push lock was not released").unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

#[cfg(unix)]
#[tokio::test]
async fn backend_deadline_runs_follow_up_and_releases_push_lock() {
    push_timeout_runs_follow_up("exec sleep 5", Duration::from_secs(10), Duration::from_secs(1)).await;
}

#[cfg(unix)]
#[tokio::test]
async fn backend_deadline_kills_pre_receive_descendants() {
    use std::os::unix::fs::PermissionsExt;

    let server = start_with(|c| c.push_max = Duration::from_secs(1)).await;
    let token = server.join("ws", &["claude"]).await;
    let (dir, baseline) = seed(&server, "ws", &token, &["README.md"]).await;
    let repo = dir.path().join("repo");
    write_item(&repo, "personal/wip/SKILL.md", "committed\n");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "--quiet", "-m", "commit"]);
    let bare = server.data().join("repo.git");
    let hook = bare.join("hooks/pre-receive");
    // The hook also leaves a TERM-resistant child that holds no pipe, so it
    // outlives the backend; cleanup must wait for the whole group.
    let hook_text = r#"#!/bin/sh
sh -c 'trap "" TERM; echo $$ > descendant.tmp; mv descendant.tmp descendant; exec sleep 30' </dev/null >/dev/null 2>&1 &
touch pre-receive-started
sleep 3
touch pre-receive-finished
"#;
    std::fs::write(&hook, hook_text).unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut push = push_main(&repo).spawn().unwrap();
    eventually(|| async { bare.join("pre-receive-started").exists().then_some(()) }).await;
    let out = tokio::time::timeout(Duration::from_secs(5), push.wait()).await.expect("push did not time out").unwrap();
    assert!(!out.success(), "a push past its deadline must fail");
    // Wait past the hook's sleep: killing only http-backend lets receive-pack
    // update HEAD after the server has already checked it during follow-up.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(git(&bare, &["rev-parse", "HEAD"]), baseline);
    assert!(!bare.join("pre-receive-finished").exists(), "the timed-out hook survived");
    let descendant: libc::pid_t = sk1ll_testkit::read(&bare.join("descendant")).trim().parse().unwrap();
    // Signal 0 only checks that the process exists.
    assert_ne!(unsafe { libc::kill(descendant, 0) }, 0, "the TERM-resistant descendant survived");
    let fleet: FleetView = server.get_json(&token, "/fleet").await;
    assert_eq!(fleet.baseline.as_deref(), Some(baseline.as_str()));

    // The next push must acquire the lock after the entire group is gone.
    std::fs::remove_file(&hook).unwrap();
    tokio::time::timeout(Duration::from_secs(4), git_async(&repo, &["push", "--quiet", "origin", "HEAD:main"]))
        .await
        .expect("push lock was not released");
    let committed = git(&repo, &["rev-parse", "HEAD"]);
    eventually(|| async {
        let fleet: FleetView = server.get_json(&token, "/fleet").await;
        (fleet.baseline.as_deref() == Some(committed.as_str())).then_some(())
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn stalled_response_runs_follow_up_and_releases_push_lock() {
    push_timeout_runs_follow_up("head -c 4194304 /dev/zero", Duration::from_secs(1), Duration::from_secs(10)).await;
}

/// Default changes and their undo preserve the destination's reported local content.
#[tokio::test]
async fn defaults_and_undo_materialize_destination_drafts_and_unpushed() {
    let server = start().await;
    let alpha = server.join("alpha", &["claude"]).await;
    let beta = server.join("beta", &["claude"]).await;
    let mut ws = ws_connect(&server, &alpha).await;
    let mut other_ws = ws_connect(&server, &beta).await;
    let mut report = MachineReport {
        seq: 1,
        inventory_complete: true,
        harnesses: harnesses(&["claude"]),
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
        let response = server.undo(&beta, entry.id).await;
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
    let conn = rusqlite::Connection::open(server.data().join("server.db")).unwrap();
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
    let server = start_with(|c| c.join_code_ttl = Duration::from_secs(TTL as u64)).await;
    let expired = server.join_code().await;
    backdate_code(&server, &expired, TTL + 60);
    let boundary = server.join_code().await;
    backdate_code(&server, &boundary, TTL);
    let almost = server.join_code().await;
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
#[tokio::test]
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
        let out = push_output(&repo, &refspec);
        assert!(!out.status.success(), "pushing {refspec} must fail");
        // Hiding refs matches by prefix, so the hook is what refuses this one.
        if refspec.ends_with("main/x") {
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(stderr.contains("only refs/heads/main takes pushes, not refs/heads/main/x"), "{stderr}");
        }
    }
    let bare = server.data().join("repo.git");
    assert_eq!(git(&bare, &["for-each-ref", "--format=%(refname)"]), "refs/heads/main");
    assert_eq!(git(&bare, &["rev-parse", "refs/heads/main"]), head);
}

/// The pre-receive hook refuses a baseline with an item (personal skill,
/// Rule, Import) that has no Item ID or shares one, naming each, so every
/// baseline the server reads gives each item an Item ID of its own. The hook
/// runs the check binary from a path that needs shell quoting.
#[cfg(unix)]
#[tokio::test]
async fn pushes_need_an_item_id_of_its_own_on_every_item() {
    let bin = tempfile::tempdir().unwrap();
    let check = bin.path().join("it's a $dir").join("sk1ll-server");
    std::fs::create_dir_all(check.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_sk1ll-server"), &check).unwrap();
    let server = start_with(move |c| c.push_check = Some(check.clone())).await;
    let token = server.join("writer", &[]).await;
    let (dir, head) = seed(&server, "writer", &token, &["personal/kept/SKILL.md"]).await;
    let repo = dir.path().join("repo");
    write(&repo.join("personal/bare/SKILL.md"), "no id\n");
    write(&repo.join("instructions/plain.md"), "No id.\n");
    write(&repo.join("imported/web/SKILL.md"), "vendored\n");
    write(&repo.join("sk1ll.toml"), "[import.web]\nsource = 'github:o/r'\n");
    write_item(&repo, "personal/copy/SKILL.md", "copied\n");
    write(&repo.join("personal/copy/.sk1ll-id"), "kept\n");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "--quiet", "-m", "items without their own Item IDs"]);
    let refused = push_output(&repo, "HEAD:main");
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    for expected in [
        "skill:bare (personal/bare/.sk1ll-id) has no Item ID",
        "rule:plain (instructions/plain.md) has no Item ID",
        "skill:web (sk1ll.toml [import.web]) has no Item ID",
        "skill:copy (personal/copy/.sk1ll-id), skill:kept (personal/kept/.sk1ll-id) share the Item ID `kept`",
        "Run `sk1ll repair-ids`",
    ] {
        assert!(stderr.contains(expected), "{expected}: {stderr}");
    }
    let bare = server.data().join("repo.git");
    assert_eq!(git(&bare, &["rev-parse", "refs/heads/main"]), head);

    write(&repo.join("personal/bare/.sk1ll-id"), "bare\n");
    write(&repo.join("instructions/plain.md"), "---\nid: plain\n---\nNo id.\n");
    write(&repo.join("sk1ll.toml"), "[import.web]\nsource = 'github:o/r'\nid = 'web'\n");
    write(&repo.join("personal/copy/.sk1ll-id"), "copy\n");
    let fixed = commit_push(&server, &repo, "every item has its own Item ID").await;
    assert_eq!(git(&bare, &["rev-parse", "refs/heads/main"]), fixed);
    let fleet: FleetView = server.get("/fleet").await;
    assert_eq!(fleet.baseline.as_deref(), Some(fixed.as_str()));
}

#[tokio::test]
async fn main_rejects_rewrites_and_deletion() {
    let server = start().await;
    let token = server.join("writer", &[]).await;
    let dir = clone(&server, "writer", &token).await;
    let repo = dir.path().join("repo");
    git(&repo, &["commit", "--allow-empty", "-m", "fast forward"]);
    git(&repo, &["push", "origin", "HEAD:main"]);
    git(&repo, &["reset", "--hard", "HEAD~1"]);
    for args in [["push", "--force", "origin", "HEAD:main"], ["push", "--delete", "origin", "main"]] {
        let out = git::output(&repo, &args);
        assert!(!out.status.success());
    }
}

#[tokio::test]
async fn copy_machine_assignment_previews_merges_replaces_and_logs() {
    let server = start().await;
    let source = server.join_scoped("source", &["claude"], Scope::SelfOnly).await;
    let target = server.join_scoped("target", &["claude", "codex"], Scope::SelfOnly).await;
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
    let post = |token: &str, path: &str, body: Value| {
        server.http().post(server.api(path)).bearer_auth(token).json(&body).send()
    };
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
    assert_eq!(server.get::<AssignmentSnapshot>("/machines/target/assignment").await.revision, update.revision);
    replace["expected"] = preview["target"].clone();
    replace["source_expected"] = preview["source"].clone();
    let copied: AssignmentUpdate =
        post(&target, path, replace.clone()).await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let src: AssignmentSnapshot = server.get("/machines/source/assignment").await;
    assert_eq!(copied.assignment.harness("codex"), src.assignment.harness("claude"));
    assert_eq!(copied.assignment.harness("claude"), update.assignment.harness("claude"));
    assert_eq!(copied.entries.len(), 4);
    // The preview lists the very changes the copy logged, before and after.
    let previewed: Vec<sk1ll_proto::CopyChange> = serde_json::from_value(preview["changes"].clone()).unwrap();
    let logged: Vec<_> = copied
        .entries
        .iter()
        .map(|e| sk1ll_proto::CopyChange {
            harness: e.harness.clone(),
            kind: e.kind,
            item_id: e.item_id.clone(),
            name: e.name.clone(),
            before: e.before,
            after: e.after,
        })
        .collect();
    assert_eq!(previewed, logged);
    assert!(previewed.iter().any(|c| c.name.as_deref() == Some("private")), "{previewed:?}");
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
    let restored: AssignmentSnapshot = server.get("/machines/target/assignment").await;
    assert_eq!(restored.assignment, update.assignment);
}

/// A Machine seeds one of its Harnesses from another of its own, also one it
/// hasn't reported yet (`sk1ll harnesses add --copy-assignment-from` copies
/// before the Harness exists); only the Machine itself may name such a
/// Harness, and once it reports it the copied column stays.
#[tokio::test]
async fn copy_within_one_machine_seeds_a_harness_before_it_is_reported() {
    let server = start().await;
    let token = server.join_scoped("solo", &["claude", "codex"], Scope::SelfOnly).await;
    let other = server.join("other", &["claude"]).await;
    server
        .put_assignment(
            &token,
            "solo",
            json!([
                {"harness":"claude","kind":"skill","item_id":"pdf","name":"pdf","enabled":true},
                {"harness":"claude","kind":"rule","item_id":"git","enabled":false},
                {"harness":"claude","kind":"new_skills_default","enabled":true},
                {"harness":"codex","kind":"skill","item_id":"old","enabled":true}
            ]),
        )
        .await;
    let path = "/machines/solo/assignment/copy-from";
    let post = |token: &str, body: Value| server.http().post(server.api(path)).bearer_auth(token).json(&body).send();
    let copy = |to: &str| json!({"source":"solo","mode":"replace","harnesses":[{"from":"claude","to":to}]});

    // A reported Harness: replaced, its own toggles cleared.
    let update: AssignmentUpdate =
        post(&token, copy("codex")).await.unwrap().error_for_status().unwrap().json().await.unwrap();
    assert_eq!(update.assignment.harness("codex"), update.assignment.harness("claude"));

    // One not reported yet: only the Machine itself may name it, and only by a harness name.
    for caller in [ADMIN, other.as_str()] {
        assert_eq!(post(caller, copy("claude_2")).await.unwrap().status(), StatusCode::BAD_REQUEST, "{caller}");
    }
    assert_eq!(post(&token, copy("../x")).await.unwrap().status(), StatusCode::BAD_REQUEST);
    let seeded: AssignmentUpdate =
        post(&token, copy("claude_2")).await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let claude = seeded.assignment.harness("claude").clone();
    assert_eq!(seeded.assignment.harness("claude_2"), &claude);
    assert!(seeded.entries.iter().all(|e| e.harness == "claude_2" && e.cause == LogCause::Change));

    // Its report then finds the column there: nothing is reset to all off.
    let mut ws = ws_connect(&server, &token).await;
    ws_assignment(&mut ws).await;
    ws_send(&mut ws, &DaemonMessage::Hello { machine_id: "solo".into() }).await;
    let report = MachineReport { seq: 1, harnesses: harnesses(&["claude", "claude_2", "codex"]), ..Default::default() };
    ws_send(&mut ws, &DaemonMessage::Status(report.clone())).await;
    server.reported("solo", &report).await;
    let now: AssignmentSnapshot = server.get("/machines/solo/assignment").await;
    assert_eq!(now.revision, seeded.revision);
    assert_eq!(now.assignment.harness("claude_2"), &claude);
    let log: Vec<LogEntry> = server.get("/log?machine=solo").await;
    assert!(!log.iter().any(|e| e.cause == LogCause::HarnessAdded && e.harness == "claude_2"), "{log:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_rules_keep_their_claim_and_incomplete_baselines_refuse_prune() {
    let server = start().await;
    let token = server.join("alpha", &["claude"]).await;
    let dir = clone(&server, "alpha", &token).await;
    let repo = dir.path().join("repo");
    let rule = format!("---\nid: retained-rule\n---\n{}", "body\n".repeat(250_000));
    write(&repo.join("instructions/large.md"), &rule);
    // The manifest used to share the Rule body cap.
    let manifest =
        format!("{}\n[import.pdf]\nsource = 'github:o/r'\nid = 'retained-import'\n", "# description\n".repeat(100_000));
    write(&repo.join("sk1ll.toml"), &manifest);
    write(&repo.join("imported/pdf/SKILL.md"), "imported content");
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

    write(&repo.join("instructions/broken.md"), "---\nid: [invalid]\n---\n");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "--quiet", "-m", "unreadable claim"]);
    let refused = push_output(&repo, "HEAD:main");
    assert!(!refused.status.success(), "a push with an unreadable Item ID is refused");
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("instructions/broken.md") && stderr.contains("sk1ll repair-ids"), "{stderr}");
    // A baseline that never went through the check (written on the server
    // by hand, say) is served, but nothing reads its Item IDs.
    let bare = server.data().join("repo.git");
    git(&bare, &["fetch", "--quiet", &repo.to_string_lossy(), "HEAD:refs/heads/main"]);
    for path in ["/assignment/orphans", "/fleet"] {
        let status = server.get_as(&token, path).await.status();
        assert_eq!(status, if path == "/fleet" { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE });
    }
    // Boot the Server over this same Baseline. Inventory errors must not prevent startup.
    sk1ll_server::Server::open(sk1ll_server::Config::new(server.data())).await.unwrap();
    let prune = server.http().post(server.api("/assignment/orphans/prune")).bearer_auth(&token).send().await.unwrap();
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
    let bare = server.data().join("repo.git");
    // Simulate the gap before post-push follow-up publishes the new HEAD.
    let push_unannounced = |name: &str, id: &str| {
        write(&work.join(format!("instructions/{name}.md")), &format!("---\nid: {id}\n---\nbody"));
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
    let undo: AssignmentUpdate = server.undo(&token, undo_id).await.error_for_status().unwrap().json().await.unwrap();
    assert_eq!(undo.assignment.harness("claude").rules.get(&id("second-id")), Some(&true));
    assert!(!undo.assignment.harness("claude").new_rules_default);
}

/// Hyper only sends 100 Continue once receive-pack owns the push lock and polls the body.
async fn hold_receive_pack(server: &TestServer, token: &str) -> tokio::net::TcpStream {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let mut tcp = tokio::net::TcpStream::connect(("127.0.0.1", server.port())).await.unwrap();
    let auth = STANDARD.encode(format!("writer:{token}"));
    tcp.write_all(format!(
        "POST /git/repo.git/git-receive-pack HTTP/1.1\r\nHost: localhost\r\nAuthorization: Basic {auth}\r\nContent-Type: application/x-git-receive-pack-request\r\nContent-Length: 100\r\nExpect: 100-continue\r\n\r\n"
    ).as_bytes()).await.unwrap();
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), BufReader::new(&mut tcp).read_line(&mut line)).await.unwrap().unwrap();
    assert!(line.starts_with("HTTP/1.1 100"), "{line}");
    tcp
}

/// A bodyless receive-pack POST as `id`.
fn receive_pack(server: &TestServer, id: &str, token: &str) -> reqwest::RequestBuilder {
    server
        .http()
        .post(format!("{}/git/repo.git/git-receive-pack", server.url()))
        .basic_auth(id, Some(token))
        .header("content-type", "application/x-git-receive-pack-request")
        .timeout(Duration::from_secs(5))
}

/// What happens at one server's push lock, through its failpoints: each
/// request that gets in line for it, and each removal that starts its
/// database work there and then waits for [`release`](Self::release).
#[cfg(feature = "failpoints")]
struct PushLock {
    events: tokio::sync::mpsc::UnboundedReceiver<sk1ll_core::failpoints::Point>,
    release: Option<std::sync::mpsc::Sender<()>>,
    _watch: sk1ll_core::failpoints::Guard,
}

#[cfg(feature = "failpoints")]
impl PushLock {
    fn watch(server: &TestServer) -> Self {
        use sk1ll_core::failpoints::{self, Action, Point};
        let (tx, events) = tokio::sync::mpsc::unbounded_channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let (repo, released) = (server.data().join("repo.git"), std::sync::Mutex::new(released));
        let _watch = failpoints::install_shared(move |point, path| {
            if path == repo && matches!(point, Point::ServerPushLockQueued | Point::ServerRemovalEntered) {
                tx.send(point).unwrap();
                if point == Point::ServerRemovalEntered {
                    let _ = released.lock().unwrap().recv();
                }
            }
            Action::Continue
        });
        Self { events, release: Some(release), _watch }
    }

    async fn next(&mut self) -> sk1ll_core::failpoints::Point {
        tokio::time::timeout(Duration::from_secs(10), self.events.recv()).await.expect("no push lock event").unwrap()
    }

    /// Lets removals waiting in their database work, and later ones, go on.
    fn release(&mut self) {
        self.release = None;
    }
}

#[cfg(feature = "failpoints")]
#[tokio::test]
async fn unconfirmed_cleanup_refuses_pushes_and_prune_until_restart() {
    use sk1ll_core::failpoints::{self, Action, Point};

    let mut server = start_with(|c| c.push_idle = Duration::from_secs(10)).await;
    let token = server.join("writer", &["claude"]).await;
    let (dir, head) = seed(&server, "writer", &token, &["README.md"]).await;
    let repo = dir.path().join("repo");
    let update = server.put_assignment(&token, "writer", json!([skill_toggle("gone", true)])).await;
    let bare = server.data().join("repo.git");
    let failed = failpoints::install_shared(move |point, path| {
        if point == Point::ServerCleanupConfirmation && path == bare {
            Action::Fail(std::io::ErrorKind::Other)
        } else {
            Action::Continue
        }
    });
    let mut lock = PushLock::watch(&server);
    let stalled = hold_receive_pack(&server, &token).await;
    // In line before the cleanup fails, so it is refused after the wait.
    let queued = tokio::spawn(receive_pack(&server, "writer", &token).send());
    assert_eq!(lock.next().await, Point::ServerPushLockQueued);
    drop(stalled);
    let discovery = format!("{}/git/repo.git/info/refs?service=git-receive-pack", server.url());
    for response in [
        queued.await.unwrap().unwrap(),
        receive_pack(&server, "writer", &token).send().await.unwrap(),
        server.http().get(discovery).basic_auth("writer", Some(&token)).send().await.unwrap(),
    ] {
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(response.text().await.unwrap().contains("unconfirmed backend cleanup"));
    }
    let health = server.http().get(format!("{}/healthz", server.url())).send().await.unwrap();
    assert_eq!(health.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        health.json::<Value>().await.unwrap(),
        json!({
            "status":"degraded", "push_degraded":true,
            "version":sk1ll_core::VERSION, "protocol":sk1ll_proto::PROTOCOL_VERSION,
        })
    );
    let prune = server.http().post(server.api("/assignment/orphans/prune")).bearer_auth(&token).send().await.unwrap();
    assert_eq!(prune.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(prune.json::<sk1ll_proto::ErrorBody>().await.unwrap().error.contains("degraded"));
    let fetched = clone(&server, "writer", &token).await;
    assert_eq!(git(&fetched.path().join("repo"), &["rev-parse", "HEAD"]), head);

    drop(failed);
    server.restart().await;
    let snapshot: AssignmentSnapshot = server.get("/machines/writer/assignment").await;
    assert_eq!((snapshot.revision, snapshot.assignment), (update.revision, update.assignment));
    assert_eq!(server.get::<FleetView>("/fleet").await.baseline.as_deref(), Some(head.as_str()));
    write(&repo.join("README.md"), "after restart\n");
    commit_push(&server, &repo, "push after restart").await;
}

/// Simulation: the server stops while a receive-pack holds the push lock and
/// after a push it never announced moved HEAD. Restarted on the same data,
/// it serves that HEAD, keeps every Assignment, and takes the next push.
#[tokio::test]
async fn a_restart_mid_push_keeps_assignments_and_serves_the_current_head() {
    let mut server = start().await;
    let token = server.join("writer", &["claude"]).await;
    let (dir, _) = seed(&server, "writer", &token, &["personal/pdf/SKILL.md"]).await;
    let repo = dir.path().join("repo");
    let update = server
        .put_assignment(
            &token,
            "writer",
            json!([{"harness": "claude", "kind": "skill", "item_id": "pdf", "enabled": true}]),
        )
        .await;
    let stalled = hold_receive_pack(&server, &token).await;
    write_item(&repo, "personal/unannounced/SKILL.md", "x\n");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "--quiet", "-m", "unannounced"]);
    let bare = server.data().join("repo.git");
    git(&repo, &["push", "--quiet", bare.to_str().unwrap(), "HEAD:main"]);
    let moved = git(&repo, &["rev-parse", "HEAD"]);
    assert_ne!(server.get::<FleetView>("/fleet").await.baseline.as_deref(), Some(moved.as_str()));

    server.restart().await;
    drop(stalled);
    assert_eq!(server.get::<FleetView>("/fleet").await.baseline.as_deref(), Some(moved.as_str()));
    let health: Value =
        server.http().get(format!("{}/healthz", server.url())).send().await.unwrap().json().await.unwrap();
    assert_eq!((&health["status"], &health["push_degraded"]), (&json!("ok"), &json!(false)));
    let snapshot: AssignmentSnapshot = server.get("/machines/writer/assignment").await;
    assert_eq!((snapshot.revision, snapshot.assignment), (update.revision, update.assignment));
    let mut ws = ws_connect(&server, &token).await;
    ws_assignment(&mut ws).await;
    assert_eq!(ws_next(&mut ws).await, ServerMessage::Nudge { baseline: moved });
    write_item(&repo, "personal/after/SKILL.md", "x\n");
    let after = commit_push(&server, &repo, "after restart").await;
    assert_eq!(ws_next(&mut ws).await, ServerMessage::Nudge { baseline: after });
}

/// The order in which `n` concurrent requests are first polled, from a
/// fixed-seed generator so a failing round can be replayed.
fn shuffled(n: usize, seed: &mut u64) -> Vec<usize> {
    let mut order: Vec<usize> = (0..n).collect();
    for i in (1..n).rev() {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        order.swap(i, (*seed >> 33) as usize % (i + 1));
    }
    order
}

/// Simulation: several machines write one Assignment at once. Of writes
/// expecting the same revision exactly one commits; writes expecting none
/// all commit, each at a revision of its own.
#[tokio::test]
async fn concurrent_assignment_writes_commit_once_per_revision() {
    const CLIENTS: usize = 4;
    let server = start().await;
    server.join("alpha", &["claude"]).await;
    let mut tokens = Vec::new();
    for c in 0..CLIENTS {
        tokens.push(server.join(&format!("c{c}"), &[]).await);
    }
    let route = "/machines/alpha/assignment";
    let write = |c: usize, item: String, expected: Option<u64>| {
        let mut body = json!({"changes": [{"harness": "claude", "kind": "skill", "item_id": item, "enabled": true}]});
        if let Some(revision) = expected {
            body["expected"] = json!({"revision": revision});
        }
        server.http().put(server.api(route)).bearer_auth(&tokens[c]).json(&body).send()
    };
    let mut seed = 0x5eed;
    let mut revision = server.get::<AssignmentSnapshot>(route).await.revision;
    let mut items = 0;
    for round in 0..6 {
        let order = shuffled(CLIENTS, &mut seed);
        let responses =
            futures_util::future::join_all(order.iter().map(|&c| write(c, format!("c{c}-r{round}"), Some(revision))))
                .await;
        let mut winners = Vec::new();
        for response in responses {
            let response = response.unwrap();
            match response.status() {
                StatusCode::OK => winners.push(response.json::<AssignmentUpdate>().await.unwrap()),
                StatusCode::CONFLICT => {
                    let error = response.json::<sk1ll_proto::ErrorBody>().await.unwrap().error;
                    assert!(error.contains("changed since revision"), "{error}");
                }
                status => panic!("round {round} ({order:?}): {status}"),
            }
        }
        assert_eq!(winners.len(), 1, "round {round} ({order:?})");
        let winner = winners.pop().unwrap();
        assert!(winner.revision > revision);
        assert_eq!(winner.entries.len(), 1);
        items += 1;
        let current: AssignmentSnapshot = server.get(route).await;
        assert_eq!((current.revision, &current.assignment), (winner.revision, &winner.assignment));
        assert_eq!(current.assignment.harness("claude").skills.len(), items);
        revision = winner.revision;
    }

    let order = shuffled(CLIENTS, &mut seed);
    let responses = futures_util::future::join_all(order.iter().map(|&c| write(c, format!("blind-{c}"), None))).await;
    let mut revisions = Vec::new();
    for response in responses {
        let update: AssignmentUpdate = response.unwrap().error_for_status().unwrap().json().await.unwrap();
        revisions.push(update.revision);
    }
    revisions.sort_unstable();
    revisions.dedup();
    assert_eq!(revisions.len(), CLIENTS, "{revisions:?}");
    assert!(revisions[0] > revision);
    let current: AssignmentSnapshot = server.get(route).await;
    assert_eq!(current.revision, revisions[CLIENTS - 1]);
    assert_eq!(current.assignment.harness("claude").skills.len(), items + CLIENTS);
    let log: Vec<LogEntry> = server.get("/log?machine=alpha").await;
    assert_eq!(log.len(), items + CLIENTS);
}

/// A removal waits for the running push. A push that gets in line behind
/// it checks its Machine's registration again once it has the lock and is
/// refused, even when the removal's caller disconnected while the removal
/// ran: the removal keeps the lock until its database work commits.
#[cfg(feature = "failpoints")]
#[tokio::test]
async fn a_push_queued_behind_a_removal_checks_registration_again() {
    use sk1ll_core::failpoints::Point;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let server = start().await;
    let old = server.join("old", &[]).await;
    let writer = server.join("writer", &[]).await;
    let mut lock = PushLock::watch(&server);
    let active = hold_receive_pack(&server, &writer).await;
    let mut remover = tokio::net::TcpStream::connect(("127.0.0.1", server.port())).await.unwrap();
    let delete =
        format!("DELETE /api/machines/old HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {ADMIN}\r\n\r\n");
    remover.write_all(delete.as_bytes()).await.unwrap();
    assert_eq!(lock.next().await, Point::ServerPushLockQueued);
    assert!(server.row("old").await.is_some(), "the removal waits for the running push");
    drop(active);
    assert_eq!(lock.next().await, Point::ServerRemovalEntered);
    // The server drops the request, and closes, once it reads EOF.
    remover.shutdown().await.unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(10), remover.read_to_end(&mut Vec::new())).await;
    assert!(closed.is_ok(), "the server kept the removal's request");
    let pending = tokio::spawn(receive_pack(&server, "old", &old).send());
    assert_eq!(lock.next().await, Point::ServerPushLockQueued);
    lock.release();
    assert_eq!(pending.await.unwrap().unwrap().status(), StatusCode::UNAUTHORIZED);
    assert!(server.row("old").await.is_none());
}

#[tokio::test]
async fn a_database_reopens_current_data_and_resets_an_old_schema() {
    let mut server = start().await;
    let token = server.join("writer", &["claude"]).await;
    let updated = server.put_assignment(&token, "writer", json!([skill_toggle("pdf", true)])).await;
    server.restart().await;
    assert_eq!(server.get::<Vec<LogEntry>>("/log?author=writer").await, updated.entries);
    assert_eq!(server.get::<AssignmentSnapshot>("/machines/writer/assignment").await.assignment, updated.assignment);
    let head = server.head();
    server.stop();
    let db = rusqlite::Connection::open(server.data().join("server.db")).unwrap();
    assert_eq!(db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0)).unwrap(), 7);
    db.pragma_update(None, "user_version", 5).unwrap();
    drop(db);
    server.restart().await;
    assert_eq!(server.head(), head);
    assert_eq!(server.get_as(&token, "/machines/writer/assignment").await.status(), StatusCode::UNAUTHORIZED);
    assert!(server.get::<FleetView>("/fleet").await.machines.is_empty());
    assert!(server.get::<Vec<LogEntry>>("/log").await.is_empty());
}

#[tokio::test]
async fn setting_a_harness_updates_defaults_and_explicit_toggles() {
    let server = start().await;
    let me = server.join_scoped("me", &["claude"], Scope::SelfOnly).await;
    server.join("other", &["claude"]).await;
    server.put_assignment(&me, "me", json!([skill_toggle("a", true), skill_toggle("b", false)])).await;
    for (machine, status) in [("other", StatusCode::FORBIDDEN), ("me", StatusCode::OK)] {
        let response = server
            .http()
            .post(server.api(&format!("/machines/{machine}/assignment/harness/claude")))
            .bearer_auth(&me)
            .json(&json!({"enabled":true}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        if status == StatusCode::FORBIDDEN {
            let error = response.json::<sk1ll_proto::ErrorBody>().await.unwrap().error;
            assert!(error.contains("may only manage itself"), "{error}");
            continue;
        }
        let update: AssignmentUpdate = response.json().await.unwrap();
        let claude = update.assignment.harness("claude");
        assert!(claude.new_skills_default && claude.new_rules_default);
        assert_eq!(claude.skills.len(), 2);
        assert!(claude.skills.values().all(|v| *v));
        assert_eq!(update.entries.len(), 3);
        assert!(update.entries.iter().all(|e| e.cause == LogCause::Change && e.author == "me"));
    }
}

#[tokio::test]
async fn pruning_orphans_respects_scope_and_preserves_reported_items() {
    use sk1ll_proto::{OrphansPruned, OrphansView};

    let server = start().await;
    let solo = server.join_scoped("solo", &["claude"], Scope::SelfOnly).await;
    let fleet = server.join("fleet", &["claude"]).await;
    for (token, machine) in [(&solo, "solo"), (&fleet, "fleet")] {
        let added = server
            .put_assignment(token, machine, json!([skill_toggle("gone", true), skill_toggle("drafted", false)]))
            .await;
        assert_eq!(added.entries[0].name.as_deref(), Some("gone"));
        assert!(added.assignment.harness("claude").skills[&id("gone")]);
    }
    let mut ws = ws_connect(&server, &fleet).await;
    let mut report = MachineReport {
        seq: 1,
        items: vec![ReportedItem { item: ItemRef::Skill("drafted".into()), id: id("drafted") }],
        inventory_error: Some("bad Rule ID".into()),
        ..Default::default()
    };
    ws_send(&mut ws, &DaemonMessage::Status(report.clone())).await;
    server.reported("fleet", &report).await;
    let prune = |token: &str, body: Value| {
        server.http().post(server.api("/assignment/orphans/prune")).bearer_auth(token).json(&body).send()
    };
    let before: AssignmentSnapshot = server.get("/machines/fleet/assignment").await;
    let incomplete = prune(&fleet, json!({})).await.unwrap();
    assert_eq!(incomplete.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(incomplete.json::<sk1ll_proto::ErrorBody>().await.unwrap().error.contains("incomplete"));
    assert_eq!(server.get::<AssignmentSnapshot>("/machines/fleet/assignment").await, before);

    report.seq += 1;
    report.inventory_complete = true;
    report.inventory_error = None;
    ws_send(&mut ws, &DaemonMessage::Status(report.clone())).await;
    server.reported("fleet", &report).await;
    let view: OrphansView = server.get_json(&solo, "/assignment/orphans").await;
    assert_eq!(view.orphans.len(), 2);
    assert!(view.orphans.iter().all(|orphan| orphan.item_id == id("gone") && orphan.name.as_deref() == Some("gone")));
    assert_eq!(prune(&solo, json!({"machine":"fleet"})).await.unwrap().status(), StatusCode::FORBIDDEN);
    assert_eq!(prune(&fleet, json!({"machine":"missing"})).await.unwrap().status(), StatusCode::NOT_FOUND);
    for (token, machine) in [(&solo, "solo"), (&fleet, "fleet")] {
        let update: OrphansPruned =
            prune(token, json!({})).await.unwrap().error_for_status().unwrap().json().await.unwrap();
        assert_eq!(update.entries.len(), 1);
        let entry = &update.entries[0];
        assert_eq!((&*entry.machine, &*entry.author, &entry.cause), (machine, machine, &LogCause::OrphanPruned));
        assert_eq!((entry.before, entry.after, entry.name.as_deref()), (Some(true), None, Some("gone")));
        let remaining: AssignmentSnapshot = server.get(&format!("/machines/{machine}/assignment")).await;
        assert_eq!(remaining.assignment.harness("claude").skills.get(&id("drafted")), Some(&false));
    }
    assert!(server.get::<OrphansView>("/assignment/orphans").await.orphans.is_empty());
}

async fn ws_result(ws: &mut Ws, request: u64) -> (u64, sk1ll_proto::AssignmentOutcome) {
    loop {
        if let ServerMessage::AssignmentResult { req, seq, result } = ws_next(ws).await {
            assert_eq!(req, request);
            return (seq, result);
        }
    }
}

#[tokio::test]
async fn a_websocket_write_uses_the_status_sent_before_it() {
    use sk1ll_proto::{AssignmentChanges, AssignmentOutcome, AssignmentWrite, ToggleChange};

    let server = start().await;
    let token = server.join_scoped("ws", &["claude"], Scope::SelfOnly).await;
    let mut ws = ws_connect(&server, &token).await;
    let draft = id("draft");
    let report = MachineReport {
        seq: 7,
        items: vec![ReportedItem { item: ItemRef::Skill("new".into()), id: draft.clone() }],
        inventory_complete: true,
        ..Default::default()
    };
    ws_send(&mut ws, &DaemonMessage::Status(report)).await;
    let write = AssignmentWrite::Changes(AssignmentChanges {
        changes: vec![ToggleChange {
            harness: "claude".into(),
            kind: ToggleKind::NewSkillsDefault,
            item_id: None,
            name: None,
            enabled: Some(true),
        }],
        expected: None,
    });
    ws_send(&mut ws, &DaemonMessage::SetAssignment { req: 9, write }).await;
    let (seq, result) = ws_result(&mut ws, 9).await;
    let AssignmentOutcome::Applied(update) = result else { panic!("{result:?}") };
    assert_eq!(seq, 7);
    let claude = update.assignment.harness("claude");
    assert!(claude.new_skills_default);
    assert_eq!(claude.skills.get(&draft), Some(&false));
    assert!(
        update.entries.iter().any(|e| e.cause == LogCause::DefaultMaterialized && e.name.as_deref() == Some("new"))
    );
}

#[tokio::test]
async fn renaming_a_harness_moves_toggles_and_replay_leaves_the_destination_intact() {
    use sk1ll_proto::{AssignmentOutcome, AssignmentWrite, HarnessRename};

    let server = start().await;
    let token = server.join_scoped("ws", &["claude", "work"], Scope::SelfOnly).await;
    server.put_assignment(&token, "ws", json!([skill_toggle("pdf", true)])).await;
    let route = "/machines/ws/assignment";
    let post = |rename: &HarnessRename| {
        server.http().post(server.api(&format!("{route}/rename-harness"))).bearer_auth(&token).json(rename).send()
    };
    let mut rename =
        HarnessRename { from: "claude".into(), to: "work".into(), operation_id: "move".into(), replace: false };
    assert_eq!(post(&rename).await.unwrap().status(), StatusCode::CONFLICT);
    rename.replace = true;
    let first: AssignmentUpdate = post(&rename).await.unwrap().error_for_status().unwrap().json().await.unwrap();
    assert!(!first.assignment.harnesses.contains_key("claude"));
    assert!(first.assignment.skill_enabled("work", &id("pdf")));
    assert!(!first.assignment.harness("work").new_skills_default);
    assert!(!first.entries.is_empty());
    assert!(
        first.entries.iter().all(|e| e.cause == LogCause::HarnessRenamed { from: "claude".into(), to: "work".into() })
    );
    assert!(
        first.entries.iter().any(|e| e.harness == "claude" && e.name.as_deref() == Some("pdf") && e.after.is_none())
    );
    let replay: AssignmentUpdate = post(&rename).await.unwrap().error_for_status().unwrap().json().await.unwrap();
    assert_eq!((replay.revision, replay.assignment), (first.revision, first.assignment));
    assert!(replay.entries.is_empty());

    let mut ws = ws_connect(&server, &token).await;
    let report =
        MachineReport { seq: 7, harnesses: harnesses(&["claude"]), inventory_complete: true, ..Default::default() };
    ws_send(&mut ws, &DaemonMessage::Status(report)).await;
    ws_send(&mut ws, &DaemonMessage::SetAssignment { req: 9, write: AssignmentWrite::RenameHarness(rename) }).await;
    let (seq, result) = ws_result(&mut ws, 9).await;
    let AssignmentOutcome::Applied(update) = result else { panic!("{result:?}") };
    assert_eq!(seq, 7);
    assert!(update.entries.is_empty());
    assert!(update.assignment.skill_enabled("work", &id("pdf")));
    assert!(!update.assignment.harness("claude").new_rules_default);
}

#[tokio::test]
async fn the_fleet_reports_the_most_severe_current_status() {
    use MachineStatus::*;

    let server = start().await;
    let token = server.join("ws", &[]).await;
    let mut ws = ws_connect(&server, &token).await;
    ws_assignment(&mut ws).await;
    // Heard from, but no report yet: nothing says it has the baseline.
    ws_send(&mut ws, &DaemonMessage::Hello { machine_id: "ws".into() }).await;
    let row = eventually(|| async { server.row("ws").await.filter(|row| row.last_seen.is_some()) }).await;
    assert_eq!((row.report, row.status), (None, Behind));
    let baseline = server.get::<FleetView>("/fleet").await.baseline;
    let mut report = MachineReport { baseline: baseline.clone(), ..Default::default() };
    for want in [InSync, Drafting, Behind, Conflict, Blocked, Behind, Blocked] {
        report.seq += 1;
        match want {
            InSync => {}
            Drafting => report.drafts.push(ItemRef::Skill("pdf".into())),
            Behind if report.conflicts.is_empty() => report.baseline = Some("previous".into()),
            Conflict => report
                .conflicts
                .push(sk1ll_proto::ConflictRef { item: ItemRef::Rule("git".into()), harness: "codex".into() }),
            Blocked if report.behind => report.pull_pending = Some(vec![]),
            Blocked => report.blocked = true,
            Behind => {
                report =
                    MachineReport { seq: report.seq, baseline: baseline.clone(), behind: true, ..Default::default() }
            }
            Offline => unreachable!(),
        }
        ws_send(&mut ws, &DaemonMessage::Status(report.clone())).await;
        server.reported("ws", &report).await;
        assert_eq!(server.row("ws").await.unwrap().status, want);
    }
}

#[tokio::test]
async fn a_baseline_inventory_ignores_local_replacement_refs() {
    let mut server = start().await;
    let token = server.join("writer", &["claude"]).await;
    let without = server.get::<FleetView>("/fleet").await.baseline.unwrap();
    let (_, with) = seed(&server, "writer", &token, &["personal/pdf/SKILL.md"]).await;
    let bare = server.data().join("repo.git");
    git(&bare, &["config", "core.useReplaceRefs", "true"]);
    git(&bare, &["replace", &with, &without]);
    let update = server
        .put_assignment(
            &token,
            "writer",
            json!([
                {"harness":"claude", "kind":"skill","item_id":"pdf","enabled":true}
            ]),
        )
        .await;
    assert_eq!(update.entries[0].name.as_deref(), Some("pdf"));
    let orphans: sk1ll_proto::OrphansView = server.get("/assignment/orphans").await;
    assert!(orphans.orphans.is_empty(), "{orphans:?}");
    assert_eq!(server.get::<FleetView>("/fleet").await.baseline.as_deref(), Some(with.as_str()));

    // A restart turns the repo's own setting off again and still ignores them.
    git(&bare, &["config", "--unset", "core.useReplaceRefs"]);
    server.restart().await;
    assert_eq!(git(&bare, &["config", "core.useReplaceRefs"]), "false");
    assert!(server.get::<sk1ll_proto::OrphansView>("/assignment/orphans").await.orphans.is_empty());
    assert_eq!(server.get::<FleetView>("/fleet").await.baseline.as_deref(), Some(with.as_str()));
}

#[tokio::test]
async fn a_push_rejects_filter_drivers_in_the_new_baseline() {
    let server = start().await;
    let token = server.join("writer", &[]).await;
    let (dir, _) = seed(&server, "writer", &token, &["personal/pdf/SKILL.md"]).await;
    let repo = dir.path().join("repo");
    write(&repo.join(".gitattributes"), "*.md filter\n*.txt -filter\n");
    let valid = commit_push(&server, &repo, "boolean attributes").await;
    write(&repo.join("personal/pdf/.gitattributes"), "*.md filter=lfs\n");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-m", "use a filter driver"]);
    let out = push_output(&repo, "HEAD:main");
    let error = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(error.contains("filter driver") && error.contains("personal/pdf/SKILL.md"), "{error}");
    assert_eq!(git(&server.data().join("repo.git"), &["rev-parse", "HEAD"]), valid);
}

#[tokio::test]
async fn removing_and_rejoining_revokes_requests_that_already_authenticated() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let server = start().await;
    let target = server.join("other", &["claude"]).await;
    let before: AssignmentSnapshot = server.get("/machines/other/assignment").await;
    let change = json!({"changes":[skill_toggle("pdf", true)]}).to_string();
    for (method, path, body) in [
        ("POST", "/join-codes", "{}"),
        ("PUT", "/machines/other/assignment", &change),
        ("POST", "/assignment/orphans/prune", "{}"),
    ] {
        let old = server.join("writer", &["claude"]).await;
        let mut conn = BufReader::new(tokio::net::TcpStream::connect(("127.0.0.1", server.port())).await.unwrap());
        conn.write_all(format!(
            "{method} /api{path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {old}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n",
            body.len()
        ).as_bytes()).await.unwrap();
        let read_line = |line: &str, status| assert!(line.starts_with(&format!("HTTP/1.1 {status} ")), "{line}");
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), conn.read_line(&mut line)).await.unwrap().unwrap();
        read_line(&line, 100);
        line.clear();
        conn.read_line(&mut line).await.unwrap();
        assert_eq!(line, "\r\n");

        assert_eq!(server.remove(ADMIN, "writer").await.status(), StatusCode::NO_CONTENT);
        let fresh = server.join_scoped("writer", &["claude"], Scope::SelfOnly).await;
        assert_ne!(old, fresh);
        conn.write_all(body.as_bytes()).await.unwrap();
        line.clear();
        tokio::time::timeout(Duration::from_secs(5), conn.read_line(&mut line)).await.unwrap().unwrap();
        read_line(&line, 401);
        assert_eq!(server.remove(ADMIN, "writer").await.status(), StatusCode::NO_CONTENT);
    }
    assert_eq!(server.get_json::<AssignmentSnapshot>(&target, "/machines/other/assignment").await, before);
}

#[cfg(unix)]
#[tokio::test]
async fn a_push_retries_its_head_read_and_nudges_when_the_ref_recovers() {
    use std::os::unix::fs::PermissionsExt;

    let server = start().await;
    let token = server.join("writer", &[]).await;
    let initial = server.get::<FleetView>("/fleet").await.baseline.unwrap();
    let mut ws = ws_connect(&server, &token).await;
    ws_assignment(&mut ws).await;
    assert_eq!(ws_next(&mut ws).await, ServerMessage::Nudge { baseline: initial.clone() });
    let dir = clone(&server, "writer", &token).await;
    let repo = dir.path().join("repo");
    write(&repo.join("README.md"), "next\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "next"]);
    let next = git(&repo, &["rev-parse", "HEAD"]);
    let tree = git(&repo, &["rev-parse", "HEAD^{tree}"]);
    let bare = server.data().join("repo.git");
    let hook = bare.join("hooks/post-receive");
    write(&hook, &format!("#!/bin/sh\nprintf '%s\\n' '{tree}' > refs/heads/main\n"));
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = push_output(&repo, "HEAD:main");
    assert!(out.status.success(), "{}", sk1ll_testkit::text(&out));

    // Pruning must take the push lock. Its error confirms the push's first read
    // finished with the invalid ref still in place.
    let prune = server.http().post(server.api("/assignment/orphans/prune")).bearer_auth(&token).send().await.unwrap();
    assert_eq!(prune.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(server.get::<FleetView>("/fleet").await.baseline.as_deref(), Some(initial.as_str()));
    write(&bare.join("refs/heads/main"), &format!("{next}\n"));
    assert_eq!(ws_next(&mut ws).await, ServerMessage::Nudge { baseline: next.clone() });
    assert_eq!(server.get::<FleetView>("/fleet").await.baseline, Some(next));
}

#[tokio::test]
async fn a_blocked_database_write_leaves_health_requests_responsive() {
    let server = on_test_runtime().await;
    let db = rusqlite::Connection::open(server.data().join("server.db")).unwrap();
    let tx = db.unchecked_transaction().unwrap();
    tx.execute("UPDATE counters SET value = value", []).unwrap();
    let mut waiting = Box::pin(server.create_code(ADMIN, None));
    tokio::select! {
        response = &mut waiting => panic!("join code write finished while SQLite was locked: {response:?}"),
        () = tokio::time::sleep(Duration::from_millis(100)) => {}
    }
    let health = server.http().get(format!("{}/healthz", server.url())).timeout(Duration::from_secs(1));
    assert_eq!(health.send().await.unwrap().status(), StatusCode::OK);
    tx.commit().unwrap();
    assert_eq!(waiting.await.status(), StatusCode::CREATED);
}

#[cfg(unix)]
#[tokio::test]
async fn a_failed_or_cancelled_baseline_read_stops_its_child_processes() {
    use sk1ll_testkit::child;
    use std::os::unix::fs::PermissionsExt;

    const TEST: &str = "a_failed_or_cancelled_baseline_read_stops_its_child_processes";
    if let Some(dir) = child::dir() {
        let fixture = std::env::var("BASELINE_READ_CASE").unwrap();
        let server = on_test_runtime().await;
        let token = server.join("writer", &[]).await;
        let marker = dir.join("mode");
        let pid_file = dir.join("child-pid");
        write(&marker, &fixture);
        let mut get = Box::pin(server.get_as(&token, "/assignment/orphans"));
        let ready = async {
            while !pid_file.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::select! {
            response = &mut get => panic!("baseline read completed before the subprocess started: {response:?}"),
            result = tokio::time::timeout(Duration::from_secs(10), ready) => result.unwrap(),
        }
        let pid = sk1ll_testkit::read(&pid_file).trim().parse::<libc::pid_t>().unwrap();
        let running = || {
            // Only check the fixture process. No signal is sent.
            unsafe { libc::kill(pid, 0) == 0 }
        };
        if fixture == "timeout" {
            assert!(running());
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(60)).await;
            tokio::time::resume();
        }
        if fixture == "cancel" {
            assert!(running());
            drop(get);
        } else {
            let response = tokio::time::timeout(Duration::from_secs(10), get).await.unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            while running() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the baseline reader's child survived");
        std::fs::remove_file(marker).unwrap();
        assert!(server.get::<sk1ll_proto::OrphansView>("/assignment/orphans").await.orphans.is_empty());
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let wrapper = dir.path().join("git");
    write(
        &wrapper,
        r#"#!/bin/sh
dir=${0%/*}
if test "$3" = rev-parse && test "$5" = --quiet && test -f "$dir/mode"; then
    sh -c 'trap "" TERM; printf "%s\n" "$$" > "$1.tmp"; mv "$1.tmp" "$1"; exec sleep 90' child "$dir/child-pid" &
    while ! test -f "$dir/child-pid"; do sleep 0.01; done
    if test "$(cat "$dir/mode")" = output-cap; then exec yes; fi
    wait
else
    PATH=${PATH#*:}
    export PATH
    exec git "$@"
fi
"#,
    );
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::iter::once(dir.path().to_path_buf())
        .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap()))
        .collect::<Vec<_>>();
    for case in ["timeout", "output-cap", "cancel"] {
        let _ = std::fs::remove_file(dir.path().join("child-pid"));
        let out = child::command(TEST, dir.path())
            .env("PATH", std::env::join_paths(&path).unwrap())
            .env("BASELINE_READ_CASE", case)
            .output()
            .unwrap();
        assert!(out.status.success(), "{case}: {}", sk1ll_testkit::text(&out));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn allowlisted_imports_ignore_inherited_git_rewrites_and_ssh_configuration() {
    use sk1ll_testkit::child;
    use std::os::unix::fs::PermissionsExt;

    const TEST: &str = "allowlisted_imports_ignore_inherited_git_rewrites_and_ssh_configuration";
    if let Some(dir) = child::dir() {
        let ssh = dir.join("ssh");
        let log = dir.join("ssh.log");
        for isolated in [false, true] {
            let ssh = ssh.clone();
            let server = start_with(move |config| {
                if isolated {
                    config.import_hosts = Some(vec!["approved.example".into()]);
                    config.import_ssh = ssh.to_str().unwrap().to_owned();
                }
            })
            .await;
            let token = server.join("writer", &[]).await;
            let checkout = clone(&server, "writer", &token).await;
            let repo = checkout.path().join("repo");
            write_imports(&repo, &[("allowed", "ssh://git@approved.example/o/r", "main")], &"a".repeat(40));
            commit_push(&server, &repo, "add import").await;
            eventually(|| async {
                let imports: ImportsView = server.get("/imports").await;
                (!imports.errors.is_empty()).then_some(())
            })
            .await;
            let log_text = sk1ll_testkit::read(&log);
            std::fs::remove_file(&log).unwrap();
            if isolated {
                assert!(log_text.contains("git@approved.example") && !log_text.contains("other.example"), "{log_text}");
                assert!(log_text.starts_with("-F /dev/null -o BatchMode=yes "), "{log_text}");
            } else {
                assert!(log_text.contains("git@other.example"), "{log_text}");
            }
        }
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let ssh = dir.path().join("ssh");
    write(&ssh, "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"${0%/*}/ssh.log\"\nexit 1\n");
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let rewrite = dir.path().join("gitconfig");
    write(&rewrite, "[url \"ssh://git@other.example/\"]\n\tinsteadOf = ssh://git@approved.example/\n");
    for via_environment in [false, true] {
        let mut cmd = child::command(TEST, dir.path());
        cmd.env("GIT_SSH_COMMAND", format!("'{}'", ssh.display()))
            .env("GIT_SSH_VARIANT", "ssh")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        if via_environment {
            cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "url.ssh://git@other.example/.insteadOf")
                .env("GIT_CONFIG_VALUE_0", "ssh://git@approved.example/");
        } else {
            cmd.env("GIT_CONFIG_GLOBAL", &rewrite).env_remove("GIT_CONFIG_COUNT");
        }
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{}", sk1ll_testkit::text(&out));
    }
}
