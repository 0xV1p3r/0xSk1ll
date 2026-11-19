//! The daemon's shared state and its operations on the repo and harnesses.
//!
//! Every operation that changes the repo, the harnesses or the daemon's state
//! runs on a blocking thread with the engine mutex held ([`Daemon::blocking`]),
//! so they never interleave; across processes, the daemon holds core's
//! [`EngineLock`] on the repo for its whole lifetime, so direct CLI mutations
//! are refused.
//!
//! Operations that only read (the draft, a diff, the skills browser, the
//! conflicts, the inventory) run without the engine mutex
//! ([`Daemon::reading`]), so the GUI keeps answering while a pull or a push
//! waits for the server under it. They need the repo root, which is fixed
//! for the process ([`Daemon::ident`]), and the applied assignment, which is
//! published in [`Daemon::snapshot`]. A reader:
//!
//! - never writes: not the repo, not a harness, not git's index (the process
//!   environment turns off git's opportunistic index refresh, see
//!   [`crate::prepare_process`]), and it does not finish an interrupted
//!   import ([`Daemon::recover_imports`] needs the engine mutex and the
//!   [`EngineLock`]). Until the next refresh has put such an import back, a
//!   reader shows the repo as it is, with that import missing;
//! - may see the repo in the middle of a concurrent operation (a pull moving
//!   the working tree). What it returns is then momentarily stale and the
//!   event that follows the operation makes the client ask again; a read
//!   that failed while an operation was running is retried before it is
//!   reported as a failure.
//!
//! What the outside sees ([`StatusView`]) lives in a `watch` channel: the server
//! session sends a status whenever its report changes, and the local API and
//! its event stream read from it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, TryLockError};

use anyhow::Context;
use sk1ll_core::imports;
use sk1ll_core::sync::{self, Action, ActionKind, Item, PlanOptions};
use sk1ll_core::{
    EngineLock, HarnessOrigin, ItemRef, MachineConfig, PullOutcome, Repo, default_commit_message, machine_author,
};
use sk1ll_proto::{AssignmentSnapshot, ConflictRef, MachineReport, headline};
use tokio::sync::{Notify, broadcast, watch};

use crate::client::{
    AddImportRequest, ChangeKind, CommitMessage, CommitRequest, CommitResponse, ConflictTarget, ConflictView,
    Connection, DaemonMode, DiffView, DraftEntry, Event, HarnessChoice, HarnessView, ImportUpdated, ItemView,
    PullResult, PushResult, ResolveImportRequest, ResolvedSource, RestoreRequest, RestoreResponse, SkillCandidate,
    StatusView, SyncResult, UpdateImportsRequest,
};
use crate::error::{Error, Result};
use crate::join::{Joined, Registered};
use crate::outbox::{self, Outbox};
use crate::state::{self, DaemonState};
use crate::watch::WatchRoot;
use crate::{Config, gitx};

/// Name a `block` instructions conflict goes by (one block per harness).
const BLOCK_NAME: &str = "instructions";

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

    fn revoked(&self) -> bool {
        matches!(self, Self::Joined(s) if s.revoked)
    }
}

pub(crate) struct Daemon {
    pub(crate) cfg: Config,
    /// Fixed for the life of the process: machine id, server URL and token. A
    /// rejoin restarts the process (see [`Daemon::rejoin`]).
    pub(crate) ident: Mode,
    engine: Mutex<Engine>,
    /// The applied assignment. `None` when it was lost (or never stored):
    /// harnesses are left alone until the server sends one. Replaced only
    /// with the engine mutex held ([`Daemon::apply_assignment`]); read by
    /// anyone, readers without the engine mutex included.
    snapshot: RwLock<Option<Arc<AssignmentSnapshot>>>,
    view: watch::Sender<StatusView>,
    events: broadcast::Sender<Event>,
    /// `draft_discarded` messages not yet delivered, persisted.
    outbox: Mutex<Outbox>,
    /// Signals the server session that the outbox has something new.
    pub(crate) outbox_ready: Notify,
    connected: AtomicBool,
    revoked: AtomicBool,
    /// Since when (Unix seconds) the server has been rejecting the token.
    auth_rejected: Mutex<Option<i64>>,
    pub(crate) roots: watch::Sender<Vec<WatchRoot>>,
    pub(crate) http: reqwest::Client,
}

/// See [`Daemon::pull_state`].
#[derive(PartialEq)]
struct PullState {
    head: Option<String>,
    remote: Option<String>,
    blocked: Option<Vec<ItemRef>>,
}

/// Mutable state, only touched with the engine mutex held.
pub(crate) struct Engine {
    state: Mode,
    /// Draft items clashing with the server's newer baseline (Blocked).
    blocked: Option<Vec<ItemRef>>,
    /// The server's baseline as last announced or fetched.
    server_baseline: Option<String>,
    /// Core's engine lock on the repo, held from startup (or from the clone).
    lock: Option<EngineLock>,
    /// Joined again and about to restart: do nothing more.
    frozen: bool,
    /// The published report is not the result of a successful refresh: the
    /// last one failed, or none ran yet. Nothing may be derived from it.
    stale: bool,
}

impl Daemon {
    pub(crate) fn new(cfg: Config, state: DaemonState) -> anyhow::Result<Arc<Self>> {
        Self::with_mode(cfg, Mode::Joined(state))
    }

    pub(crate) fn standalone(cfg: Config) -> anyhow::Result<Arc<Self>> {
        let machine = MachineConfig::load(&cfg.machine_config)?;
        let repo = crate::locate_repo(cfg.repo.as_deref(), false)?;
        Self::with_mode(cfg, Mode::Standalone { machine_id: machine.id, repo })
    }

    fn with_mode(cfg: Config, state: Mode) -> anyhow::Result<Arc<Self>> {
        let standalone = matches!(state, Mode::Standalone { .. });
        let snapshot = if standalone {
            let assignment = sk1ll_core::Assignment::load(&cfg.assignment)?;
            let previous = state::load_snapshot(&cfg.state.with_file_name("standalone-assignment.json"))?;
            let revision = previous.as_ref().map_or(1, |s| s.revision + u64::from(s.assignment != assignment));
            Some(AssignmentSnapshot { revision, assignment })
        } else {
            match state::load_snapshot(&cfg.snapshot_path()) {
                Ok(Some(s)) => Some(s),
                Ok(None) => {
                    tracing::warn!("no stored assignment: leaving the harnesses alone until the server sends one");
                    None
                }
                Err(e) => {
                    tracing::warn!("{e:#}; leaving the harnesses alone until the server sends an assignment");
                    None
                }
            }
        };
        if standalone && let Some(snap) = &snapshot {
            state::save_snapshot(&cfg.state.with_file_name("standalone-assignment.json"), snap)?;
        }
        let outbox = match &state {
            Mode::Joined(joined) => Outbox::load(cfg.outbox_path(), joined),
            Mode::Standalone { .. } => Outbox::empty(cfg.outbox_path()),
        };
        let view = StatusView {
            machine_id: state.machine_id().to_string(),
            server_url: state.joined().ok().map(|s| s.server_url.clone()),
            mode: if standalone { DaemonMode::Standalone } else { DaemonMode::Joined },
            has_origin: !standalone || gitx::has_origin(state.repo()).unwrap_or(false),
            repo_ready: !standalone || state.repo().join(".git").exists(),
            repo: state.repo().to_path_buf(),
            headline: sk1ll_proto::MachineStatus::Offline,
            connection: if standalone {
                Connection::Standalone
            } else if state.revoked() {
                Connection::Revoked
            } else {
                Connection::Connecting
            },
            last_error: None,
            revoked: state.revoked(),
            server_baseline: None,
            assignment_revision: snapshot.as_ref().map_or(0, |s| s.revision),
            version: sk1ll_core::VERSION.to_string(),
            awaiting_assignment: snapshot.is_none(),
            auth_rejected_since: None,
            pending_discards: outbox.len(),
            report: MachineReport::default(),
        };
        Ok(Arc::new(Self {
            cfg,
            ident: state.clone(),
            revoked: AtomicBool::new(state.revoked()),
            snapshot: RwLock::new(snapshot.map(Arc::new)),
            engine: Mutex::new(Engine {
                state,
                blocked: None,
                server_baseline: None,
                lock: None,
                frozen: false,
                stale: true,
            }),
            view: watch::channel(view).0,
            events: broadcast::channel(256).0,
            outbox: Mutex::new(outbox),
            outbox_ready: Notify::new(),
            connected: AtomicBool::new(false),
            auth_rejected: Mutex::new(None),
            roots: watch::channel(Vec::new()).0,
            http: crate::server::http_client().build().context("building the HTTP client")?,
        }))
    }

    fn require_origin(&self) -> Result<()> {
        self.require_repo()?;
        if !gitx::has_origin(self.repo_root())? {
            return Err(Error::Conflict("this repo has no origin remote".into()));
        }
        Ok(())
    }

    pub(crate) fn init(&self, eng: &mut Engine) -> Result<()> {
        if eng.frozen || !self.is_standalone() || self.repo_root().join(".git").exists() {
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
        Repo::init_committed(root)?;
        self.lock_repo(eng)?;
        self.refresh(eng)
    }

    /// Publishes local file edits through the same snapshot the engine's
    /// readers use. The separate revision file never becomes fleet state.
    fn reload_local_assignment(&self) -> Result<()> {
        let assignment = sk1ll_core::Assignment::load(&self.cfg.assignment)?;
        let previous = self.snapshot();
        if previous.as_ref().is_some_and(|s| s.assignment == assignment) {
            return Ok(());
        }
        let revision = previous.as_ref().map_or(1, |s| s.revision + 1);
        let snap = AssignmentSnapshot { revision, assignment };
        state::save_snapshot(&self.cfg.state.with_file_name("standalone-assignment.json"), &snap)?;
        *self.snapshot.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(snap));
        self.update(|v| {
            v.assignment_revision = revision;
            v.awaiting_assignment = false;
        });
        self.event(Event::Assignment { revision });
        Ok(())
    }

    pub(crate) fn set_local_assignment(
        &self,
        eng: &mut Engine,
        changes: &sk1ll_proto::AssignmentChanges,
    ) -> Result<sk1ll_proto::AssignmentUpdate> {
        use sk1ll_proto::ToggleKind;
        self.ensure_managed(eng)?;
        self.lock_repo(eng)?;
        let repo = Repo::open(Self::root(eng))?;
        let harnesses = MachineConfig::load(&self.cfg.machine_config)?.harnesses();
        let skills: Vec<String> = repo.skills()?.into_iter().map(|s| s.name).collect();
        let rules: Vec<String> = repo.rules()?.into_iter().map(|r| r.name).collect();
        let mut assignment = sk1ll_core::Assignment::load(&self.cfg.assignment)?;
        for change in &changes.changes {
            if !harnesses.contains_key(&change.harness) {
                return Err(Error::BadRequest(format!("unknown harness `{}`", change.harness)));
            }
            let name = change.name.as_deref();
            if change.kind.has_name() != name.is_some() || (!change.kind.has_name() && change.enabled.is_none()) {
                return Err(Error::BadRequest(
                    "item toggles need a name; defaults need enabled and take no name".into(),
                ));
            }
            if let Some(name) = name {
                sk1ll_core::imports::validate_name(name)?;
                let existing = if change.kind == ToggleKind::Skill { &skills } else { &rules };
                if change.enabled == Some(true) && !existing.iter().any(|n| n == name) {
                    return Err(Error::BadRequest(format!("unknown item `{name}`")));
                }
            }
            match change.kind {
                ToggleKind::NewSkillsDefault => {
                    assignment.materialize_skills(&change.harness, &skills);
                    assignment.set_new_skills_default(&change.harness, change.enabled.unwrap());
                }
                ToggleKind::NewRulesDefault => {
                    assignment.materialize_rules(&change.harness, &rules);
                    assignment.set_new_rules_default(&change.harness, change.enabled.unwrap());
                }
                ToggleKind::Skill | ToggleKind::Rule => {
                    let h = assignment.harnesses.entry(change.harness.clone()).or_default();
                    let toggles = if change.kind == ToggleKind::Skill { &mut h.skills } else { &mut h.rules };
                    let name = name.unwrap();
                    if let Some(enabled) = change.enabled {
                        toggles.insert(name.into(), enabled);
                    } else {
                        toggles.remove(name);
                    }
                }
            }
        }
        // Assignment::save replaces the file atomically, like the direct CLI.
        assignment.save(&self.cfg.assignment)?;
        self.refresh(eng)?;
        let snap = self.snapshot().expect("standalone always has assignment");
        Ok(sk1ll_proto::AssignmentUpdate {
            revision: snap.revision,
            assignment: snap.assignment.clone(),
            entries: Vec::new(),
        })
    }

    /// Runs `f` with the engine on a blocking thread. The engine mutex is
    /// released as soon as `f` returns, whether it succeeded or not.
    pub(crate) async fn blocking<T, F>(self: &Arc<Self>, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Daemon, &mut Engine) -> Result<T> + Send + 'static,
    {
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
    /// long operation (a hanging fetch) delays a real error only briefly.
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

    /// Whether an engine operation is running right now.
    fn engine_busy(&self) -> bool {
        matches!(self.engine.try_lock(), Err(TryLockError::WouldBlock))
    }

    /// The applied assignment, if there is one.
    pub(crate) fn snapshot(&self) -> Option<Arc<AssignmentSnapshot>> {
        self.snapshot.read().unwrap_or_else(|e| e.into_inner()).clone()
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

    pub(crate) fn require_repo(&self) -> Result<()> {
        if self.is_standalone() && !self.repo_root().join(".git").exists() {
            return Err(Error::Conflict(
                "this machine has no skills repo yet; initialize it with `sk1ll init` or POST /init".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    pub(crate) fn is_revoked(&self) -> bool {
        self.revoked.load(Ordering::SeqCst)
    }

    /// Changes the view, recomputing the headline; emits events for changes.
    pub(crate) fn update(&self, f: impl FnOnce(&mut StatusView)) {
        let mut draft_changed = false;
        let changed = self.view.send_if_modified(|v| {
            let before = v.clone();
            f(v);
            let offline = !matches!(v.connection, Connection::Connected | Connection::Standalone);
            v.headline = headline(offline, Some(&v.report), v.server_baseline.as_deref());
            draft_changed = before.report.drafts != v.report.drafts;
            *v != before
        });
        if changed {
            self.event(Event::Status);
        }
        if draft_changed {
            self.event(Event::Draft);
        }
    }

    /// Connected or Offline. Connecting clears an auth rejection.
    pub(crate) fn set_connection(&self, connection: Connection) {
        if self.is_revoked() {
            return;
        }
        let connected = connection == Connection::Connected;
        let was = self.connected.swap(connected, Ordering::SeqCst);
        if connected {
            *lock(&self.auth_rejected) = None;
        }
        self.update(|v| {
            v.connection = connection;
            if connected {
                v.auth_rejected_since = None;
            }
        });
        if was != connected {
            self.event(Event::Fleet);
        }
    }

    /// The server answered 401 to the token. That can be a proxy or a server
    /// misconfiguration as well as a removal while the daemon was away, so the
    /// machine keeps being managed and the connection is retried; only the
    /// server's `revoked` message revokes it. After `auth_warn_after`, the status
    /// says the machine was probably removed.
    pub(crate) fn set_auth_rejected(&self) {
        if self.is_revoked() {
            return;
        }
        let was = self.connected.swap(false, Ordering::SeqCst);
        let now = unix_now();
        let since = *lock(&self.auth_rejected).get_or_insert(now);
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
        self.update(|v| {
            v.connection = Connection::AuthRejected;
            v.auth_rejected_since = Some(since);
            v.last_error = Some(message);
        });
        if was {
            self.event(Event::Fleet);
        }
    }

    pub(crate) fn set_error(&self, e: impl ToString) {
        let e = crate::auth::redact(&e.to_string());
        tracing::warn!("{e}");
        self.update(|v| v.last_error = Some(e));
    }

    // -----------------------------------------------------------------------
    // Outbox

    /// Queues a `draft_discarded` durably and wakes the server session.
    fn queue_discarded(&self, items: Vec<ItemRef>) {
        if self.is_standalone() {
            return;
        }
        let pending = {
            let mut o = lock(&self.outbox);
            if let Err(e) = o.push(items) {
                self.set_error(format!("saving the outbox (kept in memory): {e:#}"));
            }
            o.len()
        };
        self.update(|v| v.pending_discards = pending);
        self.outbox_ready.notify_one();
    }

    pub(crate) fn outbox_front(&self, sent: &std::collections::HashSet<u64>) -> Option<outbox::Entry> {
        lock(&self.outbox).unsent(sent)
    }

    /// The server confirmed `id`, or filtering left nothing to deliver.
    pub(crate) fn outbox_delivered(&self, id: u64) {
        let pending = {
            let mut o = lock(&self.outbox);
            if let Err(e) = o.remove(id) {
                tracing::warn!("saving the outbox: {e:#}");
            }
            o.len()
        };
        self.update(|v| v.pending_discards = pending);
    }

    // -----------------------------------------------------------------------
    // Engine operations (blocking, with the engine mutex held)

    fn ensure_managed(&self, eng: &Engine) -> Result<()> {
        if eng.state.revoked() {
            return Err(Error::Conflict(
                "this machine was removed from the fleet; the daemon no longer manages it".into(),
            ));
        }
        if eng.frozen {
            return Err(Error::Conflict("the daemon is restarting after joining again; try again".into()));
        }
        self.require_repo()
    }

    /// Nothing to manage: revoked, or about to restart.
    fn idle(eng: &Engine) -> bool {
        eng.state.revoked() || eng.frozen
    }

    fn root(eng: &Engine) -> &Path {
        eng.state.repo()
    }

    /// Takes the engine lock on the repo if it exists and isn't held yet. Held
    /// by someone else means another sk1ll process is changing it.
    pub(crate) fn lock_repo(&self, eng: &mut Engine) -> Result<()> {
        if eng.lock.is_some() || !eng.state.repo().join(".git").exists() {
            return Ok(());
        }
        match EngineLock::try_acquire(eng.state.repo())? {
            Some(l) => {
                tracing::debug!("holding {}", l.path().display());
                eng.lock = Some(l);
                Ok(())
            }
            None => Err(Error::Conflict(format!(
                "another sk1ll process (a second daemon, or a CLI command working directly) is changing {}",
                eng.state.repo().display()
            ))),
        }
    }

    /// At startup: clone the repo if it isn't there yet, drop credentials an
    /// older version left in origin's URL, and bring the harnesses in line.
    pub(crate) fn startup(&self, eng: &mut Engine) -> Result<()> {
        if Self::idle(eng) {
            return Ok(());
        }
        let root = eng.state.repo().to_path_buf();
        // Git authenticates through the process environment (`prepare_process`).
        if !self.is_standalone()
            && let Err(e) = gitx::ensure_clone(&root, &eng.state.joined()?.repo_url(), &[])
        {
            self.set_error(format!("setting up {}: {e:#}", root.display()));
        }
        self.lock_repo(eng)?;
        if !self.is_standalone()
            && let Some(s) = self.snapshot()
            && let Err(e) = s.assignment.save(&self.cfg.assignment)
        {
            tracing::warn!("writing the assignment cache {}: {e}", self.cfg.assignment.display());
        }
        self.refresh(eng)
    }

    /// Plans and applies the sync, then recomputes the report. Conflicts are
    /// reported, never applied. Without an assignment, harnesses are left alone.
    ///
    /// This is the expensive operation (a full plan and a dozen git
    /// processes), so callers that know nothing it reads has changed since
    /// the last one skip it: see [`Daemon::pull_refreshed`], [`Daemon::nudge`]
    /// and [`Daemon::report_is_stale`].
    pub(crate) fn refresh(&self, eng: &mut Engine) -> Result<()> {
        if Self::idle(eng) {
            return Ok(());
        }
        if self.is_standalone() {
            self.reload_local_assignment()?;
        }
        tracing::debug!("refreshing the local state");
        eng.stale = true;
        let root = eng.state.repo().to_path_buf();
        if self.is_standalone() && !root.join(".git").exists() {
            self.update(|v| {
                v.repo_ready = false;
                v.has_origin = false;
                v.last_error = None;
            });
            self.roots
                .send_replace(vec![WatchRoot::new(root, true), WatchRoot::new(self.cfg.assignment.clone(), false)]);
            return Ok(());
        }
        if !root.join(".git").exists() {
            return Err(Error::Conflict(format!("{} is not set up yet (the server was unreachable)", root.display())));
        }
        self.lock_repo(eng)?;
        let machine = MachineConfig::load(&self.cfg.machine_config)?;
        let harnesses = machine.harnesses();
        let lock = eng.lock.as_ref().ok_or_else(|| Error::Conflict(format!("{} is not locked", root.display())))?;
        sync::recover(lock, &harnesses)?;
        Self::recover_imports(&root)?;
        let mut errors = Vec::new();
        let mut conflicts: Vec<ConflictRef> = Vec::new();
        let snapshot = self.snapshot();
        if let Some(snap) = &snapshot {
            let planned = Repo::open(&root).and_then(|repo| {
                let actions = sync::plan(&repo, &harnesses, &snap.assignment, PlanOptions::default())?;
                Ok((repo, actions))
            });
            match planned {
                Ok((repo, actions)) => {
                    for a in &actions {
                        if a.kind == ActionKind::Conflict {
                            for item in conflict_items(a) {
                                let c = ConflictRef { item, harness: a.harness.clone() };
                                if !conflicts.contains(&c) {
                                    conflicts.push(c);
                                }
                            }
                        } else if let Err(e) = sync::apply(&repo, a)
                            && !matches!(e, sk1ll_core::Error::Displaced { .. })
                        {
                            errors.push(format!("sync {}: {e}", a.dest.display()));
                        }
                    }
                }
                Err(e) => errors.push(format!("sync: {e}")),
            }
        }

        for entry in sync::displaced::changed()? {
            let c = ConflictRef { item: displaced_item(&entry), harness: entry.harness.clone() };
            if !conflicts.contains(&c) {
                conflicts.push(c);
            }
        }
        let repo = Repo::open_git_only(&root)?;
        let drafts = repo.draft()?.into_iter().map(|d| d.item).collect();
        let baseline = gitx::baseline(&root)?;
        let unpushed = gitx::unpushed_items(&root)?;
        let clashes = eng.blocked.clone().unwrap_or_default();
        let blocked = eng.blocked.is_some();
        let behind = blocked || eng.server_baseline.as_ref().is_some_and(|h| baseline.as_ref() != Some(h));
        let report = MachineReport {
            baseline,
            drafts,
            unpushed,
            conflicts,
            harnesses: harnesses.keys().cloned().collect(),
            blocked,
            clashes,
            behind,
        };
        let server_baseline = eng.server_baseline.clone();
        let awaiting = snapshot.is_none();
        self.update(|v| {
            v.repo_ready = true;
            v.has_origin = !self.is_standalone() || gitx::has_origin(&root).unwrap_or(false);
            v.report = report;
            v.server_baseline = server_baseline;
            v.awaiting_assignment = awaiting;
            if let Some(e) = errors.first() {
                v.last_error = Some(crate::auth::redact(e));
            }
        });
        for e in &errors {
            tracing::warn!("{}", crate::auth::redact(e));
        }

        let mut roots = vec![WatchRoot::new(root.canonicalize().unwrap_or(root), true)];
        if self.is_standalone() {
            roots.push(WatchRoot::new(self.cfg.assignment.clone(), false));
        }
        if let Some(dir) = self.cfg.machine_config.parent() {
            roots.push(WatchRoot::new(dir.to_path_buf(), false));
        }
        for h in harnesses.values() {
            roots.extend(h.skills_dir().map(|p| WatchRoot::new(p, true)));
            roots.extend(h.instructions_file().map(|p| WatchRoot::new(p, true)));
        }
        roots.sort();
        roots.dedup();
        self.roots.send_if_modified(|r| {
            let changed = *r != roots;
            *r = roots;
            changed
        });
        eng.stale = false;
        Ok(())
    }

    /// Whether the published report can't be relied on (the last refresh
    /// failed): whoever is about to act on it has to refresh first.
    pub(crate) fn report_is_stale(eng: &Engine) -> bool {
        eng.stale
    }

    /// Publishes the server's baseline and the one thing in the report that
    /// follows from it alone (`behind`), for when it is all that changed
    /// since the last successful refresh.
    fn publish_server_baseline(&self, eng: &Engine) {
        let server_baseline = eng.server_baseline.clone();
        let blocked = eng.blocked.is_some();
        self.update(|v| {
            v.report.behind =
                blocked || server_baseline.as_ref().is_some_and(|h| v.report.baseline.as_ref() != Some(h));
            v.server_baseline = server_baseline;
        });
    }

    /// What a pull or push can change among the things the report is made
    /// from: where `HEAD` and origin's branch are (the baseline, the unpushed
    /// items and the working tree a pull moves all follow from them) and
    /// whether the machine is Blocked.
    fn pull_state(eng: &Engine) -> Result<PullState> {
        let root = Self::root(eng);
        Ok(PullState { head: gitx::rev(root, "HEAD")?, remote: gitx::remote_head(root)?, blocked: eng.blocked.clone() })
    }

    /// After file changes (or the periodic reconciliation): refresh, and if
    /// Blocked and the draft changed, try the pull again.
    pub(crate) fn on_files_changed(&self, eng: &mut Engine) -> Result<()> {
        if Self::idle(eng) {
            return Ok(());
        }
        sync::displaced::prune()?;
        let before = self.view.borrow().report.drafts.clone();
        self.refresh(eng)?;
        if eng.blocked.is_some()
            && self.view.borrow().report.drafts != before
            && (self.is_connected() || self.is_standalone())
        {
            self.pull_refreshed(eng)?;
        }
        Ok(())
    }

    /// Fetches the server's baseline and moves onto it (draft carried along),
    /// pushes queued commits, and syncs.
    pub(crate) fn pull(&self, eng: &mut Engine) -> Result<PullResult> {
        if self.is_standalone() {
            self.ensure_managed(eng)?;
            self.require_origin()?;
            self.lock_repo(eng)?;
            let result = self.pull_inner(eng);
            self.refresh(eng)?;
            return result;
        }
        self.pull_with(eng, false)
    }

    /// A pull where the local state was refreshed just before (by the caller,
    /// or by the session on connecting): the sync after it is skipped if the
    /// pull moved nothing, since it would recompute the same report from the
    /// same inputs.
    fn pull_refreshed(&self, eng: &mut Engine) -> Result<PullResult> {
        if self.is_standalone() {
            return self.pull(eng);
        }
        self.pull_with(eng, true)
    }

    fn pull_with(&self, eng: &mut Engine, refreshed: bool) -> Result<PullResult> {
        self.ensure_managed(eng)?;
        let root = eng.state.repo().to_path_buf();
        if !root.join(".git").exists() {
            gitx::ensure_clone(&root, &eng.state.joined()?.repo_url(), &[])?;
        }
        self.lock_repo(eng)?;
        // `None`: refresh whatever happens.
        let before = if refreshed && !eng.stale { Self::pull_state(eng).ok() } else { None };
        let result = self.pull_inner(eng).and_then(|r| {
            if !matches!(r, PullResult::Blocked { .. }) {
                self.push_pending(eng)?;
            }
            Ok(r)
        });
        // A failed pull may have stopped half-way: that always refreshes.
        if result.is_ok() && before.is_some() && before == Self::pull_state(eng).ok() {
            self.publish_server_baseline(eng);
        } else {
            self.refresh(eng)?;
        }
        match &result {
            Ok(r) => {
                if let PullResult::Updated { to, .. } = r {
                    tracing::info!("baseline is now {to}");
                }
                if !matches!(r, PullResult::Blocked { .. }) {
                    self.update(|v| v.last_error = None);
                }
            }
            Err(e) => self.set_error(format!("pull: {e}")),
        }
        result
    }

    fn pull_inner(&self, eng: &mut Engine) -> Result<PullResult> {
        let root = eng.state.repo().to_path_buf();
        let first = match gitx::rev(&root, "HEAD")? {
            None => Some(gitx::adopt_first_baseline(&root)?),
            Some(_) => gitx::adopt_first_baseline_over_init(&root)?,
        };
        let result = if let Some(first) = first {
            eng.blocked = None;
            match first {
                gitx::FirstBaseline::Adopted(to) => PullResult::Updated { from: None, to },
                gitx::FirstBaseline::Empty => PullResult::UpToDate,
                gitx::FirstBaseline::Blocked { baseline, items } => {
                    eng.blocked = Some(items.clone());
                    PullResult::Blocked { baseline, items }
                }
            }
        } else {
            match Repo::open_git_only(&root)?.pull()? {
                PullOutcome::UpToDate => {
                    eng.blocked = None;
                    PullResult::UpToDate
                }
                PullOutcome::Updated { from, to } => {
                    eng.blocked = None;
                    PullResult::Updated { from: Some(from), to }
                }
                PullOutcome::Blocked { baseline, items } => {
                    tracing::warn!("blocked: baseline {baseline} clashes with the draft");
                    eng.blocked = Some(items.clone());
                    PullResult::Blocked { baseline, items }
                }
            }
        };
        if let Some(remote) = gitx::remote_head(&root)? {
            eng.server_baseline = Some(remote);
        }
        Ok(result)
    }

    /// Pushes committed commits the server doesn't have. A push the server rejected
    /// as not fast-forward is retried once after rebasing onto its baseline;
    /// any other failure (unreachable, timed out) is returned right away.
    fn push_pending(&self, eng: &mut Engine) -> Result<bool> {
        let root = eng.state.repo().to_path_buf();
        if !gitx::has_unpushed(&root)? {
            return Ok(false);
        }
        let repo = Repo::open_git_only(&root)?;
        if let Err(first) = repo.push() {
            if !is_rejected_push(&first) {
                return Err(first.into());
            }
            tracing::info!("push rejected ({first}); pulling first");
            if let PullResult::Blocked { items, .. } = self.pull_inner(eng)? {
                let names: Vec<String> = items.iter().map(ToString::to_string).collect();
                return Err(Error::Conflict(format!(
                    "can't push: the server's baseline clashes with {}; commit or restore them",
                    names.join(", ")
                )));
            }
            repo.push()?;
        }
        tracing::info!("pushed to the server");
        eng.server_baseline = gitx::remote_head(&root)?;
        Ok(true)
    }

    /// `POST /push`: pushes committed commits now.
    pub(crate) fn push(&self, eng: &mut Engine) -> Result<PushResult> {
        self.ensure_managed(eng)?;
        self.lock_repo(eng)?;
        if self.is_standalone() {
            self.require_origin()?;
            let repo = Repo::open_git_only(Self::root(eng))?;
            repo.push()?;
            eng.server_baseline = gitx::remote_head(Self::root(eng))?;
            self.refresh(eng)?;
            return Ok(PushResult { pushed: true });
        }
        let result = self.push_pending(eng);
        self.refresh(eng)?;
        match result {
            Ok(pushed) => Ok(PushResult { pushed }),
            Err(Error::Internal(e)) => {
                Err(Error::Unavailable(format!("push: {}", crate::auth::redact(&format!("{e:#}")))))
            }
            Err(e) => Err(e),
        }
    }

    /// On (re)connecting: push what's queued and pull.
    pub(crate) fn on_connected(&self, eng: &mut Engine) -> Result<()> {
        if Self::idle(eng) {
            return Ok(());
        }
        // The session refreshed before its first status, moments ago.
        self.pull_refreshed(eng).map(drop)
    }

    /// A reader: no engine mutex.
    pub(crate) fn commit_message(&self, items: &[String]) -> Result<CommitMessage> {
        let repo = Repo::open_git_only(self.repo_root())?;
        let draft = repo.draft()?;
        let items = items.iter().map(|i| ItemRef::resolve(i, &draft)).collect::<Result<Vec<_>, _>>()?;
        let message = default_commit_message(&items, self.ident.machine_id());
        Ok(CommitMessage { items, message })
    }

    pub(crate) fn commit(&self, eng: &mut Engine, req: CommitRequest) -> Result<CommitResponse> {
        self.ensure_managed(eng)?;
        self.lock_repo(eng)?;
        let root = eng.state.repo().to_path_buf();
        let repo = Repo::open_git_only(&root)?;
        let draft = repo.draft()?;
        let items = req.items.iter().map(|i| ItemRef::resolve(i, &draft)).collect::<Result<Vec<_>, _>>()?;
        if items.is_empty() {
            return Err(Error::BadRequest("name the draft items to commit".into()));
        }
        let id = eng.state.machine_id().to_string();
        let message =
            req.message.filter(|m| !m.trim().is_empty()).unwrap_or_else(|| default_commit_message(&items, &id));
        let author = match req.author {
            Some(a) => Some(a),
            None if !repo.has_git_identity()? => Some(machine_author(&id)),
            None => None,
        };
        let sha = repo.commit(&items, &message, author.as_deref())?;
        tracing::info!("committed {} as {sha}", items.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "));
        let (pushed, push_error) = if self.is_standalone() {
            (false, None)
        } else if self.is_connected() {
            match self.push_pending(eng) {
                Ok(_) => (true, None),
                Err(e) => (false, Some(crate::auth::redact(&e.to_string()))),
            }
        } else {
            (false, Some("offline: queued until the server is reachable".to_string()))
        };
        self.refresh(eng)?;
        if let Some(e) = &push_error {
            self.set_error(format!("push: {e}"));
        }
        if eng.blocked.is_some() && (self.is_connected() || self.is_standalone()) {
            let _ = self.pull_refreshed(eng);
        }
        Ok(CommitResponse { sha, items, message, pushed, push_error })
    }

    /// Restores draft items, then queues `draft_discarded` durably. The server
    /// session sends the updated status before it.
    pub(crate) fn restore(&self, eng: &mut Engine, req: RestoreRequest) -> Result<RestoreResponse> {
        self.ensure_managed(eng)?;
        self.lock_repo(eng)?;
        let repo = Repo::open_git_only(Self::root(eng))?;
        let draft = repo.draft()?;
        let before: BTreeSet<ItemRef> = draft.iter().map(|d| d.item.clone()).collect();
        match (req.all, req.items.is_empty()) {
            (true, true) => repo.restore_all()?,
            (false, false) => {
                let items = req.items.iter().map(|i| ItemRef::resolve(i, &draft)).collect::<Result<Vec<_>, _>>()?;
                repo.restore(&items)?;
            }
            (true, false) => {
                return Err(Error::BadRequest("pass either items or all, not both".into()));
            }
            (false, true) => {
                return Err(Error::BadRequest("name the draft items to restore, or pass all".into()));
            }
        }
        let after: BTreeSet<ItemRef> = repo.draft()?.into_iter().map(|d| d.item).collect();
        let discarded: Vec<ItemRef> = before.difference(&after).cloned().collect();
        let refreshed = self.refresh(eng);
        if !discarded.is_empty() {
            self.queue_discarded(discarded.clone());
        }
        refreshed?;
        if eng.blocked.is_some() && (self.is_connected() || self.is_standalone()) {
            let _ = self.pull_refreshed(eng);
        }
        Ok(RestoreResponse { discarded })
    }

    /// A read-only inventory. GIT_OPTIONAL_LOCKS=0 prevents status from refreshing
    /// the index, and no engine mutex or EngineLock is acquired here.
    pub(crate) fn items(root: &Path) -> Result<Vec<ItemView>> {
        use sk1ll_core::git::Git;
        let read = |args: &[&str]| Git::new(root).env("GIT_OPTIONAL_LOCKS", "0").args(args).run();
        let paths = |revision: Option<&str>| -> Result<Vec<String>> {
            let Some(revision) = revision else { return Ok(Vec::new()) };
            let bytes = read(&["ls-tree", "-r", "-z", "--name-only", revision])?;
            Ok(bytes
                .split(|b| *b == 0)
                .filter(|p| !p.is_empty())
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .collect())
        };
        let baseline = gitx::baseline(root)?;
        let head = gitx::rev(root, "HEAD")?;
        let baseline_paths = paths(baseline.as_deref())?;
        let head_paths = paths(head.as_deref())?;
        let mut inventory = BTreeMap::<ItemRef, ItemView>::new();
        let entry =
            |item: ItemRef| ItemView { item, in_baseline: false, draft: None, unpushed: false, description: None };
        for path in &baseline_paths {
            let item = ItemRef::for_path(Path::new(path));
            if matches!(item, ItemRef::Skill(_) | ItemRef::Rule(_)) {
                inventory.entry(item.clone()).or_insert_with(|| entry(item)).in_baseline = true;
            }
        }
        let status = read(&[
            "-c",
            "core.fsmonitor=false",
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
            "--ignore-submodules=all",
        ])?;
        let mut changed = BTreeMap::<ItemRef, Vec<String>>::new();
        for raw in status.split(|b| *b == 0).filter(|p| p.len() >= 4) {
            let path = String::from_utf8_lossy(&raw[3..]).into_owned();
            if sk1ll_core::source::is_staging_path(Path::new(&path)) {
                continue;
            }
            changed.entry(ItemRef::for_path(Path::new(&path))).or_default().push(path);
        }
        for (item, paths) in changed {
            let tracked = head_paths.iter().filter(|p| ItemRef::for_path(Path::new(p)) == item);
            let existed = tracked.clone().next().is_some();
            let exists = tracked.chain(paths.iter()).any(|p| root.join(p).symlink_metadata().is_ok());
            let change = match (existed, exists) {
                (_, false) => ChangeKind::Deleted,
                (true, true) => ChangeKind::Modified,
                (false, true) => ChangeKind::Added,
            };
            inventory.entry(item.clone()).or_insert_with(|| entry(item)).draft = Some(change);
        }
        for item in gitx::unpushed_items(root)? {
            inventory.entry(item.clone()).or_insert_with(|| entry(item)).unpushed = true;
        }
        // Descriptions of skills without a draft come from git: one
        // `cat-file --batch` for all of them, not a process per skill.
        let mut wanted = Vec::new();
        for value in inventory.values().filter(|value| value.draft.is_none()) {
            let revision = if value.unpushed { head.as_ref() } else { baseline.as_ref() };
            let (ItemRef::Skill(name), Some(rev)) = (&value.item, revision) else { continue };
            // The batch protocol is line-based: such a name can't be asked for.
            if !name.contains('\n') {
                wanted.extend(SKILL_DIRS.iter().map(|directory| format!("{rev}:{directory}/{name}/SKILL.md")));
            }
        }
        let mut blobs = BTreeMap::new();
        let cat_file = |mode: &str, objects: &[String]| {
            let input = objects.iter().flat_map(|object| [object.as_str(), "\n"]).collect::<String>().into_bytes();
            Git::new(root).env("GIT_OPTIONAL_LOCKS", "0").args(["cat-file", mode]).stdin(input).run()
        };
        // Sizes first, so git never streams us a blob over the cap.
        if !wanted.is_empty() {
            let sizes = cat_file("--batch-check", &wanted).unwrap_or_default();
            let mut sizes = sizes.split(|b| *b == b'\n').map(|line| small_blob(line, DESCRIPTION_CAP));
            wanted.retain(|_| sizes.next().unwrap_or(false));
        }
        if !wanted.is_empty() {
            let batch = cat_file("--batch", &wanted);
            let contents = batch.map(|out| parse_batch(&out, wanted.len(), DESCRIPTION_CAP)).unwrap_or_default();
            blobs.extend(wanted.into_iter().zip(contents).filter_map(|(object, bytes)| Some((object, bytes?))));
        }
        for value in inventory.values_mut() {
            let ItemRef::Skill(name) = &value.item else { continue };
            for directory in SKILL_DIRS {
                let path = format!("{directory}/{name}/SKILL.md");
                let text = if value.draft.is_some() {
                    use std::io::Read;
                    let mut text = String::new();
                    std::fs::File::open(root.join(&path))
                        .ok()
                        .and_then(|file| file.take(DESCRIPTION_CAP as u64).read_to_string(&mut text).ok())
                        .map(|_| text)
                } else {
                    let revision = if value.unpushed { head.as_ref() } else { baseline.as_ref() };
                    revision
                        .and_then(|rev| blobs.remove(&format!("{rev}:{path}")))
                        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                };
                if let Some(text) = text {
                    value.description = skill_description(&text);
                    break;
                }
            }
        }
        Ok(inventory.into_values().collect())
    }

    /// A reader: no engine mutex.
    pub(crate) fn draft(&self) -> Result<Vec<DraftEntry>> {
        let repo = Repo::open_git_only(self.repo_root())?;
        Ok(repo
            .draft()?
            .into_iter()
            .map(|d| DraftEntry {
                item: d.item,
                change: match d.change {
                    sk1ll_core::Change::Added => ChangeKind::Added,
                    sk1ll_core::Change::Modified => ChangeKind::Modified,
                    sk1ll_core::Change::Deleted => ChangeKind::Deleted,
                },
                paths: d.paths,
            })
            .collect())
    }

    /// A reader: no engine mutex.
    pub(crate) fn diff(&self, item: &str) -> Result<DiffView> {
        let repo = Repo::open_git_only(self.repo_root())?;
        let item = ItemRef::resolve(item, &repo.draft()?)?;
        let diff = repo.diff(&item)?;
        Ok(DiffView { item, diff })
    }

    /// The current plan; `force` plans as if overwriting. For engine
    /// operations: interrupted imports are put back first.
    fn plan(&self, eng: &Engine, force: bool) -> Result<(Repo, Vec<Action>)> {
        let Some(snap) = self.snapshot() else {
            return Err(Error::Conflict(
                "waiting for the server's assignment; the harnesses are left alone until it arrives".into(),
            ));
        };
        Self::recover_imports(Self::root(eng))?;
        self.plan_as_is(&snap, force)
    }

    /// The plan for the repo as it is, changing nothing: what a reader may
    /// run. An import that an interrupted update left set aside is not put
    /// back here (that needs the engine mutex and the repo's engine lock), so
    /// until the next refresh has done it this plan sees it as missing.
    fn plan_as_is(&self, snap: &AssignmentSnapshot, force: bool) -> Result<(Repo, Vec<Action>)> {
        let repo = Repo::open(self.repo_root())?;
        let harnesses = MachineConfig::load(&self.cfg.machine_config)?.harnesses();
        let actions = sync::plan(&repo, &harnesses, &snap.assignment, PlanOptions { force })?;
        Ok((repo, actions))
    }

    /// Puts back imports an interrupted (or failed, unrolled-back) update
    /// left set aside, before anything plans a sync: otherwise the sync would
    /// remove their harness entries. Failing to stops the sync. It changes
    /// the repo: only with the engine mutex and the repo's engine lock held.
    fn recover_imports(root: &Path) -> Result<()> {
        for name in sk1ll_core::source::recover_replaced(root)? {
            tracing::warn!("restored import {name}, which an interrupted update had set aside");
        }
        Ok(())
    }

    /// A reader: no engine mutex.
    pub(crate) fn recovered(&self) -> Result<Vec<ConflictView>> {
        Ok(sync::displaced::list()?.into_iter().map(displaced_view).collect())
    }

    /// A reader: no engine mutex, and so no recovery of interrupted imports
    /// (see [`Daemon::plan_as_is`]).
    pub(crate) fn conflicts(&self) -> Result<Vec<ConflictView>> {
        let mut result: Vec<_> = if let Some(snap) = self.snapshot() {
            let (_, actions) = self.plan_as_is(&snap, false)?;
            actions.iter().filter(|a| a.kind == ActionKind::Conflict).map(conflict_view).collect()
        } else {
            Vec::new()
        };
        for entry in sync::displaced::changed()? {
            result.push(displaced_view(entry));
        }
        Ok(result)
    }

    /// `POST /sync`: re-syncs now; `force` also overwrites what a forced plan
    /// may overwrite (hand edits inside managed blocks).
    pub(crate) fn sync(&self, eng: &mut Engine, force: bool) -> Result<SyncResult> {
        self.ensure_managed(eng)?;
        self.lock_repo(eng)?;
        if force {
            let (repo, actions) = self.plan(eng, true)?;
            for a in actions.iter().filter(|a| a.kind != ActionKind::Conflict) {
                sync::apply(&repo, a)?;
            }
        }
        self.refresh(eng)?;
        Ok(SyncResult { conflicts: self.conflicts()? })
    }

    pub(crate) fn resolve_displaced(&self, eng: &mut Engine, id: &str, adopt: bool) -> Result<()> {
        self.ensure_managed(eng)?;
        self.lock_repo(eng)?;
        let result = if adopt {
            sync::displaced::adopt(&Repo::open(Self::root(eng))?, id)
        } else {
            sync::displaced::discard(id)
        };
        self.refresh(eng)?;
        self.event(Event::Status);
        if adopt {
            self.event(Event::Draft);
        }
        result?;
        Ok(())
    }

    fn find_conflict<'a>(actions: &'a [Action], t: &ConflictTarget) -> Result<&'a Action> {
        actions.iter().find(|a| a.kind == ActionKind::Conflict && matches_target(a, t)).ok_or_else(|| {
            Error::NotFound(format!("no conflict for `{}` in `{}` (see GET /conflicts)", t.name, t.harness))
        })
    }

    /// Takes the harness's content into the draft.
    pub(crate) fn adopt(&self, eng: &mut Engine, t: ConflictTarget) -> Result<()> {
        self.ensure_managed(eng)?;
        self.lock_repo(eng)?;
        let repo = Repo::open(Self::root(eng))?;
        let harnesses = MachineConfig::load(&self.cfg.machine_config)?.harnesses();
        let unmanaged = sync::unmanaged::discover(&repo, &harnesses)?
            .iter()
            .any(|s| s.name == t.name && s.variants.iter().any(|v| v.locations.iter().any(|l| l.harness == t.harness)));
        let adopted = if unmanaged { None } else { sync::unmanaged::adopted(&repo, &harnesses, &t.harness, &t.name)? };
        if unmanaged || adopted.is_some() {
            if !self.is_standalone() && !self.is_connected() {
                return Err(Error::Unavailable("Adopt needs a connection to the Server".into()));
            }
            let adoption = match adopted {
                Some(adoption) => adoption,
                None => sync::unmanaged::select(&repo, &harnesses, &t.harness, &t.name)?,
            };
            if self.is_standalone() {
                sync::unmanaged::publish(&repo, &adoption)?;
                let changes = sk1ll_proto::AssignmentChanges {
                    changes: adoption
                        .locations
                        .iter()
                        .map(|l| sk1ll_proto::ToggleChange {
                            harness: l.harness.clone(),
                            kind: sk1ll_proto::ToggleKind::Skill,
                            name: Some(adoption.name.clone()),
                            enabled: Some(true),
                        })
                        .collect(),
                };
                self.set_local_assignment(eng, &changes)?;
                sync::unmanaged::complete(&repo, &harnesses, &adoption)?;
                self.refresh(eng)?;
                self.event(Event::Draft);
                return Ok(());
            }
            let created = repo.personal_dir().join(&adoption.name).symlink_metadata().is_err();
            // The Server rejects unknown items, so publish and report the draft
            // before enabling it. A crash leaves the original harness copies
            // untouched and an identical draft that Adopt can retry. Only a
            // newly created, unchanged repo copy may be rolled back on failure;
            // retry destinations and user edits must survive. Displacement runs
            // only after the synchronous, naturally idempotent explicit enable.
            sync::unmanaged::publish(&repo, &adoption)?;
            let result = (|| {
                self.refresh(eng)?;
                tokio::runtime::Handle::current()
                    .block_on(crate::server::assign_adoption(self, &adoption))
                    .map_err(Error::Internal)
            })();
            let update = match result {
                Ok(update) => update,
                Err(error) => {
                    if created && sync::unmanaged::rollback(&repo, &adoption)? {
                        self.queue_discarded(vec![ItemRef::Skill(adoption.name.clone())]);
                    }
                    self.refresh(eng)?;
                    self.event(Event::Draft);
                    return Err(error);
                }
            };
            self.apply_assignment(
                eng,
                AssignmentSnapshot { revision: update.revision, assignment: update.assignment },
            )?;
            let mut adoption = adoption;
            let snapshot = self.snapshot();
            adoption
                .locations
                .retain(|l| snapshot.as_ref().is_some_and(|s| s.assignment.skill_enabled(&l.harness, &adoption.name)));
            sync::unmanaged::complete(&repo, &harnesses, &adoption)?;
            self.refresh(eng)?;
            self.event(Event::Draft);
            self.event(Event::Fleet);
            return Ok(());
        }
        let (repo, actions) = self.plan(eng, false)?;
        sync::adopt(&repo, Self::find_conflict(&actions, &t)?)?;
        tracing::info!("adopted {} from {} into the draft", t.name, t.harness);
        self.refresh(eng)?;
        if eng.blocked.is_some() && (self.is_connected() || self.is_standalone()) {
            let _ = self.pull_refreshed(eng);
        }
        Ok(())
    }

    /// A reader: no engine mutex.
    pub(crate) fn skills(&self) -> Result<Vec<sk1ll_core::content::ContentEntry>> {
        let repo = Repo::open(self.repo_root())?;
        let harnesses = MachineConfig::load(&self.cfg.machine_config)?.harnesses();
        let baseline = gitx::baseline(repo.root())?;
        let snapshot = self.snapshot();
        let assignment = snapshot.as_ref().map(|s| s.assignment.clone()).unwrap_or_default();
        let mut entries = sk1ll_core::content::list(&repo, &harnesses, &assignment, baseline.as_deref())?;
        if snapshot.is_none() {
            for entry in &mut entries {
                entry.enabled_harnesses.clear();
            }
        }
        Ok(entries)
    }

    /// A reader: no engine mutex.
    pub(crate) fn skill_tree(
        &self,
        q: &sk1ll_core::content::ContentQuery,
    ) -> Result<Vec<sk1ll_core::content::FileEntry>> {
        Ok(sk1ll_core::content::tree(
            &Repo::open(self.repo_root())?,
            &MachineConfig::load(&self.cfg.machine_config)?.harnesses(),
            q,
        )?)
    }

    /// A reader: no engine mutex.
    pub(crate) fn skill_file(&self, q: &sk1ll_core::content::FileQuery) -> Result<sk1ll_core::content::FileContent> {
        Ok(sk1ll_core::content::file(
            &Repo::open(self.repo_root())?,
            &MachineConfig::load(&self.cfg.machine_config)?.harnesses(),
            q,
        )?)
    }

    /// Replaces the harness's content with what the draft says: a forced plan,
    /// applied for this one item only.
    pub(crate) fn overwrite(&self, eng: &mut Engine, t: ConflictTarget) -> Result<()> {
        self.ensure_managed(eng)?;
        self.lock_repo(eng)?;
        let (_, actions) = self.plan(eng, false)?;
        let conflict = Self::find_conflict(&actions, &t)?;
        let (repo, forced) = self.plan(eng, true)?;
        let fixes: Vec<&Action> =
            forced.iter().filter(|a| a.kind != ActionKind::Conflict && matches_target(a, &t)).collect();
        if fixes.is_empty() {
            let why = conflict.reason.as_deref().unwrap_or("not managed by sk1ll");
            return Err(Error::Conflict(format!(
                "`{}` in `{}` can't be overwritten ({why}); sk1ll never replaces content it doesn't manage",
                t.name, t.harness
            )));
        }
        for a in fixes {
            sync::apply(&repo, a)?;
        }
        tracing::info!("overwrote {} in {}", t.name, t.harness);
        self.refresh(eng)
    }

    /// A reader: no engine mutex.
    /// Every harness first-run setup can switch on or off.
    pub(crate) fn setup_harnesses(&self) -> Result<Vec<HarnessChoice>> {
        Ok(MachineConfig::load(&self.cfg.machine_config)?.choices())
    }

    /// Only before setup ends: dropping a harness later would strand what
    /// sync already placed there, and a joined machine registered its
    /// harnesses with the server. Takes the engine so it can't race Init or
    /// a join.
    pub(crate) fn set_setup_harness(&self, _eng: &mut Engine, name: &str, enabled: bool) -> Result<Vec<HarnessChoice>> {
        if !self.is_standalone() || self.repo_root().join(".git").exists() {
            return Err(Error::Conflict(format!(
                "harnesses can be chosen here only during first-run setup; edit {} instead",
                self.cfg.machine_config.display()
            )));
        }
        if !self.setup_harnesses()?.iter().any(|c| c.name == name) {
            return Err(Error::NotFound(format!("no harness `{name}`")));
        }
        MachineConfig::set_enabled(&self.cfg.machine_config, name, enabled)?;
        self.setup_harnesses()
    }

    pub(crate) fn harnesses(&self) -> Result<Vec<HarnessView>> {
        let machine = MachineConfig::load(&self.cfg.machine_config)?;
        Ok(machine
            .effective()
            .into_iter()
            .map(|(name, e)| HarnessView {
                name,
                harness: e.harness,
                origin: match e.origin {
                    HarnessOrigin::Detected => "detected",
                    HarnessOrigin::Override => "override",
                    HarnessOrigin::Local => "local",
                }
                .into(),
            })
            .collect())
    }

    // Import operations fetch in two or three steps, so a slow or hanging
    // remote never holds the engine mutex: check what can be checked under
    // it, fetch into the import cache and stage the checkout without it (core
    // serializes fetches per cache repo; checkout may run smudge filters that
    // fetch), then take it again to re-check against the repo as it is now
    // and install the staged content: a rename, with no git or network.

    /// Re-vendors imports into the draft: `name`, or all of them.
    pub(crate) async fn update_imports(self: &Arc<Self>, req: UpdateImportsRequest) -> Result<Vec<ImportUpdated>> {
        let UpdateImportsRequest { name, force } = req;
        let (root, todo) = self
            .blocking(move |d, eng| {
                d.ensure_managed(eng)?;
                let todo = Repo::open(Self::root(eng))?.update_targets(name.as_deref(), force)?;
                Ok((Self::root(eng).to_path_buf(), todo))
            })
            .await?;
        let fetched = unlocked(move || imports::fetch_updates(&root, todo)).await?;
        self.blocking(move |d, eng| {
            d.ensure_managed(eng)?;
            d.lock_repo(eng)?;
            let reports = Repo::open(Self::root(eng))?.apply_updates(fetched, force);
            d.refresh(eng)?;
            Ok(reports?.into_iter().map(import_updated).collect())
        })
        .await
    }

    /// Re-vendors one import into the draft.
    pub(crate) async fn update_import(self: &Arc<Self>, name: String) -> Result<ImportUpdated> {
        let req = UpdateImportsRequest { name: Some(name), force: false };
        self.update_imports(req).await?.into_iter().next().context("no update report").map_err(Error::Internal)
    }

    /// What a link leads to; only the import cache is written.
    pub(crate) async fn resolve_import(self: &Arc<Self>, req: ResolveImportRequest) -> Result<ResolvedSource> {
        let root = self.blocking(|d, eng| d.import_root(eng)).await?;
        let fetched = unlocked(move || imports::fetch_link(&root, &req.link, given(&req.path), None, None)).await?;
        // `taken` reflects the repo after the fetch, not before it.
        let r = self
            .blocking(move |d, eng| {
                d.ensure_managed(eng)?;
                Ok(Repo::open(Self::root(eng))?.with_taken(fetched.resolved))
            })
            .await?;
        let candidates = r
            .candidates
            .into_iter()
            .map(|c| SkillCandidate {
                path: c.path,
                default_name: c.default_name,
                description: c.description,
                taken: c.taken,
            })
            .collect();
        Ok(ResolvedSource { source: r.source, git_ref: r.git_ref, commit: r.commit, candidates })
    }

    /// Adds an import, from a link or plain source, to the draft.
    pub(crate) async fn add_import(self: &Arc<Self>, req: AddImportRequest) -> Result<ImportUpdated> {
        let root = self.blocking(|d, eng| d.import_root(eng)).await?;
        let AddImportRequest { name, source, path, git_ref, commit } = req;
        // Staging runs checkout (and any smudge filter) here too, outside the mutex.
        let fetched = unlocked(move || {
            let git_ref = git_ref.as_deref().and_then(given);
            let mut fetched = imports::fetch_link(&root, &source, path.as_deref(), git_ref, commit.as_deref())?;
            fetched.stage()?;
            Ok(fetched)
        })
        .await?;
        self.blocking(move |d, eng| {
            d.ensure_managed(eng)?;
            d.lock_repo(eng)?;
            let added = Repo::open(Self::root(eng))?.add_fetched(given(&name), fetched);
            d.refresh(eng)?;
            Ok(import_updated(added?))
        })
        .await
    }

    /// The repo an import operation fetches for, if the daemon manages it.
    fn import_root(&self, eng: &Engine) -> Result<PathBuf> {
        self.ensure_managed(eng)?;
        Ok(Self::root(eng).to_path_buf())
    }

    /// Removes an import (a draft change).
    pub(crate) fn remove_import(&self, eng: &mut Engine, name: &str) -> Result<()> {
        self.ensure_managed(eng)?;
        self.lock_repo(eng)?;
        let removed = Repo::open(Self::root(eng))?.remove_import(name);
        self.refresh(eng)?;
        Ok(removed?)
    }

    /// Turns an import into a personal skill (a draft change).
    pub(crate) fn fork(&self, eng: &mut Engine, name: &str) -> Result<()> {
        self.ensure_managed(eng)?;
        self.lock_repo(eng)?;
        let forked = Repo::open(Self::root(eng))?.fork(name);
        self.refresh(eng)?;
        Ok(forked?)
    }

    /// Stores `snap` and syncs, unless it is older than what was applied. With
    /// no stored snapshot, any snapshot is taken (also one of the same
    /// revision as before), since the harnesses wait for it.
    pub(crate) fn apply_assignment(&self, eng: &mut Engine, snap: AssignmentSnapshot) -> Result<bool> {
        if Self::idle(eng) {
            return Ok(false);
        }
        if let Some(have) = self.snapshot()
            && !newer(have.revision, snap.revision)
        {
            // The same revision arrives on every connect; only an older one is news.
            if snap.revision < have.revision {
                tracing::info!("ignoring stale assignment revision {} (have {})", snap.revision, have.revision);
            }
            return Ok(false);
        }
        state::save_snapshot(&self.cfg.snapshot_path(), &snap)?;
        if let Err(e) = snap.assignment.save(&self.cfg.assignment) {
            tracing::warn!("writing the assignment cache {}: {e}", self.cfg.assignment.display());
        }
        let revision = snap.revision;
        *self.snapshot.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(snap));
        tracing::info!("applied assignment revision {revision}");
        self.update(|v| {
            v.assignment_revision = revision;
            v.awaiting_assignment = false;
        });
        self.event(Event::Assignment { revision });
        self.refresh(eng)?;
        Ok(true)
    }

    /// The server's baseline is now `baseline`: pull unless already on it. A
    /// nudge for the baseline the machine is on (the server sends one on every
    /// connect) says nothing about the local state, so it costs no refresh.
    pub(crate) fn nudge(&self, eng: &mut Engine, baseline: String) -> Result<()> {
        if Self::idle(eng) {
            return Ok(());
        }
        eng.server_baseline = Some(baseline.clone());
        self.update(|v| v.server_baseline = Some(baseline.clone()));
        self.event(Event::Fleet);
        let current = gitx::baseline(Self::root(eng)).unwrap_or(None);
        if current.as_deref() != Some(baseline.as_str()) || eng.blocked.is_some() {
            // Not `pull_refreshed`: the last refresh may be long ago.
            self.pull(eng).map(drop)
        } else if eng.stale {
            self.refresh(eng)
        } else {
            self.publish_server_baseline(eng);
            Ok(())
        }
    }

    /// Stops managing this machine. Harness files are left as they are.
    pub(crate) fn revoke(&self, eng: &mut Engine, why: &str) -> Result<()> {
        if eng.state.revoked() {
            return Ok(());
        }
        tracing::warn!("revoked: {why}; no longer managing this machine (its files are left as they are)");
        let Mode::Joined(state) = &mut eng.state else {
            return Err(Error::Conflict("a standalone machine cannot be revoked".into()));
        };
        state.revoked = true;
        state.save(&self.cfg.state)?;
        self.revoked.store(true, Ordering::SeqCst);
        self.connected.store(false, Ordering::SeqCst);
        self.roots.send_replace(Vec::new());
        self.update(|v| {
            v.revoked = true;
            v.connection = Connection::Revoked;
            v.last_error = Some(why.to_string());
        });
        self.event(Event::Fleet);
        Ok(())
    }

    /// Whether `POST /join` may join again: the machine was removed, or the
    /// server rejects its token.
    pub(crate) fn can_rejoin(&self) -> bool {
        self.is_standalone() || self.is_revoked() || self.view().connection == Connection::AuthRejected
    }

    /// Validate before registration and again under the engine lock afterwards.
    pub(crate) fn check_standalone_join_request(&self, eng: &Engine, repo: &Path) -> Result<()> {
        if eng.frozen {
            return Err(Error::Conflict("already joining; the daemon is restarting".into()));
        }
        let requested = std::path::absolute(repo).context("resolving join repo")?;
        let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        if canon(&requested) != canon(self.repo_root()) {
            return Err(Error::Conflict(format!(
                "join must use the daemon's repo at {}; restart with SK1LL_REPO to select another directory",
                self.repo_root().display()
            )));
        }
        Ok(())
    }

    pub(crate) fn join_standalone(&self, eng: &mut Engine, reg: Registered) -> Result<Joined> {
        self.check_standalone_join_request(eng, &reg.state().repo)?;
        self.lock_repo(eng)?;
        let mut prepared = reg.prepare_standalone_repo().map_err(|e| Error::Conflict(format!("{e:#}")))?;
        let activated = prepared.publish().and_then(|()| reg.persist_standalone(&self.cfg));
        if let Err(error) = activated {
            if let Err(recovery) = prepared.rollback() {
                // If the replacement is still live, retain its lock. Freeze
                // mutation when concurrent content prevents a safe restoration.
                if let Some(lock) = prepared.take_live_lock() {
                    eng.lock = Some(lock);
                }
                eng.frozen = true;
                return Err(Error::Internal(anyhow::anyhow!("{error:#}; rollback failed: {recovery:#}")));
            }
            return Err(Error::Internal(error));
        }
        if let Some(lock) = prepared.take_live_lock() {
            eng.lock = Some(lock);
        }
        let joined = prepared.joined;
        eng.frozen = true;
        Ok(joined)
    }

    /// Second half of `POST /join`: stores the new credentials and snapshot,
    /// sets up the repo with them, and freezes the engine until the restart
    /// ([`Daemon::restart`]) that brings them into effect. The outer error
    /// means nothing was stored; the inner one that the repo setup failed (the
    /// restarted daemon retries it).
    pub(crate) fn rejoin(&self, eng: &mut Engine, reg: Registered) -> Result<Result<Joined>> {
        if eng.frozen {
            return Err(Error::Conflict("already joined again; the daemon is restarting".into()));
        }
        reg.persist(&self.cfg)?;
        eng.frozen = true;
        tracing::info!("joined again as `{}`", reg.state().machine_id);
        Ok(reg.set_up_repo().map_err(Error::Internal))
    }

    /// Replaces this process with a fresh copy of itself (same PID, same
    /// arguments) once the engine is idle. Git's credentials live in the
    /// process environment, set before any thread starts, so new credentials
    /// take effect only in a new process image. Exits if that fails, so a
    /// service manager restarts it.
    pub(crate) fn restart(&self) -> ! {
        use std::os::unix::process::CommandExt;
        let _engine = lock(&self.engine);
        tracing::info!("restarting to use the new credentials");
        let _ = std::fs::remove_file(&self.cfg.socket);
        let err = match std::env::current_exe() {
            Ok(exe) => std::process::Command::new(exe).args(std::env::args_os().skip(1)).exec(),
            Err(e) => e,
        };
        tracing::error!("restarting failed ({err}); exiting so that it is started again");
        std::process::exit(1)
    }
}

/// Whether a snapshot at `incoming` replaces one at `applied`. Revision 0
/// (unknown, from an older server) is only taken while nothing newer was seen.
pub(crate) fn newer(applied: u64, incoming: u64) -> bool {
    incoming > applied || (incoming == 0 && applied == 0)
}

/// Whether a push failed because the server has commits we don't (worth a pull
/// and one retry), as opposed to the server being unreachable.
fn is_rejected_push(e: &sk1ll_core::Error) -> bool {
    let text = e.to_string();
    ["[rejected]", "non-fast-forward", "fetch first", "failed to push some refs"].iter().any(|m| text.contains(m))
        && !text.contains("unable to access")
}

/// Runs `f` (an import's network phase) on a blocking thread without the
/// engine mutex.
async fn unlocked<T: Send + 'static>(f: impl FnOnce() -> sk1ll_core::Result<T> + Send + 'static) -> Result<T> {
    let out = tokio::task::spawn_blocking(f).await;
    Ok(out.map_err(|e| Error::Internal(anyhow::anyhow!("import fetch task failed: {e}")))??)
}

/// An optional request field: empty means not given.
fn given(s: &str) -> Option<&str> {
    Some(s).filter(|s| !s.is_empty())
}

fn import_updated(r: sk1ll_core::UpdateReport) -> ImportUpdated {
    ImportUpdated { name: r.name, old_commit: r.old_commit, new_commit: r.new_commit, changed: r.changed }
}

/// Where a skill's files live in the repo, in the order they are looked up.
const SKILL_DIRS: [&str; 2] = ["personal", "imported"];

/// A `SKILL.md` larger than this is not read for its description.
const DESCRIPTION_CAP: usize = 16384;

/// Whether a `git cat-file --batch-check` line (`<oid> <type> <size>`)
/// describes a blob of at most `cap` bytes.
fn small_blob(line: &[u8], cap: usize) -> bool {
    let line = String::from_utf8_lossy(line);
    let mut fields = line.rsplitn(3, ' ');
    let (size, kind) = (fields.next(), fields.next());
    kind == Some("blob") && size.and_then(|size| size.parse::<usize>().ok()).is_some_and(|size| size <= cap)
}

/// Reads `git cat-file --batch` output for `count` requested objects, in
/// request order: the content of each blob of at most `cap` bytes, `None` for
/// anything else (missing, not a blob, larger). Content is raw bytes, so it is
/// taken by the size in its header, never by looking for a line end; output
/// that ends early or doesn't parse leaves the remaining objects `None`.
fn parse_batch(mut out: &[u8], count: usize, cap: usize) -> Vec<Option<Vec<u8>>> {
    let mut contents = vec![None; count];
    for content in &mut contents {
        let Some(end) = out.iter().position(|b| *b == b'\n') else { break };
        let header = String::from_utf8_lossy(&out[..end]);
        out = &out[end + 1..];
        // `<oid> <type> <size>`; anything else (`<name> missing`, where the
        // name may itself contain spaces) has no content after it.
        let mut fields = header.rsplitn(3, ' ');
        let (size, kind, oid) = (fields.next(), fields.next(), fields.next());
        let object = matches!(kind, Some("blob" | "tree" | "commit" | "tag"))
            && oid.is_some_and(|oid| !oid.is_empty() && oid.bytes().all(|b| b.is_ascii_hexdigit()));
        let Some(size) = size.filter(|_| object).and_then(|size| size.parse::<usize>().ok()) else { continue };
        if out.len() <= size || out[size] != b'\n' {
            break;
        }
        if kind == Some("blob") && size <= cap {
            *content = Some(out[..size].to_vec());
        }
        out = &out[size + 1..];
    }
    contents
}

fn skill_description(text: &str) -> Option<String> {
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    lines
        .take_while(|line| line.trim() != "---")
        .find_map(|line| line.strip_prefix("description:").map(str::trim))
        .filter(|description| !description.is_empty() && !matches!(*description, "|" | ">"))
        .map(|description| description.trim_matches(['\"', '\'']).to_string())
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// The items a conflict action is about.
fn conflict_items(a: &Action) -> Vec<ItemRef> {
    match &a.item {
        Item::Skill { name, .. } => vec![ItemRef::Skill(name.clone())],
        Item::Rule { file, .. } => vec![ItemRef::Rule(file.strip_suffix(".md").unwrap_or(file).to_string())],
        Item::Instructions { rules, .. } if !rules.is_empty() => {
            rules.iter().map(|f| ItemRef::Rule(f.clone())).collect()
        }
        Item::Instructions { .. } => vec![ItemRef::Other(a.dest.display().to_string())],
    }
}

fn displaced_view(entry: sync::displaced::Entry) -> ConflictView {
    let item = displaced_item(&entry);
    ConflictView {
        kind: Some("displaced".into()),
        id: Some(entry.id),
        item: Some(item.clone()),
        name: entry.dest.file_name().unwrap_or_default().to_string_lossy().into_owned(),
        items: vec![item],
        harness: entry.harness,
        dest: entry.dest,
        path: Some(entry.path),
        displaced_at: Some(entry.displaced_at),
        detected_at: None,
        reason: None,
        adoptable: true,
    }
}

fn displaced_item(entry: &sync::displaced::Entry) -> ItemRef {
    let name = entry.dest.file_name().unwrap_or_default().to_string_lossy();
    match entry.kind {
        sync::displaced::Kind::Skill => ItemRef::Skill(name.into_owned()),
        sync::displaced::Kind::Rule => ItemRef::Rule(name.strip_suffix(".md").unwrap_or(&name).to_string()),
        sync::displaced::Kind::Instructions => ItemRef::Other(entry.dest.display().to_string()),
    }
}

fn conflict_view(a: &Action) -> ConflictView {
    let (name, adoptable) = match &a.item {
        Item::Skill { name, .. } => (name.clone(), true),
        Item::Rule { file, .. } => (file.strip_suffix(".md").unwrap_or(file).to_string(), true),
        Item::Instructions { .. } => (BLOCK_NAME.to_string(), false),
    };
    ConflictView {
        kind: None,
        id: None,
        item: None,
        path: None,
        displaced_at: None,
        detected_at: modified_at(&a.dest),
        harness: a.harness.clone(),
        name,
        items: conflict_items(a),
        dest: a.dest.clone(),
        reason: a.reason.clone(),
        adoptable,
    }
}

/// Newest mtime of `path` and everything under it (directories too, so a
/// deleted file counts), without following symlinks; the management marker
/// and `.git` are skipped. Bounded, so a huge tree can't stall `/conflicts`.
fn modified_at(path: &Path) -> Option<u64> {
    let seconds =
        |m: &std::fs::Metadata| m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs());
    let meta = std::fs::symlink_metadata(path).ok()?;
    let (mut newest, mut stack, mut budget) = (seconds(&meta), Vec::new(), 10_000usize);
    if meta.is_dir() {
        stack.push(path.to_path_buf());
    }
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).ok()?.flatten() {
            budget = budget.checked_sub(1)?;
            if entry.file_name() == sync::MARKER || entry.file_name() == ".git" {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            newest = newest.max(seconds(&meta));
            if meta.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    newest
}

fn matches_target(a: &Action, t: &ConflictTarget) -> bool {
    a.harness == t.harness
        && match &a.item {
            Item::Skill { name, .. } => *name == t.name,
            Item::Rule { file, .. } => *file == t.name || *file == format!("{}.md", t.name),
            Item::Instructions { rules, .. } => t.name == BLOCK_NAME || rules.contains(&t.name),
        }
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Daemon {
    pub(crate) fn create_item(&self, eng: &mut Engine, req: crate::client::CreateItemRequest) -> Result<()> {
        self.ensure_managed(eng)?;
        self.lock_repo(eng)?;
        let repo = Repo::open(Self::root(eng))?;
        match (&req.bundled, req.kind.as_str()) {
            (Some(bundled), "skill") => repo.add_bundled(bundled, &req.name)?,
            (Some(_), _) => return Err(Error::BadRequest("only skills are bundled".into())),
            (None, _) => repo.create_item(&req.kind, &req.name, req.description.as_deref().unwrap_or(""))?,
        }
        self.refresh(eng)?;
        Ok(())
    }
    /// Toggles stay until a push drops them (`item_removed`), so Restore brings the item back as it was.
    pub(crate) fn delete_item(&self, eng: &mut Engine, req: crate::client::DeleteItemRequest) -> Result<()> {
        self.ensure_managed(eng)?;
        self.lock_repo(eng)?;
        let deleted = Repo::open(Self::root(eng))?.delete_item(&req.kind, &req.name);
        self.refresh(eng)?;
        Ok(deleted?)
    }
    /// Renames in the Draft, then copies this machine's explicit toggles for the
    /// old name to the new one. The old name's toggles stay until a push drops
    /// them, so restoring both Draft items undoes the rename.
    pub(crate) fn rename_item(
        &self,
        eng: &mut Engine,
        req: crate::client::RenameItemRequest,
    ) -> Result<crate::client::RenameItemResponse> {
        use sk1ll_proto::{AssignmentChanges, ToggleChange, ToggleKind};
        self.ensure_managed(eng)?;
        self.lock_repo(eng)?;
        let renamed = Repo::open(Self::root(eng))?.rename_item(&req.kind, &req.name, &req.to);
        self.refresh(eng)?;
        renamed?;
        let (item, kind) = if req.kind == "skill" {
            (ItemRef::Skill(req.to.clone()), ToggleKind::Skill)
        } else {
            (ItemRef::Rule(req.to.clone()), ToggleKind::Rule)
        };
        let snapshot = self.snapshot();
        let changes: Vec<ToggleChange> = snapshot
            .iter()
            .flat_map(|s| &s.assignment.harnesses)
            .filter_map(|(harness, h)| {
                let toggles = if kind == ToggleKind::Skill { &h.skills } else { &h.rules };
                toggles.get(&req.name).map(|&enabled| ToggleChange {
                    harness: harness.clone(),
                    kind,
                    name: Some(req.to.clone()),
                    enabled: Some(enabled),
                })
            })
            .collect();
        let mut response =
            crate::client::RenameItemResponse { item: item.clone(), migrated: Vec::new(), assignment_error: None };
        if changes.is_empty() {
            return Ok(response);
        }
        let changes = AssignmentChanges { changes };
        let result = if self.is_standalone() {
            self.set_local_assignment(eng, &changes).map_err(|e| anyhow::anyhow!(e))
        } else if self.is_connected() {
            tokio::runtime::Handle::current()
                .block_on(crate::server::assign_draft_item(self, &item, &changes, "Rename"))
        } else {
            Err(anyhow::anyhow!("offline: the Server is unreachable"))
        };
        match result {
            Ok(update) => {
                if !self.is_standalone() {
                    self.apply_assignment(
                        eng,
                        AssignmentSnapshot { revision: update.revision, assignment: update.assignment },
                    )?;
                }
                response.migrated = changes.changes.into_iter().map(|c| c.harness).collect();
                self.event(Event::Fleet);
            }
            Err(error) => {
                let message = crate::auth::redact(&format!("{error:#}"));
                tracing::warn!("renamed {} to {item}, but its Assignment was not carried over: {message}", req.name);
                response.assignment_error = Some(message);
            }
        }
        Ok(response)
    }
    pub(crate) async fn local_imports(self: &Arc<Self>, check: bool) -> Result<crate::client::ImportsView> {
        let (root, entries) = self
            .reading(|d| {
                let repo = Repo::open(d.repo_root())?;
                Ok((
                    repo.root().to_path_buf(),
                    repo.manifest
                        .import
                        .iter()
                        .map(|(name, i)| crate::client::ImportEntry {
                            name: name.clone(),
                            source: i.source.clone(),
                            git_ref: i.git_ref.clone(),
                            path: i.path.clone(),
                            locked: repo.lock.skills.get(name).map(|l| l.commit.clone()),
                        })
                        .collect::<Vec<_>>(),
                ))
            })
            .await?;
        tokio::task::spawn_blocking(move || {
            static CACHE: std::sync::OnceLock<
                std::sync::Mutex<std::collections::BTreeMap<PathBuf, crate::client::ImportsView>>,
            > = std::sync::OnceLock::new();
            let cache = CACHE.get_or_init(Default::default);
            let previous = cache.lock().unwrap().get(&root).cloned().unwrap_or_default();
            let mut view = crate::client::ImportsView { imports: entries, ..previous };
            if check {
                view.updates.clear();
                view.errors.clear();
                // This machine checks every import itself.
                view.skipped.clear();
            }
            if check {
                for i in &view.imports {
                    match sk1ll_core::imports::fetch_link(&root, &i.source, Some(&i.path), Some(&i.git_ref), None) {
                        Ok(f) => {
                            if let Some(locked) = &i.locked
                                && locked != &f.resolved.commit
                            {
                                view.updates.push(sk1ll_proto::ImportUpdate {
                                    name: i.name.clone(),
                                    source: i.source.clone(),
                                    git_ref: i.git_ref.clone(),
                                    locked: locked.clone(),
                                    upstream: f.resolved.commit,
                                });
                            }
                        }
                        Err(e) => {
                            view.errors.push(sk1ll_proto::ImportError { name: i.name.clone(), message: e.to_string() })
                        }
                    }
                }
                view.checked_at = Some(
                    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()
                        as i64,
                );
            }
            view.updates
                .retain(|u| view.imports.iter().any(|i| i.name == u.name && i.locked.as_ref() == Some(&u.locked)));
            view.errors.retain(|e| view.imports.iter().any(|i| i.name == e.name));
            cache.lock().unwrap().insert(root, view.clone());
            Ok(view)
        })
        .await
        .map_err(|e| Error::Internal(e.into()))?
    }
    /// A reader: no engine mutex.
    pub(crate) fn conflict_diff(&self, req: &crate::client::ConflictDiffRequest) -> Result<DiffView> {
        let view = if let Some(id) = &req.id {
            self.recovered()?.into_iter().find(|v| v.id.as_ref() == Some(id))
        } else {
            self.conflicts()?
                .into_iter()
                .find(|v| Some(&v.harness) == req.harness.as_ref() && Some(&v.name) == req.name.as_ref())
        }
        .ok_or_else(|| Error::NotFound("Conflict no longer exists".into()))?;
        let item = if view.name == BLOCK_NAME { Some(ItemRef::Other(view.name.clone())) } else { None }
            .or(view.item.clone())
            .or_else(|| view.items.first().cloned())
            .unwrap_or(ItemRef::Other(view.name.clone()));
        match &item {
            ItemRef::Skill(name) | ItemRef::Rule(name) => imports::validate_name(name)?,
            _ => {}
        }
        let repo = Repo::open(self.repo_root())?;
        let snapshot = self.snapshot();
        let temp = tempfile::tempdir().map_err(anyhow::Error::from)?;
        let mut right = match &item {
            ItemRef::Skill(name) => {
                let p = repo.root().join("personal").join(name);
                if p.exists() { p } else { repo.root().join("imported").join(name) }
            }
            ItemRef::Rule(name) => repo.root().join("instructions").join(format!("{name}.md")),
            _ => {
                let harnesses = MachineConfig::load(&self.cfg.machine_config)?.harnesses();
                let assignment =
                    &snapshot.as_ref().ok_or_else(|| Error::Conflict("Awaiting Assignment".into()))?.assignment;
                let files = sync::render_instructions(&repo, &harnesses, assignment, &view.harness)?;
                let path = temp.path().join("Draft");
                std::fs::write(&path, files.into_iter().map(|f| f.content).collect::<Vec<_>>().join("\n"))
                    .map_err(anyhow::Error::from)?;
                path
            }
        };
        let mut left = view.path.unwrap_or(view.dest);
        if view.name == BLOCK_NAME && matches!(item, ItemRef::Other(_)) {
            let text = std::fs::read_to_string(&left).map_err(anyhow::Error::from)?;
            let content = sk1ll_core::instructions::managed_content(&text, &left)?;
            left = temp.path().join("Harness");
            std::fs::write(&left, content).map_err(anyhow::Error::from)?;
        } else if left.is_dir() {
            // Diff only authored files: the harness copy carries the management marker.
            let copy = temp.path().join("Harness");
            copy_authored(&left, &copy).map_err(anyhow::Error::from)?;
            left = copy;
        }
        if !right.exists() {
            right = temp.path().join("Draft");
            if left.is_dir() {
                std::fs::create_dir(&right).map_err(anyhow::Error::from)?;
            } else {
                std::fs::write(&right, "").map_err(anyhow::Error::from)?;
            }
        }
        let output = std::process::Command::new("git")
            .args(["diff", "--no-index", "--no-ext-diff", "--no-textconv", "--"])
            .arg(&left)
            .arg(&right)
            .output()
            .map_err(anyhow::Error::from)?;
        if !matches!(output.status.code(), Some(0 | 1)) {
            return Err(Error::BadRequest(String::from_utf8_lossy(&output.stderr).into_owned()));
        }
        Ok(DiffView { item, diff: String::from_utf8_lossy(&output.stdout).into_owned() })
    }
}

fn copy_authored(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == sync::MARKER || name == ".git" {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_authored(&entry.path(), &to.join(&name))?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), to.join(&name))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_reads_applied_baseline_and_never_refreshes_the_index() {
        use sk1ll_core::git::Git;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = |args: &[&str]| {
            Git::new(root)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .run()
                .unwrap()
        };
        let write = |path: &str, text: &str| {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "test"]);
        git(&["config", "user.email", "test@example.invalid"]);
        write("personal/base/SKILL.md", "---\ndescription: Baseline description\n---\nbase\n");
        write("instructions/git.md", "baseline rule\n");
        write("imported/vendored/SKILL.md", "---\ndescription: Imported description\n---\n");
        write("personal/my skill/SKILL.md", "---\ndescription: 'Spaced'\n---\n\x00\nbody\n");
        write("personal/huge/SKILL.md", &format!("---\ndescription: Too big\n---\n{}", "x".repeat(DESCRIPTION_CAP)));
        write("personal/plain/SKILL.md", "no front matter\n");
        git(&["add", "."]);
        git(&["commit", "-qm", "baseline"]);
        let baseline = String::from_utf8(git(&["rev-parse", "HEAD"])).unwrap();
        git(&["update-ref", "refs/remotes/origin/main", baseline.trim()]);
        write("personal/queued/SKILL.md", "---\ndescription: Queued description\n---\n");
        git(&["add", "."]);
        git(&["commit", "-qm", "unpushed"]);
        write("personal/base/SKILL.md", "---\ndescription: Draft description\n---\nedited\n");
        write("personal/local/SKILL.md", "new\n");
        std::fs::remove_file(root.join("instructions/git.md")).unwrap();
        let index_before = std::fs::read(root.join(".git/index")).unwrap();
        let head_before = git(&["rev-parse", "HEAD"]);
        let items = Daemon::items(root).unwrap();
        let find = |name: &str| items.iter().find(|entry| entry.item.to_string() == name).unwrap();
        assert!(find("skill:base").in_baseline);
        assert_eq!(find("skill:base").draft, Some(ChangeKind::Modified));
        assert_eq!(find("skill:base").description.as_deref(), Some("Draft description"));
        // Descriptions of items without a draft are read from git, in one batch.
        assert_eq!(find("skill:vendored").description.as_deref(), Some("Imported description"));
        assert_eq!(find("skill:my skill").description.as_deref(), Some("Spaced"));
        assert_eq!(find("skill:queued").description.as_deref(), Some("Queued description"));
        assert_eq!(find("skill:huge").description, None, "over the cap: not read");
        assert_eq!(find("skill:plain").description, None);
        assert_eq!(find("skill:local").description, None);
        assert!(find("rule:git").in_baseline, "deleted Draft item still comes from the git tree");
        assert_eq!(find("rule:git").draft, Some(ChangeKind::Deleted));
        assert!(!find("skill:queued").in_baseline);
        assert!(find("skill:queued").unpushed);
        assert!(!find("skill:local").in_baseline);
        assert_eq!(find("skill:local").draft, Some(ChangeKind::Added));
        assert_eq!(std::fs::read(root.join(".git/index")).unwrap(), index_before);
        assert_eq!(git(&["rev-parse", "HEAD"]), head_before);

        // A fetched but clashing Baseline has not been applied locally.
        git(&["checkout", "-qb", "remote", baseline.trim()]);
        write("instructions/remote.md", "remote rule\n");
        git(&["add", "instructions/remote.md"]);
        git(&["commit", "-qm", "remote"]);
        let remote = String::from_utf8(git(&["rev-parse", "HEAD"])).unwrap();
        git(&["update-ref", "refs/remotes/origin/main", remote.trim()]);
        git(&["checkout", "-q", "main"]);
        let items = Daemon::items(root).unwrap();
        assert!(!items.iter().any(|entry| entry.item.to_string() == "rule:remote"));
        assert!(items.iter().find(|entry| entry.item.to_string() == "skill:base").unwrap().in_baseline);
    }

    #[test]
    fn only_small_blobs_are_fetched() {
        assert!(small_blob(b"0123abcd blob 16", 16));
        assert!(!small_blob(b"0123abcd blob 17", 16));
        assert!(!small_blob(b"0123abcd tree 4", 16));
        assert!(!small_blob(b"HEAD:personal/a b/SKILL.md missing", 16));
        assert!(!small_blob(b"", 16));
    }

    #[test]
    fn batch_output_is_read_by_size() {
        let oid = "ce013625030ba8dba906f756967f9e9ca394464a";
        let mut out = Vec::new();
        out.extend_from_slice(format!("{oid} blob 8\n").as_bytes());
        out.extend_from_slice(b"a\n\0 b\nc\n\n");
        out.extend_from_slice(b"abc:personal/a b/SKILL.md missing\n");
        out.extend_from_slice(format!("{oid} blob 0\n\n").as_bytes());
        out.extend_from_slice(format!("{oid} tree 4\nabcd\n").as_bytes());
        out.extend_from_slice(format!("{oid} blob 7\n").as_bytes());
        // Content that looks like headers is still content.
        out.extend_from_slice(b"x blob\n\n");
        out.extend_from_slice(format!("{oid} blob 5\nlarge\n").as_bytes());
        out.extend_from_slice(format!("{oid} blob 2\nok\n").as_bytes());
        out.extend_from_slice(format!("{oid} blob 9\ncut").as_bytes());
        let got = parse_batch(&out, 9, 4);
        let want: [Option<&[u8]>; 9] = [None, None, Some(b""), None, None, None, Some(b"ok"), None, None];
        assert_eq!(got, want.map(|content| content.map(<[u8]>::to_vec)));
        assert_eq!(parse_batch(&out, 1, 16)[0].as_deref(), Some(&b"a\n\0 b\nc\n"[..]));
        assert_eq!(parse_batch(b"", 2, 16), [None, None]);
    }

    #[test]
    fn stale_revisions_are_ignored() {
        assert!(newer(0, 1));
        assert!(newer(4, 5));
        assert!(!newer(5, 5));
        assert!(!newer(5, 3));
        assert!(!newer(5, 0));
        assert!(newer(0, 0));
    }

    #[test]
    fn only_rejected_pushes_are_retried() {
        let git = |stderr: &str| sk1ll_core::Error::Git { args: "push".into(), stderr: stderr.into() };
        assert!(is_rejected_push(&git(" ! [rejected]        main -> main (fetch first)")));
        assert!(!is_rejected_push(&git("fatal: unable to access 'http://h/git/repo.git/': Failed to connect")));
        assert!(!is_rejected_push(&git("fatal: the remote end hung up unexpectedly")));
    }
    #[tokio::test]
    async fn refused_adoption_rolls_back_only_its_own_unchanged_copy_and_can_retry() {
        use axum::{
            Json, Router,
            http::StatusCode,
            routing::{get, put},
        };
        use sk1ll_proto::{AssignmentChanges, AssignmentUpdate};
        for scenario in ["new", "retry", "edit_repo", "edit_source"] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("repo");
            let repo = Repo::init(&root).unwrap();
            let source = dir.path().join("h/pdf");
            std::fs::create_dir_all(&source).unwrap();
            std::fs::write(source.join("SKILL.md"), "original").unwrap();
            let destination = repo.personal_dir().join("pdf");
            if scenario == "retry" {
                std::fs::create_dir_all(&destination).unwrap();
                std::fs::write(destination.join("SKILL.md"), "original").unwrap();
            }
            let accept = Arc::new(AtomicBool::new(false));
            let app = Router::new()
                .route(
                    "/api/fleet",
                    get(|| async {
                        Json(sk1ll_proto::FleetView {
                            baseline: None,
                            machines: vec![sk1ll_proto::MachineRow {
                                id: "a".into(),
                                status: sk1ll_proto::MachineStatus::Drafting,
                                connected: true,
                                joined_at: 0,
                                last_seen: None,
                                report: Some(MachineReport {
                                    drafts: vec![ItemRef::Skill("pdf".into())],
                                    ..Default::default()
                                }),
                                scope: Default::default(),
                            }],
                        })
                    }),
                )
                .route(
                    "/api/machines/a/assignment",
                    put({
                        let accept = accept.clone();
                        let (destination, source) = (destination.clone(), source.clone());
                        move |Json(body): Json<AssignmentChanges>| {
                            let (accept, destination, source) = (accept.clone(), destination.clone(), source.clone());
                            async move {
                                assert_eq!(body.changes.len(), 1);
                                assert_eq!(body.changes[0].enabled, Some(true));
                                assert_eq!(body.changes[0].name.as_deref(), Some("pdf"));
                                if !accept.load(Ordering::SeqCst) {
                                    if scenario == "edit_repo" {
                                        std::fs::write(destination.join("SKILL.md"), "later edit").unwrap();
                                    }
                                    if scenario == "edit_source" {
                                        std::fs::write(source.join("SKILL.md"), "later edit").unwrap();
                                    }
                                    return (
                                        StatusCode::UNPROCESSABLE_ENTITY,
                                        "Server rejected assignment".to_string(),
                                    );
                                }
                                let mut assignment = sk1ll_proto::Assignment::default();
                                assignment.set_skill("h", "pdf", true);
                                (
                                    StatusCode::OK,
                                    serde_json::to_string(&AssignmentUpdate {
                                        assignment,
                                        revision: 2,
                                        entries: vec![],
                                    })
                                    .unwrap(),
                                )
                            }
                        }
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let mut cfg = Config::new().unwrap();
            cfg.state = dir.path().join("state/daemon.json");
            cfg.assignment = dir.path().join("assignment.json");
            cfg.machine_config = dir.path().join("machine.toml");
            std::fs::write(
                &cfg.machine_config,
                // The presets would find the developer's real harnesses.
                format!(
                    "[harnesses.h]\nskills = {:?}\nmode = \"copy\"\n\
                     [harnesses.claude]\nenabled = false\n[harnesses.codex]\nenabled = false\n\
                     [harnesses.opencode]\nenabled = false\n[harnesses.gemini]\nenabled = false\n",
                    dir.path().join("h")
                ),
            )
            .unwrap();
            let mut snap = AssignmentSnapshot { revision: 1, ..Default::default() };
            snap.assignment.set_new_skills_default("h", false);
            state::save_snapshot(&cfg.snapshot_path(), &snap).unwrap();
            let d = Daemon::new(
                cfg,
                DaemonState {
                    machine_id: "a".into(),
                    token: "t".into(),
                    server_url: format!("http://{address}"),
                    repo: root,
                    revoked: false,
                },
            )
            .unwrap();
            d.set_connection(Connection::Connected);
            let error = d
                .blocking(|d, eng| d.adopt(eng, ConflictTarget { harness: "h".into(), name: "pdf".into() }))
                .await
                .unwrap_err();
            assert!(error.to_string().contains("Server refused"), "{error}");
            assert_eq!(
                std::fs::read_to_string(source.join("SKILL.md")).unwrap(),
                if scenario == "edit_source" { "later edit" } else { "original" }
            );
            assert!(!source.join(".sk1ll-managed").exists());
            if scenario == "new" {
                assert!(!destination.exists());
                accept.store(true, Ordering::SeqCst);
                d.blocking(|d, eng| d.adopt(eng, ConflictTarget { harness: "h".into(), name: "pdf".into() }))
                    .await
                    .unwrap();
                assert!(source.join(".sk1ll-managed").is_file());
            } else {
                assert_eq!(
                    std::fs::read_to_string(destination.join("SKILL.md")).unwrap(),
                    if scenario == "edit_repo" { "later edit" } else { "original" }
                );
            }
            server.abort();
        }
    }
}
