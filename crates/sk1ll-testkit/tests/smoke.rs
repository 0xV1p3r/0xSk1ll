//! One smoke test per fixture: each sets up what it says it does.

use sk1ll_core::failpoints::{self, Action, Point};
use sk1ll_core::machine::MachineConfig;
use sk1ll_testkit::{Harness, Home, InstructionsFormat, MachineToml, Rule, Skill, SkillsRepo, child, git};

#[test]
fn a_skills_repo_starts_with_one_unsigned_seed_commit_by_the_test_identity() {
    let repo = SkillsRepo::new();
    assert_eq!(repo.git(&["log", "--format=%an <%ae> %G? %s"]), "Test <test@example.com> N seed");
    assert_eq!(repo.git(&["symbolic-ref", "--short", "HEAD"]), "main");
    assert!(repo.open().draft().unwrap().is_empty());
}

#[test]
fn skill_and_rule_builders_write_items_core_reads() {
    let repo = SkillsRepo::new();
    repo.skill(Skill::new("pdf").description(Some("PDFs")).id("id-pdf").file("notes.md", "notes\n"));
    repo.rule(Rule::new("style").id("id-style").body("Be brief.\n"));
    let sha = repo.commit_all("items");
    assert_eq!(repo.head(), sha);
    let core = repo.open();
    let skill = core.skills().unwrap().into_iter().find(|s| s.name == "pdf").expect("skill pdf");
    assert_eq!(skill.id.map(|id| id.to_string()).as_deref(), Some("id-pdf"));
    assert_eq!(repo.read("personal/pdf/notes.md"), "notes\n");
    assert_eq!(repo.read("instructions/style.md"), "---\nid: id-style\n---\nBe brief.\n");

    let bare = repo.root().parent().unwrap().join("origin.git");
    repo.push_to_new_origin(&bare);
    let clone = SkillsRepo::clone_from(&bare, repo.root().parent().unwrap().join("clone"));
    assert_eq!(clone.head(), sha);
}

#[test]
fn machine_toml_turns_presets_off_and_has_only_its_harnesses() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("machine.toml");
    MachineToml::new()
        .id("box")
        .harness(Harness::copy("a", tmp.path().join("a")))
        .harness(
            Harness::symlink("b", tmp.path().join("b"))
                .instructions(tmp.path().join("b.md"), InstructionsFormat::Block),
        )
        .write(&path);
    let harnesses = MachineConfig::load(&path).unwrap().harnesses();
    assert_eq!(harnesses.keys().collect::<Vec<_>>(), ["a", "b"]);
    assert_eq!(harnesses["a"].mode, sk1ll_testkit::LinkMode::Copy);
    assert_eq!(harnesses["b"].mode, sk1ll_testkit::LinkMode::Symlink);
    assert_eq!(harnesses["b"].instructions_file(), Some(tmp.path().join("b.md")));
}

#[test]
fn a_home_isolates_spawned_commands_and_in_process_core() {
    let home = Home::new();
    let out = home.command("sh").args(["-c", "echo $HOME $XDG_STATE_HOME $GIT_CONFIG_NOSYSTEM"]).output().unwrap();
    let h = home.path().display();
    assert_eq!(String::from_utf8(out.stdout).unwrap().trim(), format!("{h} {h}/.local/state 1"));
    {
        let _in = home.enter();
        assert_eq!(sk1ll_core::paths::state_dir().unwrap(), home.state_dir());
        assert_eq!(sk1ll_core::paths::cache_dir().unwrap(), home.cache_dir());
    }
    assert_ne!(sk1ll_core::paths::state_dir().ok(), Some(home.state_dir()));
}

#[test]
fn git_commands_ignore_global_config() {
    let tmp = tempfile::tempdir().unwrap();
    git::git(tmp.path(), &["init", "--quiet"]);
    let out = git::output(tmp.path(), &["config", "--global", "--list"]);
    assert!(String::from_utf8_lossy(&out.stdout).trim().is_empty());
}

/// The child exits at a failpoint without unwinding; the parent finds the
/// file as the crash left it: staged, not published.
#[test]
fn a_child_crash_at_a_failpoint_ends_the_process_there() {
    if let Some(dir) = child::dir() {
        let _exit = failpoints::install(|point, _| match point {
            Point::ConfigBeforePublish => Action::Exit(child::CRASHED),
            _ => Action::Continue,
        });
        MachineConfig::set_enabled(&dir.join("machine.toml"), "a", false, std::path::Path::new("/nonexistent-repo"))
            .unwrap();
        unreachable!("the process exits at the failpoint");
    }
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("machine.toml");
    MachineToml::new().harness(Harness::copy("a", tmp.path().join("a"))).write(&path);
    let before = sk1ll_testkit::read(&path);
    let out = child::run("a_child_crash_at_a_failpoint_ends_the_process_there", tmp.path());
    assert_eq!(out.status.code(), Some(child::CRASHED), "{}", sk1ll_testkit::text(&out));
    assert_eq!(sk1ll_testkit::read(&path), before);
}

#[cfg(feature = "server")]
#[tokio::test(flavor = "multi_thread")]
async fn a_server_restarts_on_its_port_with_its_data() {
    let mut server = sk1ll_testkit::server::Server::start().await;
    let (port, head) = (server.port(), server.head());
    assert!(head.is_some(), "a fresh server has its root commit");
    assert!(!server.join_code().await.is_empty());
    server.stop();
    assert!(server.http().get(format!("{}/healthz", server.url())).send().await.is_err());
    server.restart().await;
    assert_eq!((server.port(), server.head()), (port, head));
}
