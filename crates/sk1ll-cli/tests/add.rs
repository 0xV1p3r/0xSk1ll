//! `sk1ll add` from a local source repo with several skills, working
//! directly (no daemon) with its own HOME and cache.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Env {
    root: PathBuf,
}

impl Env {
    fn new(test: &str) -> Self {
        let root = std::env::temp_dir().join(format!("sk1ll-cli-add-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let origin = root.join("origin");
        for (dir, description) in [("skills/pdf", "Work with PDFs"), ("skills/web", "Browse the web")] {
            std::fs::create_dir_all(origin.join(dir)).unwrap();
            std::fs::write(origin.join(dir).join("SKILL.md"), format!("---\ndescription: {description}\n---\n"))
                .unwrap();
        }
        git(&origin, &["init", "--quiet", "-b", "main"]);
        git(&origin, &["add", "-A"]);
        git(&origin, &["-c", "user.name=T", "-c", "user.email=t@example.com", "commit", "--quiet", "-m", "seed"]);
        sk1ll_core::Repo::init(&root.join("repo")).unwrap();
        Self { root }
    }

    fn origin(&self) -> String {
        self.root.join("origin").to_string_lossy().into_owned()
    }

    fn sk1ll(&self, args: &[&str]) -> Output {
        self.sk1ll_in(&self.root, args)
    }

    fn sk1ll_in(&self, cwd: &Path, args: &[&str]) -> Output {
        let r = |p: &str| self.root.join(p).to_string_lossy().into_owned();
        Command::new(env!("CARGO_BIN_EXE_sk1ll"))
            .current_dir(cwd)
            .args(["--repo", &r("repo"), "--daemon-socket", &r("absent.sock"), "--daemon-state", &r("absent.json")])
            .args(args)
            .env("HOME", r("home"))
            .env("XDG_CONFIG_HOME", r("home/.config"))
            .env("XDG_STATE_HOME", r("home/.local/state"))
            .env("XDG_CACHE_HOME", r("home/.cache"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            // `github.com/o/r` is the local origin, so no test touches the network.
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", format!("url.{}.insteadOf", self.origin()))
            .env("GIT_CONFIG_VALUE_0", "https://github.com/o/r.git")
            .output()
            .unwrap()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

fn text(out: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

#[cfg(unix)]
#[test]
fn add_lists_picks_and_adds_a_skill() {
    let env = Env::new("list");
    let origin = env.origin();
    let manifest = || std::fs::read_to_string(env.root.join("repo/sk1ll.toml")).unwrap();
    let before = manifest();

    let list = env.sk1ll(&["add", "--list", &origin]);
    assert!(list.status.success(), "{}", text(&list));
    let listed = text(&list);
    assert!(listed.contains("skills/pdf") && listed.contains("Work with PDFs"), "{listed}");
    assert!(listed.contains("skills/web") && listed.contains("Browse the web"), "{listed}");
    assert_eq!(manifest(), before, "--list writes nothing");

    let ambiguous = env.sk1ll(&["add", &origin]);
    assert!(!ambiguous.status.success());
    let err = text(&ambiguous);
    assert!(err.contains("skills/pdf, skills/web") && err.contains("--path") && err.contains("--list"), "{err}");
    assert_eq!(manifest(), before);

    let added = env.sk1ll(&["add", &origin, "--path", "skills/pdf"]);
    assert!(added.status.success(), "{}", text(&added));
    assert!(String::from_utf8_lossy(&added.stdout).starts_with("added pdf @ "), "{}", text(&added));
    assert!(env.root.join("repo/imported/pdf/SKILL.md").is_file());

    let list = text(&env.sk1ll(&["add", "--list", &origin]));
    let pdf = list.lines().find(|l| l.starts_with("skills/pdf")).unwrap();
    assert!(pdf.contains("taken"), "{list}");

    // An explicit ref, HEAD included, is what gets recorded.
    let added = env.sk1ll(&["add", &origin, "--path", "skills/web", "--ref", "HEAD", "--name", "web2"]);
    assert!(added.status.success(), "{}", text(&added));
    assert!(manifest().contains("[import.web2]") && manifest().contains("ref = \"HEAD\""), "{}", manifest());
}

#[cfg(unix)]
#[test]
fn fetch_failures_keep_gits_message_and_say_what_to_check() {
    let env = Env::new("fetch");
    let missing = env.root.join("no-such-repo").to_string_lossy().into_owned();
    for args in [vec!["add", &missing], vec!["add", "--list", &missing]] {
        let out = env.sk1ll(&args);
        assert!(!out.status.success());
        let err = text(&out);
        assert!(err.contains("`git clone") && err.contains("no-such-repo"), "{args:?}: {err}");
        assert_eq!(err.matches("Check the repository URL and your git credentials").count(), 1, "{err}");
    }
}

#[cfg(unix)]
#[test]
fn relative_sources_mean_the_cwd_on_the_command_line_and_the_repo_root_in_the_manifest() {
    let env = Env::new("relative");
    let cwd = env.root.join("repo/personal");
    let add = env.sk1ll_in(&cwd, &["add", "../../origin", "--path", "skills/pdf"]);
    assert!(add.status.success(), "{}", text(&add));
    let manifest = std::fs::read_to_string(env.root.join("repo/sk1ll.toml")).unwrap();
    assert!(manifest.contains(&format!("source = \"{}\"", env.origin())), "recorded absolute: {manifest}");

    std::fs::write(env.root.join("repo/sk1ll.toml"), "[import.web]\nsource = \"../origin\"\npath = \"skills/web\"\n")
        .unwrap();
    // Scheme-less GitHub links and invalid sources are not treated as paths.
    let link = env.sk1ll_in(&cwd, &["add", "--list", "github.com/o/r"]);
    assert!(link.status.success() && text(&link).contains("github:o/r @ HEAD"), "{}", text(&link));
    let dash = env.sk1ll_in(&cwd, &["add", "--", "-x"]);
    assert!(text(&dash).contains("invalid source `-x`"), "{}", text(&dash));

    let update = env.sk1ll(&["update", "web"]);
    assert!(update.status.success(), "{}", text(&update));
    assert!(env.root.join("repo/imported/web/SKILL.md").is_file());
}
