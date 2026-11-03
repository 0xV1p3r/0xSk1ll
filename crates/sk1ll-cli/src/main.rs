use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use sk1ll_core::{Action, ActionKind, Import, ImportState, Item, Origin, PlanOptions, Repo, sync};

#[derive(Parser)]
#[command(name = "sk1ll", version, about = "0xSk1ll manages agent skills and instructions and syncs them across machines and harnesses")]
struct Cli {
    /// Skills repo (default: nearest sk1ll.toml above cwd, else ~/skills)
    #[arg(long, global = true, env = "SK1LL_REPO")]
    repo: Option<PathBuf>,
    /// Profile selecting which skills/instructions/targets apply on this machine
    #[arg(long, global = true, env = "SK1LL_PROFILE", default_value = "default")]
    profile: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
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
    /// Print the instruction files a target would receive
    Render { target: String },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let repo_path = match &cli.repo {
        Some(p) => p.clone(),
        None => Repo::locate(&std::env::current_dir()?).context("cannot determine repo location")?,
    };

    if let Cmd::Init { path } = &cli.cmd {
        let root = path.clone().unwrap_or(repo_path);
        std::fs::create_dir_all(&root)?;
        let repo = Repo::init(&root)?;
        println!("initialized skills repo at {}", repo.root().display());
        return Ok(());
    }

    let mut repo = Repo::open(&repo_path)?;
    match cli.cmd {
        Cmd::Init { .. } => unreachable!(),
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
                let scope = f.targets.map(|t| format!("[{}] ", t.join(", "))).unwrap_or_default();
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
                match (&r.old_commit, r.changed) {
                    (_, false) => println!("{:<24} up to date", r.name),
                    (Some(old), true) => println!("{:<24} {} -> {}", r.name, short(old), short(&r.new_commit)),
                    (None, true) => println!("{:<24} vendored @ {}", r.name, short(&r.new_commit)),
                }
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
            print_plan(&sync::plan(&repo, &cli.profile, PlanOptions::default())?);
        }
        Cmd::Sync { dry_run, force } => {
            let actions = sync::plan(&repo, &cli.profile, PlanOptions { force })?;
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
        Cmd::Render { target } => {
            let files = sync::render_instructions(&repo, &cli.profile, &target)?;
            if files.is_empty() {
                eprintln!("no instructions selected for `{target}`");
            }
            for f in files {
                println!("==> {} <==\n{}\n", f.path.display(), f.content.trim_end());
            }
        }
    }
    Ok(())
}

fn print_plan(actions: &[Action]) {
    if actions.is_empty() {
        println!("targets in sync");
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
        Item::Skill { name, .. } => format!("{:<10} {name}", a.target),
        Item::Instructions { fragments, .. } => {
            format!("{:<10} instructions [{}] -> {}", a.target, fragments.join(", "), a.dest.display())
        }
        Item::Rule { .. } => format!("{:<10} rule -> {}", a.target, a.dest.display()),
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
