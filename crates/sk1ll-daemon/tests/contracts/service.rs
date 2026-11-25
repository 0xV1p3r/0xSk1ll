use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use sk1ll_daemon::service::{Environment, render_unit};

#[test]
fn unit_points_at_the_binary() {
    let unit = render_unit(Path::new("/opt/my apps/100%/sk1ll"));
    assert!(unit.contains("\nExecStart=\"/opt/my apps/100%%/sk1ll\" daemon\n"), "{unit}");
    assert_eq!(unit.matches("ExecStart=").count(), 1);
    assert!(!unit.contains(".local/bin"), "{unit}");
    assert!(unit.contains("[Install]\nWantedBy=default.target\n"), "{unit}");
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
