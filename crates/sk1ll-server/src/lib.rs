//! The server: a self-hosted server that hosts the fleet's canonical bare repo
//! (git smart HTTP under `/git/`), holds each machine's assignment and its
//! log, collects status from daemons over WebSockets, and nudges them to pull
//! after every push (ADR 0001, ADR 0002).

mod api;
mod assign;
pub mod config;
mod db;
mod error;
mod git;

pub use git::check_push;
mod imports;
mod ws;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::Context;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use sk1ll_proto::{
    AssignmentSnapshot, ErrorBody, ImportsView, ItemId, ItemRef, PROTOCOL_HEADER, PROTOCOL_VERSION, Scope,
    ServerMessage,
};
use tokio::net::TcpListener;
use tokio::sync::Notify;

pub use config::Config;
use error::{ApiError, Result};

/// Shared server state.
pub struct Server {
    config: Config,
    /// The baseline branch (`refs/heads/<name>`), decided once at startup.
    branch: String,
    /// Only ever locked on a blocking thread, through [`Server::db`].
    db: Arc<Mutex<Connection>>,
    /// Live daemon sessions by machine id; a newer one replaces (and cancels)
    /// an older. Locked briefly, never across an `.await` or the DB lock.
    conns: Mutex<HashMap<String, ws::Handle>>,
    next_conn: AtomicU64,
    /// Last baseline the server read.
    baseline: Mutex<Baseline>,
    /// Held across a whole push: receive-pack and the pass after it.
    push_lock: Arc<tokio::sync::Mutex<()>>,
    /// Reject pushes until restart if a backend may still be writing refs.
    push_degraded: AtomicBool,
    imports: Mutex<ImportsView>,
    imports_due: Notify,
}

/// HEAD as last read, and the last commit whose items were listed. They
/// differ while listing HEAD fails.
#[derive(Default)]
struct Baseline {
    head: Option<String>,
    inventory: Option<Inventory>,
    head_stale: bool,
}

/// A commit and the skills and rules in its tree, with their Item IDs.
#[derive(Clone)]
struct Inventory {
    commit: String,
    items: Arc<Vec<(ItemRef, ItemId)>>,
}

/// Who made an authenticated request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// The bootstrap token (`SK1LL_SERVER_ADMIN_TOKEN`); manages the whole fleet.
    Admin,
    /// A joined machine, with the scope of the join code it joined with and
    /// the hash of the token it authenticated with.
    Machine { id: String, scope: Scope, token_hash: String },
}

impl Caller {
    /// Name recorded as a log entry's author.
    pub fn author(&self) -> &str {
        match self {
            Self::Admin => "admin",
            Self::Machine { id, .. } => id,
        }
    }

    /// The caller as the database has it now, for a write to recheck inside
    /// its transaction: a request authenticated before its machine was
    /// removed (or removed and joined again with another scope) must not act
    /// on the old token or scope. 401 once the token is gone.
    pub fn current(&self, conn: &Connection) -> Result<Self> {
        match self {
            Self::Admin => Ok(Self::Admin),
            Self::Machine { id, token_hash, .. } => match db::machine_for_token(conn, token_hash)? {
                Some((current, scope)) if current == *id => {
                    Ok(Self::Machine { id: current, scope, token_hash: token_hash.clone() })
                }
                _ => Err(ApiError::Unauthorized),
            },
        }
    }

    /// Refuses (403) unless the caller manages the whole fleet. `what` says
    /// what was refused, e.g. "create join codes".
    pub fn require_fleet(&self, what: &str) -> Result<()> {
        match self {
            Self::Machine { id, scope: Scope::SelfOnly, .. } => Err(ApiError::Forbidden(format!(
                "machine `{id}` joined with a self-only join code and may not {what}; \
                 ask a machine with fleet scope or the server's admin"
            ))),
            _ => Ok(()),
        }
    }

    /// Refuses (403) unless the caller may change `machine`: its assignment,
    /// undoing log entries for it, or removing it. Checked before the
    /// machine is looked up, so a refusal doesn't depend on whether it exists.
    pub fn require_machine(&self, machine: &str) -> Result<()> {
        match self {
            Self::Machine { id, scope: Scope::SelfOnly, .. } if id != machine => Err(ApiError::Forbidden(format!(
                "machine `{id}` joined with a self-only join code and may only manage itself, not `{machine}`"
            ))),
            _ => Ok(()),
        }
    }
}

impl Server {
    /// Creates the data dir, bare repo and database as needed.
    pub async fn open(config: Config) -> anyhow::Result<Arc<Self>> {
        let (data_dir, repo, db_path) = (config.data_dir.clone(), config.repo_dir(), config.db_path());
        let push_check = config.push_check.clone();
        let (conn, branch) = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
            git::init_bare(&repo)?;
            git::init_baseline(&repo)?;
            let branch = git::configure_pushes(&repo, push_check.as_deref())?;
            anyhow::Ok((db::open(&db_path)?, branch))
        })
        .await??;
        let baseline = match git::head(&config.repo_dir(), &branch).await? {
            git::Head::Commit(commit) => Baseline { head: Some(commit), ..Baseline::default() },
            git::Head::Unborn => Baseline::default(),
        };
        if config.admin_token.is_none() {
            tracing::warn!("SK1LL_SERVER_ADMIN_TOKEN is not set: only joined machines can create join codes");
        }
        Ok(Arc::new(Self {
            config,
            branch,
            db: Arc::new(Mutex::new(conn)),
            conns: Mutex::default(),
            next_conn: AtomicU64::new(1),
            baseline: Mutex::new(baseline),
            push_lock: Arc::default(),
            push_degraded: AtomicBool::new(false),
            imports: Mutex::default(),
            imports_due: Notify::new(),
        }))
    }

    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .nest("/api", api::routes().layer(middleware::from_fn(check_protocol)))
            .route("/git/{*path}", any(git::http))
            .route("/healthz", get(healthz))
            .layer(middleware::from_fn(announce_protocol))
            .with_state(self.clone())
    }

    /// Runs `f` on the database on a blocking thread, so neither SQLite nor
    /// waiting for the connection ever stalls an async worker.
    async fn db<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || f(&mut lock(&db)))
            .await
            .map_err(|e| ApiError::Internal(anyhow::anyhow!("database task failed: {e}")))?
    }

    /// HEAD as last read; `None` while the repo is empty.
    fn head(&self) -> Option<String> {
        lock(&self.baseline).head.clone()
    }

    /// Reads the baseline branch from the repo, never from the cache.
    async fn read_head(&self) -> anyhow::Result<git::Head> {
        git::head(&self.config.repo_dir(), &self.branch).await
    }

    /// The items of the last baseline listed, with their Item IDs; empty
    /// while the repo is. Lags HEAD while listing it fails, which naming log
    /// entries and materializing defaults can live with.
    fn baseline_items(&self) -> Arc<Vec<(ItemRef, ItemId)>> {
        lock(&self.baseline).inventory.as_ref().map(|i| i.items.clone()).unwrap_or_default()
    }

    /// The items of the baseline as it is now: HEAD is read again, and its
    /// items listed unless they are cached. For finding orphans, which must
    /// not take an item a push just added for gone.
    async fn live_items(&self) -> Result<Arc<Vec<(ItemRef, ItemId)>>> {
        let unavailable = |e: anyhow::Error| {
            tracing::error!("reading the baseline: {e:#}");
            ApiError::Unavailable("the server can't read the baseline right now; try again later".into())
        };
        match self.read_head().await.map_err(unavailable)? {
            git::Head::Commit(commit) => self.inventory(&commit).await.map_err(unavailable),
            git::Head::Unborn => Ok(Arc::default()),
        }
    }

    /// The items at `commit`, from the cache when it holds that commit, else
    /// listed (and cached).
    async fn inventory(&self, commit: &str) -> anyhow::Result<Arc<Vec<(ItemRef, ItemId)>>> {
        if let Some(i) = lock(&self.baseline).inventory.as_ref().filter(|i| i.commit == commit) {
            return Ok(i.items.clone());
        }
        let inventory = git::item_ids_at(&self.config.repo_dir(), commit).await?;
        // The pre-receive hook refuses a baseline without them (see
        // `git::check_push`), so every item has an Item ID of its own.
        if let Some(problems) = inventory.id_problems() {
            anyhow::bail!("baseline {commit} is unusable: {problems}");
        }
        let items: Arc<Vec<_>> = Arc::new(
            inventory.items.into_iter().map(|(item, id)| (item, id.expect("id_problems found none missing"))).collect(),
        );
        let mut baseline = lock(&self.baseline);
        if baseline.head.as_deref() == Some(commit) {
            baseline.inventory = Some(Inventory { commit: commit.to_string(), items: items.clone() });
        }
        Ok(items)
    }

    pub async fn authenticate(&self, token: &str) -> Result<Caller> {
        if token.is_empty() {
            return Err(ApiError::Unauthorized);
        }
        if let Some(admin) = &self.config.admin_token
            && Sha256::digest(admin.as_bytes()) == Sha256::digest(token.as_bytes())
        {
            return Ok(Caller::Admin);
        }
        let token_hash = hash(token);
        let hash = token_hash.clone();
        self.db(move |conn| db::machine_for_token(conn, &hash))
            .await?
            .map(|(id, scope)| Caller::Machine { id, scope, token_hash })
            .ok_or(ApiError::Unauthorized)
    }

    /// Registers a daemon session, cancelling the one it replaces. Only
    /// called under the DB lock, after checking the session's token, so it
    /// is ordered with [`Server::remove_machine`].
    fn connect(&self, machine: &str, handle: ws::Handle) {
        let old = lock(&self.conns).insert(machine.to_string(), handle);
        if let Some(old) = old {
            tracing::info!("{machine}: a newer connection replaces session {}", old.id);
            old.cancel();
        }
    }

    fn disconnect(&self, machine: &str, conn: u64) {
        let mut conns = lock(&self.conns);
        if conns.get(machine).is_some_and(|c| c.id == conn) {
            conns.remove(machine);
        }
    }

    /// Whether session `conn` is still the machine's registered one.
    fn is_current(&self, machine: &str, conn: u64) -> bool {
        lock(&self.conns).get(machine).is_some_and(|c| c.id == conn)
    }

    fn is_connected(&self, machine: &str) -> bool {
        lock(&self.conns).contains_key(machine)
    }

    /// Removes and cancels the machine's session if it authenticated with
    /// `token_hash`, so revoking a removed token never ends the session of a
    /// machine that joined again under the same id. The session is told
    /// `Revoked` if its socket isn't stalled.
    fn revoke(&self, machine: &str, token_hash: &str) {
        let mut conns = lock(&self.conns);
        if conns.get(machine).is_some_and(|h| h.token_hash == token_hash)
            && let Some(h) = conns.remove(machine)
        {
            h.revoke();
        }
    }

    fn broadcast(&self, msg: &ServerMessage) {
        for (machine, c) in lock(&self.conns).iter() {
            c.send(machine, msg.clone());
        }
    }

    /// Hands the machine's session `snapshot` unless it already has a newer one.
    fn publish_assignment(&self, machine: &str, snapshot: AssignmentSnapshot) {
        if let Some(c) = lock(&self.conns).get(machine) {
            c.publish(snapshot);
        }
    }

    /// Removes `machine` (logged) and revokes its session. Holds the push
    /// lock, so a push either rechecks its token after the removal and is
    /// refused, or finishes before the machine goes. The
    /// session is revoked under the DB lock that sessions register under, and
    /// only if it holds the removed token.
    /// The guard moves into the database work, so dropping this future (a
    /// caller that disconnects) can't let a push in before the removal commits.
    async fn remove_machine(self: &Arc<Self>, caller: Caller, machine: String) -> Result<()> {
        let push = self.push_lock.clone().lock_owned().await;
        let (server, author, now) = (self.clone(), caller.author().to_string(), now());
        self.db(move |conn| {
            let _push = push;
            caller.current(conn)?.require_machine(&machine)?;
            let token_hash = db::token_hash(conn, &machine)?;
            assign::remove_machine(conn, &machine, &author, now)?;
            if let Some(token_hash) = token_hash {
                server.revoke(&machine, &token_hash);
            }
            Ok(())
        })
        .await
    }

    /// A push moves HEAD and nudges the machines. Inventory is read on
    /// demand by operations that need it.
    async fn after_push(self: &Arc<Self>) {
        if self.refresh_head().await.is_ok() {
            return;
        }
        {
            let mut baseline = lock(&self.baseline);
            if baseline.head_stale {
                return;
            }
            baseline.head_stale = true;
        }
        let server = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                let Some(server) = server.upgrade() else { break };
                let _push = server.push_lock.lock().await;
                if !lock(&server.baseline).head_stale || server.refresh_head().await.is_ok() {
                    break;
                }
            }
        });
    }

    async fn refresh_head(&self) -> anyhow::Result<()> {
        let new = match self.read_head().await {
            Ok(git::Head::Commit(commit)) => Some(commit),
            Ok(git::Head::Unborn) => None,
            Err(error) => {
                tracing::error!("reading HEAD after push: {error:#}");
                return Err(error);
            }
        };
        {
            let mut baseline = lock(&self.baseline);
            baseline.head_stale = false;
            if baseline.head == new {
                return Ok(());
            }
            baseline.head = new.clone();
        }
        if let Some(baseline) = new {
            tracing::info!("baseline is now {baseline}");
            self.broadcast(&ServerMessage::Nudge { baseline });
        }
        self.imports_due.notify_one();
        Ok(())
    }

    async fn inventory_lock(&self) -> Result<tokio::sync::OwnedMutexGuard<()>> {
        let push = self.push_lock.clone().lock_owned().await;
        if self.push_degraded.load(Ordering::SeqCst) {
            return Err(ApiError::Unavailable("git push state is degraded; restart the Server".into()));
        }
        Ok(push)
    }
}

/// Refuses an `/api` request from a peer that speaks another protocol version
/// (426, with the reason). Requests without the header are let through.
async fn check_protocol(request: Request, next: Next) -> Response {
    let theirs = request.headers().get(PROTOCOL_HEADER).map(|v| v.to_str().unwrap_or("?").trim().to_string());
    match theirs {
        Some(v) if v != PROTOCOL_VERSION.to_string() => {
            let error = sk1ll_proto::protocol_mismatch(&v);
            tracing::warn!("{error}");
            (StatusCode::UPGRADE_REQUIRED, axum::Json(ErrorBody { error })).into_response()
        }
        _ => next.run(request).await,
    }
}

/// Tags every response with the server's protocol and release versions.
async fn announce_protocol(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(PROTOCOL_HEADER, HeaderValue::from(PROTOCOL_VERSION));
    headers.insert(sk1ll_proto::VERSION_HEADER, HeaderValue::from_static(env!("CARGO_PKG_VERSION")));
    response
}

async fn healthz(State(server): State<Arc<Server>>) -> Response {
    let degraded = server.push_degraded.load(Ordering::SeqCst);
    let status = if degraded { StatusCode::SERVICE_UNAVAILABLE } else { StatusCode::OK };
    (
        status,
        axum::Json(serde_json::json!({
            "status": if degraded { "degraded" } else { "ok" },
            "push_degraded": degraded,
            "version": sk1ll_core::VERSION,
            "protocol": PROTOCOL_VERSION,
        })),
    )
        .into_response()
}

/// Binds `config.bind` and serves until Ctrl-C / SIGTERM.
pub async fn run(config: Config) -> anyhow::Result<()> {
    let listener = TcpListener::bind(config.bind).await.with_context(|| format!("binding {}", config.bind))?;
    serve(config, listener).await
}

/// Serves on `listener` (tests pass an ephemeral one).
pub async fn serve(config: Config, listener: TcpListener) -> anyhow::Result<()> {
    let server = Server::open(config).await?;
    tokio::spawn(imports::poll(server.clone()));
    tracing::info!("sk1ll-server {} listening on {}", sk1ll_core::VERSION, listener.local_addr()?);
    axum::serve(listener, server.router()).with_graceful_shutdown(shutdown()).await?;
    Ok(())
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { () = ctrl_c => {}, () = term => {} }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

fn hash(secret: &str) -> String {
    hex(&Sha256::digest(secret.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf).expect("OS random source");
    buf
}

/// Machine ids, harness and item names: 1-128 of `[A-Za-z0-9._-]`, not starting with `.`.
fn check_name(what: &str, s: &str) -> Result<()> {
    let ok = !s.is_empty()
        && s.len() <= 128
        && !s.starts_with('.')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok { Ok(()) } else { Err(ApiError::BadRequest(format!("invalid {what} `{s}`"))) }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// A slow database must not stall other tasks, even on a single worker.
    #[tokio::test(flavor = "current_thread")]
    async fn database_work_runs_off_the_async_workers() {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::open(Config::new(dir.path().join("data"))).await.unwrap();
        let held = server.db.clone();
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _guard = lock(&held);
            locked_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(500));
        });
        locked_rx.recv().unwrap();
        let start = std::time::Instant::now();
        let query = tokio::spawn({
            let server = server.clone();
            async move { server.authenticate("some-token").await.is_err() }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(start.elapsed() < Duration::from_millis(300), "the runtime was blocked for {:?}", start.elapsed());
        assert!(query.await.unwrap());
        holder.join().unwrap();
    }

    #[tokio::test]
    async fn a_stale_head_is_retried_after_push() {
        use sk1ll_core::git::Git;
        let dir = tempfile::tempdir().unwrap();
        let server = Server::open(Config::new(dir.path().join("data"))).await.unwrap();
        let bare = server.config.repo_dir();
        let initial = server.head().unwrap();
        let git = |args: &[&str]| {
            Git::new(&bare)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .args(args)
                .text()
                .unwrap()
        };
        let tree = git(&["rev-parse", "HEAD^{tree}"]);
        let next = git(&["commit-tree", &tree, "-p", &initial, "-m", "update"]);
        let branch = bare.join(&server.branch);
        std::fs::write(&branch, format!("{tree}\n")).unwrap();
        server.after_push().await;
        assert!(lock(&server.baseline).head_stale);
        std::fs::write(&branch, format!("{next}\n")).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while server.head().as_deref() != Some(next.as_str()) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(!lock(&server.baseline).head_stale);
    }

    /// A push that deletes items changes no assignment: their toggles stay,
    /// inert, until someone prunes the orphans.
    #[tokio::test]
    async fn a_push_that_deletes_items_changes_no_assignment() {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::open(Config::new(dir.path().join("data"))).await.unwrap();
        let work = dir.path().join("work");
        let git = |dir: &std::path::Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        git(dir.path(), &["clone", "--quiet", &server.config.repo_dir().to_string_lossy(), &work.to_string_lossy()]);
        let push = |msg: &str| {
            git(&work, &["add", "--all"]);
            git(&work, &["commit", "--quiet", "-m", msg]);
            git(&work, &["push", "--quiet", "origin", "HEAD:main"]);
        };
        std::fs::create_dir_all(work.join("personal/pdf")).unwrap();
        std::fs::write(work.join("personal/pdf/SKILL.md"), "x\n").unwrap();
        std::fs::write(work.join("personal/pdf/.sk1ll-id"), "pdf-id\n").unwrap();
        push("pdf");
        server.after_push().await;
        let pdf: ItemId = "pdf-id".parse().unwrap();
        assert!(server.live_items().await.unwrap().iter().any(|(_, id)| *id == pdf));
        let mut a = sk1ll_proto::Assignment::default();
        a.set_skill("claude", &pdf, true);
        let stored = a.clone();
        let revision =
            server.db(move |conn| db::insert_machine(conn, "m", &hash("m"), 0, &stored, Scope::Fleet)).await.unwrap();

        std::fs::remove_dir_all(work.join("personal/pdf")).unwrap();
        push("without pdf");
        server.after_push().await;
        assert!(server.live_items().await.unwrap().iter().all(|(_, id)| *id != pdf));
        let after = server.db(|conn| db::snapshot(conn, "m")).await.unwrap().unwrap();
        assert_eq!((after.revision, after.assignment), (revision, a));
        let log = server.db(|conn| db::log_entries(conn, &Default::default())).await.unwrap();
        assert!(log.is_empty(), "{log:?}");
    }
}
