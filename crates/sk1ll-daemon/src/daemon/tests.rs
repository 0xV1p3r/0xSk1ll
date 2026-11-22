use std::sync::atomic::{AtomicBool, Ordering};

use sk1ll_core::sync::{self, Action, ActionKind, Item};
use sk1ll_proto::{AssignmentUpdate, AssignmentWrite};

use super::assignment::newer;
use super::pull::is_rejected_push;
use super::read::{DESCRIPTION_CAP, parse_batch, small_blob};
use super::*;
use crate::client::{ChangeKind, ConflictTarget};
use crate::state::PullState;
use sk1ll_core::ItemRef;

#[test]
fn inventory_keeps_both_ids_when_a_draft_recreates_the_same_name() {
    use sk1ll_core::git::Git;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let repo = Repo::init_committed(root).unwrap();
    repo.create_item("skill", "pdf", "").unwrap();
    let old = repo.skills().unwrap()[0].id.clone();
    let git = |args: &[&str]| {
        Git::new(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["-c", "commit.gpgsign=false", "-c", "user.name=test", "-c", "user.email=test@example.com"])
            .args(args)
            .text()
            .unwrap()
    };
    git(&["add", "--all"]);
    git(&["commit", "--quiet", "-m", "baseline"]);
    let head = git(&["rev-parse", "HEAD"]);
    git(&["update-ref", "refs/remotes/origin/main", &head]);
    repo.delete_item("skill", "pdf").unwrap();
    repo.create_item("skill", "pdf", "").unwrap();
    let current = repo.skills().unwrap()[0].id.clone();
    let items = Daemon::items(root).unwrap();
    assert!(
        items.iter().any(|i| i.id.as_ref() == old.as_ref() && i.in_baseline && i.draft == Some(ChangeKind::Deleted))
    );
    assert!(
        items.iter().any(|i| i.id.as_ref() == current.as_ref() && !i.in_baseline && i.draft == Some(ChangeKind::Added))
    );
}

#[test]
fn inventory_reads_applied_baseline_and_never_refreshes_the_index() {
    use sk1ll_core::git::Git;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let git = |args: &[&str]| {
        Git::new(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .run()
            .unwrap()
    };
    let write = |path: &str, text: &str| {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.name", "test"]);
    git(&["config", "user.email", "test@example.invalid"]);
    write("sk1ll.toml", "");
    write("personal/base/SKILL.md", "---\ndescription: Baseline description\n---\nbase\n");
    write("personal/base/.sk1ll-id", "base-skill\n");
    write(
        "instructions/git.md",
        &format!("---\nid: git-rule\n---\n{}", "x".repeat(sk1ll_core::content::MAX_FILE_BYTES as usize)),
    );

    write("imported/vendored/SKILL.md", "---\ndescription: Imported description\n---\n");
    write("personal/my skill/SKILL.md", "---\ndescription: 'Spaced'\n---\n\x00\nbody\n");
    write("personal/huge/SKILL.md", &format!("---\ndescription: Too big\n---\n{}", "x".repeat(DESCRIPTION_CAP)));
    write("personal/plain/SKILL.md", "no front matter\n");
    git(&["add", "."]);
    git(&["commit", "-qm", "baseline"]);
    let baseline = String::from_utf8(git(&["rev-parse", "HEAD"])).unwrap();
    git(&["update-ref", "refs/remotes/origin/main", baseline.trim()]);
    write("personal/queued/SKILL.md", "---\ndescription: Queued description\n---\n");
    git(&["add", "."]);
    git(&["commit", "-qm", "unpushed"]);
    write("personal/base/SKILL.md", "---\ndescription: Draft description\n---\nedited\n");
    write("personal/local/SKILL.md", "new\n");
    std::fs::remove_file(root.join("instructions/git.md")).unwrap();
    let index_before = std::fs::read(root.join(".git/index")).unwrap();
    let head_before = git(&["rev-parse", "HEAD"]);
    let items = Daemon::items(root).unwrap();
    let find = |name: &str| items.iter().find(|entry| entry.item.to_string() == name).unwrap();
    assert!(find("skill:base").in_baseline);
    assert_eq!(find("skill:base").draft, Some(ChangeKind::Modified));
    assert_eq!(find("skill:base").description.as_deref(), Some("Draft description"));
    assert_eq!(find("skill:base").id.as_ref().unwrap().to_string(), "base-skill");
    // Descriptions of items without a draft are read from git, in one batch.
    assert_eq!(find("skill:vendored").description.as_deref(), Some("Imported description"));
    assert_eq!(find("skill:my skill").description.as_deref(), Some("Spaced"));
    assert_eq!(find("skill:queued").description.as_deref(), Some("Queued description"));
    assert_eq!(find("skill:huge").description, None, "over the cap: not read");
    assert_eq!(find("skill:plain").description, None);
    assert_eq!(find("skill:local").description, None);
    assert!(find("rule:git").in_baseline, "deleted Draft item still comes from the git tree");
    assert_eq!(find("rule:git").draft, Some(ChangeKind::Deleted));
    assert_eq!(find("rule:git").id.as_ref().unwrap().to_string(), "git-rule");
    assert!(!find("skill:queued").in_baseline);
    assert!(find("skill:queued").unpushed);
    assert!(!find("skill:local").in_baseline);
    assert_eq!(find("skill:local").draft, Some(ChangeKind::Added));
    assert_eq!(std::fs::read(root.join(".git/index")).unwrap(), index_before);
    assert_eq!(git(&["rev-parse", "HEAD"]), head_before);

    write("sk1ll.toml", "[import.vendored]\nid = 42\n");
    assert!(Daemon::items(root).unwrap_err().to_string().contains("sk1ll.toml"));
    write("sk1ll.toml", "");

    // A fetched but clashing Baseline has not been applied locally.
    git(&["checkout", "-qb", "remote", baseline.trim()]);
    write("instructions/remote.md", "remote rule\n");
    git(&["add", "instructions/remote.md"]);
    git(&["commit", "-qm", "remote"]);
    let remote = String::from_utf8(git(&["rev-parse", "HEAD"])).unwrap();
    git(&["update-ref", "refs/remotes/origin/main", remote.trim()]);
    git(&["checkout", "-q", "main"]);
    let items = Daemon::items(root).unwrap();
    assert!(!items.iter().any(|entry| entry.item.to_string() == "rule:remote"));
    assert!(items.iter().find(|entry| entry.item.to_string() == "skill:base").unwrap().in_baseline);
}

#[test]
fn only_small_blobs_are_fetched() {
    assert!(small_blob(b"0123abcd blob 16", 16));
    assert!(!small_blob(b"0123abcd blob 17", 16));
    assert!(!small_blob(b"0123abcd tree 4", 16));
    assert!(!small_blob(b"HEAD:personal/a b/SKILL.md missing", 16));
    assert!(!small_blob(b"", 16));
}

#[test]
fn batch_output_is_read_by_size() {
    let oid = "ce013625030ba8dba906f756967f9e9ca394464a";
    let mut out = Vec::new();
    out.extend_from_slice(format!("{oid} blob 8\n").as_bytes());
    out.extend_from_slice(b"a\n\0 b\nc\n\n");
    out.extend_from_slice(b"abc:personal/a b/SKILL.md missing\n");
    out.extend_from_slice(format!("{oid} blob 0\n\n").as_bytes());
    out.extend_from_slice(format!("{oid} tree 4\nabcd\n").as_bytes());
    out.extend_from_slice(format!("{oid} blob 7\n").as_bytes());
    // Content that looks like headers is still content.
    out.extend_from_slice(b"x blob\n\n");
    out.extend_from_slice(format!("{oid} blob 5\nlarge\n").as_bytes());
    out.extend_from_slice(format!("{oid} blob 2\nok\n").as_bytes());
    out.extend_from_slice(format!("{oid} blob 9\ncut").as_bytes());
    let got = parse_batch(&out, 9, 4);
    let want: [Option<&[u8]>; 9] = [None, None, Some(b""), None, None, None, Some(b"ok"), None, None];
    assert_eq!(got, want.map(|content| content.map(<[u8]>::to_vec)));
    assert_eq!(parse_batch(&out, 1, 16)[0].as_deref(), Some(&b"a\n\0 b\nc\n"[..]));
    assert_eq!(parse_batch(b"", 2, 16), [None, None]);
}

#[test]
fn plan_hashes_each_copy_source_once_per_computation() {
    let copy = |harness: &str, source: &str| Action {
        harness: harness.into(),
        kind: ActionKind::Create,
        dest: PathBuf::from(harness).join(source),
        item: Item::Skill {
            name: source.into(),
            mode: sk1ll_core::LinkMode::Copy,
            source: Some(source.into()),
            item_id: None,
            metadata_only: false,
            expected: None,
            expected_index: None,
        },
        reason: None,
    };
    let actions = [copy("a", "pdf"), copy("a", "notes"), copy("b", "pdf")];
    let mut calls = Vec::new();
    for digest in ["before", "after"] {
        let sources = Daemon::sync_source_digests(&actions, |path| {
            calls.push(path.to_path_buf());
            Ok(format!("{}-{digest}", path.display()))
        })
        .unwrap();
        assert_eq!(
            sources,
            [Some(format!("pdf-{digest}")), Some(format!("notes-{digest}")), Some(format!("pdf-{digest}"))]
        );
    }
    assert_eq!(calls, ["pdf", "notes", "pdf", "notes"].map(PathBuf::from));
}

#[tokio::test]
async fn reviewed_sync_reports_partial_progress_and_stops_on_digest_mismatch() {
    use crate::client::{Client, SyncRequest};

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    let repo = Repo::init(&root).unwrap();
    let source = repo.personal_dir().join("pdf/SKILL.md");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, "reviewed").unwrap();
    for name in ["a-success", "b-io-error", "z-later"] {
        let skill = repo.personal_dir().join(name);
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), name).unwrap();
    }
    repo.assign_missing_ids().unwrap();
    let failing_source = repo.personal_dir().join("b-io-error");
    let cfg = Config {
        repo: Some(root),
        state: dir.path().join("state/daemon.json"),
        assignment: dir.path().join("assignment.json"),
        machine_config: dir.path().join("machine.toml"),
        socket: dir.path().join("daemon.sock"),
        ..Config::new().unwrap()
    };
    let skills = dir.path().join("skills");
    let presets =
        sk1ll_core::machine::presets().map(|name| format!("[harnesses.{name}]\nenabled = false\n")).collect::<String>();
    std::fs::write(
        &cfg.machine_config,
        format!("id = \"review\"\n[harnesses.test]\nskills = {skills:?}\nmode = \"copy\"\n{presets}"),
    )
    .unwrap();
    let client = Client::new(&cfg.socket);
    let listener = tokio::net::UnixListener::bind(&cfg.socket).unwrap();
    let daemon = Daemon::standalone(cfg).unwrap();
    lock(&daemon.engine).after_sync_review = Some(Box::new(move || {
        std::fs::remove_dir_all(failing_source).unwrap();
        std::fs::write(source, "changed after the fingerprint matched").unwrap();
    }));
    let mut events = daemon.subscribe_events();
    let served = daemon.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, crate::api::router(served)).await.unwrap();
    });
    let reviewed = client.preview_sync().await.unwrap().fingerprint.unwrap();
    let returned = client.apply_sync_preview(reviewed.clone()).await.unwrap();
    let fresh = returned.fingerprint.unwrap();
    assert_ne!(fresh, reviewed);
    let remaining: Vec<_> = returned.actions.unwrap().into_iter().map(|action| action.name).collect();
    assert_eq!(remaining, ["pdf", "z-later"]);
    assert_eq!(returned.completed.len(), 1);
    assert_eq!(returned.completed[0].name, "a-success");
    assert_eq!(returned.changed.unwrap().name, "pdf");
    assert_eq!(returned.errors.len(), 1);
    assert!(returned.errors[0].contains("b-io-error"));
    assert_eq!(daemon.view().last_error.as_ref(), returned.errors.first());
    assert!(lock(&daemon.engine).reconciled.is_none(), "a reviewed sync leaves the rest to the next reconcile");
    assert!(matches!(events.try_recv().unwrap(), Event::Status));
    assert!(skills.join("a-success/SKILL.md").is_file());
    assert!(!skills.join("z-later").exists());
    assert_eq!(client.preview_sync().await.unwrap().fingerprint.as_deref(), Some(fresh.as_str()));
    assert!(!skills.join("pdf").exists());
    let error = client.sync(&SyncRequest { fingerprint: Some(reviewed), ..Default::default() }).await.unwrap_err();
    assert_eq!(error.status, Some(409));
    let applied = client.apply_sync_preview(fresh).await.unwrap();
    assert!(applied.errors.is_empty());
    assert_eq!(std::fs::read_to_string(skills.join("pdf/SKILL.md")).unwrap(), "changed after the fingerprint matched");
    server.abort();
}

#[test]
fn stale_revisions_are_ignored() {
    assert!(newer(0, 1));
    assert!(newer(4, 5));
    assert!(!newer(5, 5));
    assert!(!newer(5, 3));
    assert!(!newer(5, 0));
    assert!(!newer(0, 0));
}

#[test]
fn only_rejected_pushes_are_retried() {
    let git = |stderr: &str| sk1ll_core::Error::Git { args: "push".into(), stderr: stderr.into() };
    assert!(is_rejected_push(&git(" ! [rejected]        main -> main (fetch first)")));
    assert!(!is_rejected_push(&git("fatal: unable to access 'http://h/git/repo.git/': Failed to connect")));
    assert!(!is_rejected_push(&git("fatal: the remote end hung up unexpectedly")));
}
#[tokio::test]
async fn refused_adoption_rolls_back_only_its_own_unchanged_copy_and_can_retry() {
    use axum::http::StatusCode;
    for scenario in ["new", "retry", "edit_repo", "edit_source"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let repo = Repo::init(&root).unwrap();
        let source = dir.path().join("h/pdf");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("SKILL.md"), "original").unwrap();
        let destination = repo.personal_dir().join("pdf");
        if scenario == "retry" {
            std::fs::create_dir_all(&destination).unwrap();
            std::fs::write(destination.join("SKILL.md"), "original").unwrap();
        }
        let accept = Arc::new(AtomicBool::new(false));
        let mut cfg = Config::new().unwrap();
        cfg.state = dir.path().join("state/daemon.json");
        cfg.assignment = dir.path().join("assignment.json");
        cfg.machine_config = dir.path().join("machine.toml");
        std::fs::write(
            &cfg.machine_config,
            // The presets would find the developer's real harnesses.
            format!(
                "[harnesses.h]\nskills = {:?}\nmode = \"copy\"\n{}",
                dir.path().join("h"),
                sk1ll_core::machine::presets()
                    .map(|p| format!("[harnesses.{p}]\nenabled = false\n"))
                    .collect::<String>()
            ),
        )
        .unwrap();
        let mut snap = AssignmentSnapshot { revision: 1, ..Default::default() };
        snap.assignment.set_new_skills_default("h", false);
        state::save_snapshot(&cfg.snapshot_path(), &snap).unwrap();
        let d = Daemon::new(
            cfg,
            DaemonState {
                machine_id: "a".into(),
                token: "t".into(),
                server_url: "http://127.0.0.1:1".into(),
                repo: root,
                revoked: false,
                record: Default::default(),
            },
        )
        .unwrap();
        let (tx, mut rx) = mpsc::channel::<crate::server::SessionWrite>(1);
        d.assignment_session.send_replace(Some(tx));
        let server = tokio::spawn({
            let (accept, destination, source) = (accept.clone(), destination.clone(), source.clone());
            async move {
                while let Some(request) = rx.recv().await {
                    let AssignmentWrite::Changes(body) = request.write else { panic!("expected toggles") };
                    assert_eq!(body.changes.len(), 1);
                    let change = &body.changes[0];
                    assert_eq!(change.enabled, Some(true));
                    assert_eq!(change.name.as_deref(), Some("pdf"));
                    let result = if accept.load(Ordering::SeqCst) {
                        let mut assignment = sk1ll_proto::Assignment::default();
                        assignment.set_skill("h", change.item_id.as_ref().unwrap(), true);
                        Ok(AssignmentUpdate { assignment, revision: 2, entries: vec![], errors: vec![] })
                    } else {
                        if scenario == "edit_repo" {
                            std::fs::write(destination.join("SKILL.md"), "later edit").unwrap();
                        }
                        if scenario == "edit_source" {
                            std::fs::write(source.join("SKILL.md"), "later edit").unwrap();
                        }
                        Err(Error::Upstream(StatusCode::UNPROCESSABLE_ENTITY, "Server refused".into()))
                    };
                    request.result.send(result).unwrap();
                }
            }
        });
        d.set_link(Link::Connected);
        let error = d.adopt(ConflictTarget { harness: "h".into(), name: "pdf".into() }).await.unwrap_err();
        assert!(error.to_string().contains("Server refused"), "{error}");
        assert_eq!(
            std::fs::read_to_string(source.join("SKILL.md")).unwrap(),
            if scenario == "edit_source" { "later edit" } else { "original" }
        );
        assert!(!source.parent().unwrap().join(sync::INDEX_PATH).exists());
        if scenario == "new" {
            assert!(!destination.exists());
            accept.store(true, Ordering::SeqCst);
            d.adopt(ConflictTarget { harness: "h".into(), name: "pdf".into() }).await.unwrap();
            let index = std::fs::read(source.parent().unwrap().join(sync::INDEX_PATH)).unwrap();
            let index: serde_json::Value = serde_json::from_slice(&index).unwrap();
            assert_eq!(index["entries"]["pdf"]["kind"], "copy");
            let token = std::fs::read_to_string(source.join(sync::PLACEMENT_MARKER)).unwrap();
            assert_eq!(index["entries"]["pdf"]["token"], token);
        } else {
            assert_eq!(
                std::fs::read_to_string(destination.join("SKILL.md")).unwrap(),
                if scenario == "edit_repo" { "later edit" } else { "original" }
            );
        }
        server.abort();
    }
}

/// A joined daemon on a fresh repo, its Assignment at revision 1 and
/// `record` in its state file, with a server session the test answers.
fn joined_daemon(
    dir: &Path,
    record: PullRecord,
) -> (Arc<Daemon>, mpsc::Receiver<crate::server::SessionWrite>, DaemonState) {
    let root = dir.join("repo");
    Repo::init_committed(&root).unwrap();
    let mut cfg = Config::new().unwrap();
    cfg.state = dir.join("state/daemon.json");
    cfg.assignment = dir.join("assignment.json");
    cfg.machine_config = dir.join("machine.toml");
    std::fs::write(
        &cfg.machine_config,
        format!(
            "[harnesses.h]\nskills = {:?}\nmode = \"copy\"\n{}",
            dir.join("h"),
            sk1ll_core::machine::presets().map(|p| format!("[harnesses.{p}]\nenabled = false\n")).collect::<String>()
        ),
    )
    .unwrap();
    state::save_snapshot(&cfg.snapshot_path(), &AssignmentSnapshot { revision: 1, ..Default::default() }).unwrap();
    let state = DaemonState { record, ..DaemonState::new("a".into(), "http://127.0.0.1:1".into(), "t".into(), root) };
    state.save(&cfg.state).unwrap();
    let d = Daemon::new(cfg, state.clone()).unwrap();
    let (tx, rx) = mpsc::channel::<crate::server::SessionWrite>(1);
    d.assignment_session.send_replace(Some(tx));
    d.set_link(Link::Connected);
    (d, rx, state)
}

fn harness_toggle(enabled: bool) -> AssignmentWrite {
    AssignmentWrite::Harness { harness: "h".into(), enabled }
}

fn confirmed(revision: u64, enabled: bool) -> Result<AssignmentUpdate> {
    let mut assignment = sk1ll_proto::Assignment::default();
    assignment.set_harness("h", enabled);
    Ok(AssignmentUpdate { assignment, revision, entries: vec![], errors: vec![] })
}

/// The server takes its time to confirm a write; meanwhile the engine,
/// the readers and the status all answer.
#[tokio::test(flavor = "multi_thread")]
async fn a_slow_assignment_confirmation_blocks_no_other_call() {
    let dir = tempfile::tempdir().unwrap();
    let (d, mut session, _) = joined_daemon(dir.path(), PullRecord::default());
    d.blocking(|d, eng| d.reconcile(eng).map(drop)).await.unwrap();
    let writer = d.clone();
    let write = tokio::spawn(async move { writer.write_assignment(harness_toggle(false)).await });
    let request = session.recv().await.expect("the write reached the session");

    let quick = std::time::Duration::from_secs(5);
    let started = std::time::Instant::now();
    tokio::time::timeout(quick, d.reading(|d| d.draft())).await.expect("a reader waited").unwrap();
    tokio::time::timeout(quick, d.blocking(|d, eng| d.reconcile(eng).map(drop)))
        .await
        .expect("the engine mutex was held during the request")
        .unwrap();
    d.set_link(Link::Offline);
    assert_eq!(d.view().connection, Connection::Offline);
    assert!(started.elapsed() < quick);
    assert!(!write.is_finished(), "the write was still waiting for the server");

    request.result.send(confirmed(2, false)).unwrap();
    let update = write.await.unwrap().unwrap();
    assert_eq!(update.revision, 2);
    assert_eq!(d.snapshot().unwrap().revision, 2);
    assert_eq!(d.snapshot().unwrap().assignment, confirmed(2, false).unwrap().assignment);
}

/// What changes while a write is in flight decides what its confirmation
/// does: a newer revision stands, and a daemon that stopped managing
/// applies nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_confirmation_overtaken_by_a_newer_revision_is_not_applied() {
    let dir = tempfile::tempdir().unwrap();
    let (d, mut session, _) = joined_daemon(dir.path(), PullRecord::default());
    let writer = d.clone();
    let write = tokio::spawn(async move { writer.write_assignment(harness_toggle(false)).await });
    let request = session.recv().await.unwrap();
    // The server's snapshot of a later write (this one's included)
    // arrives before this write's own confirmation.
    let newer = confirmed(3, false).unwrap();
    let snapshot = AssignmentSnapshot { revision: 3, assignment: newer.assignment.clone(), registration: None };
    d.blocking(move |d, eng| d.apply_assignment(eng, snapshot)).await.unwrap();
    request.result.send(confirmed(2, true)).unwrap();
    let update = write.await.unwrap().unwrap();
    assert_eq!(update.revision, 3, "the reply carries the revision that stands");
    assert_eq!(update.assignment, newer.assignment);
    assert_eq!(d.snapshot().unwrap().revision, 3);
    assert_eq!(state::load_snapshot(&d.cfg.snapshot_path()).unwrap().unwrap().revision, 3);

    let writer = d.clone();
    let write = tokio::spawn(async move { writer.write_assignment(harness_toggle(true)).await });
    let request = session.recv().await.unwrap();
    d.blocking(|d, eng| d.revoke(eng, "removed from the fleet")).await.unwrap();
    request.result.send(confirmed(4, true)).unwrap();
    assert_eq!(write.await.unwrap().unwrap().revision, 4);
    assert_eq!(d.snapshot().unwrap().revision, 3, "a removed Machine applies nothing");
}

/// The state file keeps the block and the server's Baseline, so a
/// restarted daemon starts Blocked and Behind, before any pull.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_remembers_the_block_and_the_server_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let (d, _session, state) = joined_daemon(dir.path(), PullRecord::default());
    d.blocking(|d, eng| d.reconcile(eng).map(drop)).await.unwrap();
    let head = gitx::rev(d.repo_root(), "HEAD").unwrap();
    let clash = ItemRef::Skill("pdf".into());
    let blocked = PullState::Blocked { baseline: "f00d".repeat(10), items: vec![clash.clone()] };
    d.note(|lc| {
        lc.set_pull(blocked.clone());
        lc.set_server_baseline(Some("f00d".repeat(10)));
    });
    let saved = DaemonState::load(&d.cfg.state).unwrap().unwrap();
    assert_eq!(saved.record.pull, blocked);
    assert_eq!(saved.record.server_baseline, Some("f00d".repeat(10)));
    assert_eq!((saved.token.as_str(), saved.revoked), (state.token.as_str(), false));

    let cfg = d.cfg.clone();
    drop(d);
    let restarted = Daemon::new(cfg.clone(), DaemonState::load(&cfg.state).unwrap().unwrap()).unwrap();
    let view = restarted.view();
    assert!(view.report.blocked && view.report.behind);
    assert_eq!(view.report.clashes, [clash]);
    assert_eq!(view.server_baseline, Some("f00d".repeat(10)));
    assert_eq!(view.connection, Connection::Connecting);
    assert_eq!(restarted.lifecycle().pull(), &blocked);
    // A reconcile keeps it: HEAD is still not on the blocking Baseline.
    restarted.blocking(|d, eng| d.reconcile(eng).map(drop)).await.unwrap();
    assert!(restarted.view().report.blocked);

    // Once HEAD contains that Baseline (a pull while the daemon was
    // stopped), the block is over.
    let head = head.expect("an initialized repo has a commit");
    restarted.note(|lc| lc.set_pull(PullState::Blocked { baseline: head, items: Vec::new() }));
    restarted.blocking(|d, eng| d.reconcile(eng).map(drop)).await.unwrap();
    assert_eq!(restarted.lifecycle().pull(), &PullState::Clear);
    assert!(!restarted.view().report.blocked);
    assert_eq!(DaemonState::load(&cfg.state).unwrap().unwrap().record.pull, PullState::Clear);

    // Removed from the Fleet: a restart stays removed.
    restarted.blocking(|d, eng| d.revoke(eng, "removed")).await.unwrap();
    let state = DaemonState::load(&cfg.state).unwrap().unwrap();
    assert!(state.revoked);
    let removed = Daemon::new(cfg, state).unwrap();
    assert_eq!(removed.view().connection, Connection::Revoked);
    assert!(removed.lifecycle().idle());
}

/// A halted daemon (joined again, restarting) never writes the state
/// file: it may already hold the next registration.
#[tokio::test(flavor = "multi_thread")]
async fn a_halted_daemon_leaves_the_state_file_alone() {
    let dir = tempfile::tempdir().unwrap();
    let (d, _session, state) = joined_daemon(dir.path(), PullRecord::default());
    let next = DaemonState::new("b".into(), "http://127.0.0.1:2".into(), "new".into(), state.repo.clone());
    d.halt_with(|| next.save(&d.cfg.state).map(|()| true)).unwrap();
    d.note(|lc| lc.set_server_baseline(Some("abc".into())));
    d.blocking(|d, eng| d.revoke(eng, "the old registration was removed")).await.unwrap();
    assert_eq!(DaemonState::load(&d.cfg.state).unwrap(), Some(next));
    assert!(d.ensure_managed().is_err());
}

/// A reconcile plans only when an Input changed, and each kind of change
/// costs one; trees that did not change are not hashed again.
#[tokio::test(flavor = "multi_thread")]
async fn a_reconcile_runs_once_per_input_change_and_rehashes_only_changed_trees() {
    use std::sync::atomic::Ordering::Relaxed;
    let dir = tempfile::tempdir().unwrap();
    let (d, _session, _) = joined_daemon(dir.path(), PullRecord::default());
    let root = d.repo_root().to_path_buf();
    std::fs::create_dir_all(root.join("personal/pdf")).unwrap();
    std::fs::write(root.join("personal/pdf/SKILL.md"), "PDFs.\n").unwrap();
    Repo::open(&root).unwrap().assign_missing_ids().unwrap();
    let reconcile = || {
        let d = d.clone();
        async move { d.blocking(|d, eng| d.reconcile(eng)).await.unwrap() }
    };
    // Placing the skill changes the harness: one more pass settles it.
    assert!(reconcile().await.is_empty());
    assert!(reconcile().await.is_empty());
    let placed = dir.path().join("h/pdf/SKILL.md");
    assert_eq!(std::fs::read_to_string(&placed).unwrap(), "PDFs.\n");
    let counts = || (d.plans.load(Relaxed), d.hashes.hashed());
    let settled = counts();
    reconcile().await;
    assert_eq!(counts(), settled, "nothing changed: no plan, nothing hashed");

    let git = |args: &[&str]| sk1ll_core::git::Git::new(&root).args(args).run().unwrap();
    type Change<'a> = Box<dyn Fn() + 'a>;
    let changes: [(&str, Change, u64); 6] = [
        ("working tree", Box::new(|| std::fs::write(root.join("README.md"), "notes\n").unwrap()), 0),
        ("git", Box::new(|| drop(git(&["branch", "other"]))), 0),
        (
            "machine config",
            Box::new(|| {
                let text = std::fs::read_to_string(&d.cfg.machine_config).unwrap();
                std::fs::write(&d.cfg.machine_config, format!("{text}\n# edited\n")).unwrap();
            }),
            0,
        ),
        (
            "assignment",
            Box::new(|| {
                let next = AssignmentSnapshot { revision: 2, ..d.snapshot().unwrap().as_ref().clone() };
                *d.snapshot.write().unwrap() = Some(Arc::new(next));
            }),
            0,
        ),
        (
            "pull state",
            Box::new(|| {
                let pull = PullState::Blocked { baseline: "f00d".repeat(10), items: Vec::new() };
                d.note(|lc| lc.set_pull(pull));
            }),
            0,
        ),
        // The placement is read again: edited, it is a conflict.
        ("harness", Box::new(|| std::fs::write(&placed, "Edited in the harness.\n").unwrap()), 1),
    ];
    for (input, change, rehashed) in changes {
        let (plans, hashed) = counts();
        // A change within one tick of the clock could keep every timestamp.
        std::thread::sleep(std::time::Duration::from_millis(30));
        change();
        reconcile().await;
        assert_eq!(counts(), (plans + 1, hashed + rehashed), "{input}: one plan");
        reconcile().await;
        assert_eq!(counts(), (plans + 1, hashed + rehashed), "{input}: settled");
    }
    assert_eq!(std::fs::read_to_string(&placed).unwrap(), "Edited in the harness.\n");
    assert_eq!(d.view().report.conflicts.len(), 1);
}

/// A reconcile that failed records nothing: the next one runs again.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_reconcile_is_tried_again() {
    use std::sync::atomic::Ordering::Relaxed;
    let dir = tempfile::tempdir().unwrap();
    let (d, _session, _) = joined_daemon(dir.path(), PullRecord::default());
    std::fs::write(&d.cfg.machine_config, "not toml [").unwrap();
    for _ in 0..2 {
        assert!(d.blocking(|d, eng| d.reconcile(eng)).await.is_err());
    }
    assert!(lock(&d.engine).reconciled.is_none());
    assert_eq!(d.plans.load(Relaxed), 0);
}
