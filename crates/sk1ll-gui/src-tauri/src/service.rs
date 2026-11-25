//! Starts or restarts this Machine's Daemon when the app can't connect to its local API.
use serde::Serialize;
use sk1ll_daemon::service::{self, Environment, Manager, Started};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServiceInfo {
    /// The app uses the default socket, so a Daemon started here is the one it talks to.
    pub default_socket: bool,
    /// The `sk1ll` executable that runs the Daemon.
    pub binary: Option<PathBuf>,
    /// This platform's user service manager: systemd.
    pub manager: Manager,
    /// That manager is running for this user, so starting installs the user service.
    pub available: bool,
    /// The user service is currently running.
    pub active: bool,
    /// The Daemon on the app's socket is the user service's main process, so restarting the
    /// service restarts the Daemon the app talks to.
    pub restartable: bool,
    /// The systemd user unit, if it is installed.
    pub unit: Option<PathBuf>,
}

pub fn info() -> ServiceInfo {
    let manager = Manager::platform();
    let available = Manager::running().is_some();
    let active = available && manager.active();
    ServiceInfo {
        default_socket: default_socket(),
        binary: Environment::current().find_binary(),
        manager,
        available,
        active,
        restartable: active && default_socket() && socket_served_by_unit(manager),
        unit: manager.path().filter(|p| p.is_file()),
    }
}

/// Compares the process serving the default socket with the user service's main process. A
/// unit with its own socket override and a Daemon started in a terminal can both be running.
fn socket_served_by_unit(manager: Manager) -> bool {
    match (socket_peer_pid(), manager.main_pid()) {
        (Some(peer), Some(main)) => peer == main,
        _ => false,
    }
}

fn socket_peer_pid() -> Option<u32> {
    use std::os::fd::AsRawFd;
    let client = sk1ll_daemon::client::Client::default_socket().ok()?;
    let stream = std::os::unix::net::UnixStream::connect(client.socket()).ok()?;
    peer_pid(stream.as_raw_fd())
}

#[cfg(target_os = "linux")]
fn peer_pid(fd: std::os::fd::RawFd) -> Option<u32> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are valid for writes and describe a `ucred`, which is what
    // SO_PEERCRED fills in.
    let rc = unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_PEERCRED, (&raw mut cred).cast(), &mut len) };
    (rc == 0 && cred.pid > 0).then_some(cred.pid as u32)
}

// Without the peer's pid the owner can't be established, so the app shows the commands instead.
#[cfg(not(target_os = "linux"))]
fn peer_pid(_fd: std::os::fd::RawFd) -> Option<u32> {
    None
}

pub fn restart() -> Result<(), String> {
    if !default_socket() {
        return Err("This app uses a custom Daemon socket (SK1LL_DAEMON_SOCKET). Restart that Daemon yourself.".into());
    }
    let manager = Manager::platform();
    let name = "the sk1ll-daemon user service";
    if Manager::running().is_none() || !manager.active() {
        return Err(format!("{name} isn't running."));
    }
    if !socket_served_by_unit(manager) {
        return Err(format!("The Daemon on this app's socket isn't {name}. Restart it where you started it."));
    }
    manager.restart().map_err(|e| format!("{e:#}"))
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
