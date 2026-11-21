//! Everything that needs the daemon (Unix only): `join`, `daemon`, `fleet`,
//! and routing mutating commands through a running daemon.

use std::io::{IsTerminal, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use sk1ll_daemon::client::{
    AddImportRequest, ChangeKind, Client, CommitRequest, ConflictTarget, CreateItemRequest, DaemonMode, PullResult,
    RestoreRequest, StatusView, SyncRequest, UpdateImportsRequest,
};
use sk1ll_daemon::service::Manager;
use sk1ll_daemon::{Config, DaemonState};
use sk1ll_proto::{AssignmentChanges, LogQuery, Scope, ToggleChange, ToggleKind};

use crate::{
    Cli, Cmd, FleetCmd, InviteScope, Kind, assign_target, bundled_fallback, check_exists, confirm, every_item,
    fetch_hint, names, print_bundled, print_import_changes, print_update, short,
};

/// Sets up this process's environment, before any thread starts: the
/// daemon's (`sk1ll daemon`), or for direct mode the machine's git
/// credentials (which live only in the daemon's state file), so a direct
/// push or pull to the server authenticates.
pub(crate) fn prepare(cli: &Cli) -> Result<()> {
    if matches!(cli.cmd, Cmd::Daemon) {
        let cfg = daemon_config(cli)?;
        // SAFETY: called from `main` before any runtime or thread starts.
        return unsafe { sk1ll_daemon::prepare_process(&cfg) };
    }
    if matches!(cli.cmd, Cmd::Join { .. } | Cmd::Setup) {
        // SAFETY: as above. Git must fail instead of prompting.
        unsafe { std::env::set_var("GIT_TERMINAL_PROMPT", "0") };
    }
    if let Some(state) = joined_state(cli) {
        for (k, v) in sk1ll_daemon::auth::git_credentials_env(&state) {
            // SAFETY: as above.
            unsafe { std::env::set_var(k, v) };
        }
    }
    Ok(())
}

fn joined_state(cli: &Cli) -> Option<DaemonState> {
    let cfg = daemon_config(cli).ok()?;
    DaemonState::load(&cfg.state).ok().flatten()
}

/// Whether this machine joined a fleet.
pub(crate) fn is_joined(cli: &Cli) -> bool {
    joined_state(cli).is_some()
}

pub(crate) fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread().enable_all().build()?)
}

/// A joined machine always changes assignments at the Server, even without a daemon.
pub(crate) fn assign_direct(cli: &Cli, repo: &Path, lock: Option<&sk1ll_core::EngineLock>) -> Result<bool> {
    let cfg = daemon_config(cli)?;
    let Some(state) = DaemonState::load(&cfg.state)?.filter(|s| !s.revoked) else { return Ok(false) };
    let Cmd::Assign { harness, kind, name, state: enabled, default_skills, default_rules, all } = &cli.cmd else {
        return Ok(false);
    };
    if !sk1ll_core::MachineConfig::load(&cfg.machine_config)?.harnesses().contains_key(harness) {
        bail!("unknown harness `{harness}` on this machine (see `sk1ll harnesses`)");
    }
    let mut changes = Vec::new();
    let toggle = |kind, name, on: bool| ToggleChange { harness: harness.clone(), kind, name, enabled: Some(on) };
    let target = assign_target(*kind, name.clone(), *enabled, *default_skills, *default_rules, *all)?;
    if let Some(s) = all {
        let lock = lock.context("a fleet assignment change requires the engine lock")?;
        if let Some(result) =
            runtime()?.block_on(sk1ll_daemon::set_harness_assignment_direct(&cfg, lock, harness, s.on()))?
        {
            println!(
                "fleet assignment of {} is now revision {}; run `sk1ll sync` to apply",
                state.machine_id, result.update.revision
            );
            for item in result.skipped {
                eprintln!("Skipped item with an ineligible name: {item}");
            }
            return Ok(true);
        }
        eprintln!("warning: {}", sk1ll_daemon::client::LEGACY_HARNESS_ASSIGNMENT_WARNING);
        changes.extend(every_toggle(&sk1ll_core::Repo::open(repo)?, harness, s.on())?);
    }
    if let Some((kind, name, enabled)) = target {
        check_exists(&sk1ll_core::Repo::open(repo)?, kind, &name)?;
        let kind = match kind {
            Kind::Skill => ToggleKind::Skill,
            Kind::Rule => ToggleKind::Rule,
        };
        changes.push(toggle(kind, Some(name), enabled.on()));
    }
    if let Some(s) = default_skills {
        changes.push(toggle(ToggleKind::NewSkillsDefault, None, s.on()));
    }
    if let Some(s) = default_rules {
        changes.push(toggle(ToggleKind::NewRulesDefault, None, s.on()));
    }
    let lock = lock.context("a fleet assignment change requires the engine lock")?;
    let update = runtime()?.block_on(sk1ll_daemon::set_assignment_direct(
        &cfg,
        lock,
        &AssignmentChanges { changes, ..Default::default() },
    ))?;
    println!("fleet assignment of {} is now revision {}; run `sk1ll sync` to apply", state.machine_id, update.revision);
    Ok(true)
}

/// `assign <harness> --all on|off`: every skill and rule, then both defaults.
fn every_toggle(repo: &sk1ll_core::Repo, harness: &str, on: bool) -> Result<Vec<ToggleChange>> {
    let (skills, rules) = every_item(repo)?;
    let toggle = |kind, name| ToggleChange { harness: harness.to_string(), kind, name, enabled: Some(on) };
    Ok(skills
        .into_iter()
        .map(|s| toggle(ToggleKind::Skill, Some(s)))
        .chain(rules.into_iter().map(|r| toggle(ToggleKind::Rule, Some(r))))
        .chain([toggle(ToggleKind::NewSkillsDefault, None), toggle(ToggleKind::NewRulesDefault, None)])
        .collect())
}

/// The daemon's config, with the CLI's `--machine-config` / `--assignment`.
pub(crate) fn daemon_config(cli: &Cli) -> Result<Config> {
    let mut cfg = Config::from_env()?;
    cfg.repo = cli.repo.clone();
    if let Some(p) = &cli.daemon_state {
        cfg.state = p.clone();
    }
    if let Some(p) = &cli.machine_config {
        cfg.machine_config = p.clone();
    }
    if let Some(p) = &cli.assignment {
        cfg.assignment = p.clone();
    }
    if let Some(p) = &cli.daemon_socket {
        cfg.socket = p.clone();
    }
    Ok(cfg)
}

pub(crate) fn assignment(state: Option<&Path>, cache: &Path) -> Result<sk1ll_core::Assignment> {
    let mut cfg = Config::from_env()?;
    if let Some(p) = state {
        cfg.state = p.to_path_buf();
    }
    cfg.assignment = cache.to_path_buf();
    sk1ll_daemon::state::load_assignment(&cfg)
}

/// [`assignment`], with a joined machine still waiting for its own as `None`.
pub(crate) fn assignment_if_present(state: Option<&Path>, cache: &Path) -> Result<Option<sk1ll_core::Assignment>> {
    let mut cfg = Config::from_env()?;
    if let Some(p) = state {
        cfg.state = p.to_path_buf();
    }
    cfg.assignment = cache.to_path_buf();
    sk1ll_daemon::state::load_assignment_if_present(&cfg)
}

pub(crate) fn queue_discarded(
    state: Option<&Path>,
    lock: Option<&sk1ll_core::EngineLock>,
    items: Vec<sk1ll_core::ItemRef>,
) -> Result<()> {
    let mut cfg = Config::from_env()?;
    if let Some(p) = state {
        cfg.state = p.to_path_buf();
    }
    if DaemonState::load(&cfg.state)?.is_some_and(|s| !s.revoked) {
        sk1ll_daemon::state::queue_discards(&cfg, lock.context("a fleet restore requires the engine lock")?, items)?;
    }
    Ok(())
}

pub(crate) fn require_standalone_adoption(state: Option<&Path>, repo: &sk1ll_core::Repo) -> Result<()> {
    let mut cfg = Config::from_env()?;
    if let Some(p) = state {
        cfg.state = p.to_path_buf();
    }
    if let Some(state) = DaemonState::load(&cfg.state)?.filter(|s| !s.revoked) {
        let joined = state.repo.canonicalize()?;
        anyhow::ensure!(
            repo.root() == joined,
            "selected repo {} differs from the joined repo {}; select the joined repo to change fleet content",
            repo.root().display(),
            joined.display()
        );
        bail!(
            "Adopt needs a connection to the Server; start the daemon with `sk1ll daemon` and wait for it to connect"
        );
    }
    Ok(())
}

pub(crate) fn client(cli: &Cli) -> Result<Client> {
    Ok(match &cli.daemon_socket {
        Some(p) => Client::new(p),
        None => Client::default_socket()?,
    })
}

/// `join`, `daemon` and `fleet`.
pub(crate) fn command(cli: Cli) -> Result<()> {
    match &cli.cmd {
        Cmd::Fleet { command } => fleet(&cli, command.as_ref()),
        Cmd::Daemon => {
            let cfg = daemon_config(&cli)?;
            sk1ll_daemon::init_logging();
            tokio::runtime::Runtime::new()?.block_on(sk1ll_daemon::run(cfg))
        }
        Cmd::Join { .. } => join(cli),
        Cmd::Setup => crate::setup::run(&cli),
        _ => unreachable!(),
    }
}

fn join(cli: Cli) -> Result<()> {
    let cfg = daemon_config(&cli)?;
    let Cmd::Join { server_url, code, admin_token, id, copy_from } = cli.cmd else { unreachable!() };
    let repo = sk1ll_daemon::locate_repo(cli.repo.as_deref(), false)?;
    if sk1ll_daemon::join::insecure_http(&server_url) {
        eprintln!("warning: joining over plain HTTP; the machine token travels in clear text");
    }
    let opts = sk1ll_daemon::JoinOptions {
        server_url,
        code: code.unwrap_or_default(),
        admin_token,
        machine_id: id,
        copy_from,
        repo,
    };
    let (joined, via_daemon) = runtime()?.block_on(sk1ll_daemon::join_or_rejoin(&cfg, opts))?;
    let how = match joined.setup {
        sk1ll_daemon::gitx::Setup::Cloned => "cloned the fleet repo into",
        sk1ll_daemon::gitx::Setup::Initialized => {
            "the server is empty; started a new skills repo (commit to share it) in"
        }
        sk1ll_daemon::gitx::Setup::Adopted => "using the existing clone at",
    };
    println!("joined the fleet as {}", joined.machine_id);
    println!("{how} {}", joined.repo.display());
    if joined.harnesses.is_empty() {
        println!("no harnesses detected (see `sk1ll harnesses`)");
    } else {
        println!("harnesses: {}", joined.harnesses.join(", "));
    }
    if via_daemon {
        println!("the running daemon restarts with the new credentials and manages this machine again");
    } else {
        let example = match Manager::platform() {
            Manager::Systemd => "the systemd user unit in deploy/sk1ll-daemon.service",
            Manager::Launchd => "the launchd agent in deploy/app.v1p3r.sk1ll.daemon.plist",
        };
        println!("now run `sk1ll daemon` (e.g. as {example})");
    }
    Ok(())
}

fn fleet(cli: &Cli, command: Option<&FleetCmd>) -> Result<()> {
    let client = client(cli)?;
    let rt = runtime()?;
    match command {
        Some(FleetCmd::Invite { scope }) => {
            let status = rt.block_on(client.status()).map_err(fleet_error)?;
            let url = status.server_url.context("this machine hasn't joined a fleet")?;
            let scope = match scope {
                InviteScope::Fleet => Scope::Fleet,
                InviteScope::SelfOnly => Scope::SelfOnly,
            };
            let code = rt.block_on(client.join_code(scope)).map_err(fleet_error)?;
            println!("{}", code.code);
            println!("sk1ll join {} {}", shell_arg(&url), shell_arg(&code.code));
            match code.expires_at {
                Some(t) => println!("expires {}", utc(t)),
                None => println!("expires after the Server's SK1LL_SERVER_JOIN_CODE_TTL (default 24h)"),
            }
            match scope {
                Scope::Fleet => println!("scope fleet: the Machine may manage the whole Fleet"),
                Scope::SelfOnly => println!("scope self: the Machine may manage only itself"),
            }
        }
        Some(FleetCmd::Remove { machine, yes }) => {
            if !yes {
                let stdin = std::io::stdin();
                if !stdin.is_terminal() {
                    bail!("removing a machine requires terminal confirmation or `--yes`");
                }
                eprint!("Type the machine id `{machine}` to confirm removal: ");
                std::io::stderr().flush()?;
                let mut answer = String::new();
                stdin.read_line(&mut answer)?;
                if answer.trim_end_matches(['\r', '\n']) != machine {
                    bail!("machine id did not match; removal aborted");
                }
            }
            rt.block_on(client.remove_machine(machine)).map_err(fleet_error)?;
            println!("removed {machine} from the fleet");
        }
        Some(FleetCmd::CopyAssignment { source, target, merge, maps, yes }) => {
            use sk1ll_proto::{CopyMode, HarnessMapping, MachineAssignmentCopy};
            let mappings = maps
                .iter()
                .map(|map| {
                    let (from, to) = map
                        .split_once('=')
                        .filter(|(from, to)| !from.is_empty() && !to.is_empty() && !to.contains('='))
                        .with_context(|| format!("invalid Harness mapping `{map}`; use FROM=TO"))?;
                    Ok(HarnessMapping { from: from.into(), to: to.into() })
                })
                .collect::<Result<Vec<_>>>()?;
            let mut copy = MachineAssignmentCopy {
                source: source.clone(),
                harnesses: (!mappings.is_empty()).then_some(mappings),
                mode: if *merge { CopyMode::Merge } else { CopyMode::Replace },
                expected: None,
                source_expected: None,
            };
            let preview = rt.block_on(client.preview_assignment_copy(target, &copy)).map_err(fleet_error)?;
            println!("{} Assignment changes on {target} (item toggles and defaults)", preview.count);
            if preview.count == 0 {
                return Ok(());
            }
            if !yes {
                if !std::io::stdin().is_terminal() {
                    bail!("copying an Assignment requires terminal confirmation or `--yes`");
                }
                if !confirm("Apply these changes?")? {
                    bail!("Assignment copy aborted");
                }
            }
            copy.expected = Some(preview.target);
            copy.source_expected = Some(preview.source);
            let update = rt.block_on(client.copy_machine_assignment(target, &copy)).map_err(fleet_error)?;
            println!("copied Assignment to {target} at revision {}", update.revision);
        }
        Some(FleetCmd::Log { machine, harness, author, item, before, limit }) => {
            let query = LogQuery {
                machine: machine.clone(),
                harness: harness.clone(),
                author: author.clone(),
                item: item.clone(),
                before: *before,
                limit: *limit,
            };
            let entries = rt.block_on(client.log(&query)).map_err(fleet_error)?;
            for entry in entries {
                let kind = match entry.kind {
                    ToggleKind::Skill => "skill",
                    ToggleKind::Rule => "rule",
                    ToggleKind::NewSkillsDefault => "new_skills_default",
                    ToggleKind::NewRulesDefault => "new_rules_default",
                };
                let item = entry.name.map_or_else(|| kind.to_string(), |name| format!("{kind}:{name}"));
                let state = |value| match value {
                    Some(true) => "on",
                    Some(false) => "off",
                    None => "unset",
                };
                println!(
                    "{}  {}  {} / {}  {}  {} -> {}  by {} ({:?})",
                    entry.id,
                    entry.at,
                    entry.machine,
                    entry.harness,
                    item,
                    state(entry.before),
                    state(entry.after),
                    entry.author,
                    entry.cause
                );
            }
        }
        Some(FleetCmd::Undo { entry_id }) => {
            let update = rt.block_on(client.undo(*entry_id)).map_err(fleet_error)?;
            if update.entries.is_empty() {
                println!("Assignment log entry {entry_id}'s previous value is already set; nothing changed");
            } else {
                println!("undid Assignment log entry {entry_id} at revision {}", update.revision);
            }
        }
        None => print_fleet(rt.block_on(client.fleet()).map_err(fleet_error)?),
    }
    Ok(())
}

fn fleet_error(e: sk1ll_daemon::client::ClientError) -> anyhow::Error {
    if e.is_not_running() {
        anyhow::anyhow!("the sk1ll daemon isn't running (start it with `sk1ll daemon`)")
    } else {
        e.into()
    }
}

fn shell_arg(arg: &str) -> String {
    if !arg.is_empty() && arg.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._~:/%+=@".contains(&b)) {
        arg.into()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

fn print_fleet(fleet: sk1ll_proto::FleetView) {
    println!("baseline {}", fleet.baseline.as_deref().map_or("(none yet)", short));
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    for m in fleet.machines {
        use sk1ll_proto::MachineStatus as S;
        let status = match m.status {
            S::Offline => "offline",
            S::Blocked => "blocked",
            S::Conflict => "conflict",
            S::Behind => "behind",
            S::Drafting => "drafting",
            S::InSync => "in sync",
        };
        let seen = match m.last_seen {
            Some(t) => ago(now - t),
            None => "never".into(),
        };
        let mut notes = Vec::new();
        if let Some(r) = &m.report {
            if let Some(items) = &r.pull_pending {
                notes.push(if items.is_empty() {
                    "pull pending".into()
                } else {
                    format!("pull pending, blocked by {}", names(items))
                });
            } else if r.blocked {
                notes.push(format!("clashes: {}", names(&r.clashes)));
            }
            if !r.conflicts.is_empty() {
                notes.push(format!("{} conflict(s)", r.conflicts.len()));
            }
            if !r.drafts.is_empty() {
                notes.push(format!("{} draft item(s)", r.drafts.len()));
            }
            if !r.unpushed.is_empty() {
                notes.push(format!("{} unpushed", r.unpushed.len()));
            }
        }
        if m.scope == Scope::SelfOnly {
            notes.push("manages only itself".into());
        }
        println!("{:<20} {:<9} seen {:<10} {}", m.id, status, seen, notes.join(", "));
    }
}

/// Unix seconds as `YYYY-MM-DD HH:MM:SS UTC`.
fn utc(secs: i64) -> String {
    let (days, rest) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC", rest / 3600, rest % 3600 / 60, rest % 60)
}

fn ago(secs: i64) -> String {
    match secs.max(0) {
        s if s < 60 => format!("{s}s ago"),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86400),
    }
}

/// The running daemon, when it manages the repo a command is about.
pub(crate) struct Daemon {
    rt: tokio::runtime::Runtime,
    client: Client,
    status: StatusView,
}

/// The daemon managing `repo`, if one runs. `None` means work directly
/// (after taking the engine lock): certainly no daemon runs (no socket, or
/// connection refused), or it manages another repo. Any other failure to
/// talk to it is an error, since a daemon may well be running.
pub(crate) fn connect(cli: &Cli, repo: &Path) -> Result<Option<Daemon>> {
    let client = client(cli)?;
    let rt = runtime()?;
    let status = match rt.block_on(client.status()) {
        Ok(s) => s,
        Err(e) if e.is_not_running() => {
            if is_joined(cli) {
                eprintln!("note: the sk1ll daemon isn't running; working on the repo directly");
            }
            return Ok(None);
        }
        Err(e) => bail!(
            "cannot talk to the sk1ll daemon ({e}); not working on the repo directly while it may be running \
             (check `systemctl --user status sk1ll-daemon`)"
        ),
    };
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    if canon(&status.repo) != canon(repo) {
        eprintln!("note: the sk1ll daemon manages {}, not this repo; working on it directly", status.repo.display());
        return Ok(None);
    }
    Ok(Some(Daemon { rt, client, status }))
}

impl Daemon {
    pub(crate) fn into_parts(self) -> (tokio::runtime::Runtime, Client) {
        (self.rt, self.client)
    }

    fn block<T>(&self, f: impl Future<Output = sk1ll_daemon::client::Result<T>>) -> Result<T> {
        Ok(self.rt.block_on(f)?)
    }

    /// Runs a mutating command through the daemon. `repo` is only read
    /// (to check names).
    pub(crate) fn run(self, cmd: Cmd, repo: &Path) -> Result<()> {
        let c = &self.client;
        match cmd {
            Cmd::Conflicts | Cmd::Recover { command: crate::RecoverCmd::List } => {
                let entries =
                    if matches!(cmd, Cmd::Conflicts) { self.block(c.conflicts())? } else { self.block(c.recovered())? };
                for e in entries {
                    if e.kind.as_deref() == Some("displaced") {
                        println!(
                            "{} {} {} {}",
                            e.id.unwrap_or_default(),
                            e.harness,
                            e.name,
                            e.path.unwrap_or_default().display()
                        );
                    } else if matches!(cmd, Cmd::Conflicts) {
                        println!("! {} {} {}", e.harness, e.name, e.dest.display());
                    }
                }
            }
            Cmd::Recover { command: crate::RecoverCmd::Adopt { id } } => {
                println!("adopted into {}", self.block(c.adopt_displaced(&id))?.display());
            }
            Cmd::Recover { command: crate::RecoverCmd::Discard { id } } => self.block(c.discard_displaced(&id))?,
            Cmd::Commit { items, message, author, push } => {
                let resp = self.block(c.commit(&CommitRequest { items, message, author }))?;
                println!("committed {} as {}", names(&resp.items), short(&resp.sha));
                if self.status.mode == DaemonMode::Standalone {
                    if push {
                        self.block(c.push())?;
                        println!("pushed to origin");
                    }
                    return Ok(());
                }
                match (resp.pushed, resp.push_error) {
                    (true, _) => println!("pushed to the server"),
                    (false, Some(e)) => println!("not pushed yet ({e}); the daemon pushes it when it can"),
                    (false, None) => println!("not pushed yet; the daemon pushes it when it can"),
                }
            }
            Cmd::RepairIds => {
                println!("reassigned {} duplicate Item ID(s) in the Draft", self.block(c.repair_ids())?);
            }
            Cmd::Push => match self.block(c.push())?.pushed {
                true => println!(
                    "pushed to {}",
                    if self.status.mode == DaemonMode::Standalone { "origin" } else { "the server" }
                ),
                false => println!("nothing to push"),
            },
            Cmd::Restore { items, all, yes } => {
                if all {
                    let draft = self.block(c.draft())?;
                    if draft.is_empty() {
                        println!("no draft to restore");
                        return Ok(());
                    }
                    for d in &draft {
                        let sym = match d.change {
                            ChangeKind::Added => "A",
                            ChangeKind::Modified => "M",
                            ChangeKind::Deleted => "D",
                        };
                        println!("{sym} {}", d.item);
                    }
                    if !yes && !confirm(&format!("discard all {} draft item(s)?", draft.len()))? {
                        bail!("aborted");
                    }
                }
                let resp = self.block(c.restore(&RestoreRequest { items, all }))?;
                if resp.discarded.is_empty() {
                    println!("nothing was restored");
                } else {
                    println!("restored {}", names(&resp.discarded));
                }
            }
            Cmd::Pull => match self.block(c.pull())? {
                PullResult::UpToDate => println!("already on the latest baseline"),
                PullResult::Updated { from, to } => {
                    println!("baseline {} -> {}", from.as_deref().map_or("(none)", short), short(&to));
                }
                PullResult::Blocked { baseline, items, pending: true } => bail!(
                    "pull pending at {}. Move aside any edits you want to keep, then restore or clear {} and pull again",
                    short(&baseline),
                    names(&items)
                ),
                PullResult::Blocked { baseline, items, pending: false } => bail!(
                    "blocked: baseline {} clashes with {}; commit or restore them (the daemon pulls again then)",
                    short(&baseline),
                    names(&items)
                ),
            },
            Cmd::Sync { dry_run: _, force } => {
                let result = self.block(c.sync(&SyncRequest { force, ..Default::default() }))?;
                if result.conflicts.is_empty() {
                    println!("harnesses in sync");
                }
                for x in result.conflicts {
                    let reason = x.reason.map(|r| format!("  ({r})")).unwrap_or_default();
                    println!("! {:<10} {}{reason}", x.harness, x.name);
                }
            }
            Cmd::Adopt { harness, name } => {
                self.block(c.adopt(&ConflictTarget { harness: harness.clone(), name: name.clone() }))?;
                println!("adopted {name} from {harness} into the draft; review with `sk1ll diff {name}`");
            }
            Cmd::Add { source, path, git_ref, name, list: false } => {
                // An empty name is the skill's default name, decided by the daemon.
                let narrowed = path.is_some() || git_ref.is_some();
                let req =
                    AddImportRequest { name: name.clone().unwrap_or_default(), source, path, git_ref, commit: None };
                match self.block(c.add_import(&req)).map_err(fetch_hint) {
                    Ok(r) => println!("added {} @ {}", r.name, short(&r.new_commit)),
                    Err(e) => {
                        let (bundled, e) = bundled_fallback(&req.source, narrowed, e)?;
                        let name = name.unwrap_or_else(|| bundled.to_string());
                        let create = CreateItemRequest {
                            kind: "skill".into(),
                            name: name.clone(),
                            description: None,
                            bundled: Some(bundled.into()),
                        };
                        self.block(c.create_item(&create))?;
                        print_bundled(&e, &name);
                    }
                }
            }
            Cmd::Update { name: Some(name), dry_run: true, .. } => {
                print_import_changes(&name, self.block(c.import_changes(&name, None))?);
            }
            Cmd::Update { name, force, .. } => {
                for r in self.block(c.update_imports(&UpdateImportsRequest { name, force, expected: None }))? {
                    print_update(&r.name, r.old_commit.as_deref(), &r.new_commit, r.changed);
                }
                println!("review with `sk1ll diff`, then commit");
            }
            Cmd::Remove { name } => {
                self.block(c.remove_import(&name))?;
                println!("removed {name}");
            }
            Cmd::Fork { name } => {
                self.block(c.fork(&name))?;
                println!("moved {name} to personal/");
            }
            Cmd::Assign { harness, kind, name, state, default_skills, default_rules, all } => {
                // Assignment is server state: change it there, for this machine.
                if !self.block(c.harnesses())?.iter().any(|h| h.name == harness) {
                    bail!("unknown harness `{harness}` on this machine (see `sk1ll harnesses`)");
                }
                let mut changes = Vec::new();
                let toggle =
                    |kind, name, on: bool| ToggleChange { harness: harness.clone(), kind, name, enabled: Some(on) };
                let target = assign_target(kind, name, state, default_skills, default_rules, all)?;
                if let Some((kind, name, state)) = target {
                    check_exists(&sk1ll_core::Repo::open(repo)?, kind, &name)?;
                    let kind = match kind {
                        Kind::Skill => ToggleKind::Skill,
                        Kind::Rule => ToggleKind::Rule,
                    };
                    changes.push(toggle(kind, Some(name), state.on()));
                }
                if let Some(s) = default_skills {
                    changes.push(toggle(ToggleKind::NewSkillsDefault, None, s.on()));
                }
                if let Some(s) = default_rules {
                    changes.push(toggle(ToggleKind::NewRulesDefault, None, s.on()));
                }
                let machine = self.status.machine_id.clone();
                let update = if let Some(s) = all {
                    let result = self.block(c.set_harness_assignment(&harness, s.on()))?;
                    if let Some(warning) = result.warning {
                        eprintln!("warning: {warning}");
                    }
                    result.update
                } else {
                    self.block(c.set_assignment(&machine, &AssignmentChanges { changes, ..Default::default() }))?
                };
                if all.is_some() && !update.errors.is_empty() {
                    println!("Assignment of {machine} was saved at revision {}.", update.revision);
                    for error in &update.errors {
                        eprintln!("{error}");
                    }
                    bail!("Could not apply all placements");
                }
                if self.status.mode == DaemonMode::Standalone {
                    println!(
                        "local assignment of {machine} is now revision {}; the daemon applies it",
                        update.revision
                    );
                    return Ok(());
                }
                if update.entries.is_empty() {
                    println!("{harness}: already so on {machine}");
                }
                for e in &update.entries {
                    let what = match (&e.kind, &e.name) {
                        (ToggleKind::NewSkillsDefault, _) => "skills default".to_string(),
                        (ToggleKind::NewRulesDefault, _) => "rules default".to_string(),
                        (k, Some(n)) => format!("{} {n}", if *k == ToggleKind::Skill { "skill" } else { "rule" }),
                        (_, None) => "?".to_string(),
                    };
                    let after = match e.after {
                        Some(true) => "on",
                        Some(false) => "off",
                        None => "default",
                    };
                    println!("{}: {what} {after}", e.harness);
                }
                println!("fleet assignment of {machine} is now revision {}; the daemon applies it", update.revision);
            }
            _ => unreachable!("not a mutating command"),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::utc;

    #[test]
    fn utc_dates() {
        assert_eq!(utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc(1_700_086_400), "2023-11-15 22:13:20 UTC");
        assert_eq!(utc(951_782_400), "2000-02-29 00:00:00 UTC");
        assert_eq!(utc(-1), "1969-12-31 23:59:59 UTC");
    }
}
