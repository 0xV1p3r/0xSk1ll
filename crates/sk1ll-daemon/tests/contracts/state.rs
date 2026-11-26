//! The credentials, Assignment snapshot and pull records on disk.

use std::fs;

use sk1ll_daemon::state::*;
use sk1ll_proto::AssignmentSnapshot;
use sk1ll_testkit::SkillsRepo;

#[test]
fn private_write_ignores_staging_symlinks() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("daemon.json");
    let target = dir.path().join("existing");
    fs::write(&target, b"keep").unwrap();
    for name in [".daemon.json.tmp", ".daemon.json.sk1ll-tmp"] {
        symlink(&target, dir.path().join(name)).unwrap();
    }

    state().save(&path).unwrap();

    assert_eq!(fs::read(&target).unwrap(), b"keep");
    assert_eq!(DaemonState::load(&path).unwrap(), Some(state()));
    assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
}

/// A record another version wrote (a pending pull without its
/// Baseline) loads without it, the rest of the state intact.
#[test]
fn an_unreadable_pull_record_is_dropped_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("daemon.json");
    let mut json = serde_json::to_value(state()).unwrap();
    json["pull"] = serde_json::json!({ "state": "pending", "items": ["skill:pdf"] });
    json["server_baseline"] = serde_json::json!(["not", "a", "commit"]);
    fs::write(&path, json.to_string()).unwrap();
    let loaded = DaemonState::load(&path).unwrap().unwrap();
    assert_eq!(loaded.record, PullRecord::default());
    assert_eq!(loaded, state());

    json["pull"] = serde_json::json!({ "state": "blocked", "baseline": "abc", "items": ["skill:pdf"] });
    json["server_baseline"] = serde_json::json!("abc");
    fs::write(&path, json.to_string()).unwrap();
    let loaded = DaemonState::load(&path).unwrap().unwrap();
    let items = vec!["skill:pdf".parse().unwrap()];
    assert_eq!(loaded.record.pull, PullState::Blocked { baseline: "abc".into(), items });
    assert_eq!(loaded.record.server_baseline.as_deref(), Some("abc"));

    // A standalone daemon's record the same way.
    let repo = dir.path().join("repo");
    SkillsRepo::init(&repo);
    let record = dir.path().join("standalone-state.json");
    save_record(&record, &repo, &PullRecord::default()).unwrap();
    let mut json: serde_json::Value = serde_json::from_str(&fs::read_to_string(&record).unwrap()).unwrap();
    json["pull"] = serde_json::json!({ "state": "pending", "items": [] });
    json["server_baseline"] = serde_json::json!("abc");
    fs::write(&record, json.to_string()).unwrap();
    let loaded = load_record(&record, &repo).unwrap();
    assert_eq!(loaded, PullRecord { server_baseline: Some("abc".into()), ..PullRecord::default() });
}

fn state() -> DaemonState {
    DaemonState::new("ws".into(), "https://server.example:8443".into(), "t0k".into(), "/r".into())
}

#[cfg(unix)]
#[test]
fn a_dangling_snapshot_link_is_an_error_not_a_missing_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshot.json");
    assert!(load_snapshot(&path, None).unwrap().is_none());
    std::os::unix::fs::symlink(dir.path().join("gone.json"), &path).unwrap();
    let err = load_snapshot(&path, None).unwrap_err();
    assert!(format!("{err:#}").contains("symlink to a missing file"), "{err:#}");
}

/// The server's git repo and WebSocket endpoints, from the URL joined.
#[test]
fn the_server_url_names_the_git_and_websocket_endpoints() {
    assert_eq!(state().repo_url(), "https://server.example:8443/git/repo.git");
    assert_eq!(state().ws_url(), "wss://server.example:8443/api/ws");
    let plain = DaemonState { server_url: "http://127.0.0.1:9".into(), ..state() };
    assert_eq!(plain.repo_url(), "http://127.0.0.1:9/git/repo.git");
    assert_eq!(plain.ws_url(), "ws://127.0.0.1:9/api/ws");
}

#[test]
fn debug_hides_the_token() {
    let text = format!("{:?}", state());
    assert!(!text.contains("t0k"), "{text}");
}

#[test]
fn state_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sk1ll/daemon.json");
    let s = DaemonState { token: "secret".into(), ..state() };
    s.save(&path).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(DaemonState::load(&path).unwrap(), Some(s));
    assert_eq!(DaemonState::load(&dir.path().join("none")).unwrap(), None);
    // Older state files carried the assignment revision; it is ignored now.
    fs::write(&path, r#"{"machine_id":"m","server_url":"http://h","token":"t","repo":"/r","revision":4}"#).unwrap();
    assert!(DaemonState::load(&path).unwrap().is_some());
}

#[test]
fn the_baseline_record_survives_a_save() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sk1ll/daemon.json");
    let record = PullRecord {
        server_baseline: Some("abc".into()),
        pull: PullState::Blocked { baseline: "abc".into(), items: vec!["skill:pdf".parse().unwrap()] },
        push_refused: None,
        draft_commit_complete: true,
    };
    let s = DaemonState { record: record.clone(), ..state() };
    s.save(&path).unwrap();
    assert_eq!(DaemonState::load(&path).unwrap().unwrap().record, record);
    // A state file without one knows nothing about the Baseline.
    fs::write(&path, r#"{"machine_id":"m","server_url":"http://h","token":"t","repo":"/r"}"#).unwrap();
    assert_eq!(DaemonState::load(&path).unwrap().unwrap().record, PullRecord::default());

    let standalone = standalone_record_path(&path);
    let repo = dir.path().join("repo");
    SkillsRepo::init(&repo);
    assert_eq!(load_record(&standalone, &repo).unwrap(), PullRecord::default());
    save_record(&standalone, &repo, &record).unwrap();
    assert_eq!(load_record(&standalone, &repo).unwrap(), record);
}

/// A standalone record is about one repo: another repo, at another path
/// or re-created at the same one, starts without it.
#[test]
fn a_standalone_record_is_not_restored_for_another_repo() {
    let dir = tempfile::tempdir().unwrap();
    let path = standalone_record_path(&dir.path().join("sk1ll/daemon.json"));
    let record = PullRecord {
        server_baseline: Some("abc".into()),
        pull: PullState::Blocked { baseline: "abc".into(), items: vec!["skill:pdf".parse().unwrap()] },
        push_refused: None,
        draft_commit_complete: true,
    };
    let (one, other) = (dir.path().join("one"), dir.path().join("other"));
    SkillsRepo::init(&one);
    SkillsRepo::init(&other);
    save_record(&path, &one, &record).unwrap();
    assert_eq!(load_record(&path, &other).unwrap(), PullRecord::default());
    assert_eq!(load_record(&path, &one).unwrap(), record);
    fs::remove_dir_all(&one).unwrap();
    assert_eq!(load_record(&path, &one).unwrap(), PullRecord::default(), "no repo there");
    fs::rename(&other, &one).unwrap();
    assert_eq!(load_record(&path, &one).unwrap(), PullRecord::default(), "another repo at the same path");
    // A record without a repo (an older one) is about none.
    fs::write(&path, r#"{"server_baseline":"abc","pull":{"state":"clear"}}"#).unwrap();
    assert_eq!(load_record(&path, &one).unwrap(), PullRecord::default());
}

#[test]
fn snapshots_keep_revision_and_content_together() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sk1ll/daemon-assignment.json");
    assert_eq!(load_snapshot(&path, None).unwrap(), None);
    let mut snap = AssignmentSnapshot { revision: 7, ..Default::default() };
    snap.assignment.set_skill("h", &"x".parse().unwrap(), false);
    save_snapshot(&path, &snap, None).unwrap();
    assert_eq!(load_snapshot(&path, None).unwrap(), Some(snap.clone()));
    // Joined, a snapshot belongs to one registration.
    save_snapshot(&path, &snap, Some("old")).unwrap();
    assert_eq!(load_snapshot(&path, Some("old")).unwrap(), Some(snap.clone()));
    assert_eq!(load_snapshot(&path, Some("new")).unwrap(), None, "another registration's");
    save_snapshot(&path, &snap, None).unwrap();
    assert_eq!(load_snapshot(&path, Some("new")).unwrap(), None, "an untagged one");
    fs::write(&path, "{broken").unwrap();
    assert!(load_snapshot(&path, None).is_err());
}
