//! The local API: HTTP + JSON on a Unix domain socket (mode 0600) in a
//! directory that must be ours and mode 0700; connections from other users
//! are dropped. See [`crate::client`] for the endpoint table and types.

use std::convert::Infallible;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, bail};
use axum::body::Bytes;
use axum::extract::{OriginalUri, Path as UrlPath, Query, State};
use axum::http::{Method, StatusCode};
use axum::response::sse::{self, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast::error::RecvError;

use crate::client::{
    AddImportRequest, ConflictTarget, ConflictView, DiffView, DraftEntry, Event, HarnessView, ImportName,
    ImportUpdated, ItemView, JoinApiRequest, CommitMessage, CommitRequest, CommitResponse, PullResult, PushResult,
    RestoreRequest, RestoreResponse, ResolveImportRequest, ResolvedSource, StatusView, SyncRequest, SyncResult,
    UpdateImportsRequest,
};
use crate::daemon::Daemon;
use crate::error::{Error, Result};
use crate::join::{self, JoinOptions, Joined};
use crate::server;

type St = State<Arc<Daemon>>;

pub(crate) fn router(d: Arc<Daemon>) -> Router {
    Router::new()
        .route("/status", get(status))
        .route("/items", get(items))
        .route("/skills", get(skills))
        .route("/skills/tree", get(skill_tree))
        .route("/skills/file", get(skill_file))
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
        .route("/imports", get(imports))
        .route("/imports/check", post(check_imports))
        .route("/items/create", post(create_item))
        .route("/items/delete", post(delete_item))
        .route("/items/rename", post(rename_item))
        .route("/conflicts/diff", post(conflict_diff))
        .route("/imports/{name}/update", post(update_import))
        .route("/imports/resolve", post(resolve_import))
        .route("/imports/add", post(add_import))
        .route("/imports/update", post(update_imports))
        .route("/imports/remove", post(remove_import))
        .route("/imports/fork", post(fork))
        .route("/sync", post(sync))
        .route("/push", post(push))
        .route("/join", post(join))
        .route("/fleet", get(pass))
        .route("/machines/{id}", delete(pass))
        .route("/machines/{id}/assignment", get(pass).put(pass))
        .route("/log", get(pass))
        .route("/log/facets", get(pass))
        .route("/log/{id}/undo", post(pass))
        .route("/join-codes", post(pass))
        .route("/events", get(events))
        .with_state(d)
}

/// Binds the socket with mode 0600 in a 0700 directory of ours: an existing
/// directory that isn't is refused, so nobody else can reach (or swap) the
/// socket before its own mode is set. A stale socket is replaced; a live one
/// means another daemon is running.
pub(crate) async fn bind(path: &Path) -> anyhow::Result<PeerChecked> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
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

async fn status(State(d): St) -> Json<StatusView> {
    Json(d.view())
}

async fn items(State(d): St) -> Result<Json<Vec<ItemView>>> {
    // No mutable engine guard: inventory only reads git and the working tree.
    let root = d.view().repo;
    let inventory = tokio::task::spawn_blocking(move || Daemon::items(&root))
        .await.map_err(|e| Error::Internal(anyhow::anyhow!("inventory task failed: {e}")))??;
    Ok(Json(inventory))
}

async fn draft(State(d): St) -> Result<Json<Vec<DraftEntry>>> {
    Ok(Json(d.blocking(|d, eng| d.draft(eng)).await?))
}

async fn skills(State(d): St) -> Result<Json<Vec<sk1ll_core::content::ContentEntry>>> {
    Ok(Json(d.blocking(|d, eng| d.skills(eng)).await?))
}

async fn skill_tree(
    State(d): St,
    Query(q): Query<sk1ll_core::content::ContentQuery>,
) -> Result<Json<Vec<sk1ll_core::content::FileEntry>>> {
    Ok(Json(d.blocking(move |d, eng| d.skill_tree(eng, q)).await?))
}

async fn skill_file(
    State(d): St,
    Query(q): Query<sk1ll_core::content::FileQuery>,
) -> Result<Json<sk1ll_core::content::FileContent>> {
    Ok(Json(d.blocking(move |d, eng| d.skill_file(eng, q)).await?))
}

#[derive(Deserialize)]
struct ItemQuery {
    item: String,
}

async fn diff(State(d): St, Query(q): Query<ItemQuery>) -> Result<Json<DiffView>> {
    Ok(Json(d.blocking(move |d, eng| d.diff(eng, &q.item)).await?))
}

#[derive(Deserialize)]
struct ItemsQuery {
    items: String,
}

async fn commit_message(State(d): St, Query(q): Query<ItemsQuery>) -> Result<Json<CommitMessage>> {
    let items: Vec<String> = q.items.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect();
    Ok(Json(d.blocking(move |d, eng| d.commit_message(eng, &items)).await?))
}

async fn commit(State(d): St, Json(req): Json<CommitRequest>) -> Result<Json<CommitResponse>> {
    Ok(Json(d.blocking(move |d, eng| d.commit(eng, req)).await?))
}

async fn restore(State(d): St, Json(req): Json<RestoreRequest>) -> Result<Json<RestoreResponse>> {
    Ok(Json(d.blocking(move |d, eng| d.restore(eng, req)).await?))
}

async fn pull(State(d): St) -> Result<Json<PullResult>> {
    Ok(Json(d.blocking(|d, eng| d.pull(eng)).await?))
}

async fn recovered(State(d): St) -> Result<Json<Vec<ConflictView>>> {
    Ok(Json(d.blocking(|d, _| d.recovered()).await?))
}

async fn conflicts(State(d): St) -> Result<Json<Vec<ConflictView>>> {
    Ok(Json(d.blocking(|d, eng| d.conflicts(eng)).await?))
}

async fn adopt_displaced(State(d): St, UrlPath(id): UrlPath<String>) -> Result<StatusCode> {
    d.blocking(move |d, eng| d.resolve_displaced(eng, &id, true)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn discard_displaced(State(d): St, UrlPath(id): UrlPath<String>) -> Result<StatusCode> {
    d.blocking(move |d, eng| d.resolve_displaced(eng, &id, false)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn adopt(State(d): St, Json(t): Json<ConflictTarget>) -> Result<StatusCode> {
    d.blocking(move |d, eng| d.adopt(eng, t)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn overwrite(State(d): St, Json(t): Json<ConflictTarget>) -> Result<StatusCode> {
    d.blocking(move |d, eng| d.overwrite(eng, t)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn harnesses(State(d): St) -> Result<Json<Vec<HarnessView>>> {
    Ok(Json(d.blocking(|d, _| d.harnesses()).await?))
}

async fn update_import(State(d): St, UrlPath(name): UrlPath<String>) -> Result<Json<ImportUpdated>> {
    Ok(Json(d.update_import(name).await?))
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

async fn sync(State(d): St, Json(req): Json<SyncRequest>) -> Result<Json<SyncResult>> {
    Ok(Json(d.blocking(move |d, eng| d.sync(eng, req.force)).await?))
}

async fn push(State(d): St) -> Result<Json<PushResult>> {
    Ok(Json(d.blocking(|d, eng| d.push(eng)).await?))
}

/// Joins again (after a removal, or while the server rejects the token), then
/// restarts the daemon in place so the new credentials take effect.
async fn join(State(d): St, Json(req): Json<JoinApiRequest>) -> Result<Json<Joined>> {
    if !d.can_rejoin() {
        return Err(Error::Conflict(format!(
            "this machine is in the fleet as `{}`; there is nothing to join",
            d.ident.machine_id
        )));
    }
    let opts = JoinOptions {
        server_url: req.server_url,
        code: req.code,
        machine_id: req.machine_id,
        copy_from: req.copy_from,
        repo: req.repo.unwrap_or_else(|| d.ident.repo.clone()),
    };
    let replace = !d.is_revoked();
    let reg = join::register(&d.cfg, &opts, replace).await.map_err(|e| Error::BadRequest(format!("{e:#}")))?;
    let joined = d.blocking(move |d, eng| d.rejoin(eng, reg)).await?;
    let restart = d.clone();
    tokio::spawn(async move {
        // Let the response go out first.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let _ = tokio::task::spawn_blocking(move || restart.restart()).await;
    });
    Ok(Json(joined?))
}

/// Server pass-through: same path and query under the server's `/api`.
/// Assignment GETs preserve the flattened assignment snapshot, including its revision.
async fn pass(State(d): St, method: Method, OriginalUri(uri): OriginalUri, body: Bytes) -> Result<Response> {
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

async fn create_item(State(d): St, Json(req): Json<crate::client::CreateItemRequest>) -> Result<StatusCode> {
    d.blocking(move |d, eng| d.create_item(eng, req)).await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn delete_item(State(d): St, Json(req): Json<crate::client::DeleteItemRequest>) -> Result<StatusCode> {
    d.blocking(move |d, eng| d.delete_item(eng, req)).await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn rename_item(State(d): St, Json(req): Json<crate::client::RenameItemRequest>) -> Result<Json<crate::client::RenameItemResponse>> {
    Ok(Json(d.blocking(move |d, eng| d.rename_item(eng, req)).await?))
}
async fn conflict_diff(State(d): St, Json(req): Json<crate::client::ConflictDiffRequest>) -> Result<Json<DiffView>> {
    Ok(Json(d.blocking(move |d, eng| d.conflict_diff(eng, req)).await?))
}
async fn imports(State(d): St) -> Result<Json<crate::client::ImportsView>> {
    let mut view = d.local_imports(false).await?;
    if let Ok(response) = server::proxy(&d, Method::GET, "/imports", Bytes::new()).await
        && response.status().is_success()
            && let Ok(bytes) = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024).await
                && let Ok(remote) = serde_json::from_slice::<sk1ll_proto::ImportsView>(&bytes)
                    && remote.checked_at > view.checked_at { view.checked_at = remote.checked_at; view.updates = remote.updates; view.errors = remote.errors; }
    view.updates.retain(|u| view.imports.iter().any(|i| i.name == u.name && i.locked.as_ref() == Some(&u.locked)));
    view.errors.retain(|e| view.imports.iter().any(|i| i.name == e.name));
    Ok(Json(view))
}
async fn check_imports(State(d): St) -> Result<Json<crate::client::ImportsView>> {
    Ok(Json(d.local_imports(true).await?))
}
