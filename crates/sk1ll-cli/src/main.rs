use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use sk1ll_core::{
    Action, ActionKind, Assignment, Change, EngineLock, HarnessOrigin, Import, ImportState, InstructionsFormat, Item,
    ItemRef, LinkMode, MachineConfig, Origin, PlanOptions, PullOutcome, Repo, default_promote_message, machine_author,
    sync,
};

#[cfg(unix)]
mod fleet;

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

    pub(crate) fn queue_discarded(
        _state: Option<&Path>, _lock: Option<&sk1ll_core::EngineLock>, _items: Vec<sk1ll_core::ItemRef>,
    ) -> Result<()> {
        Ok(())
    }

    pub(crate) fn assign_direct(_cli: &Cli, _repo: &Path, _lock: Option<&sk1ll_core::EngineLock>) -> Result<bool> {
        Ok(false)
    }

    pub(crate) fn connect(cli: &Cli, _repo: &Path) -> Result<Option<Daemon>> {
        let _ = &cli.daemon_socket;
        Ok(None)
    }

    pub(crate) fn command(cli: Cli) -> Result<()> {
        let name = match cli.cmd {
            Cmd::Join { .. } => "join",
            Cmd::Daemon => "daemon",
            _ => "fleet",
        };
        bail!("`sk1ll {name}` is not supported on this platform: the daemon and the fleet need Unix")
    }
}

#[derive(Parser)]
#[command(
    name = "sk1ll",
    version,
    about = "0xSk1ll manages agent skills and instructions and syncs them across machines and harnesses"
)]
struct Cli {
    /// Skills repo (default: nearest sk1ll.toml above cwd, else ~/skills)
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
    /// List skills and instruction fragments in the repo
    List,
    /// Import a skill from a git repo (URL, local path, or github:owner/repo)
    Add {
        source: String,
        /// Subdirectory containing SKILL.md
        #[arg(long, default_value = "")]
        path: String,
        /// Branch, tag or commit
        #[arg(long = "ref", default_value = "HEAD")]
        git_ref: String,
        /// Local name (default: last path segment or repo name)
        #[arg(long)]
        name: Option<String>,
    },
    /// Re-fetch imports and update the lockfile
    Update {
        name: Option<String>,
        /// Overwrite local edits to vendored imports
        #[arg(long)]
        force: bool,
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
        /// Overwrite hand edits inside managed instruction blocks
        #[arg(long)]
        force: bool,
    },
    /// List this machine's draft: uncommitted changes, per skill/fragment
    Draft,
    /// Show a draft item's changes against the baseline
    #[command(after_help = "Items: a skill or fragment name, or skill:<name>, fragment:<name>, manifest, other:<path>")]
    Diff { item: String },
    /// Commit draft items as the new baseline
    #[command(after_help = "Items: a skill or fragment name, or skill:<name>, fragment:<name>, manifest, other:<path>")]
    Promote {
        #[arg(required = true)]
        items: Vec<String>,
        /// Commit message (default: "promote <items> from <machine>")
        #[arg(short, long)]
        message: Option<String>,
        /// Commit author, "Name <email>" (default: git's identity, else the machine id)
        #[arg(long)]
        author: Option<String>,
        /// Push to origin afterwards
        #[arg(long)]
        push: bool,
    },
    /// Push promoted commits to origin
    Push,
    /// Discard draft items, returning them to the baseline
    Reset {
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
    /// List this machine's harnesses (detected presets + machine.toml)
    Harnesses,
    /// Turn a skill or fragment on/off for a harness, or set its defaults
    #[command(after_help = "Examples:\n  sk1ll assign claude skill pdf off\n  sk1ll assign codex --default-skills off")]
    Assign {
        harness: String,
        kind: Option<Kind>,
        name: Option<String>,
        state: Option<Switch>,
        /// Whether newly appearing skills are enabled
        #[arg(long, value_name = "on|off")]
        default_skills: Option<Switch>,
        /// Whether newly appearing fragments are enabled
        #[arg(long, value_name = "on|off")]
        default_fragments: Option<Switch>,
    },
    /// Join a fleet: register with the hub and clone its repo into --repo (default ~/skills)
    Join {
        /// The hub's base URL, e.g. https://hub.example
        hub_url: String,
        /// Single-use join code from `POST /api/join-codes`
        code: String,
        /// Machine id (default: machine.toml's id, else the hostname)
        #[arg(long)]
        id: Option<String>,
        /// Start from this machine's assignment instead of everything off
        #[arg(long)]
        copy_from: Option<String>,
    },
    /// Run the daemon in the foreground (what systemd or launchd runs)
    Daemon,
    /// Show every machine in the fleet (through the daemon)
    Fleet,
}

#[derive(Subcommand)]
enum RecoverCmd {
    List,
    Adopt { id: String },
    Discard { id: String },
}

fn print_recovered(entries: Vec<sync::displaced::Entry>) {
    for e in entries {
        println!("{} {} {:?} {} {}{}", e.id, e.harness, e.kind, e.dest.display(), e.path.display(),
            if e.changed { " (changed)" } else { "" });
    }
}

impl Cmd {
    fn git_only(&self) -> bool {
        matches!(self, Self::Draft | Self::Diff { .. } | Self::Reset { .. } | Self::Promote { .. } | Self::Pull)
    }

    /// Changes the repo, the harnesses or the assignment. With a daemon
    /// managing the repo, these go through it; otherwise they hold the
    /// engine lock while they run.
    fn mutates(&self) -> bool {
        match self {
            Self::Recover { .. } | Self::Conflicts | Self::Add { .. }
            | Self::Update { .. }
            | Self::Remove { .. }
            | Self::Fork { .. }
            | Self::Promote { .. }
            | Self::Push
            | Self::Reset { .. }
            | Self::Pull
            | Self::Adopt { .. }
            | Self::Assign { .. } => true,
            Self::Sync { dry_run, .. } => !dry_run,
            Self::Init { .. }
            | Self::List
            | Self::Status
            | Self::Draft
            | Self::Diff { .. }
            | Self::Render { .. }
            | Self::Harnesses
            | Self::Join { .. }
            | Self::Daemon
            | Self::Fleet => false,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum Kind {
    Skill,
    Fragment,
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
    let cli = Cli::parse();
    // Single-threaded here, before any runtime starts: the only safe place to
    // change the environment (git's credentials for the hub, see `fleet`).
    fleet::prepare(&cli)?;
    run(cli)
}

fn run(cli: Cli) -> Result<()> {
    if matches!(cli.cmd, Cmd::Join { .. } | Cmd::Daemon | Cmd::Fleet) {
        return fleet::command(cli);
    }
    let repo_path = match &cli.repo {
        Some(p) => p.clone(),
        None => {
            let cwd = std::env::current_dir()?;
            if cli.cmd.git_only() { Repo::locate_git(&cwd) } else { Repo::locate(&cwd) }
                .context("cannot determine repo location")?
        }
    };

    if let Cmd::Init { path } = &cli.cmd {
        let root = path.clone().unwrap_or(repo_path);
        let _lock = lock_direct(&root)?;
        std::fs::create_dir_all(&root)?;
        let repo = Repo::init(&root)?;
        println!("initialized skills repo at {}", repo.root().display());
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

    if let Cmd::Harnesses = &cli.cmd {
        let machine = machine()?;
        println!("machine {} ({})", machine.id, machine_path.display());
        for (name, e) in machine.effective() {
            let origin = match e.origin {
                HarnessOrigin::Detected => "detected",
                HarnessOrigin::Override => "override",
                HarnessOrigin::Local => "local",
            };
            let mode = match e.harness.mode {
                LinkMode::Symlink => "symlink",
                LinkMode::Copy => "copy",
            };
            let format = match e.harness.instructions_format {
                InstructionsFormat::Block => "block",
                InstructionsFormat::Rules => "rules",
            };
            let skills = e.harness.skills.as_deref().map(|s| format!("skills {s} ({mode})")).unwrap_or_default();
            let instr = e.harness.instructions.as_deref().map(|i| format!("instructions {i} ({format})"));
            println!("{name:<10} {origin:<9} {skills:<40} {}", instr.unwrap_or_default());
        }
        return Ok(());
    }

    // A daemon managing this repo does every change, so that changes never
    // interleave and it pushes, reports status and tells the hub. Without one,
    // a change holds the engine lock (which fails if a daemon holds it).
    let lock = if cli.cmd.mutates() {
        if let Some(daemon) = fleet::connect(&cli, &repo_path)? {
            return daemon.run(cli.cmd, &repo_path);
        }
        lock_direct(&repo_path)?
    } else {
        None
    };
    if matches!(cli.cmd, Cmd::Assign { .. }) && fleet::assign_direct(&cli, &repo_path, lock.as_ref())? {
        return Ok(());
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
        Cmd::Init { .. } | Cmd::Join { .. } | Cmd::Daemon | Cmd::Fleet => unreachable!(),
        Cmd::List => {
            for s in repo.skills()? {
                let origin = match s.origin {
                    Origin::Personal if s.shadows_import => "personal*",
                    Origin::Personal => "personal",
                    Origin::Imported => "imported",
                };
                println!("{:<24} {:<10} {}", s.name, origin, s.description.unwrap_or_default());
            }
            for f in repo.fragments()? {
                let scope = f.harnesses.map(|t| format!("[{}] ", t.join(", "))).unwrap_or_default();
                println!("{:<24} {:<10} {scope}{}", f.name, "instr", f.description.unwrap_or_default());
            }
        }
        Cmd::Add { source, path, git_ref, name } => {
            let name = name.unwrap_or_else(|| default_name(&source, &path));
            let r = repo.add_import(&name, Import { source, path, git_ref })?;
            println!("added {} @ {}", r.name, short(&r.new_commit));
        }
        Cmd::Update { name, force } => {
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
        Cmd::Harnesses => unreachable!(),
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
        Cmd::Promote { items, message, author, push } => {
            let draft = repo.draft()?;
            let items = items.iter().map(|i| ItemRef::resolve(i, &draft)).collect::<Result<Vec<_>, _>>()?;
            let machine = machine()?;
            let message = message.unwrap_or_else(|| default_promote_message(&items, &machine.id));
            let author = match author {
                Some(a) => Some(a),
                None if !repo.has_git_identity()? => Some(machine_author(&machine.id)),
                None => None,
            };
            let sha = repo.promote(&items, &message, author.as_deref())?;
            println!("promoted {} as {}", names(&items), short(&sha));
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
        Cmd::Reset { items, all, yes } => {
            let before: std::collections::BTreeSet<ItemRef> = repo.draft()?.into_iter().map(|d| d.item).collect();
            if all {
                let draft = repo.draft()?;
                if draft.is_empty() {
                    println!("no draft to reset");
                    return Ok(());
                }
                for d in &draft {
                    println!("{} {}", change_sym(d.change), d.item);
                }
                if !yes && !confirm(&format!("discard all {} draft item(s)?", draft.len()))? {
                    bail!("aborted");
                }
                repo.reset_all()?;
                println!("draft reset to the baseline");
            } else {
                let draft = repo.draft()?;
                let items = items.iter().map(|i| ItemRef::resolve(i, &draft)).collect::<Result<Vec<_>, _>>()?;
                repo.reset(&items)?;
                println!("reset {}", names(&items));
            }
            let after: std::collections::BTreeSet<ItemRef> = repo.draft()?.into_iter().map(|d| d.item).collect();
            let discarded = before.difference(&after).cloned().collect();
            fleet::queue_discarded(daemon_state.as_deref(), lock.as_ref(), discarded)?;
            println!("run `sk1ll sync` to apply");
        }
        Cmd::Pull => match repo.pull()? {
            PullOutcome::UpToDate => println!("already on the latest baseline"),
            PullOutcome::Updated { from, to } => {
                println!("baseline {} -> {}; run `sk1ll sync` to apply", short(&from), short(&to));
            }
            PullOutcome::Blocked { baseline, items } => bail!(
                "blocked: baseline {} clashes with {}; promote or reset them, then pull again",
                short(&baseline),
                names(&items)
            ),
        },
        Cmd::Adopt { harness, name } => {
            let harnesses = machine()?.harnesses();
            if !harnesses.contains_key(&harness) {
                bail!("unknown harness `{harness}` on this machine (see `sk1ll harnesses`)");
            }
            let actions = sync::plan(&repo, &harnesses, &assignment()?, PlanOptions::default())?;
            let file = format!("{name}.md");
            let action = actions
                .iter()
                .filter(|a| a.harness == harness && a.kind == ActionKind::Conflict)
                .find(|a| match &a.item {
                    Item::Skill { name: n, .. } => *n == name,
                    Item::Rule { file: f, .. } => *f == name || *f == file,
                    Item::Instructions { fragments, .. } => fragments.contains(&name),
                })
                .with_context(|| format!("no conflict for `{name}` in `{harness}` (see `sk1ll status`)"))?;
            sync::adopt(&repo, action)?;
            println!("adopted {name} from {harness} into the draft; review with `sk1ll diff {name}`");
        }
        Cmd::Assign { harness, kind, name, state, default_skills, default_fragments } => {
            if !machine()?.harnesses().contains_key(&harness) {
                bail!("unknown harness `{harness}` on this machine (see `sk1ll harnesses`)");
            }
            let mut a = assignment()?;
            if let Some((kind, name, state)) = assign_target(kind, name, state, default_skills, default_fragments)? {
                let label = check_exists(&repo, kind, &name)?;
                match kind {
                    Kind::Skill => a.set_skill(&harness, &name, state.on()),
                    Kind::Fragment => a.set_fragment(&harness, &name, state.on()),
                }
                println!("{harness}: {label} {name} {}", state.label());
            }
            if let Some(s) = default_skills {
                let names = repo.skills()?.into_iter().map(|s| s.name).collect::<Vec<_>>();
                a.materialize_skills(&harness, &names);
                a.set_new_skills_default(&harness, s.on());
                println!("{harness}: skills default {}", s.label());
            }
            if let Some(s) = default_fragments {
                let names = repo.fragments()?.into_iter().map(|f| f.name).collect::<Vec<_>>();
                a.materialize_fragments(&harness, &names);
                a.set_new_fragments_default(&harness, s.on());
                println!("{harness}: fragments default {}", s.label());
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
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(existing)
        .args(["rev-parse", "--show-toplevel"])
        .output()?;
    if !out.status.success() {
        return Ok(None);
    }
    Ok(Some(EngineLock::acquire(existing)?))
}

/// What `assign` toggles: an item (`None` when only defaults change).
fn assign_target(
    kind: Option<Kind>,
    name: Option<String>,
    state: Option<Switch>,
    default_skills: Option<Switch>,
    default_fragments: Option<Switch>,
) -> Result<Option<(Kind, String, Switch)>> {
    match (kind, name, state) {
        (Some(kind), Some(name), Some(state)) => Ok(Some((kind, name, state))),
        (None, None, None) if default_skills.is_some() || default_fragments.is_some() => Ok(None),
        _ => bail!(
            "usage: sk1ll assign <harness> <skill|fragment> <name> <on|off>, \
             or sk1ll assign <harness> --default-skills|--default-fragments <on|off>"
        ),
    }
}

/// Fails unless the repo has that skill or fragment; returns its label.
fn check_exists(repo: &Repo, kind: Kind, name: &str) -> Result<&'static str> {
    let (exists, label) = match kind {
        Kind::Skill => (repo.skills()?.iter().any(|s| s.name == name), "skill"),
        Kind::Fragment => (repo.fragments()?.iter().any(|f| f.name == name), "fragment"),
    };
    if !exists {
        bail!("no {label} named `{name}` in the repo");
    }
    Ok(label)
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
        Item::Instructions { fragments, .. } => {
            format!("{:<10} instructions [{}] -> {}", a.harness, fragments.join(", "), a.dest.display())
        }
        Item::Rule { .. } => format!("{:<10} rule -> {}", a.harness, a.dest.display()),
    }
}

fn default_name(source: &str, path: &str) -> String {
    let base = path.trim_matches('/').rsplit('/').next().filter(|s| !s.is_empty());
    let base = base.unwrap_or_else(|| source.trim_end_matches('/').rsplit(['/', ':']).next().unwrap_or(source));
    base.trim_end_matches(".git").to_string()
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
        fn joined(&self, hub_url: &str) {
            sk1ll_daemon::DaemonState {
                machine_id: "test".into(), hub_url: hub_url.into(), token: "tok".into(),
                repo: self.0.clone(), revoked: false,
            }.save(&self.0.join(".git/daemon.json")).unwrap();
        }
    }

    impl Drop for TestRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
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
    #[cfg(unix)]
    fn direct_fleet_assignment_authenticates_and_stores_the_hub_snapshot() {
        use std::io::Read;
        let repo = TestRepo::new().with_skill();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        repo.joined(&format!("http://{}", listener.local_addr().unwrap()));
        let root = repo.0.clone();
        let server = std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let mut stream = loop {
                match listener.accept() {
                    Ok((s, _)) => break s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(started.elapsed() < std::time::Duration::from_secs(3), "no Hub PUT received");
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            stream.set_read_timeout(Some(std::time::Duration::from_secs(3))).unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                headers.push_str(&line);
                if line == "\r\n" { break; }
            }
            let len: usize = headers.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().unwrap())
            }).unwrap();
            let mut body = vec![0; len];
            reader.read_exact(&mut body).unwrap();
            assert!(EngineLock::try_acquire(&root).unwrap().is_none(), "held across the Hub request");
            let response = r#"{"revision":9,"assignment":{"harnesses":{
                "h":{"new_skills_default":false,"skills":{"pdf":false}}}},"entries":[]}"#;
            write!(stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{response}", response.len()).unwrap();
            (headers, String::from_utf8(body).unwrap())
        });
        let result = repo.run(&["assign", "h", "skill", "pdf", "off", "--default-skills", "off"]);
        let (headers, body) = server.join().unwrap();
        result.unwrap();
        assert!(headers.starts_with("PUT /api/machines/test/assignment HTTP/1.1\r\n"), "{headers}");
        assert!(headers.to_ascii_lowercase().contains("authorization: bearer tok\r\n"), "{headers}");
        assert!(body.contains(r#""kind":"skill","name":"pdf","enabled":false"#), "{body}");
        assert!(body.contains(r#""kind":"new_skills_default","enabled":false"#), "{body}");
        let snap = sk1ll_daemon::state::load_snapshot(&repo.0.join(".git/daemon-assignment.json")).unwrap().unwrap();
        assert_eq!(snap.revision, 9);
        assert!(!snap.assignment.skill_enabled("h", "pdf"));
        assert!(!repo.0.join("assignment.json").exists(), "Hub state is stored without depending on the mirror");
    }

    #[test]
    #[cfg(unix)]
    fn direct_fleet_assignment_fails_when_the_hub_is_unreachable() {
        let repo = TestRepo::new().with_skill();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        repo.joined(&url);
        sk1ll_daemon::state::save_snapshot(&repo.0.join(".git/daemon-assignment.json"),
            &sk1ll_proto::AssignmentSnapshot::default()).unwrap();
        let err = repo.run(&["assign", "h", "skill", "pdf", "on"]).expect_err("fleet assignment must reach the hub");
        assert!(err.to_string().contains("hub is unreachable"), "{err}");
        assert!(!repo.0.join("assignment.json").exists(), "the mirror cannot record an unaccepted change");
        assert_eq!(sk1ll_daemon::state::load_snapshot(&repo.0.join(".git/daemon-assignment.json"))
            .unwrap().unwrap().revision, 0);
    }

    #[test]
    #[cfg(unix)]
    fn direct_resets_persist_discard_notifications_for_the_daemon() {
        for all in [false, true] {
            let repo = TestRepo::new().with_skill();
            repo.joined("http://127.0.0.1:1");
            fs::write(repo.0.join("machine.toml"), "[broken").unwrap();
            repo.run(if all { &["reset", "--all", "--yes"] } else { &["reset", "pdf"] }).unwrap();
            let outbox = fs::read_to_string(repo.0.join(".git/daemon-outbox.json"))
                .expect("direct reset queues the same durable notification as the daemon");
            assert!(outbox.contains("skill:pdf"), "{outbox}");
            assert!(!repo.0.join("personal/pdf").exists());
        }
    }

    #[test]
    #[cfg(unix)]
    fn direct_sync_uses_the_authoritative_assignment_and_requires_a_snapshot() {
        let repo = TestRepo::new().with_skill();
        let skills = repo.0.join(".git/harness/skills");
        fs::write(repo.0.join("machine.toml"), format!(
            "id = 'test'\n[harnesses.h]\nskills = '{}'\nmode = 'copy'\n\
             [harnesses.claude]\nenabled = false\n[harnesses.codex]\nenabled = false\n\
             [harnesses.opencode]\nenabled = false\n[harnesses.cursor]\nenabled = false\n", skills.display()
        )).unwrap();
        sk1ll_daemon::DaemonState {
            machine_id: "test".into(), hub_url: "http://127.0.0.1:1".into(), token: "tok".into(),
            repo: repo.0.clone(), revoked: false,
        }.save(&repo.0.join(".git/daemon.json")).unwrap();
        let mut assignment = Assignment::default();
        assignment.set_skill("h", "pdf", false);
        sk1ll_daemon::state::save_snapshot(&repo.0.join(".git/daemon-assignment.json"),
            &sk1ll_proto::AssignmentSnapshot { revision: 7, assignment }).unwrap();
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
        repo.run(&["assign", "h", "--default-skills", "off", "--default-fragments", "off"]).unwrap();
        let a = Assignment::load(&repo.0.join("assignment.json")).unwrap();
        assert!(a.skill_enabled("h", "pdf"));
        assert!(a.fragment_enabled("h", "git"));
        assert!(!a.skill_enabled("h", "later") && !a.fragment_enabled("h", "later"));
        repo.run(&["assign", "h", "skill", "pdf", "off", "--default-skills", "on"]).unwrap();
        let a = Assignment::load(&repo.0.join("assignment.json")).unwrap();
        assert!(!a.skill_enabled("h", "pdf"));
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
            &["promote", "pdf"],
            &["reset", "pdf"],
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
        fs::write(repo.0.join("machine.toml"), format!(
            "id = 'test'\n[harnesses.h]\nskills = '{}'\nmode = 'copy'\n\
             [harnesses.claude]\nenabled = false\n[harnesses.codex]\nenabled = false\n\
             [harnesses.opencode]\nenabled = false\n[harnesses.cursor]\nenabled = false\n", skills.display()
        )).unwrap();
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
        assert!(sock.exists());
        repo.run_on(&sock, &assign).unwrap();
        assert!(repo.0.join("assignment.json").exists());

        fs::remove_file(&sock).unwrap();
        repo.run_on(&sock, &["assign", "h", "skill", "pdf", "on"]).unwrap();
    }

    #[test]
    fn recovery_commands_work_with_damaged_configuration() {
        for deleted in [false, true] {
            for command in ["draft", "diff", "reset", "promote", "pull"] {
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
                    "diff" | "reset" => vec![command, "manifest"],
                    "promote" => vec![command, "manifest", "--author", "Test <test@example.com>"],
                    _ => vec![command],
                };
                assert!(repo.run(&args).is_ok(), "{command}, deleted={deleted}");
                if command == "reset" {
                    assert!(Repo::open(&repo.0).is_ok());
                } else {
                    assert!(Repo::open(&repo.0).is_err());
                }
            }
        }
    }
}
