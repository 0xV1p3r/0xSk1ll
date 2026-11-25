// Each integration test target shares these helpers and uses its own subset.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use sk1ll_core::{ItemId, ItemRef};
use sk1ll_testkit::{Machine, Skill, SkillsRepo, outcome};

pub const BIN: &str = env!("CARGO_BIN_EXE_sk1ll");

pub fn command(machine: &Machine, args: &[&str]) -> Command {
    machine.sk1ll(BIN, args)
}

pub fn command_for(machine: &Machine, repo: &Path, socket: &Path, args: &[&str]) -> Command {
    let mut command = machine.home.command(BIN);
    command
        .env("SK1LL_REPO", repo)
        .env("SK1LL_MACHINE_CONFIG", machine.machine_toml())
        .env("SK1LL_ASSIGNMENT", machine.assignment())
        .env("SK1LL_DAEMON_STATE", machine.daemon_state())
        .env("SK1LL_DAEMON_SOCKET", socket)
        .args(args);
    command
}

pub fn run(machine: &Machine, args: &[&str]) -> Result<String, String> {
    outcome(command(machine, args).output().unwrap())
}

pub fn run_on(machine: &Machine, socket: &Path, args: &[&str]) -> Result<String, String> {
    outcome(command_for(machine, machine.repo.root(), socket, args).output().unwrap())
}

#[track_caller]
pub fn failure(output: Output) -> String {
    assert_eq!(output.status.code(), Some(1), "{}", sk1ll_testkit::text(&output));
    String::from_utf8(output.stderr).unwrap()
}

pub fn path(path: &Path) -> &str {
    path.to_str().unwrap()
}

pub fn with_pdf() -> Machine {
    let mut machine = Machine::new();
    machine.repo.skill(Skill::new("pdf").description(None).body("PDFs."));
    machine.harness("h");
    machine
}

pub fn with_harnesses() -> Machine {
    let mut machine = Machine::new();
    for name in ["a", "b", "c"] {
        machine.harness(name);
    }
    machine
}

pub fn other_repo(machine: &Machine) -> PathBuf {
    let repo = SkillsRepo::init(machine.root().join("other"));
    repo.skill(Skill::new("web").description(None).body("Web."));
    repo.commit_all("web");
    repo.root().to_path_buf()
}

pub fn daemon_config(machine: &Machine, repo: &Path) -> sk1ll_daemon::Config {
    sk1ll_daemon::Config {
        repo: Some(repo.into()),
        state: machine.daemon_state(),
        machine_config: machine.machine_toml(),
        assignment: machine.assignment(),
        socket: machine.no_daemon(),
        state_dir: machine.home.state_dir(),
        cache_dir: machine.home.cache_dir(),
        ..sk1ll_daemon::Config::new().unwrap()
    }
}

pub fn joined(machine: &Machine, server_url: &str) -> String {
    let state =
        sk1ll_daemon::DaemonState::new("test".into(), server_url.into(), "tok".into(), machine.repo.root().into());
    state.save(&machine.daemon_state()).unwrap();
    state.registration_key()
}

pub fn item_id(machine: &Machine, item: ItemRef) -> ItemId {
    let repo = machine.repo.open();
    repo.assign_missing_ids().unwrap();
    repo.item_ids()
        .unwrap()
        .into_iter()
        .find(|(found, _)| *found == item)
        .and_then(|(_, id)| id)
        .expect("an item with an Item ID")
}

/// Runs the CLI's daemon command until the test drops it.
pub struct RunningDaemon {
    child: std::process::Child,
    pub socket: PathBuf,
}

impl RunningDaemon {
    pub async fn start(machine: &Machine, repo: &Path) -> Self {
        use std::process::Stdio;
        let socket = machine.home.path().join("run/daemon.sock");
        let child = command_for(machine, repo, &socket, &["daemon"]).stdout(Stdio::null()).spawn().unwrap();
        let daemon = Self { child, socket };
        let client = sk1ll_daemon::client::Client::new(&daemon.socket);
        sk1ll_testkit::eventually(|| async { client.status().await.ok() }).await;
        daemon
    }
}

impl Drop for RunningDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

/// Join as the first Machine when there is no code, or consume the supplied code.
pub fn join_fleet(machine: &Machine, server: &sk1ll_testkit::server::Server, id: &str, code: Option<&str>) {
    use sk1ll_testkit::{git, server::ADMIN};
    // Machine creates a seeded repo. Joining starts with an empty destination.
    std::fs::remove_dir_all(machine.repo.root()).unwrap();
    let url = server.url();
    let mut args = vec!["join", &url, "--id", id];
    if let Some(code) = code {
        args.push(code);
    } else {
        args.extend(["--admin-token", ADMIN]);
    }
    let output = run(machine, &args).unwrap();
    assert!(output.contains(&format!("joined the fleet as {id}")), "{output}");
    git::configure(machine.repo.root());
}
