//! What the repo fetches from and pushes to, and what answers fleet requests
//! ([`Remote`]): the fleet server for a joined Machine ([ADR 0002]), the
//! repo's own `origin` (if it has one) for a standalone one. Fetches and
//! pushes run without the engine mutex ([`Daemon::network`]); what they
//! bring is applied under it, after checking the [`World`] again.
//!
//! [ADR 0002]: ../../../docs/adr/0002-server-is-the-git-remote.md
//! [`World`]: crate::daemon::World

use std::path::Path;
use std::sync::Arc;

use axum::http::Method;
use axum::response::Response;
use bytes::Bytes;
use futures_util::future::BoxFuture;
use sk1ll_core::Repo;
use sk1ll_core::git::plumbing;

use crate::daemon::Daemon;
use crate::error::{Error, Result};
use crate::state::DaemonState;

/// Where the repo's Baseline comes from and commits go.
pub(crate) trait Remote: Send + Sync {
    /// Without the engine mutex: sets up a repo that isn't there yet (the
    /// fleet's clone); a repo that is there is left as it is.
    fn set_up(&self, root: &Path) -> Result<()>;

    /// Whether the repo can fetch and push at all.
    fn check(&self, root: &Path) -> Result<()>;

    /// Without the engine mutex: fetches the Baseline (or clones a missing
    /// repo). Moves neither the working tree, `HEAD` nor the index.
    fn fetch(&self, root: &Path) -> Result<()>;

    /// Whether commits are pushed as soon as they are made, and after every
    /// pull: the fleet's are, a standalone repo's only on request.
    fn pushes_commits(&self) -> bool;

    /// Whether it is worth trying to reach now.
    fn reachable(&self, d: &Daemon) -> bool;

    /// Whether the repo has somewhere to pull from and push to.
    fn has_origin(&self, root: &Path) -> bool;

    /// Whether the remote checks imports' upstreams itself (the server
    /// does, for the Fleet); otherwise the daemon checks them.
    fn checks_imports(&self) -> bool;

    /// Forwards a local API call to the server's `/api`.
    fn proxy<'a>(
        &'a self,
        d: &'a Daemon,
        method: Method,
        path_and_query: &'a str,
        body: Bytes,
    ) -> BoxFuture<'a, Result<Response>>;

    /// Starts what keeps the Machine in step with it while the daemon runs:
    /// the server session and the periodic pull.
    fn run(&self, d: &Arc<Daemon>);
}

/// The remote for a Machine: the server it joined, else its repo's origin.
pub(crate) fn for_machine(joined: Option<&DaemonState>) -> Box<dyn Remote> {
    match joined {
        Some(state) => Box::new(Server { repo_url: state.repo_url() }),
        None => Box::new(Origin),
    }
}

/// The fleet server: the git remote (git authenticates through the process
/// environment, see [`crate::prepare_process`]) and the fleet API.
struct Server {
    repo_url: String,
}

impl Remote for Server {
    fn set_up(&self, root: &Path) -> Result<()> {
        if let Err(e) = plumbing::ensure_clone(root, &self.repo_url, &[]) {
            return Err(Error::Unavailable(format!("setting up {}: {e:#}", root.display())));
        }
        Ok(())
    }

    fn check(&self, _: &Path) -> Result<()> {
        Ok(())
    }

    fn fetch(&self, root: &Path) -> Result<()> {
        if !root.join(".git").exists() {
            plumbing::ensure_clone(root, &self.repo_url, &[])?;
            return Ok(());
        }
        Ok(Repo::open_git_only(root)?.fetch()?)
    }

    fn pushes_commits(&self) -> bool {
        true
    }

    fn reachable(&self, d: &Daemon) -> bool {
        // A daemon that connects on demand has no session to tell: git tries.
        d.is_connected() || d.on_demand()
    }

    fn has_origin(&self, _: &Path) -> bool {
        true
    }

    fn checks_imports(&self) -> bool {
        true
    }

    fn proxy<'a>(
        &'a self,
        d: &'a Daemon,
        method: Method,
        path_and_query: &'a str,
        body: Bytes,
    ) -> BoxFuture<'a, Result<Response>> {
        Box::pin(crate::server::proxy(d, method, path_and_query, body))
    }

    fn run(&self, d: &Arc<Daemon>) {
        tokio::spawn(crate::server::run(d.clone()));
        tokio::spawn(periodic_pull(d.clone()));
    }
}

/// A standalone repo's `origin`, if it has one: fetched and pushed only on
/// request. There is no server to ask.
struct Origin;

impl Remote for Origin {
    fn set_up(&self, _: &Path) -> Result<()> {
        Ok(())
    }

    fn check(&self, root: &Path) -> Result<()> {
        if !plumbing::has_origin(root)? {
            return Err(Error::Conflict("this repo has no origin remote".into()));
        }
        Ok(())
    }

    fn fetch(&self, root: &Path) -> Result<()> {
        Ok(Repo::open_git_only(root)?.fetch()?)
    }

    fn pushes_commits(&self) -> bool {
        false
    }

    fn reachable(&self, _: &Daemon) -> bool {
        true
    }

    fn has_origin(&self, root: &Path) -> bool {
        plumbing::has_origin(root).unwrap_or(false)
    }

    fn checks_imports(&self) -> bool {
        false
    }

    fn proxy<'a>(&'a self, _: &'a Daemon, _: Method, _: &'a str, _: Bytes) -> BoxFuture<'a, Result<Response>> {
        Box::pin(async { Err(standalone()) })
    }

    fn run(&self, _: &Arc<Daemon>) {}
}

/// Why a standalone Machine can't do what needs a Fleet.
pub(crate) fn standalone() -> Error {
    Error::Conflict("This Machine is standalone; join a Fleet to use this".into())
}

/// The fallback for missed nudges.
async fn periodic_pull(d: Arc<Daemon>) {
    let mut tick = tokio::time::interval(d.cfg.pull_every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await;
    loop {
        tick.tick().await;
        if d.is_revoked() {
            return;
        }
        if d.is_connected() {
            let _ = d.pull().await;
        }
    }
}
