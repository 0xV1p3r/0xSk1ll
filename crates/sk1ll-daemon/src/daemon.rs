//! The daemon's shared state and its operations on the repo and harnesses.
//!
//! Every operation that changes the repo, the harnesses or the daemon's state
//! runs on a blocking thread with the engine mutex held ([`Daemon::blocking`]),
//! so they never interleave; across processes, the daemon holds core's
//! [`EngineLock`] on the repo for its whole lifetime, so direct CLI mutations
//! are refused.
//!
//! No network I/O happens with the engine mutex held. An operation that
//! talks to the server runs in steps: it checks and takes what it will send
//! under the mutex, releases it for the network (git's clone, fetch and push
//! through [`Daemon::network`], Assignment writes over the server session),
//! then takes it again to apply the result, re-checking what the result
//! depends on: an Assignment revision applied meanwhile wins over an older
//! confirmation, a pull moves onto whatever was fetched last, an Adopt whose
//! item changed meanwhile is a conflict, and a daemon that stopped managing
//! meanwhile applies nothing. Its lifecycle ([`Lifecycle`]) has a mutex of its
//! own, so the session can mark it connected or offline at any time.
//!
//! Operations that only read (the draft, a diff, the skills browser, the
//! conflicts, the inventory) run without the engine mutex
//! ([`Daemon::reading`]), so the GUI keeps answering while a long engine
//! operation (a sync of many skills) runs. They need the repo root, which is fixed
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
//! its event stream read from it. Its lifecycle fields are derived from the
//! [`Lifecycle`] whenever it is published ([`Daemon::publish`]).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, TryLockError};

use anyhow::Context;
use sk1ll_core::sync::{self, Action, ActionKind, Item, PlanOptions};
use sk1ll_core::{
    EngineLock, HarnessChange, HarnessOrigin, ItemRef, MachineConfig, PullOutcome, Repo, default_commit_message,
    machine_author,
};
use sk1ll_core::{harness_change, harness_rename::RenameIntent, imports};
use sk1ll_proto::{AssignmentSnapshot, AssignmentUpdate, AssignmentWrite, ConflictRef, MachineReport};
use tokio::sync::{Notify, broadcast, mpsc, watch};

use crate::client::{
    AddImportRequest, ChangeKind, CommitMessage, CommitRequest, CommitResponse, ConflictTarget, ConflictView,
    Connection, DaemonMode, DiffView, DraftEntry, Event, HarnessApplied, HarnessChangeRequest, HarnessChoice,
    HarnessPreview, HarnessSettings, HarnessView, ImportUpdated, InstructionPreview, ItemView, PlacementView,
    PullResult, PushResult, ResolveImportRequest, ResolvedSource, RestoreRequest, RestoreResponse, SkillCandidate,
    StatusView, SyncResult, UpdateImportsRequest,
};
use crate::error::{Error, Result};
use crate::join::{Joined, Registered};
use crate::lifecycle::Lifecycle;
pub(crate) use crate::lifecycle::Link;
use crate::state::{self, DaemonState, PullRecord, PullState};
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
}

pub(crate) struct Daemon {
    pub(crate) cfg: Config,
    /// Fixed for the life of the process: machine id, server URL and token. A
    /// rejoin restarts the process (see [`Daemon::rejoin`]). Its `revoked`
    /// and record are as of startup; [`Daemon::lifecycle`] owns them.
    pub(crate) ident: Mode,
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
    snapshot: RwLock<Option<Arc<AssignmentSnapshot>>>,
    view: watch::Sender<StatusView>,
    events: broadcast::Sender<Event>,
    pub(crate) assignment_session: watch::Sender<Option<mpsc::Sender<crate::server::SessionWrite>>>,
    pub(crate) restart_assignment_session: Notify,
    pub(crate) roots: watch::Sender<Vec<WatchRoot>>,
    pub(crate) http: reqwest::Client,
}

/// See [`Daemon::pull_inputs`].
#[derive(PartialEq)]
struct PullInputs {
    head: Option<String>,
    remote: Option<String>,
    pull: PullState,
    pending: bool,
}

type ReviewedSync = (Repo, Vec<(Action, Option<String>)>);

/// An unmanaged skill published into the draft, waiting for the Assignment
/// write that enables it (see [`Daemon::adopt`]).
struct PendingAdoption {
    adoption: sync::unmanaged::Adoption,
    /// Publishing created it, so a refused write takes it back out.
    created: bool,
    id: sk1ll_core::ItemId,
    changes: sk1ll_proto::AssignmentChanges,
}

#[derive(Default)]
struct RefreshOutcome {
    errors: Vec<String>,
    completed: Vec<PlacementView>,
    changed: Option<PlacementView>,
}

/// Mutable state, only touched with the engine mutex held.
pub(crate) struct Engine {
    /// Core's engine lock on the repo, held from startup (or from the clone).
    lock: Option<EngineLock>,
    /// The published report is not the result of a successful refresh: the
    /// last one failed, or none ran yet. Nothing may be derived from it.
    stale: bool,
    assignment_placement_failed: bool,
    #[cfg(test)]
    after_sync_review: Option<Box<dyn FnOnce() + Send>>,
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
        let lifecycle = match &state {
            Mode::Joined(s) => Lifecycle::joined(s.revoked, s.record.clone()),
            Mode::Standalone { .. } => {
                let path = state::standalone_record_path(&cfg.state);
                Lifecycle::standalone(state::load_record(&path).unwrap_or_else(|e| {
                    tracing::warn!("{e:#}; starting without a remembered Baseline");
                    PullRecord::default()
                }))
            }
        };
        let snapshot = if standalone {
            let assignment = sk1ll_core::Assignment::load(&cfg.assignment)?;
            let previous = state::load_snapshot(&cfg.state.with_file_name("standalone-assignment.json"))?;
            let revision = previous.as_ref().map_or(1, |s| s.revision + u64::from(s.assignment != assignment));
            Some(AssignmentSnapshot { revision, assignment, registration: None })
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
        let mut view = StatusView {
            machine_id: state.machine_id().to_string(),
            server_url: state.joined().ok().map(|s| s.server_url.clone()),
            mode: if standalone { DaemonMode::Standalone } else { DaemonMode::Joined },
            has_origin: !standalone || gitx::has_origin(state.repo()).unwrap_or(false),
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
            snapshot: RwLock::new(snapshot.map(Arc::new)),
            lifecycle: Mutex::new(lifecycle),
            net: tokio::sync::Mutex::new(()),
            assignment_writes: tokio::sync::Mutex::new(()),
            engine: Mutex::new(Engine {
                lock: None,
                stale: true,
                assignment_placement_failed: true,
                #[cfg(test)]
                after_sync_review: None,
            }),
            view: watch::channel(view).0,
            events: broadcast::channel(256).0,
            assignment_session: watch::channel(None).0,
            restart_assignment_session: Notify::new(),
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
        self.refresh(eng)
    }

    /// Publishes local file edits through the same snapshot the engine's
    /// readers use. The separate revision file never becomes fleet state.
    fn reload_local_assignment(&self) -> Result<()> {
        if let Some(intent) = RenameIntent::load(&self.cfg.machine_config, None)? {
            intent.apply_standalone(&self.cfg.machine_config, &self.cfg.assignment)?;
        }
        let assignment = sk1ll_core::Assignment::load(&self.cfg.assignment)?;
        let previous = self.snapshot();
        if previous.as_ref().is_some_and(|s| s.assignment == assignment) {
            return Ok(());
        }
        let revision = previous.as_ref().map_or(1, |s| s.revision + 1);
        let snap = AssignmentSnapshot { revision, assignment, registration: None };
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
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        // An edit made outside the daemon counts as a newer revision.
        self.reload_local_assignment()?;
        let mut repo = Repo::open(self.repo_root())?;
        // Changing a default records every item's current value by Item ID,
        // so each item needs one. A pending pull's tree is partial: no writes.
        if !repo.pull_pending()? && repo.assign_missing_ids()? > 0 {
            repo = Repo::open(self.repo_root())?;
        }
        let harnesses = MachineConfig::load(&self.cfg.machine_config)?.harnesses();
        let skills: Vec<_> = repo.skills()?.into_iter().map(|s| s.id).collect();
        let rules: Vec<_> = repo.rules()?.into_iter().map(|r| r.id).collect();
        let unassigned = skills.iter().chain(&rules).any(Option::is_none);
        let (skills, rules): (Vec<_>, Vec<_>) =
            (skills.into_iter().flatten().collect(), rules.into_iter().flatten().collect());
        if let Some(expected) = &changes.expected {
            let revision = self.snapshot().map_or(0, |s| s.revision);
            if expected.registration.is_some() {
                return Err(Error::Conflict(
                    "this Machine is standalone now; reload its Assignment before changing it".into(),
                ));
            }
            if revision != expected.revision {
                return Err(Error::Conflict(format!(
                    "the Assignment of `{}` changed since revision {} (now {revision}); reload it",
                    self.ident.machine_id(),
                    expected.revision
                )));
            }
        }
        let mut assignment = sk1ll_core::Assignment::load(&self.cfg.assignment)?;
        for change in &changes.changes {
            if !harnesses.contains_key(&change.harness) {
                return Err(Error::BadRequest(format!("unknown harness `{}`", change.harness)));
            }
            if change.kind.has_item() != change.item_id.is_some()
                || (!change.kind.has_item() && (change.enabled.is_none() || change.name.is_some()))
            {
                return Err(Error::BadRequest(
                    "item toggles need an Item ID; defaults need enabled and take no item".into(),
                ));
            }
            if unassigned && matches!(change.kind, ToggleKind::NewSkillsDefault | ToggleKind::NewRulesDefault) {
                return Err(Error::Conflict(
                    "an item has no Item ID yet (a pull is pending); change the defaults once it has finished".into(),
                ));
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
                    let id = change.item_id.as_ref().unwrap();
                    if let Some(enabled) = change.enabled {
                        toggles.insert(id.clone(), enabled);
                    } else {
                        toggles.remove(id);
                    }
                }
            }
        }
        // Assignment::save replaces the file atomically, like the direct CLI.
        assignment.save(&self.cfg.assignment)?;
        let errors = self.saved_local_assignment(eng);
        let snap = self.snapshot().expect("standalone always has assignment");
        Ok(sk1ll_proto::AssignmentUpdate {
            revision: snap.revision,
            assignment: snap.assignment.clone(),
            entries: Vec::new(),
            errors,
        })
    }

    /// Publishes and applies a just saved local Assignment. The save is the
    /// write: a failure from here on shows in the status, never as a failed write.
    fn saved_local_assignment(&self, eng: &mut Engine) -> Vec<String> {
        let result = self.reload_local_assignment().and_then(|()| self.refresh_reporting(eng));
        self.assignment_placement_outcome(eng, result)
    }

    fn assignment_placement_outcome(&self, eng: &mut Engine, result: Result<Vec<String>>) -> Vec<String> {
        let errors = result.unwrap_or_else(|e| {
            let message = crate::auth::redact(&format!("applying the saved Assignment: {e}"));
            self.set_error(message.clone());
            vec![message]
        });
        eng.assignment_placement_failed = !errors.is_empty();
        errors.into_iter().map(|error| crate::auth::redact(&error)).collect()
    }

    /// Writes Assignment: to the machine-local file standalone, else through
    /// the server session. The server may take up to its request timeout to
    /// confirm, so a joined write holds the engine mutex only to check
    /// whether it may be sent and, once confirmed, to apply it.
    pub(crate) async fn write_assignment(self: &Arc<Self>, write: AssignmentWrite) -> Result<AssignmentUpdate> {
        if self.is_standalone() {
            return self.blocking(move |d, eng| d.write_local_assignment(eng, write)).await;
        }
        let _writes = self.assignment_writes.lock().await;
        self.blocking(|d, _| {
            if RenameIntent::exists(&d.cfg.machine_config)? {
                return Err(Error::Unavailable("waiting for the pending Harness rename to finish".into()));
            }
            Ok(())
        })
        .await?;
        let update = crate::server::write_assignment(self, write).await?;
        self.blocking(move |d, eng| Ok(d.apply_confirmed(eng, update))).await
    }

    fn write_local_assignment(&self, eng: &mut Engine, write: AssignmentWrite) -> Result<AssignmentUpdate> {
        match write {
            AssignmentWrite::Changes(changes) => self.set_local_assignment(eng, &changes),
            AssignmentWrite::Harness { harness, enabled } => {
                self.ensure_managed()?;
                self.lock_repo(eng)?;
                self.reload_local_assignment()?;
                if !MachineConfig::load(&self.cfg.machine_config)?.harnesses().contains_key(&harness) {
                    return Err(Error::BadRequest(format!("unknown harness `{harness}`")));
                }
                let mut assignment = sk1ll_core::Assignment::load(&self.cfg.assignment)?;
                assignment.set_harness(&harness, enabled);
                assignment.save(&self.cfg.assignment)?;
                let errors = self.saved_local_assignment(eng);
                let snap = self.snapshot().expect("standalone always has assignment");
                Ok(AssignmentUpdate {
                    revision: snap.revision,
                    assignment: snap.assignment.clone(),
                    entries: Vec::new(),
                    errors,
                })
            }
            AssignmentWrite::RenameHarness(_) => unreachable!("handled with the machine config edit"),
        }
    }

    /// Applies an Assignment the server confirmed, as of now rather than as
    /// of when it was sent. A daemon that stopped managing meanwhile applies
    /// nothing; a newer revision applied meanwhile (its snapshot overtook the
    /// confirmation) stands, and since the server numbers revisions in order
    /// it already includes this write: the reply then carries that one.
    fn apply_confirmed(&self, eng: &mut Engine, mut update: AssignmentUpdate) -> AssignmentUpdate {
        if self.idle() {
            tracing::info!("not applying Assignment revision {}: no longer managing", update.revision);
            return update;
        }
        let snapshot =
            AssignmentSnapshot { revision: update.revision, assignment: update.assignment.clone(), registration: None };
        update.errors = self.apply_assignment(eng, snapshot).unwrap_or_else(|e| {
            self.set_error(format!("applying the saved Assignment: {e}"));
            vec![crate::auth::redact(&e.to_string())]
        });
        if let Some(current) = self.snapshot()
            && current.revision > update.revision
        {
            tracing::info!(
                "Assignment revision {} was confirmed after revision {} had been applied",
                update.revision,
                current.revision
            );
            update.revision = current.revision;
            update.assignment = current.assignment.clone();
        }
        self.event(Event::Fleet);
        update
    }

    /// Renames a Harness with its Assignment. Joined, the server's
    /// Assignment is fetched and the rename written without the engine
    /// mutex; the intent and the machine config change under it.
    pub(crate) async fn rename_harness(
        self: &Arc<Self>,
        request: crate::client::HarnessRenameRequest,
    ) -> Result<crate::client::HarnessRenameResult> {
        if self.is_standalone() {
            return self.blocking(move |d, eng| d.rename_local_harness(eng, request)).await;
        }
        let _writes = self.assignment_writes.lock().await;
        let state = self.ident.joined()?.clone();
        let authority = state.rename_authority();
        let pending = self
            .blocking(move |d, eng| {
                d.ensure_managed()?;
                d.lock_repo(eng)?;
                Ok(RenameIntent::load(&d.cfg.machine_config, Some(&authority))?)
            })
            .await?;
        // A rename that is already pending is resumed, not prepared again.
        let authoritative = match pending {
            Some(intent) if intent.request == request => None,
            Some(_) => return Err(Error::Conflict("finish the pending Harness rename first".into())),
            None => Some(crate::server::fetch_machine_assignment(&state, &self.http).await?.assignment),
        };
        let intent = self
            .blocking(move |d, eng| {
                d.ensure_managed()?;
                d.lock_repo(eng)?;
                let intent = crate::server::prepare_rename(&d.cfg, &state, request, authoritative.as_ref())?;
                intent.apply_config(&d.cfg.machine_config)?;
                Ok(intent)
            })
            .await?;
        let result = crate::server::write_assignment(self, crate::server::rename_write(&intent)).await;
        let routed_rules = intent.routed_rules.clone();
        match self.blocking(move |d, eng| d.finish_harness_rename(eng, intent, result)).await {
            Ok(update) => Ok(crate::client::HarnessRenameResult { update, routed_rules }),
            Err(error) => {
                self.restart_assignment_session.notify_one();
                Err(error)
            }
        }
    }

    fn rename_local_harness(
        &self,
        eng: &mut Engine,
        request: crate::client::HarnessRenameRequest,
    ) -> Result<crate::client::HarnessRenameResult> {
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        let config = &self.cfg.machine_config;
        let authoritative = sk1ll_core::Assignment::load(&self.cfg.assignment)?;
        let intent = RenameIntent::prepare(config, self.repo_root(), request, &authoritative, None)?;
        intent.apply_standalone(config, &self.cfg.assignment)?;
        let errors = self.saved_local_assignment(eng);
        let snap = self.snapshot().expect("standalone always has assignment");
        Ok(crate::client::HarnessRenameResult {
            update: AssignmentUpdate {
                revision: snap.revision,
                assignment: snap.assignment.clone(),
                entries: Vec::new(),
                errors,
            },
            routed_rules: intent.routed_rules,
        })
    }

    pub(crate) fn pending_harness_rename(&self, eng: &mut Engine) -> Result<Option<RenameIntent>> {
        let authority = self.ident.joined()?.rename_authority();
        let config = &self.cfg.machine_config;
        let Some(intent) = RenameIntent::load(config, Some(&authority))? else { return Ok(None) };
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        intent.apply_config(config)?;
        Ok(Some(intent))
    }

    pub(crate) fn finish_harness_rename(
        &self,
        eng: &mut Engine,
        intent: RenameIntent,
        result: Result<AssignmentUpdate>,
    ) -> Result<AssignmentUpdate> {
        // If the process stops after clearing the intent, startup must
        // already have the Assignment for the new config.
        if let Ok(update) = &result
            && self.snapshot().is_none_or(|current| current.revision < update.revision)
        {
            crate::server::save_assignment_snapshot(&self.cfg, update)?;
        }
        let mut update = crate::server::resolve_rename(&self.cfg.machine_config, &intent, result)?;
        let snapshot =
            AssignmentSnapshot { revision: update.revision, assignment: update.assignment.clone(), registration: None };
        if self.snapshot().is_some_and(|current| current.revision >= snapshot.revision) {
            update.errors = self.refresh_reporting(eng)?;
        } else {
            update.errors = self.apply_assignment(eng, snapshot)?;
        }
        self.event(Event::Fleet);
        Ok(update)
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
            Mode::Standalone { .. } => state::save_record(&state::standalone_record_path(&self.cfg.state), record),
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
    pub(crate) fn set_link(&self, link: Link) {
        self.note(|lc| lc.set_link(link));
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

    fn ensure_managed(&self) -> Result<()> {
        lock(&self.lifecycle).managed().map_err(|why| Error::Conflict(why.into()))?;
        self.require_repo()
    }

    /// Nothing to manage: revoked, or about to restart.
    fn idle(&self) -> bool {
        lock(&self.lifecycle).idle()
    }

    /// Takes the engine lock on the repo if it exists and isn't held yet. Held
    /// by someone else means another sk1ll process is changing it.
    pub(crate) fn lock_repo(&self, eng: &mut Engine) -> Result<()> {
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

    /// At startup: clone the repo if it isn't there yet (without the engine
    /// mutex), drop credentials an older version left in origin's URL, and
    /// bring the harnesses in line.
    pub(crate) async fn startup(self: &Arc<Self>) -> Result<()> {
        if self.idle() {
            return Ok(());
        }
        if !self.is_standalone() {
            // Git authenticates through the process environment (`prepare_process`).
            let cloned =
                self.network(|d| Ok(gitx::ensure_clone(d.repo_root(), &d.ident.joined()?.repo_url(), &[]))).await?;
            if let Err(e) = cloned {
                self.set_error(format!("setting up {}: {e:#}", self.repo_root().display()));
            }
        }
        self.blocking(|d, eng| {
            d.lock_repo(eng)?;
            if !d.is_standalone()
                && let Some(s) = d.snapshot()
                && let Err(e) = s.assignment.save(&d.cfg.assignment)
            {
                tracing::warn!("writing the assignment cache {}: {e}", d.cfg.assignment.display());
            }
            d.refresh(eng)
        })
        .await
    }

    /// Plans and applies the sync, then recomputes the report. Conflicts are
    /// reported, never applied. Without an assignment, harnesses are left alone.
    ///
    /// This is the expensive operation (a full plan and a dozen git
    /// processes), so callers that know nothing it reads has changed since
    /// the last one skip it: see [`Daemon::pull_refreshed`], [`Daemon::nudge`]
    /// uses this to decide when it must recompute a report.
    pub(crate) fn refresh(&self, eng: &mut Engine) -> Result<()> {
        self.refresh_reporting(eng).map(drop)
    }

    /// [`Daemon::refresh`], returning what the sync failed to place (also
    /// published as the last error).
    fn refresh_reporting(&self, eng: &mut Engine) -> Result<Vec<String>> {
        let result = self.refresh_with_plan(eng, None).map(|outcome| outcome.errors);
        eng.assignment_placement_failed = result.as_ref().map_or(true, |errors| !errors.is_empty());
        result
    }

    fn refresh_with_plan(&self, eng: &mut Engine, reviewed: Option<ReviewedSync>) -> Result<RefreshOutcome> {
        if self.idle() {
            return Ok(RefreshOutcome::default());
        }
        if self.is_standalone() {
            self.reload_local_assignment()?;
        } else if let Ok(state) = self.ident.joined()
            && RenameIntent::load(&self.cfg.machine_config, Some(&state.rename_authority()))?.is_some()
        {
            return Err(Error::Unavailable("waiting for the pending Harness rename to finish".into()));
        }
        tracing::debug!("refreshing the local state");
        eng.stale = true;
        let root = self.repo_root().to_path_buf();
        if self.is_standalone() && !root.join(".git").exists() {
            self.update(|v| {
                v.repo_ready = false;
                v.has_origin = false;
                v.last_error = None;
            });
            self.roots
                .send_replace(vec![WatchRoot::new(root, true), WatchRoot::new(self.cfg.assignment.clone(), false)]);
            return Ok(RefreshOutcome::default());
        }
        if !root.join(".git").exists() {
            return Err(Error::Conflict(format!("{} is not set up yet (the server was unreachable)", root.display())));
        }
        self.lock_repo(eng)?;
        let machine = MachineConfig::load(&self.cfg.machine_config)?;
        let harnesses = machine.harnesses();
        let lock = eng.lock.as_ref().ok_or_else(|| Error::Conflict(format!("{} is not locked", root.display())))?;
        let mut outcome = RefreshOutcome::default();
        let repo = Repo::open_git_only(&root)?;
        let finish = repo.finish_pull(lock);
        let pending = repo.pull_pending()?;
        let finished = match finish {
            Ok(None) => None,
            Ok(Some(PullOutcome::Blocked { baseline, items })) if !pending => {
                Some(PullState::Blocked { baseline, items })
            }
            Ok(Some(PullOutcome::Blocked { items, .. })) => Some(PullState::Pending { items }),
            Ok(Some(_)) | Err(sk1ll_core::Error::Displaced { .. }) if !pending => {
                tracing::info!("finished a pull that had stopped half-way");
                Some(PullState::Clear)
            }
            Err(e) => {
                outcome.errors.push(format!("pull: {e}"));
                None
            }
            Ok(Some(_)) => None,
        };
        let pull = self.pull_after_refresh(&root, finished, pending);
        // Sync waits for every item to have an Item ID; the report doesn't.
        let mut ids_ready = true;
        if !pending && reviewed.is_none() {
            sync::recover(Some(lock), &harnesses)?;
            Self::recover_imports(&root)?;
            // After the pull is finished: a pending pull's tree is partial, and
            // writing into it could block the paths it still has to update.
            // Before sync, so placements carry the IDs.
            if let Err(e) = Repo::open(&root).and_then(|repo| repo.assign_missing_ids()) {
                outcome.errors.push(format!("Item IDs: {e}"));
                ids_ready = false;
            }
        }
        let is_reviewed = reviewed.is_some();
        let mut conflicts: Vec<ConflictRef> = Vec::new();
        let snapshot = self.snapshot();
        if !pending
            && ids_ready
            && let Some(snap) = &snapshot
        {
            let planned = reviewed.map(Ok).unwrap_or_else(|| {
                Repo::open(&root).and_then(|repo| {
                    let actions = sync::plan(&repo, &harnesses, &snap.assignment, PlanOptions::default())?;
                    Ok((repo, actions.into_iter().map(|action| (action, None)).collect()))
                })
            });
            match planned {
                Ok((repo, actions)) => {
                    for (action, digest) in &actions {
                        if action.kind == ActionKind::Conflict {
                            continue;
                        }
                        match sync::apply_reviewed(&repo, action, digest.as_deref()) {
                            Ok(()) if is_reviewed => outcome.completed.push(placement_view(action)),
                            Ok(()) | Err(sk1ll_core::Error::Displaced { .. }) => {}
                            Err(sk1ll_core::Error::Changed(_)) if digest.is_some() => {
                                outcome.changed = Some(placement_view(action));
                                break;
                            }
                            Err(e) => outcome.errors.push(format!("sync {}: {e}", action.dest.display())),
                        }
                    }

                    let remaining = if outcome.changed.is_some() {
                        sync::plan(&repo, &harnesses, &snap.assignment, PlanOptions::default())?
                    } else {
                        actions.into_iter().map(|(action, _)| action).collect()
                    };
                    for action in remaining.iter().filter(|a| a.kind == ActionKind::Conflict) {
                        for item in conflict_items(action) {
                            let conflict = ConflictRef { item, harness: action.harness.clone() };
                            if !conflicts.contains(&conflict) {
                                conflicts.push(conflict);
                            }
                        }
                    }
                }
                Err(e) => outcome.errors.push(format!("sync: {e}")),
            }
        }

        for entry in sync::displaced::changed()? {
            let c = ConflictRef { item: displaced_item(&entry, &root), harness: entry.harness.clone() };
            if !conflicts.contains(&c) {
                conflicts.push(c);
            }
        }
        let mut report = repo_report(&root, harnesses.keys().cloned().collect())?;
        if pending {
            // The partial checkout is not a Draft. Keep the last complete
            // inventory while the journal is active; `repo_report` already
            // marks it incomplete.
            let previous = self.view.borrow().report.clone();
            report.drafts = previous.drafts;
            report.items = previous.items;
        }
        report.conflicts = conflicts;
        let awaiting = snapshot.is_none();
        let has_origin = !self.is_standalone() || gitx::has_origin(&root).unwrap_or(false);
        // The lifecycle derives the report's Blocked, pending and behind.
        let ((), saved) = self.publish(
            |lc| lc.set_pull(pull),
            |v| {
                v.repo_ready = true;
                v.has_origin = has_origin;
                v.report = report;
                v.awaiting_assignment = awaiting;
                if let Some(e) = outcome.errors.first() {
                    v.last_error = Some(crate::auth::redact(e));
                }
            },
        );
        if let Err(e) = saved {
            outcome.errors.push(format!("saving the daemon state: {e:#}"));
            self.update(|v| v.last_error = Some(format!("saving the daemon state: {e:#}")));
        }
        for e in &outcome.errors {
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
        Ok(outcome)
    }

    /// Where a refresh leaves the pull: what finishing a stopped pull
    /// (`finished`) found, else what was known, checked against the repo. The
    /// journal decides whether a pull is pending. A remembered block whose
    /// Baseline `HEAD` has reached since (a pull while the daemon was
    /// stopped) is over.
    fn pull_after_refresh(&self, root: &Path, finished: Option<PullState>, pending: bool) -> PullState {
        if let Some(pull) = finished {
            return pull;
        }
        match self.lifecycle().pull().clone() {
            PullState::Pending { items } if pending => PullState::Pending { items },
            _ if pending => PullState::Pending { items: Vec::new() },
            PullState::Pending { items } if items.is_empty() => PullState::Clear,
            PullState::Pending { items } => match gitx::remote_head(root) {
                Ok(Some(baseline)) => PullState::Blocked { baseline, items },
                _ => PullState::Clear,
            },
            PullState::Blocked { baseline, .. }
                if gitx::rev(root, "HEAD")
                    .ok()
                    .flatten()
                    .is_some_and(|head| gitx::is_ancestor(root, &baseline, &head).unwrap_or(false)) =>
            {
                PullState::Clear
            }
            pull => pull,
        }
    }

    /// What a pull or push can change among the things the report is made
    /// from: where `HEAD` and origin's branch are (the baseline, the unpushed
    /// items and the working tree a pull moves all follow from them) and
    /// where the pull stands.
    fn pull_inputs(&self) -> Result<PullInputs> {
        let root = self.repo_root();
        Ok(PullInputs {
            head: gitx::rev(root, "HEAD")?,
            remote: gitx::remote_head(root)?,
            pull: self.lifecycle().pull().clone(),
            pending: Repo::open_git_only(root)?.pull_pending()?,
        })
    }

    /// After file changes (or the periodic reconciliation): refresh, and if
    /// Blocked and the draft changed, try the pull again.
    pub(crate) async fn on_files_changed(self: &Arc<Self>) -> Result<()> {
        let drafted = self
            .blocking(|d, eng| {
                if d.idle() {
                    return Ok(false);
                }
                sync::displaced::prune()?;
                let before = d.view.borrow().report.drafts.clone();
                d.refresh(eng)?;
                Ok(d.view.borrow().report.drafts != before)
            })
            .await?;
        if drafted {
            self.retry_stopped_pull().await;
        }
        Ok(())
    }

    /// After a Draft change, which may have cleared what stopped the last
    /// pull: a Blocked Machine pulls again.
    async fn retry_stopped_pull(self: &Arc<Self>) {
        if self.lifecycle().pull_stopped() && (self.is_connected() || self.is_standalone()) {
            let _ = self.pull_refreshed().await;
        }
    }

    /// Fetches the server's baseline and moves onto it (draft carried along),
    /// pushes queued commits, and syncs.
    pub(crate) async fn pull(self: &Arc<Self>) -> Result<PullResult> {
        self.pull_with(false).await
    }

    /// A pull where the local state was refreshed just before (by the caller,
    /// or by the session on connecting): the sync after it is skipped if the
    /// pull moved nothing, since it would recompute the same report from the
    /// same inputs. Standalone always syncs.
    async fn pull_refreshed(self: &Arc<Self>) -> Result<PullResult> {
        self.pull_with(!self.is_standalone()).await
    }

    /// The fetch (or the first clone) and the push run without the engine
    /// mutex; moving onto what was fetched and the sync hold it. Each step
    /// under the mutex checks again that the daemon still manages the repo,
    /// and moves onto whatever was fetched last, whoever fetched it.
    async fn pull_with(self: &Arc<Self>, refreshed: bool) -> Result<PullResult> {
        let standalone = self.is_standalone();
        // `None`: refresh whatever happens.
        let before = self
            .blocking(move |d, eng| {
                d.ensure_managed()?;
                if standalone {
                    d.require_origin()?;
                }
                d.lock_repo(eng)?;
                Ok(if refreshed && !eng.stale { d.pull_inputs().ok() } else { None })
            })
            .await?;
        let result = match self.network(|d| d.fetch_baseline()).await {
            Ok(()) => {
                self.blocking(|d, eng| {
                    d.ensure_managed()?;
                    d.lock_repo(eng)?;
                    d.pull_inner(eng)
                })
                .await
            }
            Err(e) => Err(e),
        };
        let result = match result {
            Ok(r) if !standalone && !matches!(r, PullResult::Blocked { .. }) => self.push_pending().await.map(|_| r),
            result => result,
        };
        self.blocking(move |d, eng| {
            if d.idle() {
                return result;
            }
            // A failed pull may have stopped half-way: that always refreshes.
            // So does one that another operation ran beside.
            if !(result.is_ok() && before.is_some() && !eng.stale && before == d.pull_inputs().ok()) {
                d.refresh(eng)?;
            }
            match &result {
                Ok(r) => {
                    if let PullResult::Updated { to, .. } = r {
                        tracing::info!("baseline is now {to}");
                    }
                    if !matches!(r, PullResult::Blocked { .. }) {
                        d.update(|v| v.last_error = None);
                    }
                }
                Err(e) => d.set_error(format!("pull: {e}")),
            }
            result
        })
        .await
    }

    /// Without the engine mutex: clones the repo if it isn't there yet (a
    /// clone has fetched), else fetches origin's branches. Neither moves the
    /// working tree, `HEAD` or the index of an existing repo.
    fn fetch_baseline(&self) -> Result<()> {
        let root = self.repo_root();
        if !root.join(".git").exists() {
            gitx::ensure_clone(root, &self.ident.joined()?.repo_url(), &[])?;
            return Ok(());
        }
        Ok(Repo::open_git_only(root)?.fetch()?)
    }

    /// The local half of a pull: moves onto the baseline fetched last.
    fn pull_inner(&self, eng: &mut Engine) -> Result<PullResult> {
        let root = self.repo_root().to_path_buf();
        let repo = Repo::open_git_only(&root)?;
        let first = if repo.pull_pending()? {
            None
        } else {
            match gitx::rev(&root, "HEAD")? {
                None => Some(gitx::adopt_first_baseline(&root)?),
                Some(_) => gitx::adopt_first_baseline_over_init(&root)?,
            }
        };
        let (result, pull) = if let Some(first) = first {
            match first {
                gitx::FirstBaseline::Adopted(to) => (PullResult::Updated { from: None, to }, PullState::Clear),
                gitx::FirstBaseline::Empty => (PullResult::UpToDate, PullState::Clear),
                gitx::FirstBaseline::Blocked { baseline, items } => (
                    PullResult::Blocked { baseline: baseline.clone(), items: items.clone(), pending: false },
                    PullState::Blocked { baseline, items },
                ),
            }
        } else {
            let head = gitx::rev(&root, "HEAD")?;
            let lock = eng.lock.as_ref().ok_or_else(|| Error::Conflict(format!("{} is not locked", root.display())))?;
            match repo.pull_fetched(lock) {
                Ok(PullOutcome::UpToDate) => (PullResult::UpToDate, PullState::Clear),
                Ok(PullOutcome::Updated { from, to }) => {
                    (PullResult::Updated { from: Some(from), to }, PullState::Clear)
                }
                Ok(PullOutcome::Blocked { baseline, items }) => {
                    tracing::warn!("blocked: baseline {baseline} clashes with the draft");
                    let pending = repo.pull_pending()?;
                    let pull = if pending {
                        PullState::Pending { items: items.clone() }
                    } else {
                        PullState::Blocked { baseline: baseline.clone(), items: items.clone() }
                    };
                    (PullResult::Blocked { baseline, items, pending }, pull)
                }
                // The pull went through; a save made to a file it replaced
                // is in the recovery area, which the report lists as a conflict.
                Err(e @ sk1ll_core::Error::Displaced { .. }) if !repo.pull_pending()? => {
                    tracing::warn!("{e}");
                    let result = match gitx::rev(&root, "HEAD")? {
                        Some(to) if Some(&to) != head.as_ref() => PullResult::Updated { from: head, to },
                        _ => PullResult::UpToDate,
                    };
                    (result, PullState::Clear)
                }
                Err(e) => return Err(e.into()),
            }
        };
        let remote = gitx::remote_head(&root)?;
        self.note(|lc| {
            lc.set_pull(pull);
            if remote.is_some() {
                lc.set_server_baseline(remote);
            }
        });
        Ok(result)
    }

    /// Pushes committed commits the server doesn't have. What to push is
    /// checked under the engine mutex and pushed without it: exactly that
    /// commit, so one made meanwhile waits for the next push. A push the
    /// server rejected as not fast-forward is retried once after pulling its
    /// baseline; any other failure (unreachable, timed out) is returned right
    /// away.
    async fn push_pending(self: &Arc<Self>) -> Result<bool> {
        let mut retried = false;
        loop {
            let commit = self
                .blocking(|d, _| {
                    d.ensure_managed()?;
                    let root = d.repo_root();
                    if !gitx::has_unpushed(root)? {
                        return Ok(None);
                    }
                    Ok(Repo::open_git_only(root)?.prepare_push()?)
                })
                .await?;
            let Some(commit) = commit else { return Ok(false) };
            let pushed = self
                .network(move |d| {
                    let root = d.repo_root();
                    let pushed = Repo::open_git_only(root).and_then(|repo| repo.push_commit(&commit));
                    if pushed.is_ok() {
                        let remote = gitx::remote_head(root)?;
                        d.note(|lc| lc.set_server_baseline(remote));
                    }
                    Ok(pushed)
                })
                .await?;
            match pushed {
                Ok(()) => {
                    tracing::info!("pushed to the server");
                    return Ok(true);
                }
                Err(first) if !retried && is_rejected_push(&first) => {
                    tracing::info!("push rejected ({first}); pulling first");
                    retried = true;
                    self.network(|d| d.fetch_baseline()).await?;
                    let pulled = self
                        .blocking(|d, eng| {
                            d.ensure_managed()?;
                            d.pull_inner(eng)
                        })
                        .await?;
                    if let PullResult::Blocked { items, .. } = pulled {
                        let names: Vec<String> = items.iter().map(ToString::to_string).collect();
                        return Err(Error::Conflict(format!(
                            "can't push: the server's baseline clashes with {}; commit or restore them",
                            names.join(", ")
                        )));
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// `POST /push`: pushes committed commits now.
    pub(crate) async fn push(self: &Arc<Self>) -> Result<PushResult> {
        self.blocking(|d, eng| {
            d.ensure_managed()?;
            d.lock_repo(eng)?;
            if d.is_standalone() {
                d.require_origin()?;
            }
            Ok(())
        })
        .await?;
        let result = self.push_pending().await;
        self.blocking(|d, eng| d.refresh(eng)).await?;
        match result {
            Ok(pushed) => Ok(PushResult { pushed }),
            Err(Error::Internal(e)) => {
                Err(Error::Unavailable(format!("push: {}", crate::auth::redact(&format!("{e:#}")))))
            }
            Err(e) => Err(e),
        }
    }

    /// On (re)connecting: push what's queued and pull.
    pub(crate) async fn on_connected(self: &Arc<Self>) -> Result<()> {
        if self.idle() {
            return Ok(());
        }
        // The session refreshed before its first status, moments ago.
        self.pull_refreshed().await.map(drop)
    }

    /// A reader: no engine mutex.
    pub(crate) fn commit_message(&self, items: &[String]) -> Result<CommitMessage> {
        let repo = Repo::open_git_only(self.repo_root())?;
        let draft = repo.draft()?;
        let items = items.iter().map(|i| ItemRef::resolve(i, &draft)).collect::<Result<Vec<_>, _>>()?;
        let message = default_commit_message(&items, self.ident.machine_id());
        Ok(CommitMessage { items, message })
    }

    /// Commits under the engine mutex; a joined daemon then pushes, and the
    /// state is refreshed once the push is done.
    pub(crate) async fn commit(self: &Arc<Self>, req: CommitRequest) -> Result<CommitResponse> {
        let mut response = self.blocking(move |d, eng| d.commit_locally(eng, req)).await?;
        if !self.is_standalone() {
            if self.is_connected() {
                match self.push_pending().await {
                    Ok(_) => response.pushed = true,
                    Err(e) => response.push_error = Some(crate::auth::redact(&e.to_string())),
                }
            } else {
                response.push_error = Some("offline: queued until the server is reachable".to_string());
            }
        }
        let push_error = response.push_error.clone();
        self.blocking(move |d, eng| {
            d.refresh(eng)?;
            if let Some(e) = &push_error {
                d.set_error(format!("push: {e}"));
            }
            Ok(())
        })
        .await?;
        self.retry_stopped_pull().await;
        Ok(response)
    }

    fn commit_locally(&self, eng: &mut Engine, req: CommitRequest) -> Result<CommitResponse> {
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        let repo = Repo::open_git_only(self.repo_root())?;
        let draft = repo.draft()?;
        let items = req.items.iter().map(|i| ItemRef::resolve(i, &draft)).collect::<Result<Vec<_>, _>>()?;
        if items.is_empty() {
            return Err(Error::BadRequest("name the draft items to commit".into()));
        }
        let id = self.ident.machine_id().to_string();
        let message =
            req.message.filter(|m| !m.trim().is_empty()).unwrap_or_else(|| default_commit_message(&items, &id));
        let author = match req.author {
            Some(a) => Some(a),
            None if !repo.has_git_identity()? => Some(machine_author(&id)),
            None => None,
        };
        let sha = repo.commit(&items, &message, author.as_deref())?;
        tracing::info!("committed {} as {sha}", items.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "));
        Ok(CommitResponse { sha, items, message, pushed: false, push_error: None })
    }

    /// Gives each item whose Item ID is a duplicate a new one, then refreshes.
    pub(crate) fn repair_ids(&self, eng: &mut Engine) -> Result<usize> {
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        let count = Repo::open(self.repo_root())?.repair_ids()?;
        self.refresh(eng)?;
        Ok(count)
    }

    /// Restores draft items and refreshes the local state.
    pub(crate) async fn restore(self: &Arc<Self>, req: RestoreRequest) -> Result<RestoreResponse> {
        let response = self.blocking(move |d, eng| d.restore_locally(eng, req)).await?;
        self.retry_stopped_pull().await;
        Ok(response)
    }

    fn restore_locally(&self, eng: &mut Engine, req: RestoreRequest) -> Result<RestoreResponse> {
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        let repo = Repo::open_git_only(self.repo_root())?;
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
        self.refresh(eng)?;
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
        let head_paths = if head == baseline { baseline_paths.clone() } else { paths(head.as_deref())? };
        let mut inventory = BTreeMap::<ItemRef, ItemView>::new();
        let entry = |item: ItemRef| ItemView {
            item,
            id: None,
            in_baseline: false,
            draft: None,
            unpushed: false,
            description: None,
        };
        for path in &baseline_paths {
            let item = ItemRef::for_path(Path::new(path));
            if matches!(item, ItemRef::Skill(_) | ItemRef::Rule(_)) {
                inventory.entry(item.clone()).or_insert_with(|| entry(item)).in_baseline = true;
            }
        }
        let repo = Repo::open_git_only(root)?;
        for draft in repo.draft()? {
            let change = match draft.change {
                sk1ll_core::Change::Added => ChangeKind::Added,
                sk1ll_core::Change::Modified => ChangeKind::Modified,
                sk1ll_core::Change::Deleted => ChangeKind::Deleted,
            };
            inventory.entry(draft.item.clone()).or_insert_with(|| entry(draft.item)).draft = Some(change);
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
        let claims_at = |revision: Option<&str>, paths: &[String]| -> BTreeMap<_, _> {
            revision
                .map(|rev| sk1ll_core::item_id::committed_inventory(root, rev, paths).items)
                .unwrap_or_default()
                .into_iter()
                .collect()
        };
        let baseline_ids = claims_at(baseline.as_deref(), &baseline_paths);
        let head_ids = if head == baseline { baseline_ids.clone() } else { claims_at(head.as_deref(), &head_paths) };
        // Malformed metadata and duplicate Item IDs (until `repair-ids`) are
        // errors here. The report to the server stays partial instead.
        sk1ll_core::content::item_ids_on_disk(root)?;
        let on_disk: BTreeMap<_, _> = sk1ll_core::item_id::working_inventory(root).items.into_iter().collect();
        for item in on_disk.keys() {
            inventory.entry(item.clone()).or_insert_with(|| entry(item.clone()));
        }
        let baseline_item_ids: BTreeSet<_> = baseline_ids.values().filter_map(Option::as_ref).collect();
        let mut historical = Vec::new();
        for value in inventory.values_mut() {
            let old = baseline_ids.get(&value.item);
            let committed = head_ids.get(&value.item);
            let working = on_disk.get(&value.item);
            value.id = working.or(committed).or(old).cloned().flatten();
            value.in_baseline = match value.id.as_ref() {
                Some(id) => baseline_item_ids.contains(id),
                None => old.is_some_and(|id| id.is_none()),
            };
            if let (Some(Some(old)), Some(current)) = (old, working.or(committed))
                && current.as_ref() != Some(old)
            {
                let mut previous = entry(value.item.clone());
                previous.id = Some(old.clone());
                previous.in_baseline = true;
                previous.draft = (working != committed).then_some(ChangeKind::Deleted);
                previous.unpushed = committed != Some(&previous.id);
                historical.push(previous);
                if working != committed {
                    value.draft = Some(ChangeKind::Added);
                }
            }
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
        let mut by_id = BTreeMap::new();
        for value in historical.into_iter().chain(inventory.into_values()) {
            let key = (value.id.clone(), value.id.is_none().then(|| value.item.clone()));
            match by_id.entry(key) {
                std::collections::btree_map::Entry::Occupied(mut item) => {
                    if value.draft != Some(ChangeKind::Deleted) {
                        item.insert(value);
                    }
                }
                std::collections::btree_map::Entry::Vacant(item) => {
                    item.insert(value);
                }
            }
        }
        let mut items: Vec<_> = by_id.into_values().collect();
        items.sort_by(|a, b| a.item.cmp(&b.item).then(a.id.cmp(&b.id)));
        Ok(items)
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
        Ok(DiffView { item, diff, target: None })
    }

    /// The current plan; `force` plans as if overwriting. For engine
    /// operations: interrupted imports are put back first.
    fn plan(&self, force: bool) -> Result<(Repo, Vec<Action>)> {
        let Some(snap) = self.snapshot() else {
            return Err(Error::Conflict(
                "waiting for the server's assignment; the harnesses are left alone until it arrives".into(),
            ));
        };
        Self::recover_imports(self.repo_root())?;
        self.plan_as_is(&snap, force)
    }

    /// The plan for the repo as it is, changing nothing: what a reader may
    /// run. An import that an interrupted update left set aside is not put
    /// back here (that needs the engine mutex and the repo's engine lock), so
    /// until the next refresh has done it this plan sees it as missing.
    fn plan_as_is(&self, snap: &AssignmentSnapshot, force: bool) -> Result<(Repo, Vec<Action>)> {
        let repo = Repo::open(self.repo_root())?;
        Self::require_sync_ready(&repo)?;
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
        let root = self.repo_root();
        Ok(sync::displaced::list()?.into_iter().map(|entry| displaced_view(entry, root)).collect())
    }

    /// A reader: no engine mutex, and so no recovery of interrupted imports
    /// (see [`Daemon::plan_as_is`]).
    pub(crate) fn conflicts(&self) -> Result<Vec<ConflictView>> {
        let mut result: Vec<_> = if self.view.borrow().report.pull_pending.is_none()
            && let Some(snap) = self.snapshot()
        {
            let (_, actions) = self.plan_as_is(&snap, false)?;
            actions.iter().filter(|a| a.kind == ActionKind::Conflict).map(conflict_view).collect()
        } else {
            Vec::new()
        };
        let root = self.repo_root();
        for entry in sync::displaced::changed()? {
            result.push(displaced_view(entry, root));
        }
        Ok(result)
    }

    fn current_assignment(&self) -> Result<sk1ll_core::Assignment> {
        if self.is_standalone() {
            return Ok(sk1ll_core::Assignment::load(&self.cfg.assignment)?);
        }
        self.snapshot().map(|snap| snap.assignment.clone()).ok_or_else(|| {
            Error::Conflict("waiting for the server's assignment; the harnesses are left alone until it arrives".into())
        })
    }

    pub(crate) fn preview_sync(&self, force: bool) -> Result<SyncResult> {
        if self.view().revoked {
            return Err(Error::Conflict(
                "this machine was removed from the fleet; the daemon no longer manages it".into(),
            ));
        }
        let repo = Repo::open(self.repo_root())?;
        Self::require_sync_ready(&repo)?;
        let harnesses = MachineConfig::load(&self.cfg.machine_config)?.harnesses();
        let actions = sync::plan(&repo, &harnesses, &self.current_assignment()?, PlanOptions { force })?;
        let sources = Self::sync_source_digests(&actions, sync::deploy_hash)?;
        Self::sync_preview_result(&actions, &sources)
    }

    fn require_sync_ready(repo: &Repo) -> Result<()> {
        if repo.pull_pending()? {
            return Err(Error::Conflict("pull pending. Finish `sk1ll pull` before syncing harnesses".into()));
        }
        Ok(())
    }

    fn sync_source_digests(
        actions: &[Action],
        mut hash: impl FnMut(&Path) -> sk1ll_core::Result<String>,
    ) -> Result<Vec<Option<String>>> {
        let mut digests = BTreeMap::new();
        Ok(actions
            .iter()
            .map(|action| match (&action.kind, &action.item) {
                (
                    ActionKind::Create | ActionKind::Update,
                    sync::Item::Skill { mode: sk1ll_core::LinkMode::Copy, source: Some(path), .. },
                ) => Some(path),
                _ => None,
            })
            .map(|path| {
                path.map(|path| {
                    use std::collections::btree_map::Entry;
                    let digest = match digests.entry(path) {
                        Entry::Occupied(entry) => entry.into_mut(),
                        Entry::Vacant(entry) => entry.insert(hash(path)?),
                    };
                    Ok(digest.clone())
                })
                .transpose()
            })
            .collect::<sk1ll_core::Result<Vec<_>>>()?)
    }

    fn sync_preview_result(actions: &[Action], sources: &[Option<String>]) -> Result<SyncResult> {
        let serialized = serde_json::to_string(&(actions, sources)).map_err(|e| Error::Internal(e.into()))?;
        Ok(SyncResult {
            fingerprint: Some(sk1ll_core::hash::hash_str(&serialized)),
            conflicts: actions.iter().filter(|a| a.kind == ActionKind::Conflict).map(conflict_view).collect(),
            actions: Some(actions.iter().map(placement_view).collect()),
            completed: Vec::new(),
            changed: None,
            errors: Vec::new(),
        })
    }

    pub(crate) fn render_instructions(&self, name: &str) -> Result<Vec<InstructionPreview>> {
        let repo = Repo::open(self.repo_root())?;
        let harnesses = MachineConfig::load(&self.cfg.machine_config)?.harnesses();
        let files = sync::render_instructions(&repo, &harnesses, &self.current_assignment()?, name)?;
        let format = harnesses[name].instructions_format;
        files
            .into_iter()
            .map(|file| {
                let (before, next_content) = sync::instruction_file_text(&file, format)?;
                let diff = gitx::diff_text(before.as_deref(), &next_content)?;
                Ok(InstructionPreview {
                    path: file.path,
                    format,
                    content: file.content,
                    differs: before.as_deref() != Some(&next_content),
                    next_content,
                    diff,
                })
            })
            .collect()
    }

    /// `POST /sync`: re-syncs now; `force` also overwrites what a forced plan
    /// may overwrite (hand edits inside managed blocks).
    pub(crate) fn sync(&self, eng: &mut Engine, force: bool, fingerprint: Option<&str>) -> Result<SyncResult> {
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        Self::require_sync_ready(&Repo::open_git_only(self.repo_root())?)?;
        let reviewed = if let Some(fingerprint) = fingerprint {
            if self.is_standalone() {
                self.reload_local_assignment()?;
            }
            let snapshot =
                self.snapshot().ok_or_else(|| Error::Conflict("waiting for the server's assignment".into()))?;
            let root = self.repo_root();
            let lock = eng.lock.as_ref().ok_or_else(|| Error::Conflict(format!("{} is not locked", root.display())))?;
            let harnesses = MachineConfig::load(&self.cfg.machine_config)?.harnesses();
            // Recovery may restore a source or change a placement. Compare the
            // plan for the recovered state, before applying any placements.
            sync::recover(Some(lock), &harnesses)?;
            Self::recover_imports(root)?;
            let repo = Repo::open(root)?;
            repo.assign_missing_ids()?;
            let actions = sync::plan(&repo, &harnesses, &snapshot.assignment, PlanOptions { force })?;
            let sources = Self::sync_source_digests(&actions, sync::deploy_hash)?;
            let preview = Self::sync_preview_result(&actions, &sources)?;
            if preview.fingerprint.as_deref() != Some(fingerprint) {
                return Ok(preview);
            }
            #[cfg(test)]
            if let Some(after_review) = eng.after_sync_review.take() {
                after_review();
            }
            Some((repo, actions.into_iter().zip(sources).collect()))
        } else {
            None
        };
        if force && reviewed.is_none() {
            Repo::open(self.repo_root())?.assign_missing_ids()?;
            let (repo, actions) = self.plan(true)?;
            for a in actions.iter().filter(|a| a.kind != ActionKind::Conflict) {
                sync::apply(&repo, a)?;
            }
        }
        let RefreshOutcome { errors, completed, changed } = self.refresh_with_plan(eng, reviewed)?;
        self.event(Event::Status);
        if changed.is_some() {
            let mut preview = self.preview_sync(force)?;
            preview.errors = errors;
            preview.completed = completed;
            preview.changed = changed;
            return Ok(preview);
        }
        Ok(SyncResult {
            fingerprint: None,
            conflicts: self.conflicts()?,
            actions: None,
            errors,
            completed: Vec::new(),
            changed: None,
        })
    }

    pub(crate) fn resolve_displaced(&self, eng: &mut Engine, id: &str, adopt: bool) -> Result<Option<PathBuf>> {
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        let result = if adopt {
            let root = self.repo_root();
            let lock = eng.lock.as_ref().ok_or_else(|| Error::Conflict(format!("{} is not locked", root.display())))?;
            sync::displaced::adopt(&Repo::open_git_only(root)?, id, lock).map(Some)
        } else {
            sync::displaced::discard_from(&Repo::open_git_only(self.repo_root())?, id).map(|()| None)
        };
        self.refresh(eng)?;
        self.event(Event::Status);
        if adopt {
            self.event(Event::Draft);
        }
        Ok(result?)
    }

    fn find_conflict<'a>(actions: &'a [Action], t: &ConflictTarget) -> Result<&'a Action> {
        actions.iter().find(|a| a.kind == ActionKind::Conflict && matches_target(a, t)).ok_or_else(|| {
            Error::NotFound(format!("no conflict for `{}` in `{}` (see GET /conflicts)", t.name, t.harness))
        })
    }

    /// Takes the harness's content into the draft. Adopting an unmanaged
    /// skill also enables it where it was found: the Assignment write runs
    /// without the engine mutex, between publishing the skill and removing
    /// the copies it came from.
    pub(crate) async fn adopt(self: &Arc<Self>, t: ConflictTarget) -> Result<()> {
        let Some(pending) = self.blocking(move |d, eng| d.adopt_locally(eng, t)).await? else {
            self.retry_stopped_pull().await;
            return Ok(());
        };
        let write = AssignmentWrite::Changes(pending.changes.clone());
        let written = self.write_assignment(write).await;
        self.blocking(move |d, eng| d.finish_adoption(eng, pending, written)).await
    }

    /// Adopts a conflict (`None`), or publishes an unmanaged skill into the
    /// draft and returns the Assignment write that enables it.
    fn adopt_locally(&self, eng: &mut Engine, t: ConflictTarget) -> Result<Option<PendingAdoption>> {
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        let repo = Repo::open(self.repo_root())?;
        let harnesses = MachineConfig::load(&self.cfg.machine_config)?.harnesses();
        let unmanaged = sync::unmanaged::discover(&repo, &harnesses)?
            .iter()
            .any(|s| s.name == t.name && s.variants.iter().any(|v| v.locations.iter().any(|l| l.harness == t.harness)));
        let adopted = if unmanaged { None } else { sync::unmanaged::adopted(&repo, &harnesses, &t.harness, &t.name)? };
        if !unmanaged && adopted.is_none() {
            let (repo, actions) = self.plan(false)?;
            sync::adopt(&repo, Self::find_conflict(&actions, &t)?)?;
            tracing::info!("adopted {} from {} into the draft", t.name, t.harness);
            self.refresh(eng)?;
            return Ok(None);
        }
        if !self.is_standalone() && !self.is_connected() {
            return Err(Error::Unavailable("Adopt needs a connection to the Server".into()));
        }
        let adoption = match adopted {
            Some(adoption) => adoption,
            None => sync::unmanaged::select(&repo, &harnesses, &t.harness, &t.name)?,
        };
        let created = repo.personal_dir().join(&adoption.name).symlink_metadata().is_err();
        sync::unmanaged::publish(&repo, &adoption)?;
        // Its toggles are keyed by Item ID: one published before (by
        // hand, or by an interrupted Adopt) may still lack it.
        if !repo.pull_pending()? {
            repo.assign_missing_ids()?;
        }
        let id = Self::adopted_id(&repo, &adoption)?
            .ok_or_else(|| Error::Conflict(format!("Adopt did not give skill:{} an Item ID", adoption.name)))?;
        let changes = sk1ll_proto::AssignmentChanges {
            changes: adoption
                .locations
                .iter()
                .map(|location| sk1ll_proto::ToggleChange {
                    harness: location.harness.clone(),
                    kind: sk1ll_proto::ToggleKind::Skill,
                    item_id: Some(id.clone()),
                    name: Some(adoption.name.clone()),
                    enabled: Some(true),
                })
                .collect(),
            ..Default::default()
        };
        // Publish the draft before enabling it. On a lost reply it stays
        // available to retry with the same ID. Original copies stay until
        // the Assignment has been confirmed.
        self.refresh(eng)?;
        Ok(Some(PendingAdoption { adoption, created, id, changes }))
    }

    /// The Item ID of an adopted skill as the repo has it now.
    fn adopted_id(repo: &Repo, adoption: &sync::unmanaged::Adoption) -> Result<Option<sk1ll_core::ItemId>> {
        let item = ItemRef::Skill(adoption.name.clone());
        repo.item_ids()?
            .into_iter()
            .find_map(|(name, id)| (name == item).then_some(id))
            .ok_or_else(|| Error::NotFound(format!("Adopt did not create {item}")))
    }

    /// After the Assignment write: removes the copies an enabled adoption
    /// came from, or rolls back a refused one. If the published skill no
    /// longer has the Item ID that was enabled (changed while the write was
    /// in flight), the copies stay where they are and it is a conflict.
    fn finish_adoption(
        &self,
        eng: &mut Engine,
        pending: PendingAdoption,
        written: Result<AssignmentUpdate>,
    ) -> Result<()> {
        let PendingAdoption { mut adoption, created, id, .. } = pending;
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        let repo = Repo::open(self.repo_root())?;
        if let Err(error) = written {
            if created && matches!(error, Error::Upstream(_, _)) {
                sync::unmanaged::rollback(&repo, &adoption)?;
            }
            self.refresh(eng)?;
            self.event(Event::Draft);
            return Err(error);
        }
        if Self::adopted_id(&repo, &adoption).ok().flatten().as_ref() != Some(&id) {
            self.refresh(eng)?;
            return Err(Error::Conflict(format!(
                "skill:{} changed while its Assignment was being written; its copies in the harnesses are kept, \
                 adopt it again",
                adoption.name
            )));
        }
        let harnesses = MachineConfig::load(&self.cfg.machine_config)?.harnesses();
        let snapshot = self.snapshot();
        adoption.locations.retain(|l| snapshot.as_ref().is_some_and(|s| s.assignment.skill_enabled(&l.harness, &id)));
        sync::unmanaged::complete(&repo, &harnesses, &adoption)?;
        self.refresh(eng)?;
        self.event(Event::Draft);
        self.event(Event::Fleet);
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

    pub(crate) fn write_skill_file(&self, eng: &mut Engine, req: &crate::client::WriteFileRequest) -> Result<()> {
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        if !sk1ll_core::content::write_file(&Repo::open(self.repo_root())?, req)? {
            return Err(Error::Conflict("File changed on disk".into()));
        }
        self.refresh(eng)
    }

    /// Replaces the harness's content with what the draft says: a forced plan,
    /// applied for this one item only.
    pub(crate) fn overwrite(&self, eng: &mut Engine, t: ConflictTarget) -> Result<()> {
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        let (_, actions) = self.plan(false)?;
        let conflict = Self::find_conflict(&actions, &t)?;
        let (repo, forced) = self.plan(true)?;
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
    /// During first-run setup, opt-in presets found here read as on: setup
    /// takes them up when it ends (see [`MachineConfig::setup_choices`]).
    /// Writes nothing.
    pub(crate) fn setup_harnesses(&self) -> Result<Vec<HarnessChoice>> {
        if self.is_standalone() && !self.repo_root().join(".git").exists() {
            return Ok(MachineConfig::setup_choices(&self.cfg.machine_config, self.repo_root())?);
        }
        Ok(MachineConfig::load(&self.cfg.machine_config)?.choices())
    }

    /// Only before setup ends, when sync hasn't placed anything yet. Later,
    /// [`Daemon::apply_harness`] shows and handles what a change leaves
    /// behind. Takes the engine so it can't race Init or a join.
    pub(crate) fn set_setup_harness(&self, _eng: &mut Engine, name: &str, enabled: bool) -> Result<Vec<HarnessChoice>> {
        if !self.is_standalone() || self.repo_root().join(".git").exists() {
            return Err(Error::Conflict(format!(
                "harnesses can be chosen here only during first-run setup; use POST /harnesses/apply \
                 (Settings or `sk1ll harnesses`), or edit {}",
                self.cfg.machine_config.display()
            )));
        }
        if !self.setup_harnesses()?.iter().any(|c| c.name == name) {
            return Err(Error::NotFound(format!("no harness `{name}`")));
        }
        MachineConfig::set_enabled(&self.cfg.machine_config, name, enabled)?;
        self.setup_harnesses()
    }

    /// A reader: no engine mutex.
    pub(crate) fn harness_settings(&self) -> Result<HarnessSettings> {
        let harnesses = MachineConfig::load(&self.cfg.machine_config)?.choices();
        Ok(HarnessSettings { path: self.cfg.machine_config.clone(), harnesses })
    }

    /// A reader: plans the change without making it.
    pub(crate) fn preview_harness(&self, change: &HarnessChange) -> Result<HarnessPreview> {
        let repo = Repo::open(self.repo_root())?;
        let snapshot = self.snapshot();
        let plan =
            harness_change::plan(&repo, &self.cfg.machine_config, change, snapshot.as_ref().map(|s| &s.assignment))?;
        Ok(HarnessPreview {
            fingerprint: plan.fingerprint,
            path: self.cfg.machine_config.clone(),
            name: change.name().to_string(),
            before: plan.before,
            after: plan.after,
            placed: plan.placed.iter().map(placement_view).collect(),
            warnings: plan.warnings,
            sync: plan.sync.iter().map(placement_view).collect(),
            awaiting_assignment: snapshot.is_none(),
        })
    }

    /// Changes `machine.toml` and syncs; what sk1ll placed where the change
    /// stops syncing stays. A joined machine reports its new set of
    /// harnesses to the server with the next status.
    pub(crate) fn apply_harness(&self, eng: &mut Engine, req: HarnessChangeRequest) -> Result<HarnessApplied> {
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        let repo = Repo::open(self.repo_root())?;
        let lock = eng.lock.as_ref().ok_or_else(|| Error::Conflict("the repo is not locked".into()))?;
        let fingerprint = req
            .fingerprint
            .as_deref()
            .ok_or_else(|| Error::BadRequest("this client is older than the daemon; update it".into()))?;
        let plan = harness_change::apply(lock, &repo, &self.cfg.machine_config, &req.change, fingerprint)?;
        tracing::info!("harness change {:?}: {} placed item(s) left in place", req.change, plan.placed.len());
        let errors = match self.refresh_reporting(eng) {
            Ok(failed) => failed,
            Err(e) => vec![format!("sync: {e}")],
        };
        self.event(Event::Status);
        Ok(HarnessApplied { harnesses: MachineConfig::load(&self.cfg.machine_config)?.choices(), errors })
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
        let UpdateImportsRequest { name, force, expected } = req;
        let (root, todo) = self
            .blocking(move |d, _| {
                d.ensure_managed()?;
                let todo = Repo::open(d.repo_root())?.update_targets(name.as_deref(), force)?;
                Ok((d.repo_root().to_path_buf(), todo))
            })
            .await?;
        let fetched = unlocked(move || imports::fetch_updates(&root, todo)).await?;
        if let Some(expected) = &expected {
            fetched.check_commit(expected)?;
        }
        self.blocking(move |d, eng| {
            d.ensure_managed()?;
            d.lock_repo(eng)?;
            let reports = Repo::open(d.repo_root())?.apply_updates(fetched, force);
            d.refresh(eng)?;
            Ok(reports?.into_iter().map(import_updated).collect())
        })
        .await
    }

    /// Re-vendors one import into the draft.
    pub(crate) async fn update_import(
        self: &Arc<Self>,
        name: String,
        force: bool,
        expected: Option<String>,
    ) -> Result<ImportUpdated> {
        let req = UpdateImportsRequest { name: Some(name), force, expected };
        self.update_imports(req).await?.into_iter().next().context("no update report").map_err(Error::Internal)
    }

    pub(crate) async fn import_changes(
        self: &Arc<Self>,
        name: String,
        upstream: Option<String>,
    ) -> Result<imports::ImportChanges> {
        let (root, targets) = self
            .blocking(move |d, _| {
                let root = d.import_root()?;
                let targets = Repo::open(&root)?.update_targets(Some(&name), true)?;
                Ok((root, targets))
            })
            .await?;
        unlocked(move || imports::fetch_changes(&root, targets, upstream.as_deref())).await
    }

    /// What a link leads to; only the import cache is written.
    pub(crate) async fn resolve_import(self: &Arc<Self>, req: ResolveImportRequest) -> Result<ResolvedSource> {
        let root = self.blocking(|d, _| d.import_root()).await?;
        let fetched = unlocked(move || {
            imports::fetch_link(&root, &req.link, given(&req.path), given(&req.git_ref), None)?.inspect()
        })
        .await?;
        // `taken` reflects the repo after the fetch, not before it.
        let r = self
            .blocking(move |d, _| {
                d.ensure_managed()?;
                Ok(Repo::open(d.repo_root())?.with_taken(fetched.resolved))
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
                total_bytes: c.total_bytes,
                warnings: c.warnings,
            })
            .collect();
        Ok(ResolvedSource { source: r.source, git_ref: r.git_ref, commit: r.commit, candidates })
    }

    /// Adds an import, from a link or plain source, to the draft.
    pub(crate) async fn add_import(self: &Arc<Self>, req: AddImportRequest) -> Result<ImportUpdated> {
        let root = self.blocking(|d, _| d.import_root()).await?;
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
            d.ensure_managed()?;
            d.lock_repo(eng)?;
            let added = Repo::open(d.repo_root())?.add_fetched(given(&name), fetched);
            d.refresh(eng)?;
            Ok(import_updated(added?))
        })
        .await
    }

    /// The repo an import operation fetches for, if the daemon manages it.
    fn import_root(&self) -> Result<PathBuf> {
        self.ensure_managed()?;
        Ok(self.repo_root().to_path_buf())
    }

    /// Removes an import (a draft change).
    pub(crate) fn remove_import(&self, eng: &mut Engine, name: &str) -> Result<()> {
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        let removed = Repo::open(self.repo_root())?.remove_import(name);
        self.refresh(eng)?;
        Ok(removed?)
    }

    /// Turns an import into a personal skill (a draft change).
    pub(crate) fn fork(&self, eng: &mut Engine, name: &str) -> Result<()> {
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        let forked = Repo::open(self.repo_root())?.fork(name);
        self.refresh(eng)?;
        Ok(forked?)
    }

    /// Stores `snap` and syncs, unless it is older than what was applied. With
    /// no stored snapshot, any snapshot is taken (also one of the same
    /// revision as before), since the harnesses wait for it.
    pub(crate) fn apply_assignment(&self, eng: &mut Engine, snap: AssignmentSnapshot) -> Result<Vec<String>> {
        if self.idle() {
            return Ok(Vec::new());
        }
        if let Some(have) = self.snapshot()
            && !newer(have.revision, snap.revision)
        {
            // The same revision arrives on every connect; only an older one is news.
            if snap.revision < have.revision {
                tracing::info!("ignoring stale assignment revision {} (have {})", snap.revision, have.revision);
            }
            if snap.revision == have.revision && eng.assignment_placement_failed {
                let result = self.refresh_reporting(eng);
                return Ok(self.assignment_placement_outcome(eng, result));
            }
            return Ok(Vec::new());
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
        let result = self.refresh_reporting(eng);
        Ok(self.assignment_placement_outcome(eng, result))
    }

    /// The server's baseline is now `baseline`: pull unless already on it. A
    /// nudge for the baseline the machine is on (the server sends one on every
    /// connect) says nothing about the local state, so it costs no refresh.
    pub(crate) async fn nudge(self: &Arc<Self>, baseline: String) -> Result<()> {
        let pull = self
            .blocking(move |d, eng| {
                if d.idle() {
                    return Ok(false);
                }
                d.note(|lc| lc.set_server_baseline(Some(baseline.clone())));
                d.event(Event::Fleet);
                let current = gitx::baseline(d.repo_root()).unwrap_or(None);
                if current.as_deref() != Some(baseline.as_str()) || d.lifecycle().pull_stopped() {
                    return Ok(true);
                }
                if eng.stale {
                    d.refresh(eng)?;
                }
                Ok(false)
            })
            .await?;
        // Not `pull_refreshed`: the last refresh may be long ago.
        if pull {
            self.pull().await?;
        }
        Ok(())
    }

    /// Stops managing this machine. Harness files are left as they are.
    pub(crate) fn revoke(&self, _eng: &mut Engine, why: &str) -> Result<()> {
        if self.is_standalone() {
            return Err(Error::Conflict("a standalone machine cannot be revoked".into()));
        }
        let lifecycle = self.lifecycle();
        if lifecycle.revoked() || lifecycle.halted() {
            return Ok(());
        }
        tracing::warn!("revoked: {why}; no longer managing this machine (its files are left as they are)");
        self.transition(Lifecycle::revoke)?;
        self.roots.send_replace(Vec::new());
        self.update(|v| v.last_error = Some(why.to_string()));
        Ok(())
    }

    /// Whether `POST /join` may join again: the machine is standalone, was
    /// removed, or the server rejects its token.
    pub(crate) fn can_rejoin(&self) -> bool {
        lock(&self.lifecycle).can_rejoin()
    }

    /// Validate before registration and again under the engine lock afterwards.
    pub(crate) fn check_standalone_join_request(&self, repo: &Path) -> Result<()> {
        if self.lifecycle().halted() {
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

    /// Second half of a standalone `POST /join`: the Fleet's repo is cloned
    /// privately (or the existing clone checked against the server) without
    /// the engine mutex, then published, the credentials stored and the
    /// daemon halted until the restart, under it.
    pub(crate) async fn join_standalone(self: &Arc<Self>, reg: Registered) -> Result<Joined> {
        let repo = reg.state().repo.clone();
        self.blocking(move |d, eng| {
            d.check_standalone_join_request(&repo)?;
            d.lock_repo(eng)
        })
        .await?;
        let (reg, prepared) = self
            .network(move |_| {
                let prepared = reg.prepare_standalone_repo().map_err(|e| Error::Conflict(format!("{e:#}")));
                Ok((reg, prepared))
            })
            .await?;
        let prepared = prepared?;
        self.blocking(move |d, eng| d.activate_standalone_join(eng, reg, prepared)).await
    }

    fn activate_standalone_join(
        &self,
        eng: &mut Engine,
        reg: Registered,
        mut prepared: crate::join::StandaloneRepo,
    ) -> Result<Joined> {
        self.check_standalone_join_request(&reg.state().repo)?;
        self.lock_repo(eng)?;
        // A clone used in place was checked before its fetch: check again
        // that no draft or commit appeared since. A replacement's publication
        // checks the original itself.
        if !prepared.replaces() {
            gitx::check_standalone_join(self.repo_root(), &reg.state().repo_url())
                .map_err(|e| Error::Conflict(format!("{e:#}")))?;
        }
        let mut activated = Ok(());
        self.halt_with(|| {
            activated = prepared.publish().and_then(|()| reg.persist_standalone(&self.cfg));
            match &activated {
                Ok(()) => Ok(true),
                Err(_) => match prepared.rollback() {
                    Ok(()) => Ok(false),
                    // If the replacement is still live, retain its lock. Stop
                    // all changes when concurrent content prevents a safe
                    // restoration.
                    Err(recovery) => {
                        if let Some(lock) = prepared.take_live_lock() {
                            eng.lock = Some(lock);
                        }
                        Err(recovery)
                    }
                },
            }
        })
        .map_err(|recovery| {
            let error = activated.as_ref().err().map_or_else(String::new, |e| format!("{e:#}"));
            Error::Internal(anyhow::anyhow!("{error}; rollback failed: {recovery:#}"))
        })?;
        activated.map_err(Error::Internal)?;
        if let Some(lock) = prepared.take_live_lock() {
            eng.lock = Some(lock);
        }
        Ok(prepared.joined)
    }

    /// Runs `f` with the lifecycle mutex held, halting the daemon if it
    /// returns `Ok(true)`. The state file writes take that mutex too, so none
    /// of the old identity can follow new credentials `f` stored. An `Err`
    /// also halts it: nothing may touch the repo after a failed recovery.
    fn halt_with(&self, f: impl FnOnce() -> anyhow::Result<bool>) -> anyhow::Result<()> {
        let mut lc = lock(&self.lifecycle);
        let result = f();
        if !matches!(result, Ok(false)) {
            lc.halt();
        }
        drop(lc);
        self.update(|_| {});
        result.map(drop)
    }

    /// Second half of a joined `POST /join`: stores the new credentials and
    /// snapshot and halts the daemon until the restart ([`Daemon::restart`])
    /// that brings them into effect, then sets up the repo with them, without
    /// the engine mutex (a halted daemon changes nothing). The outer error
    /// means nothing was stored; the inner one that the repo setup failed (the
    /// restarted daemon retries it).
    pub(crate) async fn rejoin(self: &Arc<Self>, reg: Registered) -> Result<Result<Joined>> {
        let reg = self
            .blocking(move |d, _| {
                if d.lifecycle().halted() {
                    return Err(Error::Conflict("already joined again; the daemon is restarting".into()));
                }
                d.halt_with(|| reg.persist(&d.cfg).map(|()| true))?;
                tracing::info!("joined again as `{}`", reg.state().machine_id);
                Ok(reg)
            })
            .await?;
        self.network(move |_| Ok(reg.set_up_repo().map_err(Error::Internal))).await
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

/// The working tree's items, by Item ID. Incomplete, so the server neither
/// materializes defaults nor prunes orphans by it, while metadata is
/// unreadable, a pull is pending (the tree is partial), an item has no
/// Item ID yet (a refresh writes one; none is ever derived), or two items
/// share one (until `repair-ids`).
pub(crate) fn repo_inventory(root: &Path, harnesses: Vec<String>) -> sk1ll_proto::InventoryReport {
    let inventory = sk1ll_core::item_id::working_inventory(root);
    let mut errors = inventory.errors;
    match Repo::open_git_only(root).and_then(|repo| repo.pull_pending()) {
        Ok(false) => {}
        Ok(true) => errors.insert(0, "a pull is pending, so the working tree is partial".into()),
        Err(e) => errors.insert(0, e.to_string()),
    }
    let mut items = Vec::new();
    let mut unassigned = Vec::new();
    for (item, id) in inventory.items {
        match id {
            Some(id) => items.push(sk1ll_proto::ReportedItem { item, id }),
            None => unassigned.push(item.to_string()),
        }
    }
    if !unassigned.is_empty() {
        errors.push(format!("no Item ID yet: {}", unassigned.join(", ")));
    }
    let mut seen = BTreeMap::new();
    for reported in &items {
        if let Some(first) = seen.insert(&reported.id, &reported.item) {
            errors.push(format!("{first} and {} share the Item ID {}", reported.item, reported.id));
        }
    }
    let inventory_error = errors.first().map(|error| crate::auth::redact(error));
    sk1ll_proto::InventoryReport {
        items,
        inventory_complete: inventory_error.is_none(),
        inventory_error,
        harnesses,
        ..Default::default()
    }
}

fn repo_report(root: &Path, harnesses: Vec<String>) -> Result<MachineReport> {
    let repo = Repo::open_git_only(root)?;
    let inventory = repo_inventory(root, harnesses);
    Ok(MachineReport {
        baseline: gitx::baseline(root)?,
        drafts: repo.draft()?.into_iter().map(|d| d.item).collect(),
        unpushed: gitx::unpushed_items(root)?,
        harnesses: inventory.harnesses,
        items: inventory.items,
        inventory_complete: inventory.inventory_complete,
        inventory_error: inventory.inventory_error,
        ..Default::default()
    })
}

/// Whether a snapshot at `incoming` replaces one at `applied`.
pub(crate) fn newer(applied: u64, incoming: u64) -> bool {
    incoming > applied
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

fn displaced_view(entry: sync::displaced::Entry, root: &Path) -> ConflictView {
    let item = displaced_item(&entry, root);
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

/// `root`: the skills repo, which a pull's entries are from.
fn displaced_item(entry: &sync::displaced::Entry, root: &Path) -> ItemRef {
    let name = entry.dest.file_name().unwrap_or_default().to_string_lossy();
    match entry.kind {
        sync::displaced::Kind::Skill => ItemRef::Skill(name.into_owned()),
        sync::displaced::Kind::Rule => ItemRef::Rule(name.strip_suffix(".md").unwrap_or(&name).to_string()),
        sync::displaced::Kind::Instructions => ItemRef::Other(entry.dest.display().to_string()),
        sync::displaced::Kind::Draft => {
            let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
            match entry.dest.strip_prefix(&root) {
                Ok(path) => ItemRef::for_path(path),
                Err(_) => ItemRef::Other(entry.dest.display().to_string()),
            }
        }
    }
}

fn placement_view(a: &Action) -> PlacementView {
    let (item, name) = match &a.item {
        Item::Skill { name, .. } => ("skill", name.clone()),
        Item::Rule { file, .. } => ("rule", file.clone()),
        Item::Instructions { rules, .. } => ("instructions", rules.join(", ")),
    };
    let kind = match a.kind {
        ActionKind::Create => "create",
        ActionKind::Update => "update",
        ActionKind::Remove => "remove",
        ActionKind::Conflict => "conflict",
    };
    PlacementView {
        harness: a.harness.clone(),
        kind: kind.into(),
        item: item.into(),
        name,
        dest: a.dest.clone(),
        reason: a.reason.clone(),
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
/// deleted file counts), without following symlinks or looking at what a
/// skill's hash leaves out. Bounded, so a huge tree can't stall `/conflicts`.
fn modified_at(path: &Path) -> Option<u64> {
    let seconds =
        |m: &std::fs::Metadata| m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs());
    let meta = std::fs::symlink_metadata(path).ok()?;
    let (mut newest, mut stack, mut budget) = (seconds(&meta), Vec::new(), 10_000usize);
    if meta.is_dir() {
        stack.push((path.to_path_buf(), 1));
    }
    while let Some((dir, depth)) = stack.pop() {
        for entry in std::fs::read_dir(&dir).ok()?.flatten() {
            budget = budget.checked_sub(1)?;
            if sync::excluded_from_personal_skill(depth, &entry.file_name()) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            newest = newest.max(seconds(&meta));
            if meta.is_dir() {
                stack.push((entry.path(), depth + 1));
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
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        let repo = Repo::open(self.repo_root())?;
        match (&req.bundled, req.kind.as_str()) {
            (Some(bundled), "skill") => repo.add_bundled(bundled, &req.name)?,
            (Some(_), _) => return Err(Error::BadRequest("only skills are bundled".into())),
            (None, _) => repo.create_item(&req.kind, &req.name, req.description.as_deref().unwrap_or(""))?,
        }
        self.refresh(eng)?;
        Ok(())
    }
    /// Restore brings the same Item ID and its toggles back.
    pub(crate) fn delete_item(&self, eng: &mut Engine, req: crate::client::DeleteItemRequest) -> Result<()> {
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        let deleted = Repo::open(self.repo_root())?.delete_item(&req.kind, &req.name);
        self.refresh(eng)?;
        Ok(deleted?)
    }
    pub(crate) fn rename_item(
        &self,
        eng: &mut Engine,
        req: crate::client::RenameItemRequest,
    ) -> Result<crate::client::RenameItemResponse> {
        use sk1ll_core::repo::RenameError;
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        let repo = Repo::open(self.repo_root())?;
        // A hand-made item gets its Item ID first, so the rename keeps it.
        repo.assign_missing_ids()?;
        repo.check_rename(&req.kind, &req.name, &req.to)?;
        let item = if req.kind == "skill" { ItemRef::Skill(req.to.clone()) } else { ItemRef::Rule(req.to.clone()) };
        match repo.rename_item(&req.kind, &req.name, &req.to) {
            Ok(()) => {
                if let Err(e) = self.refresh(eng) {
                    self.set_error(format!("refreshing after renaming {} to {item}: {e}", req.name));
                }
                Ok(crate::client::RenameItemResponse { item })
            }
            Err(RenameError::Moved(e)) => {
                let _ = self.refresh(eng);
                Err(Error::Internal(anyhow::anyhow!(
                    "`{}` was renamed to `{}`, but updating its metadata failed: {e}",
                    req.name,
                    req.to
                )))
            }
            Err(e @ RenameError::Stranded { .. }) => {
                self.set_error(e.to_string());
                Err(Error::Conflict(e.to_string()))
            }
            Err(RenameError::NotMoved(e)) => Err(e.into()),
        }
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
        use sk1ll_core::Origin;
        let repo = Repo::open(self.repo_root())?;
        let (view, recovery) = if let Some(id) = &req.id {
            let entry = sync::displaced::get(id)?;
            let target = sync::displaced::target(&repo, &entry)?;
            if entry.kind == sync::displaced::Kind::Draft {
                return Self::pull_recovery_diff(&repo, entry, target.path);
            }
            let view = displaced_view(entry, repo.root());
            (view, Some(target))
        } else {
            let view = self
                .conflicts()?
                .into_iter()
                .find(|v| Some(&v.harness) == req.harness.as_ref() && Some(&v.name) == req.name.as_ref())
                .ok_or_else(|| Error::NotFound("Conflict no longer exists".into()))?;
            (view, None)
        };
        let left_source = view.path.unwrap_or(view.dest);
        let mut item = if view.name == BLOCK_NAME { Some(ItemRef::Other(view.name.clone())) } else { None }
            .or(view.item.clone())
            .or_else(|| view.items.first().cloned())
            .unwrap_or(ItemRef::Other(view.name.clone()));
        let mut left_origin = Origin::Personal;
        let mut target = recovery.as_ref().map(|target| target.path.clone());
        let snapshot = self.snapshot();
        let temp = tempfile::tempdir().map_err(anyhow::Error::from)?;
        let right = temp.path().join("Draft");
        match &item {
            ItemRef::Skill(name) => {
                imports::validate_name(name)?;
                let (source, origin) = if let Some(target) = &recovery {
                    (target.path.clone(), target.origin.unwrap_or(Origin::Personal))
                } else if let Some(skill) = repo.skills()?.into_iter().find(|skill| skill.name == *name) {
                    (skill.path, skill.origin)
                } else {
                    (repo.personal_dir().join(name), Origin::Personal)
                };
                item = ItemRef::Skill(source.file_name().unwrap_or_default().to_string_lossy().into_owned());
                left_origin = match &recovery {
                    Some(target) => target.placed_origin.unwrap_or(origin),
                    None => sync::displaced::skill_placement_origin(&left_source)?.unwrap_or(origin),
                };
                if source.symlink_metadata().is_ok_and(|meta| meta.is_dir()) {
                    sync::copy_skill_for_diff(&source, &right, origin)?;
                }
                target = Some(source);
            }
            ItemRef::Rule(name) => {
                imports::validate_name(name)?;
                let path =
                    target.clone().unwrap_or_else(|| repo.root().join("instructions").join(format!("{name}.md")));
                item = ItemRef::Rule(path.file_stem().unwrap_or_default().to_string_lossy().into_owned());
                if let Some(rule) = repo.rules()?.into_iter().find(|rule| rule.path == path) {
                    let (_, content) = sk1ll_core::instructions::render_rule(&rule);
                    std::fs::write(&right, content).map_err(anyhow::Error::from)?;
                }
                target = Some(path);
            }
            _ => {
                let harnesses = MachineConfig::load(&self.cfg.machine_config)?.harnesses();
                let assignment =
                    &snapshot.as_ref().ok_or_else(|| Error::Conflict("Awaiting Assignment".into()))?.assignment;
                let files = sync::render_instructions(&repo, &harnesses, assignment, &view.harness)?;
                std::fs::write(&right, files.into_iter().map(|f| f.content).collect::<Vec<_>>().join("\n"))
                    .map_err(anyhow::Error::from)?;
            }
        }
        let mut left = left_source;
        if view.name == BLOCK_NAME && matches!(item, ItemRef::Other(_)) {
            let text = std::fs::read_to_string(&left).map_err(anyhow::Error::from)?;
            let content = sk1ll_core::instructions::managed_content(&text, &left)?;
            left = temp.path().join("Harness");
            std::fs::write(&left, content).map_err(anyhow::Error::from)?;
        } else if left.symlink_metadata().is_ok_and(|meta| meta.is_dir()) {
            let copy = temp.path().join("Harness");
            sync::copy_skill_for_diff(&left, &copy, left_origin)?;
            left = copy;
        }
        if !right.exists() {
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
        Ok(DiffView { item, target, diff: String::from_utf8_lossy(&output.stdout).into_owned() })
    }

    /// What a pull displaced from the working tree, against what is at its
    /// path now: adopting puts it back there.
    fn pull_recovery_diff(repo: &Repo, entry: sync::displaced::Entry, target: PathBuf) -> Result<DiffView> {
        let item = displaced_item(&entry, repo.root());
        let temp = tempfile::tempdir().map_err(anyhow::Error::from)?;
        let right = if target.symlink_metadata().is_ok() {
            target.clone()
        } else {
            let empty = temp.path().join("Draft");
            std::fs::write(&empty, "").map_err(anyhow::Error::from)?;
            empty
        };
        let output = std::process::Command::new("git")
            .args(["diff", "--no-index", "--no-ext-diff", "--no-textconv", "--"])
            .arg(&entry.path)
            .arg(&right)
            .output()
            .map_err(anyhow::Error::from)?;
        if !matches!(output.status.code(), Some(0 | 1)) {
            return Err(Error::BadRequest(String::from_utf8_lossy(&output.stderr).into_owned()));
        }
        Ok(DiffView { item, target: Some(target), diff: String::from_utf8_lossy(&output.stdout).into_owned() })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[test]
    fn inventory_keeps_both_ids_when_a_draft_recreates_the_same_name() {
        use sk1ll_core::git::Git;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let repo = Repo::init_committed(root).unwrap();
        repo.create_item("skill", "pdf", "").unwrap();
        let old = repo.skills().unwrap()[0].id.clone();
        let git = |args: &[&str]| {
            Git::new(root)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(["-c", "commit.gpgsign=false", "-c", "user.name=test", "-c", "user.email=test@example.com"])
                .args(args)
                .text()
                .unwrap()
        };
        git(&["add", "--all"]);
        git(&["commit", "--quiet", "-m", "baseline"]);
        let head = git(&["rev-parse", "HEAD"]);
        git(&["update-ref", "refs/remotes/origin/main", &head]);
        repo.delete_item("skill", "pdf").unwrap();
        repo.create_item("skill", "pdf", "").unwrap();
        let current = repo.skills().unwrap()[0].id.clone();
        let items = Daemon::items(root).unwrap();
        assert!(
            items
                .iter()
                .any(|i| i.id.as_ref() == old.as_ref() && i.in_baseline && i.draft == Some(ChangeKind::Deleted))
        );
        assert!(
            items
                .iter()
                .any(|i| i.id.as_ref() == current.as_ref() && !i.in_baseline && i.draft == Some(ChangeKind::Added))
        );
    }

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
        write("sk1ll.toml", "");
        write("personal/base/SKILL.md", "---\ndescription: Baseline description\n---\nbase\n");
        write("personal/base/.sk1ll-id", "base-skill\n");
        write(
            "instructions/git.md",
            &format!("---\nid: git-rule\n---\n{}", "x".repeat(sk1ll_core::content::MAX_FILE_BYTES as usize)),
        );

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
        assert_eq!(find("skill:base").id.as_ref().unwrap().to_string(), "base-skill");
        // Descriptions of items without a draft are read from git, in one batch.
        assert_eq!(find("skill:vendored").description.as_deref(), Some("Imported description"));
        assert_eq!(find("skill:my skill").description.as_deref(), Some("Spaced"));
        assert_eq!(find("skill:queued").description.as_deref(), Some("Queued description"));
        assert_eq!(find("skill:huge").description, None, "over the cap: not read");
        assert_eq!(find("skill:plain").description, None);
        assert_eq!(find("skill:local").description, None);
        assert!(find("rule:git").in_baseline, "deleted Draft item still comes from the git tree");
        assert_eq!(find("rule:git").draft, Some(ChangeKind::Deleted));
        assert_eq!(find("rule:git").id.as_ref().unwrap().to_string(), "git-rule");
        assert!(!find("skill:queued").in_baseline);
        assert!(find("skill:queued").unpushed);
        assert!(!find("skill:local").in_baseline);
        assert_eq!(find("skill:local").draft, Some(ChangeKind::Added));
        assert_eq!(std::fs::read(root.join(".git/index")).unwrap(), index_before);
        assert_eq!(git(&["rev-parse", "HEAD"]), head_before);

        write("sk1ll.toml", "[import.vendored]\nid = 42\n");
        assert!(Daemon::items(root).unwrap_err().to_string().contains("sk1ll.toml"));
        write("sk1ll.toml", "");

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
    fn plan_hashes_each_copy_source_once_per_computation() {
        let copy = |harness: &str, source: &str| Action {
            harness: harness.into(),
            kind: ActionKind::Create,
            dest: PathBuf::from(harness).join(source),
            item: Item::Skill {
                name: source.into(),
                mode: sk1ll_core::LinkMode::Copy,
                source: Some(source.into()),
                item_id: None,
                metadata_only: false,
                expected: None,
                expected_index: None,
            },
            reason: None,
        };
        let actions = [copy("a", "pdf"), copy("a", "notes"), copy("b", "pdf")];
        let mut calls = Vec::new();
        for digest in ["before", "after"] {
            let sources = Daemon::sync_source_digests(&actions, |path| {
                calls.push(path.to_path_buf());
                Ok(format!("{}-{digest}", path.display()))
            })
            .unwrap();
            assert_eq!(
                sources,
                [Some(format!("pdf-{digest}")), Some(format!("notes-{digest}")), Some(format!("pdf-{digest}"))]
            );
        }
        assert_eq!(calls, ["pdf", "notes", "pdf", "notes"].map(PathBuf::from));
    }

    #[tokio::test]
    async fn reviewed_sync_reports_partial_progress_and_stops_on_digest_mismatch() {
        use crate::client::{Client, SyncRequest};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let repo = Repo::init(&root).unwrap();
        let source = repo.personal_dir().join("pdf/SKILL.md");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, "reviewed").unwrap();
        for name in ["a-success", "b-io-error", "z-later"] {
            let skill = repo.personal_dir().join(name);
            std::fs::create_dir_all(&skill).unwrap();
            std::fs::write(skill.join("SKILL.md"), name).unwrap();
        }
        repo.assign_missing_ids().unwrap();
        let failing_source = repo.personal_dir().join("b-io-error");
        let cfg = Config {
            repo: Some(root),
            state: dir.path().join("state/daemon.json"),
            assignment: dir.path().join("assignment.json"),
            machine_config: dir.path().join("machine.toml"),
            socket: dir.path().join("daemon.sock"),
            ..Config::new().unwrap()
        };
        let skills = dir.path().join("skills");
        let presets = sk1ll_core::machine::presets()
            .map(|name| format!("[harnesses.{name}]\nenabled = false\n"))
            .collect::<String>();
        std::fs::write(
            &cfg.machine_config,
            format!("id = \"review\"\n[harnesses.test]\nskills = {skills:?}\nmode = \"copy\"\n{presets}"),
        )
        .unwrap();
        let client = Client::new(&cfg.socket);
        let listener = tokio::net::UnixListener::bind(&cfg.socket).unwrap();
        let daemon = Daemon::standalone(cfg).unwrap();
        lock(&daemon.engine).after_sync_review = Some(Box::new(move || {
            std::fs::remove_dir_all(failing_source).unwrap();
            std::fs::write(source, "changed after the fingerprint matched").unwrap();
        }));
        let mut events = daemon.subscribe_events();
        let served = daemon.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, crate::api::router(served)).await.unwrap();
        });
        let reviewed = client.preview_sync().await.unwrap().fingerprint.unwrap();
        let returned = client.apply_sync_preview(reviewed.clone()).await.unwrap();
        let fresh = returned.fingerprint.unwrap();
        assert_ne!(fresh, reviewed);
        let remaining: Vec<_> = returned.actions.unwrap().into_iter().map(|action| action.name).collect();
        assert_eq!(remaining, ["pdf", "z-later"]);
        assert_eq!(returned.completed.len(), 1);
        assert_eq!(returned.completed[0].name, "a-success");
        assert_eq!(returned.changed.unwrap().name, "pdf");
        assert_eq!(returned.errors.len(), 1);
        assert!(returned.errors[0].contains("b-io-error"));
        assert_eq!(daemon.view().last_error.as_ref(), returned.errors.first());
        assert!(!lock(&daemon.engine).stale);
        assert!(matches!(events.try_recv().unwrap(), Event::Status));
        assert!(skills.join("a-success/SKILL.md").is_file());
        assert!(!skills.join("z-later").exists());
        assert_eq!(client.preview_sync().await.unwrap().fingerprint.as_deref(), Some(fresh.as_str()));
        assert!(!skills.join("pdf").exists());
        let error = client.sync(&SyncRequest { fingerprint: Some(reviewed), ..Default::default() }).await.unwrap_err();
        assert_eq!(error.status, Some(409));
        let applied = client.apply_sync_preview(fresh).await.unwrap();
        assert!(applied.errors.is_empty());
        assert_eq!(
            std::fs::read_to_string(skills.join("pdf/SKILL.md")).unwrap(),
            "changed after the fingerprint matched"
        );
        server.abort();
    }

    #[test]
    fn stale_revisions_are_ignored() {
        assert!(newer(0, 1));
        assert!(newer(4, 5));
        assert!(!newer(5, 5));
        assert!(!newer(5, 3));
        assert!(!newer(5, 0));
        assert!(!newer(0, 0));
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
        use axum::http::StatusCode;
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
            let mut cfg = Config::new().unwrap();
            cfg.state = dir.path().join("state/daemon.json");
            cfg.assignment = dir.path().join("assignment.json");
            cfg.machine_config = dir.path().join("machine.toml");
            std::fs::write(
                &cfg.machine_config,
                // The presets would find the developer's real harnesses.
                format!(
                    "[harnesses.h]\nskills = {:?}\nmode = \"copy\"\n{}",
                    dir.path().join("h"),
                    sk1ll_core::machine::presets()
                        .map(|p| format!("[harnesses.{p}]\nenabled = false\n"))
                        .collect::<String>()
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
                    server_url: "http://127.0.0.1:1".into(),
                    repo: root,
                    revoked: false,
                    record: Default::default(),
                },
            )
            .unwrap();
            let (tx, mut rx) = mpsc::channel::<crate::server::SessionWrite>(1);
            d.assignment_session.send_replace(Some(tx));
            let server = tokio::spawn({
                let (accept, destination, source) = (accept.clone(), destination.clone(), source.clone());
                async move {
                    while let Some(request) = rx.recv().await {
                        let AssignmentWrite::Changes(body) = request.write else { panic!("expected toggles") };
                        assert_eq!(body.changes.len(), 1);
                        let change = &body.changes[0];
                        assert_eq!(change.enabled, Some(true));
                        assert_eq!(change.name.as_deref(), Some("pdf"));
                        let result = if accept.load(Ordering::SeqCst) {
                            let mut assignment = sk1ll_proto::Assignment::default();
                            assignment.set_skill("h", change.item_id.as_ref().unwrap(), true);
                            Ok(AssignmentUpdate { assignment, revision: 2, entries: vec![], errors: vec![] })
                        } else {
                            if scenario == "edit_repo" {
                                std::fs::write(destination.join("SKILL.md"), "later edit").unwrap();
                            }
                            if scenario == "edit_source" {
                                std::fs::write(source.join("SKILL.md"), "later edit").unwrap();
                            }
                            Err(Error::Upstream(StatusCode::UNPROCESSABLE_ENTITY, "Server refused".into()))
                        };
                        request.result.send(result).unwrap();
                    }
                }
            });
            d.set_link(Link::Connected);
            let error = d.adopt(ConflictTarget { harness: "h".into(), name: "pdf".into() }).await.unwrap_err();
            assert!(error.to_string().contains("Server refused"), "{error}");
            assert_eq!(
                std::fs::read_to_string(source.join("SKILL.md")).unwrap(),
                if scenario == "edit_source" { "later edit" } else { "original" }
            );
            assert!(!source.parent().unwrap().join(sync::INDEX_PATH).exists());
            if scenario == "new" {
                assert!(!destination.exists());
                accept.store(true, Ordering::SeqCst);
                d.adopt(ConflictTarget { harness: "h".into(), name: "pdf".into() }).await.unwrap();
                let index = std::fs::read(source.parent().unwrap().join(sync::INDEX_PATH)).unwrap();
                let index: serde_json::Value = serde_json::from_slice(&index).unwrap();
                assert_eq!(index["entries"]["pdf"]["kind"], "copy");
                let token = std::fs::read_to_string(source.join(sync::PLACEMENT_MARKER)).unwrap();
                assert_eq!(index["entries"]["pdf"]["token"], token);
            } else {
                assert_eq!(
                    std::fs::read_to_string(destination.join("SKILL.md")).unwrap(),
                    if scenario == "edit_repo" { "later edit" } else { "original" }
                );
            }
            server.abort();
        }
    }

    /// A joined daemon on a fresh repo, its Assignment at revision 1 and
    /// `record` in its state file, with a server session the test answers.
    fn joined_daemon(
        dir: &Path,
        record: PullRecord,
    ) -> (Arc<Daemon>, mpsc::Receiver<crate::server::SessionWrite>, DaemonState) {
        let root = dir.join("repo");
        Repo::init_committed(&root).unwrap();
        let mut cfg = Config::new().unwrap();
        cfg.state = dir.join("state/daemon.json");
        cfg.assignment = dir.join("assignment.json");
        cfg.machine_config = dir.join("machine.toml");
        std::fs::write(
            &cfg.machine_config,
            format!(
                "[harnesses.h]\nskills = {:?}\nmode = \"copy\"\n{}",
                dir.join("h"),
                sk1ll_core::machine::presets()
                    .map(|p| format!("[harnesses.{p}]\nenabled = false\n"))
                    .collect::<String>()
            ),
        )
        .unwrap();
        state::save_snapshot(&cfg.snapshot_path(), &AssignmentSnapshot { revision: 1, ..Default::default() }).unwrap();
        let state =
            DaemonState { record, ..DaemonState::new("a".into(), "http://127.0.0.1:1".into(), "t".into(), root) };
        state.save(&cfg.state).unwrap();
        let d = Daemon::new(cfg, state.clone()).unwrap();
        let (tx, rx) = mpsc::channel::<crate::server::SessionWrite>(1);
        d.assignment_session.send_replace(Some(tx));
        d.set_link(Link::Connected);
        (d, rx, state)
    }

    fn harness_toggle(enabled: bool) -> AssignmentWrite {
        AssignmentWrite::Harness { harness: "h".into(), enabled }
    }

    fn confirmed(revision: u64, enabled: bool) -> Result<AssignmentUpdate> {
        let mut assignment = sk1ll_proto::Assignment::default();
        assignment.set_harness("h", enabled);
        Ok(AssignmentUpdate { assignment, revision, entries: vec![], errors: vec![] })
    }

    /// The server takes its time to confirm a write; meanwhile the engine,
    /// the readers and the status all answer.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_slow_assignment_confirmation_blocks_no_other_call() {
        let dir = tempfile::tempdir().unwrap();
        let (d, mut session, _) = joined_daemon(dir.path(), PullRecord::default());
        d.blocking(|d, eng| d.refresh(eng)).await.unwrap();
        let writer = d.clone();
        let write = tokio::spawn(async move { writer.write_assignment(harness_toggle(false)).await });
        let request = session.recv().await.expect("the write reached the session");

        let quick = std::time::Duration::from_secs(5);
        let started = std::time::Instant::now();
        tokio::time::timeout(quick, d.reading(|d| d.draft())).await.expect("a reader waited").unwrap();
        tokio::time::timeout(quick, d.blocking(|d, eng| d.refresh(eng)))
            .await
            .expect("the engine mutex was held during the request")
            .unwrap();
        d.set_link(Link::Offline);
        assert_eq!(d.view().connection, Connection::Offline);
        assert!(started.elapsed() < quick);
        assert!(!write.is_finished(), "the write was still waiting for the server");

        request.result.send(confirmed(2, false)).unwrap();
        let update = write.await.unwrap().unwrap();
        assert_eq!(update.revision, 2);
        assert_eq!(d.snapshot().unwrap().revision, 2);
        assert_eq!(d.snapshot().unwrap().assignment, confirmed(2, false).unwrap().assignment);
    }

    /// What changes while a write is in flight decides what its confirmation
    /// does: a newer revision stands, and a daemon that stopped managing
    /// applies nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_confirmation_overtaken_by_a_newer_revision_is_not_applied() {
        let dir = tempfile::tempdir().unwrap();
        let (d, mut session, _) = joined_daemon(dir.path(), PullRecord::default());
        let writer = d.clone();
        let write = tokio::spawn(async move { writer.write_assignment(harness_toggle(false)).await });
        let request = session.recv().await.unwrap();
        // The server's snapshot of a later write (this one's included)
        // arrives before this write's own confirmation.
        let newer = confirmed(3, false).unwrap();
        let snapshot = AssignmentSnapshot { revision: 3, assignment: newer.assignment.clone(), registration: None };
        d.blocking(move |d, eng| d.apply_assignment(eng, snapshot)).await.unwrap();
        request.result.send(confirmed(2, true)).unwrap();
        let update = write.await.unwrap().unwrap();
        assert_eq!(update.revision, 3, "the reply carries the revision that stands");
        assert_eq!(update.assignment, newer.assignment);
        assert_eq!(d.snapshot().unwrap().revision, 3);
        assert_eq!(state::load_snapshot(&d.cfg.snapshot_path()).unwrap().unwrap().revision, 3);

        let writer = d.clone();
        let write = tokio::spawn(async move { writer.write_assignment(harness_toggle(true)).await });
        let request = session.recv().await.unwrap();
        d.blocking(|d, eng| d.revoke(eng, "removed from the fleet")).await.unwrap();
        request.result.send(confirmed(4, true)).unwrap();
        assert_eq!(write.await.unwrap().unwrap().revision, 4);
        assert_eq!(d.snapshot().unwrap().revision, 3, "a removed Machine applies nothing");
    }

    /// The state file keeps the block and the server's Baseline, so a
    /// restarted daemon starts Blocked and Behind, before any pull.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_restart_remembers_the_block_and_the_server_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let (d, _session, state) = joined_daemon(dir.path(), PullRecord::default());
        d.blocking(|d, eng| d.refresh(eng)).await.unwrap();
        let head = gitx::rev(d.repo_root(), "HEAD").unwrap();
        let clash = ItemRef::Skill("pdf".into());
        let blocked = PullState::Blocked { baseline: "f00d".repeat(10), items: vec![clash.clone()] };
        d.note(|lc| {
            lc.set_pull(blocked.clone());
            lc.set_server_baseline(Some("f00d".repeat(10)));
        });
        let saved = DaemonState::load(&d.cfg.state).unwrap().unwrap();
        assert_eq!(saved.record.pull, blocked);
        assert_eq!(saved.record.server_baseline, Some("f00d".repeat(10)));
        assert_eq!((saved.token.as_str(), saved.revoked), (state.token.as_str(), false));

        let cfg = d.cfg.clone();
        drop(d);
        let restarted = Daemon::new(cfg.clone(), DaemonState::load(&cfg.state).unwrap().unwrap()).unwrap();
        let view = restarted.view();
        assert!(view.report.blocked && view.report.behind);
        assert_eq!(view.report.clashes, [clash]);
        assert_eq!(view.server_baseline, Some("f00d".repeat(10)));
        assert_eq!(view.connection, Connection::Connecting);
        assert_eq!(restarted.lifecycle().pull(), &blocked);
        // A refresh keeps it: HEAD is still not on the blocking Baseline.
        restarted.blocking(|d, eng| d.refresh(eng)).await.unwrap();
        assert!(restarted.view().report.blocked);

        // Once HEAD contains that Baseline (a pull while the daemon was
        // stopped), the block is over.
        let head = head.expect("an initialized repo has a commit");
        restarted.note(|lc| lc.set_pull(PullState::Blocked { baseline: head, items: Vec::new() }));
        restarted.blocking(|d, eng| d.refresh(eng)).await.unwrap();
        assert_eq!(restarted.lifecycle().pull(), &PullState::Clear);
        assert!(!restarted.view().report.blocked);
        assert_eq!(DaemonState::load(&cfg.state).unwrap().unwrap().record.pull, PullState::Clear);

        // Removed from the Fleet: a restart stays removed.
        restarted.blocking(|d, eng| d.revoke(eng, "removed")).await.unwrap();
        let state = DaemonState::load(&cfg.state).unwrap().unwrap();
        assert!(state.revoked);
        let removed = Daemon::new(cfg, state).unwrap();
        assert_eq!(removed.view().connection, Connection::Revoked);
        assert!(removed.lifecycle().idle());
    }

    /// A halted daemon (joined again, restarting) never writes the state
    /// file: it may already hold the next registration.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_halted_daemon_leaves_the_state_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let (d, _session, state) = joined_daemon(dir.path(), PullRecord::default());
        let next = DaemonState::new("b".into(), "http://127.0.0.1:2".into(), "new".into(), state.repo.clone());
        d.halt_with(|| next.save(&d.cfg.state).map(|()| true)).unwrap();
        d.note(|lc| lc.set_server_baseline(Some("abc".into())));
        d.blocking(|d, eng| d.revoke(eng, "the old registration was removed")).await.unwrap();
        assert_eq!(DaemonState::load(&d.cfg.state).unwrap(), Some(next));
        assert!(d.ensure_managed().is_err());
    }
}
