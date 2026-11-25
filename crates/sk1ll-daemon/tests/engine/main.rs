//! In-process local API tests. Each runtime owns its core paths and git environment.
#![cfg(feature = "failpoints")]

mod races;

use std::path::PathBuf;
use std::time::Duration;

use sk1ll_core::failpoints::{self, Guard};
use sk1ll_daemon::{Config, local::Engine};
use sk1ll_testkit::{Harness, InstructionsFormat, Machine, git, read, write};

fn fixture() -> (Machine, Config) {
    let mut machine = Machine::new();
    let root = machine.root().to_path_buf();
    machine.add_harness(
        Harness::copy("agent", root.join("skills")).instructions(root.join("rules"), InstructionsFormat::Rules),
    );
    let config = Config {
        repo: Some(machine.repo.root().into()),
        state: machine.daemon_state(),
        machine_config: machine.machine_toml(),
        assignment: machine.assignment(),
        state_dir: machine.home.state_dir(),
        cache_dir: machine.home.cache_dir(),
        socket: machine.no_daemon(),
        ..Config::new().unwrap()
    };
    (machine, config)
}

fn runtime(dirs: sk1ll_core::paths::Dirs) -> tokio::runtime::Runtime {
    thread_local! {
        static ISOLATION: std::cell::OnceCell<(Guard, Guard)> = const { std::cell::OnceCell::new() };
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .on_thread_start(move || {
            ISOLATION.with(|isolation| {
                isolation.get_or_init(|| (failpoints::dirs(dirs.clone()), failpoints::git_env(git::ENV)));
            });
        })
        .build()
        .unwrap()
}

fn writer(config: &Config) -> Engine {
    Engine::for_tests(config.clone(), true, Duration::ZERO).unwrap()
}

fn placed(machine: &Machine, name: &str) -> PathBuf {
    machine.root().join("skills").join(name).join("SKILL.md")
}

#[test]
fn inventory_reads_the_applied_baseline_and_leaves_the_index_alone() {
    use sk1ll_daemon::client::ChangeKind;
    use sk1ll_testkit::Skill;

    let (m, cfg) = fixture();
    let _home = m.home.enter();
    let repo = &m.repo;
    repo.skill(Skill::new("pdf").id("first").description(Some("Baseline")));
    repo.skill(Skill::new("my skill").id("spaced").description(Some("Spaced")).body("\0\nbody"));
    repo.skill(Skill::new("huge").id("huge").description(Some("Too big")).body(&"x".repeat(17 * 1024)));
    let baseline = repo.commit_all("baseline");
    repo.git(&["update-ref", "refs/remotes/origin/main", &baseline]);
    repo.git(&["branch", "remote"]);
    repo.git(&["switch", "remote"]);
    repo.write("instructions/remote.md", "A fetched rule");
    let remote = repo.commit_all("remote");
    repo.git(&["update-ref", "refs/remotes/origin/main", &remote]);
    repo.git(&["switch", "main"]);
    repo.skill(Skill::new("queued").id("queued").description(Some("Queued")));
    let head = repo.commit_all("local commit");
    repo.skill(Skill::new("pdf").id("second").description(Some("Draft")));
    let index = repo.root().join(".git/index");
    let before = std::fs::read(&index).unwrap();

    runtime(m.home.dirs()).block_on(async {
        let engine = Engine::for_tests(cfg, false, Duration::ZERO).unwrap();
        let items = engine.client().items().await.unwrap();
        let with_id =
            |id: &str| items.iter().find(|entry| entry.id.as_ref().is_some_and(|i| i.to_string() == id)).unwrap();
        let old = with_id("first");
        assert!(old.in_baseline);
        assert_eq!(old.draft, Some(ChangeKind::Deleted));
        let current = with_id("second");
        assert!(!current.in_baseline);
        assert_eq!(current.draft, Some(ChangeKind::Added));
        assert_eq!(current.description.as_deref(), Some("Draft"));
        assert_eq!(with_id("spaced").description.as_deref(), Some("Spaced"));
        assert_eq!(with_id("huge").description, None);
        assert!(with_id("queued").unpushed);
        assert!(!with_id("queued").in_baseline);
        assert!(!items.iter().any(|entry| entry.item.to_string() == "rule:remote"));

        repo.write("sk1ll.toml", "[import.vendored]\nid = 42\n");
        let error = engine.client().items().await.unwrap_err();
        assert!(error.message.contains("sk1ll.toml"), "{error}");
    });
    assert_eq!(std::fs::read(index).unwrap(), before);
    assert_eq!(repo.head(), head);
}

#[test]
fn previews_leave_a_pending_rename_for_the_writer_even_through_a_second_config_link() {
    use sk1ll_core::Assignment;
    use sk1ll_core::harness_rename::{RenameIntent, RenameRequest};
    use std::os::unix::fs::symlink;

    let (m, mut cfg) = fixture();
    let _home = m.home.enter();
    let config = cfg.machine_config.clone();
    let a = m.root().join("a.toml");
    let b = m.root().join("b.toml");
    symlink(&config, &a).unwrap();
    symlink("machine.toml", &b).unwrap();
    cfg.machine_config = b.clone();
    let mut assignment = Assignment::default();
    assignment.set_harness("agent", false);
    assignment.save(&cfg.assignment).unwrap();
    let request = RenameRequest { from: "agent".into(), to: "renamed".into(), replace: false };
    let intent = RenameIntent::prepare(&cfg.state_dir, &a, m.repo.root(), request, &assignment, None).unwrap();
    intent.apply_config(&a).unwrap();
    let before = (read(&config), read(&cfg.assignment));
    let skill = m.repo.write("personal/new/SKILL.md", "New.");
    let id = skill.with_file_name(".sk1ll-id");

    runtime(m.home.dirs()).block_on(async {
        let reader = Engine::for_tests(cfg.clone(), false, Duration::ZERO).unwrap();
        let c = reader.client();
        assert!(c.assignment("test").await.unwrap().assignment.harnesses.contains_key("agent"));
        let content = c.repo(false).await.unwrap();
        assert!(content.skills.iter().any(|s| s.name == "new" && s.id.is_none()));
        let plan = c.preview_sync().await.unwrap();
        assert!(!plan.actions.unwrap().iter().any(|a| a.name == "new"), "preview sees the renamed harness's toggles");
        c.draft().await.unwrap();
        c.harnesses().await.unwrap();
        c.harness_settings().await.unwrap();
        c.render_instructions("renamed").await.unwrap();
        c.diff("skill:new").await.unwrap();
        c.editable("new").await.unwrap();
        assert_eq!((read(&config), read(&cfg.assignment)), before);
        assert!(RenameIntent::exists(&cfg.state_dir, &b).unwrap());
        assert!(!id.exists());

        let writer = writer(&cfg);
        writer.client().repo(true).await.unwrap();
        assert!(id.exists());
        assert!(!RenameIntent::exists(&cfg.state_dir, &a).unwrap());
        let saved = Assignment::load(&cfg.assignment).unwrap();
        assert!(!saved.harnesses.contains_key("agent"));
        assert!(!saved.harness("renamed").new_skills_default);
        assert!(!placed(&m, "new").exists());
    });
}

#[test]
fn reconcile_publishes_and_dismisses_the_note_from_an_interrupted_keep() {
    let (m, cfg) = fixture();
    let _home = m.home.enter();
    let dest = m.repo.write("personal/pdf/SKILL.md", "my draft");
    let journal = m.repo.write(
        ".git/sk1ll-keep.json",
        &serde_json::json!({
            "phase": "pulling",
            "kept": [{ "item": "skill:pdf", "dest": format!("\"{}\"", dest.display()), "entry": null }],
            "staged": []
        })
        .to_string(),
    );
    runtime(m.home.dirs()).block_on(async {
        let engine = writer(&cfg);
        let c = engine.client();
        for _ in 0..2 {
            c.draft_fresh().await.unwrap();
            assert!(!journal.exists());
            assert_eq!(read(&dest), "my draft");
            let report = c.status().await.unwrap().keep_report;
            assert_eq!(report.len(), 1);
            assert_eq!(report[0].item.to_string(), "skill:pdf");
            assert!(report[0].entry.is_none());
        }
        assert_eq!(c.dismiss_keep_notes().await.unwrap(), 1);
        assert!(c.status().await.unwrap().keep_report.is_empty());
    });
}

#[test]
fn a_restart_clears_a_finished_pull_and_keeps_a_newer_baseline_pending() {
    use sk1ll_daemon::DaemonState;
    use sk1ll_daemon::state::{PullRecord, PullState};

    let (m, cfg) = fixture();
    let _home = m.home.enter();
    let repo = &m.repo;
    let head = repo.head();
    let future = repo.git(&["commit-tree", "HEAD^{tree}", "-p", &head, "-m", "next baseline"]);
    repo.git(&["update-ref", "refs/remotes/origin/main", &future]);
    let state = DaemonState::new("test".into(), "http://127.0.0.1:1".into(), "token".into(), repo.root().into());
    runtime(m.home.dirs()).block_on(async {
        for (baseline, blocked) in [(&head, false), (&future, true)] {
            let saved = DaemonState {
                record: PullRecord {
                    server_baseline: Some(future.clone()),
                    pull: PullState::Pending { baseline: baseline.clone(), items: vec!["skill:pdf".parse().unwrap()] },
                    push_refused: None,
                },
                ..state.clone()
            };
            saved.save(&cfg.state).unwrap();
            let engine = writer(&cfg);
            engine.client().draft_fresh().await.unwrap();
            let status = engine.client().status().await.unwrap();
            assert!(status.report.behind);
            assert_eq!(status.report.blocked, blocked, "{status:?}");
            let pull = DaemonState::load(&cfg.state).unwrap().unwrap().record.pull;
            if blocked {
                assert!(matches!(pull, PullState::Blocked { baseline, .. } if baseline == future));
            } else {
                assert_eq!(pull, PullState::Clear);
            }
        }
        let mut old = serde_json::to_value(state).unwrap();
        old["pull"] = serde_json::json!({ "state": "pending", "items": ["skill:pdf"] });
        write(&cfg.state, &old.to_string());
        let engine = writer(&cfg);
        engine.client().draft_fresh().await.unwrap();
        assert!(!engine.client().status().await.unwrap().report.blocked);
    });
}

#[test]
fn an_on_demand_engine_reports_its_inventory_and_protects_server_only_assignment() {
    use sk1ll_daemon::client::HarnessRenameRequest;
    use sk1ll_daemon::{DaemonState, JoinOptions, join, state};
    use sk1ll_proto::{AssignmentSnapshot, AssignmentUpdate};
    use sk1ll_testkit::server::{ADMIN, Server};

    let (m, cfg) = fixture();
    let _home = m.home.enter();
    let repo = m.repo.root();
    std::fs::remove_dir_all(repo).unwrap();
    runtime(m.home.dirs()).block_on(async {
        let server = Server::start().await;
        join(&cfg, JoinOptions {
            server_url: server.url(),
            code: server.join_code().await,
            admin_token: None,
            machine_id: Some("test".into()),
            copy_from: None,
            repo: repo.into(),
        })
        .await
        .unwrap();
        m.repo.skill(sk1ll_testkit::Skill::new("pdf").id("pdf"));
        let engine = writer(&cfg);
        let c = engine.client();
        assert!(!server.row("test").await.unwrap().connected);
        let confirmed = c.set_harness_assignment("agent", false).await.unwrap();
        let report = server.row("test").await.unwrap().report.unwrap();
        assert!(report.items.iter().any(|item| item.id.to_string() == "pdf"));
        let owner = DaemonState::load(&cfg.state).unwrap().unwrap().registration_key();
        let stored = state::load_snapshot(&cfg.snapshot_path(), Some(&owner)).unwrap().unwrap();
        assert_eq!((stored.revision, stored.assignment), (confirmed.revision, confirmed.assignment));

        let updated: AssignmentUpdate = server.http().put(format!("{}/api/machines/test/assignment", server.url()))
            .bearer_auth(ADMIN)
            .json(&serde_json::json!({"changes":[{"harness":"occupied","kind":"skill","item_id":"absent","enabled":true}]}))
            .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
        let before = read(&cfg.machine_config);
        let req = HarnessRenameRequest { from: "agent".into(), to: "occupied".into(), replace: false };
        let error = c.rename_harness(&req).await.unwrap_err();
        assert!(error.message.contains("still has an Assignment"), "{error}");
        assert_eq!(read(&cfg.machine_config), before);
        let remote: AssignmentSnapshot = server.get("/machines/test/assignment").await;
        assert_eq!((remote.revision, remote.assignment), (updated.revision, updated.assignment));
    });
}

#[test]
fn standalone_assignment_tracks_item_ids_through_adopt_and_recreate() {
    use sk1ll_core::Assignment;
    use sk1ll_daemon::client::{ConflictTarget, CreateItemRequest, DeleteItemRequest};
    use sk1ll_proto::{AssignmentChanges, ToggleChange, ToggleKind};
    use sk1ll_testkit::Skill;

    let (m, cfg) = fixture();
    let _home = m.home.enter();
    m.repo.skill(Skill::new("old name").id("spaced"));
    let source = placed(&m, "pdf");
    write(&source, "unmanaged");
    runtime(m.home.dirs()).block_on(async {
        let engine = writer(&cfg);
        let c = engine.client();
        let assignment = c.set_harness_assignment("agent", false).await.unwrap();
        assert!(assignment.errors.is_empty());
        assert!(!assignment.assignment.skill_enabled("agent", &"spaced".parse().unwrap()));
        c.adopt_unmanaged(&ConflictTarget { harness: "agent".into(), name: "pdf".into() }).await.unwrap();
        assert_eq!(m.repo.read("personal/pdf/SKILL.md"), "unmanaged");
        assert_eq!(read(&source), "unmanaged");
        let id = || m.repo.open().skills().unwrap().into_iter().find(|s| s.name == "pdf").unwrap().id.unwrap();
        let old = id();
        assert!(Assignment::load(&cfg.assignment).unwrap().skill_enabled("agent", &old));
        let index = source.ancestors().nth(2).unwrap().join(sk1ll_core::sync::INDEX_PATH);
        let entry: serde_json::Value = serde_json::from_str(&read(&index)).unwrap();
        assert_eq!(entry["entries"]["pdf"]["kind"], "copy");
        assert_eq!(entry["entries"]["pdf"]["token"], read(&source.with_file_name(sk1ll_core::sync::PLACEMENT_MARKER)));
        assert!(c.draft().await.unwrap().iter().any(|entry| entry.item.to_string() == "skill:pdf"));

        // With the harness back to its default, an explicit off applies to the old ID.
        c.set_harness_assignment("agent", true).await.unwrap();
        c.set_assignment(
            "test",
            &AssignmentChanges {
                changes: vec![ToggleChange {
                    harness: "agent".into(),
                    kind: ToggleKind::Skill,
                    item_id: Some(old.clone()),
                    name: Some("pdf".into()),
                    enabled: Some(false),
                }],
                expected: None,
            },
        )
        .await
        .unwrap();
        assert!(!source.exists());
        c.delete_item(&DeleteItemRequest { kind: "skill".into(), name: "pdf".into() }).await.unwrap();
        c.create_item(&CreateItemRequest {
            kind: "skill".into(),
            name: "pdf".into(),
            description: None,
            bundled: None,
        })
        .await
        .unwrap();
        assert_ne!(id(), old);
        assert!(source.exists());
        assert!(!c.assignment("test").await.unwrap().assignment.skill_enabled("agent", &old));
    });
}

#[test]
fn a_bundled_skill_is_created_as_a_personal_draft_skill() {
    use sk1ll_daemon::client::CreateItemRequest;
    let (m, cfg) = fixture();
    let _home = m.home.enter();
    runtime(m.home.dirs()).block_on(async {
        let engine = writer(&cfg);
        let c = engine.client();
        let bundled = |kind: &str, name: &str, bundled: &str| CreateItemRequest {
            kind: kind.into(),
            name: name.into(),
            description: None,
            bundled: Some(bundled.into()),
        };
        // The kit seeds built-in items. Remove this skill to request a fresh copy.
        let skill = m.repo.root().join("personal/sk1ll");
        if skill.exists() {
            std::fs::remove_dir_all(skill).unwrap();
        }
        c.create_item(&bundled("skill", "sk1ll", "sk1ll")).await.unwrap();
        assert_eq!(Some(m.repo.read("personal/sk1ll/SKILL.md").as_str()), sk1ll_core::bundled::skill("sk1ll"));
        for (kind, name, from) in [("skill", "other", "pdf"), ("rule", "rule", "sk1ll"), ("skill", "sk1ll", "sk1ll")] {
            let err = c.create_item(&bundled(kind, name, from)).await.unwrap_err();
            assert_eq!(err.status, Some(400), "{kind} {name} from {from}: {err:?}");
        }
    });
}

#[test]
fn reviewed_sync_replans_after_recovering_an_interrupted_import() {
    let (m, cfg) = fixture();
    let _home = m.home.enter();
    m.repo.write("imported/pdf/SKILL.md", "pdf");
    runtime(m.home.dirs()).block_on(async {
        let engine = writer(&cfg);
        let c = engine.client();
        c.items_fresh().await.unwrap();
        assert_eq!(read(&placed(&m, "pdf")), "pdf");
        let imported = m.repo.root().join("imported");
        std::fs::rename(imported.join("pdf"), imported.join(".sk1ll-stage-old-pdf")).unwrap();
        let preview = c.preview_sync().await.unwrap();
        assert!(preview.actions.unwrap().iter().any(|action| action.kind == "remove" && action.name == "pdf"));
        let fresh = c.apply_sync_preview(preview.fingerprint.unwrap()).await.unwrap();
        assert!(fresh.fingerprint.is_some());
        assert!(fresh.actions.unwrap().is_empty());
        assert_eq!(read(&imported.join("pdf/SKILL.md")), "pdf");
        assert_eq!(read(&placed(&m, "pdf")), "pdf");
    });
}

#[test]
fn a_starting_daemon_waits_for_an_on_demand_engine_and_loads_its_revision() {
    use sk1ll_daemon::client::Client;
    use sk1ll_testkit::eventually;

    let (m, cfg) = fixture();
    let _home = m.home.enter();
    m.repo.skill(sk1ll_testkit::Skill::new("pdf").id("pdf"));
    runtime(m.home.dirs()).block_on(async {
        let engine = writer(&cfg);
        let command = engine.client();
        let base = command.status().await.unwrap().assignment_revision;
        let mut launch = m.home.command(env!("CARGO_BIN_EXE_sk1ll-daemon"));
        launch
            .env("SK1LL_REPO", m.repo.root())
            .env("SK1LL_DAEMON_STATE", &cfg.state)
            .env("SK1LL_MACHINE_CONFIG", &cfg.machine_config)
            .env("SK1LL_ASSIGNMENT", &cfg.assignment)
            .env("SK1LL_DAEMON_SOCKET", m.home.socket())
            .env("TOKIO_WORKER_THREADS", "2")
            .env("SK1LL_NOTIFICATIONS", "0");
        let mut process = tokio::process::Command::from(launch).kill_on_drop(true).spawn().unwrap();
        let client = Client::new(m.home.socket());
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(process.try_wait().unwrap().is_none());
        assert!(client.status().await.unwrap_err().is_not_running());
        let updated = command.set_harness_assignment("agent", false).await.unwrap();
        assert_eq!(updated.revision, base + 1);
        drop(engine);
        let ready = eventually(|| async { client.status().await.ok() }).await;
        assert_eq!(ready.assignment_revision, updated.revision);
        assert!(!client.assignment("test").await.unwrap().assignment.harness("agent").new_skills_default);
        assert!(!placed(&m, "pdf").exists());
        process.kill().await.unwrap();
    });
}

#[test]
fn reconcile_follows_symlinked_config_and_distinguishes_skill_content_from_staging() {
    use sk1ll_core::failpoints::{Action, Point};
    use sk1ll_testkit::{MachineToml, Skill};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    let (m, cfg) = fixture();
    let _home = m.home.enter();
    let config = m.root().join("dotfiles/machine.toml");
    write(&config, &read(&cfg.machine_config));
    std::fs::remove_file(&cfg.machine_config).unwrap();
    std::os::unix::fs::symlink(&config, &cfg.machine_config).unwrap();
    let rel = "refs/.sk1ll-stage-notes.md";
    m.repo.skill(Skill::new("pdf").id("pdf").file(rel, "notes"));
    let installed = m.root().join("skills/pdf").join(rel);
    let counts = Arc::new(AtomicUsize::new(0));
    let placement = m.root().join("skills/pdf");
    let _plans = failpoints::install_shared({
        let counts = counts.clone();
        move |point, path| {
            if point == Point::PlanSource && path == placement {
                counts.fetch_add(1, Ordering::Relaxed);
            }
            Action::Continue
        }
    });

    runtime(m.home.dirs()).block_on(async {
        let engine = writer(&cfg);
        let c = engine.client();
        c.items_fresh().await.unwrap();
        c.items_fresh().await.unwrap();
        let settled = counts.load(Ordering::Relaxed);
        assert!(settled > 0);
        m.repo.write("imported/.sk1ll-stage-1-2-3/tree/SKILL.md", "being staged");
        c.items_fresh().await.unwrap();
        assert_eq!(counts.load(Ordering::Relaxed), settled);

        m.repo.write(&format!("personal/pdf/{rel}"), "edited notes");
        c.items_fresh().await.unwrap();
        assert_eq!(read(&installed), "edited notes");
        assert!(counts.load(Ordering::Relaxed) > settled);

        let other = m.root().join("other-skills");
        MachineToml::new().id("test").harness(Harness::copy("agent", &other)).write(&config);
        c.items_fresh().await.unwrap();
        assert_eq!(read(&other.join("pdf").join(rel)), "edited notes");
        assert!(cfg.machine_config.is_symlink());
    });
}

#[test]
fn a_full_reconcile_rehashes_the_same_files_after_the_interval() {
    use sk1ll_core::failpoints::{Action, Point};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    let (m, mut cfg) = fixture();
    let _home = m.home.enter();
    // Long enough for the first reads to finish inside it on a loaded machine.
    cfg.full_reconcile_every = Duration::from_secs(3);
    let skill = m.repo.skill(sk1ll_testkit::Skill::new("pdf").id("pdf"));
    let source = skill.join("SKILL.md");
    let hashes = Arc::new(AtomicUsize::new(0));
    let _reads = failpoints::install_shared({
        let hashes = hashes.clone();
        move |point, path| {
            if point == Point::HashMidRead && path == source {
                hashes.fetch_add(1, Ordering::Relaxed);
            }
            Action::Continue
        }
    });
    runtime(m.home.dirs()).block_on(async {
        let started = std::time::Instant::now();
        let engine = writer(&cfg);
        let c = engine.client();
        c.items_fresh().await.unwrap();
        c.items_fresh().await.unwrap();
        let settled = hashes.load(Ordering::Relaxed);
        assert!(settled > 0);
        c.items_fresh().await.unwrap();
        assert_eq!(hashes.load(Ordering::Relaxed), settled, "rehashed {:?} after the start", started.elapsed());

        tokio::time::sleep(cfg.full_reconcile_every).await;
        c.items_fresh().await.unwrap();
        assert!(hashes.load(Ordering::Relaxed) > settled, "the full reconcile bypasses the hash cache");
    });
}

/// A commit is noticed through a `.git` pointer file, though the tree is
/// unchanged: in a linked worktree (whose branch is in the common git dir)
/// and in a repo whose git dir is elsewhere.
#[test]
fn a_commit_behind_a_git_pointer_is_noticed_without_a_change_to_the_tree() {
    use sk1ll_daemon::client::DraftEntry;

    for layout in ["linked worktree", "separate git dir"] {
        let (m, mut cfg) = fixture();
        let _home = m.home.enter();
        let repo = if layout == "linked worktree" {
            let tree = m.root().join("worktree");
            m.repo.git(&["worktree", "add", "-q", "-b", "elsewhere", tree.to_str().unwrap()]);
            tree
        } else {
            let separate = m.root().join("separate.git");
            std::fs::rename(m.repo.root().join(".git"), &separate).unwrap();
            write(&m.repo.root().join(".git"), &format!("gitdir: {}\n", separate.display()));
            m.repo.root().to_path_buf()
        };
        cfg.repo = Some(repo.clone());
        sk1ll_testkit::Skill::new("notes").id("notes").write(&repo.join("personal"));
        runtime(m.home.dirs()).block_on(async {
            let engine = writer(&cfg);
            let c = engine.client();
            let drafted = |draft: Vec<DraftEntry>| draft.iter().any(|entry| entry.item.to_string() == "skill:notes");
            for _ in 0..2 {
                assert!(drafted(c.draft_fresh().await.unwrap()), "{layout}");
            }
            git::git(&repo, &["add", "-A"]);
            git::git(&repo, &["commit", "-qm", "notes"]);
            assert!(!drafted(c.draft_fresh().await.unwrap()), "{layout}: the commit went unnoticed");
        });
    }
}

/// A reconcile that saw an Input change within the racy window records no
/// fingerprint, so the next one runs again instead of trusting it; once the
/// window has passed, an unchanged reconcile is skipped.
#[test]
fn a_harness_edit_within_the_racy_window_is_reconciled_again() {
    use sk1ll_core::failpoints::{Action, Point};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    // Wide enough for both reads after the edit to fall inside it on a
    // loaded machine.
    let window = Duration::from_secs(1);
    let (m, cfg) = fixture();
    let _home = m.home.enter();
    m.repo.skill(sk1ll_testkit::Skill::new("pdf").id("pdf"));
    let plans = Arc::new(AtomicUsize::new(0));
    let placement = m.root().join("skills/pdf");
    let _plans = failpoints::install_shared({
        let (plans, placement) = (plans.clone(), placement.clone());
        move |point, path| {
            if point == Point::PlanSource && path == placement {
                plans.fetch_add(1, Ordering::Relaxed);
            }
            Action::Continue
        }
    });
    runtime(m.home.dirs()).block_on(async {
        let engine = Engine::for_tests(cfg, true, window).unwrap();
        let c = engine.client();
        let planned = || plans.load(Ordering::Relaxed);
        let settle = || async {
            tokio::time::sleep(window + Duration::from_millis(200)).await;
            c.items_fresh().await.unwrap();
            let settled = planned();
            c.items_fresh().await.unwrap();
            assert_eq!(planned(), settled, "a settled reconcile is skipped");
        };
        c.items_fresh().await.unwrap();
        settle().await;

        write(&placement.join("SKILL.md"), "Edited in the harness.");
        for _ in 0..2 {
            let before = planned();
            c.items_fresh().await.unwrap();
            assert!(planned() > before, "an edit within the window is reconciled again");
        }
        settle().await;
    });
}
