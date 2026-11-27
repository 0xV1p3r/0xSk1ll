//! `sk1ll setup`: first-run setup on the command line, the same steps as the
//! GUI's setup wizard, through the running daemon or an engine in this process.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sk1ll_core::{HarnessChange, HarnessChoice, ItemRef, MachineConfig};
use sk1ll_daemon::client::{Connection, DaemonMode, HarnessChangeRequest};
use sk1ll_daemon::service::{self, Manager, Started};
use sk1ll_daemon::{DaemonState, JoinOptions};
use sk1ll_proto::{AssignmentChanges, ToggleChange, ToggleKind};

use crate::Cli;
use crate::fleet::{Session, daemon_config, runtime};

pub(crate) fn run(cli: &Cli) -> Result<()> {
    let cfg = daemon_config(cli)?;
    if let Some(state) = DaemonState::load(&cfg.state)? {
        if state.revoked {
            bail!("this machine was removed from its fleet; join again with `sk1ll join <server-url> <code>`");
        }
        println!("this machine already joined {} as `{}`; nothing to set up", state.server_url, state.machine_id);
        return Ok(());
    }
    // The running daemon, else an engine in this process for the located repo.
    let session = Session::any(cli)?;
    let daemon = !session.in_process();
    let repo = session.status.repo.clone();
    // A standalone repo made with `sk1ll init`: how the machine is used is
    // decided, the harnesses and the daemon are still to set up.
    let existing = repo.join(".git").exists();
    sk1ll_core::git::require_git(sk1ll_core::git::MIN_VERSION)?;
    let mut input = Input::stdin();

    let path = if existing {
        println!("Using the skills repo at {} (join a fleet later with `sk1ll join`).", repo.display());
        0
    } else {
        println!("How will this machine use 0xSk1ll?");
        println!("  1) Only this machine: a local skills repo; you can join a fleet later");
        println!("  2) Join a fleet: you have a join code from another machine");
        println!("  3) Join as the first machine: you have the deployed server's admin token");
        loop {
            match input.line("Choose [1]: ")?.as_str() {
                "" | "1" => break 1,
                "2" => break 2,
                "3" => break 3,
                _ => println!("enter 1, 2 or 3"),
            }
        }
    };

    println!();
    choose_harnesses(&mut input, &cfg.machine_config, &session, existing)?;
    println!();

    let joined = if existing {
        None
    } else if path == 1 {
        session.block(session.client.init())?;
        println!("created the skills repo at {}", repo.display());
        None
    } else {
        // The join sets the repo up; this process's engine has no part in it.
        drop(session);
        Some(join(&mut input, &cfg, path == 3, &repo)?)
    };

    if let Some((machine_id, copied, via_daemon)) = &joined
        && !copied
    {
        println!();
        println!("{machine_id} starts with every skill and rule off.");
        if input.yes("Turn on everything, including skills and rules added later?", true)? {
            turn_everything_on(cli, machine_id, *via_daemon)?;
            println!("turned on everything for {machine_id}");
        } else {
            println!("assign items with `sk1ll assign` or in the GUI's Assignment matrix");
        }
    }

    if !daemon {
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

/// `existing`: the repo is there already, so a change is made as `sk1ll
/// harnesses enable|disable` makes it, and syncs.
fn choose_harnesses(input: &mut Input, machine_config: &Path, session: &Session, existing: bool) -> Result<()> {
    // The opt-in presets found here read as on: setup enables them as it ends.
    let choices: Vec<HarnessChoice> = session.block(session.client.setup_harnesses())?;
    // So do the config directories this environment points at, the first time.
    let candidates = crate::harnesses::env_candidates(&choices, &session.status.repo);
    let (shown, missing): (Vec<_>, Vec<_>) = choices.into_iter().partition(|c| c.installed || c.enabled);
    if shown.is_empty() && candidates.is_empty() {
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
        if enabled == choice.enabled {
            continue;
        }
        if !existing {
            session.block(session.client.set_setup_harness(&choice.name, enabled))?;
            continue;
        }
        let name = choice.name.clone();
        let change = if enabled { HarnessChange::Enable { name } } else { HarnessChange::Disable { name } };
        let placed = apply_change(session, change)?;
        if !enabled && placed {
            println!(
                "  what sk1ll placed in {0} stays there, no longer managed; to take it out, enable {0} again, then run \
                 `sk1ll harnesses disable {0} --take-out`",
                choice.name
            );
        }
    }
    for c in candidates {
        let what = crate::harnesses::describe(&c);
        if let Some(warning) = &c.warning {
            println!("  {} ({what}): not offered: {warning}", c.name);
            continue;
        }
        let paths = [c.harness.skills.as_deref(), c.harness.instructions.as_deref()];
        let paths = paths.into_iter().flatten().collect::<Vec<_>>().join(", ");
        if !input.yes(&format!("  Sync {} ({what}: {paths})?", c.name), !existing)? {
            continue;
        }
        let added = match existing {
            true => apply_change(session, c.change()).map(drop),
            false => session.block(session.client.add_setup_instance(&c.name, &c.harness)).map(drop),
        };
        // One the engine refuses (it overlaps one kept before it) is skipped, not the rest.
        match added {
            Err(e) if crate::harnesses::refused(&e) => println!("  {} ({what}): not added: {e:#}", c.name),
            added => added?,
        }
    }
    if !missing.is_empty() {
        let names = missing.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ");
        println!("Not found on this machine: {names}");
    }
    Ok(())
}

/// Makes `change` as `sk1ll harnesses` does, after its preview, and syncs.
/// Whether sk1ll had placed anything where the change stops syncing.
fn apply_change(session: &Session, change: HarnessChange) -> Result<bool> {
    let mut request = HarnessChangeRequest { copy_assignment_from: None, change, fingerprint: None };
    let preview = session.block(session.client.preview_harness(&request))?;
    for warning in &preview.warnings {
        println!("  warning: {warning}");
    }
    request.fingerprint = Some(preview.fingerprint);
    let applied = session.block(session.client.apply_harness(&request))?;
    for error in &applied.errors {
        println!("  error: {error}");
    }
    Ok(!preview.placed.is_empty())
}

/// Returns the machine id, whether the assignment was copied, and whether the daemon joined.
fn join(input: &mut Input, cfg: &sk1ll_daemon::Config, create: bool, repo: &Path) -> Result<(String, bool, bool)> {
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
    let (joined, via_daemon) = runtime()?.block_on(sk1ll_daemon::join_or_rejoin(cfg, opts))?;
    println!("joined the fleet as {} with the repo at {}", joined.machine_id, joined.repo.display());
    Ok((joined.machine_id, copied, via_daemon))
}

/// Through the daemon that joined (once it restarted with the new
/// credentials and reconnected), else an engine in this process.
fn turn_everything_on(cli: &Cli, machine_id: &str, via_daemon: bool) -> Result<()> {
    let session = if via_daemon {
        let rt = runtime()?;
        let client = crate::fleet::client(cli)?;
        let connected = rt.block_on(async {
            for _ in 0..60 {
                if client
                    .status()
                    .await
                    .is_ok_and(|s| s.mode == DaemonMode::Joined && s.connection == Connection::Connected)
                {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            false
        });
        if !connected {
            bail!("the daemon didn't reconnect to the server in time; turn items on with `sk1ll assign` later");
        }
        drop(rt);
        Session::any(cli)?
    } else {
        Session::any(cli)?
    };
    let harnesses: Vec<String> = session.block(session.client.harnesses())?.into_iter().map(|h| h.name).collect();
    let repo = session.block(session.client.repo(true))?;
    let items: Vec<_> = repo
        .skills
        .into_iter()
        .map(|s| (ItemRef::Skill(s.name), s.id))
        .chain(repo.rules.into_iter().map(|r| (ItemRef::Rule(r.name), r.id)))
        .collect();
    let changes = AssignmentChanges { changes: everything_on(&harnesses, &items), expected: None };
    session.block(session.client.set_assignment(machine_id, &changes))?;
    Ok(())
}

/// Defaults first: changing a default records each existing item's value as
/// an explicit toggle, which the item toggles then set to on.
/// Turns everything on in `harnesses`. An item without an Item ID yet has no
/// toggle to set; the defaults cover it.
fn everything_on(
    harnesses: &[String],
    items: &[(sk1ll_core::ItemRef, Option<sk1ll_core::ItemId>)],
) -> Vec<ToggleChange> {
    harnesses
        .iter()
        .flat_map(|harness| {
            let toggle = |kind, item_id, name| ToggleChange {
                harness: harness.clone(),
                kind,
                item_id,
                name,
                enabled: Some(true),
            };
            [toggle(ToggleKind::NewSkillsDefault, None, None), toggle(ToggleKind::NewRulesDefault, None, None)]
                .into_iter()
                .chain(items.iter().filter_map(move |(item, id)| {
                    let id = id.as_ref()?;
                    let kind = match item {
                        sk1ll_core::ItemRef::Skill(_) => ToggleKind::Skill,
                        sk1ll_core::ItemRef::Rule(_) => ToggleKind::Rule,
                        _ => return None,
                    };
                    Some(toggle(kind, Some(id.clone()), Some(item.name().into())))
                }))
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
    };
    if !input.yes(question, true)? {
        println!("The daemon keeps your harnesses in step; start it with `sk1ll daemon`.");
        return Ok(false);
    }
    let binary: PathBuf = std::env::current_exe().context("locating this sk1ll executable")?;
    match service::start(&binary)? {
        Started::Systemd => println!("started the sk1ll-daemon user service (logs: journalctl --user -u sk1ll-daemon)"),
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
