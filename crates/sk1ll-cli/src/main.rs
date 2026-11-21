use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use sk1ll_core::{
    Action, ActionKind, Assignment, Change, EngineLock, ImportState, Item, ItemRef, MachineConfig, Origin, PlanOptions,
    PullOutcome, Repo, default_commit_message, machine_author, sync,
};

#[cfg(unix)]
mod fleet;
mod harnesses;
#[cfg(unix)]
mod setup;

/// Without Unix there is no daemon: every command works directly.
#[cfg(not(unix))]
mod fleet {
    use std::path::Path;

    use anyhow::{Result, bail};

    use crate::{Cli, Cmd};

    pub(crate) enum Daemon {}

    impl Daemon {
        pub(crate) fn run(self, _cmd: Cmd, _repo: &Path) -> Result<()> {
            match self {}
        }
    }

    pub(crate) fn prepare(_cli: &Cli) -> Result<()> {
        Ok(())
    }

    pub(crate) fn assignment(_state: Option<&Path>, cache: &Path) -> Result<sk1ll_core::Assignment> {
        Ok(sk1ll_core::Assignment::load(cache)?)
    }

    pub(crate) fn assign_direct(_cli: &Cli, _repo: &Path, _lock: Option<&sk1ll_core::EngineLock>) -> Result<bool> {
        Ok(false)
    }

    pub(crate) fn require_standalone_adoption(_state: Option<&Path>, _repo: &sk1ll_core::Repo) -> Result<()> {
        Ok(())
    }

    pub(crate) fn connect(cli: &Cli, _repo: &Path) -> Result<Option<Daemon>> {
        let _ = &cli.daemon_socket;
        Ok(None)
    }

    pub(crate) fn command(cli: Cli) -> Result<()> {
        let name = match cli.cmd {
            Cmd::Join { .. } => "join",
            Cmd::Setup => "setup",
            Cmd::Daemon => "daemon",
            _ => "fleet",
        };
        bail!("`sk1ll {name}` is not supported on this platform: the daemon and the fleet need Unix")
    }
}

#[derive(Parser)]
#[command(
    name = "sk1ll",
    version = sk1ll_core::VERSION,
    about = "0xSk1ll manages agent skills and instructions and syncs them across machines and harnesses"
)]
struct Cli {
    /// Skills repo (default: nearest sk1ll.toml above cwd, else ~/.local/share/sk1ll/repo)
    #[arg(long, global = true, env = "SK1LL_REPO")]
    repo: Option<PathBuf>,
    /// Machine-local harness config (default: ~/.config/sk1ll/machine.toml)
    #[arg(long, global = true, env = "SK1LL_MACHINE_CONFIG")]
    machine_config: Option<PathBuf>,
    /// Assignment cache (default: ~/.cache/sk1ll/assignment.json)
    #[arg(long, global = true, env = "SK1LL_ASSIGNMENT")]
    assignment: Option<PathBuf>,
    /// The daemon's local API socket (default: $XDG_RUNTIME_DIR/sk1ll/daemon.sock)
    #[arg(long, global = true, hide = true, env = "SK1LL_DAEMON_SOCKET")]
    daemon_socket: Option<PathBuf>,
    /// Persisted fleet identity and credentials
    #[arg(long, global = true, hide = true, env = "SK1LL_DAEMON_STATE")]
    daemon_state: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List harness and recovery conflicts
    Conflicts,
    /// Inspect or resolve recovered content
    Recover {
        #[command(subcommand)]
        command: RecoverCmd,
    },
    /// Create a new skills repo
    Init { path: Option<PathBuf> },
    /// List skills and instruction rules in the repo
    List {
        /// List unmanaged skills in this machine's harnesses
        #[arg(long)]
        unmanaged: bool,
        /// Print repo Skills and Rules as JSON
        #[arg(long, conflicts_with = "unmanaged")]
        json: bool,
    },
    /// Open a personal Skill or Rule in $VISUAL or $EDITOR
    Edit { name: String },
    /// Import a skill from a link (GitHub repo, folder or file), github:owner/repo, git URL or local path
    #[command(after_help = "Examples:\n  sk1ll add https://github.com/owner/repo/tree/main/skills/pdf\n  \
                            sk1ll add --list https://github.com/owner/repo\n  \
                            sk1ll add github:owner/repo --path skills/pdf --ref v1.2")]
    Add {
        #[arg(value_name = "LINK")]
        source: String,
        /// Subdirectory containing SKILL.md (default: the link's)
        #[arg(long)]
        path: Option<String>,
        /// Branch, tag or commit (default: the link's, else HEAD). If the link names a ref, they must agree
        /// unless --path is given too
        #[arg(long = "ref")]
        git_ref: Option<String>,
        /// Local name (default: last path segment or repo name)
        #[arg(long)]
        name: Option<String>,
        /// List the skills the link leads to, without adding any
        #[arg(long, conflicts_with = "name")]
        list: bool,
    },
    /// Re-fetch imports and update the lockfile
    Update {
        name: Option<String>,
        /// Overwrite local edits to vendored imports
        #[arg(long)]
        force: bool,
        /// Show upstream commits and changed files for one import
        #[arg(long, requires = "name", conflicts_with = "force")]
        dry_run: bool,
    },
    /// Remove an import
    Remove { name: String },
    /// Turn an import into a personal skill you can edit
    Fork { name: String },
    /// Show import drift and pending sync actions
    Status,
    /// Link/copy skills and write instructions into harnesses
    Sync {
        #[arg(long)]
        dry_run: bool,
        /// Overwrite or remove hand-edited managed instruction blocks, skill copies, and rule files
        #[arg(long)]
        force: bool,
    },
    /// List this machine's draft: uncommitted changes, per skill/rule
    Draft,
    /// Show a draft item's changes against the baseline
    #[command(after_help = "Items: a skill or rule name, or skill:<name>, rule:<name>, manifest, other:<path>")]
    Diff { item: String },
    /// Commit draft items as the new baseline
    #[command(after_help = "Items: a skill or rule name, or skill:<name>, rule:<name>, manifest, other:<path>")]
    Commit {
        #[arg(required = true)]
        items: Vec<String>,
        /// Commit message (default: "commit <items> from <machine>")
        #[arg(short, long)]
        message: Option<String>,
        /// Commit author, "Name <email>" (default: git's identity, else the machine id)
        #[arg(long)]
        author: Option<String>,
        /// Push to origin afterwards
        #[arg(long)]
        push: bool,
    },
    /// Push committed commits to origin
    Push,
    /// Discard draft items, returning them to the baseline
    Restore {
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        items: Vec<String>,
        /// Discard the whole draft
        #[arg(long)]
        all: bool,
        /// Don't ask for confirmation (with --all)
        #[arg(long, short)]
        yes: bool,
    },
    /// Fetch the latest baseline from origin, keeping the draft on top
    Pull,
    /// Copy a conflicting harness copy back into the repo as a draft change
    Adopt { harness: String, name: String },
    /// Print the instruction files a harness would receive
    Render { harness: String },
    /// List the harnesses sync writes into (detected presets + machine.toml), or change them
    Harnesses {
        /// Also list disabled harnesses, and presets a newer sk1ll added that are off until enabled
        #[arg(long)]
        all: bool,
        #[command(subcommand)]
        command: Option<harnesses::HarnessCmd>,
    },
    /// Turn a skill or rule on/off for a harness, or set its defaults
    #[command(
        after_help = "Examples:\n  sk1ll assign claude skill pdf off\n  sk1ll assign codex --default-skills off\n  \
                      sk1ll assign kiro --all off   # then `sk1ll sync` takes out what sk1ll placed there"
    )]
    Assign {
        harness: String,
        kind: Option<Kind>,
        name: Option<String>,
        state: Option<Switch>,
        /// Whether newly appearing skills are enabled
        #[arg(long, value_name = "on|off")]
        default_skills: Option<Switch>,
        /// Whether newly appearing rules are enabled
        #[arg(long, value_name = "on|off")]
        default_rules: Option<Switch>,
        /// Every skill and rule in the repo, and both defaults
        #[arg(long, value_name = "on|off", conflicts_with_all = ["kind", "default_skills", "default_rules"])]
        all: Option<Switch>,
    },
    /// Set up this machine step by step: a standalone repo, joining a fleet or starting one
    Setup,
    /// Join a fleet: register with the server and clone its repo into --repo (default ~/.local/share/sk1ll/repo)
    Join {
        /// The server's base URL, e.g. https://server.example
        server_url: String,
        /// Single-use join code from `sk1ll fleet invite` on a joined machine
        #[arg(required_unless_present = "admin_token")]
        code: Option<String>,
        /// The server's admin token: creates a join code for the first machine of a new fleet (a given code wins)
        #[arg(long, env = "SK1LL_SERVER_ADMIN_TOKEN", hide_env_values = true)]
        admin_token: Option<String>,
        /// Machine id (default: machine.toml's id, else the hostname)
        #[arg(long)]
        id: Option<String>,
        /// Start from this machine's assignment instead of everything off
        #[arg(long)]
        copy_from: Option<String>,
    },
    /// Run the daemon in the foreground (what systemd or launchd runs)
    Daemon,
    /// Show every machine in the fleet or administer it through the daemon
    Fleet {
        #[command(subcommand)]
        command: Option<FleetCmd>,
    },
    /// Generate shell completions
    Completions { shell: clap_complete::Shell },
}

#[derive(Subcommand)]
enum FleetCmd {
    /// Create a single-use join code
    #[command(after_help = "Examples:\n  sk1ll fleet invite\n  sk1ll fleet invite --scope self")]
    Invite {
        /// What the joining Machine may manage: the whole Fleet, or only itself
        #[arg(long, value_enum, default_value_t = InviteScope::Fleet, value_name = "fleet|self")]
        scope: InviteScope,
    },
    /// Remove a machine from the fleet
    Remove {
        /// Machine id (put `--` before an id that starts with `-`)
        machine: String,
        /// Remove without asking for confirmation
        #[arg(long)]
        yes: bool,
    },
    /// Copy a joined Machine's Assignment to another Machine
    CopyAssignment {
        source: String,
        target: String,
        /// Fill only unset toggles and keep the target defaults
        #[arg(long)]
        merge: bool,
        /// Map a source Harness to a target Harness (repeatable)
        #[arg(long = "map", value_name = "FROM=TO")]
        maps: Vec<String>,
        /// Apply without asking for confirmation
        #[arg(long)]
        yes: bool,
    },
    /// List toggles for items absent from the Baseline and every Machine's last report
    Orphans,
    /// Clear orphan toggles in your scope
    PruneOrphans {
        /// Clear only this Machine's orphan toggles
        #[arg(long)]
        machine: Option<String>,
    },
    /// Show the Assignment log, newest first
    Log {
        #[arg(long)]
        machine: Option<String>,
        #[arg(long)]
        harness: Option<String>,
        #[arg(long)]
        author: Option<String>,
        /// Match part of the item key, such as skill:pdf (case-insensitive)
        #[arg(long)]
        item: Option<String>,
        /// Show entries below this entry id
        #[arg(long)]
        before: Option<i64>,
        /// Maximum entries to show (server default 100, maximum 1000)
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Undo an Assignment log entry
    Undo { entry_id: i64 },
}

#[derive(Subcommand)]
enum RecoverCmd {
    List,
    Adopt { id: String },
    Discard { id: String },
}

fn print_recovered(entries: Vec<sync::displaced::Entry>) {
    for e in entries {
        println!(
            "{} {} {:?} {} {}{}",
            e.id,
            e.harness,
            e.kind,
            e.dest.display(),
            e.path.display(),
            if e.changed { " (changed)" } else { "" }
        );
    }
}

impl Cmd {
    fn git_only(&self) -> bool {
        matches!(self, Self::Draft | Self::Diff { .. } | Self::Restore { .. } | Self::Commit { .. } | Self::Pull)
    }

    /// Changes the repo, the harnesses or the assignment. With a daemon
    /// managing the repo, these go through it; otherwise they hold the
    /// engine lock while they run.
    fn mutates(&self) -> bool {
        match self {
            Self::Recover { .. }
            | Self::Conflicts
            | Self::Remove { .. }
            | Self::Fork { .. }
            | Self::Commit { .. }
            | Self::Push
            | Self::Restore { .. }
            | Self::Pull
            | Self::Adopt { .. }
            | Self::Assign { .. } => true,
            Self::Sync { dry_run, .. } | Self::Update { dry_run, .. } => !dry_run,
            Self::Add { list, .. } => !list,
            Self::Init { .. }
            | Self::List { .. }
            | Self::Edit { .. }
            | Self::Status
            | Self::Draft
            | Self::Diff { .. }
            | Self::Render { .. }
            | Self::Harnesses { .. }
            | Self::Join { .. }
            | Self::Setup
            | Self::Daemon
            | Self::Fleet { .. }
            | Self::Completions { .. } => false,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum Kind {
    Skill,
    Rule,
}

#[derive(Clone, Copy, ValueEnum)]
enum InviteScope {
    Fleet,
    #[value(name = "self")]
    SelfOnly,
}

#[derive(Clone, Copy, ValueEnum)]
enum Switch {
    On,
    Off,
}

impl Switch {
    fn on(self) -> bool {
        matches!(self, Switch::On)
    }

    fn label(self) -> &'static str {
        if self.on() { "on" } else { "off" }
    }
}

fn main() -> Result<()> {
    let mut cli = Cli::parse();
    if let Cmd::Completions { shell } = cli.cmd {
        clap_complete::generate(shell, &mut Cli::command(), "sk1ll", &mut std::io::stdout());
        return Ok(());
    }
    // A relative local source means the user's cwd, which the daemon doesn't
    // share. Links and invalid sources are left for `parse_link` to handle.
    if let Cmd::Add { source, .. } = &mut cli.cmd
        && let Ok(link) = sk1ll_core::source::parse_link(source)
        && sk1ll_core::source::is_local_path(&link.source)
        && Path::new(&link.source).is_relative()
    {
        let path = std::fs::canonicalize(&link.source).or_else(|_| std::path::absolute(&link.source))?;
        *source = path.to_string_lossy().into_owned();
    }
    // Single-threaded here, before any runtime starts: the only safe place to
    // change the environment (git's credentials for the server, see `fleet`).
    fleet::prepare(&cli)?;
    run(cli)
}

fn run(cli: Cli) -> Result<()> {
    if matches!(cli.cmd, Cmd::Join { .. } | Cmd::Setup | Cmd::Daemon | Cmd::Fleet { .. }) {
        return fleet::command(cli);
    }
    #[cfg(unix)]
    let repo_path = sk1ll_daemon::locate_repo(cli.repo.as_deref(), cli.cmd.git_only())?;
    #[cfg(not(unix))]
    let repo_path = match &cli.repo {
        Some(p) => p.clone(),
        None => {
            let cwd = std::env::current_dir()?;
            if cli.cmd.git_only() { Repo::locate_git(&cwd) } else { Repo::locate(&cwd) }
                .context("cannot determine repo location")?
        }
    };

    if let Cmd::Init { path } = &cli.cmd {
        sk1ll_core::git::require_git(sk1ll_core::git::MIN_VERSION)?;
        let root = path.clone().unwrap_or(repo_path);
        let _lock = lock_direct(&root)?;
        // A machine without machine.toml is set up for the first time here:
        // it takes up the opt-in presets it finds (see MachineConfig::adopt_detected_presets).
        let machine_path = cli.machine_config.clone().map_or_else(MachineConfig::default_path, Ok)?;
        if !root.join(".git").exists() && std::fs::symlink_metadata(&machine_path).is_err() {
            for warning in MachineConfig::adopt_detected_presets(&machine_path, &root)? {
                eprintln!("{warning}");
            }
        }
        std::fs::create_dir_all(&root)?;
        let repo = Repo::init_committed(&root)?;
        println!("initialized skills repo at {}", repo.root().display());
        println!("To let agents add skills: sk1ll add https://github.com/0xV1p3r/0xSk1ll/tree/main/skills/sk1ll");
        return Ok(());
    }

    let machine_path = match &cli.machine_config {
        Some(p) => p.clone(),
        None => MachineConfig::default_path()?,
    };
    let assignment_path = match &cli.assignment {
        Some(p) => p.clone(),
        None => Assignment::default_path()?,
    };
    let machine = || MachineConfig::load(&machine_path);
    let daemon_state = cli.daemon_state.clone();
    let assignment = || fleet::assignment(daemon_state.as_deref(), &assignment_path);

    if let Cmd::Harnesses { .. } = &cli.cmd {
        return harnesses::run(cli, &repo_path, &machine_path, &assignment_path);
    }

    // A daemon managing this repo does every change, so that changes never
    // interleave and it pushes, reports status and tells the server. Without one,
    // a change holds the engine lock (which fails if a daemon holds it).
    let mutates = cli.cmd.mutates();
    if (mutates || matches!(cli.cmd, Cmd::Update { dry_run: true, .. }))
        && let Some(daemon) = fleet::connect(&cli, &repo_path)?
    {
        return daemon.run(cli.cmd, &repo_path);
    }
    let lock = if mutates { lock_direct(&repo_path)? } else { None };
    if matches!(cli.cmd, Cmd::Assign { .. }) && fleet::assign_direct(&cli, &repo_path, lock.as_ref())? {
        return Ok(());
    }

    if lock.is_some() && matches!(cli.cmd, Cmd::Sync { .. } | Cmd::Assign { .. } | Cmd::Adopt { .. }) {
        harnesses::recover_pending(&cli, &machine_path, &assignment_path)?;
    }

    let mut repo = if cli.cmd.git_only() { Repo::open_git_only(&repo_path)? } else { Repo::open(&repo_path)? };
    match cli.cmd {
        Cmd::Recover { command } => match command {
            RecoverCmd::List => print_recovered(sync::displaced::list()?),
            RecoverCmd::Adopt { id } => sync::displaced::adopt(&repo, &id)?,
            RecoverCmd::Discard { id } => sync::displaced::discard(&id)?,
        },
        Cmd::Conflicts => {
            let actions = sync::plan(&repo, &machine()?.harnesses(), &assignment()?, PlanOptions::default())?;
            print_plan(&actions.into_iter().filter(|a| a.kind == ActionKind::Conflict).collect::<Vec<_>>());
            print_recovered(sync::displaced::changed()?);
        }
        Cmd::Init { .. }
        | Cmd::Join { .. }
        | Cmd::Setup
        | Cmd::Daemon
        | Cmd::Fleet { .. }
        | Cmd::Completions { .. } => unreachable!(),
        Cmd::List { unmanaged: true, .. } => {
            for skill in sync::unmanaged::discover(&repo, &machine()?.harnesses())? {
                let harnesses: std::collections::BTreeSet<_> =
                    skill.variants.iter().flat_map(|v| v.locations.iter().map(|l| l.harness.as_str())).collect();
                println!(
                    "{:<24} {:<24} {}",
                    skill.name,
                    harnesses.into_iter().collect::<Vec<_>>().join(", "),
                    if skill.variants.len() > 1 { "differs" } else { "" }
                );
            }
        }
        Cmd::Edit { name } => {
            let path = sk1ll_core::content::editable_path(&repo, &machine()?.harnesses(), &name)?;
            let editor = std::env::var("VISUAL")
                .ok()
                .filter(|e| !e.trim().is_empty())
                .or_else(|| std::env::var("EDITOR").ok().filter(|e| !e.trim().is_empty()))
                .context("set $VISUAL or $EDITOR to edit a Skill or Rule")?;
            #[cfg(unix)]
            let status = std::process::Command::new("sh")
                .args(["-c", &format!("{editor} \"$1\""), "sk1ll-edit"])
                .arg(&path)
                .status()?;
            #[cfg(not(unix))]
            let status = std::process::Command::new(&editor).arg(&path).status()?;
            if !status.success() {
                bail!("editor exited with {status}");
            }
        }
        Cmd::List { unmanaged: false, json: true } => {
            let content = serde_json::json!({ "skills": repo.skills()?, "rules": repo.rules()? });
            println!("{}", serde_json::to_string_pretty(&content)?);
        }
        Cmd::List { unmanaged: false, json: false } => {
            for s in repo.skills()? {
                let origin = match s.origin {
                    Origin::Personal if s.shadows_import => "personal*",
                    Origin::Personal => "personal",
                    Origin::Imported => "imported",
                };
                println!("{:<24} {:<10} {}", s.name, origin, s.description.unwrap_or_default());
            }
            for f in repo.rules()? {
                let scope = f.harnesses.map(|t| format!("[{}] ", t.join(", "))).unwrap_or_default();
                println!("{:<24} {:<10} {scope}{}", f.name, "instr", f.description.unwrap_or_default());
            }
        }
        Cmd::Add { source, path, git_ref, list: true, .. } => {
            let r = repo.resolve_source(&source, path.as_deref(), git_ref.as_deref()).map_err(fetch_hint)?;
            println!("{} @ {} ({})", r.source, r.git_ref, short(&r.commit));
            if r.candidates.is_empty() {
                println!("no SKILL.md found");
            }
            for c in r.candidates {
                let path = if c.path.is_empty() { "." } else { &c.path };
                let taken = if c.taken { "taken" } else { "" };
                println!("{path:<32} {:<24} {taken:<5} {}", c.default_name, c.description.unwrap_or_default());
                println!("  {} bytes total", c.total_bytes);
                for warning in c.warnings {
                    let kinds: Vec<_> = warning.kinds.into_iter().map(|kind| kind.label()).collect();
                    println!("  {:?}: {}", warning.path, kinds.join(", "));
                }
            }
        }
        Cmd::Add { source, path, git_ref, name, list: false } => {
            match repo.add_link(name.as_deref(), &source, path.as_deref(), git_ref.as_deref(), None).map_err(fetch_hint)
            {
                Ok(r) => println!("added {} @ {}", r.name, short(&r.new_commit)),
                Err(e) => {
                    let (bundled, e) = bundled_fallback(&source, path.is_some() || git_ref.is_some(), e)?;
                    let name = name.unwrap_or_else(|| bundled.to_string());
                    repo.add_bundled(bundled, &name)?;
                    print_bundled(&e, &name);
                }
            }
        }
        Cmd::Update { name: Some(name), dry_run: true, .. } => {
            let targets = repo.update_targets(Some(&name), true)?;
            print_import_changes(&name, sk1ll_core::imports::fetch_changes(repo.root(), targets, None)?);
        }
        Cmd::Update { name, force, .. } => {
            for r in repo.update(name.as_deref(), force)? {
                print_update(&r.name, r.old_commit.as_deref(), &r.new_commit, r.changed);
            }
            println!("review with `git diff imported/`, then commit");
        }
        Cmd::Remove { name } => {
            repo.remove_import(&name)?;
            println!("removed {name}");
        }
        Cmd::Fork { name } => {
            repo.fork(&name)?;
            println!("moved {name} to personal/");
        }
        Cmd::Status => {
            for s in repo.import_status()? {
                let state = match s.state {
                    ImportState::Clean => "clean",
                    ImportState::Modified => "modified locally",
                    ImportState::Missing => "not vendored (run `sk1ll update`)",
                };
                println!("import {:<24} {}", s.name, state);
            }
            print_plan(&sync::plan(&repo, &machine()?.harnesses(), &assignment()?, PlanOptions::default())?);
        }
        Cmd::Sync { dry_run, force } => {
            let harnesses = machine()?.harnesses();
            // Recovery moves and deletes; only a real sync, holding the lock, may run it.
            if !dry_run && let Some(lock) = &lock {
                sync::recover(lock, &harnesses)?;
            }
            // An import an interrupted update set aside is put back first, so
            // the sync doesn't remove it from the harnesses.
            if !dry_run {
                for name in repo.recover_imports()? {
                    eprintln!("restored import {name}, which an interrupted update had set aside");
                }
            }
            let actions = sync::plan(&repo, &harnesses, &assignment()?, PlanOptions { force })?;
            print_plan(&actions);
            if dry_run {
                return Ok(());
            }
            let mut failed = 0;
            for a in &actions {
                if let Err(e) = sync::apply(&repo, a) {
                    eprintln!("error: {}: {e}", label(a));
                    failed += 1;
                }
            }
            if failed > 0 {
                bail!("{failed} action(s) failed");
            }
        }
        Cmd::Render { harness } => {
            let files = sync::render_instructions(&repo, &machine()?.harnesses(), &assignment()?, &harness)?;
            if files.is_empty() {
                eprintln!("no instructions selected for `{harness}`");
            }
            for f in files {
                println!("==> {} <==\n{}\n", f.path.display(), f.content.trim_end());
            }
        }
        Cmd::Harnesses { .. } => unreachable!(),
        Cmd::Draft => {
            let draft = repo.draft()?;
            if draft.is_empty() {
                println!("no draft: on the baseline");
            }
            for d in draft {
                println!("{} {}", change_sym(d.change), d.item);
            }
        }
        Cmd::Diff { item } => {
            let item = ItemRef::resolve(&item, &repo.draft()?)?;
            print!("{}", repo.diff(&item)?);
        }
        Cmd::Commit { items, message, author, push } => {
            let draft = repo.draft()?;
            let items = items.iter().map(|i| ItemRef::resolve(i, &draft)).collect::<Result<Vec<_>, _>>()?;
            let machine = machine()?;
            let message = message.unwrap_or_else(|| default_commit_message(&items, &machine.id));
            let author = match author {
                Some(a) => Some(a),
                None if !repo.has_git_identity()? => Some(machine_author(&machine.id)),
                None => None,
            };
            let sha = repo.commit(&items, &message, author.as_deref())?;
            println!("committed {} as {}", names(&items), short(&sha));
            if push {
                repo.push()?;
                println!("pushed to origin");
            } else {
                println!("run `sk1ll push` to share it with the fleet");
            }
        }
        Cmd::Push => {
            repo.push()?;
            println!("pushed to origin");
        }
        Cmd::Restore { items, all, yes } => {
            if all {
                let draft = repo.draft()?;
                if draft.is_empty() {
                    println!("no draft to restore");
                    return Ok(());
                }
                for d in &draft {
                    println!("{} {}", change_sym(d.change), d.item);
                }
                if !yes && !confirm(&format!("discard all {} draft item(s)?", draft.len()))? {
                    bail!("aborted");
                }
                repo.restore_all()?;
                println!("draft restored to the baseline");
            } else {
                let draft = repo.draft()?;
                let items = items.iter().map(|i| ItemRef::resolve(i, &draft)).collect::<Result<Vec<_>, _>>()?;
                repo.restore(&items)?;
                println!("restored {}", names(&items));
            }
            println!("run `sk1ll sync` to apply");
        }
        Cmd::Pull => match repo.pull()? {
            PullOutcome::UpToDate => println!("already on the latest baseline"),
            PullOutcome::Updated { from, to } => {
                println!("baseline {} -> {}; run `sk1ll sync` to apply", short(&from), short(&to));
            }
            PullOutcome::Blocked { baseline, items } => bail!(
                "blocked: baseline {} clashes with {}; commit or restore them, then pull again",
                short(&baseline),
                names(&items)
            ),
        },
        Cmd::Adopt { harness, name } => {
            let harnesses = machine()?.harnesses();
            if !harnesses.contains_key(&harness) {
                bail!("unknown harness `{harness}` on this machine (see `sk1ll harnesses`)");
            }
            let unmanaged = sync::unmanaged::discover(&repo, &harnesses)?
                .iter()
                .any(|s| s.name == name && s.variants.iter().any(|v| v.locations.iter().any(|l| l.harness == harness)));
            let adopted = if unmanaged { None } else { sync::unmanaged::adopted(&repo, &harnesses, &harness, &name)? };
            if unmanaged || adopted.is_some() {
                fleet::require_standalone_adoption(daemon_state.as_deref(), &repo)?;
                let adoption = match adopted {
                    Some(adoption) => adoption,
                    None => sync::unmanaged::select(&repo, &harnesses, &harness, &name)?,
                };
                sync::unmanaged::publish(&repo, &adoption)?;
                let id = assignment_item(&repo, Kind::Skill, &name)?;
                let mut assignments = assignment()?;
                for location in &adoption.locations {
                    assignments.set_skill(&location.harness, &id, true);
                }
                assignments.save(&assignment_path)?;
                sync::unmanaged::complete(&repo, &harnesses, &adoption)?;
                for action in sync::plan(&repo, &harnesses, &assignments, PlanOptions::default())? {
                    sync::apply(&repo, &action)?;
                }
                println!("adopted {name} from {harness} into the draft; review with `sk1ll diff {name}`");
                return Ok(());
            }
            let actions = sync::plan(&repo, &harnesses, &assignment()?, PlanOptions::default())?;
            let file = format!("{name}.md");
            let action = actions
                .iter()
                .filter(|a| a.harness == harness && a.kind == ActionKind::Conflict)
                .find(|a| match &a.item {
                    Item::Skill { name: n, .. } => *n == name,
                    Item::Rule { file: f, .. } => *f == name || *f == file,
                    Item::Instructions { rules, .. } => rules.contains(&name),
                })
                .with_context(|| format!("no conflict for `{name}` in `{harness}` (see `sk1ll status`)"))?;
            sync::adopt(&repo, action)?;
            println!("adopted {name} from {harness} into the draft; review with `sk1ll diff {name}`");
        }
        Cmd::Assign { harness, kind, name, state, default_skills, default_rules, all } => {
            if !machine()?.harnesses().contains_key(&harness) {
                bail!("unknown harness `{harness}` on this machine (see `sk1ll harnesses`)");
            }
            let mut a = assignment()?;
            if let Some(s) = all {
                a.set_harness(&harness, s.on());
                println!("{harness}: skills and rules {}, and both defaults", s.label());
            }
            if let Some((kind, name, state)) = assign_target(kind, name, state, default_skills, default_rules, all)?
                && let Some(id) = resolve_assignment_item(&repo, kind, &name, state)?
            {
                match kind {
                    Kind::Skill => a.set_skill(&harness, &id, state.on()),
                    Kind::Rule => a.set_rule(&harness, &id, state.on()),
                }
                println!("{harness}: {} {name} {}", kind.label(), state.label());
            }
            if let Some(s) = default_skills {
                let names = repo.skills()?.into_iter().map(|s| s.id).collect::<Vec<_>>();
                a.materialize_skills(&harness, &names);
                a.set_new_skills_default(&harness, s.on());
                println!("{harness}: skills default {}", s.label());
            }
            if let Some(s) = default_rules {
                let names = repo.rules()?.into_iter().map(|r| r.id).collect::<Vec<_>>();
                a.materialize_rules(&harness, &names);
                a.set_new_rules_default(&harness, s.on());
                println!("{harness}: rules default {}", s.label());
            }
            a.save(&assignment_path)?;
            println!("run `sk1ll sync` to apply");
        }
    }
    Ok(())
}

/// Takes the engine lock for a direct change; fails while a daemon (or
/// another sk1ll command) holds it. A repo outside git has no lock (and no
/// daemon can manage it).
fn lock_direct(repo: &Path) -> Result<Option<EngineLock>> {
    let absolute = std::path::absolute(repo)?;
    let existing = absolute.ancestors().find(|p| p.is_dir()).context("repo has no existing ancestor")?;
    let out =
        std::process::Command::new("git").arg("-C").arg(existing).args(["rev-parse", "--show-toplevel"]).output()?;
    if !out.status.success() {
        return Ok(None);
    }
    Ok(Some(EngineLock::acquire(existing)?))
}

/// What `assign` toggles: an item (`None` when only defaults, or `--all`, change).
fn assign_target(
    kind: Option<Kind>,
    name: Option<String>,
    state: Option<Switch>,
    default_skills: Option<Switch>,
    default_rules: Option<Switch>,
    all: Option<Switch>,
) -> Result<Option<(Kind, String, Switch)>> {
    match (kind, name, state) {
        (Some(kind), Some(name), Some(state)) => Ok(Some((kind, name, state))),
        (None, None, None) if default_skills.is_some() || default_rules.is_some() || all.is_some() => Ok(None),
        _ => bail!(
            "usage: sk1ll assign <harness> <skill|rule> <name> <on|off>, \
             or sk1ll assign <harness> --default-skills|--default-rules|--all <on|off>"
        ),
    }
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::Skill => "skill",
            Self::Rule => "rule",
        }
    }
}

fn find_assignment_item(repo: &Repo, kind: Kind, name: &str) -> Result<Option<sk1ll_core::ItemId>> {
    let item = match kind {
        Kind::Skill => ItemRef::Skill(name.into()),
        Kind::Rule => ItemRef::Rule(name.into()),
    };
    Ok(repo.item_ids()?.into_iter().find_map(|(candidate, id)| (candidate == item).then_some(id)))
}

fn assignment_item(repo: &Repo, kind: Kind, name: &str) -> Result<sk1ll_core::ItemId> {
    find_assignment_item(repo, kind, name)?.with_context(|| format!("no {} named `{name}` in the repo", kind.label()))
}

fn resolve_assignment_item(repo: &Repo, kind: Kind, name: &str, state: Switch) -> Result<Option<sk1ll_core::ItemId>> {
    if state.on() {
        let id = find_assignment_item(repo, kind, name)?;
        if id.is_none() {
            eprintln!("warning: no {} named `{name}` in the repo", kind.label());
        }
        Ok(id)
    } else {
        assignment_item(repo, kind, name).map(Some)
    }
}

fn print_import_changes(name: &str, changes: sk1ll_core::imports::ImportChanges) {
    println!("{name}: {} → {}", short(&changes.locked), short(&changes.upstream));
    println!("Upstream commits:");
    if changes.commits.is_empty() {
        println!("  None");
    }
    for commit in changes.commits {
        println!("  {} {} {}", commit.sha, commit.date, commit.subject);
    }
    if changes.truncated {
        println!("  Showing the first 50 commits");
    }
    println!("Changed files:");
    if changes.files.is_empty() {
        println!("  None");
    }
    for file in changes.files {
        println!("  {file:?}");
    }
}

fn print_update(name: &str, old: Option<&str>, new: &str, changed: bool) {
    match (old, changed) {
        (_, false) => println!("{name:<24} up to date"),
        (Some(old), true) => println!("{name:<24} {} -> {}", short(old), short(new)),
        (None, true) => println!("{name:<24} vendored @ {}", short(new)),
    }
}

fn change_sym(change: Change) -> &'static str {
    match change {
        Change::Added => "A",
        Change::Modified => "M",
        Change::Deleted => "D",
    }
}

fn names(items: &[ItemRef]) -> String {
    items.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
}

fn confirm(question: &str) -> Result<bool> {
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

fn print_plan(actions: &[Action]) {
    if actions.is_empty() {
        println!("harnesses in sync");
    }
    for a in actions {
        let sym = match a.kind {
            ActionKind::Create => "+",
            ActionKind::Update => "~",
            ActionKind::Remove => "-",
            ActionKind::Conflict => "!",
        };
        let reason = a.reason.as_deref().map(|r| format!("  ({r})")).unwrap_or_default();
        println!("{sym} {}{reason}", label(a));
    }
}

fn label(a: &Action) -> String {
    match &a.item {
        Item::Skill { name, .. } => format!("{:<10} {name}", a.harness),
        Item::Instructions { rules, .. } => {
            format!("{:<10} instructions [{}] -> {}", a.harness, rules.join(", "), a.dest.display())
        }
        Item::Rule { .. } => format!("{:<10} rule -> {}", a.harness, a.dest.display()),
    }
}

/// Fetching a source failed: keep git's message and say what to check.
/// `sk1ll add` of a link to a skill bundled with sk1ll whose fetch failed:
/// the bundled skill to add instead, and the fetch error to report. Any other
/// failure, or a link narrowed with `--path` or `--ref`, is returned as is.
fn bundled_fallback(link: &str, narrowed: bool, e: anyhow::Error) -> Result<(&'static str, anyhow::Error)> {
    let msg = e.to_string();
    let fetch_failed = msg.contains("`git clone ") || msg.contains("`git fetch ");
    match sk1ll_core::bundled::for_link(link) {
        Some(bundled) if fetch_failed && !narrowed => Ok((bundled, e)),
        _ => Err(e),
    }
}

fn print_bundled(fetch_error: &anyhow::Error, name: &str) {
    let reason = fetch_error.to_string();
    eprintln!("could not fetch the link: {}", reason.lines().next().unwrap_or_default());
    println!("added the copy bundled with sk1ll {} as the personal skill {name}", sk1ll_core::VERSION);
    println!(
        "it won't follow upstream; to import it instead, delete personal/{name} and run `sk1ll add` again once the link works"
    );
}

fn fetch_hint(e: impl Into<anyhow::Error>) -> anyhow::Error {
    const HINT: &str = "Check the repository URL and your git credentials (SSH agent / credential helper).";
    let e = e.into();
    let msg = e.to_string();
    if (msg.contains("`git clone ") || msg.contains("`git fetch ")) && !msg.contains(HINT) {
        return anyhow::anyhow!("{msg}\n{HINT}");
    }
    e
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(10)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestRepo(PathBuf);

    impl TestRepo {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "sk1ll-cli-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed),
            ));
            Repo::init(&path).unwrap();
            git(&path, &["config", "user.name", "Test"]);
            git(&path, &["config", "user.email", "test@example.com"]);
            git(&path, &["add", "."]);
            git(&path, &["commit", "-qm", "seed"]);
            Self(path)
        }

        /// Runs a command; no daemon (a socket path that doesn't exist).
        fn run(&self, args: &[&str]) -> Result<()> {
            self.run_on(&self.0.join(".git/no-daemon.sock"), args)
        }

        /// Runs a command with the daemon's socket at `socket`.
        fn run_on(&self, socket: &Path, args: &[&str]) -> Result<()> {
            let cli = Cli::try_parse_from(
                [
                    "sk1ll",
                    "--repo",
                    self.0.to_str().unwrap(),
                    "--machine-config",
                    self.0.join("machine.toml").to_str().unwrap(),
                    "--assignment",
                    self.0.join("assignment.json").to_str().unwrap(),
                    "--daemon-socket",
                    socket.to_str().unwrap(),
                    "--daemon-state",
                    self.0.join(".git/daemon.json").to_str().unwrap(),
                ]
                .into_iter()
                .chain(args.iter().copied()),
            )?;
            run(cli)
        }

        /// A personal skill `pdf` and a harness `h` (never synced to).
        fn with_skill(self) -> Self {
            fs::create_dir(self.0.join("personal/pdf")).unwrap();
            fs::write(self.0.join("personal/pdf/SKILL.md"), "PDFs.").unwrap();
            fs::write(self.0.join("machine.toml"), "id = 'test'\n[harnesses.h]\nskills = '/unused'\n").unwrap();
            self
        }

        #[cfg(unix)]
        fn joined(&self, server_url: &str) {
            sk1ll_daemon::DaemonState {
                machine_id: "test".into(),
                server_url: server_url.into(),
                token: "tok".into(),
                repo: self.0.clone(),
                revoked: false,
            }
            .save(&self.0.join(".git/daemon.json"))
            .unwrap();
        }
    }

    impl Drop for TestRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Disables every preset: they would find the developer's real harnesses.
    fn no_presets() -> String {
        sk1ll_core::machine::presets().map(|p| format!("[harnesses.{p}]\nenabled = false\n")).collect()
    }

    fn git(root: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", if cfg!(windows) { "NUL" } else { "/dev/null" })
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }

    #[test]
    fn import_dry_run_requires_a_name_and_rejects_force() {
        assert!(Cli::try_parse_from(["sk1ll", "update", "--dry-run"]).is_err());
        assert!(Cli::try_parse_from(["sk1ll", "update", "pdf", "--dry-run", "--force"]).is_err());
        let cli = Cli::try_parse_from(["sk1ll", "update", "pdf", "--dry-run"]).unwrap();
        assert!(matches!(cli.cmd, Cmd::Update { name: Some(name), dry_run: true, force: false } if name == "pdf"));
    }

    #[test]
    #[cfg(unix)]
    fn direct_fleet_assignment_fails_when_the_server_is_unreachable() {
        let repo = TestRepo::new().with_skill();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        repo.joined(&url);
        sk1ll_daemon::state::save_snapshot(
            &repo.0.join(".git/daemon-assignment.json"),
            &sk1ll_proto::AssignmentSnapshot::default(),
        )
        .unwrap();
        let err = repo.run(&["assign", "h", "skill", "pdf", "on"]).expect_err("fleet assignment must reach the server");
        assert!(err.to_string().contains("server unreachable"), "{err}");
        assert!(!repo.0.join("assignment.json").exists(), "the mirror cannot record an unaccepted change");
        assert_eq!(
            sk1ll_daemon::state::load_snapshot(&repo.0.join(".git/daemon-assignment.json")).unwrap().unwrap().revision,
            0
        );
    }

    #[test]
    #[cfg(unix)]
    fn direct_sync_uses_the_authoritative_assignment_and_requires_a_snapshot() {
        let repo = TestRepo::new().with_skill();
        let skills = repo.0.join(".git/harness/skills");
        fs::write(
            repo.0.join("machine.toml"),
            format!("id = 'test'\n[harnesses.h]\nskills = '{}'\nmode = 'copy'\n{}", skills.display(), no_presets()),
        )
        .unwrap();
        sk1ll_daemon::DaemonState {
            machine_id: "test".into(),
            server_url: "http://127.0.0.1:1".into(),
            token: "tok".into(),
            repo: repo.0.clone(),
            revoked: false,
        }
        .save(&repo.0.join(".git/daemon.json"))
        .unwrap();
        let mut assignment = Assignment::default();
        assignment.set_skill("h", &sk1ll_core::ItemId::derived("skill", "pdf"), false);
        sk1ll_daemon::state::save_snapshot(
            &repo.0.join(".git/daemon-assignment.json"),
            &sk1ll_proto::AssignmentSnapshot { revision: 7, assignment, registration: None },
        )
        .unwrap();
        repo.run(&["sync"]).unwrap();
        assert!(!skills.join("pdf").exists(), "a missing mirror must not enable a disabled skill");
        Assignment::default().save(&repo.0.join("assignment.json")).unwrap();
        repo.run(&["sync"]).unwrap();
        assert!(!skills.join("pdf").exists(), "a stale mirror must not override the snapshot");
        fs::remove_file(repo.0.join(".git/daemon-assignment.json")).unwrap();
        let err = repo.run(&["sync"]).expect_err("a joined machine needs its authoritative assignment");
        assert!(err.to_string().contains("no authoritative assignment"), "{err}");
        assert!(!skills.join("pdf").exists());
    }

    /// The daemon's message may already carry the hint; it's added once.
    #[test]
    fn only_a_failed_fetch_of_the_bundled_link_falls_back() {
        let link = sk1ll_core::bundled::SK1LL_SKILL_LINK;
        let fetch = || anyhow::anyhow!("`git clone --quiet -- {link} /c` failed: Repository not found");
        assert_eq!(bundled_fallback(link, false, fetch()).unwrap().0, "sk1ll");
        assert!(bundled_fallback(link, true, fetch()).is_err(), "--path or --ref asks for that exact source");
        assert!(bundled_fallback(link, false, anyhow::anyhow!("import `sk1ll` already exists")).is_err());
        assert!(bundled_fallback("https://github.com/o/r", false, fetch()).is_err());
    }

    #[test]
    fn fetch_hint_is_added_once_and_only_to_fetch_failures() {
        let failed = "`git clone --quiet -- https://x.invalid/r.git /c` failed: could not resolve host";
        let once = fetch_hint(anyhow::anyhow!("{failed}")).to_string();
        assert!(once.starts_with(failed) && once.ends_with("(SSH agent / credential helper)."), "{once}");
        assert_eq!(fetch_hint(anyhow::anyhow!("{once}")).to_string(), once);
        let other = "import `pdf` already exists";
        assert_eq!(fetch_hint(anyhow::anyhow!("{other}")).to_string(), other);
    }

    #[test]
    fn init_takes_the_existing_repos_engine_lock_before_writing() {
        let repo = TestRepo::new();
        fs::remove_file(repo.0.join("sk1ll.toml")).unwrap();
        let held = EngineLock::acquire(&repo.0).unwrap();
        for target in [repo.0.clone(), repo.0.join("new/nested")] {
            let err = repo.run(&["init", target.to_str().unwrap()]).expect_err("init must hold the engine lock");
            assert!(err.to_string().contains("another sk1ll process"), "{err}");
            assert!(!target.join("sk1ll.toml").exists());
        }
        assert!(!repo.0.join("new").exists(), "even directory creation waits for the lock");
        drop(held);
        repo.run(&["init"]).unwrap();
        assert!(repo.0.join("sk1ll.toml").is_file());
        let fresh = repo.0.with_extension("fresh");
        repo.run(&["init", fresh.to_str().unwrap()]).unwrap();
        fs::remove_dir_all(fresh).unwrap();
    }

    #[test]
    fn assignment_defaults_preserve_existing_items() {
        let repo = TestRepo::new().with_skill();
        fs::write(repo.0.join("instructions/git.md"), "Git.").unwrap();
        repo.run(&["assign", "h", "--default-skills", "off", "--default-rules", "off"]).unwrap();
        let a = Assignment::load(&repo.0.join("assignment.json")).unwrap();
        assert!(a.skill_enabled("h", &sk1ll_core::ItemId::derived("skill", "pdf")));
        assert!(a.rule_enabled("h", &sk1ll_core::ItemId::derived("rule", "git")));
        assert!(
            !a.skill_enabled("h", &sk1ll_core::ItemId::derived("skill", "later"))
                && !a.rule_enabled("h", &sk1ll_core::ItemId::derived("rule", "later"))
        );
        repo.run(&["assign", "h", "skill", "pdf", "off", "--default-skills", "on"]).unwrap();
        let a = Assignment::load(&repo.0.join("assignment.json")).unwrap();
        assert!(!a.skill_enabled("h", &sk1ll_core::ItemId::derived("skill", "pdf")));
    }

    /// Without a daemon, a change holds the engine lock, so it fails while
    /// another process (normally the daemon) holds it; reads don't need it.
    #[test]
    fn direct_changes_take_the_engine_lock() {
        let repo = TestRepo::new().with_skill();
        let held = EngineLock::acquire(&repo.0).unwrap();
        let changes: [&[&str]; 11] = [
            &["sync"],
            &["assign", "h", "skill", "pdf", "off"],
            &["push"],
            &["pull"],
            &["commit", "pdf"],
            &["restore", "pdf"],
            &["adopt", "h", "pdf"],
            &["add", "github:o/r"],
            &["update"],
            &["remove", "x"],
            &["fork", "x"],
        ];
        for args in changes {
            let err = repo.run(args).expect_err("refused while the lock is held");
            assert!(err.to_string().contains("another sk1ll process"), "{args:?}: {err}");
        }
        assert!(!repo.0.join("assignment.json").exists(), "assign changed nothing");
        let reads: [&[&str]; 5] = [&["list"], &["status"], &["draft"], &["sync", "--dry-run"], &["render", "h"]];
        for args in reads {
            repo.run(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
        }
        drop(held);
        repo.run(&["assign", "h", "skill", "pdf", "off"]).unwrap();
        assert!(repo.0.join("assignment.json").exists());
    }

    /// A dry run reads; another process (the daemon) holding the lock may be
    /// mid-swap, and its scratch looks just like a crash's leftovers.
    #[test]
    fn dry_run_leaves_another_syncs_work_in_progress_alone() {
        let repo = TestRepo::new().with_skill();
        let skills = repo.0.join(".git/harness/skills");
        fs::write(
            repo.0.join("machine.toml"),
            format!("id = 'test'\n[harnesses.h]\nskills = '{}'\nmode = 'copy'\n{}", skills.display(), no_presets()),
        )
        .unwrap();
        let stage = skills.join(".sk1ll-tmp/stage-pdf");
        fs::create_dir_all(&stage).unwrap();
        fs::write(stage.join("SKILL.md"), "being staged by the lock holder").unwrap();
        let held = EngineLock::acquire(&repo.0).unwrap();
        repo.run(&["sync", "--dry-run"]).unwrap();
        assert!(stage.join("SKILL.md").is_file(), "a dry run must not recover without the engine lock");
        drop(held);
        repo.run(&["sync", "--dry-run"]).unwrap();
        assert!(stage.join("SKILL.md").is_file(), "nor with it: a dry run changes nothing");
        repo.run(&["sync"]).unwrap();
        assert!(!stage.exists(), "a real sync, under the lock, cleans up");
        assert_eq!(fs::read_to_string(skills.join("pdf/SKILL.md")).unwrap(), "PDFs.");
    }

    #[cfg(unix)]
    #[test]
    fn fleet_copy_assignment_confirms_with_preview_preconditions() {
        use serde_json::{Value, json};
        use std::io::{BufRead, Read, Write};

        let repo = TestRepo::new();
        let socket = repo.0.join(".git/copy.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let daemon = std::thread::spawn(move || {
            let target = json!({"revision": 2, "registration": "target"});
            let source = json!({"revision": 4, "registration": "source"});
            let path = "/machines/work/assignment/copy-from";
            for (path, expected, source_expected, response) in [
                (
                    format!("{path}/preview"),
                    Value::Null,
                    Value::Null,
                    json!({"count":1,"target":target,"source":source}),
                ),
                (path.into(), target, source, json!({"revision":5,"assignment":{"harnesses":{}},"entries":[]})),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(std::time::Duration::from_secs(3))).unwrap();
                let mut reader = std::io::BufReader::new(&mut stream);
                let mut headers = String::new();
                while !headers.ends_with("\r\n\r\n") {
                    assert_ne!(reader.read_line(&mut headers).unwrap(), 0);
                }
                assert!(headers.starts_with(&format!("POST {path} HTTP/1.1\r\n")), "{headers}");
                let len = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap();
                let mut body = vec![0; len];
                reader.read_exact(&mut body).unwrap();
                assert_eq!(
                    serde_json::from_slice::<Value>(&body).unwrap(),
                    json!({
                        "source":"home","mode":"merge","harnesses":[{"from":"claude","to":"codex"}],
                        "expected":expected,"source_expected":source_expected,
                    })
                );
                let response = response.to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}",
                    response.len()
                )
                .unwrap();
            }
        });
        repo.run_on(
            &socket,
            &["fleet", "copy-assignment", "home", "work", "--merge", "--map", "claude=codex", "--yes"],
        )
        .unwrap();
        daemon.join().unwrap();
    }

    /// Only "no socket" and "connection refused" mean no daemon runs; any
    /// other failure to talk to it is reported instead of working directly.
    #[cfg(unix)]
    #[test]
    fn only_a_missing_or_refused_socket_means_no_daemon() {
        use std::io::{Read, Write};
        let repo = TestRepo::new().with_skill();
        let sock = repo.0.join(".git/daemon.sock");
        let assign = ["assign", "h", "skill", "pdf", "off"];

        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 512];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = s.read(&mut chunk).unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            s.write_all(b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 4\r\nconnection: close\r\n\r\noops")
                .unwrap();
        });
        let err = repo.run_on(&sock, &assign).expect_err("a broken daemon isn't bypassed");
        assert!(err.to_string().contains("cannot talk to the sk1ll daemon"), "{err}");
        assert!(!repo.0.join("assignment.json").exists());
        server.join().unwrap();

        // The listener is gone but its socket file stays: connection refused.
        // Other tests spawn git meanwhile, and a child forked before its exec
        // briefly holds a copy of the listening socket (CLOEXEC only closes it
        // at exec); a connect in that window is accepted, then reset. So try
        // again, for a bounded time, until no such copy is left.
        assert!(sock.exists());
        let gave_up = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while let Err(e) = repo.run_on(&sock, &assign) {
            assert!(std::time::Instant::now() < gave_up, "{e}");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(repo.0.join("assignment.json").exists());

        fs::remove_file(&sock).unwrap();
        repo.run_on(&sock, &["assign", "h", "skill", "pdf", "on"]).unwrap();
    }

    #[test]
    fn recovery_commands_work_with_damaged_configuration() {
        for deleted in [false, true] {
            for command in ["draft", "diff", "restore", "commit", "pull"] {
                let repo = TestRepo::new();
                // A local origin lets pull run through its real Git path.
                git(&repo.0, &["init", "--bare", "--quiet", ".git/test-origin.git"]);
                git(&repo.0, &["remote", "add", "origin", ".git/test-origin.git"]);
                git(&repo.0, &["push", "--quiet", "-u", "origin", "HEAD"]);
                if deleted {
                    fs::remove_file(repo.0.join("sk1ll.toml")).unwrap();
                } else {
                    fs::write(repo.0.join("sk1ll.toml"), "[broken").unwrap();
                }
                fs::write(repo.0.join("sk1ll.lock"), "[broken").unwrap();
                assert!(repo.run(&["list"]).is_err());
                let args = match command {
                    "diff" | "restore" => vec![command, "manifest"],
                    "commit" => vec![command, "manifest", "--author", "Test <test@example.com>"],
                    _ => vec![command],
                };
                assert!(repo.run(&args).is_ok(), "{command}, deleted={deleted}");
                if command == "restore" {
                    assert!(Repo::open(&repo.0).is_ok());
                } else {
                    assert!(Repo::open(&repo.0).is_err());
                }
            }
        }
    }
}
