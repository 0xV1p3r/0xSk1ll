//! `sk1ll harnesses`: list this machine's harnesses, and change them in
//! `machine.toml` after showing what sk1ll placed where the change stops
//! syncing (it stays there) and what sync does next. A change goes through
//! the daemon when it manages the repo, otherwise it is made directly under
//! the engine lock and synced.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use sk1ll_core::harness_change;
use sk1ll_core::{
    Action, ActionKind, Harness, HarnessChange, HarnessOrigin, InstructionsFormat, Item, LinkMode, MachineConfig,
    PlanOptions, Repo, sync,
};

use crate::{Cli, confirm};

#[derive(Subcommand)]
pub(crate) enum HarnessCmd {
    /// Rename a harness and move its Assignment
    Rename { old: String, new: String },
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
pub(crate) fn run(mut cli: Cli, repo: &Path, machine_path: &Path, assignment_path: &Path) -> Result<()> {
    let crate::Cmd::Harnesses { command, all } = &mut cli.cmd else { unreachable!() };
    let all = *all;
    let command = command.take();
    let Some(command) = command else { return list(machine_path, all, &mut std::io::stdout()) };
    if let HarnessCmd::Rename { old, new } = command {
        return rename(&cli, repo, machine_path, assignment_path, &old, &new);
    }
    let current = match &command {
        HarnessCmd::Add { name, .. } => {
            MachineConfig::load(machine_path)?.choices().into_iter().find(|c| c.name == *name).map(|c| c.harness)
        }
        _ => None,
    };
    let (change, opts) = command.split(current);
    #[cfg(unix)]
    if let Some(daemon) = crate::fleet::connect(&cli, repo)? {
        return via_daemon(daemon, change, opts);
    }
    let Some(lock) = crate::lock_direct(repo)? else {
        bail!("{} is not a git repo; harnesses can be changed once the skills repo exists", repo.display());
    };
    let repo = Repo::open(repo)?;
    // `None` only for a joined machine still waiting for its assignment;
    // anything unreadable stops here, before machine.toml changes.
    let assignment = crate::fleet::assignment_if_present(cli.daemon_state.as_deref(), assignment_path)?;
    let plan = harness_change::plan(&repo, machine_path, &change, assignment.as_ref())?;
    let rows: Vec<Row> =
        plan.placed.iter().map(|a| row(a, true)).chain(plan.sync.iter().map(|a| row(a, false))).collect();
    for warning in &plan.warnings {
        eprintln!("warning: {warning}");
    }
    print_preview(machine_path, &change, &rows, assignment.is_none(), true);
    decide(&opts)?;
    harness_change::apply(&lock, &repo, machine_path, &change, &plan.fingerprint)?;
    let mut failed = 0;
    match assignment {
        None => println!("no assignment yet; the harnesses sync once the daemon has the server's"),
        Some(assignment) => {
            let harnesses = MachineConfig::load(machine_path)?.harnesses();
            sync::recover(&lock, &harnesses)?;
            repo.recover_imports()?;
            for a in sync::plan(&repo, &harnesses, &assignment, PlanOptions::default())? {
                if a.kind != ActionKind::Conflict
                    && let Err(e) = sync::apply(&repo, &a)
                {
                    eprintln!("error: {}: {e}", a.dest.display());
                    failed += 1;
                }
            }
        }
    }
    if failed > 0 {
        bail!("machine.toml changed, but {failed} step(s) failed");
    }
    println!("harnesses in sync");
    Ok(())
}

fn rename(cli: &Cli, repo: &Path, machine: &Path, cache: &Path, from: &str, to: &str) -> Result<()> {
    #[cfg(unix)]
    if let Some(daemon) = crate::fleet::connect(cli, repo)? {
        let rename = sk1ll_proto::HarnessRename { from: from.into(), to: to.into() };
        let update = crate::fleet::runtime()?.block_on(daemon.client.rename_harness(&rename))?;
        for error in &update.errors {
            eprintln!("{error}");
        }
        println!("renamed harness {from} to {to}");
        return Ok(());
    }
    let lock = crate::lock_direct(repo)?.ok_or_else(|| anyhow::anyhow!("{} is not a git repo", repo.display()))?;
    let snapshot = sk1ll_core::config_file::Snapshot::take(machine)?;
    let edit = MachineConfig::rename_harness(&snapshot, from, to)?;
    #[cfg(unix)]
    if let Some(state) = crate::fleet::joined_state(cli).filter(|s| !s.revoked) {
        crate::fleet::check_joined_repo(repo, &state)?;
        let cfg = crate::fleet::daemon_config(cli)?;
        let write = sk1ll_proto::AssignmentWrite::RenameHarness(sk1ll_proto::HarnessRename {
            from: from.into(),
            to: to.into(),
        });
        crate::fleet::runtime()?.block_on(sk1ll_daemon::write_assignment_direct(&cfg, &lock, write))?;
        edit.write(&snapshot).map_err(|e| {
            anyhow::anyhow!("the Server moved the harness Assignment, but editing machine.toml failed: {e}")
        })?;
        println!("renamed harness {from} to {to}; run `sk1ll sync` to apply");
        return Ok(());
    }
    let _ = (cli, lock);
    let before = sk1ll_core::Assignment::load(cache)?;
    let mut assignment = before.clone();
    assignment.rename_harness(from, to);
    assignment.save(cache)?;
    if let Err(e) = edit.write(&snapshot) {
        before.save(cache)?;
        return Err(e.into());
    }
    println!("renamed harness {from} to {to}; run `sk1ll sync` to apply");
    Ok(())
}

/// The harnesses sync writes into; with `all`, also those it doesn't.
fn list(machine_path: &Path, all: bool, out: &mut dyn Write) -> Result<()> {
    let machine = MachineConfig::load(machine_path)?;
    writeln!(out, "machine {} ({})", machine.id, machine_path.display())?;
    let effective = machine.effective();
    for (name, e) in &effective {
        let origin = match e.origin {
            HarnessOrigin::Detected => "detected",
            HarnessOrigin::Override => "override",
            HarnessOrigin::Local => "local",
        };
        writeln!(out, "{}", line(name, origin, &e.harness))?;
    }
    let choices = machine.choices();
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

fn row(a: &Action, placed: bool) -> Row {
    let what = match &a.item {
        Item::Skill { name, .. } => format!("skill {name}"),
        Item::Rule { file, .. } => format!("rule {file}"),
        Item::Instructions { rules, .. } => format!("instructions [{}]", rules.join(", ")),
    };
    let kind = match a.kind {
        ActionKind::Create => "create",
        ActionKind::Update => "update",
        ActionKind::Remove => "remove",
        ActionKind::Conflict => "conflict",
    };
    Row { kind: kind.into(), harness: a.harness.clone(), what, dest: a.dest.clone(), reason: a.reason.clone(), placed }
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

/// `direct`: no daemon syncs by itself, so the hint says to.
fn print_preview(config: &Path, change: &HarnessChange, rows: &[Row], awaiting_assignment: bool, direct: bool) {
    println!("changes {}", config.display());
    let placed: Vec<_> = rows.iter().filter(|r| r.placed).collect();
    if !placed.is_empty() {
        println!("sk1ll placed this where the harness stops syncing; it stays there, no longer managed:");
        print_rows(&placed);
        if placed.iter().any(|r| r.kind == "remove") {
            let name = change.name();
            let sync = if direct { ", `sk1ll sync`" } else { " (the daemon syncs)" };
            println!(
                "to take it out instead, cancel and turn the harness's items off first: \
                 `sk1ll assign {name} --all off`{sync}, then run this again"
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

#[cfg(unix)]
fn via_daemon(daemon: crate::fleet::Daemon, change: HarnessChange, opts: ChangeOpts) -> Result<()> {
    use sk1ll_daemon::client::{HarnessChangeRequest, PlacementView};
    let (rt, client) = daemon.into_parts();
    let view = |p: &PlacementView, placed: bool| Row {
        kind: p.kind.clone(),
        harness: p.harness.clone(),
        what: format!("{} {}", p.item, p.name),
        dest: p.dest.clone(),
        reason: p.reason.clone(),
        placed,
    };
    let mut req = HarnessChangeRequest { change, fingerprint: None };
    let preview = rt.block_on(client.preview_harness(&req))?;
    let rows: Vec<Row> =
        preview.placed.iter().map(|p| view(p, true)).chain(preview.sync.iter().map(|p| view(p, false))).collect();
    for warning in &preview.warnings {
        eprintln!("warning: {warning}");
    }
    print_preview(&preview.path, &req.change, &rows, preview.awaiting_assignment, false);
    decide(&opts)?;
    req.fingerprint = Some(preview.fingerprint);
    let applied = rt.block_on(client.apply_harness(&req))?;
    for e in &applied.errors {
        eprintln!("error: {e}");
    }
    if !applied.errors.is_empty() {
        bail!("machine.toml changed, but {} step(s) failed", applied.errors.len());
    }
    println!("the daemon synced the harnesses");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use clap::Parser;

    use super::*;

    /// Runs `sk1ll <args>` on `root` directly (no daemon socket there).
    fn sk1ll(root: &Path, args: &[&str]) -> Result<()> {
        let path = |p: &str| root.join(p).to_string_lossy().into_owned();
        let base = [
            "sk1ll".to_string(),
            "--repo".into(),
            path("repo"),
            "--machine-config".into(),
            path("machine.toml"),
            "--assignment".into(),
            path("assignment.json"),
            "--daemon-socket".into(),
            path("none.sock"),
            "--daemon-state".into(),
            path("daemon.json"),
        ];
        crate::run(Cli::try_parse_from(base.into_iter().chain(args.iter().map(|a| a.to_string())))?)
    }

    #[test]
    fn direct_changes_preview_apply_and_sync() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let repo = root.join("repo");
        Repo::init(&repo).unwrap();
        fs::create_dir_all(repo.join("personal/pdf")).unwrap();
        fs::write(repo.join("personal/pdf/SKILL.md"), "PDFs.").unwrap();
        let presets: String =
            sk1ll_core::machine::presets().map(|p| format!("[harnesses.{p}]\nenabled = false\n")).collect();
        fs::write(root.join("machine.toml"), format!("# keep me\n{presets}")).unwrap();
        let skills = root.join("h/skills");

        let add = ["harnesses", "add", "h", "--skills", skills.to_str().unwrap(), "--mode", "copy", "--yes"];
        sk1ll(root, &add).unwrap();
        assert!(skills.join("pdf/SKILL.md").is_file(), "adding a harness syncs it");
        assert!(fs::read_to_string(root.join("machine.toml")).unwrap().starts_with("# keep me\n"));
        sk1ll(root, &["harnesses"]).unwrap();

        sk1ll(root, &["harnesses", "disable", "h", "--yes"]).unwrap();
        assert!(skills.join("pdf").is_dir(), "left in place");
        sk1ll(root, &["harnesses", "enable", "h", "--yes"]).unwrap();
        // Its items off first: sync takes out what it placed, then disabling leaves nothing.
        sk1ll(root, &["assign", "h", "--all", "off"]).unwrap();
        sk1ll(root, &["sync"]).unwrap();
        assert!(!skills.join("pdf").exists());
        sk1ll(root, &["harnesses", "disable", "h", "--yes"]).unwrap();
        let assignment = sk1ll_core::Assignment::load(&root.join("assignment.json")).unwrap();
        assert!(
            !assignment.skill_enabled("h", &sk1ll_core::ItemId::derived("skill", "pdf"))
                && !assignment.skill_enabled("h", &sk1ll_core::ItemId::derived("skill", "added-later"))
        );

        sk1ll(root, &["harnesses", "remove", "h", "--yes"]).unwrap();
        assert!(!fs::read_to_string(root.join("machine.toml")).unwrap().contains("[harnesses.h]"));
        assert!(sk1ll(root, &["harnesses", "remove", "h", "--yes"]).is_err());
        assert!(sk1ll(root, &["harnesses", "add", "x", "--skills", "relative", "--yes"]).is_err());
    }

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
            let mut out = Vec::new();
            list(&root.join("machine.toml"), all, &mut out).unwrap();
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
    fn direct_changes_stop_on_an_unreadable_assignment_and_report_failed_placements() {
        let (_tmp, root) = direct_root();
        let config = root.join("machine.toml");
        let before = fs::read_to_string(&config).unwrap();
        let skills = root.join("h/skills");
        let add = ["harnesses", "add", "h", "--skills", skills.to_str().unwrap(), "--mode", "copy", "--yes"];

        fs::write(root.join("assignment.json"), "{ not json").unwrap();
        assert!(sk1ll(&root, &add).is_err());
        assert_eq!(fs::read_to_string(&config).unwrap(), before, "nothing changes when the assignment can't be read");
        fs::remove_file(root.join("assignment.json")).unwrap();

        // A skills directory sync can read but not write into.
        fs::create_dir_all(&skills).unwrap();
        fs::set_permissions(&skills, std::os::unix::fs::PermissionsExt::from_mode(0o555)).unwrap();
        let err = sk1ll(&root, &add).unwrap_err();
        fs::set_permissions(&skills, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        assert!(err.to_string().contains("machine.toml changed, but 1 step(s) failed"), "{err}");
        assert!(fs::read_to_string(&config).unwrap().contains("[harnesses.h]"), "the change itself was made");
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
