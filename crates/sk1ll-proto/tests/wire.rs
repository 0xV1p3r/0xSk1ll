//! The wire contract between the server, the daemons and the GUI: every
//! message and body as golden JSON, what a peer must refuse, and what it must
//! tolerate. A change here an older peer would misread needs a
//! `PROTOCOL_VERSION` bump.

use std::fmt::Debug;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sk1ll_proto::*;

fn id(s: &str) -> ItemId {
    s.parse().unwrap()
}

/// `value` serializes to exactly `golden`, and `golden` parses back to it.
#[track_caller]
fn golden<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: T, golden: Value) {
    assert_eq!(serde_json::to_value(&value).unwrap(), golden, "{value:?}");
    assert_eq!(serde_json::from_value::<T>(golden).unwrap(), value);
}

#[track_caller]
fn refused<T: DeserializeOwned + Debug>(text: Value) {
    let parsed = serde_json::from_value::<T>(text.clone());
    assert!(parsed.is_err(), "{text} parsed as {parsed:?}");
}

fn entry(cause: LogCause) -> LogEntry {
    LogEntry {
        id: 1,
        at: 2,
        author: "admin".into(),
        machine: "ws".into(),
        harness: "claude".into(),
        kind: ToggleKind::Skill,
        item_id: Some(id("01J0")),
        name: Some("pdf".into()),
        before: None,
        after: Some(true),
        cause,
    }
}

fn entry_json(cause: Value) -> Value {
    json!({
        "id": 1, "at": 2, "author": "admin", "machine": "ws", "harness": "claude", "kind": "skill",
        "item_id": "01J0", "name": "pdf", "before": null, "after": true, "cause": cause,
    })
}

fn assignment() -> (Assignment, Value) {
    let mut a = Assignment::fleet();
    a.set_skill("claude", &id("01J0"), true);
    a.harnesses.get_mut("claude").unwrap().new_rules_default = true;
    let v = json!({"harnesses": {"claude": {
        "new_skills_default": false, "new_rules_default": true, "skills": {"01J0": true}, "rules": {},
    }}, "new_harnesses_default": false});
    (a, v)
}

#[test]
fn the_protocol_version_and_its_headers_are_pinned() {
    // Bumping it means older peers are refused: review every golden below.
    assert_eq!(PROTOCOL_VERSION, 1);
    assert_eq!((PROTOCOL_HEADER, VERSION_HEADER, REPO_PATH), ("sk1ll-protocol", "sk1ll-version", "/git/repo.git"));
    let why = protocol_mismatch("2");
    assert!(why.contains("the server speaks version 1, this machine speaks 2"), "{why}");
}

#[test]
fn daemon_messages_have_golden_json() {
    golden(DaemonMessage::Hello { machine_id: "ws".into() }, json!({"type": "hello", "machine_id": "ws"}));

    let report = MachineReport {
        seq: 4,
        baseline: Some("abc".into()),
        drafts: vec![
            ItemRef::Skill("pdf".into()),
            ItemRef::Rule("git".into()),
            ItemRef::Manifest,
            ItemRef::Other("README.md".into()),
        ],
        unpushed: vec![ItemRef::Skill("wip".into())],
        items: vec![ReportedItem { item: ItemRef::Skill("pdf".into()), id: id("01J0") }],
        inventory_complete: true,
        inventory_error: Some("bad Rule ID".into()),
        conflicts: vec![ConflictRef { item: ItemRef::Rule("git".into()), harness: "codex".into() }],
        harnesses: vec!["claude".into()],
        blocked: true,
        clashes: vec![ItemRef::Skill("pdf".into())],
        pull_pending: Some(vec![ItemRef::Rule("git".into())]),
        behind: true,
        push_refused: Some("no Item ID".into()),
    };
    golden(
        DaemonMessage::Status(report),
        json!({
            "type": "status", "seq": 4, "baseline": "abc",
            "drafts": ["skill:pdf", "rule:git", "manifest", "other:README.md"],
            "unpushed": ["skill:wip"], "items": [{"item": "skill:pdf", "id": "01J0"}],
            "inventory_complete": true, "inventory_error": "bad Rule ID",
            "conflicts": [{"item": "rule:git", "harness": "codex"}], "harnesses": ["claude"],
            "blocked": true, "clashes": ["skill:pdf"], "pull_pending": ["rule:git"], "behind": true,
            "push_refused": "no Item ID",
        }),
    );
    golden(
        DaemonMessage::Status(MachineReport::default()),
        json!({
            "type": "status", "seq": 0, "baseline": null, "drafts": [], "unpushed": [], "items": [],
            "inventory_complete": false, "conflicts": [], "harnesses": [], "blocked": false, "clashes": [],
            "behind": false,
        }),
    );

    let changes = AssignmentChanges {
        changes: vec![
            ToggleChange {
                harness: "claude".into(),
                kind: ToggleKind::Skill,
                item_id: Some(id("01J0")),
                name: Some("pdf".into()),
                enabled: None,
            },
            ToggleChange {
                harness: "claude".into(),
                kind: ToggleKind::NewRulesDefault,
                item_id: None,
                name: None,
                enabled: Some(true),
            },
        ],
        expected: Some(Expected { revision: 3, registration: Some("reg".into()) }),
    };
    let rename = HarnessRename { from: "a".into(), to: "b".into(), operation_id: "op".into(), replace: true };
    for (write, json) in [
        (
            AssignmentWrite::Changes(changes),
            json!({"op": "changes", "changes": [
                {"harness": "claude", "kind": "skill", "item_id": "01J0", "name": "pdf", "enabled": null},
                {"harness": "claude", "kind": "new_rules_default", "enabled": true},
            ], "expected": {"revision": 3, "registration": "reg"}}),
        ),
        (
            AssignmentWrite::Harness { harness: "claude".into(), enabled: false },
            json!({"op": "harness", "harness": "claude", "enabled": false}),
        ),
        (
            AssignmentWrite::RenameHarness(rename),
            json!({"op": "rename_harness", "from": "a", "to": "b", "operation_id": "op", "replace": true}),
        ),
    ] {
        golden(
            DaemonMessage::SetAssignment { req: 7, write },
            json!({"type": "set_assignment", "req": 7, "write": json}),
        );
    }
}

#[test]
fn server_messages_have_golden_json() {
    let (a, a_json) = assignment();
    let mut snapshot = json!({"type": "assignment", "revision": 7});
    snapshot.as_object_mut().unwrap().extend(a_json.as_object().unwrap().clone());
    golden(
        ServerMessage::Assignment(AssignmentSnapshot { revision: 7, assignment: a.clone(), registration: None }),
        snapshot.clone(),
    );
    snapshot["registration"] = json!("reg");
    golden(
        ServerMessage::Assignment(AssignmentSnapshot {
            revision: 7,
            assignment: a.clone(),
            registration: Some("reg".into()),
        }),
        snapshot,
    );
    golden(ServerMessage::Nudge { baseline: "abc".into() }, json!({"type": "nudge", "baseline": "abc"}));
    golden(ServerMessage::Revoked, json!({"type": "revoked"}));
    let update = ImportUpdate {
        name: "pdf".into(),
        source: "github:o/r".into(),
        git_ref: "main".into(),
        locked: "aaa".into(),
        upstream: "bbb".into(),
    };
    golden(
        ServerMessage::ImportsChanged { updates: vec![update] },
        json!({"type": "imports_changed", "updates": [
            {"name": "pdf", "source": "github:o/r", "ref": "main", "locked": "aaa", "upstream": "bbb"},
        ]}),
    );
    golden(
        ServerMessage::AssignmentResult {
            req: 7,
            seq: 2,
            result: AssignmentOutcome::Refused { status: 409, error: "changed".into() },
        },
        json!({"type": "assignment_result", "req": 7, "seq": 2,
            "result": {"refused": {"status": 409, "error": "changed"}}}),
    );
    let applied = AssignmentUpdate {
        assignment: a,
        revision: 8,
        entries: vec![entry(LogCause::Change)],
        errors: vec!["harness `x` failed".into()],
    };
    golden(
        ServerMessage::AssignmentResult { req: 8, seq: 0, result: AssignmentOutcome::Applied(applied) },
        json!({"type": "assignment_result", "req": 8, "seq": 0, "result": {"applied": {
            "assignment": a_json, "revision": 8, "entries": [entry_json(json!({"type": "change"}))],
            "errors": ["harness `x` failed"],
        }}}),
    );
}

#[test]
fn every_enum_variant_has_its_wire_name() {
    use MachineStatus::*;
    for (status, name) in [
        (Offline, "offline"),
        (Blocked, "blocked"),
        (Conflict, "conflict"),
        (Behind, "behind"),
        (Drafting, "drafting"),
        (InSync, "in_sync"),
    ] {
        golden(status, json!(name));
    }
    for (scope, name) in [(Scope::Fleet, "fleet"), (Scope::SelfOnly, "self")] {
        golden(scope, json!(name));
        assert_eq!((scope.as_str(), name.parse::<Scope>()), (name, Ok(scope)));
    }
    for (kind, name, has_item) in [
        (ToggleKind::Skill, "skill", true),
        (ToggleKind::Rule, "rule", true),
        (ToggleKind::NewSkillsDefault, "new_skills_default", false),
        (ToggleKind::NewRulesDefault, "new_rules_default", false),
    ] {
        golden(kind, json!(name));
        assert_eq!(kind.has_item(), has_item, "{name}");
    }
    for (mode, name) in [(CopyMode::Replace, "replace"), (CopyMode::Merge, "merge")] {
        golden(mode, json!(name));
    }
    for (cause, json) in [
        (LogCause::Change, json!({"type": "change"})),
        (LogCause::Undo { entry: 3 }, json!({"type": "undo", "entry": 3})),
        (LogCause::DefaultMaterialized, json!({"type": "default_materialized"})),
        (LogCause::MachineRemoved, json!({"type": "machine_removed"})),
        (LogCause::HarnessAdded, json!({"type": "harness_added"})),
        (
            LogCause::HarnessRenamed { from: "a".into(), to: "b".into() },
            json!({"type": "harness_renamed", "from": "a", "to": "b"}),
        ),
        (LogCause::OrphanPruned, json!({"type": "orphan_pruned"})),
    ] {
        golden(entry(cause), entry_json(json));
    }
    for (item, text) in [
        (ItemRef::Skill("pdf".into()), "skill:pdf"),
        (ItemRef::Rule("10-git".into()), "rule:10-git"),
        (ItemRef::Manifest, "manifest"),
        (ItemRef::Other("notes/a.txt".into()), "other:notes/a.txt"),
    ] {
        golden(item, json!(text));
    }
}

/// REST bodies the GUI reads too have one sample each in `wire.json`, which
/// `crates/sk1ll-gui/ui/src/apiContract.ts` checks against `types.ts`. Each
/// reads back to exactly its sample.
#[test]
fn shared_rest_bodies_match_their_samples() {
    fn same<T: Serialize + DeserializeOwned>(samples: &mut serde_json::Map<String, Value>, key: &str) {
        let sample = samples.remove(key).unwrap_or_else(|| panic!("no sample of `{key}`"));
        let value: T = serde_json::from_value(sample.clone()).unwrap_or_else(|e| panic!("{key}: {e}"));
        assert_eq!(serde_json::to_value(value).unwrap(), sample, "{key}");
    }
    let Value::Object(mut samples) = serde_json::from_str(include_str!("wire.json")).unwrap() else { panic!() };
    same::<FleetView>(&mut samples, "fleet_view");
    same::<AssignmentChanges>(&mut samples, "assignment_changes");
    same::<AssignmentUpdate>(&mut samples, "assignment_update");
    same::<AssignmentCopyPreview>(&mut samples, "assignment_copy_preview");
    same::<OrphansView>(&mut samples, "orphans_view");
    same::<OrphansPruned>(&mut samples, "orphans_pruned");
    same::<LogFacets>(&mut samples, "log_facets");
    same::<JoinCode>(&mut samples, "join_code");
    assert!(samples.is_empty(), "unchecked samples: {:?}", samples.keys().collect::<Vec<_>>());
}

/// The other bodies, and the shared ones where fields are left out.
#[test]
fn rest_bodies_have_golden_json() {
    let (a, a_json) = assignment();
    golden(FleetView { baseline: None, machines: vec![] }, json!({"baseline": null, "machines": []}));
    golden(
        AssignmentChanges {
            changes: vec![ToggleChange {
                harness: "claude".into(),
                kind: ToggleKind::NewSkillsDefault,
                item_id: None,
                name: None,
                enabled: Some(false),
            }],
            expected: None,
        },
        json!({"changes": [{"harness": "claude", "kind": "new_skills_default", "enabled": false}]}),
    );
    let set = serde_json::from_value::<SetHarnessAssignment>(json!({"enabled": true})).unwrap();
    assert_eq!(serde_json::to_value(set).unwrap(), json!({"enabled": true}));
    golden(Expected { revision: 3, registration: None }, json!({"revision": 3}));
    golden(
        MachineAssignmentCopy {
            source: "a".into(),
            harnesses: Some(vec![HarnessMapping { from: "claude".into(), to: "codex".into() }]),
            mode: CopyMode::Replace,
            expected: Some(Expected { revision: 1, registration: None }),
            source_expected: None,
        },
        json!({"source": "a", "harnesses": [{"from": "claude", "to": "codex"}], "mode": "replace",
            "expected": {"revision": 1}, "source_expected": null}),
    );
    let change = CopyChange {
        harness: "claude".into(),
        kind: ToggleKind::Rule,
        item_id: Some(id("git")),
        name: Some("git".into()),
        before: None,
        after: Some(false),
    };
    golden(
        AssignmentCopyPreview {
            count: 1,
            changes: vec![change],
            target: Expected { revision: 1, registration: None },
            source: Expected { revision: 2, registration: Some("reg".into()) },
        },
        json!({"count": 1, "changes": [
            {"harness": "claude", "kind": "rule", "item_id": "git", "name": "git", "before": null, "after": false},
        ], "target": {"revision": 1}, "source": {"revision": 2, "registration": "reg"}}),
    );
    golden(
        AssignmentUpdate { assignment: a.clone(), revision: 5, entries: vec![], errors: vec![] },
        json!({"assignment": a_json, "revision": 5, "entries": []}),
    );
    golden(PruneOrphans { machine: None }, json!({}));
    golden(PruneOrphans { machine: Some("ws".into()) }, json!({"machine": "ws"}));
    golden(
        JoinRequest { code: "C".into(), machine_id: "ws".into(), harnesses: vec!["claude".into()], copy_from: None },
        json!({"code": "C", "machine_id": "ws", "harnesses": ["claude"], "copy_from": null}),
    );
    golden(
        JoinResponse {
            machine_id: "ws".into(),
            token: "t".into(),
            repo_path: REPO_PATH.into(),
            assignment: a,
            revision: 1,
        },
        json!({"machine_id": "ws", "token": "t", "repo_path": "/git/repo.git", "assignment": a_json, "revision": 1}),
    );
    golden(JoinCodeRequest { scope: Scope::SelfOnly }, json!({"scope": "self"}));
    golden(
        ImportsView {
            checked_at: Some(9),
            updates: vec![],
            errors: vec![ImportError { name: "web".into(), message: "unreachable".into() }],
            skipped: vec![ImportSkipped { name: "pdf".into(), reason: "checks are off".into() }],
        },
        json!({"checked_at": 9, "updates": [], "errors": [{"name": "web", "message": "unreachable"}],
            "skipped": [{"name": "pdf", "reason": "checks are off"}]}),
    );
    golden(ErrorBody { error: "nope".into() }, json!({"error": "nope"}));
}

/// Fields with defaults may be absent, so an older peer's message (or a
/// minimal `curl` body) still parses, and fields a newer peer adds are ignored.
#[test]
fn absent_defaulted_fields_and_unknown_fields_are_accepted() {
    for (text, parsed) in [
        (json!({"type": "status"}), DaemonMessage::Status(MachineReport::default())),
        (json!({"type": "hello", "machine_id": "ws", "since": 2}), DaemonMessage::Hello { machine_id: "ws".into() }),
    ] {
        assert_eq!(serde_json::from_value::<DaemonMessage>(text).unwrap(), parsed);
    }
    let rename = json!({"from": "a", "to": "b", "operation_id": "op"});
    assert!(!serde_json::from_value::<HarnessRename>(rename).unwrap().replace);
    let join = serde_json::from_value::<JoinRequest>(json!({"code": "C", "machine_id": "ws"})).unwrap();
    assert_eq!((join.harnesses, join.copy_from), (vec![], None));
    assert_eq!(serde_json::from_value::<JoinCodeRequest>(json!({})).unwrap().scope, Scope::Fleet);
    let copy = serde_json::from_value::<MachineAssignmentCopy>(json!({"source": "a", "mode": "merge"})).unwrap();
    assert_eq!((copy.harnesses, copy.expected, copy.source_expected), (None, None, None));
    let view = serde_json::from_value::<ImportsView>(json!({"checked_at": null, "updates": [], "errors": []})).unwrap();
    assert!(view.skipped.is_empty());
    let preview = json!({"count": 0, "target": {"revision": 1}, "source": {"revision": 1}});
    assert!(serde_json::from_value::<AssignmentCopyPreview>(preview).unwrap().changes.is_empty());
    // An assignment harness without defaults has them on, and so does a
    // harness without an entry unless `new_harnesses_default` says otherwise.
    let snapshot = json!({"type": "assignment", "revision": 1, "harnesses": {"claude": {}}});
    let ServerMessage::Assignment(s) = serde_json::from_value(snapshot).unwrap() else { panic!() };
    let h = s.assignment.harness("claude");
    assert!(h.new_skills_default && h.new_rules_default && h.skills.is_empty());
    assert!(s.assignment.harness("codex").new_skills_default && s.assignment.harness("codex").new_rules_default);
    let fleet = serde_json::from_value::<Assignment>(json!({"new_harnesses_default": false})).unwrap();
    assert!(!fleet.harness("codex").new_skills_default && !fleet.harness("codex").new_rules_default);
}

#[test]
fn malformed_messages_are_refused() {
    for text in [
        json!({"machine_id": "ws"}),
        json!({"type": "goodbye"}),
        json!({"type": "hello"}),
        json!({"type": "status", "drafts": ["pdf"]}),
        json!({"type": "status", "items": [{"item": "skill:pdf", "id": ""}]}),
        json!({"type": "set_assignment", "req": 1, "write": {"op": "delete"}}),
        json!({"type": "set_assignment", "write": {"op": "harness", "harness": "claude", "enabled": true}}),
    ] {
        refused::<DaemonMessage>(text);
    }
    for text in [
        json!({"type": "nudge"}),
        json!({"type": "assignment_result", "req": 1, "seq": 1, "result": {"maybe": {}}}),
        json!({"type": "assignment", "harnesses": {}}),
    ] {
        refused::<ServerMessage>(text);
    }
    for text in ["pdf", "fragment:x", "skill:", "rule:", "other:", "Skill:pdf"] {
        refused::<ItemRef>(json!(text));
    }
    for text in ["", "a\nb"] {
        refused::<ItemId>(json!(text));
    }
    refused::<Scope>(json!("admin"));
    assert!("admin".parse::<Scope>().is_err());
    refused::<MachineStatus>(json!("online"));
    refused::<ToggleKind>(json!("skills"));
    refused::<LogCause>(json!({"type": "undo"}));
    refused::<JoinCodeRequest>(json!({"scope": "admin"}));
}
