mod common;

use sk1ll_core::{Assignment, ItemRef};
use sk1ll_testkit::{Machine, SkillsRepo, outcome, presets_off, text, write};
use std::{fs, path::Path};

use common::{command, failure, item_id, path, run, with_harnesses, with_pdf};

/// A machine-local `assign` echoes each toggle as a fleet one does and says
/// when it changed nothing; new defaults leave existing items as they are.
#[test]
fn standalone_assign_echoes_toggles_and_defaults_preserve_existing_items() {
    let repo = with_pdf();
    repo.repo.write("instructions/git.md", "Git.");
    let default = run(&repo, &["assign", "h", "--default-rules", "off"]).unwrap();
    assert!(default.starts_with("h: rules default off\n"), "{default}");
    run(&repo, &["assign", "h", "--default-skills", "off"]).unwrap();
    let a = Assignment::load(&repo.assignment()).unwrap();
    let pdf = item_id(&repo, ItemRef::Skill("pdf".into()));
    assert!(a.skill_enabled("h", &pdf));
    assert!(a.rule_enabled("h", &item_id(&repo, ItemRef::Rule("git".into()))));
    // Items that appear later follow the new defaults.
    assert!(!a.harness("h").new_skills_default && !a.harness("h").new_rules_default);

    let off = run(&repo, &["assign", "h", "skill", "pdf", "off"]).unwrap();
    assert!(off.starts_with("h: skill pdf off\nlocal assignment of test is now revision "), "{off}");
    let again = run(&repo, &["assign", "h", "skill", "pdf", "off"]).unwrap();
    assert!(again.contains("h: skill pdf off (already)") && again.contains("unchanged at revision"), "{again}");
    run(&repo, &["assign", "h", "skill", "pdf", "off", "--default-skills", "on"]).unwrap();
    let a = Assignment::load(&repo.assignment()).unwrap();
    assert!(!a.skill_enabled("h", &pdf) && a.harness("h").new_skills_default);
}

/// A copy of a committed skill shares its Item ID; `repair-ids` gives
/// the copy a new one, so the original keeps its toggles and placement.
#[test]
fn repair_ids_keeps_the_id_with_the_baselines_claimant() {
    let mut repo = Machine::new();
    let skills = repo.harness("h");
    repo.repo.write("personal/web/SKILL.md", "Web.");
    run(&repo, &["commit", "web"]).unwrap();
    run(&repo, &["assign", "h", "--all", "off"]).unwrap();
    run(&repo, &["assign", "h", "skill", "web", "on"]).unwrap();
    run(&repo, &["sync"]).unwrap();
    assert!(skills.join("web/SKILL.md").is_file());
    let web = item_id(&repo, ItemRef::Skill("web".into()));

    // `cp -r personal/web personal/aweb`: `aweb` sorts first by path.
    fs::create_dir(repo.repo.root().join("personal/aweb")).unwrap();
    for file in ["SKILL.md", ".sk1ll-id"] {
        fs::copy(repo.repo.root().join("personal/web").join(file), repo.repo.root().join("personal/aweb").join(file))
            .unwrap();
    }
    run(&repo, &["repair-ids"]).unwrap();
    assert_eq!(item_id(&repo, ItemRef::Skill("web".into())), web);
    assert_ne!(item_id(&repo, ItemRef::Skill("aweb".into())), web);
    run(&repo, &["sync"]).unwrap();
    assert!(skills.join("web/SKILL.md").is_file(), "web keeps its placement");
    assert!(!skills.join("aweb").exists(), "the copy is new, and new skills are off in h");
}

/// `new`, `rename` and `delete` do what the Library does, and `remove` of a
/// personal item points to `delete`.
#[test]
fn new_rename_and_delete_personal_items() {
    let mut repo = Machine::new();
    repo.harness("h");
    let created = run(&repo, &["new", "skill", "review", "--description", "Review a change"]).unwrap();
    let skill = repo.repo.root().join("personal/review/SKILL.md");
    assert!(created.contains(&skill.display().to_string()) && created.contains("`sk1ll edit review`"), "{created}");
    assert!(fs::read_to_string(&skill).unwrap().contains("Review a change"));
    run(&repo, &["new", "rule", "review"]).unwrap();
    assert!(repo.repo.root().join("instructions/review.md").is_file());

    let error = run(&repo, &["remove", "review"]).unwrap_err();
    assert!(error.contains("`sk1ll delete skill:review` or `sk1ll delete rule:review`"), "{error}");
    let error = run(&repo, &["remove", "skill:review"]).unwrap_err();
    assert!(
        error.contains("personal skill, not an import") && error.contains("`sk1ll delete skill:review`"),
        "{error}"
    );
    assert!(run(&repo, &["rename", "review", "x"]).unwrap_err().contains("say `skill:review` or `rule:review`"));

    run(&repo, &["assign", "h", "skill", "review", "off"]).unwrap();
    let id = item_id(&repo, ItemRef::Skill("review".into()));
    let renamed = run(&repo, &["rename", "skill:review", "code-review"]).unwrap();
    assert!(renamed.contains("`sk1ll commit skill:review skill:code-review`"), "{renamed}");
    assert_eq!(item_id(&repo, ItemRef::Skill("code-review".into())), id, "the Item ID goes with it");
    assert!(!Assignment::load(&repo.assignment()).unwrap().skill_enabled("h", &id), "and so does its toggle");

    let error = run(&repo, &["delete", "code-review"]).unwrap_err();
    assert!(error.contains("terminal confirmation or `--yes`"), "{error}");
    assert!(repo.repo.root().join("personal/code-review").is_dir());
    let deleted = run(&repo, &["delete", "code-review", "--yes"]).unwrap();
    assert!(deleted.contains("`sk1ll restore skill:code-review`"), "{deleted}");
    assert!(!repo.repo.root().join("personal/code-review").exists());
    assert!(run(&repo, &["delete", "nope", "--yes"]).unwrap_err().contains("no personal skill or rule named `nope`"));
}

/// `list` labels rules `rule`, leaves no trailing space for an item without
/// a description, and speaks of Item IDs only when a pending pull holds them up.
#[test]
fn list_labels_rules_and_mentions_missing_ids_only_when_they_wait() {
    let repo = with_pdf();
    repo.repo.write("instructions/git.md", "Use Conventional Commits.\n");
    let listed = run(&repo, &["list"]).unwrap();
    assert_eq!(listed, format!("{:<24} personal\n{:<24} rule\n", "pdf", "git"));
    repo.repo.write(".git/sk1ll-pull.json", "{}");
    let listed = run(&repo, &["list"]).unwrap();
    assert!(
        listed.contains("2 items have no Item ID until the pending pull finishes") && listed.contains("`sk1ll pull`"),
        "{listed}"
    );
}

#[test]
fn list_and_status_report_duplicate_item_id_paths() {
    let repo = with_pdf();
    repo.repo.open().create_item("skill", "docs", "").unwrap();
    fs::copy(repo.repo.root().join("personal/docs/.sk1ll-id"), repo.repo.root().join("personal/pdf/.sk1ll-id"))
        .unwrap();
    for command in ["list", "status"] {
        let err = run(&repo, &[command]).unwrap_err();
        for expected in ["duplicate Item ID", "personal/docs/.sk1ll-id", "personal/pdf/.sk1ll-id", "sk1ll repair-ids"] {
            assert!(err.contains(expected), "{command}: {err}");
        }
    }
}

#[test]
fn harness_changes_preview_apply_and_sync() {
    let repo = Machine::new();
    repo.repo.write("personal/pdf/SKILL.md", "PDFs.");
    let config = repo.machine_toml();
    let instructions = repo.root().join("rules");
    let skills = repo.root().join("h/skills");
    let harness = sk1ll_testkit::Harness::copy("h", &skills)
        .instructions(&instructions, sk1ll_testkit::InstructionsFormat::Block);
    write(&config, &format!("# keep me\n{}{}", presets_off(), harness.toml()));

    let add = ["harnesses", "add", "h", "--skills", path(&skills), "--format", "rules", "--yes"];
    run(&repo, &add).unwrap();
    assert!(skills.join("pdf/SKILL.md").is_file(), "changing a harness syncs it");
    let listing = run(&repo, &["harnesses"]).unwrap();
    let line = listing.lines().find(|line| line.starts_with("h ")).unwrap();
    assert!(line.contains(path(&instructions)) && line.contains("(rules)") && line.contains("(copy)"), "{line}");
    assert!(fs::read_to_string(&config).unwrap().starts_with("# keep me\n"));
    run(&repo, &["harnesses", "disable", "h", "--yes"]).unwrap();
    assert!(!run(&repo, &["harnesses"]).unwrap().lines().any(|line| line.starts_with("h ")));
    let all = run(&repo, &["harnesses", "--all"]).unwrap();
    assert!(all.lines().any(|line| line.starts_with("h ") && line.contains("disabled")), "{all}");
    for preset in sk1ll_core::machine::presets() {
        assert!(all.lines().any(|line| line.starts_with(&format!("{preset} "))), "{preset}: {all}");
    }
    assert!(skills.join("pdf").is_dir(), "left in place");
    run(&repo, &["harnesses", "enable", "h", "--yes"]).unwrap();
    // Its items off first: sync takes out what it placed, then disabling leaves nothing.
    run(&repo, &["assign", "h", "--all", "off"]).unwrap();
    run(&repo, &["sync"]).unwrap();
    assert!(!skills.join("pdf").exists());
    run(&repo, &["harnesses", "disable", "h", "--yes"]).unwrap();
    let assignment = Assignment::load(&repo.assignment()).unwrap();
    let pdf = repo.repo.open().skills().unwrap()[0].id.clone().expect("an Item ID");
    assert!(!assignment.skill_enabled("h", &pdf) && !assignment.harness("h").new_skills_default);

    run(&repo, &["harnesses", "remove", "h", "--yes"]).unwrap();
    assert!(!fs::read_to_string(&config).unwrap().contains("[harnesses.h]"));
    assert!(run(&repo, &["harnesses", "remove", "h", "--yes"]).is_err());
    assert!(run(&repo, &["harnesses", "add", "x", "--skills", "relative", "--yes"]).is_err());
}

/// `add --preset --home` makes a second config directory of one tool an
/// instance of its preset: machine.toml gets just those keys, and sync places
/// skills and the tool's rules (`harnesses: [claude]`) under that directory.
#[test]
fn harnesses_add_a_preset_instance_in_another_config_directory() {
    let repo = Machine::new();
    repo.repo.write("personal/pdf/SKILL.md", "PDFs.");
    repo.repo.write("instructions/all.md", "---\nharnesses: [claude]\n---\nFor Claude Code.\n");
    let config = repo.machine_toml();
    let add = ["harnesses", "add", "claude_2", "--preset", "claude", "--home", "~/.claude_2", "--yes"];
    run(&repo, &add).unwrap();
    let text = fs::read_to_string(&config).unwrap();
    assert!(text.ends_with("[harnesses.claude_2]\npreset = \"claude\"\nhome = \"~/.claude_2\"\n"), "{text}");
    let dir = repo.home.path().join(".claude_2");
    assert!(dir.join("skills/pdf/SKILL.md").exists());
    assert!(fs::read_to_string(dir.join("rules/all.md")).unwrap().contains("For Claude Code."));
    let listing = run(&repo, &["harnesses"]).unwrap();
    let line = listing.lines().find(|line| line.starts_with("claude_2 ")).unwrap();
    assert!(line.contains("~/.claude_2/skills") && line.ends_with("[claude in ~/.claude_2]"), "{line}");

    // Options left out keep their value: still the instance, the paths still derived.
    run(&repo, &["harnesses", "add", "claude_2", "--mode", "copy", "--yes"]).unwrap();
    let text = fs::read_to_string(&config).unwrap();
    assert!(text.ends_with("home = \"~/.claude_2\"\nmode = \"copy\"\n"), "{text}");
    // Another home moves the derived paths along.
    run(&repo, &["harnesses", "add", "claude_2", "--home", "~/.claude-work", "--yes"]).unwrap();
    let text = fs::read_to_string(&config).unwrap();
    assert!(text.ends_with("home = \"~/.claude-work\"\nmode = \"copy\"\n"), "{text}");
    assert!(repo.home.path().join(".claude-work/skills/pdf/SKILL.md").is_file());
    let refused = run(&repo, &["harnesses", "add", "x", "--preset", "nope", "--home", "~/.x", "--yes"]).unwrap_err();
    assert!(refused.contains("no preset `nope`"), "{refused}");
}

/// `--take-out` turns the harness's items off and syncs, so what sk1ll
/// placed there goes, and makes the change only once that worked.
#[cfg(unix)]
#[test]
fn harness_disable_and_remove_take_out_what_was_placed() {
    use std::os::unix::fs::PermissionsExt;
    let repo = Machine::new();
    for name in ["pdf", "web"] {
        fs::create_dir_all(repo.repo.root().join("personal").join(name)).unwrap();
        fs::write(repo.repo.root().join("personal").join(name).join("SKILL.md"), "Skill.").unwrap();
    }
    let config = repo.machine_toml();
    let skills = repo.root().join("h/skills");
    let add = ["harnesses", "add", "h", "--skills", skills.to_str().unwrap(), "--mode", "copy", "--yes"];
    run(&repo, &add).unwrap();
    assert!(skills.join("pdf/SKILL.md").is_file() && skills.join("web/SKILL.md").is_file());
    let disabled = || !run(&repo, &["harnesses"]).unwrap().lines().any(|line| line.starts_with("h "));

    // Without it, the preview names the one-step command.
    let preview = run(&repo, &["harnesses", "disable", "h"]).unwrap_err();
    assert!(preview.contains("`sk1ll harnesses disable h --take-out`"), "{preview}");

    // A removal that fails: nothing changes in machine.toml, the items stay off.
    fs::set_permissions(&skills, fs::Permissions::from_mode(0o555)).unwrap();
    let failed = run(&repo, &["harnesses", "disable", "h", "--take-out", "--yes"]);
    fs::set_permissions(&skills, fs::Permissions::from_mode(0o755)).unwrap();
    let failed = failed.unwrap_err();
    assert!(failed.contains("left machine.toml as it is: 2 placed items are still in h"), "{failed}");
    assert!(failed.contains("`sk1ll harnesses disable h --take-out` again"), "{failed}");
    assert!(!disabled());
    assert!(!Assignment::load(&repo.assignment()).unwrap().harness("h").new_skills_default, "the items stay off");

    // A copy edited by hand stays; the rest goes.
    fs::write(skills.join("web/SKILL.md"), "Edited by hand.").unwrap();
    let done = run(&repo, &["harnesses", "disable", "h", "--take-out", "--yes"]).unwrap();
    assert!(done.contains("first h's items are turned off and sync takes out what sk1ll placed there"), "{done}");
    assert!(done.contains("edited by hand, so it stays there"), "{done}");
    assert!(done.contains("took out 1 placed item from h"), "{done}");
    assert!(!skills.join("pdf").exists(), "taken out");
    assert_eq!(fs::read_to_string(skills.join("web/SKILL.md")).unwrap(), "Edited by hand.");
    assert!(disabled());

    // `remove --take-out` takes out what is placed, then drops the entry.
    fs::remove_dir_all(skills.join("web")).unwrap();
    run(&repo, &["harnesses", "enable", "h", "--yes"]).unwrap();
    run(&repo, &["assign", "h", "--all", "on"]).unwrap();
    assert!(skills.join("pdf/SKILL.md").is_file());
    run(&repo, &["harnesses", "remove", "h", "--take-out", "--yes"]).unwrap();
    assert!(!skills.join("pdf").exists() && !skills.join("web").exists());
    assert!(!fs::read_to_string(&config).unwrap().contains("[harnesses.h]"));
}

#[test]
fn harness_changes_stop_on_an_unreadable_assignment_and_report_failed_placements() {
    use std::os::unix::fs::PermissionsExt;
    let repo = Machine::new();
    repo.repo.write("personal/pdf/SKILL.md", "PDFs.");
    let config = repo.machine_toml();
    let before = fs::read_to_string(&config).unwrap();
    let skills = repo.root().join("h/skills");
    let add = ["harnesses", "add", "h", "--skills", skills.to_str().unwrap(), "--mode", "copy", "--yes"];

    fs::write(repo.assignment(), "{ not json").unwrap();
    assert!(run(&repo, &add).is_err());
    assert_eq!(fs::read_to_string(&config).unwrap(), before, "nothing changes when the assignment can't be read");
    fs::remove_file(repo.assignment()).unwrap();

    // A skills directory sync can read but not write into.
    fs::create_dir_all(&skills).unwrap();
    fs::set_permissions(&skills, fs::Permissions::from_mode(0o555)).unwrap();
    let err = run(&repo, &add).unwrap_err();
    fs::set_permissions(&skills, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(err.contains("machine.toml changed, but"), "{err}");
    assert!(fs::read_to_string(&config).unwrap().contains("[harnesses.h]"), "the change itself was made");
}

/// Toggling a rule off and on rewrites a block instructions file twice;
/// neither rewrite leaves a recovery entry, since the block was sk1ll's own.
#[test]
fn toggling_a_rule_in_a_block_file_leaves_no_recovery_entry() {
    let repo = Machine::new();
    let agents = repo.root().join("AGENTS.md");
    let mut config = fs::read_to_string(repo.machine_toml()).unwrap();
    config.push_str(&format!("[harnesses.b]\ninstructions = {:?}\n", agents.to_str().unwrap()));
    fs::write(repo.machine_toml(), config).unwrap();
    fs::write(repo.repo.root().join("instructions/git.md"), "---\ndescription: Git\n---\nUse Conventional Commits.\n")
        .unwrap();
    fs::write(&agents, "# Mine\n").unwrap();
    run(&repo, &["sync"]).unwrap();
    for state in ["off", "on", "off", "on"] {
        run(&repo, &["assign", "b", "rule", "git", state]).unwrap();
        run(&repo, &["sync"]).unwrap();
    }
    let text = fs::read_to_string(&agents).unwrap();
    assert!(text.starts_with("# Mine\n") && text.contains("Use Conventional Commits."), "{text}");
    assert_eq!(run(&repo, &["recover", "list"]).unwrap(), "");
}

/// `sk1ll adopt` of a harness copy placed before its skill was renamed
/// updates the renamed skill: the placement's Item ID finds it.
#[test]
fn adopt_updates_a_renamed_skill_instead_of_recreating_the_old_name() {
    let repo = with_pdf();
    run(&repo, &["sync"]).unwrap();
    fs::rename(repo.repo.root().join("personal/pdf"), repo.repo.root().join("personal/docs")).unwrap();
    fs::write(repo.root().join("h/pdf/SKILL.md"), "Edited in the harness.").unwrap();
    run(&repo, &["adopt", "h", "pdf"]).unwrap();
    assert_eq!(fs::read_to_string(repo.repo.root().join("personal/docs/SKILL.md")).unwrap(), "Edited in the harness.");
    assert!(!repo.repo.root().join("personal/pdf").exists(), "the old name is not recreated");
}

/// `commit` without items says how to name them or commit them all, and
/// `--all` commits every draft item. Without an `origin` there is nowhere
/// to push to, and it says nothing of it.
#[test]
fn commit_names_its_items_or_takes_them_all() {
    let repo = with_pdf();
    repo.repo.write("instructions/git.md", "Use Conventional Commits.\n");
    let error = run(&repo, &["commit"]).unwrap_err();
    assert!(error.contains("`sk1ll draft`") && error.contains("`sk1ll commit --all`"), "{error}");
    assert!(run(&repo, &["commit", "pdf", "--all"]).is_err());
    let out = run(&repo, &["commit", "--all"]).unwrap();
    assert!(out.starts_with("committed ") && out.contains("skill:pdf") && out.contains("rule:git"), "{out}");
    assert!(!out.contains("push") && !out.contains("fleet"), "{out}");
    assert_eq!(run(&repo, &["draft"]).unwrap(), "no draft: on the baseline\n");
    assert!(run(&repo, &["commit", "--all"]).unwrap().contains("no draft to commit"));
}

#[test]
fn unmanaged_list_and_adopt_preserve_differing_variants_and_assignment() {
    let fixture = with_harnesses();
    write(&fixture.root().join("a/pdf/SKILL.md"), "same");
    write(&fixture.root().join("b/pdf/SKILL.md"), "same");
    write(&fixture.root().join("c/pdf/SKILL.md"), "different");
    let list = run(&fixture, &["list", "--unmanaged"]).unwrap();
    assert!(list.contains("pdf") && list.contains("a, b, c") && list.contains("differs"), "{list}");
    for h in ["a", "b", "c"] {
        run(&fixture, &["assign", h, "--default-skills", "off"]).unwrap();
    }
    run(&fixture, &["adopt", "a", "pdf"]).unwrap();
    run(&fixture, &["adopt", "a", "pdf"]).unwrap();
    let root = fixture.root();
    assert_eq!(std::fs::read_to_string(root.join("repo/personal/pdf/SKILL.md")).unwrap(), "same");
    let assignment = sk1ll_core::Assignment::load(&fixture.assignment()).unwrap();
    let pdf = fixture.repo.open().skills().unwrap()[0].id.clone().unwrap();
    assert!(assignment.skill_enabled("a", &pdf) && assignment.skill_enabled("b", &pdf));
    assert!(!assignment.skill_enabled("c", &pdf));
    for harness in ["a", "b"] {
        let dir = root.join(harness);
        let bytes = std::fs::read(dir.join(sk1ll_core::sync::INDEX_PATH)).unwrap();
        let index: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let copy = dir.join("pdf");
        let token = std::fs::read_to_string(copy.join(sk1ll_core::sync::PLACEMENT_MARKER)).unwrap();
        let entry = &index["entries"]["pdf"];
        assert_eq!(entry["kind"], "copy");
        assert_eq!(entry["hash"], sk1ll_core::sync::deploy_hash(&root.join("repo/personal/pdf")).unwrap());
        assert_eq!(entry["token"], token);
        assert!(entry["payload"].as_str().is_some_and(|hash| hash.starts_with("sha256:")));
        assert_eq!(index["pending"], serde_json::json!({}));
    }
    assert_eq!(std::fs::read_to_string(root.join("c/pdf/SKILL.md")).unwrap(), "different");
    assert!(failure(command(&fixture, &["adopt", "c", "pdf"]).output().unwrap()).contains("already exists"));
    assert!(root.join("a/.sk1ll-tmp/displaced").is_dir());
}

#[test]
fn edit_uses_visual_then_editor_and_refuses_imports_and_unmanaged_skills() {
    let fixture = with_harnesses();
    fixture.repo.write("personal/pdf/SKILL.md", "skill");
    fixture.repo.write("instructions/git.md", "rule");
    fixture.repo.write("imported/remote/SKILL.md", "import");
    write(&fixture.root().join("a/local/SKILL.md"), "unmanaged");
    write(&fixture.root().join("editor script"), "printf '%s' \"$2\" > \"$1\"\n");
    let root = fixture.root();
    let editor = format!("sh '{}' '{}'", root.join("editor script").display(), root.join("opened").display());
    let out = command(&fixture, &["edit", "pdf"]).env("VISUAL", &editor).env("EDITOR", "false").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let opened = std::fs::read_to_string(root.join("opened")).unwrap();
    assert_eq!(Path::new(&opened), root.join("repo/personal/pdf/SKILL.md"));
    let out = command(&fixture, &["edit", "git"]).env_remove("VISUAL").env("EDITOR", &editor).output().unwrap();
    assert!(out.status.success());
    assert_eq!(
        std::fs::read_to_string(root.join("opened")).unwrap(),
        root.join("repo/instructions/git.md").to_str().unwrap()
    );
    assert!(failure(command(&fixture, &["edit", "remote"]).output().unwrap()).contains("sk1ll fork remote"));
    assert!(failure(command(&fixture, &["edit", "local"]).output().unwrap()).contains("sk1ll adopt a local"));
    assert!(
        failure(command(&fixture, &["edit", "pdf"]).env_remove("VISUAL").env_remove("EDITOR").output().unwrap())
            .contains("$VISUAL or $EDITOR")
    );
}

#[test]
fn daemonless_assign_all_reaches_items_with_spaces() {
    let fixture = with_harnesses();
    fixture.repo.write("personal/old name/SKILL.md", "Legacy");
    fixture.repo.write("personal/pdf/SKILL.md", "PDF");
    fixture.repo.write("instructions/old rule.md", "Legacy rule");
    let out = command(&fixture, &["assign", "a", "--all", "off"]).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    let assignment = sk1ll_core::Assignment::load(&fixture.root().join("assignment.json")).unwrap();
    // The command wrote each hand-made item an Item ID first.
    let ids = fixture.repo.open().item_ids().unwrap();
    assert_eq!(ids.len(), 3, "{ids:?}");
    for (item, id) in ids {
        let id = id.expect("an Item ID");
        match item {
            sk1ll_core::ItemRef::Skill(_) => assert!(!assignment.skill_enabled("a", &id), "{item}"),
            _ => assert!(!assignment.rule_enabled("a", &id), "{item}"),
        }
    }
    // Items that appear later are off too.
    let a = assignment.harness("a");
    assert!(!a.new_skills_default && !a.new_rules_default);
}

#[test]
fn harness_rename_moves_toggles_and_a_recreated_item_starts_from_the_default() {
    let fixture = with_harnesses();
    let root = fixture.root();
    fixture.repo.write("personal/pdf/SKILL.md", "PDF");
    run(&fixture, &["assign", "a", "skill", "pdf", "off"]).unwrap();
    let skill_id = || fixture.repo.open().skills().unwrap()[0].id.clone();
    let old = skill_id().expect("assign wrote an Item ID");
    std::fs::remove_dir_all(root.join("repo/personal/pdf")).unwrap();
    fixture.repo.write("personal/pdf/SKILL.md", "Recreated PDF");
    run(&fixture, &["harnesses", "rename", "a", "new-a"]).unwrap();
    let assignment = sk1ll_core::Assignment::load(&fixture.assignment()).unwrap();
    assert!(!assignment.harnesses.contains_key("a"));
    assert!(!assignment.skill_enabled("new-a", &old));
    let machine = sk1ll_core::MachineConfig::load(&fixture.machine_toml()).unwrap();
    assert!(machine.harnesses().contains_key("new-a"));
    run(&fixture, &["sync"]).unwrap();
    // Re-created by hand, it got a new Item ID, which has no toggle: IDs are
    // never derived from names, so the old one's toggle doesn't apply.
    let new = skill_id().expect("sync wrote an Item ID");
    assert_ne!(new, old);
    let assignment = sk1ll_core::Assignment::load(&fixture.assignment()).unwrap();
    assert!(assignment.skill_enabled("new-a", &new));
    // The renamed harness keeps its directory.
    assert_eq!(std::fs::read_to_string(root.join("a/pdf/SKILL.md")).unwrap(), "Recreated PDF");

    let out = command(&fixture, &["assign", "new-a", "skill", "absent", "on"]).output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("warning: no skill named `absent` in the repo"));
}

#[test]
fn rename_requires_readable_membership_and_protects_destination_assignment() {
    let fixture = with_harnesses();
    let config = std::fs::read(fixture.machine_toml()).unwrap();
    write(&fixture.daemon_state(), "{broken");
    let failed = failure(command(&fixture, &["harnesses", "rename", "a", "new-a"]).output().unwrap());
    assert!(failed.contains("parsing") && failed.contains("daemon.json"), "{failed}");
    assert_eq!(std::fs::read(fixture.machine_toml()).unwrap(), config);
    assert!(!fixture.assignment().exists());

    std::fs::remove_file(fixture.daemon_state()).unwrap();
    let mut assignment = sk1ll_core::Assignment::default();
    assignment.set_harness("a", true);
    assignment.set_harness("new-a", false);
    assignment.save(&fixture.assignment()).unwrap();
    fixture.repo.write("instructions/routed.md", "---\nid: rule-id\nharnesses: [a]\n---\nrule");
    let rejected = failure(command(&fixture, &["harnesses", "rename", "a", "new-a"]).output().unwrap());
    assert!(rejected.contains("still has an Assignment") && rejected.contains("--replace"), "{rejected}");
    assert_eq!(std::fs::read(fixture.machine_toml()).unwrap(), config);
    // Onto a harness this Machine has, --replace can't help, so it isn't suggested.
    for args in [&["harnesses", "rename", "a", "c"][..], &["harnesses", "rename", "a", "c", "--replace"]] {
        let taken = failure(command(&fixture, args).output().unwrap());
        assert!(taken.contains("already exists on this Machine") && !taken.contains("use --replace"), "{taken}");
    }
    let renamed = command(&fixture, &["harnesses", "rename", "a", "new-a", "--replace"]).output().unwrap();
    assert!(renamed.status.success(), "{renamed:?}");
    assert!(String::from_utf8_lossy(&renamed.stderr).contains("Rules still routed to a"));
    assert!(String::from_utf8_lossy(&renamed.stderr).contains("routed"));
    let assignment = sk1ll_core::Assignment::load(&fixture.assignment()).unwrap();
    assert!(!assignment.harnesses.contains_key("a"));
    assert!(assignment.harness("new-a").new_skills_default);
}

#[test]
fn add_lists_picks_and_adds_a_skill() {
    let machine = Machine::new();
    let source = import_source(&machine);
    let origin = path(source.root());
    let manifest = || std::fs::read_to_string(machine.repo.root().join("sk1ll.toml")).unwrap();
    let before = manifest();

    let list = command(&machine, &["add", "--list", origin]).output().unwrap();
    assert!(list.status.success(), "{}", text(&list));
    let listed = text(&list);
    assert!(listed.contains("skills/pdf") && listed.contains("Work with PDFs"), "{listed}");
    assert!(listed.contains("skills/web") && listed.contains("Browse the web"), "{listed}");
    assert_eq!(manifest(), before, "--list writes nothing");

    let ambiguous = command(&machine, &["add", origin]).output().unwrap();
    assert!(!ambiguous.status.success());
    let err = text(&ambiguous);
    assert!(err.contains("skills/pdf, skills/web") && err.contains("--path") && err.contains("--list"), "{err}");
    assert_eq!(manifest(), before);

    let added = command(&machine, &["add", origin, "--path", "skills/pdf"]).output().unwrap();
    assert!(added.status.success(), "{}", text(&added));
    assert!(String::from_utf8_lossy(&added.stdout).starts_with("added pdf @ "), "{}", text(&added));
    assert!(machine.repo.root().join("imported/pdf/SKILL.md").is_file());

    let list = text(&command(&machine, &["add", "--list", origin]).output().unwrap());
    let pdf = list.lines().find(|l| l.starts_with("skills/pdf")).unwrap();
    assert!(pdf.contains("taken"), "{list}");

    // An explicit ref, HEAD included, is what gets recorded.
    let added = command(&machine, &["add", origin, "--path", "skills/web", "--ref", "HEAD", "--name", "web2"])
        .output()
        .unwrap();
    assert!(added.status.success(), "{}", text(&added));
    assert!(manifest().contains("[import.web2]") && manifest().contains("ref = \"HEAD\""), "{}", manifest());
}

#[test]
fn fetch_failures_keep_gits_message_and_say_what_to_check() {
    let machine = Machine::new();
    let missing = machine.root().join("no-such-repo").to_string_lossy().into_owned();
    for args in [vec!["add", &missing], vec!["add", "--list", &missing]] {
        let out = command(&machine, &args).output().unwrap();
        assert!(!out.status.success());
        let err = text(&out);
        assert!(err.contains("`git clone") && err.contains("no-such-repo"), "{args:?}: {err}");
        assert_eq!(err.matches("Check the repository URL and your git credentials").count(), 1, "{err}");
    }
}

#[test]
fn relative_sources_mean_the_cwd_on_the_command_line_and_the_repo_root_in_the_manifest() {
    let machine = Machine::new();
    let source = import_source(&machine);
    let cwd = machine.repo.root().join("personal");
    let add = import_command(&machine, source.root(), &cwd, &["add", "../../origin", "--path", "skills/pdf"])
        .output()
        .unwrap();
    assert!(add.status.success(), "{}", text(&add));
    let manifest = std::fs::read_to_string(machine.repo.root().join("sk1ll.toml")).unwrap();
    assert!(manifest.contains(&format!("source = \"{}\"", path(source.root()))), "recorded absolute: {manifest}");

    std::fs::write(
        machine.repo.root().join("sk1ll.toml"),
        "[import.web]\nsource = \"../origin\"\npath = \"skills/web\"\n",
    )
    .unwrap();
    // Scheme-less GitHub links and invalid sources are not treated as paths.
    let link = import_command(&machine, source.root(), &cwd, &["add", "--list", "github.com/o/r"]).output().unwrap();
    assert!(link.status.success() && text(&link).contains("github:o/r @ HEAD"), "{}", text(&link));
    let dash = import_command(&machine, source.root(), &cwd, &["add", "--", "-x"]).output().unwrap();
    assert!(text(&dash).contains("invalid source `-x`"), "{}", text(&dash));

    let update = command(&machine, &["update", "web"]).output().unwrap();
    assert!(update.status.success(), "{}", text(&update));
    assert!(machine.repo.root().join("imported/web/SKILL.md").is_file());
}

/// `update` says when there is nothing to update, previews upstream changes
/// with `--dry-run` without touching the lock, and asks for a review only of
/// the imports that changed.
#[test]
fn update_says_what_changed_and_asks_for_a_review_only_then() {
    let machine = Machine::new();
    let source = import_source(&machine);
    let none = run(&machine, &["update"]).unwrap();
    assert!(none.contains("no imports to update") && !none.contains("review"), "{none}");
    run(&machine, &["add", path(source.root()), "--path", "skills/pdf"]).unwrap();
    let unchanged = run(&machine, &["update"]).unwrap();
    assert!(unchanged.contains("up to date") && !unchanged.contains("review"), "{unchanged}");
    let lock = machine.repo.read("sk1ll.lock");
    source.write("skills/pdf/notes.md", "Review this\n");
    source.commit_all("Add PDF notes");

    let preview = run(&machine, &["update", "pdf", "--dry-run"]).unwrap();
    assert!(preview.contains("Add PDF notes") && preview.contains("notes.md"), "{preview}");
    assert_eq!(machine.repo.read("sk1ll.lock"), lock);
    assert!(!machine.repo.root().join("imported/pdf/notes.md").exists());
    let changed = run(&machine, &["update"]).unwrap();
    assert!(changed.contains("review with `sk1ll diff pdf`, then `sk1ll commit pdf`"), "{changed}");
    assert!(machine.repo.root().join("imported/pdf/notes.md").is_file());
}

fn import_source(machine: &Machine) -> SkillsRepo {
    let source = SkillsRepo::init(machine.root().join("origin"));
    for (dir, description) in [("skills/pdf", "Work with PDFs"), ("skills/web", "Browse the web")] {
        source.write(&format!("{dir}/SKILL.md"), &format!("---\ndescription: {description}\n---\n"));
    }
    source.commit_all("Import candidates");
    source
}

fn import_command(machine: &Machine, source: &Path, cwd: &Path, args: &[&str]) -> std::process::Command {
    let mut command = command(machine, args);
    command
        .current_dir(cwd)
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", format!("url.{}.insteadOf", source.display()))
        .env("GIT_CONFIG_VALUE_0", "https://github.com/o/r.git");
    command
}

#[test]
fn add_uses_the_bundle_only_when_the_requested_source_cannot_be_fetched() {
    let machine = Machine::new();
    let bundled = sk1ll_core::bundled::SK1LL_SKILL_LINK;
    let add = |args: &[&str]| {
        command(&machine, args)
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", format!("url.{}/missing.git.insteadOf", machine.root().display()))
            .env("GIT_CONFIG_VALUE_0", "https://github.com/")
            .output()
            .unwrap()
    };
    for args in [
        &["add", bundled, "--ref", "main"][..],
        &["add", bundled, "--path", "skills/sk1ll"],
        &["add", "https://github.com/o/r"],
    ] {
        let error = failure(add(args));
        assert!(error.contains("Check the repository URL and your git credentials"), "{error}");
        assert!(!machine.repo.root().join("personal/sk1ll").exists());
    }
    let out = add(&["add", bundled]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("bundled"), "{}", text(&out));
    assert!(machine.repo.root().join("personal/sk1ll/SKILL.md").is_file());
    let error = failure(add(&["add", bundled]));
    assert!(error.contains("name `sk1ll` is taken"), "{error}");
}

#[test]
fn list_json_includes_stable_item_ids() {
    use serde_json::{Value, json};
    use sk1ll_testkit::{Rule, Skill};
    let machine = Machine::new();
    let skill = machine.repo.skill(Skill::new("pdf").description(Some("Read PDFs")).id("pdf-id"));
    let rule = machine.repo.rule(Rule::new("git").id("git-id"));
    let output = run(&machine, &["list", "--json"]).unwrap();
    let value: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(
        value,
        json!({
            "skills": [{"name":"pdf", "id":"pdf-id", "description":"Read PDFs", "origin":"personal", "path":skill, "shadows_import":false}],
            "rules": [{"name":"git", "id":"git-id", "description":null, "path":rule, "harnesses":null}]
        })
    );
}

#[test]
fn argument_errors_and_help_have_stable_exit_codes() {
    let machine = Machine::new();
    for (args, message) in [
        (&["update", "--dry-run"][..], "<NAME>"),
        (&["update", "pdf", "--dry-run", "--force"], "cannot be used with"),
        (&["list", "--json", "--unmanaged"], "cannot be used with"),
        (&["fleet", "invite", "--scope", "admin"], "invalid value"),
    ] {
        let output = command(&machine, args).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{}", text(&output));
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains(message), "{}", text(&output));
    }
    let output = command(&machine, &["recover", "adopt", "--help"]).output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    assert!(output.stderr.is_empty());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("<ID>") && help.contains("sk1ll recover list"), "{help}");
    assert!(run(&machine, &["recover", "--help"]).unwrap().contains("Delete an entry"));
    let script = run(&machine, &["completions", "bash"]).unwrap();
    assert!(script.contains("prune-orphans"));
    for name in ["invite", "remove", "orphans", "log", "undo"] {
        assert!(script.contains(&format!("__fleet__subcmd__{name}")), "{name} missing from Bash completions");
    }
}

#[test]
fn empty_variables_use_defaults_and_missing_repos_name_the_path() {
    use common::{BIN, command_for};
    let machine = with_pdf();
    let output = machine
        .home
        .command(BIN)
        .arg("list")
        .env("SK1LL_REPO", machine.repo.root())
        .env("SK1LL_MACHINE_CONFIG", machine.machine_toml())
        .env("SK1LL_ASSIGNMENT", "")
        .env("SK1LL_DAEMON_STATE", "")
        .env("SK1LL_DAEMON_SOCKET", "")
        .output()
        .unwrap();
    assert!(outcome(output).unwrap().contains("pdf"));

    let missing = machine.root().join("nowhere");
    let error = failure(command_for(&machine, &missing, &machine.no_daemon(), &["draft"]).output().unwrap());
    assert!(error.contains("no such directory") && error.contains("nowhere"), "{error}");
    assert!(!error.contains("POST") && !error.contains("/init"), "{error}");
}

/// Runs `command` with its standard output on a terminal (a pty), as a person would.
fn on_a_terminal(mut command: std::process::Command) -> String {
    use std::io::Read;
    use std::os::fd::{FromRawFd, OwnedFd};
    let (mut master, mut slave) = (0, 0);
    let (name, termios, size) = (std::ptr::null_mut(), std::ptr::null(), std::ptr::null());
    // SAFETY: openpty writes two new descriptors, which are owned from here on.
    assert_eq!(unsafe { libc::openpty(&mut master, &mut slave, name, termios, size) }, 0);
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    let child = command.stdin(std::process::Stdio::null()).stdout(slave).stderr(std::process::Stdio::piped()).spawn();
    // The command holds the terminal's other end: the read below ends once the child's copy closes.
    drop(command);
    let child = child.unwrap();
    let mut shown = Vec::new();
    // Linux reports EIO, not EOF, once the other end is closed.
    let _ = std::fs::File::from(master).read_to_end(&mut shown);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", text(&output));
    String::from_utf8(shown).unwrap().replace("\r\n", "\n")
}

/// Answers the next request `stub` gets with `release` as GitHub's latest one.
fn answer(stub: &std::net::TcpListener, release: &str) -> std::thread::JoinHandle<()> {
    use std::io::{Read, Write};
    let stub = stub.try_clone().unwrap();
    let release = release.to_string();
    std::thread::spawn(move || {
        let (mut conn, _) = stub.accept().unwrap();
        let (mut request, mut buf) = (Vec::new(), [0; 1024]);
        while !request.ends_with(b"\r\n\r\n") {
            let n = conn.read(&mut buf).unwrap();
            assert!(n > 0, "the request ended early");
            request.extend_from_slice(&buf[..n]);
        }
        let head = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close";
        write!(conn, "{head}\r\ncontent-length: {}\r\n\r\n{release}", release.len()).unwrap();
    })
}

#[test]
fn status_on_a_terminal_announces_a_newer_release_checked_at_most_daily() {
    let repo = with_pdf();
    let stub = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/releases/latest", stub.local_addr().unwrap());
    let status = || {
        let mut command = command(&repo, &["status"]);
        command.env("SK1LL_RELEASE_URL", &url).env("NO_PROXY", "127.0.0.1").env_remove("SK1LL_NO_UPDATE_CHECK");
        command
    };
    let ours = env!("CARGO_PKG_VERSION");
    let notice = |latest: &str| {
        format!("sk1ll {latest} is out (this is {ours}): https://github.com/0xV1p3r/0xSk1ll/releases/latest")
    };
    let cache = repo.home.cache_dir().join("latest-release.json");
    let asked = || {
        stub.set_nonblocking(true).unwrap();
        let asked = stub.accept().is_ok();
        stub.set_nonblocking(false).unwrap();
        asked
    };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();

    // Piped (a script): never asks.
    assert!(!outcome(status().output().unwrap()).unwrap().contains("is out"));
    assert!(!asked() && !cache.exists());
    for (tag, announced) in [
        (r#""v99.0.0""#.to_string(), Some("99.0.0")),
        (format!(r#""v{ours}""#), None),
        (format!(r#""v{ours}-rc.1""#), None),
        (r#""v0.0.1""#.into(), None),
        (r#""garbage""#.into(), None),
        ("2".into(), None),
    ] {
        // A day-old answer is asked again.
        write(&cache, &format!(r#"{{"checked_at":{},"version":"99.0.0"}}"#, now - 24 * 60 * 60));
        let served = answer(&stub, &format!(r#"{{"tag_name":{tag},"name":"A release"}}"#));
        let shown = on_a_terminal(status());
        served.join().unwrap();
        assert_eq!(
            shown.lines().find(|line| line.contains("is out")),
            announced.map(notice).as_deref(),
            "{tag}: {shown}"
        );
    }
    let served = answer(&stub, r#"{"tag_name":"v99.0.0"}"#);
    on_a_terminal(status());
    served.join().unwrap();
    // Asked today: the cached answer, without asking again.
    assert!(on_a_terminal(status()).contains(&notice("99.0.0")));
    assert!(!asked());
    let mut off = status();
    off.env("SK1LL_NO_UPDATE_CHECK", "1");
    assert!(!on_a_terminal(off).contains("is out"));
}
