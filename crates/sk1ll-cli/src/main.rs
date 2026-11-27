//! `sk1ll`: a client of the daemon's local API (see ADR 0008). Every
//! command that reads or changes the repo, the harnesses or the Assignment
//! goes through the running daemon when it manages the repo, else through an
//! engine started in this process for the command ([`fleet::Session`]):
//! the same engine, reconcile and local API, bound to no socket.

#[cfg(not(unix))]
compile_error!("sk1ll needs Unix: it runs the daemon's engine, which uses Unix sockets and file APIs");

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use sk1ll_core::{ImportState, MachineConfig, Origin, Repo};
use sk1ll_daemon::client::{
    AddImportRequest, ChangeKind, CommitRequest, ConflictTarget, ConflictView, ContentType, CreateItemRequest,
    DaemonMode, DeleteItemRequest, PlacementView, PullKeepRequest, PullResult, RenameItemRequest, RepoContent,
    ResolveImportRequest, RestoreRequest, SyncRequest, UpdateImportsRequest,
};
use sk1ll_proto::{AssignmentChanges, AssignmentWrite, ToggleChange, ToggleKind};

use fleet::Session;

mod fleet;
mod harnesses;
mod release_check;
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
    /// Remove an import (`sk1ll delete` deletes a personal skill or rule)
    Remove { name: String },
    /// Create a personal skill or rule in the Draft
    #[command(
        after_help = "Examples:\n  sk1ll new skill code-review --description 'Review a change'\n  sk1ll new rule commits"
    )]
    New {
        kind: Kind,
        name: String,
        /// The skill's or rule's description (its frontmatter)
        #[arg(long)]
        description: Option<String>,
    },
    /// Delete a personal skill or rule in the Draft (`sk1ll remove` removes an import)
    #[command(after_help = "Items: a skill or rule name, or skill:<name>, rule:<name>")]
    Delete {
        item: String,
        /// Delete without asking for confirmation
        #[arg(long, short)]
        yes: bool,
    },
    /// Rename a personal skill or rule in the Draft; it keeps its Item ID, and with it its Assignment
    #[command(after_help = "Items: a skill or rule name, or skill:<name>, rule:<name>")]
    Rename {
        item: String,
        #[arg(value_name = "NEW_NAME")]
        to: String,
    },
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
        #[arg(conflicts_with = "all")]
        items: Vec<String>,
        /// Commit every draft item
        #[arg(long)]
        all: bool,
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
    Pull {
        /// When Blocked: keep your version of these clashing items (each whole file, over the other change)
        #[arg(long, num_args = 1.., value_name = "ITEM", conflicts_with = "keep_all")]
        keep: Vec<String>,
        /// When Blocked: keep your version of every clashing item
        #[arg(long)]
        keep_all: bool,
    },
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
        /// Another fleet Machine's Assignment (needs `fleet` scope; default: this Machine)
        #[arg(long, value_name = "ID")]
        machine: Option<String>,
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
    /// Run the daemon in the foreground (what systemd runs)
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
    /// List recovery entries: their id, item, harness (or `pull`), age and whether they changed
    List {
        /// Also print where each entry's content was and where it is kept
        #[arg(long)]
        paths: bool,
    },
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
    /// Dismiss the notes an interrupted keep left (those with no recovery entry to adopt)
    DismissNotes,
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
                | Self::Recover { command: RecoverCmd::List { .. } }
        )
    }

    /// Works on a repo whose configuration is damaged: it needs only git.
    fn git_only(&self) -> bool {
        matches!(self, Self::Draft | Self::Diff { .. } | Self::Restore { .. } | Self::Commit { .. } | Self::Pull { .. })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
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
        // The daemon's Init ends first-run setup; it can't see this shell's
        // environment, so the config directories it points at are added first.
        harnesses::adopt_candidates(&session)?;
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
            // And the config directories this environment points at.
            let choices = MachineConfig::load(&machine_path)?.choices();
            for c in harnesses::env_candidates(&choices, root) {
                let what = harnesses::describe(&c);
                match &c.warning {
                    Some(warning) => eprintln!("{} ({what}): not added: {warning}", c.name),
                    None => match MachineConfig::write_change(&machine_path, &c.change()) {
                        Ok(_) => eprintln!("added harness {} ({what})", c.name),
                        Err(e) => eprintln!("{} ({what}): not added: {e}", c.name),
                    },
                }
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
                let kind = if e.adoptable { "item" } else { "instructions" };
                let reason = conflict_reason(&e.harness, kind, &e.name, e.reason.as_deref());
                println!("! {:<10} {} {}{reason}", e.harness, e.name, e.dest.display());
            }
            // The recovery entries that hold a change need a decision too.
            for e in s.block(c.recovered())?.into_iter().filter(|e| e.changed && !e.overwritten) {
                print_recovered(&e, false);
            }
        }
        Cmd::Recover { command: RecoverCmd::List { paths } } => {
            for e in s.block(c.recovered_fresh())? {
                print_recovered(&e, paths);
            }
        }
        Cmd::Recover { command: RecoverCmd::Adopt { id } } => {
            println!("adopted into {}", s.block(c.adopt_displaced(&id))?.display());
        }
        Cmd::Recover { command: RecoverCmd::Discard { id } } => s.block(c.discard_displaced(&id))?,
        Cmd::Recover { command: RecoverCmd::DismissNotes } => match s.block(c.dismiss_keep_notes())? {
            0 => println!("no notes to dismiss"),
            n => println!("dismissed {}; `sk1ll status` shows what is left", plural(n, "note", "notes")),
        },
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
            // A new item gets its Item ID from the next change the engine
            // makes (the daemon's reconcile, a sync), except into a pending pull.
            let unassigned = repo.skills.iter().filter(|s| s.id.is_none()).count()
                + repo.rules.iter().filter(|r| r.id.is_none()).count();
            for skill in repo.skills {
                let origin = match skill.origin {
                    Origin::Personal if skill.shadows_import => "personal*",
                    Origin::Personal => "personal",
                    Origin::Imported => "imported",
                };
                let line = format!("{:<24} {:<10} {}", skill.name, origin, skill.description.unwrap_or_default());
                println!("{}", line.trim_end());
            }
            for rule in repo.rules {
                let scope = rule.harnesses.map(|t| format!("[{}] ", t.join(", "))).unwrap_or_default();
                let line = format!("{:<24} {:<10} {scope}{}", rule.name, "rule", rule.description.unwrap_or_default());
                println!("{}", line.trim_end());
            }
            if unassigned > 0 && repo.pull_pending {
                println!(
                    "{} no Item ID until the pending pull finishes, so no Assignment toggle; run `sk1ll pull`",
                    plural(unassigned, "item has", "items have")
                );
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
            let updated = s.block(c.update_imports(&UpdateImportsRequest { name, force, expected: None }))?;
            if updated.is_empty() {
                println!("no imports to update; import a skill with `sk1ll add <link>`");
            }
            for r in &updated {
                print_update(&r.name, r.old_commit.as_deref(), &r.new_commit, r.changed);
            }
            let changed: Vec<_> = updated.iter().filter(|r| r.changed).map(|r| fleet::shell_arg(&r.name)).collect();
            match &changed[..] {
                [] => {}
                [name] => println!("review with `sk1ll diff {name}`, then `sk1ll commit {name}`"),
                names => println!("review each with `sk1ll diff <name>`, then `sk1ll commit {}`", names.join(" ")),
            }
        }
        Cmd::Remove { name } => {
            if let Err(e) = s.block(c.remove_import(&name)) {
                // A personal item has a name too, but nothing upstream to remove it from: it is deleted.
                let named = |kind: Kind| personal_item(s, &format!("{}:{name}", kind.label())).ok();
                let found: Vec<_> = match personal_item(s, &name) {
                    Ok(item) => vec![item],
                    Err(_) => [Kind::Skill, Kind::Rule].into_iter().filter_map(named).collect(),
                };
                if found.is_empty() {
                    return Err(e);
                }
                let what: Vec<_> = found.iter().map(|(kind, _)| kind.label()).collect();
                let commands: Vec<_> =
                    found.iter().map(|(kind, name)| format!("`sk1ll delete {}`", item_arg(*kind, name))).collect();
                bail!(
                    "`{}` is a personal {}, not an import; delete it with {}",
                    found[0].1,
                    what.join(" and a personal "),
                    commands.join(" or ")
                );
            }
            println!("removed {name}");
        }
        Cmd::New { kind, name, description } => {
            let request =
                CreateItemRequest { kind: kind.label().into(), name: name.clone(), description, bundled: None };
            s.block(c.create_item(&request))?;
            let item = item_arg(kind, &name);
            let path = s.block(c.editable(&item))?;
            println!("created the personal {} {name} at {}", kind.label(), path.display());
            // The bare name, unless a skill and a rule share it.
            let edit = if personal_item(s, &name).is_ok() { fleet::shell_arg(&name) } else { item };
            println!("write it with `sk1ll edit {edit}`");
        }
        Cmd::Delete { item, yes } => {
            let (kind, name) = personal_item(s, &item)?;
            let item = item_arg(kind, &name);
            if !yes {
                if !std::io::stdin().is_terminal() {
                    bail!("deleting a {} requires terminal confirmation or `--yes`", kind.label());
                }
                if !confirm(&format!("delete the personal {} {name} in the draft?", kind.label()))? {
                    bail!("aborted");
                }
            }
            s.block(c.delete_item(&DeleteItemRequest { kind: kind.label().into(), name: name.clone() }))?;
            println!("deleted {} {name} in the draft", kind.label());
            println!("commit the deletion with `sk1ll commit {item}`, or bring it back with `sk1ll restore {item}`");
        }
        Cmd::Rename { item, to } => {
            let (kind, name) = personal_item(s, &item)?;
            let request = RenameItemRequest { kind: kind.label().into(), name: name.clone(), to: to.clone() };
            let renamed = s.block(c.rename_item(&request))?.item;
            println!("renamed {} {name} to {to} in the draft; its Item ID and Assignment go with it", kind.label());
            println!("commit the rename with `sk1ll commit {} {}`", item_arg(kind, &name), renamed);
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
            if let Some(line) = release_check::notice() {
                println!("{line}");
            }
            for import in &repo.imports {
                let state = match import.state {
                    ImportState::Clean => "clean",
                    ImportState::Modified => "modified locally",
                    ImportState::Missing => "not vendored (run `sk1ll update`)",
                };
                println!("import {:<24} {}", import.name, state);
            }
            // Changed entries are what the user still has to adopt or discard,
            // but for the edits they chose to overwrite, which only wait there.
            let recovered = s.block(c.recovered_fresh())?;
            let kept = recovered.iter().filter(|e| e.changed && !e.overwritten).count();
            if kept > 0 {
                println!(
                    "recovery: {} to adopt or discard; review with `sk1ll recover list`",
                    plural(kept, "changed entry", "changed entries")
                );
            }
            let overwritten = recovered.iter().filter(|e| e.overwritten).count();
            if overwritten > 0 {
                println!(
                    "recovery: {} you overwrote kept; adopt or discard with `sk1ll recover`",
                    plural(overwritten, "edit", "edits")
                );
            }
            // Blocked, Behind or a Conflict: placing nothing doesn't make the Machine in sync.
            let settled = kept == 0 && s.status.headline >= sk1ll_proto::MachineStatus::Drafting;
            let actions = s.block(c.preview_sync())?.actions.unwrap_or_default();
            print_plan(&actions, if settled { "harnesses in sync" } else { "nothing to place in the harnesses" });
        }
        Cmd::Sync { dry_run: true, force } => {
            let preview = s.block(c.sync(&SyncRequest { dry_run: true, force, ..Default::default() }))?;
            print_plan(&preview.actions.unwrap_or_default(), "harnesses in sync");
        }
        Cmd::Sync { dry_run: false, force } => {
            let result = s.block(c.sync(&SyncRequest { force, ..Default::default() }))?;
            let (kept, conflicts) = (&result.kept, &result.conflicts);
            if conflicts.is_empty() && result.errors.is_empty() {
                println!("harnesses in sync");
            }
            let mut adopts = Vec::new();
            for x in conflicts {
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
            for x in kept {
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
        Cmd::Commit { items, all, message, author, push } => {
            let items = match (items.is_empty(), all) {
                (false, _) => items,
                (true, true) => {
                    let draft = s.block(c.draft_fresh())?;
                    if draft.is_empty() {
                        println!("no draft to commit: on the baseline");
                        return Ok(());
                    }
                    draft.iter().map(|d| d.item.to_string()).collect()
                }
                (true, false) => bail!(
                    "name the draft items to commit (`sk1ll draft` lists them), or commit every one with `sk1ll \
                     commit --all`"
                ),
            };
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
        Cmd::RepairIds => match s.block(c.repair_ids())? {
            0 => println!("every item has an Item ID of its own; nothing to repair"),
            n => println!(
                "gave {} a fresh Item ID of its own in the Draft; commit {}",
                plural(n, "item", "items"),
                if n == 1 { "it" } else { "them" }
            ),
        },
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
                let question = match draft.len() {
                    1 => "discard the draft item?".to_string(),
                    n => format!("discard all {n} draft items?"),
                };
                if !yes && !confirm(&question)? {
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
                println!(
                    "run `sk1ll pull` to take the new baseline; `sk1ll recover adopt <id>` brings yours back after"
                );
            }
        }
        Cmd::Pull { keep, keep_all } => {
            let pulled = match (keep_all, keep.is_empty()) {
                (false, true) => s.block(c.pull()).map(|result| (result, Vec::new())),
                _ => s.block(c.pull_keep(&PullKeepRequest { items: keep, all: keep_all })).map(|r| (r.pull, r.kept)),
            };
            // What an interrupted keep left in recovery, whether the pull went
            // through or not; not being able to read it hides nothing else.
            if let Ok(status) = s.block(c.status()) {
                print_keep_report(&status.keep_report);
            }
            let (result, kept) = pulled?;
            match result {
                PullResult::UpToDate => println!("already on the latest baseline"),
                PullResult::Updated { from, to } => {
                    println!("baseline {} -> {}", from.as_deref().map_or("(none)", short), short(&to));
                }
                PullResult::Blocked { baseline, items, pending: true, .. } => bail!(
                    "pull pending at {}: {} changed while it was updating them. Keep your version with `sk1ll pull \
                     --keep {}`, or take the new baseline's with `sk1ll restore {}` and then `sk1ll pull`",
                    short(&baseline),
                    names(&items),
                    spaced(&items),
                    spaced(&items)
                ),
                PullResult::Blocked { baseline, items, pending: false, uncommitted } => {
                    if !uncommitted.is_empty() {
                        eprintln!(
                            "turned {} back into Draft, nothing lost: {} with baseline {}",
                            plural(uncommitted.len(), "unpushed commit", "unpushed commits"),
                            if uncommitted.len() == 1 { "it conflicts" } else { "they conflict" },
                            short(&baseline)
                        );
                        for commit in &uncommitted {
                            eprintln!("  {commit}");
                        }
                    }
                    bail!("blocked: {}", sk1ll_daemon::client::blocked_hint(&items))
                }
            }
            if !kept.is_empty() {
                println!(
                    "kept your version of {} on top, as Draft: it replaces the other change to {}",
                    names(&kept),
                    if kept.len() == 1 { "it" } else { "them" }
                );
                println!("review with `sk1ll diff {}`, then `sk1ll commit {}`", spaced(&kept), spaced(&kept));
            }
        }
        Cmd::Adopt { harness, name } => {
            require_harness(s, &harness)?;
            s.block(c.adopt(&ConflictTarget { harness: harness.clone(), name: name.clone() }))?;
            println!("adopted {name} from {harness} into the draft; review with `sk1ll diff {name}`");
        }
        Cmd::Assign { harness, kind, name, state, default_skills, default_rules, all, machine } => {
            assign(s, machine, &harness, kind, name, state, default_skills, default_rules, all)?;
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

/// `skill:<name>` or `rule:<name>`, quoted for a shell where it needs it.
fn item_arg(kind: Kind, name: &str) -> String {
    fleet::shell_arg(&format!("{}:{name}", kind.label()))
}

/// The personal skill or rule `item` names: a bare name, `skill:<name>` or
/// `rule:<name>`. An Import, or a name both a skill and a rule have, says
/// what to run instead.
fn personal_item(s: &Session, item: &str) -> Result<(Kind, String)> {
    let (kinds, name) = match item.split_once(':') {
        Some(("skill", name)) => (&[Kind::Skill][..], name),
        Some(("rule", name)) => (&[Kind::Rule][..], name),
        _ => (&[Kind::Skill, Kind::Rule][..], item),
    };
    let repo = s.block(s.client.repo(false))?;
    let skill = repo.skills.iter().find(|skill| skill.name == name);
    let found: Vec<Kind> = kinds
        .iter()
        .copied()
        .filter(|kind| match kind {
            Kind::Skill => skill.is_some_and(|skill| skill.origin == Origin::Personal),
            Kind::Rule => repo.rules.iter().any(|rule| rule.name == name),
        })
        .collect();
    match found[..] {
        [kind] => Ok((kind, name.to_string())),
        [_, _] => bail!("both a skill and a rule are named `{name}`; say `skill:{name}` or `rule:{name}`"),
        _ if kinds.contains(&Kind::Skill) && skill.is_some_and(|skill| skill.origin == Origin::Imported) => bail!(
            "`{name}` is an import; remove it with `sk1ll remove {0}`, or make it a personal skill with `sk1ll fork {0}`",
            fleet::shell_arg(name)
        ),
        _ => bail!("no personal skill or rule named `{name}` (see `sk1ll list`)"),
    }
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
    machine: Option<String>,
    harness: &str,
    kind: Option<Kind>,
    name: Option<String>,
    state: Option<Switch>,
    default_skills: Option<Switch>,
    default_rules: Option<Switch>,
    all: Option<Switch>,
) -> Result<()> {
    let own = s.status.machine_id.clone();
    let machine = match machine {
        Some(_) if s.status.mode == DaemonMode::Standalone => bail!(
            "this machine is standalone, so it has only its own Assignment; `--machine` needs a fleet (`sk1ll join`)"
        ),
        Some(machine) if machine != own => machine,
        _ => own.clone(),
    };
    if machine == own {
        require_harness(s, harness)?;
    } else {
        require_machine_harness(s, &machine, harness)?;
    }
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
    let requested = match &write {
        AssignmentWrite::Changes(changes) => changes.changes.clone(),
        _ => Vec::new(),
    };
    let update = match write {
        AssignmentWrite::Harness { harness, enabled } if machine == own => {
            s.block(s.client.set_harness_assignment(&harness, enabled))
        }
        AssignmentWrite::Harness { harness, enabled } => {
            s.block(s.client.set_machine_harness_assignment(&machine, &harness, enabled))
        }
        AssignmentWrite::Changes(changes) => s.block(s.client.set_assignment(&machine, &changes)),
        AssignmentWrite::RenameHarness(_) => unreachable!(),
    }
    .map_err(|e| scope_hint(e, &machine))?;
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
    let standalone = s.status.mode == DaemonMode::Standalone;
    for e in &update.entries {
        println!("{}: {}", e.harness, toggle_text(e.kind, e.name.as_deref(), e.after));
    }
    // A machine-local write logs nothing, and a fleet one logs only what changed:
    // the toggles asked for are echoed either way.
    let unchanged = match machine == own {
        true => update.revision == s.status.assignment_revision,
        false => update.entries.is_empty(),
    };
    if update.entries.is_empty() {
        for c in &requested {
            let already = if unchanged { " (already)" } else { "" };
            println!("{}: {}{already}", c.harness, toggle_text(c.kind, c.name.as_deref(), c.enabled));
        }
    }
    let whose = if standalone { "local" } else { "fleet" };
    match unchanged {
        true => println!("{whose} assignment of {machine} unchanged at revision {}", update.revision),
        false => println!("{whose} assignment of {machine} is now revision {}", update.revision),
    }
    Ok(())
}

/// One Assignment toggle as `assign` echoes it: `skill pdf off`, `rules default on`.
fn toggle_text(kind: ToggleKind, name: Option<&str>, value: Option<bool>) -> String {
    let what = match (kind, name) {
        (ToggleKind::NewSkillsDefault, _) => "skills default".to_string(),
        (ToggleKind::NewRulesDefault, _) => "rules default".to_string(),
        (ToggleKind::Skill, name) => format!("skill {}", name.unwrap_or("?")),
        (ToggleKind::Rule, name) => format!("rule {}", name.unwrap_or("?")),
    };
    let value = match value {
        Some(true) => "on",
        Some(false) => "off",
        None => "default",
    };
    format!("{what} {value}")
}

/// Another Machine's harness, as its last report names them: the server
/// refuses a harness the Machine doesn't have.
fn require_machine_harness(s: &Session, machine: &str, harness: &str) -> Result<()> {
    let fleet = s.block(s.client.fleet())?;
    let Some(row) = fleet.machines.iter().find(|m| m.id == machine) else {
        bail!("no machine `{machine}` in the fleet (see `sk1ll fleet`)");
    };
    if let Some(report) = &row.report
        && !report.harnesses.is_empty()
        && !report.harnesses.iter().any(|h| h.name == harness)
    {
        bail!(
            "{machine} has no harness `{harness}`; it has {}",
            report.harnesses.iter().map(|h| h.name.as_str()).collect::<Vec<_>>().join(", ")
        );
    }
    Ok(())
}

/// The server refuses a `self`-scope Machine another's Assignment: say what that means.
fn scope_hint(e: anyhow::Error, machine: &str) -> anyhow::Error {
    match e.downcast_ref::<sk1ll_daemon::client::ClientError>() {
        Some(client) if client.status == Some(403) => anyhow::anyhow!(
            "{client}; change {machine}'s Assignment on {machine} itself, or from a machine that joined with `fleet` scope"
        ),
        _ => e,
    }
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
    print_keep_report(&status.keep_report);
    if let Some(line) = server_version_line(status) {
        println!("{line}");
    }
    if let Some(error) = &status.last_error {
        println!("last error: {error}");
    }
}

/// The Server's release, once it named one, with a hint when it is older
/// than this Machine's. A protocol mismatch is in `last_error` already.
fn server_version_line(status: &sk1ll_daemon::client::StatusView) -> Option<String> {
    let server = status.server.as_ref()?.version.as_deref()?;
    let ours = env!("CARGO_PKG_VERSION");
    let older = matches!(release_order(server, ours), Some(std::cmp::Ordering::Less));
    let compatible = status.connection != sk1ll_daemon::client::Connection::Incompatible;
    Some(if older && compatible {
        format!("server: sk1ll {server}, older than this Machine's {ours}; upgrade the Server to get its fixes")
    } else {
        format!("server: sk1ll {server}")
    })
}

/// Orders two `major.minor.patch[-pre]` releases; a pre-release comes
/// before its release. `None`: one of them isn't a release number.
fn release_order(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    fn parse(v: &str) -> Option<([u64; 3], Option<&str>)> {
        let v = v.split_whitespace().next()?;
        let (core, pre) = v.split_once('-').map_or((v, None), |(c, p)| (c, Some(p)));
        let mut parts = core.split('.').map(|p| p.parse().ok());
        let numbers = [parts.next()??, parts.next()??, parts.next()??];
        parts.next().is_none().then_some((numbers, pre))
    }
    let ((a, a_pre), (b, b_pre)) = (parse(a)?, parse(b)?);
    Some(a.cmp(&b).then_with(|| match (a_pre, b_pre) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(x), Some(y)) => {
            // Dot-separated identifiers; numbers compare as numbers (`rc.9` < `rc.10`).
            let id = |p: &str| p.parse::<u64>().map_or((1, 0, p.to_string()), |n| (0, n, String::new()));
            x.split('.').map(id).cmp(y.split('.').map(id))
        }
    }))
}

/// What interrupted keeps left in recovery, each with the command that
/// brings it back.
fn print_keep_report(report: &[sk1ll_core::draft::Unkept]) {
    if report.is_empty() {
        return;
    }
    println!("an interrupted keep left your versions in recovery:");
    for u in report {
        match &u.entry {
            Some(id) => println!("  {} ({}): bring it back with `sk1ll recover adopt {id}`", u.item, u.error),
            None => println!("  {} ({})", u.item, u.error),
        }
    }
    if report.iter().any(|u| u.entry.is_none()) {
        println!("once read, dismiss the notes with `sk1ll recover dismiss-notes`");
    }
}

fn report_hint(items: &[sk1ll_core::ItemRef]) -> String {
    sk1ll_daemon::client::blocked_hint(items)
}

/// A recovery entry: its id, item, Harness (`pull` for the repo's own
/// files), age and what it holds; with `paths`, where it was and where it
/// is kept.
fn print_recovered(e: &ConflictView, paths: bool) {
    let item = e.item.as_ref().map(ToString::to_string).unwrap_or_else(|| e.name.clone());
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let age = e.displaced_at.map(|at| fleet::ago(now.saturating_sub(at) as i64)).unwrap_or_default();
    // Restore keeps the version a pull is Blocked on as a changed entry.
    // A pull, and `recover adopt` (only over a path with no Draft change),
    // keep each file they replace too: a committed version while unchanged.
    let pull = e.harness == sk1ll_core::sync::displaced::PULL;
    let what = match (e.changed, pull) {
        _ if e.overwritten => "changed: your edit, kept when you overwrote it; adopt or discard it",
        (true, true) => "changed: your version; `sk1ll recover adopt` it",
        (true, false) => "changed: written to while or after it was set aside; adopt or discard it",
        (false, true) => "unchanged: a committed version a pull or adopt replaced; safe to discard",
        (false, false) => "unchanged: as it was when set aside",
    };
    println!("{:<28} {item:<24} {:<10} {age:<8} {what}", e.id.as_deref().unwrap_or_default(), e.harness);
    if paths {
        println!("  was at   {}", e.dest.display());
        println!("  kept in  {}", e.path.as_deref().unwrap_or(Path::new("")).display());
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

/// A sync plan: `+` create, `~` update, `-` remove, `!` a conflict left
/// alone, or `empty` for a plan with nothing in it.
fn print_plan(actions: &[PlacementView], empty: &str) {
    if actions.is_empty() {
        println!("{empty}");
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
    fn documented_commands_and_flags_exist() {
        // The Commands table in README.md and the Pull flags in its workflow.
        let cli = Cli::command();
        cli.clone().debug_assert();
        let contracts: &[(&[&str], &[&str])] = &[
            (&[], &["repo", "machine-config", "assignment"]),
            (&["setup"], &[]),
            (&["init"], &[]),
            (&["add"], &["path", "name", "ref", "list"]),
            (&["update"], &["force", "dry-run"]),
            (&["remove"], &[]),
            (&["fork"], &[]),
            (&["new"], &["description"]),
            (&["edit"], &[]),
            (&["rename"], &[]),
            (&["delete"], &["yes"]),
            (&["list"], &["json", "unmanaged"]),
            (&["repair-ids"], &[]),
            (&["status"], &[]),
            (&["sync"], &["dry-run", "force"]),
            (&["render"], &[]),
            (&["harnesses"], &["all"]),
            (&["harnesses", "enable"], &[]),
            (&["harnesses", "disable"], &["take-out"]),
            (&["harnesses", "add"], &["skills", "instructions", "format", "mode"]),
            (&["harnesses", "remove"], &["take-out"]),
            (&["harnesses", "rename"], &["replace"]),
            (&["assign"], &["default-skills", "default-rules", "all", "machine"]),
            (&["conflicts"], &[]),
            (&["adopt"], &[]),
            (&["recover", "list"], &["paths"]),
            (&["recover", "adopt"], &[]),
            (&["recover", "discard"], &[]),
            (&["recover", "dismiss-notes"], &[]),
            (&["draft"], &[]),
            (&["diff"], &[]),
            (&["commit"], &["all", "push"]),
            (&["push"], &[]),
            (&["restore"], &["all"]),
            (&["pull"], &["keep", "keep-all"]),
            (&["join"], &["admin-token"]),
            (&["daemon"], &[]),
            (&["fleet", "invite"], &["scope"]),
            (&["fleet", "remove"], &["yes"]),
            (&["fleet", "orphans"], &[]),
            (&["fleet", "prune-orphans"], &["machine"]),
            (&["fleet", "copy-assignment"], &["merge", "map", "yes"]),
            (&["fleet", "log"], &["machine", "harness", "author", "item", "before", "limit"]),
            (&["fleet", "undo"], &[]),
            (&["completions"], &[]),
        ];
        for &(path, flags) in contracts {
            let command = path.iter().fold(&cli, |command, name| {
                command.find_subcommand(name).unwrap_or_else(|| panic!("missing command {path:?}"))
            });
            for flag in flags {
                assert!(command.get_arguments().any(|arg| arg.get_long() == Some(flag)), "{path:?} lacks --{flag}");
            }
        }
    }
}
