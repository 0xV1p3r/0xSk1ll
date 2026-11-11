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

struct FakeDaemon {
    _directory: tempfile::TempDir,
    client: Client,
    task: tokio::task::JoinHandle<()>,
}

impl FakeDaemon {
    async fn start(router: Router) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { _directory: directory, client: Client::new(socket), task }
    }
}

impl Drop for FakeDaemon {
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
        "new_rules_default":true, "skills":{"pdf":true}, "rules":{}}}});
    let committed = json!({"sha":"abc", "items":["skill:pdf"], "message":"Ship PDF", "pushed":false,
        "push_error":"Server offline"});
    let mut replies = Replies::new();
    for (path, value) in [
        ("GET /fleet", fleet.clone()), ("GET /items", items.clone()),
        ("GET /machines/alpha/assignment", assignment.clone()),
        ("GET /draft/diff?item=skill%3Apdf", json!({"item":"skill:pdf", "diff":"+new\n"})),
        ("GET /commit/message?items=skill%3Apdf%2Crule%3Agit",
            json!({"items":["skill:pdf","rule:git"],"message":"prefilled"})),
        ("POST /commit", committed.clone()), ("POST /restore", json!({"discarded":["skill:pdf"]})),
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
    let daemon = FakeDaemon::start(Router::new().fallback(respond).with_state((requests.clone(), replies))).await;
    let bridge = Bridge::new(daemon.client.clone());
    assert_eq!(serde_json::to_value(bridge.fleet().await.unwrap()).unwrap(), fleet);
    assert_eq!(serde_json::to_value(bridge.items().await.unwrap()).unwrap(), items);
    assert_eq!(serde_json::to_value(bridge.assignment("alpha".into()).await.unwrap()).unwrap(), assignment);
    assert_eq!(bridge.diff("skill:pdf".into()).await.unwrap().diff, "+new\n");
    assert_eq!(bridge.commit_message(vec!["skill:pdf".into(), "rule:git".into()]).await.unwrap().message,
        "prefilled");
    let request = CommitRequest { items: vec!["skill:pdf".into()], message: Some("Ship PDF".into()), author: None };
    assert_eq!(serde_json::to_value(bridge.commit(request).await.unwrap()).unwrap(), committed);
    let restore = bridge.restore(RestoreRequest { items: vec!["skill:pdf".into()], all: false }).await.unwrap();
    assert_eq!(restore.discarded.len(), 1);
    assert!(matches!(bridge.pull().await.unwrap(), PullResult::Blocked { .. }));
    assert!(bridge.log(LogQuery { before: Some(42), limit: Some(10), ..Default::default() }).await.unwrap().is_empty());
    assert_eq!(bridge.join_code().await.unwrap().code, "ONCE");
    let joined = bridge.join(JoinApiRequest { server_url: "http://server".into(), code: "CODE".into(),
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
        let body = &requests.iter().find(|(_, uri, _)| uri == "/commit").unwrap().2;
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
    let daemon = FakeDaemon::start(Router::new().route("/events", get(events)).with_state(attempts.clone())).await;
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let task = tokio::spawn(crate::events::forward(daemon.client.clone(), move |name, value| {
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
    assert_eq!(seen[4].1["retry_in_ms"], 500, "a valid event restores backoff");
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
        "machine_id":"alpha", "server_url":"http://server", "repo":directory.path(),
        "headline":"drafting", "connection":"connected", "last_error":null,
        "revoked":false, "server_baseline":null, "assignment_revision":1, "report":{},
    })));
    replies.insert("GET /draft".into(), (StatusCode::OK, json!([
        {"item":"skill:binary", "change":"added", "paths":["draft.bin"]}
    ])));
    let requests = Recorded::default();
    let daemon = FakeDaemon::start(Router::new().fallback(respond).with_state((requests.clone(), replies))).await;
    let bridge = Bridge::new(daemon.client.clone());
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_resolve_and_add_forward_once_with_payloads() {
    let requests = Recorded::default();
    let resolved = json!({"source":"github:example/skills", "git_ref":"main", "commit":"abcdef",
        "candidates":[{"path":"skills/pdf", "default_name":"pdf", "description":"Read PDFs", "taken":false}]});
    let mut replies = Replies::new();
    replies.insert("POST /imports/resolve".into(), (StatusCode::OK, resolved.clone()));
    replies.insert("POST /imports/add".into(),
        (StatusCode::CONFLICT, json!({"error":"Import name already exists"})));
    let daemon = FakeDaemon::start(Router::new().fallback(respond).with_state((requests.clone(), replies))).await;
    let bridge = Bridge::new(daemon.client.clone());
    let value = bridge.resolve_import(ResolveImportRequest {
        link: "https://github.com/example/skills/tree/main/skills/pdf".into(), path: "".into(),
    }).await.unwrap();
    assert_eq!(serde_json::to_value(value).unwrap(), resolved);
    let error = bridge.add_import(AddImportRequest {
        name: "pdf".into(), source: "github:example/skills".into(), path: Some("skills/pdf".into()),
        git_ref: Some("main".into()), commit: Some("abcdef".into()),
    }).await.unwrap_err();
    assert_eq!(error.status, Some(409));
    assert_eq!(error.message, "Import name already exists");
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].0, "POST");
    assert_eq!(requests[0].1, "/imports/resolve");
    assert_eq!(requests[0].2, json!({"link":"https://github.com/example/skills/tree/main/skills/pdf", "path":""}));
    assert_eq!(requests[1].1, "/imports/add");
    assert_eq!(requests[1].2, json!({"name":"pdf", "source":"github:example/skills", "path":"skills/pdf", "ref":"main", "commit":"abcdef"}));
}

#[test]
fn editor_detection_and_terminal_arguments() {
    use crate::editors::*;
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let executable = |name: &str| {
        let path = bin.join(name);
        std::fs::write(&path, "not executed").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    executable("nvim");
    let discovery = Discovery { path: vec![bin.clone()], data: vec![temp.path().into()], apps: vec![], terminal: None };
    assert_eq!(discovery.detect().len(), if cfg!(target_os = "macos") { 3 } else { 2 });
    executable("kitty");
    executable("zeditor");
    let found = discovery.detect();
    assert_eq!(found.iter().map(|e| e.view.id.as_str()).collect::<Vec<_>>(), ["zed", "nvim", "file-manager", "default-app"]);
    let terminal = found.iter().find(|e| e.view.id == "nvim").unwrap();
    assert_eq!(terminal.args, [std::ffi::OsString::from("--"), bin.join("nvim").into_os_string()]);
    for (name, args) in [("wezterm", vec!["start", "--"]), ("gnome-terminal", vec!["--"]), ("konsole", vec!["-e"]), ("ghostty", vec!["-e"]), ("alacritty", vec!["-e"]), ("xterm", vec!["-e"])] {
        assert_eq!(terminal_prefix(name), args.into_iter().map(std::ffi::OsString::from).collect::<Vec<_>>());
    }
    let desktop_bin = temp.path().join("code");
    std::fs::write(&desktop_bin, "never run").unwrap();
    std::fs::set_permissions(&desktop_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::create_dir(temp.path().join("applications")).unwrap();
    std::fs::write(temp.path().join("applications/code.desktop"), format!("[Desktop Entry]\nExec=\"{}\" --new-window %F\n", desktop_bin.display())).unwrap();
    let found = discovery.detect();
    assert_eq!(found[0].view.id, "code");
    assert_eq!(found[0].args, ["--new-window"]);
}

#[test]
fn editor_targets_reject_escapes_and_imports() {
    use crate::editors::*;
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    let skill = repo.join("personal/pdf");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "pdf").unwrap();
    let target = validate_target(repo, &skill, false).unwrap();
    let discovery = Discovery { path: vec![], data: vec![], apps: vec![], terminal: None };
    let mut editor = discovery.detect().remove(0);
    assert_eq!(target_path(&editor, &target), target.folder);
    editor.view.id = "default-app".into();
    assert_eq!(target_path(&editor, &target), target.file);
    editor.view.id = "code".into(); editor.view.kind = EditorKind::Gui;
    assert_eq!(target_path(&editor, &target), target.folder);
    editor.view.kind = EditorKind::Terminal;
    assert_eq!(target_path(&editor, &target), target.file);
    std::fs::create_dir(repo.join("instructions")).unwrap();
    let rule = repo.join("instructions/git.md");
    std::fs::write(&rule, "git").unwrap();
    let target = validate_target(repo, &rule, true).unwrap();
    editor.view.kind = EditorKind::Gui;
    assert_eq!(target_path(&editor, &target), rule.canonicalize().unwrap());
    assert!(validate_target(repo, std::path::Path::new("relative"), false).is_err());
    assert!(validate_target(repo, &repo.join("missing"), false).is_err());
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), repo.join("escape")).unwrap();
    assert!(validate_target(repo, &repo.join("escape"), false).is_err());
    std::fs::create_dir(repo.join("imported")).unwrap();
    std::os::unix::fs::symlink(&skill, repo.join("imported/pdf")).unwrap();
    assert!(validate_target(repo, &repo.join("imported/pdf"), false).is_err());
    std::fs::remove_file(skill.join("SKILL.md")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("file"), skill.join("SKILL.md")).unwrap();
    std::fs::write(outside.path().join("file"), "outside").unwrap();
    assert!(validate_target(repo, &skill, false).is_err());
}

#[tokio::test]
async fn editor_resolution_uses_daemon_repo_and_rejects_item_paths() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join("personal/pdf")).unwrap();
    std::fs::write(directory.path().join("personal/pdf/SKILL.md"), "pdf").unwrap();
    let mut replies = Replies::new();
    replies.insert("GET /status".into(), (StatusCode::OK, json!({
        "machine_id":"alpha", "server_url":"http://server", "repo":directory.path(),
        "headline":"drafting", "connection":"connected", "last_error":null,
        "revoked":false, "server_baseline":null, "assignment_revision":1, "report":{},
    })));
    replies.insert("GET /skills".into(), (StatusCode::OK, json!([
        {"name":"pdf", "item":"skill:pdf", "type":"personal", "description":null,
         "draft":null, "in_baseline":true, "path":directory.path().join("personal/pdf"),
         "enabled_harnesses":[], "clashes_with_repo":false, "variants":[]}
    ])));
    let daemon = FakeDaemon::start(Router::new().fallback(respond).with_state((Recorded::default(), replies))).await;
    let bridge = Bridge::new(daemon.client.clone());
    assert_eq!(bridge.editor_target("skill:pdf").await.unwrap().file, directory.path().join("personal/pdf/SKILL.md").canonicalize().unwrap());
    for item in ["skill:../pdf", "skill:/tmp/pdf", "skill:..", "unmanaged:pdf", "pdf"] {
        assert!(bridge.editor_target(item).await.is_err());
    }
}

#[test]
fn editor_bundle_detection_and_json_contract() {
    use crate::editors::*;
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("Cursor.app")).unwrap();
    let discovery = Discovery { path: vec![], data: vec![], apps: vec![temp.path().into()], terminal: None };
    let found = discovery.detect();
    assert_eq!(found[0].program, std::path::Path::new("/usr/bin/open"));
    assert_eq!(found[0].args, [std::ffi::OsString::from("-a"), temp.path().join("Cursor.app").into_os_string()]);
    let view = EditorsView { editors: found.into_iter().map(|e| e.view).collect(), preferred_editor: "cursor".into() };
    let value = serde_json::to_value(view).unwrap();
    assert_eq!(value["editors"][0], json!({"id":"cursor", "name":"Cursor", "kind":"gui"}));
    assert_eq!(value["preferred_editor"], "cursor");
    let preference = Preferences { preferred_editor: Some("cursor".into()) };
    let stored = serde_json::to_vec(&preference).unwrap();
    assert_eq!(serde_json::from_slice::<Preferences>(&stored).unwrap().preferred_editor.as_deref(), Some("cursor"));
}

#[tokio::test]
async fn skill_commands_preserve_queries_payloads_and_errors() {
    let requests = Recorded::default();
    let inventory = json!([{"name":"pdf", "item":null, "type":"unmanaged", "description":"PDF",
        "draft":null, "in_baseline":false, "path":null, "enabled_harnesses":[], "clashes_with_repo":false,
        "variants":[{"hash":"sha256:one", "description":"PDF", "locations":[{"harness":"claude", "path":"/tmp/pdf"}]}]}]);
    let tree = json!([{"path":"SKILL.md", "kind":"file", "size":12}]);
    let file = json!({"path":"docs/a b.md", "text":"# PDF", "binary":false, "too_large":false, "size":5});
    let mut replies = Replies::new();
    for (route, value) in [
        ("GET /skills", inventory.clone()),
        ("GET /skills/tree?item=unmanaged%3Apdf&hash=sha256%3Aone&harness=claude", tree.clone()),
        ("GET /skills/file?item=unmanaged%3Apdf&hash=sha256%3Aone&harness=claude&path=docs%2Fa%20b.md", file.clone()),
    ] { replies.insert(route.into(), (StatusCode::OK, value)); }
    replies.insert("POST /skills/adopt".into(), (StatusCode::NO_CONTENT, Value::Null));
    replies.insert("POST /imports/fork".into(), (StatusCode::CONFLICT, json!({"error":"Skill changed"})));
    let daemon = FakeDaemon::start(Router::new().fallback(respond).with_state((requests.clone(), replies))).await;
    let bridge = Bridge::new(daemon.client.clone());
    assert_eq!(serde_json::to_value(bridge.skills().await.unwrap()).unwrap(), inventory);
    let query = ContentQuery { item: "unmanaged:pdf".into(), hash: Some("sha256:one".into()), harness: Some("claude".into()) };
    assert_eq!(serde_json::to_value(bridge.skill_tree(query.clone()).await.unwrap()).unwrap(), tree);
    assert_eq!(serde_json::to_value(bridge.skill_file(FileQuery { content: query, path: "docs/a b.md".into() }).await.unwrap()).unwrap(), file);
    bridge.adopt_unmanaged(ConflictTarget { harness: "claude".into(), name: "pdf".into() }).await.unwrap();
    let error = bridge.fork("pdf".into()).await.unwrap_err();
    assert_eq!(error.status, Some(409));
    assert_eq!(error.message, "Skill changed");
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 5, "mutations are never retried");
    assert_eq!(requests[3].2, json!({"harness":"claude", "name":"pdf"}));
    assert_eq!(requests[4].2, json!({"name":"pdf"}));
}

#[test]
fn mac_terminal_argv_preserves_shell_metacharacters_as_arguments() {
    use crate::editors::terminal_prefix;
    use std::ffi::OsString;
    let mut args = terminal_prefix("Terminal.app");
    args.extend([OsString::from("/Applications/My editor's/bin/nvim"), OsString::from("/repo/a'; $(touch unexpected) \" b/SKILL.md")]);
    assert_eq!(args[0], "-e");
    assert!(args[1].to_str().unwrap().contains("quoted form of (argument as text)"));
    assert!(args[1].to_str().unwrap().contains("tell application \"Terminal\""));
    assert_eq!(args[2], "--");
    assert_eq!(args[3], "/Applications/My editor's/bin/nvim");
    assert_eq!(args[4], "/repo/a'; $(touch unexpected) \" b/SKILL.md");
    assert!(!args[1].to_str().unwrap().contains("unexpected"));
}

#[tokio::test]
async fn editor_resolution_uses_inventory_path_and_refuses_inventory_imports_and_missing_items() {
    let directory = tempfile::tempdir().unwrap();
    let repo = directory.path();
    let path = repo.join("custom/pdf");
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("SKILL.md"), "pdf").unwrap();
    let mut replies = Replies::new();
    replies.insert("GET /status".into(), (StatusCode::OK, json!({
        "machine_id":"alpha", "server_url":"http://server", "repo":repo, "headline":"in_sync",
        "connection":"connected", "last_error":null, "revoked":false, "server_baseline":null,
        "assignment_revision":1, "report":{}
    })));
    let make = |name: &str, kind: &str, path: &std::path::Path| json!({
        "name":name, "item":format!("skill:{name}"), "type":kind, "description":null,
        "draft":null, "in_baseline":true, "path":path, "enabled_harnesses":[],
        "clashes_with_repo":false, "variants":[]
    });
    replies.insert("GET /skills".into(), (StatusCode::OK, json!([
        make("pdf", "personal", &path), make("remote", "import", &path),
        make("gone", "personal", &repo.join("missing")), make("escape", "personal", std::path::Path::new("/tmp")),
        make("unmanaged", "unmanaged", &path),
    ])));
    let daemon = FakeDaemon::start(Router::new().fallback(respond).with_state((Recorded::default(), replies))).await;
    let bridge = Bridge::new(daemon.client.clone());
    assert_eq!(bridge.editor_target("skill:pdf").await.unwrap().folder, path.canonicalize().unwrap());
    assert!(bridge.editor_target("skill:remote").await.err().unwrap().message.contains("Fork"));
    for name in ["gone", "escape", "unknown", "unmanaged"] {
        assert!(bridge.editor_target(&format!("skill:{name}")).await.is_err());
    }
}

#[tokio::test]
async fn editor_inventory_accepts_literal_unusual_names() {
    let directory = tempfile::tempdir().unwrap();
    let names = ["my notes", "résumé", "a'\"b", "semi;colon", "$(touch nope)", "line\nbreak", "a\\b"];
    let entries: Vec<_> = names.iter().map(|name| {
        let path = directory.path().join("personal").join(name);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("SKILL.md"), "skill").unwrap();
        json!({"name":name, "item":format!("skill:{name}"), "type":"personal",
            "description":null, "draft":null, "in_baseline":true, "path":path,
            "enabled_harnesses":[], "clashes_with_repo":false, "variants":[]})
    }).collect();
    let mut replies = Replies::new();
    replies.insert("GET /skills".into(), (StatusCode::OK, json!(entries)));
    replies.insert("GET /status".into(), (StatusCode::OK, json!({
        "machine_id":"a", "server_url":"http://server", "repo":directory.path(),
        "headline":"drafting", "connection":"connected", "last_error":null,
        "revoked":false, "server_baseline":null, "assignment_revision":1, "report":{}
    })));
    let daemon = FakeDaemon::start(Router::new().fallback(respond).with_state((Recorded::default(), replies))).await;
    let bridge = Bridge::new(daemon.client.clone());
    for name in names {
        let target = bridge.editor_target(&format!("skill:{name}")).await.unwrap();
        assert_eq!(target.folder.file_name().unwrap(), name);
    }
    for name in [".", "..", "a/b", "nul\0"] {
        assert!(bridge.editor_target(&format!("skill:{name}")).await.is_err());
    }
}

#[test]
fn every_editor_launch_keeps_unusual_paths_in_one_argument() {
    use crate::editors::*;
    use std::{ffi::OsString, path::PathBuf, process::Stdio};
    let directory = tempfile::tempdir().unwrap();
    let stub = directory.path().join("capture");
    std::fs::write(&stub, "#!/bin/sh\nprintf '%s\\0' \"$@\"\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    for name in ["my notes", "résumé", "a'\"b", "semi;colon", "$(touch nope)", "line\nbreak", "a\\b"] {
        let folder = directory.path().join(name);
        let target = Target { file: folder.join("SKILL.md"), folder, rule: false };
        for kind in ["Terminal.app", "wezterm", "gnome-terminal", "kitty", "konsole", "ghostty", "alacritty", "xterm", "open", "xdg-open"] {
            let args = if matches!(kind, "open" | "xdg-open") { vec![] } else {
                let mut args = terminal_prefix(kind);
                args.push(OsString::from("/Applications/My editor's/bin/nvim"));
                args
            };
            let editor = Editor {
                view: EditorView { id: "default-app".into(), name: kind.into(), kind: EditorKind::System },
                program: stub.clone(), args: args.clone(),
            };
            let output = launch_command(&editor, &target).stdout(Stdio::piped()).output().unwrap();
            assert!(output.status.success());
            let mut expected: Vec<_> = args.iter().map(|s| s.as_encoded_bytes().to_vec()).collect();
            expected.push(target.file.as_os_str().as_encoded_bytes().to_vec());
            let mut expected = expected.join(&0);
            expected.push(0);
            assert_eq!(output.stdout, expected, "{kind} {name:?}");
            if kind == "Terminal.app" {
                assert!(!args[1].to_str().unwrap().contains(name));
                assert!(args[1].to_str().unwrap().contains("quoted form of (argument as text)"));
            }
        }
    }
    // Absolute paths also prevent open/xdg-open option interpretation.
    assert!(PathBuf::from("/repo/-option/SKILL.md").is_absolute());
}

#[cfg(target_os = "macos")]
#[test]
fn mac_terminal_script_shell_quoting_round_trips_unusual_paths() {
    use std::process::Command;
    let script = crate::editors::terminal_prefix("Terminal.app")[1].to_str().unwrap()
        .replace("    tell application \"Terminal\"\n        activate\n        do script shellCommand\n    end tell", "    return shellCommand");
    for path in ["my notes", "résumé", "a'\"b", "semi;colon", "$(touch nope)", "line\nbreak", "a\\b"] {
        let result = Command::new("/usr/bin/osascript").args(["-e", &script, "--", "/usr/bin/printf", "%s", path]).output().unwrap();
        assert!(result.status.success());
        let shell = String::from_utf8(result.stdout).unwrap();
        let output = Command::new("/bin/sh").args(["-c", shell.trim_end()]).output().unwrap();
        assert_eq!(output.stdout, path.as_bytes());
    }
}

#[tokio::test]
async fn new_commands_forward_typed_payloads() {
    let requests = Recorded::default();
    let mut replies = Replies::new();
    for path in ["POST /items/create", "POST /imports/remove", "POST /items/delete"] {
        replies.insert(path.into(), (StatusCode::NO_CONTENT, Value::Null));
    }
    replies.insert("POST /items/rename".into(), (StatusCode::OK, json!({"item":"skill:docs","migrated":["claude"],"assignment_error":null})));
    replies.insert("GET /log?machine=a%20b&harness=claude&author=dev&item=skill%3Apdf&before=9&limit=50".into(), (StatusCode::OK, json!([])));
    replies.insert("GET /log/facets".into(), (StatusCode::OK, json!({"machines":["a"],"harnesses":["claude"],"authors":["dev"]})));
    replies.insert("POST /push".into(), (StatusCode::OK, json!({"pushed":true})));
    replies.insert("POST /imports/check".into(), (StatusCode::OK, json!({"imports":[],"updates":[],"errors":[],"checked_at":1})));
    replies.insert("POST /conflicts/diff".into(), (StatusCode::OK, json!({"item":"skill:pdf","diff":"+draft"})));
    let daemon = FakeDaemon::start(Router::new().fallback(respond).with_state((requests.clone(), replies))).await;
    let bridge = Bridge::new(daemon.client.clone());
    assert!(bridge.push().await.unwrap().pushed);
    bridge.create_item(CreateItemRequest { kind: "skill".into(), name: "pdf".into(), description: Some("PDF".into()) }).await.unwrap();
    bridge.remove_import("pdf".into()).await.unwrap();
    assert_eq!(bridge.check_imports().await.unwrap().checked_at, Some(1));
    assert_eq!(bridge.conflict_diff(ConflictDiffRequest { harness: None, name: None, id: Some("recovered".into()) }).await.unwrap().diff, "+draft");
    bridge.delete_item(DeleteItemRequest { kind: "rule".into(), name: "style".into() }).await.unwrap();
    let renamed = bridge.rename_item(RenameItemRequest { kind: "skill".into(), name: "pdf".into(), to: "docs".into() }).await.unwrap();
    assert_eq!((renamed.item.to_string(), renamed.migrated), ("skill:docs".to_string(), vec!["claude".to_string()]));
    let query = LogQuery { machine: Some("a b".into()), harness: Some("claude".into()), author: Some("dev".into()),
        item: Some("skill:pdf".into()), before: Some(9), limit: Some(50) };
    assert!(bridge.log(query).await.unwrap().is_empty());
    assert_eq!(bridge.log_facets().await.unwrap().authors, ["dev"]);
    let requests = requests.lock().unwrap();
    assert_eq!(requests[1].2, json!({"kind":"skill","name":"pdf","description":"PDF"}));
    assert_eq!(requests[2].2, json!({"name":"pdf"}));
    assert_eq!(requests[4].2["id"], "recovered");
    assert_eq!(requests[5].2, json!({"kind":"rule","name":"style"}));
    assert_eq!(requests[6].2, json!({"kind":"skill","name":"pdf","to":"docs"}));
}
