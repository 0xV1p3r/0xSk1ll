//! The contract between the local API types the GUI consumes and `ui/src/types.ts`.
//!
//! Representative values of every type (and every enum variant) are serialized and compared with the golden files
//! in `crates/sk1ll-gui/contract/`, then read back to prove they round-trip. Wire bodies the Daemon passes through
//! from the Server (`FleetView`, Assignment changes and updates, copy previews, orphans, log facets, join codes)
//! have their sample in `crates/sk1ll-proto/tests/wire.json` instead, checked by that crate's `wire.rs`. `ui/src/apiContract.ts` assigns the
//! same files to the `types.ts` types, so `pnpm tsc` fails when the two sides drift apart.
//!
//! Enums are written as objects keyed by their serialized name (a tagged enum by its tag), so the TypeScript side
//! sees each variant name as a literal key and can compare the set with its union. `SK1LL_BLESS=1` rewrites the files.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use sk1ll_core::draft::Unkept;
use sk1ll_core::git::plumbing::Setup;
use sk1ll_core::imports::{FileWarning, ImportCommit, WarningKind};
use sk1ll_core::sync::unmanaged::{Location, Variant};
use sk1ll_core::{Change, Harness, HarnessAssignment, InstructionsFormat, ItemId, LinkMode};
use sk1ll_daemon::Joined;
use sk1ll_daemon::client::*;
use sk1ll_daemon::service::{Manager, Started};
use sk1ll_gui::commands::{CommandError, ReviewDraftEntry};
use sk1ll_gui::editors::{EditorKind, EditorView, EditorsView};
use sk1ll_gui::events::ConnectionEvent;
use sk1ll_gui::service::ServiceInfo;
use sk1ll_gui::updates::{AvailableUpdate, Progress};
use sk1ll_proto::{
    ConflictRef, CopyMode, Expected, HarnessMapping, ImportError, LogCause, ReportedItem, ToggleChange, ToggleKind,
};

/// `value` as JSON, after checking that the JSON reads back as the same value.
fn sample<T: Serialize + DeserializeOwned>(value: T) -> Value {
    let json = serde_json::to_value(&value).expect("serializing a sample");
    let back: T = serde_json::from_value(json.clone()).unwrap_or_else(|e| panic!("{json} does not read back: {e}"));
    assert_eq!(serde_json::to_value(&back).unwrap(), json, "a sample changed on its way back");
    json
}

/// For the few types that are only ever written (the Daemon's service enums).
fn written<T: Serialize>(value: T) -> Value {
    serde_json::to_value(&value).expect("serializing a sample")
}

/// Samples keyed by their serialized name, or by the value of their `tag` field.
fn keyed(samples: impl IntoIterator<Item = Value>, tag: Option<&str>) -> Value {
    let mut map = Map::new();
    for json in samples {
        let key = tag.map_or(&json, |tag| &json[tag]).as_str().unwrap_or_else(|| panic!("no key in {json}")).to_owned();
        assert!(map.insert(key.clone(), json).is_none(), "two samples of `{key}`");
    }
    Value::Object(map)
}

fn by_name<T: Serialize + DeserializeOwned>(values: impl IntoIterator<Item = T>) -> Value {
    keyed(values.into_iter().map(sample), None)
}

fn by_tag<T: Serialize + DeserializeOwned>(tag: &str, values: impl IntoIterator<Item = T>) -> Value {
    keyed(values.into_iter().map(sample), Some(tag))
}

fn item(text: &str) -> ItemRef {
    text.parse().unwrap()
}

fn id(text: &str) -> ItemId {
    text.parse().unwrap()
}

fn harness(
    skills: &str,
    instructions: Option<&str>,
    instructions_format: InstructionsFormat,
    mode: LinkMode,
) -> Harness {
    Harness { skills: Some(skills.into()), instructions: instructions.map(Into::into), instructions_format, mode }
}

fn claude() -> Harness {
    harness("~/.claude/skills", Some("~/.claude/CLAUDE.md"), InstructionsFormat::Block, LinkMode::Symlink)
}

fn report() -> MachineReport {
    MachineReport {
        seq: 7,
        baseline: Some("9f3c2a17d4e1".into()),
        drafts: vec![item("skill:release-notes"), item("rule:style")],
        unpushed: vec![item("skill:release-notes")],
        items: vec![ReportedItem { item: item("skill:deploy"), id: id("01J0DEPLOY") }],
        inventory_complete: false,
        inventory_error: Some("could not read ~/.codex/skills".into()),
        conflicts: vec![ConflictRef { item: item("skill:code-review"), harness: "codex".into() }],
        harnesses: vec!["claude".into(), "codex".into()],
        blocked: true,
        clashes: vec![item("skill:deploy")],
        pull_pending: None,
        behind: true,
        push_refused: Some("the Server refused the push".into()),
    }
}

fn assignment() -> Assignment {
    let claude = HarnessAssignment {
        new_skills_default: true,
        new_rules_default: false,
        skills: BTreeMap::from([(id("01J0DEPLOY"), false)]),
        rules: BTreeMap::from([(id("01J0STYLE"), true)]),
    };
    Assignment { harnesses: BTreeMap::from([("claude".into(), claude)]) }
}

fn log_entry(id_: i64, kind: ToggleKind, cause: LogCause) -> LogEntry {
    let item = kind.has_item();
    LogEntry {
        id: id_,
        at: 1_760_000_000,
        author: "dev@workstation".into(),
        machine: "workstation".into(),
        harness: "claude".into(),
        kind,
        item_id: item.then(|| id("01J0DEPLOY")),
        name: item.then(|| "deploy".into()),
        before: Some(true),
        after: None,
        cause,
    }
}

fn placement(kind: &str, reason: Option<&str>) -> PlacementView {
    PlacementView {
        harness: "claude".into(),
        kind: kind.into(),
        item: "skill".into(),
        name: "code-review".into(),
        dest: "/home/dev/.claude/skills/code-review".into(),
        reason: reason.map(Into::into),
    }
}

fn choice() -> HarnessChoice {
    HarnessChoice {
        name: "kiro".into(),
        installed: true,
        enabled: false,
        harness: harness("~/.kiro/skills", Some("~/.kiro/steering"), InstructionsFormat::Rules, LinkMode::Copy),
        preset: true,
        configured: true,
        available: true,
        setup_warning: Some("~/.kiro is not a directory".into()),
    }
}

fn conflict() -> ConflictView {
    ConflictView {
        kind: None,
        id: None,
        item: Some(item("skill:code-review")),
        path: None,
        displaced_at: None,
        detected_at: Some(1_760_000_000),
        harness: "codex".into(),
        name: "code-review".into(),
        items: vec![item("skill:code-review")],
        dest: "/home/dev/.codex/skills/code-review".into(),
        reason: Some("SKILL.md was edited in the Harness after the last sync.".into()),
        adoptable: true,
        changed: false,
    }
}

fn expected(revision: u64) -> Expected {
    Expected { revision, registration: Some("reg-workstation".into()) }
}

fn import_update() -> ImportUpdate {
    ImportUpdate {
        name: "pdf".into(),
        source: "github:example/skills".into(),
        git_ref: "main".into(),
        locked: "4e5f6a7b8c9d".into(),
        upstream: "c0ffee123456".into(),
    }
}

fn status_golden() -> Value {
    use Connection::*;
    let connections = [Standalone, Connecting, Connected, Offline, AuthRejected, Incompatible, Revoked];
    let _every = |c: Connection| match c {
        Standalone | Connecting | Connected | Offline | AuthRejected | Incompatible | Revoked => (),
    };
    use MachineStatus as S;
    let statuses = [S::Offline, S::Blocked, S::Conflict, S::Behind, S::Drafting, S::InSync];
    let _every = |s: MachineStatus| match s {
        S::Offline | S::Blocked | S::Conflict | S::Behind | S::Drafting | S::InSync => (),
    };
    let events = [
        Event::Status,
        Event::Draft,
        Event::Assignment { revision: 43 },
        Event::Imports { updates: vec![import_update()] },
        Event::Fleet,
    ];
    let _every = |e: &Event| match e {
        Event::Status | Event::Draft | Event::Assignment { .. } | Event::Imports { .. } | Event::Fleet => (),
    };
    let joined = StatusView {
        machine_id: "workstation".into(),
        server_url: Some("https://skills.example.com".into()),
        mode: DaemonMode::Joined,
        has_origin: true,
        repo_ready: true,
        repo: "/home/dev/.local/share/sk1ll/repo".into(),
        headline: MachineStatus::Conflict,
        connection: AuthRejected,
        last_error: Some("Server returned 401 Unauthorized".into()),
        revoked: false,
        server_baseline: Some("9f3c2a17d4e1".into()),
        assignment_revision: 42,
        awaiting_assignment: true,
        auth_rejected_since: Some(1_760_000_000),
        report: report(),
        version: "0.2.0 (abc1234567)".into(),
        protocol: 2,
        server: Some(ServerVersion { version: Some("0.1.0".into()), protocol: Some(1) }),
        keep_report: vec![
            Unkept { item: item("skill:deploy"), entry: Some("k-1".into()), error: "the Draft changed".into() },
            Unkept { item: item("rule:style"), entry: None, error: "deleted in the Draft".into() },
        ],
    };
    let standalone = StatusView {
        machine_id: "laptop".into(),
        server_url: None,
        mode: DaemonMode::Standalone,
        has_origin: false,
        repo_ready: false,
        repo: "/home/dev/skills".into(),
        headline: MachineStatus::InSync,
        connection: Standalone,
        last_error: None,
        revoked: false,
        server_baseline: None,
        assignment_revision: 0,
        awaiting_assignment: false,
        auth_rejected_since: None,
        report: MachineReport::default(),
        version: String::new(),
        protocol: 2,
        server: None,
        keep_report: vec![],
    };
    json!({
        "status_view": { "joined": sample(joined), "standalone": sample(standalone) },
        "connection": by_name(connections),
        "machine_status": by_name(statuses),
        "daemon_mode": by_name([DaemonMode::Joined, DaemonMode::Standalone]),
        "event": by_tag("type", events),
        "daemon_connection": [
            sample(ConnectionEvent { reachable: true, message: None, retry_in_ms: 0 }),
            sample(ConnectionEvent { reachable: false, message: Some("connection refused".into()), retry_in_ms: 2000 }),
        ],
        "command_error": [
            sample(CommandError { message: "Assignment changed".into(), status: Some(409), unreachable: false }),
            sample(CommandError { message: "connection refused".into(), status: None, unreachable: true }),
        ],
    })
}

fn fleet_golden() -> Value {
    use ToggleKind::*;
    let _every = |k: ToggleKind| match k {
        Skill | Rule | NewSkillsDefault | NewRulesDefault => (),
    };
    let causes = [
        LogCause::Change,
        LogCause::Undo { entry: 101 },
        LogCause::DefaultMaterialized,
        LogCause::MachineRemoved,
        LogCause::HarnessAdded,
        LogCause::HarnessRenamed { from: "claude-code".into(), to: "claude".into() },
        LogCause::OrphanPruned,
    ];
    let _every = |c: &LogCause| match c {
        LogCause::Change
        | LogCause::Undo { .. }
        | LogCause::DefaultMaterialized
        | LogCause::MachineRemoved
        | LogCause::HarnessAdded
        | LogCause::HarnessRenamed { .. }
        | LogCause::OrphanPruned => (),
    };
    let _every = |s: Scope| match s {
        Scope::Fleet | Scope::SelfOnly => (),
    };
    let _every = |m: CopyMode| match m {
        CopyMode::Replace | CopyMode::Merge => (),
    };
    let _every = |s: Setup| match s {
        Setup::Cloned | Setup::Adopted => (),
    };
    let toggles = [Skill, Rule, NewSkillsDefault, NewRulesDefault].map(|kind| ToggleChange {
        harness: "claude".into(),
        kind,
        item_id: kind.has_item().then(|| id("01J0DEPLOY")),
        name: kind.has_item().then(|| "deploy".into()),
        enabled: match kind {
            Skill => Some(true),
            Rule => None,
            _ => Some(false),
        },
    });
    let copy = MachineAssignmentCopy {
        source: "laptop".into(),
        harnesses: Some(vec![HarnessMapping { from: "claude".into(), to: "codex".into() }]),
        mode: CopyMode::Replace,
        expected: Some(expected(42)),
        source_expected: Some(expected(17)),
    };
    let join = |code: &str, admin_token: Option<&str>| JoinApiRequest {
        server_url: "https://skills.example.com".into(),
        code: code.into(),
        admin_token: admin_token.map(Into::into),
        machine_id: Some("laptop".into()),
        copy_from: Some("workstation".into()),
        repo: Some("/home/dev/skills".into()),
    };
    let joined = |setup| Joined {
        machine_id: "laptop".into(),
        repo: "/home/dev/skills".into(),
        setup,
        harnesses: vec!["claude".into()],
        revision: 1,
    };
    json!({
        "scope": by_name([Scope::Fleet, Scope::SelfOnly]),
        "assignment_snapshot": sample(AssignmentSnapshot { revision: 42, assignment: assignment(), registration: Some("reg-workstation".into()) }),
        "toggle_kind": by_tag("kind", toggles.clone()),
        "log_cause": by_tag("type", causes),
        "log_entry": by_tag("kind", [Skill, Rule, NewSkillsDefault, NewRulesDefault].map(|kind| log_entry(120, kind, LogCause::Change))),
        "log_query": sample(LogQuery {
            machine: Some("laptop".into()),
            harness: Some("claude".into()),
            author: Some("dev@workstation".into()),
            item: Some("deploy".into()),
            before: Some(51),
            limit: Some(50),
        }),
        "copy_mode": by_tag("mode", [CopyMode::Replace, CopyMode::Merge].map(|mode| MachineAssignmentCopy { mode, ..copy.clone() })),
        "join_api_request": { "code": sample(join("sk1ll-join-7Q2M", None)), "admin": sample(join("", Some("adm"))) },
        "joined": by_tag("setup", [Setup::Cloned, Setup::Adopted].map(joined)),
    })
}

fn draft_golden() -> Value {
    let _every = |c: ChangeKind| match c {
        ChangeKind::Added | ChangeKind::Modified | ChangeKind::Deleted => (),
    };
    let pulls = [
        PullResult::UpToDate,
        PullResult::Updated { from: None, to: "9f3c2a17d4e1".into() },
        PullResult::Blocked {
            baseline: "9f3c2a17d4e1".into(),
            items: vec![item("skill:deploy")],
            pending: true,
            uncommitted: vec!["7d1e9a0 Ship faster".into()],
        },
    ];
    let _every = |p: &PullResult| match p {
        PullResult::UpToDate | PullResult::Updated { .. } | PullResult::Blocked { .. } => (),
    };
    let entry = |change| ReviewDraftEntry {
        entry: DraftEntry {
            item: item("skill:release-notes"),
            change,
            paths: vec!["personal/release-notes/SKILL.md".into()],
        },
        fingerprint: "a1b2c3".into(),
    };
    let view = |draft| ItemView {
        item: item("skill:deploy"),
        id: Some(id("01J0DEPLOY")),
        in_baseline: true,
        draft,
        unpushed: false,
        description: Some("Builds and ships the server container.".into()),
    };
    json!({
        "item_view": [sample(view(Some(ChangeKind::Modified))), sample(ItemView { id: None, description: None, ..view(None) })],
        "draft_entry": by_tag("change", [ChangeKind::Added, ChangeKind::Modified, ChangeKind::Deleted].map(entry)),
        "diff_view": [
            sample(DiffView { item: item("rule:style"), target: Some("instructions/style.md".into()), diff: "@@ -1 +1 @@".into() }),
            sample(DiffView { item: item("skill:deploy"), target: None, diff: String::new() }),
        ],
        "commit_message": sample(CommitMessage { items: vec![item("skill:deploy")], message: "commit skill:deploy from workstation".into() }),
        "commit_request": sample(CommitRequest { items: vec!["skill:deploy".into()], message: Some("Ship faster".into()), author: Some("Dev <dev@example.com>".into()) }),
        "commit_response": sample(CommitResponse {
            sha: "7d1e9a0b3c5f8e2d".into(),
            items: vec![item("skill:deploy")],
            message: "Ship faster".into(),
            pushed: false,
            push_error: Some("connection refused".into()),
            push_refused: false,
            uncommitted: false,
        }),
        "restore_request": sample(RestoreRequest { items: vec!["skill:deploy".into()], all: false }),
        "restore_response": sample(RestoreResponse { discarded: vec![item("skill:deploy")], kept: vec!["k-1".into()] }),
        "pull_result": by_tag("state", pulls.clone()),
        "pull_keep_request": sample(PullKeepRequest { items: vec![], all: true }),
        "pull_keep_result": sample(PullKeepResult { pull: pulls[1].clone(), kept: vec![item("skill:deploy")] }),
        "push_result": sample(PushResult { pushed: true }),
    })
}

fn harness_golden() -> Value {
    let _every = |f: InstructionsFormat| match f {
        InstructionsFormat::Block | InstructionsFormat::Rules => (),
    };
    let _every = |m: LinkMode| match m {
        LinkMode::Symlink | LinkMode::Copy => (),
    };
    let changes = [
        HarnessChange::Enable { name: "kiro".into() },
        HarnessChange::Disable { name: "claude".into() },
        HarnessChange::Set {
            name: "tool".into(),
            harness: harness("/opt/tool/skills", None, InstructionsFormat::Block, LinkMode::Copy),
        },
        HarnessChange::Remove { name: "agents".into() },
    ];
    let _every = |c: &HarnessChange| match c {
        HarnessChange::Enable { .. }
        | HarnessChange::Disable { .. }
        | HarnessChange::Set { .. }
        | HarnessChange::Remove { .. } => (),
    };
    let preview = |format| InstructionPreview {
        path: "/home/dev/.claude/CLAUDE.md".into(),
        format,
        content: "Use two spaces for indentation.\n".into(),
        next_content: "# Mine\nUse two spaces for indentation.\n".into(),
        differs: true,
        diff: "@@ -1 +1 @@".into(),
    };
    let displaced = ConflictView {
        kind: Some("displaced".into()),
        id: Some("d-1".into()),
        item: None,
        path: Some("/home/dev/.local/share/sk1ll/recovered/d-1".into()),
        displaced_at: Some(1_760_000_000),
        detected_at: None,
        harness: "claude".into(),
        name: "notes".into(),
        items: vec![],
        dest: "/home/dev/.claude/skills/notes".into(),
        reason: Some("Unmanaged directory replaced by an assigned Skill.".into()),
        adoptable: true,
        changed: false,
    };
    json!({
        "conflict_view": { "harness": sample(conflict()), "displaced": sample(displaced) },
        "conflict_target": sample(ConflictTarget { harness: "codex".into(), name: "code-review".into() }),
        "harness_view": sample(HarnessView { name: "claude".into(), harness: claude(), origin: "detected".into() }),
        "instructions_format": by_tag("instructions_format", [InstructionsFormat::Block, InstructionsFormat::Rules].map(|instructions_format| Harness { instructions_format, ..claude() })),
        "link_mode": by_tag("mode", [LinkMode::Symlink, LinkMode::Copy].map(|mode| Harness { mode, ..claude() })),
        "harness_settings": sample(HarnessSettings { path: "/home/dev/.config/sk1ll/machine.toml".into(), harnesses: vec![
            choice(),
            HarnessChoice { name: "claude".into(), installed: true, enabled: true, harness: Harness { skills: None, ..claude() }, preset: false, configured: false, available: false, setup_warning: None },
        ] }),
        "harness_change": by_tag("op", changes.clone()),
        "harness_change_request": sample(HarnessChangeRequest { fingerprint: Some("f-1".into()), change: changes[1].clone() }),
        "placement_kind": keyed(["create", "update", "remove", "conflict"].map(|kind| sample(placement(kind, (kind == "conflict").then_some("edited")))), Some("kind")),
        "placement_item": keyed(["skill", "rule", "instructions"].map(|item| sample(PlacementView { item: item.into(), ..placement("create", None) })), Some("item")),
        "harness_preview": sample(HarnessPreview {
            fingerprint: "f-1".into(),
            path: "/home/dev/.config/sk1ll/machine.toml".into(),
            name: "claude".into(),
            before: Some(claude()),
            after: None,
            placed: vec![placement("remove", None), placement("conflict", Some("edited"))],
            warnings: vec!["~/.claude is a symlink".into()],
            sync: vec![],
            awaiting_assignment: false,
        }),
        "harness_applied": sample(HarnessApplied { harnesses: vec![choice()], errors: vec!["sync ~/.kiro/skills: permission denied".into()] }),
        "harness_taken_out": sample(HarnessTakenOut {
            applied: false,
            taken_out: vec![placement("remove", None)],
            left: vec![placement("remove", None)],
            harnesses: vec![choice()],
            errors: vec!["remove ~/.claude/skills/code-review: permission denied".into()],
        }),
        "sync_result": [
            sample(SyncResult {
                fingerprint: Some("s-1".into()),
                conflicts: vec![conflict()],
                actions: Some(vec![placement("update", None)]),
                completed: vec![placement("create", None)],
                changed: Some(placement("update", None)),
                errors: vec!["Could not place notes".into()],
            }),
            sample(SyncResult { fingerprint: None, conflicts: vec![], actions: None, completed: vec![], changed: None, errors: vec![] }),
        ],
        "instruction_preview": by_tag("format", [InstructionsFormat::Block, InstructionsFormat::Rules].map(preview)),
    })
}

fn content_golden() -> Value {
    use sk1ll_core::content::{
        ContentEntry, ContentQuery, ContentType, FileContent, FileEntry, FileQuery, WriteFileRequest,
    };
    let _every = |t: ContentType| match t {
        ContentType::Personal | ContentType::Import | ContentType::Rule | ContentType::Unmanaged => (),
    };
    let _every = |c: Change| match c {
        Change::Added | Change::Modified | Change::Deleted => (),
    };
    let entry = |kind| {
        let unmanaged = kind == ContentType::Unmanaged;
        ContentEntry {
            name: "notes".into(),
            item: (!unmanaged).then(|| item("skill:notes")),
            id: (!unmanaged).then(|| id("01J0NOTES")),
            kind,
            description: Some("Collect notes.".into()),
            draft: (!unmanaged).then_some(Change::Modified),
            in_baseline: !unmanaged,
            path: (!unmanaged).then(|| "/home/dev/skills/personal/notes".into()),
            enabled_harnesses: vec!["claude".into()],
            clashes_with_repo: unmanaged,
            variants: if unmanaged {
                vec![Variant {
                    hash: "sha256:one".into(),
                    description: None,
                    locations: vec![Location {
                        harness: "claude".into(),
                        path: "/home/dev/.claude/skills/notes".into(),
                    }],
                    resumable: true,
                }]
            } else {
                vec![]
            },
        }
    };
    let query = ContentQuery {
        item: "unmanaged:notes".into(),
        hash: Some("sha256:one".into()),
        harness: Some("claude".into()),
    };
    let file = FileQuery { content: query.clone(), path: "SKILL.md".into() };
    json!({
        "content_type": by_tag("type", [ContentType::Personal, ContentType::Import, ContentType::Rule, ContentType::Unmanaged].map(entry)),
        "content_query": sample(query),
        "file_query": sample(file.clone()),
        "file_kind": keyed(["file", "directory", "symlink"].map(|kind| sample(FileEntry { path: "references".into(), kind: kind.into(), size: 120 })), Some("kind")),
        "file_content": [
            sample(FileContent { fingerprint: Some("mock-file".into()), path: "SKILL.md".into(), text: Some("# Notes".into()), binary: false, too_large: false, size: 7 }),
            sample(FileContent { fingerprint: None, path: "logo.png".into(), text: None, binary: true, too_large: false, size: 2048 }),
        ],
        "write_file_request": sample(WriteFileRequest { file, content: "# Notes".into(), expected: "mock-file".into() }),
        "create_item_request": sample(CreateItemRequest { kind: "skill".into(), name: "triage".into(), description: Some("Sorts issues.".into()), bundled: None }),
        "delete_item_request": sample(DeleteItemRequest { kind: "rule".into(), name: "style".into() }),
        "rename_item_request": sample(RenameItemRequest { kind: "skill".into(), name: "deploy".into(), to: "ship".into() }),
        "rename_item_response": sample(RenameItemResponse { item: item("skill:ship") }),
    })
}

fn imports_golden() -> Value {
    use WarningKind::*;
    let _every = |k: WarningKind| match k {
        Executable | Script | Binary | Large => (),
    };
    let candidate = SkillCandidate {
        path: "skills/pdf".into(),
        default_name: "pdf".into(),
        description: Some("Tools for pdf.".into()),
        taken: false,
        total_bytes: 2048,
        warnings: vec![FileWarning { path: "scripts/check.py".into(), kinds: vec![Executable, Script, Binary, Large] }],
    };
    json!({
        "imports_view": sample(ImportsView {
            checked_at: Some(1_760_000_000),
            updates: vec![import_update()],
            errors: vec![ImportError { name: "internal-skills".into(), message: "git fetch failed".into() }],
            skipped: vec![sk1ll_proto::ImportSkipped { name: "private".into(), reason: "not allowed by the Server".into() }],
            imports: vec![ImportEntry { name: "pdf".into(), source: "github:example/skills".into(), git_ref: "main".into(), path: "skills/pdf".into(), locked: Some("4e5f6a7b8c9d".into()) }],
        }),
        "import_updated": sample(ImportUpdated { name: "pdf".into(), old_commit: Some("4e5f6a7b8c9d".into()), new_commit: "c0ffee123456".into(), changed: true }),
        "resolve_request": sample(ResolveImportRequest { link: "https://github.com/example/skills".into(), path: "skills/pdf".into(), git_ref: "v2".into() }),
        "resolved_source": sample(ResolvedSource { source: "github:example/skills".into(), git_ref: "main".into(), commit: "c0ffee1234567890".into(), candidates: vec![candidate] }),
        "warning_kind": by_name([Executable, Script, Binary, Large]),
        "add_import_request": sample(AddImportRequest {
            name: "pdf".into(),
            source: "github:example/skills".into(),
            path: Some("skills/pdf".into()),
            git_ref: Some("main".into()),
            commit: Some("c0ffee1234567890".into()),
        }),
        "import_changes": sample(ImportChanges {
            locked: "4e5f6a7b8c9d".into(),
            upstream: "c0ffee123456".into(),
            commits: vec![ImportCommit { sha: "c0ffee1".into(), date: "2026-11-01".into(), subject: "Fix PDF tables".into() }],
            truncated: false,
            files: vec!["SKILL.md".into()],
        }),
    })
}

fn app_golden() -> Value {
    let kinds = [EditorKind::Gui, EditorKind::Terminal, EditorKind::System];
    let _every = |k: EditorKind| match k {
        EditorKind::Gui | EditorKind::Terminal | EditorKind::System => (),
    };
    let progress = [
        Progress::Downloading { percent: Some(40) },
        Progress::Installing,
        Progress::RestartingDaemon,
        Progress::Relaunching,
    ];
    let _every = |p: &Progress| match p {
        Progress::Downloading { .. } | Progress::Installing | Progress::RestartingDaemon | Progress::Relaunching => (),
    };
    let _every = |s: Started| match s {
        Started::Systemd | Started::Launchd | Started::Process => (),
    };
    let _every = |m: Manager| match m {
        Manager::Systemd | Manager::Launchd => (),
    };
    let editors = EditorsView {
        preferred_editor: "code".into(),
        editors: kinds
            .iter()
            .map(|&kind| EditorView { id: format!("{kind:?}").to_lowercase(), name: format!("{kind:?}"), kind })
            .collect(),
    };
    let service = ServiceInfo {
        default_socket: true,
        binary: Some(PathBuf::from("/home/dev/.cargo/bin/sk1ll")),
        manager: Manager::Systemd,
        available: true,
        active: false,
        restartable: false,
        unit: Some("sk1ll-daemon.service".into()),
    };
    json!({
        "editors_view": sample(editors),
        "editor_kind": by_tag("kind", kinds.map(|kind| EditorView { id: "code".into(), name: "VS Code".into(), kind })),
        "service_info": written(service),
        "service_manager": keyed([Manager::Systemd, Manager::Launchd].map(written), None),
        "daemon_started": keyed([Started::Systemd, Started::Launchd, Started::Process].map(written), None),
        "available_update": [
            sample(AvailableUpdate { version: "0.2.0".into(), current_version: "0.1.0".into(), notes: Some("### Added".into()), date: Some("2026-11-01T00:00:00Z".into()) }),
            sample(AvailableUpdate { version: "0.2.0".into(), current_version: "0.1.0".into(), notes: None, date: None }),
        ],
        "update_progress": by_tag("stage", progress),
    })
}

#[test]
fn every_api_type_the_gui_consumes_matches_its_golden_json_and_round_trips() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../contract");
    let bless = std::env::var_os("SK1LL_BLESS").is_some_and(|v| v == "1");
    let goldens = [
        ("status", status_golden()),
        ("fleet", fleet_golden()),
        ("draft", draft_golden()),
        ("harness", harness_golden()),
        ("content", content_golden()),
        ("imports", imports_golden()),
        ("app", app_golden()),
    ];
    let mut drifted = vec![];
    for (name, golden) in goldens {
        let path = dir.join(format!("{name}.json"));
        let text = serde_json::to_string_pretty(&golden).unwrap() + "\n";
        if bless {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(&path, text).unwrap();
        } else if std::fs::read_to_string(&path).ok().as_deref() != Some(&text) {
            drifted.push(path.display().to_string());
        }
    }
    assert!(
        drifted.is_empty(),
        "the API's JSON no longer matches {drifted:?}. If the change is intended, run with SK1LL_BLESS=1, check the \
         diff, and update ui/src/types.ts until `pnpm tsc --noEmit` passes"
    );
}
