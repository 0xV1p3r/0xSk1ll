//! `sk1ll-daemon` keeps local harnesses matching the draft and assignment.
//! The CLI and GUI use its local API ([`client`]). Without `daemon.json`, it
//! runs standalone against the same repo and machine-local assignment as the
//! direct CLI. Joined machines use the fleet's repo and assignment snapshots.
//!
//! - Core's engine lock stays held while the daemon manages a repo.
//! - File watches and periodic reconciliation apply local changes in both modes.
//! - Standalone commits stay local; explicit push/pull use the repo's origin.
//! - Joined commits push immediately or queue while offline. Assignment arrives
//!   as revisioned snapshots, and writes follow the status on the same session.
//! - Conflicts are reported, never overwritten without an explicit action.
//! - Fleet credentials live in daemon.json; git gets them through the process
//!   environment ([`auth`]). Standalone never opens a server session.

mod api;
pub mod auth;
pub mod client;
mod config;
mod daemon;
mod error;
pub mod join;
mod lifecycle;
pub mod local;
mod notifications;
mod remote;
mod server;
pub mod service;
pub mod state;
mod store;
mod watch;

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, bail};
pub use config::{Config, locate_repo, parse_duration};
use daemon::Daemon;
pub use join::{JoinOptions, Joined, join, join_or_rejoin};
pub use server::{rename_harness_direct, write_assignment_direct};
pub use state::DaemonState;

static PREPARED: AtomicBool = AtomicBool::new(false);

/// Sets up the process environment for a daemon: git must never prompt, never
/// writes the index for a command that only reads (the local API's readers
/// run beside the engine, see [`auth::git_read_only_env`]), and authenticates to the server with this machine's credentials (see [`auth`]).
///
/// # Safety
///
/// Changes the process environment, so it must run before any other thread
/// exists (in `main`, before the async runtime starts).
pub unsafe fn prepare_process(cfg: &Config) -> anyhow::Result<()> {
    // SAFETY: the caller guarantees no other thread runs.
    unsafe { std::env::set_var("GIT_TERMINAL_PROMPT", "0") };
    // Before the credentials: each merge reads what is already set.
    for (k, v) in auth::git_read_only_env() {
        // SAFETY: as above.
        unsafe { std::env::set_var(k, v) };
    }
    if let Some(state) = DaemonState::load(&cfg.state)? {
        for (k, v) in auth::git_credentials_env(&state) {
            // SAFETY: as above.
            unsafe { std::env::set_var(k, v) };
        }
    }
    PREPARED.store(true, Ordering::SeqCst);
    Ok(())
}

/// Runs until Ctrl-C / SIGTERM, standalone when daemon.json is absent.
/// Fails if another process holds the repo's engine lock.
///
/// [`prepare_process`] must have been called first.
pub async fn run(cfg: Config) -> anyhow::Result<()> {
    if !PREPARED.load(Ordering::SeqCst) {
        bail!("sk1ll_daemon::prepare_process must run before sk1ll_daemon::run");
    }
    sk1ll_core::git::require_git(sk1ll_core::git::MIN_VERSION)?;
    let state = DaemonState::load(&cfg.state)?;
    let d = match state {
        Some(state) => Daemon::new(cfg, state)?,
        None => Daemon::standalone(cfg)?,
    };
    let listener = api::bind(&d.cfg.socket).await?;
    tracing::info!("sk1ll daemon {}: local API on {}", sk1ll_core::VERSION, d.cfg.socket.display());
    let socket = d.cfg.socket.clone();
    if let Err(e) = d.blocking(|d, eng| d.lock_repo(eng)).await {
        let _ = std::fs::remove_file(&socket);
        bail!("{e}; not starting");
    }
    if d.is_revoked() {
        tracing::warn!("this machine was removed from the fleet; not managing it (join again to resume)");
    } else {
        match d.startup().await {
            Err(error::Error::Conflict(e)) if e.contains("another sk1ll process") => {
                let _ = std::fs::remove_file(&socket);
                bail!("{e}; not starting");
            }
            Err(e) => d.set_error(format!("startup: {e}")),
            Ok(()) => {}
        }
        tokio::spawn(daemon::reconcile_loop(d.clone()));
        tokio::spawn(watch::run(d.clone()));
        d.remote.run(&d);
    }
    let notifications = tokio::spawn(notifications::run(d.clone()));
    let served = tokio::select! {
        r = axum::serve(listener, api::router(d)) => r.context("serving the local API"),
        () = shutdown() => Ok(()),
    };
    notifications.abort();
    let _ = std::fs::remove_file(&socket);
    served
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    tokio::select! { () = ctrl_c => {}, () = term => {} }
    tracing::info!("shutting down");
}

/// Sets up logging (`RUST_LOG`, default `info`) for a daemon process.
pub fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).try_init();
}
