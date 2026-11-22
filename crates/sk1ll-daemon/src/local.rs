//! An engine in this process ([`Engine`]): what the CLI runs a command
//! against when no daemon is running. It is the daemon's engine, reconcile
//! and local API, bound to no socket: a [`Client`] calls its router
//! directly. It runs nothing in the background (no file watcher, periodic
//! reconcile or pull), and connects to the server only for a request that
//! needs the session (an Assignment write, a Harness rename); it does not
//! catch up on connecting. Dropping it, or the runtime it runs on, ends it.
//!
//! Like the daemon, it takes the machine lock ([`MachineLock`]) before it
//! reads any of the Machine's state and holds it until it is dropped, so it
//! refuses to run while another engine (the daemon, for this repo or
//! another, or another command's) manages the Machine; and core's engine
//! lock on the repo for whatever changes it. A command that only reads
//! starts a read-only engine instead ([`Engine::read_only`]): it takes no
//! lock and changes nothing. See ADR 0008.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::bail;

use crate::client::Client;
use crate::daemon::{Access, Daemon};
use crate::{Config, DaemonState, MachineLock, PREPARED, api};

/// The daemon's engine for one command, in this process.
pub struct Engine {
    daemon: Arc<Daemon>,
    client: Client,
}

impl Engine {
    /// The engine for `cfg`'s Machine: joined if its state file says so,
    /// else standalone on `cfg.repo` (located like the daemon's). Fails
    /// while another process holds the machine lock. Must run within a
    /// Tokio runtime, after [`crate::prepare_process`].
    pub fn start(cfg: Config) -> anyhow::Result<Self> {
        Self::with(cfg, true)
    }

    /// An engine for a command that only reads (`status`, `render`, `sync
    /// --dry-run`, ...): it holds no lock, so it runs beside another engine,
    /// and changes nothing, not even what a reconcile would bring in line.
    /// Its readers answer from the files as they are; every request that
    /// would change something is refused.
    pub fn read_only(cfg: Config) -> anyhow::Result<Self> {
        Self::with(cfg, false)
    }

    fn with(cfg: Config, writer: bool) -> anyhow::Result<Self> {
        if !PREPARED.load(Ordering::SeqCst) {
            bail!("sk1ll_daemon::prepare_process must run before an engine starts");
        }
        sk1ll_core::git::require_git(sk1ll_core::git::MIN_VERSION)?;
        // Before anything of the Machine is read.
        let access = if writer { Access::OnDemand(MachineLock::acquire(&cfg)?) } else { Access::ReadOnly };
        let state = DaemonState::load(&cfg.state)?;
        let daemon = Daemon::for_machine(cfg, state, access)?;
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
