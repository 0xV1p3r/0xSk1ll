//! The daemon's shared state and its operations on the repo and harnesses.
//!
//! Every operation that touches git or the file system runs on a blocking
//! thread with the engine mutex held ([`Daemon::blocking`]), so they never
//! interleave; across processes, the daemon holds core's [`EngineLock`] on
//! the repo for its whole lifetime, so direct CLI mutations are refused.
//! What the outside sees ([`StatusView`]) lives in a `watch` channel: the hub
//! session sends a status whenever its report changes, and the local API and
//! its event stream read from it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::Context;
use sk1ll_core::sync::{self, Action, ActionKind, Item, PlanOptions};
use sk1ll_core::{
    EngineLock, HarnessOrigin, Import, ItemRef, MachineConfig, PullOutcome, Repo, default_promote_message,
    machine_author,
};
use sk1ll_proto::{AssignmentSnapshot, ConflictRef, MachineReport, headline};
use tokio::sync::{Notify, broadcast, watch};

use crate::client::{
    AddImportRequest, ChangeKind, ConflictTarget, ConflictView, Connection, DiffView, DraftEntry, Event, HarnessView,
    ImportUpdated, ItemView, PromoteMessage, PromoteRequest, PromoteResponse, PullResult, PushResult, ResetRequest,
    ResetResponse, StatusView, SyncResult, UpdateImportsRequest,
};
use crate::error::{Error, Result};
use crate::join::{Joined, Registered};
use crate::outbox::{self, Outbox};
use crate::state::{self, DaemonState};
use crate::watch::WatchRoot;
use crate::{Config, gitx};

/// Name a `block` instructions conflict goes by (one block per harness).
const BLOCK_NAME: &str = "instructions";

pub(crate) struct Daemon {
    pub(crate) cfg: Config,
    /// Fixed for the life of the process: machine id, hub URL and token. A
    /// rejoin restarts the process (see [`Daemon::rejoin`]).
    pub(crate) ident: DaemonState,
    engine: Mutex<Engine>,
    view: watch::Sender<StatusView>,
    events: broadcast::Sender<Event>,
    /// `draft_discarded` messages not yet delivered, persisted.
    outbox: Mutex<Outbox>,
    /// Signals the hub session that the outbox has something new.
    pub(crate) outbox_ready: Notify,
    connected: AtomicBool,
    revoked: AtomicBool,
    /// Since when (Unix seconds) the hub has been rejecting the token.
    auth_rejected: Mutex<Option<i64>>,
    pub(crate) roots: watch::Sender<Vec<WatchRoot>>,
    pub(crate) http: reqwest::Client,
}

/// Mutable state, only touched with the engine mutex held.
pub(crate) struct Engine {
    state: DaemonState,
    /// The applied assignment. `None` when it was lost (or never stored):
    /// harnesses are left alone until the hub sends one.
    snapshot: Option<AssignmentSnapshot>,
    /// Draft items clashing with the hub's newer baseline (Blocked).
    blocked: Option<Vec<ItemRef>>,
    /// The hub's baseline as last announced or fetched.
    hub_baseline: Option<String>,
    /// Core's engine lock on the repo, held from startup (or from the clone).
    lock: Option<EngineLock>,
    /// Joined again and about to restart: do nothing more.
    frozen: bool,
}

impl Daemon {
    pub(crate) fn new(cfg: Config, state: DaemonState) -> anyhow::Result<Arc<Self>> {
        let snapshot = match state::load_snapshot(&cfg.snapshot_path()) {
            Ok(Some(s)) => Some(s),
            Ok(None) => {
                tracing::warn!("no stored assignment: leaving the harnesses alone until the hub sends one");
                None
            }
            Err(e) => {
                tracing::warn!("{e:#}; leaving the harnesses alone until the hub sends an assignment");
                None
            }
        };
        let outbox = Outbox::load(cfg.outbox_path());
        let view = StatusView {
            machine_id: state.machine_id.clone(),
            hub_url: state.hub_url.clone(),
            repo: state.repo.clone(),
            headline: sk1ll_proto::MachineStatus::Offline,
            connection: if state.revoked { Connection::Revoked } else { Connection::Connecting },
            last_error: None,
            revoked: state.revoked,
            hub_baseline: None,
            assignment_revision: snapshot.as_ref().map_or(0, |s| s.revision),
            awaiting_assignment: snapshot.is_none(),
            auth_rejected_since: None,
            pending_discards: outbox.len(),
            report: MachineReport::default(),
        };
        Ok(Arc::new(Self {
            cfg,
            ident: state.clone(),
            revoked: AtomicBool::new(state.revoked),
            engine: Mutex::new(Engine {
                state,
                snapshot,
                blocked: None,
                hub_baseline: None,
                lock: None,
                frozen: false,
            }),
            view: watch::channel(view).0,
            events: broadcast::channel(256).0,
            outbox: Mutex::new(outbox),
            outbox_ready: Notify::new(),
            connected: AtomicBool::new(false),
            auth_rejected: Mutex::new(None),
            roots: watch::channel(Vec::new()).0,
            http: reqwest::Client::builder().build().context("building the HTTP client")?,
        }))
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
            let offline = v.connection != Connection::Connected;
            v.headline = headline(offline, Some(&v.report), v.hub_baseline.as_deref());
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

    /// The hub answered 401 to the token. That can be a proxy or a hub
    /// misconfiguration as well as a removal while the daemon was away, so the
    /// machine keeps being managed and the connection is retried; only the
    /// hub's `revoked` message revokes it. After `auth_warn_after`, the status
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
                "the hub has rejected this machine's token since {}h ago; it was probably removed from the fleet: \
                 join again with `sk1ll join <hub-url> <join-code>` (still managing the harnesses meanwhile)",
                (now - since) / 3600
            )
        } else {
            "the hub rejected this machine's token (HTTP 401); retrying. If it was removed from the fleet, \
             join again with `sk1ll join <hub-url> <join-code>`"
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

    /// Queues a `draft_discarded` durably and wakes the hub session.
    fn queue_discarded(&self, items: Vec<ItemRef>) {
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

    pub(crate) fn outbox_front(&self) -> Option<outbox::Entry> {
        lock(&self.outbox).front()
    }

    /// `id` reached the hub.
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
        if eng.state.revoked {
            return Err(Error::Conflict(
                "this machine was removed from the fleet; the daemon no longer manages it".into(),
            ));
        }
        if eng.frozen {
            return Err(Error::Conflict("the daemon is restarting after joining again; try again".into()));
        }
        Ok(())
    }

    /// Nothing to manage: revoked, or about to restart.
    fn idle(eng: &Engine) -> bool {
        eng.state.revoked || eng.frozen
    }

    fn root(eng: &Engine) -> &Path {
        &eng.state.repo
    }

    /// Takes the engine lock on the repo if it exists and isn't held yet. Held
    /// by someone else means another sk1ll process is changing it.
    pub(crate) fn lock_repo(&self, eng: &mut Engine) -> Result<()> {
        if eng.lock.is_some() || !eng.state.repo.join(".git").exists() {
            return Ok(());
        }
        match EngineLock::try_acquire(&eng.state.repo)? {
            Some(l) => {
                tracing::debug!("holding {}", l.path().display());
                eng.lock = Some(l);
                Ok(())
            }
            None => Err(Error::Conflict(format!(
                "another sk1ll process (a second daemon, or a CLI command working directly) is changing {}",
                eng.state.repo.display()
            ))),
        }
    }

    /// At startup: clone the repo if it isn't there yet, drop credentials an
    /// older version left in origin's URL, and bring the harnesses in line.
    pub(crate) fn startup(&self, eng: &mut Engine) -> Result<()> {
        if Self::idle(eng) {
            return Ok(());
        }
        let root = eng.state.repo.clone();
        // Git authenticates through the process environment (`prepare_process`).
        if let Err(e) = gitx::ensure_clone(&root, &eng.state.repo_url(), &[]) {
            self.set_error(format!("setting up {}: {e:#}", root.display()));
        }
        self.lock_repo(eng)?;
        if let Some(s) = &eng.snapshot
            && let Err(e) = s.assignment.save(&self.cfg.assignment)
        {
            tracing::warn!("writing the assignment cache {}: {e}", self.cfg.assignment.display());
        }
        self.refresh(eng)
    }

    /// Plans and applies the sync, then recomputes the report. Conflicts are
    /// reported, never applied. Without an assignment, harnesses are left alone.
    pub(crate) fn refresh(&self, eng: &mut Engine) -> Result<()> {
        if Self::idle(eng) {
            return Ok(());
        }
        let root = eng.state.repo.clone();
        if !root.join(".git").exists() {
            return Err(Error::Conflict(format!("{} is not set up yet (the hub was unreachable)", root.display())));
        }
        self.lock_repo(eng)?;
        let machine = MachineConfig::load(&self.cfg.machine_config)?;
        let harnesses = machine.harnesses();
        let lock = eng.lock.as_ref().ok_or_else(|| Error::Conflict(format!("{} is not locked", root.display())))?;
        sync::recover(lock, &harnesses)?;
        let mut errors = Vec::new();
        let mut conflicts: Vec<ConflictRef> = Vec::new();
        if let Some(snap) = &eng.snapshot {
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
        let behind = blocked || eng.hub_baseline.as_ref().is_some_and(|h| baseline.as_ref() != Some(h));
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
        let hub_baseline = eng.hub_baseline.clone();
        let awaiting = eng.snapshot.is_none();
        self.update(|v| {
            v.report = report;
            v.hub_baseline = hub_baseline;
            v.awaiting_assignment = awaiting;
            if let Some(e) = errors.first() {
                v.last_error = Some(crate::auth::redact(e));
            }
        });
        for e in &errors {
            tracing::warn!("{}", crate::auth::redact(e));
        }

        let mut roots = vec![WatchRoot::new(root.canonicalize().unwrap_or(root), true)];
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
        Ok(())
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
        if eng.blocked.is_some() && self.view.borrow().report.drafts != before && self.is_connected() {
            self.pull(eng)?;
        }
        Ok(())
    }

    /// Fetches the hub's baseline and moves onto it (draft carried along),
    /// pushes queued promotes, and syncs.
    pub(crate) fn pull(&self, eng: &mut Engine) -> Result<PullResult> {
        self.ensure_managed(eng)?;
        let root = eng.state.repo.clone();
        if !root.join(".git").exists() {
            gitx::ensure_clone(&root, &eng.state.repo_url(), &[])?;
        }
        self.lock_repo(eng)?;
        let result = self.pull_inner(eng).and_then(|r| {
            if !matches!(r, PullResult::Blocked { .. }) {
                self.push_pending(eng)?;
            }
            Ok(r)
        });
        self.refresh(eng)?;
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
        let root = eng.state.repo.clone();
        let result = if gitx::rev(&root, "HEAD")?.is_none() {
            eng.blocked = None;
            match gitx::adopt_first_baseline(&root)? {
                Some(to) => PullResult::Updated { from: None, to },
                None => PullResult::UpToDate,
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
            eng.hub_baseline = Some(remote);
        }
        Ok(result)
    }

    /// Pushes promoted commits the hub doesn't have. A push the hub rejected
    /// as not fast-forward is retried once after rebasing onto its baseline;
    /// any other failure (unreachable, timed out) is returned right away.
    fn push_pending(&self, eng: &mut Engine) -> Result<bool> {
        let root = eng.state.repo.clone();
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
                    "can't push: the hub's baseline clashes with {}; promote or reset them",
                    names.join(", ")
                )));
            }
            repo.push()?;
        }
        tracing::info!("pushed to the hub");
        eng.hub_baseline = gitx::remote_head(&root)?;
        Ok(true)
    }

    /// `POST /push`: pushes promoted commits now.
    pub(crate) fn push(&self, eng: &mut Engine) -> Result<PushResult> {
        self.ensure_managed(eng)?;
        self.lock_repo(eng)?;
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
        self.pull(eng).map(drop)
    }

    pub(crate) fn promote_message(&self, eng: &Engine, items: &[String]) -> Result<PromoteMessage> {
        let repo = Repo::open_git_only(Self::root(eng))?;
        let draft = repo.draft()?;
        let items = items.iter().map(|i| ItemRef::resolve(i, &draft)).collect::<Result<Vec<_>, _>>()?;
        let message = default_promote_message(&items, &eng.state.machine_id);
        Ok(PromoteMessage { items, message })
    }

    pub(crate) fn promote(&self, eng: &mut Engine, req: PromoteRequest) -> Result<PromoteResponse> {
        self.ensure_managed(eng)?;
        self.lock_repo(eng)?;
        let root = eng.state.repo.clone();
        let repo = Repo::open_git_only(&root)?;
        let draft = repo.draft()?;
        let items = req.items.iter().map(|i| ItemRef::resolve(i, &draft)).collect::<Result<Vec<_>, _>>()?;
        if items.is_empty() {
            return Err(Error::BadRequest("name the draft items to promote".into()));
        }
        let id = eng.state.machine_id.clone();
        let message =
            req.message.filter(|m| !m.trim().is_empty()).unwrap_or_else(|| default_promote_message(&items, &id));
        let author = match req.author {
            Some(a) => Some(a),
            None if !repo.has_git_identity()? => Some(machine_author(&id)),
            None => None,
        };
        let sha = repo.promote(&items, &message, author.as_deref())?;
        tracing::info!("promoted {} as {sha}", items.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "));
        let (pushed, push_error) = if self.is_connected() {
            match self.push_pending(eng) {
                Ok(_) => (true, None),
                Err(e) => (false, Some(crate::auth::redact(&e.to_string()))),
            }
        } else {
            (false, Some("offline: queued until the hub is reachable".to_string()))
        };
        self.refresh(eng)?;
        if let Some(e) = &push_error {
            self.set_error(format!("push: {e}"));
        }
        if eng.blocked.is_some() && self.is_connected() {
            let _ = self.pull(eng);
        }
        Ok(PromoteResponse { sha, items, message, pushed, push_error })
    }

    /// Resets draft items, then queues `draft_discarded` durably. The hub
    /// session sends the updated status before it.
    pub(crate) fn reset(&self, eng: &mut Engine, req: ResetRequest) -> Result<ResetResponse> {
        self.ensure_managed(eng)?;
        self.lock_repo(eng)?;
        let repo = Repo::open_git_only(Self::root(eng))?;
        let draft = repo.draft()?;
        let before: BTreeSet<ItemRef> = draft.iter().map(|d| d.item.clone()).collect();
        match (req.all, req.items.is_empty()) {
            (true, true) => repo.reset_all()?,
            (false, false) => {
                let items = req.items.iter().map(|i| ItemRef::resolve(i, &draft)).collect::<Result<Vec<_>, _>>()?;
                repo.reset(&items)?;
            }
            (true, false) => {
                return Err(Error::BadRequest("pass either items or all, not both".into()));
            }
            (false, true) => {
                return Err(Error::BadRequest("name the draft items to reset, or pass all".into()));
            }
        }
        let after: BTreeSet<ItemRef> = repo.draft()?.into_iter().map(|d| d.item).collect();
        let discarded: Vec<ItemRef> = before.difference(&after).cloned().collect();
        let refreshed = self.refresh(eng);
        if !discarded.is_empty() {
            self.queue_discarded(discarded.clone());
        }
        refreshed?;
        if eng.blocked.is_some() && self.is_connected() {
            let _ = self.pull(eng);
        }
        Ok(ResetResponse { discarded })
    }

    /// A read-only inventory. GIT_OPTIONAL_LOCKS=0 prevents status from refreshing
    /// the index, and no engine mutex or EngineLock is acquired here.
    pub(crate) fn items(root: &Path) -> Result<Vec<ItemView>> {
        use sk1ll_core::git::Git;
        let read = |args: &[&str]| {
            Git::new(root).env("GIT_OPTIONAL_LOCKS", "0").args(args).run()
        };
        let paths = |revision: Option<&str>| -> Result<Vec<String>> {
            let Some(revision) = revision else { return Ok(Vec::new()) };
            let bytes = read(&["ls-tree", "-r", "-z", "--name-only", revision])?;
            Ok(bytes.split(|b| *b == 0).filter(|p| !p.is_empty())
                .map(|p| String::from_utf8_lossy(p).into_owned()).collect())
        };
        let baseline = gitx::baseline(root)?;
        let head = gitx::rev(root, "HEAD")?;
        let baseline_paths = paths(baseline.as_deref())?;
        let head_paths = paths(head.as_deref())?;
        let mut inventory = BTreeMap::<ItemRef, ItemView>::new();
        let entry = |item: ItemRef| ItemView {
            item, in_baseline: false, draft: None, unpushed: false, description: None,
        };
        for path in &baseline_paths {
            let item = ItemRef::for_path(Path::new(path));
            if matches!(item, ItemRef::Skill(_) | ItemRef::Fragment(_)) {
                inventory.entry(item.clone()).or_insert_with(|| entry(item)).in_baseline = true;
            }
        }
        let status = read(&["-c", "core.fsmonitor=false", "status", "--porcelain=v1", "-z",
            "--untracked-files=all", "--no-renames", "--ignore-submodules=all"])?;
        let mut changed = BTreeMap::<ItemRef, Vec<String>>::new();
        for raw in status.split(|b| *b == 0).filter(|p| p.len() >= 4) {
            let path = String::from_utf8_lossy(&raw[3..]).into_owned();
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
        for value in inventory.values_mut() {
            let ItemRef::Skill(name) = &value.item else { continue };
            for directory in ["personal", "imported"] {
                let path = format!("{directory}/{name}/SKILL.md");
                let text = if value.draft.is_some() {
                    use std::io::Read;
                    let mut text = String::new();
                    std::fs::File::open(root.join(&path)).ok()
                        .and_then(|file| file.take(16384).read_to_string(&mut text).ok()).map(|_| text)
                } else {
                    let revision = if value.unpushed { head.as_ref() } else { baseline.as_ref() };
                    revision.and_then(|rev| {
                        let object = format!("{rev}:{path}");
                        let size = read(&["cat-file", "-s", &object]).ok()?;
                        let size = String::from_utf8_lossy(&size).trim().parse::<usize>().ok()?;
                        (size <= 16384).then(|| read(&["show", &object]).ok()).flatten()
                            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    })
                };
                if let Some(text) = text {
                    value.description = skill_description(&text);
                    break;
                }
            }
        }
        Ok(inventory.into_values().collect())
    }

    pub(crate) fn draft(&self, eng: &Engine) -> Result<Vec<DraftEntry>> {
        let repo = Repo::open_git_only(Self::root(eng))?;
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

    pub(crate) fn diff(&self, eng: &Engine, item: &str) -> Result<DiffView> {
        let repo = Repo::open_git_only(Self::root(eng))?;
        let item = ItemRef::resolve(item, &repo.draft()?)?;
        let diff = repo.diff(&item)?;
        Ok(DiffView { item, diff })
    }

    /// The current plan; `force` plans as if overwriting.
    fn plan(&self, eng: &Engine, force: bool) -> Result<(Repo, Vec<Action>)> {
        let Some(snap) = &eng.snapshot else {
            return Err(Error::Conflict(
                "waiting for the hub's assignment; the harnesses are left alone until it arrives".into(),
            ));
        };
        let repo = Repo::open(Self::root(eng))?;
        let harnesses = MachineConfig::load(&self.cfg.machine_config)?.harnesses();
        let actions = sync::plan(&repo, &harnesses, &snap.assignment, PlanOptions { force })?;
        Ok((repo, actions))
    }

    pub(crate) fn recovered(&self) -> Result<Vec<ConflictView>> {
        Ok(sync::displaced::list()?.into_iter().map(displaced_view).collect())
    }

    pub(crate) fn conflicts(&self, eng: &Engine) -> Result<Vec<ConflictView>> {
        let mut result: Vec<_> = if eng.snapshot.is_some() {
            let (_, actions) = self.plan(eng, false)?;
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
        Ok(SyncResult { conflicts: self.conflicts(eng)? })
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
        let (repo, actions) = self.plan(eng, false)?;
        sync::adopt(&repo, Self::find_conflict(&actions, &t)?)?;
        tracing::info!("adopted {} from {} into the draft", t.name, t.harness);
        self.refresh(eng)?;
        if eng.blocked.is_some() && self.is_connected() {
            let _ = self.pull(eng);
        }
        Ok(())
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

    /// Re-vendors imports into the draft: `name`, or all of them.
    pub(crate) fn update_imports(&self, eng: &mut Engine, req: UpdateImportsRequest) -> Result<Vec<ImportUpdated>> {
        self.ensure_managed(eng)?;
        self.lock_repo(eng)?;
        let mut repo = Repo::open(Self::root(eng))?;
        let reports = repo.update(req.name.as_deref(), req.force);
        self.refresh(eng)?;
        Ok(reports?.into_iter().map(import_updated).collect())
    }

    /// Re-vendors one import into the draft.
    pub(crate) fn update_import(&self, eng: &mut Engine, name: &str) -> Result<ImportUpdated> {
        let req = UpdateImportsRequest { name: Some(name.to_string()), force: false };
        self.update_imports(eng, req)?.into_iter().next().context("no update report").map_err(Error::Internal)
    }

    /// Adds an import to the draft.
    pub(crate) fn add_import(&self, eng: &mut Engine, req: AddImportRequest) -> Result<ImportUpdated> {
        self.ensure_managed(eng)?;
        self.lock_repo(eng)?;
        let mut repo = Repo::open(Self::root(eng))?;
        let added = repo.add_import(&req.name, Import { source: req.source, path: req.path, git_ref: req.git_ref });
        self.refresh(eng)?;
        Ok(import_updated(added?))
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
        if let Some(have) = &eng.snapshot
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
        eng.snapshot = Some(snap);
        tracing::info!("applied assignment revision {revision}");
        self.update(|v| {
            v.assignment_revision = revision;
            v.awaiting_assignment = false;
        });
        self.event(Event::Assignment { revision });
        self.refresh(eng)?;
        Ok(true)
    }

    /// The hub's baseline is now `baseline`: pull unless already on it.
    pub(crate) fn nudge(&self, eng: &mut Engine, baseline: String) -> Result<()> {
        if Self::idle(eng) {
            return Ok(());
        }
        eng.hub_baseline = Some(baseline.clone());
        self.update(|v| v.hub_baseline = Some(baseline.clone()));
        self.event(Event::Fleet);
        let current = gitx::baseline(Self::root(eng)).unwrap_or(None);
        if current.as_deref() != Some(baseline.as_str()) || eng.blocked.is_some() {
            self.pull(eng).map(drop)
        } else {
            self.refresh(eng)
        }
    }

    /// Stops managing this machine. Harness files are left as they are.
    pub(crate) fn revoke(&self, eng: &mut Engine, why: &str) -> Result<()> {
        if eng.state.revoked {
            return Ok(());
        }
        tracing::warn!("revoked: {why}; no longer managing this machine (its files are left as they are)");
        eng.state.revoked = true;
        eng.state.save(&self.cfg.state)?;
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
    /// hub rejects its token.
    pub(crate) fn can_rejoin(&self) -> bool {
        self.is_revoked() || self.view().connection == Connection::AuthRejected
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
/// (unknown, from an older hub) is only taken while nothing newer was seen.
pub(crate) fn newer(applied: u64, incoming: u64) -> bool {
    incoming > applied || (incoming == 0 && applied == 0)
}

/// Whether a push failed because the hub has commits we don't (worth a pull
/// and one retry), as opposed to the hub being unreachable.
fn is_rejected_push(e: &sk1ll_core::Error) -> bool {
    let text = e.to_string();
    ["[rejected]", "non-fast-forward", "fetch first", "failed to push some refs"].iter().any(|m| text.contains(m))
        && !text.contains("unable to access")
}

fn import_updated(r: sk1ll_core::UpdateReport) -> ImportUpdated {
    ImportUpdated { name: r.name, old_commit: r.old_commit, new_commit: r.new_commit, changed: r.changed }
}

fn skill_description(text: &str) -> Option<String> {
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" { return None }
    lines.take_while(|line| line.trim() != "---")
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
        Item::Rule { file, .. } => vec![ItemRef::Fragment(file.strip_suffix(".md").unwrap_or(file).to_string())],
        Item::Instructions { fragments, .. } if !fragments.is_empty() => {
            fragments.iter().map(|f| ItemRef::Fragment(f.clone())).collect()
        }
        Item::Instructions { .. } => vec![ItemRef::Other(a.dest.display().to_string())],
    }
}

fn displaced_view(entry: sync::displaced::Entry) -> ConflictView {
    let item = displaced_item(&entry);
    ConflictView {
        kind: Some("displaced".into()), id: Some(entry.id), item: Some(item.clone()),
        name: entry.dest.file_name().unwrap_or_default().to_string_lossy().into_owned(),
        items: vec![item], harness: entry.harness, dest: entry.dest, path: Some(entry.path),
        displaced_at: Some(entry.displaced_at), reason: None, adoptable: true,
    }
}

fn displaced_item(entry: &sync::displaced::Entry) -> ItemRef {
    let name = entry.dest.file_name().unwrap_or_default().to_string_lossy();
    match entry.kind {
        sync::displaced::Kind::Skill => ItemRef::Skill(name.into_owned()),
        sync::displaced::Kind::Rule => ItemRef::Fragment(name.strip_suffix(".md").unwrap_or(&name).to_string()),
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
        kind: None, id: None, item: None, path: None, displaced_at: None,
        harness: a.harness.clone(),
        name,
        items: conflict_items(a),
        dest: a.dest.clone(),
        reason: a.reason.clone(),
        adoptable,
    }
}

fn matches_target(a: &Action, t: &ConflictTarget) -> bool {
    a.harness == t.harness
        && match &a.item {
            Item::Skill { name, .. } => *name == t.name,
            Item::Rule { file, .. } => *file == t.name || *file == format!("{}.md", t.name),
            Item::Instructions { fragments, .. } => t.name == BLOCK_NAME || fragments.contains(&t.name),
        }
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_reads_applied_baseline_and_never_refreshes_the_index() {
        use sk1ll_core::git::Git;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = |args: &[&str]| Git::new(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["-c", "commit.gpgsign=false"]).args(args).run().unwrap();
        let write = |path: &str, text: &str| {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "test"]);
        git(&["config", "user.email", "test@example.invalid"]);
        write("personal/base/SKILL.md", "---\ndescription: Baseline description\n---\nbase\n");
        write("instructions/git.md", "baseline fragment\n");
        git(&["add", "."]);
        git(&["commit", "-qm", "baseline"]);
        let baseline = String::from_utf8(git(&["rev-parse", "HEAD"])).unwrap();
        git(&["update-ref", "refs/remotes/origin/main", baseline.trim()]);
        write("personal/queued/SKILL.md", "queued\n");
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
        assert!(find("fragment:git").in_baseline, "deleted Draft item still comes from the git tree");
        assert_eq!(find("fragment:git").draft, Some(ChangeKind::Deleted));
        assert!(!find("skill:queued").in_baseline);
        assert!(find("skill:queued").unpushed);
        assert!(!find("skill:local").in_baseline);
        assert_eq!(find("skill:local").draft, Some(ChangeKind::Added));
        assert_eq!(std::fs::read(root.join(".git/index")).unwrap(), index_before);
        assert_eq!(git(&["rev-parse", "HEAD"]), head_before);

        // A fetched but clashing Baseline has not been applied locally.
        git(&["checkout", "-qb", "remote", baseline.trim()]);
        write("instructions/remote.md", "remote fragment\n");
        git(&["add", "instructions/remote.md"]);
        git(&["commit", "-qm", "remote"]);
        let remote = String::from_utf8(git(&["rev-parse", "HEAD"])).unwrap();
        git(&["update-ref", "refs/remotes/origin/main", remote.trim()]);
        git(&["checkout", "-q", "main"]);
        let items = Daemon::items(root).unwrap();
        assert!(!items.iter().any(|entry| entry.item.to_string() == "fragment:remote"));
        assert!(items.iter().find(|entry| entry.item.to_string() == "skill:base").unwrap().in_baseline);
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
}
