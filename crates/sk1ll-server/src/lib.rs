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
mod imports;
mod ws;

use std::collections::{BTreeSet, HashMap};
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
    AssignmentSnapshot, ErrorBody, ImportsView, ItemRef, PROTOCOL_HEADER, PROTOCOL_VERSION, Scope, ServerMessage,
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
    /// Last baseline the server acted on.
    baseline: Mutex<Baseline>,
    /// Held across a whole push: receive-pack and the pass after it.
    push_lock: Arc<tokio::sync::Mutex<()>>,
    /// Reject pushes until restart if a backend may still be writing refs.
    push_degraded: AtomicBool,
    #[cfg(test)]
    fail_cleanup_confirmation: AtomicBool,
    #[cfg(test)]
    fail_listing: AtomicBool,
    #[cfg(test)]
    fail_head: AtomicBool,
    imports: Mutex<ImportsView>,
    imports_due: Notify,
}

/// HEAD as last read, and the last commit whose tree was listed. They differ
/// while listing HEAD fails: a commit's items are only ever stored with that
/// commit, so cleanup never diffs against another commit's items.
#[derive(Default)]
struct Baseline {
    head: Option<String>,
    /// HEAD may have moved since `head` was read: a push's read of it failed,
    /// or a backend may still be writing. Nothing trusts `head` until HEAD
    /// is read again.
    stale: bool,
    inventory: Option<Inventory>,
}

/// A commit and the skills and rules in its tree.
#[derive(Clone)]
struct Inventory {
    commit: String,
    items: Arc<BTreeSet<ItemRef>>,
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
        let (conn, branch) = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
            git::init_bare(&repo)?;
            git::init_baseline(&repo)?;
            let branch = git::configure_pushes(&repo)?;
            anyhow::Ok((db::open(&db_path)?, branch))
        })
        .await??;
        let baseline = match git::head(&config.repo_dir(), &branch).await? {
            git::Head::Commit(commit) => Baseline {
                head: Some(commit.clone()),
                stale: false,
                inventory: Some(Inventory {
                    items: Arc::new(git::items_at(&config.repo_dir(), &commit).await?),
                    commit,
                }),
            },
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
            #[cfg(test)]
            fail_cleanup_confirmation: AtomicBool::new(false),
            #[cfg(test)]
            fail_listing: AtomicBool::new(false),
            #[cfg(test)]
            fail_head: AtomicBool::new(false),
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

    /// HEAD as last read; `None` while the repo is empty or HEAD is stale.
    fn head(&self) -> Option<String> {
        let baseline = lock(&self.baseline);
        if baseline.stale { None } else { baseline.head.clone() }
    }

    /// Marks the cached HEAD stale: it may lag the repo.
    fn forget_head(&self) {
        lock(&self.baseline).stale = true;
    }

    /// Reads the baseline branch from the repo, never from the cache.
    async fn read_head(&self) -> anyhow::Result<git::Head> {
        #[cfg(test)]
        if self.fail_head.load(Ordering::SeqCst) {
            anyhow::bail!("forced failure");
        }
        git::head(&self.config.repo_dir(), &self.branch).await
    }

    /// The items in the baseline as it is now, for a draft discard (with the
    /// push lock held): HEAD is read again rather than taken from the cache,
    /// which lags a push whose read of HEAD failed. `None`, logged, if HEAD
    /// or its items can't be read, or a backend may still be writing.
    async fn live_items(&self) -> Option<Arc<BTreeSet<ItemRef>>> {
        if self.push_degraded.load(Ordering::SeqCst) {
            tracing::error!("not reading the baseline: {}", git::DEGRADED_MESSAGE);
            return None;
        }
        let mut budget = git::Budget::default();
        match budget.spend(self.read_head()).await? {
            Ok(git::Head::Commit(head)) => budget.spend(self.inventory(&head)).await.flatten(),
            Ok(git::Head::Unborn) => Some(Arc::default()),
            Err(e) => {
                tracing::error!("reading HEAD: {e:#}");
                None
            }
        }
    }

    /// Items in the last baseline listed; empty while the repo is. Lags HEAD
    /// while listing it fails, which only checking a toggle can live with:
    /// cleanup goes through [`Server::inventory`].
    fn baseline_items(&self) -> Arc<BTreeSet<ItemRef>> {
        lock(&self.baseline).inventory.as_ref().map(|i| i.items.clone()).unwrap_or_default()
    }

    /// The items in `commit`, from the cache when it holds that commit, else
    /// listed again (and cached if it is HEAD). `None`, logged, if listing
    /// fails or times out: cleanup that needs it must not run.
    async fn inventory(&self, commit: &str) -> Option<Arc<BTreeSet<ItemRef>>> {
        if let Some(i) = lock(&self.baseline).inventory.as_ref().filter(|i| i.commit == commit) {
            return Some(i.items.clone());
        }
        #[cfg(test)]
        if self.fail_listing.load(Ordering::SeqCst) {
            tracing::error!("listing items of {commit}: forced failure");
            return None;
        }
        match git::items_at(&self.config.repo_dir(), commit).await {
            Ok(items) => {
                let items = Arc::new(items);
                let mut baseline = lock(&self.baseline);
                if !baseline.stale && baseline.head.as_deref() == Some(commit) {
                    baseline.inventory = Some(Inventory { commit: commit.to_string(), items: items.clone() });
                }
                Some(items)
            }
            Err(e) => {
                tracing::error!("listing items of {commit}: {e:#}");
                None
            }
        }
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
    /// refused, or finishes, cleanup included, before the machine goes. The
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

    /// After a push by `pusher` (with the push lock held, `before` being HEAD
    /// before receive-pack ran, `budget` what is left for reading the repo):
    /// if HEAD moved, nudge every daemon, drop toggles for the items the push
    /// removed, and recheck imports. Drops nothing unless both `before` and
    /// the new HEAD could be listed. If HEAD can't be read, the cached one is
    /// marked stale.
    async fn after_push(self: &Arc<Self>, before: git::Head, mut budget: git::Budget, pusher: &Caller) {
        let new = match budget.spend(self.read_head()).await {
            Some(Ok(git::Head::Commit(h))) => h,
            Some(Ok(git::Head::Unborn)) => return,
            Some(Err(e)) => {
                self.forget_head();
                return tracing::error!("reading HEAD after push: {e:#}");
            }
            None => return self.forget_head(),
        };
        if before == git::Head::Commit(new.clone()) && self.head().as_deref() == Some(new.as_str()) {
            return;
        }
        let old = match &before {
            git::Head::Commit(b) => budget.spend(self.inventory(b)).await.flatten(),
            git::Head::Unborn => Some(Arc::default()),
        };
        {
            let mut baseline = lock(&self.baseline);
            baseline.head = Some(new.clone());
            baseline.stale = false;
        }
        let items = budget.spend(self.inventory(&new)).await.flatten();
        tracing::info!("baseline is now {new}");
        self.broadcast(&ServerMessage::Nudge { baseline: new });
        match (old, items) {
            (Some(old), Some(items)) => {
                let removed: BTreeSet<ItemRef> = old.difference(&items).cloned().collect();
                if !removed.is_empty()
                    && let Err(e) = self.sweep_removed(removed, items, pusher.clone()).await
                {
                    tracing::error!("dropping toggles for removed items: {e:#}");
                }
            }
            _ => tracing::warn!("keeping every toggle: this push's removed items are unknown"),
        }
        self.imports_due.notify_one();
    }

    /// [`assign::sweep`] for the items in `removed`, which a push by `pusher`
    /// took out of the baseline (now `items`), publishing what changed. A
    /// `fleet` pusher or the admin sweeps every machine; a `self` pusher only
    /// its own assignment (ADR 0005); a pusher whose token is gone, nothing.
    /// Toggles for items that were already absent are never touched, so a
    /// sweep that fails or never runs only leaves toggles that do nothing.
    async fn sweep_removed(
        &self,
        removed: BTreeSet<ItemRef>,
        items: Arc<BTreeSet<ItemRef>>,
        pusher: Caller,
    ) -> Result<()> {
        let now = now();
        let updated = self
            .db(move |conn| {
                let tx = conn.transaction()?;
                // By token, as it is now: never a machine that joined again under the same id.
                let machines = match pusher.current(&tx) {
                    Ok(Caller::Machine { id, scope: Scope::SelfOnly, .. }) => vec![id],
                    Ok(_) => db::machines(&tx)?.into_iter().map(|m| m.id).collect(),
                    Err(ApiError::Unauthorized) => Vec::new(),
                    Err(e) => return Err(e),
                };
                let removed: Vec<ItemRef> = removed.into_iter().collect();
                let mut updated = Vec::new();
                for id in machines {
                    if let Some(applied) = assign::sweep(&tx, &id, &items, &removed, now)? {
                        updated.push((id, applied.snapshot()));
                    }
                }
                tx.commit()?;
                Ok(updated)
            })
            .await?;
        for (machine, snapshot) in updated {
            self.publish_assignment(&machine, snapshot);
        }
        Ok(())
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
            (StatusCode::UPGRADE_REQUIRED, axum::Json(ErrorBody { error, code: None })).into_response()
        }
        _ => next.run(request).await,
    }
}

/// Tags every response with the server's protocol version.
async fn announce_protocol(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response.headers_mut().insert(PROTOCOL_HEADER, HeaderValue::from(PROTOCOL_VERSION));
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

    /// Review finding: when a push's new HEAD can't be listed, the cache must
    /// not pair that commit with the old items, or the next push's diff would
    /// take in what this one removed and sweep it from every machine.
    #[tokio::test]
    async fn a_failed_listing_never_feeds_a_later_sweep() {
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
        let repo = server.config.repo_dir();
        git(dir.path(), &["clone", "--quiet", &repo.to_string_lossy(), &work.to_string_lossy()]);
        // Commits and pushes the work tree; HEAD before the push.
        let push = |msg: &str| {
            git(&work, &["add", "--all"]);
            git(&work, &["commit", "--quiet", "-m", msg]);
            let before = server.head().map_or(git::Head::Unborn, git::Head::Commit);
            git(&work, &["push", "--quiet", "origin", "HEAD:main"]);
            before
        };
        let pdf = ItemRef::Skill("pdf".into());
        std::fs::create_dir_all(work.join("personal/pdf")).unwrap();
        std::fs::write(work.join("personal/pdf/SKILL.md"), "x\n").unwrap();
        server.after_push(push("pdf"), git::Budget::default(), &Caller::Admin).await;
        assert!(server.baseline_items().contains(&pdf));
        let mut a = sk1ll_proto::Assignment::default();
        a.set_skill("claude", "pdf", true);
        server
            .db(move |conn| {
                db::insert_machine(conn, "solo", &hash("solo"), 0, &a, Scope::SelfOnly)?;
                db::insert_machine(conn, "peer", &hash("peer"), 0, &a, Scope::Fleet).map(drop)
            })
            .await
            .unwrap();

        // The self push removes pdf, and its new HEAD can't be listed.
        std::fs::remove_dir_all(work.join("personal/pdf")).unwrap();
        let before = push("without pdf");
        server.fail_listing.store(true, Ordering::SeqCst);
        let solo = Caller::Machine { id: "solo".into(), scope: Scope::SelfOnly, token_hash: hash("solo") };
        server.after_push(before, git::Budget::default(), &solo).await;
        server.fail_listing.store(false, Ordering::SeqCst);

        // An unrelated fleet push removes nothing.
        std::fs::create_dir_all(work.join("instructions")).unwrap();
        std::fs::write(work.join("instructions/git.md"), "x\n").unwrap();
        server.after_push(push("a rule"), git::Budget::default(), &Caller::Admin).await;
        let items = server.baseline_items();
        assert!(!items.contains(&pdf) && items.contains(&ItemRef::Rule("git".into())));
        let peer = server
            .db(|conn| Ok(db::assignment(conn, "peer")?.unwrap().harness("claude").skills.contains_key("pdf")))
            .await
            .unwrap();
        assert!(peer, "the fleet push must not sweep what the self push removed");
    }

    /// Review finding: cleanup must follow the pusher's token as it is now. A
    /// removed pusher sweeps nothing, even once a machine joined again under
    /// its id; a current one sweeps by the scope it has now.
    #[tokio::test]
    async fn sweeps_follow_the_pushers_current_token_and_scope() {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::open(Config::new(dir.path().join("data"))).await.unwrap();
        let mut a = sk1ll_proto::Assignment::default();
        a.set_skill("claude", "pdf", true);
        let a2 = a.clone();
        server
            .db(move |conn| {
                db::insert_machine(conn, "me", &hash("old"), 0, &a2, Scope::Fleet)?;
                db::insert_machine(conn, "peer", &hash("peer"), 0, &a2, Scope::Fleet).map(drop)
            })
            .await
            .unwrap();
        let caller = |token: &str| Caller::Machine { id: "me".into(), scope: Scope::Fleet, token_hash: hash(token) };
        let has_pdf = |id: &'static str| {
            let server = server.clone();
            async move {
                server
                    .db(move |conn| Ok(db::assignment(conn, id)?.unwrap().harness("claude").skills.contains_key("pdf")))
                    .await
                    .unwrap()
            }
        };
        let removed = BTreeSet::from([ItemRef::Skill("pdf".into())]);

        // `me` is removed mid-push and joins again, self-scoped, with a new token.
        server.remove_machine(Caller::Admin, "me".into()).await.unwrap();
        server
            .db(move |conn| db::insert_machine(conn, "me", &hash("new"), 0, &a, Scope::SelfOnly).map(drop))
            .await
            .unwrap();
        server.sweep_removed(removed.clone(), Arc::default(), caller("old")).await.unwrap();
        assert!(has_pdf("me").await && has_pdf("peer").await, "a removed pusher sweeps nothing");

        // The new token's scope comes from the database, not the caller.
        server.sweep_removed(removed.clone(), Arc::default(), caller("new")).await.unwrap();
        assert!(!has_pdf("me").await && has_pdf("peer").await, "a self pusher sweeps only itself");

        server.sweep_removed(removed, Arc::default(), Caller::Admin).await.unwrap();
        assert!(!has_pdf("peer").await);
    }
}
