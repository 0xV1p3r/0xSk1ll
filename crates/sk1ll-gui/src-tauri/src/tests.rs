use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde_json::{Value, json};
use sk1ll_daemon::client::*;
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::commands::Bridge;

struct Server {
    _directory: tempfile::TempDir,
    client: Client,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    async fn start(router: Router) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { _directory: directory, client: Client::new(socket), task }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

type Recorded = Arc<Mutex<Vec<(String, String, Value)>>>;
type Replies = BTreeMap<String, (StatusCode, Value)>;

async fn respond(State((requests, replies)): State<(Recorded, Replies)>, request: Request) -> Response {
    let method = request.method().to_string();
    let uri = request.uri().to_string();
    let key = format!("{method} {uri}");
    let bytes = to_bytes(request.into_body(), 1024 * 1024).await.unwrap();
    let body = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() };
    requests.lock().unwrap().push((method, uri, body));
    let (status, value) = replies.get(&key).unwrap_or_else(|| panic!("unexpected request: {key}"));
    Response::builder().status(*status).header(header::CONTENT_TYPE, "application/json")
        .body(if *status == StatusCode::NO_CONTENT { Body::empty() } else { Body::from(value.to_string()) }).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commands_use_the_unix_client_and_preserve_payloads_and_errors() {
    let requests = Recorded::default();
    let fleet = json!({"baseline":"abc", "machines":[]});
    let items = json!([{"item":"skill:pdf", "in_baseline":true, "draft":null, "unpushed":false,
        "description":"Read PDF files"}]);
    let assignment = json!({"revision":7, "harnesses": {"claude": {"new_skills_default":false,
        "new_fragments_default":true, "skills":{"pdf":true}, "fragments":{}}}});
    let promoted = json!({"sha":"abc", "items":["skill:pdf"], "message":"Ship PDF", "pushed":false,
        "push_error":"Hub offline"});
    let mut replies = Replies::new();
    for (path, value) in [
        ("GET /fleet", fleet.clone()), ("GET /items", items.clone()),
        ("GET /machines/alpha/assignment", assignment.clone()),
        ("GET /draft/diff?item=skill%3Apdf", json!({"item":"skill:pdf", "diff":"+new\n"})),
        ("GET /promote/message?items=skill%3Apdf%2Cfragment%3Agit",
            json!({"items":["skill:pdf","fragment:git"],"message":"prefilled"})),
        ("POST /promote", promoted.clone()), ("POST /reset", json!({"discarded":["skill:pdf"]})),
        ("POST /pull", json!({"state":"blocked","baseline":"abc","items":["skill:pdf"]})),
        ("GET /log?before=42&limit=10", json!([])),
        ("POST /join-codes", json!({"code":"ONCE"})),
        ("POST /join", json!({"machine_id":"alpha","repo":"/tmp/skills","setup":"adopted","harnesses":[],"revision":7})),
    ] {
        replies.insert(path.into(), (StatusCode::OK, value));
    }
    for path in ["POST /conflicts/adopt", "POST /conflicts/overwrite",
        "POST /conflicts/displaced/a%2Fb/adopt", "POST /conflicts/displaced/a%2Fb/discard", "DELETE /machines/a%2Fb"] {
        replies.insert(path.into(), (StatusCode::NO_CONTENT, Value::Null));
    }
    replies.insert("PUT /machines/alpha/assignment".into(),
        (StatusCode::UNPROCESSABLE_ENTITY, json!({"error":"skill:local is not in the Baseline"})));
    let server = Server::start(Router::new().fallback(respond).with_state((requests.clone(), replies))).await;
    let bridge = Bridge::new(server.client.clone());
    assert_eq!(serde_json::to_value(bridge.fleet().await.unwrap()).unwrap(), fleet);
    assert_eq!(serde_json::to_value(bridge.items().await.unwrap()).unwrap(), items);
    assert_eq!(serde_json::to_value(bridge.assignment("alpha".into()).await.unwrap()).unwrap(), assignment);
    assert_eq!(bridge.diff("skill:pdf".into()).await.unwrap().diff, "+new\n");
    assert_eq!(bridge.promote_message(vec!["skill:pdf".into(), "fragment:git".into()]).await.unwrap().message,
        "prefilled");
    let request = PromoteRequest { items: vec!["skill:pdf".into()], message: Some("Ship PDF".into()), author: None };
    assert_eq!(serde_json::to_value(bridge.promote(request).await.unwrap()).unwrap(), promoted);
    let reset = bridge.reset(ResetRequest { items: vec!["skill:pdf".into()], all: false }).await.unwrap();
    assert_eq!(reset.discarded.len(), 1);
    assert!(matches!(bridge.pull().await.unwrap(), PullResult::Blocked { .. }));
    assert!(bridge.log(LogQuery { before: Some(42), limit: Some(10), ..Default::default() }).await.unwrap().is_empty());
    assert_eq!(bridge.join_code().await.unwrap().code, "ONCE");
    let joined = bridge.join(JoinApiRequest { hub_url: "http://hub".into(), code: "CODE".into(),
        machine_id: Some("alpha".into()), copy_from: Some("beta".into()), repo: None }).await.unwrap();
    assert_eq!(joined.revision, 7);
    bridge.adopt(ConflictTarget { harness: "claude".into(), name: "pdf".into() }).await.unwrap();
    bridge.overwrite(ConflictTarget { harness: "claude".into(), name: "pdf".into() }).await.unwrap();
    bridge.adopt_displaced("a/b".into()).await.unwrap();
    bridge.discard_displaced("a/b".into()).await.unwrap();
    bridge.remove_machine("a/b".into()).await.unwrap();
    let changes: AssignmentChanges = serde_json::from_value(json!({"changes":[{
        "harness":"claude","kind":"skill","name":"local","enabled":true}]})).unwrap();
    let error = bridge.set_assignment("alpha".into(), changes).await.unwrap_err();
    assert_eq!(error.status, Some(422));
    assert_eq!(error.message, "skill:local is not in the Baseline");
    assert!(!error.unreachable);
    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.iter().filter(|(method, _, _)| method == "PUT").count(), 1, "no mutation retry");
        let body = &requests.iter().find(|(_, uri, _)| uri == "/promote").unwrap().2;
        assert_eq!(body, &json!({"items":["skill:pdf"],"message":"Ship PDF","author":null}));
        let body = &requests.iter().find(|(_, uri, _)| uri == "/join").unwrap().2;
        assert_eq!(body["copy_from"], "beta");
    }

    // Exercise the actual Tauri IPC handler as well as the forwarding method.
    let app = tauri::test::mock_builder().manage(bridge)
        .invoke_handler(tauri::generate_handler![crate::commands::fleet])
        .build(tauri::test::mock_context(tauri::test::noop_assets())).unwrap();
    let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default()).build().unwrap();
    let response = tauri::test::get_ipc_response(&webview, tauri::webview::InvokeRequest {
        cmd: "fleet".into(), callback: tauri::ipc::CallbackFn(0), error: tauri::ipc::CallbackFn(1),
        url: "tauri://localhost".parse().unwrap(), body: Default::default(), headers: Default::default(),
        invoke_key: tauri::test::INVOKE_KEY.to_string(),
    }).unwrap().deserialize::<Value>().unwrap();
    assert_eq!(response, fleet);
}

#[tokio::test]
async fn transport_failure_is_structured() {
    let directory = tempfile::tempdir().unwrap();
    let bridge = Bridge::new(Client::new(directory.path().join("missing.sock")));
    let error = bridge.status().await.unwrap_err();
    assert!(error.unreachable);
    assert_eq!(error.status, None);
    assert!(error.message.contains("cannot reach the daemon"));
}

#[tokio::test]
async fn event_task_reconnects_after_failure_and_eof_and_forwards_unchanged_events() {
    async fn events(State(attempts): State<Arc<AtomicUsize>>) -> Response {
        let attempt = attempts.fetch_add(1, Ordering::SeqCst);
        if attempt != 1 {
            return Response::builder().status(503).body(Body::empty()).unwrap();
        }
        let stream = futures_util::stream::iter([Ok::<_, Infallible>(
            concat!("event: assignment\ndata: {\"type\":\"assignment\",\"revision\":9}\n\n",
                "event: draft\ndata: {\"type\":\"draft\"}\n\n")
        )]);
        Response::builder().header(header::CONTENT_TYPE, "text/event-stream").body(Body::from_stream(stream)).unwrap()
    }
    let attempts = Arc::new(AtomicUsize::new(0));
    let server = Server::start(Router::new().route("/events", get(events)).with_state(attempts.clone())).await;
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let task = tokio::spawn(crate::events::forward(server.client.clone(), move |name, value| {
        sender.send((name.to_string(), value)).unwrap();
    }));
    let mut seen = Vec::new();
    timeout(Duration::from_secs(8), async {
        while seen.len() < 7 { seen.push(receiver.recv().await.unwrap()); }
    }).await.unwrap();
    task.abort();
    assert_eq!(seen[0].0, "daemon-connection");
    assert_eq!(seen[0].1["reachable"], false);
    assert_eq!(seen[0].1["retry_in_ms"], 500);
    assert_eq!(seen[1].1["reachable"], true);
    assert_eq!(seen[2], ("assignment".into(), json!({"type":"assignment","revision":9})));
    assert_eq!(seen[3], ("draft".into(), json!({"type":"draft"})));
    assert_eq!(seen[4].1["retry_in_ms"], 500, "a valid event resets backoff");
    assert_eq!(seen[5].1["retry_in_ms"], 1000);
    assert_eq!(seen[6].1["retry_in_ms"], 2000);
    assert_eq!(attempts.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn failed_subscriptions_back_off_without_making_a_connected_claim() {
    let directory = tempfile::tempdir().unwrap();
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let task = tokio::spawn(crate::events::forward(Client::new(directory.path().join("missing.sock")), move |_, value| {
        sender.send(value).unwrap();
    }));
    for delay in [500, 1000, 2000] {
        let value = timeout(Duration::from_secs(4), receiver.recv()).await.unwrap().unwrap();
        assert_eq!(value["reachable"], false);
        assert_eq!(value["retry_in_ms"], delay);
    }
    task.abort();
}

#[test]
fn draft_review_fingerprint_tracks_binary_edits_deletions_and_symlinks() {
    use crate::commands::draft_fingerprint;
    let directory = tempfile::tempdir().unwrap();
    let repo = directory.path();
    let paths = vec!["image.bin".into(), "link".into()];
    std::fs::write(repo.join("image.bin"), [0, 1, 2]).unwrap();
    std::os::unix::fs::symlink("image.bin", repo.join("link")).unwrap();
    let first = draft_fingerprint(repo, &paths).unwrap();
    assert_eq!(first, draft_fingerprint(repo, &["link".into(), "image.bin".into()]).unwrap());
    std::fs::write(repo.join("image.bin"), [0, 3, 2]).unwrap();
    let edited = draft_fingerprint(repo, &paths).unwrap();
    assert_ne!(first, edited, "same-length binary edits must invalidate review");
    std::fs::remove_file(repo.join("link")).unwrap();
    std::os::unix::fs::symlink("elsewhere", repo.join("link")).unwrap();
    let linked = draft_fingerprint(repo, &paths).unwrap();
    assert_ne!(edited, linked, "retargeted and dangling symlinks must invalidate review");
    std::fs::remove_file(repo.join("image.bin")).unwrap();
    assert_ne!(linked, draft_fingerprint(repo, &paths).unwrap());
}

#[tokio::test]
async fn draft_bridge_adds_fingerprints_and_preserves_daemon_entries() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("draft.bin"), [0, 1, 2]).unwrap();
    let mut replies = Replies::new();
    replies.insert("GET /status".into(), (StatusCode::OK, json!({
        "machine_id":"alpha", "hub_url":"http://hub", "repo":directory.path(),
        "headline":"drafting", "connection":"connected", "last_error":null,
        "revoked":false, "hub_baseline":null, "assignment_revision":1, "report":{},
    })));
    replies.insert("GET /draft".into(), (StatusCode::OK, json!([
        {"item":"skill:binary", "change":"added", "paths":["draft.bin"]}
    ])));
    let requests = Recorded::default();
    let server = Server::start(Router::new().fallback(respond).with_state((requests.clone(), replies))).await;
    let bridge = Bridge::new(server.client.clone());
    let first = bridge.draft().await.unwrap();
    let value = serde_json::to_value(&first).unwrap();
    assert_eq!(value[0]["item"], "skill:binary");
    assert_eq!(value[0]["change"], "added");
    assert_eq!(value[0]["paths"], json!(["draft.bin"]));
    assert_eq!(value[0]["fingerprint"].as_str().unwrap().len(), 64);
    std::fs::write(directory.path().join("draft.bin"), [0, 9, 2]).unwrap();
    let second = bridge.draft().await.unwrap();
    assert_ne!(first[0].fingerprint, second[0].fingerprint);
    assert!(requests.lock().unwrap().iter().all(|(method, _, _)| method == "GET"));
}
