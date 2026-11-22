use super::*;

use sk1ll_core::harness_change;
use sk1ll_core::{HarnessChange, HarnessOrigin, MachineConfig, Repo};

use crate::client::{
    Event, HarnessApplied, HarnessChangeRequest, HarnessChoice, HarnessPreview, HarnessSettings, HarnessView,
};
use crate::error::{Error, Result};

impl Daemon {
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
        let errors = match self.reconcile(eng) {
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
}
