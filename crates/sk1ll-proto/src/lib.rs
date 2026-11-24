//! Wire types shared by the server, the daemon and the GUI: WebSocket messages,
//! REST bodies, machine status and the assignment log. Only serde types and
//! pure functions live here, no I/O or async runtime.
//!
//! Timestamps are Unix seconds (UTC).

use serde::{Deserialize, Serialize};

pub use sk1ll_core::ItemId;
pub use sk1ll_core::assignment::{Assignment, HarnessAssignment};

/// Path of the fleet repo under the server's base URL.
pub const REPO_PATH: &str = "/git/repo.git";

// ---------------------------------------------------------------------------
// Protocol version

/// The version of the server ↔ daemon protocol: the REST API, the WebSocket
/// messages and what they mean. Bump it on any change an older peer would
/// misread; additive fields with serde defaults don't need a bump.
pub const PROTOCOL_VERSION: u32 = 1;

/// Request header carrying the sender's [`PROTOCOL_VERSION`]. The daemon sends
/// it on every `/api` request; the server sends it on every response. A
/// request without it (e.g. `curl` with the admin token) is accepted.
pub const PROTOCOL_HEADER: &str = "sk1ll-protocol";

/// Response header carrying the server's release version, next to
/// [`PROTOCOL_HEADER`] on every response (a refusal included), so a daemon
/// can say which side is older.
pub const VERSION_HEADER: &str = "sk1ll-version";

/// Why a peer speaking `theirs` can't talk to one speaking [`PROTOCOL_VERSION`].
pub fn protocol_mismatch(theirs: &str) -> String {
    format!(
        "sk1ll protocol mismatch: the server speaks version {PROTOCOL_VERSION}, this machine speaks {theirs}; \
         upgrade the older side so the server and every daemon run the same sk1ll release"
    )
}

// ---------------------------------------------------------------------------
// Items

/// A skill, rule, the manifest or another repo path, as core names it in
/// drafts. On the wire it is a string: `skill:pdf`, `rule:10-git`,
/// `manifest` or `other:<path>`; it never carries content.
pub use sk1ll_core::draft::ItemRef;

// ---------------------------------------------------------------------------
// WebSocket (`GET /api/ws`), JSON text frames tagged by `type`.

/// Daemon → server. The server handles them in order, so a write follows
/// the status sent before it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonMessage {
    /// First message after connecting; must match the token's machine.
    Hello { machine_id: String },
    /// The machine's current state; send on connect and whenever it changes.
    Status(MachineReport),
    /// A change to this machine's own assignment. Answered with
    /// [`ServerMessage::AssignmentResult`] carrying the same `req`.
    SetAssignment { req: u64, write: AssignmentWrite },
}

/// A change to one machine's assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AssignmentWrite {
    /// Toggles and defaults, as `PUT /api/machines/{id}/assignment` takes them.
    Changes(AssignmentChanges),
    /// Both defaults and every toggle of one harness, as
    /// `POST /api/machines/{id}/assignment/harness/{harness}` sets them.
    Harness { harness: String, enabled: bool },
    /// Moves a harness's toggles and defaults to a new harness name.
    RenameHarness(HarnessRename),
}

/// Server → daemon.
///
/// Liveness: the server pings at a configurable interval and closes a connection
/// it has received nothing from (no frame of any kind, pongs included) for
/// max(2 × ping interval, offline-after). Daemons must keep reading so that
/// pongs go out, and should reconnect when closed. A daemon that doesn't
/// read its frames in time is disconnected too.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// The outcome of [`DaemonMessage::SetAssignment`] `req`. `seq` is the
    /// [`MachineReport::seq`] of the last status the server stored from this
    /// session before applying it (0: none yet).
    AssignmentResult { req: u64, seq: u64, result: AssignmentOutcome },
    /// This machine's full assignment. Sent on connect and after every change;
    /// snapshots that were superseded before they could be sent are skipped,
    /// so only the latest is guaranteed to arrive. Apply one only if its
    /// `revision` is newer than the last one applied.
    Assignment(AssignmentSnapshot),
    /// The baseline is now `baseline`; pull. Sent on connect and after every push.
    Nudge { baseline: String },
    /// The machine was removed from the fleet; its token no longer works. The
    /// server closes the connection right after.
    Revoked,
    /// The set of imports with a newer upstream commit changed.
    ImportsChanged { updates: Vec<ImportUpdate> },
}

/// What became of an assignment write sent over the session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentOutcome {
    Applied(AssignmentUpdate),
    /// Refused with what the REST API would answer: its HTTP status and error.
    Refused {
        status: u16,
        error: String,
    },
}

/// A machine's assignment at one revision. On the wire the revision sits next
/// to the assignment's own fields: `{"type": "assignment", "revision": 7,
/// "harnesses": {...}}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignmentSnapshot {
    /// Grows with every change. It comes from one server-wide sequence, so it
    /// also grows across removing and re-joining a machine.
    pub revision: u64,
    #[serde(flatten)]
    pub assignment: Assignment,
    /// The machine's [`MachineRow::registration`] when the Server read it for
    /// `GET /api/machines/{id}/assignment`; absent elsewhere and standalone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration: Option<String>,
}

/// What a daemon reports about its machine. Names and ids only, never content.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineReport {
    /// Grows with every status a daemon session sends, from 1; the server
    /// echoes it (see [`ServerMessage::AssignmentResult`]).
    #[serde(default)]
    pub seq: u64,
    /// Commit the machine's harnesses are built from; `None` before the first pull.
    #[serde(default)]
    pub baseline: Option<String>,
    /// Items with uncommitted changes in the machine's repo. The server skips
    /// (and logs) entries it can't parse instead of dropping the report.
    #[serde(default)]
    pub drafts: Vec<ItemRef>,
    /// Items in commits the machine has committed but not yet pushed to the server.
    #[serde(default)]
    pub unpushed: Vec<ItemRef>,
    /// The skills and rules in the machine's working tree, including
    /// items in drafts or unpushed commits.
    #[serde(default)]
    pub items: Vec<ReportedItem>,
    #[serde(default)]
    pub inventory_complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inventory_error: Option<String>,
    #[serde(default)]
    pub conflicts: Vec<ConflictRef>,
    /// Harnesses present on the machine.
    #[serde(default)]
    pub harnesses: Vec<String>,
    /// A newer baseline clashes with the draft, so the machine stays on its old one.
    #[serde(default)]
    pub blocked: bool,
    /// Draft items that clash with the newer baseline (when `blocked`).
    #[serde(default)]
    pub clashes: Vec<ItemRef>,
    /// A journal is still active. The entries are the items blocking it, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_pending: Option<Vec<ItemRef>>,
    /// The daemon knows it hasn't applied the latest baseline yet.
    #[serde(default)]
    pub behind: bool,
    /// The server refused this Machine's unpushed commits for good (its
    /// check of a new Baseline): why. Pushing again won't help.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_refused: Option<String>,
}

/// A skill or rule's name and Item ID.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ReportedItem {
    pub item: ItemRef,
    pub id: ItemId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictRef {
    pub item: ItemRef,
    pub harness: String,
}

// ---------------------------------------------------------------------------
// Status

/// A machine's headline status, ordered from most to least severe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineStatus {
    Offline,
    Blocked,
    Conflict,
    Behind,
    Drafting,
    InSync,
}

/// The most severe status that applies. `offline` is decided by the server (not
/// seen recently: `last_seen` older than its offline-after, whether or not a
/// connection is registered); `server_baseline` is the server repo's
/// HEAD (`None` while it is empty). A machine that never reported is Behind
/// once the server has a baseline.
pub fn headline(offline: bool, report: Option<&MachineReport>, server_baseline: Option<&str>) -> MachineStatus {
    if offline {
        return MachineStatus::Offline;
    }
    let empty = MachineReport::default();
    let r = report.unwrap_or(&empty);
    if r.blocked || r.pull_pending.is_some() {
        MachineStatus::Blocked
    } else if !r.conflicts.is_empty() {
        MachineStatus::Conflict
    } else if r.behind || server_baseline.is_some_and(|h| r.baseline.as_deref() != Some(h)) {
        MachineStatus::Behind
    } else if !r.drafts.is_empty() {
        MachineStatus::Drafting
    } else {
        MachineStatus::InSync
    }
}

/// `GET /api/fleet`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetView {
    /// The server repo's HEAD; `None` until the first push.
    pub baseline: Option<String>,
    pub machines: Vec<MachineRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineRow {
    pub id: String,
    pub status: MachineStatus,
    /// Has a registered WebSocket connection. Status is based on `last_seen`
    /// alone, so a connected machine can still be Offline.
    pub connected: bool,
    pub joined_at: i64,
    pub last_seen: Option<i64>,
    /// Last report, if any.
    pub report: Option<MachineReport>,
    /// What the machine may manage, from the join code it joined with.
    pub scope: Scope,
    /// Random and new for every join, unlike `joined_at`, which two joins in
    /// the same second share.
    pub registration: String,
}

/// What a machine that joined with a join code may manage (ADR 0005). The
/// admin token may always manage the whole fleet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Create join codes, change any machine's assignment, undo any log
    /// entry and remove any machine.
    #[default]
    Fleet,
    /// Only itself: its own assignment, undoing entries for it, and
    /// removing itself. It can still read the fleet view and the log.
    #[serde(rename = "self")]
    SelfOnly,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fleet => "fleet",
            Self::SelfOnly => "self",
        }
    }
}

impl std::str::FromStr for Scope {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "fleet" => Ok(Self::Fleet),
            "self" => Ok(Self::SelfOnly),
            _ => Err(format!("unknown scope `{s}`; use fleet or self")),
        }
    }
}

// ---------------------------------------------------------------------------
// Assignment changes and log

/// What a toggle change addresses within one harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToggleKind {
    Skill,
    Rule,
    NewSkillsDefault,
    NewRulesDefault,
}

impl ToggleKind {
    /// Whether `item_id` is required (item toggles) or must be absent (defaults).
    pub fn has_item(self) -> bool {
        matches!(self, Self::Skill | Self::Rule)
    }
}

/// One change in `PUT /api/machines/{id}/assignment`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToggleChange {
    pub harness: String,
    pub kind: ToggleKind,
    /// The item an item toggle is for. Any id is accepted: one no item has
    /// is inert until such an item appears.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<ItemId>,
    /// The item's name, recorded in the log for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `null` removes an item's explicit toggle so it follows the default
    /// again; defaults always need a value.
    pub enabled: Option<bool>,
}

/// `PUT /api/machines/{id}/assignment` body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignmentChanges {
    pub changes: Vec<ToggleChange>,
    /// Apply only to this state of the assignment, checked in the same
    /// transaction; otherwise the write fails with HTTP 409 and changes nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<Expected>,
}

/// `POST /api/machines/{id}/assignment/harness/{harness}` body: set both
/// defaults and every explicit toggle of the harness, so that every item,
/// pulled or not, has that value there.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SetHarnessAssignment {
    pub enabled: bool,
}

/// Move a harness's Assignment once for this operation ID and registration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessRename {
    pub from: String,
    pub to: String,
    pub operation_id: String,
    #[serde(default)]
    pub replace: bool,
}

/// A precondition of [`AssignmentChanges`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Expected {
    /// The assignment's current revision.
    pub revision: u64,
    /// The machine's current [`MachineRow::registration`]; `None` skips the check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration: Option<String>,
}

/// Copy a joined Machine's Assignment to another Machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineAssignmentCopy {
    pub source: String,
    pub harnesses: Option<Vec<HarnessMapping>>,
    pub mode: CopyMode,
    pub expected: Option<Expected>,
    /// Bind confirmation to the source snapshot used by the preview.
    pub source_expected: Option<Expected>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessMapping {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CopyMode {
    Replace,
    Merge,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignmentCopyPreview {
    pub count: usize,
    /// What the copy would change on the target, one entry per change.
    #[serde(default)]
    pub changes: Vec<CopyChange>,
    pub target: Expected,
    pub source: Expected,
}

/// One change an Assignment copy would make. For item toggles `None` means
/// "no explicit toggle" (the item follows the default).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyChange {
    pub harness: String,
    pub kind: ToggleKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<ItemId>,
    /// The item's name, where the server knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub before: Option<bool>,
    pub after: Option<bool>,
}

/// `PUT /api/machines/{id}/assignment` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignmentUpdate {
    pub assignment: Assignment,
    /// The assignment's revision after the change (see [`AssignmentSnapshot`]).
    pub revision: u64,
    /// Log entries written, one per change that actually changed something.
    pub entries: Vec<LogEntry>,
    /// Errors applying this update to local placements, when applied by a daemon.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

/// One append-only assignment log entry. For item toggles `None` means "no
/// explicit toggle"; for defaults it means the harness had no entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    pub id: i64,
    pub at: i64,
    /// Machine id that made the change, `admin` for the bootstrap token, or
    /// `server` for changes the server made itself.
    pub author: String,
    /// The machine whose assignment changed.
    pub machine: String,
    pub harness: String,
    pub kind: ToggleKind,
    /// The item of an item toggle.
    pub item_id: Option<ItemId>,
    /// The item's name when the entry was written, if known; for display.
    pub name: Option<String>,
    pub before: Option<bool>,
    pub after: Option<bool>,
    pub cause: LogCause,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LogCause {
    /// An explicit change through the API.
    Change,
    /// Restores `before` of entry `entry`.
    Undo { entry: i64 },
    /// A harness's new-items default changed, so an existing item without a
    /// toggle got an explicit one with its old effective value.
    DefaultMaterialized,
    /// The machine was removed from the fleet.
    MachineRemoved,
    /// The machine reported a harness it had no assignment for; it starts all off.
    HarnessAdded,
    /// The machine's harness was renamed: its toggles moved from `from` to `to`.
    HarnessRenamed { from: String, to: String },
    /// The toggle's item is in neither the baseline nor any machine's
    /// report, and someone pruned the orphans.
    OrphanPruned,
}

/// `GET /api/log` query.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogQuery {
    /// Only entries for this machine.
    pub machine: Option<String>,
    /// Only entries for this harness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// Only entries by this author.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Only entries whose `<kind>:<name>` (`skill:pdf`, `new_skills_default:`)
    /// or item id contains this, ignoring ASCII case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    /// Only entries with an id below this (paging, newest first). Filters
    /// apply before the limit, so the last id of a page is the next cursor.
    pub before: Option<i64>,
    /// Default 100, max 1000.
    pub limit: Option<u32>,
}

/// A toggle for an item that is in neither the baseline nor any machine's
/// last report. It does nothing; `POST /api/assignment/orphans/prune` removes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Orphan {
    pub machine: String,
    pub harness: String,
    /// `skill` or `rule`.
    pub kind: ToggleKind,
    pub item_id: ItemId,
    /// The name the log last recorded for the item, if any.
    pub name: Option<String>,
}

/// `GET /api/assignment/orphans`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrphansView {
    pub orphans: Vec<Orphan>,
}

/// `POST /api/assignment/orphans/prune` body. `machine: None` prunes every
/// machine for a `fleet` caller or the admin, and the caller's own for a
/// `self` machine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneOrphans {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
}

/// `POST /api/assignment/orphans/prune` response: one entry per toggle removed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrphansPruned {
    pub entries: Vec<LogEntry>,
}

/// `GET /api/log/facets`: the distinct values in the whole log, sorted, for filters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogFacets {
    pub machines: Vec<String>,
    pub harnesses: Vec<String>,
    pub authors: Vec<String>,
}

// ---------------------------------------------------------------------------
// Joining

/// `POST /api/join` body (unauthenticated; the code is the credential).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinRequest {
    pub code: String,
    pub machine_id: String,
    #[serde(default)]
    pub harnesses: Vec<String>,
    /// Start from this machine's assignment; otherwise everything starts off.
    #[serde(default)]
    pub copy_from: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinResponse {
    pub machine_id: String,
    /// Bearer token for the API and password for git over HTTP. Shown once.
    pub token: String,
    /// Path of the fleet repo, relative to the server's base URL.
    pub repo_path: String,
    pub assignment: Assignment,
    /// The assignment's revision (see [`AssignmentSnapshot`]).
    pub revision: u64,
}

/// `POST /api/join-codes` body. An empty body or `null` asks for the defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinCodeRequest {
    /// What the machine that joins with the code may manage.
    #[serde(default)]
    pub scope: Scope,
}

/// `POST /api/join-codes` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinCode {
    pub code: String,
    /// When the code stops working.
    pub expires_at: i64,
    /// The scope the code grants.
    pub scope: Scope,
}

// ---------------------------------------------------------------------------
// Imports

/// An import whose upstream ref now points at a different commit than locked.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ImportUpdate {
    pub name: String,
    pub source: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub locked: String,
    pub upstream: String,
}

/// `GET /api/imports`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportsView {
    /// When the last check finished; `None` before the first.
    pub checked_at: Option<i64>,
    pub updates: Vec<ImportUpdate>,
    /// Imports that couldn't be checked, e.g. unreachable sources.
    pub errors: Vec<ImportError>,
    /// Imports the server didn't check on purpose (its import check settings).
    #[serde(default)]
    pub skipped: Vec<ImportSkipped>,
}

/// An import the server's settings kept it from checking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportSkipped {
    pub name: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportError {
    pub name: String,
    pub message: String,
}

/// Body of every non-2xx API response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> ItemId {
        s.parse().unwrap()
    }

    #[test]
    fn assignment_writes_round_trip() {
        let changes = AssignmentChanges {
            changes: vec![ToggleChange {
                harness: "claude".into(),
                kind: ToggleKind::Skill,
                item_id: Some(id("01J0")),
                name: Some("pdf".into()),
                enabled: Some(true),
            }],
            expected: Some(Expected { revision: 3, registration: None }),
        };
        for write in [
            AssignmentWrite::Changes(changes),
            AssignmentWrite::Harness { harness: "claude".into(), enabled: false },
            AssignmentWrite::RenameHarness(HarnessRename {
                from: "a".into(),
                to: "b".into(),
                operation_id: "op".into(),
                replace: false,
            }),
        ] {
            let msg = DaemonMessage::SetAssignment { req: 7, write };
            let text = serde_json::to_string(&msg).unwrap();
            assert!(text.starts_with(r#"{"type":"set_assignment","req":7,"write":{"op":"#), "{text}");
            assert_eq!(serde_json::from_str::<DaemonMessage>(&text).unwrap(), msg);
        }
        let refused = ServerMessage::AssignmentResult {
            req: 7,
            seq: 2,
            result: AssignmentOutcome::Refused { status: 409, error: "changed".into() },
        };
        let value = serde_json::to_value(&refused).unwrap();
        assert_eq!(value["result"]["refused"]["status"], 409);
        assert_eq!(serde_json::from_value::<ServerMessage>(value).unwrap(), refused);
    }

    #[test]
    fn item_refs_are_strings() {
        let r: ItemRef = "skill:pdf".parse().unwrap();
        assert_eq!(r, ItemRef::Skill("pdf".into()));
        assert_eq!(serde_json::to_string(&ItemRef::Rule("10-git".into())).unwrap(), "\"rule:10-git\"");
        assert!("pdf".parse::<ItemRef>().is_err());
        assert!("fragment:x".parse::<ItemRef>().is_err());
        assert!("skill:".parse::<ItemRef>().is_err());
    }

    #[test]
    fn reports_accept_every_draft_item_and_carry_ids() {
        let text = r#"{"type":"status","seq":4,"drafts":["skill:pdf","rule:git","manifest","other:README.md"],
            "items":[{"item":"skill:pdf","id":"01J0"}]}"#;
        let DaemonMessage::Status(r) = serde_json::from_str(text).unwrap() else { panic!("expected status") };
        assert_eq!(
            r.drafts,
            [
                ItemRef::Skill("pdf".into()),
                ItemRef::Rule("git".into()),
                ItemRef::Manifest,
                ItemRef::Other("README.md".into())
            ]
        );
        assert_eq!((r.seq, &r.items[0].id), (4, &id("01J0")));
        let back = serde_json::to_string(&DaemonMessage::Status(r.clone())).unwrap();
        assert_eq!(serde_json::from_str::<DaemonMessage>(&back).unwrap(), DaemonMessage::Status(r));
    }

    #[test]
    fn assignment_snapshots_carry_a_revision() {
        let mut a = Assignment::default();
        a.set_skill("claude", &id("01J0"), true);
        let m =
            ServerMessage::Assignment(AssignmentSnapshot { revision: 7, assignment: a.clone(), registration: None });
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["type"], "assignment");
        assert_eq!(v["revision"], 7);
        assert_eq!(v["harnesses"]["claude"]["skills"]["01J0"], true);
        assert_eq!(serde_json::from_value::<ServerMessage>(v).unwrap(), m);
    }

    #[test]
    fn messages_are_tagged() {
        let hello = serde_json::to_value(DaemonMessage::Hello { machine_id: "ws".into() }).unwrap();
        assert_eq!(hello, serde_json::json!({"type": "hello", "machine_id": "ws"}));
        let status =
            DaemonMessage::Status(MachineReport { drafts: vec![ItemRef::Skill("pdf".into())], ..Default::default() });
        let text = serde_json::to_string(&status).unwrap();
        assert!(text.starts_with(r#"{"type":"status""#), "{text}");
        assert_eq!(serde_json::from_str::<DaemonMessage>(&text).unwrap(), status);
        let mut a = Assignment::default();
        a.set_skill("claude", &id("01J0"), false);
        let snapshot = AssignmentSnapshot { revision: 1, assignment: a, registration: None };
        for m in [
            ServerMessage::Assignment(snapshot),
            ServerMessage::Nudge { baseline: "abc".into() },
            ServerMessage::Revoked,
        ] {
            let text = serde_json::to_string(&m).unwrap();
            assert_eq!(serde_json::from_str::<ServerMessage>(&text).unwrap(), m);
        }
        assert_eq!(serde_json::to_string(&ServerMessage::Revoked).unwrap(), r#"{"type":"revoked"}"#);
    }

    #[test]
    fn scopes_parse() {
        assert_eq!(serde_json::to_value(Scope::SelfOnly).unwrap(), "self");
        assert_eq!("fleet".parse::<Scope>().unwrap(), Scope::Fleet);
        assert!("admin".parse::<Scope>().is_err());
        assert_eq!(serde_json::from_str::<JoinCodeRequest>("{}").unwrap().scope, Scope::Fleet);
    }

    #[test]
    fn headline_picks_most_severe() {
        use MachineStatus::*;
        let head = Some("h1");
        let synced = MachineReport { baseline: Some("h1".into()), ..Default::default() };
        assert_eq!(headline(false, Some(&synced), head), InSync);
        assert_eq!(headline(true, Some(&synced), head), Offline);

        let drafting = MachineReport { drafts: vec![ItemRef::Skill("pdf".into())], ..synced.clone() };
        assert_eq!(headline(false, Some(&drafting), head), Drafting);

        let behind = MachineReport { baseline: Some("h0".into()), ..drafting.clone() };
        assert_eq!(headline(false, Some(&behind), head), Behind);
        let self_reported = MachineReport { behind: true, ..synced.clone() };
        assert_eq!(headline(false, Some(&self_reported), head), Behind);

        let conflict = MachineReport {
            conflicts: vec![ConflictRef { item: ItemRef::Rule("git".into()), harness: "codex".into() }],
            ..behind.clone()
        };
        assert_eq!(headline(false, Some(&conflict), head), Conflict);

        let blocked = MachineReport { blocked: true, clashes: vec![ItemRef::Skill("pdf".into())], ..conflict.clone() };
        assert_eq!(headline(false, Some(&blocked), head), Blocked);
        assert_eq!(headline(true, Some(&blocked), head), Offline);

        assert_eq!(headline(false, None, head), Behind);
        assert_eq!(headline(false, None, None), InSync);
        assert!(Offline < Blocked && Drafting < InSync);
    }
}
