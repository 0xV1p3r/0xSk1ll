mod common;

use std::{
    fs,
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
};

use axum::{
    Json, Router,
    extract::{Request, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde_json::{Value, json};
use sk1ll_core::{Assignment, ItemRef};
use sk1ll_testkit::{Machine, outcome, server::Server, write};

use common::{
    RunningDaemon, command, command_for, failure, item_id, join_fleet, joined, other_repo, path, run, run_on, runtime,
    with_harnesses, with_pdf,
};

/// A joined Machine's Assignment changes at the server, through the engine's
/// session: an unreachable server changes nothing.
#[test]
fn fleet_assignment_fails_when_the_server_is_unreachable() {
    let repo = with_pdf();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let owner = joined(&repo, &url);
    let snapshot = sk1ll_proto::AssignmentSnapshot::default();
    sk1ll_daemon::state::save_snapshot(
        &repo.daemon_state().with_file_name("daemon-assignment.json"),
        &snapshot,
        Some(&owner),
    )
    .unwrap();
    let err = run(&repo, &["assign", "h", "skill", "pdf", "on"]).expect_err("fleet assignment must reach the server");
    assert!(err.contains("server unreachable"), "{err}");
    let stored =
        sk1ll_daemon::state::load_snapshot(&repo.daemon_state().with_file_name("daemon-assignment.json"), Some(&owner))
            .unwrap()
            .unwrap();
    assert_eq!(stored.revision, 0);
    let pdf = item_id(&repo, ItemRef::Skill("pdf".into()));
    let mirror = Assignment::load(&repo.assignment()).unwrap();
    assert!(!mirror.harness("h").skills.contains_key(&pdf), "the mirror cannot record an unaccepted change");
}

/// A joined Machine syncs by the server's snapshot, never by the machine-local
/// file, and leaves the harnesses alone without one.
#[test]
fn sync_uses_the_authoritative_assignment_and_requires_a_snapshot() {
    let repo = with_pdf();
    let skills = repo.root().join("h");
    let owner = joined(&repo, "http://127.0.0.1:1");
    let mut assignment = Assignment::default();
    assignment.set_skill("h", &item_id(&repo, ItemRef::Skill("pdf".into())), false);
    sk1ll_daemon::state::save_snapshot(
        &repo.daemon_state().with_file_name("daemon-assignment.json"),
        &sk1ll_proto::AssignmentSnapshot { revision: 7, assignment, registration: None },
        Some(&owner),
    )
    .unwrap();
    run(&repo, &["sync"]).unwrap();
    assert!(!skills.join("pdf").exists(), "a disabled skill stays out");
    Assignment::default().save(&repo.assignment()).unwrap();
    run(&repo, &["sync"]).unwrap();
    assert!(!skills.join("pdf").exists(), "a stale mirror must not override the snapshot");
    fs::remove_file(repo.daemon_state().with_file_name("daemon-assignment.json")).unwrap();
    let err = run(&repo, &["sync"]).expect_err("a joined machine needs its authoritative assignment");
    assert!(err.contains("waiting for the server's assignment"), "{err}");
    assert!(!skills.join("pdf").exists());
}

/// A joined Machine reads another repo as a standalone one (`draft`), and
/// still refuses to change it.
#[test]
fn a_joined_machine_reads_another_repo_but_changes_only_its_own() {
    let repo = with_pdf();
    joined(&repo, "http://127.0.0.1:1");
    let other = other_repo(&repo);
    fs::write(other.join("personal/web/SKILL.md"), "Edited.").unwrap();
    let socket = repo.no_daemon();
    let draft = outcome(command_for(&repo, &other, &socket, &["draft"]).output().unwrap()).unwrap();
    assert!(draft.contains("M skill:web"), "{draft}");
    let error = outcome(command_for(&repo, &other, &socket, &["sync"]).output().unwrap()).unwrap_err();
    assert!(error.contains("differs from the joined repo"), "{error}");
}

/// A joined Machine adopts only into the joined repo, and, without a daemon,
/// only once the engine in this process reaches the server for the
/// Assignment write. Either refusal publishes nothing.
#[test]
fn joined_adoption_needs_the_joined_repo_and_a_reachable_server() {
    let fixture = with_harnesses();
    let root = fixture.root();
    let elsewhere = sk1ll_core::Repo::init(&root.join("joined")).unwrap();
    // A different process may own the joined repo's lock. --repo must not bypass it.
    let _lock = sk1ll_core::EngineLock::acquire(elsewhere.root()).unwrap();
    write(&root.join("a/pdf/SKILL.md"), "unmanaged user content");
    for (joined_repo, expected) in
        [(elsewhere.root().to_path_buf(), "joined repo"), (root.join("repo"), "Adopt needs a connection to the Server")]
    {
        let state =
            sk1ll_daemon::DaemonState::new("a".into(), "http://127.0.0.1:1".into(), "token".into(), joined_repo);
        state.save(&fixture.daemon_state()).unwrap();
        let message = failure(command(&fixture, &["adopt", "a", "pdf"]).output().unwrap());
        assert!(message.contains(expected), "{message}");
        assert!(!elsewhere.personal_dir().join("pdf").exists() && !root.join("repo/personal/pdf").exists());
        assert!(!fixture.assignment().exists());
        assert_eq!(fs::read_to_string(root.join("a/pdf/SKILL.md")).unwrap(), "unmanaged user content");
    }
}

#[test]
fn another_repos_daemon_refuses_writes_and_allows_previews() {
    let machine = with_pdf();
    let other = other_repo(&machine);
    let daemon = runtime().block_on(RunningDaemon::start(&machine, &other));
    let error = run_on(&machine, &daemon.socket, &["sync"]).unwrap_err();
    assert!(error.contains(&format!("the sk1ll daemon manages {}", other.display())), "{error}");
    let deployed = machine.root().join("h");
    let web = sk1ll_testkit::read(&deployed.join("web/SKILL.md"));
    assert!(!deployed.join("pdf").exists());
    let plan = run_on(&machine, &daemon.socket, &["sync", "--dry-run"]).unwrap();
    assert!(plan.contains("pdf"), "{plan}");
    assert!(!deployed.join("pdf").exists());
    assert_eq!(sk1ll_testkit::read(&deployed.join("web/SKILL.md")), web);

    let target = machine.root().join("new-repo");
    let error = run_on(&machine, &daemon.socket, &["init", path(&target)]).unwrap_err();
    assert!(error.contains("the sk1ll daemon manages") && error.contains("new-repo"), "{error}");
    assert!(!target.exists());
}

/// Joining without a repo is first-run setup: the config directory an
/// environment variable points at is added, and the join registers it.
#[test]
fn join_adds_a_config_directory_from_the_environment() {
    runtime().block_on(async {
        let server = Server::start().await;
        let machine = Machine::new();
        let dir = machine.home.path().join(".claude_2");
        fs::create_dir_all(&dir).unwrap();
        fs::remove_dir_all(machine.repo.root()).unwrap();
        let url = server.url();
        let join = ["join", &url, "--id", "m", "--admin-token", sk1ll_testkit::server::ADMIN];
        let out = command(&machine, &join).env("CLAUDE_CONFIG_DIR", &dir).output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        let stdout = outcome(out).unwrap();
        assert!(stderr.contains("added harness claude_2 (claude in ~/.claude_2, from CLAUDE_CONFIG_DIR)"), "{stderr}");
        assert!(stdout.contains("harnesses: claude_2\n"), "{stdout}");
        let text = fs::read_to_string(machine.machine_toml()).unwrap();
        assert!(text.ends_with("[harnesses.claude_2]\npreset = \"claude\"\nhome = \"~/.claude_2\"\n"), "{text}");
    });
}

#[test]
fn joined_machines_share_a_baseline_and_scope_limits_changes() {
    runtime().block_on(async {
        let server = Server::start().await;
        let mut home = Machine::new();
        home.harness("desk");
        join_fleet(&home, &server, "home", None);
        run(&home, &["new", "skill", "pdf"]).unwrap();
        run(&home, &["commit", "pdf", "--push"]).unwrap();

        let invite = run(&home, &["fleet", "invite"]).unwrap();
        assert!(invite.ends_with("scope fleet: the Machine may manage the whole Fleet\n"));
        let invite = run(&home, &["fleet", "invite", "--scope", "self"]).unwrap();
        let mut lines = invite.lines();
        let code = lines.next().unwrap();
        assert_eq!(lines.next().unwrap(), format!("sk1ll join {} {code}", server.url()));
        let expires = lines.next().unwrap();
        assert!(expires.starts_with("expires ") && expires.ends_with(" UTC"), "{expires}");
        assert_eq!(lines.next(), Some("scope self: the Machine may manage only itself"));

        let mut work = Machine::new();
        let work_skills = work.harness("lab");
        join_fleet(&work, &server, "work", Some(code));
        assert_eq!(server.row("work").await.unwrap().scope, sk1ll_proto::Scope::SelfOnly);
        assert_eq!(home.repo.head(), work.repo.head());
        assert!(!work_skills.join("pdf").exists(), "new harnesses start off");

        let daemon = RunningDaemon::start(&work, work.repo.root()).await;
        sk1ll_testkit::eventually(|| async {
            let report = server.row("work").await?.report?;
            (report.harnesses.iter().map(|h| h.name.as_str()).collect::<Vec<_>>() == ["lab"]).then_some(())
        })
        .await;
        let fleet = run_on(&work, &daemon.socket, &["fleet"]).unwrap();
        let mut lines = fleet.lines();
        assert_eq!(lines.next().unwrap(), format!("baseline {}", &home.repo.head()[..10]));
        for id in ["home", "work"] {
            assert!(fleet.lines().any(|line| line.starts_with(&format!("{id} "))), "{fleet}");
        }

        let refused =
            run_on(&work, &daemon.socket, &["assign", "desk", "--all", "on", "--machine", "home"]).unwrap_err();
        assert!(refused.contains("self-only join code") && refused.contains("on home itself"), "{refused}");
        for (harness, machine, expected) in [
            ("desk", "work", "work has no harness `desk`; it has lab"),
            ("lab", "gone", "no machine `gone` in the fleet"),
        ] {
            let error = run(&home, &["assign", harness, "--all", "on", "--machine", machine]).unwrap_err();
            assert!(error.contains(expected), "{error}");
        }
        let applied = run(&home, &["assign", "lab", "--all", "on", "--machine", "work"]).unwrap();
        assert!(
            applied.contains("lab: skills and rules on") && applied.contains("fleet assignment of work"),
            "{applied}"
        );
        sk1ll_testkit::eventually(|| async { work_skills.join("pdf/SKILL.md").is_file().then_some(()) }).await;

        let off = run_on(&work, &daemon.socket, &["assign", "lab", "skill", "pdf", "off"]).unwrap();
        assert!(off.contains("skill pdf off"), "{off}");
        assert!(!work_skills.join("pdf").exists());

        let filter = [
            "fleet",
            "log",
            "--machine",
            "work",
            "--harness",
            "lab",
            "--author",
            "work",
            "--item",
            "skill:pdf",
            "--limit",
            "1",
        ];
        let log = run(&home, &filter).unwrap();
        assert_eq!(log.lines().count(), 1, "{log}");
        assert!(
            log.contains(" UTC")
                && log.contains("work / lab")
                && log.contains("skill:pdf")
                && log.contains("-> off  by work"),
            "{log}"
        );
        let id = log.split_whitespace().next().unwrap();
        assert!(run(&home, &[&filter[..], &["--before", id]].concat()).unwrap().is_empty());
        assert!(run(&home, &["fleet", "undo", id]).unwrap().contains("undid Assignment log entry"));
        let undo = run(&home, &["fleet", "log", "--machine", "work", "--item", "skill:pdf", "--limit", "1"]).unwrap();
        assert!(undo.contains(&format!("(undoing entry {id})")), "{undo}");
        let again = run(&home, &["fleet", "undo", id]).unwrap();
        assert!(again.contains("previous value is already set; nothing changed"), "{again}");
        sk1ll_testkit::eventually(|| async { work_skills.join("pdf/SKILL.md").is_file().then_some(()) }).await;

        // A toggle whose Item ID appears in neither the Baseline nor any report.
        for (machine, harness) in [("home", "desk"), ("work", "lab")] {
            let response = server
                .http()
                .put(format!("{}/api/machines/{machine}/assignment", server.url()))
                .bearer_auth(sk1ll_testkit::server::ADMIN)
                .json(&json!({"changes":[
                    {"harness":harness,"kind":"skill","item_id":"stale-id","name":"old","enabled":true}
                ]}))
                .send()
                .await
                .unwrap();
            assert!(response.status().is_success(), "{}", response.text().await.unwrap());
        }
        let listed = run(&home, &["fleet", "orphans"]).unwrap();
        assert!(listed.contains("stale-id (old)") && listed.contains("2 orphan toggles"), "{listed}");
        let pruned = run(&home, &["fleet", "prune-orphans", "--machine", "work"]).unwrap();
        assert!(pruned.contains("cleared 1 orphan toggle"), "{pruned}");
        let listed = run(&home, &["fleet", "orphans"]).unwrap();
        assert!(listed.contains("home / desk") && !listed.contains("work / lab"), "{listed}");
        let prune_log = run(&home, &["fleet", "log", "--machine", "work", "--limit", "1"]).unwrap();
        assert!(prune_log.contains("(orphan toggle pruned)"), "{prune_log}");

        let unconfirmed = failure(command(&home, &["fleet", "remove", "work"]).stdin(Stdio::null()).output().unwrap());
        assert!(unconfirmed.contains("requires terminal confirmation or `--yes`"), "{unconfirmed}");
        assert!(server.row("work").await.is_some());
        assert_eq!(run(&home, &["fleet", "remove", "work", "--yes"]).unwrap(), "removed work from the fleet\n");
        assert!(server.row("work").await.is_none());
    });
}

#[test]
fn a_standalone_machine_explains_which_commands_need_a_fleet() {
    let machine = with_pdf();
    for (args, hint) in [
        (&["fleet", "remove", "box", "--yes"][..], "join a Fleet"),
        (&["assign", "h", "--all", "off", "--machine", "work"][..], "standalone"),
    ] {
        let error = failure(command(&machine, args).output().unwrap());
        assert!(error.contains(hint), "{args:?}: {error}");
    }
}

type Requests = Arc<Mutex<Vec<(String, String, Value)>>>;
type Replies = Arc<Vec<(&'static str, StatusCode, Value)>>;

/// A daemon substitute for responses that would otherwise require injecting a race or I/O failure.
struct DaemonStub {
    _runtime: tokio::runtime::Runtime,
    requests: Requests,
}

impl DaemonStub {
    fn start(socket: &Path, replies: Vec<(&'static str, StatusCode, Value)>) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let requests = Requests::default();
        let router = Router::new().fallback(respond).with_state((requests.clone(), Arc::new(replies)));
        let listener = {
            let _entered = runtime.enter();
            tokio::net::UnixListener::bind(socket).unwrap()
        };
        runtime.spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { _runtime: runtime, requests }
    }
}

async fn respond(State((requests, replies)): State<(Requests, Replies)>, request: Request) -> impl IntoResponse {
    let (parts, body) = request.into_parts();
    let path = parts.uri.to_string();
    let bytes = axum::body::to_bytes(body, 4096).await.unwrap();
    let body = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() };
    requests.lock().unwrap().push((parts.method.to_string(), path.clone(), body));
    let key = format!("{} {path}", parts.method);
    replies
        .iter()
        .find(|(name, _, _)| *name == key)
        .map(|(_, status, value)| (*status, Json(value.clone())))
        .unwrap_or((StatusCode::NOT_FOUND, Json(json!({"error": format!("unexpected {key}")}))))
}

fn status(machine: &Machine, mode: &str) -> Value {
    json!({"machine_id":"work", "server_url":"https://fleet.example", "mode":mode,
        "has_origin":true, "repo_ready":true, "repo":machine.repo.root(), "headline":"offline",
        "connection":"offline", "revoked":false, "assignment_revision":2, "report":{}, "draft_commit_complete":false})
}

#[test]
fn assignment_copy_confirms_with_both_reviewed_registrations_and_revisions() {
    let machine = Machine::new();
    for changed in [true, false] {
        let socket = machine.root().join(format!("copy-{changed}.sock"));
        let target = json!({"revision": 2, "registration": "target"});
        let source = json!({"revision": 4, "registration": "source"});
        let changes = if changed {
            json!([
                {"harness":"codex","kind":"skill","item_id":"pdf-id","name":"pdf","before":null,"after":true},
                {"harness":"codex","kind":"new_rules_default","before":true,"after":false},
            ])
        } else {
            json!([])
        };
        let daemon = DaemonStub::start(
            &socket,
            vec![
                ("GET /status", StatusCode::OK, status(&machine, "joined")),
                (
                    "POST /machines/work/assignment/copy-from/preview",
                    StatusCode::OK,
                    json!({"count": if changed { 2 } else { 0 }, "changes": changes, "target": target, "source": source}),
                ),
                (
                    "POST /machines/work/assignment/copy-from",
                    StatusCode::OK,
                    json!({"revision":5,"assignment":{"harnesses":{}},"entries":[]}),
                ),
            ],
        );
        let mut args = vec!["fleet", "copy-assignment", "home", "work", "--merge", "--map", "claude=codex"];
        if changed {
            args.push("--yes");
        }
        let out = run_on(&machine, &socket, &args).unwrap();
        let requests = daemon.requests.lock().unwrap();
        let mut body = json!({"source":"home","mode":"merge","harnesses":[{"from":"claude","to":"codex"}],
            "expected":null,"source_expected":null});
        assert_eq!(requests[1], ("POST".into(), "/machines/work/assignment/copy-from/preview".into(), body.clone()));
        if changed {
            body["expected"] = target;
            body["source_expected"] = source;
            assert_eq!(requests.len(), 3);
            assert_eq!(requests[2], ("POST".into(), "/machines/work/assignment/copy-from".into(), body));
            assert!(out.contains("2 Assignment changes on work:"), "{out}");
            assert!(out.contains("skill pdf") && out.contains("default -> on"), "{out}");
            assert!(out.contains("rules default") && out.contains("on -> off"), "{out}");
        } else {
            assert_eq!(requests.len(), 2);
            assert_eq!(out, "work already matches home's Assignment; nothing to copy\n");
        }
    }
}

#[test]
fn assign_all_reports_the_saved_revision_even_when_placement_fails() {
    let machine = with_pdf();
    for mode in ["joined", "standalone"] {
        let socket = machine.root().join(format!("{mode}.sock"));
        let daemon = DaemonStub::start(
            &socket,
            vec![
                ("GET /status", StatusCode::OK, status(&machine, mode)),
                ("GET /harnesses", StatusCode::OK, json!([{"name":"h","harness":{},"origin":"local"}])),
                (
                    "POST /assignment/harness/h",
                    StatusCode::OK,
                    json!({
                        "assignment":{"harnesses":{}},"revision":9,"entries":[],
                        "errors":["could not place pdf","could not place style"],
                    }),
                ),
            ],
        );
        let out =
            command_for(&machine, machine.repo.root(), &socket, &["assign", "h", "--all", "on"]).output().unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains("Assignment of work was saved at revision 9"));
        let stderr = failure(out);
        assert!(stderr.contains("could not place pdf") && stderr.contains("could not place style"), "{stderr}");
        assert_eq!(
            daemon.requests.lock().unwrap()[2],
            ("POST".into(), "/assignment/harness/h".into(), json!({"enabled":true}))
        );
    }
}

#[test]
fn only_a_missing_or_refused_socket_means_no_daemon() {
    let machine = with_pdf();
    let socket = machine.root().join("daemon.sock");
    let args = ["assign", "h", "skill", "pdf", "off"];
    let daemon = DaemonStub::start(&socket, vec![("GET /status", StatusCode::INTERNAL_SERVER_ERROR, json!("oops"))]);
    let error = run_on(&machine, &socket, &args).unwrap_err();
    assert!(error.contains("cannot talk to the sk1ll daemon"), "{error}");
    assert!(!machine.assignment().exists());
    drop(daemon);

    assert!(socket.exists(), "the closed listener leaves a refused socket");
    run_on(&machine, &socket, &args).unwrap();
    assert!(machine.assignment().exists());
    fs::remove_file(&socket).unwrap();
    run_on(&machine, &socket, &["assign", "h", "skill", "pdf", "on"]).unwrap();
}
