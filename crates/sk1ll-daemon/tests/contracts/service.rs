use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use sk1ll_daemon::service::{Environment, render_agent, render_unit};

#[test]
fn unit_points_at_the_binary() {
    let unit = render_unit(Path::new("/opt/my apps/100%/sk1ll"));
    assert!(unit.contains("\nExecStart=\"/opt/my apps/100%%/sk1ll\" daemon\n"), "{unit}");
    assert_eq!(unit.matches("ExecStart=").count(), 1);
    assert!(!unit.contains(".local/bin"), "{unit}");
    assert!(unit.contains("[Install]\nWantedBy=default.target\n"), "{unit}");
}

#[test]
fn agent_runs_the_binary_and_logs_under_home() {
    let agent = render_agent(Path::new("/Applications/0xSk1ll & co.app/Contents/MacOS/sk1ll"), Path::new("/Users/me"));
    assert!(
        agent.contains(
            "<array>\n    <string>/Applications/0xSk1ll &amp; co.app/Contents/MacOS/sk1ll</string>\n    <string>daemon</string>\n  </array>"
        ),
        "{agent}"
    );
    assert!(agent.contains("<key>Label</key>\n  <string>app.v1p3r.sk1ll.daemon</string>"), "{agent}");
    assert_eq!(agent.matches("<string>/Users/me/Library/Logs/sk1ll/daemon.log</string>").count(), 2, "{agent}");
    assert!(!agent.contains("<string>/Users/USER"), "{agent}");
    assert!(!agent.contains(".local/bin"), "{agent}");
    assert!(agent.contains("<key>RunAtLoad</key>\n  <true/>"), "{agent}");
    assert!(agent.ends_with("</plist>\n"));
}

#[test]
fn binary_is_found_next_to_the_executable_then_on_path_then_in_the_user_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let make = |sub: &str| {
        let path = dir.path().join(sub).join("sk1ll");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    };
    let env = || Environment {
        exe_dir: Some(dir.path().join("app")),
        path: vec![dir.path().join("bin")],
        home: Some(dir.path().join("home")),
    };
    assert_eq!(env().find_binary(), None);
    let cargo = make("home/.cargo/bin");
    assert_eq!(env().find_binary(), Some(cargo));
    let local = make("home/.local/bin");
    assert_eq!(env().find_binary(), Some(local));
    let on_path = make("bin");
    assert_eq!(env().find_binary(), Some(on_path.clone()));
    let bundled = make("app");
    assert_eq!(env().find_binary(), Some(bundled.clone()));
    std::fs::set_permissions(&bundled, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(env().find_binary(), Some(on_path));
}

#[cfg(target_os = "macos")]
#[test]
fn moving_a_daemon_reloads_the_generated_agent_and_preserves_an_edited_plist() {
    use sk1ll_daemon::service::{self, Started};
    use sk1ll_testkit::{Home, child, read, write};

    if let Some(root) = child::dir() {
        let old = root.join("old & app/sk1ll");
        let new = root.join("new & app/sk1ll");
        let path = service::agent_path().unwrap();
        let output = root.join("calls");
        let generated = render_agent(&old, &root);
        let edited = generated.replace("<string>info</string>", "<string>debug</string>");
        write(&path, &edited);
        let error = service::start(&new).unwrap_err().to_string();
        assert!(error.contains(old.to_str().unwrap()));
        assert_eq!(read(&path), edited);
        assert!(!read(&output).contains("bootout"));

        write(&path, &generated);
        write(&output, "");
        assert!(matches!(service::start(&new).unwrap(), Started::Launchd));
        let installed = read(&path);
        assert!(installed.contains("new &amp; app/sk1ll"));
        assert!(installed.contains("git &amp; tools:/opt/homebrew/bin:"));
        assert!(service::agent_log_path().unwrap().parent().unwrap().is_dir());
        let calls = read(&output);
        // SAFETY: getuid reads the current process UID and takes no arguments.
        let uid = unsafe { libc::getuid() };
        let domain = format!("gui/{uid}");
        let target = format!("{domain}/{}", service::LABEL);
        assert!(calls.contains(&format!("bootout {target}")));
        assert!(calls.contains(&format!("bootstrap {domain} {}", path.display())));
        assert!(calls.contains(&format!("kickstart {target}")));
        assert!(service::Manager::Launchd.active());
        assert_eq!(service::Manager::Launchd.main_pid(), Some(4242));
        return;
    }
    let home = Home::new();
    let root = home.path();
    let bin = root.join("git & tools");
    let executable = |path: &Path, script: &str| {
        write(path, script);
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    executable(&bin.join("git"), "#!/bin/sh\nexit 0\n");
    executable(&root.join("new & app/sk1ll"), "#!/bin/sh\nexit 0\n");
    executable(
        &bin.join("launchctl"),
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/calls"
case "$1:$2" in
    print:*/app.v1p3r.sk1ll.daemon)
        test -e "$HOME/unloaded" && exit 1
        printf '\tstate = running\n\tendpoints = {\n\t\tpid = 7\n\t}\n\tpid = 4242\n'
        ;;
    bootout:*) : > "$HOME/unloaded" ;;
    bootstrap:*) /bin/rm "$HOME/unloaded" ;;
esac
exit 0
"#,
    );
    let mut process = child::command("moving_a_daemon_reloads_the_generated_agent_and_preserves_an_edited_plist", root);
    home.apply(&mut process).env("PATH", &bin);
    let output = process.output().unwrap();
    assert!(output.status.success(), "{}", sk1ll_testkit::text(&output));
}
