#![cfg(unix)]
use std::path::Path;
use std::process::{Command, Output};

struct Fixture(tempfile::TempDir);
impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        sk1ll_core::Repo::init(&tmp.path().join("repo")).unwrap();
        let config = ["a", "b", "c"]
            .into_iter()
            .map(|h| format!("[harnesses.{h}]\nskills = {:?}\nmode = \"copy\"\n", tmp.path().join(h).to_str().unwrap()))
            .collect::<String>();
        std::fs::write(tmp.path().join("machine.toml"), config).unwrap();
        Self(tmp)
    }
    fn command(&self, args: &[&str]) -> Command {
        let root = self.0.path();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sk1ll"));
        cmd.arg("--repo")
            .arg(root.join("repo"))
            .arg("--machine-config")
            .arg(root.join("machine.toml"))
            .arg("--assignment")
            .arg(root.join("assignment.json"))
            .arg("--daemon-state")
            .arg(root.join("absent.json"))
            .arg("--daemon-socket")
            .arg(root.join("absent.sock"))
            .env("HOME", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args);
        cmd
    }
    fn run(&self, args: &[&str]) -> String {
        let out = self.command(args).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }
    fn write(&self, path: &str, text: &str) {
        let path = self.0.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}
fn error(out: Output) -> String {
    assert!(!out.status.success());
    String::from_utf8(out.stderr).unwrap()
}

#[test]
fn unmanaged_list_and_adopt_preserve_differing_variants_and_assignment() {
    let fixture = Fixture::new();
    fixture.write("a/pdf/SKILL.md", "same");
    fixture.write("b/pdf/SKILL.md", "same");
    fixture.write("c/pdf/SKILL.md", "different");
    let list = fixture.run(&["list", "--unmanaged"]);
    assert!(list.contains("pdf") && list.contains("a, b, c") && list.contains("differs"), "{list}");
    for h in ["a", "b", "c"] {
        fixture.run(&["assign", h, "--default-skills", "off"]);
    }
    fixture.run(&["adopt", "a", "pdf"]);
    fixture.run(&["adopt", "a", "pdf"]);
    let root = fixture.0.path();
    assert_eq!(std::fs::read_to_string(root.join("repo/personal/pdf/SKILL.md")).unwrap(), "same");
    let assignment = sk1ll_core::Assignment::load(&root.join("assignment.json")).unwrap();
    let pdf = sk1ll_core::Repo::open(root.join("repo")).unwrap().skills().unwrap()[0].id.clone().unwrap();
    assert!(assignment.skill_enabled("a", &pdf) && assignment.skill_enabled("b", &pdf));
    assert!(!assignment.skill_enabled("c", &pdf));
    assert!(root.join("a/pdf/.sk1ll-managed").is_file() && root.join("b/pdf/.sk1ll-managed").is_file());
    assert_eq!(std::fs::read_to_string(root.join("c/pdf/SKILL.md")).unwrap(), "different");
    assert!(error(fixture.command(&["adopt", "c", "pdf"]).output().unwrap()).contains("already exists"));
    assert!(root.join("a/.sk1ll-tmp/displaced").is_dir());
}

#[test]
fn edit_uses_visual_then_editor_and_refuses_imports_and_unmanaged_skills() {
    let fixture = Fixture::new();
    fixture.write("repo/personal/pdf/SKILL.md", "skill");
    fixture.write("repo/instructions/git.md", "rule");
    fixture.write("repo/imported/remote/SKILL.md", "import");
    fixture.write("a/local/SKILL.md", "unmanaged");
    fixture.write("editor script", "printf '%s' \"$2\" > \"$1\"\n");
    let root = fixture.0.path();
    let editor = format!("sh '{}' '{}'", root.join("editor script").display(), root.join("opened").display());
    let out = fixture.command(&["edit", "pdf"]).env("VISUAL", &editor).env("EDITOR", "false").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let opened = std::fs::read_to_string(root.join("opened")).unwrap();
    assert_eq!(Path::new(&opened), root.join("repo/personal/pdf/SKILL.md"));
    let out = fixture.command(&["edit", "git"]).env_remove("VISUAL").env("EDITOR", &editor).output().unwrap();
    assert!(out.status.success());
    assert_eq!(
        std::fs::read_to_string(root.join("opened")).unwrap(),
        root.join("repo/instructions/git.md").to_str().unwrap()
    );
    assert!(error(fixture.command(&["edit", "remote"]).output().unwrap()).contains("sk1ll fork remote"));
    assert!(error(fixture.command(&["edit", "local"]).output().unwrap()).contains("sk1ll adopt a local"));
    assert!(
        error(fixture.command(&["edit", "pdf"]).env_remove("VISUAL").env_remove("EDITOR").output().unwrap())
            .contains("$VISUAL or $EDITOR")
    );
}

#[test]
fn direct_adoption_rejects_a_repo_other_than_the_joined_repo() {
    let fixture = Fixture::new();
    let root = fixture.0.path();
    let joined = sk1ll_core::Repo::init(&root.join("joined")).unwrap();
    // A different process may own the joined repo's lock. --repo must not bypass it.
    let _joined_lock = sk1ll_core::EngineLock::acquire(joined.root()).unwrap();
    sk1ll_daemon::DaemonState {
        machine_id: "a".into(),
        token: "token".into(),
        server_url: "http://unused".into(),
        repo: joined.root().to_path_buf(),
        revoked: false,
    }
    .save(&root.join("absent.json"))
    .unwrap();
    fixture.write("a/second/SKILL.md", "unmanaged user content");
    let out = fixture.command(&["adopt", "a", "second"]).output().unwrap();
    let message = error(out);
    assert!(message.contains("selected repo") && message.contains("joined repo"), "{message}");
    assert!(!joined.personal_dir().join("second").exists());
    assert!(!root.join("repo/personal/second").exists());
    assert_eq!(std::fs::read_to_string(root.join("a/second/SKILL.md")).unwrap(), "unmanaged user content");
}

#[test]
fn joined_adoption_requires_a_running_connected_daemon() {
    let fixture = Fixture::new();
    let root = fixture.0.path();
    sk1ll_daemon::DaemonState {
        machine_id: "a".into(),
        token: "token".into(),
        server_url: "http://unused".into(),
        repo: root.join("repo"),
        revoked: false,
    }
    .save(&root.join("absent.json"))
    .unwrap();
    fixture.write("a/pdf/SKILL.md", "unmanaged user content");
    let message = error(fixture.command(&["adopt", "a", "pdf"]).output().unwrap());
    assert!(
        message.contains("Adopt needs a connection to the Server") && message.contains("sk1ll daemon"),
        "{message}"
    );
    assert!(!root.join("repo/personal/pdf").exists());
    assert!(!root.join("assignment.json").exists());
    assert_eq!(std::fs::read_to_string(root.join("a/pdf/SKILL.md")).unwrap(), "unmanaged user content");
}

#[test]
fn daemonless_assign_all_reaches_items_with_spaces() {
    let fixture = Fixture::new();
    fixture.write("repo/personal/old name/SKILL.md", "Legacy");
    fixture.write("repo/personal/pdf/SKILL.md", "PDF");
    fixture.write("repo/instructions/old rule.md", "Legacy rule");
    let out = fixture.command(&["assign", "a", "--all", "off"]).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    let assignment = sk1ll_core::Assignment::load(&fixture.0.path().join("assignment.json")).unwrap();
    // The command wrote each hand-made item an Item ID first.
    let ids = sk1ll_core::Repo::open(fixture.0.path().join("repo")).unwrap().item_ids().unwrap();
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
    let fixture = Fixture::new();
    let root = fixture.0.path();
    fixture.write("repo/personal/pdf/SKILL.md", "PDF");
    fixture.run(&["assign", "a", "skill", "pdf", "off"]);
    let skill_id = || sk1ll_core::Repo::open(root.join("repo")).unwrap().skills().unwrap()[0].id.clone();
    let old = skill_id().expect("assign wrote an Item ID");
    std::fs::remove_dir_all(root.join("repo/personal/pdf")).unwrap();
    fixture.write("repo/personal/pdf/SKILL.md", "Recreated PDF");
    fixture.run(&["harnesses", "rename", "a", "new-a"]);
    let assignment = sk1ll_core::Assignment::load(&root.join("assignment.json")).unwrap();
    assert!(!assignment.harnesses.contains_key("a"));
    assert!(!assignment.skill_enabled("new-a", &old));
    let machine = sk1ll_core::MachineConfig::load(&root.join("machine.toml")).unwrap();
    assert!(machine.harnesses().contains_key("new-a"));
    fixture.run(&["sync"]);
    // Re-created by hand, it got a new Item ID, which has no toggle: IDs are
    // never derived from names, so the old one's toggle doesn't apply.
    let new = skill_id().expect("sync wrote an Item ID");
    assert_ne!(new, old);
    let assignment = sk1ll_core::Assignment::load(&root.join("assignment.json")).unwrap();
    assert!(assignment.skill_enabled("new-a", &new));
    // The renamed harness keeps its directory.
    assert_eq!(std::fs::read_to_string(root.join("a/pdf/SKILL.md")).unwrap(), "Recreated PDF");

    let out = fixture.command(&["assign", "new-a", "skill", "absent", "on"]).output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("warning: no skill named `absent` in the repo"));
}

#[test]
fn rename_requires_readable_membership_and_protects_destination_assignment() {
    let fixture = Fixture::new();
    let root = fixture.0.path();
    let config = std::fs::read(root.join("machine.toml")).unwrap();
    fixture.write("absent.json", "{broken");
    let failed = error(fixture.command(&["harnesses", "rename", "a", "new-a"]).output().unwrap());
    assert!(failed.contains("parsing") && failed.contains("absent.json"), "{failed}");
    assert_eq!(std::fs::read(root.join("machine.toml")).unwrap(), config);
    assert!(!root.join("assignment.json").exists());

    std::fs::remove_file(root.join("absent.json")).unwrap();
    let mut assignment = sk1ll_core::Assignment::default();
    assignment.set_harness("a", true);
    assignment.set_harness("new-a", false);
    assignment.save(&root.join("assignment.json")).unwrap();
    fixture.write("repo/instructions/routed.md", "---\nid: rule-id\nharnesses: [a]\n---\nrule");
    let rejected = error(fixture.command(&["harnesses", "rename", "a", "new-a"]).output().unwrap());
    assert!(rejected.contains("already has an Assignment"), "{rejected}");
    assert_eq!(std::fs::read(root.join("machine.toml")).unwrap(), config);
    let renamed = fixture.command(&["harnesses", "rename", "a", "new-a", "--replace"]).output().unwrap();
    assert!(renamed.status.success(), "{renamed:?}");
    assert!(String::from_utf8_lossy(&renamed.stderr).contains("Rules still routed to a"));
    assert!(String::from_utf8_lossy(&renamed.stderr).contains("routed"));
    let assignment = sk1ll_core::Assignment::load(&root.join("assignment.json")).unwrap();
    assert!(!assignment.harnesses.contains_key("a"));
    assert!(assignment.harness("new-a").new_skills_default);
}

#[test]
fn standalone_sync_recovers_a_harness_rename_after_config_was_saved() {
    use sk1ll_core::harness_rename::{RenameIntent, RenameRequest};
    let fixture = Fixture::new();
    let root = fixture.0.path();
    let mut assignment = sk1ll_core::Assignment::default();
    assignment.set_harness("a", false);
    assignment.save(&root.join("assignment.json")).unwrap();
    let config = root.join("machine.toml");
    let pending = RenameIntent::prepare(
        &config,
        &root.join("repo"),
        RenameRequest { from: "a".into(), to: "new-a".into(), replace: false },
        &assignment,
        None,
    )
    .unwrap();
    pending.apply_config(&config).unwrap();

    fixture.run(&["sync"]);
    assert!(!RenameIntent::exists(&config).unwrap());
    let saved = sk1ll_core::Assignment::load(&root.join("assignment.json")).unwrap();
    assert!(!saved.harnesses.contains_key("a"));
    assert!(!saved.harness("new-a").new_skills_default);
}
