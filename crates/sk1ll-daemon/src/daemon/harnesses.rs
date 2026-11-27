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

    /// Adds the preset instance `name` (`harness.kind` in `harness.home`)
    /// during first-run setup: a candidate the client found in its
    /// environment (see [`sk1ll_core::machine::candidates`]) and kept on.
    /// The usual checks apply, as for `sk1ll harnesses add`.
    pub(crate) fn add_setup_instance(
        &self,
        _eng: &mut Engine,
        name: &str,
        harness: sk1ll_core::Harness,
    ) -> Result<Vec<HarnessChoice>> {
        if !self.is_standalone() || self.repo_root().join(".git").exists() {
            return Err(Error::Conflict(format!(
                "harnesses can be added here only during first-run setup; use POST /harnesses/apply \
                 (Settings or `sk1ll harnesses add`), or edit {}",
                self.cfg.machine_config.display()
            )));
        }
        if harness.kind.is_none() || harness.home.is_none() {
            return Err(Error::BadRequest("only a preset instance (a kind and a home) is added in setup".into()));
        }
        if self.setup_harnesses()?.iter().any(|c| c.name == name) {
            return Err(Error::Conflict(format!("harness `{name}` exists already")));
        }
        let change = HarnessChange::Set { name: name.into(), harness };
        MachineConfig::write_change(&self.cfg.machine_config, &change, self.repo_root())?;
        self.setup_harnesses()
    }

    /// A reader: no engine mutex.
    pub(crate) fn harness_settings(&self) -> Result<HarnessSettings> {
        let harnesses = MachineConfig::load(&self.cfg.machine_config)?.choices();
        Ok(HarnessSettings { path: self.cfg.machine_config.clone(), harnesses })
    }

    /// A reader: a Harness of this Machine by `name`, in any state.
    pub(crate) fn known_harness(&self, name: &str) -> Result<()> {
        if !MachineConfig::load(&self.cfg.machine_config)?.choices().iter().any(|c| c.name == name) {
            return Err(Error::NotFound(format!("no harness `{name}` on this machine")));
        }
        Ok(())
    }

    /// A reader: plans the change without making it, with `copy_from`'s
    /// Assignment copied to the changed Harness first (see
    /// [`HarnessChangeRequest::copy_assignment_from`]).
    pub(crate) fn preview_harness(&self, change: &HarnessChange, copy_from: Option<&str>) -> Result<HarnessPreview> {
        let repo = Repo::open(self.repo_root())?;
        let snapshot = self.applied();
        let mut assignment = snapshot.as_ref().map(|s| s.assignment.clone());
        if let Some(from) = copy_from {
            self.check_copy(change, from)?;
            if let Some(a) = &mut assignment {
                a.copy_harness(from, change.name());
            }
        }
        let plan = harness_change::plan(&repo, &self.cfg.machine_config, change, assignment.as_ref())?;
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

    /// Whether `change` can take `from`'s Assignment: adding or enabling a
    /// harness that isn't synced into yet, from another Harness of this
    /// Machine. The copy is made before `machine.toml` changes, and syncs:
    /// into a harness that is off nothing, so the first sync into it is the
    /// one after the change. One that is on would be synced with the copy
    /// at its old paths first, so it copies with `assign --copy-from` instead.
    fn check_copy(&self, change: &HarnessChange, from: &str) -> Result<()> {
        if !matches!(change, HarnessChange::Set { .. } | HarnessChange::Enable { .. }) {
            return Err(Error::BadRequest("only adding or enabling a harness copies an Assignment".into()));
        }
        let to = change.name();
        if from == to {
            return Err(Error::BadRequest(format!("`{from}` can't copy its own Assignment")));
        }
        self.known_harness(from)?;
        if MachineConfig::load(&self.cfg.machine_config)?.harnesses().contains_key(to) {
            return Err(Error::BadRequest(format!(
                "`{to}` is on already, so a change to it can't copy an Assignment; make the change, then copy \
                 with `sk1ll assign {to} --copy-from {from}`"
            )));
        }
        Ok(())
    }

    /// `POST /harnesses/apply`, one harness change at a time (see
    /// [`Daemon::harness_changes`]). With
    /// [`HarnessChangeRequest::copy_assignment_from`], the Assignment is
    /// copied first, so the sync after the change places what was copied:
    /// a copy that fails leaves `machine.toml` as it is, and a change that
    /// fails after the copy says the copy stands.
    pub(crate) async fn apply_harness_serialized(
        self: &Arc<Self>,
        req: HarnessChangeRequest,
    ) -> Result<HarnessApplied> {
        let _harnesses = self.harness_changes.lock().await;
        sk1ll_core::failpoint!(DaemonApplyEntered, &self.cfg.machine_config);
        let Some(from) = req.copy_assignment_from.clone() else {
            return self.blocking(move |d, eng| d.apply_harness(eng, req)).await;
        };
        // Checked first, so an old client, a stale preview, or a change the plan refuses, copies nothing.
        let expected = req
            .fingerprint
            .clone()
            .ok_or_else(|| Error::BadRequest("this client is older than the daemon; update it".into()))?;
        let (change, copy_from) = (req.change.clone(), from.clone());
        self.blocking(move |d, _| {
            d.ensure_managed()?;
            let preview = d.preview_harness(&change, Some(&copy_from))?;
            if preview.fingerprint != expected {
                return Err(sk1ll_core::Error::Changed(
                    "machine.toml changed since the preview — preview again".into(),
                )
                .into());
            }
            Ok(())
        })
        .await?;
        let to = req.change.name().to_string();
        let copied = self.copy_harness_assignment(&from, &to).await.map_err(|e| {
            e.reworded(|why| {
                format!("copying {from}'s Assignment to {to} failed, so machine.toml was not changed: {why}")
            })
        })?;
        let (revision, install_errors) = (copied.revision, copied.errors.clone());
        let applied = self
            .blocking(move |d, eng| {
                // The sync after the change must plan with the copy: a joined Machine that couldn't store
                // the Server's confirmation would plan with the Assignment from before it.
                if d.applied().is_none_or(|have| have.revision < revision) {
                    let why = match install_errors.is_empty() {
                        true => String::new(),
                        false => format!(": {}", install_errors.join("; ")),
                    };
                    return Err(Error::Unavailable(format!(
                        "it was saved as revision {revision}, but this Machine couldn't install that revision{why}"
                    )));
                }
                d.apply_harness(eng, req)
            })
            .await;
        let mut applied = applied.map_err(|e| {
            e.reworded(|why| {
                format!("{to} now has {from}'s Assignment, but machine.toml was not changed: {why}. Preview again")
            })
        })?;
        // What the copy's own sync failed to place in the other harnesses.
        applied.errors.splice(0..0, copied.errors);
        Ok(applied)
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
                let preview = d.preview_harness(&change, None)?;
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
            let preview = match d.preview_harness(&req.change, None) {
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
