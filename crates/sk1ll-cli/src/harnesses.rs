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
    /// Stop syncing into a harness (what sk1ll placed there stays, unless --take-out)
    Disable {
        name: String,
        #[command(flatten)]
        take: TakeOut,
        #[command(flatten)]
        opts: ChangeOpts,
    },
    /// Add a custom harness or preset instance, or change one (or a preset's paths) in machine.toml; options left out keep their value
    #[command(after_help = "Examples:\n  sk1ll harnesses add agents --skills ~/.agents/skills\n  \
                            sk1ll harnesses add mytool --skills /opt/mytool/skills --mode copy \\\n    \
                            --instructions ~/.mytool/AGENTS.md\n  \
                            sk1ll harnesses add claude_2 --preset claude --home ~/.claude_2")]
    Add {
        name: String,
        /// Make it an instance of this preset (e.g. claude), whose paths follow from --home
        #[arg(long)]
        preset: Option<String>,
        /// The instance's config directory (e.g. ~/.claude_2), which the preset's paths are relative to
        #[arg(long)]
        home: Option<String>,
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
        take: TakeOut,
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

#[derive(Args)]
pub(crate) struct TakeOut {
    /// First turn the harness's items off and sync, taking out what sk1ll placed there; the change is made only if that worked
    #[arg(long)]
    take_out: bool,
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
    /// The change and its options, and whether to take out what sk1ll
    /// placed first; `add` starts from `current`, the harness as it is now.
    fn split(self, current: Option<Harness>) -> Result<(HarnessChange, ChangeOpts, bool)> {
        let (change, opts) = match self {
            Self::Rename { .. } => unreachable!("renames are applied separately"),
            Self::Enable { name, opts } => (HarnessChange::Enable { name }, opts),
            Self::Disable { name, take, opts } => return Ok((HarnessChange::Disable { name }, opts, take.take_out)),
            Self::Remove { name, take, opts } => return Ok((HarnessChange::Remove { name }, opts, take.take_out)),
            Self::Add { name, preset, home, skills, instructions, format, mode, opts } => {
                let mut harness = current.unwrap_or_default();
                if preset.is_some() || home.is_some() {
                    let kind = preset.or_else(|| harness.kind.clone());
                    let home = home.or_else(|| harness.home.clone().filter(|_| kind == harness.kind));
                    harness = sk1ll_core::machine::rebase(&name, &harness, kind.as_deref(), home.as_deref())?;
                }
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
        };
        Ok((change, opts, false))
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
    let (change, opts, take_out) = command.split(current)?;
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
    print_preview(&preview.path, &req.change, &rows, preview.awaiting_assignment, take_out);
    decide(&opts)?;
    req.fingerprint = Some(preview.fingerprint);
    if take_out {
        return take_out_and_apply(s, &req);
    }
    let applied = s.block(s.client.apply_harness(&req))?;
    for e in &applied.errors {
        eprintln!("error: {e}");
    }
    if !applied.errors.is_empty() {
        bail!("machine.toml changed, but {} failed", crate::plural(applied.errors.len(), "step", "steps"));
    }
    println!("harnesses in sync");
    Ok(())
}

/// `--take-out`: the engine turns the items off and syncs, then makes the
/// change only if nothing it placed is left.
fn take_out_and_apply(s: &Session, req: &HarnessChangeRequest) -> Result<()> {
    let name = req.change.name();
    let done = s.block(s.client.take_out_harness(req))?;
    if !done.taken_out.is_empty() {
        println!("took out {} from {name}", crate::plural(done.taken_out.len(), "placed item", "placed items"));
    }
    for e in &done.errors {
        eprintln!("error: {e}");
    }
    if !done.applied {
        for p in &done.left {
            eprintln!("still placed: {} {} at {}", p.item, p.name, p.dest.display());
        }
        let why = match done.left.len() {
            0 => "see the errors above".to_string(),
            n => format!("{} still in {name}", crate::plural(n, "placed item is", "placed items are")),
        };
        bail!(
            "left machine.toml as it is: {why}. Its items stay off; fix what failed, then run `sk1ll harnesses {} \
             {name} --take-out` again",
            if matches!(req.change, HarnessChange::Remove { .. }) { "remove" } else { "disable" }
        );
    }
    if !done.errors.is_empty() {
        bail!("machine.toml changed, but {} failed", crate::plural(done.errors.len(), "step", "steps"));
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
    // A preset instance, or a preset moved to another config directory: of what, and where.
    let default_home =
        h.kind.as_deref().and_then(|k| sk1ll_core::machine::preset_instance(k, None)).and_then(|p| p.home);
    let of = match (h.kind.as_deref(), h.home.as_deref()) {
        (Some(kind), Some(home)) if kind != name => format!("  [{kind} in {home}]"),
        (Some(_), Some(home)) if Some(home) != default_home.as_deref() => format!("  [in {home}]"),
        _ => String::new(),
    };
    format!("{name:<10} {origin:<9} {skills:<40} {}{of}", instr.unwrap_or_default())
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

fn print_preview(config: &Path, change: &HarnessChange, rows: &[Row], awaiting_assignment: bool, take_out: bool) {
    println!("changes {}", config.display());
    let placed: Vec<_> = rows.iter().filter(|r| r.placed).collect();
    let (managed, edited): (Vec<&Row>, Vec<&Row>) = placed.iter().partition(|r| r.kind == "remove");
    if take_out {
        let name = change.name();
        if !managed.is_empty() {
            println!(
                "first {name}'s items are turned off and sync takes out what sk1ll placed there (into recovery, as \
                 any removal):"
            );
            print_rows(&managed);
        }
        if !edited.is_empty() {
            println!("edited by hand, so it stays there, no longer managed:");
            print_rows(&edited);
        }
        if !placed.is_empty() {
            println!("the change is made only once that is done");
        }
    } else if !placed.is_empty() {
        println!("sk1ll placed this where the harness stops syncing; it stays there, no longer managed:");
        print_rows(&placed);
        if !managed.is_empty() {
            let name = change.name();
            let verb = if matches!(change, HarnessChange::Remove { .. }) { "remove" } else { "disable" };
            println!("to take it out instead, cancel and run `sk1ll harnesses {verb} {name} --take-out`");
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
