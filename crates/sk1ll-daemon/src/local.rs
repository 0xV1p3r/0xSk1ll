//! An engine in this process ([`Engine`]): what the CLI runs a command
//! against when no daemon is running. It is the daemon's engine, reconcile
//! and local API, bound to no socket: a [`Client`] calls its router
//! directly. It runs nothing in the background (no file watcher, periodic
//! reconcile or pull), and connects to the server only for a request that
//! needs the session (an Assignment write, a Harness rename); it does not
//! catch up on connecting. Dropping it, or the runtime it runs on, ends it.
//!
//! Like the daemon, it takes core's engine lock on the repo for whatever
//! changes it, so it refuses to change a repo another process (a daemon, or
//! another command) is changing. See ADR 0008.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::bail;

use crate::client::Client;
use crate::daemon::Daemon;
use crate::{Config, DaemonState, PREPARED, api};

/// The daemon's engine for one command, in this process.
pub struct Engine {
    daemon: Arc<Daemon>,
    client: Client,
}

impl Engine {
    /// The engine for `cfg`'s Machine: joined if its state file says so,
    /// else standalone on `cfg.repo` (located like the daemon's). Must run
    /// within a Tokio runtime, after [`crate::prepare_process`].
    pub fn start(cfg: Config) -> anyhow::Result<Self> {
        if !PREPARED.load(Ordering::SeqCst) {
            bail!("sk1ll_daemon::prepare_process must run before an engine starts");
        }
        sk1ll_core::git::require_git(sk1ll_core::git::MIN_VERSION)?;
        let state = DaemonState::load(&cfg.state)?;
        let daemon = Daemon::for_machine(cfg, state, true)?;
        let client = Client::in_process(api::router(daemon.clone()));
        Ok(Self { daemon, client })
    }

    /// A client of this engine's local API.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// The repo it manages.
    pub fn repo(&self) -> &std::path::Path {
        self.daemon.ident.repo()
    }
}
