//! How the daemon reads the local API's requests. The JSON the GUI gets back
//! is pinned by the golden samples in `crates/sk1ll-gui/contract`.

use serde_json::json;
use sk1ll_core::{HarnessChange, LinkMode};
use sk1ll_daemon::client::{AddImportRequest, HarnessChangeRequest, ResolveImportRequest};

/// Absent and empty differ for `path`: absent is the link's own path, `""`
/// the repo root; absent fields are left out when a client sends one.
#[test]
fn an_import_request_tells_an_absent_path_from_the_repo_root() {
    let add = |body| serde_json::from_value::<AddImportRequest>(body).unwrap();
    let root = add(json!({"name": "", "source": "s", "path": "", "ref": "main", "commit": "abcd"}));
    assert_eq!(root.path.as_deref(), Some(""));
    assert_eq!((root.git_ref.as_deref(), root.commit.as_deref()), (Some("main"), Some("abcd")));
    let linked = add(json!({"name": "", "source": "s"}));
    assert_eq!((linked.path, linked.git_ref, linked.commit), (None, None, None));
    let sent = AddImportRequest { name: "n".into(), source: "s".into(), path: None, git_ref: None, commit: None };
    assert_eq!(serde_json::to_value(&sent).unwrap(), json!({"name": "n", "source": "s"}));

    let resolve: ResolveImportRequest = serde_json::from_value(json!({"link": "github:o/r"})).unwrap();
    assert_eq!((resolve.path.as_str(), resolve.git_ref.as_str()), ("", ""));
}

/// A harness change names its operation in `op`; the review fingerprint is
/// optional.
#[test]
fn harness_changes_are_tagged_by_op() {
    let set = json!({"change": {"op": "set", "name": "agents", "harness": {
        "skills": "~/.agents/skills", "instructions": null, "instructions_format": "block", "mode": "copy"}}});
    let req: HarnessChangeRequest = serde_json::from_value(set).unwrap();
    assert!(req.fingerprint.is_none());
    assert!(matches!(&req.change, HarnessChange::Set { name, harness } if name == "agents"
        && harness.mode == LinkMode::Copy));
    let disable = HarnessChangeRequest {
        copy_assignment_from: None,
        fingerprint: None,
        change: HarnessChange::Disable { name: "codex".into() },
    };
    assert_eq!(serde_json::to_value(&disable).unwrap(), json!({"change": {"op": "disable", "name": "codex"}}));
    let reviewed: HarnessChangeRequest =
        serde_json::from_value(json!({"fingerprint": "f", "change": {"op": "remove", "name": "codex"}})).unwrap();
    assert_eq!(reviewed.fingerprint.as_deref(), Some("f"));
    assert!(matches!(reviewed.change, HarnessChange::Remove { .. }));
}
