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

struct FakeDaemon {
    _runtime: Runtime,
    directory: tempfile::TempDir,
    requests: Requests,
}

impl FakeDaemon {
    fn start() -> Self {
        let directory = tempfile::Builder::new().prefix("sk-").tempdir_in("/var/tmp").unwrap();
        let requests = Requests::default();
        let router = Router::new().fallback(respond).with_state(requests.clone());
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let listener = {
            let _guard = runtime.enter();
            UnixListener::bind(directory.path().join("d.sock")).unwrap()
        };
        runtime.spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { _runtime: runtime, directory, requests }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_sk1ll"))
            .arg("--daemon-socket")
            .arg(self.directory.path().join("d.sock"))
            .arg("--daemon-state")
            .arg(self.directory.path().join("state.json"))
            .arg("fleet")
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

async fn respond(State(requests): State<Requests>, request: Request) -> Response {
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
        ("GET", "/fleet") => json!({
            "baseline": "0123456789abcdef",
            "machines": [{"id": "box", "status": "offline", "connected": false, "joined_at": 1700000000}]
        }),
        ("GET", "/status") => json!({
            "machine_id": "box", "server_url": "https://fleet.example", "mode": "joined",
            "has_origin": true, "repo_ready": true, "repo": "/repo", "headline": "offline",
            "connection": "offline", "revoked": false, "assignment_revision": 7, "report": {}
        }),
        ("POST", "/join-codes") => json!({"code": "ONCE"}),
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
    daemon.assert_requests(&[("GET", "/fleet", &[], "")]);
}

#[test]
fn fleet_invite_prints_code_and_join_command() {
    let daemon = FakeDaemon::start();
    assert_eq!(stdout(daemon.run(&["invite"])), "ONCE\nsk1ll join https://fleet.example ONCE\n");
    daemon.assert_requests(&[("GET", "/status", &[], ""), ("POST", "/join-codes", &[], "null")]);
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
    assert_eq!(output, "41  1700000000  box / claude  skill:pdf  off -> on  by admin (Change)\n");
    daemon.assert_requests(&[(
        "GET",
        "/log",
        &["author=admin", "before=42", "harness=claude", "item=skill%3Apdf", "limit=10", "machine=box"],
        "",
    )]);
}

#[test]
fn fleet_undo_targets_entry_and_reports_no_change() {
    let daemon = FakeDaemon::start();
    assert_eq!(
        stdout(daemon.run(&["undo", "42"])),
        "Assignment log entry 42's previous value is already set; nothing changed\n"
    );
    daemon.assert_requests(&[("POST", "/log/42/undo", &[], "null")]);
}

#[test]
fn fleet_remove_with_yes_deletes_machine() {
    let daemon = FakeDaemon::start();
    assert_eq!(stdout(daemon.run(&["remove", "box", "--yes"])), "removed box from the fleet\n");
    daemon.assert_requests(&[("DELETE", "/machines/box", &[], "")]);
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

#[test]
fn removal_requires_yes_without_a_terminal() {
    let dir = tempfile::tempdir().unwrap();
    for (flag, expected) in
        [(None, "requires terminal confirmation or `--yes`"), (Some("--yes"), "the sk1ll daemon isn't running")]
    {
        let out = Command::new(env!("CARGO_BIN_EXE_sk1ll"))
            .arg("--daemon-socket")
            .arg(dir.path().join("absent.sock"))
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
    for name in ["invite", "remove", "log", "undo"] {
        assert!(stdout.contains(&format!("__fleet__subcmd__{name}")), "{name} missing from Bash completions");
    }
}
