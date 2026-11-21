#[test]
fn recover_list_shows_displaced_content() {
    let root = std::env::temp_dir().join(format!("sk1ll-cli-recover-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(root.clone());
    let repo = root.join("repo");
    sk1ll_core::Repo::init(&repo).unwrap();
    let state = if cfg!(target_os = "macos") { "Library/Application Support" } else { "state" };
    let entry = root.join(state).join("sk1ll/displaced/test-entry");
    std::fs::create_dir_all(&entry).unwrap();
    std::fs::write(entry.join("content"), "edited rule").unwrap();
    std::fs::write(
        entry.join("entry.json"),
        format!(
            r#"{{"version":3,"harness":"agent","kind":"rule","dest":"{}","expected":"","displaced_at":0}}"#,
            root.join("rule.md").display()
        ),
    )
    .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sk1ll"))
        .args([
            "--repo",
            repo.to_str().unwrap(),
            "--daemon-socket",
            root.join("absent.sock").to_str().unwrap(),
            "--daemon-state",
            root.join("absent.json").to_str().unwrap(),
            "recover",
            "list",
        ])
        .env("HOME", &root)
        .env("XDG_STATE_HOME", root.join("state"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("test-entry") && text.contains("agent") && text.contains("(changed)"), "{text}");
}
