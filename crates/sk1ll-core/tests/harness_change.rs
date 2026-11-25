//! Previewing and applying harness changes (`sk1ll_core::harness_change`)
//! against what sync placed.

mod common;

use common::Scratch;
use sk1ll_core::harness_change::*;
use sk1ll_core::{ActionKind, Assignment, Harness, HarnessChange, LinkMode, MachineConfig, PlanOptions, Repo, sync};
use sk1ll_testkit::{SkillsRepo, write};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// A repo with skill `pdf` and rule `git`, and a machine.toml whose only
/// harness `h` has copied skills and a block instructions file.
fn setup() -> (Scratch, Repo, PathBuf, sk1ll_core::EngineLock) {
    let tmp = Scratch::new();
    let fixture = SkillsRepo::init(tmp.join("repo"));
    fixture.write("personal/pdf/SKILL.md", "PDFs.\n");
    fixture.write("instructions/git.md", "Git.\n");
    let repo = fixture.open();
    let config = tmp.join("machine.toml");
    let presets: String =
        sk1ll_core::machine::presets().map(|p| format!("[harnesses.{p}]\nenabled = false\n")).collect();
    write(
        &config,
        &format!(
            "# my harnesses\n[harnesses.h]\nskills = {:?}\ninstructions = {:?}\nmode = \"copy\"\n{presets}",
            tmp.join("h/skills"),
            tmp.join("h/AGENTS.md"),
        ),
    );
    let lock = sk1ll_core::EngineLock::acquire(repo.root()).unwrap();
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
    let skill = tmp.join("h/skills/pdf");
    let agents = tmp.join("h/AGENTS.md");
    write(&tmp.join("h/skills/mine/SKILL.md"), "Unmanaged.\n");

    let change = HarnessChange::Disable { name: "h".into() };
    let preview = plan(&repo, &config, &change, Some(&Assignment::default())).unwrap();
    assert!(preview.after.is_none() && preview.sync.is_empty());
    let kinds: Vec<_> = preview.placed.iter().map(|a| (a.dest.clone(), a.kind)).collect();
    assert_eq!(kinds, [(skill.clone(), ActionKind::Remove), (agents.clone(), ActionKind::Remove)]);
    assert!(fs::read_to_string(&config).unwrap().contains("mode = \"copy\"\n"), "a preview writes nothing");

    apply(&lock, &repo, &config, &change, &preview.fingerprint).unwrap();
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
        let path = tmp.join("h/AGENTS.md");
        if case == "malformed" {
            write(&path, "<!-- 0xSk1ll:begin broken -->\n");
        } else if case == "invalid UTF-8" {
            fs::write(&path, [0xff]).unwrap();
        } else {
            fs::remove_file(&path).unwrap();
            let fifo = if case == "fifo" { path.clone() } else { tmp.join("fifo") };
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
        let disable_preview = plan(&repo, &config, &disable, None).unwrap();
        assert_eq!(
            apply(&lock, &repo, &config, &disable, &disable_preview.fingerprint).unwrap().warnings.len(),
            1,
            "{case}"
        );
        MachineConfig::write_change(&config, &HarnessChange::Enable { name: "h".into() }).unwrap();
        let preview = plan(&repo, &config, &remove, None).unwrap();
        let removed = apply(&lock, &repo, &config, &remove, &preview.fingerprint).unwrap();
        assert_eq!(removed.warnings.len(), 1, "{case}");
        assert!(removed.after.is_none());
    }
}

#[test]
fn turning_the_items_off_first_lets_sync_take_them_out() {
    let (tmp, repo, config, lock) = setup();
    repo.assign_missing_ids().unwrap();
    let repo = Repo::open(repo.root()).unwrap();
    sync_all(&repo, &config, &Assignment::default());
    let mut off = Assignment::default();
    for (item, id) in repo.item_ids().unwrap() {
        let id = id.expect("an Item ID");
        match item {
            sk1ll_core::ItemRef::Skill(_) => off.set_skill("h", &id, false),
            _ => off.set_rule("h", &id, false),
        }
    }
    off.set_new_skills_default("h", false);
    off.set_new_rules_default("h", false);
    sync_all(&repo, &config, &off);
    let change = HarnessChange::Disable { name: "h".into() };
    let preview = plan(&repo, &config, &change, Some(&off)).unwrap();
    assert!(preview.placed.is_empty());
    apply(&lock, &repo, &config, &change, &preview.fingerprint).unwrap();
    assert!(!tmp.join("h/skills/pdf").exists());
    assert!(!fs::read_to_string(tmp.join("h/AGENTS.md")).unwrap_or_default().contains("Git."));
}

#[test]
fn moving_a_path_lists_only_the_old_location() {
    let (tmp, repo, config, lock) = setup();
    sync_all(&repo, &config, &Assignment::default());
    let moved = Harness {
        skills: Some(tmp.join("h/new-skills").to_string_lossy().into_owned()),
        instructions: Some(tmp.join("h/AGENTS.md").to_string_lossy().into_owned()),
        mode: LinkMode::Copy,
        ..Harness::default()
    };
    let change = HarnessChange::Set { name: "h".into(), harness: moved };
    let preview = plan(&repo, &config, &change, Some(&Assignment::default())).unwrap();
    assert_eq!(preview.placed.iter().map(|a| a.dest.clone()).collect::<Vec<_>>(), [tmp.join("h/skills/pdf")]);
    assert_eq!(
        preview.sync.iter().map(|a| (a.dest.clone(), a.kind)).collect::<Vec<_>>(),
        [(tmp.join("h/new-skills/pdf"), ActionKind::Create)]
    );
    apply(&lock, &repo, &config, &change, &preview.fingerprint).unwrap();
    assert!(tmp.join("h/skills/pdf/SKILL.md").is_file(), "left in place");
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

#[test]
fn sync_keeps_explicit_toggles_when_defaults_flip_and_applies_a_whole_harness_toggle() {
    let (tmp, repo, config, _) = setup();
    repo.assign_missing_ids().unwrap();
    let (skill, rule) = (repo.skills().unwrap().remove(0).id.unwrap(), repo.rules().unwrap().remove(0).id.unwrap());
    let assignment_path = tmp.join("assignment.json");
    let mut assignment = Assignment::default();
    assignment.set_rule("h", &rule, false);
    assignment.materialize_skills("h", std::slice::from_ref(&skill));
    assignment.materialize_rules("h", std::slice::from_ref(&rule));
    assignment.set_new_skills_default("h", false);
    assignment.set_new_rules_default("h", true);
    let pdf = tmp.join("h/skills/pdf/SKILL.md");
    let agents = tmp.join("h/AGENTS.md");
    for (toggle, skill_exists, rule_exists) in
        [(None, true, false), (Some(false), false, false), (Some(true), true, true)]
    {
        if let Some(enabled) = toggle {
            assignment.set_harness("h", enabled);
        }
        assignment.save(&assignment_path).unwrap();
        sync_all(&repo, &config, &Assignment::load(&assignment_path).unwrap());
        assert_eq!(pdf.is_file(), skill_exists);
        assert_eq!(fs::read_to_string(&agents).unwrap_or_default().contains("Git."), rule_exists);
    }
}

#[test]
fn syncing_a_block_replaces_and_removes_only_the_managed_lines() {
    let (tmp, repo, config, _) = setup();
    let path = tmp.join("h/AGENTS.md");
    write(&path, "# Mine\n\nKeep this.\n");
    sync_all(&repo, &config, &Assignment::default());
    let installed = fs::read_to_string(&path).unwrap();
    assert!(installed.starts_with("# Mine\n\nKeep this.\n\n<!-- 0xSk1ll:begin sha256:"));
    assert!(installed.ends_with("<!-- 0xSk1ll:end -->\n"));
    write(&path, &format!("{installed}\n# After\n"));
    write(&repo.root().join("instructions/git.md"), "Updated Git.\n");
    sync_all(&repo, &config, &Assignment::default());
    let updated = fs::read_to_string(&path).unwrap();
    assert!(updated.contains("Updated Git.") && updated.ends_with("\n# After\n"));

    let edited = updated.replace("Updated Git.", "Edited by hand.");
    write(&path, &edited);
    let harnesses = MachineConfig::load(&config).unwrap().harnesses();
    let actions = sync::plan(&repo, &harnesses, &Assignment::default(), PlanOptions::default()).unwrap();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].kind, ActionKind::Conflict);
    assert_eq!(fs::read_to_string(&path).unwrap(), edited);

    write(&path, &updated);
    let mut off = Assignment::default();
    off.set_harness("h", false);
    sync_all(&repo, &config, &off);
    assert_eq!(fs::read_to_string(&path).unwrap(), "# Mine\n\nKeep this.\n\n# After\n");
}

#[test]
fn adopting_a_rule_preserves_routing_and_item_id_while_taking_the_harness_edit() {
    let tmp = Scratch::new();
    let fixture = SkillsRepo::init(tmp.join("repo"));
    let repo = fixture.open();
    let dest = tmp.join("rules");
    let harnesses = [(
        "h".into(),
        Harness {
            instructions: Some(dest.to_str().unwrap().into()),
            instructions_format: sk1ll_core::InstructionsFormat::Rules,
            ..Harness::default()
        },
    )]
    .into();
    let plan = || sync::plan(&repo, &harnesses, &Assignment::default(), PlanOptions::default()).unwrap();

    for (name, metadata, routing) in [
        ("plain", "", "harnesses: [h]\n"),
        ("paths", "paths: [src/**]\n", "harnesses:\n  - h\n"),
        ("alias", "description: &tool h\n", "harnesses: *tool\n"),
        ("escaped", "description: Keep\n", "\"harn\\u0065sses\": [h]\n"),
        ("scalar", "description: |\n  targets: documentation\n", "targets: [h]\n"),
    ] {
        let rel = format!("instructions/{name}.md");
        fixture.write(&rel, &format!("---\n{metadata}{routing}id: {name}-id\n---\nBefore.\n"));
        let action = plan().into_iter().find(|a| a.dest == dest.join(format!("{name}.md"))).unwrap();
        sync::apply(&repo, &action).unwrap();
        let run = fs::read_to_string(&action.dest).unwrap();
        assert_eq!(
            run,
            if metadata.is_empty() { "Before.\n".into() } else { format!("---\n{metadata}---\n\nBefore.\n") }
        );
        write(&action.dest, &run.replace("Before.", "After."));
        let conflict = plan().into_iter().find(|a| a.dest == action.dest).unwrap();
        assert_eq!(conflict.kind, ActionKind::Conflict);
        sync::adopt(&repo, &conflict).unwrap();
        let restored = repo.rules().unwrap().into_iter().find(|rule| rule.name == name).unwrap();
        assert!(restored.applies_to("h") && !restored.applies_to("other"));
        assert_eq!(restored.id.unwrap().to_string(), format!("{name}-id"));
        assert_eq!(restored.body, "After.");
        assert!(fixture.read(&rel).contains(metadata));
    }
}
