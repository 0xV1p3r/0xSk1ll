use sk1ll_core::Error;
use sk1ll_core::git::Git;
use sk1ll_testkit::{Home, SkillsRepo};
use std::time::{Duration, Instant};

#[cfg(unix)]
fn alive(pid: i32) -> bool {
    // Signal 0 checks for existence; a zombie still counts, so look at /proc too.
    let exists = unsafe { libc::kill(pid, 0) } == 0;
    exists
        && std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map_or(true, |s| s.rsplit(')').next().is_some_and(|rest| !rest.trim_start().starts_with('Z')))
}

/// A network command that hangs is killed at the deadline the user set in
/// `SK1LL_GIT_TIMEOUT` (here `300ms`, in a child process of its own), and
/// so is everything it started.
#[cfg(unix)]
#[test]
fn hung_network_command_times_out_and_is_killed() {
    use sk1ll_testkit::child;
    let Some(dir) = child::dir() else {
        let home = Home::new();
        let mut cmd = child::command("hung_network_command_times_out_and_is_killed", home.path());
        let out = home.apply(&mut cmd).env("SK1LL_GIT_TIMEOUT", "300ms").output().unwrap();
        let log = sk1ll_testkit::text(&out);
        assert!(out.status.success() && log.contains("1 passed"), "{log}");
        return;
    };
    let pidfile = dir.join("ssh.pid");
    // A fake ssh that hangs (and records its pid) instead of connecting.
    let ssh = format!("sleep 30 & echo $! > '{}'; wait; :", pidfile.display());
    let _env = sk1ll_core::failpoints::git_env(&[("GIT_SSH_COMMAND", &ssh), ("GIT_SSH_VARIANT", "simple")]);
    let start = Instant::now();
    let result = Git::new(&dir).args(["ls-remote", "ssh://sk1ll.invalid/repo.git"]).run();
    let elapsed = start.elapsed();
    match result {
        Err(Error::Git { args, stderr }) => {
            assert!(args.starts_with("ls-remote"), "{args}");
            assert!(stderr.contains("timed out after") && stderr.contains("SK1LL_GIT_TIMEOUT"), "{stderr}");
        }
        other => panic!("expected a timeout, got {other:?}"),
    }
    assert!(elapsed < Duration::from_secs(10), "took {elapsed:?}");
    let pid: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
    let gone = Instant::now() + Duration::from_secs(5);
    while alive(pid) && Instant::now() < gone {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!alive(pid), "the hung ssh ({pid}) survived the timeout");
}

#[test]
fn stdin_and_output_still_flow_under_a_deadline() {
    let tmp = tempfile::tempdir().unwrap();
    Git::new(tmp.path()).args(["init", "-q"]).run().unwrap();
    // Larger than a pipe buffer both ways, so neither side can deadlock.
    let input: Vec<u8> = (0..4 << 20).map(|i| (i % 251) as u8).collect();
    let oid = Git::new(tmp.path())
        .args(["hash-object", "-w", "--stdin"])
        .stdin(input.clone())
        .timeout(Some(Duration::from_secs(60)))
        .text()
        .unwrap();
    let out = Git::new(tmp.path()).args(["cat-file", "blob", &oid]).timeout(Some(Duration::from_secs(60))).run();
    assert_eq!(out.unwrap(), input);
}

#[test]
fn ext_import_cannot_execute() {
    let tmp = tempfile::tempdir().unwrap();
    let marker = tmp.path().join("executed");
    let source = format!("ext::sh -c 'touch {}'", marker.display());
    let home = Home::new();
    let _entered = home.enter();
    let fixture = SkillsRepo::new();
    let error = fixture.open().resolve_source(&source, None, None).unwrap_err();
    assert!(error.to_string().contains("transport 'ext' not allowed"), "{error}");
    assert!(!marker.exists());
}

#[test]
fn git_commands_keep_credentials_isolated_and_merge_config_entries_case_insensitively() {
    use sk1ll_core::git::config_env::merge_config_env;
    use std::collections::BTreeMap;

    let home = Home::new();
    let _entered = home.enter();
    let repo = SkillsRepo::new();
    let mut env = BTreeMap::from([
        ("GIT_CONFIG_COUNT".into(), "1".into()),
        ("GIT_CONFIG_KEY_0".into(), "test.Preserved".into()),
        ("GIT_CONFIG_VALUE_0".into(), "mine".into()),
    ]);
    for value in ["first", "updated"] {
        env.extend(merge_config_env(|key| env.get(key).cloned(), "test.Added", value));
    }
    env.extend(merge_config_env(|key| env.get(key).cloned(), "TEST.preserved", "changed"));
    let config = |key| {
        env.iter()
            .fold(Git::new(repo.root()), |git, (k, v)| git.env(k, v))
            .args(["config", "--get-all", key])
            .text()
            .unwrap()
    };
    assert_eq!(config("test.added"), "updated");
    assert_eq!(config("test.preserved"), "changed");
    assert_eq!(env["GIT_CONFIG_COUNT"], "2");
    assert_eq!(
        merge_config_env(|_| Some("bad count".into()), "test.new", "v")[0],
        ("GIT_CONFIG_COUNT".into(), "1".into())
    );

    let locale = Git::new(repo.root())
        .args(["-c", "alias.show-locale=!printf '%s %s' \"$LC_ALL\" \"$LANGUAGE\"", "show-locale"])
        .text()
        .unwrap();
    assert_eq!(locale, "C C");
}

#[test]
fn the_repo_disables_git_drivers_named_by_local_and_environment_config() {
    use sk1ll_core::git::RepoGit;
    let home = Home::new();
    let _entered = home.enter();
    let fixture = SkillsRepo::new();
    let marker = home.path().join("ran");
    let command = format!("touch '{}'; cat", marker.display());
    for (key, value) in [
        ("filter.Mixed=Name.clean", command.as_str()),
        ("filter.Mixed=Name.required", "true"),
        ("filter.a.b.smudge", command.as_str()),
    ] {
        fixture.git(&["config", key, value]);
    }
    fixture.write(".gitattributes", "*.txt filter=Mixed=Name\n*.md filter=cli\n*.data filter=a.b\n");
    let env = [
        ("GIT_CONFIG_COUNT".into(), "2".into()),
        ("GIT_CONFIG_KEY_0".into(), "filter.cli.process".into()),
        ("GIT_CONFIG_VALUE_0".into(), command),
        ("GIT_CONFIG_KEY_1".into(), "filter.cli.required".into()),
        ("GIT_CONFIG_VALUE_1".into(), "true".into()),
    ];
    let repo = RepoGit::with_env(fixture.root(), &env).unwrap();
    for file in ["a.txt", "a.md", "a.data"] {
        fixture.write(file, "local data\n");
    }
    repo.git().args(["add", "-A"]).run().unwrap();
    repo.git().args(["commit", "-qm", "without filters"]).run().unwrap();
    std::fs::remove_file(fixture.root().join("a.data")).unwrap();
    repo.git().args(["restore", "--", "a.data"]).run().unwrap();
    assert_eq!(fixture.read("a.data"), "local data\n");
    assert!(!marker.exists());
}
