use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use sk1ll_core::machine::{InstructionsFormat, LinkMode};

use crate::{Home, SkillsRepo};

/// `[harnesses.<preset>] enabled = false` for every preset: presets would
/// find harnesses in HOME.
pub fn presets_off() -> String {
    sk1ll_core::machine::presets().map(|p| format!("[harnesses.{p}]\nenabled = false\n")).collect()
}

/// One `[harnesses.<name>]` table of a `machine.toml`.
#[derive(Debug, Clone)]
pub struct Harness {
    pub name: String,
    pub skills: Option<PathBuf>,
    pub mode: LinkMode,
    pub instructions: Option<(PathBuf, InstructionsFormat)>,
}

impl Harness {
    /// Skills copied into `skills`.
    pub fn copy(name: &str, skills: impl Into<PathBuf>) -> Self {
        Self { name: name.into(), skills: Some(skills.into()), mode: LinkMode::Copy, instructions: None }
    }

    /// Skills symlinked into `skills`.
    pub fn symlink(name: &str, skills: impl Into<PathBuf>) -> Self {
        Self { mode: LinkMode::Symlink, ..Self::copy(name, skills) }
    }

    /// Instructions too: one block file, or a directory of rule files.
    pub fn instructions(mut self, path: impl Into<PathBuf>, format: InstructionsFormat) -> Self {
        self.instructions = Some((path.into(), format));
        self
    }

    /// The table, as TOML.
    pub fn toml(&self) -> String {
        let mut out = format!("[harnesses.{}]\n", self.name);
        if let Some(skills) = &self.skills {
            out.push_str(&format!("skills = {}\n", quote(skills)));
        }
        if let Some((path, format)) = &self.instructions {
            let format = match format {
                InstructionsFormat::Block => "block",
                InstructionsFormat::Rules => "rules",
            };
            out.push_str(&format!("instructions = {}\ninstructions_format = \"{format}\"\n", quote(path)));
        }
        let mode = match self.mode {
            LinkMode::Copy => "copy",
            LinkMode::Symlink => "symlink",
        };
        out.push_str(&format!("mode = \"{mode}\"\n"));
        out
    }
}

fn quote(path: &Path) -> String {
    format!("{:?}", path.to_str().expect("a UTF-8 path"))
}

/// A `machine.toml`: an optional Machine ID, every preset off, and the
/// harnesses added.
#[derive(Debug, Clone, Default)]
pub struct MachineToml {
    pub id: Option<String>,
    pub harnesses: Vec<Harness>,
}

impl MachineToml {
    /// No ID, presets off, no harness.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn id(mut self, id: &str) -> Self {
        self.id = Some(id.into());
        self
    }

    pub fn harness(mut self, harness: Harness) -> Self {
        self.harnesses.push(harness);
        self
    }

    pub fn toml(&self) -> String {
        let mut out = self.id.as_ref().map(|id| format!("id = {id:?}\n")).unwrap_or_default();
        out.push_str(&presets_off());
        for harness in &self.harnesses {
            out.push_str(&harness.toml());
        }
        out
    }

    pub fn write(&self, path: &Path) {
        crate::write(path, &self.toml());
    }
}

/// One Machine's files, without a daemon, in one temporary directory: a
/// [`SkillsRepo`] at `repo/`, `machine.toml` (ID `test`, presets off),
/// `assignment.json`, the daemon's state file `state/daemon.json`, and a
/// [`Home`] for the commands run on it. No daemon listens on its socket.
#[derive(Debug)]
pub struct Machine {
    pub repo: SkillsRepo,
    pub home: Home,
    config: MachineToml,
    root: tempfile::TempDir,
}

impl Default for Machine {
    fn default() -> Self {
        Self::new()
    }
}

impl Machine {
    pub fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let repo = SkillsRepo::init(root.path().join("repo"));
        let config = MachineToml::new().id("test");
        config.write(&root.path().join("machine.toml"));
        Self { repo, home: Home::new(), config, root }
    }

    /// The directory everything is in.
    pub fn root(&self) -> &Path {
        self.root.path()
    }

    pub fn machine_toml(&self) -> PathBuf {
        self.root().join("machine.toml")
    }

    pub fn assignment(&self) -> PathBuf {
        self.root().join("assignment.json")
    }

    /// The daemon's state file (`daemon.json`): absent until joined.
    pub fn daemon_state(&self) -> PathBuf {
        self.root().join("state/daemon.json")
    }

    /// A socket path no daemon listens on.
    pub fn no_daemon(&self) -> PathBuf {
        self.root().join("no-daemon.sock")
    }

    /// Adds a harness to `machine.toml`, rewriting it.
    pub fn add_harness(&mut self, harness: Harness) {
        self.config.harnesses.push(harness);
        self.config.write(&self.machine_toml());
    }

    /// Adds `name`, copying skills into `<root>/<name>`; returns that directory.
    pub fn harness(&mut self, name: &str) -> PathBuf {
        let skills = self.root().join(name);
        self.add_harness(Harness::copy(name, &skills));
        skills
    }

    /// The `sk1ll` binary `bin` (`env!("CARGO_BIN_EXE_sk1ll")`) on this
    /// Machine's repo, config, Assignment and state, with its [`Home`] and
    /// no daemon, then `args`.
    pub fn sk1ll(&self, bin: impl AsRef<OsStr>, args: &[&str]) -> Command {
        let mut cmd = self.home.command(bin);
        cmd.arg("--repo")
            .arg(self.repo.root())
            .arg("--machine-config")
            .arg(self.machine_toml())
            .arg("--assignment")
            .arg(self.assignment())
            .arg("--daemon-socket")
            .arg(self.no_daemon())
            .arg("--daemon-state")
            .arg(self.daemon_state())
            .args(args);
        cmd
    }
}
