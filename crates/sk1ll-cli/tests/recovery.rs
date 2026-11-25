mod common;

use sk1ll_core::{EngineLock, Repo};
use sk1ll_testkit::{Machine, SkillsRepo, git::init_bare, outcome};
use std::{fs, path::Path};

use common::{
    RunningDaemon, command_for, daemon_config, other_repo, path, run, run_on, runtime, with_harnesses, with_pdf,
};

#[test]
fn a_pending_pull_blocks_sync_repair_and_push() {
    let repo = with_pdf();
    let skills = repo.root().join("h");
    fs::write(repo.repo.root().join(".git/sk1ll-pull.json"), "interrupted journal").unwrap();
    for (args, expected) in [
        (&["sync", "--force"][..], "pull pending"),
        (&["sync", "--dry-run"][..], "pull pending"),
        (&["repair-ids"][..], "pull stopped half-way"),
        (&["push"][..], "pull stopped half-way"),
    ] {
        let error = run(&repo, args).unwrap_err();
        assert!(error.contains(expected), "{args:?}: {error}");
    }
    assert!(!skills.exists());
    assert!(!repo.repo.root().join("personal/pdf/.sk1ll-id").exists());
}

#[test]
fn init_takes_the_existing_repos_engine_lock_before_writing() {
    let repo = Machine::new();
    fs::remove_file(repo.repo.root().join("sk1ll.toml")).unwrap();
    let held = EngineLock::acquire(repo.repo.root()).unwrap();
    for target in [repo.repo.root().to_path_buf(), repo.repo.root().join("new/nested")] {
        let err = run(&repo, &["init", target.to_str().unwrap()]).expect_err("init must hold the engine lock");
        assert!(err.contains("another sk1ll process"), "{err}");
        assert!(!target.join("sk1ll.toml").exists());
    }
    assert!(!repo.repo.root().join("new").exists(), "even directory creation waits for the lock");
    drop(held);
    run(&repo, &["init"]).unwrap();
    assert!(repo.repo.root().join("sk1ll.toml").is_file());
    let fresh = repo.root().join("fresh");
    run(&repo, &["init", fresh.to_str().unwrap()]).unwrap();
    assert!(fresh.join("sk1ll.toml").is_file());
}

/// The engine in a command's process takes the engine lock for what it
/// changes, so a change fails while another process (normally the daemon)
/// holds it. Previews read without it; what answers from a reconciled
/// Snapshot says why there is none yet.
#[test]
fn changes_take_the_engine_lock() {
    let repo = with_pdf();
    let skills = repo.root().join("h");
    let stage = skills.join(".sk1ll-tmp/stage-pdf");
    fs::create_dir_all(&stage).unwrap();
    fs::write(stage.join("SKILL.md"), "being staged by the lock holder").unwrap();
    let held = EngineLock::acquire(repo.repo.root()).unwrap();
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
        let err = run(&repo, args).expect_err("refused while the lock is held");
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
        run(&repo, args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
    }
    assert!(stage.join("SKILL.md").is_file(), "a dry run must not recover without the engine lock");
    drop(held);
    run(&repo, &["sync", "--dry-run"]).unwrap();
    assert!(stage.join("SKILL.md").is_file(), "nor with it: a dry run changes nothing");
    run(&repo, &["sync"]).unwrap();
    assert!(!stage.exists(), "a real sync, under the lock, cleans up");
    assert_eq!(fs::read_to_string(skills.join("pdf/SKILL.md")).unwrap(), "PDFs.");
    run(&repo, &["assign", "h", "skill", "pdf", "off"]).unwrap();
    assert!(repo.assignment().exists());
}

/// One engine at a time per Machine, whatever repo it manages: while one
/// for this repo runs (a daemon, or another command), a command about
/// another repo changes nothing of the Machine's harnesses or Assignment.
/// Previews still answer, changing nothing either.
#[test]
fn a_second_engine_for_another_repo_is_refused() {
    let repo = with_pdf();
    let other = other_repo(&repo);
    let socket = repo.no_daemon();
    let skills = repo.root().join("h");
    // The engine managing `repo`, as the daemon would hold it.
    let held = sk1ll_daemon::MachineLock::acquire(&daemon_config(&repo, repo.repo.root())).unwrap();
    let changes: [&[&str]; 2] = [&["sync"], &["assign", "h", "skill", "web", "off"]];
    for args in changes {
        let err = outcome(command_for(&repo, &other, &socket, args).output().unwrap()).expect_err("refused");
        assert!(err.contains("another sk1ll process") && err.contains("managing this Machine"), "{args:?}: {err}");
    }
    assert!(!skills.exists(), "no second engine placed anything");
    assert!(!repo.assignment().exists(), "nor wrote the Assignment");
    for args in [&["status"][..], &["sync", "--dry-run"], &["list"], &["draft"], &["conflicts"]] {
        let out = outcome(command_for(&repo, &other, &socket, args).output().unwrap());
        out.unwrap_or_else(|e| panic!("{args:?}: {e}"));
    }
    assert!(!skills.exists() && !repo.assignment().exists(), "previews change nothing");
    drop(held);
    outcome(command_for(&repo, &other, &socket, &["sync"]).output().unwrap()).unwrap();
    assert!(skills.join("web/SKILL.md").is_file());
}

#[test]
fn recovery_commands_work_with_damaged_configuration() {
    for deleted in [false, true] {
        for command in ["draft", "diff", "restore", "commit", "pull"] {
            let repo = Machine::new();
            // A local origin lets pull run through its real Git path.
            repo.repo.push_to_new_origin(&repo.root().join("origin.git"));
            if deleted {
                fs::remove_file(repo.repo.root().join("sk1ll.toml")).unwrap();
            } else {
                fs::write(repo.repo.root().join("sk1ll.toml"), "[broken").unwrap();
            }
            fs::write(repo.repo.root().join("sk1ll.lock"), "[broken").unwrap();
            assert!(run(&repo, &["list"]).is_err());
            let args = match command {
                "diff" | "restore" => vec![command, "manifest"],
                "commit" => vec![command, "manifest", "--author", "Test <test@example.com>"],
                _ => vec![command],
            };
            let result = run(&repo, &args);
            if command == "commit" && !deleted {
                // A commit cannot validate Import ID claims in invalid TOML.
                let error = result.unwrap_err();
                assert!(error.contains("sk1ll.toml"), "{error}");
            } else {
                assert!(result.is_ok(), "{command}, deleted={deleted}: {result:?}");
            }
            if command == "restore" {
                assert!(Repo::open(repo.repo.root()).is_ok());
            } else {
                assert!(Repo::open(repo.repo.root()).is_err());
            }
        }
    }
}

/// A pending pull's tree is partial: the change is saved, but nothing
/// is placed and no Item ID is written until the pull is finished.
#[test]
fn a_harness_change_during_a_pending_pull_defers_placement() {
    let repo = Machine::new();
    fs::create_dir_all(repo.repo.root().join("personal/pdf")).unwrap();
    fs::write(repo.repo.root().join("personal/pdf/SKILL.md"), "PDFs.").unwrap();
    fs::write(repo.repo.root().join(".git/sk1ll-pull.json"), "{}").unwrap();
    let skills = repo.root().join("h/skills");
    let add = ["harnesses", "add", "h", "--skills", skills.to_str().unwrap(), "--mode", "copy", "--yes"];
    // The sync after the change reports the stopped pull it can't finish.
    if let Err(error) = run(&repo, &add) {
        assert!(error.contains("machine.toml changed, but") && error.contains("pull"), "{error}");
    }
    assert!(fs::read_to_string(repo.machine_toml()).unwrap().contains("[harnesses.h]"));
    assert!(!skills.join("pdf").exists(), "placement waits for the pull");
    assert!(!repo.repo.root().join("personal/pdf/.sk1ll-id").exists(), "and so do Item IDs");
}

/// One Machine's changes reach the other through their shared origin.
fn two_machines() -> (Machine, Machine, tempfile::TempDir) {
    let origin = tempfile::tempdir().unwrap();
    let bare = origin.path().join("origin.git");
    init_bare(&bare);
    let a = with_pdf();
    a.repo.git(&["remote", "add", "origin", path(&bare)]);
    run(&a, &["sync"]).unwrap();
    run(&a, &["commit", "pdf", "--push"]).unwrap();
    let mut b = Machine::new();
    let root = b.repo.root().to_path_buf();
    fs::remove_dir_all(&root).unwrap();
    b.repo = SkillsRepo::clone_from(&bare, root);
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
    fs::write(b.repo.root().join("personal/pdf/SKILL.md"), "b's edit\n").unwrap();
    fs::write(a.repo.root().join("personal/pdf/SKILL.md"), "a's edit\n").unwrap();
    run(&a, &["commit", "pdf", "--push"]).unwrap();

    let blocked = run(&b, &["pull"]).unwrap_err();
    assert!(blocked.contains("`sk1ll pull --keep skill:pdf`"), "{blocked}");
    assert!(blocked.contains("`sk1ll restore skill:pdf`"), "{blocked}");
    assert!(!blocked.contains("commit or restore"), "{blocked}");
    let status = run(&b, &["status"]).unwrap();
    assert!(status.contains("blocked: the pull of baseline") && status.contains("sk1ll restore skill:pdf"), "{status}");
    assert!(status.contains("nothing to place") && !status.contains("in sync"), "{status}");
    // What the docs used to suggest: commit the clashing item.
    run(&b, &["commit", "pdf"]).unwrap();
    let blocked = run(&b, &["pull"]).unwrap_err();
    assert!(blocked.contains("turned 1 unpushed commit back into Draft"), "{blocked}");
    assert!(blocked.contains("`sk1ll restore skill:pdf`"), "{blocked}");
    assert_eq!(skill(b.repo.root()), "b's edit\n", "nothing lost");
    assert!(run(&b, &["draft"]).unwrap().contains("M skill:pdf"));
    // Pushing says the same.
    let push = run(&b, &["push"]).unwrap_or_else(|e| e);
    assert!(!push.contains("commit or restore"), "{push}");

    let restored = run(&b, &["restore", "pdf"]).unwrap();
    let id = restored
        .lines()
        .find_map(|line| line.strip_prefix("kept your version of what the pull is blocked on in recovery: "))
        .unwrap_or_else(|| panic!("{restored}"))
        .to_string();
    run(&b, &["pull"]).unwrap();
    assert_eq!(skill(b.repo.root()), "a's edit\n");
    // The pull keeps what it replaced too, unchanged: the restored entry is the one marked as b's version.
    let listed = run(&b, &["recover", "list"]).unwrap();
    let line = listed.lines().find(|l| l.starts_with(&id)).unwrap_or_else(|| panic!("{listed}"));
    assert!(line.contains("skill:pdf") && line.contains(" pull ") && line.contains("your version"), "{listed}");
    assert!(!listed.contains(".sk1ll-tmp"), "paths only with --paths: {listed}");
    assert!(run(&b, &["recover", "list", "--paths"]).unwrap().contains(".sk1ll-tmp"));
    let status = run(&b, &["status"]).unwrap();
    assert!(status.contains("1 changed entry") && status.contains("`sk1ll recover list`"), "{status}");
    assert!(!status.contains("in sync"), "{status}");
    run(&b, &["recover", "adopt", &id]).unwrap();
    assert_eq!(skill(b.repo.root()), "b's edit\n");
    assert!(run(&b, &["status"]).unwrap().contains("harnesses in sync"));
    run(&b, &["commit", "pdf", "--push"]).unwrap();
    run(&a, &["pull"]).unwrap();
    assert_eq!(skill(a.repo.root()), "b's edit\n");
}

/// `pull --keep` clears Blocked in one step: the new Baseline comes in and
/// this Machine's version goes back on top as Draft, ready to review and
/// commit; an item not named keeps the pull Blocked.
#[test]
fn a_blocked_machine_keeps_its_version_in_one_step() {
    let (a, b, _origin) = two_machines();
    let read = |path: &Path| fs::read_to_string(path).unwrap();
    let skill = |repo: &Path| read(&repo.join("personal/pdf/SKILL.md"));
    fs::write(a.repo.root().join("personal/pdf/SKILL.md"), "a's edit\n").unwrap();
    fs::write(a.repo.root().join("instructions/git.md"), "a's rule\n").unwrap();
    run(&a, &["commit", "pdf", "rule:git", "--push"]).unwrap();
    fs::write(b.repo.root().join("personal/pdf/SKILL.md"), "b's edit\n").unwrap();
    fs::write(b.repo.root().join("instructions/git.md"), "b's rule\n").unwrap();
    // Committing the clash first changes nothing: the pull turns it back into Draft.
    run(&b, &["commit", "pdf"]).unwrap();

    // The Draft's clash is found first.
    let blocked = run(&b, &["pull"]).unwrap_err();
    assert!(blocked.contains("`sk1ll pull --keep rule:git`"), "{blocked}");
    let status = run(&b, &["status"]).unwrap();
    assert!(status.contains("`sk1ll pull --keep rule:git`"), "{status}");
    // Keeping it finds the commit's clash next, which isn't named: still
    // Blocked, on both, and the Draft is as it was.
    let still = run(&b, &["pull", "--keep", "git"]).unwrap_err();
    assert!(still.contains("turned 1 unpushed commit back into Draft"), "{still}");
    assert!(still.contains("`sk1ll pull --keep skill:pdf rule:git`"), "{still}");
    assert_eq!(skill(b.repo.root()), "b's edit\n");
    assert!(read(&b.repo.root().join("instructions/git.md")).ends_with("b's rule\n"));

    let kept = run(&b, &["pull", "--keep", "pdf", "--keep", "git"]).unwrap();
    assert!(kept.contains("baseline "), "{kept}");
    assert!(kept.contains("kept your version of skill:pdf, rule:git on top"), "{kept}");
    assert!(kept.contains("replaces the other change"), "{kept}");
    assert!(kept.contains("`sk1ll diff skill:pdf rule:git`"), "{kept}");
    assert_eq!(skill(b.repo.root()), "b's edit\n");
    assert!(read(&b.repo.root().join("instructions/git.md")).ends_with("b's rule\n"));
    let diff = run(&b, &["diff", "pdf"]).unwrap();
    assert!(diff.contains("-a's edit") && diff.contains("+b's edit"), "{diff}");
    assert!(!run(&b, &["status"]).unwrap().contains("blocked"));
    run(&b, &["commit", "--all", "--push"]).unwrap();
    run(&a, &["pull"]).unwrap();
    assert_eq!(skill(a.repo.root()), "b's edit\n");

    // `--keep-all` names every clashing item.
    fs::write(a.repo.root().join("personal/pdf/SKILL.md"), "a again\n").unwrap();
    run(&a, &["commit", "pdf", "--push"]).unwrap();
    fs::write(b.repo.root().join("personal/pdf/SKILL.md"), "b again\n").unwrap();
    run(&b, &["pull", "--keep-all"]).unwrap();
    assert_eq!(skill(b.repo.root()), "b again\n");
    assert!(run(&b, &["pull", "--keep", "pdf"]).unwrap().contains("already on the latest baseline"));
}

/// What an interrupted keep left is printed by `status` and by every
/// `pull`, a failed one too, until acted on; its notes stay until
/// `recover dismiss-notes`.
#[test]
fn an_interrupted_keep_s_report_is_shown_until_dismissed() {
    let (a, b, _origin) = two_machines();
    let note = "the keep stopped after its pull; your draft deleted personal/pdf/ref.md";
    let report = serde_json::json!([{ "item": "skill:pdf", "entry": null, "error": note }]);
    fs::write(b.repo.root().join(".git/sk1ll-keep-report.json"), report.to_string()).unwrap();
    fs::write(a.repo.root().join("personal/pdf/SKILL.md"), "a's edit\n").unwrap();
    run(&a, &["commit", "pdf", "--push"]).unwrap();
    fs::write(b.repo.root().join("personal/pdf/SKILL.md"), "b's edit\n").unwrap();

    let failed = run(&b, &["pull"]).unwrap_err();
    assert!(failed.contains("blocked:"), "the pull's own error: {failed}");
    assert!(failed.contains("an interrupted keep left your versions in recovery") && failed.contains(note), "{failed}");
    // A pull that fails outright, here because the remote is gone, says so too.
    b.repo.git(&["remote", "set-url", "origin", "/nonexistent/origin.git"]);
    let failed = run(&b, &["pull"]).unwrap_err();
    assert!(failed.contains("/nonexistent/origin.git"), "the pull's own error: {failed}");
    assert!(failed.contains(note), "{failed}");
    let status = run(&b, &["status"]).unwrap();
    assert!(status.contains(note) && status.contains("`sk1ll recover dismiss-notes`"), "{status}");
    assert!(run(&b, &["status"]).unwrap().contains(note), "still there on a second read");
    assert!(run(&b, &["recover", "dismiss-notes"]).unwrap().contains("dismissed 1 note"));
    assert!(!run(&b, &["status"]).unwrap().contains(note));
    assert!(run(&b, &["recover", "dismiss-notes"]).unwrap().contains("no notes to dismiss"));
}

/// `draft`, `conflicts` and `recover list` only read: without a daemon they
/// write no Item ID and sync nothing, and they answer beside a daemon that
/// manages another repo.
#[test]
fn draft_conflicts_and_recover_list_only_read() {
    let repo = with_pdf();
    let sidecar = repo.repo.root().join("personal/pdf").join(sk1ll_core::skill::ID_FILE);
    assert!(run(&repo, &["draft"]).unwrap().contains("A skill:pdf"));
    assert_eq!(run(&repo, &["conflicts"]).unwrap(), "");
    assert_eq!(run(&repo, &["recover", "list"]).unwrap(), "");
    assert!(!sidecar.exists(), "no Item ID written");
    assert!(!repo.root().join("h").exists(), "no harness synced");

    // A hand edit in a harness is a conflict, planned in memory.
    run(&repo, &["sync"]).unwrap();
    fs::write(repo.root().join("h/pdf/SKILL.md"), "edited in the harness").unwrap();
    let conflicts = run(&repo, &["conflicts"]).unwrap();
    assert!(conflicts.contains("pdf"), "{conflicts}");

    let other = other_repo(&repo);
    let daemon = runtime().block_on(RunningDaemon::start(&repo, &other));
    repo.repo.write("personal/pdf/SKILL.md", "Edited.");
    assert!(run_on(&repo, &daemon.socket, &["draft"]).unwrap().contains("A skill:pdf"));
    assert!(run_on(&repo, &daemon.socket, &["conflicts"]).unwrap().contains("pdf"));
    assert_eq!(run_on(&repo, &daemon.socket, &["recover", "list"]).unwrap(), "");
}

/// A commit whose tree the server would refuse for good (a file through a
/// filter driver) is refused before anything is committed or pushed.
#[test]
fn commit_refuses_a_tree_the_server_would_refuse() {
    let (a, _b, _origin) = two_machines();
    let before = a.repo.head();
    fs::write(a.repo.root().join(".gitattributes"), "*.bin filter=lfs\n").unwrap();
    fs::write(a.repo.root().join("personal/pdf/data.bin"), "data\n").unwrap();
    let error = run(&a, &["commit", "pdf", "other:.gitattributes", "--push"]).unwrap_err();
    assert!(error.contains("filter driver") && error.contains("personal/pdf/data.bin"), "{error}");
    assert_eq!(a.repo.head(), before);
    fs::remove_file(a.repo.root().join(".gitattributes")).unwrap();
    run(&a, &["commit", "pdf", "--push"]).unwrap();
}

/// A manifest committed broken (invalid TOML in HEAD) can be repaired with
/// `sk1ll commit manifest`.
#[test]
fn a_manifest_broken_in_head_can_be_fixed_with_a_commit() {
    let repo = Machine::new();
    let manifest = fs::read_to_string(repo.repo.root().join("sk1ll.toml")).unwrap();
    fs::write(repo.repo.root().join("sk1ll.toml"), "[import.web\n").unwrap();
    repo.repo.commit_all("broken by hand");
    fs::write(repo.repo.root().join("sk1ll.toml"), &manifest).unwrap();
    run(&repo, &["commit", "manifest", "-m", "fix the manifest"]).unwrap();
    assert_eq!(run(&repo, &["draft"]).unwrap(), "no draft: on the baseline\n");
}

/// `sync` fails while harness conflicts are left alone, naming the exact
/// command that adopts the edit, and `sync --force` says where the edit it
/// overwrote went.
#[test]
fn sync_fails_on_conflicts_and_force_says_where_the_edit_went() {
    let repo = with_pdf();
    run(&repo, &["sync"]).unwrap();
    fs::write(repo.root().join("h/pdf/SKILL.md"), "edited in the harness").unwrap();
    let error = run(&repo, &["sync"]).unwrap_err();
    assert!(error.contains("! h") && error.contains("1 conflict left alone: `sk1ll adopt h pdf`"), "{error}");
    for command in ["status", "conflicts"] {
        let out = run(&repo, &[command]).unwrap();
        assert!(out.contains("edited by hand in the harness; `sk1ll adopt h pdf` it into the repo"), "{out}");
    }
    let forced = run(&repo, &["sync", "--force"]).unwrap();
    assert!(forced.contains("kept in recovery as ") && forced.contains("h/pdf"), "{forced}");
    let id = forced.split("kept in recovery as ").nth(1).unwrap().split(':').next().unwrap();
    let listed = run(&repo, &["recover", "list"]).unwrap();
    assert!(listed.contains(id) && listed.contains("changed:"), "{listed}");
    assert!(!listed.contains(".sk1ll-tmp"), "{listed}");
    assert!(run(&repo, &["recover", "list", "--paths"]).unwrap().contains(".sk1ll-tmp"));
    run(&repo, &["recover", "adopt", id]).unwrap();
    assert_eq!(repo.repo.read("personal/pdf/SKILL.md"), "edited in the harness");
}

/// Every file and directory under `root`: its kind, mode and contents.
fn tree(root: &Path) -> std::collections::BTreeMap<std::path::PathBuf, (String, u32, Vec<u8>)> {
    use std::os::unix::fs::PermissionsExt;
    let mut found = std::collections::BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            let mode = meta.permissions().mode();
            let rel = path.strip_prefix(root).unwrap().to_path_buf();
            let value = if meta.is_symlink() {
                ("link".into(), mode, std::fs::read_link(&path).unwrap().into_os_string().into_encoded_bytes())
            } else if meta.is_dir() {
                pending.push(path);
                ("dir".into(), mode, Vec::new())
            } else {
                ("file".into(), mode, std::fs::read(&path).unwrap())
            };
            found.insert(rel, value);
        }
    }
    found
}

/// Previews only read: with a Harness rename stopped half-way, a disabled
/// Harness and a skill without an Item ID (all of which a reconcile would
/// change), `status`, `render` and `sync --dry-run` leave every file as it
/// was. They plan as the next reconcile would, with the rename finished.
#[test]
fn previews_leave_a_pending_rename_and_the_harnesses_byte_identical() {
    use sk1ll_core::harness_rename::{RenameIntent, RenameRequest};
    let fixture = with_harnesses();
    let root = fixture.root();
    fixture.repo.write("personal/pdf/SKILL.md", "PDFs.");
    fixture.repo.write("instructions/git.md", "Git.");
    let mut assignment = sk1ll_core::Assignment::default();
    assignment.set_harness("b", false);
    assignment.set_skill("a", &"disabled-id".parse().unwrap(), false);
    assignment.save(&fixture.assignment()).unwrap();
    let config = fixture.machine_toml();
    // sk1ll's state directory, as the CLI finds it from `XDG_STATE_HOME`.
    let state = fixture.home.state_dir();
    let pending = RenameIntent::prepare(
        &state,
        &config,
        &root.join("repo"),
        RenameRequest { from: "a".into(), to: "new-a".into(), replace: false },
        &assignment,
        None,
    )
    .unwrap();
    pending.apply_config(&config).unwrap();
    let before = tree(root);

    let status = run(&fixture, &["status"]).unwrap();
    assert!(status.contains("+ ") && status.contains("pdf"), "{status}");
    let plan = run(&fixture, &["sync", "--dry-run"]).unwrap();
    assert_eq!(plan, status, "status shows the dry run's plan");
    let harnesses: Vec<_> = plan.lines().filter_map(|line| line.split_whitespace().nth(1)).collect();
    assert_eq!(harnesses, ["c", "new-a"], "the renamed Harness, not the disabled one: {plan}");
    run(&fixture, &["render", "new-a"]).unwrap();

    assert_eq!(tree(root), before, "a preview changed the filesystem");
    assert!(RenameIntent::exists(&state, &config).unwrap());
    // A sync, which changes things, finishes the rename.
    run(&fixture, &["sync"]).unwrap();
    assert!(!RenameIntent::exists(&state, &config).unwrap());
    let assignment = sk1ll_core::Assignment::load(&fixture.assignment()).unwrap();
    assert!(!assignment.harnesses.contains_key("a"));
    assert!(!assignment.skill_enabled("new-a", &"disabled-id".parse().unwrap()));
    // `new-a` keeps `a`'s skills directory.
    assert!(root.join("a/pdf/SKILL.md").is_file() && !root.join("b/pdf").exists());
}

#[test]
fn previews_explain_which_harness_edits_can_be_adopted() {
    use sk1ll_testkit::{Harness, InstructionsFormat, write};

    let mut machine = Machine::new();
    let skills = machine.root().join("skills");
    let rules = machine.root().join("rules");
    let block = machine.root().join("AGENTS.md");
    machine.add_harness(Harness::copy("h", &skills).instructions(&rules, InstructionsFormat::Rules));
    machine.add_harness(
        Harness::copy("block", machine.root().join("block")).instructions(&block, InstructionsFormat::Block),
    );
    machine.repo.write("personal/my skill/SKILL.md", "My skill");
    machine.repo.write("instructions/git.md", "Use Conventional Commits.\n");
    run(&machine, &["sync"]).unwrap();

    write(&skills.join("my skill/SKILL.md"), "Skill edited in the harness");
    write(&rules.join("git.md"), "Rule edited in the harness");
    let edited = sk1ll_testkit::read(&block).replace("Use Conventional Commits.", "Block edited in the harness.");
    write(&block, &edited);
    write(&rules.join("new.md"), "Unmanaged rule");
    machine.repo.write("instructions/new.md", "New repo rule");

    let plan = run(&machine, &["sync", "--dry-run"]).unwrap();
    assert!(plan.contains("`sk1ll adopt h 'my skill'`"), "{plan}");
    assert!(plan.contains("`sk1ll adopt h git`") && !plan.contains("adopt h git.md"), "{plan}");
    assert!(plan.contains("move the edits into instructions/"), "{plan}");
    assert!(plan.lines().any(|line| line.contains("(existing rule file not managed by sk1ll)")), "{plan}");
    assert_eq!(sk1ll_testkit::read(&block), edited);
}
