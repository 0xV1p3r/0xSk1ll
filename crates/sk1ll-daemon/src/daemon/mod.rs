//! The daemon's shared state and its operations on the repo and harnesses.
//!
//! Every operation that changes the repo, the harnesses or the daemon's state
//! runs on a blocking thread with the engine mutex held ([`Daemon::blocking`]),
//! so they never interleave. Across processes, an engine holds the
//! [`MachineLock`] for its whole life, taken before it reads any of the
//! Machine's state, so no two engines ever change one Machine; and core's
//! [`EngineLock`] on the repo it manages. An engine that holds no machine lock
//! ([`Access::ReadOnly`], a CLI preview) changes nothing at all.
//!
//! No network I/O happens with the engine mutex held. An operation that
//! talks to the server runs in steps: it checks and takes what it will send
//! under the mutex, releases it for the network (git's clone, fetch and push
//! through [`Daemon::network`], Assignment writes over the server session),
//! then takes it again to apply the result, re-checking what the result
//! depends on: an Assignment revision applied meanwhile wins over an older
//! confirmation, a pull moves onto whatever was fetched last, an Adopt whose
//! item changed meanwhile is a conflict, and a daemon that stopped managing
//! meanwhile applies nothing. What every such step checks first is one token,
//! the [`World`] it started in: still managing the same registration, and
//! for what changes the harnesses, no Pull pending. Its lifecycle
//! ([`Lifecycle`]) has a mutex of its own, so the session can mark it
//! connected or offline at any time.
//!
//! Everything that brings the harnesses and the report in line with the repo
//! is one reconcile over fingerprinted Inputs ([`reconcile`]): it plans and
//! applies only when an Input changed since the last one that succeeded.
//!
//! Nothing that only reads takes the engine mutex, so the GUI keeps
//! answering while a long engine operation (a sync of many skills) runs:
//!
//! - The Draft, the inventory, the conflicts and the recovery area come from
//!   the [`Snapshot`] the last reconcile published ([`Daemon::snapshot`]),
//!   the applied Assignment from [`Daemon::applied`]: both are immutable
//!   values swapped whole, so a reader never waits and never sees half of an
//!   update. The local API first brings the Snapshot up to date when the
//!   engine is free ([`Daemon::caught_up`]), so a read right after an edit
//!   sees it; while an engine operation runs, it answers from the last one.
//! - What a request asks about in particular (a diff, a file, the skills
//!   browser, a preview) is read from the repo as it is ([`Daemon::reading`]),
//!   with the repo root, which is fixed for the process ([`Daemon::ident`]).
//!   Such a reader never writes: not the repo, not a harness, not git's index
//!   (the process environment turns off git's opportunistic index refresh,
//!   see [`crate::prepare_process`]), and it does not finish an interrupted
//!   import ([`Daemon::recover_imports`] needs the engine mutex and the
//!   [`EngineLock`]); until the next reconcile has put such an import back,
//!   it shows the repo with that import missing. It may see the repo in the
//!   middle of a concurrent operation (a pull moving the working tree): what
//!   it returns is then momentarily stale and the event that follows the
//!   operation makes the client ask again, and a read that failed while an
//!   operation was running is retried before it is reported as a failure.
//!
//! What the outside sees ([`StatusView`]) lives in a `watch` channel: the server
//! session sends a status whenever its report changes, and the local API and
//! its event stream read from it. Its lifecycle fields are derived from the
//! [`Lifecycle`] whenever it is published ([`Daemon::publish`]).

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, RwLock, TryLockError};

use anyhow::Context;
use sk1ll_core::{EngineLock, MachineConfig, Repo};
use sk1ll_proto::{AssignmentSnapshot, MachineReport};
use tokio::sync::{Notify, broadcast, mpsc, watch};

use crate::client::{Connection, DaemonMode, Event, StatusView};
use crate::error::{Error, Result};
use crate::lifecycle::Lifecycle;
pub(crate) use crate::lifecycle::Link;
use crate::state::{self, DaemonState, PullRecord};
use crate::watch::WatchRoot;
use crate::{Config, MachineLock};

mod adopt;
mod assignment;
mod content;
mod harness_rename;
mod harnesses;
mod import;
mod membership;
mod placement;
mod pull;
mod read;
mod reconcile;
mod snapshot;
mod views;

pub(crate) use harness_rename::rename_write;
pub(crate) use reconcile::run as reconcile_loop;
pub(crate) use snapshot::Snapshot;
use {placement::*, reconcile::*, views::*};

/// Name a `block` instructions conflict goes by (one block per harness).
const BLOCK_NAME: &str = "instructions";

/// What an operation that releases the engine mutex for the network checks
/// again once it holds it again ([`Daemon::world`], [`Daemon::same_world`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct World {
    epoch: u64,
}

/// What an engine may change, and how long it holds the [`MachineLock`].
pub(crate) enum Access {
    /// The daemon: holds the lock for its life and works in the background.
    Daemon(MachineLock),
    /// A command's engine in the CLI ([`crate::local`]): holds the lock for
    /// the command, runs nothing in the background and connects to the
    /// server only for a request that needs the session.
    OnDemand(MachineLock),
    /// A command that only reads (a preview): holds no lock and changes
    /// nothing, not even what a reconcile would bring in line. Every engine
    /// operation is refused; readers answer from the files as they are.
    ReadOnly,
}

/// Process identity. Standalone has no server credentials or fleet state.
#[derive(Clone)]
pub(crate) enum Mode {
    Joined(DaemonState),
    Standalone { machine_id: String, repo: PathBuf },
}

impl Mode {
    pub(crate) fn machine_id(&self) -> &str {
        match self {
            Self::Joined(s) => &s.machine_id,
            Self::Standalone { machine_id, .. } => machine_id,
        }
    }

    pub(crate) fn repo(&self) -> &Path {
        match self {
            Self::Joined(s) => &s.repo,
            Self::Standalone { repo, .. } => repo,
        }
    }

    pub(crate) fn joined(&self) -> Result<&DaemonState> {
        match self {
            Self::Joined(s) => Ok(s),
            Self::Standalone { .. } => {
                Err(Error::Conflict("This Machine is standalone; join a Fleet to use this".into()))
            }
        }
    }
}

pub(crate) struct Daemon {
    pub(crate) cfg: Config,
    /// Fixed for the life of the process: machine id, server URL and token. A
    /// rejoin restarts the process (see [`Daemon::rejoin`]). Its `revoked`
    /// and record are as of startup; [`Daemon::lifecycle`] owns them.
    pub(crate) ident: Mode,
    /// Where the Assignment lives: the server, or the machine-local file.
    pub(crate) store: Box<dyn crate::store::AssignmentStore>,
    /// What the repo fetches from and pushes to: the server, or its origin.
    pub(crate) remote: Box<dyn crate::remote::Remote>,
    /// An engine in a CLI process ([`crate::local`]): it connects to the
    /// server only when a request needs the session, and runs nothing in
    /// the background.
    on_demand: bool,
    /// Held for the engine's life; `None` for a read-only one, which may
    /// change nothing (see [`Access`]).
    machine_lock: Option<MachineLock>,
    /// Started the server session for a request (see [`Daemon::on_demand`]).
    pub(crate) session_started: std::sync::Once,
    engine: Mutex<Engine>,
    /// What the daemon is doing. Never held across network I/O; the lock
    /// order is the engine mutex, then this, then the view.
    lifecycle: Mutex<Lifecycle>,
    /// One git network operation on the repo at a time (clone, fetch, push):
    /// they run without the engine mutex, and git refuses concurrent ref
    /// updates.
    net: tokio::sync::Mutex<()>,
    /// One Assignment write at a time, from its checks to applying the
    /// server's confirmation, as when the engine mutex covered all of it.
    assignment_writes: tokio::sync::Mutex<()>,
    /// The applied assignment. `None` when it was lost (or never stored):
    /// harnesses are left alone until the server sends one. Replaced only
    /// with the engine mutex held ([`Daemon::apply_assignment`]); read by
    /// anyone, readers without the engine mutex included.
    applied: RwLock<Option<Arc<AssignmentSnapshot>>>,
    /// The readers' view of the last reconcile (see [`snapshot`]).
    snapshot: watch::Sender<Arc<Snapshot>>,
    view: watch::Sender<StatusView>,
    events: broadcast::Sender<Event>,
    pub(crate) assignment_session: watch::Sender<Option<mpsc::Sender<crate::server::SessionWrite>>>,
    pub(crate) restart_assignment_session: Notify,
    pub(crate) roots: watch::Sender<Vec<WatchRoot>>,
    pub(crate) http: reqwest::Client,
    /// Wakes the background reconcile ([`reconcile::run`]).
    wakeups: Notify,
    /// The hashes of trees planning found unchanged (see [`HashCache`]).
    hashes: sk1ll_core::hash::HashCache,
    /// How many reconciles planned a sync, for tests.
    plans: std::sync::atomic::AtomicU64,
    /// The Draft changed since a stopped pull was last tried again (see
    /// [`Daemon::retry_stopped_pull`]).
    draft_moved: AtomicBool,
}

/// Mutable state, only touched with the engine mutex held.
pub(crate) struct Engine {
    /// Core's engine lock on the repo, held from startup (or from the clone).
    lock: Option<EngineLock>,
    /// The Inputs last reconciled successfully (see [`reconcile`]); `None`
    /// when the last reconcile failed, reported errors, or none ran yet, so
    /// the next one runs whatever the Inputs.
    reconciled: Option<reconcile::Inputs>,
    /// When the last full reconcile began (see [`Config::full_reconcile_every`]);
    /// `None` until one succeeds, so the first one after startup is full.
    full_at: Option<std::time::Instant>,
    /// How long an Input must have been left alone to be recorded
    /// ([`reconcile::RACY_WINDOW`]; tests widen it).
    racy: std::time::Duration,
    #[cfg(test)]
    after_sync_review: Option<Box<dyn FnOnce() + Send>>,
    /// Runs in [`Daemon::rejoin`] once the old registration was found
    /// replaceable, before the new one is stored.
    #[cfg(test)]
    before_rejoin_persist: Option<Box<dyn FnOnce() + Send>>,
}

impl Daemon {
    #[cfg(test)]
    pub(crate) fn new(cfg: Config, state: DaemonState) -> anyhow::Result<Arc<Self>> {
        let lock = MachineLock::acquire(&cfg)?;
        Self::for_machine(cfg, Some(state), Access::Daemon(lock))
    }

    #[cfg(test)]
    pub(crate) fn standalone(cfg: Config) -> anyhow::Result<Arc<Self>> {
        let lock = MachineLock::acquire(&cfg)?;
        Self::for_machine(cfg, None, Access::Daemon(lock))
    }

    /// The daemon for `cfg`'s Machine. It takes the machine lock before it
    /// reads anything of the Machine, waiting up to `patience` for a
    /// command's engine to end, so it starts from what that engine saved,
    /// never from what it read before.
    pub(crate) async fn start(cfg: Config, patience: std::time::Duration) -> anyhow::Result<Arc<Self>> {
        use anyhow::Context;
        let machine = MachineLock::wait(&cfg, patience).await.context("not starting")?;
        let state = DaemonState::load(&cfg.state)?;
        Self::for_machine(cfg, state, Access::Daemon(machine))
    }

    /// The engine for `cfg`'s Machine: joined if `state` says so. The
    /// caller took the machine lock `access` carries before it read `state`.
    pub(crate) fn for_machine(cfg: Config, state: Option<DaemonState>, access: Access) -> anyhow::Result<Arc<Self>> {
        let mode = match state {
            Some(state) => Mode::Joined(state),
            None => {
                let machine = MachineConfig::load(&cfg.machine_config)?;
                let repo = crate::locate_repo(cfg.repo.as_deref(), false)?;
                Mode::Standalone { machine_id: machine.id, repo }
            }
        };
        Self::with_mode(cfg, mode, access)
    }

    fn with_mode(cfg: Config, state: Mode, access: Access) -> anyhow::Result<Arc<Self>> {
        let (on_demand, machine_lock) = match access {
            Access::Daemon(lock) => (false, Some(lock)),
            Access::OnDemand(lock) => (true, Some(lock)),
            Access::ReadOnly => (true, None),
        };
        let standalone = matches!(state, Mode::Standalone { .. });
        let lifecycle = match &state {
            Mode::Joined(s) => Lifecycle::joined(s.revoked, s.record.clone()),
            Mode::Standalone { repo, .. } => {
                let path = state::standalone_record_path(&cfg.state);
                Lifecycle::standalone(state::load_record(&path, repo).unwrap_or_else(|e| {
                    tracing::warn!("{e:#}; starting without a remembered Baseline");
                    PullRecord::default()
                }))
            }
        };
        let store = crate::store::for_machine(&cfg, state.joined().ok());
        let remote = crate::remote::for_machine(state.joined().ok());
        let snapshot = store.load()?;
        // Only an engine that holds the machine lock writes what it loaded.
        if machine_lock.is_some()
            && let Some(snap) = &snapshot
        {
            store.save(snap)?;
        }
        let mut view = StatusView {
            machine_id: state.machine_id().to_string(),
            server_url: state.joined().ok().map(|s| s.server_url.clone()),
            mode: if standalone { DaemonMode::Standalone } else { DaemonMode::Joined },
            has_origin: remote.has_origin(state.repo()),
            repo_ready: !standalone || state.repo().join(".git").exists(),
            repo: state.repo().to_path_buf(),
            headline: sk1ll_proto::MachineStatus::Offline,
            connection: Connection::Connecting,
            last_error: None,
            revoked: false,
            server_baseline: None,
            assignment_revision: snapshot.as_ref().map_or(0, |s| s.revision),
            version: sk1ll_core::VERSION.to_string(),
            awaiting_assignment: snapshot.is_none(),
            auth_rejected_since: None,
            report: MachineReport::default(),
        };
        lifecycle.apply(&mut view);
        Ok(Arc::new(Self {
            cfg,
            ident: state,
            store,
            remote,
            on_demand,
            machine_lock,
            session_started: std::sync::Once::new(),
            applied: RwLock::new(snapshot.map(Arc::new)),
            snapshot: watch::channel(Snapshot::initial()).0,
            lifecycle: Mutex::new(lifecycle),
            net: tokio::sync::Mutex::new(()),
            assignment_writes: tokio::sync::Mutex::new(()),
            engine: Mutex::new(Engine {
                lock: None,
                reconciled: None,
                full_at: None,
                racy: reconcile::RACY_WINDOW,
                #[cfg(test)]
                after_sync_review: None,
                #[cfg(test)]
                before_rejoin_persist: None,
            }),
            view: watch::channel(view).0,
            events: broadcast::channel(256).0,
            assignment_session: watch::channel(None).0,
            restart_assignment_session: Notify::new(),
            roots: watch::channel(Vec::new()).0,
            http: crate::server::http_client().build().context("building the HTTP client")?,
            wakeups: Notify::new(),
            // Tests reconcile trees they wrote a moment ago.
            hashes: if cfg!(test) {
                sk1ll_core::hash::HashCache::with_racy_window(std::time::Duration::ZERO)
            } else {
                sk1ll_core::hash::HashCache::new()
            },
            plans: Default::default(),
            draft_moved: AtomicBool::new(false),
        }))
    }

    pub(crate) fn init(&self, eng: &mut Engine) -> Result<()> {
        if self.idle() || !self.is_standalone() || self.repo_root().join(".git").exists() {
            return Err(Error::Conflict("Init requires a standalone machine without a repo".into()));
        }
        let root = self.repo_root();
        if let Ok(meta) = root.symlink_metadata()
            && (!meta.is_dir() || std::fs::read_dir(root).context("reading repo directory")?.next().is_some())
        {
            return Err(Error::Conflict(format!(
                "{} is not an empty directory; choose an empty repo directory",
                root.display()
            )));
        }
        std::fs::create_dir_all(root).context("creating repo directory")?;
        // First-run setup ends here: take up the opt-in presets it kept on.
        for warning in MachineConfig::adopt_detected_presets(&self.cfg.machine_config, root)? {
            tracing::warn!("{warning}");
        }
        Repo::init_committed(root)?;
        self.lock_repo(eng)?;
        self.reconcile(eng).map(drop)
    }

    /// Runs `f` with the engine on a blocking thread. The engine mutex is
    /// released as soon as `f` returns, whether it succeeded or not.
    pub(crate) async fn blocking<T, F>(self: &Arc<Self>, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Daemon, &mut Engine) -> Result<T> + Send + 'static,
    {
        self.require_writer()?;
        let d = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut engine = lock(&d.engine);
            f(&d, &mut engine)
        })
        .await
        .map_err(|e| Error::Internal(anyhow::anyhow!("daemon task failed: {e}")))?
    }

    /// Runs the read-only `f` on a blocking thread without the engine mutex
    /// (see the module docs for what a reader may do). If `f` fails while an
    /// engine operation is running, the repo may just have been in between
    /// two states: `f` is tried again a moment later, twice at most, so a
    /// long operation delays a real error only briefly.
    pub(crate) async fn reading<T, F>(self: &Arc<Self>, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: Fn(&Daemon) -> Result<T> + Send + 'static,
    {
        const RETRIES: usize = 2;
        const PAUSE: std::time::Duration = std::time::Duration::from_millis(100);
        let d = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut tries = 0;
            loop {
                match f(&d) {
                    Err(e) if tries < RETRIES && d.engine_busy() => {
                        tracing::debug!("retrying a read that failed during an engine operation: {e}");
                        tries += 1;
                        std::thread::sleep(PAUSE);
                    }
                    result => return result,
                }
            }
        })
        .await
        .map_err(|e| Error::Internal(anyhow::anyhow!("daemon read task failed: {e}")))?
    }

    /// Runs `f`, a git network operation on the repo (clone, fetch, push), on
    /// a blocking thread without the engine mutex, one at a time.
    async fn network<T, F>(self: &Arc<Self>, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Daemon) -> Result<T> + Send + 'static,
    {
        let _net = self.net.lock().await;
        let d = self.clone();
        tokio::task::spawn_blocking(move || f(&d))
            .await
            .map_err(|e| Error::Internal(anyhow::anyhow!("daemon network task failed: {e}")))?
    }

    /// Fails for a read-only engine ([`Access::ReadOnly`]): it holds no
    /// machine lock, so it may change nothing.
    pub(crate) fn require_writer(&self) -> Result<()> {
        if self.machine_lock.is_none() {
            return Err(Error::Conflict(
                "this command only reads, so its engine changes nothing; run a command that changes it".into(),
            ));
        }
        Ok(())
    }

    /// Whether an engine operation is running right now.
    fn engine_busy(&self) -> bool {
        matches!(self.engine.try_lock(), Err(TryLockError::WouldBlock))
    }

    /// What the last reconcile published for readers. Never waits.
    pub(crate) fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot.borrow().clone()
    }

    /// The Snapshot, brought up to date first if the engine is free, so a
    /// read right after an edit sees it: the reconcile skips at once if no
    /// Input changed. A reader never waits for an engine operation: while
    /// one runs, it gets the Snapshot of the last reconcile. Only when none
    /// was ever published (an engine in a CLI process whose repo another
    /// process is changing) is the reconcile's failure the answer. A
    /// read-only engine never reconciles: it only has what it published.
    pub(crate) async fn caught_up(self: &Arc<Self>) -> Result<Arc<Snapshot>> {
        if let Err(e) = self.require_writer() {
            let snapshot = self.snapshot();
            return if snapshot.generation == 0 { Err(e) } else { Ok(snapshot) };
        }
        let d = self.clone();
        let failed = tokio::task::spawn_blocking(move || {
            let mut eng = match d.engine.try_lock() {
                Ok(eng) => eng,
                Err(TryLockError::Poisoned(e)) => e.into_inner(),
                Err(TryLockError::WouldBlock) => return None,
            };
            d.reconcile(&mut eng).err()
        })
        .await
        .ok()
        .flatten();
        // A stopped pull worth trying again is the background's to try.
        if self.draft_moved.load(std::sync::atomic::Ordering::SeqCst) {
            self.wake();
        }
        let snapshot = self.snapshot();
        match failed {
            Some(e) if snapshot.generation == 0 => Err(e),
            Some(e) => {
                tracing::debug!("reading the last Snapshot: {e}");
                Ok(snapshot)
            }
            None => Ok(snapshot),
        }
    }

    /// The Snapshot as of now, for a caller about to act on what it lists
    /// (`?fresh=true`): waits for a running engine operation, then
    /// reconciles. Unlike [`Daemon::caught_up`], it may wait.
    pub(crate) async fn fresh(self: &Arc<Self>) -> Result<Arc<Snapshot>> {
        self.blocking(|d, eng| if d.idle() { Ok(()) } else { d.reconcile(eng).map(drop) }).await?;
        Ok(self.snapshot())
    }

    /// The applied assignment, if there is one.
    pub(crate) fn applied(&self) -> Option<Arc<AssignmentSnapshot>> {
        self.applied.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The repo, for readers: the same path as the engine's, which never
    /// changes while the process runs.
    fn repo_root(&self) -> &Path {
        self.ident.repo()
    }

    pub(crate) fn view(&self) -> StatusView {
        self.view.borrow().clone()
    }

    pub(crate) fn subscribe_view(&self) -> watch::Receiver<StatusView> {
        self.view.subscribe()
    }

    pub(crate) fn subscribe_events(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub(crate) fn event(&self, e: Event) {
        let _ = self.events.send(e);
    }

    pub(crate) fn is_standalone(&self) -> bool {
        matches!(self.ident, Mode::Standalone { .. })
    }

    /// An engine in a CLI process ([`crate::local`]), bound to no socket:
    /// it reaches the server only for the request it serves.
    pub(crate) fn on_demand(&self) -> bool {
        self.on_demand
    }

    pub(crate) fn require_repo(&self) -> Result<()> {
        if self.is_standalone() && !reconcile::repo_present(self.repo_root()) {
            return Err(Self::no_repo());
        }
        Ok(())
    }

    /// Why a standalone daemon without its repo answers nothing about it.
    pub(super) fn no_repo() -> Error {
        Error::Conflict("this machine has no skills repo yet; initialize it with `sk1ll init` or POST /init".into())
    }

    /// The current lifecycle.
    pub(crate) fn lifecycle(&self) -> Lifecycle {
        lock(&self.lifecycle).clone()
    }

    pub(crate) fn is_connected(&self) -> bool {
        lock(&self.lifecycle).connected()
    }

    pub(crate) fn is_revoked(&self) -> bool {
        lock(&self.lifecycle).revoked()
    }

    /// Changes the lifecycle (`t`) and the rest of the view (`f`) as one
    /// publication, deriving the view's lifecycle fields and headline, and
    /// emits events for what changed. A change to what the state file keeps
    /// is saved first; the error says if that failed (the change stands).
    fn publish<T>(
        &self,
        t: impl FnOnce(&mut Lifecycle) -> T,
        f: impl FnOnce(&mut StatusView),
    ) -> (T, anyhow::Result<()>) {
        let mut lc = lock(&self.lifecycle);
        let before = lc.clone();
        let out = t(&mut lc);
        let saved = if lc.persisted() != before.persisted() { self.save_lifecycle(&lc) } else { Ok(()) };
        let mut draft_changed = false;
        let changed = self.view.send_if_modified(|v| {
            let old = v.clone();
            f(v);
            lc.apply(v);
            draft_changed = old.report.drafts != v.report.drafts;
            *v != old
        });
        let fleet = before.connected() != lc.connected() || before.revoked() != lc.revoked();
        drop(lc);
        if changed {
            self.event(Event::Status);
        }
        if draft_changed {
            self.draft_moved.store(true, std::sync::atomic::Ordering::SeqCst);
            self.event(Event::Draft);
        }
        if fleet {
            self.event(Event::Fleet);
        }
        (out, saved)
    }

    /// Writes the lifecycle's persistent part. A halted daemon writes
    /// nothing: the state file may already hold its next registration.
    fn save_lifecycle(&self, lc: &Lifecycle) -> anyhow::Result<()> {
        if lc.halted() {
            return Ok(());
        }
        let (revoked, record) = lc.persisted();
        match &self.ident {
            Mode::Joined(s) => DaemonState { revoked, record: record.clone(), ..s.clone() }.save(&self.cfg.state),
            Mode::Standalone { repo, .. } => {
                state::save_record(&state::standalone_record_path(&self.cfg.state), repo, record)
            }
        }
    }

    /// A lifecycle transition whose state file failure fails the caller.
    fn transition<T>(&self, t: impl FnOnce(&mut Lifecycle) -> T) -> Result<T> {
        let (out, saved) = self.publish(t, |_| {});
        saved.context("saving the daemon state")?;
        Ok(out)
    }

    /// A lifecycle transition whose state file failure only shows in the
    /// status: the daemon goes on from what it knows.
    fn note<T>(&self, t: impl FnOnce(&mut Lifecycle) -> T) -> T {
        let (out, saved) = self.publish(t, |_| {});
        if let Err(e) = saved {
            self.set_error(format!("saving the daemon state: {e:#}"));
        }
        out
    }

    /// Changes the view, recomputing the headline; emits events for changes.
    pub(crate) fn update(&self, f: impl FnOnce(&mut StatusView)) {
        let ((), saved) = self.publish(|_| (), f);
        debug_assert!(saved.is_ok());
    }

    /// The server session's connection changed.
    /// `false`: the daemon has no connection any more (halted or removed).
    pub(crate) fn set_link(&self, link: Link) -> bool {
        self.note(|lc| lc.set_link(link))
    }

    /// The server answered 401 to the token. That can be a proxy or a server
    /// misconfiguration as well as a removal while the daemon was away, so the
    /// machine keeps being managed and the connection is retried; only the
    /// server's `revoked` message revokes it. After `auth_warn_after`, the status
    /// says the machine was probably removed.
    pub(crate) fn set_auth_rejected(&self) {
        let now = unix_now();
        let Some(since) = self.note(|lc| lc.auth_rejected(now)) else { return };
        let message = if now - since >= self.cfg.auth_warn_after.as_secs() as i64 {
            format!(
                "the server has rejected this machine's token since {}h ago; it was probably removed from the fleet: \
                 join again with `sk1ll join <server-url> <join-code>` (still managing the harnesses meanwhile)",
                (now - since) / 3600
            )
        } else {
            "the server rejected this machine's token (HTTP 401); retrying. If it was removed from the fleet, \
             join again with `sk1ll join <server-url> <join-code>`"
                .to_string()
        };
        if self.view().last_error.as_deref() != Some(message.as_str()) {
            tracing::warn!("{message}");
        }
        self.update(|v| v.last_error = Some(message));
    }

    pub(crate) fn set_error(&self, e: impl ToString) {
        let e = crate::auth::redact(&e.to_string());
        tracing::warn!("{e}");
        self.update(|v| v.last_error = Some(e));
    }

    // -----------------------------------------------------------------------
    // Engine operations (blocking, with the engine mutex held)

    pub(crate) fn ensure_managed(&self) -> Result<()> {
        lock(&self.lifecycle).managed().map_err(|why| Error::Conflict(why.into()))?;
        self.require_repo()
    }

    /// The world an operation that releases the engine mutex starts in,
    /// taken with the mutex held: managing, and which registration.
    pub(crate) fn world(&self) -> Result<World> {
        self.ensure_managed()?;
        Ok(World { epoch: lock(&self.lifecycle).epoch() })
    }

    /// With the engine mutex held again: whether the daemon still manages
    /// the registration it managed when `world` was taken. A removal, a halt
    /// for a rejoin or a restart since all fail it, so nothing that belonged
    /// to the old registration is applied to (or saved over) the new one.
    pub(crate) fn same_world(&self, world: World) -> Result<()> {
        let lc = lock(&self.lifecycle);
        lc.managed().map_err(|why| Error::Conflict(why.into()))?;
        if lc.epoch() != world.epoch {
            return Err(Error::Conflict("this Machine's registration changed meanwhile; nothing was applied".into()));
        }
        drop(lc);
        self.require_repo()
    }

    /// [`Daemon::same_world`], and no Pull pending: what changes the
    /// harnesses waits until the pull is finished (core refuses it too).
    pub(crate) fn same_world_settled(&self, world: World) -> Result<()> {
        self.same_world(world)?;
        Ok(Repo::open_git_only(self.repo_root())?.require_no_pending_pull()?)
    }

    /// Nothing to manage: revoked, or about to restart.
    fn idle(&self) -> bool {
        lock(&self.lifecycle).idle()
    }

    /// Takes the engine lock on the repo if it exists and isn't held yet. Held
    /// by someone else means another sk1ll process is changing it.
    pub(crate) fn lock_repo(&self, eng: &mut Engine) -> Result<()> {
        self.require_writer()?;
        let root = self.repo_root();
        if eng.lock.is_some() || !root.join(".git").exists() {
            return Ok(());
        }
        match EngineLock::try_acquire(root)? {
            Some(l) => {
                tracing::debug!("holding {}", l.path().display());
                eng.lock = Some(l);
                Ok(())
            }
            None => Err(Error::Conflict(format!(
                "another sk1ll process (a second daemon, or a CLI command working directly) is changing {}",
                root.display()
            ))),
        }
    }

    /// At startup: set up the repo if it isn't there yet (the fleet's
    /// clone, without the engine mutex; it also drops credentials an older
    /// version left in origin's URL), and bring the harnesses in line.
    pub(crate) async fn startup(self: &Arc<Self>) -> Result<()> {
        if self.idle() {
            return Ok(());
        }
        // Git authenticates through the process environment (`prepare_process`).
        if let Err(e) = self.network(|d| Ok(d.remote.set_up(d.repo_root()))).await? {
            self.set_error(e);
        }
        self.blocking(|d, eng| {
            d.lock_repo(eng)?;
            d.reconcile(eng).map(drop)
        })
        .await
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests;
