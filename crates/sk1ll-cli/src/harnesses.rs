//! `sk1ll harnesses`: list this machine's harnesses, and change them in
//! `machine.toml` after showing what sk1ll placed where the change stops
//! syncing (it stays there) and what sync does next. The engine makes the
//! change and syncs.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use sk1ll_core::{Harness, HarnessChange, HarnessChoice, InstructionsFormat, LinkMode};
use sk1ll_daemon::client::{HarnessChangeRequest, HarnessView, PlacementView};

use crate::confirm;
use crate::fleet::Session;

#[derive(Subcommand)]
pub(crate) enum HarnessCmd {
    /// Rename a harness and move its Assignment
    Rename {
        old: String,
        new: String,
        /// Replace an Assignment still stored for the new name (of a harness that is gone)
        #[arg(long)]
        replace: bool,
        /// Accepted as for the other changes; a rename asks no confirmation
        #[arg(long, short)]
        yes: bool,
    },
    /// Sync into a harness again
    Enable {
        name: String,
        #[command(flatten)]
        opts: ChangeOpts,
    },
    /// Stop syncing into a harness (what sk1ll placed there stays; `sk1ll assign <name> --all off` first takes it out)
    Disable {
        name: String,
        #[command(flatten)]
        opts: ChangeOpts,
    },
    /// Add a custom harness, or change one (or a preset's paths) in machine.toml; options left out keep their value
    #[command(after_help = "Examples:\n  sk1ll harnesses add agents --skills ~/.agents/skills\n  \
                            sk1ll harnesses add mytool --skills /opt/mytool/skills --mode copy \\\n    \
                            --instructions ~/.mytool/AGENTS.md")]
    Add {
        name: String,
        /// Skills directory: absolute or ~/-relative; '' for none
        #[arg(long)]
        skills: Option<String>,
        /// Global instructions file (block format) or rules directory (rules format); '' for none
        #[arg(long)]
        instructions: Option<String>,
        /// Instructions format (default for a new harness: block)
        #[arg(long, value_enum)]
        format: Option<Format>,
        /// How skills are placed: symlinks to the repo, or marked copies (default for a new harness: symlink)
        #[arg(long, value_enum)]
        mode: Option<Mode>,
        #[command(flatten)]
        opts: ChangeOpts,
    },
    /// Remove a harness's machine.toml entry: a custom harness goes away, a preset returns to its defaults
    Remove {
        name: String,
        #[command(flatten)]
        opts: ChangeOpts,
    },
}

#[derive(Args)]
pub(crate) struct ChangeOpts {
    /// Don't ask for confirmation
    #[arg(long, short)]
    yes: bool,
}

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum Format {
    Block,
    Rules,
}

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum Mode {
    Symlink,
    Copy,
}

impl HarnessCmd {
    /// The change and its options; `add` starts from `current`, the harness as it is now.
    fn split(self, current: Option<Harness>) -> (HarnessChange, ChangeOpts) {
        match self {
            Self::Rename { .. } => unreachable!("renames are applied separately"),
            Self::Enable { name, opts } => (HarnessChange::Enable { name }, opts),
            Self::Disable { name, opts } => (HarnessChange::Disable { name }, opts),
            Self::Remove { name, opts } => (HarnessChange::Remove { name }, opts),
            Self::Add { name, skills, instructions, format, mode, opts } => {
                let mut harness = current.unwrap_or_default();
                let path = |v: String| Some(v).filter(|v| !v.is_empty());
                if let Some(v) = skills {
                    harness.skills = path(v);
                }
                if let Some(v) = instructions {
                    harness.instructions = path(v);
                }
                match format {
                    Some(Format::Block) => harness.instructions_format = InstructionsFormat::Block,
                    Some(Format::Rules) => harness.instructions_format = InstructionsFormat::Rules,
                    None => {}
                }
                match mode {
                    Some(Mode::Symlink) => harness.mode = LinkMode::Symlink,
                    Some(Mode::Copy) => harness.mode = LinkMode::Copy,
                    None => {}
                }
                (HarnessChange::Set { name, harness }, opts)
            }
        }
    }
}

/// `sk1ll harnesses [--all] [<command>]`.
pub(crate) fn run(s: &Session, all: bool, command: Option<HarnessCmd>) -> Result<()> {
    let Some(command) = command else {
        let settings = s.block(s.client.harness_settings())?;
        let effective = s.block(s.client.harnesses())?;
        let listing =
            Listing { machine: &s.status.machine_id, config: &settings.path, effective, choices: settings.harnesses };
        return list(&listing, all, &mut std::io::stdout());
    };
    if let HarnessCmd::Rename { old, new, replace, .. } = command {
        let request = sk1ll_core::harness_rename::RenameRequest { from: old, to: new, replace };
        let (from, to) = (request.from.clone(), request.to.clone());
        let renamed = s.block(s.client.rename_harness(&request))?;
        print_routed_rules(&renamed.routed_rules, &from);
        println!("renamed harness {from} to {to}");
        for error in &renamed.update.errors {
            eprintln!("{error}");
        }
        if !renamed.update.errors.is_empty() {
            bail!("could not apply all placements");
        }
        return Ok(());
    }
    let current = match &command {
        HarnessCmd::Add { name, .. } => {
            s.block(s.client.harness_settings())?.harnesses.into_iter().find(|c| c.name == *name).map(|c| c.harness)
        }
        _ => None,
    };
    let (change, opts) = command.split(current);
    let view = |p: &PlacementView, placed: bool| Row {
        kind: p.kind.clone(),
        harness: p.harness.clone(),
        what: format!("{} {}", p.item, p.name),
        dest: p.dest.clone(),
        reason: p.reason.clone(),
        placed,
    };
    let mut req = HarnessChangeRequest { change, fingerprint: None };
    let preview = s.block(s.client.preview_harness(&req))?;
    let rows: Vec<Row> =
        preview.placed.iter().map(|p| view(p, true)).chain(preview.sync.iter().map(|p| view(p, false))).collect();
    for warning in &preview.warnings {
        eprintln!("warning: {warning}");
    }
    print_preview(&preview.path, &req.change, &rows, preview.awaiting_assignment);
    decide(&opts)?;
    req.fingerprint = Some(preview.fingerprint);
    let applied = s.block(s.client.apply_harness(&req))?;
    for e in &applied.errors {
        eprintln!("error: {e}");
    }
    if !applied.errors.is_empty() {
        bail!("machine.toml changed, but {} step(s) failed", applied.errors.len());
    }
    println!("harnesses in sync");
    Ok(())
}

fn print_routed_rules(rules: &[String], old: &str) {
    if !rules.is_empty() {
        eprintln!("Rules still routed to {old}. Update their harnesses frontmatter: {}", rules.join(", "));
    }
}

/// What `sk1ll harnesses` lists.
struct Listing<'a> {
    machine: &'a str,
    config: &'a Path,
    /// The harnesses sync writes into.
    effective: Vec<HarnessView>,
    choices: Vec<HarnessChoice>,
}

/// The harnesses sync writes into; with `all`, also those it doesn't.
fn list(listing: &Listing, all: bool, out: &mut dyn Write) -> Result<()> {
    writeln!(out, "machine {} ({})", listing.machine, listing.config.display())?;
    for e in &listing.effective {
        writeln!(out, "{}", line(&e.name, &e.origin, &e.harness))?;
    }
    let choices = &listing.choices;
    let available: Vec<_> = choices.iter().filter(|c| c.available && (c.installed || c.enabled)).collect();
    if !all {
        if !available.is_empty() {
            let names = available.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ");
            eprintln!("presets available, off until enabled: {names} (see `sk1ll harnesses --all`)");
        }
        return Ok(());
    }
    for c in choices.iter().filter(|c| !c.enabled) {
        let state = match (c.installed, c.available) {
            (false, _) => "not found",
            (true, true) => "available",
            (true, false) => "disabled",
        };
        writeln!(out, "{}", line(&c.name, state, &c.harness))?;
    }
    for c in &available {
        let what = if c.enabled { "can also sync its skills directory" } else { "is an opt-in preset" };
        writeln!(out, "note: {} {what}, off until `sk1ll harnesses enable {}` (which previews it)", c.name, c.name)?;
    }
    Ok(())
}

fn line(name: &str, origin: &str, h: &Harness) -> String {
    let mode = match h.mode {
        LinkMode::Symlink => "symlink",
        LinkMode::Copy => "copy",
    };
    let format = match h.instructions_format {
        InstructionsFormat::Block => "block",
        InstructionsFormat::Rules => "rules",
    };
    let skills = h.skills.as_deref().map(|s| format!("skills {s} ({mode})")).unwrap_or_default();
    let instr = h.instructions.as_deref().map(|i| format!("instructions {i} ({format})"));
    format!("{name:<10} {origin:<9} {skills:<40} {}", instr.unwrap_or_default())
}

/// One line of a preview: `kind` is `create`, `update`, `remove` or `conflict`.
struct Row {
    kind: String,
    harness: String,
    what: String,
    dest: PathBuf,
    reason: Option<String>,
    /// Placed where the change stops syncing, rather than what sync does next.
    placed: bool,
}

fn print_rows(rows: &[&Row]) {
    for r in rows {
        let sym = match r.kind.as_str() {
            "create" => "+",
            "update" => "~",
            "remove" => "-",
            _ => "!",
        };
        let reason = r.reason.as_deref().map(|r| format!("  ({r})")).unwrap_or_default();
        println!("  {sym} {:<10} {:<24} {}{reason}", r.harness, r.what, r.dest.display());
    }
}

fn print_preview(config: &Path, change: &HarnessChange, rows: &[Row], awaiting_assignment: bool) {
    println!("changes {}", config.display());
    let placed: Vec<_> = rows.iter().filter(|r| r.placed).collect();
    if !placed.is_empty() {
        println!("sk1ll placed this where the harness stops syncing; it stays there, no longer managed:");
        print_rows(&placed);
        if placed.iter().any(|r| r.kind == "remove") {
            let name = change.name();
            println!(
                "to take it out instead, cancel and turn the harness's items off first: \
                 `sk1ll assign {name} --all off` (which syncs), then run this again"
            );
        }
    }
    let sync: Vec<_> = rows.iter().filter(|r| !r.placed).collect();
    if !sync.is_empty() {
        println!("then sync does:");
        print_rows(&sync);
    } else if awaiting_assignment {
        println!("no assignment yet: the harness is left alone until it arrives");
    }
}

/// Confirms the change.
fn decide(opts: &ChangeOpts) -> Result<()> {
    if opts.yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        bail!("not a terminal: pass --yes to make this change");
    }
    if !confirm("make this change?")? {
        bail!("aborted");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use sk1ll_core::{MachineConfig, Repo};

    use super::*;

    fn direct_root() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        Repo::init(&repo).unwrap();
        fs::create_dir_all(repo.join("personal/pdf")).unwrap();
        fs::write(repo.join("personal/pdf/SKILL.md"), "PDFs.").unwrap();
        let presets: String =
            sk1ll_core::machine::presets().map(|p| format!("[harnesses.{p}]\nenabled = false\n")).collect();
        fs::write(tmp.path().join("machine.toml"), presets).unwrap();
        let root = tmp.path().to_path_buf();
        (tmp, root)
    }

    #[test]
    fn listing_shows_what_sync_writes_into_unless_all() {
        let (_tmp, root) = direct_root();
        let skills = |n: &str| root.join(n).to_string_lossy().into_owned();
        let text = format!(
            "{}[harnesses.on]\nskills = {:?}\n[harnesses.off]\nskills = {:?}\nenabled = false\n",
            fs::read_to_string(root.join("machine.toml")).unwrap(),
            skills("on"),
            skills("off")
        );
        fs::write(root.join("machine.toml"), text).unwrap();
        let listed = |all: bool| {
            let config = root.join("machine.toml");
            let machine = MachineConfig::load(&config).unwrap();
            let effective = machine
                .effective()
                .into_iter()
                .map(|(name, e)| HarnessView { name, harness: e.harness, origin: "local".into() })
                .collect();
            let listing = Listing { machine: &machine.id, config: &config, effective, choices: machine.choices() };
            let mut out = Vec::new();
            list(&listing, all, &mut out).unwrap();
            String::from_utf8(out).unwrap()
        };
        let names = |text: &str| {
            text.lines().skip(1).map(|l| l.split_whitespace().next().unwrap().to_string()).collect::<Vec<_>>()
        };
        assert_eq!(names(&listed(false)), ["on"]);
        let all = listed(true);
        assert_eq!(names(&all)[0], "on", "{all}");
        assert!(all.lines().any(|l| l.starts_with("off ") && l.contains("disabled")), "{all}");
        // Every inactive choice, a configured preset not installed here among them.
        for preset in sk1ll_core::machine::presets() {
            assert!(all.lines().any(|l| l.starts_with(&format!("{preset} "))), "{preset}: {all}");
        }
    }

    #[test]
    fn add_changes_only_the_given_options() {
        let cmd = HarnessCmd::Add {
            name: "agents".into(),
            skills: Some("~/.agents/skills".into()),
            instructions: None,
            format: Some(Format::Rules),
            mode: None,
            opts: ChangeOpts { yes: true },
        };
        let current = Harness { instructions: Some("/rules".into()), mode: LinkMode::Copy, ..Harness::default() };
        let (change, _) = cmd.split(Some(current));
        let HarnessChange::Set { name, harness } = change else { panic!("{change:?}") };
        assert_eq!(name, "agents");
        assert_eq!(harness.skills.as_deref(), Some("~/.agents/skills"));
        assert_eq!(harness.instructions.as_deref(), Some("/rules"), "left out: kept");
        assert_eq!(harness.instructions_format, InstructionsFormat::Rules);
        assert_eq!(harness.mode, LinkMode::Copy);
    }
}
