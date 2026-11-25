use super::*;

use sk1ll_core::harness_change;
use sk1ll_core::{HarnessChange, HarnessOrigin, MachineConfig, Repo};

use std::sync::Arc;

use crate::client::{
    Event, HarnessApplied, HarnessChangeRequest, HarnessChoice, HarnessPreview, HarnessSettings, HarnessTakenOut,
    HarnessView, PlacementView,
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
        let snapshot = self.applied();
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

    /// `POST /harnesses/apply`, one harness change at a time (see
    /// [`Daemon::harness_changes`]).
    pub(crate) async fn apply_harness_serialized(
        self: &Arc<Self>,
        req: HarnessChangeRequest,
    ) -> Result<HarnessApplied> {
        let _harnesses = self.harness_changes.lock().await;
        sk1ll_core::failpoint!(DaemonApplyEntered, &self.cfg.machine_config);
        self.blocking(move |d, eng| d.apply_harness(eng, req)).await
    }

    /// `POST /harnesses/take-out`: disables or removes a harness after
    /// taking out what sk1ll placed there. It turns the harness's items
    /// off in the Assignment, which syncs, so each managed placement goes
    /// the way sync removes it (into recovery where its rules say so; a
    /// copy edited by hand stays). Only if a complete scan finds nothing
    /// managed left does it change `machine.toml`, with the preview's
    /// fingerprint checked as [`Daemon::apply_harness`] does; otherwise the
    /// items stay off, so the same request can be retried.
    ///
    /// It holds [`Daemon::harness_changes`] throughout, so no other change
    /// through the daemon lands between its first check and the change. A
    /// hand edit of `machine.toml` still can: then nothing is changed
    /// there, and the errors say what the take-out already did.
    pub(crate) async fn take_out_harness(self: &Arc<Self>, req: HarnessChangeRequest) -> Result<HarnessTakenOut> {
        let name = match &req.change {
            HarnessChange::Disable { name } | HarnessChange::Remove { name } => name.clone(),
            _ => return Err(Error::BadRequest("only disable and remove take a harness's content out".into())),
        };
        let fingerprint = req
            .fingerprint
            .clone()
            .ok_or_else(|| Error::BadRequest("this client is older than the daemon; update it".into()))?;
        let _harnesses = self.harness_changes.lock().await;
        // Off the runtime: a test's callback may hold the take-out here.
        #[cfg(feature = "failpoints")]
        {
            let config = self.cfg.machine_config.clone();
            let _ = tokio::task::spawn_blocking(move || sk1ll_core::failpoint!(DaemonTakeOutEntered, &config)).await;
        }
        let change = req.change.clone();
        let expected = fingerprint.clone();
        // Checked first, so a stale preview turns nothing off.
        let before = self
            .blocking(move |d, _| {
                d.ensure_managed()?;
                let preview = d.preview_harness(&change)?;
                if preview.fingerprint != expected {
                    return Err(sk1ll_core::Error::Changed(
                        "machine.toml changed since the preview — preview again".into(),
                    )
                    .into());
                }
                Ok(managed(&preview.placed))
            })
            .await?;
        let write = sk1ll_proto::AssignmentWrite::Harness { harness: name.clone(), enabled: false };
        let mut errors = self.write_assignment(write).await?.errors;
        // From here on the Assignment is changed: every outcome says so.
        self.blocking(move |d, eng| {
            if let Err(e) = d.reconcile(eng).map(|failed| errors.extend(failed)) {
                errors.push(format!("sync: {e}"));
            }
            sk1ll_core::failpoint!(DaemonBeforeTakeOutApply, &d.cfg.machine_config);
            // What machine.toml says now, read fresh for each report.
            let config = || MachineConfig::load(&d.cfg.machine_config).map(|c| c.choices());
            // Every report states what is known: the Assignment change, what a complete scan
            // verified was taken out (else that it is unknown), and the harness as machine.toml has it.
            let not_applied = |taken_out: Option<Vec<PlacementView>>, left, mut errors: Vec<String>, why: String| {
                let taken = match &taken_out {
                    Some(taken) => format!(
                        "{name}'s items were turned off and {} placed {} taken out",
                        taken.len(),
                        if taken.len() == 1 { "item was" } else { "items were" }
                    ),
                    None => format!(
                        "{name}'s items were turned off; what sync took out is unknown, since no complete scan \
                         verified it"
                    ),
                };
                let (harnesses, status) = match config() {
                    Ok(choices) => {
                        let status = match choices.iter().find(|c| c.name == name) {
                            Some(c) if c.enabled => format!("{name} is still on"),
                            Some(_) => format!("{name} is off in machine.toml"),
                            None => format!("{name} is no longer in machine.toml"),
                        };
                        (choices, status)
                    }
                    Err(e) => (Vec::new(), format!("machine.toml can't be read ({e})")),
                };
                errors.push(format!("{why}. {taken}; {status}. Preview again"));
                let taken_out = taken_out.unwrap_or_default();
                Ok(HarnessTakenOut { applied: false, taken_out, left, harnesses, errors })
            };
            let preview = match d.preview_harness(&req.change) {
                Ok(preview) => preview,
                Err(e) => return not_applied(None, Vec::new(), errors, format!("machine.toml was not changed: {e}")),
            };
            // Only a complete scan shows what is gone.
            let left = managed(&preview.placed);
            if !preview.warnings.is_empty() {
                tracing::warn!("{name}: {} scan error(s)", preview.warnings.len());
                errors.extend(preview.warnings);
                let why = "machine.toml was not changed: not every placement could be listed".to_string();
                return not_applied(None, left, errors, why);
            }
            let taken_out: Vec<_> = before.into_iter().filter(|p| !left.iter().any(|l| l.dest == p.dest)).collect();
            if !left.is_empty() {
                tracing::warn!("{name}: {} placed item(s) left", left.len());
                let why = format!(
                    "machine.toml was not changed: {} placed {} still there",
                    left.len(),
                    if left.len() == 1 { "item is" } else { "items are" }
                );
                return not_applied(Some(taken_out), left, errors, why);
            }
            if preview.fingerprint != fingerprint {
                return not_applied(
                    Some(taken_out),
                    Vec::new(),
                    errors,
                    "machine.toml changed since the preview".into(),
                );
            }
            sk1ll_core::failpoint!(DaemonBeforeTakeOutWrite, &d.cfg.machine_config);
            // The write's own plan scans again, and refuses what this one would.
            let written = (|| -> Result<()> {
                d.ensure_managed()?;
                d.lock_repo(eng)?;
                let repo = Repo::open(d.repo_root())?;
                let lock = eng.lock.as_ref().ok_or_else(|| Error::Conflict("the repo is not locked".into()))?;
                harness_change::apply_taken_out(lock, &repo, &d.cfg.machine_config, &req.change, &fingerprint)?;
                Ok(())
            })();
            if let Err(e) = written {
                return not_applied(Some(taken_out), Vec::new(), errors, e.to_string());
            }
            tracing::info!("harness change {:?} after taking out {} placed item(s)", req.change, taken_out.len());
            match d.reconcile(eng) {
                Ok(failed) => errors.extend(failed),
                Err(e) => errors.push(format!("sync: {e}")),
            }
            d.event(Event::Status);
            let harnesses = config().unwrap_or_default();
            Ok(HarnessTakenOut { applied: true, taken_out, left: Vec::new(), harnesses, errors })
        })
        .await
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

/// The managed placements of a preview: what sync can take out (a copy
/// edited by hand is a `conflict`, which stays).
fn managed(placed: &[PlacementView]) -> Vec<PlacementView> {
    placed.iter().filter(|p| p.kind == "remove").cloned().collect()
}
