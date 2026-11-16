//! Installing and starting the daemon as a user service, for `sk1ll setup`
//! and the GUI. With a systemd user manager it uses the shipped unit;
//! otherwise it runs the daemon as a detached background process.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, bail};
use serde::Serialize;

pub const UNIT: &str = "sk1ll-daemon.service";
const TEMPLATE: &str = include_str!("../../../deploy/sk1ll-daemon.service");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Started {
    /// Installed if needed, enabled and started as the systemd user unit.
    Systemd,
    /// Running in the background until logout, logging to [`log_path`].
    Process,
}

/// Where [`find_binary`] looks, so tests can supply their own.
pub struct Environment {
    pub exe_dir: Option<PathBuf>,
    pub path: Vec<PathBuf>,
    pub home: Option<PathBuf>,
}

impl Environment {
    pub fn current() -> Self {
        Self {
            exe_dir: std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)),
            path: std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .filter(|p| p.is_absolute())
                .collect(),
            home: dirs::home_dir(),
        }
    }

    /// Next to the running executable first, so a bundled CLI matches its
    /// version. A desktop launcher's PATH often lacks `~/.local/bin` (where
    /// the release archive installs) and `~/.cargo/bin`, so those come last.
    pub fn find_binary(&self) -> Option<PathBuf> {
        let user = [".local/bin", ".cargo/bin"].map(|d| self.home.as_ref().map(|h| h.join(d)));
        self.exe_dir
            .iter()
            .chain(&self.path)
            .chain(user.iter().flatten())
            .map(|dir| dir.join("sk1ll"))
            .find(|p| executable(p))
    }
}

fn executable(path: &Path) -> bool {
    path.is_file() && path.metadata().is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

/// `$XDG_CONFIG_HOME/systemd/user/sk1ll-daemon.service`.
pub fn unit_path() -> Option<PathBuf> {
    dirs::config_dir().map(|c| c.join("systemd/user").join(UNIT))
}

/// The background process's log when there is no systemd.
pub fn log_path() -> Option<PathBuf> {
    dirs::cache_dir().map(|c| c.join("sk1ll/daemon.log"))
}

/// Whether a systemd user manager is running.
pub fn systemd() -> bool {
    cfg!(target_os = "linux") && systemctl(&["show-environment"]).is_ok()
}

/// Uses an installed unit as it is; otherwise writes one for `binary`.
pub fn start(binary: &Path) -> anyhow::Result<Started> {
    if !systemd() {
        spawn(binary)?;
        return Ok(Started::Process);
    }
    let unit = unit_path().context("cannot determine the config directory")?;
    if !unit.exists() {
        let dir = unit.parent().expect("unit path has a parent");
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::write(&unit, render_unit(binary)).with_context(|| format!("writing {}", unit.display()))?;
        systemctl(&["daemon-reload"])?;
    }
    systemctl(&["enable", "--now", UNIT])?;
    Ok(Started::Systemd)
}

/// The shipped unit, with `ExecStart` pointing at `binary`.
pub fn render_unit(binary: &Path) -> String {
    let quoted =
        binary.display().to_string().replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%").replace('$', "$$");
    TEMPLATE
        .lines()
        .filter(|line| !line.starts_with("# Adjust if sk1ll") && !line.starts_with("# %h/.cargo/bin"))
        .map(|line| if line.starts_with("ExecStart=") { format!("ExecStart=\"{quoted}\" daemon") } else { line.into() })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn spawn(binary: &Path) -> anyhow::Result<()> {
    let log = log_path().context("cannot determine the cache directory")?;
    let dir = log.parent().expect("log path has a parent");
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .with_context(|| format!("opening {}", log.display()))?;
    let mut child = Command::new(binary)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(file)
        // Its own process group, so closing the caller or its terminal doesn't stop it.
        .process_group(0)
        .spawn()
        .with_context(|| format!("starting {}", binary.display()))?;
    // Reap it if it exits while the caller still runs.
    std::thread::spawn(move || child.wait());
    Ok(())
}

fn systemctl(args: &[&str]) -> anyhow::Result<()> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("running systemctl")?;
    if !output.status.success() {
        bail!("systemctl --user {} failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_points_at_the_binary() {
        let unit = render_unit(Path::new("/opt/my apps/100%/sk1ll"));
        assert!(unit.contains("\nExecStart=\"/opt/my apps/100%%/sk1ll\" daemon\n"), "{unit}");
        assert_eq!(unit.matches("ExecStart=").count(), 1);
        assert!(!unit.contains(".local/bin"), "{unit}");
        assert!(unit.contains("[Install]\nWantedBy=default.target\n"), "{unit}");
    }

    #[test]
    fn binary_is_found_next_to_the_executable_then_on_path_then_in_the_user_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let make = |sub: &str| {
            let path = dir.path().join(sub).join("sk1ll");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        let env = || Environment {
            exe_dir: Some(dir.path().join("app")),
            path: vec![dir.path().join("bin")],
            home: Some(dir.path().join("home")),
        };
        assert_eq!(env().find_binary(), None);
        let cargo = make("home/.cargo/bin");
        assert_eq!(env().find_binary(), Some(cargo));
        let local = make("home/.local/bin");
        assert_eq!(env().find_binary(), Some(local));
        let on_path = make("bin");
        assert_eq!(env().find_binary(), Some(on_path.clone()));
        let bundled = make("app");
        assert_eq!(env().find_binary(), Some(bundled.clone()));
        std::fs::set_permissions(&bundled, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(env().find_binary(), Some(on_path));
    }
}
