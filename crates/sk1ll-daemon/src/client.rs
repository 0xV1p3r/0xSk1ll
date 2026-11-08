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
//! | POST | `/skills/adopt` | `ConflictTarget` → 204; unmanaged Adopt needs a Hub connection |
//! | GET | `/draft` | → `Vec<`[`DraftEntry`]`>` |
//! | GET | `/draft/diff?item=` | → [`DiffView`] |
//! | GET | `/promote/message?items=a,b` | → [`PromoteMessage`] |
//! | POST | `/promote` | [`PromoteRequest`] → [`PromoteResponse`] |
//! | POST | `/reset` | [`ResetRequest`] → [`ResetResponse`] |
//! | POST | `/pull` | → [`PullResult`] |
//! | GET | `/conflicts` | → `Vec<`[`ConflictView`]`>` |
//! | GET | `/conflicts/displaced` | → all recovered entries |
//! | POST | `/conflicts/displaced/{id}/adopt`, `/conflicts/displaced/{id}/discard` | → 204 |
//! | POST | `/conflicts/adopt`, `/conflicts/overwrite` | [`ConflictTarget`] → 204 |
//! | GET | `/harnesses` | → `Vec<`[`HarnessView`]`>` |
//! | POST | `/imports/{name}/update` | → [`ImportUpdated`] |
//! | POST | `/imports/resolve` | [`ResolveImportRequest`] → [`ResolvedSource`] |
//! | POST | `/imports/add` | [`AddImportRequest`] → [`ImportUpdated`] |
//! | POST | `/imports/update` | [`UpdateImportsRequest`] → `Vec<`[`ImportUpdated`]`>` |
//! | POST | `/imports/remove`, `/imports/fork` | [`ImportName`] → 204 |
//! | POST | `/sync` | [`SyncRequest`] → [`SyncResult`] |
//! | POST | `/push` | → [`PushResult`] |
//! | POST | `/join` | [`JoinApiRequest`] → [`Joined`](crate::Joined) |
//! | GET | `/events` | → SSE stream of [`Event`] |
//!
//! `POST /join` is for a machine that was removed (or whose token the hub
//! rejects): the daemon joins again and then restarts itself in place with
//! the new credentials, so the socket goes away for a moment.
//!
//! The socket's directory must be ours and mode 0700, and connections from
//! other users are refused.
//!
//! Hub pass-throughs (the daemon adds its machine token; 503 while the hub
//! is unreachable): `GET /fleet`, `GET|PUT /machines/{id}/assignment`,
//! `DELETE /machines/{id}`, `GET /log`, `POST /log/{id}/undo`,
//! `GET /imports`, `POST /join-codes`. Bodies are the hub's (`sk1ll-proto`).
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

pub use sk1ll_core::content::{ContentEntry, ContentQuery, ContentType, FileContent, FileEntry, FileQuery};
pub use sk1ll_proto::{
    Assignment, AssignmentChanges, AssignmentSnapshot, AssignmentUpdate, ErrorBody, FleetView, ImportUpdate, ImportsView, ItemRef,
    JoinCode, LogEntry, LogQuery, MachineReport, MachineStatus,
};
use tokio::net::UnixStream;

// ---------------------------------------------------------------------------
// Types

/// The daemon's connection to the hub.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Connection {
    Connecting,
    Connected,
    /// The hub is unreachable; the machine keeps working from its cache.
    Offline,
    /// The hub (or something in front of it) answers 401 to the machine's
    /// token. Like Offline, the machine keeps working and keeps retrying;
    /// only the hub's explicit `revoked` message removes it.
    AuthRejected,
    /// The machine was removed from the fleet; the daemon no longer manages it.
    Revoked,
}

/// `GET /status`: this machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusView {
    pub machine_id: String,
    pub hub_url: String,
    pub repo: PathBuf,
    /// Computed locally: Offline while not connected to the hub.
    pub headline: MachineStatus,
    pub connection: Connection,
    pub last_error: Option<String>,
    pub revoked: bool,
    /// The hub's baseline as last announced (nudge or our own push).
    pub hub_baseline: Option<String>,
    /// Revision of the applied assignment snapshot.
    pub assignment_revision: u64,
    /// No assignment snapshot is stored (lost or never received): harnesses
    /// are left alone until the hub sends one.
    #[serde(default)]
    pub awaiting_assignment: bool,
    /// Since when (Unix seconds) the hub has been rejecting the token.
    #[serde(default)]
    pub auth_rejected_since: Option<i64>,
    /// `draft_discarded` messages waiting to be delivered to the hub.
    #[serde(default)]
    pub pending_discards: usize,
    /// What the daemon reports to the hub.
    pub report: MachineReport,
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

/// `GET /promote/message`: the message a promote of `items` gets by default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromoteMessage {
    pub items: Vec<ItemRef>,
    pub message: String,
}

/// Items are `kind:name` or a bare skill/fragment name that is in the draft.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromoteRequest {
    pub items: Vec<String>,
    /// Default: `promote <items> from <machine>`.
    #[serde(default)]
    pub message: Option<String>,
    /// `"Name <email>"`. Default: git's identity, else the machine id.
    #[serde(default)]
    pub author: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromoteResponse {
    pub sha: String,
    pub items: Vec<ItemRef>,
    pub message: String,
    /// Pushed to the hub; otherwise queued until the daemon reconnects.
    pub pushed: bool,
    pub push_error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetRequest {
    #[serde(default)]
    pub items: Vec<String>,
    /// Reset the whole draft (`items` must be empty).
    #[serde(default)]
    pub all: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetResponse {
    /// Items that left the draft (reported to the hub as `draft_discarded`).
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
    pub harness: String,
    /// What to pass to adopt/overwrite: a skill name, or a rule's fragment name.
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
    /// Also overwrite hand edits sk1ll would otherwise report as conflicts
    /// (content it doesn't manage is still never replaced).
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncResult {
    /// Conflicts left after the sync.
    pub conflicts: Vec<ConflictView>,
}

/// `POST /push`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushResult {
    /// Whether there was anything to push.
    pub pushed: bool,
}

/// `POST /join`: like `sk1ll join`, done by the running daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinApiRequest {
    pub hub_url: String,
    pub code: String,
    #[serde(default)]
    pub machine_id: Option<String>,
    #[serde(default)]
    pub copy_from: Option<String>,
    /// Absolute path of the clone; default: the daemon's current repo.
    #[serde(default)]
    pub repo: Option<PathBuf>,
}

/// One `GET /events` notification; the SSE event name is its `type`.
/// Notifications carry no state: re-fetch what changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// `GET /status` changed. Also invalidates `/items` when a pull or
    /// promote changes the applied Baseline or unpushed commits.
    Status,
    /// The draft's item list changed.
    Draft,
    /// A new assignment was applied.
    Assignment { revision: u64 },
    /// The hub's set of imports with upstream updates changed.
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
            let not_running =
                matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused);
            let message = format!("cannot reach the daemon at {}: {e}", self.socket.display());
            ClientError { status: None, message, not_running }
        })?;
        let (mut sender, conn) =
            hyper::client::conn::http1::handshake(TokioIo::new(stream)).await.map_err(ClientError::transport)?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let mut req = Request::builder().method(method).uri(path).header(hyper::header::HOST, "localhost");
        if body.is_some() {
            req = req.header(hyper::header::CONTENT_TYPE, "application/json");
        }
        let req = req.body(Full::new(Bytes::from(body.unwrap_or_default()))).map_err(ClientError::transport)?;
        sender.send_request(req).await.map_err(ClientError::transport)
    }

    /// Sends a request and returns the response body of a 2xx response.
    async fn raw(&self, method: Method, path: &str, body: Option<Vec<u8>>) -> Result<Bytes> {
        let resp = self.send(method, path, body).await?;
        let status = resp.status();
        let bytes = resp.into_body().collect().await.map_err(ClientError::transport)?.to_bytes();
        if status.is_success() {
            return Ok(bytes);
        }
        let message = serde_json::from_slice::<ErrorBody>(&bytes)
            .map(|e| e.error)
            .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).trim().to_string());
        let message = if message.is_empty() { status.to_string() } else { message };
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
        self.get(&format!("/skills/tree?{}", content_query(query)))
            .await
    }

    pub async fn skill_file(&self, query: &FileQuery) -> Result<FileContent> {
        self.get(&format!(
            "/skills/file?{}&path={}",
            content_query(&query.content),
            encode(&query.path)
        ))
        .await
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

    pub async fn promote_message(&self, items: &[String]) -> Result<PromoteMessage> {
        self.get(&format!("/promote/message?items={}", encode(&items.join(",")))).await
    }

    pub async fn promote(&self, req: &PromoteRequest) -> Result<PromoteResponse> {
        self.post("/promote", req).await
    }

    pub async fn reset(&self, req: &ResetRequest) -> Result<ResetResponse> {
        self.post("/reset", req).await
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

    pub async fn update_import(&self, name: &str) -> Result<ImportUpdated> {
        self.post(&format!("/imports/{}/update", encode(name)), &()).await
    }

    pub async fn resolve_import(&self, req: &ResolveImportRequest) -> Result<ResolvedSource> {
        self.post("/imports/resolve", req).await
    }

    pub async fn add_import(&self, req: &AddImportRequest) -> Result<ImportUpdated> {
        self.post("/imports/add", req).await
    }

    pub async fn update_imports(&self, req: &UpdateImportsRequest) -> Result<Vec<ImportUpdated>> {
        self.post("/imports/update", req).await
    }

    pub async fn remove_import(&self, name: &str) -> Result<()> {
        self.post_empty("/imports/remove", &ImportName { name: name.into() }).await
    }

    pub async fn fork(&self, name: &str) -> Result<()> {
        self.post_empty("/imports/fork", &ImportName { name: name.into() }).await
    }

    pub async fn sync(&self, req: &SyncRequest) -> Result<SyncResult> {
        self.post("/sync", req).await
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

    /// The Hub's current assignment and revision, read together.
    pub async fn assignment(&self, machine: &str) -> Result<AssignmentSnapshot> {
        self.get(&format!("/machines/{}/assignment", encode(machine))).await
    }

    pub async fn set_assignment(&self, machine: &str, changes: &AssignmentChanges) -> Result<AssignmentUpdate> {
        self.call(Method::PUT, &format!("/machines/{}/assignment", encode(machine)), Some(changes)).await
    }

    pub async fn remove_machine(&self, machine: &str) -> Result<()> {
        self.raw(Method::DELETE, &format!("/machines/{}", encode(machine)), None).await.map(drop)
    }

    pub async fn log(&self, query: &LogQuery) -> Result<Vec<LogEntry>> {
        let mut params = Vec::new();
        if let Some(m) = &query.machine {
            params.push(format!("machine={}", encode(m)));
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

    pub async fn undo(&self, entry: i64) -> Result<AssignmentUpdate> {
        self.post(&format!("/log/{entry}/undo"), &()).await
    }

    pub async fn imports(&self) -> Result<ImportsView> {
        self.get("/imports").await
    }

    pub async fn join_code(&self) -> Result<JoinCode> {
        self.post("/join-codes", &()).await
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

fn encode(s: &str) -> String {
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
            }],
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"source": "github:o/r", "git_ref": "main", "commit": "c", "candidates": [
                {"path": "", "default_name": "r", "description": null, "taken": true}]})
        );
        assert_eq!(serde_json::from_value::<ResolvedSource>(v).unwrap(), r);
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
