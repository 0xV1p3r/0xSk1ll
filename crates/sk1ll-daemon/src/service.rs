//! Installing and starting the daemon as a user service, for `sk1ll setup`
//! and the GUI. With a systemd user manager it uses the shipped unit, on macOS
//! the shipped launchd agent; otherwise it runs the daemon as a detached
//! background process.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Serialize;

pub const UNIT: &str = "sk1ll-daemon.service";
const TEMPLATE: &str = include_str!("../../../deploy/sk1ll-daemon.service");
/// The launchd agent's label, also its plist's file name.
pub const LABEL: &str = "app.v1p3r.sk1ll.daemon";
const AGENT_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";
const GENERATED_PATH_MARKER: &str = "<!-- sk1ll generated PATH: ";
const AGENT_TEMPLATE: &str = include_str!("../../../deploy/app.v1p3r.sk1ll.daemon.plist");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Started {
    /// Installed if needed, enabled and started as the systemd user unit.
    Systemd,
    /// Installed if needed, loaded and started as the launchd user agent.
    Launchd,
    /// Running in the background until logout, logging to [`log_path`].
    Process,
}

/// The user service manager [`start`] installs the daemon with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Manager {
    Systemd,
    Launchd,
}

impl Manager {
    /// launchd on macOS, systemd elsewhere, whether or not it is running.
    pub fn platform() -> Self {
        if cfg!(target_os = "macos") { Self::Launchd } else { Self::Systemd }
    }

    /// The platform's manager, if it runs for this user.
    pub fn running() -> Option<Self> {
        let manager = Self::platform();
        let running = match manager {
            Self::Systemd => systemd(),
            Self::Launchd => launchd(),
        };
        running.then_some(manager)
    }

    /// The installed unit or agent file, wherever it would go.
    pub fn path(self) -> Option<PathBuf> {
        match self {
            Self::Systemd => unit_path(),
            Self::Launchd => agent_path(),
        }
    }

    /// Whether the service is running.
    pub fn active(self) -> bool {
        match self {
            Self::Systemd => systemctl(&["is-active", UNIT]).is_ok(),
            Self::Launchd => agent_status().is_some_and(|s| s.running),
        }
    }

    /// The service's main process.
    pub fn main_pid(self) -> Option<u32> {
        match self {
            Self::Systemd => {
                let output = Command::new("systemctl")
                    .args(["--user", "show", "--property=MainPID", "--value", UNIT])
                    .stdin(Stdio::null())
                    .output()
                    .ok()?;
                if !output.status.success() {
                    return None;
                }
                String::from_utf8_lossy(&output.stdout).trim().parse().ok().filter(|&pid| pid > 0)
            }
            Self::Launchd => agent_status().and_then(|s| s.pid),
        }
    }

    /// Stops the running service and starts it again.
    pub fn restart(self) -> anyhow::Result<()> {
        match self {
            Self::Systemd => systemctl(&["restart", UNIT]),
            Self::Launchd => {
                let log = agent_log_path().context("cannot determine the home directory")?;
                create_agent_log_dir(&log)?;
                launchctl(&["kickstart", "-k", &agent_target()])
            }
        }
    }
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

/// `~/Library/LaunchAgents/app.v1p3r.sk1ll.daemon.plist`.
pub fn agent_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
}

/// The launchd agent's log, as the shipped plist names it.
pub fn agent_log_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join("Library/Logs/sk1ll/daemon.log"))
}

/// Whether a systemd user manager is running.
pub fn systemd() -> bool {
    cfg!(target_os = "linux") && systemctl(&["show-environment"]).is_ok()
}

/// Whether this user has a launchd GUI domain: not in an SSH session without a login.
pub fn launchd() -> bool {
    cfg!(target_os = "macos") && launchctl(&["print", &gui_domain(uid())]).is_ok()
}

/// Installs the user service if needed and updates generated agents after a binary move.
pub fn start(binary: &Path) -> anyhow::Result<Started> {
    match Manager::running() {
        Some(Manager::Systemd) => start_systemd(binary),
        Some(Manager::Launchd) => start_launchd(binary),
        None => {
            spawn(binary)?;
            Ok(Started::Process)
        }
    }
}

fn start_systemd(binary: &Path) -> anyhow::Result<Started> {
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

/// Like `systemctl enable --now`: clears a disabled override, loads the agent
/// unless it is loaded, and starts it unless it runs.
fn start_launchd(binary: &Path) -> anyhow::Result<Started> {
    let home = dirs::home_dir().context("cannot determine the home directory")?;
    install_and_start_agent(binary, &home, &current_search_path(), uid(), launchctl)?;
    Ok(Started::Launchd)
}

fn create_agent_log_dir(log: &Path) -> anyhow::Result<()> {
    let dir = log.parent().expect("log path has a parent");
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))
}

fn install_and_start_agent(
    binary: &Path,
    home: &Path,
    search_path: &[PathBuf],
    uid: u32,
    mut ctl: impl FnMut(&[&str]) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    use std::io::{ErrorKind, Write};

    let dir = home.join("Library/LaunchAgents");
    let agent = dir.join(format!("{LABEL}.plist"));
    let domain = gui_domain(uid);
    let target = service_target(uid);
    create_agent_log_dir(&home.join("Library/Logs/sk1ll/daemon.log"))?;

    let contents = match std::fs::read(&agent) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(error).with_context(|| format!("reading {}", agent.display())),
    };
    if let Some(contents) = contents {
        let configured = agent_program(&contents);
        if configured.as_deref() != Some(binary) || !executable(binary) {
            let original_path =
                configured.as_deref().and_then(|program| generated_agent_path(&contents, program, home));
            let Some(original_path) = original_path.filter(|_| !agent.is_symlink()) else {
                let program = configured
                    .as_deref()
                    .map_or_else(|| "an unreadable program path".into(), |path| path.display().to_string());
                bail!(
                    "The LaunchAgent at {} uses {}. Edit its Program or first ProgramArguments entry to point to {}, then run:\nlaunchctl bootout {target}\nlaunchctl enable {target} && launchctl bootstrap {domain} \"$HOME/Library/LaunchAgents/{LABEL}.plist\" && launchctl kickstart {target}",
                    agent.display(),
                    program,
                    binary.display()
                );
            };

            let path = original_path.unwrap_or_else(|| agent_search_path(search_path));
            let mut file = tempfile::NamedTempFile::new_in(&dir)?;
            file.write_all(render_agent_with_path(binary, home, Some(&path)).as_bytes())?;
            if ctl(&["print", &target]).is_ok() {
                ctl(&["bootout", &target])?;
            }
            if agent.is_symlink() || std::fs::read(&agent)? != contents {
                bail!("{} changed while starting the daemon. Try again.", agent.display());
            }
            file.persist(&agent).with_context(|| format!("writing {}", agent.display()))?;
        }
    } else if ctl(&["print", &target]).is_err() {
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut file = tempfile::NamedTempFile::new_in(&dir)?;
        let path = agent_search_path(search_path);
        file.write_all(render_agent_with_path(binary, home, Some(&path)).as_bytes())?;
        if let Err(error) = file.persist_noclobber(&agent) {
            if error.error.kind() == ErrorKind::AlreadyExists {
                bail!("{} appeared while starting the daemon. Try again.", agent.display());
            }
            return Err(error.error).with_context(|| format!("writing {}", agent.display()));
        }
    }
    ctl(&["enable", &target])?;
    if ctl(&["print", &target]).is_err() {
        let path = agent.to_str().context("LaunchAgent path is not valid UTF-8")?;
        if let Err(error) = ctl(&["bootstrap", &domain, path])
            && ctl(&["print", &target]).is_err()
        {
            return Err(error);
        }
    }
    ctl(&["kickstart", &target])
}

fn agent_program(contents: &[u8]) -> Option<PathBuf> {
    let value = plist::Value::from_reader(std::io::Cursor::new(contents)).ok()?;
    let dict = value.as_dictionary()?;
    let program = match dict.get("Program") {
        Some(program) => program,
        None => dict.get("ProgramArguments")?.as_array()?.first()?,
    };
    Some(PathBuf::from(program.as_string()?))
}

/// Only an exact template match is eligible for repair. The marker records
/// the generated PATH so an edit to the PATH field also breaks the match.
/// Older generated files match the template without a marker.
fn generated_agent_path(contents: &[u8], program: &Path, home: &Path) -> Option<Option<String>> {
    let xml = std::str::from_utf8(contents).ok()?;
    let path = match xml.lines().find_map(|line| line.strip_prefix(GENERATED_PATH_MARKER)) {
        Some(marker) => Some(String::from_utf8(STANDARD.decode(marker.strip_suffix(" -->")?).ok()?).ok()?),
        None => None,
    };
    (render_agent_with_path(program, home, path.as_deref()) == xml).then_some(path)
}

fn current_search_path() -> Vec<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .filter_map(|dir| std::path::absolute(if dir.as_os_str().is_empty() { PathBuf::from(".") } else { dir }).ok())
        .collect()
}

fn agent_search_path(search_path: &[PathBuf]) -> String {
    let git_dir = search_path.iter().find(|dir| executable(&dir.join("git")));
    match git_dir.and_then(|dir| dir.to_str()) {
        Some(dir) if !dir.contains(':') && !AGENT_PATH.split(':').any(|entry| entry == dir) => {
            format!("{dir}:{AGENT_PATH}")
        }
        _ => AGENT_PATH.into(),
    }
}

/// The shipped plist, running `binary` and logging under `home`.
pub fn render_agent(binary: &Path, home: &Path) -> String {
    let path = agent_search_path(&current_search_path());
    render_agent_with_path(binary, home, Some(&path))
}

fn render_agent_with_path(binary: &Path, home: &Path, path: Option<&str>) -> String {
    let escape = |p: &Path| {
        p.display().to_string().replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
    };
    AGENT_TEMPLATE
        .lines()
        .filter(|line| !line.contains("<!-- Adjust if sk1ll"))
        .map(|line| match line.trim_start() {
            "<string>/Users/USER/.local/bin/sk1ll</string>" => format!("    <string>{}</string>", escape(binary)),
            trimmed if trimmed.starts_with("<string>/Users/USER/") => line.replacen("/Users/USER", &escape(home), 1),
            "<plist version=\"1.0\">" => match path {
                Some(path) => format!("{GENERATED_PATH_MARKER}{} -->\n{line}", STANDARD.encode(path)),
                None => line.into(),
            },
            _ if line.trim_start().strip_prefix("<string>").and_then(|s| s.strip_suffix("</string>"))
                == Some(AGENT_PATH) =>
            {
                format!("    <string>{}</string>", escape(Path::new(path.unwrap_or(AGENT_PATH))))
            }
            _ => line.into(),
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// `gui/<uid>/app.v1p3r.sk1ll.daemon`.
fn agent_target() -> String {
    service_target(uid())
}

fn gui_domain(uid: u32) -> String {
    format!("gui/{uid}")
}

fn service_target(uid: u32) -> String {
    format!("{}/{LABEL}", gui_domain(uid))
}

fn uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// What `launchctl print` says about the loaded agent.
#[derive(Debug, PartialEq, Eq)]
struct AgentStatus {
    running: bool,
    pid: Option<u32>,
}

/// `None` when the agent isn't loaded.
fn agent_status() -> Option<AgentStatus> {
    let output = Command::new("launchctl").args(["print", &agent_target()]).stdin(Stdio::null()).output().ok()?;
    output.status.success().then(|| parse_agent_status(&String::from_utf8_lossy(&output.stdout)))
}

/// Reads the service's own `state` and `pid`, one tab in; nested sections
/// such as its endpoints are indented further.
fn parse_agent_status(print: &str) -> AgentStatus {
    let field = |name: &str| {
        print.lines().find_map(|line| {
            let line = line.strip_prefix('\t').filter(|l| !l.starts_with('\t'))?;
            line.strip_prefix(name)?.strip_prefix(" = ").map(str::trim)
        })
    };
    AgentStatus {
        running: field("state") == Some("running"),
        pid: field("pid").and_then(|p| p.parse().ok()).filter(|&pid| pid > 0),
    }
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

fn launchctl(args: &[&str]) -> anyhow::Result<()> {
    let output = Command::new("launchctl").args(args).stdin(Stdio::null()).output().context("running launchctl")?;
    if !output.status.success() {
        // launchctl writes some errors to stdout.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = if stderr.trim().is_empty() { String::from_utf8_lossy(&output.stdout) } else { stderr };
        bail!("launchctl {} failed: {}", args.join(" "), message.trim());
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
    fn agent_runs_the_binary_and_logs_under_home() {
        let agent =
            render_agent(Path::new("/Applications/0xSk1ll & co.app/Contents/MacOS/sk1ll"), Path::new("/Users/me"));
        assert!(
            agent.contains(
                "<array>\n    <string>/Applications/0xSk1ll &amp; co.app/Contents/MacOS/sk1ll</string>\n    <string>daemon</string>\n  </array>"
            ),
            "{agent}"
        );
        assert!(agent.contains("<key>Label</key>\n  <string>app.v1p3r.sk1ll.daemon</string>"), "{agent}");
        assert_eq!(agent.matches("<string>/Users/me/Library/Logs/sk1ll/daemon.log</string>").count(), 2, "{agent}");
        assert!(!agent.contains("<string>/Users/USER"), "{agent}");
        assert!(!agent.contains(".local/bin"), "{agent}");
        assert!(agent.contains("<key>RunAtLoad</key>\n  <true/>"), "{agent}");
        assert!(agent.ends_with("</plist>\n"));
        assert!(agent_path().unwrap().ends_with("Library/LaunchAgents/app.v1p3r.sk1ll.daemon.plist"));
    }

    #[test]
    fn agent_path_prepends_the_git_directory_and_escapes_it_in_xml() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("git & tools");
        std::fs::create_dir(&bin).unwrap();
        let git = bin.join("git");
        std::fs::write(&git, "").unwrap();
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();

        let path = agent_search_path(&[dir.path().into(), bin.clone()]);
        assert_eq!(path, format!("{}:{AGENT_PATH}", bin.display()));
        let agent = render_agent_with_path(Path::new("/bin/sk1ll"), dir.path(), Some(&path));
        assert!(agent.contains("git &amp; tools:/opt/homebrew/bin:"));
        assert_eq!(generated_agent_path(agent.as_bytes(), Path::new("/bin/sk1ll"), dir.path()), Some(Some(path)));
    }

    #[test]
    fn launchd_reloads_moved_agents_and_preserves_edited_plists() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let binary = home.join("new & app/sk1ll");
        std::fs::create_dir(binary.parent().unwrap()).unwrap();
        std::fs::write(&binary, "").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let agent = home.join("Library/LaunchAgents").join(format!("{LABEL}.plist"));
        std::fs::create_dir_all(agent.parent().unwrap()).unwrap();
        let old_binary = home.join("old & app/sk1ll");
        let custom_path = "/custom/git/bin:/usr/bin";
        let target = service_target(501);
        let mut loaded = true;
        let mut calls = Vec::new();

        for path in [None, Some(custom_path)] {
            let original = render_agent_with_path(&old_binary, home, path);
            let edited = original.replace("<string>info</string>", "<string>debug</string>");
            std::fs::write(&agent, &edited).unwrap();
            let error = install_and_start_agent(&binary, home, &[], 501, |_| panic!("edited plist must be preserved"))
                .unwrap_err()
                .to_string();
            assert!(error.contains(old_binary.to_str().unwrap()));
            assert!(error.contains(&format!("launchctl bootout {target}")));
            assert_eq!(std::fs::read_to_string(&agent).unwrap(), edited);

            std::fs::write(&agent, original).unwrap();
            std::fs::remove_dir(home.join("Library/Logs/sk1ll")).unwrap();
            install_and_start_agent(&binary, home, &[], 501, |args| {
                assert!(home.join("Library/Logs/sk1ll").is_dir());
                calls.push(args.join(" "));
                match args[0] {
                    "print" if !loaded => bail!("unloaded"),
                    "bootout" => loaded = false,
                    "bootstrap" => loaded = true,
                    _ => {}
                }
                Ok(())
            })
            .unwrap();
            assert_eq!(
                calls,
                [
                    format!("print {target}"),
                    format!("bootout {target}"),
                    format!("enable {target}"),
                    format!("print {target}"),
                    format!("bootstrap gui/501 {}", agent.display()),
                    format!("kickstart {target}"),
                ]
            );
            calls.clear();
            assert_eq!(
                std::fs::read_to_string(&agent).unwrap(),
                render_agent_with_path(&binary, home, Some(path.unwrap_or(AGENT_PATH))),
            );
        }

        std::fs::remove_dir(home.join("Library/Logs/sk1ll")).unwrap();
        install_and_start_agent(&binary, home, &[], 501, |args| {
            assert!(home.join("Library/Logs/sk1ll").is_dir());
            calls.push(args.join(" "));
            Ok(())
        })
        .unwrap();
        assert_eq!(calls, [format!("enable {target}"), format!("print {target}"), format!("kickstart {target}")]);
    }

    #[test]
    fn launchctl_targets_the_users_gui_domain() {
        assert_eq!(gui_domain(501), "gui/501");
        assert_eq!(service_target(501), "gui/501/app.v1p3r.sk1ll.daemon");
    }

    #[test]
    fn agent_status_reads_the_services_own_fields() {
        let print = "gui/501/app.v1p3r.sk1ll.daemon = {\n\tactive count = 1\n\tpath = /Users/me/Library/LaunchAgents/app.v1p3r.sk1ll.daemon.plist\n\ttype = LaunchAgent\n\tstate = running\n\n\tprogram = /Users/me/.local/bin/sk1ll\n\tendpoints = {\n\t\tpid = 7\n\t}\n\tpid = 4242\n}\n";
        assert_eq!(parse_agent_status(print), AgentStatus { running: true, pid: Some(4242) });
        let stopped = "gui/501/app.v1p3r.sk1ll.daemon = {\n\tstate = not running\n\tlast exit code = 1\n}\n";
        assert_eq!(parse_agent_status(stopped), AgentStatus { running: false, pid: None });
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
