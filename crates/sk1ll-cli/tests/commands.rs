//! `sk1ll` commands without a daemon: each runs the daemon's engine in its
//! own process. Every run gets its own HOME and XDG directories, so no
//! preset finds the developer's own harnesses, and no global git config.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use sk1ll_core::{Assignment, EngineLock, ItemRef, Repo};

struct TestRepo {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
}

impl TestRepo {
    /// A committed repo, a `machine.toml` with every preset off, no daemon.
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let repo = root.join("repo");
        Repo::init(&repo).unwrap();
        git(&repo, &["config", "user.name", "Test"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "seed"]);
        fs::write(root.join("machine.toml"), format!("id = 'test'\n{}", no_presets())).unwrap();
        Self { _tmp: tmp, root, repo }
    }

    /// A personal skill `pdf` and a copy-mode harness `h`.
    fn with_skill(self) -> Self {
        fs::create_dir(self.repo.join("personal/pdf")).unwrap();
        fs::write(self.repo.join("personal/pdf/SKILL.md"), "PDFs.").unwrap();
        self.harness("h");
        self
    }

    /// Adds a copy-mode harness `name` in `<root>/<name>`.
    fn harness(&self, name: &str) -> PathBuf {
        let skills = self.root.join(name);
        let mut config = fs::read_to_string(self.root.join("machine.toml")).unwrap();
        config.push_str(&format!("[harnesses.{name}]\nskills = {:?}\nmode = 'copy'\n", skills.to_str().unwrap()));
        fs::write(self.root.join("machine.toml"), config).unwrap();
        skills
    }

    fn command_on(&self, socket: &Path, args: &[&str]) -> Command {
        self.command_for(&self.repo, socket, args)
    }

    /// A command about `repo`, on this Machine (its config, Assignment and
    /// state file).
    fn command_for(&self, repo: &Path, socket: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sk1ll"));
        cmd.arg("--repo")
            .arg(repo)
            .arg("--machine-config")
            .arg(self.root.join("machine.toml"))
            .arg("--assignment")
            .arg(self.assignment())
            .arg("--daemon-socket")
            .arg(socket)
            .arg("--daemon-state")
            .arg(self.state())
            .args(args)
            .env("HOME", self.root.join("home"))
            .env_remove("SK1LL_STATE_DIR")
            .env("XDG_CONFIG_HOME", self.root.join("home/.config"))
            .env("XDG_CACHE_HOME", self.root.join("home/.cache"))
            .env("XDG_STATE_HOME", self.root.join("home/.local/state"))
            .env("XDG_DATA_HOME", self.root.join("home/.local/share"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("SK1LL_REPO")
            .env_remove("SK1LL_DAEMON_SOCKET");
        cmd
    }

    /// Runs a command with no daemon (a socket path that doesn't exist).
    fn run(&self, args: &[&str]) -> Result<String, String> {
        self.run_on(&self.root.join("no-daemon.sock"), args)
    }

    fn run_on(&self, socket: &Path, args: &[&str]) -> Result<String, String> {
        outcome(self.command_on(socket, args).output().unwrap())
    }

    /// Another committed repo on the same Machine, with a skill `web`.
    fn other_repo(&self) -> PathBuf {
        let other = self.root.join("other");
        Repo::init(&other).unwrap();
        fs::create_dir(other.join("personal/web")).unwrap();
        fs::write(other.join("personal/web/SKILL.md"), "Web.").unwrap();
        git(&other, &["config", "user.name", "Test"]);
        git(&other, &["config", "user.email", "test@example.com"]);
        git(&other, &["add", "."]);
        git(&other, &["commit", "-qm", "seed"]);
        other
    }

    /// The daemon's configuration for this Machine, managing `repo`.
    fn daemon_config(&self, repo: &Path) -> sk1ll_daemon::Config {
        sk1ll_daemon::Config {
            repo: Some(repo.to_path_buf()),
            state: self.state(),
            machine_config: self.root.join("machine.toml"),
            assignment: self.assignment(),
            socket: self.root.join("no-daemon.sock"),
            // As the CLI finds it from `XDG_STATE_HOME`.
            state_dir: self.root.join("home/.local/state/sk1ll"),
            ..sk1ll_daemon::Config::new().unwrap()
        }
    }

    fn assignment(&self) -> PathBuf {
        self.root.join("assignment.json")
    }

    fn state(&self) -> PathBuf {
        self.root.join("state/daemon.json")
    }

    fn snapshot_path(&self) -> PathBuf {
        self.root.join("state/daemon-assignment.json")
    }

    /// Joined to `server_url`; returns the registration's key, which its
    /// Assignment snapshot is tagged with.
    fn joined(&self, server_url: &str) -> String {
        let state = sk1ll_daemon::DaemonState::new("test".into(), server_url.into(), "tok".into(), self.repo.clone());
        state.save(&self.state()).unwrap();
        state.registration_key()
    }

    /// `item`'s Item ID, written first if it has none.
    fn item_id(&self, item: ItemRef) -> sk1ll_core::ItemId {
        Repo::open(&self.repo).unwrap().assign_missing_ids().unwrap();
        let ids = Repo::open(&self.repo).unwrap().item_ids().unwrap();
        ids.into_iter().find(|(found, _)| *found == item).and_then(|(_, id)| id).expect("an item with an Item ID")
    }
}

fn outcome(out: Output) -> Result<String, String> {
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if out.status.success() { Ok(stdout) } else { Err(format!("{stderr}{stdout}")) }
}

/// Disables every preset: they would find harnesses in HOME.
fn no_presets() -> String {
    sk1ll_core::machine::presets().map(|p| format!("[harnesses.{p}]\nenabled = false\n")).collect()
}

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

/// A joined Machine's Assignment changes at the server, through the engine's
/// session: an unreachable server changes nothing.
#[test]
fn fleet_assignment_fails_when_the_server_is_unreachable() {
    let repo = TestRepo::new().with_skill();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let owner = repo.joined(&url);
    let snapshot = sk1ll_proto::AssignmentSnapshot::default();
    sk1ll_daemon::state::save_snapshot(&repo.snapshot_path(), &snapshot, Some(&owner)).unwrap();
    let err = repo.run(&["assign", "h", "skill", "pdf", "on"]).expect_err("fleet assignment must reach the server");
    assert!(err.contains("server unreachable"), "{err}");
    let stored = sk1ll_daemon::state::load_snapshot(&repo.snapshot_path(), Some(&owner)).unwrap().unwrap();
    assert_eq!(stored.revision, 0);
    let pdf = repo.item_id(ItemRef::Skill("pdf".into()));
    let mirror = Assignment::load(&repo.assignment()).unwrap();
    assert!(!mirror.harness("h").skills.contains_key(&pdf), "the mirror cannot record an unaccepted change");
}

/// A joined Machine syncs by the server's snapshot, never by the machine-local
/// file, and leaves the harnesses alone without one.
#[test]
fn sync_uses_the_authoritative_assignment_and_requires_a_snapshot() {
    let repo = TestRepo::new().with_skill();
    let skills = repo.root.join("h");
    let owner = repo.joined("http://127.0.0.1:1");
    let mut assignment = Assignment::default();
    assignment.set_skill("h", &repo.item_id(ItemRef::Skill("pdf".into())), false);
    sk1ll_daemon::state::save_snapshot(
        &repo.snapshot_path(),
        &sk1ll_proto::AssignmentSnapshot { revision: 7, assignment, registration: None },
        Some(&owner),
    )
    .unwrap();
    repo.run(&["sync"]).unwrap();
    assert!(!skills.join("pdf").exists(), "a disabled skill stays out");
    Assignment::default().save(&repo.assignment()).unwrap();
    repo.run(&["sync"]).unwrap();
    assert!(!skills.join("pdf").exists(), "a stale mirror must not override the snapshot");
    fs::remove_file(repo.snapshot_path()).unwrap();
    let err = repo.run(&["sync"]).expect_err("a joined machine needs its authoritative assignment");
    assert!(err.contains("waiting for the server's assignment"), "{err}");
    assert!(!skills.join("pdf").exists());
}

#[test]
fn sync_refuses_a_pending_pull() {
    let repo = TestRepo::new().with_skill();
    let skills = repo.root.join("h");
    fs::write(repo.repo.join(".git/sk1ll-pull.json"), "interrupted journal").unwrap();
    for command in [["sync", "--force"], ["sync", "--dry-run"]] {
        let error = repo.run(&command).unwrap_err();
        assert!(error.contains("pull pending"), "{error}");
        assert!(!skills.exists());
    }
}

#[test]
fn init_takes_the_existing_repos_engine_lock_before_writing() {
    let repo = TestRepo::new();
    fs::remove_file(repo.repo.join("sk1ll.toml")).unwrap();
    let held = EngineLock::acquire(&repo.repo).unwrap();
    for target in [repo.repo.clone(), repo.repo.join("new/nested")] {
        let err = repo.run(&["init", target.to_str().unwrap()]).expect_err("init must hold the engine lock");
        assert!(err.contains("another sk1ll process"), "{err}");
        assert!(!target.join("sk1ll.toml").exists());
    }
    assert!(!repo.repo.join("new").exists(), "even directory creation waits for the lock");
    drop(held);
    repo.run(&["init"]).unwrap();
    assert!(repo.repo.join("sk1ll.toml").is_file());
    let fresh = repo.root.join("fresh");
    repo.run(&["init", fresh.to_str().unwrap()]).unwrap();
    assert!(fresh.join("sk1ll.toml").is_file());
}

#[test]
fn assignment_defaults_preserve_existing_items() {
    let repo = TestRepo::new().with_skill();
    fs::write(repo.repo.join("instructions/git.md"), "Git.").unwrap();
    repo.run(&["assign", "h", "--default-skills", "off", "--default-rules", "off"]).unwrap();
    let a = Assignment::load(&repo.assignment()).unwrap();
    let pdf = repo.item_id(ItemRef::Skill("pdf".into()));
    assert!(a.skill_enabled("h", &pdf));
    assert!(a.rule_enabled("h", &repo.item_id(ItemRef::Rule("git".into()))));
    // Items that appear later follow the new defaults.
    assert!(!a.harness("h").new_skills_default && !a.harness("h").new_rules_default);
    repo.run(&["assign", "h", "skill", "pdf", "off", "--default-skills", "on"]).unwrap();
    let a = Assignment::load(&repo.assignment()).unwrap();
    assert!(!a.skill_enabled("h", &pdf));
}

/// The engine in a command's process takes the engine lock for what it
/// changes, so a change fails while another process (normally the daemon)
/// holds it. Previews read without it; what answers from a reconciled
/// Snapshot says why there is none yet.
#[test]
fn changes_take_the_engine_lock() {
    let repo = TestRepo::new().with_skill();
    let held = EngineLock::acquire(&repo.repo).unwrap();
    let changes: [&[&str]; 12] = [
        &["sync"],
        &["assign", "h", "skill", "pdf", "off"],
        &["assign", "h", "--all", "off"],
        &["push"],
        &["pull"],
        &["commit", "pdf"],
        &["restore", "pdf"],
        &["adopt", "h", "pdf"],
        &["add", "github:o/r"],
        &["update"],
        &["remove", "x"],
        &["fork", "x"],
    ];
    for args in changes {
        let err = repo.run(args).expect_err("refused while the lock is held");
        assert!(err.contains("another sk1ll process"), "{args:?}: {err}");
    }
    assert!(!repo.assignment().exists(), "assign changed nothing");
    let reads: [&[&str]; 7] = [
        &["list"],
        &["status"],
        &["sync", "--dry-run"],
        &["render", "h"],
        &["draft"],
        &["conflicts"],
        &["recover", "list"],
    ];
    for args in reads {
        repo.run(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
    }
    drop(held);
    repo.run(&["assign", "h", "skill", "pdf", "off"]).unwrap();
    assert!(repo.assignment().exists());
}

/// A dry run reads; another process (the daemon) holding the lock may be
/// mid-swap, and its scratch looks just like a crash's leftovers.
#[test]
fn dry_run_leaves_another_syncs_work_in_progress_alone() {
    let repo = TestRepo::new().with_skill();
    let skills = repo.root.join("h");
    let stage = skills.join(".sk1ll-tmp/stage-pdf");
    fs::create_dir_all(&stage).unwrap();
    fs::write(stage.join("SKILL.md"), "being staged by the lock holder").unwrap();
    let held = EngineLock::acquire(&repo.repo).unwrap();
    repo.run(&["sync", "--dry-run"]).unwrap();
    assert!(stage.join("SKILL.md").is_file(), "a dry run must not recover without the engine lock");
    drop(held);
    repo.run(&["sync", "--dry-run"]).unwrap();
    assert!(stage.join("SKILL.md").is_file(), "nor with it: a dry run changes nothing");
    repo.run(&["sync"]).unwrap();
    assert!(!stage.exists(), "a real sync, under the lock, cleans up");
    assert_eq!(fs::read_to_string(skills.join("pdf/SKILL.md")).unwrap(), "PDFs.");
}

/// A copy of a committed skill shares its Item ID; `repair-ids` gives
/// the copy a new one, so the original keeps its toggles and placement.
#[test]
fn repair_ids_keeps_the_id_with_the_baselines_claimant() {
    let repo = TestRepo::new();
    let skills = repo.harness("h");
    fs::create_dir(repo.repo.join("personal/web")).unwrap();
    fs::write(repo.repo.join("personal/web/SKILL.md"), "Web.").unwrap();
    repo.run(&["commit", "web"]).unwrap();
    repo.run(&["assign", "h", "--all", "off"]).unwrap();
    repo.run(&["assign", "h", "skill", "web", "on"]).unwrap();
    repo.run(&["sync"]).unwrap();
    assert!(skills.join("web/SKILL.md").is_file());
    let web = repo.item_id(ItemRef::Skill("web".into()));

    // `cp -r personal/web personal/aweb`: `aweb` sorts first by path.
    fs::create_dir(repo.repo.join("personal/aweb")).unwrap();
    for file in ["SKILL.md", ".sk1ll-id"] {
        fs::copy(repo.repo.join("personal/web").join(file), repo.repo.join("personal/aweb").join(file)).unwrap();
    }
    repo.run(&["repair-ids"]).unwrap();
    assert_eq!(repo.item_id(ItemRef::Skill("web".into())), web);
    assert_ne!(repo.item_id(ItemRef::Skill("aweb".into())), web);
    repo.run(&["sync"]).unwrap();
    assert!(skills.join("web/SKILL.md").is_file(), "web keeps its placement");
    assert!(!skills.join("aweb").exists(), "the copy is new, and new skills are off in h");
}

/// One engine at a time per Machine, whatever repo it manages: while one
/// for this repo runs (a daemon, or another command), a command about
/// another repo changes nothing of the Machine's harnesses or Assignment.
/// Previews still answer, changing nothing either.
#[test]
fn a_second_engine_for_another_repo_is_refused() {
    let repo = TestRepo::new().with_skill();
    let other = repo.other_repo();
    let socket = repo.root.join("no-daemon.sock");
    let skills = repo.root.join("h");
    // The engine managing `repo`, as the daemon would hold it.
    let held = sk1ll_daemon::MachineLock::acquire(&repo.daemon_config(&repo.repo)).unwrap();
    let changes: [&[&str]; 2] = [&["sync"], &["assign", "h", "skill", "web", "off"]];
    for args in changes {
        let err = outcome(repo.command_for(&other, &socket, args).output().unwrap()).expect_err("refused");
        assert!(err.contains("another sk1ll process") && err.contains("managing this Machine"), "{args:?}: {err}");
    }
    assert!(!skills.exists(), "no second engine placed anything");
    assert!(!repo.assignment().exists(), "nor wrote the Assignment");
    for args in [&["status"][..], &["sync", "--dry-run"], &["list"], &["draft"], &["conflicts"]] {
        let out = outcome(repo.command_for(&other, &socket, args).output().unwrap());
        out.unwrap_or_else(|e| panic!("{args:?}: {e}"));
    }
    assert!(!skills.exists() && !repo.assignment().exists(), "previews change nothing");
    drop(held);
    outcome(repo.command_for(&other, &socket, &["sync"]).output().unwrap()).unwrap();
    assert!(skills.join("web/SKILL.md").is_file());
}

/// A standalone daemon managing another repo is this Machine's engine: a
/// command that changes something is refused, saying so, instead of
/// running a second engine beside it; one that only reads runs here.
#[test]
fn a_daemon_for_another_repo_refuses_changes_and_lets_reads_through() {
    let repo = TestRepo::new().with_skill();
    let socket = repo.root.join("daemon.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let status = STATUS.replace(r#""mode":"joined""#, r#""mode":"standalone""#);
    let daemon = std::thread::spawn(move || {
        for _ in 0..2 {
            assert_eq!(answer(&listener, "200 OK", &status).0, "GET /status HTTP/1.1");
        }
    });
    let err = repo.run_on(&socket, &["sync"]).expect_err("the daemon alone changes this Machine");
    assert!(err.contains("the sk1ll daemon manages /elsewhere"), "{err}");
    assert!(!repo.root.join("h").exists());
    let plan = repo.run_on(&socket, &["sync", "--dry-run"]).unwrap();
    assert!(plan.contains("pdf"), "{plan}");
    assert!(!repo.root.join("h").exists());
    daemon.join().unwrap();
}

/// A pull that stopped half-way leaves a partial tree: nothing writes
/// Item IDs into it or pushes until it is finished.
#[test]
fn repair_ids_and_push_wait_for_a_pending_pull() {
    let repo = TestRepo::new().with_skill();
    fs::write(repo.repo.join(".git/sk1ll-pull.json"), "{}").unwrap();
    for args in [&["repair-ids"][..], &["push"]] {
        let err = repo.run(args).expect_err("refused while a pull is pending");
        assert!(err.contains("pull stopped half-way"), "{args:?}: {err}");
    }
    assert!(!repo.repo.join("personal/pdf/.sk1ll-id").exists());
}

/// Reads one request from a fake daemon's socket and answers it with `body`.
fn answer(listener: &std::os::unix::net::UnixListener, status: &str, body: &str) -> (String, Vec<u8>) {
    use std::io::{BufRead, Read, Write};
    let (mut stream, _) = listener.accept().unwrap();
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    let mut reader = std::io::BufReader::new(&mut stream);
    let mut headers = String::new();
    while !headers.ends_with("\r\n\r\n") {
        assert_ne!(reader.read_line(&mut headers).unwrap(), 0);
    }
    let len = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    let mut request = vec![0; len];
    reader.read_exact(&mut request).unwrap();
    write!(stream, "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).unwrap();
    (headers.lines().next().unwrap().to_string(), request)
}

const STATUS: &str = r#"{"machine_id":"work","server_url":"https://fleet.example","mode":"joined","has_origin":true,"repo_ready":true,"repo":"/elsewhere","headline":"offline","connection":"offline","revoked":false,"assignment_revision":2,"report":{}}"#;

#[test]
fn fleet_copy_assignment_confirms_with_preview_preconditions() {
    use serde_json::{Value, json};
    let repo = TestRepo::new();
    let socket = repo.root.join("copy.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let daemon = std::thread::spawn(move || {
        // Fleet commands go to the running daemon, whatever repo it manages.
        assert_eq!(answer(&listener, "200 OK", STATUS).0, "GET /status HTTP/1.1");
        let target = json!({"revision": 2, "registration": "target"});
        let source = json!({"revision": 4, "registration": "source"});
        let path = "/machines/work/assignment/copy-from";
        let changes = json!([
            {"harness":"codex","kind":"skill","item_id":"pdf-id","name":"pdf","before":null,"after":true},
            {"harness":"codex","kind":"new_rules_default","before":true,"after":false},
        ]);
        let preview = json!({"count":2,"changes":changes,"target":target,"source":source});
        for (path, expected, source_expected, response) in [
            (format!("{path}/preview"), Value::Null, Value::Null, preview),
            (path.into(), target, source, json!({"revision":5,"assignment":{"harnesses":{}},"entries":[]})),
        ] {
            let (line, body) = answer(&listener, "200 OK", &response.to_string());
            assert_eq!(line, format!("POST {path} HTTP/1.1"));
            assert_eq!(
                serde_json::from_slice::<Value>(&body).unwrap(),
                json!({
                    "source":"home","mode":"merge","harnesses":[{"from":"claude","to":"codex"}],
                    "expected":expected,"source_expected":source_expected,
                })
            );
        }
    });
    let out = repo
        .run_on(&socket, &["fleet", "copy-assignment", "home", "work", "--merge", "--map", "claude=codex", "--yes"])
        .unwrap();
    daemon.join().unwrap();
    assert!(out.contains("2 Assignment changes on work:"), "{out}");
    assert!(out.contains("skill pdf") && out.contains("default -> on"), "{out}");
    assert!(out.contains("rules default") && out.contains("on -> off"), "{out}");
}

/// A copy that changes nothing says the target matches, and asks nothing.
#[test]
fn fleet_copy_assignment_with_nothing_to_change_asks_nothing() {
    let repo = TestRepo::new();
    let socket = repo.root.join("copy.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let daemon = std::thread::spawn(move || {
        assert_eq!(answer(&listener, "200 OK", STATUS).0, "GET /status HTTP/1.1");
        let state = r#"{"revision":2,"registration":"r"}"#;
        let preview = format!(r#"{{"count":0,"changes":[],"target":{state},"source":{state}}}"#);
        assert_eq!(
            answer(&listener, "200 OK", &preview).0,
            "POST /machines/work/assignment/copy-from/preview HTTP/1.1"
        );
    });
    let out = repo.run_on(&socket, &["fleet", "copy-assignment", "home", "work"]).unwrap();
    daemon.join().unwrap();
    assert_eq!(out, "work already matches home's Assignment; nothing to copy\n");
}

/// Only "no socket" and "connection refused" mean no daemon runs; any
/// other failure to talk to it is reported instead of running the engine
/// in this process.
#[test]
fn only_a_missing_or_refused_socket_means_no_daemon() {
    let repo = TestRepo::new().with_skill();
    let sock = repo.root.join("daemon.sock");
    let assign = ["assign", "h", "skill", "pdf", "off"];

    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let server = std::thread::spawn(move || answer(&listener, "500 Internal Server Error", "oops"));
    let err = repo.run_on(&sock, &assign).expect_err("a broken daemon isn't bypassed");
    assert!(err.contains("cannot talk to the sk1ll daemon"), "{err}");
    assert!(!repo.assignment().exists());
    server.join().unwrap();

    // The listener is gone but its socket file stays: connection refused.
    assert!(sock.exists());
    repo.run_on(&sock, &assign).unwrap();
    assert!(repo.assignment().exists());

    fs::remove_file(&sock).unwrap();
    repo.run_on(&sock, &["assign", "h", "skill", "pdf", "on"]).unwrap();
}

/// `new`, `rename` and `delete` do what the Library does, and `remove` of a
/// personal item points to `delete`.
#[test]
fn new_rename_and_delete_personal_items() {
    let repo = TestRepo::new();
    repo.harness("h");
    let created = repo.run(&["new", "skill", "review", "--description", "Review a change"]).unwrap();
    let skill = repo.repo.join("personal/review/SKILL.md");
    assert!(created.contains(&skill.display().to_string()) && created.contains("`sk1ll edit review`"), "{created}");
    assert!(fs::read_to_string(&skill).unwrap().contains("Review a change"));
    repo.run(&["new", "rule", "review"]).unwrap();
    assert!(repo.repo.join("instructions/review.md").is_file());

    let error = repo.run(&["remove", "review"]).unwrap_err();
    assert!(error.contains("`sk1ll delete skill:review` or `sk1ll delete rule:review`"), "{error}");
    let error = repo.run(&["remove", "skill:review"]).unwrap_err();
    assert!(
        error.contains("personal skill, not an import") && error.contains("`sk1ll delete skill:review`"),
        "{error}"
    );
    assert!(repo.run(&["rename", "review", "x"]).unwrap_err().contains("say `skill:review` or `rule:review`"));

    repo.run(&["assign", "h", "skill", "review", "off"]).unwrap();
    let id = repo.item_id(ItemRef::Skill("review".into()));
    let renamed = repo.run(&["rename", "skill:review", "code-review"]).unwrap();
    assert!(renamed.contains("`sk1ll commit skill:review skill:code-review`"), "{renamed}");
    assert_eq!(repo.item_id(ItemRef::Skill("code-review".into())), id, "the Item ID goes with it");
    assert!(!Assignment::load(&repo.assignment()).unwrap().skill_enabled("h", &id), "and so does its toggle");

    let error = repo.run(&["delete", "code-review"]).unwrap_err();
    assert!(error.contains("terminal confirmation or `--yes`"), "{error}");
    assert!(repo.repo.join("personal/code-review").is_dir());
    let deleted = repo.run(&["delete", "code-review", "--yes"]).unwrap();
    assert!(deleted.contains("`sk1ll restore skill:code-review`"), "{deleted}");
    assert!(!repo.repo.join("personal/code-review").exists());
    assert!(repo.run(&["delete", "nope", "--yes"]).unwrap_err().contains("no personal skill or rule named `nope`"));
}

/// `list` labels rules `rule`, leaves no trailing space for an item without
/// a description, and speaks of Item IDs only when a pending pull holds them up.
#[test]
fn list_labels_rules_and_mentions_missing_ids_only_when_they_wait() {
    let repo = TestRepo::new().with_skill();
    fs::write(repo.repo.join("instructions/git.md"), "Use Conventional Commits.\n").unwrap();
    let listed = repo.run(&["list"]).unwrap();
    assert_eq!(listed, format!("{:<24} personal\n{:<24} rule\n", "pdf", "git"));
    fs::write(repo.repo.join(".git/sk1ll-pull.json"), "{}").unwrap();
    let listed = repo.run(&["list"]).unwrap();
    assert!(
        listed.contains("2 items have no Item ID until the pending pull finishes") && listed.contains("`sk1ll pull`"),
        "{listed}"
    );
}

#[test]
fn list_and_status_report_duplicate_item_id_paths() {
    let repo = TestRepo::new().with_skill();
    Repo::open(&repo.repo).unwrap().create_item("skill", "docs", "").unwrap();
    fs::copy(repo.repo.join("personal/docs/.sk1ll-id"), repo.repo.join("personal/pdf/.sk1ll-id")).unwrap();
    for command in ["list", "status"] {
        let err = repo.run(&[command]).unwrap_err();
        for expected in ["duplicate Item ID", "personal/docs/.sk1ll-id", "personal/pdf/.sk1ll-id", "sk1ll repair-ids"] {
            assert!(err.contains(expected), "{command}: {err}");
        }
    }
}

#[test]
fn recovery_commands_work_with_damaged_configuration() {
    for deleted in [false, true] {
        for command in ["draft", "diff", "restore", "commit", "pull"] {
            let repo = TestRepo::new();
            // A local origin lets pull run through its real Git path.
            git(&repo.repo, &["init", "--bare", "--quiet", ".git/test-origin.git"]);
            git(&repo.repo, &["remote", "add", "origin", ".git/test-origin.git"]);
            git(&repo.repo, &["push", "--quiet", "-u", "origin", "HEAD"]);
            if deleted {
                fs::remove_file(repo.repo.join("sk1ll.toml")).unwrap();
            } else {
                fs::write(repo.repo.join("sk1ll.toml"), "[broken").unwrap();
            }
            fs::write(repo.repo.join("sk1ll.lock"), "[broken").unwrap();
            assert!(repo.run(&["list"]).is_err());
            let args = match command {
                "diff" | "restore" => vec![command, "manifest"],
                "commit" => vec![command, "manifest", "--author", "Test <test@example.com>"],
                _ => vec![command],
            };
            let result = repo.run(&args);
            if command == "commit" && !deleted {
                // A commit cannot validate Import ID claims in invalid TOML.
                let error = result.unwrap_err();
                assert!(error.contains("sk1ll.toml"), "{error}");
            } else {
                assert!(result.is_ok(), "{command}, deleted={deleted}: {result:?}");
            }
            if command == "restore" {
                assert!(Repo::open(&repo.repo).is_ok());
            } else {
                assert!(Repo::open(&repo.repo).is_err());
            }
        }
    }
}

#[test]
fn harness_changes_preview_apply_and_sync() {
    let repo = TestRepo::new();
    fs::create_dir_all(repo.repo.join("personal/pdf")).unwrap();
    fs::write(repo.repo.join("personal/pdf/SKILL.md"), "PDFs.").unwrap();
    let config = repo.root.join("machine.toml");
    fs::write(&config, format!("# keep me\n{}", no_presets())).unwrap();
    let skills = repo.root.join("h/skills");

    let add = ["harnesses", "add", "h", "--skills", skills.to_str().unwrap(), "--mode", "copy", "--yes"];
    repo.run(&add).unwrap();
    assert!(skills.join("pdf/SKILL.md").is_file(), "adding a harness syncs it");
    assert!(fs::read_to_string(&config).unwrap().starts_with("# keep me\n"));
    repo.run(&["harnesses"]).unwrap();

    repo.run(&["harnesses", "disable", "h", "--yes"]).unwrap();
    assert!(skills.join("pdf").is_dir(), "left in place");
    repo.run(&["harnesses", "enable", "h", "--yes"]).unwrap();
    // Its items off first: sync takes out what it placed, then disabling leaves nothing.
    repo.run(&["assign", "h", "--all", "off"]).unwrap();
    repo.run(&["sync"]).unwrap();
    assert!(!skills.join("pdf").exists());
    repo.run(&["harnesses", "disable", "h", "--yes"]).unwrap();
    let assignment = Assignment::load(&repo.assignment()).unwrap();
    let pdf = Repo::open(&repo.repo).unwrap().skills().unwrap()[0].id.clone().expect("an Item ID");
    assert!(!assignment.skill_enabled("h", &pdf) && !assignment.harness("h").new_skills_default);

    repo.run(&["harnesses", "remove", "h", "--yes"]).unwrap();
    assert!(!fs::read_to_string(&config).unwrap().contains("[harnesses.h]"));
    assert!(repo.run(&["harnesses", "remove", "h", "--yes"]).is_err());
    assert!(repo.run(&["harnesses", "add", "x", "--skills", "relative", "--yes"]).is_err());
}

/// A pending pull's tree is partial: the change is saved, but nothing
/// is placed and no Item ID is written until the pull is finished.
#[test]
fn a_harness_change_during_a_pending_pull_defers_placement() {
    let repo = TestRepo::new();
    fs::create_dir_all(repo.repo.join("personal/pdf")).unwrap();
    fs::write(repo.repo.join("personal/pdf/SKILL.md"), "PDFs.").unwrap();
    fs::write(repo.repo.join(".git/sk1ll-pull.json"), "{}").unwrap();
    let skills = repo.root.join("h/skills");
    let add = ["harnesses", "add", "h", "--skills", skills.to_str().unwrap(), "--mode", "copy", "--yes"];
    // The sync after the change reports the stopped pull it can't finish.
    if let Err(error) = repo.run(&add) {
        assert!(error.contains("machine.toml changed, but") && error.contains("pull"), "{error}");
    }
    assert!(fs::read_to_string(repo.root.join("machine.toml")).unwrap().contains("[harnesses.h]"));
    assert!(!skills.join("pdf").exists(), "placement waits for the pull");
    assert!(!repo.repo.join("personal/pdf/.sk1ll-id").exists(), "and so do Item IDs");
}

#[test]
fn harness_changes_stop_on_an_unreadable_assignment_and_report_failed_placements() {
    use std::os::unix::fs::PermissionsExt;
    let repo = TestRepo::new();
    fs::create_dir_all(repo.repo.join("personal/pdf")).unwrap();
    fs::write(repo.repo.join("personal/pdf/SKILL.md"), "PDFs.").unwrap();
    let config = repo.root.join("machine.toml");
    let before = fs::read_to_string(&config).unwrap();
    let skills = repo.root.join("h/skills");
    let add = ["harnesses", "add", "h", "--skills", skills.to_str().unwrap(), "--mode", "copy", "--yes"];

    fs::write(repo.assignment(), "{ not json").unwrap();
    assert!(repo.run(&add).is_err());
    assert_eq!(fs::read_to_string(&config).unwrap(), before, "nothing changes when the assignment can't be read");
    fs::remove_file(repo.assignment()).unwrap();

    // A skills directory sync can read but not write into.
    fs::create_dir_all(&skills).unwrap();
    fs::set_permissions(&skills, fs::Permissions::from_mode(0o555)).unwrap();
    let err = repo.run(&add).unwrap_err();
    fs::set_permissions(&skills, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(err.contains("machine.toml changed, but"), "{err}");
    assert!(fs::read_to_string(&config).unwrap().contains("[harnesses.h]"), "the change itself was made");
}

/// Two Machines sharing a bare `origin`: `a` (its repo pushed there first)
/// and `b`, a clone of it.
fn two_machines() -> (TestRepo, TestRepo, tempfile::TempDir) {
    let origin = tempfile::tempdir().unwrap();
    let bare = origin.path().join("origin.git");
    let a = TestRepo::new().with_skill();
    git(origin.path(), &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()]);
    git(&a.repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
    a.run(&["sync"]).unwrap();
    a.run(&["commit", "pdf", "--push"]).unwrap();
    let b = TestRepo::new();
    fs::remove_dir_all(&b.repo).unwrap();
    git(origin.path(), &["clone", "-q", bare.to_str().unwrap(), b.repo.to_str().unwrap()]);
    git(&b.repo, &["config", "user.name", "Test"]);
    git(&b.repo, &["config", "user.email", "test@example.com"]);
    (a, b, origin)
}

/// The route the Blocked message gives works, also after committing the
/// clashing item: the pull turns the conflicting unpushed commit back into
/// Draft (nothing lost), restore keeps that version in recovery, the pull
/// goes through, and adopting the entry brings the version back as Draft
/// on the new Baseline, ready to commit again.
#[test]
fn a_blocked_machine_that_committed_the_clash_follows_the_hint_out() {
    let (a, b, _origin) = two_machines();
    let skill = |repo: &Path| fs::read_to_string(repo.join("personal/pdf/SKILL.md")).unwrap();
    fs::write(b.repo.join("personal/pdf/SKILL.md"), "b's edit\n").unwrap();
    fs::write(a.repo.join("personal/pdf/SKILL.md"), "a's edit\n").unwrap();
    a.run(&["commit", "pdf", "--push"]).unwrap();

    let blocked = b.run(&["pull"]).unwrap_err();
    assert!(blocked.contains("Run `sk1ll restore skill:pdf`"), "{blocked}");
    assert!(!blocked.contains("commit or restore"), "{blocked}");
    let status = b.run(&["status"]).unwrap();
    assert!(status.contains("blocked: the pull of baseline") && status.contains("sk1ll restore skill:pdf"), "{status}");
    assert!(status.contains("nothing to place") && !status.contains("in sync"), "{status}");
    // What the docs used to suggest: commit the clashing item.
    b.run(&["commit", "pdf"]).unwrap();
    let blocked = b.run(&["pull"]).unwrap_err();
    assert!(blocked.contains("turned 1 unpushed commit(s) back into Draft"), "{blocked}");
    assert!(blocked.contains("Run `sk1ll restore skill:pdf`"), "{blocked}");
    assert_eq!(skill(&b.repo), "b's edit\n", "nothing lost");
    assert!(b.run(&["draft"]).unwrap().contains("M skill:pdf"));
    // Pushing says the same.
    let push = b.run(&["push"]).unwrap_or_else(|e| e);
    assert!(!push.contains("commit or restore"), "{push}");

    let restored = b.run(&["restore", "pdf"]).unwrap();
    let id = restored
        .lines()
        .find_map(|line| line.strip_prefix("kept your version of what the pull is blocked on in recovery: "))
        .unwrap_or_else(|| panic!("{restored}"))
        .to_string();
    b.run(&["pull"]).unwrap();
    assert_eq!(skill(&b.repo), "a's edit\n");
    // The pull keeps what it replaced too, unchanged: the restored entry is the one marked as b's version.
    let listed = b.run(&["recover", "list"]).unwrap();
    let line = listed.lines().find(|l| l.starts_with(&id)).unwrap_or_else(|| panic!("{listed}"));
    assert!(line.contains("skill:pdf") && line.contains(" pull ") && line.contains("your version"), "{listed}");
    assert!(!listed.contains(".sk1ll-tmp"), "paths only with --paths: {listed}");
    assert!(b.run(&["recover", "list", "--paths"]).unwrap().contains(".sk1ll-tmp"));
    let status = b.run(&["status"]).unwrap();
    assert!(status.contains("1 changed entry") && status.contains("`sk1ll recover list`"), "{status}");
    assert!(!status.contains("in sync"), "{status}");
    b.run(&["recover", "adopt", &id]).unwrap();
    assert_eq!(skill(&b.repo), "b's edit\n");
    assert!(b.run(&["status"]).unwrap().contains("harnesses in sync"));
    b.run(&["commit", "pdf", "--push"]).unwrap();
    a.run(&["pull"]).unwrap();
    assert_eq!(skill(&a.repo), "b's edit\n");
}

/// `draft`, `conflicts` and `recover list` only read: without a daemon they
/// write no Item ID and sync nothing, and they answer beside a daemon that
/// manages another repo.
#[test]
fn draft_conflicts_and_recover_list_only_read() {
    let repo = TestRepo::new().with_skill();
    let sidecar = repo.repo.join("personal/pdf").join(sk1ll_core::skill::ID_FILE);
    assert!(repo.run(&["draft"]).unwrap().contains("A skill:pdf"));
    assert_eq!(repo.run(&["conflicts"]).unwrap(), "");
    assert_eq!(repo.run(&["recover", "list"]).unwrap(), "");
    assert!(!sidecar.exists(), "no Item ID written");
    assert!(!repo.root.join("h").exists(), "no harness synced");

    // A hand edit in a harness is a conflict, planned in memory.
    repo.run(&["sync"]).unwrap();
    fs::write(repo.root.join("h/pdf/SKILL.md"), "edited in the harness").unwrap();
    let conflicts = repo.run(&["conflicts"]).unwrap();
    assert!(conflicts.contains("pdf"), "{conflicts}");

    let socket = repo.root.join("daemon.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let status = STATUS.replace(r#""mode":"joined""#, r#""mode":"standalone""#);
    let daemon = std::thread::spawn(move || {
        for _ in 0..3 {
            assert_eq!(answer(&listener, "200 OK", &status).0, "GET /status HTTP/1.1");
        }
    });
    fs::write(repo.repo.join("personal/pdf/SKILL.md"), "Edited.").unwrap();
    assert!(repo.run_on(&socket, &["draft"]).unwrap().contains("A skill:pdf"));
    assert!(repo.run_on(&socket, &["conflicts"]).unwrap().contains("pdf"));
    repo.run_on(&socket, &["recover", "list"]).unwrap();
    daemon.join().unwrap();
}

/// Toggling a rule off and on rewrites a block instructions file twice;
/// neither rewrite leaves a recovery entry, since the block was sk1ll's own.
#[test]
fn toggling_a_rule_in_a_block_file_leaves_no_recovery_entry() {
    let repo = TestRepo::new();
    let agents = repo.root.join("AGENTS.md");
    let mut config = fs::read_to_string(repo.root.join("machine.toml")).unwrap();
    config.push_str(&format!("[harnesses.b]\ninstructions = {:?}\n", agents.to_str().unwrap()));
    fs::write(repo.root.join("machine.toml"), config).unwrap();
    fs::write(repo.repo.join("instructions/git.md"), "---\ndescription: Git\n---\nUse Conventional Commits.\n")
        .unwrap();
    fs::write(&agents, "# Mine\n").unwrap();
    repo.run(&["sync"]).unwrap();
    for state in ["off", "on", "off", "on"] {
        repo.run(&["assign", "b", "rule", "git", state]).unwrap();
        repo.run(&["sync"]).unwrap();
    }
    let text = fs::read_to_string(&agents).unwrap();
    assert!(text.starts_with("# Mine\n") && text.contains("Use Conventional Commits."), "{text}");
    assert_eq!(repo.run(&["recover", "list"]).unwrap(), "");
}

/// A commit whose tree the server would refuse for good (a file through a
/// filter driver) is refused before anything is committed or pushed.
#[test]
fn commit_refuses_a_tree_the_server_would_refuse() {
    let (a, _b, _origin) = two_machines();
    let head = || git_out(&a.repo, &["rev-parse", "HEAD"]);
    let before = head();
    fs::write(a.repo.join(".gitattributes"), "*.bin filter=lfs\n").unwrap();
    fs::write(a.repo.join("personal/pdf/data.bin"), "data\n").unwrap();
    let error = a.run(&["commit", "pdf", "other:.gitattributes", "--push"]).unwrap_err();
    assert!(error.contains("filter driver") && error.contains("personal/pdf/data.bin"), "{error}");
    assert_eq!(head(), before);
    fs::remove_file(a.repo.join(".gitattributes")).unwrap();
    a.run(&["commit", "pdf", "--push"]).unwrap();
}

fn git_out(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// `sk1ll adopt` of a harness copy placed before its skill was renamed
/// updates the renamed skill: the placement's Item ID finds it.
#[test]
fn adopt_updates_a_renamed_skill_instead_of_recreating_the_old_name() {
    let repo = TestRepo::new().with_skill();
    repo.run(&["sync"]).unwrap();
    fs::rename(repo.repo.join("personal/pdf"), repo.repo.join("personal/docs")).unwrap();
    fs::write(repo.root.join("h/pdf/SKILL.md"), "Edited in the harness.").unwrap();
    repo.run(&["adopt", "h", "pdf"]).unwrap();
    assert_eq!(fs::read_to_string(repo.repo.join("personal/docs/SKILL.md")).unwrap(), "Edited in the harness.");
    assert!(!repo.repo.join("personal/pdf").exists(), "the old name is not recreated");
}

/// A manifest committed broken (invalid TOML in HEAD) can be repaired with
/// `sk1ll commit manifest`.
#[test]
fn a_manifest_broken_in_head_can_be_fixed_with_a_commit() {
    let repo = TestRepo::new();
    let manifest = fs::read_to_string(repo.repo.join("sk1ll.toml")).unwrap();
    fs::write(repo.repo.join("sk1ll.toml"), "[import.web\n").unwrap();
    git(&repo.repo, &["commit", "-qam", "broken by hand"]);
    fs::write(repo.repo.join("sk1ll.toml"), &manifest).unwrap();
    repo.run(&["commit", "manifest", "-m", "fix the manifest"]).unwrap();
    assert_eq!(repo.run(&["draft"]).unwrap(), "no draft: on the baseline\n");
}

/// `sync` fails while harness conflicts are left alone, naming the exact
/// command that adopts the edit, and `sync --force` says where the edit it
/// overwrote went.
#[test]
fn sync_fails_on_conflicts_and_force_says_where_the_edit_went() {
    let repo = TestRepo::new().with_skill();
    repo.run(&["sync"]).unwrap();
    fs::write(repo.root.join("h/pdf/SKILL.md"), "edited in the harness").unwrap();
    let error = repo.run(&["sync"]).unwrap_err();
    assert!(error.contains("! h") && error.contains("1 conflict left alone: `sk1ll adopt h pdf`"), "{error}");
    for command in ["status", "conflicts"] {
        let out = repo.run(&[command]).unwrap();
        assert!(out.contains("edited by hand in the harness; `sk1ll adopt h pdf` it into the repo"), "{out}");
    }
    let forced = repo.run(&["sync", "--force"]).unwrap();
    assert!(forced.contains("kept in recovery as ") && forced.contains("h/pdf"), "{forced}");
    let id = forced.split("kept in recovery as ").nth(1).unwrap().split(':').next().unwrap();
    assert!(repo.run(&["recover", "list"]).unwrap().contains(id));
}

/// An empty `SK1LL_*` variable counts as unset, and a `--repo` that isn't
/// there is said to be missing, in those words.
#[test]
fn empty_variables_are_unset_and_a_missing_repo_is_named() {
    let repo = TestRepo::new().with_skill();
    let mut cmd = repo.command_on(&repo.root.join("no-daemon.sock"), &["list"]);
    cmd.env("SK1LL_ADMIN_UNUSED", "").env("SK1LL_SERVER_ADMIN_TOKEN", "");
    outcome(cmd.output().unwrap()).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sk1ll"));
    cmd.arg("list")
        .env("SK1LL_REPO", &repo.repo)
        .env("SK1LL_MACHINE_CONFIG", repo.root.join("machine.toml"))
        .env("SK1LL_ASSIGNMENT", "")
        .env("SK1LL_DAEMON_STATE", "")
        .env("SK1LL_DAEMON_SOCKET", "")
        .env("HOME", repo.root.join("home"))
        .env("XDG_CONFIG_HOME", repo.root.join("home/.config"))
        .env("XDG_CACHE_HOME", repo.root.join("home/.cache"))
        .env("XDG_STATE_HOME", repo.root.join("home/.local/state"))
        .env("XDG_DATA_HOME", repo.root.join("home/.local/share"))
        .env("XDG_RUNTIME_DIR", repo.root.join("run"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    let listed = outcome(cmd.output().unwrap()).unwrap();
    assert!(listed.contains("pdf"), "{listed}");

    let missing = repo.root.join("nowhere");
    let error = repo.command_for(&missing, &repo.root.join("no-daemon.sock"), &["draft"]).output().unwrap();
    let error = outcome(error).unwrap_err();
    assert!(error.contains("no such directory") && error.contains("nowhere"), "{error}");
    assert!(!error.contains("POST") && !error.contains("/init"), "{error}");
}

#[test]
fn recover_subcommands_explain_themselves_and_their_id() {
    let repo = TestRepo::new();
    let help = repo.run(&["recover", "adopt", "--help"]).unwrap();
    assert!(help.contains("<ID>") && help.contains("sk1ll recover list"), "{help}");
    assert!(repo.run(&["recover", "--help"]).unwrap().contains("Delete an entry"));
}

/// `sk1ll init` of another path while a daemon manages a repo says so,
/// rather than that another process holds a lock.
#[test]
fn init_beside_a_daemon_for_another_repo_names_it() {
    let repo = TestRepo::new();
    let socket = repo.root.join("daemon.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let status = STATUS.replace(r#""mode":"joined""#, r#""mode":"standalone""#);
    let daemon = std::thread::spawn(move || {
        assert_eq!(answer(&listener, "200 OK", &status).0, "GET /status HTTP/1.1");
    });
    let target = repo.root.join("new-repo");
    let error = repo.run_on(&socket, &["init", target.to_str().unwrap()]).unwrap_err();
    assert!(error.contains("the sk1ll daemon manages /elsewhere") && error.contains("new-repo"), "{error}");
    assert!(!target.exists());
    daemon.join().unwrap();
}

/// `commit` without items says how to name them or commit them all, and
/// `--all` commits every draft item.
#[test]
fn commit_names_its_items_or_takes_them_all() {
    let repo = TestRepo::new().with_skill();
    fs::write(repo.repo.join("instructions/git.md"), "Use Conventional Commits.\n").unwrap();
    let error = repo.run(&["commit"]).unwrap_err();
    assert!(error.contains("`sk1ll draft`") && error.contains("`sk1ll commit --all`"), "{error}");
    assert!(repo.run(&["commit", "pdf", "--all"]).is_err());
    let out = repo.run(&["commit", "--all"]).unwrap();
    assert!(out.contains("skill:pdf") && out.contains("rule:git"), "{out}");
    assert_eq!(repo.run(&["draft"]).unwrap(), "no draft: on the baseline\n");
    assert!(repo.run(&["commit", "--all"]).unwrap().contains("no draft to commit"));
}

/// A standalone commit without an `origin` has nowhere to push to, and says nothing of it.
#[test]
fn a_commit_without_an_origin_says_nothing_of_pushing() {
    let repo = TestRepo::new().with_skill();
    let out = repo.run(&["commit", "pdf"]).unwrap();
    assert!(out.starts_with("committed skill:pdf") && !out.contains("push") && !out.contains("fleet"), "{out}");
}

/// A joined Machine reads another repo as a standalone one (`draft`), and
/// still refuses to change it.
#[test]
fn a_joined_machine_reads_another_repo_but_changes_only_its_own() {
    let repo = TestRepo::new().with_skill();
    repo.joined("http://127.0.0.1:1");
    let other = repo.other_repo();
    fs::write(other.join("personal/web/SKILL.md"), "Edited.").unwrap();
    let socket = repo.root.join("no-daemon.sock");
    let draft = outcome(repo.command_for(&other, &socket, &["draft"]).output().unwrap()).unwrap();
    assert!(draft.contains("M skill:web"), "{draft}");
    let error = outcome(repo.command_for(&other, &socket, &["sync"]).output().unwrap()).unwrap_err();
    assert!(error.contains("differs from the joined repo"), "{error}");
}
