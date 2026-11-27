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

/// `init` through a daemon waiting for its repo sets up as `init` without
/// one does: the config directory an environment variable points at is
/// added only on a Machine without the machine.toml the daemon manages.
#[test]
fn init_through_a_daemon_adds_a_config_directory_only_without_a_machine_toml() {
    for configured in [true, false] {
        let machine = Machine::new();
        let dir = machine.home.path().join(".claude_2");
        fs::create_dir_all(&dir).unwrap();
        fs::remove_dir_all(machine.repo.root()).unwrap();
        if !configured {
            fs::remove_file(machine.machine_toml()).unwrap();
        }
        let before = fs::read_to_string(machine.machine_toml()).ok();
        let daemon = runtime().block_on(RunningDaemon::start(&machine, machine.repo.root()));
        let mut init = command_for(&machine, machine.repo.root(), &daemon.socket, &["init"]);
        outcome(init.env("CLAUDE_CONFIG_DIR", &dir).output().unwrap()).unwrap();
        assert!(machine.repo.root().join(".git").exists());
        let text = fs::read_to_string(machine.machine_toml()).ok();
        match configured {
            true => assert_eq!(text, before),
            false => {
                assert_eq!(text.as_deref(), Some("[harnesses.claude_2]\npreset = \"claude\"\nhome = \"~/.claude_2\"\n"))
            }
        }
    }
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

/// On a joined Machine, `harnesses add --copy-assignment-from` seeds the new
/// Harness at the Server, which takes a column the Machine hasn't reported
/// yet, so the first sync places what the other Harness has on rather than
/// nothing; the report that follows keeps that column.
#[test]
fn a_joined_machine_seeds_a_new_harness_from_another_one() {
    runtime().block_on(async {
        let server = Server::start().await;
        let mut home = Machine::new();
        home.harness("desk");
        join_fleet(&home, &server, "home", None);
        run(&home, &["new", "skill", "pdf"]).unwrap();
        run(&home, &["new", "skill", "web"]).unwrap();
        run(&home, &["commit", "--all", "-m", "skills", "--push"]).unwrap();
        run(&home, &["assign", "desk", "--all", "on"]).unwrap();
        run(&home, &["assign", "desk", "skill", "pdf", "off"]).unwrap();

        let add = ["harnesses", "add", "claude_2", "--preset", "claude", "--home", "~/.claude_2"];
        let added = run(&home, &[&add[..], &["--copy-assignment-from", "desk", "--yes"]].concat()).unwrap();
        assert!(added.contains("first claude_2 gets desk's Assignment"), "{added}");
        let skills = home.home.path().join(".claude_2/skills");
        assert!(skills.join("web/SKILL.md").is_file(), "{added}");
        assert!(!skills.join("pdf").exists());
        let assignment = |server: &Server| {
            let request = server
                .http()
                .get(format!("{}/api/machines/home/assignment", server.url()))
                .bearer_auth(sk1ll_testkit::server::ADMIN);
            async move { request.send().await.unwrap().json::<sk1ll_proto::AssignmentSnapshot>().await.unwrap() }
        };
        let seeded = assignment(&server).await;
        assert_eq!(seeded.assignment.harness("claude_2"), seeded.assignment.harness("desk"));

        // Its report finds the column there and leaves it as copied.
        let daemon = RunningDaemon::start(&home, home.repo.root()).await;
        sk1ll_testkit::eventually(|| async {
            let report = server.row("home").await?.report?;
            report.harnesses.iter().any(|h| h.name == "claude_2" && h.kind.as_deref() == Some("claude")).then_some(())
        })
        .await;
        let now = assignment(&server).await;
        assert_eq!(now.assignment.harness("claude_2"), seeded.assignment.harness("desk"));
        let log =
            run_on(&home, &daemon.socket, &["fleet", "log", "--machine", "home", "--harness", "claude_2"]).unwrap();
        assert!(!log.is_empty() && !log.contains("new harness"), "{log}");

        // Later copies go to the Server as well.
        run_on(&home, &daemon.socket, &["assign", "claude_2", "skill", "pdf", "on"]).unwrap();
        sk1ll_testkit::eventually(|| async { skills.join("pdf/SKILL.md").is_file().then_some(()) }).await;
        let copied = run_on(&home, &daemon.socket, &["assign", "claude_2", "--copy-from", "desk"]).unwrap();
        assert!(copied.contains("claude_2: skill pdf off") && copied.contains("fleet assignment of home"), "{copied}");
        sk1ll_testkit::eventually(|| async { (!skills.join("pdf").exists()).then_some(()) }).await;
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

/// `assign --copy-from --machine` checks both names in one report and
/// copies only to that registration, at the revision its preview read: a
/// rejoin in between leaves the new registration's column alone.
#[test]
fn copying_within_another_machine_is_guarded_by_its_registration_and_revision() {
    let machine = Machine::new();
    for rejoined in [false, true] {
        let socket = machine.root().join(format!("copy-within-{rejoined}.sock"));
        let reported = json!({"harnesses":[{"name":"claude","kind":"claude"},{"name":"claude_2","kind":"claude"}]});
        let row = json!({"id":"home","status":"in_sync","connected":true,"joined_at":1,"last_seen":2,
            "report":reported,"scope":"fleet","registration":"first"});
        let expected = json!({"revision": 7, "registration": if rejoined { "second" } else { "first" }});
        let daemon = DaemonStub::start(
            &socket,
            vec![
                ("GET /status", StatusCode::OK, status(&machine, "joined")),
                ("GET /fleet", StatusCode::OK, json!({"baseline":null,"machines":[row]})),
                (
                    "POST /machines/home/assignment/copy-from/preview",
                    StatusCode::OK,
                    json!({"count":1,"changes":[],"target":expected,"source":expected}),
                ),
                (
                    "POST /machines/home/assignment/copy-from",
                    StatusCode::OK,
                    json!({"revision":8,"assignment":{"harnesses":{}},"entries":[]}),
                ),
            ],
        );
        let out = run_on(&machine, &socket, &["assign", "claude_2", "--copy-from", "claude", "--machine", "home"]);
        let requests = daemon.requests.lock().unwrap();
        assert_eq!(requests.iter().filter(|(_, path, _)| path == "/fleet").count(), 1, "one report for both names");
        let copies: Vec<_> =
            requests.iter().filter(|(_, path, _)| path == "/machines/home/assignment/copy-from").collect();
        if rejoined {
            assert!(out.unwrap_err().contains("home joined the fleet again just now"));
            assert!(copies.is_empty());
        } else {
            out.unwrap();
            let body = json!({"source":"home","mode":"replace","harnesses":[{"from":"claude","to":"claude_2"}],
                "expected":expected,"source_expected":expected});
            assert_eq!(copies, [&("POST".into(), "/machines/home/assignment/copy-from".into(), body)]);
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
