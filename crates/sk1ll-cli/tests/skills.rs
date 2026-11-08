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
            .map(|h| {
                format!(
                    "[harnesses.{h}]\nskills = {:?}\nmode = \"copy\"\n",
                    tmp.path().join(h).to_str().unwrap()
                )
            })
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
    assert!(
        list.contains("pdf") && list.contains("a, b, c") && list.contains("differs"),
        "{list}"
    );
    for h in ["a", "b", "c"] {
        fixture.run(&["assign", h, "--default-skills", "off"]);
    }
    fixture.run(&["adopt", "a", "pdf"]);
    fixture.run(&["adopt", "a", "pdf"]);
    let root = fixture.0.path();
    assert_eq!(
        std::fs::read_to_string(root.join("repo/personal/pdf/SKILL.md")).unwrap(),
        "same"
    );
    let assignment = sk1ll_core::Assignment::load(&root.join("assignment.json")).unwrap();
    assert!(assignment.skill_enabled("a", "pdf") && assignment.skill_enabled("b", "pdf"));
    assert!(!assignment.skill_enabled("c", "pdf"));
    assert!(root.join("a/pdf/.sk1ll-managed").is_file() && root.join("b/pdf/.sk1ll-managed").is_file());
    assert_eq!(
        std::fs::read_to_string(root.join("c/pdf/SKILL.md")).unwrap(),
        "different"
    );
    assert!(error(fixture.command(&["adopt", "c", "pdf"]).output().unwrap()).contains("already exists"));
    assert!(root.join("a/.sk1ll-tmp/displaced").is_dir());
}

#[test]
fn edit_uses_visual_then_editor_and_refuses_imports_and_unmanaged_skills() {
    let fixture = Fixture::new();
    fixture.write("repo/personal/pdf/SKILL.md", "skill");
    fixture.write("repo/instructions/git.md", "fragment");
    fixture.write("repo/imported/remote/SKILL.md", "import");
    fixture.write("a/local/SKILL.md", "unmanaged");
    fixture.write("editor script", "printf '%s' \"$2\" > \"$1\"\n");
    let root = fixture.0.path();
    let editor = format!(
        "sh '{}' '{}'",
        root.join("editor script").display(),
        root.join("opened").display()
    );
    let out = fixture
        .command(&["edit", "pdf"])
        .env("VISUAL", &editor)
        .env("EDITOR", "false")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let opened = std::fs::read_to_string(root.join("opened")).unwrap();
    assert_eq!(Path::new(&opened), root.join("repo/personal/pdf/SKILL.md"));
    let out = fixture
        .command(&["edit", "git"])
        .env_remove("VISUAL")
        .env("EDITOR", &editor)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        std::fs::read_to_string(root.join("opened")).unwrap(),
        root.join("repo/instructions/git.md").to_str().unwrap()
    );
    assert!(error(fixture.command(&["edit", "remote"]).output().unwrap()).contains("sk1ll fork remote"));
    assert!(error(fixture.command(&["edit", "local"]).output().unwrap()).contains("sk1ll adopt a local"));
    assert!(
        error(
            fixture
                .command(&["edit", "pdf"])
                .env_remove("VISUAL")
                .env_remove("EDITOR")
                .output()
                .unwrap()
        )
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
        machine_id: "a".into(), token: "token".into(), hub_url: "http://unused".into(),
        repo: joined.root().to_path_buf(), revoked: false,
    }.save(&root.join("absent.json")).unwrap();
    fixture.write("a/second/SKILL.md", "unmanaged user content");
    let out = fixture.command(&["adopt", "a", "second"]).output().unwrap();
    let message = error(out);
    assert!(message.contains("selected repo") && message.contains("joined repo"), "{message}");
    assert!(!joined.personal_dir().join("second").exists());
    assert!(!root.join("repo/personal/second").exists());
    assert!(!root.join("daemon-outbox.json").exists());
    assert_eq!(std::fs::read_to_string(root.join("a/second/SKILL.md")).unwrap(), "unmanaged user content");
}


#[test]
fn joined_adoption_requires_a_running_connected_daemon() {
    let fixture = Fixture::new();
    let root = fixture.0.path();
    sk1ll_daemon::DaemonState {
        machine_id: "a".into(), token: "token".into(), hub_url: "http://unused".into(),
        repo: root.join("repo"), revoked: false,
    }.save(&root.join("absent.json")).unwrap();
    fixture.write("a/pdf/SKILL.md", "unmanaged user content");
    let message = error(fixture.command(&["adopt", "a", "pdf"]).output().unwrap());
    assert!(message.contains("Adopt needs a connection to the Hub") && message.contains("sk1ll daemon"), "{message}");
    assert!(!root.join("repo/personal/pdf").exists());
    assert!(!root.join("assignment.json").exists());
    assert!(!root.join("daemon-outbox.json").exists());
    assert_eq!(std::fs::read_to_string(root.join("a/pdf/SKILL.md")).unwrap(), "unmanaged user content");
}
