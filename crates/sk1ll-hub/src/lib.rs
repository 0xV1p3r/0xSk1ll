//! The hub: a self-hosted server that hosts the fleet's canonical bare repo
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
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use sk1ll_proto::{AssignmentSnapshot, HubMessage, ImportsView, ItemRef};
use tokio::net::TcpListener;
use tokio::sync::Notify;

pub use config::Config;
use error::{ApiError, Result};

/// Shared hub state.
pub struct Hub {
    config: Config,
    /// Only ever locked on a blocking thread, through [`Hub::db`].
    db: Arc<Mutex<Connection>>,
    /// Live daemon sessions by machine id; a newer one replaces (and cancels)
    /// an older. Locked briefly, never across an `.await` or the DB lock.
    conns: Mutex<HashMap<String, ws::Handle>>,
    next_conn: AtomicU64,
    /// Last baseline the hub acted on.
    baseline: Mutex<Option<Baseline>>,
    /// Held across a whole push: receive-pack and the pass after it.
    push_lock: Arc<tokio::sync::Mutex<()>>,
    /// Reject pushes until restart if a backend may still be writing refs.
    push_degraded: AtomicBool,
    #[cfg(test)]
    fail_cleanup_confirmation: AtomicBool,
    imports: Mutex<ImportsView>,
    imports_due: Notify,
}

/// A baseline commit and the skills and fragments in its tree.
#[derive(Clone)]
struct Baseline {
    commit: String,
    items: Arc<BTreeSet<ItemRef>>,
}

/// Who made an authenticated request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// The bootstrap token (`SK1LL_HUB_ADMIN_TOKEN`).
    Admin,
    Machine(String),
}

impl Caller {
    /// Name recorded as a log entry's author.
    pub fn author(&self) -> &str {
        match self {
            Self::Admin => "admin",
            Self::Machine(id) => id,
        }
    }
}

impl Hub {
    /// Creates the data dir, bare repo and database as needed.
    pub async fn open(config: Config) -> anyhow::Result<Arc<Self>> {
        let (data_dir, repo, db_path) = (config.data_dir.clone(), config.repo_dir(), config.db_path());
        let conn = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
            git::init_bare(&repo)?;
            git::init_baseline(&repo)?;
            db::open(&db_path)
        })
        .await??;
        let baseline = match git::head(&config.repo_dir()).await? {
            Some(commit) => {
                Some(Baseline { items: Arc::new(git::items_at(&config.repo_dir(), &commit).await?), commit })
            }
            None => None,
        };
        if config.admin_token.is_none() {
            tracing::warn!("SK1LL_HUB_ADMIN_TOKEN is not set: only joined machines can create join codes");
        }
        Ok(Arc::new(Self {
            config,
            db: Arc::new(Mutex::new(conn)),
            conns: Mutex::default(),
            next_conn: AtomicU64::new(1),
            baseline: Mutex::new(baseline),
            push_lock: Arc::default(),
            push_degraded: AtomicBool::new(false),
            #[cfg(test)]
            fail_cleanup_confirmation: AtomicBool::new(false),
            imports: Mutex::default(),
            imports_due: Notify::new(),
        }))
    }

    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .nest("/api", api::routes())
            .route("/git/{*path}", any(git::http))
            .route("/healthz", get(healthz))
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

    fn head(&self) -> Option<String> {
        lock(&self.baseline).as_ref().map(|b| b.commit.clone())
    }

    /// Items in the current baseline; empty while the repo is.
    fn baseline_items(&self) -> Arc<BTreeSet<ItemRef>> {
        lock(&self.baseline).as_ref().map(|b| b.items.clone()).unwrap_or_default()
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
        self.db(move |conn| db::machine_for_token(conn, &token_hash))
            .await?
            .map(Caller::Machine)
            .ok_or(ApiError::Unauthorized)
    }

    /// Registers a daemon session, cancelling the one it replaces.
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

    /// Removes and cancels the machine's session; it is told `Revoked` if
    /// its socket isn't stalled.
    fn revoke(&self, machine: &str) {
        if let Some(h) = lock(&self.conns).remove(machine) {
            h.revoke();
        }
    }

    fn broadcast(&self, msg: &HubMessage) {
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

    /// After a push (with the push lock held, `before` being HEAD before
    /// receive-pack ran): if HEAD moved, nudge every daemon, drop toggles for
    /// items no machine can have any more, and recheck imports.
    async fn after_push(self: &Arc<Self>, before: Option<String>) {
        let repo = self.config.repo_dir();
        let new = match git::head(&repo).await {
            Ok(Some(h)) => h,
            Ok(None) => return,
            Err(e) => return tracing::error!("reading HEAD after push: {e:#}"),
        };
        if before.as_deref() == Some(new.as_str()) && self.head().as_deref() == Some(new.as_str()) {
            return;
        }
        let items = match git::items_at(&repo, &new).await {
            Ok(items) => Arc::new(items),
            Err(e) => {
                tracing::error!("listing items of {new}: {e:#}");
                self.baseline_items()
            }
        };
        *lock(&self.baseline) = Some(Baseline { commit: new.clone(), items: items.clone() });
        tracing::info!("baseline is now {new}");
        self.broadcast(&HubMessage::Nudge { baseline: new });
        if let Err(e) = self.sweep_all(items).await {
            tracing::error!("dropping toggles for removed items: {e:#}");
        }
        self.imports_due.notify_one();
    }

    /// [`assign::sweep`] for every machine, publishing what changed.
    async fn sweep_all(&self, items: Arc<BTreeSet<ItemRef>>) -> Result<()> {
        let now = now();
        let updated = self
            .db(move |conn| {
                let tx = conn.transaction()?;
                let mut updated = Vec::new();
                for m in db::machines(&tx)? {
                    if let Some(applied) = assign::sweep(&tx, &m.id, &items, None, now)? {
                        updated.push((m.id, applied.snapshot()));
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

async fn healthz(State(hub): State<Arc<Hub>>) -> Response {
    let degraded = hub.push_degraded.load(Ordering::SeqCst);
    let status = if degraded { StatusCode::SERVICE_UNAVAILABLE } else { StatusCode::OK };
    (
        status,
        axum::Json(serde_json::json!({
            "status": if degraded { "degraded" } else { "ok" },
            "push_degraded": degraded,
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
    let hub = Hub::open(config).await?;
    tokio::spawn(imports::poll(hub.clone()));
    tracing::info!("listening on {}", listener.local_addr()?);
    axum::serve(listener, hub.router()).with_graceful_shutdown(shutdown()).await?;
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
        let hub = Hub::open(Config::new(dir.path().join("data"))).await.unwrap();
        let held = hub.db.clone();
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _guard = lock(&held);
            locked_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(500));
        });
        locked_rx.recv().unwrap();
        let start = std::time::Instant::now();
        let query = tokio::spawn({
            let hub = hub.clone();
            async move { hub.authenticate("some-token").await.is_err() }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(start.elapsed() < Duration::from_millis(300), "the runtime was blocked for {:?}", start.elapsed());
        assert!(query.await.unwrap());
        holder.join().unwrap();
    }
}
