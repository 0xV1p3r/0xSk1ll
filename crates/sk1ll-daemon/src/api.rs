//! The local API: HTTP + JSON on a Unix domain socket (mode 0600) in a
//! directory that must be ours and mode 0700; connections from other users
//! are dropped. See [`crate::client`] for the endpoint table and types.

use std::convert::Infallible;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, bail};
use axum::body::Bytes;
use axum::extract::Request;
use axum::extract::{OriginalUri, Path as UrlPath, Query, State};
use axum::http::{Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{self, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast::error::RecvError;

use crate::client::{
    API_HEADER, API_VERSION, AddImportRequest, CommitMessage, CommitRequest, CommitResponse, ConflictTarget,
    ConflictView, DiffView, DraftEntry, Event, HarnessApplied, HarnessChangeRequest, HarnessChoice, HarnessPreview,
    HarnessSettings, HarnessView, ImportName, ImportUpdated, InstructionPreview, ItemView, JoinApiRequest, PullResult,
    PushResult, ResolveImportRequest, ResolvedSource, RestoreRequest, RestoreResponse, SetHarnessEnabled, StatusView,
    SyncRequest, SyncResult, UpdateImportsRequest,
};
use crate::daemon::Daemon;
use crate::error::{Error, Result};
use crate::join::{self, JoinOptions, Joined};
use crate::server;

type St = State<Arc<Daemon>>;

pub(crate) fn router(d: Arc<Daemon>) -> Router {
    Router::new()
        .route("/status", get(status))
        .route("/init", post(init))
        .route("/items", get(items))
        .route("/skills", get(skills))
        .route("/skills/tree", get(skill_tree))
        .route(
            "/skills/file",
            get(skill_file).post(write_skill_file).layer(axum::extract::DefaultBodyLimit::max(7 * 1024 * 1024)),
        )
        .route("/skills/adopt", post(adopt))
        .route("/draft", get(draft))
        .route("/draft/diff", get(diff))
        .route("/commit", post(commit))
        .route("/commit/message", get(commit_message))
        .route("/restore", post(restore))
        .route("/pull", post(pull))
        .route("/conflicts", get(conflicts))
        .route("/conflicts/displaced", get(recovered))
        .route("/conflicts/displaced/{id}/adopt", post(adopt_displaced))
        .route("/conflicts/displaced/{id}/discard", post(discard_displaced))
        .route("/conflicts/adopt", post(adopt))
        .route("/conflicts/overwrite", post(overwrite))
        .route("/harnesses", get(harnesses))
        .route("/notifications", get(notifications_enabled).post(set_notifications))
        .route("/harnesses/config", get(harness_settings))
        .route("/harnesses/config/path", get(machine_config_path))
        .route("/harnesses/preview", post(preview_harness))
        .route("/harnesses/apply", post(apply_harness))
        .route("/setup/harnesses", get(setup_harnesses))
        .route("/setup/harnesses/{name}", post(set_setup_harness))
        .route("/imports", get(imports))
        .route("/imports/check", post(check_imports))
        .route("/items/create", post(create_item))
        .route("/items/repair-ids", post(repair_ids))
        .route("/items/delete", post(delete_item))
        .route("/items/rename", post(rename_item))
        .route("/conflicts/diff", post(conflict_diff))
        .route("/imports/{name}/update", post(update_import))
        .route("/imports/{name}/update-reviewed", post(update_reviewed_import))
        .route("/imports/{name}/changes", get(import_changes))
        .route("/imports/resolve", post(resolve_import))
        .route("/imports/add", post(add_import))
        .route("/imports/update", post(update_imports))
        .route("/imports/remove", post(remove_import))
        .route("/imports/fork", post(fork))
        .route("/harnesses/{name}/render", get(render_instructions))
        .route("/sync", get(preview_sync).post(sync))
        .route("/push", post(push))
        .route("/join", post(join))
        .route("/fleet", get(pass))
        .route("/machines/{id}", delete(pass))
        .route("/machines/{id}/assignment", get(assignment).put(assignment))
        .route("/assignment/harness/{harness}", post(set_harness_assignment))
        .route("/assignment/rename-harness", post(rename_harness))
        .route("/assignment/orphans", get(pass))
        .route("/assignment/orphans/prune", post(pass))
        .route("/machines/{id}/assignment/copy-from", post(pass))
        .route("/machines/{id}/assignment/copy-from/preview", post(pass))
        .route("/log", get(pass))
        .route("/log/facets", get(pass))
        .route("/log/{id}/undo", post(pass))
        .route("/join-codes", post(pass))
        .route("/events", get(events))
        .layer(middleware::from_fn_with_state(d.clone(), require_repo))
        .layer(middleware::from_fn(check_api_version))
        .with_state(d)
}

/// Refuses a client built for another local API version, typically a newer
/// `sk1ll` or GUI talking to a daemon started before the upgrade.
async fn check_api_version(request: Request, next: Next) -> Response {
    let theirs = request.headers().get(API_HEADER).map(|v| v.to_str().unwrap_or("?").trim().to_string());
    match theirs {
        Some(v) if v != API_VERSION.to_string() => {
            let error = format!(
                "the running daemon (sk1ll {}) speaks local API version {API_VERSION}, this client speaks {v}; \
                 restart the daemon so it runs the installed sk1ll (systemctl --user restart sk1ll-daemon)",
                sk1ll_core::VERSION
            );
            (StatusCode::UPGRADE_REQUIRED, Json(sk1ll_proto::ErrorBody { error })).into_response()
        }
        _ => next.run(request).await,
    }
}

/// Repo-dependent endpoints fail consistently while standalone awaits Init.
async fn require_repo(State(d): St, request: Request, next: Next) -> Result<Response> {
    let path = request.uri().path();
    if !matches!(
        path,
        "/status" | "/events" | "/init" | "/join" | "/harnesses" | "/fleet" | "/log" | "/log/facets" | "/join-codes"
    ) && !path.starts_with("/machines/")
        && !path.starts_with("/setup/")
        && !path.starts_with("/log/")
    {
        d.require_repo()?;
    }
    Ok(next.run(request).await)
}

async fn init(State(d): St) -> Result<StatusCode> {
    d.blocking(|d, eng| d.init(eng)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Binds the socket with mode 0600 in a 0700 directory of ours: an existing
/// directory that isn't is refused, so nobody else can reach (or swap) the
/// socket before its own mode is set. A stale socket is replaced; a live one
/// means another daemon is running.
pub(crate) async fn bind(path: &Path) -> anyhow::Result<PeerChecked> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if !fits_sun_path(path) {
        bail!(
            "the socket path {} is too long for a Unix socket here; set SK1LL_DAEMON_SOCKET to a shorter one",
            path.display()
        );
    }
    let dir = path.parent().context("socket path has no parent")?;
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod {}", dir.display()))?;
    }
    check_private_dir(dir)?;
    if path.exists() {
        if UnixStream::connect(path).await.is_ok() {
            bail!("another daemon is already listening on {}", path.display());
        }
        std::fs::remove_file(path).with_context(|| format!("removing stale {}", path.display()))?;
    }
    let listener = UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod {}", path.display()))?;
    Ok(PeerChecked(listener))
}

/// `sockaddr_un.sun_path` holds the path and its NUL: 108 bytes on Linux, 104 on macOS.
fn fits_sun_path(path: &Path) -> bool {
    // SAFETY: sockaddr_un is plain data, for which all zeroes is valid.
    let addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    path.as_os_str().len() < addr.sun_path.len()
}

/// `dir` is a real directory (not a symlink) owned by us with mode 0700.
fn check_private_dir(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(dir).with_context(|| format!("reading {}", dir.display()))?;
    if !meta.is_dir() {
        bail!("{} must be a directory, not a symlink or file", dir.display());
    }
    if meta.uid() != euid() {
        bail!("refusing to use {}: it is owned by uid {}, not by us ({})", dir.display(), meta.uid(), euid());
    }
    if meta.mode() & 0o777 != 0o700 {
        let mode = meta.mode() & 0o777;
        bail!("refusing to use {}: its mode is {mode:o}, it must be 700 (chmod 700 it)", dir.display());
    }
    Ok(())
}

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// The API listener: drops connections whose peer is another user.
pub(crate) struct PeerChecked(UnixListener);

impl axum::serve::Listener for PeerChecked {
    type Io = UnixStream;
    type Addr = tokio::net::unix::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (stream, addr) = axum::serve::Listener::accept(&mut self.0).await;
            match stream.peer_cred() {
                Ok(cred) if cred.uid() == euid() => return (stream, addr),
                Ok(cred) => tracing::warn!("refused a local API connection from uid {}", cred.uid()),
                Err(e) => tracing::warn!("refused a local API connection: no peer credentials ({e})"),
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.0.local_addr()
    }
}

// Handlers that only read never wait for the engine mutex, so they answer
// while a reconcile, a pull or a push runs: the Draft, the inventory, the
// conflicts and the recovery area come from the published Snapshot (brought
// up to date first when the engine is free, see `Daemon::caught_up`), and
// the rest is read from the repo as it is, through `Daemon::reading`.

async fn status(State(d): St) -> Json<StatusView> {
    Json(d.view())
}

async fn items(State(d): St) -> Result<Json<Vec<ItemView>>> {
    Ok(Json(d.caught_up().await.items()?))
}

async fn draft(State(d): St) -> Result<Json<Vec<DraftEntry>>> {
    Ok(Json(d.caught_up().await.draft()?))
}

async fn skills(State(d): St) -> Result<Json<Vec<sk1ll_core::content::ContentEntry>>> {
    Ok(Json(d.reading(|d| d.skills()).await?))
}

async fn skill_tree(
    State(d): St,
    Query(q): Query<sk1ll_core::content::ContentQuery>,
) -> Result<Json<Vec<sk1ll_core::content::FileEntry>>> {
    Ok(Json(d.reading(move |d| d.skill_tree(&q)).await?))
}

async fn skill_file(
    State(d): St,
    Query(q): Query<sk1ll_core::content::FileQuery>,
) -> Result<Json<sk1ll_core::content::FileContent>> {
    Ok(Json(d.reading(move |d| d.skill_file(&q)).await?))
}

async fn write_skill_file(State(d): St, Json(req): Json<crate::client::WriteFileRequest>) -> Result<StatusCode> {
    d.blocking(move |d, eng| d.write_skill_file(eng, &req)).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ItemQuery {
    item: String,
}

async fn diff(State(d): St, Query(q): Query<ItemQuery>) -> Result<Json<DiffView>> {
    Ok(Json(d.reading(move |d| d.diff(&q.item)).await?))
}

#[derive(Deserialize)]
struct ItemsQuery {
    items: String,
}

async fn commit_message(State(d): St, Query(q): Query<ItemsQuery>) -> Result<Json<CommitMessage>> {
    let items: Vec<String> = q.items.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect();
    Ok(Json(d.reading(move |d| d.commit_message(&items)).await?))
}

async fn commit(State(d): St, Json(req): Json<CommitRequest>) -> Result<Json<CommitResponse>> {
    Ok(Json(d.commit(req).await?))
}

async fn repair_ids(State(d): St) -> Result<Json<usize>> {
    Ok(Json(d.blocking(move |d, eng| d.repair_ids(eng)).await?))
}

async fn restore(State(d): St, Json(req): Json<RestoreRequest>) -> Result<Json<RestoreResponse>> {
    Ok(Json(d.restore(req).await?))
}

async fn pull(State(d): St) -> Result<Json<PullResult>> {
    Ok(Json(d.pull().await?))
}

async fn recovered(State(d): St) -> Result<Json<Vec<ConflictView>>> {
    Ok(Json(d.caught_up().await.recovered()?))
}

async fn conflicts(State(d): St) -> Result<Json<Vec<ConflictView>>> {
    Ok(Json(d.caught_up().await.conflicts()?))
}

async fn adopt_displaced(State(d): St, UrlPath(id): UrlPath<String>) -> Result<Json<PathBuf>> {
    let target = d.blocking(move |d, eng| d.resolve_displaced(eng, &id, true)).await?;
    Ok(Json(target.expect("a successful Adopt has a target")))
}

async fn discard_displaced(State(d): St, UrlPath(id): UrlPath<String>) -> Result<StatusCode> {
    d.blocking(move |d, eng| d.resolve_displaced(eng, &id, false)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn adopt(State(d): St, Json(t): Json<ConflictTarget>) -> Result<StatusCode> {
    d.adopt(t).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn overwrite(State(d): St, Json(t): Json<ConflictTarget>) -> Result<StatusCode> {
    d.blocking(move |d, eng| d.overwrite(eng, t)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn harnesses(State(d): St) -> Result<Json<Vec<HarnessView>>> {
    Ok(Json(d.reading(|d| d.harnesses()).await?))
}

async fn setup_harnesses(State(d): St) -> Result<Json<Vec<HarnessChoice>>> {
    Ok(Json(d.reading(|d| d.setup_harnesses()).await?))
}

async fn set_setup_harness(
    State(d): St,
    UrlPath(name): UrlPath<String>,
    Json(req): Json<SetHarnessEnabled>,
) -> Result<Json<Vec<HarnessChoice>>> {
    Ok(Json(d.blocking(move |d, eng| d.set_setup_harness(eng, &name, req.enabled)).await?))
}

async fn notifications_enabled(State(d): St) -> Result<Json<bool>> {
    Ok(Json(
        d.reading(|d| Ok(sk1ll_core::machine::MachineConfig::load(&d.cfg.machine_config)?.notifications.enabled))
            .await?,
    ))
}

async fn set_notifications(State(d): St, Json(enabled): Json<bool>) -> Result<Json<bool>> {
    d.blocking(move |d, _| Ok(sk1ll_core::machine::MachineConfig::set_notifications(&d.cfg.machine_config, enabled)?))
        .await?;
    Ok(Json(enabled))
}

async fn machine_config_path(State(d): St) -> Result<Json<std::path::PathBuf>> {
    Ok(Json(std::path::absolute(&d.cfg.machine_config).map_err(|error| Error::Internal(error.into()))?))
}

async fn harness_settings(State(d): St) -> Result<Json<HarnessSettings>> {
    Ok(Json(d.reading(|d| d.harness_settings()).await?))
}

async fn preview_harness(State(d): St, Json(req): Json<HarnessChangeRequest>) -> Result<Json<HarnessPreview>> {
    Ok(Json(d.reading(move |d| d.preview_harness(&req.change)).await?))
}

async fn apply_harness(State(d): St, Json(req): Json<HarnessChangeRequest>) -> Result<Json<HarnessApplied>> {
    Ok(Json(d.blocking(move |d, eng| d.apply_harness(eng, req)).await?))
}

#[derive(serde::Deserialize)]
struct ReviewedImportUpdate {
    expected: String,
    force: bool,
}

async fn update_reviewed_import(
    State(d): St,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<ReviewedImportUpdate>,
) -> Result<Json<ImportUpdated>> {
    Ok(Json(d.update_import(name, body.force, Some(body.expected)).await?))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateImportQuery {
    #[serde(default)]
    force: bool,
}

async fn update_import(
    State(d): St,
    UrlPath(name): UrlPath<String>,
    Query(q): Query<UpdateImportQuery>,
) -> Result<Json<ImportUpdated>> {
    Ok(Json(d.update_import(name, q.force, None).await?))
}

async fn import_changes(
    State(d): St,
    UrlPath(name): UrlPath<String>,
    Query(q): Query<crate::client::ImportChangesQuery>,
) -> Result<Json<crate::client::ImportChanges>> {
    Ok(Json(d.import_changes(name, q.upstream).await?))
}

async fn resolve_import(State(d): St, Json(req): Json<ResolveImportRequest>) -> Result<Json<ResolvedSource>> {
    Ok(Json(d.resolve_import(req).await?))
}

async fn add_import(State(d): St, Json(req): Json<AddImportRequest>) -> Result<Json<ImportUpdated>> {
    Ok(Json(d.add_import(req).await?))
}

async fn update_imports(State(d): St, Json(req): Json<UpdateImportsRequest>) -> Result<Json<Vec<ImportUpdated>>> {
    Ok(Json(d.update_imports(req).await?))
}

async fn remove_import(State(d): St, Json(req): Json<ImportName>) -> Result<StatusCode> {
    d.blocking(move |d, eng| d.remove_import(eng, &req.name)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn fork(State(d): St, Json(req): Json<ImportName>) -> Result<StatusCode> {
    d.blocking(move |d, eng| d.fork(eng, &req.name)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn sync(State(d): St, Json(req): Json<SyncRequest>) -> Result<impl IntoResponse> {
    if req.dry_run {
        return Ok((StatusCode::OK, Json(d.reading(move |d| d.preview_sync(req.force)).await?)));
    }
    let result = d.blocking(move |d, eng| d.sync(eng, req.force, req.fingerprint.as_deref())).await?;
    let status = if result.fingerprint.is_some() { StatusCode::CONFLICT } else { StatusCode::OK };
    Ok((status, Json(result)))
}

#[derive(Deserialize)]
struct SyncPreviewQuery {
    #[serde(default)]
    force: bool,
}

async fn preview_sync(State(d): St, Query(query): Query<SyncPreviewQuery>) -> Result<Json<SyncResult>> {
    Ok(Json(d.reading(move |d| d.preview_sync(query.force)).await?))
}

async fn render_instructions(State(d): St, UrlPath(name): UrlPath<String>) -> Result<Json<Vec<InstructionPreview>>> {
    Ok(Json(d.reading(move |d| d.render_instructions(&name)).await?))
}

async fn push(State(d): St) -> Result<Json<PushResult>> {
    Ok(Json(d.push().await?))
}

/// Joins again (after a removal, or while the server rejects the token), then
/// restarts the daemon in place so the new credentials take effect.
async fn join(State(d): St, Json(req): Json<JoinApiRequest>) -> Result<Json<Joined>> {
    let Some(world) = d.can_rejoin() else {
        return Err(Error::Conflict(format!(
            "this machine is in the fleet as `{}`; there is nothing to join",
            d.ident.machine_id()
        )));
    };
    let opts = JoinOptions {
        server_url: req.server_url,
        code: req.code,
        admin_token: req.admin_token,
        machine_id: req.machine_id,
        copy_from: req.copy_from,
        repo: req.repo.unwrap_or_else(|| d.ident.repo().to_path_buf()),
    };
    let joined = if d.is_standalone() {
        let repo = opts.repo.clone();
        d.blocking(move |d, _| d.check_standalone_join_request(&repo)).await?;
        let (reg, pending) =
            join::register_standalone(&d.cfg, &opts).await.map_err(|e| Error::Conflict(format!("{e:#}")))?;
        let joined = d.join_standalone(reg).await.map_err(|e| {
            Error::Conflict(format!("{e}; registration consumed the join code and reserved the machine id. Pending credentials are kept at {}. Fix the local obstruction and repeat the same join request to retry without a new code", pending.display()))
        })?;
        if let Err(e) = std::fs::remove_file(&pending) {
            tracing::warn!("removing pending registration {}: {e}", pending.display());
        }
        Ok(joined)
    } else {
        let replace = !d.is_revoked();
        let reg = join::register(&d.cfg, &opts, replace).await.map_err(|e| Error::BadRequest(format!("{e:#}")))?;
        d.rejoin(reg, world).await?
    };
    let restart = d.clone();
    tokio::spawn(async move {
        // Let the response go out first.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let _ = tokio::task::spawn_blocking(move || restart.restart()).await;
    });
    Ok(Json(joined?))
}

/// Assignment paths extract the decoded machine id, like the fleet API.
async fn assignment(
    State(d): St,
    UrlPath(machine): UrlPath<String>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    body: Bytes,
) -> Result<Response> {
    if !d.is_standalone() && (method == Method::GET || machine != d.ident.machine_id()) {
        return pass(State(d), method, OriginalUri(uri), body).await;
    }
    if machine != d.ident.machine_id() {
        return Err(Error::Conflict("This Machine is standalone; join a Fleet to use this".into()));
    }
    if method == Method::GET {
        // The applied Assignment, published whole: a hand edit of the file
        // is applied (and announced) by the reconcile it wakes.
        let snap = d.applied().expect("standalone always has an assignment");
        return Ok(Json(snap.as_ref()).into_response());
    }
    let changes = serde_json::from_slice(&body).map_err(|e| Error::BadRequest(e.to_string()))?;
    let update = d.write_assignment(sk1ll_proto::AssignmentWrite::Changes(changes)).await?;
    Ok(Json(update).into_response())
}

async fn set_harness_assignment(
    State(d): St,
    UrlPath(harness): UrlPath<String>,
    Json(body): Json<sk1ll_proto::SetHarnessAssignment>,
) -> Result<Json<sk1ll_proto::AssignmentUpdate>> {
    let write = sk1ll_proto::AssignmentWrite::Harness { harness, enabled: body.enabled };
    Ok(Json(d.write_assignment(write).await?))
}

async fn rename_harness(
    State(d): St,
    Json(rename): Json<crate::client::HarnessRenameRequest>,
) -> Result<Json<crate::client::HarnessRenameResult>> {
    Ok(Json(d.rename_harness(rename).await?))
}

/// Server pass-through: same path and query under the server's `/api`.
async fn pass(State(d): St, method: Method, OriginalUri(uri): OriginalUri, body: Bytes) -> Result<Response> {
    d.ident.joined()?;
    let path = uri.path_and_query().map_or(uri.path(), |p| p.as_str());
    server::proxy(&d, method, path, body).await
}

async fn events(State(d): St) -> impl IntoResponse {
    let rx = d.subscribe_events();
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        let event = match rx.recv().await {
            Ok(e) => e,
            // Missed some: a status hint makes the client re-fetch.
            Err(RecvError::Lagged(_)) => Event::Status,
            Err(RecvError::Closed) => return None,
        };
        let sse = sse::Event::default().event(event.name()).json_data(&event).expect("serializing plain data");
        Some((Ok::<_, Infallible>(sse), rx))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn create_item(State(d): St, Json(req): Json<crate::client::CreateItemRequest>) -> Result<StatusCode> {
    d.blocking(move |d, eng| d.create_item(eng, req)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_item(State(d): St, Json(req): Json<crate::client::DeleteItemRequest>) -> Result<StatusCode> {
    d.blocking(move |d, eng| d.delete_item(eng, req)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn rename_item(
    State(d): St,
    Json(req): Json<crate::client::RenameItemRequest>,
) -> Result<Json<crate::client::RenameItemResponse>> {
    Ok(Json(d.blocking(move |d, eng| d.rename_item(eng, req)).await?))
}

async fn conflict_diff(State(d): St, Json(req): Json<crate::client::ConflictDiffRequest>) -> Result<Json<DiffView>> {
    d.caught_up().await;
    Ok(Json(d.reading(move |d| d.conflict_diff(&req)).await?))
}

async fn imports(State(d): St) -> Result<Json<crate::client::ImportsView>> {
    let view = d.local_imports(false).await?;
    Ok(Json(import_checks(&d, view).await))
}

pub(crate) async fn import_checks(d: &Daemon, mut view: crate::client::ImportsView) -> crate::client::ImportsView {
    if !d.is_standalone()
        && let Ok(response) = server::proxy(d, Method::GET, "/imports", Bytes::new()).await
        && response.status().is_success()
        && let Ok(bytes) = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024).await
        && let Ok(remote) = serde_json::from_slice::<sk1ll_proto::ImportsView>(&bytes)
        && remote.checked_at > view.checked_at
    {
        view.checked_at = remote.checked_at;
        view.updates = remote.updates;
        view.errors = remote.errors;
        view.skipped = remote.skipped;
    }
    view.updates.retain(|u| view.imports.iter().any(|i| i.name == u.name && i.locked.as_ref() == Some(&u.locked)));
    view.errors.retain(|e| view.imports.iter().any(|i| i.name == e.name));
    view.skipped.retain(|s| view.imports.iter().any(|i| i.name == s.name));
    view
}

async fn check_imports(State(d): St) -> Result<Json<crate::client::ImportsView>> {
    Ok(Json(d.local_imports(true).await?))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[tokio::test]
    async fn the_socket_dir_must_be_private() {
        let dir = tempfile::tempdir().unwrap();
        let open = dir.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        let err = bind(&open.join("daemon.sock")).await.err().expect("a 0755 directory is refused");
        assert!(err.to_string().contains("mode is 755"), "{err}");
        assert!(!open.join("daemon.sock").exists());

        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&open, &link).unwrap();
        assert!(bind(&link.join("daemon.sock")).await.is_err(), "a symlinked directory is refused");

        // A fresh directory is created 0700, and the socket 0600.
        let fresh = dir.path().join("a/b");
        let sock = fresh.join("daemon.sock");
        let _listener = bind(&sock).await.unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&fresh), 0o700);
        assert_eq!(mode(&sock), 0o600);
        // A second daemon finds it live.
        assert!(bind(&sock).await.is_err());
    }

    #[tokio::test]
    async fn a_socket_path_too_long_for_sun_path_is_refused_before_creating_its_dir() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("d".repeat(120));
        let err = bind(&deep.join("daemon.sock")).await.err().expect("a 120-byte directory name is too long");
        assert!(err.to_string().contains("SK1LL_DAEMON_SOCKET"), "{err}");
        assert!(!deep.exists());
        assert!(fits_sun_path(Path::new("/Users/someone/Library/Application Support/sk1ll/run/daemon.sock")));
    }

    #[tokio::test]
    async fn our_own_connections_are_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("run/daemon.sock");
        let mut listener = bind(&sock).await.unwrap();
        let client = tokio::spawn({
            let sock = sock.clone();
            async move { UnixStream::connect(&sock).await.unwrap() }
        });
        let (stream, _) =
            tokio::time::timeout(std::time::Duration::from_secs(5), axum::serve::Listener::accept(&mut listener))
                .await
                .unwrap();
        assert_eq!(stream.peer_cred().unwrap().uid(), euid());
        client.await.unwrap();
    }
}
