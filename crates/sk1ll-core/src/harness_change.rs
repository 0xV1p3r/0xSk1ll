//! Changing this machine's harnesses after setup, and what that does to the
//! content sync already placed in them.
//!
//! Sync only looks at the harnesses `machine.toml` currently names. Content
//! it placed in a harness that is disabled or removed, or at a path a
//! harness no longer uses, is never revisited: it stays where it is,
//! unmanaged from then on. A change never removes anything. [`plan`] lists
//! that content first, so it can be taken out the usual way before the
//! change: turn the harness's items off in its assignment
//! (`sk1ll assign <harness> --all off`) and let sync remove them, with all
//! its checks for hand edits.
//!
//! A change never touches the assignment: re-enabling a harness brings back
//! its toggles.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config_file::Snapshot;
use crate::machine::{Harness, HarnessChange, InstructionsFormat, MachineConfig, overlaps, real};
use crate::sync::{self, Action, PlanOptions};
use crate::{Assignment, Error, Repo, Result};

/// What a harness change would do, computed without changing anything.
#[derive(Debug, Clone, Serialize)]
pub struct ChangePlan {
    pub change: HarnessChange,
    /// The changed harness as it is now (`None`: not synced into).
    pub before: Option<Harness>,
    /// The changed harness once the change is made (`None`: not synced into).
    pub after: Option<Harness>,
    /// Content sk1ll placed where the change stops syncing, which stays
    /// there: managed items as Removes (what sync would do were they turned
    /// off), hand-edited copies as Conflicts.
    pub placed: Vec<Action>,
    pub warnings: Vec<String>,
    /// What the next sync does in the changed harness, given the assignment.
    pub sync: Vec<Action>,
    /// The new `machine.toml` text.
    #[serde(skip)]
    pub text: String,
    #[serde(skip)]
    snapshot: Snapshot,
}

/// What `change` to the `machine.toml` at `path` would do. Without an
/// `assignment` (a joined machine still waiting for its own), `sync` is empty.
pub fn plan(repo: &Repo, path: &Path, change: &HarnessChange, assignment: Option<&Assignment>) -> Result<ChangePlan> {
    let snapshot = Snapshot::take(path)?;
    let name = change.name();
    let before = MachineConfig::from_snapshot(&snapshot)?.harnesses();
    let edit = MachineConfig::edit(&snapshot, change)?;
    let after = edit.config.harnesses();
    if let Some(h) = after.get(name) {
        check_outside_repo(repo, name, h)?;
    }
    let mut placed = Vec::new();
    let mut warnings = Vec::new();
    if let Some(old) = before.get(name) {
        let left = left_part(old, &after);
        for part in [Harness { instructions: None, ..left.clone() }, Harness { skills: None, ..left }] {
            match sync::placed(repo, name, &part) {
                Ok(items) => placed.extend(items),
                Err(e) => warnings.push(format!("Could not list old placements for {name}: {e}")),
            }
        }
    }
    let sync = match (after.get(name), assignment) {
        (Some(h), Some(a)) => {
            let one = BTreeMap::from([(name.to_string(), h.clone())]);
            sync::plan(repo, &one, a, PlanOptions::default())?
        }
        _ => Vec::new(),
    };
    Ok(ChangePlan {
        change: change.clone(),
        before: before.get(name).cloned(),
        after: after.get(name).cloned(),
        placed,
        warnings,
        sync,
        text: edit.text,
        snapshot,
    })
}

/// Makes `change`: plans it, and writes `machine.toml` only if it is still
/// as read ([`Error::Changed`] otherwise). Nothing placed is removed. The
/// caller syncs afterwards, so the changed harness receives its content.
/// Hold the engine lock.
pub fn apply(_lock: &crate::EngineLock, repo: &Repo, path: &Path, change: &HarnessChange) -> Result<ChangePlan> {
    let plan = plan(repo, path, change, None)?;
    if plan.snapshot.text()? != plan.text {
        plan.snapshot.replace(&plan.text)?;
    }
    Ok(plan)
}

/// The part of `old` that no harness in `after` syncs into any more.
fn left_part(old: &Harness, after: &BTreeMap<String, Harness>) -> Harness {
    let skills = old.skills_dir().map(|p| real(&p));
    let instructions = old.instructions_file().map(|p| (real(&p), old.instructions_format));
    let still = |h: &Harness| -> (Option<PathBuf>, Option<(PathBuf, InstructionsFormat)>) {
        (h.skills_dir().map(|p| real(&p)), h.instructions_file().map(|p| (real(&p), h.instructions_format)))
    };
    let used: Vec<_> = after.values().map(still).collect();
    Harness {
        skills: old.skills.clone().filter(|_| !used.iter().any(|(s, _)| s.is_some() && *s == skills)),
        instructions: old.instructions.clone().filter(|_| !used.iter().any(|(_, i)| i.is_some() && *i == instructions)),
        instructions_format: old.instructions_format,
        mode: old.mode,
    }
}

/// Syncing into the repo itself would mix harness copies with repo content.
fn check_outside_repo(repo: &Repo, name: &str, h: &Harness) -> Result<()> {
    for path in [h.skills_dir(), h.instructions_file()].into_iter().flatten() {
        let path = real(&path);
        if overlaps(&path, repo.root()) {
            return Err(Error::Invalid(format!(
                "`{name}` can't sync into {}: it overlaps the skills repo {}",
                path.display(),
                repo.root().display()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::LinkMode;
    use crate::sync::ActionKind;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// A repo with skill `pdf` and rule `git`, and a machine.toml whose only
    /// harness `h` has copied skills and a block instructions file.
    fn setup() -> (tempfile::TempDir, Repo, PathBuf, crate::EngineLock) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        write(&root.join("sk1ll.toml"), "");
        write(&root.join("personal/pdf/SKILL.md"), "PDFs.\n");
        write(&root.join("instructions/git.md"), "Git.\n");
        crate::git::Git::new(&root).args(["init", "-q"]).text().unwrap();
        let repo = Repo::open(&root).unwrap();
        let config = tmp.path().join("machine.toml");
        let presets: String =
            crate::machine::presets().map(|p| format!("[harnesses.{p}]\nenabled = false\n")).collect();
        write(
            &config,
            &format!(
                "# my harnesses\n[harnesses.h]\nskills = {:?}\ninstructions = {:?}\nmode = \"copy\"\n{presets}",
                tmp.path().join("h/skills"),
                tmp.path().join("h/AGENTS.md"),
            ),
        );
        let lock = crate::EngineLock::acquire(&root).unwrap();
        (tmp, repo, config, lock)
    }

    fn sync_all(repo: &Repo, config: &Path, assignment: &Assignment) {
        let harnesses = MachineConfig::load(config).unwrap().harnesses();
        for a in sync::plan(repo, &harnesses, assignment, PlanOptions::default()).unwrap() {
            sync::apply(repo, &a).unwrap();
        }
    }

    #[test]
    fn disabling_lists_what_sync_placed_and_leaves_it() {
        let (tmp, repo, config, lock) = setup();
        sync_all(&repo, &config, &Assignment::default());
        let skill = tmp.path().join("h/skills/pdf");
        let agents = tmp.path().join("h/AGENTS.md");
        write(&tmp.path().join("h/skills/mine/SKILL.md"), "Unmanaged.\n");

        let change = HarnessChange::Disable { name: "h".into() };
        let preview = plan(&repo, &config, &change, Some(&Assignment::default())).unwrap();
        assert!(preview.after.is_none() && preview.sync.is_empty());
        let kinds: Vec<_> = preview.placed.iter().map(|a| (a.dest.clone(), a.kind)).collect();
        assert_eq!(kinds, [(skill.clone(), ActionKind::Remove), (agents.clone(), ActionKind::Remove)]);
        assert!(fs::read_to_string(&config).unwrap().contains("mode = \"copy\"\n"), "a preview writes nothing");

        apply(&lock, &repo, &config, &change).unwrap();
        assert!(skill.join("SKILL.md").is_file() && fs::read_to_string(&agents).unwrap().contains("Git."));
        let text = fs::read_to_string(&config).unwrap();
        assert!(text.starts_with("# my harnesses\n") && text.contains("enabled = false"), "{text}");
        assert!(!MachineConfig::load(&config).unwrap().harnesses().contains_key("h"));
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_old_instructions_warn_without_blocking_disable_or_remove() {
        for case in ["malformed", "invalid UTF-8", "fifo", "symlink to fifo"] {
            let (tmp, repo, config, lock) = setup();
            sync_all(&repo, &config, &Assignment::default());
            let path = tmp.path().join("h/AGENTS.md");
            if case == "malformed" {
                write(&path, "<!-- 0xSk1ll:begin broken -->\n");
            } else if case == "invalid UTF-8" {
                fs::write(&path, [0xff]).unwrap();
            } else {
                fs::remove_file(&path).unwrap();
                let fifo = if case == "fifo" { path.clone() } else { tmp.path().join("fifo") };
                let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
                if fifo != path {
                    std::os::unix::fs::symlink(fifo, path).unwrap();
                }
            }
            let remove = HarnessChange::Remove { name: "h".into() };
            let preview = plan(&repo, &config, &remove, Some(&Assignment::default())).unwrap();
            assert_eq!(preview.warnings.len(), 1, "{case}");
            assert_eq!(preview.placed.len(), 1, "{case}");
            let disable = HarnessChange::Disable { name: "h".into() };
            assert_eq!(apply(&lock, &repo, &config, &disable).unwrap().warnings.len(), 1, "{case}");
            MachineConfig::write_change(&config, &HarnessChange::Enable { name: "h".into() }).unwrap();
            let removed = apply(&lock, &repo, &config, &remove).unwrap();
            assert_eq!(removed.warnings.len(), 1, "{case}");
            assert!(removed.after.is_none());
        }
    }

    #[test]
    fn turning_the_items_off_first_lets_sync_take_them_out() {
        let (tmp, repo, config, lock) = setup();
        sync_all(&repo, &config, &Assignment::default());
        let mut off = Assignment::default();
        off.set_skill("h", "pdf", false);
        off.set_rule("h", "git", false);
        off.set_new_skills_default("h", false);
        off.set_new_rules_default("h", false);
        sync_all(&repo, &config, &off);
        let change = HarnessChange::Disable { name: "h".into() };
        assert!(plan(&repo, &config, &change, Some(&off)).unwrap().placed.is_empty());
        apply(&lock, &repo, &config, &change).unwrap();
        assert!(!tmp.path().join("h/skills/pdf").exists());
        assert!(!fs::read_to_string(tmp.path().join("h/AGENTS.md")).unwrap_or_default().contains("Git."));
    }

    #[test]
    fn moving_a_path_lists_only_the_old_location() {
        let (tmp, repo, config, lock) = setup();
        sync_all(&repo, &config, &Assignment::default());
        let moved = Harness {
            skills: Some(tmp.path().join("h/new-skills").to_string_lossy().into_owned()),
            instructions: Some(tmp.path().join("h/AGENTS.md").to_string_lossy().into_owned()),
            mode: LinkMode::Copy,
            ..Harness::default()
        };
        let change = HarnessChange::Set { name: "h".into(), harness: moved };
        let preview = plan(&repo, &config, &change, Some(&Assignment::default())).unwrap();
        assert_eq!(
            preview.placed.iter().map(|a| a.dest.clone()).collect::<Vec<_>>(),
            [tmp.path().join("h/skills/pdf")]
        );
        assert_eq!(
            preview.sync.iter().map(|a| (a.dest.clone(), a.kind)).collect::<Vec<_>>(),
            [(tmp.path().join("h/new-skills/pdf"), ActionKind::Create)]
        );
        apply(&lock, &repo, &config, &change).unwrap();
        assert!(tmp.path().join("h/skills/pdf/SKILL.md").is_file(), "left in place");
    }

    #[test]
    fn a_harness_cannot_sync_into_the_repo() {
        let (_tmp, repo, config, _lock) = setup();
        let inside =
            Harness { skills: Some(repo.root().join("personal").to_string_lossy().into_owned()), ..Harness::default() };
        let change = HarnessChange::Set { name: "x".into(), harness: inside };
        let err = plan(&repo, &config, &change, None).unwrap_err();
        assert!(err.to_string().contains("overlaps the skills repo"), "{err}");
    }
}
