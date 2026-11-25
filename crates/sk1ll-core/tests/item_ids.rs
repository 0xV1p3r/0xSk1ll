use sk1ll_core::baseline_check::check_tree;
use sk1ll_core::content::{item_ids_at, item_ids_on_disk};
use sk1ll_core::{ItemRef, Repo};
use sk1ll_testkit::{Home, SkillsRepo};

#[test]
fn the_baseline_rejection_names_missing_and_shared_item_ids() {
    let home = Home::new();
    let _entered = home.enter();
    let fixture = SkillsRepo::new();
    for name in ["a", "b"] {
        fixture.write(&format!("personal/{name}/SKILL.md"), "Skill.\n");
        fixture.write(&format!("personal/{name}/.sk1ll-id"), "same\n");
    }
    fixture.write("instructions/r.md", "No ID.\n");
    fixture.write("imported/web/SKILL.md", "Web.\n");
    fixture.write("sk1ll.toml", "[import.web]\nsource = 'github:o/r'\n");
    fixture.commit_all("missing and duplicate IDs");

    let error = check_tree(fixture.root(), "HEAD", |_, _| true).unwrap().unwrap();
    for expected in [
        "skill:web (sk1ll.toml [import.web]) has no Item ID",
        "rule:r (instructions/r.md) has no Item ID",
        "skill:a (personal/a/.sk1ll-id)",
        "skill:b (personal/b/.sk1ll-id)",
        "Run `sk1ll repair-ids`, commit the repaired items and push again",
    ] {
        assert!(error.contains(expected), "{error}");
    }
    fixture.open().repair_ids().unwrap();
    fixture.commit_all("repaired");
    assert_eq!(check_tree(fixture.root(), "HEAD", |_, _| true).unwrap(), None);
}

#[test]
fn item_id_discovery_agrees_between_git_and_the_working_tree() {
    let home = Home::new();
    let _entered = home.enter();
    let fixture = SkillsRepo::new();
    let repo = fixture.open();
    repo.create_item("skill", "made", "").unwrap();
    repo.create_item("rule", "made", "").unwrap();
    for file in ["personal/hand/SKILL.md", "imported/hand/SKILL.md", "imported/pdf/SKILL.md"] {
        fixture.write(file, "Skill.\n");
    }
    fixture.write("instructions/hand.md", "Rule.\n");
    fixture.write("instructions/long.md", &format!("---\nid: long-id\n---\n{}", "x".repeat(2 * 1024 * 1024)));
    fixture.write("sk1ll.toml", "[import.pdf]\nsource = 'github:o/r'\nid = 'pdf-id'\n");
    let head = fixture.commit_all("record the skills repo layout");
    let on_disk = item_ids_on_disk(repo.root()).unwrap();
    assert_eq!(item_ids_at(repo.root(), &head).unwrap(), on_disk);
    assert_eq!(on_disk[&ItemRef::Skill("pdf".into())].as_ref().unwrap().to_string(), "pdf-id");
    assert_eq!(on_disk[&ItemRef::Rule("long".into())].as_ref().unwrap().to_string(), "long-id");
    assert!(on_disk[&ItemRef::Skill("made".into())].is_some());
    assert_eq!(on_disk[&ItemRef::Skill("hand".into())], None);
    let listed = Repo::open(repo.root()).unwrap().item_ids().unwrap();
    assert_eq!(listed.into_iter().collect::<std::collections::BTreeMap<_, _>>(), on_disk);
}
