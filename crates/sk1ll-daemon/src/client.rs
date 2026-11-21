//! The daemon's local API: request and response types, and a small async
//! client for it. The CLI and the GUI use this to talk to the daemon over
//! its Unix domain socket (HTTP/1.1 + JSON).
//!
//! | Method | Path | Body → Response |
//! |---|---|---|
//! | GET | `/status` | → [`StatusView`] |
//! | GET | `/items` | → `Vec<`[`ItemView`]`>` |
//! | GET | `/skills` | → `Vec<ContentEntry>` |
//! | GET | `/skills/tree?item=skill:pdf` | → `Vec<FileEntry>` |
//! | GET | `/skills/file?item=skill:pdf&path=SKILL.md` | → `FileContent` |
//! | POST | `/skills/file` | `WriteFileRequest` → 204, or 409 if the file changed |
//! | POST | `/skills/adopt` | `ConflictTarget` → 204; unmanaged Adopt needs a Server connection when joined |
//! | POST | `/items/create` | [`CreateItemRequest`] → 204 |
//! | POST | `/items/delete` | [`DeleteItemRequest`] → 204 (a Draft deletion) |
//! | POST | `/items/rename` | [`RenameItemRequest`] → [`RenameItemResponse`] |
//! | GET | `/draft` | → `Vec<`[`DraftEntry`]`>` |
//! | GET | `/draft/diff?item=` | → [`DiffView`] |
//! | GET | `/commit/message?items=a,b` | → [`CommitMessage`] |
//! | POST | `/commit` | [`CommitRequest`] → [`CommitResponse`] |
//! | POST | `/restore` | [`RestoreRequest`] → [`RestoreResponse`] |
//! | POST | `/pull` | → [`PullResult`] |
//! | GET | `/conflicts` | → `Vec<`[`ConflictView`]`>` |
//! | GET | `/conflicts/displaced` | → all recovered entries |
//! | POST | `/conflicts/displaced/{id}/adopt`, `/conflicts/displaced/{id}/discard` | → 204 |
//! | POST | `/conflicts/adopt`, `/conflicts/overwrite` | [`ConflictTarget`] → 204 |
//! | GET | `/harnesses` | → `Vec<`[`HarnessView`]`>` |
//! | GET | `/setup/harnesses` | → `Vec<`[`HarnessChoice`]`>` |
//! | POST | `/setup/harnesses/{name}` | [`SetHarnessEnabled`] → `Vec<`[`HarnessChoice`]`>`; standalone without a repo only |
//! | GET / POST | `/notifications` | read or set the desktop notification preference as a JSON boolean |
//! | GET | `/harnesses/config` | → [`HarnessSettings`] |
//! | POST | `/harnesses/preview` | [`HarnessChangeRequest`] → [`HarnessPreview`]; changes nothing |
//! | POST | `/harnesses/apply` | [`HarnessChangeRequest`] → [`HarnessApplied`]; then syncs; never removes placed content; 409 if `machine.toml` changed while it was written |
//! | POST | `/imports/{name}/update` | Optional `?force=true` → [`ImportUpdated`] |
//! | POST | `/imports/{name}/update-reviewed` | `{expected, force}` → [`ImportUpdated`] |
//! | GET | `/imports/{name}/changes` | Optional `?upstream=<commit>` → [`ImportChanges`] |
//! | POST | `/imports/resolve` | [`ResolveImportRequest`] → [`ResolvedSource`] |
//! | POST | `/imports/add` | [`AddImportRequest`] → [`ImportUpdated`] |
//! | POST | `/imports/update` | [`UpdateImportsRequest`] → `Vec<`[`ImportUpdated`]`>` |
//! | POST | `/imports/remove`, `/imports/fork` | [`ImportName`] → 204 |
//! | GET | `/harnesses/{name}/render` | [`InstructionPreview`] list |
//! | GET | `/sync` | [`SyncResult`] plan |
//! | POST | `/sync` | [`SyncRequest`] → [`SyncResult`] |
//! | POST | `/push` | → [`PushResult`] |
//! | POST | `/init` | → 204; standalone without a repo only |
//! | POST | `/join` | [`JoinApiRequest`] → [`Joined`](crate::Joined) |
//! | GET | `/events` | → SSE stream of [`Event`] |
//!
//! `POST /join` is for a machine that was removed (or whose token the server
//! rejects): the daemon joins again and then restarts itself in place with
//! the new credentials, so the socket goes away for a moment.
//!
//! The socket's directory must be ours and mode 0700, and connections from
//! other users are refused.
//!
//! Server pass-throughs (the daemon adds its machine token; 503 while the server
//! is unreachable): `GET /fleet`, `GET|PUT /machines/{id}/assignment`,
//! `DELETE /machines/{id}`, `GET /log`, `GET /log/facets`, `POST /log/{id}/undo`,
//! `GET /imports`, `POST /join-codes`. Bodies are the server's (`sk1ll-proto`).
//!
//! Every request carries [`API_HEADER`]; a daemon of another [`API_VERSION`]
//! answers 426 and says to restart it.
//!
//! Errors are [`ErrorBody`] with a 4xx/5xx status.

use std::path::{Path, PathBuf};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub use sk1ll_core::content::{
    ContentEntry, ContentQuery, ContentType, FileContent, FileEntry, FileQuery, WriteFileRequest,
};
pub use sk1ll_core::{HarnessChange, HarnessChoice};
pub use sk1ll_proto::{
    Assignment, AssignmentChanges, AssignmentCopyPreview, AssignmentSnapshot, AssignmentUpdate, ErrorBody, FleetView,
    ImportSkipped, ImportUpdate, ItemRef, JoinCode, JoinCodeRequest, LogEntry, LogFacets, LogQuery,
    MachineAssignmentCopy, MachineReport, MachineStatus, OrphansPruned, OrphansView, PruneOrphans, Scope,
};
use tokio::net::UnixStream;

/// The version of this local API. Bump it on any change an older daemon or
/// client would misread; additive fields with serde defaults don't need one.
pub const API_VERSION: u32 = 2;

/// Request header carrying the client's [`API_VERSION`]. A daemon refuses a
/// request that names another version (426); one without it is accepted.
pub const API_HEADER: &str = "sk1ll-api";

/// The server's error, its response text, or the HTTP status.
fn failure_message(status: StatusCode, bytes: &[u8]) -> String {
    let message = serde_json::from_slice::<ErrorBody>(bytes)
        .map(|e| e.error)
        .unwrap_or_else(|_| String::from_utf8_lossy(bytes).trim().to_string());
    if message.is_empty() { status.to_string() } else { message }
}

// ---------------------------------------------------------------------------
// Types

/// The daemon's connection to the server, or standalone operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Connection {
    Standalone,
    Connecting,
    Connected,
    /// The server is unreachable; the machine keeps working from its cache.
    Offline,
    /// The server (or something in front of it) answers 401 to the machine's
    /// token. Like Offline, the machine keeps working and keeps retrying;
    /// only the server's explicit `revoked` message removes it.
    AuthRejected,
    /// The machine was removed from the fleet; the daemon no longer manages it.
    Revoked,
}

/// Whether this machine manages its own repo or belongs to a fleet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonMode {
    Joined,
    Standalone,
}

/// `GET /status`: this machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusView {
    pub machine_id: String,
    pub server_url: Option<String>,
    pub mode: DaemonMode,
    pub has_origin: bool,
    pub repo_ready: bool,
    pub repo: PathBuf,
    /// Computed locally; standalone never becomes Offline.
    pub headline: MachineStatus,
    pub connection: Connection,
    pub last_error: Option<String>,
    pub revoked: bool,
    /// The server's baseline as last announced (nudge or our own push).
    pub server_baseline: Option<String>,
    /// Revision of the applied assignment snapshot.
    pub assignment_revision: u64,
    /// No assignment snapshot is stored (lost or never received): harnesses
    /// are left alone until the server sends one.
    #[serde(default)]
    pub awaiting_assignment: bool,
    /// Since when (Unix seconds) the server has been rejecting the token.
    #[serde(default)]
    pub auth_rejected_since: Option<i64>,
    /// What the daemon reports to the server.
    pub report: MachineReport,
    /// The daemon's build ([`sk1ll_core::VERSION`]).
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

/// Read-only inventory (`GET /items`). Baseline membership comes from the
/// applied origin/main commit, never from the working tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemView {
    pub item: ItemRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<sk1ll_core::ItemId>,
    pub in_baseline: bool,
    pub draft: Option<ChangeKind>,
    pub unpushed: bool,
    /// A cheap, single-line SKILL.md frontmatter description, when available.
    pub description: Option<String>,
}

/// One draft item (`GET /draft`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftEntry {
    pub item: ItemRef,
    pub change: ChangeKind,
    /// Repo-relative paths.
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffView {
    pub item: ItemRef,
    pub diff: String,
}

/// `GET /commit/message`: the message a commit of `items` gets by default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitMessage {
    pub items: Vec<ItemRef>,
    pub message: String,
}

/// Items are `kind:name` or a bare skill/rule name that is in the draft.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitRequest {
    pub items: Vec<String>,
    /// Default: `commit <items> from <machine>`.
    #[serde(default)]
    pub message: Option<String>,
    /// `"Name <email>"`. Default: git's identity, else the machine id.
    #[serde(default)]
    pub author: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitResponse {
    pub sha: String,
    pub items: Vec<ItemRef>,
    pub message: String,
    /// Pushed to the server; otherwise queued until the daemon reconnects.
    pub pushed: bool,
    pub push_error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreRequest {
    #[serde(default)]
    pub items: Vec<String>,
    /// Restore the whole draft (`items` must be empty).
    #[serde(default)]
    pub all: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreResponse {
    /// Items that left the draft (reported to the server as `draft_discarded`).
    pub discarded: Vec<ItemRef>,
}

/// `POST /pull`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PullResult {
    UpToDate,
    /// `from` is `None` when the machine had no baseline yet.
    Updated {
        from: Option<String>,
        to: String,
    },
    /// The new baseline clashes with `items`; the machine stays on its old one.
    Blocked {
        baseline: String,
        items: Vec<ItemRef>,
    },
}

/// `GET /conflicts`: harness content sk1ll won't overwrite on its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<ItemRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub displaced_at: Option<u64>,
    /// Harness conflicts: the newest modification time (Unix seconds) of the
    /// Harness content, i.e. when it was last edited, not a first-seen time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detected_at: Option<u64>,
    pub harness: String,
    /// What to pass to adopt/overwrite: a skill name, or a rule's rule name.
    pub name: String,
    pub items: Vec<ItemRef>,
    pub dest: PathBuf,
    pub reason: Option<String>,
    /// Only copied skills and rule files can be adopted, not `block` instructions.
    pub adoptable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictTarget {
    pub harness: String,
    pub name: String,
}

/// `GET /harnesses`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessView {
    pub name: String,
    pub harness: sk1ll_core::Harness,
    /// `detected`, `override` or `local`.
    pub origin: String,
}

/// `POST /setup/harnesses/{name}` body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetHarnessEnabled {
    pub enabled: bool,
}

/// `GET /harnesses/config`: every harness this machine can sync into, and
/// the `machine.toml` they come from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessSettings {
    /// The daemon's `machine.toml` (it may not exist yet).
    pub path: PathBuf,
    pub harnesses: Vec<HarnessChoice>,
}

/// `POST /harnesses/preview`, `POST /harnesses/apply`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessChangeRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    pub change: HarnessChange,
}

/// One thing sync placed, or would place, in a harness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementView {
    pub harness: String,
    /// `create`, `update`, `remove` or `conflict` (left alone). In
    /// [`HarnessPreview::placed`], `remove` is managed content and
    /// `conflict` a copy edited by hand.
    pub kind: String,
    /// `skill`, `rule` or `instructions` (a block).
    pub item: String,
    /// A skill name, a rule file, or the rules a block holds.
    pub name: String,
    pub dest: PathBuf,
    pub reason: Option<String>,
}

/// `POST /harnesses/preview`: what a harness change would do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessPreview {
    pub fingerprint: String,
    pub path: PathBuf,
    pub name: String,
    /// The harness as synced into now (`None`: it isn't).
    pub before: Option<sk1ll_core::Harness>,
    /// The harness once the change is made (`None`: no longer synced into).
    pub after: Option<sk1ll_core::Harness>,
    /// Content sk1ll placed where the change stops syncing. The change
    /// leaves it in place, unmanaged; to take it out, turn the harness's
    /// items off in its assignment first and let sync remove them.
    pub placed: Vec<PlacementView>,
    #[serde(default)]
    pub warnings: Vec<String>,
    /// What the sync after the change does in this harness.
    pub sync: Vec<PlacementView>,
    /// No assignment yet (a joined machine waiting for the server's), so
    /// `sync` is empty and the harness is left alone until it arrives.
    pub awaiting_assignment: bool,
}

/// `POST /harnesses/apply`: `machine.toml` was changed and the harnesses synced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessApplied {
    pub harnesses: Vec<HarnessChoice>,
    /// What the sync after the change failed to place; `machine.toml` was
    /// changed either way.
    pub errors: Vec<String>,
}

/// `POST /imports/{name}/update`: the import was re-vendored into the draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportUpdated {
    pub name: String,
    pub old_commit: Option<String>,
    pub new_commit: String,
    pub changed: bool,
}

/// `POST /imports/resolve`: what a pasted link leads to. Fetches it into the
/// import cache; the repo is not changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolveImportRequest {
    /// A GitHub repo, folder (`/tree/<ref>/<path>`) or file (`/blob/...`)
    /// link, `github:owner/repo`, or any git URL or local path.
    pub link: String,
    /// Overrides the link's path when non-empty (`""` keeps the link's,
    /// since a resolve lists every skill under it anyway).
    #[serde(default)]
    pub path: String,
    #[serde(rename = "ref", default, skip_serializing_if = "String::is_empty")]
    pub git_ref: String,
}

pub use sk1ll_core::imports::ImportChanges;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ImportChangesQuery {
    pub upstream: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedSource {
    /// What an import from it records: `github:owner/repo`, or the URL/path.
    pub source: String,
    /// The link's ref, `HEAD` if it names none.
    pub git_ref: String,
    pub commit: String,
    /// One entry when the link points at a skill directory; otherwise every
    /// directory with a `SKILL.md` under it, sorted by path.
    pub candidates: Vec<SkillCandidate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillCandidate {
    /// Repo-relative directory; `""` for the repo root. Pass it as
    /// [`AddImportRequest::path`].
    pub path: String,
    pub default_name: String,
    pub description: Option<String>,
    /// An import or personal skill already has `default_name`.
    pub taken: bool,
    #[serde(default)]
    pub total_bytes: u64,
    #[serde(default)]
    pub warnings: Vec<sk1ll_core::imports::FileWarning>,
}

/// `POST /imports/add`. `source` may be any link [`ResolveImportRequest`]
/// takes, and must lead to exactly one skill. A given `path` or `ref`
/// overrides what the link names; a `ref` must equal the link's own ref
/// unless `path` is given too. To add a previewed candidate, send
/// [`ResolvedSource::git_ref`], [`SkillCandidate::path`] and
/// [`ResolvedSource::commit`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddImportRequest {
    /// Empty: the skill's default name.
    pub name: String,
    pub source: String,
    /// Absent: the link's path. `""` is the repo root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Absent (or empty): the link's ref, else `HEAD`.
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// Vendor exactly this commit (it must be in the fetched source), e.g.
    /// the one a resolve previewed; the manifest still records `ref`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

/// `POST /imports/update`: one import (`name`) or all.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateImportsRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// Overwrite local edits to vendored imports.
    #[serde(default)]
    pub force: bool,
}

/// `POST /imports/remove`, `POST /imports/fork`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportName {
    pub name: String,
}

/// `POST /sync`: re-sync the harnesses now.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
    /// Also overwrite hand edits sk1ll would otherwise report as conflicts
    /// (content it doesn't manage is still never replaced).
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// Conflicts left after the sync.
    pub conflicts: Vec<ConflictView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actions: Option<Vec<PlacementView>>,
    /// Placements completed before a reviewed apply stopped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed: Vec<PlacementView>,
    /// The placement whose staged copy no longer matched the reviewed digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed: Option<PlacementView>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

/// One instruction file in a harness's render preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionPreview {
    pub path: PathBuf,
    pub format: sk1ll_core::machine::InstructionsFormat,
    /// Rendered rules, as printed by the CLI.
    pub content: String,
    /// Complete proposed file, including any surrounding user content.
    pub next_content: String,
    pub differs: bool,
    pub diff: String,
}

/// `POST /push`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushResult {
    /// Whether there was anything to push.
    pub pushed: bool,
}

/// `POST /join`: like `sk1ll join`, done by the running daemon.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinApiRequest {
    pub server_url: String,
    /// Empty when `admin_token` is set.
    #[serde(default)]
    pub code: String,
    /// The server's admin token, used once to create a join code for the
    /// first machine of a new fleet. Never stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_token: Option<String>,
    #[serde(default)]
    pub machine_id: Option<String>,
    #[serde(default)]
    pub copy_from: Option<String>,
    /// Absolute path of the clone; default: the daemon's current repo.
    #[serde(default)]
    pub repo: Option<PathBuf>,
}

impl std::fmt::Debug for JoinApiRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinApiRequest")
            .field("server_url", &crate::auth::redact(&self.server_url))
            .field("code", &"<redacted>")
            .field("admin_token", &self.admin_token.as_ref().map(|_| "<redacted>"))
            .field("machine_id", &self.machine_id)
            .field("copy_from", &self.copy_from)
            .field("repo", &self.repo)
            .finish()
    }
}

/// One `GET /events` notification; the SSE event name is its `type`.
/// Notifications carry no state: re-fetch what changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// `GET /status` changed. Also invalidates `/items` when a pull or
    /// commit changes the applied Baseline or unpushed commits.
    Status,
    /// The draft's item list changed.
    Draft,
    /// A new assignment was applied.
    Assignment { revision: u64 },
    /// The server's set of imports with upstream updates changed.
    Imports { updates: Vec<ImportUpdate> },
    /// The fleet view may have changed (connection, baseline, removals).
    Fleet,
}

impl Event {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Draft => "draft",
            Self::Assignment { .. } => "assignment",
            Self::Imports { .. } => "imports",
            Self::Fleet => "fleet",
        }
    }
}

// ---------------------------------------------------------------------------
// Client

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportsView {
    pub checked_at: Option<i64>,
    pub updates: Vec<ImportUpdate>,
    pub errors: Vec<sk1ll_proto::ImportError>,
    /// Imports the server's settings kept it from checking, with the reason.
    #[serde(default)]
    pub skipped: Vec<ImportSkipped>,
    #[serde(default)]
    pub imports: Vec<ImportEntry>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportEntry {
    pub name: String,
    pub source: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub path: String,
    pub locked: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateItemRequest {
    pub kind: String,
    pub name: String,
    pub description: Option<String>,
    /// A skill shipped with sk1ll ([`sk1ll_core::bundled`]) to copy instead
    /// of starting empty; `description` is then ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundled: Option<String>,
}
/// A personal Skill or Rule (`kind` is `skill` or `rule`); Imports use `/imports/remove`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteItemRequest {
    pub kind: String,
    pub name: String,
}
/// Renames an item in the Draft while keeping its Item ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameItemRequest {
    pub kind: String,
    pub name: String,
    pub to: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenameItemResponse {
    pub item: ItemRef,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictDiffRequest {
    pub harness: Option<String>,
    pub name: Option<String>,
    pub id: Option<String>,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ClientError {
    /// HTTP status, `None` if the daemon couldn't be reached.
    pub status: Option<u16>,
    pub message: String,
    /// The socket doesn't exist or nothing listens on it: no daemon runs.
    pub not_running: bool,
}

impl ClientError {
    fn transport(e: impl std::fmt::Display) -> Self {
        Self { status: None, message: e.to_string(), not_running: false }
    }

    /// No HTTP response: the daemon isn't running, or talking to it failed.
    pub fn is_unreachable(&self) -> bool {
        self.status.is_none()
    }

    /// Certainly no daemon is running: its socket is missing or the
    /// connection was refused. Any other failure may hide a running daemon.
    pub fn is_not_running(&self) -> bool {
        self.not_running
    }
}

pub type Result<T> = std::result::Result<T, ClientError>;

/// Talks to a daemon's local API; one connection per request.
#[derive(Debug, Clone)]
pub struct Client {
    socket: PathBuf,
}

impl Client {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self { socket: socket.into() }
    }

    /// A client for the default socket (see [`crate::Config::default_socket`]).
    pub fn default_socket() -> anyhow::Result<Self> {
        Ok(Self::new(crate::Config::default_socket()?))
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    async fn send(&self, method: Method, path: &str, body: Option<Vec<u8>>) -> Result<hyper::Response<Incoming>> {
        let stream = UnixStream::connect(&self.socket).await.map_err(|e| {
            let not_running = matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused);
            let message = format!("cannot reach the daemon at {}: {e}", self.socket.display());
            ClientError { status: None, message, not_running }
        })?;
        let (mut sender, conn) =
            hyper::client::conn::http1::handshake(TokioIo::new(stream)).await.map_err(ClientError::transport)?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header(hyper::header::HOST, "localhost")
            .header(API_HEADER, API_VERSION);
        if body.is_some() {
            req = req.header(hyper::header::CONTENT_TYPE, "application/json");
        }
        let req = req.body(Full::new(Bytes::from(body.unwrap_or_default()))).map_err(ClientError::transport)?;
        sender.send_request(req).await.map_err(ClientError::transport)
    }

    /// Sends a request and returns the response body of a 2xx response.
    async fn raw(&self, method: Method, path: &str, body: Option<Vec<u8>>) -> Result<Bytes> {
        let resp = self.send(method.clone(), path, body).await?;
        let status = resp.status();
        let bytes = resp.into_body().collect().await.map_err(ClientError::transport)?.to_bytes();
        if status.is_success() {
            return Ok(bytes);
        }
        let message = failure_message(status, &bytes);
        Err(ClientError { status: Some(status.as_u16()), message, not_running: false })
    }

    async fn call<T: DeserializeOwned>(&self, method: Method, path: &str, body: Option<&impl Serialize>) -> Result<T> {
        let body = body.map(|b| serde_json::to_vec(b).expect("serializing plain data"));
        let bytes = self.raw(method, path, body).await?;
        serde_json::from_slice(&bytes).map_err(|e| ClientError::transport(format!("bad response to {path}: {e}")))
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.call(Method::GET, path, None::<&()>).await
    }

    async fn post<T: DeserializeOwned>(&self, path: &str, body: &impl Serialize) -> Result<T> {
        self.call(Method::POST, path, Some(body)).await
    }

    async fn post_empty(&self, path: &str, body: &impl Serialize) -> Result<()> {
        let body = serde_json::to_vec(body).expect("serializing plain data");
        self.raw(Method::POST, path, Some(body)).await.map(drop)
    }

    pub async fn create_item(&self, request: &CreateItemRequest) -> Result<()> {
        self.post_empty("/items/create", request).await
    }
    pub async fn delete_item(&self, request: &DeleteItemRequest) -> Result<()> {
        self.post_empty("/items/delete", request).await
    }
    pub async fn rename_item(&self, request: &RenameItemRequest) -> Result<RenameItemResponse> {
        self.post("/items/rename", request).await
    }
    pub async fn conflict_diff(&self, request: &ConflictDiffRequest) -> Result<DiffView> {
        self.post("/conflicts/diff", request).await
    }
    pub async fn check_imports(&self) -> Result<ImportsView> {
        self.post("/imports/check", &()).await
    }
    pub async fn init(&self) -> Result<()> {
        self.post_empty("/init", &()).await
    }

    pub async fn status(&self) -> Result<StatusView> {
        self.get("/status").await
    }

    pub async fn items(&self) -> Result<Vec<ItemView>> {
        self.get("/items").await
    }

    pub async fn skills(&self) -> Result<Vec<ContentEntry>> {
        self.get("/skills").await
    }

    pub async fn skill_tree(&self, query: &ContentQuery) -> Result<Vec<FileEntry>> {
        self.get(&format!("/skills/tree?{}", content_query(query))).await
    }

    pub async fn skill_file(&self, query: &FileQuery) -> Result<FileContent> {
        self.get(&format!("/skills/file?{}&path={}", content_query(&query.content), encode(&query.path))).await
    }

    pub async fn write_skill_file(&self, request: &WriteFileRequest) -> Result<()> {
        self.post_empty("/skills/file", request).await
    }

    pub async fn adopt_unmanaged(&self, target: &ConflictTarget) -> Result<()> {
        self.post_empty("/skills/adopt", target).await
    }

    pub async fn draft(&self) -> Result<Vec<DraftEntry>> {
        self.get("/draft").await
    }

    pub async fn diff(&self, item: &str) -> Result<DiffView> {
        self.get(&format!("/draft/diff?item={}", encode(item))).await
    }

    pub async fn commit_message(&self, items: &[String]) -> Result<CommitMessage> {
        self.get(&format!("/commit/message?items={}", encode(&items.join(",")))).await
    }

    pub async fn commit(&self, req: &CommitRequest) -> Result<CommitResponse> {
        self.post("/commit", req).await
    }

    pub async fn restore(&self, req: &RestoreRequest) -> Result<RestoreResponse> {
        self.post("/restore", req).await
    }

    pub async fn pull(&self) -> Result<PullResult> {
        self.post("/pull", &()).await
    }

    pub async fn recovered(&self) -> Result<Vec<ConflictView>> {
        self.get("/conflicts/displaced").await
    }

    pub async fn adopt_displaced(&self, id: &str) -> Result<()> {
        self.post_empty(&format!("/conflicts/displaced/{}/adopt", encode(id)), &()).await
    }

    pub async fn discard_displaced(&self, id: &str) -> Result<()> {
        self.post_empty(&format!("/conflicts/displaced/{}/discard", encode(id)), &()).await
    }

    pub async fn conflicts(&self) -> Result<Vec<ConflictView>> {
        self.get("/conflicts").await
    }

    pub async fn adopt(&self, target: &ConflictTarget) -> Result<()> {
        self.post_empty("/conflicts/adopt", target).await
    }

    pub async fn overwrite(&self, target: &ConflictTarget) -> Result<()> {
        self.post_empty("/conflicts/overwrite", target).await
    }

    pub async fn harnesses(&self) -> Result<Vec<HarnessView>> {
        self.get("/harnesses").await
    }

    pub async fn setup_harnesses(&self) -> Result<Vec<HarnessChoice>> {
        self.get("/setup/harnesses").await
    }

    pub async fn set_setup_harness(&self, name: &str, enabled: bool) -> Result<Vec<HarnessChoice>> {
        self.post(&format!("/setup/harnesses/{}", encode(name)), &SetHarnessEnabled { enabled }).await
    }

    pub async fn notifications_enabled(&self) -> Result<bool> {
        self.get("/notifications").await
    }

    pub async fn set_notifications(&self, enabled: bool) -> Result<bool> {
        self.post("/notifications", &enabled).await
    }

    pub async fn machine_config_path(&self) -> Result<PathBuf> {
        self.get("/harnesses/config/path").await
    }

    pub async fn harness_settings(&self) -> Result<HarnessSettings> {
        self.get("/harnesses/config").await
    }

    pub async fn preview_harness(&self, req: &HarnessChangeRequest) -> Result<HarnessPreview> {
        self.post("/harnesses/preview", req).await
    }

    pub async fn apply_harness(&self, req: &HarnessChangeRequest) -> Result<HarnessApplied> {
        if req.fingerprint.as_deref().is_none_or(str::is_empty) {
            return Err(ClientError::transport("a reviewed preview is required"));
        }
        self.post("/harnesses/apply", req).await
    }

    pub async fn update_import(&self, name: &str) -> Result<ImportUpdated> {
        self.post(&format!("/imports/{}/update", encode(name)), &()).await
    }

    pub async fn force_update_import(&self, name: &str) -> Result<ImportUpdated> {
        self.post(&format!("/imports/{}/update?force=true", encode(name)), &()).await
    }

    pub async fn update_reviewed_import(
        &self,
        name: &str,
        force: bool,
        expected: Option<&str>,
    ) -> Result<ImportUpdated> {
        match expected {
            Some(sha) => {
                self.post(
                    &format!("/imports/{}/update-reviewed", encode(name)),
                    &serde_json::json!({"expected": sha, "force": force}),
                )
                .await
            }
            None if force => self.force_update_import(name).await,
            None => self.update_import(name).await,
        }
    }

    pub async fn import_changes(&self, name: &str, upstream: Option<&str>) -> Result<ImportChanges> {
        let mut path = format!("/imports/{}/changes", encode(name));
        if let Some(commit) = upstream {
            path.push_str(&format!("?upstream={}", encode(commit)));
        }
        self.get(&path).await
    }

    pub async fn resolve_import(&self, req: &ResolveImportRequest) -> Result<ResolvedSource> {
        self.post("/imports/resolve", req).await
    }

    pub async fn add_import(&self, req: &AddImportRequest) -> Result<ImportUpdated> {
        self.post("/imports/add", req).await
    }

    pub async fn update_imports(&self, req: &UpdateImportsRequest) -> Result<Vec<ImportUpdated>> {
        if let Some(expected) = req.expected.as_deref() {
            let name =
                req.name.as_deref().ok_or_else(|| ClientError::transport("a reviewed update needs an import name"))?;
            return self.update_reviewed_import(name, req.force, Some(expected)).await.map(|updated| vec![updated]);
        }
        self.post("/imports/update", req).await
    }

    pub async fn remove_import(&self, name: &str) -> Result<()> {
        self.post_empty("/imports/remove", &ImportName { name: name.into() }).await
    }

    pub async fn fork(&self, name: &str) -> Result<()> {
        self.post_empty("/imports/fork", &ImportName { name: name.into() }).await
    }

    pub async fn render_instructions(&self, name: &str) -> Result<Vec<InstructionPreview>> {
        self.get(&format!("/harnesses/{}/render", encode(name))).await
    }

    // An older daemon ignores unknown POST fields and would apply the sync.
    pub async fn preview_sync(&self) -> Result<SyncResult> {
        self.get("/sync").await
    }

    pub async fn sync(&self, req: &SyncRequest) -> Result<SyncResult> {
        if req.dry_run && !req.force {
            return self.preview_sync().await;
        }
        if req.dry_run {
            return self.get("/sync?force=true").await;
        }
        self.post("/sync", req).await
    }

    /// Returns a fresh preview if the daemon refused the reviewed plan.
    pub async fn apply_sync_preview(&self, fingerprint: String) -> Result<SyncResult> {
        let path = "/sync";
        let req = SyncRequest { fingerprint: Some(fingerprint), ..Default::default() };
        let body = serde_json::to_vec(&req).map_err(ClientError::transport)?;
        let response = self.send(Method::POST, path, Some(body)).await?;
        let status = response.status();
        let bytes = response.into_body().collect().await.map_err(ClientError::transport)?.to_bytes();
        if (status.is_success() || status == StatusCode::CONFLICT)
            && let Ok(result) = serde_json::from_slice(&bytes)
        {
            return Ok(result);
        }
        Err(ClientError { status: Some(status.as_u16()), message: failure_message(status, &bytes), not_running: false })
    }

    pub async fn push(&self) -> Result<PushResult> {
        self.post("/push", &()).await
    }

    /// Joins again through the running daemon, which then restarts itself.
    pub async fn join(&self, req: &JoinApiRequest) -> Result<crate::Joined> {
        self.post("/join", req).await
    }

    pub async fn fleet(&self) -> Result<FleetView> {
        self.get("/fleet").await
    }

    /// The Server's current assignment and revision, read together.
    pub async fn assignment(&self, machine: &str) -> Result<AssignmentSnapshot> {
        self.get(&format!("/machines/{}/assignment", encode(machine))).await
    }

    pub async fn preview_assignment_copy(
        &self,
        machine: &str,
        copy: &MachineAssignmentCopy,
    ) -> Result<AssignmentCopyPreview> {
        self.call(Method::POST, &format!("/machines/{}/assignment/copy-from/preview", encode(machine)), Some(copy))
            .await
    }

    pub async fn copy_machine_assignment(
        &self,
        machine: &str,
        copy: &MachineAssignmentCopy,
    ) -> Result<AssignmentUpdate> {
        self.call(Method::POST, &format!("/machines/{}/assignment/copy-from", encode(machine)), Some(copy)).await
    }

    pub async fn set_assignment(&self, machine: &str, changes: &AssignmentChanges) -> Result<AssignmentUpdate> {
        self.call(Method::PUT, &format!("/machines/{}/assignment", encode(machine)), Some(changes)).await
    }

    pub async fn set_harness_assignment(&self, harness: &str, enabled: bool) -> Result<AssignmentUpdate> {
        self.post(&format!("/assignment/harness/{}", encode(harness)), &sk1ll_proto::SetHarnessAssignment { enabled })
            .await
    }

    pub async fn remove_machine(&self, machine: &str) -> Result<()> {
        self.raw(Method::DELETE, &format!("/machines/{}", encode(machine)), None).await.map(drop)
    }

    pub async fn log(&self, query: &LogQuery) -> Result<Vec<LogEntry>> {
        let mut params = Vec::new();
        for (key, value) in
            [("machine", &query.machine), ("harness", &query.harness), ("author", &query.author), ("item", &query.item)]
        {
            if let Some(v) = value {
                params.push(format!("{key}={}", encode(v)));
            }
        }
        if let Some(b) = query.before {
            params.push(format!("before={b}"));
        }
        if let Some(l) = query.limit {
            params.push(format!("limit={l}"));
        }
        let q = if params.is_empty() { String::new() } else { format!("?{}", params.join("&")) };
        self.get(&format!("/log{q}")).await
    }

    /// The distinct machines, harnesses and authors in the whole log.
    pub async fn log_facets(&self) -> Result<LogFacets> {
        self.get("/log/facets").await
    }

    pub async fn undo(&self, entry: i64) -> Result<AssignmentUpdate> {
        self.post(&format!("/log/{entry}/undo"), &()).await
    }

    pub async fn imports(&self) -> Result<ImportsView> {
        self.get("/imports").await
    }

    pub async fn join_code(&self, scope: Scope) -> Result<JoinCode> {
        self.post("/join-codes", &JoinCodeRequest { scope }).await
    }

    pub async fn rename_harness(&self, rename: &sk1ll_proto::HarnessRename) -> Result<AssignmentUpdate> {
        self.post("/assignment/rename-harness", rename).await
    }

    pub async fn orphans(&self) -> Result<sk1ll_proto::OrphansView> {
        self.get("/assignment/orphans").await
    }

    pub async fn prune_orphans(&self, request: &sk1ll_proto::PruneOrphans) -> Result<sk1ll_proto::OrphansPruned> {
        self.post("/assignment/orphans/prune", request).await
    }

    /// Subscribes to `GET /events`.
    pub async fn events(&self) -> Result<EventStream> {
        let resp = self.send(Method::GET, "/events", None).await?;
        if resp.status() != StatusCode::OK {
            let status = Some(resp.status().as_u16());
            return Err(ClientError { status, message: "cannot subscribe".into(), not_running: false });
        }
        Ok(EventStream { body: resp.into_body(), buf: String::new() })
    }
}

/// Server-sent events from `GET /events`.
pub struct EventStream {
    body: Incoming,
    buf: String,
}

impl EventStream {
    /// The next event; `None` when the daemon closed the stream.
    pub async fn next(&mut self) -> Option<Result<Event>> {
        loop {
            if let Some(end) = self.buf.find("\n\n") {
                let block: String = self.buf.drain(..end + 2).collect();
                let data: Vec<&str> = block.lines().filter_map(|l| l.strip_prefix("data:")).map(str::trim).collect();
                if data.is_empty() {
                    continue; // keep-alive comment
                }
                return Some(serde_json::from_str(&data.join("\n")).map_err(ClientError::transport));
            }
            match self.body.frame().await? {
                Ok(frame) => {
                    if let Some(data) = frame.data_ref() {
                        self.buf.push_str(&String::from_utf8_lossy(data).replace("\r\n", "\n"));
                    }
                }
                Err(e) => return Some(Err(ClientError::transport(e))),
            }
        }
    }
}

/// Percent-encodes a path segment or query value.
fn content_query(q: &ContentQuery) -> String {
    let mut query = format!("item={}", encode(&q.item));
    if let Some(hash) = &q.hash {
        query.push_str(&format!("&hash={}", encode(hash)));
    }
    if let Some(harness) = &q.harness {
        query.push_str(&format!("&harness={}", encode(harness)));
    }
    query
}

pub(crate) fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_query_values() {
        assert_eq!(encode("skill:pdf"), "skill%3Apdf");
        assert_eq!(encode("other:a b/c"), "other%3Aa%20b%2Fc");
        assert_eq!(encode("10-git.md"), "10-git.md");
    }

    #[test]
    fn events_are_tagged() {
        let e = Event::Assignment { revision: 3 };
        assert_eq!(serde_json::to_value(&e).unwrap(), serde_json::json!({"type": "assignment", "revision": 3}));
        assert_eq!(e.name(), "assignment");
        let p = PullResult::Blocked { baseline: "b".into(), items: vec![ItemRef::Skill("x".into())] };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["state"], "blocked");
        assert_eq!(serde_json::from_value::<PullResult>(v).unwrap(), p);
    }

    /// The GUI's contract: serde's default field names.
    #[test]
    fn resolved_sources_use_plain_field_names() {
        let req: ResolveImportRequest = serde_json::from_value(serde_json::json!({"link": "github:o/r"})).unwrap();
        assert_eq!(req.path, "");
        let r = ResolvedSource {
            source: "github:o/r".into(),
            git_ref: "main".into(),
            commit: "c".into(),
            candidates: vec![SkillCandidate {
                path: "".into(),
                default_name: "r".into(),
                description: None,
                taken: true,
                total_bytes: 0,
                warnings: vec![],
            }],
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"source": "github:o/r", "git_ref": "main", "commit": "c", "candidates": [
                {"path": "", "default_name": "r", "description": null, "taken": true, "total_bytes": 0, "warnings": []}]})
        );
        assert_eq!(serde_json::from_value::<ResolvedSource>(v).unwrap(), r);
    }

    #[test]
    fn response_errors_keep_the_message_or_status() {
        assert_eq!(failure_message(StatusCode::NOT_FOUND, b""), "404 Not Found");
        assert_eq!(failure_message(StatusCode::NOT_FOUND, br#"{"error":"no harness `x`"}"#), "no harness `x`");
    }

    /// The GUI's contract for harness changes.
    #[test]
    fn harness_changes_are_tagged_by_op() {
        let json = serde_json::json!({"change": {"op": "set", "name": "agents", "harness": {
            "skills": "~/.agents/skills", "instructions": null, "instructions_format": "block", "mode": "copy"}}});
        let req: HarnessChangeRequest = serde_json::from_value(json).unwrap();
        assert!(matches!(&req.change, HarnessChange::Set { name, harness } if name == "agents"
            && harness.mode == sk1ll_core::LinkMode::Copy));
        let disable =
            HarnessChangeRequest { fingerprint: None, change: HarnessChange::Disable { name: "codex".into() } };
        assert_eq!(
            serde_json::to_value(&disable).unwrap(),
            serde_json::json!({"change": {"op": "disable", "name": "codex"}})
        );
    }

    /// Absent and empty are different for `path`: `""` is the repo root.
    #[test]
    fn add_requests_tell_an_absent_path_from_the_root() {
        let json = serde_json::json!({"name": "", "source": "s", "path": "", "ref": "main", "commit": "abcd"});
        let req: AddImportRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.path.as_deref(), Some(""));
        assert_eq!((req.git_ref.as_deref(), req.commit.as_deref()), (Some("main"), Some("abcd")));
        let req: AddImportRequest = serde_json::from_value(serde_json::json!({"name": "", "source": "s"})).unwrap();
        assert_eq!((req.path, req.git_ref, req.commit), (None, None, None));
        let none = AddImportRequest { name: "n".into(), source: "s".into(), path: None, git_ref: None, commit: None };
        assert_eq!(serde_json::to_value(&none).unwrap(), serde_json::json!({"name": "n", "source": "s"}));
    }
}
