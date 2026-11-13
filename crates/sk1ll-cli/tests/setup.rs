#![cfg(unix)]
use std::io::Write;
use std::process::{Command, Output, Stdio};

/// A machine with two harnesses, `a` and `b`, no repo and no daemon.
struct Fixture(tempfile::TempDir);
impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let config = ["a", "b"]
            .into_iter()
            .map(|h| format!("[harnesses.{h}]\nskills = {:?}\nmode = \"copy\"\n", tmp.path().join(h).to_str().unwrap()))
            .collect::<String>();
        std::fs::write(tmp.path().join("machine.toml"), format!("# my machine\nid = \"box\"\n{config}")).unwrap();
        Self(tmp)
    }
    fn setup(&self, answers: &str) -> Output {
        let root = self.0.path();
        let mut child = Command::new(env!("CARGO_BIN_EXE_sk1ll"))
            .arg("--repo")
            .arg(root.join("repo"))
            .arg("--machine-config")
            .arg(root.join("machine.toml"))
            .arg("--assignment")
            .arg(root.join("assignment.json"))
            .arg("--daemon-state")
            .arg(root.join("absent.json"))
            .arg("--daemon-socket")
            .arg(root.join("absent.sock"))
            .arg("setup")
            .env("HOME", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(answers.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    }
}

#[test]
fn setup_creates_a_standalone_repo_with_the_chosen_harnesses() {
    let fixture = Fixture::new();
    // Only this machine, keep `a`, drop `b`.
    let out = fixture.setup("1\n\nn\n");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}{}", String::from_utf8_lossy(&out.stderr));
    assert!(fixture.0.path().join("repo/sk1ll.toml").is_file());
    assert!(stdout.contains("Sync a ("), "{stdout}");
    assert!(stdout.contains("custom daemon socket"), "{stdout}");
    let config = std::fs::read_to_string(fixture.0.path().join("machine.toml")).unwrap();
    assert!(config.starts_with("# my machine\n"), "{config}");
    let machine = sk1ll_core::MachineConfig::load(&fixture.0.path().join("machine.toml")).unwrap();
    assert_eq!(machine.harnesses["b"].enabled, Some(false));
    assert_eq!(machine.harnesses["a"].enabled, None);

    let again = fixture.setup("");
    assert!(again.status.success());
    assert!(String::from_utf8_lossy(&again.stdout).contains("already has a skills repo"));
}

#[test]
fn setup_reprompts_bad_answers_and_stops_without_input() {
    let fixture = Fixture::new();
    let out = fixture.setup("4\n1\nmaybe\n");
    assert!(!out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("enter 1, 2 or 3") && stdout.contains("answer y or n"), "{stdout}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("setup needs answers on standard input"));
    assert!(!fixture.0.path().join("repo").exists());
}

#[test]
fn setup_requires_a_join_code() {
    let fixture = Fixture::new();
    let out = fixture.setup("2\n\n\nftp://x\nhttp://127.0.0.1:1\n\n");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("must start with http:// or https://"), "{stdout}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("join code is required"));
    assert!(!fixture.0.path().join("repo").exists());
}
