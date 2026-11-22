#![cfg(unix)]

use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde_json::json;
use sk1ll_daemon::client::{API_HEADER, API_VERSION};
use tokio::net::UnixListener;
use tokio::runtime::Runtime;

#[derive(Debug, PartialEq, Eq)]
struct RecordedRequest {
    method: String,
    path: String,
    query: Vec<String>,
    body: Vec<u8>,
}

type Requests = Arc<Mutex<Vec<RecordedRequest>>>;
type FakeState = (Requests, sk1ll_daemon::client::DaemonMode);

struct FakeDaemon {
    _runtime: Runtime,
    directory: tempfile::TempDir,
    requests: Requests,
}

impl FakeDaemon {
    fn start() -> Self {
        Self::start_in_mode(sk1ll_daemon::client::DaemonMode::Joined)
    }

    fn start_in_mode(mode: sk1ll_daemon::client::DaemonMode) -> Self {
        let directory = tempfile::Builder::new().prefix("sk-").tempdir_in("/var/tmp").unwrap();
        let requests = Requests::default();
        let router = Router::new().fallback(respond).with_state((requests.clone(), mode));
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let listener = {
            let _guard = runtime.enter();
            UnixListener::bind(directory.path().join("d.sock")).unwrap()
        };
        runtime.spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { _runtime: runtime, directory, requests }
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_cli(&[&["fleet"], args].concat())
    }

    fn run_cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_sk1ll"))
            .arg("--daemon-socket")
            .arg(self.directory.path().join("d.sock"))
            .arg("--daemon-state")
            .arg(self.directory.path().join("state.json"))
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn assert_requests(&self, expected: &[(&str, &str, &[&str], &str)]) {
        let expected: Vec<_> = expected
            .iter()
            .map(|(method, path, query, body)| RecordedRequest {
                method: (*method).into(),
                path: (*path).into(),
                query: query.iter().map(|part| (*part).into()).collect(),
                body: body.as_bytes().to_vec(),
            })
            .collect();
        assert_eq!(*self.requests.lock().unwrap(), expected);
    }
}

async fn respond(State((requests, mode)): State<FakeState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, 1024).await.unwrap();
    let mut query: Vec<_> =
        parts.uri.query().into_iter().flat_map(|query| query.split('&')).map(String::from).collect();
    query.sort();
    requests.lock().unwrap().push(RecordedRequest {
        method: parts.method.to_string(),
        path: parts.uri.path().into(),
        query,
        body: body.to_vec(),
    });

    // The daemon checks the request's API version. The client needs no response version header.
    if parts.headers.get(API_HEADER).and_then(|value| value.to_str().ok()) != Some(&API_VERSION.to_string()) {
        return StatusCode::UPGRADE_REQUIRED.into_response();
    }

    let reply = match (parts.method.as_str(), parts.uri.path()) {
        ("GET", "/harnesses") => json!([{"name": "claude", "harness": {}, "origin": "local"}]),
        ("POST", "/assignment/harness/claude") if body.as_ref() == br#"{"enabled":true}"# => json!({
            "assignment": {"harnesses": {}}, "revision": 9, "entries": [],
            "errors": ["could not place pdf", "could not place style"]
        }),
        ("POST", "/assignment/harness/claude") => json!({
            "assignment": {"harnesses": {}}, "revision": 8, "entries": []
        }),
        ("GET", "/fleet") => json!({
            "baseline": "0123456789abcdef",
            "machines": [{"id": "box", "status": "offline", "connected": false, "joined_at": 1700000000, "scope": "fleet", "registration": "box-reg"}]
        }),
        ("GET", "/status") => json!({
            "machine_id": "box", "server_url": "https://fleet.example", "mode": mode,
            "has_origin": true, "repo_ready": true, "repo": "/repo", "headline": "offline",
            "connection": "offline", "revoked": false, "assignment_revision": 7, "report": {}
        }),
        ("POST", "/join-codes") => {
            let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
            json!({"code": "ONCE", "expires_at": 1700086400, "scope": request["scope"]})
        }
        ("GET", "/assignment/orphans") => json!({"orphans": [{
            "machine": "box", "harness": "claude", "kind": "skill", "item_id": "stale-id", "name": "old"
        }]}),
        ("POST", "/assignment/orphans/prune") => json!({"entries": []}),
        ("GET", "/log") => json!([{
            "id": 41, "at": 1700000000, "machine": "box", "harness": "claude", "author": "admin",
            "kind": "skill", "name": "pdf", "before": false, "after": true, "cause": {"type": "change"}
        }]),
        ("POST", "/log/42/undo") => json!({"assignment": {"harnesses": {}}, "revision": 7, "entries": []}),
        ("DELETE", "/machines/box") => return StatusCode::NO_CONTENT.into_response(),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    Json(reply).into_response()
}

fn stdout(out: Output) -> String {
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn fleet_lists_machines_and_baseline() {
    let daemon = FakeDaemon::start();
    let output = stdout(daemon.run(&[]));
    let mut lines = output.lines();
    assert_eq!(lines.next(), Some("baseline 0123456789"));
    assert_eq!(lines.next().unwrap().split_whitespace().collect::<Vec<_>>(), ["box", "offline", "seen", "never"]);
    assert_eq!(lines.next(), None);
    daemon.assert_requests(&[("GET", "/status", &[], ""), ("GET", "/fleet", &[], "")]);
}

#[test]
fn fleet_invite_prints_code_and_join_command() {
    let daemon = FakeDaemon::start();
    assert_eq!(
        stdout(daemon.run(&["invite"])),
        "ONCE\nsk1ll join https://fleet.example ONCE\nexpires 2023-11-15 22:13:20 UTC\n\
         scope fleet: the Machine may manage the whole Fleet\n"
    );
    daemon.assert_requests(&[("GET", "/status", &[], ""), ("POST", "/join-codes", &[], r#"{"scope":"fleet"}"#)]);
}

#[test]
fn fleet_invite_can_limit_the_scope() {
    let daemon = FakeDaemon::start();
    let out = stdout(daemon.run(&["invite", "--scope", "self"]));
    assert!(out.ends_with("scope self: the Machine may manage only itself\n"), "{out}");
    daemon.assert_requests(&[("GET", "/status", &[], ""), ("POST", "/join-codes", &[], r#"{"scope":"self"}"#)]);
    assert!(!daemon.run(&["invite", "--scope", "admin"]).status.success());
}

#[test]
fn fleet_log_forwards_filters_and_prints_entries() {
    let daemon = FakeDaemon::start();
    let output = stdout(daemon.run(&[
        "log",
        "--machine",
        "box",
        "--harness",
        "claude",
        "--author",
        "admin",
        "--item",
        "skill:pdf",
        "--limit",
        "10",
        "--before",
        "42",
    ]));
    assert_eq!(output, "41  2023-11-14 22:13:20 UTC  box / claude  skill:pdf  off -> on  by admin\n");
    daemon.assert_requests(&[
        ("GET", "/status", &[], ""),
        (
            "GET",
            "/log",
            &["author=admin", "before=42", "harness=claude", "item=skill%3Apdf", "limit=10", "machine=box"],
            "",
        ),
    ]);
}

#[test]
fn fleet_undo_targets_entry_and_reports_no_change() {
    let daemon = FakeDaemon::start();
    assert_eq!(
        stdout(daemon.run(&["undo", "42"])),
        "Assignment log entry 42's previous value is already set; nothing changed\n"
    );
    daemon.assert_requests(&[("GET", "/status", &[], ""), ("POST", "/log/42/undo", &[], "null")]);
}

#[test]
fn fleet_remove_with_yes_deletes_machine() {
    let daemon = FakeDaemon::start();
    assert_eq!(stdout(daemon.run(&["remove", "box", "--yes"])), "removed box from the fleet\n");
    daemon.assert_requests(&[("GET", "/status", &[], ""), ("DELETE", "/machines/box", &[], "")]);
}

#[test]
fn fleet_remove_without_yes_or_terminal_sends_no_request() {
    let daemon = FakeDaemon::start();
    let out = daemon.run(&["remove", "box"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("requires terminal confirmation or `--yes`"), "{stderr}");
    daemon.assert_requests(&[]);
}

/// Without a daemon, the engine in this process asks the server; a
/// standalone Machine has none to ask.
#[test]
fn removal_requires_yes_without_a_terminal() {
    let dir = tempfile::tempdir().unwrap();
    for (flag, expected) in [(None, "requires terminal confirmation or `--yes`"), (Some("--yes"), "join a Fleet")] {
        let out = Command::new(env!("CARGO_BIN_EXE_sk1ll"))
            .arg("--daemon-socket")
            .arg(dir.path().join("absent.sock"))
            .arg("--daemon-state")
            .arg(dir.path().join("absent.json"))
            .arg("--repo")
            .arg(dir.path().join("repo"))
            .env("HOME", dir.path())
            .env_remove("SK1LL_STATE_DIR")
            .env("XDG_CONFIG_HOME", dir.path().join("config"))
            .env("XDG_CACHE_HOME", dir.path().join("cache"))
            .env("XDG_STATE_HOME", dir.path().join("state"))
            .args(["fleet", "remove", "box"])
            .args(flag)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!out.status.success());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(expected), "{stderr}");
    }
}

#[test]
fn bash_completions_include_fleet_commands() {
    let out = Command::new(env!("CARGO_BIN_EXE_sk1ll")).args(["completions", "bash"]).output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("prune-orphans"));
    for name in ["invite", "remove", "orphans", "log", "undo"] {
        assert!(stdout.contains(&format!("__fleet__subcmd__{name}")), "{name} missing from Bash completions");
    }
}

#[test]
fn assign_all_uses_the_harness_endpoint() {
    let daemon = FakeDaemon::start();
    let out = daemon.run_cli(&["--repo", "/repo", "assign", "claude", "--all", "off"]);
    stdout(out);
    daemon.assert_requests(&[
        ("GET", "/status", &[], ""),
        ("GET", "/harnesses", &[], ""),
        ("POST", "/assignment/harness/claude", &[], r#"{"enabled":false}"#),
    ]);
}

#[test]
fn assign_all_reports_saved_revision_and_placement_failures() {
    use sk1ll_daemon::client::DaemonMode;

    for mode in [DaemonMode::Joined, DaemonMode::Standalone] {
        let daemon = FakeDaemon::start_in_mode(mode);
        let out = daemon.run_cli(&["--repo", "/repo", "assign", "claude", "--all", "on"]);
        assert!(!out.status.success());
        assert!(String::from_utf8(out.stdout).unwrap().contains("Assignment of box was saved at revision 9"));
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(stderr.contains("could not place pdf") && stderr.contains("could not place style"));
    }
}

#[test]
fn fleet_lists_and_prunes_orphans_with_an_optional_machine() {
    let daemon = FakeDaemon::start();
    let listed = stdout(daemon.run(&["orphans"]));
    assert!(listed.contains("stale-id") && listed.contains("old"), "{listed}");
    assert!(stdout(daemon.run(&["prune-orphans", "--machine", "box"])).contains("cleared 0 orphan toggle"));
    daemon.assert_requests(&[
        ("GET", "/status", &[], ""),
        ("GET", "/assignment/orphans", &[], ""),
        ("GET", "/status", &[], ""),
        ("POST", "/assignment/orphans/prune", &[], r#"{"machine":"box"}"#),
    ]);
}
