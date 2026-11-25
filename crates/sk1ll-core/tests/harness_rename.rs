use sk1ll_core::harness_rename::{RenameIntent, RenameRequest};
use sk1ll_core::{Assignment, ItemId, MachineConfig};
use sk1ll_testkit::{Harness, Home, MachineToml, SkillsRepo, read, write};

#[test]
fn a_pending_rename_carries_assignment_toggles_and_reports_rules_still_routed_to_the_old_name() {
    let home = Home::new();
    let _entered = home.enter();
    let repo = SkillsRepo::new();
    let config = home.path().join("machine.toml");
    MachineToml::new().harness(Harness::copy("old", home.path().join("skills"))).write(&config);
    repo.write("instructions/routed.md", "---\nharnesses: [old]\n---\nKeep routing.\n");
    let file = home.path().join("assignment.json");
    let id: ItemId = "pdf-id".parse().unwrap();
    let mut assignment = Assignment::default();
    assignment.set_skill("old", &id, false);
    assignment.set_new_rules_default("old", false);
    assignment.set_skill("new", &id, true);
    assignment.save(&file).unwrap();
    let state = home.state_dir();
    let request = RenameRequest { from: "old".into(), to: "new".into(), replace: false };
    let prepare = |request| RenameIntent::prepare(&state, &config, repo.root(), request, &assignment, None);
    assert!(prepare(request.clone()).unwrap_err().to_string().contains("use --replace"));
    let request = RenameRequest { replace: true, ..request };
    let intent = prepare(request.clone()).unwrap();
    assert_eq!(intent.routed_rules, ["routed"]);
    assert_eq!(prepare(request).unwrap().operation_id, intent.operation_id);
    assert!(RenameIntent::load(&state, &config, Some("another-server")).is_err());
    intent.apply_config(&config).unwrap();
    let reopened = RenameIntent::load(&state, &config, None).unwrap().unwrap();
    reopened.apply_standalone(&config, &file).unwrap();
    let cfg = MachineConfig::load(&config).unwrap();
    assert_eq!(cfg.harnesses["new"].skills, Some(home.path().join("skills").to_string_lossy().into_owned()));
    assert!(!cfg.harnesses.contains_key("old"));
    let after = Assignment::load(&file).unwrap();
    assert!(!after.skill_enabled("new", &id) && !after.harness("new").new_rules_default);
    assert!(!after.harnesses.contains_key("old"));
    reopened.finish().unwrap();
    assert!(!RenameIntent::exists(&state, &config).unwrap());
    assert!(repo.read("instructions/routed.md").contains("harnesses: [old]"));
}

#[test]
fn rollback_preserves_an_edit_made_after_the_rename_config_was_written() {
    let home = Home::new();
    let config = home.path().join("machine.toml");
    MachineToml::new().harness(Harness::copy("old", home.path().join("skills"))).write(&config);
    let repo = home.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let initial = read(&config);
    let state = home.state_dir();
    let intent = RenameIntent::prepare(
        &state,
        &config,
        &repo,
        RenameRequest { from: "old".into(), to: "new".into(), replace: false },
        &Assignment::default(),
        None,
    )
    .unwrap();
    intent.apply_config(&config).unwrap();
    let renamed = read(&config);
    let edited = format!("# my edit\n{renamed}");
    write(&config, &edited);
    assert!(intent.rollback_config(&config).unwrap_err().to_string().contains("changed during"));
    assert_eq!(read(&config), edited);
    assert!(RenameIntent::exists(&state, &config).unwrap());
    write(&config, &renamed);
    intent.rollback_config(&config).unwrap();
    assert_eq!(read(&config), initial);
    assert!(!RenameIntent::exists(&state, &config).unwrap());
}
