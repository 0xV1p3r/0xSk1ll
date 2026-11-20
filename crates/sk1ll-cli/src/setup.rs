//! `sk1ll setup`: first-run setup on the command line, the same steps as the
//! GUI's setup wizard. It goes through a running daemon when there is one.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sk1ll_core::{HarnessChoice, MachineConfig, Repo};
use sk1ll_daemon::client::{Client, Connection, DaemonMode};
use sk1ll_daemon::service::{self, Manager, Started};
use sk1ll_daemon::{DaemonState, JoinOptions};
use sk1ll_proto::{AssignmentChanges, ToggleChange, ToggleKind};

use crate::Cli;
use crate::fleet::{client, daemon_config, runtime};

pub(crate) fn run(cli: &Cli) -> Result<()> {
    let cfg = daemon_config(cli)?;
    let rt = runtime()?;
    let client = client(cli)?;
    let daemon = match rt.block_on(client.status()) {
        Ok(status) => Some(status),
        Err(e) if e.is_not_running() => None,
        Err(e) => bail!("a daemon may be running but can't be reached ({e}); stop it or try again"),
    };
    if let Some(state) = DaemonState::load(&cfg.state)? {
        if state.revoked {
            bail!("this machine was removed from its fleet; join again with `sk1ll join <server-url> <code>`");
        }
        println!("this machine already joined {} as `{}`; nothing to set up", state.server_url, state.machine_id);
        return Ok(());
    }
    let repo = match &daemon {
        Some(status) => status.repo.clone(),
        None => sk1ll_daemon::locate_repo(cli.repo.as_deref(), false)?,
    };
    if repo.join(".git").exists() {
        println!("this machine already has a skills repo at {}; nothing to set up", repo.display());
        return Ok(());
    }
    sk1ll_core::git::require_git(sk1ll_core::git::MIN_VERSION)?;
    let mut input = Input::stdin();

    println!("How will this machine use 0xSk1ll?");
    println!("  1) Only this machine: a local skills repo; you can join a fleet later");
    println!("  2) Join a fleet: you have a join code from another machine");
    println!("  3) Join as the first machine: you have the deployed server's admin token");
    let path = loop {
        match input.line("Choose [1]: ")?.as_str() {
            "" | "1" => break 1,
            "2" => break 2,
            "3" => break 3,
            _ => println!("enter 1, 2 or 3"),
        }
    };

    println!();
    choose_harnesses(&mut input, &cfg.machine_config, &repo, daemon.is_some().then_some(&client), &rt)?;
    println!();

    let joined = if path == 1 {
        match &daemon {
            Some(_) => rt.block_on(client.init())?,
            None => {
                let _lock = crate::lock_direct(&repo)?;
                std::fs::create_dir_all(&repo)?;
                for warning in MachineConfig::adopt_detected_presets(&cfg.machine_config, &repo)? {
                    eprintln!("{warning}");
                }
                Repo::init_committed(&repo)?;
            }
        }
        println!("created the skills repo at {}", repo.display());
        None
    } else {
        Some(join(&mut input, &cfg, path == 3, &repo, &rt)?)
    };

    if let Some((machine_id, copied, via_daemon)) = &joined
        && !copied
    {
        println!();
        println!("{machine_id} starts with every skill and rule off.");
        if input.yes("Turn on everything, including skills and rules added later?", true)? {
            turn_everything_on(&cfg, &client, machine_id, *via_daemon, &rt)?;
            println!("turned on everything for {machine_id}");
        } else {
            println!("assign items with `sk1ll assign` or in the GUI's Assignment matrix");
        }
    }

    if daemon.is_none() {
        println!();
        if !start_daemon(&mut input, cli)? {
            let overrides = path_overrides(cli, &repo);
            println!("Preview and apply changes to your harnesses manually:");
            println!("  sk1ll{overrides} sync --dry-run");
            println!("  sk1ll{overrides} sync");
        }
    }
    println!();
    println!("Next, let agents add skills for you:");
    println!("  sk1ll add https://github.com/0xV1p3r/0xSk1ll/tree/main/skills/sk1ll");
    Ok(())
}

/// The path flags this run was given, so the printed commands act on the same repo and
/// harnesses from any directory.
fn path_overrides(cli: &Cli, repo: &Path) -> String {
    let mut flags = String::new();
    let mut push = |flag: &str, path: &Path| flags.push_str(&format!(" {flag} {}", shell_quote(path)));
    if cli.repo.is_some() {
        push("--repo", repo);
    }
    if let Some(path) = &cli.machine_config {
        push("--machine-config", path);
    }
    if let Some(path) = &cli.assignment {
        push("--assignment", path);
    }
    flags
}

fn shell_quote(path: &Path) -> String {
    let text = path.to_string_lossy();
    if !text.is_empty() && text.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+=:@%,".contains(c)) {
        text.into_owned()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

fn choose_harnesses(
    input: &mut Input,
    machine_config: &Path,
    repo: &Path,
    client: Option<&Client>,
    rt: &tokio::runtime::Runtime,
) -> Result<()> {
    let choices: Vec<HarnessChoice> = match client {
        Some(client) => rt.block_on(client.setup_harnesses())?,
        // The opt-in presets found here read as on: setup enables them as it ends.
        None => MachineConfig::setup_choices(machine_config, repo)?,
    };
    let (shown, missing): (Vec<_>, Vec<_>) = choices.into_iter().partition(|c| c.installed || c.enabled);
    if shown.is_empty() {
        println!("No harnesses found. Install an agent tool or add one to {}.", machine_config.display());
    } else {
        println!("Choose the harnesses to sync into (saved in {}):", machine_config.display());
    }
    for choice in shown {
        if let Some(warning) = &choice.setup_warning {
            println!("  {}: {warning}", choice.name);
            if !choice.enabled {
                continue;
            }
        }
        let paths = [choice.harness.skills.as_deref(), choice.harness.instructions.as_deref()];
        let paths = paths.into_iter().flatten().collect::<Vec<_>>().join(", ");
        let enabled = input.yes(&format!("  Sync {} ({paths})?", choice.name), choice.enabled)?;
        if enabled != choice.enabled {
            match client {
                Some(client) => drop(rt.block_on(client.set_setup_harness(&choice.name, enabled))?),
                None => MachineConfig::set_enabled(machine_config, &choice.name, enabled)?,
            }
        }
    }
    if !missing.is_empty() {
        let names = missing.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ");
        println!("Not found on this machine: {names}");
    }
    Ok(())
}

/// Returns the machine id, whether the assignment was copied, and whether the daemon joined.
fn join(
    input: &mut Input,
    cfg: &sk1ll_daemon::Config,
    create: bool,
    repo: &Path,
    rt: &tokio::runtime::Runtime,
) -> Result<(String, bool, bool)> {
    if create {
        println!("Deploy the server first. See the deployment section in docs/fleet.md.");
    }
    let server_url = loop {
        let url = input.line("Server URL: ")?;
        if url.starts_with("http://") || url.starts_with("https://") {
            break url;
        }
        println!("the URL must start with http:// or https://");
    };
    if sk1ll_daemon::join::insecure_http(&server_url) {
        println!("warning: plain HTTP sends this machine's token unencrypted");
    }
    let (code, admin_token) = if create {
        match std::env::var("SK1LL_SERVER_ADMIN_TOKEN").ok().filter(|t| !t.trim().is_empty()) {
            Some(token) => {
                println!("using SK1LL_SERVER_ADMIN_TOKEN from the environment");
                (String::new(), Some(token))
            }
            None => (String::new(), Some(input.secret("Admin token (used once, not saved): ")?)),
        }
    } else {
        (input.secret("Join code: ")?, None)
    };
    let default_id = MachineConfig::load(&cfg.machine_config)?.id;
    let machine_id = Some(input.line(&format!("Machine id [{default_id}]: "))?).filter(|id| !id.is_empty());
    let copy_from = if create {
        None
    } else {
        Some(input.line("Copy the assignment from machine (empty for none): ")?).filter(|m| !m.is_empty())
    };
    let copied = copy_from.is_some();
    let opts = JoinOptions { server_url, code, admin_token, machine_id, copy_from, repo: repo.to_path_buf() };
    let (joined, via_daemon) = rt.block_on(sk1ll_daemon::join_or_rejoin(cfg, opts))?;
    println!("joined the fleet as {} with the repo at {}", joined.machine_id, joined.repo.display());
    Ok((joined.machine_id, copied, via_daemon))
}

fn turn_everything_on(
    cfg: &sk1ll_daemon::Config,
    client: &Client,
    machine_id: &str,
    via_daemon: bool,
    rt: &tokio::runtime::Runtime,
) -> Result<()> {
    let state = DaemonState::load(&cfg.state)?.context("the join didn't store this machine's credentials")?;
    let repo = Repo::open(&state.repo)?;
    let harnesses: Vec<String> = MachineConfig::load(&cfg.machine_config)?.harnesses().into_keys().collect();
    let skills: Vec<String> = repo.skills()?.into_iter().map(|s| s.name).collect();
    let rules: Vec<String> = repo.rules()?.into_iter().map(|r| r.name).collect();
    let changes = AssignmentChanges { changes: everything_on(&harnesses, &skills, &rules), ..Default::default() };
    if !via_daemon {
        let lock = crate::lock_direct(&state.repo)?.context("the joined repo is not a git repo")?;
        rt.block_on(sk1ll_daemon::set_assignment_direct(cfg, &lock, &changes))?;
        return Ok(());
    }
    // The daemon restarts itself with the new credentials first.
    rt.block_on(async {
        for _ in 0..60 {
            if client
                .status()
                .await
                .is_ok_and(|s| s.mode == DaemonMode::Joined && s.connection == Connection::Connected)
            {
                return Ok(client.set_assignment(machine_id, &changes).await.map(drop)?);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        bail!("the daemon didn't reconnect to the server in time; turn items on with `sk1ll assign` later")
    })
}

/// Defaults first: changing a default records each existing item's value as
/// an explicit toggle, which the item toggles then set to on.
fn everything_on(harnesses: &[String], skills: &[String], rules: &[String]) -> Vec<ToggleChange> {
    let toggle = |harness: &String, kind, name: Option<&String>| ToggleChange {
        harness: harness.clone(),
        kind,
        name: name.cloned(),
        enabled: Some(true),
    };
    harnesses
        .iter()
        .flat_map(|h| {
            [toggle(h, ToggleKind::NewSkillsDefault, None), toggle(h, ToggleKind::NewRulesDefault, None)]
                .into_iter()
                .chain(skills.iter().map(move |s| toggle(h, ToggleKind::Skill, Some(s))))
                .chain(rules.iter().map(move |r| toggle(h, ToggleKind::Rule, Some(r))))
        })
        .collect()
}

fn start_daemon(input: &mut Input, cli: &Cli) -> Result<bool> {
    let custom = cli.daemon_socket.is_some() || std::env::var_os("SK1LL_DAEMON_SOCKET").is_some_and(|v| !v.is_empty());
    if custom {
        println!("This uses a custom daemon socket; start that daemon with `sk1ll daemon`.");
        return Ok(false);
    }
    let manager = Manager::running();
    let installed = manager.and_then(Manager::path).is_some_and(|p| p.is_file());
    let question = match (manager, installed) {
        (None, _) => "Start the daemon in the background now (until you log out)?",
        (Some(Manager::Systemd), true) => "Enable and start the sk1ll-daemon user service?",
        (Some(Manager::Systemd), false) => {
            "Install and start the sk1ll-daemon user service (it starts again when you log in)?"
        }
        (Some(Manager::Launchd), true) => "Load and start the app.v1p3r.sk1ll.daemon launch agent?",
        (Some(Manager::Launchd), false) => {
            "Install and start the app.v1p3r.sk1ll.daemon launch agent (it starts again when you log in)?"
        }
    };
    if !input.yes(question, true)? {
        println!("The daemon keeps your harnesses in step; start it with `sk1ll daemon`.");
        return Ok(false);
    }
    let binary: PathBuf = std::env::current_exe().context("locating this sk1ll executable")?;
    match service::start(&binary)? {
        Started::Systemd => println!("started the sk1ll-daemon user service (logs: journalctl --user -u sk1ll-daemon)"),
        Started::Launchd => {
            let log = service::agent_log_path().map(|p| p.display().to_string()).unwrap_or_default();
            println!("started the {} launch agent (log: {log})", service::LABEL);
        }
        Started::Process => {
            let log = service::log_path().map(|p| p.display().to_string()).unwrap_or_default();
            println!("started the daemon in the background (log: {log})");
        }
    }
    Ok(true)
}

/// Answers come from standard input, so setup can also be scripted.
struct Input {
    stdin: std::io::StdinLock<'static>,
}

impl Input {
    fn stdin() -> Self {
        Self { stdin: std::io::stdin().lock() }
    }

    fn line(&mut self, prompt: &str) -> Result<String> {
        print!("{prompt}");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        if self.stdin.read_line(&mut answer)? == 0 {
            bail!("setup needs answers on standard input");
        }
        Ok(answer.trim().to_string())
    }

    fn yes(&mut self, question: &str, default: bool) -> Result<bool> {
        let hint = if default { "[Y/n]" } else { "[y/N]" };
        loop {
            match self.line(&format!("{question} {hint} "))?.to_ascii_lowercase().as_str() {
                "" => return Ok(default),
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => println!("answer y or n"),
            }
        }
    }

    /// Without echo when standard input is a terminal.
    fn secret(&mut self, prompt: &str) -> Result<String> {
        let terminal = std::io::stdin().is_terminal();
        let stty = |arg: &str| std::process::Command::new("stty").arg(arg).status().is_ok_and(|s| s.success());
        let hidden = terminal && stty("-echo");
        let answer = self.line(prompt);
        if hidden {
            stty("echo");
            println!();
        }
        let answer = answer?;
        if answer.is_empty() {
            bail!("{} is required", prompt.trim_end_matches([':', ' ']).to_lowercase());
        }
        Ok(answer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_on_sets_defaults_before_items() {
        let changes = everything_on(&["claude".into()], &["pdf".into()], &["git".into()]);
        let kinds: Vec<_> = changes.iter().map(|c| (c.kind, c.name.as_deref())).collect();
        assert_eq!(
            kinds,
            [
                (ToggleKind::NewSkillsDefault, None),
                (ToggleKind::NewRulesDefault, None),
                (ToggleKind::Skill, Some("pdf")),
                (ToggleKind::Rule, Some("git")),
            ]
        );
        assert!(changes.iter().all(|c| c.harness == "claude" && c.enabled == Some(true)));
    }

    #[test]
    fn shell_quote_leaves_plain_paths_and_quotes_the_rest() {
        assert_eq!(shell_quote(Path::new("/home/me/skills")), "/home/me/skills");
        assert_eq!(shell_quote(Path::new("/home/me/my skills")), "'/home/me/my skills'");
        assert_eq!(shell_quote(Path::new("/tmp/it's")), "'/tmp/it'\\''s'");
    }
}
