#![cfg(unix)]
#![recursion_limit = "256"]

use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{StatusCode, header},
    response::Response,
};
use serde_json::{Value, json};
use sk1ll_daemon::client::Client;
use sk1ll_gui::{commands::Bridge, editors::EditorCache, events, register_commands};
use sk1ll_testkit::{Home, child, outcome, read, write};
use tauri::{
    WebviewWindow, WebviewWindowBuilder,
    ipc::{CallbackFn, InvokeBody},
    test::{self, MockRuntime},
    webview::InvokeRequest,
};
use tokio::{net::UnixListener, time::timeout};

struct Desktop {
    _app: tauri::App<MockRuntime>,
    window: WebviewWindow<MockRuntime>,
}

impl Desktop {
    fn new(socket: &Path, data: &Path) -> Self {
        let app = register_commands(test::mock_builder())
            .manage(Bridge::new(Client::new(socket)))
            .manage(EditorCache::default())
            .build(test::mock_context(test::noop_assets()))
            .unwrap();
        let window = WebviewWindowBuilder::new(&app, "main", Default::default())
            .data_directory(data.to_path_buf())
            .build()
            .unwrap();
        Self { _app: app, window }
    }

    fn call(&self, cmd: &str, args: Value) -> Result<Value, Value> {
        test::get_ipc_response(
            &self.window,
            InvokeRequest {
                cmd: cmd.into(),
                callback: CallbackFn(0),
                error: CallbackFn(1),
                url: "tauri://localhost".parse().unwrap(),
                body: InvokeBody::Json(args),
                headers: Default::default(),
                invoke_key: test::INVOKE_KEY.into(),
            },
        )
        .map(|response| response.deserialize().unwrap())
    }

    #[track_caller]
    fn invoke(&self, cmd: &str, args: Value) -> Value {
        self.call(cmd, args).unwrap_or_else(|error| panic!("{cmd}: {error}"))
    }
}

type Requests = Arc<Mutex<Vec<(String, Value)>>>;
type Replies = BTreeMap<String, (StatusCode, Value)>;

struct DaemonStub {
    task: tokio::task::JoinHandle<()>,
    socket: PathBuf,
    requests: Requests,
}

impl DaemonStub {
    fn start(directory: &Path, replies: Replies) -> Self {
        let requests = Requests::default();
        let router = Router::new().fallback(respond).with_state((requests.clone(), replies));
        let (socket, task) = serve(directory, router);
        Self { task, socket, requests }
    }
}

impl Drop for DaemonStub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Replies 200 with each body.
fn ok<const N: usize>(rows: [(&str, Value); N]) -> Replies {
    rows.into_iter().map(|(route, body)| (route.to_owned(), (StatusCode::OK, body))).collect()
}

fn serve(directory: &Path, router: Router) -> (PathBuf, tokio::task::JoinHandle<()>) {
    let socket = directory.join("daemon.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (socket, task)
}

async fn respond(State((requests, replies)): State<(Requests, Replies)>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let route = format!("{} {}", parts.method, parts.uri);
    let bytes = to_bytes(body, 4096).await.unwrap();
    let body = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() };
    requests.lock().unwrap().push((route.clone(), body));
    let (status, value) = replies.get(&route).cloned().unwrap_or((StatusCode::NOT_FOUND, Value::Null));
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(if status == StatusCode::NO_CONTENT || value.is_null() {
            Body::empty()
        } else {
            Body::from(value.to_string())
        })
        .unwrap()
}

fn status(repo: &Path) -> Value {
    json!({
        "machine_id":"alpha", "server_url":"http://server", "mode":"joined", "has_origin":true,
        "repo_ready":true, "repo":repo, "headline":"drafting", "connection":"connected", "last_error":null,
        "revoked":false, "server_baseline":null, "assignment_revision":1, "report":{}, "draft_commit_complete":false,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn desktop_commands_preserve_daemon_payloads_and_return_errors_without_retrying() {
    let directory = tempfile::tempdir().unwrap();
    let query = json!({"item":"unmanaged:pdf", "hash":"sha256:one", "harness":"claude"});
    let file_query = json!({"item":"unmanaged:pdf", "hash":"sha256:one", "harness":"claude", "path":"docs/a b.md"});
    let file = json!({"path":"docs/a b.md", "text":"# PDF", "binary":false, "too_large":false, "size":5, "fingerprint":"sha256:loaded"});
    let change = json!({"change":{"op":"disable","name":"codex"}});
    let changes = json!({"changes":[{"harness":"claude","kind":"skill","item_id":"local-id","name":"local","enabled":true}],
        "expected":{"revision":7}});
    let commit = json!({"items":["skill:pdf"],"message":"Ship PDF","author":null});
    let resolve = json!({"link":"https://github.com/example/skills/tree/main/skills/pdf","path":""});
    let import =
        json!({"name":"pdf","source":"github:example/skills","path":"skills/pdf","ref":"main","commit":"abcdef"});
    let join = json!({"server_url":"http://server","code":"CODE","machine_id":"alpha","copy_from":"beta","repo":null});
    let log = json!({"machine":"a b","harness":"claude","author":"dev","item":"skill:pdf","before":9,"limit":50});
    let write_file = json!({"item":"rule:style","harness":null,"hash":null,"path":"style.md",
        "content":"# Updated","expected":"sha256:loaded"});
    let create = json!({"kind":"skill","name":"pdf","description":"PDF"});
    let delete = json!({"kind":"rule","name":"style"});
    let rename = json!({"kind":"skill","name":"pdf","to":"docs"});
    let target = json!({"harness":"claude","name":"pdf"});

    // Each row: registered Tauri name, IPC arguments, the one daemon request it makes, the
    // request body, and the daemon's reply. `connection_info` answers without a request.
    let cases = json!([
        ["connection_info", {}, "", null, 200, directory.path().join("daemon.sock")],
        ["fleet", {}, "GET /fleet", null, 200, {"baseline":"abc","machines":[]}],
        ["items", {}, "GET /items", null, 200,
            [{"item":"skill:pdf","in_baseline":true,"draft":null,"unpushed":false,"description":"Read PDF files","id":null}]],
        ["assignment", {"machine":"alpha"}, "GET /machines/alpha/assignment", null, 200, {"revision":7,"harnesses":{}}],
        ["diff", {"item":"skill:pdf"}, "GET /draft/diff?item=skill%3Apdf", null, 200, {"item":"skill:pdf","diff":"+new\n"}],
        ["commit_message", {"items":["skill:pdf","rule:git"]}, "GET /commit/message?items=skill%3Apdf%2Crule%3Agit", null, 200,
            {"items":["skill:pdf","rule:git"],"message":"prefilled"}],
        ["commit", {"request":commit}, "POST /commit", commit, 200,
            {"sha":"abc","items":["skill:pdf"],"message":"Ship PDF","pushed":false,"push_error":"Server offline"}],
        ["pull", {}, "POST /pull", null, 200, {"state":"blocked","baseline":"abc","items":["skill:pdf"]}],
        ["restore", {"request":{"items":["skill:pdf"],"all":false}}, "POST /restore", {"items":["skill:pdf"],"all":false}, 200,
            {"discarded":["skill:pdf"]}],
        ["set_assignment", {"machine":"alpha","changes":changes}, "PUT /machines/alpha/assignment", changes, 409,
            {"error":"the Assignment changed since revision 7"}],
        ["log", {"query":log}, "GET /log?machine=a%20b&harness=claude&author=dev&item=skill%3Apdf&before=9&limit=50", null, 200, []],
        ["log_facets", {}, "GET /log/facets", null, 200, {"machines":["a"],"harnesses":["claude"],"authors":["dev"]}],
        ["join_code", {"scope":"fleet"}, "POST /join-codes", {"scope":"fleet"}, 200, {"code":"ONCE","scope":"fleet","expires_at":100}],
        ["join", {"request":join}, "POST /join", join, 200,
            {"machine_id":"alpha","repo":"/tmp/skills","setup":"adopted","harnesses":[],"revision":7}],
        ["adopt", {"target":target}, "POST /conflicts/adopt", target, 204, null],
        ["overwrite", {"target":target}, "POST /conflicts/overwrite", target, 204, null],
        ["adopt_displaced", {"id":"a/b"}, "POST /conflicts/displaced/a%2Fb/adopt", null, 200, "/tmp/skills/personal/pdf"],
        ["discard_displaced", {"id":"a/b"}, "POST /conflicts/displaced/a%2Fb/discard", null, 204, null],
        ["remove_machine", {"machine":"a/b"}, "DELETE /machines/a%2Fb", null, 204, null],
        ["resolve_import", {"request":resolve}, "POST /imports/resolve", resolve, 200,
            {"source":"github:example/skills","git_ref":"main","commit":"abcdef","candidates":[]}],
        ["add_import", {"request":import}, "POST /imports/add", import, 409, {"error":"Import name already exists"}],
        ["skill_tree", {"query":query}, "GET /skills/tree?item=unmanaged%3Apdf&hash=sha256%3Aone&harness=claude", null, 200,
            [{"path":"SKILL.md","kind":"file","size":12}]],
        ["skill_file", {"query":file_query},
            "GET /skills/file?item=unmanaged%3Apdf&hash=sha256%3Aone&harness=claude&path=docs%2Fa%20b.md", null, 200, file],
        ["write_skill_file", {"request":write_file}, "POST /skills/file", write_file, 409, {"error":"File changed on disk"}],
        ["adopt_unmanaged", {"target":target}, "POST /skills/adopt", target, 204, null],
        ["fork", {"name":"pdf"}, "POST /imports/fork", {"name":"pdf"}, 409, {"error":"Skill changed"}],
        ["create_item", {"request":create}, "POST /items/create", create, 204, null],
        ["delete_item", {"request":delete}, "POST /items/delete", delete, 204, null],
        ["rename_item", {"request":rename}, "POST /items/rename", rename, 200, {"item":"skill:docs"}],
        ["remove_import", {"name":"pdf"}, "POST /imports/remove", {"name":"pdf"}, 204, null],
        ["check_imports", {}, "POST /imports/check", null, 200, {"imports":[],"updates":[],"errors":[],"checked_at":1}],
        ["push", {}, "POST /push", null, 200, {"pushed":true}],
        ["conflict_diff", {"request":{"id":"recovered"}}, "POST /conflicts/diff", {"harness":null,"name":null,"id":"recovered"}, 200,
            {"item":"skill:pdf","diff":"+draft"}],
        ["preview_harness", {"request":change}, "POST /harnesses/preview", change, 200,
            {"path":"/h/machine.toml","fingerprint":"preview","name":"codex","before":null,"after":null,
             "placed":[{"harness":"codex","kind":"remove","item":"skill","name":"pdf","dest":"/h/pdf","reason":null}],
             "sync":[],"awaiting_assignment":false}],
        ["render_instructions", {"name":"codex"}, "GET /harnesses/codex/render", null, 200, []],
        ["preview_sync", {}, "GET /sync", null, 200, {"conflicts":[],"actions":[]}],
        ["sync_now", {"fingerprint":"reviewed"}, "POST /sync", {"dry_run":false,"force":false,"fingerprint":"reviewed"}, 200,
            {"conflicts":[]}],
    ]);
    let cases: Vec<(String, Value, String, Value, u16, Value)> = serde_json::from_value(cases).unwrap();

    let replies = cases
        .iter()
        .filter(|case| !case.2.is_empty())
        .map(|case| (case.2.clone(), (StatusCode::from_u16(case.4).unwrap(), case.5.clone())))
        .collect();
    let daemon = DaemonStub::start(directory.path(), replies);
    let desktop = Desktop::new(&daemon.socket, &directory.path().join("webview"));
    for (cmd, args, route, body, status, response) in cases {
        let result = desktop.call(&cmd, args);
        if status == 409 {
            assert_eq!(
                result.unwrap_err(),
                json!({"status":409,"message":response["error"],"unreachable":false}),
                "{cmd}"
            );
        } else {
            // Older daemon responses omit optional fields. Require the supplied fields at IPC.
            let actual = result.unwrap_or_else(|error| panic!("{cmd}: {error}"));
            if let Some(fields) = response.as_object() {
                for (field, value) in fields {
                    assert_eq!(&actual[field], value, "{cmd}.{field}");
                }
            } else {
                assert_eq!(actual, response, "{cmd}");
            }
        }
        let mut requests = daemon.requests.lock().unwrap();
        let expected: Vec<_> = (!route.is_empty()).then_some((route, body)).into_iter().collect();
        assert_eq!(*requests, expected, "{cmd}");
        requests.clear();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn desktop_distinguishes_a_missing_daemon_from_a_missing_route() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("missing.sock");
    let desktop = Desktop::new(&socket, &directory.path().join("offline"));
    let error = desktop.call("status", json!({})).unwrap_err();
    assert_eq!(error["unreachable"], true);
    assert!(error["status"].is_null());
    assert!(error["message"].as_str().unwrap().contains("cannot reach the daemon"));

    let daemon = DaemonStub::start(directory.path(), Replies::new());
    let desktop = Desktop::new(&daemon.socket, &directory.path().join("connected"));
    let error = desktop.call("harness_settings", json!({})).unwrap_err();
    assert_eq!(error, json!({"message":"404 Not Found","status":404,"unreachable":false}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn draft_review_fingerprints_change_when_binary_bytes_links_or_files_change() {
    let directory = tempfile::tempdir().unwrap();
    let repo = directory.path();
    let binary = repo.join("draft.bin");
    let link = repo.join("link");
    fs::write(&binary, [0, 1, 2]).unwrap();
    symlink("draft.bin", &link).unwrap();
    let daemon = DaemonStub::start(
        repo,
        ok([
            ("GET /status", status(repo)),
            (
                "GET /draft",
                json!([
                    {"item":"skill:binary","change":"added","paths":["draft.bin","link"]},
                    {"item":"skill:reversed","change":"modified","paths":["link","draft.bin"]},
                ]),
            ),
        ]),
    );
    let desktop = Desktop::new(&daemon.socket, &repo.join("webview"));
    let draft = || desktop.invoke("draft", json!({}));
    let first = draft();
    assert_eq!(first[0]["item"], "skill:binary");
    assert_eq!(first[0]["change"], "added");
    assert_eq!(first[0]["paths"], json!(["draft.bin", "link"]));
    assert_eq!(first[0]["fingerprint"].as_str().unwrap().len(), 64);
    assert_eq!(first[0]["fingerprint"], first[1]["fingerprint"], "path order does not affect review");

    fs::write(&binary, [0, 3, 2]).unwrap();
    let edited = draft();
    assert_ne!(first[0]["fingerprint"], edited[0]["fingerprint"], "same-length binary edit");
    fs::remove_file(&link).unwrap();
    symlink("elsewhere", &link).unwrap();
    let linked = draft();
    assert_ne!(edited[0]["fingerprint"], linked[0]["fingerprint"], "a dangling symlink still has a target");
    fs::remove_file(&binary).unwrap();
    assert_ne!(linked[0]["fingerprint"], draft()[0]["fingerprint"], "deleting the file");
    assert_eq!(
        *daemon.requests.lock().unwrap(),
        ["GET /status", "GET /draft"]
            .repeat(4)
            .into_iter()
            .map(|route| (route.into(), Value::Null))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn event_task_reconnects_after_failures_eof_and_malformed_events_and_forwards_valid_ones() {
    use axum::routing::get;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::mpsc;

    async fn events(State(attempts): State<Arc<AtomicUsize>>) -> Response {
        let stream = match attempts.fetch_add(1, Ordering::SeqCst) {
            1 => concat!(
                "event: assignment\ndata: {\"type\":\"assignment\",\"revision\":9}\n\n",
                "event: draft\ndata: {\"type\":\"draft\"}\n\n"
            ),
            2 => "event: draft\ndata: {not json\n\n",
            _ => return Response::builder().status(503).body(Body::empty()).unwrap(),
        };
        Response::builder().header(header::CONTENT_TYPE, "text/event-stream").body(Body::from(stream)).unwrap()
    }
    let attempts = Arc::new(AtomicUsize::new(0));
    let directory = tempfile::tempdir().unwrap();
    let (socket, server) =
        serve(directory.path(), Router::new().route("/events", get(events)).with_state(attempts.clone()));
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let task = tokio::spawn(events::forward(Client::new(socket), move |name, value| {
        sender.send((name.to_string(), value)).unwrap();
    }));
    let mut seen = Vec::new();
    timeout(Duration::from_secs(8), async {
        while seen.len() < 8 {
            seen.push(receiver.recv().await.unwrap());
        }
    })
    .await
    .unwrap();
    task.abort();
    server.abort();
    assert_eq!(seen[0].0, "daemon-connection");
    assert_eq!(seen[0].1["reachable"], false);
    assert_eq!(seen[0].1["retry_in_ms"], 500);
    assert_eq!(seen[1].1["reachable"], true);
    assert_eq!(seen[2], ("assignment".into(), json!({"type":"assignment","revision":9})));
    assert_eq!(seen[3], ("draft".into(), json!({"type":"draft"})));
    assert_eq!(seen[4].1["retry_in_ms"], 500, "a valid event restores backoff");
    assert_eq!(seen[5].1["reachable"], true);
    assert_eq!(seen[6].1["reachable"], false, "a malformed event drops the subscription");
    assert_eq!(seen[6].1["retry_in_ms"], 1000);
    assert_eq!(seen[7].1["retry_in_ms"], 2000);
    assert_eq!(attempts.load(Ordering::SeqCst), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn editors_use_daemon_paths_and_keep_names_in_one_argument() {
    let Some(root) = child::dir() else {
        let home = Home::new();
        let root = home.path();
        let mut cmd = child::command("editors_use_daemon_paths_and_keep_names_in_one_argument", root);
        home.apply(&mut cmd)
            .current_dir(root)
            .env("PATH", root.join("bin"))
            .env("TERMINAL", "kitty")
            .env("XDG_DATA_DIRS", root.join("empty"))
            .env("SK1LL_EDITOR_HOME", root.join("user"))
            .env("SK1LL_EDITOR_XDG_CONFIG_HOME", root.join("user/config"))
            .env("SK1LL_EDITOR_XDG_DATA_HOME", root.join("user/data"))
            .env("SK1LL_EDITOR_XDG_CACHE_HOME", "")
            .env("SK1LL_EDITOR_XDG_STATE_HOME", "")
            .env("SK1LL_EDITOR_RECORD", root.join("launch"));
        outcome(cmd.output().unwrap()).unwrap();
        return;
    };

    let repo = root.join("repo");
    fs::create_dir(&repo).unwrap();
    let repo = repo.canonicalize().unwrap();
    let names = ["my notes", "résumé", "a'\"b", "semi;colon", "$(touch nope)", "line\nbreak", "a\\b"];
    let mut entries = Vec::new();
    for name in names {
        let folder = repo.join("custom").join(name);
        write(&folder.join("SKILL.md"), "skill");
        entries.push(inventory(name, "personal", &folder));
    }
    let outside = root.join("outside");
    write(&outside.join("SKILL.md"), "outside");
    symlink(&outside, repo.join("escape")).unwrap();
    let escaped_file = repo.join("custom/escaped-file");
    fs::create_dir(&escaped_file).unwrap();
    symlink(outside.join("SKILL.md"), escaped_file.join("SKILL.md")).unwrap();
    let imported = repo.join("imported");
    fs::create_dir(&imported).unwrap();
    symlink(repo.join("custom/my notes"), imported.join("symlink")).unwrap();
    entries.extend([
        inventory("remote", "import", &repo.join("custom/my notes")),
        inventory("unmanaged", "unmanaged", &repo.join("custom/my notes")),
        inventory("gone", "personal", &repo.join("missing")),
        inventory("escape", "personal", &repo.join("escape")),
        inventory("escaped-file", "personal", &escaped_file),
        inventory("relative", "personal", Path::new("repo/custom/my notes")),
        inventory("import-symlink", "personal", &imported.join("symlink")),
    ]);
    let rule = repo.join("instructions/git.md");
    write(&rule, "Git");
    entries.push(inventory("git", "rule", &rule));

    let config = root.join("config/machine.toml");
    let daemon = DaemonStub::start(
        &root,
        ok([
            ("GET /status", status(&repo)),
            ("GET /skills", json!(entries)),
            ("GET /harnesses/config/path", json!(config)),
        ]),
    );
    let desktop = Desktop::new(&daemon.socket, &root.join("webview"));

    let code = root.join("My editor/bin/code");
    let terminal = root.join("bin/kitty");
    for editor in [&code, &terminal, &root.join("bin/xdg-open"), &root.join("bin/nvim")] {
        write(
            editor,
            r#"#!/bin/sh
printf '%s\0' "$HOME" "$XDG_CONFIG_HOME" "${XDG_CACHE_HOME-unset}" "${SK1LL_EDITOR_HOME-unset}" "$@" > "$SK1LL_EDITOR_RECORD.tmp"
/bin/mv "$SK1LL_EDITOR_RECORD.tmp" "$SK1LL_EDITOR_RECORD"
"#,
        );
        fs::set_permissions(editor, fs::Permissions::from_mode(0o755)).unwrap();
    }
    write(
        &root.join("user/data/applications/code.desktop"),
        &format!("[Desktop Entry]\nExec=\"{}\" --new-window %F\n", code.display()),
    );

    let editors = desktop.invoke("editors", json!({}));
    assert_eq!(editors["editors"][0], json!({"id":"code","name":"VS Code","kind":"gui"}));
    assert_eq!(editors["preferred_editor"], "code");
    assert!(
        editors["editors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e == &json!({"id":"nvim","name":"Neovim","kind":"terminal"}))
    );
    desktop.invoke("set_preferred_editor", json!({"editorId":"nvim"}));
    assert_eq!(desktop.invoke("editors", json!({"refresh":true}))["preferred_editor"], "nvim");

    for (item, hint) in [
        ("skill:remote", "Fork"),
        ("skill:unmanaged", "Adopt"),
        ("unmanaged:pdf", "Adopt"),
        ("skill:unknown", "no longer in Skills"),
        ("skill:gone", "does not exist"),
        ("skill:escape", "inside the repo"),
        ("skill:escaped-file", "inside the repo"),
        ("skill:import-symlink", "Fork"),
        ("skill:relative", "must be absolute"),
        ("skill:../pdf", "Invalid item name"),
        ("skill:/tmp/pdf", "Invalid item name"),
        ("skill:.", "Invalid item name"),
        ("skill:..", "Invalid item name"),
        ("skill:nul\0", "Invalid item name"),
        ("pdf", "Expected skill"),
    ] {
        let error = desktop.call("open_in_editor", json!({"editorId":"code","item":item})).unwrap_err();
        assert!(error["message"].as_str().unwrap().contains(hint), "{item:?}: {error}");
    }
    assert!(!root.join("launch").exists(), "rejected inventory entries never launch an editor");

    for name in names {
        let skill = format!("skill:{name}");
        let folder = repo.join("custom").join(name).canonicalize().unwrap();
        desktop.invoke("open_in_editor", json!({"editorId":"code","item":skill}));
        assert_eq!(editor_arguments(&root), ["--new-window", folder.to_str().unwrap()]);

        desktop.invoke("open_in_editor", json!({"editorId":"nvim","item":skill}));
        let args = editor_arguments(&root);
        let file = folder.join("SKILL.md");
        let file = file.to_str().unwrap();
        let nvim = root.join("bin/nvim");
        let nvim = nvim.to_str().unwrap();
        assert_eq!(args, ["--", nvim, file]);
        for (editor, destination) in [("file-manager", folder.to_str().unwrap()), ("default-app", file)] {
            desktop.invoke("open_in_editor", json!({"editorId":editor,"item":skill}));
            assert_eq!(editor_arguments(&root), [destination]);
        }
    }
    desktop.invoke("open_in_editor", json!({"editorId":"code","item":"rule:git"}));
    assert_eq!(editor_arguments(&root), ["--new-window", rule.canonicalize().unwrap().to_str().unwrap()]);

    desktop.invoke("open_machine_config", json!({"editorId":"code"}));
    assert_eq!(editor_arguments(&root), ["--new-window", config.canonicalize().unwrap().to_str().unwrap()]);
    assert_eq!(config.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    assert!(read(&config).starts_with("# "));
    write(&config, "id = \"box\"\n");
    desktop.invoke("open_machine_config", json!({"editorId":"code"}));
    editor_arguments(&root);
    assert_eq!(read(&config), "id = \"box\"\n");

    for (path, hint) in [(Path::new("relative.toml"), "must be absolute"), (root.as_path(), "is not a file")] {
        let socket_dir = tempfile::tempdir().unwrap();
        let daemon = DaemonStub::start(socket_dir.path(), ok([("GET /harnesses/config/path", json!(path))]));
        let desktop = Desktop::new(&daemon.socket, &socket_dir.path().join("webview"));
        let error = desktop.call("open_machine_config", json!({"editorId":"code"})).unwrap_err();
        assert!(error["message"].as_str().unwrap().contains(hint), "{error}");
    }
    assert!(!root.join("launch").exists());
}

fn inventory(name: &str, kind: &str, path: &Path) -> Value {
    let prefix = if kind == "rule" { "rule" } else { "skill" };
    json!({"name":name,"item":format!("{prefix}:{name}"),"type":kind,"description":null,"draft":null,
        "in_baseline":true,"path":path,"enabled_harnesses":[],"clashes_with_repo":false,"variants":[]})
}

#[track_caller]
fn editor_arguments(root: &Path) -> Vec<String> {
    let record = root.join("launch");
    let bytes = (0..200)
        .find_map(|_| {
            let result = fs::read(&record);
            if matches!(&result, Err(error) if error.kind() == std::io::ErrorKind::NotFound) {
                std::thread::sleep(Duration::from_millis(10));
                return None;
            }
            Some(result.unwrap())
        })
        .expect("editor did not launch");
    fs::remove_file(record).unwrap();
    let output = String::from_utf8(bytes).unwrap();
    let mut args = output.split_terminator('\0');
    assert_eq!(args.next(), root.join("user").to_str());
    assert_eq!(args.next(), root.join("user/config").to_str());
    assert_eq!(args.next(), Some("unset"), "the sandbox cache path was removed");
    assert_eq!(args.next(), Some("unset"), "editor override variables were removed");
    args.map(str::to_owned).collect()
}
