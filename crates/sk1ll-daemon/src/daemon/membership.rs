use super::*;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;

use crate::error::{Error, Result};
use crate::join::{Joined, Registered};
use crate::lifecycle::Lifecycle;
use sk1ll_core::git::plumbing;

impl Daemon {
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
    /// removed, or the server rejects its token. `Some`: the world it may
    /// replace, which [`Daemon::rejoin`] checks again.
    pub(crate) fn can_rejoin(&self) -> Option<World> {
        let lc = lock(&self.lifecycle);
        lc.can_rejoin().then(|| World { epoch: lc.epoch() })
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

    pub(super) fn activate_standalone_join(
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
            plumbing::check_standalone_join(self.repo_root(), &reg.state().repo_url())
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
    pub(super) fn halt_with(&self, f: impl FnOnce() -> anyhow::Result<bool>) -> anyhow::Result<()> {
        let mut lc = lock(&self.lifecycle);
        let result = f();
        if !matches!(result, Ok(false)) {
            lc.halt();
        }
        drop(lc);
        self.update(|_| {});
        result.map(drop)
    }

    /// Checks that the registration `world` allowed a join to replace still
    /// is replaceable, runs `store` and halts the daemon, all in one critical
    /// section of the lifecycle mutex: the session can't connect the old
    /// registration in between, and once halted its connection is ignored.
    /// The outer `Err` is `store`'s (it halts too); the inner one says why
    /// nothing was stored.
    fn halt_for_rejoin(
        &self,
        world: World,
        store: impl FnOnce() -> anyhow::Result<()>,
    ) -> anyhow::Result<std::result::Result<(), &'static str>> {
        let mut lc = lock(&self.lifecycle);
        if let Err(why) = lc.check_rejoin(world.epoch) {
            return Ok(Err(why));
        }
        let stored = store();
        lc.halt();
        drop(lc);
        self.update(|_| {});
        stored.map(Ok)
    }

    /// Second half of a joined `POST /join`: stores the new credentials and
    /// snapshot and halts the daemon until the restart ([`Daemon::restart`])
    /// that brings them into effect, then sets up the repo with them, without
    /// the engine mutex (a halted daemon changes nothing). The outer error
    /// means nothing was stored; the inner one that the repo setup failed (the
    /// restarted daemon retries it).
    ///
    /// `world`: what the join was allowed to replace. The registration
    /// request ran without the engine mutex; if the old registration
    /// recovered meanwhile (the session connected again, so the token works
    /// after all), the new one is not stored: it is removed from the Fleet
    /// again, as far as the server can be reached.
    pub(crate) async fn rejoin(self: &Arc<Self>, reg: Registered, world: World) -> Result<Result<Joined>> {
        let stored = self
            .blocking(move |d, eng| {
                #[cfg(test)]
                let hook = eng.before_rejoin_persist.take();
                #[cfg(not(test))]
                let _ = eng;
                let checked = d.halt_for_rejoin(world, || {
                    #[cfg(test)]
                    if let Some(hook) = hook {
                        hook();
                    }
                    reg.persist(&d.cfg)
                })?;
                Ok(match checked {
                    Ok(()) => {
                        tracing::info!("joined again as `{}`", reg.state().machine_id);
                        Ok(reg)
                    }
                    Err(why) => Err((reg, why)),
                })
            })
            .await?;
        let reg = match stored {
            Ok(reg) => reg,
            Err((reg, why)) => {
                reg.discard(&self.http).await;
                return Err(Error::Conflict(why.into()));
            }
        };
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
