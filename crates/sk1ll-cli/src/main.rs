//! `sk1ll`: a client of the daemon's local API (see ADR 0008). Every
//! command that reads or changes the repo, the harnesses or the Assignment
//! goes through the running daemon when it manages the repo, else through an
//! engine started in this process for the command ([`fleet::Session`]):
//! the same engine, reconcile and local API, bound to no socket.

#[cfg(not(unix))]
compile_error!("sk1ll needs Unix: it runs the daemon's engine, which uses Unix sockets and file APIs");

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use sk1ll_core::{ImportState, MachineConfig, Origin, Repo};
use sk1ll_daemon::client::{
    AddImportRequest, ChangeKind, CommitRequest, ConflictTarget, ConflictView, ContentType, CreateItemRequest,
    DaemonMode, PlacementView, PullResult, RepoContent, ResolveImportRequest, RestoreRequest, SyncRequest,
    UpdateImportsRequest,
};
use sk1ll_proto::{AssignmentChanges, AssignmentWrite, ToggleChange, ToggleKind};

use fleet::Session;

mod fleet;
mod harnesses;
mod setup;

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
    /// Give each item that shares an Item ID, or has none, a fresh one in the Draft
    RepairIds,
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
    /// List recovery entries: their id, harness, item, where the content was and where it is kept
    List,
    /// Put an entry's content back (into the repo as a Draft change, or a block file into its harness)
    Adopt {
        /// The entry's id, as `sk1ll recover list` or `sk1ll conflicts` prints it
        #[arg(value_name = "ID")]
        id: String,
    },
    /// Delete an entry and its content for good
    Discard {
        /// The entry's id, as `sk1ll recover list` or `sk1ll conflicts` prints it
        #[arg(value_name = "ID")]
        id: String,
    },
}

impl Cmd {
    /// Only reads: a preview or a listing, which changes nothing on disk,
    /// not even what a reconcile would bring in line, so it runs beside
    /// another engine (see [`fleet::Session`]). Without a daemon, the
    /// Draft, the conflicts and the recovery area are planned in memory.
    fn reads_only(&self) -> bool {
        matches!(
            self,
            Self::Status
                | Self::Render { .. }
                | Self::Sync { dry_run: true, .. }
                | Self::List { .. }
                | Self::Diff { .. }
                | Self::Edit { .. }
                | Self::Harnesses { command: None, .. }
                | Self::Draft
                | Self::Conflicts
                | Self::Recover { command: RecoverCmd::List }
        )
    }

    /// Works on a repo whose configuration is damaged: it needs only git.
    fn git_only(&self) -> bool {
        matches!(self, Self::Draft | Self::Diff { .. } | Self::Restore { .. } | Self::Commit { .. } | Self::Pull)
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
    // An empty variable counts as unset, as it does for the daemon: clap
    // would take it as an empty value. Single-threaded here.
    for (key, value) in std::env::vars_os() {
        if value.is_empty() && key.to_str().is_some_and(|k| k.starts_with("SK1LL_")) {
            // SAFETY: no other thread runs yet.
            unsafe { std::env::remove_var(&key) };
        }
    }
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
    // change the environment. The engine, in a daemon or in this process,
    // runs git without prompts and with the machine's credentials.
    let cfg = fleet::daemon_config(&cli)?;
    // SAFETY: no other thread runs yet.
    unsafe { sk1ll_daemon::prepare_process(&cfg)? };
    run(cli).map_err(repair_hint)
}

/// A duplicate Item ID is repaired with `sk1ll repair-ids`: every error
/// that names one says so.
fn repair_hint(e: anyhow::Error) -> anyhow::Error {
    let msg = format!("{e:#}");
    if msg.contains("duplicate Item ID") && !msg.contains("repair-ids") {
        return anyhow::anyhow!("{msg}\nRun `sk1ll repair-ids`, then commit the repaired items");
    }
    e
}

fn run(cli: Cli) -> Result<()> {
    if matches!(cli.cmd, Cmd::Join { .. } | Cmd::Setup | Cmd::Daemon | Cmd::Fleet { .. }) {
        return fleet::command(cli);
    }
    let repo = sk1ll_daemon::locate_repo(cli.repo.as_deref(), cli.cmd.git_only())?;
    if let Cmd::Init { path } = &cli.cmd {
        let root = path.as_deref().map_or_else(|| Ok(repo.clone()), std::path::absolute)?;
        return init(&cli, &root);
    }
    let access = if cli.cmd.reads_only() { fleet::Access::Read } else { fleet::Access::Write };
    let session = Session::open(&cli, &repo, access)?;
    match cli.cmd {
        Cmd::Harnesses { all, command } => harnesses::run(&session, all, command),
        cmd => command(&session, cmd),
    }
}

/// `sk1ll init`: creates the repo an engine then manages. A daemon waiting
/// for one at this path makes it; otherwise it is made here, under the
/// machine lock (it may write `machine.toml`) and the engine lock of the
/// repo the path is in (if it is in one).
fn init(cli: &Cli, root: &Path) -> Result<()> {
    sk1ll_core::git::require_git(sk1ll_core::git::MIN_VERSION)?;
    if let Some(repo) = Session::daemon_repo(cli)?
        && !fleet::same_path(&repo, root)
    {
        bail!(
            "the sk1ll daemon manages {} on this Machine, and one engine manages a Machine at a time; stop the \
             daemon to create a repo at {}, or select {} to work there",
            repo.display(),
            root.display(),
            repo.display()
        );
    }
    if let Some(session) = Session::daemon_for(cli, root)?
        && !session.status.repo_ready
    {
        session.block(session.client.init())?;
    } else if let Some(session) = Session::daemon_for(cli, root)? {
        // The daemon manages this repo, which exists already.
        drop(session);
        bail!("{} is already a skills repo", root.display());
    } else {
        let cfg = fleet::daemon_config(cli)?;
        let _machine = sk1ll_daemon::MachineLock::acquire(&cfg)?;
        let _lock = sk1ll_core::EngineLock::for_new_repo(root)?;
        // A machine without machine.toml is set up for the first time here:
        // it takes up the opt-in presets it finds (see MachineConfig::adopt_detected_presets).
        let machine_path = cfg.machine_config;
        if !root.join(".git").exists() && std::fs::symlink_metadata(&machine_path).is_err() {
            for warning in MachineConfig::adopt_detected_presets(&machine_path, root)? {
                eprintln!("{warning}");
            }
        }
        std::fs::create_dir_all(root)?;
        Repo::init_committed(root)?;
    }
    println!("initialized skills repo at {}", root.display());
    println!("To let agents add skills: sk1ll add https://github.com/0xV1p3r/0xSk1ll/tree/main/skills/sk1ll");
    Ok(())
}

/// Every other command, as a request to the engine.
fn command(s: &Session, cmd: Cmd) -> Result<()> {
    let c = &s.client;
    match cmd {
        Cmd::Conflicts => {
            // Their ids and names are what the next command acts on.
            for e in s.block(c.conflicts_fresh())? {
                if e.kind.as_deref() == Some("displaced") {
                    print_recovered(&e);
                } else {
                    let kind = if e.adoptable { "item" } else { "instructions" };
                    let reason = conflict_reason(&e.harness, kind, &e.name, e.reason.as_deref());
                    println!("! {:<10} {} {}{reason}", e.harness, e.name, e.dest.display());
                }
            }
        }
        Cmd::Recover { command: RecoverCmd::List } => {
            for e in s.block(c.recovered_fresh())? {
                print_recovered(&e);
            }
        }
        Cmd::Recover { command: RecoverCmd::Adopt { id } } => {
            println!("adopted into {}", s.block(c.adopt_displaced(&id))?.display());
        }
        Cmd::Recover { command: RecoverCmd::Discard { id } } => s.block(c.discard_displaced(&id))?,
        Cmd::List { unmanaged: true, .. } => {
            for skill in s.block(c.skills())?.into_iter().filter(|e| e.kind == ContentType::Unmanaged) {
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
        Cmd::List { unmanaged: false, json: true } => {
            let repo = s.block(c.repo(false))?;
            let content = serde_json::json!({ "skills": repo.skills, "rules": repo.rules });
            println!("{}", serde_json::to_string_pretty(&content)?);
        }
        Cmd::List { unmanaged: false, json: false } => {
            let repo = s.block(c.repo(false))?;
            for skill in repo.skills {
                let origin = match skill.origin {
                    Origin::Personal if skill.shadows_import => "personal*",
                    Origin::Personal => "personal",
                    Origin::Imported => "imported",
                };
                let id = if skill.id.is_none() { " [unassigned id]" } else { "" };
                println!("{:<24} {:<10} {}{id}", skill.name, origin, skill.description.unwrap_or_default());
            }
            for rule in repo.rules {
                let scope = rule.harnesses.map(|t| format!("[{}] ", t.join(", "))).unwrap_or_default();
                let id = if rule.id.is_none() { " [unassigned id]" } else { "" };
                println!("{:<24} {:<10} {scope}{}{id}", rule.name, "instr", rule.description.unwrap_or_default());
            }
        }
        Cmd::Edit { name } => {
            let path = s.block(c.editable(&name))?;
            let editor = std::env::var("VISUAL")
                .ok()
                .filter(|e| !e.trim().is_empty())
                .or_else(|| std::env::var("EDITOR").ok().filter(|e| !e.trim().is_empty()))
                .context("set $VISUAL or $EDITOR to edit a Skill or Rule")?;
            let status = std::process::Command::new("sh")
                .args(["-c", &format!("{editor} \"$1\""), "sk1ll-edit"])
                .arg(&path)
                .status()?;
            if !status.success() {
                bail!("editor exited with {status}");
            }
        }
        Cmd::Add { source, path, git_ref, list: true, .. } => {
            let request = ResolveImportRequest {
                link: source,
                path: path.unwrap_or_default(),
                git_ref: git_ref.unwrap_or_default(),
            };
            let r = s.block(c.resolve_import(&request)).map_err(fetch_hint)?;
            println!("{} @ {} ({})", r.source, r.git_ref, short(&r.commit));
            if r.candidates.is_empty() {
                println!("no SKILL.md found");
            }
            for candidate in r.candidates {
                let path = if candidate.path.is_empty() { "." } else { &candidate.path };
                let taken = if candidate.taken { "taken" } else { "" };
                let description = candidate.description.unwrap_or_default();
                println!("{path:<32} {:<24} {taken:<5} {description}", candidate.default_name);
                println!("  {} bytes total", candidate.total_bytes);
                for warning in candidate.warnings {
                    let kinds: Vec<_> = warning.kinds.into_iter().map(|kind| kind.label()).collect();
                    println!("  {:?}: {}", warning.path, kinds.join(", "));
                }
            }
        }
        Cmd::Add { source, path, git_ref, name, list: false } => {
            // An empty name is the skill's default name, decided by the engine.
            let narrowed = path.is_some() || git_ref.is_some();
            let req = AddImportRequest { name: name.clone().unwrap_or_default(), source, path, git_ref, commit: None };
            match s.block(c.add_import(&req)).map_err(fetch_hint) {
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
                    s.block(c.create_item(&create))?;
                    print_bundled(&e, &name);
                }
            }
        }
        Cmd::Update { name: Some(name), dry_run: true, .. } => {
            print_import_changes(&name, s.block(c.import_changes(&name, None))?);
        }
        Cmd::Update { name, force, .. } => {
            for r in s.block(c.update_imports(&UpdateImportsRequest { name, force, expected: None }))? {
                print_update(&r.name, r.old_commit.as_deref(), &r.new_commit, r.changed);
            }
            println!("review with `sk1ll diff`, then commit");
        }
        Cmd::Remove { name } => {
            s.block(c.remove_import(&name))?;
            println!("removed {name}");
        }
        Cmd::Fork { name } => {
            s.block(c.fork(&name))?;
            println!("moved {name} to personal/");
        }
        Cmd::Status => {
            let repo = s.block(c.repo(false))?;
            if repo.pull_pending {
                println!("pull pending. Run `sk1ll pull` to finish");
                return Ok(());
            }
            print_machine_state(&s.status);
            for import in &repo.imports {
                let state = match import.state {
                    ImportState::Clean => "clean",
                    ImportState::Modified => "modified locally",
                    ImportState::Missing => "not vendored (run `sk1ll update`)",
                };
                println!("import {:<24} {}", import.name, state);
            }
            print_plan(&s.block(c.preview_sync())?.actions.unwrap_or_default());
        }
        Cmd::Sync { dry_run: true, force } => {
            let preview = s.block(c.sync(&SyncRequest { dry_run: true, force, ..Default::default() }))?;
            print_plan(&preview.actions.unwrap_or_default());
        }
        Cmd::Sync { dry_run: false, force } => {
            let result = s.block(c.sync(&SyncRequest { force, ..Default::default() }))?;
            let (kept, conflicts): (Vec<_>, Vec<_>) =
                result.conflicts.iter().partition(|x| x.kind.as_deref() == Some("displaced"));
            if conflicts.is_empty() && result.errors.is_empty() {
                println!("harnesses in sync");
            }
            let mut adopts = Vec::new();
            for x in &conflicts {
                let kind = match (x.items.first(), x.adoptable) {
                    (_, false) if x.name == "instructions" => "instructions",
                    (Some(sk1ll_core::ItemRef::Rule(_)), _) => "rule",
                    _ => "skill",
                };
                let reason = conflict_reason(&x.harness, kind, &x.name, x.reason.as_deref());
                adopts.extend(adopt_command(&x.harness, kind, &x.name, x.reason.as_deref()));
                let name = if kind == "instructions" { names_only(&x.items) } else { x.name.clone() };
                println!("! {:<10} {}{reason}", x.harness, placement_what(kind, &name, &x.dest));
            }
            for x in &kept {
                println!(
                    "kept in recovery as {}: the edit to {} (`sk1ll recover adopt` or `sk1ll recover discard` it)",
                    x.id.as_deref().unwrap_or_default(),
                    x.dest.display()
                );
            }
            for error in &result.errors {
                eprintln!("error: {error}");
            }
            if !result.errors.is_empty() {
                bail!("{} failed", plural(result.errors.len(), "action", "actions"));
            }
            match (&conflicts[..], &adopts[..]) {
                ([], _) => {}
                ([_], [adopt]) => bail!(
                    "1 conflict left alone: `{adopt}` the edit into the repo, or `sk1ll sync --force` to overwrite it \
                     (it is kept in recovery)"
                ),
                (conflicts, _) => bail!(
                    "{} left alone: resolve each as its line above says, or `sk1ll sync --force` to overwrite the \
                     edits (each is kept in recovery)",
                    plural(conflicts.len(), "conflict", "conflicts")
                ),
            }
        }
        Cmd::Render { harness } => {
            let files = s.block(c.render_instructions(&harness))?;
            if files.is_empty() {
                eprintln!("no instructions selected for `{harness}`");
            }
            for f in files {
                println!("==> {} <==\n{}\n", f.path.display(), f.content.trim_end());
            }
        }
        Cmd::Draft => {
            let draft = s.block(c.draft())?;
            if draft.is_empty() {
                println!("no draft: on the baseline");
            }
            for d in draft {
                println!("{} {}", change_sym(d.change), d.item);
            }
        }
        Cmd::Diff { item } => print!("{}", s.block(c.diff(&item))?.diff),
        Cmd::Commit { items, message, author, push } => {
            let resp = s.block(c.commit(&CommitRequest { items, message, author }))?;
            println!("committed {} as {}", names(&resp.items), short(&resp.sha));
            if s.status.mode == DaemonMode::Standalone {
                if push {
                    s.block(c.push())?;
                    println!("pushed to origin");
                } else if s.status.has_origin {
                    println!("run `sk1ll push` to push it to origin");
                }
                return Ok(());
            }
            let later = match s.in_process() {
                true => "run `sk1ll push` once the server is reachable, or start the daemon to push it",
                false => "the daemon pushes it when it can",
            };
            match (resp.pushed, resp.push_error) {
                (true, _) => println!("pushed to the server"),
                (false, Some(_)) if resp.uncommitted => bail!(
                    "the server's baseline conflicts with this commit, so the pull before the push turned it back into \
                     Draft changes (nothing is lost): {}",
                    sk1ll_daemon::client::blocked_hint(&resp.items)
                ),
                (false, Some(e)) if resp.push_refused => bail!("not pushed: {e}"),
                (false, Some(e)) if push => bail!("not pushed yet ({e}); {later}"),
                (false, Some(e)) => println!("not pushed yet ({e}); {later}"),
                (false, None) => println!("not pushed yet; {later}"),
            }
        }
        Cmd::RepairIds => {
            println!(
                "gave {} item(s) a fresh Item ID of their own in the Draft; commit them",
                s.block(c.repair_ids())?
            );
        }
        Cmd::Push => match s.block(c.push())?.pushed {
            true => {
                println!("pushed to {}", if s.status.mode == DaemonMode::Standalone { "origin" } else { "the server" })
            }
            false => println!("nothing to push"),
        },
        Cmd::Restore { items, all, yes } => {
            if all {
                // What is confirmed is what gets restored: as of now.
                let draft = s.block(c.draft_fresh())?;
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
            }
            let resp = s.block(c.restore(&RestoreRequest { items, all }))?;
            if all {
                println!("draft restored to the baseline");
            } else if resp.discarded.is_empty() {
                println!("nothing was restored");
            } else {
                println!("restored {}", names(&resp.discarded));
            }
            if !resp.kept.is_empty() {
                println!("kept your version of what the pull is blocked on in recovery: {}", resp.kept.join(" "));
                println!("after `sk1ll pull`, bring it back as Draft with `sk1ll recover adopt <id>`");
            }
        }
        Cmd::Pull => match s.block(c.pull())? {
            PullResult::UpToDate => println!("already on the latest baseline"),
            PullResult::Updated { from, to } => {
                println!("baseline {} -> {}", from.as_deref().map_or("(none)", short), short(&to));
            }
            PullResult::Blocked { baseline, items, pending: true, .. } => bail!(
                "pull pending at {}: {} changed while it was updating them. Run `sk1ll restore {}` (your version is \
                 kept in recovery), then `sk1ll pull` to finish, then `sk1ll recover adopt <id>` for each id the \
                 restore printed",
                short(&baseline),
                names(&items),
                spaced(&items)
            ),
            PullResult::Blocked { baseline, items, pending: false, uncommitted } => {
                if !uncommitted.is_empty() {
                    eprintln!(
                        "turned {} unpushed commit(s) back into Draft, nothing lost: they conflict with baseline {}",
                        uncommitted.len(),
                        short(&baseline)
                    );
                    for commit in &uncommitted {
                        eprintln!("  {commit}");
                    }
                }
                bail!("blocked: {}", sk1ll_daemon::client::blocked_hint(&items))
            }
        },
        Cmd::Adopt { harness, name } => {
            require_harness(s, &harness)?;
            s.block(c.adopt(&ConflictTarget { harness: harness.clone(), name: name.clone() }))?;
            println!("adopted {name} from {harness} into the draft; review with `sk1ll diff {name}`");
        }
        Cmd::Assign { harness, kind, name, state, default_skills, default_rules, all } => {
            assign(s, &harness, kind, name, state, default_skills, default_rules, all)?;
        }
        Cmd::Init { .. }
        | Cmd::Harnesses { .. }
        | Cmd::Join { .. }
        | Cmd::Setup
        | Cmd::Daemon
        | Cmd::Fleet { .. }
        | Cmd::Completions { .. } => unreachable!(),
    }
    Ok(())
}

fn require_harness(s: &Session, harness: &str) -> Result<()> {
    if !s.block(s.client.harnesses())?.iter().any(|h| h.name == harness) {
        bail!("unknown harness `{harness}` on this machine (see `sk1ll harnesses`)");
    }
    Ok(())
}

/// `sk1ll assign`: one write where this Machine's Assignment lives.
#[allow(clippy::too_many_arguments)]
fn assign(
    s: &Session,
    harness: &str,
    kind: Option<Kind>,
    name: Option<String>,
    state: Option<Switch>,
    default_skills: Option<Switch>,
    default_rules: Option<Switch>,
    all: Option<Switch>,
) -> Result<()> {
    require_harness(s, harness)?;
    let target = assign_target(kind, name, state, default_skills, default_rules, all)?;
    let write = if let Some(state) = all {
        AssignmentWrite::Harness { harness: harness.into(), enabled: state.on() }
    } else {
        let mut changes = Vec::new();
        let toggle =
            |kind, item_id, name, on| ToggleChange { harness: harness.into(), kind, item_id, name, enabled: Some(on) };
        if let Some((kind, name, state)) = target {
            // As of now: missing Item IDs are written first.
            let repo = s.block(s.client.repo(true))?;
            if let Some(id) = resolve_assignment_item(&repo, kind, &name, state)? {
                let toggle_kind = match kind {
                    Kind::Skill => ToggleKind::Skill,
                    Kind::Rule => ToggleKind::Rule,
                };
                changes.push(toggle(toggle_kind, Some(id), Some(name), state.on()));
            }
        }
        if let Some(state) = default_skills {
            changes.push(toggle(ToggleKind::NewSkillsDefault, None, None, state.on()));
        }
        if let Some(state) = default_rules {
            changes.push(toggle(ToggleKind::NewRulesDefault, None, None, state.on()));
        }
        if changes.is_empty() {
            return Ok(());
        }
        AssignmentWrite::Changes(AssignmentChanges { changes, expected: None })
    };
    let machine = s.status.machine_id.clone();
    let update = match write {
        AssignmentWrite::Harness { harness, enabled } => s.block(s.client.set_harness_assignment(&harness, enabled))?,
        AssignmentWrite::Changes(changes) => s.block(s.client.set_assignment(&machine, &changes))?,
        AssignmentWrite::RenameHarness(_) => unreachable!(),
    };
    if !update.errors.is_empty() {
        println!("Assignment of {machine} was saved at revision {}.", update.revision);
        for error in &update.errors {
            eprintln!("{error}");
        }
        bail!("Could not apply all placements");
    }
    if let Some(state) = all {
        println!("{harness}: skills and rules {}, and both defaults", state.label());
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
    let whose = if s.status.mode == DaemonMode::Standalone { "local" } else { "fleet" };
    println!("{whose} assignment of {machine} is now revision {}", update.revision);
    Ok(())
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

/// The Item ID `assign` toggles. Turning on an item that isn't there (yet)
/// only warns; turning one off needs it.
fn resolve_assignment_item(
    repo: &RepoContent,
    kind: Kind,
    name: &str,
    state: Switch,
) -> Result<Option<sk1ll_core::ItemId>> {
    let found = match kind {
        Kind::Skill => repo.skills.iter().find(|s| s.name == name).map(|s| &s.id),
        Kind::Rule => repo.rules.iter().find(|r| r.name == name).map(|r| &r.id),
    };
    match found {
        Some(Some(id)) => Ok(Some(id.clone())),
        // The engine writes missing IDs first, except into a pending pull.
        Some(None) => {
            bail!("{} {name} has no Item ID yet; finish the pending pull with `sk1ll pull` first", kind.label())
        }
        None if state.on() => {
            eprintln!("warning: no {} named `{name}` in the repo", kind.label());
            Ok(None)
        }
        None => bail!("no {} named `{name}` in the repo", kind.label()),
    }
}

/// What stands between this Machine and the latest Baseline, as `status`
/// says it before the sync plan: Blocked (and on what), a pull to run, a
/// push the server refused.
fn print_machine_state(status: &sk1ll_daemon::client::StatusView) {
    let report = &status.report;
    if report.blocked {
        let baseline = status.server_baseline.as_deref().map(short).unwrap_or("the new one");
        println!("blocked: the pull of baseline {baseline} is pending; {}", report_hint(&report.clashes));
    } else if report.behind {
        println!("behind: a newer baseline is waiting; run `sk1ll pull`");
    }
    if let Some(reason) = &report.push_refused {
        println!("push refused by the server: {reason}");
        println!("pushing again won't help: fix what it names and commit the fix");
    }
}

fn report_hint(items: &[sk1ll_core::ItemRef]) -> String {
    sk1ll_daemon::client::blocked_hint(items)
}

/// A recovery entry: its id, Harness, item, where it was and where it is kept.
fn print_recovered(e: &ConflictView) {
    let item = e.item.as_ref().map(ToString::to_string).unwrap_or_else(|| e.name.clone());
    println!(
        "{} {} {item} {} {}{}",
        e.id.as_deref().unwrap_or_default(),
        e.harness,
        e.dest.display(),
        e.path.as_deref().unwrap_or(Path::new("")).display(),
        if e.changed { " (changed)" } else { "" }
    );
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

fn change_sym(change: ChangeKind) -> &'static str {
    match change {
        ChangeKind::Added => "A",
        ChangeKind::Modified => "M",
        ChangeKind::Deleted => "D",
    }
}

/// `items` as command arguments.
fn spaced(items: &[sk1ll_core::ItemRef]) -> String {
    items.iter().map(ToString::to_string).collect::<Vec<_>>().join(" ")
}

fn names(items: &[sk1ll_core::ItemRef]) -> String {
    items.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
}

fn confirm(question: &str) -> Result<bool> {
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

/// A sync plan: `+` create, `~` update, `-` remove, `!` a conflict left alone.
fn print_plan(actions: &[PlacementView]) {
    if actions.is_empty() {
        println!("harnesses in sync");
    }
    for a in actions {
        let sym = match a.kind.as_str() {
            "create" => "+",
            "update" => "~",
            "remove" => "-",
            _ => "!",
        };
        let reason = conflict_reason(&a.harness, &a.item, &a.name, a.reason.as_deref());
        println!("{sym} {:<10} {}{reason}", a.harness, placement_what(&a.item, &a.name, &a.dest));
    }
}

/// A conflict's reason as printed after it, `  (<reason>; <what to run>)`:
/// an edit by hand to managed content names the exact command that takes
/// it into the repo, or says where block edits go. `kind`: `skill`, `rule`
/// (`name` with or without `.md`), `instructions`, or `item` for either of
/// the first two.
fn conflict_reason(harness: &str, kind: &str, name: &str, reason: Option<&str>) -> String {
    let Some(reason) = reason else { return String::new() };
    let hint = match adopt_command(harness, kind, name, Some(reason)) {
        Some(adopt) => format!("; `{adopt}` it into the repo, or overwrite it with `sk1ll sync --force`"),
        None if kind == "instructions" && reason.ends_with(sk1ll_core::sync::EDITED) => {
            "; move the edits into instructions/, or overwrite them with `sk1ll sync --force`".into()
        }
        None => String::new(),
    };
    format!("  ({reason}{hint})")
}

/// `sk1ll adopt <harness> <name>` for a skill or rule edited by hand in a harness.
fn adopt_command(harness: &str, kind: &str, name: &str, reason: Option<&str>) -> Option<String> {
    if kind == "instructions" || !reason?.ends_with(sk1ll_core::sync::EDITED) {
        return None;
    }
    let name = if kind == "skill" { name } else { name.strip_suffix(".md").unwrap_or(name) };
    Some(format!("sk1ll adopt {} {}", fleet::shell_arg(harness), fleet::shell_arg(name)))
}

/// `n` and the noun for it: `1 conflict`, `2 conflicts`.
fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// What a placement is, as `status`, `sync --dry-run` and `sync` print it:
/// a skill by name, a rule or a block of rules with where it goes.
fn placement_what(kind: &str, name: &str, dest: &Path) -> String {
    match kind {
        "skill" => name.to_string(),
        "rule" => format!("rule {} -> {}", name.strip_suffix(".md").unwrap_or(name), dest.display()),
        _ => format!("instructions [{name}] -> {}", dest.display()),
    }
}

/// The names of `items`, comma-separated: `git, style`.
fn names_only(items: &[sk1ll_core::ItemRef]) -> String {
    items.iter().map(sk1ll_core::ItemRef::name).collect::<Vec<_>>().join(", ")
}

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

/// Fetching a source failed: keep git's message and say what to check.
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

    #[test]
    fn import_dry_run_requires_a_name_and_rejects_force() {
        assert!(Cli::try_parse_from(["sk1ll", "update", "--dry-run"]).is_err());
        assert!(Cli::try_parse_from(["sk1ll", "update", "pdf", "--dry-run", "--force"]).is_err());
        let cli = Cli::try_parse_from(["sk1ll", "update", "pdf", "--dry-run"]).unwrap();
        assert!(matches!(cli.cmd, Cmd::Update { name: Some(name), dry_run: true, force: false } if name == "pdf"));
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
    fn conflict_reasons_name_the_command_for_an_edit_by_hand() {
        let edited = format!("rule {}", sk1ll_core::sync::EDITED);
        assert_eq!(
            conflict_reason("codex", "rule", "git.md", Some(&edited)),
            format!(
                "  ({edited}; `sk1ll adopt codex git` it into the repo, or overwrite it with `sk1ll sync --force`)"
            )
        );
        let reason = conflict_reason("h", "skill", "my skill", Some(sk1ll_core::sync::EDITED));
        assert!(reason.contains("`sk1ll adopt h 'my skill'`"), "{reason}");
        let block = conflict_reason("codex", "instructions", "git", Some(sk1ll_core::sync::EDITED));
        assert!(block.contains("move the edits into instructions/") && !block.contains("adopt"), "{block}");
        let unmanaged = "existing rule file not managed by sk1ll";
        assert_eq!(conflict_reason("h", "rule", "git.md", Some(unmanaged)), format!("  ({unmanaged})"));
        assert_eq!(conflict_reason("h", "skill", "pdf", None), "");
        assert_eq!(
            (plural(1, "conflict", "conflicts"), plural(2, "entry", "entries")),
            ("1 conflict".into(), "2 entries".into())
        );
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
}
