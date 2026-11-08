//! `sk1ll-daemon`: runs on every machine, keeps its harnesses matching the
//! local draft and the server's assignment. The CLI and GUI talk to it over a
//! local API ([`client`]); a direct CLI command can use the same Server client
//! and persistent files while holding the engine lock if the daemon is stopped.
//!
//! - The repo is a full clone whose only remote is the server. Its working tree
//!   is the draft: live in local harnesses, never sent; only its item list
//!   is reported.
//! - The daemon holds core's engine lock on the repo while it runs, so no
//!   other sk1ll process changes it behind its back.
//! - Assignment arrives as whole snapshots over the WebSocket and is applied
//!   only if newer. The snapshot is stored with its revision next to the
//!   state file, so the machine works while the server is away; without one the
//!   harnesses are left alone until the server sends one.
//! - Commits are pushed right away, or queued while offline.
//! - Pulls happen on nudge, on reconnect and periodically. A Blocked pull is
//!   reported and the machine stays on its old baseline until resolved.
//! - Conflicts are reported, never applied; the API adopts or overwrites them.
//! - The machine token lives only in the state file; git gets it from the
//!   environment ([`auth`]).

mod api;
pub mod auth;
pub mod client;
mod config;
mod daemon;
mod error;
pub mod gitx;
mod server;
pub mod join;
mod outbox;
pub mod state;
mod watch;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, bail};
pub use config::{Config, parse_duration};
pub use server::set_assignment_direct;
use daemon::Daemon;
pub use join::{JoinOptions, Joined, join, join_or_rejoin};
pub use state::DaemonState;

static PREPARED: AtomicBool = AtomicBool::new(false);

/// Sets up the process environment for a daemon: git must never prompt, and
/// authenticates to the server with this machine's credentials (see [`auth`]).
///
/// # Safety
///
/// Changes the process environment, so it must run before any other thread
/// exists (in `main`, before the async runtime starts).
pub unsafe fn prepare_process(cfg: &Config) -> anyhow::Result<()> {
    // SAFETY: the caller guarantees no other thread runs.
    unsafe { std::env::set_var("GIT_TERMINAL_PROMPT", "0") };
    if let Some(state) = DaemonState::load(&cfg.state)? {
        for (k, v) in auth::git_credentials_env(&state) {
            // SAFETY: as above.
            unsafe { std::env::set_var(k, v) };
        }
    }
    PREPARED.store(true, Ordering::SeqCst);
    Ok(())
}

/// Runs the daemon until Ctrl-C / SIGTERM. Fails if the machine hasn't
/// joined, or if another process holds the repo's engine lock.
///
/// [`prepare_process`] must have been called first.
pub async fn run(cfg: Config) -> anyhow::Result<()> {
    if !PREPARED.load(Ordering::SeqCst) {
        bail!("sk1ll_daemon::prepare_process must run before sk1ll_daemon::run");
    }
    let state = DaemonState::load(&cfg.state)?.with_context(|| {
        format!(
            "this machine hasn't joined a fleet ({} is missing); run `sk1ll join <server-url> <join-code>`",
            cfg.state.display()
        )
    })?;
    let listener = api::bind(&cfg.socket).await?;
    tracing::info!("local API on {}", cfg.socket.display());
    let socket = cfg.socket.clone();
    let d = Daemon::new(cfg, state)?;
    if let Err(e) = d.blocking(|d, eng| d.lock_repo(eng)).await {
        let _ = std::fs::remove_file(&socket);
        bail!("{e}; not starting");
    }
    if d.is_revoked() {
        tracing::warn!("this machine was removed from the fleet; not managing it (join again to resume)");
    } else {
        match d.blocking(|d, eng| d.startup(eng)).await {
            Err(error::Error::Conflict(e)) if e.contains("another sk1ll process") => {
                let _ = std::fs::remove_file(&socket);
                bail!("{e}; not starting");
            }
            Err(e) => d.set_error(format!("startup: {e}")),
            Ok(()) => {}
        }
        tokio::spawn(watch::run(d.clone()));
        tokio::spawn(server::run(d.clone()));
        tokio::spawn(periodic_pull(d.clone()));
    }
    let served = tokio::select! {
        r = axum::serve(listener, api::router(d)) => r.context("serving the local API"),
        () = shutdown() => Ok(()),
    };
    let _ = std::fs::remove_file(&socket);
    served
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
            let _ = d.blocking(|d, eng| d.pull(eng)).await;
        }
    }
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
