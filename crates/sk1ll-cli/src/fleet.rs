//! Everything that needs the daemon (Unix only): `join`, `daemon`, `fleet`,
//! and routing mutating commands through a running daemon.

use std::path::Path;

use anyhow::{Context, Result, bail};
use sk1ll_daemon::client::{
    AddImportRequest, ChangeKind, Client, ConflictTarget, PromoteRequest, PullResult, ResetRequest, StatusView,
    SyncRequest, UpdateImportsRequest,
};
use sk1ll_daemon::{Config, DaemonState};
use sk1ll_proto::{AssignmentChanges, ToggleChange, ToggleKind};

use crate::{Cli, Cmd, Kind, assign_target, check_exists, confirm, fetch_hint, names, print_update, short};

/// Sets up this process's environment, before any thread starts: the
/// daemon's (`sk1ll daemon`), or for direct mode the machine's git
/// credentials (which live only in the daemon's state file), so a direct
/// push or pull to the hub authenticates.
pub(crate) fn prepare(cli: &Cli) -> Result<()> {
    if matches!(cli.cmd, Cmd::Daemon) {
        let cfg = daemon_config(cli)?;
        // SAFETY: called from `main` before any runtime or thread starts.
        return unsafe { sk1ll_daemon::prepare_process(&cfg) };
    }
    if matches!(cli.cmd, Cmd::Join { .. }) {
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

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread().enable_all().build()?)
}

/// A joined machine always changes assignments at the Hub, even without a daemon.
pub(crate) fn assign_direct(cli: &Cli, repo: &Path, lock: Option<&sk1ll_core::EngineLock>) -> Result<bool> {
    let cfg = daemon_config(cli)?;
    let Some(state) = DaemonState::load(&cfg.state)?.filter(|s| !s.revoked) else { return Ok(false) };
    let Cmd::Assign { harness, kind, name, state: enabled, default_skills, default_fragments } = &cli.cmd else {
        return Ok(false);
    };
    if !sk1ll_core::MachineConfig::load(&cfg.machine_config)?.harnesses().contains_key(harness) {
        bail!("unknown harness `{harness}` on this machine (see `sk1ll harnesses`)");
    }
    let mut changes = Vec::new();
    let toggle = |kind, name, on: bool| ToggleChange {
        harness: harness.clone(), kind, name, enabled: Some(on),
    };
    let target = assign_target(*kind, name.clone(), *enabled, *default_skills, *default_fragments)?;
    if let Some((kind, name, enabled)) = target {
        check_exists(&sk1ll_core::Repo::open(repo)?, kind, &name)?;
        let kind = match kind { Kind::Skill => ToggleKind::Skill, Kind::Fragment => ToggleKind::Fragment };
        changes.push(toggle(kind, Some(name), enabled.on()));
    }
    if let Some(s) = default_skills {
        changes.push(toggle(ToggleKind::NewSkillsDefault, None, s.on()));
    }
    if let Some(s) = default_fragments {
        changes.push(toggle(ToggleKind::NewFragmentsDefault, None, s.on()));
    }
    let lock = lock.context("a fleet assignment change requires the engine lock")?;
    let update = runtime()?.block_on(sk1ll_daemon::set_assignment_direct(&cfg, lock, &AssignmentChanges { changes }))?;
    println!("fleet assignment of {} is now revision {}; run `sk1ll sync` to apply", state.machine_id, update.revision);
    Ok(true)
}

/// The daemon's config, with the CLI's `--machine-config` / `--assignment`.
fn daemon_config(cli: &Cli) -> Result<Config> {
    let mut cfg = Config::from_env()?;
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

pub(crate) fn queue_discarded(
    state: Option<&Path>, lock: Option<&sk1ll_core::EngineLock>, items: Vec<sk1ll_core::ItemRef>,
) -> Result<()> {
    let mut cfg = Config::from_env()?;
    if let Some(p) = state {
        cfg.state = p.to_path_buf();
    }
    if DaemonState::load(&cfg.state)?.is_some_and(|s| !s.revoked) {
        sk1ll_daemon::state::queue_discards(&cfg, lock.context("a fleet reset requires the engine lock")?, items)?;
    }
    Ok(())
}

fn client(cli: &Cli) -> Result<Client> {
    Ok(match &cli.daemon_socket {
        Some(p) => Client::new(p),
        None => Client::default_socket()?,
    })
}

/// `join`, `daemon` and `fleet`.
pub(crate) fn command(cli: Cli) -> Result<()> {
    match &cli.cmd {
        Cmd::Fleet => fleet(&cli),
        Cmd::Daemon => {
            let cfg = daemon_config(&cli)?;
            sk1ll_daemon::init_logging();
            tokio::runtime::Runtime::new()?.block_on(sk1ll_daemon::run(cfg))
        }
        Cmd::Join { .. } => join(cli),
        _ => unreachable!(),
    }
}

fn join(cli: Cli) -> Result<()> {
    let cfg = daemon_config(&cli)?;
    let Cmd::Join { hub_url, code, id, copy_from } = cli.cmd else { unreachable!() };
    let repo = match cli.repo {
        Some(r) => r,
        None => dirs::home_dir().context("cannot determine the home directory")?.join("skills"),
    };
    let opts = sk1ll_daemon::JoinOptions { hub_url, code, machine_id: id, copy_from, repo };
    let (joined, via_daemon) = runtime()?.block_on(sk1ll_daemon::join_or_rejoin(&cfg, opts))?;
    let how = match joined.setup {
        sk1ll_daemon::gitx::Setup::Cloned => "cloned the fleet repo into",
        sk1ll_daemon::gitx::Setup::Initialized => {
            "the hub is empty; started a new skills repo (promote to share it) in"
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
        println!("now run `sk1ll daemon` (e.g. as the systemd user unit in deploy/sk1ll-daemon.service)");
    }
    Ok(())
}

fn fleet(cli: &Cli) -> Result<()> {
    let client = client(cli)?;
    let rt = runtime()?;
    let fleet = rt.block_on(client.fleet()).map_err(|e| {
        if e.is_not_running() {
            anyhow::anyhow!("the sk1ll daemon isn't running (start it with `sk1ll daemon`)")
        } else {
            e.into()
        }
    })?;
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
            if r.blocked {
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
        println!("{:<20} {:<9} seen {:<10} {}", m.id, status, seen, notes.join(", "));
    }
    Ok(())
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
    fn block<T>(&self, f: impl Future<Output = sk1ll_daemon::client::Result<T>>) -> Result<T> {
        Ok(self.rt.block_on(f)?)
    }

    /// Runs a mutating command through the daemon. `repo` is only read
    /// (to check names).
    pub(crate) fn run(self, cmd: Cmd, repo: &Path) -> Result<()> {
        let c = &self.client;
        match cmd {
            Cmd::Conflicts | Cmd::Recover { command: crate::RecoverCmd::List } => {
                let entries = if matches!(cmd, Cmd::Conflicts) {
                    self.block(c.conflicts())?
                } else {
                    self.block(c.recovered())?
                };
                for e in entries {
                    if e.kind.as_deref() == Some("displaced") {
                        println!("{} {} {} {}", e.id.unwrap_or_default(), e.harness, e.name,
                            e.path.unwrap_or_default().display());
                    } else if matches!(cmd, Cmd::Conflicts) {
                        println!("! {} {} {}", e.harness, e.name, e.dest.display());
                    }
                }
            }
            Cmd::Recover { command: crate::RecoverCmd::Adopt { id } } => self.block(c.adopt_displaced(&id))?,
            Cmd::Recover { command: crate::RecoverCmd::Discard { id } } => self.block(c.discard_displaced(&id))?,
            Cmd::Promote { items, message, author, push: _ } => {
                let resp = self.block(c.promote(&PromoteRequest { items, message, author }))?;
                println!("promoted {} as {}", names(&resp.items), short(&resp.sha));
                match (resp.pushed, resp.push_error) {
                    (true, _) => println!("pushed to the hub"),
                    (false, Some(e)) => println!("not pushed yet ({e}); the daemon pushes it when it can"),
                    (false, None) => println!("not pushed yet; the daemon pushes it when it can"),
                }
            }
            Cmd::Push => match self.block(c.push())?.pushed {
                true => println!("pushed to the hub"),
                false => println!("nothing to push"),
            },
            Cmd::Reset { items, all, yes } => {
                if all {
                    let draft = self.block(c.draft())?;
                    if draft.is_empty() {
                        println!("no draft to reset");
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
                let resp = self.block(c.reset(&ResetRequest { items, all }))?;
                if resp.discarded.is_empty() {
                    println!("nothing was reset");
                } else {
                    println!("reset {}", names(&resp.discarded));
                }
            }
            Cmd::Pull => match self.block(c.pull())? {
                PullResult::UpToDate => println!("already on the latest baseline"),
                PullResult::Updated { from, to } => {
                    println!("baseline {} -> {}", from.as_deref().map_or("(none)", short), short(&to));
                }
                PullResult::Blocked { baseline, items } => bail!(
                    "blocked: baseline {} clashes with {}; promote or reset them (the daemon pulls again then)",
                    short(&baseline),
                    names(&items)
                ),
            },
            Cmd::Sync { dry_run: _, force } => {
                let result = self.block(c.sync(&SyncRequest { force }))?;
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
                let req = AddImportRequest { name: name.unwrap_or_default(), source, path, git_ref, commit: None };
                let r = self.block(c.add_import(&req)).map_err(fetch_hint)?;
                println!("added {} @ {}", r.name, short(&r.new_commit));
            }
            Cmd::Update { name, force } => {
                for r in self.block(c.update_imports(&UpdateImportsRequest { name, force }))? {
                    print_update(&r.name, r.old_commit.as_deref(), &r.new_commit, r.changed);
                }
                println!("review with `sk1ll diff`, then promote");
            }
            Cmd::Remove { name } => {
                self.block(c.remove_import(&name))?;
                println!("removed {name}");
            }
            Cmd::Fork { name } => {
                self.block(c.fork(&name))?;
                println!("moved {name} to personal/");
            }
            Cmd::Assign { harness, kind, name, state, default_skills, default_fragments } => {
                // Assignment is hub state: change it there, for this machine.
                if !self.block(c.harnesses())?.iter().any(|h| h.name == harness) {
                    bail!("unknown harness `{harness}` on this machine (see `sk1ll harnesses`)");
                }
                let mut changes = Vec::new();
                let toggle = |kind, name, on: bool| ToggleChange {
                    harness: harness.clone(),
                    kind,
                    name,
                    enabled: Some(on),
                };
                let target = assign_target(kind, name, state, default_skills, default_fragments)?;
                if let Some((kind, name, state)) = target {
                    check_exists(&sk1ll_core::Repo::open(repo)?, kind, &name)?;
                    let kind = match kind {
                        Kind::Skill => ToggleKind::Skill,
                        Kind::Fragment => ToggleKind::Fragment,
                    };
                    changes.push(toggle(kind, Some(name), state.on()));
                }
                if let Some(s) = default_skills {
                    changes.push(toggle(ToggleKind::NewSkillsDefault, None, s.on()));
                }
                if let Some(s) = default_fragments {
                    changes.push(toggle(ToggleKind::NewFragmentsDefault, None, s.on()));
                }
                let machine = self.status.machine_id.clone();
                let update = self.block(c.set_assignment(&machine, &AssignmentChanges { changes }))?;
                if update.entries.is_empty() {
                    println!("{harness}: already so on {machine}");
                }
                for e in &update.entries {
                    let what = match (&e.kind, &e.name) {
                        (ToggleKind::NewSkillsDefault, _) => "skills default".to_string(),
                        (ToggleKind::NewFragmentsDefault, _) => "fragments default".to_string(),
                        (k, Some(n)) => format!("{} {n}", if *k == ToggleKind::Skill { "skill" } else { "fragment" }),
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
