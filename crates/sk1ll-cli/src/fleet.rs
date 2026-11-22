//! `join`, `daemon` and `fleet`, and where every other command's requests
//! go ([`Session`]): the running daemon, or an engine in this process.

use std::io::{IsTerminal, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use sk1ll_daemon::client::{Client, StatusView};
use sk1ll_daemon::service::Manager;
use sk1ll_daemon::{Config, DaemonState};
use sk1ll_proto::{LogQuery, Scope, ToggleKind};

use crate::{Cli, Cmd, FleetCmd, InviteScope, confirm, names, plural, short};

pub(crate) fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread().enable_all().build()?)
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

/// The repo a joined Machine's engine manages is the one it joined with.
fn check_joined_repo(repo: &Path, state: &DaemonState) -> Result<()> {
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    anyhow::ensure!(
        canon(repo) == canon(&state.repo),
        "selected repo {} differs from the joined repo {}; select the joined repo (--repo or SK1LL_REPO)",
        repo.display(),
        state.repo.display()
    );
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
        sk1ll_core::git::plumbing::Setup::Cloned => "cloned the fleet repo into",
        sk1ll_core::git::plumbing::Setup::Adopted => "using the existing clone at",
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

/// Fleet administration: the server's API, through the engine (which adds
/// the machine token).
fn fleet(cli: &Cli, command: Option<&FleetCmd>) -> Result<()> {
    // Confirmed before anything is asked of the engine.
    if let Some(FleetCmd::Remove { machine, yes: false }) = command {
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
    let s = Session::any(cli)?;
    let client = &s.client;
    match command {
        Some(FleetCmd::Invite { scope }) => {
            let url = s.status.server_url.clone().context("this machine hasn't joined a fleet")?;
            let scope = match scope {
                InviteScope::Fleet => Scope::Fleet,
                InviteScope::SelfOnly => Scope::SelfOnly,
            };
            let code = s.block(client.join_code(scope))?;
            println!("{}", code.code);
            println!("sk1ll join {} {}", shell_arg(&url), shell_arg(&code.code));
            println!("expires {}", utc(code.expires_at));
            match scope {
                Scope::Fleet => println!("scope fleet: the Machine may manage the whole Fleet"),
                Scope::SelfOnly => println!("scope self: the Machine may manage only itself"),
            }
        }
        Some(FleetCmd::Remove { machine, .. }) => {
            s.block(client.remove_machine(machine))?;
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
            let preview = s.block(client.preview_assignment_copy(target, &copy))?;
            if preview.count == 0 {
                println!("{target} already matches {source}'s Assignment; nothing to copy");
                return Ok(());
            }
            let count = plural(preview.count, "Assignment change", "Assignment changes");
            println!("{count} on {target}:");
            for change in &preview.changes {
                let item = match (change.kind, &change.name, &change.item_id) {
                    (ToggleKind::NewSkillsDefault, ..) => "skills default".to_string(),
                    (ToggleKind::NewRulesDefault, ..) => "rules default".to_string(),
                    (kind, name, id) => {
                        let kind = if kind == ToggleKind::Skill { "skill" } else { "rule" };
                        let name = name.clone().or_else(|| id.as_ref().map(|id| format!("[{id}]")));
                        format!("{kind} {}", name.unwrap_or_default())
                    }
                };
                let state = |value| match value {
                    Some(true) => "on",
                    Some(false) => "off",
                    None => "default",
                };
                println!("  {:<10} {item:<32} {} -> {}", change.harness, state(change.before), state(change.after));
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
            let update = s.block(client.copy_machine_assignment(target, &copy))?;
            println!("copied Assignment to {target} at revision {}", update.revision);
        }
        Some(FleetCmd::Orphans) => {
            let view = s.block(client.orphans())?;
            for orphan in &view.orphans {
                let name = orphan.name.as_deref().unwrap_or("-");
                println!("{} / {}  {:?} {} ({name})", orphan.machine, orphan.harness, orphan.kind, orphan.item_id);
            }
            println!("{}", plural(view.orphans.len(), "orphan toggle", "orphan toggles"));
        }
        Some(FleetCmd::PruneOrphans { machine }) => {
            let result = s.block(client.prune_orphans(&sk1ll_proto::PruneOrphans { machine: machine.clone() }))?;
            println!("cleared {}", plural(result.entries.len(), "orphan toggle", "orphan toggles"));
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
            let entries = s.block(client.log(&query))?;
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
                    "{}  {}  {} / {}  {}  {} -> {}  by {}{}",
                    entry.id,
                    utc(entry.at),
                    entry.machine,
                    entry.harness,
                    item,
                    state(entry.before),
                    state(entry.after),
                    entry.author,
                    cause(&entry.cause)
                );
            }
        }
        Some(FleetCmd::Undo { entry_id }) => {
            let update = s.block(client.undo(*entry_id))?;
            if update.entries.is_empty() {
                println!("Assignment log entry {entry_id}'s previous value is already set; nothing changed");
            } else {
                println!("undid Assignment log entry {entry_id} at revision {}", update.revision);
            }
        }
        None => print_fleet(s.block(client.fleet())?),
    }
    Ok(())
}

/// Why an Assignment log entry was written, for `fleet log`: nothing for
/// an ordinary change.
fn cause(cause: &sk1ll_proto::LogCause) -> String {
    use sk1ll_proto::LogCause as C;
    match cause {
        C::Change => String::new(),
        C::Undo { entry } => format!(" (undoing entry {entry})"),
        C::DefaultMaterialized => " (kept as the old default when the default changed)".into(),
        C::MachineRemoved => " (machine removed from the fleet)".into(),
        C::HarnessAdded => " (new harness, starts all off)".into(),
        C::HarnessRenamed { from, to } => format!(" (harness renamed from {from} to {to})"),
        C::OrphanPruned => " (orphan toggle pruned)".into(),
    }
}

pub(crate) fn shell_arg(arg: &str) -> String {
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
                notes.push(plural(r.conflicts.len(), "conflict", "conflicts"));
            }
            if !r.drafts.is_empty() {
                notes.push(plural(r.drafts.len(), "draft item", "draft items"));
            }
            if !r.unpushed.is_empty() {
                notes.push(format!("{} unpushed", r.unpushed.len()));
            }
            if let Some(reason) = &r.push_refused {
                notes.push(format!("push refused: {}", reason.lines().next().unwrap_or_default()));
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

pub(crate) fn ago(secs: i64) -> String {
    match secs.max(0) {
        s if s < 60 => format!("{s}s ago"),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86400),
    }
}

/// Where a command's requests go: the running daemon when it manages the
/// repo, else an engine started in this process for the command (see
/// [`sk1ll_daemon::local`]). Either way, the same local API.
///
/// One engine at a time manages a Machine, whatever repo it is for: a
/// command that changes something runs only where it holds the machine
/// lock. One that only reads ([`Access::Read`]) starts a read-only engine,
/// which holds no lock and changes nothing, so it answers beside another.
pub(crate) struct Session {
    /// The engine in this process, if no daemon serves the command.
    engine: Option<sk1ll_daemon::local::Engine>,
    pub(crate) client: Client,
    pub(crate) status: StatusView,
    rt: tokio::runtime::Runtime,
}

/// What a command does to the Machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    /// Only reads: a preview, a listing.
    Read,
    /// May change the repo, the harnesses or the Assignment.
    Write,
}

impl Session {
    /// The session for a command about `repo`. A daemon that isn't
    /// running (no socket, or connection refused) means an engine in this
    /// process; any other failure to talk to the daemon is an error, since
    /// one may well be running. A daemon that manages another repo is the
    /// Machine's engine: a command that only reads runs beside it, read-only;
    /// one that changes something is refused.
    pub(crate) fn open(cli: &Cli, repo: &Path, access: Access) -> Result<Self> {
        let (rt, daemon) = Self::daemon(cli)?;
        // A repo asked for by name has to be there (only `init` makes one),
        // unless the daemon serves it.
        let missing = || {
            if cli.repo.is_some()
                && !repo.is_dir()
                && !matches!(cli.cmd, Cmd::Fleet { .. } | Cmd::Setup | Cmd::Join { .. })
            {
                bail!(
                    "no such directory: {} (from --repo or SK1LL_REPO); create a skills repo there with `sk1ll init \
                     {}`",
                    repo.display(),
                    repo.display()
                );
            }
            Ok(())
        };
        if let Some((client, status)) = daemon {
            if same_path(&status.repo, repo) {
                return Ok(Self { engine: None, client, status, rt });
            }
            missing()?;
            if access == Access::Write {
                bail!(
                    "the sk1ll daemon manages {} on this Machine, so it alone changes its harnesses and \
                     Assignment; select that repo, or stop the daemon to work on {}",
                    status.repo.display(),
                    repo.display()
                );
            }
            eprintln!(
                "note: the sk1ll daemon manages {}, not {}; reading {} in this process",
                status.repo.display(),
                repo.display(),
                repo.display()
            );
        }
        missing()?;
        let cfg = Config { repo: Some(repo.to_path_buf()), ..daemon_config(cli)? };
        let state_path = cfg.state.clone();
        let engine = {
            let _runtime = rt.enter();
            match access {
                // Takes the machine lock before reading the state file.
                Access::Write => sk1ll_daemon::local::Engine::start(cfg)?,
                Access::Read => sk1ll_daemon::local::Engine::read_only(cfg)?,
            }
        };
        // Reading another repo than the joined one is reading a standalone one.
        if access == Access::Write
            && let Some(state) = DaemonState::load(&state_path)?.filter(|s| !s.revoked)
        {
            check_joined_repo(repo, &state)?;
        }
        let client = engine.client().clone();
        let status = rt.block_on(client.status())?;
        Ok(Self { engine: Some(engine), client, status, rt })
    }

    /// For a command about the Machine rather than a repo: the running
    /// daemon whatever it manages, else an engine for the joined repo (or,
    /// standalone, the selected one).
    pub(crate) fn any(cli: &Cli) -> Result<Self> {
        let cfg = daemon_config(cli)?;
        let (rt, daemon) = Self::daemon(cli)?;
        if let Some((client, status)) = daemon {
            return Ok(Self { engine: None, client, status, rt });
        }
        drop(rt);
        let repo = match DaemonState::load(&cfg.state)? {
            Some(state) => state.repo,
            None => sk1ll_daemon::locate_repo(cli.repo.as_deref(), false)?,
        };
        Self::open(cli, &repo, Access::Write)
    }

    /// The running daemon, if it manages `repo`.
    pub(crate) fn daemon_for(cli: &Cli, repo: &Path) -> Result<Option<Self>> {
        let (rt, daemon) = Self::daemon(cli)?;
        Ok(daemon.filter(|(_, status)| same_path(&status.repo, repo)).map(|(client, status)| Self {
            engine: None,
            client,
            status,
            rt,
        }))
    }

    /// The repo the running daemon manages, if one runs.
    pub(crate) fn daemon_repo(cli: &Cli) -> Result<Option<std::path::PathBuf>> {
        Ok(Self::daemon(cli)?.1.map(|(_, status)| status.repo))
    }

    fn daemon(cli: &Cli) -> Result<(tokio::runtime::Runtime, Option<(Client, StatusView)>)> {
        let rt = runtime()?;
        let client = client(cli)?;
        let daemon = match rt.block_on(client.status()) {
            Ok(status) => Some((client, status)),
            Err(e) if e.is_not_running() => None,
            Err(e) => bail!(
                "cannot talk to the sk1ll daemon ({e}); not running this command without it while it may be \
                 running (check `systemctl --user status sk1ll-daemon`)"
            ),
        };
        Ok((rt, daemon))
    }

    /// Whether the engine runs in this process rather than in a daemon.
    pub(crate) fn in_process(&self) -> bool {
        self.engine.is_some()
    }

    pub(crate) fn block<T>(&self, f: impl Future<Output = sk1ll_daemon::client::Result<T>>) -> Result<T> {
        Ok(self.rt.block_on(f)?)
    }
}

pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

#[cfg(test)]
mod tests {
    use super::utc;

    #[test]
    fn log_causes_read_as_text() {
        use sk1ll_proto::LogCause as C;
        assert_eq!(super::cause(&C::Change), "");
        assert_eq!(super::cause(&C::Undo { entry: 7 }), " (undoing entry 7)");
        let renamed = super::cause(&C::HarnessRenamed { from: "a".into(), to: "b".into() });
        assert_eq!(renamed, " (harness renamed from a to b)");
        assert!(!super::cause(&C::OrphanPruned).contains('{'));
    }

    #[test]
    fn utc_dates() {
        assert_eq!(utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc(1_700_086_400), "2023-11-15 22:13:20 UTC");
        assert_eq!(utc(951_782_400), "2000-02-29 00:00:00 UTC");
        assert_eq!(utc(-1), "1969-12-31 23:59:59 UTC");
    }
}
