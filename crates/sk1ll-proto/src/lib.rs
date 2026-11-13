//! Wire types shared by the server, the daemon and the GUI: WebSocket messages,
//! REST bodies, machine status and the assignment log. Only serde types and
//! pure functions live here, no I/O or async runtime.
//!
//! Timestamps are Unix seconds (UTC).

use serde::{Deserialize, Serialize};

pub use sk1ll_core::assignment::{Assignment, HarnessAssignment};

/// Path of the fleet repo under the server's base URL.
pub const REPO_PATH: &str = "/git/repo.git";

// ---------------------------------------------------------------------------
// Items

/// A skill, rule, the manifest or another repo path, as core names it in
/// drafts. On the wire it is a string: `skill:pdf`, `rule:10-git`,
/// `manifest` or `other:<path>`; it never carries content.
pub use sk1ll_core::draft::ItemRef;

// ---------------------------------------------------------------------------
// WebSocket (`GET /api/ws`), JSON text frames tagged by `type`.

/// Daemon → server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonMessage {
    /// First message after connecting; must match the token's machine.
    Hello { machine_id: String },
    /// The machine's current state; send on connect and whenever it changes.
    Status(MachineReport),
    /// The user Restore these draft items. Send after the updated status so
    /// the server can drop toggles for items absent from both baseline and draft.
    DraftDiscarded {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<u64>,
        items: Vec<ItemRef>,
    },
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
    /// Confirms that the discard transaction committed.
    DraftDiscardApplied { id: u64 },
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

/// A machine's assignment at one revision. On the wire the revision sits next
/// to the assignment's own fields: `{"type": "assignment", "revision": 7,
/// "harnesses": {...}}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignmentSnapshot {
    /// Grows with every change. It comes from one server-wide sequence, so it
    /// also grows across removing and re-joining a machine. 0 means unknown.
    #[serde(default)]
    pub revision: u64,
    #[serde(flatten)]
    pub assignment: Assignment,
}

/// What a daemon reports about its machine. Names only, never content.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineReport {
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
    /// The daemon knows it hasn't applied the latest baseline yet.
    #[serde(default)]
    pub behind: bool,
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
    if r.blocked {
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
}

// ---------------------------------------------------------------------------
// Assignment changes and log

/// What a toggle change addresses within one harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToggleKind {
    Skill,
    Rule,
    NewSkillsDefault,
    NewRulesDefault,
}

impl ToggleKind {
    /// Whether `name` is required (item toggles) or must be absent (defaults).
    pub fn has_name(self) -> bool {
        matches!(self, Self::Skill | Self::Rule)
    }
}

/// One change in `PUT /api/machines/{id}/assignment`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToggleChange {
    pub harness: String,
    pub kind: ToggleKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `null` removes an item's explicit toggle so it follows the default
    /// again; defaults always need a value.
    pub enabled: Option<bool>,
}

/// `PUT /api/machines/{id}/assignment` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignmentChanges {
    pub changes: Vec<ToggleChange>,
}

/// `PUT /api/machines/{id}/assignment` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignmentUpdate {
    pub assignment: Assignment,
    /// The assignment's revision after the change (see [`AssignmentSnapshot`]).
    #[serde(default)]
    pub revision: u64,
    /// Log entries written, one per change that actually changed something.
    pub entries: Vec<LogEntry>,
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
    /// The item is in neither the baseline nor the machine's reported draft.
    ItemRemoved,
    /// A harness's new-items default changed, so an existing baseline item
    /// without a toggle got an explicit one with its old effective value.
    DefaultMaterialized,
    /// The machine was removed from the fleet.
    MachineRemoved,
    /// The machine reported a harness it had no assignment for; it starts all off.
    HarnessAdded,
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
    /// contains this, ignoring ASCII case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    /// Only entries with an id below this (paging, newest first). Filters
    /// apply before the limit, so the last id of a page is the next cursor.
    pub before: Option<i64>,
    /// Default 100, max 1000.
    pub limit: Option<u32>,
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
    #[serde(default)]
    pub revision: u64,
}

/// `POST /api/join-codes` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinCode {
    pub code: String,
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

    #[test]
    fn discard_ack_round_trip() {
        for id in [None, Some(42)] {
            let msg = DaemonMessage::DraftDiscarded { id, items: vec![] };
            let text = serde_json::to_string(&msg).unwrap();
            assert_eq!(serde_json::from_str::<DaemonMessage>(&text).unwrap(), msg);
        }
        let msg = ServerMessage::DraftDiscardApplied { id: 42 };
        let text = serde_json::to_string(&msg).unwrap();
        assert_eq!(text, r#"{"type":"draft_discard_applied","id":42}"#);
        assert_eq!(serde_json::from_str::<ServerMessage>(&text).unwrap(), msg);
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
    fn reports_accept_every_draft_item() {
        let text = r#"{"type":"status","drafts":["skill:pdf","rule:git","manifest","other:README.md"]}"#;
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
        let back = serde_json::to_string(&DaemonMessage::Status(r.clone())).unwrap();
        assert_eq!(serde_json::from_str::<DaemonMessage>(&back).unwrap(), DaemonMessage::Status(r));
    }

    #[test]
    fn draft_discarded_wire_format() {
        let value = serde_json::json!({"type": "draft_discarded", "items": ["skill:wip", "rule:git", "manifest", "other:notes.txt"]});
        let message: DaemonMessage = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(message).unwrap(), value);
    }

    #[test]
    fn assignment_snapshots_carry_a_revision() {
        let mut a = Assignment::default();
        a.set_skill("claude", "pdf", true);
        let m = ServerMessage::Assignment(AssignmentSnapshot { revision: 7, assignment: a.clone() });
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["type"], "assignment");
        assert_eq!(v["revision"], 7);
        assert_eq!(v["harnesses"]["claude"]["skills"]["pdf"], true);
        assert_eq!(serde_json::from_value::<ServerMessage>(v).unwrap(), m);
        // Without a revision (an older server) it reads as 0.
        let old: ServerMessage = serde_json::from_str(r#"{"type":"assignment","harnesses":{}}"#).unwrap();
        assert_eq!(old, ServerMessage::Assignment(AssignmentSnapshot::default()));
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
        a.set_skill("claude", "pdf", false);
        let snapshot = AssignmentSnapshot { revision: 1, assignment: a };
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
