mod common;

use std::{
    fs,
    io::Write,
    os::unix::fs::symlink,
    path::Path,
    process::{Output, Stdio},
};

use sk1ll_testkit::{Harness, Home, Machine, outcome, server::Server, text, write};

use common::{BIN, join_fleet, run, runtime};

struct Setup {
    home: Home,
}

impl Setup {
    fn new() -> Self {
        let home = Home::new();
        let root = home.path();
        let mut config = "# my machine\nid = \"box\"\n".to_string();
        for name in ["a", "b"] {
            config.push_str(&Harness::copy(name, root.join(name)).toml());
        }
        write(&root.join("machine.toml"), &config);
        Self { home }
    }

    fn setup(&self, answers: &str) -> Output {
        self.setup_at(&self.home.path().join("repo"), answers)
    }

    fn setup_at(&self, repo: &Path, answers: &str) -> Output {
        self.setup_env(repo, answers, &[])
    }

    /// With these environment variables set, as a user's shell may have them.
    fn setup_env(&self, repo: &Path, answers: &str, env: &[(&str, &Path)]) -> Output {
        let root = self.home.path();
        let mut child = self
            .home
            .command(BIN)
            .envs(env.iter().copied())
            .arg("--repo")
            .arg(repo)
            .arg("--machine-config")
            .arg(root.join("machine.toml"))
            .arg("--assignment")
            .arg(root.join("assignment.json"))
            .arg("--daemon-state")
            .arg(root.join("daemon.json"))
            .arg("--daemon-socket")
            .arg(root.join("absent.sock"))
            .env_remove("SK1LL_SERVER_ADMIN_TOKEN")
            .arg("setup")
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
    let fixture = Setup::new();
    // Only this machine, keep `a`, drop `b`.
    let out = fixture.setup("1\n\nn\n");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}{}", String::from_utf8_lossy(&out.stderr));
    assert!(fixture.home.path().join("repo/sk1ll.toml").is_file());
    assert!(stdout.contains("Sync a ("), "{stdout}");
    assert!(stdout.contains("custom daemon socket"), "{stdout}");
    // The printed commands keep this run's path flags, so they act on the same repo from anywhere.
    let root = fixture.home.path().display();
    let flags = format!("--repo {root}/repo --machine-config {root}/machine.toml --assignment {root}/assignment.json");
    assert!(stdout.contains(&format!("  sk1ll {flags} sync --dry-run\n  sk1ll {flags} sync\n")), "{stdout}");
    let config = std::fs::read_to_string(fixture.home.path().join("machine.toml")).unwrap();
    assert!(config.starts_with("# my machine\n"), "{config}");
    let machine = sk1ll_core::MachineConfig::load(&fixture.home.path().join("machine.toml")).unwrap();
    assert_eq!(machine.harnesses["b"].enabled, Some(false));
    assert_eq!(machine.harnesses["a"].enabled, None);

    // With the repo there (as after `sk1ll init`), setup still offers the harnesses and the daemon.
    std::fs::create_dir_all(fixture.home.path().join("repo/personal/pdf")).unwrap();
    std::fs::write(fixture.home.path().join("repo/personal/pdf/SKILL.md"), "PDFs.").unwrap();
    let again = fixture.setup("\ny\n");
    let stdout = String::from_utf8_lossy(&again.stdout);
    assert!(again.status.success(), "{stdout}{}", String::from_utf8_lossy(&again.stderr));
    assert!(stdout.contains("Using the skills repo at") && !stdout.contains("How will this machine"), "{stdout}");
    assert!(stdout.contains("Sync b (") && stdout.contains("custom daemon socket"), "{stdout}");
    let machine = sk1ll_core::MachineConfig::load(&fixture.home.path().join("machine.toml")).unwrap();
    assert_ne!(machine.harnesses["b"].enabled, Some(false));
    assert!(fixture.home.path().join("b/pdf/SKILL.md").is_file(), "enabling a harness syncs it");
}

/// First-run setup offers the opt-in presets it finds as on, and enables
/// the ones kept on in machine.toml.
#[test]
fn setup_offers_and_enables_opt_in_presets() {
    let fixture = Setup::new();
    std::fs::create_dir_all(fixture.home.path().join(".kiro")).unwrap();
    let out = fixture.setup("1\n\n\nn\n");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}{}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout.contains("Sync kiro (~/.kiro/skills, ~/.kiro/steering)? [Y/n]"), "{stdout}");
    let config = std::fs::read_to_string(fixture.home.path().join("machine.toml")).unwrap();
    assert!(config.starts_with("# my machine\n") && config.contains("[harnesses.kiro]\nenabled = true\n"), "{config}");
    assert!(!fixture.home.path().join("presets.toml").exists());
}

/// First-run setup offers the config directory an environment variable
/// points at as on, and adds it as a preset instance when kept; one that
/// overlaps a harness is shown with why, and not offered.
#[test]
fn setup_offers_config_directories_from_the_environment() {
    let fixture = Setup::new();
    let root = fixture.home.path();
    for dir in [".claude_2", "a"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    // `a` is the custom harness's skills directory: CODEX_HOME would sync around it.
    let env = [("CLAUDE_CONFIG_DIR", root.join(".claude_2")), ("CODEX_HOME", root.join("a"))];
    let env: Vec<_> = env.iter().map(|(var, dir)| (*var, dir.as_path())).collect();
    let out = fixture.setup_env(&root.join("repo"), "1\n\n\n\n", &env);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}{}", String::from_utf8_lossy(&out.stderr));
    let offered =
        "Sync claude_2 (claude in ~/.claude_2, from CLAUDE_CONFIG_DIR: ~/.claude_2/skills, ~/.claude_2/rules)? [Y/n]";
    assert!(stdout.contains(offered), "{stdout}");
    // `a` is taken, so it is offered under the preset's name.
    assert!(stdout.contains("codex-2 (codex in ~/a, from CODEX_HOME): not offered: overlaps a\n"), "{stdout}");
    let config = std::fs::read_to_string(root.join("machine.toml")).unwrap();
    assert!(config.contains("[harnesses.claude_2]\npreset = \"claude\"\nhome = \"~/.claude_2\"\n"), "{config}");
    assert!(!config.contains("codex"), "{config}");
}

/// `sk1ll init` on a machine that already has a machine.toml (set up with
/// an older sk1ll, say) takes up no opt-in preset and adds no config
/// directory from the environment; on a new one it does both.
#[test]
fn init_adopts_opt_in_presets_only_without_a_machine_toml() {
    let fixture = Setup::new();
    let root = fixture.home.path();
    std::fs::create_dir_all(root.join(".kiro")).unwrap();
    std::fs::create_dir_all(root.join(".claude_2")).unwrap();
    let init = |config: &str, repo: &str| {
        let out = fixture
            .home
            .command(BIN)
            .env("CLAUDE_CONFIG_DIR", root.join(".claude_2"))
            .arg("--machine-config")
            .arg(root.join(config))
            .arg("init")
            .arg(root.join(repo))
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    };
    let before = std::fs::read_to_string(root.join("machine.toml")).unwrap();
    init("machine.toml", "repo");
    assert_eq!(std::fs::read_to_string(root.join("machine.toml")).unwrap(), before);
    init("new/machine.toml", "repo2");
    let config = std::fs::read_to_string(root.join("new/machine.toml")).unwrap();
    assert!(config.contains("[harnesses.kiro]\nenabled = true\n"), "{config}");
    assert!(config.contains("[harnesses.claude_2]\npreset = \"claude\"\nhome = \"~/.claude_2\"\n"), "{config}");
}

#[test]
fn setup_validates_answers_and_explains_first_machine_deployment() {
    for (answers, code, prompts, error) in [
        ("4\n1\nmaybe\n", 1, &["enter 1, 2 or 3", "answer y or n"][..], "setup needs answers on standard input"),
        (
            "2\n\n\nftp://x\nhttp://127.0.0.1:1\n\n",
            1,
            &["must start with http:// or https://"],
            "join code is required",
        ),
        (
            "3\n\n\n",
            1,
            &["Join as the first machine", "deployment section in docs/fleet.md"],
            "setup needs answers on standard input",
        ),
    ] {
        let fixture = Setup::new();
        let output = fixture.setup(answers);
        assert_eq!(output.status.code(), Some(code), "{}", text(&output));
        let stdout = String::from_utf8(output.stdout).unwrap();
        for expected in prompts {
            assert!(stdout.contains(expected), "{stdout}");
        }
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(error), "{stderr}");
        assert!(!fixture.home.path().join("repo").exists());
    }
}

#[test]
fn setup_prints_a_shell_command_that_accepts_spaces_and_apostrophes() {
    let fixture = Setup::new();
    let root = fixture.home.path();
    let repo = root.join("the machine's skills");
    let out = outcome(fixture.setup_at(&repo, "1\n\n\n")).unwrap();
    write(&repo.join("personal/pdf/SKILL.md"), "PDFs.");
    let preview = out
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("sk1ll ") && line.ends_with(" sync --dry-run"))
        .unwrap();
    let bin = root.join("bin");
    fs::create_dir(&bin).unwrap();
    symlink(BIN, bin.join("sk1ll")).unwrap();
    let old_path = std::env::var_os("PATH").unwrap_or_default();
    let search_path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&old_path))).unwrap();
    let output =
        fixture.home.command("sh").current_dir(root).env("PATH", search_path).args(["-c", preview]).output().unwrap();
    assert!(outcome(output).unwrap().contains("pdf"), "{preview}");
    assert!(!root.join("a/pdf").exists(), "the printed command previews only");
}

#[test]
fn setup_turns_on_existing_fleet_items_and_defaults_for_new_items() {
    runtime().block_on(async {
        let server = Server::start().await;
        let bootstrap = Machine::new();
        join_fleet(&bootstrap, &server, "first", None);
        run(&bootstrap, &["new", "skill", "pdf"]).unwrap();
        run(&bootstrap, &["new", "rule", "git"]).unwrap();
        run(&bootstrap, &["commit", "--all", "--push"]).unwrap();

        let fixture = Setup::new();
        let code = server.join_code().await;
        let output = fixture.setup(&format!("2\n\n\n{}\n{code}\n\n\n\n", server.url()));
        let stdout = outcome(output).unwrap();
        assert!(stdout.contains("turned on everything for box"), "{stdout}");

        let snapshot: sk1ll_proto::AssignmentSnapshot = server.get("/machines/box/assignment").await;
        let repo = sk1ll_core::Repo::open(fixture.home.path().join("repo")).unwrap();
        let pdf = repo.skills().unwrap().pop().unwrap().id.unwrap();
        let git = repo.rules().unwrap().pop().unwrap().id.unwrap();
        for harness in ["a", "b"] {
            let assignment = snapshot.assignment.harness(harness);
            assert!(assignment.new_skills_default && assignment.new_rules_default);
            assert!(snapshot.assignment.skill_enabled(harness, &pdf));
            assert!(snapshot.assignment.rule_enabled(harness, &git));
            assert!(fixture.home.path().join(harness).join("pdf/SKILL.md").is_file());
        }
    });
}
