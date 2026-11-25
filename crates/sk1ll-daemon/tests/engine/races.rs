use std::sync::mpsc;
use std::time::Duration;

use sk1ll_core::failpoints::{self, Action, Point};
use sk1ll_core::{Assignment, HarnessChange, MachineConfig};
use sk1ll_daemon::client::{ConflictDiffRequest, HarnessChangeRequest, SyncRequest};
use sk1ll_daemon::{Config, state};
use sk1ll_testkit::{Machine, MachineToml, Skill, child, read, write};

use super::{fixture, placed, runtime, writer};

#[test]
fn reviewed_sync_reports_partial_progress_and_stops_on_digest_mismatch() {
    let (m, cfg) = fixture();
    let _home = m.home.enter();
    for skill in ["a-success", "b-io-error", "pdf", "z-later"] {
        m.repo.skill(Skill::new(skill).id(skill).description(None).body(skill));
    }
    let source = m.repo.root().join("personal/pdf/SKILL.md");
    let missing = m.repo.root().join("personal/b-io-error");

    runtime(m.home.dirs()).block_on(async {
        let engine = writer(&cfg);
        let c = engine.client();
        let reviewed = c.preview_sync().await.unwrap().fingerprint.unwrap();
        let hook = failpoints::once_shared(Point::DaemonAfterSyncReview, &cfg.machine_config, move |_| {
            std::fs::remove_dir_all(missing).unwrap();
            write(&source, "changed after review");
        });
        let returned = c.apply_sync_preview(reviewed.clone()).await.unwrap();
        drop(hook);

        let fresh = returned.fingerprint.unwrap();
        assert_ne!(fresh, reviewed);
        let remaining: Vec<_> = returned.actions.unwrap().into_iter().map(|action| action.name).collect();
        assert_eq!(remaining, ["pdf", "z-later"]);
        assert_eq!(returned.completed.len(), 1);
        assert_eq!(returned.completed[0].name, "a-success");
        assert_eq!(returned.changed.unwrap().name, "pdf");
        assert_eq!(returned.errors.len(), 1);
        assert!(returned.errors[0].contains("b-io-error"));
        assert_eq!(c.status().await.unwrap().last_error.as_ref(), returned.errors.first());
        assert!(placed(&m, "a-success").exists());
        assert!(!placed(&m, "z-later").exists());
        assert!(!placed(&m, "pdf").exists());
        assert_eq!(c.preview_sync().await.unwrap().fingerprint.as_deref(), Some(fresh.as_str()));
        let error = c.sync(&SyncRequest { fingerprint: Some(reviewed), ..Default::default() }).await.unwrap_err();
        assert_eq!(error.status, Some(409));
        assert!(c.apply_sync_preview(fresh).await.unwrap().errors.is_empty());
        assert_eq!(read(&placed(&m, "pdf")), "changed after review");
    });
}

#[test]
fn readers_see_the_published_snapshot_while_fresh_reads_wait_for_reconcile() {
    let (m, cfg) = fixture();
    let _home = m.home.enter();
    m.repo.write("personal/pdf/SKILL.md", "PDFs.");
    runtime(m.home.dirs()).block_on(async {
        let engine = writer(&cfg);
        let c = engine.client();
        assert!(c.items_fresh().await.unwrap().iter().any(|item| item.item.to_string() == "skill:pdf"));

        let (started, busy) = tokio::sync::oneshot::channel();
        let (finish, blocked) = mpsc::channel::<()>();
        let _hook = failpoints::once_shared(Point::BeforeCopy, &m.repo.root().join("personal/notes"), move |_| {
            started.send(()).unwrap();
            blocked.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        // A new item starts a reconcile. While it plans, add a harness edit.
        m.repo.write("personal/notes/SKILL.md", "Notes.");
        let reconcile = tokio::spawn({
            let c = c.clone();
            async move { c.draft_fresh().await }
        });
        tokio::time::timeout(Duration::from_secs(5), busy).await.unwrap().unwrap();
        write(&placed(&m, "pdf"), "Harness edit");
        let names = |draft: Vec<sk1ll_daemon::client::DraftEntry>| {
            draft.into_iter().map(|entry| entry.item.to_string()).collect::<Vec<_>>()
        };
        tokio::time::timeout(Duration::from_secs(3), async {
            assert_eq!(names(c.draft().await.unwrap()), ["skill:pdf"]);
            assert!(!c.items().await.unwrap().iter().any(|item| item.item.to_string() == "skill:notes"));
            assert!(c.conflicts().await.unwrap().is_empty());
            assert!(c.recovered().await.unwrap().is_empty());
            assert_eq!(c.status().await.unwrap().report.drafts.len(), 1);
        })
        .await
        .expect("a read waited for the engine");

        let diff = ConflictDiffRequest { harness: Some("agent".into()), name: Some("pdf".into()), id: None };
        let mut waiting = std::pin::pin!(async { tokio::join!(c.draft_fresh(), c.conflict_diff(&diff)) });
        tokio::select! {
            biased;
            _ = &mut waiting => panic!("fresh reads returned while reconcile held the engine"),
            _ = std::future::ready(()) => {}
        }
        finish.send(()).unwrap();
        reconcile.await.unwrap().unwrap();
        let (draft, diff) = waiting.await;
        assert_eq!(names(draft.unwrap()), ["skill:notes", "skill:pdf"]);
        let diff = diff.unwrap().diff;
        assert!(diff.contains("-Harness edit") && diff.contains("+PDFs."), "{diff}");
    });
}

#[test]
fn assignment_store_retries_on_a_newer_hand_edit_and_keeps_the_record_in_step() {
    use std::os::unix::fs::MetadataExt;

    for operation in ["startup", "refresh", "toggle"] {
        let (m, cfg) = fixture();
        let _home = m.home.enter();
        let initial = Assignment::default();
        initial.save(&cfg.assignment).unwrap();
        let record = cfg.standalone_assignment_path();
        runtime(m.home.dirs()).block_on(async {
            let first = writer(&cfg);
            let base = first.client().status().await.unwrap().assignment_revision;
            drop(first);
            let stamp = |path: &std::path::Path| {
                std::fs::metadata(path).map(|meta| (meta.ino(), meta.mtime_nsec(), meta.len())).ok()
            };
            let before = (stamp(&cfg.assignment), stamp(&record));
            let mut engine = Some(writer(&cfg));
            assert_eq!((stamp(&cfg.assignment), stamp(&record)), before, "startup without edits");
            if operation == "startup" {
                drop(engine.take());
            }
            if operation != "toggle" {
                let mut hand = initial;
                hand.set_harness("agent", false);
                hand.save(&cfg.assignment).unwrap();
            }
            let _hook = failpoints::once_shared(Point::DaemonStoreBeforeSwap, &cfg.assignment, |file| {
                let mut hand = Assignment::load(file).unwrap();
                hand.set_harness("other", false);
                hand.save(file).unwrap();
            });
            let engine = engine.unwrap_or_else(|| writer(&cfg));
            let c = engine.client();
            if operation == "toggle" {
                c.set_harness_assignment("agent", false).await.unwrap();
            } else {
                c.draft_fresh().await.unwrap();
            }
            let saved = Assignment::load(&cfg.assignment).unwrap();
            assert!(!saved.harness("agent").new_skills_default, "{operation}");
            assert!(!saved.harness("other").new_skills_default, "{operation}");
            let applied = c.status().await.unwrap().assignment_revision;
            assert_eq!(applied, base + if operation == "toggle" { 2 } else { 1 }, "{operation}");
            let snapshot = state::load_snapshot(&record, None).unwrap().unwrap();
            assert_eq!((snapshot.revision, snapshot.assignment), (applied, saved), "{operation}");
        });
    }
}

async fn take_out_request(c: &sk1ll_daemon::client::Client) -> HarnessChangeRequest {
    c.items_fresh().await.unwrap();
    let change = HarnessChange::Disable { name: "agent".into() };
    let fingerprint = Some(
        c.preview_harness(&HarnessChangeRequest { change: change.clone(), fingerprint: None })
            .await
            .unwrap()
            .fingerprint,
    );
    HarnessChangeRequest { change, fingerprint }
}

#[test]
fn take_out_only_disables_a_harness_after_a_complete_scan_and_preserves_concurrent_edits() {
    for scenario in
        ["stale preview", "first scan", "second scan", "config edit", "removed by hand", "disabled by hand", "clean"]
    {
        let (m, cfg) = fixture();
        let _home = m.home.enter();
        m.repo.skill(Skill::new("pdf").id("pdf"));
        runtime(m.home.dirs()).block_on(async {
            let engine = writer(&cfg);
            let c = engine.client();
            let mut req = take_out_request(c).await;
            let before = read(&cfg.machine_config);
            let index = m.root().join("skills").join(sk1ll_core::sync::INDEX_PATH);
            if scenario == "stale preview" {
                req.fingerprint = Some("stale".into());
                let err = c.take_out_harness(&req).await.unwrap_err();
                assert_eq!(err.status, Some(409));
                assert!(placed(&m, "pdf").exists());
                assert!(c.assignment("test").await.unwrap().assignment.harness("agent").new_skills_default);
                return;
            }
            if scenario == "first scan" {
                write(&index, "corrupt");
            }
            let point = if scenario == "second scan" {
                Point::DaemonBeforeTakeOutWrite
            } else {
                Point::DaemonBeforeTakeOutApply
            };
            let _hook = failpoints::once_shared(point, &cfg.machine_config, move |path| match scenario {
                "second scan" => write(&index, "corrupt"),
                "config edit" => write(path, &format!("{}\n# edited by hand\n", read(path))),
                "removed by hand" => MachineToml::new().id("test").write(path),
                "disabled by hand" => MachineConfig::set_enabled(path, "agent", false).unwrap(),
                _ => {}
            });
            let result = c.take_out_harness(&req).await.unwrap();
            let applied = scenario == "clean";
            assert_eq!(result.applied, applied, "{scenario}: {result:?}");
            assert_eq!(result.errors.is_empty(), applied, "{scenario}: {result:?}");
            assert!(!c.assignment("test").await.unwrap().assignment.harness("agent").new_skills_default);
            if scenario == "first scan" {
                assert!(result.errors.iter().any(|e| e.contains("Could not list old placements")), "{result:?}");
                assert_eq!(read(&cfg.machine_config), before);
                assert!(placed(&m, "pdf").exists());
                assert!(result.taken_out.is_empty());
            } else {
                assert_eq!(
                    result.taken_out.len(),
                    usize::from(scenario != "removed by hand"),
                    "{scenario}: {result:?}"
                );
                assert!(result.left.is_empty(), "{scenario}: {result:?}");
                assert!(!placed(&m, "pdf").exists());
            }
            let message = match scenario {
                "second scan" => "machine.toml was not changed",
                "config edit" => "agent is still on",
                "removed by hand" => "agent is no longer in machine.toml",
                "disabled by hand" => "agent is off in machine.toml",
                _ => "",
            };
            if !message.is_empty() {
                assert!(result.errors.iter().any(|e| e.contains(message)), "{result:?}");
            }
            let enabled = ["first scan", "second scan", "config edit"].contains(&scenario);
            assert_eq!(c.harnesses().await.unwrap().iter().any(|h| h.name == "agent"), enabled);
            if scenario == "config edit" {
                assert!(read(&cfg.machine_config).ends_with("# edited by hand\n"));
            }
        });
    }
}

#[test]
fn applying_a_harness_change_waits_for_take_out_then_rechecks_its_preview() {
    let (m, cfg) = fixture();
    let _home = m.home.enter();
    m.repo.skill(Skill::new("pdf").id("pdf"));
    runtime(m.home.dirs()).block_on(async {
        let engine = writer(&cfg);
        let c = engine.client();
        let req = take_out_request(c).await;
        let (started, entered) = tokio::sync::oneshot::channel();
        let (proceed, blocked) = mpsc::channel();
        let _in_take_out = failpoints::once_shared(Point::DaemonTakeOutEntered, &cfg.machine_config, move |_| {
            started.send(()).unwrap();
            blocked.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        let taking_out = tokio::spawn({
            let (c, req) = (c.clone(), req.clone());
            async move { c.take_out_harness(&req).await }
        });
        tokio::time::timeout(Duration::from_secs(5), entered).await.unwrap().unwrap();
        let (applied, entered_apply) = mpsc::channel();
        let _in_apply = failpoints::once_shared(Point::DaemonApplyEntered, &cfg.machine_config, move |_| {
            applied.send(()).unwrap();
        });
        let mut applying = std::pin::pin!(c.apply_harness(&req));
        tokio::select! {
            biased;
            _ = &mut applying => panic!("applied while a take-out holds the lock"),
            _ = std::future::ready(()) => {}
        }
        assert!(entered_apply.try_recv().is_err());
        proceed.send(()).unwrap();
        assert!(taking_out.await.unwrap().unwrap().applied);
        assert_eq!(applying.await.unwrap_err().status, Some(409));
        entered_apply.try_recv().unwrap();
    });
}

fn exit_at(point: Point, path: &std::path::Path) -> failpoints::Guard {
    let path = path.to_path_buf();
    failpoints::install_shared(move |current, about| {
        if point == current && about == path { Action::Exit(sk1ll_testkit::child::CRASHED) } else { Action::Continue }
    })
}

fn replay_child(test: &str, m: &Machine, cfg: &Config) -> std::process::Command {
    let mut command = child::command(test, m.root());
    m.home
        .apply(&mut command)
        .env("SK1LL_REPO", cfg.repo.as_ref().unwrap())
        .env("SK1LL_DAEMON_STATE", &cfg.state)
        .env("SK1LL_MACHINE_CONFIG", &cfg.machine_config)
        .env("SK1LL_ASSIGNMENT", &cfg.assignment)
        .env("SK1LL_DAEMON_SOCKET", &cfg.socket);
    command
}

fn child_runtime(config: &Config) -> tokio::runtime::Runtime {
    runtime(sk1ll_core::paths::Dirs { state: config.state_dir.clone(), cache: config.cache_dir.clone() })
}

#[test]
fn a_crash_before_the_assignment_swap_keeps_the_previous_revision() {
    if child::dir().is_some() {
        let config = Config::from_env().unwrap();
        child_runtime(&config).block_on(async {
            let engine = writer(&config);
            let _crash = exit_at(Point::DaemonStoreBeforeSwap, &config.assignment);
            engine.client().set_harness_assignment("agent", true).await.unwrap();
        });
        panic!("the store reached no failpoint");
    }
    let (m, cfg) = fixture();
    let _home = m.home.enter();
    let mut original = Assignment::default();
    original.set_harness("agent", false);
    original.save(&cfg.assignment).unwrap();
    let run = runtime(m.home.dirs());
    let revision = run.block_on(async { writer(&cfg).client().status().await.unwrap().assignment_revision });
    let output = replay_child("races::a_crash_before_the_assignment_swap_keeps_the_previous_revision", &m, &cfg)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(child::CRASHED), "{}", sk1ll_testkit::text(&output));
    assert_eq!(Assignment::load(&cfg.assignment).unwrap(), original);
    run.block_on(async {
        let engine = writer(&cfg);
        let c = engine.client();
        assert_eq!(c.status().await.unwrap().assignment_revision, revision);
        let next = c.set_harness_assignment("agent", true).await.unwrap();
        assert_eq!(next.revision, revision + 1);
        assert_eq!(Assignment::load(&cfg.assignment).unwrap(), next.assignment);
    });
}

#[test]
fn interrupted_join_activation_keeps_credentials_and_toggles_from_the_same_registration() {
    use sk1ll_daemon::DaemonState;
    use sk1ll_daemon::client::{DaemonMode, JoinApiRequest};
    use sk1ll_proto::AssignmentSnapshot;
    use sk1ll_testkit::server::Server;

    if let Some(root) = child::dir() {
        let config = Config::from_env().unwrap();
        let point = match std::env::var("SK1LL_PC1_JOIN_POINT").unwrap().as_str() {
            "before rejoin persist" => Point::DaemonBeforeRejoinPersist,
            "after rejoin snapshot" => Point::DaemonJoinAfterSnapshot,
            "before standalone credentials" => Point::DaemonJoinBeforeCredentials,
            other => panic!("unknown activation scenario {other}"),
        };
        let req: JoinApiRequest = serde_json::from_str(&read(&root.join("request.json"))).unwrap();
        let path = if point == Point::DaemonBeforeRejoinPersist { &config.machine_config } else { &config.state };
        child_runtime(&config).block_on(async {
            let engine = writer(&config);
            let _crash = exit_at(point, path);
            let result = engine.client().join(&req).await;
            panic!("join reached no failpoint: {result:?}");
        });
        unreachable!();
    }

    // Each interrupted activation gets a fresh slot and code at the server.
    let home = sk1ll_testkit::Home::new();
    let run = runtime(home.dirs());
    run.block_on(async {
        let server = Server::start().await;
        for (index, scenario) in
            ["before rejoin persist", "after rejoin snapshot", "before standalone credentials"].into_iter().enumerate()
        {
            let (m, cfg) = fixture();
            let root = m.root();
            let repo = m.repo.root();
            std::fs::remove_dir_all(repo).unwrap();
            let joined = scenario != "before standalone credentials";
            let old = DaemonState {
                revoked: true,
                ..DaemonState::new("old".into(), server.url(), "old-token".into(), repo.into())
            };
            let snap = AssignmentSnapshot { revision: 37, ..Default::default() };
            if joined {
                old.save(&cfg.state).unwrap();
                state::save_snapshot(&cfg.snapshot_path(), &snap, Some(&old.registration_key())).unwrap();
            }
            let req = JoinApiRequest {
                server_url: server.url(),
                code: server.join_code().await,
                admin_token: None,
                machine_id: Some(format!("next-{index}")),
                copy_from: None,
                repo: None,
            };
            write(&root.join("request.json"), &serde_json::to_string(&req).unwrap());
            let output = replay_child(
                "races::interrupted_join_activation_keeps_credentials_and_toggles_from_the_same_registration",
                &m,
                &cfg,
            )
            .env("SK1LL_PC1_JOIN_POINT", scenario)
            .output()
            .unwrap();
            assert_eq!(output.status.code(), Some(child::CRASHED), "{scenario}: {}", sk1ll_testkit::text(&output));
            assert_eq!(DaemonState::load(&cfg.state).unwrap(), joined.then_some(old.clone()));
            if joined {
                let found = state::load_snapshot(&cfg.snapshot_path(), Some(&old.registration_key())).unwrap();
                assert_eq!(found, (index == 0).then_some(snap.clone()), "{scenario}");
            } else {
                assert!(cfg.snapshot_path().is_file(), "credentials are the final activation file");
                assert!(
                    std::fs::read_dir(cfg.state.parent().unwrap()).unwrap().any(|file| {
                        file.unwrap().file_name().to_string_lossy().starts_with("daemon-pending-join-")
                    })
                );
            }
            let _home = m.home.enter();
            let engine = writer(&cfg);
            let restarted = engine.client().status().await.unwrap();
            assert_eq!(restarted.mode, if joined { DaemonMode::Joined } else { DaemonMode::Standalone });
            if joined {
                assert!(restarted.revoked);
                assert_eq!(restarted.assignment_revision, if index == 0 { snap.revision } else { 0 });
            }
        }
    });
}

#[test]
fn a_credentials_collision_restores_the_original_and_saves_edits_to_the_replacement() {
    use sk1ll_daemon::client::{DaemonMode, JoinApiRequest};
    use sk1ll_testkit::server::Server;

    let (m, cfg) = fixture();
    let _home = m.home.enter();
    let repo = m.repo.root();
    std::fs::remove_dir_all(repo).unwrap();
    runtime(m.home.dirs()).block_on(async {
        let server = Server::start().await;
        let engine = writer(&cfg);
        let c = engine.client();
        c.init().await.unwrap();
        let mut local = Assignment::default();
        local.set_harness("agent", false);
        local.save(&cfg.assignment).unwrap();
        let assignment = read(&cfg.assignment);
        write(&cfg.snapshot_path(), "the original snapshot");
        let head = m.repo.head();
        let root = m.root().to_path_buf();
        let live = repo.to_path_buf();
        let area = |root: &std::path::Path| {
            std::fs::read_dir(root)
                .unwrap()
                .map(|e| e.unwrap().path())
                .find(|path| path.file_name().unwrap().to_string_lossy().starts_with("sk1ll-before-join-"))
                .unwrap()
        };
        let _collision = failpoints::once_shared(Point::DaemonJoinBeforeCredentials, &cfg.state, move |state| {
            let previous = area(&root).join("repo");
            assert!(sk1ll_core::EngineLock::try_acquire(&live).unwrap().is_none());
            assert!(sk1ll_core::EngineLock::try_acquire(&previous).unwrap().is_none());
            write(&live.join("notes"), "saved during activation");
            write(&state.join("notes"), "private obstruction");
        });
        let request = JoinApiRequest {
            server_url: server.url(),
            code: server.join_code().await,
            admin_token: None,
            machine_id: Some("test".into()),
            copy_from: None,
            repo: None,
        };
        let error = c.join(&request).await.unwrap_err();
        assert!(error.message.contains("Pending credentials"), "{error}");
        assert_eq!(read(&cfg.state.join("notes")), "private obstruction");
        assert_eq!(read(&cfg.assignment), assignment);
        assert_eq!(read(&cfg.snapshot_path()), "the original snapshot");
        assert_eq!(m.repo.head(), head);
        assert!(!repo.join("notes").exists());
        assert_eq!(read(&area(m.root()).join("unactivated-repo/notes")), "saved during activation");
        assert!(sk1ll_core::EngineLock::try_acquire(repo).unwrap().is_none());
        assert_eq!(c.status().await.unwrap().mode, DaemonMode::Standalone);
    });
}
