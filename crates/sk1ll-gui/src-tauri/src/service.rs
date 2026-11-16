//! Starts this Machine's Daemon when the app can't reach it. This is the one
//! thing the app does without the Daemon; everything else goes through it.
use serde::Serialize;
use sk1ll_daemon::service::{self, Environment, Started};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServiceInfo {
    /// The app uses the default socket, so a Daemon started here is the one it talks to.
    pub default_socket: bool,
    /// The `sk1ll` executable that runs the Daemon.
    pub binary: Option<PathBuf>,
    /// A systemd user manager is running.
    pub systemd: bool,
    /// The user unit, if it is installed.
    pub unit: Option<PathBuf>,
}

pub fn info() -> ServiceInfo {
    ServiceInfo {
        default_socket: default_socket(),
        binary: Environment::current().find_binary(),
        systemd: service::systemd(),
        unit: service::unit_path().filter(|p| p.is_file()),
    }
}

fn default_socket() -> bool {
    std::env::var_os("SK1LL_DAEMON_SOCKET").is_none_or(|v| v.is_empty())
}

pub fn start() -> Result<Started, String> {
    if !default_socket() {
        return Err("This app uses a custom Daemon socket (SK1LL_DAEMON_SOCKET). Start that Daemon yourself.".into());
    }
    let binary = Environment::current().find_binary().ok_or(
        "The sk1ll command wasn't found next to this app, on PATH, in ~/.local/bin or in ~/.cargo/bin. Install it, then try again.",
    )?;
    service::start(&binary).map_err(|e| format!("{e:#}"))
}
