use std::sync::atomic::{AtomicBool, Ordering};

use sk1ll_core::sync::{self, Action, ActionKind, Item};
use sk1ll_proto::{AssignmentUpdate, AssignmentWrite};

use super::assignment::newer;
use super::pull::is_rejected_push;
use super::read::{DESCRIPTION_CAP, parse_batch, small_blob};
use super::*;
use crate::client::{ChangeKind, ConflictTarget, DraftEntry, HarnessChangeRequest};
use crate::state::PullState;
use sk1ll_core::ItemRef;
use sk1ll_core::git::plumbing;

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
        state_dir: dir.path().join("sk1ll-state"),
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
    let _hook = sk1ll_core::failpoints::once_shared(
        sk1ll_core::failpoints::Point::DaemonAfterSyncReview,
        &daemon.cfg.machine_config,
        move |_| {
            std::fs::remove_dir_all(failing_source).unwrap();
            std::fs::write(source, "changed after the fingerprint matched").unwrap();
        },
    );
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
        cfg.state_dir = dir.path().join("sk1ll-state");
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
        let state = DaemonState::new("a".into(), "http://127.0.0.1:1".into(), "t".into(), root);
        state::save_snapshot(&cfg.snapshot_path(), &snap, Some(&state.registration_key())).unwrap();
        let d = Daemon::new(cfg, state).unwrap();
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
    cfg.state_dir = dir.join("sk1ll-state");
    std::fs::write(
        &cfg.machine_config,
        format!(
            "[harnesses.h]\nskills = {:?}\nmode = \"copy\"\n{}",
            dir.join("h"),
            sk1ll_core::machine::presets().map(|p| format!("[harnesses.{p}]\nenabled = false\n")).collect::<String>()
        ),
    )
    .unwrap();
    let state = DaemonState { record, ..DaemonState::new("a".into(), "http://127.0.0.1:1".into(), "t".into(), root) };
    let snapshot = AssignmentSnapshot { revision: 1, ..Default::default() };
    state::save_snapshot(&cfg.snapshot_path(), &snapshot, Some(&state.registration_key())).unwrap();
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
    assert_eq!(d.applied().unwrap().revision, 2);
    assert_eq!(d.applied().unwrap().assignment, confirmed(2, false).unwrap().assignment);
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
    assert_eq!(d.applied().unwrap().revision, 3);
    let owner = d.ident.joined().ok().map(DaemonState::registration_key);
    assert_eq!(state::load_snapshot(&d.cfg.snapshot_path(), owner.as_deref()).unwrap().unwrap().revision, 3);

    let writer = d.clone();
    let write = tokio::spawn(async move { writer.write_assignment(harness_toggle(true)).await });
    let request = session.recv().await.unwrap();
    d.blocking(|d, eng| d.revoke(eng, "removed from the fleet")).await.unwrap();
    request.result.send(confirmed(4, true)).unwrap();
    assert_eq!(write.await.unwrap().unwrap().revision, 4);
    assert_eq!(d.applied().unwrap().revision, 3, "a removed Machine applies nothing");
}

/// The state file keeps the block and the server's Baseline, so a
/// restarted daemon starts Blocked and Behind, before any pull.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_remembers_the_block_and_the_server_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let (d, _session, state) = joined_daemon(dir.path(), PullRecord::default());
    d.blocking(|d, eng| d.reconcile(eng).map(drop)).await.unwrap();
    let head = plumbing::rev(d.repo_root(), "HEAD").unwrap();
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
    drop(restarted);
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
                let next = AssignmentSnapshot { revision: 2, ..d.applied().unwrap().as_ref().clone() };
                *d.applied.write().unwrap() = Some(Arc::new(next));
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

/// Readers answer from the Snapshot the last reconcile published, at once,
/// even while a reconcile or a mutation holds the engine mutex; the next
/// reconcile publishes a new one, and the old one stays as it was.
#[tokio::test(flavor = "multi_thread")]
async fn readers_answer_from_the_last_snapshot_while_the_engine_is_busy() {
    use crate::client::Client;
    let dir = tempfile::tempdir().unwrap();
    let (d, _session, _) = joined_daemon(dir.path(), PullRecord::default());
    let root = d.repo_root().to_path_buf();
    let skill = |name: &str| {
        std::fs::create_dir_all(root.join("personal").join(name)).unwrap();
        std::fs::write(root.join("personal").join(name).join("SKILL.md"), "A skill.\n").unwrap();
    };
    skill("pdf");
    let early = d.snapshot();
    assert_eq!(early.generation, 0);
    assert!(matches!(early.draft(), Err(Error::Unavailable(_))), "nothing reconciled yet");
    d.blocking(|d, eng| d.reconcile(eng)).await.unwrap();
    let first = d.snapshot();
    let names = |draft: Vec<DraftEntry>| draft.into_iter().map(|e| e.item.to_string()).collect::<Vec<_>>();
    assert_eq!(names(first.draft().unwrap()), ["skill:pdf"]);
    assert!(first.items().unwrap().iter().any(|i| i.item.to_string() == "skill:pdf" && i.id.is_some()));

    let socket = dir.path().join("daemon.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let served = d.clone();
    let server = tokio::spawn(async move { axum::serve(listener, crate::api::router(served)).await.unwrap() });
    let client = Client::new(&socket);

    // A long engine operation: it holds the mutex until told to finish.
    let (finish, finished) = std::sync::mpsc::channel::<()>();
    let (started, running) = tokio::sync::oneshot::channel();
    let engine = d.clone();
    let busy = tokio::spawn(async move {
        engine
            .blocking(move |_, _| {
                started.send(()).unwrap();
                finished.recv().unwrap();
                Ok(())
            })
            .await
    });
    running.await.unwrap();
    skill("notes");
    let quick = std::time::Duration::from_secs(5);
    let read = async {
        let draft = client.draft().await.unwrap();
        let items = client.items().await.unwrap();
        let conflicts = client.conflicts().await.unwrap();
        let recovered = client.recovered().await.unwrap();
        let status = client.status().await.unwrap();
        (draft, items, conflicts, recovered, status)
    };
    let (draft, items, conflicts, _, status) = tokio::time::timeout(quick, read).await.expect("a reader waited");
    assert_eq!(names(draft), ["skill:pdf"], "the Snapshot before the change");
    assert!(!items.iter().any(|i| i.item.to_string() == "skill:notes"));
    assert!(conflicts.is_empty());
    assert_eq!(status.report.drafts.len(), 1);
    // A caller about to act on the list asks for it as of now: it waits
    // for the engine, then sees the change.
    let fresh = tokio::spawn({
        let socket = socket.clone();
        async move { Client::new(&socket).draft_fresh().await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!fresh.is_finished(), "a fresh read waits for the engine");
    finish.send(()).unwrap();
    busy.await.unwrap().unwrap();
    assert_eq!(names(fresh.await.unwrap()), ["skill:notes", "skill:pdf"]);

    // The engine is free: a reader first brings the Snapshot up to date.
    assert_eq!(names(client.draft().await.unwrap()), ["skill:notes", "skill:pdf"]);
    let second = d.snapshot();
    assert!(second.generation > first.generation);
    assert_eq!(names(first.draft().unwrap()), ["skill:pdf"], "a published Snapshot never changes");
    // Writing the new skill's Item ID changed the tree: one more pass, then
    // nothing changes and no new Snapshot is published.
    d.blocking(|d, eng| d.reconcile(eng)).await.unwrap();
    let settled = d.snapshot();
    d.blocking(|d, eng| d.reconcile(eng)).await.unwrap();
    assert!(Arc::ptr_eq(&d.snapshot(), &settled));
    server.abort();
}

/// An unmanaged skill `pdf` in the harness `h` of a [`joined_daemon`].
fn unmanaged_pdf(dir: &Path) -> std::path::PathBuf {
    let source = dir.join("h/pdf");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("SKILL.md"), "original").unwrap();
    source
}

fn pdf() -> ConflictTarget {
    ConflictTarget { harness: "h".into(), name: "pdf".into() }
}

/// Answers the next Assignment write with `answer`, after running `meanwhile`.
fn answer_once(
    mut session: mpsc::Receiver<crate::server::SessionWrite>,
    meanwhile: impl FnOnce(&AssignmentWrite) + Send + 'static,
    answer: impl FnOnce(&AssignmentWrite) -> Result<AssignmentUpdate> + Send + 'static,
) -> tokio::task::JoinHandle<mpsc::Receiver<crate::server::SessionWrite>> {
    tokio::spawn(async move {
        let request = session.recv().await.unwrap();
        meanwhile(&request.write);
        request.result.send(answer(&request.write)).unwrap();
        session
    })
}

fn refused(_: &AssignmentWrite) -> Result<AssignmentUpdate> {
    Err(Error::Upstream(axum::http::StatusCode::UNPROCESSABLE_ENTITY, "Server refused".into()))
}

/// A refused Adopt takes back only the skill it published, as it published
/// it: not once it was committed meanwhile, nor once it has another Item ID.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_adoption_keeps_a_skill_committed_or_reidentified_meanwhile() {
    for scenario in ["committed", "new id"] {
        let dir = tempfile::tempdir().unwrap();
        let (d, session, _) = joined_daemon(dir.path(), PullRecord::default());
        let source = unmanaged_pdf(dir.path());
        let root = d.repo_root().to_path_buf();
        let published = root.join("personal/pdf");
        let server = answer_once(
            session,
            {
                let root = root.clone();
                move |_| {
                    let repo = Repo::open(&root).unwrap();
                    if scenario == "committed" {
                        let author = sk1ll_core::machine_author("a");
                        repo.commit(&[ItemRef::Skill("pdf".into())], "Add pdf", Some(&author)).unwrap();
                    } else {
                        std::fs::write(root.join("personal/pdf/.sk1ll-id"), "01HZZZZZZZZZZZZZZZZZZZZZZZ\n").unwrap();
                    }
                }
            },
            refused,
        );
        let error = d.adopt(pdf()).await.unwrap_err().to_string();
        server.await.unwrap();
        assert!(error.contains("Server refused") && error.contains("stays in the Draft"), "{scenario}: {error}");
        assert_eq!(std::fs::read_to_string(published.join("SKILL.md")).unwrap(), "original", "{scenario}");
        assert_eq!(std::fs::read_to_string(source.join("SKILL.md")).unwrap(), "original", "{scenario}");
        let draft = Repo::open(&root).unwrap().draft().unwrap();
        assert!(
            !draft.iter().any(|c| c.item == ItemRef::Skill("pdf".into()) && c.change == sk1ll_core::Change::Deleted),
            "{scenario}: no Draft deletion"
        );
        assert!(sync::displaced::list().unwrap().iter().all(|e| !e.path.starts_with(dir.path())), "{scenario}");
    }
}

/// A pull that became pending while Adopt's Assignment write was in flight
/// pauses sync: the unmanaged copies stay where they are.
#[tokio::test(flavor = "multi_thread")]
async fn an_adoption_confirmed_during_a_pending_pull_leaves_the_harness_alone() {
    let dir = tempfile::tempdir().unwrap();
    let (d, session, _) = joined_daemon(dir.path(), PullRecord::default());
    let source = unmanaged_pdf(dir.path());
    let journal = d.repo_root().join(".git/sk1ll-pull.json");
    let server = answer_once(
        session,
        {
            let journal = journal.clone();
            move |_| std::fs::write(&journal, "{}").unwrap()
        },
        |write| {
            let AssignmentWrite::Changes(body) = write else { panic!("expected toggles") };
            let mut assignment = sk1ll_proto::Assignment::default();
            assignment.set_skill("h", body.changes[0].item_id.as_ref().unwrap(), true);
            Ok(AssignmentUpdate { assignment, revision: 2, entries: vec![], errors: vec![] })
        },
    );
    let error = d.adopt(pdf()).await.unwrap_err().to_string();
    server.await.unwrap();
    assert!(error.contains("pull") && error.contains("kept"), "{error}");
    assert!(!dir.path().join("h").join(sync::INDEX_PATH).exists(), "the harness was not changed");
    assert!(!source.join(sync::PLACEMENT_MARKER).exists());
    assert_eq!(std::fs::read_to_string(source.join("SKILL.md")).unwrap(), "original");
    // Core refuses it too, whoever calls it.
    let repo = Repo::open(d.repo_root()).unwrap();
    let adoption = sync::unmanaged::Adoption {
        name: "pdf".into(),
        hash: sk1ll_core::hash::hash_dir(&source).unwrap(),
        locations: vec![sync::unmanaged::Location { harness: "h".into(), path: source.clone() }],
    };
    let harnesses = MachineConfig::load(&d.cfg.machine_config).unwrap().harnesses();
    assert!(matches!(sync::unmanaged::complete(&repo, &harnesses, &adoption), Err(sk1ll_core::Error::PullPending)));
    std::fs::remove_file(journal).unwrap();
}

/// A Harness rename's confirmation that arrives after the Machine was
/// removed and joined again belongs to the old registration: it must not
/// overwrite the new registration's Assignment snapshot.
#[tokio::test(flavor = "multi_thread")]
async fn a_stale_rename_confirmation_leaves_the_next_registrations_snapshot_alone() {
    use sk1ll_core::harness_rename::{RenameIntent, RenameRequest};
    let dir = tempfile::tempdir().unwrap();
    let (d, _session, state) = joined_daemon(dir.path(), PullRecord::default());
    let request = RenameRequest { from: "h".into(), to: "g".into(), replace: false };
    let intent = RenameIntent::prepare(
        &d.cfg.state_dir,
        &d.cfg.machine_config,
        &state.repo,
        request,
        &sk1ll_proto::Assignment::default(),
        Some(&state.registration_key()),
    )
    .unwrap();
    let world = d.blocking(|d, _| d.world()).await.unwrap();
    // Removed, then joined again: the next registration's files are stored.
    d.blocking(|d, eng| d.revoke(eng, "removed")).await.unwrap();
    let next = DaemonState::new("a".into(), "http://127.0.0.1:1".into(), "new".into(), state.repo.clone());
    let fresh = AssignmentSnapshot { revision: 5, ..Default::default() };
    let (cfg, saved) = (d.cfg.clone(), fresh.clone());
    let key = next.registration_key();
    d.halt_with(|| {
        state::save_snapshot(&cfg.snapshot_path(), &saved, Some(&key))?;
        next.save(&cfg.state).map(|()| true)
    })
    .unwrap();
    let stale = confirmed(9, true).unwrap();
    let finished = d.blocking(move |d, eng| d.finish_harness_rename(eng, world, intent, Ok(stale))).await;
    assert!(finished.is_err());
    assert_eq!(state::load_snapshot(&d.cfg.snapshot_path(), Some(&next.registration_key())).unwrap(), Some(fresh));
    assert_eq!(d.applied().unwrap().revision, 1);
}

/// The daemon for `dir`'s state again, as after a restart, with `record`.
fn restarted(d: Arc<Daemon>, state: &DaemonState, record: PullRecord) -> Arc<Daemon> {
    let state = DaemonState { record, ..state.clone() };
    state.save(&d.cfg.state).unwrap();
    let cfg = d.cfg.clone();
    drop(d);
    Daemon::new(cfg, state).unwrap()
}

/// A pull finished (the journal gone) but the process stopped before it
/// recorded that. A retry may have fetched a newer Baseline B meanwhile,
/// while the reconcile finished the pull onto A. The restart decides from A,
/// which `HEAD` contains: Clear, not Blocked on B with A's clash items; B is
/// left to the next pull.
#[tokio::test(flavor = "multi_thread")]
async fn a_finished_pull_recorded_as_pending_is_clear_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (d, _session, state) = joined_daemon(dir.path(), PullRecord::default());
    let root = d.repo_root().to_path_buf();
    let git = |args: &[&str]| sk1ll_core::git::Git::new(&root).args(args).text().unwrap();
    let a = git(&["rev-parse", "HEAD"]);
    let tree = git(&["rev-parse", "HEAD^{tree}"]);
    let commit =
        |m: &str| git(&["-c", "user.name=t", "-c", "user.email=t@example.invalid", "commit-tree", &tree, "-m", m]);
    let b = commit("b");
    git(&["update-ref", "refs/remotes/origin/main", &b]);
    let items = vec![ItemRef::Skill("pdf".into())];
    let pending = |baseline: &str| PullState::Pending { baseline: baseline.into(), items: items.clone() };
    let record = |pull| PullRecord { server_baseline: Some(b.clone()), pull, push_refused: None };

    let d = restarted(d, &state, record(pending(&a)));
    d.blocking(|d, eng| d.reconcile(eng).map(drop)).await.unwrap();
    assert_eq!(d.lifecycle().pull(), &PullState::Clear);
    assert_eq!(DaemonState::load(&d.cfg.state).unwrap().unwrap().record.pull, PullState::Clear);
    assert!(d.view().report.behind, "B is still to pull");

    // A pull onto a Baseline HEAD does not contain is still in the way.
    let d = restarted(d, &state, record(pending(&b)));
    d.blocking(|d, eng| d.reconcile(eng).map(drop)).await.unwrap();
    assert!(matches!(d.lifecycle().pull(), PullState::Blocked { baseline, .. } if *baseline == b));
}

/// A state file whose pull record another version wrote (a pending pull
/// without its Baseline) doesn't keep the daemon from starting: the record
/// is dropped, and the reconcile decides from the journal and `HEAD`.
#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_starts_over_an_unreadable_pull_record() {
    let dir = tempfile::tempdir().unwrap();
    let (d, _session, state) = joined_daemon(dir.path(), PullRecord::default());
    let mut json = serde_json::to_value(&state).unwrap();
    json["pull"] = serde_json::json!({ "state": "pending", "items": ["skill:pdf"] });
    std::fs::write(&d.cfg.state, json.to_string()).unwrap();
    let cfg = d.cfg.clone();
    drop(d);
    let loaded = DaemonState::load(&cfg.state).unwrap().expect("loads");
    assert_eq!(loaded.record.pull, PullState::Clear);
    let d = Daemon::new(cfg, loaded).unwrap();
    d.blocking(|d, eng| d.reconcile(eng).map(drop)).await.unwrap();
    assert_eq!(d.lifecycle().pull(), &PullState::Clear);
}

/// A join that registered while the old registration recovered (its token
/// works again) stores nothing: the daemon keeps its registration.
#[tokio::test(flavor = "multi_thread")]
async fn a_rejoin_does_not_replace_a_registration_that_recovered_meanwhile() {
    let dir = tempfile::tempdir().unwrap();
    let (d, _session, state) = joined_daemon(dir.path(), PullRecord::default());
    d.set_auth_rejected();
    let world = d.can_rejoin().expect("an auth-rejected Machine may join again");
    // The registration request is in flight; meanwhile the session connects.
    let next = DaemonState::new("b".into(), "http://127.0.0.1:1".into(), "new".into(), state.repo.clone());
    let reg = crate::join::Registered::for_test(next, AssignmentSnapshot { revision: 9, ..Default::default() });
    d.set_link(Link::Connected);
    let error = d.rejoin(reg, world).await.unwrap_err();
    assert!(error.to_string().contains("works again"), "{error}");
    assert_eq!(DaemonState::load(&d.cfg.state).unwrap().unwrap().token, state.token);
    let owner = d.ident.joined().ok().map(DaemonState::registration_key);
    assert_eq!(state::load_snapshot(&d.cfg.snapshot_path(), owner.as_deref()).unwrap().unwrap().revision, 1);
    assert!(d.ensure_managed().is_ok(), "still managing, not halted");
}

/// A status event must expose the rejection and its explanation together.
#[test]
fn auth_rejection_publishes_one_complete_status() {
    let dir = tempfile::tempdir().unwrap();
    let (d, _session, _) = joined_daemon(dir.path(), PullRecord::default());
    d.set_link(Link::Offline);
    d.set_error("an earlier repository error");
    let mut events = d.subscribe_events();

    d.set_auth_rejected();

    let view = d.view();
    assert_eq!(view.connection, crate::client::Connection::AuthRejected);
    assert!(view.auth_rejected_since.is_some());
    assert!(view.last_error.as_deref().unwrap().contains("401"));
    assert!(matches!(events.try_recv(), Ok(Event::Status)));
    assert!(matches!(events.try_recv(), Err(broadcast::error::TryRecvError::Empty)));
}

/// The session's connection can't slip in between the check that the old
/// registration is replaceable and the halt that replaces it: either it
/// connects first (and the join stores nothing) or the halt comes first (and
/// the connection is ignored).
#[tokio::test(flavor = "multi_thread")]
async fn a_rejoin_checks_and_halts_in_one_step() {
    let dir = tempfile::tempdir().unwrap();
    let (d, _session, state) = joined_daemon(dir.path(), PullRecord::default());
    d.set_auth_rejected();
    let world = d.can_rejoin().expect("an auth-rejected Machine may join again");
    let next = DaemonState::new("b".into(), "http://127.0.0.1:1".into(), "new".into(), state.repo.clone());
    let reg = crate::join::Registered::for_test(next, AssignmentSnapshot { revision: 9, ..Default::default() });
    let session = d.clone();
    let (tx, connected) = std::sync::mpsc::channel();
    let _hook = sk1ll_core::failpoints::once_shared(
        sk1ll_core::failpoints::Point::DaemonBeforeRejoinPersist,
        &d.cfg.machine_config,
        move |_| {
            std::thread::spawn(move || {
                session.set_link(Link::Connected);
                let _ = tx.send(());
            });
            // Gives the session every chance to connect inside the window.
            std::thread::sleep(std::time::Duration::from_millis(200));
        },
    );
    d.rejoin(reg, world).await.unwrap().ok();
    connected.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
    assert!(d.lifecycle().halted());
    assert_eq!(
        d.view().connection,
        crate::client::Connection::AuthRejected,
        "the replaced registration never showed as connected"
    );
}

/// Settles `d`, makes `change`, and returns how many plans the reconciles
/// after it ran: one when the change is an Input the fingerprint sees.
async fn plans_after(d: &Arc<Daemon>, change: impl FnOnce()) -> u64 {
    use std::sync::atomic::Ordering::Relaxed;
    let reconcile = || {
        let d = d.clone();
        async move { d.blocking(|d, eng| d.reconcile(eng)).await.unwrap() }
    };
    reconcile().await;
    reconcile().await;
    let before = d.plans.load(Relaxed);
    reconcile().await;
    assert_eq!(d.plans.load(Relaxed), before, "settled before the change");
    // A change within one tick of the clock could keep every timestamp.
    std::thread::sleep(std::time::Duration::from_millis(30));
    change();
    reconcile().await;
    reconcile().await;
    d.plans.load(Relaxed) - before
}

/// `personal/pdf` with a `SKILL.md`, its Item ID written.
fn pdf_skill(root: &Path) {
    std::fs::create_dir_all(root.join("personal/pdf")).unwrap();
    std::fs::write(root.join("personal/pdf/SKILL.md"), "PDFs.\n").unwrap();
    Repo::open(root).unwrap().assign_missing_ids().unwrap();
}

/// A `machine.toml` that is a symlink into a dotfiles repo is stamped
/// through the link: an edit of its target is an Input change.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_symlinked_machine_config_is_read_through_the_link() {
    let dir = tempfile::tempdir().unwrap();
    let (d, _session, _) = joined_daemon(dir.path(), PullRecord::default());
    let target = dir.path().join("dotfiles/machine.toml");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::rename(&d.cfg.machine_config, &target).unwrap();
    std::os::unix::fs::symlink(&target, &d.cfg.machine_config).unwrap();
    let edit = || {
        let text = std::fs::read_to_string(&target).unwrap();
        std::fs::write(&target, format!("{text}\n# edited\n")).unwrap();
    };
    assert_eq!(plans_after(&d, edit).await, 1);
}

/// A skill's own file named like sk1ll's staging directories is content
/// sync deploys, so an edit of it is seen; staging directories where sk1ll
/// creates them are still left out.
#[tokio::test(flavor = "multi_thread")]
async fn a_skill_file_named_like_a_stage_is_content() {
    let dir = tempfile::tempdir().unwrap();
    let (d, _session, _) = joined_daemon(dir.path(), PullRecord::default());
    let root = d.repo_root().to_path_buf();
    let own = root.join("personal/pdf/refs/.sk1ll-stage-notes.md");
    std::fs::create_dir_all(own.parent().unwrap()).unwrap();
    std::fs::write(&own, "Notes.\n").unwrap();
    pdf_skill(&root);
    // One plan for the edit, one for the placement it changed.
    assert_eq!(plans_after(&d, || std::fs::write(&own, "Edited notes.\n").unwrap()).await, 2);
    let placed = dir.path().join("h/pdf/refs/.sk1ll-stage-notes.md");
    assert_eq!(std::fs::read_to_string(placed).unwrap(), "Edited notes.\n");
    let stage = root.join(sk1ll_core::skill::IMPORTED_DIR).join(".sk1ll-stage-1-2-3/tree/SKILL.md");
    let staged = || {
        std::fs::create_dir_all(stage.parent().unwrap()).unwrap();
        std::fs::write(&stage, "Being staged.\n").unwrap();
    };
    assert_eq!(plans_after(&d, staged).await, 0, "an import being staged is no Input");
}

/// A harness changed within the racy window is not recorded, like any
/// other Input: the next reconcile runs again.
#[tokio::test(flavor = "multi_thread")]
async fn a_harness_changed_within_the_racy_window_is_reconciled_again() {
    let dir = tempfile::tempdir().unwrap();
    let (d, _session, _) = joined_daemon(dir.path(), PullRecord::default());
    pdf_skill(d.repo_root());
    for _ in 0..2 {
        d.blocking(|d, eng| d.reconcile(eng)).await.unwrap();
    }
    let window = std::time::Duration::from_millis(400);
    std::thread::sleep(2 * window);
    std::fs::write(dir.path().join("h/pdf/SKILL.md"), "Edited in the harness.\n").unwrap();
    let (_, racy) = reconcile::Inputs::gather(&d, window).unwrap();
    assert_eq!(racy, ["harness h"]);
    lock(&d.engine).racy = window;
    d.blocking(|d, eng| d.reconcile(eng)).await.unwrap();
    assert!(lock(&d.engine).reconciled.is_none(), "changed within the window: not recorded");
    std::thread::sleep(window);
    d.blocking(|d, eng| d.reconcile(eng)).await.unwrap();
    assert!(lock(&d.engine).reconciled.is_some());
}

/// With `.git` a pointer file (`--separate-git-dir`, a linked worktree),
/// the git directory it names and its common directory are the Inputs.
#[test]
fn a_git_pointer_is_followed_to_the_git_and_common_dirs() {
    use sk1ll_core::git::Git;
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main");
    Repo::init_committed(&main).unwrap();
    let git = |at: &Path, args: &[&str]| drop(Git::new(at).args(args).run().unwrap());
    git(&main, &["worktree", "add", "-q", "-b", "wt", dir.path().join("wt").to_str().unwrap()]);
    let separate = dir.path().join("separate");
    Repo::init_committed(&separate).unwrap();
    std::fs::rename(separate.join(".git"), dir.path().join("separate.git")).unwrap();
    std::fs::write(separate.join(".git"), "gitdir: ../separate.git\n").unwrap();
    type Change<'a> = Box<dyn Fn() + 'a>;
    let wt = dir.path().join("wt");
    let cases: [(&Path, &str, Change); 3] = [
        (&wt, "a branch, in the common dir", Box::new(|| git(&wt, &["branch", "other"]))),
        (&wt, "the worktree's own HEAD", Box::new(|| git(&wt, &["checkout", "-q", "-b", "moved"]))),
        (&separate, "a separate git dir's refs", Box::new(|| git(&separate, &["branch", "other"]))),
    ];
    for (root, what, change) in cases {
        let before = reconcile::git_dirs(root).unwrap();
        assert_eq!(reconcile::git_dirs(root).unwrap(), before, "{what}");
        std::thread::sleep(std::time::Duration::from_millis(30));
        change();
        assert_ne!(reconcile::git_dirs(root).unwrap(), before, "{what}");
    }
}

/// A standalone daemon for `dir/repo`, with a committed `pdf` skill.
fn standalone_with_pdf(dir: &Path) -> Arc<Daemon> {
    let root = dir.join("repo");
    Repo::init_committed(&root).unwrap();
    pdf_skill(&root);
    let cfg = Config {
        repo: Some(root.clone()),
        state: dir.join("state/daemon.json"),
        assignment: dir.join("assignment.json"),
        machine_config: dir.join("machine.toml"),
        state_dir: dir.join("sk1ll-state"),
        socket: dir.join("daemon.sock"),
        ..Config::new().unwrap()
    };
    let presets =
        sk1ll_core::machine::presets().map(|name| format!("[harnesses.{name}]\nenabled = false\n")).collect::<String>();
    std::fs::write(&cfg.machine_config, format!("id = \"m\"\n{presets}")).unwrap();
    Daemon::standalone(cfg).unwrap()
}

/// Every part of `d`'s Snapshot is an error, and the report is empty.
fn assert_without_repo(d: &Daemon) {
    let snapshot = d.snapshot();
    assert!(snapshot.items().is_err());
    assert!(snapshot.draft().is_err());
    assert!(snapshot.conflicts().is_err());
    assert!(snapshot.recovered().is_err());
    assert!(!d.view().repo_ready);
    assert!(d.view().report.items.is_empty());
}

/// A standalone daemon whose repo was moved away publishes a Snapshot
/// without it: readers stop answering with the vanished repo's items.
#[tokio::test(flavor = "multi_thread")]
async fn a_missing_standalone_repo_replaces_the_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let d = standalone_with_pdf(dir.path());
    let root = d.repo_root().to_path_buf();
    d.blocking(|d, eng| d.reconcile(eng)).await.unwrap();
    assert!(d.snapshot().items().unwrap().iter().any(|i| i.item.to_string() == "skill:pdf"));
    assert!(!d.view().report.items.is_empty());
    std::fs::rename(&root, dir.path().join("moved")).unwrap();
    d.blocking(|d, eng| d.reconcile(eng)).await.unwrap();
    assert!(matches!(d.snapshot().items(), Err(Error::Conflict(_))));
    assert_without_repo(&d);
}

/// A `.git` pointer file whose git directory was moved away is a missing
/// repo too; and a repo that can't be opened for another reason publishes
/// a Snapshot without it as well, rather than keeping the stale one.
#[tokio::test(flavor = "multi_thread")]
async fn a_repo_whose_git_dir_is_gone_replaces_the_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let d = standalone_with_pdf(dir.path());
    let root = d.repo_root().to_path_buf();
    let git_dir = dir.path().join("separate.git");
    std::fs::rename(root.join(".git"), &git_dir).unwrap();
    std::fs::write(root.join(".git"), format!("gitdir: {}\n", git_dir.display())).unwrap();
    d.blocking(|d, eng| d.reconcile(eng)).await.unwrap();
    assert!(d.snapshot().items().unwrap().iter().any(|i| i.item.to_string() == "skill:pdf"));

    std::fs::rename(&git_dir, dir.path().join("moved.git")).unwrap();
    d.blocking(|d, eng| d.reconcile(eng)).await.unwrap();
    assert!(matches!(d.snapshot().items(), Err(Error::Conflict(_))));
    assert_without_repo(&d);

    // Back, then unreadable: the reconcile fails, and says so to readers.
    std::fs::rename(dir.path().join("moved.git"), &git_dir).unwrap();
    d.blocking(|d, eng| d.reconcile(eng)).await.unwrap();
    assert!(d.snapshot().items().is_ok());
    std::fs::write(root.join(".git"), "not a pointer\n").unwrap();
    assert!(d.blocking(|d, eng| d.reconcile(eng)).await.is_err());
    assert_without_repo(&d);
}

/// The periodic full reconcile plans whatever the recorded Inputs say and
/// rehashes every tree: it puts back a placement whose change no stamp saw.
#[tokio::test(flavor = "multi_thread")]
async fn a_full_reconcile_corrects_what_the_fingerprint_missed() {
    use std::sync::atomic::Ordering::Relaxed;
    let dir = tempfile::tempdir().unwrap();
    let (d, _session, _) = joined_daemon(dir.path(), PullRecord::default());
    pdf_skill(d.repo_root());
    for _ in 0..2 {
        d.blocking(|d, eng| d.reconcile(eng)).await.unwrap();
    }
    let placed = dir.path().join("h/pdf");
    assert!(placed.join("SKILL.md").is_file());
    // A change the fingerprint missed: recorded as if seen.
    std::fs::remove_dir_all(&placed).unwrap();
    let (inputs, _) = reconcile::Inputs::gather(&d, std::time::Duration::ZERO).unwrap();
    lock(&d.engine).reconciled = Some(inputs);
    let plans = d.plans.load(Relaxed);
    d.blocking(|d, eng| d.reconcile(eng)).await.unwrap();
    assert_eq!(d.plans.load(Relaxed), plans, "stuck until the full reconcile");
    assert!(!placed.exists());
    let due = std::time::Instant::now().checked_sub(d.cfg.full_reconcile_every);
    lock(&d.engine).full_at = due;
    d.blocking(|d, eng| d.reconcile(eng)).await.unwrap();
    assert_eq!(d.plans.load(Relaxed), plans + 1);
    assert_eq!(std::fs::read_to_string(placed.join("SKILL.md")).unwrap(), "PDFs.\n");
    // Nothing is due again until the interval has passed once more.
    for _ in 0..2 {
        d.blocking(|d, eng| d.reconcile(eng)).await.unwrap();
    }
    let settled = (d.plans.load(Relaxed), d.hashes.hashed());
    d.blocking(|d, eng| d.reconcile(eng)).await.unwrap();
    assert_eq!((d.plans.load(Relaxed), d.hashes.hashed()), settled);
    // Then nothing changed, and every tree is read again all the same.
    lock(&d.engine).full_at = std::time::Instant::now().checked_sub(d.cfg.full_reconcile_every);
    d.blocking(|d, eng| d.reconcile(eng)).await.unwrap();
    assert_eq!(d.plans.load(Relaxed), settled.0 + 1);
    assert!(d.hashes.hashed() > settled.1 + 1, "the source and the placement, past the cache");
}

/// A conflict made a moment ago is not in the last Snapshot yet while an
/// engine operation runs: its diff waits for a fresh one instead of
/// answering that it no longer exists.
#[tokio::test(flavor = "multi_thread")]
async fn a_conflict_diff_while_the_engine_is_busy_waits_for_a_fresh_snapshot() {
    use crate::client::{Client, ConflictDiffRequest};
    let dir = tempfile::tempdir().unwrap();
    let (d, _session, _) = joined_daemon(dir.path(), PullRecord::default());
    pdf_skill(d.repo_root());
    d.blocking(|d, eng| d.reconcile(eng)).await.unwrap();
    let placed = dir.path().join("h/pdf/SKILL.md");
    assert_eq!(std::fs::read_to_string(&placed).unwrap(), "PDFs.\n");
    let socket = dir.path().join("daemon.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let served = d.clone();
    let server = tokio::spawn(async move { axum::serve(listener, crate::api::router(served)).await.unwrap() });

    let (finish, finished) = std::sync::mpsc::channel::<()>();
    let (started, running) = tokio::sync::oneshot::channel();
    let engine = d.clone();
    let busy = tokio::spawn(async move {
        engine
            .blocking(move |_, _| {
                started.send(()).unwrap();
                finished.recv().unwrap();
                Ok(())
            })
            .await
    });
    running.await.unwrap();
    std::fs::write(&placed, "Harness edit\n").unwrap();
    let request = ConflictDiffRequest { harness: Some("h".into()), name: Some("pdf".into()), id: None };
    let diff = tokio::spawn(async move { Client::new(&socket).conflict_diff(&request).await });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!diff.is_finished(), "the diff waits for the engine");
    finish.send(()).unwrap();
    busy.await.unwrap().unwrap();
    let diff = diff.await.unwrap().unwrap().diff;
    assert!(diff.contains("-Harness edit") && diff.contains("+PDFs."), "{diff}");
    server.abort();
}

/// A real server on a runtime of its own, and an engine that connects to
/// it on demand (as the CLI's does) for a Machine `a` joined with its admin
/// token: one harness `h`, and a skill `x` with an Item ID in its Draft.
struct OnDemand {
    d: Arc<Daemon>,
    url: String,
    http: reqwest::Client,
    _dir: tempfile::TempDir,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

const ADMIN: &str = "admin-secret";

impl OnDemand {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (port_tx, port_rx) = std::sync::mpsc::channel();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let data = dir.path().join("server");
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                port_tx.send(listener.local_addr().unwrap().port()).unwrap();
                let mut config = sk1ll_server::Config::new(data);
                config.admin_token = Some(ADMIN.into());
                tokio::select! {
                    r = sk1ll_server::serve(config, listener) => r.unwrap(),
                    _ = stopped => {}
                }
            });
        });
        let url = format!("http://127.0.0.1:{}", port_rx.recv().unwrap());
        let http = reqwest::Client::new();
        for _ in 0..200 {
            if http.get(format!("{url}/healthz")).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let root = dir.path().join("repo");
        let cfg = Config {
            repo: Some(root.clone()),
            state: dir.path().join("state/daemon.json"),
            assignment: dir.path().join("assignment.json"),
            machine_config: dir.path().join("machine.toml"),
            state_dir: dir.path().join("sk1ll-state"),
            socket: dir.path().join("daemon.sock"),
            ..Config::new().unwrap()
        };
        let presets = sk1ll_core::machine::presets().map(|p| format!("[harnesses.{p}]\nenabled = false\n"));
        std::fs::write(
            &cfg.machine_config,
            format!("id = \"a\"\n[harnesses.h]\nskills = {:?}\nmode = \"copy\"\n{}", dir.path().join("h"), {
                presets.collect::<String>()
            }),
        )
        .unwrap();
        let opts = crate::JoinOptions {
            server_url: url.clone(),
            code: String::new(),
            admin_token: Some(ADMIN.into()),
            machine_id: Some("a".into()),
            copy_from: None,
            repo: root.clone(),
        };
        crate::join(&cfg, opts).await.unwrap();
        std::fs::create_dir_all(root.join("personal/x")).unwrap();
        std::fs::write(root.join("personal/x/SKILL.md"), "x\n").unwrap();
        std::fs::write(root.join("personal/x/.sk1ll-id"), "id-x\n").unwrap();
        let state = DaemonState::load(&cfg.state).unwrap();
        let lock = crate::MachineLock::acquire(&cfg).unwrap();
        let d = Daemon::for_machine(cfg, state, super::Access::OnDemand(lock)).unwrap();
        Self { d, url, http, _dir: dir, stop: Some(stop), thread: Some(thread) }
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> T {
        let resp = self.http.get(format!("{}/api{path}", self.url)).bearer_auth(ADMIN).send().await.unwrap();
        assert!(resp.status().is_success(), "GET {path}: {}", resp.status());
        resp.json().await.unwrap()
    }
}

impl Drop for OnDemand {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// An engine that connects on demand starts the session for the write,
/// reports its inventory first (the server materializes defaults by it) and
/// stores the confirmed snapshot.
#[tokio::test(flavor = "multi_thread")]
async fn an_on_demand_engine_connects_reports_and_stores_its_write() {
    let fleet = OnDemand::start().await;
    let d = &fleet.d;
    assert!(!d.is_connected(), "nothing connects before a request needs it");
    let x: sk1ll_core::ItemId = "id-x".parse().unwrap();
    let toggle = sk1ll_proto::ToggleChange {
        harness: "h".into(),
        kind: sk1ll_proto::ToggleKind::Skill,
        item_id: Some(x.clone()),
        name: Some("x".into()),
        enabled: Some(false),
    };
    let write = AssignmentWrite::Changes(sk1ll_proto::AssignmentChanges { changes: vec![toggle], expected: None });
    let update = d.write_assignment(write).await.unwrap();
    assert!(d.is_connected());
    let fleet_view: sk1ll_proto::FleetView = fleet.get("/fleet").await;
    let report = fleet_view.machines.into_iter().find(|m| m.id == "a").unwrap().report.unwrap();
    assert!(report.items.iter().any(|item| item.id == x), "{report:?}");
    let owner = d.ident.joined().unwrap().registration_key();
    let stored = state::load_snapshot(&d.cfg.snapshot_path(), Some(&owner)).unwrap().unwrap();
    assert_eq!(stored.revision, update.revision);
    assert!(!stored.assignment.skill_enabled("h", &x));
    assert_eq!(d.applied().unwrap().revision, update.revision);
}

/// A rename is prepared from the Assignment the server holds, not from a
/// stale local one: a destination stored there meanwhile is protected.
#[tokio::test(flavor = "multi_thread")]
async fn an_on_demand_rename_protects_a_destination_only_the_server_has() {
    use sk1ll_core::harness_rename::RenameIntent;
    let fleet = OnDemand::start().await;
    let d = &fleet.d;
    let saved: AssignmentUpdate = fleet
        .http
        .put(format!("{}/api/machines/a/assignment", fleet.url))
        .bearer_auth(ADMIN)
        .json(&serde_json::json!({"changes":[{
            "harness":"already-stored","kind":"skill","item_id":"absent","enabled":true
        }]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(d.applied().is_none_or(|s| !s.assignment.harnesses.contains_key("already-stored")));
    let before = std::fs::read(&d.cfg.machine_config).unwrap();
    let request = crate::client::HarnessRenameRequest { from: "h".into(), to: "already-stored".into(), replace: false };
    let error = d.rename_harness(request).await.unwrap_err();
    assert!(error.to_string().contains("still has an Assignment"), "{error}");
    assert_eq!(std::fs::read(&d.cfg.machine_config).unwrap(), before);
    assert!(!RenameIntent::exists(&d.cfg.state_dir, &d.cfg.machine_config).unwrap());
    let snapshot: AssignmentSnapshot = fleet.get("/machines/a/assignment").await;
    assert_eq!((snapshot.revision, snapshot.assignment), (saved.revision, saved.assignment));
}

/// Adds copy-mode harnesses to `d`'s `machine.toml`.
fn add_harnesses(d: &Daemon, names: &[&str]) {
    let mut config = std::fs::read_to_string(&d.cfg.machine_config).unwrap();
    for name in names {
        let skills = d.cfg.machine_config.with_file_name(name);
        config.push_str(&format!("[harnesses.{name}]\nskills = {:?}\nmode = \"copy\"\n", skills.to_str().unwrap()));
    }
    std::fs::write(&d.cfg.machine_config, config).unwrap();
}

/// A daemon that starts while a command's engine (for any repo) is making
/// a toggle waits for it to end, then starts from what it saved: it never
/// loads revision N before the command persists N+1 and reconciles with it.
#[tokio::test(flavor = "multi_thread")]
async fn a_starting_daemon_waits_for_a_commands_toggle_and_starts_from_it() {
    let dir = tempfile::tempdir().unwrap();
    let command = standalone_with_pdf(dir.path());
    add_harnesses(&command, &["h"]);
    let cfg = command.cfg.clone();
    let before = command.applied().unwrap().revision;
    let starting = tokio::spawn(Daemon::start(cfg.clone(), std::time::Duration::from_secs(20)));
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!starting.is_finished(), "the daemon waits for the command's engine");
    let update = command.write_assignment(harness_toggle(false)).await.unwrap();
    assert_eq!(update.revision, before + 1);
    drop(command);
    let daemon = tokio::time::timeout(std::time::Duration::from_secs(10), starting).await.unwrap().unwrap().unwrap();
    let applied = daemon.applied().unwrap();
    assert_eq!(applied.revision, update.revision);
    assert!(!applied.assignment.harness("h").new_skills_default, "the toggle is applied");
    assert_eq!(
        state::load_snapshot(&cfg.state.with_file_name("standalone-assignment.json"), None).unwrap().unwrap().revision,
        update.revision
    );
}

/// A machine-local toggle and a hand edit of the file that lands while the
/// toggle is made: the toggle is made again on top of the edit, so both
/// are kept, each a revision of its own, and what it reports is saved.
#[tokio::test(flavor = "multi_thread")]
async fn a_toggle_racing_a_hand_edit_keeps_both() {
    let dir = tempfile::tempdir().unwrap();
    let d = standalone_with_pdf(dir.path());
    add_harnesses(&d, &["h", "g"]);
    let base = d.applied().unwrap().revision;
    let file = d.cfg.assignment.clone();
    let edit = file.clone();
    let _hook =
        sk1ll_core::failpoints::once_shared(sk1ll_core::failpoints::Point::DaemonStoreBeforeSwap, &file, move |_| {
            let mut hand = sk1ll_core::Assignment::load(&edit).unwrap();
            hand.set_harness("g", false);
            hand.save(&edit).unwrap();
        });
    let update = d.write_assignment(harness_toggle(false)).await.unwrap();
    let saved = sk1ll_core::Assignment::load(&file).unwrap();
    assert!(!saved.harness("h").new_skills_default, "the toggle is saved");
    assert!(!saved.harness("g").new_skills_default, "and the hand edit kept");
    assert_eq!(update.assignment, saved);
    assert_eq!(update.revision, base + 2, "the edit is one revision, the toggle the next");
    let stored =
        state::load_snapshot(&d.cfg.state.with_file_name("standalone-assignment.json"), None).unwrap().unwrap();
    assert_eq!((stored.revision, &stored.assignment), (update.revision, &saved));
    assert_eq!(d.applied().unwrap().revision, update.revision);
}

/// Reading the Assignment never finishes a pending machine-local rename:
/// only a reconcile, under the machine lock, does.
#[tokio::test(flavor = "multi_thread")]
async fn reading_the_assignment_leaves_a_pending_rename_alone() {
    use sk1ll_core::harness_rename::{RenameIntent, RenameRequest};
    let dir = tempfile::tempdir().unwrap();
    let d = standalone_with_pdf(dir.path());
    add_harnesses(&d, &["h"]);
    let mut assignment = sk1ll_core::Assignment::default();
    assignment.set_harness("h", false);
    assignment.save(&d.cfg.assignment).unwrap();
    let config = d.cfg.machine_config.clone();
    let request = RenameRequest { from: "h".into(), to: "renamed".into(), replace: false };
    RenameIntent::prepare(&d.cfg.state_dir, &config, d.repo_root(), request, &assignment, None)
        .unwrap()
        .apply_config(&config)
        .unwrap();
    let (config_before, file_before) = (std::fs::read(&config).unwrap(), std::fs::read(&d.cfg.assignment).unwrap());
    let current = d.current_assignment().unwrap();
    let (harnesses, planned) = d.preview_inputs().unwrap();
    assert!(current.harnesses.contains_key("h"), "the file as it is");
    assert!(harnesses.contains_key("renamed") && !planned.harness("renamed").new_skills_default);
    assert!(RenameIntent::exists(&d.cfg.state_dir, &config).unwrap());
    assert_eq!(std::fs::read(&config).unwrap(), config_before);
    assert_eq!(std::fs::read(&d.cfg.assignment).unwrap(), file_before);
    d.blocking(|d, eng| d.reconcile(eng).map(drop)).await.unwrap();
    assert!(!RenameIntent::exists(&d.cfg.state_dir, &config).unwrap(), "a reconcile finishes it");
}

/// A pending Harness rename is kept under `machine.toml`'s real path: one
/// that stopped half-way under one name for the file (a symlink into a
/// dotfiles directory) is finished by an engine that knows it by another.
#[tokio::test(flavor = "multi_thread")]
async fn a_rename_stopped_under_one_name_is_finished_through_another() {
    use sk1ll_core::harness_rename::{RenameIntent, RenameRequest};
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let d = standalone_with_pdf(dir.path());
    add_harnesses(&d, &["h"]);
    let cfg = d.cfg.clone();
    drop(d);
    // `machine.toml` lives in a dotfiles directory, known by two names.
    let dotfiles = dir.path().join("dotfiles");
    std::fs::create_dir(&dotfiles).unwrap();
    std::fs::rename(&cfg.machine_config, dotfiles.join("machine.toml")).unwrap();
    let (a, b) = (dir.path().join("a.toml"), dir.path().join("b.toml"));
    symlink(dotfiles.join("machine.toml"), &a).unwrap();
    symlink("dotfiles", dir.path().join("dots")).unwrap();
    symlink("dots/machine.toml", &b).unwrap();
    let mut assignment = sk1ll_core::Assignment::default();
    assignment.set_harness("h", false);
    assignment.save(&cfg.assignment).unwrap();

    // Started through `a`, stopped after `machine.toml`, before the Assignment.
    let request = RenameRequest { from: "h".into(), to: "renamed".into(), replace: false };
    let repo = cfg.repo.clone().unwrap();
    RenameIntent::prepare(&cfg.state_dir, &a, &repo, request, &assignment, None).unwrap().apply_config(&a).unwrap();
    assert!(MachineConfig::load(&b).unwrap().harnesses().contains_key("renamed"));
    assert!(sk1ll_core::Assignment::load(&cfg.assignment).unwrap().harnesses.contains_key("h"));
    assert!(RenameIntent::exists(&cfg.state_dir, &b).unwrap(), "seen through the other name");
    assert_eq!(RenameIntent::path(&cfg.state_dir, &a).unwrap(), RenameIntent::path(&cfg.state_dir, &b).unwrap());
    // Nothing of it is kept beside `machine.toml`.
    assert_eq!(std::fs::read_dir(&dotfiles).unwrap().count(), 1);

    // An engine knowing it as `b` finishes it.
    let d = Daemon::standalone(Config { machine_config: b.clone(), ..cfg.clone() }).unwrap();
    d.blocking(|d, eng| d.reconcile(eng).map(drop)).await.unwrap();
    assert!(!RenameIntent::exists(&cfg.state_dir, &a).unwrap());
    let saved = sk1ll_core::Assignment::load(&cfg.assignment).unwrap();
    assert!(!saved.harnesses.contains_key("h"));
    assert!(!saved.harness("renamed").new_skills_default, "the toggles moved with the name");
    assert!(MachineConfig::load(&a).unwrap().harnesses().contains_key("renamed"));
    assert_eq!(std::fs::read_dir(&dotfiles).unwrap().count(), 1);
}

/// What the CLI's previews ask a running daemon for (`status`, `list`,
/// `render`, `sync --dry-run`, `diff`, `edit`, `harnesses`) only reads: it
/// never reconciles, so a new skill keeps going without its Item ID until
/// something that changes the repo runs. `?fresh=true` does catch up.
#[tokio::test(flavor = "multi_thread")]
async fn previews_through_the_daemon_never_reconcile() {
    use std::sync::atomic::Ordering::Relaxed;
    let dir = tempfile::tempdir().unwrap();
    let d = standalone_with_pdf(dir.path());
    add_harnesses(&d, &["h"]);
    for _ in 0..2 {
        d.blocking(|d, eng| d.reconcile(eng)).await.unwrap();
    }
    let new = d.repo_root().join("personal/new");
    std::fs::create_dir_all(&new).unwrap();
    std::fs::write(new.join("SKILL.md"), "New.\n").unwrap();
    let id = new.join(sk1ll_core::skill::ID_FILE);
    let client = crate::client::Client::in_process(crate::api::router(d.clone()));
    let plans = d.plans.load(Relaxed);
    let repo = client.repo(false).await.unwrap();
    assert!(repo.skills.iter().any(|s| s.name == "new" && s.id.is_none()), "read as it is");
    client.preview_sync().await.unwrap();
    client.skills().await.unwrap();
    client.harnesses().await.unwrap();
    client.harness_settings().await.unwrap();
    client.render_instructions("h").await.unwrap();
    client.diff("skill:new").await.unwrap();
    client.editable("new").await.unwrap();
    assert_eq!(d.plans.load(Relaxed), plans, "a preview reconciled");
    assert!(!id.exists(), "a preview wrote an Item ID");
    let repo = client.repo(true).await.unwrap();
    assert!(repo.skills.iter().any(|s| s.name == "new" && s.id.is_some()), "a fresh read catches up");
    assert!(id.exists());
}

/// `g` switched off in the Assignment file, as a hand edit.
fn hand_edit(file: &Path, harness: &'static str) -> impl FnOnce() + Send + 'static {
    let file = file.to_path_buf();
    move || {
        let mut hand = sk1ll_core::Assignment::load(&file).unwrap();
        hand.set_harness(harness, false);
        hand.save(&file).unwrap();
    }
}

fn revision_record(d: &Daemon) -> std::path::PathBuf {
    d.cfg.state.with_file_name("standalone-assignment.json")
}

/// Taking up a hand edit of the machine-local file writes only its
/// revision, and only if the file still holds what was read: a second hand
/// edit landing in between is kept, never replaced by the first.
#[tokio::test(flavor = "multi_thread")]
async fn taking_up_a_hand_edit_never_replaces_a_newer_one() {
    let dir = tempfile::tempdir().unwrap();
    let d = standalone_with_pdf(dir.path());
    add_harnesses(&d, &["h", "g"]);
    let base = d.applied().unwrap().revision;
    let file = d.cfg.assignment.clone();
    hand_edit(&file, "h")();
    let edit = hand_edit(&file, "g");
    let _hook =
        sk1ll_core::failpoints::once_shared(sk1ll_core::failpoints::Point::DaemonStoreBeforeSwap, &file, move |_| {
            edit()
        });
    d.blocking(|d, _| d.refresh_assignment()).await.unwrap();
    let saved = sk1ll_core::Assignment::load(&file).unwrap();
    assert!(!saved.harness("h").new_skills_default && !saved.harness("g").new_skills_default, "both edits kept");
    let applied = d.applied().unwrap();
    assert_eq!((applied.revision, &applied.assignment), (base + 1, &saved), "taken up as one revision");
    let stored = state::load_snapshot(&revision_record(&d), None).unwrap().unwrap();
    assert_eq!((stored.revision, &stored.assignment), (base + 1, &saved));
}

/// A daemon starting over a hand edit made while no engine ran records it
/// as a revision, without replacing a second edit that lands meanwhile; and
/// one starting over nothing new writes nothing at all.
#[tokio::test(flavor = "multi_thread")]
async fn a_starting_daemon_writes_back_nothing_it_loaded() {
    use std::os::unix::fs::MetadataExt;
    let dir = tempfile::tempdir().unwrap();
    let d = standalone_with_pdf(dir.path());
    add_harnesses(&d, &["h", "g"]);
    let (cfg, base) = (d.cfg.clone(), d.applied().unwrap().revision);
    let record = revision_record(&d);
    drop(d);
    let stamp = |path: &Path| std::fs::metadata(path).map(|m| (m.ino(), m.mtime_nsec(), m.len())).ok();
    let before = (stamp(&cfg.assignment), stamp(&record));
    let d = Daemon::standalone(cfg.clone()).unwrap();
    assert_eq!((stamp(&cfg.assignment), stamp(&record)), before, "nothing changed, nothing written");
    assert_eq!(d.applied().unwrap().revision, base);
    drop(d);

    hand_edit(&cfg.assignment, "h")();
    let edit = hand_edit(&cfg.assignment, "g");
    let _hook = sk1ll_core::failpoints::once_shared(
        sk1ll_core::failpoints::Point::DaemonStoreBeforeSwap,
        &cfg.assignment,
        move |_| edit(),
    );
    let d = Daemon::standalone(cfg.clone()).unwrap();
    let saved = sk1ll_core::Assignment::load(&cfg.assignment).unwrap();
    assert!(!saved.harness("h").new_skills_default && !saved.harness("g").new_skills_default, "both edits kept");
    let applied = d.applied().unwrap();
    assert_eq!((applied.revision, &applied.assignment), (base + 1, &saved));
    let stored = state::load_snapshot(&record, None).unwrap().unwrap();
    assert_eq!((stored.revision, &stored.assignment), (base + 1, &saved));
}

/// `POST /harnesses/take-out` turns the harness's items off, lets sync take
/// out what it placed, and only then disables the harness; a stale preview
/// turns nothing off, and a removal that fails leaves `machine.toml` as it
/// is and the items off, so a retry finishes it.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn take_out_disables_a_harness_only_once_its_placements_are_gone() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let d = standalone_with_pdf(dir.path());
    add_harnesses(&d, &["h"]);
    d.write_assignment(harness_toggle(true)).await.unwrap();
    let skills = d.cfg.machine_config.with_file_name("h");
    assert!(skills.join("pdf/SKILL.md").exists());
    let request = |fingerprint: Option<String>| HarnessChangeRequest {
        change: sk1ll_core::HarnessChange::Disable { name: "h".into() },
        fingerprint,
    };
    let preview = d.preview_harness(&request(None).change).unwrap();
    assert_eq!(preview.placed.iter().filter(|p| p.kind == "remove").count(), 1, "{preview:?}");
    let enabled = || d.harnesses().unwrap().iter().any(|h| h.name == "h");

    let stale = d.take_out_harness(request(Some("stale".into()))).await.unwrap_err();
    assert!(stale.to_string().contains("preview again"), "{stale}");
    assert!(d.applied().unwrap().assignment.harness("h").new_skills_default, "nothing turned off");

    std::fs::set_permissions(&skills, std::fs::Permissions::from_mode(0o555)).unwrap();
    let failed = d.take_out_harness(request(Some(preview.fingerprint.clone()))).await.unwrap();
    std::fs::set_permissions(&skills, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!failed.applied && failed.left.len() == 1 && !failed.errors.is_empty(), "{failed:?}");
    assert!(enabled(), "machine.toml unchanged");
    assert!(!d.applied().unwrap().assignment.harness("h").new_skills_default, "the items stay off");

    let done = d.take_out_harness(request(Some(preview.fingerprint))).await.unwrap();
    assert!(done.applied && done.left.is_empty(), "{done:?}");
    assert_eq!(done.taken_out.len(), 1);
    assert!(!skills.join("pdf").exists(), "taken out");
    assert!(!enabled(), "disabled");
}

/// Only a "keep mine" run moves a Draft file into recovery: a Blocked pull
/// state on the edited item, without a keep journal, leaves every
/// reconcile's hands off the Draft.
#[tokio::test(flavor = "multi_thread")]
async fn a_blocked_reconcile_without_a_keep_leaves_the_draft_alone() {
    let dir = tempfile::tempdir().unwrap();
    let d = standalone_with_pdf(dir.path());
    let skill = d.repo_root().join("personal/pdf/SKILL.md");
    std::fs::write(&skill, "my edit\n").unwrap();
    let pdf = ItemRef::Skill("pdf".into());
    d.note(|lc| lc.set_pull(PullState::Blocked { baseline: "f00d".repeat(10), items: vec![pdf.clone()] }));
    for _ in 0..3 {
        d.blocking(|d, eng| d.reconcile(eng).map(drop)).await.unwrap();
    }
    assert_eq!(std::fs::read_to_string(&skill).unwrap(), "my edit\n");
    let own: Vec<_> = sync::displaced::list().unwrap().into_iter().filter(|e| e.dest.starts_with(dir.path())).collect();
    assert!(own.is_empty(), "{own:?}");
    assert!(d.snapshot().draft().unwrap().iter().any(|e| e.item == pdf));
}

/// A standalone daemon with harness `h` holding a copy of `pdf`, and the
/// request that disables `h` after taking it out, as previewed.
async fn placed_in_h(dir: &Path) -> (Arc<Daemon>, std::path::PathBuf, HarnessChangeRequest) {
    let d = standalone_with_pdf(dir);
    add_harnesses(&d, &["h"]);
    d.write_assignment(harness_toggle(true)).await.unwrap();
    let skills = d.cfg.machine_config.with_file_name("h");
    assert!(skills.join("pdf/SKILL.md").exists());
    let change = sk1ll_core::HarnessChange::Disable { name: "h".into() };
    let fingerprint = Some(d.preview_harness(&change).unwrap().fingerprint);
    (d, skills, HarnessChangeRequest { change, fingerprint })
}

fn h_enabled(d: &Daemon) -> bool {
    d.harnesses().unwrap().iter().any(|h| h.name == "h")
}

/// A scan that can't list what is placed is not an empty harness: nothing
/// changes in `machine.toml`, the scan error is reported, the items stay off.
#[tokio::test(flavor = "multi_thread")]
async fn take_out_needs_a_complete_scan_before_it_changes_machine_toml() {
    let dir = tempfile::tempdir().unwrap();
    let (d, skills, request) = placed_in_h(dir.path()).await;
    let index = skills.join(".sk1ll-state/index.json");
    assert!(index.exists());
    std::fs::write(&index, "not an index").unwrap();
    let failed = d.take_out_harness(request).await.unwrap();
    assert!(!failed.applied, "{failed:?}");
    assert!(failed.errors.iter().any(|e| e.contains("Could not list old placements")), "{failed:?}");
    assert!(h_enabled(&d), "machine.toml unchanged");
    assert!(!d.applied().unwrap().assignment.harness("h").new_skills_default, "the items stay off");
}

/// A hand edit of `machine.toml` while a take-out runs: nothing changes
/// there, and the errors say what was already done.
#[tokio::test(flavor = "multi_thread")]
async fn take_out_reports_what_it_did_when_machine_toml_changed_meanwhile() {
    let dir = tempfile::tempdir().unwrap();
    let (d, skills, request) = placed_in_h(dir.path()).await;
    let config = d.cfg.machine_config.clone();
    let _hook = sk1ll_core::failpoints::once_shared(
        sk1ll_core::failpoints::Point::DaemonBeforeTakeOutApply,
        &config.clone(),
        move |_| {
            let text = std::fs::read_to_string(&config).unwrap();
            std::fs::write(&config, format!("{text}\n# edited by hand\n")).unwrap();
        },
    );
    let done = d.take_out_harness(request).await.unwrap();
    assert!(!done.applied && done.left.is_empty(), "{done:?}");
    assert_eq!(done.taken_out.len(), 1);
    assert_eq!(
        done.errors,
        ["machine.toml changed since the preview. h's items were turned off and 1 placed item was taken out; h is \
          still on. Preview again"]
    );
    assert!(!skills.join("pdf").exists());
    assert!(h_enabled(&d), "still on");
}

/// Harness changes through the daemon wait for a take-out in progress:
/// while one holds the lock, an apply polled to its first wait has not
/// entered its protected step; once the take-out ends, it does.
#[tokio::test(flavor = "multi_thread")]
async fn harness_changes_wait_for_a_take_out() {
    let dir = tempfile::tempdir().unwrap();
    let (d, _, request) = placed_in_h(dir.path()).await;
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let (proceed, proceed_rx) = std::sync::mpsc::channel::<()>();
    let _entered = sk1ll_core::failpoints::once_shared(
        sk1ll_core::failpoints::Point::DaemonTakeOutEntered,
        &d.cfg.machine_config,
        move |_| {
            let _ = entered_tx.send(());
            let _ = proceed_rx.recv();
        },
    );
    let taking_out = tokio::spawn({
        let (d, request) = (d.clone(), request.clone());
        async move { d.take_out_harness(request).await }
    });
    entered.await.unwrap();
    let (applied_tx, applied) = std::sync::mpsc::channel();
    let _applied = sk1ll_core::failpoints::once_shared(
        sk1ll_core::failpoints::Point::DaemonApplyEntered,
        &d.cfg.machine_config,
        move |_| {
            let _ = applied_tx.send(());
        },
    );
    let mut applying = std::pin::pin!(d.apply_harness_serialized(request));
    tokio::select! {
        biased;
        _ = &mut applying => panic!("applied while a take-out holds the lock"),
        _ = std::future::ready(()) => {}
    }
    assert!(applied.try_recv().is_err(), "the apply entered while the take-out held the lock");
    proceed.send(()).unwrap();
    assert!(taking_out.await.unwrap().unwrap().applied);
    // The take-out changed machine.toml since this request's preview.
    assert!(applying.await.is_err());
    assert!(applied.try_recv().is_ok(), "the apply ran once the take-out was done");
}

/// The write scans again: a scan that fails there writes nothing, and
/// the result still says what the take-out did.
#[tokio::test(flavor = "multi_thread")]
async fn take_out_writes_only_from_a_complete_scan_of_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let (d, skills, request) = placed_in_h(dir.path()).await;
    // Another skill is placed, then its index entry made unreadable, after the last check.
    let index = skills.join(".sk1ll-state/index.json");
    let _hook = sk1ll_core::failpoints::once_shared(
        sk1ll_core::failpoints::Point::DaemonBeforeTakeOutWrite,
        &d.cfg.machine_config,
        move |_| std::fs::write(&index, "not an index").unwrap(),
    );
    let done = d.take_out_harness(request).await.unwrap();
    assert!(!done.applied, "{done:?}");
    assert!(done.errors.iter().any(|e| e.contains("machine.toml was not changed")), "{done:?}");
    assert!(
        done.errors.iter().any(|e| e.contains("h's items were turned off and 1 placed item was taken out")),
        "{done:?}"
    );
    assert!(h_enabled(&d), "machine.toml unchanged");
    assert!(!d.applied().unwrap().assignment.harness("h").new_skills_default, "the items stay off");
}

/// A hand edit that removes the harness while a take-out runs: the result
/// still says what was done, and lists the harnesses as they are now.
#[tokio::test(flavor = "multi_thread")]
async fn take_out_reports_what_it_did_when_the_harness_went_meanwhile() {
    let dir = tempfile::tempdir().unwrap();
    let (d, _, request) = placed_in_h(dir.path()).await;
    let config = d.cfg.machine_config.clone();
    let _hook = sk1ll_core::failpoints::once_shared(
        sk1ll_core::failpoints::Point::DaemonBeforeTakeOutApply,
        &config.clone(),
        move |_| {
            let text = std::fs::read_to_string(&config).unwrap();
            let start = text.find("[harnesses.h]").unwrap();
            let end = text[start + 1..].find("\n[").map_or(text.len(), |i| start + 1 + i + 1);
            std::fs::write(&config, format!("{}{}", &text[..start], &text[end..])).unwrap();
        },
    );
    let done = d.take_out_harness(request).await.unwrap();
    assert!(!done.applied, "{done:?}");
    assert_eq!(
        done.errors,
        ["machine.toml was not changed: no harness `h` (see `sk1ll harnesses --all`). h's items were turned off; \
          what sync took out is unknown, since no complete scan verified it; h is no longer in machine.toml. Preview \
          again"]
    );
    assert!(done.taken_out.is_empty() && done.left.is_empty(), "{done:?}");
    assert!(!done.harnesses.iter().any(|h| h.name == "h"), "as machine.toml is now: {:?}", done.harnesses);
}

/// A hand edit that turns the harness off while a take-out runs: the
/// report says it is off now, and what the take-out did.
#[tokio::test(flavor = "multi_thread")]
async fn take_out_reports_a_harness_turned_off_by_hand_meanwhile() {
    let dir = tempfile::tempdir().unwrap();
    let (d, _, request) = placed_in_h(dir.path()).await;
    let config = d.cfg.machine_config.clone();
    let _hook = sk1ll_core::failpoints::once_shared(
        sk1ll_core::failpoints::Point::DaemonBeforeTakeOutApply,
        &config.clone(),
        move |_| {
            let text = std::fs::read_to_string(&config).unwrap();
            std::fs::write(&config, text.replace("[harnesses.h]\n", "[harnesses.h]\nenabled = false\n")).unwrap();
        },
    );
    let done = d.take_out_harness(request).await.unwrap();
    assert!(!done.applied, "{done:?}");
    assert_eq!(
        done.errors,
        ["machine.toml changed since the preview. h's items were turned off and 1 placed item was taken out; h is \
          off in machine.toml. Preview again"]
    );
    assert_eq!(done.taken_out.len(), 1);
    assert!(done.harnesses.iter().any(|h| h.name == "h" && !h.enabled), "{:?}", done.harnesses);
}

/// A reconcile that finds an interrupted keep's journal saves its report
/// and shows it in the status, adopts nothing, and drops the journal; a
/// note stays through later reconciles until dismissed.
#[tokio::test(flavor = "multi_thread")]
async fn a_reconcile_reports_an_interrupted_keep() {
    let dir = tempfile::tempdir().unwrap();
    let d = standalone_with_pdf(dir.path());
    let root = d.repo_root().to_path_buf();
    let journal = root.join(".git/sk1ll-keep.json");
    let dest = root.join("personal/pdf/SKILL.md");
    let body = serde_json::json!({
        "phase": "pulling",
        "kept": [{ "item": "skill:pdf", "dest": format!("\"{}\"", dest.display()), "entry": null }],
        "staged": [],
    });
    std::fs::write(&journal, body.to_string()).unwrap();
    d.blocking(|d, eng| d.reconcile(eng).map(drop)).await.unwrap();
    assert!(!journal.exists());
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "PDFs.\n", "nothing adopted or changed");
    // A note stays, through later reconciles, until dismissed.
    for _ in 0..2 {
        let shown = d.view().keep_report;
        assert!(shown.len() == 1 && shown[0].entry.is_none(), "{shown:?}");
        d.blocking(|d, eng| d.reconcile(eng).map(drop)).await.unwrap();
    }
    assert_eq!(d.blocking(|d, eng| d.dismiss_keep_notes(eng)).await.unwrap(), 1);
    assert!(d.view().keep_report.is_empty(), "{:?}", d.view().keep_report);
    // With a version in recovery, the report stays through any later reconcile and error.
    // A version in recovery is end to end, in tests/daemon.rs: a recovery
    // entry here would be in the index every test in this process shares.
}
